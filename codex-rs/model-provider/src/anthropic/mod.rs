//! Model providers for Claude models.
//!
//! [`AnthropicModelProvider`] talks to the Anthropic Messages API with an Anthropic API key that
//! Codex stores separately from ChatGPT credentials (falling back to `ANTHROPIC_API_KEY`).
//! [`ClaudeCodeModelProvider`] drives the user's installed Claude Code CLI, which signs in on its
//! own; Codex never sees those credentials, so this provider attaches no HTTP auth at all.
//! Neither provider ever uses the ChatGPT [`AuthManager`] for requests, so ChatGPT refreshes and
//! logouts cannot affect Claude, and a Claude 401 never triggers ChatGPT recovery.

mod catalog;

pub use catalog::CLAUDE_BACKGROUND_MODEL;
pub use catalog::DEFAULT_CLAUDE_MODEL;
pub use catalog::claude_model_catalog;
pub use catalog::is_known_claude_model;

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::ApiError;
use codex_api::SharedAuthProvider;
use codex_api::TransportError;
use codex_api::anthropic::AnthropicApiKeyAuth;
use codex_login::AnthropicAuthConfig;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ANTHROPIC_API_KEY_ENV_VAR;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::EnvVarError;
use codex_protocol::error::Result;
use codex_protocol::openai_models::ModelsResponse;

use crate::ProviderCapabilities;
use crate::RemoteCompactionSupport;
use crate::auth::resolve_provider_auth;
use crate::auth::unauthenticated_auth_provider;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;
use crate::provider::ProviderUnauthorizedRecovery;

const MISSING_KEY_INSTRUCTIONS: &str = "Run `codex login --provider anthropic` to store an \
Anthropic API key (kept separate from your ChatGPT login), or export ANTHROPIC_API_KEY. To use a \
Claude subscription instead, sign in to Claude Code and select the `claude-code` provider.";

/// Capabilities shared by both Claude providers.
fn claude_capabilities() -> ProviderCapabilities {
    ProviderCapabilities {
        // OpenAI-hosted tools have no Anthropic equivalent wired up yet.
        image_generation: false,
        web_search: false,
        external_web_access: false,
        // Remote compaction is an OpenAI protocol; Codex compacts locally instead.
        remote_compaction: RemoteCompactionSupport::Unsupported,
    }
}

fn claude_models_manager(config_model_catalog: Option<ModelsResponse>) -> SharedModelsManager {
    let catalog = config_model_catalog.unwrap_or_else(claude_model_catalog);
    Arc::new(StaticModelsManager::new(
        /*auth_manager*/ None, catalog,
    ))
}

/// Provider for the Anthropic Messages API.
#[derive(Clone, Debug)]
pub(crate) struct AnthropicModelProvider {
    info: ModelProviderInfo,
    auth_config: Option<AnthropicAuthConfig>,
    command_auth_manager: Option<Arc<AuthManager>>,
}

impl AnthropicModelProvider {
    pub(crate) fn new(info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
        let auth_config = auth_manager
            .as_ref()
            .map(|manager| manager.anthropic_auth_config());
        let command_auth_manager = info.auth.clone().map(AuthManager::external_bearer_only);
        Self {
            info,
            auth_config,
            command_auth_manager,
        }
    }

    /// Resolves the API key from the Codex Anthropic store, falling back to the environment.
    fn resolve_api_key(&self) -> Result<Option<String>> {
        match &self.auth_config {
            Some(config) => config
                .resolve()
                .map(|credential| credential.map(|credential| credential.api_key().to_string()))
                .map_err(CodexErr::Io),
            None => Ok(std::env::var(ANTHROPIC_API_KEY_ENV_VAR)
                .ok()
                .filter(|key| !key.trim().is_empty())),
        }
    }

    async fn resolve_api_auth(&self) -> Result<SharedAuthProvider> {
        // An explicit `env_key` (for example a gateway's key variable) wins.
        if let Some(key) = self.info.api_key()? {
            return Ok(Arc::new(AnthropicApiKeyAuth::new(key)));
        }
        // Bearer-token gateways use the generic provider auth paths.
        if self.info.experimental_bearer_token.is_some() {
            return resolve_provider_auth(/*auth*/ None, &self.info);
        }
        if let Some(manager) = &self.command_auth_manager {
            let auth = manager.auth().await;
            return resolve_provider_auth(auth.as_ref(), &self.info);
        }
        match self.resolve_api_key()? {
            Some(api_key) => Ok(Arc::new(AnthropicApiKeyAuth::new(api_key))),
            None => Err(CodexErr::EnvVar(EnvVarError {
                var: ANTHROPIC_API_KEY_ENV_VAR.to_string(),
                instructions: Some(MISSING_KEY_INSTRUCTIONS.to_string()),
            })),
        }
    }
}

impl ModelProvider for AnthropicModelProvider {
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
        // Never expose the ChatGPT manager: its unauthorized recovery refreshes ChatGPT tokens.
        None
    }

    fn is_recoverable_auth_error(&self, _error: &TransportError) -> bool {
        false
    }

    fn recover_from_unauthorized(
        &self,
    ) -> ModelProviderFuture<'_, Result<ProviderUnauthorizedRecovery>> {
        Box::pin(async { Ok(ProviderUnauthorizedRecovery::NotConfigured) })
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async { None })
    }

    fn account_state(&self) -> ProviderAccountResult {
        // App-visible account variants describe OpenAI accounts; reporting the Anthropic key as
        // `ApiKey` would make clients present it as an OpenAI API key. Connection state is shown
        // by `codex login status` instead.
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        })
    }

    fn map_api_error(&self, error: ApiError) -> CodexErr {
        codex_api::map_api_error(codex_api::anthropic::map_anthropic_transport_error(error))
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, Result<codex_api::Provider>> {
        Box::pin(async move {
            self.info.to_api_provider(/*auth_mode*/ None)
        })
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, Result<SharedAuthProvider>> {
        Box::pin(self.resolve_api_auth())
    }

    fn models_manager(
        &self,
        _codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        claude_models_manager(config_model_catalog)
    }

    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        claude_models_manager(config_model_catalog)
    }
}

/// Provider for the locally installed Claude Code CLI.
#[derive(Clone, Debug)]
pub(crate) struct ClaudeCodeModelProvider {
    info: ModelProviderInfo,
}

impl ClaudeCodeModelProvider {
    pub(crate) fn new(info: ModelProviderInfo) -> Self {
        Self { info }
    }
}

impl ModelProvider for ClaudeCodeModelProvider {
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
        None
    }

    fn is_recoverable_auth_error(&self, _error: &TransportError) -> bool {
        false
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        Box::pin(async { None })
    }

    fn account_state(&self) -> ProviderAccountResult {
        // Claude Code manages its own sign-in; Codex holds no credentials for it.
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        })
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, Result<codex_api::Provider>> {
        Box::pin(async {
            Err(CodexErr::UnsupportedOperation(
                "the claude-code provider runs through the Claude Code CLI and has no HTTP endpoint"
                    .to_string(),
            ))
        })
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, Result<SharedAuthProvider>> {
        Box::pin(async { Ok(unauthenticated_auth_provider()) })
    }

    fn models_manager(
        &self,
        _codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        claude_models_manager(config_model_catalog)
    }

    fn models_manager_without_cache(
        &self,
        config_model_catalog: Option<ModelsResponse>,
    ) -> SharedModelsManager {
        claude_models_manager(config_model_catalog)
    }
}

#[cfg(test)]
#[path = "mod_tests.rs"]
mod tests;
