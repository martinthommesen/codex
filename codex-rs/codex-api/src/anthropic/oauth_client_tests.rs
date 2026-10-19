use super::client::*;
use super::model_traits::AnthropicThinking;
use super::oauth::OAuthRequestContext;
use super::request::{AnthropicRequestOptions, PreparedAnthropicRequest, build_anthropic_request};
use crate::{AuthProvider, Provider, ResponseEvent, RetryConfig};
use bytes::Bytes;
use codex_client::{HttpTransport, Request, RequestBody, Response, StreamResponse, TransportError};
use codex_protocol::models::ResponseItem;
use futures::StreamExt;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Default)]
struct RecordingTransport(Arc<Mutex<Vec<Request>>>);

impl HttpTransport for RecordingTransport {
    async fn execute(&self, _: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build(
            "unexpected non-stream request".into(),
        ))
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        let body: Value = match request.body.as_ref() {
            Some(RequestBody::Raw(body)) => serde_json::from_slice(body).expect("JSON"),
            _ => panic!("OAuth must send final signed bytes"),
        };
        let alias = body["tools"][0]["name"].as_str().expect("tool alias");
        self.0.lock().expect("lock").push(request);
        let events = vec![
            json!({"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":5,"cache_read_input_tokens":7}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Inspect first"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_test","name":alias,"input":{}}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"input\":\"patch\"}"}}),
            json!({"type":"content_block_stop","index":2}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
            json!({"type":"message_stop"}),
        ];
        let sse: String = events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect();
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-ratelimit-unified-5h-utilization",
            HeaderValue::from_static("0.25"),
        );
        Ok(StreamResponse {
            status: StatusCode::OK,
            headers,
            // Exercise the actual SSE framing across byte boundaries.
            bytes: Box::pin(
                futures::stream::iter(sse.into_bytes())
                    .chunks(11)
                    .map(|chunk| Ok(Bytes::from(chunk))),
            ),
        })
    }
}

struct TestBearer;
impl AuthProvider for TestBearer {
    fn claude_oauth_identity(&self) -> Option<crate::ClaudeOAuthIdentity> {
        Some(crate::ClaudeOAuthIdentity {
            account_uuid: context().account_uuid,
            device_id: context().device_id,
        })
    }
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        let mut value = HeaderValue::from_static("Bearer oauth-test-secret");
        value.set_sensitive(true);
        headers.insert(http::header::AUTHORIZATION, value);
    }
}

fn provider(base: &str) -> Provider {
    Provider {
        name: "claude-oauth".into(),
        base_url: base.into(),
        query_params: None,
        headers: HeaderMap::new(),
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(2),
    }
}

fn context() -> OAuthRequestContext {
    OAuthRequestContext {
        account_uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
        device_id: "a".repeat(64),
        session_id: "bbbbbbbb-cccc-4ddd-8eee-ffffffffffff".into(),
        ..Default::default()
    }
}

fn prepared() -> PreparedAnthropicRequest {
    let input = serde_json::from_value(
        json!({"type":"message", "role":"user", "content":[{"type":"input_text","text":"Hello"}]}),
    )
    .expect("user message");
    build_anthropic_request(
        "Follow Codex approvals",
        &[input],
        &[
            json!({"type":"custom","name":"apply_patch","format":{"type":"grammar","syntax":"lark","definition":"x"}}),
        ],
        &AnthropicRequestOptions {
            model: "claude-opus-5-5".into(),
            max_tokens: 32000,
            thinking: AnthropicThinking::Adaptive,
            summarize_thinking: true,
            effort: Some("high"),
            parallel_tool_calls: true,
            prompt_caching: true,
            output_schema: None,
            drop_mismatched_thinking: true,
            strip_thinking: false,
        },
    )
}

#[tokio::test]
async fn oauth_never_accepts_api_key_authentication() {
    let transport = RecordingTransport::default();
    let client = AnthropicMessagesClient::new(
        transport.clone(),
        provider("https://api.anthropic.com/v1"),
        Arc::new(AnthropicApiKeyAuth::new("api-key-must-not-be-used").expect("valid key")),
    );
    assert!(
        client
            .stream_oauth(&prepared(), HeaderMap::new(), &context())
            .await
            .is_err()
    );
    assert!(transport.0.lock().expect("lock").is_empty());
}

struct EchoingFailure;
impl HttpTransport for EchoingFailure {
    async fn execute(&self, _: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("unexpected execute".into()))
    }
    async fn stream(&self, _: Request) -> Result<StreamResponse, TransportError> {
        Err(TransportError::Http {
            status: StatusCode::UNAUTHORIZED,
            url: Some("https://example.invalid/?code=oauth-test-secret".into()),
            headers: Some(HeaderMap::from_iter([(
                http::header::AUTHORIZATION,
                HeaderValue::from_static("Bearer oauth-test-secret"),
            )])),
            body: Some("oauth-test-secret".into()),
            retry_after: None,
        })
    }
}

#[derive(Default)]
struct CaptureTelemetry(Mutex<Vec<String>>);
impl codex_client::RequestTelemetry for CaptureTelemetry {
    fn on_request(
        &self,
        _: u64,
        _: Option<StatusCode>,
        error: Option<&TransportError>,
        _: Duration,
    ) {
        self.0.lock().expect("lock").push(format!("{error:?}"));
    }
}

#[tokio::test]
async fn oauth_http_error_is_redacted_before_telemetry_and_ui() {
    let telemetry = Arc::new(CaptureTelemetry::default());
    let client = AnthropicMessagesClient::new(
        EchoingFailure,
        provider("https://api.anthropic.com/v1"),
        Arc::new(TestBearer),
    )
    .with_telemetry(Some(telemetry.clone()), None);
    let error = match client
        .stream_oauth(&prepared(), HeaderMap::new(), &context())
        .await
    {
        Ok(_) => panic!("must fail"),
        Err(error) => error,
    };
    assert!(!format!("{error:?}").contains("oauth-test-secret"));
    let records = telemetry.0.lock().expect("lock");
    assert_eq!(records.len(), 1);
    assert!(!records[0].contains("oauth-test-secret"));
}

#[tokio::test]
async fn oauth_signed_request_and_sse_use_existing_tool_reasoning_usage_pipeline() {
    let transport = RecordingTransport::default();
    let client = AnthropicMessagesClient::new(
        transport.clone(),
        provider("https://api.anthropic.com/v1"),
        Arc::new(TestBearer),
    );
    let (mut stream, metadata) = client
        .stream_oauth(&prepared(), HeaderMap::new(), &context())
        .await
        .expect("stream");
    assert!(!metadata.rate_limits.is_empty());
    let mut text = false;
    let mut thinking = false;
    let mut tool = false;
    let mut usage = false;
    while let Some(event) = stream.next().await {
        match event.expect("valid SSE") {
            ResponseEvent::OutputTextDelta(delta) => text |= delta == "Hello",
            ResponseEvent::ReasoningSummaryDelta { delta, .. } => {
                thinking |= delta == "Inspect first"
            }
            ResponseEvent::OutputItemDone(ResponseItem::CustomToolCall { name, input, .. }) => {
                tool = name == "apply_patch" && input == "patch";
            }
            ResponseEvent::Completed {
                token_usage: Some(tokens),
                ..
            } => usage = tokens.output_tokens == 3 && tokens.cached_input_tokens == 7,
            _ => {}
        }
    }
    assert!(text && thinking && tool && usage);
    let requests = transport.0.lock().expect("lock");
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(
        request.url,
        "https://api.anthropic.com/v1/messages?beta=true"
    );
    assert_eq!(
        request.headers[http::header::AUTHORIZATION],
        "Bearer oauth-test-secret"
    );
    assert!(request.headers[http::header::AUTHORIZATION].is_sensitive());
    assert!(!request.headers.contains_key("x-api-key"));
    assert!(
        request.headers["anthropic-beta"]
            .to_str()
            .expect("header")
            .contains("oauth-2025-04-20")
    );
    let raw = request
        .prepare_body_for_send()
        .expect("wire bytes")
        .body_bytes();
    let body: Value = serde_json::from_slice(&raw).expect("body");
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert_eq!(body["output_config"]["effort"], "high");
    assert!(body.to_string().contains("cch="));
    assert!(!String::from_utf8_lossy(&raw).contains("oauth-test-secret"));
}

#[tokio::test]
async fn oauth_rejects_any_configured_non_anthropic_destination_before_auth_or_send() {
    for base in [
        "http://api.anthropic.com/v1",
        "https://evil.example/v1",
        "https://api.anthropic.com.evil.example/v1",
        "https://api.anthropic.com:444/v1",
        "https://user@api.anthropic.com/v1",
        "https://api.anthropic.com/v2",
    ] {
        let transport = RecordingTransport::default();
        let client =
            AnthropicMessagesClient::new(transport.clone(), provider(base), Arc::new(TestBearer));
        assert!(
            client
                .stream_oauth(&prepared(), HeaderMap::new(), &context())
                .await
                .is_err()
        );
        assert!(transport.0.lock().expect("lock").is_empty());
    }
}
