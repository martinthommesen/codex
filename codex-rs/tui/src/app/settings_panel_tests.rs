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
) -> Option<(OverrideLayer, String)> {
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
        Some((OverrideLayer::Project, "agents.enabled".to_string()))
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
        Some((OverrideLayer::BaseUser, "features.connectors".to_string()))
    );
}

#[test]
fn override_notice_names_the_key_to_remove() {
    let (notice, outcome) = save_notice(
        SettingsSwitch::PluginsAndApps,
        /*enabled*/ false,
        SwitchState::Partial,
        SwitchState::Off,
        SaveCheck {
            overridden: None,
            unverified: None,
            winner: Some((OverrideLayer::BaseUser, "features.connectors".to_string())),
        },
    );

    assert_eq!(
        (notice.as_str(), outcome),
        (
            "Plugins & apps saved, but it is overridden: your main config.toml still sets `features.connectors`; remove it there, since /settings only writes the selected profile",
            SaveOutcome::Failed,
        )
    );
}

#[test]
fn unchecked_save_is_not_reported_as_a_plain_success() {
    let (notice, outcome) = save_notice(
        SettingsSwitch::Skills,
        /*enabled*/ false,
        SwitchState::Off,
        SwitchState::Off,
        SaveCheck {
            overridden: None,
            unverified: Some("config/read failed in TUI".to_string()),
            winner: None,
        },
    );

    assert_eq!(
        (notice.as_str(), outcome),
        (
            "Skills saved, but /settings could not check what new sessions will get: config/read failed in TUI",
            SaveOutcome::Failed,
        )
    );
}

#[test]
fn a_plain_bool_structured_feature_is_named_by_its_enabled_origin() {
    let written = user_file("config.toml");
    // Origins name `multi_agent_v2 = true` as `features.multi_agent_v2.enabled`.
    let effective_config = config_read(serde_json::json!({
        "config": {
            "agents": { "enabled": false },
            "features": { "multi_agent": false, "multi_agent_v2": true },
        },
        "origins": {
            "agents.enabled": origin(ApiSource::User { file: written.clone(), profile: None }),
            "features.multi_agent": origin(ApiSource::User { file: written.clone(), profile: None }),
            "features.multi_agent_v2.enabled": origin(ApiSource::Project {
                dot_codex_folder: user_file(".codex"),
            }),
        },
    }));

    assert_eq!(
        remote_override(
            SettingsSwitch::Subagents,
            /*enabled*/ false,
            &effective_config,
            &written,
        ),
        Some((
            OverrideLayer::Project,
            "features.multi_agent_v2.enabled".to_string()
        ))
    );
}

#[test]
fn launch_table_keeps_a_structured_feature_enabled() {
    let mut effective_config = config_read(serde_json::json!({
        "config": { "features": { "multi_agent_v2": true } },
        "origins": {},
    }));
    let launch_overrides = HashMap::from([(
        "features".to_string(),
        serde_json::json!({ "multi_agent_v2": { "max_concurrent_threads_per_session": 4 } }),
    )]);

    overlay_launch_overrides(&mut effective_config, Some(&launch_overrides));

    assert_eq!(
        effective_config.config.additional["features"]["multi_agent_v2"],
        serde_json::json!({ "enabled": true, "max_concurrent_threads_per_session": 4 })
    );
    assert!(feature_enabled_from_effective_config(
        &effective_config,
        Feature::MultiAgentV2
    ));
}

#[test]
fn legacy_search_features_in_a_profile_are_named_as_the_profile() {
    let launch_origins = HashMap::from([(
        "features.web_search_request".to_string(),
        codex_config::ConfigLayerMetadata {
            name: codex_config::ConfigLayerSource::User {
                file: user_file("work.config.toml"),
                profile: Some("work".to_string()),
            },
            version: "1".to_string(),
        },
    )]);

    assert_eq!(
        OverrideLayer::launch(launch_origin(&launch_origins, "web_search")),
        OverrideLayer::Profile
    );
    assert_eq!(
        OverrideLayer::launch(launch_origin(&launch_origins, "features.apps")),
        OverrideLayer::CommandLine
    );
}

#[test]
fn remote_web_search_follows_the_app_servers_requirements() {
    let live_only = [WebSearchMode::Live];
    let constraint = web_search_constraint(Some(&live_only), WebSearchMode::Cached);
    assert_eq!(constraint.value(), WebSearchMode::Live);
    assert!(constraint.can_set(&WebSearchMode::Disabled).is_ok());
    assert!(constraint.can_set(&WebSearchMode::Cached).is_err());
    assert!(
        web_search_constraint(/*modes*/ None, WebSearchMode::Indexed)
            .can_set(&WebSearchMode::Live)
            .is_ok()
    );

    let mut effective_config = config_read(serde_json::json!({
        "config": { "web_search": "live" },
        "origins": {},
    }));
    let cached_only = [WebSearchMode::Cached];
    assert!(constrain_web_search(
        &mut effective_config,
        Some(&cached_only)
    ));
    assert_eq!(
        effective_config.config.web_search,
        Some(WebSearchMode::Cached)
    );
    let disabled_only: [WebSearchMode; 0] = [];
    assert!(constrain_web_search(
        &mut effective_config,
        Some(&disabled_only)
    ));
    assert_eq!(
        effective_config.config.web_search,
        Some(WebSearchMode::Disabled)
    );
    assert!(!constrain_web_search(
        &mut effective_config,
        Some(&disabled_only)
    ));
}
