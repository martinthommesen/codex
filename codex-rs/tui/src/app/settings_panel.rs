//! Persists `/settings` kill switches through the app server and mirrors saved values locally.

use super::config_persistence::feature_enabled_from_effective_config;
use super::config_persistence::overridden_write_message;
use super::*;
use crate::config_update::format_config_error;
use crate::settings_panel::SettingsSwitch;
use codex_protocol::config_types::WebSearchMode;

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
            self.chat_widget.refresh_settings_panel(Some(format!(
                "{label} is managed by your administrator: {err}"
            )));
            return;
        }
        let response = match crate::config_update::write_config_batch(
            app_server.request_handle(),
            switch.edits(enabled),
        )
        .await
        {
            Ok(response) => response,
            Err(err) => {
                let error = format_config_error(&err);
                tracing::error!(error = %error, switch = label, "failed to save settings switch");
                self.chat_widget
                    .refresh_settings_panel(Some(format!("Could not save {label}: {error}")));
                return;
            }
        };
        let notice = if response.status == WriteStatus::OkOverridden {
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
            format!("{label} saved, but it is overridden: {message}")
        } else {
            switch.adopt(&mut self.config, &next_config);
            crate::settings_panel::saved_notice(switch, enabled)
        };
        self.chat_widget.apply_settings_switch(switch, &self.config);
        self.chat_widget.refresh_settings_panel(Some(notice));
    }
}

/// Mirrors the server's effective values after a higher-priority layer overrode a save.
fn adopt_effective_switch_state(
    config: &mut Config,
    switch: SettingsSwitch,
    effective_config: &ConfigReadResponse,
) {
    for feature in switch.features() {
        let enabled = feature_enabled_from_effective_config(effective_config, *feature);
        if let Err(err) = config.features.set_enabled(*feature, enabled) {
            tracing::warn!(
                error = %err,
                feature = feature.key(),
                "failed to sync effective feature state after an overridden settings save"
            );
        }
    }
    match switch {
        SettingsSwitch::Subagents => {
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
                .unwrap_or(WebSearchMode::Cached);
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
