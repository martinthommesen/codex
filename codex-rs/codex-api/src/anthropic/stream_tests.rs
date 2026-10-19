use super::*;
use crate::anthropic::tools::translate_tools;
use bytes::Bytes;
use codex_protocol::models::MessagePhase;
use futures::stream;
use pretty_assertions::assert_eq;
use serde_json::json;

fn event(value: Value) -> AnthropicStreamEvent {
    serde_json::from_value(value).expect("valid event")
}

fn tool_names() -> AnthropicToolNameMap {
    translate_tools(&[
        json!({"type": "function", "name": "shell", "description": "", "strict": false, "parameters": {"type": "object", "properties": {}}}),
        json!({"type": "custom", "name": "apply_patch", "description": "", "format": {"type": "grammar", "syntax": "lark", "definition": "x"}}),
        json!({"type": "namespace", "name": "mcp__fs__", "description": "", "tools": [
            {"type": "function", "name": "read", "description": "", "strict": false, "parameters": {"type": "object", "properties": {}}}
        ]}),
    ])
    .names
}

fn run(events: Vec<Value>) -> Result<Vec<ResponseEvent>, ApiError> {
    let mut translator = AnthropicStreamTranslator::new(tool_names());
    let mut out = Vec::new();
    for value in events {
        out.extend(translator.handle(event(value))?);
    }
    Ok(out)
}

fn message_start() -> Value {
    json!({"type": "message_start", "message": {"id": "msg_1", "model": "claude-opus-5-5", "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 5, "output_tokens": 1}}})
}

#[test]
fn streams_thinking_text_and_tool_calls() {
    let events = run(vec![
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Need ls."}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Listing"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_1", "name": "shell", "input": {}}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
        json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "[\"ls\"]}"}}),
        json!({"type": "content_block_stop", "index": 2}),
        json!({"type": "content_block_start", "index": 3, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "mcp__fs__read", "input": {}}}),
        json!({"type": "content_block_stop", "index": 3}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 42}}),
        json!({"type": "message_stop"}),
    ])
    .expect("translation succeeds");

    let mut iter = events.into_iter();
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::Created { response_id: Some(id) }) if id == "msg_1"
    ));
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::ServerReasoningIncluded(true))
    ));
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::OutputItemAdded(
            ResponseItem::Reasoning { .. }
        ))
    ));
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::ReasoningSummaryDelta { delta, summary_index: 0 }) if delta == "Need ls."
    ));
    match iter.next() {
        Some(ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
            summary,
            encrypted_content,
            ..
        })) => {
            assert_eq!(
                summary,
                vec![ReasoningItemReasoningSummary::SummaryText {
                    text: "Need ls.".to_string()
                }]
            );
            assert_eq!(
                encrypted_content.as_deref(),
                Some("anthropic.thinking.v1:sig")
            );
        }
        other => panic!("unexpected event {other:?}"),
    }
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::OutputItemAdded(ResponseItem::Message { .. }))
    ));
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::OutputTextDelta(delta)) if delta == "Listing"
    ));
    assert!(matches!(
        iter.next(),
        Some(ResponseEvent::OutputItemDone(ResponseItem::Message {
            phase: Some(MessagePhase::Commentary),
            ..
        }))
    ));
    match iter.next() {
        Some(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            call_id,
            encrypted_function_args,
            ..
        })) => {
            assert_eq!(name, "shell");
            assert_eq!(namespace, None);
            assert_eq!(arguments, r#"{"command":["ls"]}"#);
            assert_eq!(call_id, "toolu_1");
            assert_eq!(encrypted_function_args, Some(Vec::new()));
        }
        other => panic!("unexpected event {other:?}"),
    }
    match iter.next() {
        Some(ResponseEvent::OutputItemDone(ResponseItem::FunctionCall {
            name,
            namespace,
            arguments,
            ..
        })) => {
            assert_eq!(name, "read");
            assert_eq!(namespace.as_deref(), Some("mcp__fs__"));
            assert_eq!(arguments, "{}");
        }
        other => panic!("unexpected event {other:?}"),
    }
    match iter.next() {
        Some(ResponseEvent::Completed {
            response_id,
            token_usage,
            end_turn,
            ..
        }) => {
            assert_eq!(response_id, "msg_1");
            assert_eq!(end_turn, None);
            assert_eq!(
                token_usage,
                Some(TokenUsage {
                    input_tokens: 105,
                    cached_input_tokens: 90,
                    cache_write_input_tokens: 5,
                    output_tokens: 42,
                    reasoning_output_tokens: 0,
                    total_tokens: 147,
                    codex_rollout_budget_units: None,
                })
            );
        }
        other => panic!("unexpected event {other:?}"),
    }
    assert!(iter.next().is_none());
}

#[test]
fn freeform_tool_calls_are_restored() {
    let events = run(vec![
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_p", "name": "apply_patch", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"input\": \"*** Begin Patch\\n*** End Patch\"}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}),
        json!({"type": "message_stop"}),
    ])
    .expect("translation succeeds");
    let item = events
        .iter()
        .find_map(|event| match event {
            ResponseEvent::OutputItemDone(item) => Some(item.clone()),
            _ => None,
        })
        .expect("tool call item");
    assert_eq!(
        item,
        ResponseItem::CustomToolCall {
            encrypted_content: None,
            id: None,
            status: None,
            call_id: "toolu_p".to_string(),
            name: "apply_patch".to_string(),
            namespace: None,
            input: "*** Begin Patch\n*** End Patch".to_string(),
            internal_chat_message_metadata_passthrough: None,
        }
    );
}

#[test]
fn end_turn_and_redacted_thinking() {
    let events = run(vec![
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "redacted_thinking", "data": "opaque"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": "Done."}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        json!({"type": "message_stop"}),
    ])
    .expect("translation succeeds");
    assert!(events.iter().any(|event| matches!(
        event,
        ResponseEvent::OutputItemDone(ResponseItem::Reasoning { encrypted_content: Some(content), .. })
            if content == "anthropic.redacted_thinking.v1:opaque"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponseEvent::OutputTextDelta(delta) if delta == "Done."
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponseEvent::OutputItemDone(ResponseItem::Message {
            phase: Some(MessagePhase::FinalAnswer),
            ..
        })
    )));
    assert!(matches!(
        events.last(),
        Some(ResponseEvent::Completed {
            end_turn: Some(true),
            ..
        })
    ));
}

#[test]
fn stop_reasons_map_to_errors() {
    let refusal = run(vec![
        message_start(),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber", "explanation": "no"}}}),
        json!({"type": "message_stop"}),
    ]);
    assert!(matches!(refusal, Err(ApiError::CyberPolicy { .. })));

    let window = run(vec![
        message_start(),
        json!({"type": "message_delta", "delta": {"stop_reason": "model_context_window_exceeded"}}),
        json!({"type": "message_stop"}),
    ]);
    assert!(matches!(window, Err(ApiError::ContextWindowExceeded)));

    let truncated = run(vec![
        message_start(),
        json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
        json!({"type": "message_stop"}),
    ]);
    assert!(matches!(truncated, Err(ApiError::ResponseIncomplete(_))));

    let overloaded = run(vec![
        message_start(),
        json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
    ]);
    assert!(matches!(overloaded, Err(ApiError::Retryable { .. })));
}

#[test]
fn truncated_tool_input_is_dropped_and_a_truncated_answer_is_incomplete_at_max_tokens() {
    // The API closes every block before `message_delta` reports the stop reason.
    for partial_json in ["{\"command\": [\"l", ""] {
        let mut translator = AnthropicStreamTranslator::new(tool_names());
        let mut events = Vec::new();
        for value in [
            message_start(),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": "partial"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_x", "name": "shell", "input": {}}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": partial_json}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
        ] {
            events.extend(translator.handle(event(value)).expect("event"));
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. })
            )),
            "{partial_json:?}"
        );
        // The partial text is kept, but the turn does not end as if it were complete.
        assert!(events.iter().any(|event| matches!(
            event,
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
        )));
        assert!(matches!(
            translator.handle(event(json!({"type": "message_stop"}))),
            Err(ApiError::ResponseIncomplete(_))
        ));
    }
}

#[test]
fn complete_tool_calls_still_run_after_a_max_tokens_stop() {
    let events = run(vec![
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_a", "name": "shell", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": [\"ls\"]}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_b", "name": "shell", "input": {}}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"comm"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}}),
        json!({"type": "message_stop"}),
    ])
    .expect("complete tool calls continue the turn");
    let call_ids = events
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, .. }) => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids, vec!["toolu_a"]);
    assert!(matches!(
        events.last(),
        Some(ResponseEvent::Completed { end_turn: None, .. })
    ));
}

#[test]
fn tool_calls_wait_for_the_stop_reason_and_keep_model_order() {
    let mut translator = AnthropicStreamTranslator::new(tool_names());
    let mut feed = |value: Value| translator.handle(event(value)).expect("event");
    feed(message_start());
    feed(
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_a", "name": "shell", "input": {}}}),
    );
    assert!(feed(json!({"type": "content_block_stop", "index": 0})).is_empty());
    feed(
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "toolu_b", "name": "shell", "input": {}}}),
    );
    feed(
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": [\"ls\"]}"}}),
    );
    assert!(feed(json!({"type": "content_block_stop", "index": 1})).is_empty());
    let released = feed(json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}}));
    let call_ids = released
        .iter()
        .filter_map(|event| match event {
            ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { call_id, .. }) => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(call_ids, vec!["toolu_a", "toolu_b"]);
}

#[test]
fn refused_responses_run_no_tool_calls_and_record_no_answer() {
    let mut translator = AnthropicStreamTranslator::new(tool_names());
    let mut events = Vec::new();
    for value in [
        message_start(),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_a", "name": "shell", "input": {}}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\": [\"ls\"]}"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "partial"}}),
        json!({"type": "content_block_stop", "index": 1}),
        json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"category": "cyber"}}}),
    ] {
        events.extend(translator.handle(event(value)).expect("event"));
    }
    assert!(!events.iter().any(|event| matches!(
        event,
        ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. })
            | ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
    )));
    assert!(matches!(
        translator.handle(event(json!({"type": "message_stop"}))),
        Err(ApiError::CyberPolicy { .. })
    ));
}

#[test]
fn input_transformations_from_message_start_are_kept() {
    let dropped = json!({"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"});
    let mut translator = AnthropicStreamTranslator::new(tool_names());
    translator
        .handle(event(json!({
            "type": "message_start",
            "message": {"id": "msg_1", "input_transformations": [dropped]}
        })))
        .expect("message_start");
    assert_eq!(translator.input_transformations(), [dropped].as_slice());
}

#[tokio::test]
async fn spawned_stream_reads_sse_bytes() {
    let body = [
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\",\"usage\":{\"input_tokens\":3,\"output_tokens\":1}}}\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
    ]
    .concat();
    let bytes: ByteStream = Box::pin(stream::iter(vec![Ok(Bytes::from(body))]));
    let mut headers = http::HeaderMap::new();
    headers.insert("request-id", http::HeaderValue::from_static("req_123"));
    let response = StreamResponse {
        status: http::StatusCode::OK,
        headers,
        bytes,
    };
    let mut stream = spawn_anthropic_stream(
        response,
        Duration::from_secs(5),
        AnthropicToolNameMap::default(),
        /*telemetry*/ None,
    );
    assert_eq!(stream.upstream_request_id.as_deref(), Some("req_123"));
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.expect("no stream error"));
    }
    assert!(matches!(
        events.last(),
        Some(ResponseEvent::Completed {
            end_turn: Some(true),
            ..
        })
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        ResponseEvent::OutputTextDelta(delta) if delta == "hi"
    )));
}

#[tokio::test]
async fn spawned_stream_reports_early_close() {
    let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_9\"}}\n\n";
    let bytes: ByteStream = Box::pin(stream::iter(vec![Ok(Bytes::from(body))]));
    let response = StreamResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        bytes,
    };
    let mut stream = spawn_anthropic_stream(
        response,
        Duration::from_secs(5),
        AnthropicToolNameMap::default(),
        /*telemetry*/ None,
    );
    let mut last = None;
    while let Some(event) = stream.next().await {
        last = Some(event);
    }
    assert!(matches!(last, Some(Err(ApiError::Stream(_)))));
}

#[derive(Default)]
struct RecordingSseTelemetry {
    polls: std::sync::Mutex<Vec<String>>,
}

impl SseTelemetry for RecordingSseTelemetry {
    fn on_sse_poll(&self, result: &SsePollResult, _duration: Duration) {
        self.polls
            .lock()
            .expect("telemetry lock")
            .push(format!("{result:?}"));
    }
}

fn oauth_stream(bytes: ByteStream, telemetry: Option<Arc<dyn SseTelemetry>>) -> ResponseStream {
    spawn_claude_oauth_stream(
        StreamResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            bytes,
        },
        Duration::from_secs(5),
        AnthropicToolNameMap::default(),
        telemetry,
        Vec::new(),
    )
}

#[tokio::test]
async fn oauth_stream_transport_errors_are_redacted_before_telemetry_and_user_output() {
    let telemetry = Arc::new(RecordingSseTelemetry::default());
    let bytes: ByteStream = Box::pin(stream::iter(vec![Err(
        codex_client::TransportError::Network(
            "Authorization: Bearer secret-access-token; code=secret-authorization-code".into(),
        ),
    )]));
    let mut response = oauth_stream(bytes, Some(telemetry.clone()));
    let error = response
        .next()
        .await
        .expect("stream error")
        .expect_err("transport failed");
    assert!(!error.to_string().contains("secret-"));
    assert!(!format!("{error:?}").contains("secret-"));
    let polls = telemetry.polls.lock().expect("telemetry lock");
    assert_eq!(polls.len(), 1);
    assert!(!polls[0].contains("secret-"));
    assert!(polls[0].contains("Err"));
}

#[test]
fn oauth_telemetry_never_receives_raw_event_names_ids_or_payloads() {
    let telemetry = RecordingSseTelemetry::default();
    for event_name in ["response.failed", "secret-access-token", "message_start"] {
        let poll = Ok(Some(Ok(eventsource_stream::Event {
            event: event_name.to_string(),
            data: "secret-authorization-code".to_string(),
            id: "secret-refresh-token".to_string(),
            retry: None,
        })));
        record_oauth_sse_poll(&telemetry, &poll, Duration::from_millis(1));
    }
    let polls = telemetry.polls.lock().expect("telemetry lock");
    assert_eq!(polls.len(), 3);
    assert!(polls.iter().all(|poll| !poll.contains("secret-")));
    assert!(polls[0].contains("unknown"));
    assert!(polls[2].contains("message_start"));
}

#[tokio::test]
async fn oauth_telemetry_redaction_preserves_streamed_model_text() {
    let body = [
        json!({"type":"message_start","message":{"id":"msg_1"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"model output stays intact"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
        json!({"type":"message_stop"}),
    ]
    .into_iter()
    .map(|event| format!("data: {event}\n\n"))
    .collect::<String>();
    let telemetry = Arc::new(RecordingSseTelemetry::default());
    let bytes: ByteStream = Box::pin(stream::iter(vec![Ok(Bytes::from(body))]));
    let mut response = oauth_stream(bytes, Some(telemetry.clone()));
    let mut text = String::new();
    let mut completed = false;
    while let Some(event) = response.next().await {
        match event.expect("valid response") {
            ResponseEvent::OutputTextDelta(delta) => text.push_str(&delta),
            ResponseEvent::Completed { .. } => completed = true,
            _ => {}
        }
    }
    assert_eq!(text, "model output stays intact");
    assert!(completed);
    assert!(
        telemetry
            .polls
            .lock()
            .expect("telemetry lock")
            .iter()
            .all(|poll| !poll.contains(&text))
    );
}

#[tokio::test]
async fn oauth_stream_redaction_preserves_thinking_context_and_rate_limit_errors() {
    for (kind, message) in [
        (
            "invalid_request_error",
            "Invalid thinking block signature: secret-access-token",
        ),
        (
            "invalid_request_error",
            "prompt is too long: secret-access-token",
        ),
        ("rate_limit_error", "secret-access-token"),
        ("billing_error", "secret-access-token"),
        ("secret-kind", "secret-access-token"),
    ] {
        let body = format!(
            "event: error\ndata: {}\n\n",
            json!({"type":"error","error":{"type":kind,"message":message}})
        );
        let telemetry = Arc::new(RecordingSseTelemetry::default());
        let bytes: ByteStream = Box::pin(stream::iter(vec![Ok(Bytes::from(body))]));
        let mut response = oauth_stream(bytes, Some(telemetry.clone()));
        let error = response
            .next()
            .await
            .expect("stream error")
            .expect_err("upstream error");
        assert!(!error.to_string().contains("secret-"));
        assert!(!format!("{error:?}").contains("secret-"));
        if message.starts_with("Invalid thinking") {
            assert!(crate::anthropic::is_thinking_replay_rejection(&error));
        } else if message.starts_with("prompt is too long") {
            assert!(matches!(error, ApiError::ContextWindowExceeded));
        } else if kind == "rate_limit_error" {
            assert!(matches!(error, ApiError::RateLimitExceeded { .. }));
        } else if kind == "billing_error" {
            assert!(error.to_string().contains("billing or entitlement"));
        }
        assert!(
            telemetry
                .polls
                .lock()
                .expect("telemetry lock")
                .iter()
                .all(|poll| !poll.contains("secret-"))
        );
    }
}

#[tokio::test]
async fn oauth_stream_redaction_keeps_network_policy_errors_structured() {
    let bytes: ByteStream = Box::pin(stream::iter(vec![Err(
        codex_client::TransportError::Policy(codex_http_client::NetworkPolicyDenied::Revoked),
    )]));
    let mut response = oauth_stream(bytes, None);
    assert!(matches!(
        response.next().await,
        Some(Err(ApiError::Transport(
            codex_client::TransportError::Policy(codex_http_client::NetworkPolicyDenied::Revoked)
        )))
    ));
}

#[tokio::test]
async fn oauth_refusal_diagnostics_withhold_opaque_explanations() {
    for category in ["bio", "cyber", "secret-category"] {
        let body = [
            json!({"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":category,"explanation":"secret-access-token"}}}),
            json!({"type":"message_stop"}),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
        let bytes: ByteStream = Box::pin(stream::iter(vec![Ok(Bytes::from(body))]));
        let mut response = oauth_stream(bytes, None);
        let error = response
            .next()
            .await
            .expect("refusal")
            .expect_err("refused");
        assert!(!error.to_string().contains("secret-"));
        if category == "bio" {
            assert!(matches!(error, ApiError::BioPolicy { .. }));
        } else if category == "cyber" {
            assert!(matches!(error, ApiError::CyberPolicy { .. }));
        }
    }
}
