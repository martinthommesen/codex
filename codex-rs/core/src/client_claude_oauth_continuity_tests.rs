#![allow(clippy::unwrap_used)]

use super::*;
use codex_api::ApiError;
use codex_api::ResponseEvent;
use tokio::sync::oneshot;
use tokio::time::Duration;
use tokio::time::timeout;

fn context() -> OAuthRequestContext {
    OAuthRequestContext {
        account_uuid: "11111111-2222-4333-8444-555555555555".into(),
        device_id: "a".repeat(64),
        session_id: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        ..Default::default()
    }
}

fn completed(id: &str) -> ResponseEvent {
    ResponseEvent::Completed {
        response_id: id.to_owned(),
        token_usage: None,
        usage_metadata: None,
        end_turn: Some(true),
    }
}

fn api_stream() -> (
    mpsc::Sender<std::result::Result<ResponseEvent, ApiError>>,
    codex_api::ResponseStream,
) {
    let (tx_event, rx_event) = mpsc::channel(8);
    (
        tx_event,
        codex_api::ResponseStream {
            rx_event,
            upstream_request_id: Some("req_actual_upstream".into()),
            interrupt: None,
        },
    )
}

#[test]
fn completed_tool_turns_reuse_actual_ids_and_stable_prompt_identity() {
    let mut state = ClaudeOAuthContinuity::default();
    let turn = "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff";
    let (first, sequence) = state.begin(context(), Some(turn));
    assert!(first.previous_message_id.is_none());
    assert!(first.previous_request_id.is_none());
    assert_eq!(first.prompt_id.as_deref(), Some(turn));
    state.commit(sequence, "msg_actual_1", Some("req_actual_1"));
    let (tool_result, _) = state.begin(context(), Some(turn));
    assert_eq!(
        tool_result.previous_message_id.as_deref(),
        Some("msg_actual_1")
    );
    assert_eq!(
        tool_result.previous_request_id.as_deref(),
        Some("req_actual_1")
    );
    assert_eq!(tool_result.prompt_id, first.prompt_id);
    let (new_turn, _) = state.begin(context(), Some("next-codex-turn"));
    assert_ne!(new_turn.prompt_id, first.prompt_id);
    assert!(uuid::Uuid::parse_str(new_turn.prompt_id.as_deref().unwrap()).is_ok());
    assert_eq!(
        new_turn.previous_message_id,
        tool_result.previous_message_id
    );
}

#[test]
fn stale_concurrent_completions_and_changed_accounts_do_not_overwrite_continuity() {
    let mut state = ClaudeOAuthContinuity::default();
    let (_, older) = state.begin(context(), Some("turn"));
    let (_, newer) = state.begin(context(), Some("turn"));
    state.commit(newer, "msg_newer", Some("req_newer"));
    state.commit(older, "msg_older", Some("req_older"));
    assert_eq!(state.previous_message_id.as_deref(), Some("msg_newer"));
    let mut other = context();
    other.device_id = "b".repeat(64);
    let (other, _) = state.begin(other, Some("turn"));
    assert!(other.previous_message_id.is_none());
    state.commit(newer, "msg_old_account", Some("req_old_account"));
    assert!(state.previous_message_id.is_none());
    // Reauthenticating the original account/device still starts a new generation.
    state.begin(context(), Some("turn"));
    state.commit(newer, "msg_stale_same_account", None);
    assert!(state.previous_message_id.is_none());
}

#[test]
fn only_valid_completion_ids_enter_compatibility_fields() {
    let mut state = ClaudeOAuthContinuity::default();
    let (_, sequence) = state.begin(context(), None);
    for invalid in ["", "\n", "msg_id; injected=yes;"] {
        state.commit(sequence, invalid, Some("req_test"));
        assert!(state.previous_message_id.is_none());
    }
    state.commit(sequence, "msg_good", Some("req_bad; cc_fake=true;"));
    assert_eq!(state.previous_message_id.as_deref(), Some("msg_good"));
    assert!(state.previous_request_id.is_none());
}

#[tokio::test]
async fn terminal_stream_completion_commits_server_message_and_request_ids() {
    let state = ClaudeOAuthContinuityState::default();
    let (_, sequence) = state.lock().unwrap().begin(context(), Some("turn"));
    let (tx, input) = api_stream();
    let mut output = track_oauth_continuity(input, Arc::clone(&state), sequence);
    assert_eq!(
        output.upstream_request_id.as_deref(),
        Some("req_actual_upstream")
    );
    tx.send(Ok(ResponseEvent::OutputTextDelta("hello".into())))
        .await
        .unwrap();
    tx.send(Ok(completed("msg_actual_upstream"))).await.unwrap();
    assert!(matches!(
        output.next().await,
        Some(Ok(ResponseEvent::OutputTextDelta(_)))
    ));
    assert!(matches!(
        output.next().await,
        Some(Ok(ResponseEvent::Completed { .. }))
    ));
    assert!(output.next().await.is_none());
    let state = state.lock().unwrap();
    assert_eq!(
        state.previous_message_id.as_deref(),
        Some("msg_actual_upstream")
    );
    assert_eq!(
        state.previous_request_id.as_deref(),
        Some("req_actual_upstream")
    );
}

#[tokio::test]
async fn truncated_and_failed_streams_preserve_last_completed_response() {
    for failed in [false, true] {
        let state = ClaudeOAuthContinuityState::default();
        let (_, first_sequence) = state.lock().unwrap().begin(context(), Some("turn"));
        state
            .lock()
            .unwrap()
            .commit(first_sequence, "msg_before", Some("req_before"));
        let (_, sequence) = state.lock().unwrap().begin(context(), Some("turn"));
        let (tx, input) = api_stream();
        let mut output = track_oauth_continuity(input, Arc::clone(&state), sequence);
        tx.send(Ok(ResponseEvent::Created {
            response_id: Some("msg_incomplete".into()),
        }))
        .await
        .unwrap();
        if failed {
            tx.send(Err(ApiError::Stream("upstream interrupted".into())))
                .await
                .unwrap();
        }
        drop(tx);
        while output.next().await.is_some() {}
        assert_eq!(
            state.lock().unwrap().previous_message_id.as_deref(),
            Some("msg_before")
        );
    }
}

#[tokio::test]
async fn consumer_cancellation_closes_upstream_without_advancing_state() {
    let state = ClaudeOAuthContinuityState::default();
    let (_, sequence) = state.lock().unwrap().begin(context(), Some("turn"));
    let (tx, input) = api_stream();
    let mut output = track_oauth_continuity(input, Arc::clone(&state), sequence);
    tx.send(Ok(ResponseEvent::OutputTextDelta("partial".into())))
        .await
        .unwrap();
    assert!(output.next().await.is_some());
    drop(output);
    timeout(Duration::from_secs(2), tx.closed()).await.unwrap();
    assert!(state.lock().unwrap().previous_message_id.is_none());
}

#[tokio::test]
async fn original_interrupt_channel_is_preserved() {
    let state = ClaudeOAuthContinuityState::default();
    let (_, sequence) = state.lock().unwrap().begin(context(), Some("turn"));
    let (_, mut input) = api_stream();
    let (interrupt, receiver) = oneshot::channel();
    input.interrupt = Some(interrupt);
    let mut output = track_oauth_continuity(input, state, sequence);
    output
        .interrupt
        .take()
        .unwrap()
        .send(codex_api::ResponseInterrupt::Drain)
        .ok()
        .unwrap();
    assert!(matches!(
        receiver.await.unwrap(),
        codex_api::ResponseInterrupt::Drain
    ));
}
