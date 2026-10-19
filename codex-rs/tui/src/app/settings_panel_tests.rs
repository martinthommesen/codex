use super::*;
use codex_app_server_protocol::ConfigLayerSource as ApiSource;
use pretty_assertions::assert_eq;

fn config_read(json: serde_json::Value) -> ConfigReadResponse {
    serde_json::from_value(json).expect("valid config read response")
}

fn origin(source: ApiSource) -> serde_json::Value {
    serde_json::json!({ "name": source, "version": "1" })
}

fn user_file(name: &str) -> AbsolutePathBuf {
    AbsolutePathBuf::from_absolute_path(std::env::temp_dir().join(name)).expect("absolute path")
}

/// The layer `switch_override_layer` blames for a remote save written to `written`.
fn remote_override(
    switch: SettingsSwitch,
    enabled: bool,
    effective_config: &ConfigReadResponse,
    written: &AbsolutePathBuf,
) -> Option<OverrideLayer> {
    switch_override_layer(
        switch,
        enabled,
        &serde_json::to_value(&effective_config.config).expect("config serializes"),
        effective_config.origins.iter().map(|(key, origin)| {
            (
                key.as_str(),
                origin.name.precedence(),
                OverrideLayer::from_api(&origin.name, written),
            )
        }),
    )
}

#[test]
fn session_flags_win_over_what_config_read_reports() {
    let mut effective_config = config_read(serde_json::json!({
        "config": { "features": { "apps": false, "plugins": false } },
        "origins": {},
    }));
    let launch_overrides = HashMap::from([(
        "features".to_string(),
        serde_json::json!({ "connectors": true }),
    )]);

    let flagged = overlay_launch_overrides(&mut effective_config, Some(&launch_overrides));

    // The legacy `connectors` flag still turns apps on for every new thread.
    assert_eq!(
        (
            feature_enabled_from_effective_config(&effective_config, Feature::Apps),
            feature_enabled_from_effective_config(&effective_config, Feature::Plugins),
        ),
        (true, false)
    );
    assert_eq!(flagged, HashSet::from(["features.connectors".to_string()]));
}

#[test]
fn launch_web_search_wins_unless_a_higher_layer_sets_it() {
    let written = user_file("config.toml");
    let saved = || {
        config_read(serde_json::json!({
            "config": { "web_search": "disabled" },
            "origins": {
                "web_search": origin(ApiSource::User { file: written.clone(), profile: None }),
            },
        }))
    };
    let launch_overrides = HashMap::from([("web_search".to_string(), serde_json::json!("live"))]);

    let mut effective_config = saved();
    let flagged = overlay_launch_overrides(&mut effective_config, Some(&launch_overrides));
    assert_eq!(
        effective_config.config.web_search,
        Some(WebSearchMode::Live)
    );
    assert_eq!(flagged, HashSet::from(["web_search".to_string()]));

    let mut effective_config = saved();
    effective_config.origins.insert(
        "web_search".to_string(),
        serde_json::from_value(origin(ApiSource::LegacyManagedConfigTomlFromMdm))
            .expect("valid origin"),
    );
    let flagged = overlay_launch_overrides(&mut effective_config, Some(&launch_overrides));
    assert_eq!(
        effective_config.config.web_search,
        Some(WebSearchMode::Disabled)
    );
    assert!(flagged.is_empty());
}

#[test]
fn override_names_the_layer_that_opposes_the_save() {
    let written = user_file("config.toml");
    // Turning subagents on: the project keeps them off, while a managed layer only sets v2.
    let effective_config = config_read(serde_json::json!({
        "config": {
            "agents": { "enabled": false },
            "features": { "multi_agent": true, "multi_agent_v2": false },
        },
        "origins": {
            "agents.enabled": origin(ApiSource::Project {
                dot_codex_folder: user_file(".codex"),
            }),
            "features.multi_agent": origin(ApiSource::User { file: written.clone(), profile: None }),
            "features.multi_agent_v2": origin(ApiSource::LegacyManagedConfigTomlFromMdm),
        },
    }));

    assert_eq!(
        remote_override(
            SettingsSwitch::Subagents,
            /*enabled*/ true,
            &effective_config,
            &written,
        ),
        Some(OverrideLayer::Project)
    );
}

#[test]
fn override_under_a_profile_blames_the_main_config() {
    // With a profile selected, `/settings` writes the profile and a legacy alias in the main
    // config.toml still turns apps on.
    let main_config = user_file("config.toml");
    let profile = user_file("work.config.toml");
    let effective_config = config_read(serde_json::json!({
        "config": { "features": { "apps": false, "plugins": false, "connectors": true } },
        "origins": {
            "features.apps": origin(ApiSource::User {
                file: profile.clone(),
                profile: Some("work".to_string()),
            }),
            "features.plugins": origin(ApiSource::User {
                file: profile.clone(),
                profile: Some("work".to_string()),
            }),
            "features.connectors": origin(ApiSource::User { file: main_config, profile: None }),
        },
    }));

    assert_eq!(
        remote_override(
            SettingsSwitch::PluginsAndApps,
            /*enabled*/ false,
            &effective_config,
            &profile,
        ),
        Some(OverrideLayer::BaseUser)
    );
}
