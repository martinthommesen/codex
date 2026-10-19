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
        // The server only checks user-level layers for overrides, so a trusted project, a profile
        // or a legacy alias in another layer can still win. Read back what this cwd resolves to.
        let cwd = self.chat_widget.config_ref().cwd.display().to_string();
        let session_flags = session_flag_features(&self.config);
        match crate::config_update::read_effective_config(app_server.request_handle(), cwd).await {
            Ok(mut effective_config) => {
                overlay_session_flag_features(&mut effective_config, session_flags.as_ref());
                adopt_effective_switch_state(&mut self.config, switch, &effective_config);
            }
            Err(err) => {
                tracing::warn!(error = %err, switch = label, "failed to read config after a settings save");
                if overridden.is_none() {
                    switch.adopt(&mut self.config, &next_config);
                }
            }
        }
        let (notice, outcome) = if switch.state(&self.config) == switch.state(&next_config) {
            (
                crate::settings_panel::saved_notice(switch, enabled, switch.state(&self.config)),
                SaveOutcome::Saved,
            )
        } else {
            let flagged = session_flags.as_ref().is_some_and(|flags| {
                switch.features().iter().any(|feature| {
                    std::iter::once(feature.key())
                        .chain(codex_features::legacy_keys_for_feature(*feature))
                        .any(|key| flags.contains_key(key))
                })
            });
            let reason = overridden.unwrap_or_else(|| {
                if flagged {
                    "a command-line flag for this Codex session still sets it".to_string()
                } else {
                    "another config file still sets it for this folder".to_string()
                }
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

/// New threads also get this TUI's session flags, which `config/read` cannot see when the app
/// server runs separately, so apply them on top of what the server read.
fn overlay_session_flag_features(
    effective_config: &mut ConfigReadResponse,
    session_flags: Option<&serde_json::Map<String, serde_json::Value>>,
) {
    let Some(session_flags) = session_flags else {
        return;
    };
    let features = effective_config
        .config
        .additional
        .entry("features".to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(features) = features.as_object_mut() {
        features.extend(session_flags.clone());
    }
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
