//! Per-provider model catalogs and provider selection for agents.
//!
//! Every thread resolves model metadata through the catalog of its own provider, so a Claude
//! sub-agent spawned by an OpenAI parent (or the reverse) gets correct context windows, reasoning
//! levels, and tool shapes. Catalogs are shared by all threads of a thread manager.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use codex_features::Feature;
use codex_login::AuthManager;
use codex_model_provider::claude_model_catalog;
use codex_model_provider::create_model_provider;
use codex_model_provider_info::ANTHROPIC_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_OAUTH_PROVIDER_ID;
use codex_model_provider_info::ModelProviderInfo;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_model_provider_info::WireApi;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelVisibility;

use crate::config::Config;

/// Non-secret marker written by `codex login --provider claude-code` under `CODEX_HOME`.
pub const CLAUDE_CODE_CONNECTED_MARKER: &str = "claude-code/connected.json";

/// Model catalogs keyed by provider id.
pub(crate) struct ProviderModelsRegistry {
    root_provider_id: Option<String>,
    root: SharedModelsManager,
    auth_manager: Option<Arc<AuthManager>>,
    managers: StdMutex<HashMap<String, SharedModelsManager>>,
}

impl ProviderModelsRegistry {
    pub(crate) fn new(
        root_provider_id: String,
        root: SharedModelsManager,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            root_provider_id: Some(root_provider_id),
            root,
            auth_manager,
            managers: StdMutex::new(HashMap::new()),
        }
    }

    /// A registry that resolves every provider to `root` (used by tests and delegates).
    pub(crate) fn single(root: SharedModelsManager) -> Self {
        Self {
            root_provider_id: None,
            root,
            auth_manager: None,
            managers: StdMutex::new(HashMap::new()),
        }
    }

    /// Returns the catalog for the provider selected by `config`, building it on first use.
    pub(crate) fn for_config(&self, config: &Config) -> SharedModelsManager {
        let provider_id = config.model_provider_id.as_str();
        match self.root_provider_id.as_deref() {
            None => return Arc::clone(&self.root),
            Some(root_id) if root_id == provider_id => return Arc::clone(&self.root),
            Some(_) => {}
        }
        let mut managers = self
            .managers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(managers.entry(provider_id.to_string()).or_insert_with(|| {
            let provider = &config.model_provider;
            // Secondary catalogs never write the root provider's on-disk cache. Claude providers
            // ignore the configured catalog and serve their own.
            let manager = create_model_provider(provider.clone(), self.auth_manager.clone())
                .models_manager_without_cache(config.model_catalog.clone());
            manager.set_api_key_model_discovery_enabled(
                config.features.enabled(Feature::ApiKeyModelDiscovery),
            );
            manager
        }))
    }
}

impl std::fmt::Debug for ProviderModelsRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderModelsRegistry")
            .field("root_provider_id", &self.root_provider_id)
            .finish_non_exhaustive()
    }
}

/// Whether a provider can serve requests right now, and why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderConnection {
    Connected,
    NotConnected { remedy: String },
}

/// Checks whether `provider` has credentials configured, or needs none. Only presence is checked;
/// keys and tokens are not validated. For Claude Code this is the non-secret marker written by
/// `codex login --provider claude-code`: Codex never reads Claude Code's sign-in, and a signed-out
/// CLI fails its first turn with a `claude auth login` hint.
pub(crate) fn provider_connection(
    provider: &ModelProviderInfo,
    auth_manager: Option<&AuthManager>,
    codex_home: &std::path::Path,
) -> ProviderConnection {
    // OAuth never inherits generic API-key, command, or bearer settings, even if an invalid
    // programmatic configuration reaches this read-only discovery path.
    if provider.is_claude_oauth() {
        let connected = provider.validate().is_ok()
            && auth_manager.is_some_and(|manager| {
                manager
                    .claude_oauth_auth_config()
                    .load_stored_credential()
                    .ok()
                    .flatten()
                    .is_some()
            });
        return if connected {
            ProviderConnection::Connected
        } else {
            ProviderConnection::NotConnected {
                remedy: "sign in explicitly with `codex login --provider claude-oauth` (experimental native OAuth)".to_string(),
            }
        };
    }
    // A configured `env_key` is the only key the provider will send, so it alone decides.
    if let Some(var) = provider.env_key.as_deref() {
        let present = std::env::var(var)
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        return if present {
            ProviderConnection::Connected
        } else {
            ProviderConnection::NotConnected {
                remedy: format!("set the {var} environment variable"),
            }
        };
    }
    if provider.experimental_bearer_token.is_some() || provider.auth.is_some() {
        return ProviderConnection::Connected;
    }
    match provider.wire_api {
        WireApi::AnthropicMessages => {
            let has_key = auth_manager
                .map(|manager| manager.anthropic_auth_config().resolve())
                .map(|resolved| resolved.ok().flatten().is_some())
                .unwrap_or_else(|| {
                    std::env::var(codex_model_provider_info::ANTHROPIC_API_KEY_ENV_VAR)
                        .ok()
                        .is_some_and(|value| !value.trim().is_empty())
                });
            if has_key {
                ProviderConnection::Connected
            } else {
                // Every anthropic_messages provider shares the Anthropic key store, whatever its id.
                ProviderConnection::NotConnected {
                    remedy: "store an Anthropic API key with `codex login --provider anthropic \
--with-api-key` (the key on stdin) or set ANTHROPIC_API_KEY"
                        .to_string(),
                }
            }
        }
        WireApi::ClaudeCode => {
            if codex_home.join(CLAUDE_CODE_CONNECTED_MARKER).exists() {
                ProviderConnection::Connected
            } else {
                ProviderConnection::NotConnected {
                    remedy: "connect Claude Code with `codex login --provider claude-code` (it \
uses the official claude CLI and its own sign-in)"
                        .to_string(),
                }
            }
        }
        WireApi::ClaudeOAuth => unreachable!("Claude OAuth connection checked before generic auth"),
        WireApi::Responses => {
            if !provider.requires_openai_auth
                || auth_manager.is_some_and(|manager| manager.auth_cached().is_some())
            {
                ProviderConnection::Connected
            } else {
                ProviderConnection::NotConnected {
                    remedy: "sign in with `codex login`".to_string(),
                }
            }
        }
    }
}

/// One provider offered to `spawn_agent`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SpawnProviderOption {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) is_current: bool,
    pub(crate) wire_api: WireApi,
    /// Picker-visible model slugs, best first.
    pub(crate) models: Vec<String>,
}

const MAX_MODELS_PER_PROVIDER: usize = 5;

/// Lists the providers an agent may delegate to: the current one plus every other built-in
/// provider (OpenAI, Anthropic API, Claude Code, experimental Claude OAuth) that is currently connected.
pub(crate) fn spawn_provider_options(
    config: &Config,
    auth_manager: Option<&AuthManager>,
    current_models: &[ModelPreset],
) -> Vec<SpawnProviderOption> {
    let current_id = config.model_provider_id.as_str();
    let mut options = vec![SpawnProviderOption {
        id: current_id.to_string(),
        description: provider_description(current_id, &config.model_provider),
        is_current: true,
        wire_api: config.model_provider.wire_api,
        models: visible_slugs(current_models.iter().filter(|preset| preset.show_in_picker)),
    }];
    for candidate in [
        OPENAI_PROVIDER_ID,
        ANTHROPIC_PROVIDER_ID,
        CLAUDE_CODE_PROVIDER_ID,
        CLAUDE_OAUTH_PROVIDER_ID,
    ] {
        if candidate == current_id {
            continue;
        }
        let Some(provider) = config.model_providers.get(candidate) else {
            continue;
        };
        if provider_connection(provider, auth_manager, &config.codex_home)
            != ProviderConnection::Connected
        {
            continue;
        }
        let models = match provider.wire_api {
            WireApi::AnthropicMessages | WireApi::ClaudeCode | WireApi::ClaudeOAuth => {
                let catalog = claude_model_catalog();
                let mut models = catalog.models;
                models.sort_by_key(|model| model.priority);
                visible_slugs_from_info(
                    models
                        .iter()
                        .filter(|model| model.visibility == ModelVisibility::List),
                )
            }
            WireApi::Responses => codex_models_manager::bundled_models_response()
                .map(|catalog| {
                    let mut models = catalog.models;
                    models.sort_by_key(|model| model.priority);
                    visible_slugs_from_info(
                        models
                            .iter()
                            .filter(|model| model.visibility == ModelVisibility::List),
                    )
                })
                .unwrap_or_default(),
        };
        options.push(SpawnProviderOption {
            id: candidate.to_string(),
            description: provider_description(candidate, provider),
            is_current: false,
            wire_api: provider.wire_api,
            models,
        });
    }
    options
}

/// Whether collaboration messages must be plaintext so agents on other providers can read them.
///
/// OpenAI can encrypt collaboration arguments server-side, but only OpenAI models can read that
/// ciphertext. A connected Claude provider means a Claude agent may join the tree; once one has,
/// the tree itself keeps plaintext (`LocalAgentRuntime`), even if the provider is disconnected.
pub(crate) fn collaboration_messages_require_plaintext(options: &[SpawnProviderOption]) -> bool {
    options.iter().any(|option| option.wire_api.is_claude())
}

fn provider_description(provider_id: &str, provider: &ModelProviderInfo) -> String {
    match provider.wire_api {
        WireApi::AnthropicMessages => format!("{provider_id}: Claude models via the Anthropic API"),
        WireApi::ClaudeCode => {
            format!("{provider_id}: Claude models via the Claude Code CLI and its own sign-in")
        }
        WireApi::ClaudeOAuth => {
            format!(
                "{provider_id}: Claude models via experimental native OAuth; quota treatment unverified"
            )
        }
        WireApi::Responses => format!("{provider_id}: {}", provider.name),
    }
}

fn visible_slugs<'a>(presets: impl Iterator<Item = &'a ModelPreset>) -> Vec<String> {
    presets
        .take(MAX_MODELS_PER_PROVIDER)
        .map(|preset| preset.model.clone())
        .collect()
}

fn visible_slugs_from_info<'a>(
    models: impl Iterator<Item = &'a codex_protocol::openai_models::ModelInfo>,
) -> Vec<String> {
    models
        .take(MAX_MODELS_PER_PROVIDER)
        .map(|model| model.slug.clone())
        .collect()
}

#[cfg(test)]
#[path = "provider_models_tests.rs"]
mod tests;
