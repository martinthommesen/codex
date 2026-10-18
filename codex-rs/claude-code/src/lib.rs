//! Runs the official, unmodified Claude Code CLI (`claude`) as a Codex model backend.
//!
//! Claude Code authenticates with its own sign-in (for example a Claude subscription); Codex never
//! reads Claude Code credentials. Claude Code's built-in tools are disabled and Codex's tools are
//! exposed to it through an MCP bridge. Bridged tool calls are parked until Codex has executed the
//! tool itself (through its normal sandbox and approval flow) and supplies the output, so Claude
//! Code never runs commands or edits files on its own.

mod auth;
mod bridge;
mod env;
mod error;
mod history;
mod process;
mod session;
mod state;

use std::path::Path;
use std::path::PathBuf;

pub use auth::ClaudeCodeAuthStatus;
pub use auth::claude_code_auth_status;
pub use env::SCRUBBED_ENV_VARS;
pub use error::ClaudeCodeError;
pub use session::CLAUDE_CODE_EFFORT_LEVELS;
pub use session::ClaudeCodeSession;
pub use session::ClaudeCodeSessionConfig;
pub use session::ClaudeCodeTurnRequest;

/// Environment variable that overrides the Claude Code executable.
pub const CLAUDE_CODE_PATH_ENV_VAR: &str = "CODEX_CLAUDE_CODE_PATH";

/// Executable name used when neither an explicit path nor the override variable is set.
pub const DEFAULT_CLAUDE_COMMAND: &str = "claude";

/// MCP server name Codex registers inside Claude Code; tools appear as `mcp__codex__<name>`.
pub const CODEX_MCP_SERVER_NAME: &str = "codex";

/// Shown when the Claude Code CLI cannot be found.
pub const CLAUDE_CODE_INSTALL_HINT: &str = "Install Claude Code (https://code.claude.com/docs/en/setup), \
sign in with `claude auth login`, or point CODEX_CLAUDE_CODE_PATH at the `claude` executable.";

/// Resolves the Claude Code executable: an explicit path wins, then `CODEX_CLAUDE_CODE_PATH`,
/// then `claude` on `PATH`.
pub fn resolve_claude_command(explicit: Option<&Path>) -> PathBuf {
    if let Some(explicit) = explicit {
        return explicit.to_path_buf();
    }
    match std::env::var_os(CLAUDE_CODE_PATH_ENV_VAR) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(DEFAULT_CLAUDE_COMMAND),
    }
}

/// Notes for tool declarations that cannot be exposed to Claude Code (for example hosted OpenAI
/// tools), so callers can report the capability gap instead of dropping the tools silently.
pub fn unsupported_tool_notes(tools: &[serde_json::Value]) -> Vec<String> {
    codex_api::anthropic::translate_tools(tools).unsupported
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
