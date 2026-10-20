#![cfg(unix)]
#![allow(clippy::expect_used)]
//! Live validation against the real Claude Code CLI; ignored by default.
//!
//! Run with a signed-in `claude` on `PATH` (or `CODEX_CLAUDE_CODE_PATH`) and the stdio relay:
//!
//! ```text
//! cargo build -p codex-stdio-to-uds
//! CODEX_CLAUDE_CODE_LIVE_BRIDGE=target/debug/codex-stdio-to-uds \
//!     cargo test -p codex-claude-code --test live -- --ignored
//! ```
//!
//! This uses the current user's Claude Code sign-in and spends a small amount of usage.

use codex_api::ResponseEvent;
use codex_claude_code::ClaudeCodeSession;
use codex_claude_code::ClaudeCodeSessionConfig;
use codex_claude_code::ClaudeCodeTurnRequest;
use codex_claude_code::claude_code_auth_status;
use codex_claude_code::resolve_claude_command;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

const BRIDGE_ENV: &str = "CODEX_CLAUDE_CODE_LIVE_BRIDGE";
const MODEL_ENV: &str = "CODEX_CLAUDE_CODE_LIVE_MODEL";

async fn collect(session: &ClaudeCodeSession, input: &[ResponseItem]) -> Vec<ResponseEvent> {
    let mut stream = session
        .stream(ClaudeCodeTurnRequest {
            model: std::env::var(MODEL_ENV).unwrap_or_else(|_| "haiku".to_string()),
            effort: None,
            base_instructions: "You are a test agent. Follow instructions exactly.".to_string(),
            input: input.to_vec(),
            tools: vec![json!({
                "type": "function",
                "name": "get_secret_word",
                "description": "Returns the secret word.",
                "parameters": {"type": "object", "properties": {}},
            })],
        })
        .await
        .expect("stream");
    let mut events = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(180), stream.rx_event.recv())
        .await
        .expect("timely event")
    {
        events.push(event.expect("event"));
    }
    events
}

fn outputs(events: &[ResponseEvent]) -> Vec<ResponseItem> {
    events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a signed-in Claude Code CLI, network access, and CODEX_CLAUDE_CODE_LIVE_BRIDGE"]
async fn real_claude_code_round_trips_a_codex_tool_call() {
    let Some(bridge) = std::env::var_os(BRIDGE_ENV) else {
        eprintln!("skipping: set {BRIDGE_ENV} to the codex-stdio-to-uds binary");
        return;
    };
    let command = resolve_claude_command(None);
    let status = claude_code_auth_status(&command)
        .await
        .expect("auth status");
    assert!(status.logged_in, "{}", status.describe());
    eprintln!("Claude Code: {}", status.describe());

    let dir = tempfile::tempdir().expect("tempdir");
    let session = ClaudeCodeSession::new(ClaudeCodeSessionConfig {
        command,
        bridge_command: PathBuf::from(bridge),
        bridge_args: Vec::new(),
        cwd: dir.path().to_path_buf(),
        thread_id: uuid::Uuid::new_v4().to_string(),
        state_dir: dir.path().join("state"),
    });
    let mut input = vec![ResponseItem::Message {
        encrypted_content: None,
        status: None,
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "Call the get_secret_word tool, then reply with only the word it returned."
                .to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }];

    let first = collect(&session, &input).await;
    let call_id = outputs(&first)
        .into_iter()
        .find_map(|item| match item {
            ResponseItem::FunctionCall { name, call_id, .. } if name == "get_secret_word" => {
                Some(call_id)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no tool call in {first:?}"));
    assert!(matches!(
        first.last(),
        Some(ResponseEvent::Completed { end_turn: None, .. })
    ));

    input.extend(outputs(&first));
    input.push(ResponseItem::FunctionCallOutput {
        id: None,
        call_id: Some(call_id),
        name: None,
        namespace: None,
        output: FunctionCallOutputPayload::from_text("pineapple".to_string()),
        internal_chat_message_metadata_passthrough: None,
    });
    let second = collect(&session, &input).await;
    let text: String = outputs(&second)
        .into_iter()
        .filter_map(|item| match item {
            ResponseItem::Message { role, content, .. } if role == "assistant" => Some(
                content
                    .into_iter()
                    .filter_map(|content| match content {
                        ContentItem::OutputText { text, .. } => Some(text),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            _ => None,
        })
        .collect();
    assert!(text.to_lowercase().contains("pineapple"), "{second:?}");
    assert!(matches!(
        second.last(),
        Some(ResponseEvent::Completed {
            end_turn: Some(true),
            ..
        })
    ));
}
