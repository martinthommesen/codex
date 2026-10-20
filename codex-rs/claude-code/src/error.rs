use std::time::Duration;

/// Errors from running the Claude Code CLI outside of a model turn.
#[derive(Debug, thiserror::Error)]
pub enum ClaudeCodeError {
    #[error("Claude Code CLI `{command}` was not found. {hint}")]
    NotInstalled { command: String, hint: &'static str },
    #[error("failed to run Claude Code CLI: {0}")]
    Io(#[from] std::io::Error),
    #[error("unexpected output from Claude Code CLI: {0}")]
    Protocol(String),
    #[error("Claude Code CLI did not finish within {0:?}")]
    Timeout(Duration),
    #[error("Claude Code CLI exited with {status}: {stderr}")]
    CommandFailed { status: String, stderr: String },
}
