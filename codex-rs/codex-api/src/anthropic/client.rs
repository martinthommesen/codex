//! HTTP client for the Anthropic Messages API (`POST /v1/messages`).

use super::error::map_anthropic_transport_error;
use super::request::PreparedAnthropicRequest;
use super::stream::spawn_anthropic_stream;
use crate::auth::AuthProvider;
use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::SseTelemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use http::HeaderMap;
use http::HeaderName;
use http::HeaderValue;
use http::Method;
use std::fmt;
use std::sync::Arc;
use tracing::instrument;

/// Value of the required `anthropic-version` header.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_VERSION_HEADER: &str = "anthropic-version";
const ANTHROPIC_BETA_HEADER: &str = "anthropic-beta";
const API_KEY_HEADER: &str = "x-api-key";

/// Adds `x-api-key` authentication for Anthropic API keys.
///
/// The key is never included in `Debug` output.
#[derive(Clone)]
pub struct AnthropicApiKeyAuth {
    api_key: String,
}

impl AnthropicApiKeyAuth {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }
}

impl fmt::Debug for AnthropicApiKeyAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnthropicApiKeyAuth")
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl AuthProvider for AnthropicApiKeyAuth {
    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        if let Ok(mut value) = HeaderValue::from_str(&self.api_key) {
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static(API_KEY_HEADER), value);
        }
    }
}

/// Streams Messages API responses.
pub struct AnthropicMessagesClient<T: HttpTransport> {
    session: EndpointSession<T>,
    sse_telemetry: Option<Arc<dyn SseTelemetry>>,
}

impl<T: HttpTransport> AnthropicMessagesClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
            sse_telemetry: None,
        }
    }

    pub fn with_telemetry(
        self,
        request: Option<Arc<dyn RequestTelemetry>>,
        sse: Option<Arc<dyn SseTelemetry>>,
    ) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
            sse_telemetry: sse,
        }
    }

    #[instrument(
        name = "anthropic.messages.stream",
        level = "info",
        skip_all,
        fields(http.method = "POST", api.path = "/messages")
    )]
    pub async fn stream(
        &self,
        prepared: &PreparedAnthropicRequest,
        extra_headers: HeaderMap,
    ) -> Result<ResponseStream, ApiError> {
        let body = EncodedJsonBody::encode(&prepared.request).map_err(|err| {
            ApiError::Stream(format!("failed to encode Anthropic request: {err}"))
        })?;
        let mut headers = extra_headers;
        headers.insert(
            HeaderName::from_static(ANTHROPIC_VERSION_HEADER),
            HeaderValue::from_static(ANTHROPIC_VERSION),
        );
        if !prepared.betas.is_empty()
            && let Ok(value) = HeaderValue::from_str(&prepared.betas.join(","))
        {
            headers.insert(HeaderName::from_static(ANTHROPIC_BETA_HEADER), value);
        }
        headers.insert(
            http::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        let stream_response = self
            .session
            .stream_encoded_json_with(Method::POST, "/messages", headers, Some(body), |_| {})
            .await
            .map_err(map_anthropic_transport_error)?;
        Ok(spawn_anthropic_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            prepared.tool_names.clone(),
            self.sse_telemetry.clone(),
        ))
    }
}
