use super::*;
use pretty_assertions::assert_eq;

#[test]
fn catalog_lists_current_claude_models_first() {
    let catalog = claude_model_catalog();
    let listed: Vec<&str> = catalog
        .models
        .iter()
        .filter(|model| model.visibility == ModelVisibility::List)
        .map(|model| model.slug.as_str())
        .collect();
    assert_eq!(
        listed,
        vec![
            "claude-opus-5-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1"
        ]
    );
    assert_eq!(catalog.models[0].slug, DEFAULT_CLAUDE_MODEL);
    assert!(is_known_claude_model("claude-haiku-4-5"));
    assert!(!is_known_claude_model("gpt-6-sol"));
}

#[test]
fn catalog_entries_are_shaped_for_the_anthropic_adapter() {
    for model in claude_model_catalog().models {
        assert_eq!(model.tool_mode, Some(ToolMode::Direct), "{}", model.slug);
        assert!(!model.use_responses_lite, "{}", model.slug);
        assert!(!model.supports_search_tool, "{}", model.slug);
        assert!(!model.support_verbosity, "{}", model.slug);
        assert!(model.service_tiers.is_empty(), "{}", model.slug);
        assert_eq!(
            model.apply_patch_tool_type,
            Some(ApplyPatchToolType::Freeform),
            "{}",
            model.slug
        );
        assert!(model.default_reasoning_level.is_some(), "{}", model.slug);
        assert!(
            model
                .model_messages
                .as_ref()
                .and_then(|messages| messages.instructions_template.as_ref())
                .is_some(),
            "{}",
            model.slug
        );
    }
}

#[test]
fn reasoning_levels_follow_model_rules() {
    let catalog = claude_model_catalog();
    let find = |slug: &str| {
        catalog
            .models
            .iter()
            .find(|model| model.slug == slug)
            .cloned()
            .expect("model in catalog")
    };
    let levels = |model: &ModelInfo| {
        model
            .supported_reasoning_levels
            .iter()
            .map(|preset| preset.effort.clone())
            .collect::<Vec<_>>()
    };

    // Opus 5.5 cannot disable thinking, so "none" is not offered.
    assert_eq!(
        levels(&find("claude-opus-5-5")),
        vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
            ReasoningEffort::Max,
        ]
    );
    assert_eq!(levels(&find("claude-opus-4-8"))[0], ReasoningEffort::None);
    assert_eq!(find("claude-haiku-4-5").context_window, Some(200_000));
    assert_eq!(find("claude-sonnet-5-5").context_window, Some(1_000_000));
}

#[test]
fn catalog_round_trips_through_models_response_serialization() {
    let catalog = claude_model_catalog();
    let json = serde_json::to_string(&catalog).expect("serialize catalog");
    let parsed: ModelsResponse = serde_json::from_str(&json).expect("catalog deserializes");
    assert_eq!(parsed.models.len(), catalog.models.len());
}
