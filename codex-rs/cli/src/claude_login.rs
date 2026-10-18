//! `codex login` / `codex logout` / `codex login status` support for Claude providers.
//!
//! Claude credentials are kept apart from ChatGPT credentials: the Anthropic API key lives in its
//! own Codex store, and Claude Code (subscription) sign-in stays inside the official `claude`
//! CLI. Nothing here reads, writes, or invalidates ChatGPT/OpenAI credentials.

use std::path::Path;
use std::path::PathBuf;

use codex_claude_code::ClaudeCodeAuthStatus;
use codex_claude_code::ClaudeCodeError;
use codex_claude_code::claude_code_auth_status;
use codex_claude_code::resolve_claude_command;
use codex_core::config::Config;
use codex_core::provider_models::CLAUDE_CODE_CONNECTED_MARKER;
use codex_login::AnthropicAuthConfig;
use codex_login::AnthropicCredentialSource;
use codex_model_provider_info::ANTHROPIC_API_KEY_ENV_VAR;

/// Provider selected with `--provider` on login/logout/status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum LoginProvider {
    /// ChatGPT sign-in or an OpenAI API key (the default).
    #[default]
    #[value(alias = "chatgpt")]
    Openai,
    /// An Anthropic API key for the Messages API.
    Anthropic,
    /// The official Claude Code CLI with its own sign-in (for example a Claude subscription).
    ClaudeCode,
    /// Every provider (logout only).
    All,
}

fn anthropic_auth_config(config: &Config) -> AnthropicAuthConfig {
    AnthropicAuthConfig::new(
        config.codex_home.to_path_buf(),
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
}

fn marker_path(codex_home: &Path) -> PathBuf {
    codex_home.join(CLAUDE_CODE_CONNECTED_MARKER)
}

/// Stores an Anthropic API key without touching ChatGPT credentials.
pub fn login_anthropic_with_api_key(config: &Config, api_key: &str) -> Result<String, String> {
    if !api_key.starts_with("sk-ant-") {
        eprintln!(
            "Warning: the key does not look like an Anthropic API key (expected an `sk-ant-` prefix)."
        );
    }
    let auth = anthropic_auth_config(config);
    auth.save_api_key(api_key)
        .map_err(|err| format!("Error storing Anthropic API key: {err}"))?;
    let masked = auth
        .resolve()
        .ok()
        .flatten()
        .map(|credential| credential.masked())
        .unwrap_or_else(|| "***".to_string());
    Ok(format!(
        "Stored Anthropic API key ({masked}). Your ChatGPT/OpenAI login is unchanged.\n\
Use it with `-c model_provider=anthropic` (or `model_provider = \"anthropic\"` in config.toml)."
    ))
}

/// Connects the official Claude Code CLI, running its own sign-in if needed.
///
/// Codex never sees Claude Code credentials; it only records a non-secret marker so agents may
/// delegate to the `claude-code` provider.
pub async fn login_claude_code(config: &Config) -> Result<String, String> {
    let command = resolve_claude_command(/*explicit*/ None);
    let mut status = claude_code_auth_status(&command)
        .await
        .map_err(describe_claude_code_error)?;
    if !status.logged_in {
        eprintln!("Claude Code is not signed in. Starting `claude auth login`...");
        let exit = tokio::process::Command::new(&command)
            .args(["auth", "login"])
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .await
            .map_err(|err| format!("Failed to run `claude auth login`: {err}"))?;
        if !exit.success() {
            return Err("`claude auth login` did not complete successfully.".to_string());
        }
        status = claude_code_auth_status(&command)
            .await
            .map_err(describe_claude_code_error)?;
        if !status.logged_in {
            return Err("Claude Code is still not signed in.".to_string());
        }
    }
    let marker = marker_path(&config.codex_home);
    if let Some(parent) = marker.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Failed to create {}: {err}", parent.display()))?;
    }
    let contents = serde_json::json!({
        "command": command.display().to_string(),
        "auth_method": status.auth_method,
    });
    std::fs::write(&marker, format!("{contents:#}\n"))
        .map_err(|err| format!("Failed to write {}: {err}", marker.display()))?;
    let mut message = format!(
        "Connected Claude Code ({}). Your ChatGPT/OpenAI login is unchanged.\n\
Use it with `-c model_provider=claude-code`; agents can also delegate to it.",
        status.describe()
    );
    if !status.uses_subscription() {
        message.push_str(
            "\nNote: Claude Code is not using a Claude subscription sign-in, so usage is billed to \
the credential it reports above.",
        );
    }
    Ok(message)
}

fn describe_claude_code_error(err: ClaudeCodeError) -> String {
    format!("Claude Code is unavailable: {err}")
}

/// Removes only the stored Anthropic API key.
pub fn logout_anthropic(config: &Config) -> Result<String, String> {
    let removed = anthropic_auth_config(config)
        .delete()
        .map_err(|err| format!("Error removing Anthropic API key: {err}"))?;
    let mut message = if removed {
        "Removed the stored Anthropic API key. Your ChatGPT/OpenAI login is unchanged.".to_string()
    } else {
        "No stored Anthropic API key.".to_string()
    };
    if std::env::var(ANTHROPIC_API_KEY_ENV_VAR)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
    {
        message.push_str(
            "\nANTHROPIC_API_KEY is still set in your environment; Codex keeps using it until you \
unset it.",
        );
    }
    Ok(message)
}

/// Disconnects Claude Code from Codex without signing out of Claude Code itself.
pub fn logout_claude_code(config: &Config) -> Result<String, String> {
    let marker = marker_path(&config.codex_home);
    let removed = match std::fs::remove_file(&marker) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => return Err(format!("Failed to remove {}: {err}", marker.display())),
    };
    let prefix = if removed {
        "Disconnected Claude Code from Codex."
    } else {
        "Claude Code was not connected to Codex."
    };
    Ok(format!(
        "{prefix} Codex never stores Claude Code credentials, so your Claude Code sign-in is \
unchanged; run `claude auth logout` to sign out of Claude Code itself."
    ))
}

/// Status line for the Anthropic API provider; `Ok(true)` when a key is available.
pub fn anthropic_status(config: &Config) -> (bool, String) {
    match anthropic_auth_config(config).resolve() {
        Ok(Some(credential)) => {
            let source = match credential.source() {
                AnthropicCredentialSource::Stored => "stored by Codex",
                AnthropicCredentialSource::Environment => "from ANTHROPIC_API_KEY",
            };
            (
                true,
                format!("Anthropic API: API key {} ({source})", credential.masked()),
            )
        }
        Ok(None) => (
            false,
            "Anthropic API: not configured (`codex login --provider anthropic --with-api-key`)"
                .to_string(),
        ),
        Err(err) => (
            false,
            format!("Anthropic API: error reading credentials: {err}"),
        ),
    }
}

/// Status line for the Claude Code provider; `true` when connected and signed in.
pub async fn claude_code_status(config: &Config) -> (bool, String) {
    let connected = marker_path(&config.codex_home).exists();
    let command = resolve_claude_command(/*explicit*/ None);
    match claude_code_auth_status(&command).await {
        Ok(status) => describe_claude_code_status(connected, &status),
        Err(err) => (
            false,
            format!("Claude Code: {}", describe_claude_code_error(err)),
        ),
    }
}

fn describe_claude_code_status(connected: bool, status: &ClaudeCodeAuthStatus) -> (bool, String) {
    match (connected, status.logged_in) {
        (true, true) => (
            true,
            format!("Claude Code: connected ({})", status.describe()),
        ),
        (true, false) => (
            false,
            "Claude Code: connected to Codex but not signed in (run `claude auth login`)"
                .to_string(),
        ),
        (false, true) => (
            false,
            format!(
                "Claude Code: signed in ({}) but not connected to Codex \
(`codex login --provider claude-code`)",
                status.describe()
            ),
        ),
        (false, false) => (
            false,
            "Claude Code: not connected (`codex login --provider claude-code`)".to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "claude_login_tests.rs"]
mod tests;
