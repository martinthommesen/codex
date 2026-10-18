//! Per-model request rules for Claude models on the Anthropic Messages API.
//!
//! Claude models differ in how thinking is configured: current models only accept adaptive
//! thinking controlled by `output_config.effort`, some of them refuse to disable thinking, and
//! older models still take a fixed `budget_tokens`. Codex expresses reasoning as a single
//! [`ReasoningEffort`]; this module maps it onto the request shape each model accepts and
//! reports every adjustment so callers can surface it instead of silently ignoring a setting.

use codex_protocol::openai_models::ReasoningEffort;

/// How a model expects thinking to be configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaudeThinkingStyle {
    /// Thinking is always on (adaptive); `disabled` and `budget_tokens` are rejected.
    AdaptiveAlwaysOn,
    /// Adaptive by default; `disabled` is accepted at effort `high` or below.
    AdaptiveDefaultOn,
    /// Adaptive must be requested explicitly; omitting `thinking` runs without thinking.
    AdaptiveExplicit,
    /// Legacy models configured with `{type: "enabled", budget_tokens}`.
    BudgetTokens,
}

/// Request rules for one Claude model family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeModelTraits {
    pub thinking: ClaudeThinkingStyle,
    /// Effort values accepted in `output_config.effort`, lowest first. Empty means unsupported.
    pub efforts: &'static [&'static str],
    /// Default `max_tokens` for a sampling request.
    pub default_max_tokens: u64,
    /// Upper bound for `max_tokens`.
    pub max_output_tokens: u64,
    /// Context window in tokens.
    pub context_window: i64,
}

const ALL_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];
const NO_XHIGH_EFFORTS: &[&str] = &["low", "medium", "high", "max"];
const BASIC_EFFORTS: &[&str] = &["low", "medium", "high"];
const NO_EFFORTS: &[&str] = &[];
const ONE_MILLION: i64 = 1_000_000;

impl ClaudeModelTraits {
    /// Returns the request rules for a Claude model slug.
    ///
    /// Unknown `claude-*` slugs are treated like current-generation models that accept explicit
    /// adaptive thinking, which is the most widely accepted shape.
    pub fn for_model(model: &str) -> Self {
        let slug = model.trim().to_ascii_lowercase();
        let slug = slug
            .strip_prefix("anthropic.")
            .unwrap_or(slug.as_str())
            .to_string();
        let starts = |prefix: &str| slug.starts_with(prefix);
        if starts("claude-fable-") || starts("claude-mythos-") || starts("claude-opus-5-5") {
            return Self::current(ClaudeThinkingStyle::AdaptiveAlwaysOn, ALL_EFFORTS);
        }
        if starts("claude-sonnet-5-5") {
            // `disabled` is rejected; `between_tools` is the only thinking-off mode.
            return Self::current(ClaudeThinkingStyle::AdaptiveAlwaysOn, ALL_EFFORTS);
        }
        if starts("claude-opus-5") || starts("claude-haiku-5") {
            return Self::current(ClaudeThinkingStyle::AdaptiveDefaultOn, ALL_EFFORTS);
        }
        if starts("claude-sonnet-5") {
            return Self::current(ClaudeThinkingStyle::AdaptiveDefaultOn, ALL_EFFORTS);
        }
        if starts("claude-opus-4-8") || starts("claude-opus-4-7") {
            return Self::current(ClaudeThinkingStyle::AdaptiveExplicit, ALL_EFFORTS);
        }
        if starts("claude-opus-4-6") || starts("claude-sonnet-4-6") {
            return Self::current(ClaudeThinkingStyle::AdaptiveExplicit, NO_XHIGH_EFFORTS);
        }
        if starts("claude-opus-4-5") {
            return Self {
                thinking: ClaudeThinkingStyle::BudgetTokens,
                efforts: BASIC_EFFORTS,
                default_max_tokens: 32_000,
                max_output_tokens: 64_000,
                context_window: 200_000,
            };
        }
        if starts("claude-haiku-4-5") || starts("claude-sonnet-4-5") {
            return Self {
                thinking: ClaudeThinkingStyle::BudgetTokens,
                efforts: NO_EFFORTS,
                default_max_tokens: 32_000,
                max_output_tokens: 64_000,
                context_window: 200_000,
            };
        }
        if starts("claude-opus-4") || starts("claude-sonnet-4") || starts("claude-3") {
            return Self {
                thinking: ClaudeThinkingStyle::BudgetTokens,
                efforts: NO_EFFORTS,
                default_max_tokens: 16_000,
                max_output_tokens: 32_000,
                context_window: 200_000,
            };
        }
        Self::current(ClaudeThinkingStyle::AdaptiveExplicit, ALL_EFFORTS)
    }

    const fn current(thinking: ClaudeThinkingStyle, efforts: &'static [&'static str]) -> Self {
        Self {
            thinking,
            efforts,
            default_max_tokens: 32_000,
            max_output_tokens: 128_000,
            context_window: ONE_MILLION,
        }
    }

    /// Codex reasoning efforts that map onto this model without adjustment.
    pub fn supported_reasoning_efforts(&self) -> Vec<ReasoningEffort> {
        let mut efforts = Vec::new();
        if matches!(
            self.thinking,
            ClaudeThinkingStyle::AdaptiveDefaultOn
                | ClaudeThinkingStyle::AdaptiveExplicit
                | ClaudeThinkingStyle::BudgetTokens
        ) {
            efforts.push(ReasoningEffort::None);
        }
        let levels: &[&str] = if self.efforts.is_empty() {
            BASIC_EFFORTS
        } else {
            self.efforts
        };
        for level in levels {
            efforts.push(match *level {
                "low" => ReasoningEffort::Low,
                "medium" => ReasoningEffort::Medium,
                "high" => ReasoningEffort::High,
                "xhigh" => ReasoningEffort::XHigh,
                _ => ReasoningEffort::Max,
            });
        }
        efforts
    }
}

/// Thinking configuration resolved for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicThinking {
    /// Send no `thinking` field.
    Omit,
    /// `{type: "adaptive", display: "summarized"}`.
    Adaptive,
    /// `{type: "disabled"}`.
    Disabled,
    /// `{type: "enabled", budget_tokens}`.
    Budget { budget_tokens: u64 },
}

/// Reasoning settings resolved for one request, plus any adjustments that were made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClaudeReasoning {
    pub thinking: AnthropicThinking,
    pub effort: Option<&'static str>,
    pub max_tokens: u64,
    /// Explanations of settings that could not be honored exactly.
    pub adjustments: Vec<String>,
}

/// Maps a Codex reasoning effort onto the thinking/effort request shape for `model`.
pub fn resolve_claude_reasoning(
    model: &str,
    effort: Option<&ReasoningEffort>,
    max_tokens_override: Option<u64>,
) -> ResolvedClaudeReasoning {
    let traits = ClaudeModelTraits::for_model(model);
    let mut adjustments = Vec::new();
    let max_tokens = max_tokens_override
        .unwrap_or(traits.default_max_tokens)
        .clamp(1, traits.max_output_tokens);

    // Normalize the requested level onto the Anthropic effort ladder.
    let wants_off = matches!(effort, Some(ReasoningEffort::None));
    let requested: Option<&'static str> = match effort {
        None => None,
        Some(ReasoningEffort::None) => None,
        Some(ReasoningEffort::Minimal) => {
            adjustments.push(format!(
                "reasoning effort `minimal` is not an Anthropic effort level; using `low` for {model}"
            ));
            Some("low")
        }
        Some(ReasoningEffort::Low) => Some("low"),
        Some(ReasoningEffort::Medium) => Some("medium"),
        Some(ReasoningEffort::High) => Some("high"),
        Some(ReasoningEffort::XHigh) => Some("xhigh"),
        Some(ReasoningEffort::Max) => Some("max"),
        Some(other) => {
            adjustments.push(format!(
                "reasoning effort `{}` is not supported by Anthropic models; using `max` for {model}",
                other.as_str()
            ));
            Some("max")
        }
    };

    let clamp_effort = |level: &'static str,
                        adjustments: &mut Vec<String>|
     -> Option<&'static str> {
        if traits.efforts.is_empty() {
            adjustments.push(format!(
                "{model} does not accept an effort level; reasoning effort `{level}` only controls the thinking budget"
            ));
            return None;
        }
        if traits.efforts.contains(&level) {
            return Some(level);
        }
        // Pick the highest supported level at or below the requested one.
        let order = ["low", "medium", "high", "xhigh", "max"];
        let requested_rank = order.iter().position(|value| *value == level).unwrap_or(2);
        let chosen = traits
            .efforts
            .iter()
            .copied()
            .filter(|value| order.iter().position(|o| o == value).unwrap_or(0) <= requested_rank)
            .next_back()
            .unwrap_or(traits.efforts[0]);
        adjustments.push(format!(
            "reasoning effort `{level}` is not supported by {model}; using `{chosen}`"
        ));
        Some(chosen)
    };

    let (thinking, effort) = match traits.thinking {
        ClaudeThinkingStyle::AdaptiveAlwaysOn => {
            if wants_off {
                adjustments.push(format!(
                    "thinking cannot be disabled on {model}; using adaptive thinking at effort `low`"
                ));
                (AnthropicThinking::Adaptive, Some("low"))
            } else {
                (
                    AnthropicThinking::Adaptive,
                    requested.and_then(|level| clamp_effort(level, &mut adjustments)),
                )
            }
        }
        ClaudeThinkingStyle::AdaptiveDefaultOn | ClaudeThinkingStyle::AdaptiveExplicit => {
            if wants_off {
                (AnthropicThinking::Disabled, None)
            } else {
                (
                    AnthropicThinking::Adaptive,
                    requested.and_then(|level| clamp_effort(level, &mut adjustments)),
                )
            }
        }
        ClaudeThinkingStyle::BudgetTokens => {
            if wants_off || requested.is_none() {
                (AnthropicThinking::Omit, None)
            } else {
                let level = requested.unwrap_or("medium");
                let budget = match level {
                    "low" => 2_048,
                    "medium" => 8_192,
                    "high" => 16_384,
                    _ => 24_576,
                };
                // The budget must stay below max_tokens and at least 1024.
                let budget = budget.min(max_tokens.saturating_sub(1)).max(1_024);
                let effort = if traits.efforts.is_empty() {
                    None
                } else {
                    clamp_effort(level, &mut adjustments)
                };
                if budget >= max_tokens {
                    adjustments.push(format!(
                        "max_tokens {max_tokens} is too small for extended thinking on {model}; thinking disabled"
                    ));
                    (AnthropicThinking::Omit, effort)
                } else {
                    (
                        AnthropicThinking::Budget {
                            budget_tokens: budget,
                        },
                        effort,
                    )
                }
            }
        }
    };

    ResolvedClaudeReasoning {
        thinking,
        effort,
        max_tokens,
        adjustments,
    }
}

#[cfg(test)]
#[path = "model_traits_tests.rs"]
mod tests;
