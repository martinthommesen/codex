//! Persists `/settings` kill switches through the app server and mirrors saved values locally.

use super::config_persistence::feature_enabled_from_effective_config;
use super::config_persistence::overridden_write_message;
use super::*;
use crate::config_update::format_config_error;
use crate::settings_panel::SettingsSwitch;
use crate::settings_panel::SwitchState;
use codex_protocol::config_types::WebSearchMode;

/// How a settings save ended, which decides where its notice can be shown.
#[derive(Debug, PartialEq, Eq)]
enum SaveOutcome {
    Saved,
    Failed,
}

impl App {
    pub(super) async fn save_settings_switch(
        &mut self,
        app_server: &mut AppServerSession,
        switch: SettingsSwitch,
        enabled: bool,
    ) {
        let label = switch.label();
        // Stage locally first so values pinned by requirements fail before anything is written.
        let mut next_config = self.config.clone();
        if let Err(err) = switch.apply(&mut next_config, enabled) {
            tracing::warn!(error = %err, switch = label, "settings switch rejected by requirements");
            self.report_settings_save(
                format!("{label} is managed by your administrator: {err}"),
                SaveOutcome::Failed,
            );
            return;
        }
        let response = match crate::config_update::write_config_batch(
            app_server.request_handle(),
            switch.edits(enabled, &self.config),
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                let error = format_config_error(&err);
                tracing::error!(error = %error, switch = label, "failed to save settings switch");
                self.report_settings_save(
                    format!("Could not save {label}: {error}"),
                    SaveOutcome::Failed,
                );
                return;
            }
        };
        let overridden = (response.status == WriteStatus::OkOverridden)
            .then(|| overridden_write_message(&response).to_string());
        // The server only checks user-level layers for overrides, so a trusted project, a profile,
        // this TUI's launch flags or a legacy alias in another layer can still win. Resolve what
        // new sessions will actually get.
        let written = &response.file_path;
        let mut winner = None;
        let unverified = if app_server.uses_remote_workspace() {
            // New remote sessions start where /new starts them, not in the displayed thread's cwd.
            let cwd = app_server.remote_cwd_override().unwrap_or(Path::new("."));
            match crate::config_update::read_effective_config(
                app_server.request_handle(),
                cwd.display().to_string(),
            )
            .await
            {
                Ok(mut effective_config) => {
                    // The server cannot see the launch choices this TUI sends with each new thread.
                    let launch_overrides =
                        crate::app_server_session::config_request_overrides_from_config(
                            &self.config,
                            app_server.thread_params_mode(),
                        );
                    let flagged =
                        overlay_launch_overrides(&mut effective_config, launch_overrides.as_ref());
                    adopt_effective_switch_state(&mut self.config, switch, &effective_config);
                    let launch_origins = self.config.config_layer_stack.origins();
                    let flag_precedence =
                        codex_app_server_protocol::ConfigLayerSource::SessionFlags.precedence();
                    winner = switch_override_layer(
                        switch,
                        enabled,
                        &serde_json::to_value(&effective_config.config).unwrap_or_default(),
                        effective_config
                            .origins
                            .iter()
                            .map(|(key, origin)| {
                                let layer = OverrideLayer::from_api(&origin.name, written);
                                (key.as_str(), origin.name.precedence(), layer)
                            })
                            .chain(flagged.iter().map(|key| {
                                let layer = OverrideLayer::launch(launch_origins.get(key));
                                (key.as_str(), flag_precedence, Some(layer))
                            })),
                    );
                    None
                }
                Err(err) => {
                    tracing::warn!(error = %err, switch = label, "failed to read config after a settings save");
                    Some(format_config_error(&err))
                }
            }
        } else {
            // The app server shares this machine's files, so build what /new will load.
            let cwd = self.chat_widget.config_ref().cwd.to_path_buf();
            match self.rebuild_config_for_cwd(cwd).await {
                Ok(rebuilt) => {
                    switch.adopt(&mut self.config, &rebuilt);
                    let layers = &rebuilt.config_layer_stack;
                    winner = switch_override_layer(
                        switch,
                        enabled,
                        &serde_json::to_value(layers.effective_config()).unwrap_or_default(),
                        layers.origins().iter().map(|(key, origin)| {
                            (
                                key.as_str(),
                                origin.name.precedence(),
                                OverrideLayer::from_config(&origin.name, written),
                            )
                        }),
                    );
                    None
                }
                Err(err) => {
                    tracing::warn!(error = %err, switch = label, "failed to rebuild config after a settings save");
                    Some(format_config_error(&err))
                }
            }
        };
        if unverified.is_some() && overridden.is_none() {
            switch.adopt(&mut self.config, &next_config);
        }
        let (notice, outcome) = save_notice(
            switch,
            enabled,
            switch.state(&self.config),
            switch.state(&next_config),
            SaveCheck {
                overridden,
                unverified,
                winner,
            },
        );
        self.chat_widget.apply_settings_switch(switch, &self.config);
        self.report_settings_save(notice, outcome);
    }

    /// Shows the notice in the panel, or in the transcript when the panel was already closed.
    fn report_settings_save(&mut self, notice: String, outcome: SaveOutcome) {
        let statuses = crate::settings_panel::statuses(&self.config);
        if self
            .chat_widget
            .refresh_settings_panel(statuses, Some(notice.clone()))
        {
            return;
        }
        match outcome {
            SaveOutcome::Saved => self.chat_widget.add_info_message(notice, /*hint*/ None),
            SaveOutcome::Failed => self.chat_widget.add_error_message(notice),
        }
    }
}

/// What a save's check against new sessions found.
struct SaveCheck {
    /// The app server's own override message, when it reported one.
    overridden: Option<String>,
    /// Why new sessions could not be checked, when they could not.
    unverified: Option<String>,
    /// The layer and key that still win, when another layer overrides the save.
    winner: Option<(OverrideLayer, String)>,
}

/// The notice for a written save. `resolved` is the switch's state for new sessions and
/// `requested` the state the save staged.
fn save_notice(
    switch: SettingsSwitch,
    enabled: bool,
    resolved: SwitchState,
    requested: SwitchState,
    check: SaveCheck,
) -> (String, SaveOutcome) {
    let label = switch.label();
    if resolved == requested {
        if let Some(err) = check.unverified {
            return (
                format!(
                    "{label} saved, but /settings could not check what new sessions will get: {err}"
                ),
                SaveOutcome::Failed,
            );
        }
        return (
            crate::settings_panel::saved_notice(switch, enabled, resolved),
            SaveOutcome::Saved,
        );
    }
    let reason = check.overridden.unwrap_or_else(|| match check.winner {
        Some((layer, key)) => layer.describe(&key),
        None => "another config file still sets it".to_string(),
    });
    tracing::warn!(
        reason,
        switch = label,
        "settings switch save was overridden"
    );
    (
        format!("{label} saved, but it is overridden: {reason}"),
        SaveOutcome::Failed,
    )
}

/// Applies the launch choices this TUI sends with every new thread on top of what a remote app
/// server read, below the layers that outrank them. Returns the origin keys they now decide.
fn overlay_launch_overrides(
    effective_config: &mut ConfigReadResponse,
    launch_overrides: Option<&HashMap<String, serde_json::Value>>,
) -> HashSet<String> {
    let mut flagged = HashSet::new();
    let Some(launch_overrides) = launch_overrides else {
        return flagged;
    };
    let session_flags_precedence =
        codex_app_server_protocol::ConfigLayerSource::SessionFlags.precedence();
    let outranked =
        |key: &str, origins: &HashMap<String, codex_app_server_protocol::ConfigLayerMetadata>| {
            origins.iter().any(|(origin_key, origin)| {
                (origin_key == key || origin_key.starts_with(&format!("{key}.")))
                    && origin.name.precedence() > session_flags_precedence
            })
        };
    if let Some(mode) = launch_overrides.get("web_search")
        && !outranked("web_search", &effective_config.origins)
        && let Ok(mode) = serde_json::from_value::<WebSearchMode>(mode.clone())
    {
        effective_config.config.web_search = Some(mode);
        flagged.insert("web_search".to_string());
    }
    let Some(session_features) = launch_overrides
        .get("features")
        .and_then(serde_json::Value::as_object)
    else {
        return flagged;
    };
    let mut overlays = Vec::new();
    for (name, value) in session_features {
        let key = format!("features.{name}");
        if !outranked(&key, &effective_config.origins) {
            overlays.push((name.clone(), value.clone(), key));
        }
    }
    let features = effective_config
        .config
        .additional
        .entry("features".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let Some(features) = features.as_object_mut() else {
        return flagged;
    };
    for (name, value, key) in overlays {
        // Merge tables such as `[features.multi_agent_v2]` instead of replacing them, and record
        // their leaf keys the way config origins do.
        match (features.get_mut(&name), value) {
            (Some(serde_json::Value::Object(existing)), serde_json::Value::Object(value)) => {
                flagged.extend(value.keys().map(|sub_key| format!("{key}.{sub_key}")));
                existing.extend(value);
            }
            (_, serde_json::Value::Object(value)) => {
                flagged.extend(value.keys().map(|sub_key| format!("{key}.{sub_key}")));
                features.insert(name, serde_json::Value::Object(value));
            }
            (_, value) => {
                features.insert(name, value);
                flagged.insert(key);
            }
        }
    }
    flagged
}

/// The kind of config layer that still decides a switch after a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverrideLayer {
    CommandLine,
    Project,
    Profile,
    /// The main `config.toml` under a selected profile, whose file `/settings` writes instead.
    BaseUser,
    Managed,
    Other,
}

impl OverrideLayer {
    /// `None` for the user config file `/settings` wrote.
    fn from_config(
        source: &codex_config::ConfigLayerSource,
        written: &AbsolutePathBuf,
    ) -> Option<Self> {
        use codex_config::ConfigLayerSource as Source;
        match source {
            Source::User { file, .. } if file == written => None,
            Source::User {
                profile: Some(_), ..
            } => Some(Self::Profile),
            Source::User { profile: None, .. } => Some(Self::BaseUser),
            Source::SessionFlags => Some(Self::CommandLine),
            Source::Project { .. } => Some(Self::Project),
            Source::Mdm { .. }
            | Source::EnterpriseManaged { .. }
            | Source::LegacyManagedConfigTomlFromFile { .. }
            | Source::LegacyManagedConfigTomlFromMdm => Some(Self::Managed),
            Source::PackagedDefaults { .. } | Source::System { .. } => Some(Self::Other),
        }
    }

    /// `None` for the user config file `/settings` wrote.
    fn from_api(
        source: &codex_app_server_protocol::ConfigLayerSource,
        written: &AbsolutePathBuf,
    ) -> Option<Self> {
        use codex_app_server_protocol::ConfigLayerSource as Source;
        match source {
            Source::User { file, .. } if file == written => None,
            Source::User {
                profile: Some(_), ..
            } => Some(Self::Profile),
            Source::User { profile: None, .. } => Some(Self::BaseUser),
            Source::SessionFlags => Some(Self::CommandLine),
            Source::Project { .. } => Some(Self::Project),
            Source::Mdm { .. }
            | Source::EnterpriseManaged { .. }
            | Source::LegacyManagedConfigTomlFromFile { .. }
            | Source::LegacyManagedConfigTomlFromMdm => Some(Self::Managed),
            Source::PackagedDefaults { .. } | Source::System { .. } => Some(Self::Other),
        }
    }

    /// Where a launch choice this TUI forwards to a remote app server came from.
    fn launch(origin: Option<&codex_config::ConfigLayerMetadata>) -> Self {
        match origin.map(|origin| &origin.name) {
            Some(codex_config::ConfigLayerSource::User {
                profile: Some(_), ..
            }) => Self::Profile,
            _ => Self::CommandLine,
        }
    }

    fn describe(self, key: &str) -> String {
        match self {
            Self::CommandLine => {
                format!("a command-line flag for this Codex session still sets `{key}`")
            }
            Self::Project => format!("this project's .codex/config.toml still sets `{key}`"),
            Self::Profile => format!("the selected profile still sets `{key}`"),
            // `/settings` writes only the selected profile, so the user has to edit this file.
            Self::BaseUser => format!(
                "your main config.toml still sets `{key}`; remove it there, since /settings only writes the selected profile"
            ),
            Self::Managed => format!("a managed config still sets `{key}`"),
            Self::Other => format!("another config file still sets `{key}`"),
        }
    }
}

/// The highest-precedence layer, other than the file `/settings` wrote, whose resolved value still
/// keeps the switch from reaching `enabled`, with the key it sets.
fn switch_override_layer<'a>(
    switch: SettingsSwitch,
    enabled: bool,
    values: &serde_json::Value,
    origins: impl Iterator<Item = (&'a str, i16, Option<OverrideLayer>)>,
) -> Option<(OverrideLayer, String)> {
    origins
        .filter(|(key, ..)| {
            value_at(values, key).is_some_and(|value| opposes_save(switch, enabled, key, value))
        })
        .filter_map(|(key, precedence, layer)| Some((precedence, key, layer?)))
        // Ties break on the key so the notice does not depend on map order.
        .max_by(|(a_precedence, a_key, _), (b_precedence, b_key, _)| {
            a_precedence.cmp(b_precedence).then(b_key.cmp(a_key))
        })
        .map(|(_, key, layer)| (layer, key.to_string()))
}

/// Whether `key` resolving to `value` keeps the switch from reaching `enabled`.
fn opposes_save(
    switch: SettingsSwitch,
    enabled: bool,
    key: &str,
    value: &serde_json::Value,
) -> bool {
    let sets_feature = |feature: Feature| {
        let Some(name) = key.strip_prefix("features.") else {
            return false;
        };
        let name = name.strip_suffix(".enabled").unwrap_or(name);
        std::iter::once(feature.key())
            .chain(codex_features::legacy_keys_for_feature(feature))
            .any(|feature_key| feature_key == name)
    };
    let pulls_away = value.as_bool().is_some_and(|value| value != enabled);
    match switch {
        SettingsSwitch::WebSearch => {
            key == "web_search"
                && value
                    .as_str()
                    .is_some_and(|mode| (mode == "disabled") == enabled)
        }
        // Subagents need `agents.enabled` and `multi_agent` to turn on, and `agents.enabled` and
        // `multi_agent_v2` off to turn off.
        SettingsSwitch::Subagents => {
            let deciding_feature = if enabled {
                Feature::Collab
            } else {
                Feature::MultiAgentV2
            };
            pulls_away && (key == "agents.enabled" || sets_feature(deciding_feature))
        }
        SettingsSwitch::PluginsAndApps
        | SettingsSwitch::Skills
        | SettingsSwitch::Hooks
        | SettingsSwitch::AgentsMd
        | SettingsSwitch::Memories
        | SettingsSwitch::ImageGeneration
        | SettingsSwitch::Goals => {
            pulls_away
                && switch
                    .features()
                    .iter()
                    .any(|feature| sets_feature(*feature))
        }
    }
}

/// The value at a dotted config key such as `features.apps`.
fn value_at<'a>(values: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    key.split('.')
        .try_fold(values, |value, part| value.get(part))
}

/// Mirrors the effective values this folder resolves to after a save.
fn adopt_effective_switch_state(
    config: &mut Config,
    switch: SettingsSwitch,
    effective_config: &ConfigReadResponse,
) {
    let adopt_feature = |config: &mut Config, feature: Feature| {
        let enabled = feature_enabled_from_effective_config(effective_config, feature);
        if let Err(err) = config.features.set_enabled(feature, enabled) {
            tracing::warn!(
                error = %err,
                feature = feature.key(),
                "failed to sync effective feature state after a settings save"
            );
        }
    };
    for feature in switch.features() {
        adopt_feature(config, *feature);
    }
    match switch {
        SettingsSwitch::Subagents => {
            adopt_feature(config, Feature::MultiAgentV2);
            config.agents_enabled = effective_config
                .config
                .additional
                .get("agents")
                .and_then(|agents| agents.get("enabled"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(/*default*/ true);
        }
        SettingsSwitch::WebSearch => {
            let mode = effective_config
                .config
                .web_search
                .or_else(|| crate::settings_panel::enabled_web_search_mode(config))
                .unwrap_or(WebSearchMode::Disabled);
            if let Err(err) = config.web_search_mode.set(mode) {
                tracing::warn!(
                    error = %err,
                    "failed to sync effective web search mode after a settings save"
                );
            }
        }
        SettingsSwitch::PluginsAndApps
        | SettingsSwitch::Skills
        | SettingsSwitch::Hooks
        | SettingsSwitch::AgentsMd
        | SettingsSwitch::Memories
        | SettingsSwitch::ImageGeneration
        | SettingsSwitch::Goals => {}
    }
}

#[cfg(test)]
#[path = "settings_panel_tests.rs"]
mod tests;
