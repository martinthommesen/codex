use super::*;
use crate::anthropic::model_traits::AnthropicThinking;
use crate::anthropic::request::AnthropicRequestOptions;
use crate::anthropic::request::build_anthropic_request;
use crate::provider::RetryConfig;
use codex_client::Request;
use codex_client::Response;
use codex_client::StreamResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;

/// Records each streamed request and fails it, so only the request shape is observed.
#[derive(Clone, Default)]
struct RecordingTransport(Arc<Mutex<Vec<Request>>>);

impl HttpTransport for RecordingTransport {
    async fn execute(&self, _: Request) -> Result<Response, TransportError> {
        Err(TransportError::Build("unexpected request".into()))
    }

    async fn stream(&self, request: Request) -> Result<StreamResponse, TransportError> {
        self.0.lock().expect("lock").push(request);
        Err(TransportError::Build("recorded".into()))
    }
}

#[tokio::test]
async fn configured_betas_are_merged_with_request_betas() {
    let mut provider_headers = HeaderMap::new();
    provider_headers.insert(
        ANTHROPIC_BETA_HEADER,
        HeaderValue::from_static("context-1m-2025-08-07"),
    );
    let provider = Provider {
        name: "anthropic".into(),
        base_url: "https://api.anthropic.com/v1".into(),
        query_params: None,
        headers: provider_headers,
        retry: RetryConfig {
            max_attempts: 1,
            base_delay: Duration::ZERO,
            retry_429: false,
            retry_5xx: false,
            retry_transport: false,
        },
        stream_idle_timeout: Duration::from_secs(2),
    };
    let input = serde_json::from_value(
        json!({"type":"message", "role":"user", "content":[{"type":"input_text","text":"Hello"}]}),
    )
    .expect("user message");
    let prepared = build_anthropic_request(
        "",
        &[input],
        &[],
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
    );
    assert!(!prepared.betas.is_empty());
    let transport = RecordingTransport::default();
    let client = AnthropicMessagesClient::new(
        transport.clone(),
        provider,
        Arc::new(AnthropicApiKeyAuth::new("sk-ant-test".to_string())),
    );
    assert!(client.stream(&prepared, HeaderMap::new()).await.is_err());

    let requests = transport.0.lock().expect("lock");
    let betas = requests[0]
        .headers
        .get_all(ANTHROPIC_BETA_HEADER)
        .iter()
        .map(|value| value.to_str().expect("ascii").to_string())
        .collect::<Vec<_>>();
    let mut expected = vec!["context-1m-2025-08-07".to_string()];
    expected.extend(prepared.betas.iter().map(ToString::to_string));
    assert_eq!(betas, vec![expected.join(",")]);
}
