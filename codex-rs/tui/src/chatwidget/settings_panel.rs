//! Entry points for the `/settings` kill-switch panel.

use super::ChatWidget;
use crate::legacy_core::config::Config;
use crate::settings_panel::SettingsPanel;
use crate::settings_panel::SettingsPanelUpdate;
use crate::settings_panel::SettingsSwitch;
use crate::settings_panel::SwitchStatus;

impl ChatWidget {
    /// Shows saved values, which can differ from what the running session started with.
    pub(crate) fn open_settings_panel(&mut self, statuses: Vec<SwitchStatus>) {
        self.bottom_pane.show_view(Box::new(SettingsPanel::new(
            statuses,
            self.app_event_tx.clone(),
            self.bottom_pane.list_keymap(),
        )));
    }

    /// Returns whether an open panel received the update.
    pub(crate) fn refresh_settings_panel(
        &mut self,
        statuses: Vec<SwitchStatus>,
        notice: Option<String>,
    ) -> bool {
        self.bottom_pane
            .update_settings_panel(&SettingsPanelUpdate {
                statuses,
                keymap: self.bottom_pane.list_keymap(),
                notice,
            })
    }

    /// Points at `/settings`, unless requirements keep the feature off there too.
    pub(super) fn turn_back_on_hint(&self, switch: SettingsSwitch, hint: &str) -> String {
        if switch
            .apply(&mut self.config.clone(), /*enabled*/ true)
            .is_ok()
        {
            hint.to_string()
        } else {
            format!("{} is managed by your administrator.", switch.label())
        }
    }

    /// Adopt a saved switch and refresh the TUI surfaces it gates.
    pub(crate) fn apply_settings_switch(&mut self, switch: SettingsSwitch, config: &Config) {
        if switch == SettingsSwitch::Goals {
            // The running session keeps pursuing its goal, so keep its status and /goal controls.
            return;
        }
        // Flip features first so their change-driven refreshes see the previous value.
        for feature in switch.features() {
            self.set_feature_enabled(*feature, config.features.enabled(*feature));
        }
        switch.adopt(&mut self.config, config);
    }
}
