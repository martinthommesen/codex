//! Static Claude model catalog shared by the Anthropic API and Claude Code providers.

use codex_api::anthropic::ClaudeModelTraits;
use codex_models_manager::model_info::BASE_INSTRUCTIONS;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ApplyPatchToolType;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelMessages;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::WebSearchToolType;
use codex_protocol::protocol::MultiAgentVersion;

/// Default Claude model when none is configured.
pub const DEFAULT_CLAUDE_MODEL: &str = "claude-opus-5-5";
/// Smaller Claude model used for background tasks such as approval review and memories.
pub const CLAUDE_BACKGROUND_MODEL: &str = "claude-haiku-5-5";

struct ClaudeModelSpec {
    slug: &'static str,
    display_name: &'static str,
    description: &'static str,
    default_effort: ReasoningEffort,
    visibility: ModelVisibility,
}

const CLAUDE_MODELS: &[ClaudeModelSpec] = &[
    ClaudeModelSpec {
        slug: "claude-opus-5-5",
        display_name: "Claude Opus 5.5",
        description: "Anthropic's current Opus model for long-running agentic coding.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::List,
    },
    ClaudeModelSpec {
        slug: "claude-sonnet-5-5",
        display_name: "Claude Sonnet 5.5",
        description: "Fast, capable Claude model for everyday coding and agent work.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::List,
    },
    ClaudeModelSpec {
        slug: "claude-haiku-5-5",
        display_name: "Claude Haiku 5.5",
        description: "Fastest, lowest-cost Claude model for simple tasks and sub-agents.",
        default_effort: ReasoningEffort::Medium,
        visibility: ModelVisibility::List,
    },
    ClaudeModelSpec {
        slug: "claude-fable-5-1",
        display_name: "Claude Fable 5.1",
        description: "Anthropic's most capable model for the hardest long-horizon work.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::List,
    },
    ClaudeModelSpec {
        slug: "claude-opus-5",
        display_name: "Claude Opus 5",
        description: "Previous Opus release.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        description: "Previous Sonnet release.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-opus-4-8",
        display_name: "Claude Opus 4.8",
        description: "Claude Opus 4 series.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-opus-4-7",
        display_name: "Claude Opus 4.7",
        description: "Claude Opus 4 series.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-opus-4-6",
        display_name: "Claude Opus 4.6",
        description: "Claude Opus 4 series.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-sonnet-4-6",
        display_name: "Claude Sonnet 4.6",
        description: "Claude Sonnet 4 series.",
        default_effort: ReasoningEffort::High,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-opus-4-5",
        display_name: "Claude Opus 4.5",
        description: "Claude Opus 4.5 (200K context).",
        default_effort: ReasoningEffort::Medium,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-sonnet-4-5",
        display_name: "Claude Sonnet 4.5",
        description: "Claude Sonnet 4.5 (200K context).",
        default_effort: ReasoningEffort::Medium,
        visibility: ModelVisibility::Hide,
    },
    ClaudeModelSpec {
        slug: "claude-haiku-4-5",
        display_name: "Claude Haiku 4.5",
        description: "Previous Haiku release (200K context).",
        default_effort: ReasoningEffort::Medium,
        visibility: ModelVisibility::Hide,
    },
];

/// Returns whether `slug` names a model in the bundled Claude catalog.
pub fn is_known_claude_model(slug: &str) -> bool {
    CLAUDE_MODELS.iter().any(|spec| spec.slug == slug)
}

/// Builds the bundled Claude catalog.
pub fn claude_model_catalog() -> ModelsResponse {
    ModelsResponse {
        models: CLAUDE_MODELS
            .iter()
            .enumerate()
            .map(|(priority, spec)| claude_model_info(spec, priority as i32))
            .collect(),
    }
}

fn claude_model_info(spec: &ClaudeModelSpec, priority: i32) -> ModelInfo {
    let traits = ClaudeModelTraits::for_model(spec.slug);
    let supported_reasoning_levels = traits
        .supported_reasoning_efforts()
        .into_iter()
        .map(|effort| ReasoningEffortPreset {
            description: effort_description(&effort).to_string(),
            effort,
        })
        .collect::<Vec<_>>();
    let default_reasoning_level = supported_reasoning_levels
        .iter()
        .any(|preset| preset.effort == spec.default_effort)
        .then(|| spec.default_effort.clone());
    ModelInfo {
        guardian: None,
        slug: spec.slug.to_string(),
        display_name: spec.display_name.to_string(),
        description: Some(spec.description.to_string()),
        default_reasoning_level,
        supported_reasoning_levels,
        shell_type: ConfigShellToolType::UnifiedExec,
        visibility: spec.visibility,
        supported_in_api: true,
        priority,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
        available_access_programs: None,
        availability_nux: None,
        upgrade: None,
        model_messages: Some(ModelMessages {
            instructions_template: Some(BASE_INSTRUCTIONS.to_string()),
            ..Default::default()
        }),
        include_skills_usage_instructions: true,
        include_plugin_usage_instructions: false,
        include_apps_usage_instructions: false,
        // Claude thinking is always returned as summaries; there is no summary parameter.
        supports_reasoning_summary_parameter: false,
        default_reasoning_summary: ReasoningSummary::Auto,
        support_verbosity: false,
        default_verbosity: None,
        // The Anthropic adapter exposes freeform tools as JSON tools with an `input` string.
        apply_patch_tool_type: Some(ApplyPatchToolType::Freeform),
        web_search_tool_type: WebSearchToolType::Text,
        truncation_policy: TruncationPolicyConfig::tokens(/*limit*/ 10_000),
        supports_image_detail_original: false,
        context_window: Some(traits.context_window),
        max_context_window: Some(traits.context_window),
        auto_compact_token_limit: None,
        comp_hash: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: vec![InputModality::Text, InputModality::Image],
        used_fallback_model_metadata: false,
        supports_search_tool: false,
        supports_experimental_context: false,
        use_responses_lite: false,
        supports_reasoning_effort_updates: false,
        node_repl_auto_review_required: false,
        node_repl_disabled: false,
        auto_review_model_override: None,
        model_specialty: None,
        // Code mode relies on a freeform `exec` tool; Claude uses direct tool calls.
        tool_mode: Some(ToolMode::Direct),
        // Claude agents use the V2 collaboration tools, so they can delegate to any provider.
        multi_agent_version: Some(MultiAgentVersion::V2),
        multi_agent_reasoning_effort: None,
    }
}

fn effort_description(effort: &ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None => "Thinking disabled",
        ReasoningEffort::Low => "Fast responses with lighter thinking",
        ReasoningEffort::Medium => "Balances speed and thinking depth",
        ReasoningEffort::High => "Greater thinking depth for complex problems",
        ReasoningEffort::XHigh => "Extra high thinking depth for agentic coding",
        ReasoningEffort::Max => "Maximum thinking depth for the hardest problems",
        _ => "Model-specific effort",
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
