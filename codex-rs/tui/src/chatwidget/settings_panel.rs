//! Entry points for the `/settings` kill-switch panel.

use super::ChatWidget;
use crate::legacy_core::config::Config;
use crate::settings_panel::SettingsPanel;
use crate::settings_panel::SettingsPanelUpdate;
use crate::settings_panel::SettingsSwitch;

impl ChatWidget {
    pub(crate) fn open_settings_panel(&mut self) {
        self.bottom_pane.show_view(Box::new(SettingsPanel::new(
            crate::settings_panel::statuses(&self.config),
            self.app_event_tx.clone(),
            self.bottom_pane.list_keymap(),
        )));
    }

    pub(crate) fn refresh_settings_panel(&mut self, notice: Option<String>) -> bool {
        self.bottom_pane
            .update_settings_panel(&SettingsPanelUpdate {
                statuses: crate::settings_panel::statuses(&self.config),
                keymap: self.bottom_pane.list_keymap(),
                notice,
            })
    }

    /// Adopt a saved switch and refresh the TUI surfaces it gates.
    pub(crate) fn apply_settings_switch(&mut self, switch: SettingsSwitch, config: &Config) {
        // Flip features first so their change-driven refreshes see the previous value.
        for feature in switch.features() {
            self.set_feature_enabled(*feature, config.features.enabled(*feature));
        }
        switch.adopt(&mut self.config, config);
    }
}
