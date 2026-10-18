#![cfg(unix)]
#![allow(clippy::expect_used)]
//! Codex-held and Anthropic API credentials never reach the Claude Code process.
//!
//! This file holds a single test because it changes the test process environment.

use codex_api::ResponseEvent;
use codex_claude_code::ClaudeCodeSession;
use codex_claude_code::ClaudeCodeSessionConfig;
use codex_claude_code::ClaudeCodeTurnRequest;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn provider_secrets_are_removed_from_the_child_environment() {
    // SAFETY: this test binary contains a single test, so no other thread reads the environment.
    unsafe {
        std::env::set_var("ANTHROPIC_API_KEY", "sk-ant-test-must-not-leak");
        std::env::set_var("ANTHROPIC_AUTH_TOKEN", "auth-token-must-not-leak");
        std::env::set_var("OPENAI_API_KEY", "sk-openai-must-not-leak");
        std::env::set_var("CODEX_API_KEY", "codex-key-must-not-leak");
        std::env::set_var("CLAUDE_CODE_OAUTH_TOKEN", "subscription-token-kept");
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).expect("cwd");
    let session = ClaudeCodeSession::new(ClaudeCodeSessionConfig {
        command: codex_utils_cargo_bin::cargo_bin("codex-claude-code-fake").expect("fake"),
        bridge_command: PathBuf::from("/nonexistent/codex"),
        bridge_args: vec!["stdio-to-uds".to_string()],
        cwd: cwd.clone(),
        thread_id: "not-a-uuid-thread".to_string(),
        state_dir: dir.path().join("state"),
    });
    let mut stream = session
        .stream(ClaudeCodeTurnRequest {
            model: "claude-fake-model".to_string(),
            effort: Some("high".to_string()),
            base_instructions: String::new(),
            input: vec![ResponseItem::Message {
                encrypted_content: None,
                status: None,
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "hello".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
            tools: Vec::new(),
        })
        .await
        .expect("stream");
    let mut completed = false;
    while let Some(event) = timeout(Duration::from_secs(30), stream.rx_event.recv())
        .await
        .expect("events")
    {
        if let ResponseEvent::Completed { .. } = event.expect("event") {
            completed = true;
        }
    }
    assert!(completed);

    let record = std::fs::read_to_string(cwd.join("fake-claude-record.jsonl")).expect("record");
    let spawn: Value = record
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|record| record["event"] == json!("spawn"))
        .expect("spawn record");
    assert_eq!(
        (
            &spawn["anthropic_api_key_present"],
            &spawn["anthropic_auth_token_present"],
            &spawn["openai_api_key_present"],
            &spawn["codex_api_key_present"],
            &spawn["oauth_token_present"],
        ),
        (
            &json!(false),
            &json!(false),
            &json!(false),
            &json!(false),
            &json!(true)
        )
    );
    let argv: Vec<&str> = spawn["argv"]
        .as_array()
        .expect("argv")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let effort = argv
        .iter()
        .position(|arg| *arg == "--effort")
        .map(|index| argv[index + 1]);
    assert_eq!(effort, Some("high"));
    // A non-UUID thread id gets a random Claude Code session id.
    let session_id = argv
        .iter()
        .position(|arg| *arg == "--session-id")
        .map(|index| argv[index + 1])
        .expect("session id");
    assert!(uuid_like(session_id), "{session_id}");
    assert!(!record.contains("must-not-leak"));
}

fn uuid_like(text: &str) -> bool {
    text.len() == 36 && text.chars().filter(|ch| *ch == '-').count() == 4
}
