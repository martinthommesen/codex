use super::*;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AnthropicAuthConfig;
use codex_login::AuthKeyringBackendKind;
use codex_login::login_with_api_key;
use futures::FutureExt;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn unavailable_manual_input_does_not_cancel_a_browser_callback() {
    for error in [
        "No Claude OAuth callback entered.",
        "Claude OAuth callback input closed.",
        "Could not hide Claude OAuth callback input.",
        "Could not read Claude OAuth callback input.",
    ] {
        assert!(
            callback_or_wait_for_browser(Err(error.to_string()), true)
                .now_or_never()
                .is_none()
        );
        assert_eq!(
            callback_or_wait_for_browser(Err(error.to_string()), false).now_or_never(),
            Some(Err(error.to_string()))
        );
    }
    assert_eq!(
        callback_or_wait_for_browser(Err(LOGIN_CANCELLED.to_string()), true).now_or_never(),
        Some(Err(LOGIN_CANCELLED.to_string()))
    );
    assert_eq!(
        callback_or_wait_for_browser(Ok("code#state".to_string()), true).now_or_never(),
        Some(Ok("code#state".to_string()))
    );
}

#[test]
fn oauth_status_explains_unusable_credentials_and_refresh_backoff() {
    let mut value = serde_json::json!({
        "version": 1, "access_token": "access-secret", "refresh_token": "refresh-secret",
        "expires_at": u64::MAX, "account_uuid": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        "organization_uuid": "", "email": "example@example.com", "device_id": "ab".repeat(32),
        "refresh_requires_login": true,
    });
    let credential = serde_json::from_value(value.clone()).expect("credential");
    let (connected, message) = describe_status(&credential);
    assert!(!connected);
    assert!(message.contains("codex login --provider claude-oauth"));
    assert!(!message.contains("secret"));
    value["refresh_requires_login"] = serde_json::json!(false);
    value["refresh_blocked_until"] = serde_json::json!(u64::MAX);
    let (connected, message) = describe_status(&serde_json::from_value(value).expect("credential"));
    assert!(connected, "a temporary backoff keeps the configured login");
    assert!(message.contains("refresh rate limited"));
}

#[test]
fn manual_input_accepts_callback_url_and_code_without_echoing_or_rewriting() {
    for input in [
        "http://localhost:54545/callback?code=secret-code&state=csrf-state",
        "secret-code#csrf-state",
    ] {
        let line = format!("{input}\r\n");
        assert_eq!(
            read_callback_line(io::Cursor::new(line)).expect("callback"),
            input
        );
    }
}

#[test]
fn callback_input_is_bounded_and_error_messages_never_include_secret_input() {
    let secret = "secret-authorization-code";
    let oversized = secret.repeat(MAX_CALLBACK_BYTES / secret.len() + 1);
    let error = read_callback_line(io::Cursor::new(oversized)).expect_err("input bound");
    assert!(error.contains("64 KiB"));
    assert!(!error.contains(secret));
    assert!(read_callback_line(io::Cursor::new(Vec::new())).is_err());
    let error =
        read_callback_line(io::Cursor::new(b"secret-code\xff")).expect_err("UTF-8 validation");
    assert!(!error.contains("secret-code"));
}

#[test]
fn experimental_warning_does_not_guarantee_quota_or_permission() {
    assert!(EXPERIMENTAL_WARNING.contains("unofficial"));
    assert!(EXPERIMENTAL_WARNING.contains("may violate Anthropic"));
    assert!(EXPERIMENTAL_WARNING.contains("subscription-quota treatment is unverified"));
    assert!(EXPERIMENTAL_WARNING.contains("disable separately billed extra usage"));
    assert!(EXPERIMENTAL_WARNING.contains("never falls back"));
}

#[tokio::test]
async fn oauth_status_and_logout_never_use_or_remove_other_provider_credentials() {
    let home = TempDir::new().expect("temporary Codex home");
    let mut config = Config::load_default_with_cli_overrides_for_codex_home(
        home.path().to_path_buf(),
        Vec::new(),
    )
    .await
    .expect("config");
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
    login_with_api_key(
        home.path(),
        "sk-openai-isolated-secret",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("save OpenAI credential");
    AnthropicAuthConfig::new(
        home.path().to_path_buf(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .save_api_key("sk-ant-isolated-secret")
    .expect("save Anthropic credential");
    let openai_auth_path = home.path().join("auth.json");
    let before = std::fs::read(&openai_auth_path).expect("OpenAI auth");
    let marker = home
        .path()
        .join(codex_core::provider_models::CLAUDE_CODE_CONNECTED_MARKER);
    std::fs::create_dir_all(marker.parent().expect("Claude Code marker directory"))
        .expect("create Claude Code marker directory");
    std::fs::write(&marker, "{}").expect("Claude Code marker");
    let marker_before = std::fs::read(&marker).expect("Claude Code marker contents");
    let anthropic_auth_path = home.path().join("anthropic_auth.json");
    let anthropic_before = std::fs::read(&anthropic_auth_path).expect("Anthropic auth");

    let (connected, message) = status(&config);
    assert!(!connected, "{message}");
    assert!(message.contains("not configured"), "{message}");
    assert!(!message.contains("isolated-secret"), "{message}");
    let message = logout(&config).expect("idempotent OAuth logout");
    assert!(message.contains("No Claude OAuth credentials"), "{message}");
    assert_eq!(
        std::fs::read(&openai_auth_path).expect("OpenAI auth"),
        before
    );
    assert!(marker.exists());
    assert_eq!(
        std::fs::read(&marker).expect("Claude Code marker contents"),
        marker_before
    );
    assert_eq!(
        std::fs::read(&anthropic_auth_path).expect("Anthropic auth"),
        anthropic_before
    );
    assert!(crate::claude_login::anthropic_status(&config).0);
}
