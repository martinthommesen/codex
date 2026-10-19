//! Persists `/settings` kill switches through the app server and mirrors saved values locally.

use super::config_persistence::feature_enabled_from_effective_config;
use super::config_persistence::overridden_write_message;
use super::*;
use crate::config_update::format_config_error;
use crate::settings_panel::SettingsSwitch;
use codex_protocol::config_types::WebSearchMode;

/// How a settings save ended, which decides where its notice can be shown.
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
        // new sessions in this folder will actually get.
        let cwd = self.chat_widget.config_ref().cwd.to_path_buf();
        let mut winning_layer = None;
        let resolved = if app_server.uses_remote_workspace() {
            match crate::config_update::read_effective_config(
                app_server.request_handle(),
                cwd.display().to_string(),
            )
            .await
            {
                Ok(mut effective_config) => {
                    // The server cannot see the flags this TUI sends with each new thread.
                    let flagged = overlay_session_flag_features(
                        &mut effective_config,
                        session_flag_features(&self.config).as_ref(),
                    );
                    adopt_effective_switch_state(&mut self.config, switch, &effective_config);
                    let flag_precedence =
                        codex_app_server_protocol::ConfigLayerSource::SessionFlags.precedence();
                    winning_layer = switch_override_layer(
                        switch,
                        effective_config
                            .origins
                            .iter()
                            .map(|(key, origin)| {
                                let layer = OverrideLayer::from_api(&origin.name);
                                (key.as_str(), origin.name.precedence(), layer)
                            })
                            .chain(flagged.iter().map(|key| {
                                (
                                    key.as_str(),
                                    flag_precedence,
                                    Some(OverrideLayer::CommandLine),
                                )
                            })),
                    );
                    true
                }
                Err(err) => {
                    tracing::warn!(error = %err, switch = label, "failed to read config after a settings save");
                    false
                }
            }
        } else {
            // The app server shares this machine's files, so build what /new will load.
            match self.rebuild_config_for_cwd(cwd).await {
                Ok(rebuilt) => {
                    switch.adopt(&mut self.config, &rebuilt);
                    winning_layer = switch_override_layer(
                        switch,
                        rebuilt
                            .config_layer_stack
                            .origins()
                            .iter()
                            .map(|(key, origin)| {
                                (
                                    key.as_str(),
                                    origin.name.precedence(),
                                    OverrideLayer::from_config(&origin.name),
                                )
                            }),
                    );
                    true
                }
                Err(err) => {
                    tracing::warn!(error = %err, switch = label, "failed to rebuild config after a settings save");
                    false
                }
            }
        };
        if !resolved && overridden.is_none() {
            switch.adopt(&mut self.config, &next_config);
        }
        let (notice, outcome) = if switch.state(&self.config) == switch.state(&next_config) {
            (
                crate::settings_panel::saved_notice(switch, enabled, switch.state(&self.config)),
                SaveOutcome::Saved,
            )
        } else {
            let reason = overridden.unwrap_or_else(|| {
                winning_layer
                    .unwrap_or(OverrideLayer::Other)
                    .describe()
                    .to_string()
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
        };
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

/// The `[features]` session flags this TUI sends with every new thread.
fn session_flag_features(config: &Config) -> Option<serde_json::Map<String, serde_json::Value>> {
    let mut session_flags = toml::Value::Table(toml::Table::new());
    for layer in config.config_layer_stack.layers_low_to_high() {
        if matches!(layer.name, codex_config::ConfigLayerSource::SessionFlags) {
            codex_config::merge_toml_values(&mut session_flags, &layer.config);
        }
    }
    let features = serde_json::to_value(session_flags.get("features")?).ok()?;
    features.as_object().cloned()
}

/// Applies this TUI's session flags on top of what a remote app server read, below the layers
/// that outrank session flags. Returns the origin keys the flags now decide.
fn overlay_session_flag_features(
    effective_config: &mut ConfigReadResponse,
    session_flags: Option<&serde_json::Map<String, serde_json::Value>>,
) -> std::collections::HashSet<String> {
    let mut flagged = std::collections::HashSet::new();
    let Some(session_flags) = session_flags else {
        return flagged;
    };
    let session_flags_precedence =
        codex_app_server_protocol::ConfigLayerSource::SessionFlags.precedence();
    let outranked = |key: &str,
                     origins: &std::collections::HashMap<
        String,
        codex_app_server_protocol::ConfigLayerMetadata,
    >| {
        origins.iter().any(|(origin_key, origin)| {
            (origin_key == key || origin_key.starts_with(&format!("{key}.")))
                && origin.name.precedence() > session_flags_precedence
        })
    };
    let mut overlays = Vec::new();
    for (name, value) in session_flags {
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
        // Merge tables such as `[features.multi_agent_v2]` instead of replacing them.
        match (features.get_mut(&name), value) {
            (Some(serde_json::Value::Object(existing)), serde_json::Value::Object(value)) => {
                existing.extend(value);
            }
            (_, value) => {
                features.insert(name, value);
            }
        }
        flagged.insert(key);
    }
    flagged
}

/// The kind of config layer that still decides a switch after a save.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverrideLayer {
    CommandLine,
    Project,
    Profile,
    Managed,
    Other,
}

impl OverrideLayer {
    /// `None` for the user config that `/settings` writes.
    fn from_config(source: &codex_config::ConfigLayerSource) -> Option<Self> {
        use codex_config::ConfigLayerSource as Source;
        match source {
            Source::User { profile: None, .. } => None,
            Source::SessionFlags => Some(Self::CommandLine),
            Source::Project { .. } => Some(Self::Project),
            Source::User {
                profile: Some(_), ..
            } => Some(Self::Profile),
            Source::Mdm { .. }
            | Source::EnterpriseManaged { .. }
            | Source::LegacyManagedConfigTomlFromFile { .. }
            | Source::LegacyManagedConfigTomlFromMdm => Some(Self::Managed),
            Source::PackagedDefaults { .. } | Source::System { .. } => Some(Self::Other),
        }
    }

    /// `None` for the user config that `/settings` writes.
    fn from_api(source: &codex_app_server_protocol::ConfigLayerSource) -> Option<Self> {
        use codex_app_server_protocol::ConfigLayerSource as Source;
        match source {
            Source::User { profile: None, .. } => None,
            Source::SessionFlags => Some(Self::CommandLine),
            Source::Project { .. } => Some(Self::Project),
            Source::User {
                profile: Some(_), ..
            } => Some(Self::Profile),
            Source::Mdm { .. }
            | Source::EnterpriseManaged { .. }
            | Source::LegacyManagedConfigTomlFromFile { .. }
            | Source::LegacyManagedConfigTomlFromMdm => Some(Self::Managed),
            Source::PackagedDefaults { .. } | Source::System { .. } => Some(Self::Other),
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::CommandLine => "a command-line flag for this Codex session still sets it",
            Self::Project => "this project's .codex/config.toml still sets it",
            Self::Profile => "the selected profile still sets it",
            Self::Managed => "a managed config still sets it",
            Self::Other => "another config file still sets it",
        }
    }
}

/// The highest-precedence layer, other than the user config `/settings` writes, that sets one of
/// the switch's keys.
fn switch_override_layer<'a>(
    switch: SettingsSwitch,
    origins: impl Iterator<Item = (&'a str, i16, Option<OverrideLayer>)>,
) -> Option<OverrideLayer> {
    let keys = switch_keys(switch);
    origins
        .filter_map(|(origin_key, precedence, layer)| {
            let matches = keys
                .iter()
                .any(|key| origin_key == key || origin_key.starts_with(&format!("{key}.")));
            Some((precedence, layer?)).filter(|_| matches)
        })
        .max_by_key(|(precedence, _)| *precedence)
        .map(|(_, layer)| layer)
}

/// Config keys whose value can decide the switch, including legacy aliases.
fn switch_keys(switch: SettingsSwitch) -> Vec<String> {
    let mut keys = switch
        .features()
        .iter()
        .flat_map(|feature| {
            std::iter::once(feature.key()).chain(codex_features::legacy_keys_for_feature(*feature))
        })
        .map(|key| format!("features.{key}"))
        .collect::<Vec<_>>();
    match switch {
        SettingsSwitch::Subagents => {
            keys.extend([
                "features.multi_agent_v2".to_string(),
                "agents.enabled".to_string(),
            ]);
        }
        SettingsSwitch::WebSearch => keys.push("web_search".to_string()),
        SettingsSwitch::PluginsAndApps
        | SettingsSwitch::Skills
        | SettingsSwitch::Hooks
        | SettingsSwitch::AgentsMd
        | SettingsSwitch::Memories
        | SettingsSwitch::ImageGeneration
        | SettingsSwitch::Goals => {}
    }
    keys
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
