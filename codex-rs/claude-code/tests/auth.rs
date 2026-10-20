#![cfg(unix)]
#![allow(clippy::expect_used)]
//! `claude auth status --json` handling against the fake Claude Code CLI.

use codex_claude_code::ClaudeCodeAuthStatus;
use codex_claude_code::ClaudeCodeError;
use codex_claude_code::claude_code_auth_status;
use pretty_assertions::assert_eq;
use std::path::PathBuf;
use tempfile::TempDir;

/// Wraps the fake CLI in a script that controls what `auth status` prints.
fn fake_with_auth_output(dir: &TempDir, auth_json: &str) -> PathBuf {
    let fake = codex_utils_cargo_bin::cargo_bin("codex-claude-code-fake").expect("fake claude");
    let path = dir.path().join("claude");
    let script = format!(
        "#!/bin/sh\nFAKE_CLAUDE_AUTH_JSON='{auth_json}' exec '{}' \"$@\"\n",
        fake.display()
    );
    codex_utils_cargo_bin::write_executable(&path, &script).expect("write wrapper");
    path
}

#[tokio::test]
async fn reports_subscription_sign_in() {
    let dir = tempfile::tempdir().expect("tempdir");
    let command = fake_with_auth_output(
        &dir,
        r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty"}"#,
    );
    let status = claude_code_auth_status(&command).await.expect("status");
    assert_eq!(
        status,
        ClaudeCodeAuthStatus {
            logged_in: true,
            auth_method: Some("claude.ai".to_string()),
            api_provider: Some("firstParty".to_string()),
        }
    );
    assert!(status.uses_subscription());
}

#[tokio::test]
async fn logged_out_exit_status_is_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let command = fake_with_auth_output(
        &dir,
        r#"{"loggedIn":false,"authMethod":"none","apiProvider":"firstParty"}"#,
    );
    let status = claude_code_auth_status(&command).await.expect("status");
    assert!(!status.logged_in);
    assert!(!status.uses_subscription());
}

#[tokio::test]
async fn api_key_sign_in_is_not_a_subscription() {
    let dir = tempfile::tempdir().expect("tempdir");
    let command = fake_with_auth_output(
        &dir,
        r#"{"loggedIn":true,"authMethod":"api_key","apiProvider":"firstParty"}"#,
    );
    let status = claude_code_auth_status(&command).await.expect("status");
    assert!(status.logged_in);
    assert!(!status.uses_subscription());
}

#[tokio::test]
async fn missing_binary_reports_not_installed() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("claude-does-not-exist");
    match claude_code_auth_status(&missing).await {
        Err(ClaudeCodeError::NotInstalled { command, hint }) => {
            assert_eq!(command, missing.display().to_string());
            assert!(hint.contains("claude auth login"), "{hint}");
        }
        other => panic!("unexpected result: {other:?}"),
    }
}
