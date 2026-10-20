//! Catalog of whole-feature kill switches shown by `/settings`.
//!
//! Each switch maps to server-owned config keys, so saves go through the app server instead of the
//! local TUI preferences used by `/config`. The model context of a running session keeps the
//! features it started with, and saved switches reach new sessions. The TUI's own commands and
//! mentions follow the saved values right away, as the app server's lists do; goal controls follow
//! the goal state the server reports for each thread.

mod input;
mod render;
mod switch;

#[cfg(test)]
#[path = "settings_panel_tests.rs"]
mod tests;

use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ListSelectionView;
use crate::keymap::ListKeymap;
pub(crate) use switch::ENABLED_WEB_SEARCH_MODES;
use switch::SettingsSection;
pub(crate) use switch::SettingsSwitch;
pub(crate) use switch::SwitchState;
pub(crate) use switch::SwitchStatus;
pub(crate) use switch::enabled_web_search_mode;
pub(crate) use switch::saved_notice;
pub(crate) use switch::statuses;

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
