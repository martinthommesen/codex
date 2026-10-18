use super::*;
use pretty_assertions::assert_eq;

#[test]
fn current_models_use_adaptive_thinking_with_effort() {
    let resolved = resolve_claude_reasoning(
        "claude-opus-5-5",
        Some(&ReasoningEffort::High),
        /*max_tokens_override*/ None,
    );
    assert_eq!(
        resolved,
        ResolvedClaudeReasoning {
            thinking: AnthropicThinking::Adaptive,
            effort: Some("high"),
            max_tokens: 32_000,
            adjustments: Vec::new(),
        }
    );
}

#[test]
fn thinking_cannot_be_disabled_on_always_on_models() {
    let resolved = resolve_claude_reasoning(
        "claude-sonnet-5-5",
        Some(&ReasoningEffort::None),
        /*max_tokens_override*/ None,
    );
    assert_eq!(resolved.thinking, AnthropicThinking::Adaptive);
    assert_eq!(resolved.effort, Some("low"));
    assert_eq!(resolved.adjustments.len(), 1);
}

#[test]
fn thinking_can_be_disabled_on_models_that_allow_it() {
    let resolved = resolve_claude_reasoning(
        "claude-opus-4-8",
        Some(&ReasoningEffort::None),
        /*max_tokens_override*/ None,
    );
    assert_eq!(resolved.thinking, AnthropicThinking::Disabled);
    assert_eq!(resolved.effort, None);
    assert!(resolved.adjustments.is_empty());
}

#[test]
fn unsupported_effort_levels_are_clamped_and_reported() {
    let resolved = resolve_claude_reasoning(
        "claude-opus-4-6",
        Some(&ReasoningEffort::XHigh),
        /*max_tokens_override*/ None,
    );
    assert_eq!(resolved.effort, Some("high"));
    assert_eq!(resolved.adjustments.len(), 1);

    let minimal = resolve_claude_reasoning(
        "claude-haiku-5-5",
        Some(&ReasoningEffort::Minimal),
        /*max_tokens_override*/ None,
    );
    assert_eq!(minimal.effort, Some("low"));
    assert_eq!(minimal.adjustments.len(), 1);
}

#[test]
fn legacy_models_use_budget_tokens() {
    let resolved = resolve_claude_reasoning(
        "claude-haiku-4-5",
        Some(&ReasoningEffort::Medium),
        /*max_tokens_override*/ None,
    );
    assert_eq!(
        resolved.thinking,
        AnthropicThinking::Budget {
            budget_tokens: 8_192
        }
    );
    assert_eq!(resolved.effort, None);
    assert!(resolved.adjustments.is_empty());

    let off = resolve_claude_reasoning(
        "claude-haiku-4-5",
        /*effort*/ None,
        /*max_tokens_override*/ None,
    );
    assert_eq!(off.thinking, AnthropicThinking::Omit);
}

#[test]
fn supported_efforts_follow_model_traits() {
    assert_eq!(
        ClaudeModelTraits::for_model("claude-opus-5-5").supported_reasoning_efforts(),
        vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]
    );
    assert_eq!(
        ClaudeModelTraits::for_model("claude-opus-4-8").supported_reasoning_efforts()[0],
        ReasoningEffort::None
    );
    assert_eq!(
        ClaudeModelTraits::for_model("claude-haiku-4-5").context_window,
        200_000
    );
}
