//! Environment hygiene for Claude Code child processes.
//!
//! Claude Code gives `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` precedence over its own
//! subscription sign-in, so they are always removed; Codex-held OpenAI/Codex secrets are removed so
//! they can never reach the child. Claude Code's own credentials (`CLAUDE_CODE_OAUTH_TOKEN`, the
//! keychain, `~/.claude`) are left alone and never read by Codex.

use tokio::process::Command;

/// Variables that are always removed from the Claude Code environment.
pub const SCRUBBED_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "CODEX_ACCESS_TOKEN",
    // Set inside a running Claude Code session; Codex starts an independent session.
    "CLAUDECODE",
];

/// Settings forced for model sessions:
/// - `ENABLE_TOOL_SEARCH=false`: Claude Code otherwise defers MCP tools behind its (disabled)
///   tool-search tool, which would hide Codex's tools from the model.
/// - `CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT=0`: bridged calls stay parked while Codex waits for
///   approvals, so the idle timeout must not abort them.
const SESSION_ENV_OVERRIDES: &[(&str, &str)] = &[
    ("ENABLE_TOOL_SEARCH", "false"),
    ("CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT", "0"),
];

/// Returns whether `name` must not be passed to Claude Code.
pub(crate) fn is_scrubbed_env_var(name: &str) -> bool {
    if SCRUBBED_ENV_VARS.contains(&name) {
        return true;
    }
    let upper = name.to_ascii_uppercase();
    (upper.starts_with("CODEX_") || upper.starts_with("OPENAI_"))
        && (upper.contains("KEY") || upper.contains("TOKEN") || upper.contains("SECRET"))
}

/// Removes secrets from the inherited environment of `command`.
pub(crate) fn apply_scrubbed_env(command: &mut Command) {
    for name in SCRUBBED_ENV_VARS {
        command.env_remove(name);
    }
    for (name, _) in std::env::vars_os() {
        if let Some(name) = name.to_str()
            && is_scrubbed_env_var(name)
        {
            command.env_remove(name);
        }
    }
}

/// Scrubs the environment and applies the settings model sessions depend on.
pub(crate) fn apply_session_env(command: &mut Command) {
    apply_scrubbed_env(command);
    for (name, value) in SESSION_ENV_OVERRIDES {
        command.env(name, value);
    }
}

#[cfg(test)]
#[path = "env_tests.rs"]
mod tests;
