//! Experimental direct Claude OAuth provider. Credentials, refresh, and account identity are
//! isolated from both the Anthropic API-key provider and OpenAI authentication.

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::AuthError;
use codex_api::AuthHeadersFuture;
use codex_api::AuthProvider;
use codex_api::AuthProviderFuture;
use codex_api::ClaudeOAuthIdentity;
use codex_api::SharedAuthProvider;
use codex_api::TransportError;
use codex_http_client::Request;
use codex_login::AuthManager;
use codex_login::ClaudeOAuthAuthConfig;
use codex_login::ClaudeOAuthCredential;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelsResponse;
use http::HeaderMap;
use http::HeaderValue;

use super::CLAUDE_BACKGROUND_MODEL;
use super::DEFAULT_CLAUDE_MODEL;
use super::claude_capabilities;
use super::claude_models_manager;
use crate::ProviderCapabilities;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;

const LOGIN_HINT: &str = "Sign in explicitly with `codex login --provider claude-oauth`. This is an \
experimental third-party integration; Anthropic permission and subscription billing are not guaranteed.";

#[derive(Clone, Debug)]
pub(crate) struct ClaudeOAuthModelProvider {
    info: ModelProviderInfo,
    auth_config: Option<ClaudeOAuthAuthConfig>,
}

impl ClaudeOAuthModelProvider {
    pub(crate) fn new(info: ModelProviderInfo, manager: Option<Arc<AuthManager>>) -> Self {
        Self {
            info,
            auth_config: manager.map(|manager| manager.claude_oauth_auth_config()),
        }
    }
}

impl ModelProvider for ClaudeOAuthModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn include_internal_metadata(&self, _provider: &codex_api::Provider) -> bool {
        false
    }

    fn capabilities(&self) -> ProviderCapabilities {
        claude_capabilities()
    }

    fn approval_review_preferred_model(&self) -> &'static str {
        CLAUDE_BACKGROUND_MODEL
    }

    fn memory_extraction_preferred_model(&self) -> &'static str {
        CLAUDE_BACKGROUND_MODEL
    }

    fn memory_consolidation_preferred_model(&self) -> &'static str {
        DEFAULT_CLAUDE_MODEL
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        // Never expose the OpenAI manager to unauthorized recovery or model discovery.
        None
    }

    fn is_recoverable_auth_error(&self, _error: &TransportError) -> bool {
        // Expiration is handled before each outbound request. A server 401 can also mean revoked
        // credentials; never trigger another provider's recovery or replay a consumed refresh.
        false
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async { None })
    }

    fn account_state(&self) -> ProviderAccountResult {
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        })
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, Result<codex_api::Provider>> {
        Box::pin(async move {
            self.info.validate().map_err(CodexErr::InvalidRequest)?;
            self.info.to_api_provider(/*auth_mode*/ None)
        })
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, Result<SharedAuthProvider>> {
        Box::pin(async move {
            self.info.validate().map_err(CodexErr::InvalidRequest)?;
            let config = self.auth_config.clone().ok_or_else(|| {
                CodexErr::InvalidRequest(format!(
                    "Claude OAuth authentication is unavailable. {LOGIN_HINT}"
                ))
            })?;
            let credential = config
                .resolve()
                .await
                .map_err(CodexErr::Io)?
                .ok_or_else(|| {
                    CodexErr::InvalidRequest(format!("Claude OAuth is not connected. {LOGIN_HINT}"))
                })?;
            Ok(Arc::new(ClaudeOAuthHttpAuth { config, credential }) as SharedAuthProvider)
        })
    }

    fn map_api_error(&self, error: ApiError) -> CodexErr {
        // The OAuth client and stream already sanitize semantic errors. Mapping them again
        // would discard actionable login/entitlement guidance. Raw transport errors entering
        // through another caller still require the same credential-safe conversion.
        let error = match error {
            error @ (ApiError::Transport(_) | ApiError::Api { .. }) => {
                codex_api::anthropic::map_claude_oauth_transport_error(error)
            }
            error => error,
        };
        codex_api::map_api_error(error)
    }

    fn models_manager(
        &self,
        _codex_home: PathBuf,
        _config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        claude_models_manager()
    }

    fn models_manager_without_cache(
        &self,
        _config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        claude_models_manager()
    }
}

/// Initial identity is used while constructing the request body; credentials are reloaded and
/// refreshed immediately before every actual HTTP attempt, including retries.
struct ClaudeOAuthHttpAuth {
    config: ClaudeOAuthAuthConfig,
    credential: ClaudeOAuthCredential,
}

impl ClaudeOAuthHttpAuth {
    async fn current_headers(&self) -> std::result::Result<HeaderMap, AuthError> {
        let credential = self
            .config
            .resolve()
            .await
            .map_err(|_| {
                AuthError::Build(format!(
                    "Could not resolve Claude OAuth credentials. {LOGIN_HINT}"
                ))
            })?
            .ok_or_else(|| {
                AuthError::Build(format!(
                    "Claude OAuth credentials were removed. {LOGIN_HINT}"
                ))
            })?;
        if credential.account_uuid() != self.credential.account_uuid()
            || credential.device_id() != self.credential.device_id()
        {
            return Err(AuthError::Build(
                "Claude OAuth account changed while preparing this request; start a new turn"
                    .to_string(),
            ));
        }
        bearer_headers(credential.access_token())
    }
}

impl AuthProvider for ClaudeOAuthHttpAuth {
    fn claude_oauth_identity(&self) -> Option<ClaudeOAuthIdentity> {
        Some(ClaudeOAuthIdentity {
            account_uuid: self.credential.account_uuid().to_string(),
            device_id: self.credential.device_id().to_string(),
        })
    }

    fn add_auth_headers(&self, headers: &mut HeaderMap) {
        // This snapshot is used only for synchronous auth telemetry; sending a request always
        // goes through apply_auth and resolves the latest credential from the isolated store.
        if let Ok(auth) = bearer_headers(self.credential.access_token()) {
            headers.extend(auth);
        }
    }

    fn resolve_auth_headers(&self) -> AuthHeadersFuture<'_> {
        Box::pin(self.current_headers())
    }

    fn apply_auth(&self, mut request: Request) -> AuthProviderFuture<'_> {
        Box::pin(async move {
            validate_request_destination(&request)?;
            request.headers.remove("x-api-key");
            request.headers.remove(http::header::AUTHORIZATION);
            request.headers.extend(self.current_headers().await?);
            Ok(request)
        })
    }
}

fn bearer_headers(token: &str) -> std::result::Result<HeaderMap, AuthError> {
    let mut bearer = HeaderValue::from_str(&format!("Bearer {token}")).map_err(|_| {
        AuthError::Build("Claude OAuth access token is not a valid HTTP credential".to_string())
    })?;
    bearer.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(http::header::AUTHORIZATION, bearer);
    Ok(headers)
}

fn validate_request_destination(request: &Request) -> std::result::Result<(), AuthError> {
    let allowed = url::Url::parse(&request.url).ok().is_some_and(|url| {
        url.scheme() == "https"
            && url.host_str() == Some("api.anthropic.com")
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && url.path() == "/v1/messages"
            && request.method == http::Method::POST
            && url.query().is_none_or(|query| query == "beta=true")
    });
    if !allowed {
        return Err(AuthError::Build(
            "Claude OAuth credentials may only be sent directly to Anthropic's HTTPS Messages endpoint".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;
