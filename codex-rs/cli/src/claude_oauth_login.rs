//! Native experimental Claude OAuth command handling. Never invokes the Claude Code CLI.

use std::io;
use std::io::BufRead;
use std::io::IsTerminal;
use std::io::Write;
use std::time::Duration;

use codex_core::config::Config;
use codex_login::ClaudeOAuthAuthConfig;
use codex_login::ClaudeOAuthCredential;
use codex_login::ClaudeOAuthLogin;
use crossterm::event::Event;
use crossterm::event::EventStream;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use crossterm::terminal;
use futures::StreamExt;
use tokio::sync::oneshot;

const MAX_CALLBACK_BYTES: usize = 64 * 1024;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const EXPERIMENTAL_WARNING: &str = "Experimental Claude OAuth: this unofficial integration uses \
undocumented compatibility behavior and may violate Anthropic's third-party authentication \
policy or affect your account. Anthropic controls billing: subscription-quota treatment is \
unverified. Review Usage and disable separately billed extra usage in your Claude account \
settings before continuing. Codex never falls back to an API key or another provider.";

fn auth_config(config: &Config) -> ClaudeOAuthAuthConfig {
    ClaudeOAuthAuthConfig::new(
        config.codex_home.to_path_buf(),
        config.cli_auth_credentials_store_mode,
        config.auth_keyring_backend_kind(),
    )
}

pub(crate) async fn login(config: &Config) -> Result<String, String> {
    eprintln!("{EXPERIMENTAL_WARNING}");
    let auth = auth_config(config);
    let login = ClaudeOAuthLogin::new()
        .map_err(|err| format!("Could not initialize Claude OAuth authorization: {err}"))?;
    let callback_server = login.start_callback_server();
    eprintln!(
        "\nOpen this URL in your browser to authorize your own Claude account:\n\n{}\n\n\
After authorization, Codex accepts the localhost callback automatically. You can also paste \
the full callback URL or code#state below, including when the callback page cannot load. \
Input is hidden. Press Ctrl-C to cancel.",
        login.authorization_url()
    );
    if callback_server.is_err() {
        // Do not print listener diagnostics containing request data. Manual completion uses
        // the same PKCE verifier and state validation as a successful local callback.
        eprintln!("The localhost callback listener is unavailable; use manual completion.");
    }

    let authorize = async {
        let callback = match callback_server {
            Ok(server) => {
                tokio::select! {
                    callback = server.wait_for_callback() => callback.map_err(|err| {
                        format!("Claude OAuth callback failed: {err}")
                    })?,
                    callback = read_callback(/*callback_listener_available*/ true) => callback?,
                }
            }
            Err(_) => read_callback(/*callback_listener_available*/ false).await?,
        };
        auth.complete_login(login, &callback)
            .await
            .map_err(|err| format!("Claude OAuth authorization failed: {err}"))?;
        Ok(
            "Saved Claude OAuth credentials in Codex's isolated secure store.\n\
Use `codex -c model_provider=claude-oauth -m claude-opus-5-5`.\n\
This provider remains experimental. Subscription-quota consumption has not been verified; \
check your Claude account's Usage and billing settings."
                .to_string(),
        )
    };
    tokio::select! {
        result = authorize => result,
        _ = tokio::time::sleep(LOGIN_TIMEOUT) => Err("Claude OAuth login timed out; start a new login.".to_string()),
        _ = tokio::signal::ctrl_c() => Err("Claude OAuth login cancelled.".to_string()),
    }
}

/// Reports local credential presence without exchanging or refreshing credentials.
pub(crate) fn status(config: &Config) -> (bool, String) {
    match auth_config(config).load_stored_credential() {
        Ok(Some(credential)) => (true, describe_status(&credential)),
        Ok(None) => (
            false,
            "Claude OAuth (experimental): not configured (`codex login --provider claude-oauth`)"
                .to_string(),
        ),
        Err(err) => (
            false,
            format!("Claude OAuth (experimental): could not read secure credentials: {err}"),
        ),
    }
}

fn describe_status(credential: &ClaudeOAuthCredential) -> String {
    let identity: String = credential
        .email()
        .chars()
        .filter(|character| !character.is_control())
        .collect();
    let identity = if identity.is_empty() {
        "account identity unavailable".to_string()
    } else {
        identity
    };
    let expiration = if credential.expires_at() <= chrono::Utc::now().timestamp().max(0) as u64 {
        "access token expired; refresh required on next request"
    } else {
        "access token has not expired"
    };
    format!(
        "Claude OAuth (experimental): saved credentials ({identity}; {expiration}). \
Local status only; entitlement and subscription-quota billing are unverified."
    )
}

pub(crate) fn logout(config: &Config) -> Result<String, String> {
    let removed = auth_config(config)
        .delete()
        .map_err(|err| format!("Could not remove Claude OAuth credentials: {err}"))?;
    let message = if removed {
        "Removed Codex's stored Claude OAuth credentials."
    } else {
        "No Claude OAuth credentials were stored by Codex."
    };
    Ok(format!(
        "{message} Other providers and Claude Code credentials are unchanged. \
Local logout does not revoke the grant at Anthropic; manage account access in Claude settings."
    ))
}

async fn read_callback(callback_listener_available: bool) -> Result<String, String> {
    if io::stdin().is_terminal() {
        return read_terminal_callback().await;
    }
    print_prompt()?;
    // A pipe may remain open after the HTTP callback wins. Do not block runtime shutdown
    // on an uncancellable stdin read. A detached thread contains no credential logging.
    let (sender, receiver) = oneshot::channel();
    std::thread::Builder::new()
        .name("claude-oauth-callback-input".to_string())
        .spawn(move || {
            let _ = sender.send(read_callback_line(io::stdin().lock()));
        })
        .map_err(|_| "Could not start Claude OAuth callback input.".to_string())?;
    let callback = receiver
        .await
        .map_err(|_| "Claude OAuth callback input closed.".to_string())?;
    if callback_listener_available
        && callback
            .as_ref()
            .is_err_and(|error| error == "No Claude OAuth callback received before input closed.")
    {
        // Detached or redirected stdin may be closed while a browser callback is still
        // possible. Keep that listener alive until its deadline or user cancellation.
        return futures::future::pending().await;
    }
    callback
}

fn print_prompt() -> Result<(), String> {
    eprint!("Claude OAuth callback URL or code#state (input hidden): ");
    io::stderr()
        .flush()
        .map_err(|_| "Could not display Claude OAuth callback prompt.".to_string())
}

fn read_callback_line(reader: impl BufRead) -> Result<String, String> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_CALLBACK_BYTES + 3) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|_| "Could not read Claude OAuth callback input.".to_string())?;
    if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    if bytes.ends_with(b"\r") {
        bytes.pop();
    }
    if bytes.len() > MAX_CALLBACK_BYTES {
        return Err("Claude OAuth callback exceeds 64 KiB.".to_string());
    }
    if bytes.is_empty() {
        return Err("No Claude OAuth callback received before input closed.".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "Claude OAuth callback must be valid UTF-8.".to_string())
}

struct TerminalInputGuard;

impl Drop for TerminalInputGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        eprintln!();
    }
}

async fn read_terminal_callback() -> Result<String, String> {
    terminal::enable_raw_mode()
        .map_err(|_| "Could not hide Claude OAuth callback input.".to_string())?;
    let _guard = TerminalInputGuard;
    print_prompt()?;
    let mut events = EventStream::new();
    let mut input = String::new();
    loop {
        let event = events
            .next()
            .await
            .ok_or_else(|| "Claude OAuth callback input closed.".to_string())?
            .map_err(|_| "Could not read Claude OAuth callback input.".to_string())?;
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                KeyCode::Enter => {
                    if input.is_empty() {
                        return Err("No Claude OAuth callback entered.".to_string());
                    }
                    return Ok(input);
                }
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err("Claude OAuth login cancelled.".to_string());
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Err("Claude OAuth callback input closed.".to_string());
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    input.clear();
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(character)
                    if !character.is_control()
                        && !key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if input.len() + character.len_utf8() > MAX_CALLBACK_BYTES {
                        return Err("Claude OAuth callback exceeds 64 KiB.".to_string());
                    }
                    input.push(character);
                }
                _ => {}
            },
            Event::Paste(paste) => {
                if input.len() + paste.len() > MAX_CALLBACK_BYTES {
                    return Err("Claude OAuth callback exceeds 64 KiB.".to_string());
                }
                input.push_str(&paste);
            }
            _ => {}
        }
        tokio::task::yield_now().await;
    }
}

#[cfg(test)]
#[path = "claude_oauth_login_tests.rs"]
mod tests;
