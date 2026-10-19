//! Snapshot each kill-switch group and check how switches map to config.

use super::*;
use crate::app_event::AppEvent;
use crate::bottom_pane::BottomPaneView;
use crate::keymap::RuntimeKeymap;
use crate::legacy_core::config::ManagedFeatures;
use crate::render::renderable::Renderable;
use codex_config::Constrained;
use codex_config::ConstraintError;
use codex_config::FeatureRequirementsToml;
use codex_config::RequirementSource;
use codex_config::Sourced;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::mpsc::unbounded_channel;

fn status(switch: SettingsSwitch, state: SwitchState) -> SwitchStatus {
    SwitchStatus {
        switch,
        state,
        target: Some(state.toggled()),
        managed: false,
    }
}

/// Every state the panel can show, spread across the three groups.
fn mixed_statuses() -> Vec<SwitchStatus> {
    SettingsSwitch::ALL
        .into_iter()
        .map(|switch| match switch {
            SettingsSwitch::PluginsAndApps => status(switch, SwitchState::Partial),
            SettingsSwitch::Hooks | SettingsSwitch::Memories => status(switch, SwitchState::Off),
            SettingsSwitch::WebSearch => SwitchStatus {
                target: None,
                managed: true,
                ..status(switch, SwitchState::On)
            },
            _ => status(switch, SwitchState::On),
        })
        .collect()
}

fn panel(statuses: Vec<SwitchStatus>) -> (SettingsPanel, UnboundedReceiver<AppEvent>) {
    let (tx, rx) = unbounded_channel();
    (
        SettingsPanel::new(
            statuses,
            AppEventSender::new(tx),
            RuntimeKeymap::defaults().list,
        ),
        rx,
    )
}

fn render(view: &impl Renderable, width: u16) -> String {
    let area = Rect::new(
        /*x*/ 0,
        /*y*/ 0,
        width,
        view.desired_height(width),
    );
    let mut buffer = Buffer::empty(area);
    view.render(area, &mut buffer);
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn expect_save(rx: &mut UnboundedReceiver<AppEvent>) -> (SettingsSwitch, bool) {
    match rx.try_recv().expect("save settings switch event") {
        AppEvent::SaveSettingsSwitch { switch, enabled } => (switch, enabled),
        other => panic!("expected SaveSettingsSwitch, got {other:?}"),
    }
}

async fn test_config() -> (tempfile::TempDir, Config) {
    let codex_home = tempfile::tempdir().expect("tempdir");
    let config = Config::load_default_with_cli_overrides_for_codex_home(
        codex_home.path().to_path_buf(),
        Vec::new(),
    )
    .await
    .expect("config");
    (codex_home, config)
}

#[test]
fn settings_panel_renders_each_group_at_a_stable_height() {
    let (mut panel, _rx) = panel(mixed_statuses());
    let height = panel.desired_height(/*width*/ 130);
    for section in SettingsSection::ALL {
        assert_eq!(panel.picker.active_tab_id(), Some(section.title()));
        assert_eq!(panel.desired_height(/*width*/ 130), height);
        insta::assert_snapshot!(
            format!("settings_{}", section.title()),
            render(&panel, /*width*/ 130)
        );
        panel.handle_key_event(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    }
}

#[test]
fn enter_requests_the_opposite_state_once_until_refreshed() {
    let (mut panel, mut rx) = panel(mixed_statuses());
    // Skills is on, so activating it turns it off.
    panel.handle_key_event(KeyCode::Down.into());
    panel.handle_key_event(KeyCode::Enter.into());
    assert_eq!(expect_save(&mut rx), (SettingsSwitch::Skills, false));
    assert!(panel.saving);

    panel.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err(), "a save is already in flight");

    let mut statuses = mixed_statuses();
    statuses[1] = status(SettingsSwitch::Skills, SwitchState::Off);
    assert!(panel.update_settings_panel(&SettingsPanelUpdate {
        statuses,
        keymap: RuntimeKeymap::defaults().list,
        notice: Some(saved_notice(
            SettingsSwitch::Skills,
            /*enabled*/ false,
            SwitchState::Off,
        )),
    }));
    assert!(!panel.saving);
    insta::assert_snapshot!("settings_saved_notice", render(&panel, /*width*/ 130));

    panel.handle_key_event(KeyCode::Enter.into());
    assert_eq!(expect_save(&mut rx), (SettingsSwitch::Skills, true));
}

#[test]
fn partial_switch_turns_everything_on() {
    let (mut panel, mut rx) = panel(mixed_statuses());
    panel.handle_key_event(KeyCode::Enter.into());
    assert_eq!(expect_save(&mut rx), (SettingsSwitch::PluginsAndApps, true));
}

#[test]
fn locked_switch_does_not_save() {
    let (mut panel, mut rx) = panel(mixed_statuses());
    panel.handle_key_event(KeyCode::Char('/').into());
    assert!(panel.handle_paste("web search".to_string()));
    panel.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err());
    assert!(!panel.saving);
}

#[test]
fn search_finds_switches_by_keyword_across_groups() {
    let (mut panel, mut rx) = panel(mixed_statuses());
    panel.handle_key_event(KeyCode::Char('/').into());
    assert!(panel.handle_paste("connectors".to_string()));
    insta::assert_snapshot!("settings_search", render(&panel, /*width*/ 130));
    panel.handle_key_event(KeyCode::Enter.into());
    assert_eq!(expect_save(&mut rx), (SettingsSwitch::PluginsAndApps, true));

    // A match outside the first tab maps through the full list, not the tab's rows.
    let (mut panel, mut rx) = self::panel(mixed_statuses());
    panel.handle_key_event(KeyCode::Char('/').into());
    assert!(panel.handle_paste("memory".to_string()));
    panel.handle_key_event(KeyCode::Enter.into());
    assert_eq!(expect_save(&mut rx), (SettingsSwitch::Memories, true));
}

#[tokio::test]
async fn switches_read_and_apply_config() {
    let (_codex_home, mut config) = test_config().await;
    let defaults = statuses(&config)
        .into_iter()
        .map(|status| (status.switch, status.state))
        .collect::<Vec<_>>();
    assert_eq!(
        defaults,
        SettingsSwitch::ALL
            .into_iter()
            .map(|switch| {
                let state = if switch == SettingsSwitch::Memories {
                    SwitchState::Off
                } else {
                    SwitchState::On
                };
                (switch, state)
            })
            .collect::<Vec<_>>()
    );

    config
        .features
        .set_enabled(Feature::Apps, /*enabled*/ false)
        .expect("apps is not pinned");
    assert_eq!(
        SettingsSwitch::PluginsAndApps.state(&config),
        SwitchState::Partial
    );

    for switch in SettingsSwitch::ALL {
        switch
            .apply(&mut config, /*enabled*/ false)
            .expect("nothing is pinned");
        assert_eq!(switch.state(&config), SwitchState::Off, "{switch:?}");
    }
    assert!(!config.agents_enabled);
    assert_eq!(config.web_search_mode.value(), WebSearchMode::Disabled);
    assert!(!config.features.enabled(Feature::Plugins));

    let mut source = config.clone();
    SettingsSwitch::Subagents
        .apply(&mut source, /*enabled*/ true)
        .expect("nothing is pinned");
    SettingsSwitch::Subagents.adopt(&mut config, &source);
    assert_eq!(SettingsSwitch::Subagents.state(&config), SwitchState::On);
    assert_eq!(SettingsSwitch::WebSearch.state(&config), SwitchState::Off);
}

#[tokio::test]
async fn edits_write_explicit_values_and_clear_legacy_aliases() {
    let (_codex_home, config) = test_config().await;
    let edits = |switch: SettingsSwitch, enabled: bool| {
        switch
            .edits(enabled, &config)
            .into_iter()
            .map(|edit| (edit.key_path, edit.value))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        edits(SettingsSwitch::PluginsAndApps, /*enabled*/ false),
        vec![
            ("features.plugins".to_string(), json!(false)),
            ("features.apps".to_string(), json!(false)),
            ("features.connectors".to_string(), json!(null)),
        ]
    );
    assert_eq!(
        edits(SettingsSwitch::Memories, /*enabled*/ false),
        vec![
            ("features.memories".to_string(), json!(false)),
            ("features.memory_tool".to_string(), json!(null)),
        ]
    );
    assert_eq!(
        edits(SettingsSwitch::Subagents, /*enabled*/ true),
        vec![
            ("features.multi_agent".to_string(), json!(true)),
            ("features.collab".to_string(), json!(null)),
            ("agents.enabled".to_string(), json!(true)),
        ]
    );
    assert_eq!(
        edits(SettingsSwitch::Subagents, /*enabled*/ false),
        vec![
            ("features.multi_agent".to_string(), json!(false)),
            ("features.collab".to_string(), json!(null)),
            ("agents.enabled".to_string(), json!(false)),
            ("features.multi_agent_v2".to_string(), json!(false)),
        ]
    );
    assert_eq!(
        edits(SettingsSwitch::WebSearch, /*enabled*/ false),
        vec![("web_search".to_string(), json!("disabled"))]
    );
    assert_eq!(
        edits(SettingsSwitch::WebSearch, /*enabled*/ true),
        vec![("web_search".to_string(), json!("cached"))]
    );
    assert_eq!(
        edits(SettingsSwitch::Skills, /*enabled*/ false),
        vec![("features.skills".to_string(), json!(false))]
    );
}

#[test]
fn narrow_panel_keeps_descriptions_readable() {
    let (panel, _rx) = panel(mixed_statuses());
    insta::assert_snapshot!("settings_narrow", render(&panel, /*width*/ 70));
}

#[tokio::test]
async fn pinned_features_lock_only_the_blocked_direction() {
    let (_codex_home, mut config) = test_config().await;
    let mut warnings = Vec::new();
    config.features = ManagedFeatures::from_configured_with_warnings(
        config.features.get().clone(),
        Some(Sourced {
            value: FeatureRequirementsToml {
                entries: [("apps".to_string(), false), ("skills".to_string(), true)]
                    .into_iter()
                    .collect(),
            },
            source: RequirementSource::Unknown,
        }),
        &mut warnings,
    )
    .expect("pins are valid");

    let status_of = |config: &Config, switch: SettingsSwitch| {
        statuses(config)
            .into_iter()
            .find(|status| status.switch == switch)
            .expect("every switch has a status")
    };
    let plugins = SettingsSwitch::PluginsAndApps;

    // Apps cannot turn on, but plugins can still turn off and back on.
    assert_eq!(
        status_of(&config, plugins),
        SwitchStatus {
            switch: plugins,
            state: SwitchState::Partial,
            target: Some(false),
            managed: true,
        }
    );
    plugins
        .apply(&mut config, /*enabled*/ false)
        .expect("plugins can turn off");
    assert_eq!(
        status_of(&config, plugins),
        SwitchStatus {
            switch: plugins,
            state: SwitchState::Off,
            target: Some(true),
            managed: true,
        }
    );
    assert!(
        plugins
            .edits(/*enabled*/ true, &config)
            .into_iter()
            .map(|edit| (edit.key_path, edit.value))
            .eq([("features.plugins".to_string(), json!(true))]),
        "the pinned apps key is not written"
    );
    plugins
        .apply(&mut config, /*enabled*/ true)
        .expect("plugins can turn back on");
    assert_eq!(
        status_of(&config, plugins),
        SwitchStatus {
            switch: plugins,
            state: SwitchState::Partial,
            target: Some(false),
            managed: true,
        }
    );
    assert_eq!(
        saved_notice(plugins, /*enabled*/ true, SwitchState::Partial),
        "Plugins & apps turned on for new sessions where your administrator allows it · use /new to start one"
    );

    // A switch whose only key is pinned cannot change in either direction.
    assert_eq!(
        status_of(&config, SettingsSwitch::Skills),
        SwitchStatus {
            switch: SettingsSwitch::Skills,
            state: SwitchState::On,
            target: None,
            managed: true,
        }
    );
    assert!(
        SettingsSwitch::Skills
            .apply(&mut config, /*enabled*/ false)
            .is_err()
    );
}

#[tokio::test]
async fn web_search_turns_on_with_a_mode_requirements_allow() {
    let (_codex_home, mut config) = test_config().await;
    config.web_search_mode = Constrained::new(WebSearchMode::Disabled, |mode| match mode {
        WebSearchMode::Live | WebSearchMode::Disabled => Ok(()),
        WebSearchMode::Cached | WebSearchMode::Indexed => Err(ConstraintError::InvalidValue {
            field_name: "web_search",
            candidate: format!("{mode:?}"),
            allowed: "[live, disabled]".to_string(),
            requirement_source: RequirementSource::Unknown,
        }),
    })
    .expect("disabled is allowed");

    let web_search = statuses(&config)[6];
    assert_eq!(
        (web_search.switch, web_search.state, web_search.target),
        (SettingsSwitch::WebSearch, SwitchState::Off, Some(true))
    );
    SettingsSwitch::WebSearch
        .apply(&mut config, /*enabled*/ true)
        .expect("live is allowed");
    assert_eq!(config.web_search_mode.value(), WebSearchMode::Live);
}

#[tokio::test]
async fn subagents_switch_also_turns_off_multi_agent_v2() {
    let (_codex_home, mut config) = test_config().await;
    config
        .features
        .set_enabled(Feature::MultiAgentV2, /*enabled*/ true)
        .expect("not pinned");
    config.agents_enabled = false;
    config
        .features
        .set_enabled(Feature::Collab, /*enabled*/ false)
        .expect("not pinned");
    // V2 overrides `agents.enabled = false`, so subagents are still on.
    assert_eq!(SettingsSwitch::Subagents.state(&config), SwitchState::On);
    assert!(
        SettingsSwitch::Subagents
            .edits(/*enabled*/ false, &config)
            .into_iter()
            .any(|edit| edit.key_path == "features.multi_agent_v2" && edit.value == json!(false))
    );

    SettingsSwitch::Subagents
        .apply(&mut config, /*enabled*/ false)
        .expect("not pinned");
    assert_eq!(SettingsSwitch::Subagents.state(&config), SwitchState::Off);
    assert!(!config.features.enabled(Feature::MultiAgentV2));
}

#[tokio::test]
async fn partial_subagents_turn_on_when_nothing_is_pinned() {
    let (_codex_home, mut config) = test_config().await;
    // The old way to turn subagents off leaves `agents.enabled` on, so the model catalog decides.
    config
        .features
        .set_enabled(Feature::Collab, /*enabled*/ false)
        .expect("not pinned");
    let subagents = SettingsSwitch::Subagents;
    assert_eq!(
        statuses(&config)
            .into_iter()
            .find(|status| status.switch == subagents),
        Some(SwitchStatus {
            switch: subagents,
            state: SwitchState::Partial,
            target: Some(true),
            managed: false,
        })
    );

    subagents
        .apply(&mut config, /*enabled*/ true)
        .expect("nothing is pinned");
    assert_eq!(subagents.state(&config), SwitchState::On);
}

#[tokio::test]
async fn subagents_turn_off_and_on_while_multi_agent_is_pinned_on() {
    let (_codex_home, mut config) = test_config().await;
    let mut warnings = Vec::new();
    config.features = ManagedFeatures::from_configured_with_warnings(
        config.features.get().clone(),
        Some(Sourced {
            value: FeatureRequirementsToml {
                entries: [("multi_agent".to_string(), true)].into_iter().collect(),
            },
            source: RequirementSource::Unknown,
        }),
        &mut warnings,
    )
    .expect("pins are valid");
    let subagents = SettingsSwitch::Subagents;

    // `agents.enabled = false` still turns subagents off, so the pin does not lock the switch.
    subagents
        .apply(&mut config, /*enabled*/ false)
        .expect("agents.enabled can turn subagents off");
    assert_eq!(subagents.state(&config), SwitchState::Off);
    assert!(config.features.enabled(Feature::Collab));
    assert!(
        !subagents
            .edits(/*enabled*/ false, &config)
            .into_iter()
            .any(|edit| edit.key_path == "features.multi_agent"),
        "the pinned key is not written"
    );
    assert_eq!(
        statuses(&config)
            .into_iter()
            .find(|status| status.switch == subagents)
            .map(|status| status.target),
        Some(Some(true))
    );
    subagents
        .apply(&mut config, /*enabled*/ true)
        .expect("agents.enabled can turn subagents back on");
    assert_eq!(subagents.state(&config), SwitchState::On);
}

#[tokio::test]
async fn subagents_turn_off_while_multi_agent_is_pinned_off() {
    let (_codex_home, mut config) = test_config().await;
    let mut warnings = Vec::new();
    config.features = ManagedFeatures::from_configured_with_warnings(
        config.features.get().clone(),
        Some(Sourced {
            value: FeatureRequirementsToml {
                entries: [("multi_agent".to_string(), false)].into_iter().collect(),
            },
            source: RequirementSource::Unknown,
        }),
        &mut warnings,
    )
    .expect("pins are valid");
    let subagents = SettingsSwitch::Subagents;
    let status_of = |config: &Config| {
        statuses(config)
            .into_iter()
            .find(|status| status.switch == subagents)
            .expect("subagents has a status")
    };

    // Models with a catalog version still get subagents, so the switch is partial and can still
    // turn them off through `agents.enabled`.
    assert_eq!(
        status_of(&config),
        SwitchStatus {
            switch: subagents,
            state: SwitchState::Partial,
            target: Some(false),
            managed: true,
        }
    );
    subagents
        .apply(&mut config, /*enabled*/ false)
        .expect("agents.enabled can turn subagents off");
    assert_eq!(
        status_of(&config),
        SwitchStatus {
            switch: subagents,
            state: SwitchState::Off,
            target: Some(true),
            managed: true,
        }
    );
    subagents
        .apply(&mut config, /*enabled*/ true)
        .expect("agents.enabled can turn subagents back on");
    assert_eq!(subagents.state(&config), SwitchState::Partial);
}
