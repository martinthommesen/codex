use super::*;

fn http_error(status: u16, body: &str) -> ApiError {
    ApiError::Transport(TransportError::Http {
        status: StatusCode::from_u16(status).expect("status"),
        url: None,
        headers: None,
        body: Some(body.to_string()),
        retry_after: None,
    })
}

#[test]
fn maps_http_errors() {
    assert!(matches!(
        map_anthropic_transport_error(http_error(
            400,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1200000 tokens > 1000000 maximum"}}"#
        )),
        ApiError::ContextWindowExceeded
    ));
    assert!(matches!(
        map_anthropic_transport_error(http_error(
            429,
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#
        )),
        ApiError::RateLimitExceeded { .. }
    ));
    assert!(matches!(
        map_anthropic_transport_error(http_error(
            529,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#
        )),
        ApiError::Retryable { .. }
    ));
    assert!(matches!(
        map_anthropic_transport_error(http_error(500, "boom")),
        ApiError::Retryable { .. }
    ));
    match map_anthropic_transport_error(http_error(
        401,
        r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
    )) {
        ApiError::InvalidRequest { message } => assert_eq!(
            message,
            "Anthropic authentication failed: invalid x-api-key. Run `codex login status \
--provider anthropic` to see which key Codex sends. A stored key takes precedence over \
ANTHROPIC_API_KEY: replace it with `codex login --provider anthropic --with-api-key` (the key on \
stdin) or remove it with `codex logout --provider anthropic`. If the provider sets `env_key` or a \
gateway token, fix that credential instead."
        ),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn detects_thinking_replay_rejections() {
    let err = map_anthropic_transport_error(http_error(
        400,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.5.content.0: Invalid `signature` in `thinking` block."}}"#,
    ));
    assert!(is_thinking_replay_rejection(&err));
    let other = map_anthropic_transport_error(http_error(
        400,
        r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be positive"}}"#,
    ));
    assert!(!is_thinking_replay_rejection(&other));
}

#[test]
fn maps_stream_error_events() {
    assert!(matches!(
        anthropic_stream_error("rate_limit_error", "busy"),
        ApiError::RateLimitExceeded { .. }
    ));
    assert!(matches!(
        anthropic_stream_error("api_error", "oops"),
        ApiError::Retryable { .. }
    ));
    assert!(matches!(
        anthropic_stream_error("invalid_request_error", "bad"),
        ApiError::InvalidRequest { .. }
    ));
}
