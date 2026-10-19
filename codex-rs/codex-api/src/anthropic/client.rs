//! HTTP client for the Anthropic Messages API (`POST /v1/messages`).

use super::error::map_anthropic_transport_error;
use super::oauth::OAuthRequestContext;
use super::oauth::prepare_oauth_request;
use super::oauth_response::ClaudeOAuthResponseMetadata;
use super::oauth_response::map_claude_oauth_transport_error;
use super::request::PreparedAnthropicRequest;
use super::stream::spawn_anthropic_stream;
use super::stream::spawn_claude_oauth_stream;
use crate::auth::AuthProvider;
use crate::auth::SharedAuthProvider;
use crate::common::ResponseStream;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::SseTelemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::RequestBody;
use codex_client::RequestTelemetry;
use codex_client::TransportError;
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
        if !prepared.betas.is_empty() {
            // Request headers replace same-named provider headers, so betas configured through
            // the provider's `http_headers` must be merged in here or they would be dropped.
            let configured = self
                .session
                .provider()
                .headers
                .get_all(ANTHROPIC_BETA_HEADER)
                .iter()
                .chain(headers.get_all(ANTHROPIC_BETA_HEADER).iter())
                .filter_map(|value| value.to_str().ok())
                .flat_map(|value| value.split(','))
                .map(str::trim);
            let mut betas: Vec<&str> = Vec::new();
            for beta in configured.chain(prepared.betas.iter().copied()) {
                if !beta.is_empty() && !betas.contains(&beta) {
                    betas.push(beta);
                }
            }
            if let Ok(value) = HeaderValue::from_str(&betas.join(",")) {
                headers.insert(HeaderName::from_static(ANTHROPIC_BETA_HEADER), value);
            }
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

    /// Experimental first-party OAuth transport. Only final signed bytes go to the existing
    /// HTTP/SSE pipeline; tool execution remains entirely within Codex.
    pub async fn stream_oauth(
        &self,
        prepared: &PreparedAnthropicRequest,
        extra_headers: HeaderMap,
        context: &OAuthRequestContext,
    ) -> Result<(ResponseStream, ClaudeOAuthResponseMetadata), ApiError> {
        if self
            .session
            .auth()
            .claude_oauth_identity()
            .is_none_or(|identity| {
                identity.account_uuid != context.account_uuid
                    || identity.device_id != context.device_id
            })
        {
            return Err(ApiError::InvalidRequest {
                message: "claude-oauth requires its own authenticated account and device identity; other provider credentials cannot be used".into(),
            });
        }
        let path = "/messages?beta=true";
        let target = self.session.provider().build_request(Method::POST, path);
        let url = url::Url::parse(&target.url).map_err(|_| ApiError::InvalidRequest {
            message: "invalid Claude OAuth destination".into(),
        })?;
        if url.scheme() != "https"
            || url.host_str() != Some("api.anthropic.com")
            || url.port_or_known_default() != Some(443)
            || url.path() != "/v1/messages"
            || url.query() != Some("beta=true")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(ApiError::InvalidRequest {
                message: "claude-oauth only sends credentials directly to https://api.anthropic.com/v1/messages".into(),
            });
        }
        let request = serde_json::to_value(&prepared.request)
            .map_err(|_| ApiError::Stream("failed to encode Claude OAuth request".into()))?;
        let oauth =
            prepare_oauth_request(request, context).map_err(|err| ApiError::InvalidRequest {
                message: err.to_string(),
            })?;
        let mut headers = extra_headers;
        headers.extend(oauth.headers);
        // The shared builder may require newer thinking betas than the compatibility profile.
        let mut betas = headers
            .get(ANTHROPIC_BETA_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        for beta in &prepared.betas {
            if !betas.split(',').any(|existing| existing == *beta) {
                if !betas.is_empty() {
                    betas.push(',');
                }
                betas.push_str(beta);
            }
        }
        if let Ok(value) = HeaderValue::from_str(&betas) {
            headers.insert(HeaderName::from_static(ANTHROPIC_BETA_HEADER), value);
        }
        let mut tool_names = prepared.tool_names.clone();
        for (alias, original) in oauth.tool_names {
            tool_names.insert(alias, prepared.tool_names.resolve(&original));
        }
        let body = bytes::Bytes::from(oauth.body);
        let telemetry = self
            .session
            .request_telemetry()
            .map(|inner| Arc::new(OAuthRequestTelemetry { inner }) as Arc<dyn RequestTelemetry>);
        let stream_response = self
            .session
            .stream_encoded_json_with_request_telemetry(
                Method::POST,
                path,
                headers,
                None,
                |request| {
                    // Raw bytes prevent reserialization after CCH signing and avoid body trace logs.
                    request.body = Some(RequestBody::Raw(body.clone()));
                    request.compression = codex_client::RequestCompression::None;
                    request.headers.remove("x-api-key");
                    request.headers.remove(http::header::AUTHORIZATION);
                },
                telemetry,
            )
            .await
            .map_err(map_claude_oauth_transport_error)?;
        let metadata = ClaudeOAuthResponseMetadata::from_headers(&stream_response.headers);
        let stream = spawn_claude_oauth_stream(
            stream_response,
            self.session.provider().stream_idle_timeout,
            tool_names,
            self.sse_telemetry.clone(),
            metadata.rate_limits.clone(),
        );
        Ok((stream, metadata))
    }
}

/// HTTP telemetry runs before API error mapping. It must not observe raw OAuth response
/// bodies, URLs, or server diagnostics, even when the backend unexpectedly echoes a secret.
struct OAuthRequestTelemetry {
    inner: Arc<dyn RequestTelemetry>,
}

impl RequestTelemetry for OAuthRequestTelemetry {
    fn on_request(
        &self,
        attempt: u64,
        status: Option<http::StatusCode>,
        error: Option<&TransportError>,
        duration: std::time::Duration,
    ) {
        let redacted = error.map(|_| {
            TransportError::Network("Claude OAuth request failed; details redacted".into())
        });
        self.inner
            .on_request(attempt, status, redacted.as_ref(), duration);
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
