//! MCP server that exposes Codex's tools to Claude Code over a private Unix socket.
//!
//! Claude Code launches the bridge command (`codex stdio-to-uds <socket>`), which relays MCP
//! stdio framing to this listener. `tools/call` requests are never executed here: they are parked
//! until Codex has run the tool through its own sandbox/approval flow and supplies the output.

use crate::CODEX_MCP_SERVER_NAME;
use codex_api::anthropic::AnthropicToolNameMap;
use codex_api::anthropic::AnthropicToolTarget;
use codex_api::anthropic::anthropic_wire_tool_name_with_limit;
use codex_api::anthropic::tool_name_collision_note;
use codex_api::anthropic::translate_tools;
use codex_uds::UnixListener;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::MutexGuard;
use std::time::Duration;
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use tokio::task::JoinHandle;
use tracing::debug;
use tracing::warn;

/// MCP tool names are capped so `mcp__codex__<name>` stays within Anthropic's 64 characters.
pub(crate) const MCP_TOOL_NAME_LIMIT: usize = 52;
/// Meta key Claude Code uses to tag `tools/call` requests with the model's tool_use id.
pub(crate) const TOOL_USE_ID_META_KEY: &str = "claudecode/toolUseId";
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
/// Unix socket paths must stay well below the platform `sun_path` limit.
const MAX_SOCKET_PATH_LEN: usize = 100;
const UNEXPECTED_CALL_MESSAGE: &str = "Codex did not receive this tool call from Claude Code, so it was not run. Call the tool again.";
/// Longest MCP message (one line) a client may send; far above any tool call's arguments.
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// Tool calls one connection may have waiting for Codex at once.
const MAX_PARKED_PER_CONNECTION: usize = 128;
/// Answers kept for calls that have not arrived yet; the oldest is dropped beyond this.
const MAX_PREPARED: usize = 128;
const UNTAGGED_CALL_MESSAGE: &str =
    "Codex only answers tool calls that carry Claude Code's tool_use id.";
const TOO_MANY_CALLS_MESSAGE: &str =
    "Too many tool calls are waiting for Codex. Call the tool again once they finish.";

/// Codex tools as exposed through the bridge.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ToolRegistry {
    /// MCP tool definitions (`name`, `description`, `inputSchema`).
    pub(crate) tools: Vec<Value>,
    /// MCP tool name -> Codex tool identity.
    pub(crate) targets: BTreeMap<String, AnthropicToolTarget>,
    /// Declarations with no equivalent (for example hosted OpenAI tools).
    pub(crate) unsupported: Vec<String>,
}

impl ToolRegistry {
    pub(crate) fn from_declarations(declarations: &[Value]) -> Self {
        let translated = translate_tools(declarations);
        let mut registry = Self {
            unsupported: translated.unsupported,
            ..Self::default()
        };
        for tool in translated.tools {
            let target = translated.names.resolve(&tool.name);
            let mcp_name = anthropic_wire_tool_name_with_limit(
                target.namespace.as_deref(),
                &target.name,
                MCP_TOOL_NAME_LIMIT,
            );
            // `translate_tools` already merged re-declarations of the same tool, so a shorter
            // bridged name can only collide with a different tool.
            if let Some(existing) = registry.targets.get(&mcp_name) {
                registry.unsupported.push(tool_name_collision_note(
                    target.namespace.as_deref(),
                    &target.name,
                    &wire_tool_name(&mcp_name),
                    existing,
                ));
                continue;
            }
            let mut definition = serde_json::Map::new();
            definition.insert("name".to_string(), Value::String(mcp_name.clone()));
            if !tool.description.is_empty() {
                definition.insert("description".to_string(), Value::String(tool.description));
            }
            definition.insert("inputSchema".to_string(), tool.input_schema);
            registry.tools.push(Value::Object(definition));
            registry.targets.insert(mcp_name, target);
        }
        registry
    }

    /// Name map for translating Claude Code's `tool_use` blocks back to Codex tools.
    pub(crate) fn translator_names(&self) -> AnthropicToolNameMap {
        let mut names = AnthropicToolNameMap::default();
        for (mcp_name, target) in &self.targets {
            names.insert(wire_tool_name(mcp_name), target.clone());
        }
        names
    }

    /// MCP tool name for a model-facing tool name, if it is one of Codex's tools.
    pub(crate) fn mcp_name_for_wire<'a>(&self, wire: &'a str) -> Option<&'a str> {
        let mcp_name = wire.strip_prefix(&wire_prefix())?;
        self.targets.contains_key(mcp_name).then_some(mcp_name)
    }
}

fn wire_prefix() -> String {
    format!("mcp__{CODEX_MCP_SERVER_NAME}__")
}

/// The tool name Claude Code shows the model for a bridged tool.
pub(crate) fn wire_tool_name(mcp_name: &str) -> String {
    format!("{}{mcp_name}", wire_prefix())
}

struct Connection {
    tx: mpsc::UnboundedSender<String>,
    initialized: bool,
    task: Option<AbortHandle>,
}

#[derive(Debug)]
struct ParkedCall {
    connection: u64,
    request_id: Value,
    tool_use_id: Option<String>,
    mcp_name: String,
    parked_at: Instant,
}

#[derive(Debug)]
struct PreparedAnswer {
    call_id: String,
    mcp_name: String,
    result: Value,
}

#[derive(Default)]
struct BridgeState {
    registry: ToolRegistry,
    connections: BTreeMap<u64, Connection>,
    next_connection: u64,
    parked: Vec<ParkedCall>,
    /// Outputs supplied before Claude Code sent the matching `tools/call`.
    prepared: Vec<PreparedAnswer>,
    /// A call carried a tool_use id. Claude Code then tags all its calls, so calls without one
    /// come from another client and are refused instead of matched by name.
    tagged_calls_seen: bool,
}

/// Shared bridge state; the socket listener lives in [`Bridge`].
#[derive(Default)]
pub(crate) struct BridgeCore {
    state: StdMutex<BridgeState>,
}

impl BridgeCore {
    fn lock(&self) -> MutexGuard<'_, BridgeState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Replaces the exposed tools; notifies connected clients when the set changed.
    pub(crate) fn set_tools(&self, registry: ToolRegistry) -> bool {
        let mut state = self.lock();
        if state.registry.tools == registry.tools && state.registry.targets == registry.targets {
            return false;
        }
        state.registry = registry;
        let notification =
            json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}).to_string();
        for connection in state.connections.values() {
            if connection.initialized {
                let _ = connection.tx.send(notification.clone());
            }
        }
        true
    }

    /// Answers the parked `tools/call` for `call_id`, or keeps the answer until it arrives.
    pub(crate) fn answer(&self, call_id: &str, mcp_name: &str, result: Value) {
        let mut state = self.lock();
        let position = state
            .parked
            .iter()
            .position(|call| call.tool_use_id.as_deref() == Some(call_id))
            // Id-less calls stay parked only while no call carried an id (`tagged_calls_seen`).
            .or_else(|| {
                state
                    .parked
                    .iter()
                    .position(|call| call.tool_use_id.is_none() && call.mcp_name == mcp_name)
            });
        match position {
            Some(position) => {
                let call = state.parked.remove(position);
                respond(&state, &call, result);
            }
            None => {
                if state.prepared.len() >= MAX_PREPARED {
                    state.prepared.remove(0);
                }
                state.prepared.push(PreparedAnswer {
                    call_id: call_id.to_string(),
                    mcp_name: mcp_name.to_string(),
                    result,
                });
            }
        }
    }

    /// Answers, with an error, calls parked longer than `grace` whose `tool_use_id` is not in
    /// `expected` (calls Codex will run). Returns how many were answered.
    pub(crate) fn fail_unexpected_parked(&self, expected: &[String], grace: Duration) -> usize {
        let mut state = self.lock();
        let (stale, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.parked)
            .into_iter()
            .partition(|call| {
                call.parked_at.elapsed() >= grace
                    && !call
                        .tool_use_id
                        .as_ref()
                        .is_some_and(|id| expected.contains(id))
            });
        state.parked = kept;
        for call in &stale {
            respond(
                &state,
                call,
                crate::history::error_tool_result(UNEXPECTED_CALL_MESSAGE),
            );
        }
        stale.len()
    }

    /// Fails every parked call with `message` and drops prepared answers.
    pub(crate) fn fail_all(&self, message: &str) {
        let mut state = self.lock();
        let parked = std::mem::take(&mut state.parked);
        for call in &parked {
            respond(&state, call, crate::history::error_tool_result(message));
        }
        state.prepared.clear();
    }

    #[cfg(test)]
    pub(crate) fn parked_count(&self) -> usize {
        self.lock().parked.len()
    }

    #[cfg(test)]
    pub(crate) fn prepared_count(&self) -> usize {
        self.lock().prepared.len()
    }

    fn register_connection(&self, tx: mpsc::UnboundedSender<String>) -> u64 {
        let mut state = self.lock();
        state.next_connection += 1;
        let id = state.next_connection;
        state.connections.insert(
            id,
            Connection {
                tx,
                initialized: false,
                task: None,
            },
        );
        id
    }

    fn set_connection_task(&self, id: u64, task: AbortHandle) {
        if let Some(connection) = self.lock().connections.get_mut(&id) {
            connection.task = Some(task);
        }
    }

    fn remove_connection(&self, id: u64) {
        let mut state = self.lock();
        state.connections.remove(&id);
        state.parked.retain(|call| call.connection != id);
    }

    fn abort_connections(&self) {
        let mut state = self.lock();
        for connection in state.connections.values() {
            if let Some(task) = &connection.task {
                task.abort();
            }
        }
        state.connections.clear();
        state.parked.clear();
    }

    /// Handles one JSON-RPC message; returns the immediate response, if any.
    fn handle_message(&self, connection: u64, message: &Value) -> Option<Value> {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id").filter(|id| !id.is_null()).cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.handle_request(connection, method, id, &params),
            (Some(method), None) => {
                self.handle_notification(connection, method, &params);
                None
            }
            // Responses to server-initiated requests; the bridge sends none.
            (None, _) => None,
        }
    }

    fn handle_request(
        &self,
        connection: u64,
        method: &str,
        id: Value,
        params: &Value,
    ) -> Option<Value> {
        match method {
            "initialize" => {
                let protocol_version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL_VERSION);
                Some(result_response(
                    id,
                    json!({
                        "protocolVersion": protocol_version,
                        "capabilities": {"tools": {"listChanged": true}},
                        "serverInfo": {
                            "name": CODEX_MCP_SERVER_NAME,
                            "version": env!("CARGO_PKG_VERSION"),
                        },
                    }),
                ))
            }
            "ping" => Some(result_response(id, json!({}))),
            "tools/list" => {
                let tools = self.lock().registry.tools.clone();
                Some(result_response(id, json!({ "tools": tools })))
            }
            "tools/call" => self.handle_tool_call(connection, id, params),
            other => Some(error_response(
                id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {other}"),
            )),
        }
    }

    fn handle_tool_call(&self, connection: u64, id: Value, params: &Value) -> Option<Value> {
        let Some(mcp_name) = params.get("name").and_then(Value::as_str) else {
            return Some(error_response(
                id,
                INVALID_PARAMS,
                "tools/call requires a name",
            ));
        };
        let tool_use_id = params
            .get("_meta")
            .and_then(|meta| meta.get(TOOL_USE_ID_META_KEY))
            .and_then(Value::as_str)
            .map(str::to_string);
        let mut state = self.lock();
        match &tool_use_id {
            Some(tool_use_id) => {
                if !state.tagged_calls_seen {
                    state.tagged_calls_seen = true;
                    let (untagged, tagged): (Vec<_>, Vec<_>) = std::mem::take(&mut state.parked)
                        .into_iter()
                        .partition(|call| call.tool_use_id.is_none());
                    state.parked = tagged;
                    for call in &untagged {
                        respond(
                            &state,
                            call,
                            crate::history::error_tool_result(UNTAGGED_CALL_MESSAGE),
                        );
                    }
                }
                // An answer Codex already supplied wins even if the next request exposes a
                // different tool set (for example a compaction request with no tools): Claude
                // Code sends serial calls only after earlier ones resolve, and Codex has already
                // run this one.
                if let Some(position) = state
                    .prepared
                    .iter()
                    .position(|answer| &answer.call_id == tool_use_id)
                {
                    let answer = state.prepared.remove(position);
                    return Some(result_response(id, answer.result));
                }
            }
            None if state.tagged_calls_seen => {
                return Some(result_response(
                    id,
                    crate::history::error_tool_result(UNTAGGED_CALL_MESSAGE),
                ));
            }
            None => {}
        }
        if !state.registry.targets.contains_key(mcp_name) {
            return Some(result_response(
                id,
                crate::history::error_tool_result(&format!(
                    "Codex does not provide a tool named `{mcp_name}`."
                )),
            ));
        }
        if tool_use_id.is_none()
            && let Some(position) = state
                .prepared
                .iter()
                .position(|answer| answer.mcp_name == mcp_name)
        {
            let answer = state.prepared.remove(position);
            return Some(result_response(id, answer.result));
        }
        let parked_here = state
            .parked
            .iter()
            .filter(|call| call.connection == connection)
            .count();
        if parked_here >= MAX_PARKED_PER_CONNECTION {
            return Some(result_response(
                id,
                crate::history::error_tool_result(TOO_MANY_CALLS_MESSAGE),
            ));
        }
        debug!(tool = %mcp_name, "parking Claude Code tool call until Codex supplies the output");
        state.parked.push(ParkedCall {
            connection,
            request_id: id,
            tool_use_id,
            mcp_name: mcp_name.to_string(),
            parked_at: Instant::now(),
        });
        None
    }

    fn handle_notification(&self, connection: u64, method: &str, params: &Value) {
        match method {
            "notifications/initialized" => {
                if let Some(connection) = self.lock().connections.get_mut(&connection) {
                    connection.initialized = true;
                }
            }
            "notifications/cancelled" => {
                if let Some(request_id) = params.get("requestId") {
                    self.lock().parked.retain(|call| {
                        call.connection != connection || &call.request_id != request_id
                    });
                }
            }
            _ => {}
        }
    }

    /// Starts serving one MCP client connection in the background.
    pub(crate) fn spawn_connection<S>(self: &Arc<Self>, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let id = self.register_connection(tx.clone());
        let task = tokio::spawn(Arc::clone(self).serve_connection(id, tx, rx, stream));
        self.set_connection_task(id, task.abort_handle());
    }

    async fn serve_connection<S>(
        self: Arc<Self>,
        id: u64,
        tx: mpsc::UnboundedSender<String>,
        mut rx: mpsc::UnboundedReceiver<String>,
        stream: S,
    ) where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, mut write) = tokio::io::split(stream);
        let writer = tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if write.write_all(line.as_bytes()).await.is_err()
                    || write.write_all(b"\n").await.is_err()
                    || write.flush().await.is_err()
                {
                    return;
                }
            }
        });
        let mut reader = BufReader::new(read);
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = (&mut reader)
                .take(MAX_MESSAGE_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .await;
            match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if line.len() > MAX_MESSAGE_BYTES {
                warn!(
                    limit = MAX_MESSAGE_BYTES,
                    "closing a Codex tool bridge connection that sent an oversized message"
                );
                break;
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let message: Value = match serde_json::from_slice(&line) {
                Ok(message) => message,
                Err(err) => {
                    debug!(bytes = line.len(), error = %err, "ignoring malformed MCP message");
                    continue;
                }
            };
            if let Some(response) = self.handle_message(id, &message)
                && tx.send(response.to_string()).is_err()
            {
                break;
            }
        }
        self.remove_connection(id);
        drop(tx);
        let _ = writer.await;
    }
}

fn respond(state: &BridgeState, call: &ParkedCall, result: Value) {
    if let Some(connection) = state.connections.get(&call.connection) {
        let response = result_response(call.request_id.clone(), result);
        let _ = connection.tx.send(response.to_string());
    }
}

fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A listening bridge socket in a private directory.
pub(crate) struct Bridge {
    pub(crate) core: Arc<BridgeCore>,
    socket_dir: PathBuf,
    socket_path: PathBuf,
    accept_task: JoinHandle<()>,
}

impl Bridge {
    pub(crate) async fn start() -> std::io::Result<Self> {
        let socket_dir = private_socket_dir();
        codex_uds::prepare_private_socket_directory(&socket_dir).await?;
        let socket_path = socket_dir.join("mcp.sock");
        let listener = match UnixListener::bind(&socket_path).await {
            Ok(listener) => listener,
            Err(err) => {
                let _ = std::fs::remove_dir_all(&socket_dir);
                return Err(err);
            }
        };
        // Owner-only whatever the umask, as defense in depth: the private directory already keeps
        // other users out.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Err(err) =
                std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
            {
                let _ = std::fs::remove_dir_all(&socket_dir);
                return Err(err);
            }
        }
        let core = Arc::new(BridgeCore::default());
        let accept_task = tokio::spawn(accept_loop(listener, Arc::clone(&core)));
        Ok(Self {
            core,
            socket_dir,
            socket_path,
            accept_task,
        })
    }

    pub(crate) fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.accept_task.abort();
        self.core.abort_connections();
        let _ = std::fs::remove_dir_all(&self.socket_dir);
    }
}

fn private_socket_dir() -> PathBuf {
    let name = format!(
        "codex-cc-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    );
    let candidate = std::env::temp_dir().join(&name);
    // Leave room for the socket file name inside the directory.
    if cfg!(unix) && candidate.as_os_str().len() + "/mcp.sock".len() >= MAX_SOCKET_PATH_LEN {
        return PathBuf::from("/tmp").join(name);
    }
    candidate
}

/// Access control is the private socket directory (0700, or a user-only DACL on Windows) and, on
/// Unix, an owner-only socket. Other processes of the same user can still connect, but a
/// connection can only park calls, never make Codex run a tool; id-less calls are refused once
/// calls carry tool_use ids, and message size and queued calls are bounded. Peers are not checked
/// against the Claude Code process tree: the peer is a grandchild, sometimes behind shims, ancestry
/// checks need per-OS code and are racy under PID reuse, and a same-user process can read Codex's
/// rollouts or attach to Claude Code anyway. There is deliberately no `ensure_non_elevated_peer`
/// check either: it rejects every peer when Codex runs elevated.
async fn accept_loop(mut listener: UnixListener, core: Arc<BridgeCore>) {
    loop {
        match listener.accept().await {
            Ok(stream) => core.spawn_connection(stream),
            // The bridge serves every Claude Code process of the session, so keep accepting.
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::ConnectionAborted
                        | ErrorKind::ConnectionReset
                        | ErrorKind::Interrupted
                ) =>
            {
                debug!(error = %err, "recoverable Codex tool bridge accept error");
            }
            Err(err) => {
                warn!(error = %err, "Codex tool bridge accept error");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "bridge_tests.rs"]
mod tests;
