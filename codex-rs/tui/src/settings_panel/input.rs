//! Delegate navigation to the shared list picker and request saves for toggled switches.

use super::PickerState;
use super::SettingsPanel;
use super::SettingsPanelUpdate;
use super::SettingsSection;
use crate::app_event::AppEvent;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::keymap::KeymapContext;
use crate::keymap::KeymapContextSet;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;

impl SettingsPanel {
    fn status_index(&self, selected: usize) -> Option<usize> {
        if self
            .picker
            .search_query()
            .is_some_and(|query| !query.is_empty())
        {
            return self.statuses.get(selected).map(|_| selected);
        }
        let tab = self
            .picker
            .active_tab_id()
            .unwrap_or(SettingsSection::Extensions.title());
        self.statuses
            .iter()
            .enumerate()
            .filter(|(_, status)| status.switch.section().title() == tab)
            .nth(selected)
            .map(|(index, _)| index)
    }

    fn activate(&mut self, selected: usize) {
        if self.saving {
            return;
        }
        let Some(status) = self
            .status_index(selected)
            .and_then(|index| self.statuses.get(index))
        else {
            return;
        };
        if status.locked {
            return;
        }
        self.saving = true;
        self.app_event_tx.send(AppEvent::SaveSettingsSwitch {
            switch: status.switch,
            enabled: status.state.toggled(),
        });
    }
}

impl BottomPaneView for SettingsPanel {
    fn update_settings_panel(&mut self, update: &SettingsPanelUpdate) -> bool {
        let state = PickerState {
            tab: self.picker.active_tab_id().map(str::to_string),
            selected: self.picker.selected_index(),
            query: self.picker.search_query().map(str::to_string),
            notice: update.notice.clone(),
        };
        self.statuses = update.statuses.clone();
        self.saving = false;
        self.picker = super::render::picker(
            &self.statuses,
            self.app_event_tx.clone(),
            update.keymap.clone(),
            state,
        );
        true
    }

    fn keymap_contexts(&self) -> KeymapContextSet {
        KeymapContextSet::new(KeymapContext::List)
    }

    fn handle_key_event(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        self.picker.handle_key_event(key);
        if let Some(selected) = self.picker.take_last_selected_index() {
            self.activate(selected);
        }
    }

    fn handle_paste(&mut self, text: String) -> bool {
        self.picker.handle_paste(text)
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        true
    }

    fn is_complete(&self) -> bool {
        self.picker.is_complete()
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.picker.on_ctrl_c()
    }
}
