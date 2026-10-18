use super::*;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::login_with_api_key;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

async fn file_store_config(codex_home: &TempDir) -> Config {
    let mut config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        Vec::new(),
    )
    .await
    .expect("load config");
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
    config
}

#[tokio::test]
async fn anthropic_login_and_logout_leave_chatgpt_auth_untouched() {
    let codex_home = TempDir::new().expect("tempdir");
    let config = file_store_config(&codex_home).await;
    login_with_api_key(
        codex_home.path(),
        "sk-openai-test-key-1234567890",
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::Direct,
    )
    .expect("store OpenAI key");
    let auth_json = codex_home.path().join("auth.json");
    let before = std::fs::read(&auth_json).expect("auth.json written");

    let message = login_anthropic_with_api_key(&config, "sk-ant-api03-test-key-abcdefghij")
        .expect("store Anthropic key");
    assert!(
        message.contains("ChatGPT/OpenAI login is unchanged"),
        "{message}"
    );
    assert!(!message.contains("test-key-abcdefghij"), "{message}");
    assert_eq!(std::fs::read(&auth_json).expect("auth.json"), before);
    let (connected, status) = anthropic_status(&config);
    assert!(connected, "{status}");
    assert!(status.contains("stored by Codex"), "{status}");
    assert!(!status.contains("test-key-abcdefghij"), "{status}");

    let message = logout_anthropic(&config).expect("remove Anthropic key");
    assert!(
        message.contains("Removed the stored Anthropic API key"),
        "{message}"
    );
    assert_eq!(std::fs::read(&auth_json).expect("auth.json"), before);
    assert!(!anthropic_status(&config).0);
}

#[tokio::test]
async fn claude_code_logout_only_removes_the_codex_marker() {
    let codex_home = TempDir::new().expect("tempdir");
    let config = file_store_config(&codex_home).await;
    let marker = marker_path(codex_home.path());
    std::fs::create_dir_all(marker.parent().expect("parent")).expect("mkdir");
    std::fs::write(&marker, "{}").expect("marker");

    let message = logout_claude_code(&config).expect("disconnect");
    assert!(
        message.starts_with("Disconnected Claude Code from Codex."),
        "{message}"
    );
    assert!(message.contains("claude auth logout"), "{message}");
    assert!(!marker.exists());

    let message = logout_claude_code(&config).expect("idempotent disconnect");
    assert!(
        message.starts_with("Claude Code was not connected"),
        "{message}"
    );
}

#[test]
fn claude_code_status_descriptions() {
    let subscription = ClaudeCodeAuthStatus {
        logged_in: true,
        auth_method: Some("claude.ai".to_string()),
        api_provider: Some("firstParty".to_string()),
    };
    let (connected, message) = describe_claude_code_status(/*connected*/ true, &subscription);
    assert!(connected);
    assert!(message.starts_with("Claude Code: connected"), "{message}");

    let (connected, message) = describe_claude_code_status(/*connected*/ false, &subscription);
    assert!(!connected);
    assert!(
        message.contains("codex login --provider claude-code"),
        "{message}"
    );

    let signed_out = ClaudeCodeAuthStatus {
        logged_in: false,
        auth_method: None,
        api_provider: None,
    };
    let (connected, message) = describe_claude_code_status(/*connected*/ true, &signed_out);
    assert!(!connected);
    assert!(message.contains("claude auth login"), "{message}");
}
