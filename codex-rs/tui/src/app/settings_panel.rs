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
        let (notice, outcome) = if response.status == WriteStatus::OkOverridden {
            let message = overridden_write_message(&response).to_string();
            tracing::warn!(
                message,
                switch = label,
                "settings switch save was overridden"
            );
            if let Some(effective_config) = self
                .read_effective_config_after_overridden_write(app_server, "Settings changes")
                .await
            {
                adopt_effective_switch_state(&mut self.config, switch, &effective_config);
            }
            (
                format!("{label} saved, but it is overridden: {message}"),
                SaveOutcome::Failed,
            )
        } else {
            switch.adopt(&mut self.config, &next_config);
            (
                crate::settings_panel::saved_notice(switch, enabled),
                SaveOutcome::Saved,
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

/// Mirrors the server's effective values after a higher-priority layer overrode a save.
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
                "failed to sync effective feature state after an overridden settings save"
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
                    "failed to sync effective web search mode after an overridden settings save"
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
