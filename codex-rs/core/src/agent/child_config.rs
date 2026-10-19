//! Prepares child configuration from captured step settings and requested overrides.
//!
//! Spawn and reload share live runtime policy; role and model precedence, full-history
//! inheritance, and validation messages remain the same for each multi-agent version.

use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::apply_role_to_config;
use crate::config::Config;
use crate::provider_models::ProviderConnection;
use crate::provider_models::provider_connection;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use codex_features::Feature;
use codex_model_provider_info::ANTHROPIC_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_CODE_PROVIDER_ID;
use codex_model_provider_info::CLAUDE_OAUTH_PROVIDER_ID;
use codex_model_provider_info::OPENAI_PROVIDER_ID;
use codex_model_provider_info::WireApi;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::config_types::SERVICE_TIER_DEFAULT_REQUEST_VALUE;
use codex_protocol::models::BaseInstructions;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::protocol::MultiAgentVersion;

pub(crate) const MAX_SPAWN_AGENT_MODEL_OVERRIDES: usize = 5;

pub(crate) fn model_supports_multi_agent_backend(
    model: &ModelPreset,
    multi_agent_version: MultiAgentVersion,
) -> bool {
    multi_agent_version != MultiAgentVersion::V2
        || model.multi_agent_version != Some(MultiAgentVersion::Disabled)
}

/// Selects the existing spawn tool's role-inheritance rules.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpawnConfigVersion {
    V1,
    V2,
}

pub(crate) struct SpawnConfigOptions<'a> {
    pub(crate) version: SpawnConfigVersion,
    pub(crate) full_history_fork: bool,
    pub(crate) role_name: Option<&'a str>,
    pub(crate) model_provider: Option<&'a str>,
    pub(crate) model: Option<&'a str>,
    pub(crate) reasoning_effort: Option<ReasoningEffort>,
}

pub(crate) struct PreparedSpawnConfig {
    pub(crate) config: Config,
    pub(crate) role_name: Option<String>,
}

/// Resolves child settings before starting the thread, retaining the invoking tool's precedence.
pub(crate) async fn prepare_agent_spawn_config(
    session: &Session,
    step_context: &StepContext,
    options: SpawnConfigOptions<'_>,
) -> Result<PreparedSpawnConfig, String> {
    let turn = step_context.turn.as_ref();
    let mut config =
        build_agent_spawn_config(&session.get_base_instructions().await, step_context)?;
    if options.version == SpawnConfigVersion::V1 && options.full_history_fork {
        reject_full_fork_agent_type_override(options.role_name)?;
    }
    apply_requested_spawn_agent_model_overrides(
        session,
        step_context,
        &mut config,
        options.model_provider,
        options.model,
        options.reasoning_effort,
    )
    .await?;
    let provider_changed = config.model_provider_id != turn.config.model_provider_id;
    // Every agent in a tree that includes a Claude model must send readable messages, and the
    // flag is inherited from here on.
    let mixed_provider_tree = provider_changed
        || matches!(
            turn.config.model_provider.wire_api,
            WireApi::AnthropicMessages | WireApi::ClaudeCode | WireApi::ClaudeOAuth
        );
    if mixed_provider_tree {
        config.plaintext_collaboration_messages = true;
    }
    if !options.full_history_fork
        || (options.version == SpawnConfigVersion::V2 && options.role_name.is_some())
    {
        let model_before_role = config.model.clone();
        apply_spawn_agent_role(session, &mut config, options.role_name).await?;
        if provider_changed && config.model != model_before_role {
            reject_role_model_from_other_provider(session, &config, options.role_name).await?;
        }
        if options.version == SpawnConfigVersion::V2
            && options.full_history_fork
            && config.developer_instructions.is_none()
        {
            config
                .developer_instructions
                .clone_from(&turn.developer_instructions);
        }
    }
    apply_spawn_agent_service_tier(session, &mut config).await?;
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    // Remember an applied configured default so cold reload reapplies its restrictions.
    let role_name = options
        .role_name
        .or_else(|| {
            (options.version == SpawnConfigVersion::V2
                && !options.full_history_fork
                && config
                    .agent_roles
                    .get(DEFAULT_ROLE_NAME)
                    .is_some_and(|role| role.config_file.is_some()))
            .then_some(DEFAULT_ROLE_NAME)
        })
        .map(str::to_owned);
    if mixed_provider_tree {
        // The parent, its siblings and the root keep plaintext too, even if the provider that made
        // this tree mixed is disconnected later.
        session
            .services
            .local_agent_runtime
            .mark_plaintext_collaboration_messages();
    }
    Ok(PreparedSpawnConfig { config, role_name })
}

/// Builds the base config snapshot for a newly spawned sub-agent.
///
/// The returned config starts from the parent's effective config and then refreshes the
/// model selection and reasoning settings captured for the invoking step, plus the turn's
/// runtime approval policy, sandbox, and cwd. Role-specific overrides are layered
/// after this step; skipping this helper and cloning stale config state directly can send the child
/// agent out with the wrong provider or runtime policy.
pub(crate) fn build_agent_spawn_config(
    base_instructions: &BaseInstructions,
    step_context: &StepContext,
) -> Result<Config, String> {
    let mut config = build_agent_shared_config(step_context.turn.as_ref())?;
    let settings = &step_context.settings;
    config.model = Some(settings.model_info.slug.clone());
    config.model_reasoning_effort = settings.effective_reasoning_effort();
    config.model_reasoning_summary = Some(settings.reasoning_summary);
    config.base_instructions = Some(base_instructions.text.clone());
    config.base_instructions_provenance = base_instructions.provenance.clone();
    Ok(config)
}

pub(crate) fn build_agent_resume_config(turn: &TurnContext) -> Result<Config, String> {
    let mut config = build_agent_shared_config(turn)?;
    // For resume, keep base instructions sourced from rollout/session metadata.
    config.base_instructions = None;
    config.base_instructions_provenance = None;
    Ok(config)
}

fn build_agent_shared_config(turn: &TurnContext) -> Result<Config, String> {
    let base_config = turn.config.clone();
    let mut config = (*base_config).clone();
    if config
        .features
        .enabled(Feature::SubagentDefaultContextLimits)
    {
        config.model_context_window = None;
        config.model_auto_compact_token_limit = None;
    }
    // Preserve activation for history forks without freezing the parent's model-owned prompts.
    // Fresh child startup restores configured preferences from the retained snapshot.
    config.token_budget = turn.configured_token_budget.clone();
    config.model = Some(turn.model_info().slug.clone());
    config.model_provider = turn.provider.info().clone();
    config.model_reasoning_effort = turn
        .reasoning_effort()
        .or(turn.model_info().default_reasoning_level.as_ref())
        .cloned();
    config.model_reasoning_summary = Some(turn.reasoning_summary());
    config.developer_instructions = turn.developer_instructions.clone();
    if turn.multi_agent_version == MultiAgentVersion::V2
        && let Some(developer_instructions) = turn
            .config
            .multi_agent_v2
            .subagent_developer_instructions
            .clone()
    {
        config.developer_instructions = Some(developer_instructions);
    }
    apply_spawn_agent_runtime_overrides(&mut config, turn)?;

    Ok(config)
}

fn reject_full_fork_agent_type_override(agent_type: Option<&str>) -> Result<(), String> {
    if agent_type.is_some() {
        return Err(
            "Full-history forked agents inherit the parent agent type; omit agent_type, or spawn without a full-history fork.".to_string());
    }
    Ok(())
}

/// Copies runtime-only turn state onto a child config before it is handed to `AgentControl`.
///
/// These values are chosen by the live turn rather than persisted config, so leaving them stale can
/// make a child agent disagree with its parent about approval policy, cwd, or sandboxing.
fn apply_spawn_agent_runtime_overrides(
    config: &mut Config,
    turn: &TurnContext,
) -> Result<(), String> {
    config
        .permissions
        .approval_policy
        .set(turn.approval_policy())
        .map_err(|err| format!("approval_policy is invalid: {err}"))?;
    config.approvals_reviewer = turn.config.approvals_reviewer;
    #[allow(deprecated)]
    let turn_cwd = turn.cwd.clone();
    config.cwd = turn_cwd;
    config
        .permissions
        .set_permission_profile_from_session_snapshot(
            turn.config
                .permissions
                .permission_profile_state()
                .snapshot(),
        )
        .map_err(|err| format!("permission_profile is invalid: {err}"))?;
    Ok(())
}

/// Switches the child to `provider_id`, keeping the parent and siblings untouched.
///
/// Returns whether the provider differs from the inherited one.
fn apply_requested_spawn_agent_provider(
    session: &Session,
    config: &mut Config,
    provider_id: &str,
) -> Result<bool, String> {
    let Some(provider) = config.model_providers.get(provider_id).cloned() else {
        let mut available = config.model_providers.keys().cloned().collect::<Vec<_>>();
        available.sort();
        return Err(format!(
            "Unknown model_provider `{provider_id}` for spawn_agent. Available providers: {}",
            available.join(", ")
        ));
    };
    if provider_id == config.model_provider_id {
        return Ok(false);
    }
    if let ProviderConnection::NotConnected { remedy } = provider_connection(
        &provider,
        Some(session.services.auth_manager.as_ref()),
        &config.codex_home,
    ) {
        return Err(format!(
            "model_provider `{provider_id}` is not connected. Ask the user to {remedy}."
        ));
    }
    config.model_provider_id = provider_id.to_string();
    config.model_provider = provider;
    // Provider-specific tuning from the parent does not carry over to another backend.
    config.model_verbosity = None;
    config.model_context_window = None;
    config.model_auto_compact_token_limit = None;
    // Configured sub-agent defaults name models of the original provider; keep them from reaching
    // this child's own spawns.
    config.agent_default_subagent_model = None;
    config.agent_default_subagent_reasoning_effort = None;
    Ok(true)
}

/// A role written for the parent's provider may pin a model the child's provider does not serve;
/// fail the spawn instead of starting a child that cannot make a single request.
async fn reject_role_model_from_other_provider(
    session: &Session,
    config: &Config,
    role_name: Option<&str>,
) -> Result<(), String> {
    let Some(model) = config.model.as_deref() else {
        return Ok(());
    };
    let model_info = spawn_models_manager(session, config)
        .await
        .get_model_info(model, &config.to_models_manager_config())
        .await;
    if model_info.used_fallback_model_metadata {
        return Err(format!(
            "agent role `{}` sets model `{model}`, which provider `{}` does not serve. Pass a \
`model` of that provider, or omit `model_provider`.",
            role_name.unwrap_or(DEFAULT_ROLE_NAME),
            config.model_provider_id
        ));
    }
    Ok(())
}

/// Returns the model catalog for the child's (possibly overridden) provider.
async fn spawn_models_manager(
    session: &Session,
    config: &Config,
) -> codex_models_manager::manager::SharedModelsManager {
    // Same-provider children keep the parent's catalog, which may carry session-specific
    // overrides; only a child on another provider needs that provider's catalog.
    if session.get_config().await.model_provider_id == config.model_provider_id {
        return session.services.models_manager.clone();
    }
    session
        .services
        .agent_control
        .models_manager_for_config(config)
        .unwrap_or_else(|| session.services.models_manager.clone())
}

async fn apply_requested_spawn_agent_model_overrides(
    session: &Session,
    step_context: &StepContext,
    config: &mut Config,
    requested_provider: Option<&str>,
    requested_model: Option<&str>,
    requested_reasoning_effort: Option<ReasoningEffort>,
) -> Result<(), String> {
    let turn = step_context.turn.as_ref();
    let provider_changed = match requested_provider
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
    {
        Some(provider_id) => apply_requested_spawn_agent_provider(session, config, provider_id)?,
        None => false,
    };
    // Configured sub-agent defaults name models of the parent's provider, so they only apply
    // when the provider is inherited.
    let requested_model = requested_model.or(if provider_changed {
        None
    } else {
        turn.config.agent_default_subagent_model.as_deref()
    });
    let requested_reasoning_effort = requested_reasoning_effort.or_else(|| {
        (!provider_changed)
            .then(|| turn.config.agent_default_subagent_reasoning_effort.clone())
            .flatten()
    });
    let models_manager = spawn_models_manager(session, config).await;
    if provider_changed && requested_model.is_none() {
        // Never carry the parent's model slug to another provider: use that provider's default.
        let default_model = models_manager
            .get_default_model(
                &None,
                /*allow_provider_model_fallback*/ true,
                RefreshStrategy::Offline,
                config.http_client_factory(),
            )
            .await;
        let default_info = models_manager
            .get_model_info(&default_model, &config.to_models_manager_config())
            .await;
        config.model = Some(default_model.clone());
        config.model_reasoning_effort = match requested_reasoning_effort {
            Some(reasoning_effort) => {
                validate_spawn_agent_reasoning_effort(
                    &default_model,
                    &default_info.supported_reasoning_levels,
                    &reasoning_effort,
                )?;
                Some(reasoning_effort)
            }
            None => default_info.default_reasoning_level,
        };
        return Ok(());
    }
    if requested_model.is_none() && requested_reasoning_effort.is_none() {
        return Ok(());
    }

    if let Some(requested_model) = requested_model {
        let available_models = models_manager
            .list_models(RefreshStrategy::Offline, config.http_client_factory())
            .await;
        let selected_model_name = find_spawn_agent_model_name(
            &available_models,
            requested_model,
            turn.multi_agent_version,
        )
        .map_err(|err| with_provider_hint(err, requested_model, &config.model_provider_id))?;
        let selected_model_info = models_manager
            .get_model_info(&selected_model_name, &config.to_models_manager_config())
            .await;

        config.model = Some(selected_model_name.clone());
        if let Some(reasoning_effort) = requested_reasoning_effort {
            validate_spawn_agent_reasoning_effort(
                &selected_model_name,
                &selected_model_info.supported_reasoning_levels,
                &reasoning_effort,
            )?;
            config.model_reasoning_effort = Some(reasoning_effort);
        } else {
            config.model_reasoning_effort = selected_model_info.default_reasoning_level;
        }

        return Ok(());
    }

    if let Some(reasoning_effort) = requested_reasoning_effort {
        validate_spawn_agent_reasoning_effort(
            &step_context.settings.model_info.slug,
            &step_context.settings.model_info.supported_reasoning_levels,
            &reasoning_effort,
        )?;
        config.model_reasoning_effort = Some(reasoning_effort);
    }

    Ok(())
}

pub(crate) async fn apply_spawn_agent_service_tier(
    session: &Session,
    config: &mut Config,
) -> Result<(), String> {
    let Some(service_tier) = session.services.agent_control.service_tier() else {
        config.service_tier = None;
        return Ok(());
    };
    if service_tier == SERVICE_TIER_DEFAULT_REQUEST_VALUE {
        config.service_tier = Some(service_tier);
        return Ok(());
    }

    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for service tier validation".to_string()
    })?;
    let model_info = spawn_models_manager(session, config)
        .await
        .get_model_info(model.as_str(), &config.to_models_manager_config())
        .await;

    config.service_tier = model_info
        .supports_service_tier(service_tier.as_str())
        .then_some(service_tier);
    Ok(())
}

async fn apply_spawn_agent_role(
    session: &Session,
    config: &mut Config,
    role_name: Option<&str>,
) -> Result<(), String> {
    let previous_model = config.model.clone();
    let previous_reasoning_effort = config.model_reasoning_effort.clone();
    apply_role_to_config(config, role_name).await?;
    if config.model == previous_model && config.model_reasoning_effort == previous_reasoning_effort
    {
        return Ok(());
    }

    let Some(reasoning_effort) = config.model_reasoning_effort.clone() else {
        return Ok(());
    };
    let model = config.model.clone().ok_or_else(|| {
        "spawn_agent could not resolve the child model for reasoning effort validation".to_string()
    })?;
    let model_info = spawn_models_manager(session, config)
        .await
        .get_model_info(&model, &config.to_models_manager_config())
        .await;
    if model_info.used_fallback_model_metadata {
        return Ok(());
    }

    validate_spawn_agent_reasoning_effort(
        &model,
        &model_info.supported_reasoning_levels,
        &reasoning_effort,
    )
}

/// Points the model at the provider that serves `requested_model` when it is not offered by
/// the child's provider, instead of silently switching providers.
fn with_provider_hint(error: String, requested_model: &str, provider_id: &str) -> String {
    let claude_model = codex_model_provider::is_known_claude_model(requested_model);
    match (claude_model, provider_id) {
        (true, ANTHROPIC_PROVIDER_ID | CLAUDE_CODE_PROVIDER_ID | CLAUDE_OAUTH_PROVIDER_ID) => error,
        (true, _) => format!(
            "{error}. `{requested_model}` is a Claude model: pass model_provider \"{ANTHROPIC_PROVIDER_ID}\" \
(Anthropic API key), \"{CLAUDE_CODE_PROVIDER_ID}\" (Claude Code sign-in), or \"{CLAUDE_OAUTH_PROVIDER_ID}\" (experimental native OAuth)."
        ),
        (false, ANTHROPIC_PROVIDER_ID | CLAUDE_CODE_PROVIDER_ID | CLAUDE_OAUTH_PROVIDER_ID) => {
            format!(
                "{error}. Provider `{provider_id}` only serves Claude models; pass model_provider \
\"{OPENAI_PROVIDER_ID}\" for OpenAI models."
            )
        }
        (false, _) => error,
    }
}

fn find_spawn_agent_model_name(
    available_models: &[ModelPreset],
    requested_model: &str,
    multi_agent_version: MultiAgentVersion,
) -> Result<String, String> {
    available_models
        .iter()
        .find(|model| {
            model.model == requested_model
                && model_supports_multi_agent_backend(model, multi_agent_version)
        })
        .map(|model| model.model.clone())
        .ok_or_else(|| {
            let available = available_models
                .iter()
                .filter(|model| model.show_in_picker)
                .filter(|model| model_supports_multi_agent_backend(model, multi_agent_version))
                .take(MAX_SPAWN_AGENT_MODEL_OVERRIDES)
                .map(|model| model.model.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Unknown model `{requested_model}` for spawn_agent. Available models: {available}"
            )
        })
}

fn validate_spawn_agent_reasoning_effort(
    model: &str,
    supported_reasoning_levels: &[ReasoningEffortPreset],
    requested_reasoning_effort: &ReasoningEffort,
) -> Result<(), String> {
    if supported_reasoning_levels
        .iter()
        .any(|preset| &preset.effort == requested_reasoning_effort)
    {
        return Ok(());
    }

    let supported = supported_reasoning_levels
        .iter()
        .map(|preset| preset.effort.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Reasoning effort `{requested_reasoning_effort}` is not supported for model `{model}`. Supported reasoning efforts: {supported}"
    ))
}
