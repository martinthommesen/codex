use super::check_bridge_status;
use super::result_error;
use codex_api::ApiError;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn bridge_status_must_be_connected() {
    assert!(
        check_bridge_status(&json!({"mcp_servers": [{"name": "codex", "status": "connected"}]}))
            .is_ok()
    );
    for init in [
        json!({"mcp_servers": [{"name": "codex", "status": "failed"}]}),
        json!({"mcp_servers": [{"name": "other", "status": "connected"}]}),
        json!({}),
    ] {
        match check_bridge_status(&init) {
            Err(ApiError::InvalidRequest { message }) => {
                assert!(message.contains("Codex tool bridge"), "{message}");
            }
            other => panic!("unexpected result for {init}: {other:?}"),
        }
    }
}

#[test]
fn result_errors_map_to_actionable_api_errors() {
    let classify = |value| match result_error(&value) {
        ApiError::ContextWindowExceeded => "context".to_string(),
        ApiError::InvalidRequest { message } => format!("invalid: {message}"),
        ApiError::Stream(message) => format!("stream: {message}"),
        other => format!("other: {other}"),
    };
    assert_eq!(
        classify(json!({"subtype": "success", "is_error": true, "result": "Prompt is too long"})),
        "context"
    );
    assert_eq!(
        classify(json!({"is_error": true, "result": "Not logged in · Please run /login"})),
        "invalid: Claude Code: Not logged in · Please run /login (sign in with `claude auth login`, \
then run `codex login --provider claude-code`)"
    );
    assert_eq!(
        classify(json!({"is_error": true, "result": "Claude usage limit reached"})),
        "invalid: Claude Code: Claude usage limit reached"
    );
    assert_eq!(
        classify(
            json!({"subtype": "error_during_execution", "is_error": true, "errors": ["boom"]})
        ),
        "stream: Claude Code reported an error: boom"
    );
    assert_eq!(
        classify(json!({"subtype": "error_max_turns", "is_error": true})),
        "stream: Claude Code reported an error: Claude Code ended the turn with `error_max_turns`"
    );
}

/// Input that never reached Claude Code must stay unsynced, so the next request replays it.
mod undelivered_input {
    use super::super::Payload;
    use super::super::SessionInner;
    use super::super::deliver_input;
    use crate::bridge::BridgeCore;
    use crate::history::prefix_hash;
    use crate::process::ClaudeProcess;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;

    fn user_message(text: &str) -> ResponseItem {
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: text.to_string(),
            }],
            phase: None,
            status: None,
            encrypted_content: None,
            internal_chat_message_metadata_passthrough: None,
        }
    }

    /// Delivers a follow-up question to `process` and checks the cursor stayed put. An
    /// incremental request starts after the first question; a fresh one starts from a reset.
    async fn assert_stays_unsynced(process: Option<ClaudeProcess>, payload: Payload) {
        let synced = match payload {
            Payload::Incremental => vec![user_message("first question")],
            Payload::Fresh => Vec::new(),
        };
        let input = vec![
            user_message("first question"),
            user_message("second question"),
        ];
        let mut inner = SessionInner {
            synced_len: synced.len(),
            prefix_hash: prefix_hash(&synced),
            process,
            ..SessionInner::default()
        };

        deliver_input(
            &mut inner,
            &BridgeCore::default(),
            &input,
            /*leading_len*/ 0,
            payload,
        )
        .await;

        assert_eq!(inner.synced_len, synced.len());
        assert_eq!(inner.prefix_hash, prefix_hash(&synced));
        assert!(!inner.query_in_flight);
    }

    #[tokio::test]
    async fn input_without_a_process_is_not_marked_synced() {
        for payload in [Payload::Incremental, Payload::Fresh] {
            assert_stays_unsynced(/*process*/ None, payload).await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn input_that_fails_to_reach_claude_code_is_not_marked_synced() {
        use crate::process::ProcessKey;
        use crate::process::SessionArg;
        use crate::process::SpawnSpec;
        use std::path::Path;
        use std::time::Duration;
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("tempdir");
        let prompt = dir.path().join("prompt.md");
        std::fs::write(&prompt, "").expect("prompt file");
        for payload in [Payload::Incremental, Payload::Fresh] {
            // A process that already exited, so writing to its stdin fails.
            let spec = SpawnSpec {
                command: Path::new("true"),
                cwd: dir.path(),
                model: "claude-fake-model",
                effort: None,
                system_prompt_file: &prompt,
                mcp_config: "{}",
                session: SessionArg::New(uuid::Uuid::new_v4()),
            };
            let key = ProcessKey {
                model: "claude-fake-model".to_string(),
                effort: None,
                system_prompt_hash: String::new(),
            };
            let mut process = ClaudeProcess::spawn(&spec, key).expect("spawn `true`");
            let deadline = Instant::now() + Duration::from_secs(10);
            while !process.is_closed() {
                assert!(Instant::now() < deadline, "`true` did not exit");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_stays_unsynced(Some(process), payload).await;
        }
    }
}
