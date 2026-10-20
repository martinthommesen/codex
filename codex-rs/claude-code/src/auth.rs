//! Reads Claude Code's sign-in status through `claude auth status --json`.
//!
//! Codex only asks the CLI whether it is signed in and how; it never reads Claude Code's
//! credential files or keychain entries.

use crate::CLAUDE_CODE_INSTALL_HINT;
use crate::env::apply_scrubbed_env;
use crate::error::ClaudeCodeError;
use crate::process::bounded_tail;
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_LIMIT: usize = 2_000;

/// Claude Code sign-in state as reported by the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeAuthStatus {
    pub logged_in: bool,
    /// `claude.ai`, `oauth_token`, `api_key`, `api_key_helper`, `third_party`, or `none`.
    pub auth_method: Option<String>,
    /// For example `firstParty`, `bedrock`, or `vertex`.
    pub api_provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAuthStatus {
    #[serde(default)]
    logged_in: bool,
    #[serde(default)]
    auth_method: Option<String>,
    #[serde(default)]
    api_provider: Option<String>,
}

impl ClaudeCodeAuthStatus {
    /// Whether Claude Code is signed in with a Claude subscription (claude.ai login or a
    /// `claude setup-token` OAuth token) rather than an API key or a cloud provider.
    pub fn uses_subscription(&self) -> bool {
        self.logged_in
            && matches!(
                self.auth_method.as_deref(),
                Some("claude.ai" | "oauth_token")
            )
    }

    /// Whether Codex sessions can use this sign-in. Sessions start Claude Code without user
    /// settings, so a credential that only an `apiKeyHelper` setting provides is unavailable.
    pub fn usable_in_codex(&self) -> bool {
        self.logged_in && self.auth_method.as_deref() != Some("api_key_helper")
    }

    /// Human-readable one-line summary; contains no credential material.
    pub fn describe(&self) -> String {
        if !self.logged_in {
            return "Claude Code is not signed in; run `claude auth login`".to_string();
        }
        match self.auth_method.as_deref() {
            Some("claude.ai") => "signed in to Claude Code with a Claude subscription".to_string(),
            Some("oauth_token") => {
                "signed in to Claude Code with a Claude subscription OAuth token (for example from \
`claude setup-token`)"
                    .to_string()
            }
            Some("api_key") => {
                "Claude Code is using an Anthropic API key it manages (not a Claude subscription)"
                    .to_string()
            }
            Some("api_key_helper") => {
                "Claude Code is configured with an apiKeyHelper (not a Claude \
subscription); Codex starts Claude Code without user settings, so the helper is not used in Codex \
sessions"
                    .to_string()
            }
            Some("third_party") => format!(
                "Claude Code is using a third-party provider ({})",
                self.api_provider.as_deref().unwrap_or("unknown")
            ),
            Some(other) => format!("signed in to Claude Code (method: {other})"),
            None => "signed in to Claude Code".to_string(),
        }
    }
}

/// Parses the JSON printed by `claude auth status --json`.
pub(crate) fn parse_auth_status(stdout: &str) -> Result<ClaudeCodeAuthStatus, ClaudeCodeError> {
    let raw: RawAuthStatus = serde_json::from_str(stdout.trim()).map_err(|err| {
        ClaudeCodeError::Protocol(format!(
            "`claude auth status --json` did not print a status object ({err})"
        ))
    })?;
    let auth_method = raw.auth_method.filter(|method| method != "none");
    Ok(ClaudeCodeAuthStatus {
        logged_in: raw.logged_in,
        auth_method,
        api_provider: raw.api_provider,
    })
}

/// Runs `<command> auth status --json` with a scrubbed environment.
pub async fn claude_code_auth_status(
    command: &Path,
) -> Result<ClaudeCodeAuthStatus, ClaudeCodeError> {
    let mut cmd = Command::new(command);
    cmd.args(["auth", "status", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(/*kill_on_drop*/ true);
    apply_scrubbed_env(&mut cmd);
    let child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(ClaudeCodeError::NotInstalled {
                command: command.display().to_string(),
                hint: CLAUDE_CODE_INSTALL_HINT,
            });
        }
        Err(err) => return Err(ClaudeCodeError::Io(err)),
    };
    let output = tokio::time::timeout(AUTH_STATUS_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| ClaudeCodeError::Timeout(AUTH_STATUS_TIMEOUT))??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_auth_status(&stdout) {
        Ok(status) => Ok(status),
        Err(err) if output.status.success() => Err(err),
        Err(_) => Err(ClaudeCodeError::CommandFailed {
            status: output.status.to_string(),
            stderr: bounded_tail(&String::from_utf8_lossy(&output.stderr), STDERR_LIMIT),
        }),
    }
}

/// Runs `<command> auth login` interactively (inherited stdio) with a scrubbed environment, so
/// Anthropic and Codex secrets from the caller's environment never reach the sign-in flow.
pub async fn claude_code_auth_login(
    command: &Path,
) -> Result<std::process::ExitStatus, ClaudeCodeError> {
    let mut cmd = Command::new(command);
    cmd.args(["auth", "login"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    apply_scrubbed_env(&mut cmd);
    match cmd.status().await {
        Ok(status) => Ok(status),
        Err(err) if err.kind() == ErrorKind::NotFound => Err(ClaudeCodeError::NotInstalled {
            command: command.display().to_string(),
            hint: CLAUDE_CODE_INSTALL_HINT,
        }),
        Err(err) => Err(ClaudeCodeError::Io(err)),
    }
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
