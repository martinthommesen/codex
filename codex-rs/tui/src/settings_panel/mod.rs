//! Catalog of whole-feature kill switches shown by `/settings`.
//!
//! Each switch maps to server-owned config keys, so saves go through the app server instead of the
//! local TUI preferences used by `/config`. Running sessions keep the features they started with;
//! saved switches reach new sessions, while TUI-only surfaces (slash commands, mentions) update
//! immediately.

mod input;
mod render;

#[cfg(test)]
#[path = "settings_panel_tests.rs"]
mod tests;

use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ListSelectionView;
use crate::config_update::clear_config_value;
use crate::config_update::replace_config_value;
use crate::keymap::ListKeymap;
use crate::legacy_core::config::Config;
use codex_app_server_protocol::ConfigEdit;
use codex_features::Feature;
use codex_features::legacy_keys_for_feature;
use codex_protocol::config_types::WebSearchMode;

/// A feature that `/settings` can turn completely on or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsSwitch {
    PluginsAndApps,
    Skills,
    Hooks,
    AgentsMd,
    Memories,
    Subagents,
    WebSearch,
    ImageGeneration,
    Goals,
}

impl SettingsSwitch {
    pub(crate) const ALL: [Self; 9] = [
        Self::PluginsAndApps,
        Self::Skills,
        Self::Hooks,
        Self::AgentsMd,
        Self::Memories,
        Self::Subagents,
        Self::WebSearch,
        Self::ImageGeneration,
        Self::Goals,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::PluginsAndApps => "Plugins & apps",
            Self::Skills => "Skills",
            Self::Hooks => "Hooks",
            Self::AgentsMd => "AGENTS.md",
            Self::Memories => "Memories",
            Self::Subagents => "Subagents",
            Self::WebSearch => "Web search",
            Self::ImageGeneration => "Image generation",
            Self::Goals => "Goals",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::PluginsAndApps => {
                "Load plugins and connect apps, including their tools, skills, hooks, and mentions"
            }
            Self::Skills => {
                "Discover skills and offer them through the catalog, $ mentions, and /skills"
            }
            Self::Hooks => "Run lifecycle hooks from hooks.json, config.toml, and plugins",
            Self::AgentsMd => {
                "Load instructions from ~/.codex/AGENTS.md and project AGENTS.md files"
            }
            Self::Memories => "Remember useful context and reuse it in later sessions",
            Self::Subagents => "Let Codex spawn and coordinate subagents",
            Self::WebSearch => "Let Codex search the web",
            Self::ImageGeneration => "Let Codex generate images",
            Self::Goals => "Keep working toward a goal set with /goal across turns",
        }
    }

    fn keywords(self) -> &'static str {
        match self {
            Self::PluginsAndApps => "extensions connectors marketplace mcp $app",
            Self::Skills => "skill.md catalog $skill",
            Self::Hooks => "lifecycle hooks.json automation",
            Self::AgentsMd => "instructions project docs context init",
            Self::Memories => "memory remember",
            Self::Subagents => "agents multi-agent spawn delegate",
            Self::WebSearch => "internet browse search",
            Self::ImageGeneration => "images imagegen pictures",
            Self::Goals => "goal continuation long-running",
        }
    }

    fn section(self) -> SettingsSection {
        match self {
            Self::PluginsAndApps | Self::Skills | Self::Hooks => SettingsSection::Extensions,
            Self::AgentsMd | Self::Memories => SettingsSection::Context,
            Self::Subagents | Self::WebSearch | Self::ImageGeneration | Self::Goals => {
                SettingsSection::Tools
            }
        }
    }

    /// Feature flags this switch writes. Their TUI-side gating updates as soon as a save lands.
    pub(crate) fn features(self) -> &'static [Feature] {
        match self {
            Self::PluginsAndApps => &[Feature::Plugins, Feature::Apps],
            Self::Skills => &[Feature::Skills],
            Self::Hooks => &[Feature::CodexHooks],
            Self::AgentsMd => &[Feature::AgentsMd],
            Self::Memories => &[Feature::MemoryTool],
            Self::Subagents => &[Feature::Collab],
            Self::WebSearch => &[],
            Self::ImageGeneration => &[Feature::ImageGeneration],
            Self::Goals => &[Feature::Goals],
        }
    }

    /// Effective state of every key this switch writes.
    pub(crate) fn state(self, config: &Config) -> SwitchState {
        let mut values = self
            .features()
            .iter()
            .map(|feature| config.features.enabled(*feature))
            .collect::<Vec<_>>();
        match self {
            // `features.multi_agent_v2` overrides `agents.enabled = false`, so either one keeps
            // subagent tools available.
            Self::Subagents => {
                values.push(config.agents_enabled || config.features.enabled(Feature::MultiAgentV2))
            }
            Self::WebSearch => {
                values.push(config.web_search_mode.value() != WebSearchMode::Disabled)
            }
            Self::PluginsAndApps
            | Self::Skills
            | Self::Hooks
            | Self::AgentsMd
            | Self::Memories
            | Self::ImageGeneration
            | Self::Goals => {}
        }
        SwitchState::from_values(&values)
    }

    /// Turns the switch fully on or off in `config`, rejecting values pinned by requirements.
    pub(crate) fn apply(self, config: &mut Config, enabled: bool) -> Result<(), String> {
        for feature in self.features() {
            set_feature(config, *feature, enabled)?;
        }
        match self {
            Self::Subagents => {
                config.agents_enabled = enabled;
                if !enabled {
                    set_feature(config, Feature::MultiAgentV2, /*enabled*/ false)?;
                }
            }
            Self::WebSearch => {
                let mode = if enabled {
                    enabled_web_search_mode(config)
                        .ok_or_else(|| "requirements only allow disabled web search".to_string())?
                } else {
                    WebSearchMode::Disabled
                };
                config
                    .web_search_mode
                    .set(mode)
                    .map_err(|err| err.to_string())?;
            }
            Self::PluginsAndApps
            | Self::Skills
            | Self::Hooks
            | Self::AgentsMd
            | Self::Memories
            | Self::ImageGeneration
            | Self::Goals => {}
        }
        Ok(())
    }

    /// Copies only this switch's values, leaving unrelated settings unchanged.
    pub(crate) fn adopt(self, target: &mut Config, source: &Config) {
        let adopt_feature = |target: &mut Config, feature: Feature| {
            if let Err(err) = target
                .features
                .set_enabled(feature, source.features.enabled(feature))
            {
                tracing::warn!(error = %err, feature = feature.key(), "failed to adopt feature state");
            }
        };
        for feature in self.features() {
            adopt_feature(target, *feature);
        }
        match self {
            Self::Subagents => {
                target.agents_enabled = source.agents_enabled;
                adopt_feature(target, Feature::MultiAgentV2);
            }
            Self::WebSearch => {
                if let Err(err) = target.web_search_mode.set(source.web_search_mode.value()) {
                    tracing::warn!(error = %err, "failed to adopt web search mode");
                }
            }
            Self::PluginsAndApps
            | Self::Skills
            | Self::Hooks
            | Self::AgentsMd
            | Self::Memories
            | Self::ImageGeneration
            | Self::Goals => {}
        }
    }

    /// Explicit values for every key, so runtime enablement and legacy aliases cannot win.
    pub(crate) fn edits(self, enabled: bool, config: &Config) -> Vec<ConfigEdit> {
        let mut edits = Vec::new();
        for feature in self.features() {
            edits.push(feature_edit(*feature, enabled));
            edits.extend(
                legacy_keys_for_feature(*feature)
                    .map(|legacy_key| clear_config_value(format!("features.{legacy_key}"))),
            );
        }
        match self {
            Self::Subagents => {
                edits.push(replace_config_value(
                    "agents.enabled",
                    serde_json::json!(enabled),
                ));
                if !enabled && config.features.enabled(Feature::MultiAgentV2) {
                    edits.push(feature_edit(Feature::MultiAgentV2, /*enabled*/ false));
                }
            }
            // Clearing restores the default mode instead of guessing a previous one.
            Self::WebSearch if enabled => edits.push(clear_config_value("web_search")),
            Self::WebSearch => edits.push(replace_config_value(
                "web_search",
                serde_json::json!("disabled"),
            )),
            Self::PluginsAndApps
            | Self::Skills
            | Self::Hooks
            | Self::AgentsMd
            | Self::Memories
            | Self::ImageGeneration
            | Self::Goals => {}
        }
        edits
    }
}

fn feature_edit(feature: Feature, enabled: bool) -> ConfigEdit {
    replace_config_value(
        format!("features.{}", feature.key()),
        serde_json::json!(enabled),
    )
}

fn set_feature(config: &mut Config, feature: Feature, enabled: bool) -> Result<(), String> {
    config
        .features
        .set_enabled(feature, enabled)
        .map_err(|err| err.to_string())?;
    // Requirements restore pinned features silently, so compare the effective value.
    if config.features.enabled(feature) != enabled {
        return Err(format!(
            "`features.{}` is pinned by requirements",
            feature.key()
        ));
    }
    Ok(())
}

/// The mode an unset `web_search` resolves to: the default when allowed, else another allowed one.
pub(crate) fn enabled_web_search_mode(config: &Config) -> Option<WebSearchMode> {
    [
        WebSearchMode::Cached,
        WebSearchMode::Indexed,
        WebSearchMode::Live,
    ]
    .into_iter()
    .find(|mode| config.web_search_mode.can_set(mode).is_ok())
}

/// Combined state of the keys behind one switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchState {
    On,
    Off,
    /// Some keys are on and others off, for example plugins on with apps off.
    Partial,
}

impl SwitchState {
    fn from_values(values: &[bool]) -> Self {
        if values.iter().all(|enabled| *enabled) {
            Self::On
        } else if values.iter().any(|enabled| *enabled) {
            Self::Partial
        } else {
            Self::Off
        }
    }

    /// Activating a partially enabled switch turns everything on.
    fn toggled(self) -> bool {
        self != Self::On
    }
}

/// What the panel shows for one switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SwitchStatus {
    pub(crate) switch: SettingsSwitch,
    pub(crate) state: SwitchState,
    /// Value activating the row requests, or `None` when requirements pin every direction.
    pub(crate) target: Option<bool>,
}

impl SwitchStatus {
    fn locked(self) -> bool {
        self.target.is_none()
    }

    /// Requirements block the usual toggle direction.
    fn managed(self) -> bool {
        self.target != Some(self.state.toggled())
    }
}

pub(crate) fn statuses(config: &Config) -> Vec<SwitchStatus> {
    SettingsSwitch::ALL
        .into_iter()
        .map(|switch| {
            let state = switch.state(config);
            let can_apply = |enabled| switch.apply(&mut config.clone(), enabled).is_ok();
            let preferred = state.toggled();
            // A partial switch with a key pinned off can still turn everything else off.
            let target = if can_apply(preferred) {
                Some(preferred)
            } else if state == SwitchState::Partial && can_apply(!preferred) {
                Some(!preferred)
            } else {
                None
            };
            SwitchStatus {
                switch,
                state,
                target,
            }
        })
        .collect()
}

/// Notice shown after a switch saves without being overridden.
pub(crate) fn saved_notice(switch: SettingsSwitch, enabled: bool) -> String {
    format!(
        "{} turned {} for new sessions · use /new to start one",
        switch.label(),
        if enabled { "on" } else { "off" }
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SettingsSection {
    Extensions,
    Context,
    Tools,
}

impl SettingsSection {
    const ALL: [Self; 3] = [Self::Extensions, Self::Context, Self::Tools];

    fn title(self) -> &'static str {
        match self {
            Self::Extensions => "Extensions",
            Self::Context => "Context",
            Self::Tools => "Tools",
        }
    }
}

pub(crate) struct SettingsPanelUpdate {
    pub(crate) statuses: Vec<SwitchStatus>,
    pub(crate) keymap: ListKeymap,
    pub(crate) notice: Option<String>,
}

#[derive(Default)]
struct PickerState {
    tab: Option<String>,
    selected: Option<usize>,
    query: Option<String>,
    notice: Option<String>,
}

pub(crate) struct SettingsPanel {
    statuses: Vec<SwitchStatus>,
    picker: ListSelectionView,
    saving: bool,
    app_event_tx: AppEventSender,
}

impl SettingsPanel {
    pub(crate) fn new(
        statuses: Vec<SwitchStatus>,
        app_event_tx: AppEventSender,
        keymap: ListKeymap,
    ) -> Self {
        let picker = render::picker(
            &statuses,
            app_event_tx.clone(),
            keymap,
            PickerState::default(),
        );
        Self {
            statuses,
            picker,
            saving: false,
            app_event_tx,
        }
    }
}
