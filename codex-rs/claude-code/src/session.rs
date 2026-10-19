//! One Claude Code session per Codex thread, driven turn segment by turn segment.
//!
//! Each [`ClaudeCodeSession::stream`] call is one model request from Codex's point of view. A
//! segment ends either when the model stops to call Codex tools (the tool calls are returned to
//! Codex, which executes them and calls `stream` again with the outputs) or when Claude Code
//! reports the final `result` of the turn.

use crate::CODEX_MCP_SERVER_NAME;
use crate::bridge::Bridge;
use crate::bridge::BridgeCore;
use crate::bridge::ToolRegistry;
use crate::history::Classified;
use crate::history::append_user_blocks;
use crate::history::build_system_prompt;
use crate::history::call_tool_result;
use crate::history::classify;
use crate::history::error_tool_result;
use crate::history::leading_system;
use crate::history::prefix_hash;
use crate::history::render_transcript;
use crate::history::sha256_hex;
use crate::history::text_block;
use crate::history::tool_output_note;
use crate::history::trailing_user_input_start;
use crate::history::user_message_line;
use crate::process::ChildEvent;
use crate::process::ClaudeProcess;
use crate::process::ProcessKey;
use crate::process::SessionArg;
use crate::process::SpawnSpec;
use crate::process::mcp_config_json;
use crate::state::PersistedSession;
use crate::state::load_session;
use crate::state::save_session;
use crate::state::system_prompt_file;
use crate::state::write_private_file;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use codex_api::ResponseStream;
use codex_api::anthropic::AnthropicStreamEvent;
use codex_api::anthropic::AnthropicStreamTranslator;
use codex_api::anthropic::AnthropicToolNameMap;
use codex_api::anthropic::AnthropicUsage;
use codex_protocol::models::ResponseItem;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

/// Values accepted by `claude --effort`.
pub const CLAUDE_CODE_EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// How long an interrupted turn may take to settle before the process is killed.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const EVENT_CHANNEL_CAPACITY: usize = 1600;
const INTERRUPTED_MESSAGE: &str = "The turn was interrupted before Codex ran this tool.";
const RESTARTED_MESSAGE: &str = "Claude Code was restarted before Codex returned this result.";
const MISSING_OUTPUT_MESSAGE: &str = "Codex did not return a result for this tool call.";
const HANDED_OFF_MESSAGE: &str =
    "Codex ended this turn early; this tool call's result, if it ran, comes with the next message.";
/// How long Claude Code may be silent before the driver looks for stuck tool calls.
const UNEXPECTED_CALL_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// A bridged call Codex never saw (for example from a message Claude Code did not stream) is
/// answered with an error after this long, so the turn cannot hang on it.
const UNEXPECTED_CALL_GRACE: Duration = Duration::from_secs(30);

/// Where and how to run Claude Code for one Codex thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeSessionConfig {
    /// The `claude` executable (see [`crate::resolve_claude_command`]).
    pub command: PathBuf,
    /// Command Claude Code launches as its `codex` MCP server; the bridge socket path is appended
    /// to `bridge_args`. Production uses the Codex executable with `["stdio-to-uds"]`.
    pub bridge_command: PathBuf,
    pub bridge_args: Vec<String>,
    pub cwd: PathBuf,
    pub thread_id: String,
    /// Codex-owned directory for session markers and system prompt files.
    pub state_dir: PathBuf,
}

/// One model request.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeCodeTurnRequest {
    pub model: String,
    /// One of [`CLAUDE_CODE_EFFORT_LEVELS`]; `None` uses Claude Code's default.
    pub effort: Option<String>,
    pub base_instructions: String,
    pub input: Vec<ResponseItem>,
    /// Responses-API-shaped tool declarations.
    pub tools: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OutstandingCall {
    call_id: String,
    mcp_name: String,
}

#[derive(Default)]
struct SessionInner {
    bridge: Option<Bridge>,
    loaded: bool,
    /// A state file existed when the session was created.
    had_persisted_state: bool,
    /// The thread id was already used as a Claude Code session id.
    thread_id_used: bool,
    session_id: Option<Uuid>,
    /// Claude Code has created the session, so later processes attach with `--resume`.
    session_started: bool,
    synced_len: usize,
    prefix_hash: String,
    process: Option<ClaudeProcess>,
    /// A user message was sent and its `result` has not arrived yet.
    query_in_flight: bool,
    /// Bridged tool calls returned to Codex whose outputs have not been supplied yet.
    outstanding: Vec<OutstandingCall>,
}

/// Drives the Claude Code CLI as a model backend for one Codex thread.
///
/// The child process is started lazily, reused across turns, and killed when the session is
/// dropped.
pub struct ClaudeCodeSession {
    config: Arc<ClaudeCodeSessionConfig>,
    inner: Arc<Mutex<SessionInner>>,
    abort: watch::Sender<u64>,
}

impl std::fmt::Debug for ClaudeCodeSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeCodeSession")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ClaudeCodeSession {
    pub fn new(config: ClaudeCodeSessionConfig) -> Self {
        let (abort, _) = watch::channel(0);
        Self {
            config: Arc::new(config),
            inner: Arc::new(Mutex::new(SessionInner::default())),
            abort,
        }
    }

    /// Runs one turn segment and streams its events.
    pub async fn stream(&self, request: ClaudeCodeTurnRequest) -> Result<ResponseStream, ApiError> {
        if let Some(effort) = request.effort.as_deref()
            && !CLAUDE_CODE_EFFORT_LEVELS.contains(&effort)
        {
            return Err(ApiError::InvalidRequest {
                message: format!(
                    "Claude Code does not support reasoning effort `{effort}`; supported values \
are {}",
                    CLAUDE_CODE_EFFORT_LEVELS.join(", ")
                ),
            });
        }
        let mut inner = Arc::clone(&self.inner).lock_owned().await;
        if inner.bridge.is_none() {
            let bridge = Bridge::start().await.map_err(|err| {
                ApiError::Stream(format!("failed to start the Codex tool bridge: {err}"))
            })?;
            inner.bridge = Some(bridge);
        }
        let Some(bridge) = inner.bridge.as_ref() else {
            return Err(ApiError::Stream(
                "the Codex tool bridge is unavailable".to_string(),
            ));
        };
        let core = Arc::clone(&bridge.core);
        let mcp_config = mcp_config_json(
            &self.config.bridge_command,
            &self.config.bridge_args,
            bridge.socket_path(),
        );

        let mut declarations = request.tools.clone();
        for item in &request.input {
            if let ResponseItem::AdditionalTools { tools, .. } = item {
                declarations.extend(tools.iter().cloned());
            }
        }
        let registry = ToolRegistry::from_declarations(&declarations);
        for note in &registry.unsupported {
            debug!(note = %note, "Codex tool not exposed to Claude Code");
        }
        let names = registry.translator_names();
        core.set_tools(registry.clone());

        if !inner.loaded {
            inner.loaded = true;
            if let Some(persisted) = load_session(&self.config.state_dir, &self.config.thread_id) {
                inner.had_persisted_state = true;
                if let Ok(id) = Uuid::parse_str(&persisted.session_id) {
                    inner.session_id = Some(id);
                    inner.session_started = true;
                    inner.synced_len = persisted.synced_len;
                    inner.prefix_hash = persisted.prefix_hash;
                }
            }
        }

        let input = &request.input;
        let (leading_len, hoisted) = leading_system(input);
        let system_prompt = build_system_prompt(&request.base_instructions, &hoisted);
        let key = ProcessKey {
            model: request.model.clone(),
            effort: request.effort.clone(),
            system_prompt_hash: sha256_hex(&system_prompt),
        };
        let prompt_path = system_prompt_file(&self.config.state_dir, &self.config.thread_id);
        write_private_file(&prompt_path, system_prompt.as_bytes()).map_err(|err| {
            ApiError::Stream(format!(
                "failed to write the Claude Code system prompt: {err}"
            ))
        })?;

        let prefix_matches = inner.session_id.is_some()
            && inner.synced_len <= input.len()
            && prefix_hash(&input[..inner.synced_len]) == inner.prefix_hash;
        if !prefix_matches {
            let id = self.fresh_session_id(&mut inner);
            reset_session(&mut inner, &core, id).await;
        }

        let needs_spawn = match inner.process.as_mut() {
            Some(process) => process.is_closed() || process.key != key,
            None => true,
        };
        let mut fallback = None;
        if needs_spawn {
            if let Some(mut old) = inner.process.take() {
                old.kill().await;
            }
            inner.query_in_flight = false;
            inner.outstanding.clear();
            core.fail_all(RESTARTED_MESSAGE);
            let Some(session_id) = inner.session_id else {
                return Err(ApiError::Stream(
                    "Claude Code session id is missing".to_string(),
                ));
            };
            let session = if inner.session_started {
                SessionArg::Resume(session_id)
            } else {
                SessionArg::New(session_id)
            };
            let reused_id =
                matches!(session, SessionArg::Resume(_)) || self.thread_uuid() == Some(session_id);
            if reused_id {
                fallback = Some(Fallback {
                    input: input.clone(),
                    leading_len,
                    key: key.clone(),
                });
            }
            let spec = SpawnSpec {
                command: &self.config.command,
                cwd: &self.config.cwd,
                model: &request.model,
                effort: request.effort.as_deref(),
                system_prompt_file: &prompt_path,
                mcp_config: &mcp_config,
                session,
            };
            match ClaudeProcess::spawn(&spec, key.clone()) {
                Ok(process) => inner.process = Some(process),
                Err(err) => {
                    // Nothing reached Claude Code; without this, the next request would match
                    // the empty synced prefix and seed the session without the transcript.
                    forget_unstarted_session(&mut inner);
                    return Err(err);
                }
            }
        }

        let payload = if prefix_matches {
            Payload::Incremental
        } else {
            Payload::Fresh
        };
        deliver_input(&mut inner, &core, input, leading_len, payload).await;

        let (tx, rx_event) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        let driver = Driver {
            inner,
            core,
            config: Arc::clone(&self.config),
            tx,
            abort: self.abort.subscribe(),
            names,
            registry,
            model: request.model,
            mcp_config,
            prompt_path,
            fallback,
        };
        tokio::spawn(driver.run());
        Ok(ResponseStream {
            rx_event,
            upstream_request_id: None,
            interrupt: None,
        })
    }

    /// Stops the Claude Code process (ending any running turn) and fails parked tool calls.
    /// The session stays usable; the next [`Self::stream`] resumes the Claude Code session.
    pub async fn shutdown(&self) {
        self.abort.send_modify(|generation| *generation += 1);
        let process = {
            let mut inner = self.inner.lock().await;
            inner.query_in_flight = false;
            inner.outstanding.clear();
            if let Some(bridge) = inner.bridge.as_ref() {
                bridge.core.fail_all(INTERRUPTED_MESSAGE);
            }
            inner.process.take()
        };
        if let Some(mut process) = process {
            process.kill().await;
        }
    }

    fn thread_uuid(&self) -> Option<Uuid> {
        Uuid::parse_str(&self.config.thread_id).ok()
    }

    /// The thread id for the thread's first Claude Code session, a random id afterwards.
    fn fresh_session_id(&self, inner: &mut SessionInner) -> Uuid {
        if !inner.had_persisted_state
            && !inner.thread_id_used
            && let Some(id) = self.thread_uuid()
        {
            inner.thread_id_used = true;
            return id;
        }
        Uuid::new_v4()
    }
}

impl Drop for ClaudeCodeSession {
    fn drop(&mut self) {
        self.abort.send_modify(|generation| *generation += 1);
        if let Ok(mut inner) = self.inner.try_lock() {
            inner.process = None;
            inner.bridge = None;
        }
    }
}

/// Starts over with a new, empty Claude Code session.
async fn reset_session(inner: &mut SessionInner, core: &BridgeCore, id: Uuid) {
    if let Some(mut process) = inner.process.take() {
        process.kill().await;
    }
    core.fail_all(RESTARTED_MESSAGE);
    inner.session_id = Some(id);
    inner.session_started = false;
    inner.synced_len = 0;
    inner.prefix_hash = prefix_hash(&[]);
    inner.query_in_flight = false;
    inner.outstanding.clear();
}

/// Content for a fresh session: a transcript of earlier history plus the trailing user input.
fn fresh_payload(input: &[ResponseItem], leading_len: usize) -> Vec<Value> {
    let split = trailing_user_input_start(input, leading_len);
    let mut blocks = Vec::new();
    if let Some(transcript) = render_transcript(&input[leading_len..split]) {
        blocks.push(text_block(&transcript));
    }
    let mut new_input = Vec::new();
    for item in &input[split..] {
        if let Classified::UserBlocks(item_blocks) = classify(item) {
            new_input.extend(item_blocks);
        }
    }
    if new_input.is_empty() {
        new_input.push(text_block("Continue."));
    }
    blocks.extend(new_input);
    blocks
}

/// How much of the conversation a request sends to Claude Code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Payload {
    /// What Codex added since the last segment.
    Incremental,
    /// The whole conversation, seeding a fresh session.
    Fresh,
}

/// Sends `input` to Claude Code and advances the sync cursor only once it was delivered, so input
/// that never reached the CLI is replayed by the next request instead of being skipped.
///
/// Delivered means written to the CLI's stdin, or, for tool results, handed to the bridge while
/// the process is running. A process that dies after a successful write but before reading it can
/// still lose the message; rolling the cursor back then could duplicate one the CLI had recorded.
async fn deliver_input(
    inner: &mut SessionInner,
    core: &BridgeCore,
    input: &[ResponseItem],
    leading_len: usize,
    payload: Payload,
) {
    let delivered = match payload {
        Payload::Incremental => send_incremental(inner, core, input, leading_len).await,
        Payload::Fresh => send_user_message(inner, fresh_payload(input, leading_len)).await,
    };
    if delivered {
        inner.synced_len = input.len();
        inner.prefix_hash = prefix_hash(input);
    }
}

/// Sends what Codex added since the last segment; `false` when writing to Claude Code failed.
async fn send_incremental(
    inner: &mut SessionInner,
    core: &BridgeCore,
    input: &[ResponseItem],
    leading_len: usize,
) -> bool {
    let start = inner.synced_len.max(leading_len).min(input.len());
    let mut answers: Vec<(OutstandingCall, Value)> = Vec::new();
    let mut blocks = Vec::new();
    for item in &input[start..] {
        match classify(item) {
            Classified::ToolOutput { call_id, output } => {
                match inner
                    .outstanding
                    .iter()
                    .position(|call| call.call_id == call_id)
                {
                    Some(position) => {
                        let call = inner.outstanding.remove(position);
                        answers.push((call, call_tool_result(output)));
                    }
                    None => blocks.push(text_block(&tool_output_note(call_id, output))),
                }
            }
            Classified::UserBlocks(item_blocks) => blocks.extend(item_blocks),
            Classified::Skip => {}
        }
    }
    let unanswered = std::mem::take(&mut inner.outstanding);
    answers.extend(
        unanswered
            .into_iter()
            .map(|call| (call, error_tool_result(MISSING_OUTPUT_MESSAGE))),
    );
    if !answers.is_empty() {
        // Answers for a process that has exited would be lost; the next request replays them to
        // its replacement as notes instead.
        if inner.process.as_mut().is_none_or(ClaudeProcess::is_closed) {
            return false;
        }
        if !blocks.is_empty()
            && let Some((_, result)) = answers.last_mut()
        {
            // Claude Code is still inside the turn waiting on these calls, so new input rides
            // along with the last result instead of starting a second turn.
            append_user_blocks(result, &blocks);
        }
        for (call, result) in answers {
            core.answer(&call.call_id, &call.mcp_name, result);
        }
        return true;
    }
    if !blocks.is_empty() {
        if inner.query_in_flight {
            settle_query(inner, core).await;
        }
        send_user_message(inner, blocks).await
    } else if !inner.query_in_flight {
        send_user_message(inner, vec![text_block("Continue.")]).await
    } else {
        true
    }
}

/// Writes a user message to Claude Code; `false` when the write failed.
async fn send_user_message(inner: &mut SessionInner, blocks: Vec<Value>) -> bool {
    let Some(process) = inner.process.as_mut() else {
        return false;
    };
    if let Err(err) = process.send(&user_message_line(blocks)).await {
        // The driver reports the exit (with stderr) when stdout closes.
        debug!(error = %err, "failed to write to Claude Code stdin");
        return false;
    }
    inner.query_in_flight = true;
    true
}

fn interrupt_request() -> Value {
    json!({
        "type": "control_request",
        "request_id": format!("codex-interrupt-{}", Uuid::new_v4().simple()),
        "request": {"subtype": "interrupt"},
    })
}

/// Interrupts the running query and waits briefly for it to settle; kills the process if it
/// does not, so the next segment resumes in a new process.
async fn settle_query(inner: &mut SessionInner, core: &BridgeCore) {
    core.fail_all(INTERRUPTED_MESSAGE);
    inner.outstanding.clear();
    inner.query_in_flight = false;
    let mut saw_init = false;
    let settled = {
        let Some(process) = inner.process.as_mut() else {
            return;
        };
        if process.send(&interrupt_request()).await.is_err() {
            false
        } else {
            let result = tokio::time::timeout(SETTLE_TIMEOUT, async {
                loop {
                    match process.recv().await {
                        ChildEvent::Line(value) => {
                            if value.get("type").and_then(Value::as_str) == Some("result") {
                                return true;
                            }
                            if is_init(&value) {
                                process.init_seen = true;
                                saw_init = true;
                            }
                        }
                        ChildEvent::Closed => return false,
                    }
                }
            })
            .await;
            matches!(result, Ok(true))
        }
    };
    if saw_init {
        inner.session_started = true;
    }
    if !settled && let Some(mut process) = inner.process.take() {
        process.kill().await;
        forget_unstarted_session(inner);
    }
}

/// After the process is lost, a session Claude Code never started holds none of the input that
/// was sent to it; clearing the id makes the next request rebuild the full payload.
fn forget_unstarted_session(inner: &mut SessionInner) {
    if !inner.session_started {
        inner.session_id = None;
        inner.synced_len = 0;
        inner.prefix_hash = prefix_hash(&[]);
    }
}

fn is_init(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("system")
        && value.get("subtype").and_then(Value::as_str) == Some("init")
}

/// Verifies that Claude Code connected to the Codex tool bridge.
pub(crate) fn check_bridge_status(init: &Value) -> Result<(), ApiError> {
    let status = init
        .get("mcp_servers")
        .and_then(Value::as_array)
        .and_then(|servers| {
            servers.iter().find(|server| {
                server.get("name").and_then(Value::as_str) == Some(CODEX_MCP_SERVER_NAME)
            })
        })
        .and_then(|server| server.get("status").and_then(Value::as_str));
    match status {
        Some("connected") | Some("pending") => Ok(()),
        Some(other) => Err(ApiError::InvalidRequest {
            message: format!(
                "Claude Code could not connect to the Codex tool bridge (status: {other}); the \
turn was stopped because Codex tools would be unavailable"
            ),
        }),
        None => Err(ApiError::InvalidRequest {
            message: "Claude Code did not load the Codex tool bridge MCP server; the turn was \
stopped because Codex tools would be unavailable"
                .to_string(),
        }),
    }
}

/// Maps an error `result` line to an API error.
pub(crate) fn result_error(value: &Value) -> ApiError {
    let subtype = value
        .get("subtype")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let message = value
        .get("result")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            value.get("errors").and_then(Value::as_array).map(|errors| {
                errors
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("; ")
            })
        })
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| format!("Claude Code ended the turn with `{subtype}`"));
    let lower = message.to_ascii_lowercase();
    if ["prompt is too long", "context window", "context length"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return ApiError::ContextWindowExceeded;
    }
    if [
        "not logged in",
        "/login",
        "invalid api key",
        "authentication",
        "oauth token",
        "unauthorized",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return ApiError::InvalidRequest {
            message: format!(
                "Claude Code: {message} (sign in with `claude auth login`, then run \
`codex login --provider claude-code`)"
            ),
        };
    }
    if ["usage limit", "credit balance"]
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return ApiError::InvalidRequest {
            message: format!("Claude Code: {message}"),
        };
    }
    ApiError::Stream(format!("Claude Code reported an error: {message}"))
}

/// Replays the whole history into a new session when attaching to an existing one fails.
struct Fallback {
    input: Vec<ResponseItem>,
    leading_len: usize,
    key: ProcessKey,
}

#[derive(Default)]
struct Segment {
    translator: Option<AnthropicStreamTranslator>,
    /// tool_use block id -> tool name as shown to the model.
    tool_use_names: BTreeMap<String, String>,
    /// Bridged calls in the current message.
    message_calls: Vec<OutstandingCall>,
    held_completion: Option<ResponseEvent>,
    held_error: Option<ApiError>,
    created_sent: bool,
    forwarded_any: bool,
}

enum Step {
    Continue,
    Done,
}

struct Driver {
    inner: OwnedMutexGuard<SessionInner>,
    core: Arc<BridgeCore>,
    config: Arc<ClaudeCodeSessionConfig>,
    tx: mpsc::Sender<Result<ResponseEvent, ApiError>>,
    abort: watch::Receiver<u64>,
    names: AnthropicToolNameMap,
    registry: ToolRegistry,
    model: String,
    mcp_config: String,
    prompt_path: PathBuf,
    fallback: Option<Fallback>,
}

enum Wake {
    Event(ChildEvent),
    ConsumerGone,
    Abort,
    CheckParked,
}

impl Driver {
    async fn run(mut self) {
        let mut segment = Segment::default();
        loop {
            if self.inner.process.is_none() {
                self.fail(ApiError::Stream("Claude Code is not running".to_string()))
                    .await;
                return;
            }
            let wake = {
                let Some(process) = self.inner.process.as_mut() else {
                    return;
                };
                tokio::select! {
                    biased;
                    _ = self.tx.closed() => Wake::ConsumerGone,
                    _ = self.abort.changed() => Wake::Abort,
                    event = process.recv() => Wake::Event(event),
                    _ = tokio::time::sleep(UNEXPECTED_CALL_CHECK_INTERVAL) => Wake::CheckParked,
                }
            };
            let step = match wake {
                Wake::ConsumerGone => {
                    self.cancel(&mut segment).await;
                    Step::Done
                }
                Wake::Abort => {
                    if let Some(mut process) = self.inner.process.take() {
                        process.kill().await;
                    }
                    forget_unstarted_session(&mut self.inner);
                    self.inner.query_in_flight = false;
                    self.inner.outstanding.clear();
                    self.core.fail_all(INTERRUPTED_MESSAGE);
                    self.fail(ApiError::Stream(
                        "the Claude Code session was shut down".to_string(),
                    ))
                    .await;
                    Step::Done
                }
                Wake::CheckParked => {
                    let expected = segment
                        .message_calls
                        .iter()
                        .map(|call| call.call_id.clone())
                        .collect::<Vec<_>>();
                    let released = self
                        .core
                        .fail_unexpected_parked(&expected, UNEXPECTED_CALL_GRACE);
                    if released > 0 {
                        warn!(
                            released,
                            "answered Claude Code tool calls that never reached Codex with an error"
                        );
                    }
                    Step::Continue
                }
                Wake::Event(ChildEvent::Closed) => self.on_exit(&mut segment).await,
                Wake::Event(ChildEvent::Line(value)) => self.on_line(value, &mut segment).await,
            };
            if matches!(step, Step::Done) {
                return;
            }
        }
    }

    async fn on_line(&mut self, value: Value, segment: &mut Segment) -> Step {
        match value.get("type").and_then(Value::as_str).unwrap_or("") {
            "system" if is_init(&value) => {
                if let Err(err) = check_bridge_status(&value) {
                    self.stop_process().await;
                    forget_unstarted_session(&mut self.inner);
                    self.fail(err).await;
                    return Step::Done;
                }
                if let Some(process) = self.inner.process.as_mut() {
                    process.init_seen = true;
                }
                self.inner.session_started = true;
                Step::Continue
            }
            "stream_event" => {
                if value
                    .get("parent_tool_use_id")
                    .is_some_and(|parent| !parent.is_null())
                {
                    return Step::Continue;
                }
                let Some(event) = value.get("event") else {
                    return Step::Continue;
                };
                match serde_json::from_value::<AnthropicStreamEvent>(event.clone()) {
                    Ok(event) => self.on_stream_event(event, segment).await,
                    Err(err) => {
                        debug!(error = %err, "ignoring unparseable Claude Code stream event");
                        Step::Continue
                    }
                }
            }
            "result" => self.on_result(&value, segment).await,
            _ => Step::Continue,
        }
    }

    async fn on_stream_event(
        &mut self,
        event: AnthropicStreamEvent,
        segment: &mut Segment,
    ) -> Step {
        let is_message_stop = matches!(event, AnthropicStreamEvent::MessageStop);
        match &event {
            AnthropicStreamEvent::MessageStart { .. } => {
                segment.translator = Some(AnthropicStreamTranslator::new(self.names.clone()));
                segment.tool_use_names.clear();
                segment.message_calls.clear();
                segment.held_completion = None;
                segment.held_error = None;
            }
            AnthropicStreamEvent::ContentBlockStart { content_block, .. }
                if content_block.get("type").and_then(Value::as_str) == Some("tool_use") =>
            {
                if let (Some(id), Some(name)) = (
                    content_block.get("id").and_then(Value::as_str),
                    content_block.get("name").and_then(Value::as_str),
                ) {
                    segment
                        .tool_use_names
                        .insert(id.to_string(), name.to_string());
                }
            }
            _ => {}
        }
        let translator = segment
            .translator
            .get_or_insert_with(|| AnthropicStreamTranslator::new(self.names.clone()));
        let events = match translator.handle(event) {
            Ok(events) => events,
            Err(err) => {
                if is_message_stop && !segment.message_calls.is_empty() {
                    // Codex already received these calls and runs them, and Claude Code runs
                    // every complete tool_use block too, so its query now waits on their
                    // bridged results and no `result` can arrive. Hand them back as on a
                    // tool_use stop, so the next request answers them, and report the error now.
                    self.inner.outstanding = std::mem::take(&mut segment.message_calls);
                    self.persist().await;
                    self.fail(err).await;
                    return Step::Done;
                }
                if is_message_stop {
                    // Refusals, context overflow and empty max_tokens stops: reported when the
                    // turn's result arrives, unless Claude Code continues with another message.
                    segment.held_error = Some(err);
                } else {
                    // Claude Code retries API errors itself; its result reports the outcome.
                    debug!(error = %err, "Claude Code stream reported an error event");
                }
                return Step::Continue;
            }
        };
        let stop_reason = translator.stop_reason().map(str::to_string);
        for event in events {
            match event {
                ResponseEvent::Created { .. } => {
                    if segment.created_sent {
                        continue;
                    }
                    segment.created_sent = true;
                    if !self.forward(event, segment).await {
                        return self.consumer_gone(segment).await;
                    }
                }
                ResponseEvent::OutputItemDone(item) => {
                    let call_id = match &item {
                        ResponseItem::FunctionCall { call_id, .. }
                        | ResponseItem::CustomToolCall { call_id, .. } => Some(call_id.clone()),
                        _ => None,
                    };
                    if let Some(call_id) = call_id {
                        let mcp_name = segment
                            .tool_use_names
                            .get(&call_id)
                            .and_then(|wire| self.registry.mcp_name_for_wire(wire))
                            .map(str::to_string);
                        let Some(mcp_name) = mcp_name else {
                            // Not a Codex tool: Claude Code answers it itself with an error.
                            debug!("dropping a call to a tool Codex does not provide");
                            continue;
                        };
                        segment
                            .message_calls
                            .push(OutstandingCall { call_id, mcp_name });
                    }
                    if !self
                        .forward(ResponseEvent::OutputItemDone(item), segment)
                        .await
                    {
                        return self.consumer_gone(segment).await;
                    }
                }
                ResponseEvent::Completed { .. } => {
                    // Claude Code runs every complete tool_use block, whatever the stop reason
                    // (for example `max_tokens` after a later block was cut off), so forwarded
                    // calls always go back to Codex now.
                    if !segment.message_calls.is_empty() {
                        self.inner.outstanding = std::mem::take(&mut segment.message_calls);
                        if !self.forward(event, segment).await {
                            return self.consumer_gone(segment).await;
                        }
                        self.persist().await;
                        return Step::Done;
                    }
                    if stop_reason.as_deref() == Some("tool_use") {
                        // Only tools Codex does not provide; Claude Code answers those itself.
                        continue;
                    }
                    segment.held_completion = Some(event);
                }
                other => {
                    if !self.forward(other, segment).await {
                        return self.consumer_gone(segment).await;
                    }
                }
            }
        }
        Step::Continue
    }

    async fn on_result(&mut self, value: &Value, segment: &mut Segment) -> Step {
        let init_seen = self
            .inner
            .process
            .as_ref()
            .is_some_and(|process| process.init_seen);
        if !init_seen && !segment.forwarded_any && self.fallback.is_some() {
            self.stop_process().await;
            return self.run_fallback().await;
        }
        // A started session holds this query's input even when the query failed, so the cursor
        // stays advanced and a retry continues it rather than repeating the input. Without
        // `system/init` there is no session, so the next request reseeds a fresh one.
        forget_unstarted_session(&mut self.inner);
        self.inner.query_in_flight = false;
        let is_error = value
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = if is_error {
            Err(result_error(value))
        } else if let Some(err) = segment.held_error.take() {
            Err(err)
        } else {
            let completion = match segment.held_completion.take() {
                Some(ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    usage_metadata,
                    ..
                }) => ResponseEvent::Completed {
                    response_id,
                    token_usage,
                    usage_metadata,
                    end_turn: Some(true),
                },
                _ => ResponseEvent::Completed {
                    response_id: String::new(),
                    token_usage: value
                        .get("usage")
                        .and_then(|usage| {
                            serde_json::from_value::<AnthropicUsage>(usage.clone()).ok()
                        })
                        .map(AnthropicUsage::to_token_usage),
                    usage_metadata: None,
                    end_turn: Some(true),
                },
            };
            Ok(completion)
        };
        self.persist().await;
        let _ = self.tx.send(outcome).await;
        Step::Done
    }

    async fn on_exit(&mut self, segment: &mut Segment) -> Step {
        let Some(mut process) = self.inner.process.take() else {
            return Step::Done;
        };
        let init_seen = process.init_seen;
        let description = process.exit_description().await;
        drop(process);
        self.inner.query_in_flight = false;
        self.inner.outstanding.clear();
        self.core.fail_all(RESTARTED_MESSAGE);
        if !init_seen && !segment.forwarded_any && self.fallback.is_some() {
            return self.run_fallback().await;
        }
        forget_unstarted_session(&mut self.inner);
        self.fail(ApiError::Stream(format!(
            "Claude Code exited before the turn finished ({description})"
        )))
        .await;
        Step::Done
    }

    /// Starts a fresh session seeded with the whole history after `--resume` (or reusing the
    /// thread id) failed.
    async fn run_fallback(&mut self) -> Step {
        let Some(fallback) = self.fallback.take() else {
            return Step::Done;
        };
        warn!("could not attach to the previous Claude Code session; starting a fresh one");
        let id = Uuid::new_v4();
        reset_session(&mut self.inner, &self.core, id).await;
        let spec = SpawnSpec {
            command: &self.config.command,
            cwd: &self.config.cwd,
            model: &self.model,
            effort: fallback.key.effort.as_deref(),
            system_prompt_file: &self.prompt_path,
            mcp_config: &self.mcp_config,
            session: SessionArg::New(id),
        };
        match ClaudeProcess::spawn(&spec, fallback.key.clone()) {
            Ok(process) => self.inner.process = Some(process),
            Err(err) => {
                forget_unstarted_session(&mut self.inner);
                self.fail(err).await;
                return Step::Done;
            }
        }
        deliver_input(
            &mut self.inner,
            &self.core,
            &fallback.input,
            fallback.leading_len,
            Payload::Fresh,
        )
        .await;
        Step::Continue
    }

    /// The consumer dropped the stream: interrupt the turn and release parked calls.
    async fn cancel(&mut self, segment: &mut Segment) {
        // Codex already received these calls and still settles them (a steer runs in-flight
        // tools before the next request), so their outputs come with that request; telling
        // Claude Code the turn ended before they ran would contradict it.
        let forwarded = std::mem::take(&mut segment.message_calls)
            .into_iter()
            .chain(std::mem::take(&mut self.inner.outstanding));
        for call in forwarded {
            self.core.answer(
                &call.call_id,
                &call.mcp_name,
                error_tool_result(HANDED_OFF_MESSAGE),
            );
        }
        settle_query(&mut self.inner, &self.core).await;
        self.persist().await;
    }

    async fn consumer_gone(&mut self, segment: &mut Segment) -> Step {
        self.cancel(segment).await;
        Step::Done
    }

    async fn stop_process(&mut self) {
        if let Some(mut process) = self.inner.process.take() {
            process.kill().await;
        }
        self.inner.query_in_flight = false;
        self.inner.outstanding.clear();
        self.core.fail_all(RESTARTED_MESSAGE);
    }

    async fn forward(&self, event: ResponseEvent, segment: &mut Segment) -> bool {
        segment.forwarded_any = true;
        self.tx.send(Ok(event)).await.is_ok()
    }

    async fn fail(&self, err: ApiError) {
        let _ = self.tx.send(Err(err)).await;
    }

    async fn persist(&self) {
        if !self.inner.session_started {
            return;
        }
        let Some(session_id) = self.inner.session_id else {
            return;
        };
        let session = PersistedSession {
            session_id: session_id.to_string(),
            synced_len: self.inner.synced_len,
            prefix_hash: self.inner.prefix_hash.clone(),
            model: self.model.clone(),
        };
        let state_dir = self.config.state_dir.clone();
        let thread_id = self.config.thread_id.clone();
        let result =
            tokio::task::spawn_blocking(move || save_session(&state_dir, &thread_id, &session))
                .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(err)) => warn!(error = %err, "failed to persist Claude Code session state"),
            Err(err) => warn!(error = %err, "failed to persist Claude Code session state"),
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
