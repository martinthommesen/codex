//! Spawns and talks to one `claude -p` process in stream-json mode.

use crate::CLAUDE_CODE_INSTALL_HINT;
use crate::CODEX_MCP_SERVER_NAME;
use crate::env::apply_session_env;
use codex_api::ApiError;
use serde_json::Value;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdin;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::debug;
use uuid::Uuid;

const STDERR_TAIL_LINES: usize = 40;
const STDERR_LINE_LIMIT: usize = 500;
const KILL_WAIT: Duration = Duration::from_secs(5);

/// How the process attaches to a Claude Code session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionArg {
    /// Start a new Claude Code session with this id (`--session-id`).
    New(Uuid),
    /// Continue an existing Claude Code session (`--resume`).
    Resume(Uuid),
}

/// Settings that require a new process when they change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessKey {
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) system_prompt_hash: String,
}

pub(crate) struct SpawnSpec<'a> {
    pub(crate) command: &'a Path,
    pub(crate) cwd: &'a Path,
    pub(crate) model: &'a str,
    pub(crate) effort: Option<&'a str>,
    pub(crate) system_prompt_file: &'a Path,
    pub(crate) mcp_config: &'a str,
    pub(crate) session: SessionArg,
}

/// Builds the `--mcp-config` JSON that points Claude Code at the Codex tool bridge.
pub(crate) fn mcp_config_json(
    bridge_command: &Path,
    bridge_args: &[String],
    socket_path: &Path,
) -> String {
    let mut args: Vec<Value> = bridge_args.iter().cloned().map(Value::String).collect();
    args.push(Value::String(socket_path.display().to_string()));
    let mut servers = serde_json::Map::new();
    servers.insert(
        CODEX_MCP_SERVER_NAME.to_string(),
        serde_json::json!({
            "type": "stdio",
            "command": bridge_command.display().to_string(),
            "args": args,
        }),
    );
    serde_json::json!({ "mcpServers": servers }).to_string()
}

/// Command-line arguments for a model session.
pub(crate) fn build_args(spec: &SpawnSpec<'_>) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "-p",
        "--verbose",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--include-partial-messages",
        "--model",
        spec.model,
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(effort) = spec.effort {
        args.push("--effort".into());
        args.push(effort.into());
    }
    args.push("--system-prompt-file".into());
    args.push(spec.system_prompt_file.as_os_str().to_os_string());
    let allowed_tools = format!("mcp__{CODEX_MCP_SERVER_NAME}");
    args.extend(
        [
            // Disable every built-in tool: all work goes through Codex's tools.
            "--tools",
            "",
            "--strict-mcp-config",
            "--mcp-config",
            spec.mcp_config,
            "--allowedTools",
            allowed_tools.as_str(),
            "--permission-mode",
            "dontAsk",
            // Load no user/project/local settings (hooks, env, MCP servers, apiKeyHelper).
            "--setting-sources",
            "",
            "--settings",
            r#"{"disableAllHooks":true}"#,
            "--disable-slash-commands",
        ]
        .into_iter()
        .map(OsString::from),
    );
    match spec.session {
        SessionArg::New(id) => {
            args.push("--session-id".into());
            args.push(id.to_string().into());
        }
        SessionArg::Resume(id) => {
            args.push("--resume".into());
            args.push(id.to_string().into());
        }
    }
    args
}

/// One parsed stdout line, or the end of stdout.
#[derive(Debug)]
pub(crate) enum ChildEvent {
    Line(Value),
    Closed,
}

/// A running `claude -p` process.
pub(crate) struct ClaudeProcess {
    child: Child,
    stdin: ChildStdin,
    events: mpsc::UnboundedReceiver<ChildEvent>,
    stderr_tail: Arc<StdMutex<VecDeque<String>>>,
    tasks: Vec<JoinHandle<()>>,
    pub(crate) key: ProcessKey,
    /// Whether `system/init` was received from this process.
    pub(crate) init_seen: bool,
    closed: bool,
}

impl ClaudeProcess {
    pub(crate) fn spawn(spec: &SpawnSpec<'_>, key: ProcessKey) -> Result<Self, ApiError> {
        let mut command = Command::new(spec.command);
        command
            .args(build_args(spec))
            .current_dir(spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(/*kill_on_drop*/ true);
        apply_session_env(&mut command);
        let mut child = command.spawn().map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                ApiError::InvalidRequest {
                    message: format!(
                        "Claude Code CLI `{}` was not found. {CLAUDE_CODE_INSTALL_HINT}",
                        spec.command.display()
                    ),
                }
            } else {
                ApiError::Stream(format!("failed to start Claude Code: {err}"))
            }
        })?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(ApiError::Stream(
                "failed to capture Claude Code stdio".to_string(),
            ));
        };
        let (tx, events) = mpsc::unbounded_channel();
        let stderr_tail = Arc::new(StdMutex::new(VecDeque::new()));
        let tasks = vec![
            tokio::spawn(read_stdout(stdout, tx)),
            tokio::spawn(read_stderr(stderr, Arc::clone(&stderr_tail))),
        ];
        Ok(Self {
            child,
            stdin,
            events,
            stderr_tail,
            tasks,
            key,
            init_seen: false,
            closed: false,
        })
    }

    /// Writes one stream-json line to stdin.
    pub(crate) async fn send(&mut self, value: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await?;
        self.stdin.flush().await
    }

    /// Next stdout event; returns `Closed` forever once stdout ends.
    pub(crate) async fn recv(&mut self) -> ChildEvent {
        if self.closed {
            return ChildEvent::Closed;
        }
        match self.events.recv().await {
            Some(ChildEvent::Line(value)) => ChildEvent::Line(value),
            Some(ChildEvent::Closed) | None => {
                self.closed = true;
                ChildEvent::Closed
            }
        }
    }

    pub(crate) fn is_closed(&mut self) -> bool {
        if self.closed {
            return true;
        }
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Recent stderr output, for error messages.
    pub(crate) fn stderr_tail(&self) -> String {
        match self.stderr_tail.lock() {
            Ok(tail) => tail.iter().cloned().collect::<Vec<_>>().join("\n"),
            Err(_) => String::new(),
        }
    }

    /// Describes how the process ended, waiting briefly for it to exit.
    pub(crate) async fn exit_description(&mut self) -> String {
        let status = match tokio::time::timeout(KILL_WAIT, self.child.wait()).await {
            Ok(Ok(status)) => status.to_string(),
            Ok(Err(err)) => format!("unknown exit status ({err})"),
            Err(_) => {
                self.kill().await;
                "no exit after stdout closed; killed".to_string()
            }
        };
        let stderr = self.stderr_tail();
        if stderr.trim().is_empty() {
            status
        } else {
            format!("{status}; stderr: {stderr}")
        }
    }

    pub(crate) async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(KILL_WAIT, self.child.wait()).await;
        self.closed = true;
    }
}

impl Drop for ClaudeProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn read_stdout(stdout: impl AsyncRead + Unpin, tx: mpsc::UnboundedSender<ChildEvent>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        match serde_json::from_slice::<Value>(&line) {
            Ok(value) => {
                if tx.send(ChildEvent::Line(value)).is_err() {
                    return;
                }
            }
            Err(err) => debug!(
                bytes = line.len(),
                error = %err,
                "ignoring non-JSON line from Claude Code"
            ),
        }
    }
    let _ = tx.send(ChildEvent::Closed);
}

async fn read_stderr(stderr: impl AsyncRead + Unpin, tail: Arc<StdMutex<VecDeque<String>>>) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end();
        if text.is_empty() {
            continue;
        }
        if let Ok(mut tail) = tail.lock() {
            if tail.len() == STDERR_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(bounded_tail(text, STDERR_LINE_LIMIT));
        }
    }
}

/// Keeps the last `limit` bytes of `text` (on a char boundary).
pub(crate) fn bounded_tail(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.len() <= limit {
        return text.to_string();
    }
    let mut start = text.len() - limit;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &text[start..])
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
