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
