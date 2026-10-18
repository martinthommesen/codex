//! Render the kill-switch catalog as a tabbed, searchable panel.

use super::PickerState;
use super::SettingsPanel;
use super::SettingsSection;
use super::SwitchState;
use super::SwitchStatus;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::ColumnWidthMode;
use crate::bottom_pane::ListSelectionView;
use crate::bottom_pane::PickerSurface;
use crate::bottom_pane::SearchMode;
use crate::bottom_pane::SelectionDescriptionLayout;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionRowDisplay;
use crate::bottom_pane::SelectionTab;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::footer_hint_items_line;
use crate::config_panel::panel_header;
use crate::config_panel::panel_hint;
use crate::key_hint::KeyBinding;
use crate::keymap::ListKeymap;
use crate::render::renderable::Renderable;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use unicode_width::UnicodeWidthStr;

const DEFAULT_SUBTITLE: &str = "Turn whole features off · changes apply to new sessions";
const LOCKED_DESCRIPTION: &str = "Managed by your administrator";
/// Fits the longest value label, `[-] Partial · managed`, plus a gap.
const VALUE_COLUMN_WIDTH: usize = 22;

fn value_label(status: SwitchStatus) -> String {
    let label = match status.state {
        SwitchState::On => "[x] On",
        SwitchState::Off => "[ ] Off",
        SwitchState::Partial => "[-] Partial",
    };
    if status.locked {
        format!("{label} · managed")
    } else {
        label.to_string()
    }
}

#[derive(Clone, Copy)]
enum ItemContext {
    Group,
    Search,
}

fn item(
    status: SwitchStatus,
    keymap: &ListKeymap,
    search_hint: Option<KeyBinding>,
    context: ItemContext,
) -> SelectionItem {
    let switch = status.switch;
    let accept_label = (!status.locked).then_some("toggle");
    let description = if status.locked {
        LOCKED_DESCRIPTION
    } else {
        switch.description()
    };
    let description = match context {
        ItemContext::Group => description.to_string(),
        ItemContext::Search => format!("{} · {description}", switch.section().title()),
    };
    let value_label = value_label(status);
    let search_footer_hint = footer_hint_items_line(
        &accept_label
            .map(|label| ("enter".to_string(), label.to_string()))
            .into_iter()
            .chain([("esc".to_string(), "clear search".to_string())])
            .collect::<Vec<_>>(),
    );
    SelectionItem {
        name: switch.label().to_string(),
        category_tag: Some(format!("{value_label:<VALUE_COLUMN_WIDTH$}")),
        description: Some(description),
        is_disabled: status.locked,
        footer_hint: Some(panel_hint(keymap, search_hint, accept_label)),
        search_footer_hint: Some(search_footer_hint),
        search_value: Some(format!(
            "{} {} {} {} {}",
            switch.section().title(),
            switch.label(),
            value_label,
            switch.keywords(),
            switch.description()
        )),
        ..Default::default()
    }
}

pub(super) fn picker(
    statuses: &[SwitchStatus],
    app_event_tx: AppEventSender,
    keymap: ListKeymap,
    state: PickerState,
) -> ListSelectionView {
    let search = crate::key_hint::plain(KeyCode::Char('/'));
    let (search_code, search_modifiers) = search.parts();
    let search_event = KeyEvent::new(search_code, search_modifiers);
    let search_hint = (keymap.action_for(search_event).is_none()
        && !keymap.has_chord_prefix(search_event))
    .then_some(search);
    let subtitle = state
        .notice
        .clone()
        .unwrap_or_else(|| DEFAULT_SUBTITLE.to_string());
    let mut params = SelectionViewParams {
        picker_surface: PickerSurface::Panel,
        max_visible_rows: SettingsSection::ALL
            .iter()
            .map(|section| {
                statuses
                    .iter()
                    .filter(|status| status.switch.section() == *section)
                    .count()
            })
            .max()
            .unwrap_or_default(),
        reserve_result_rows: true,
        row_display: SelectionRowDisplay::SingleLine,
        col_width_mode: ColumnWidthMode::AutoAllRows,
        // The value column renders inside the description area, so reserve room for both.
        description_layout: SelectionDescriptionLayout::SelectedBelowWhenNarrow {
            min_description_width: 45 + VALUE_COLUMN_WIDTH as u16,
        },
        footer_hint: Some(panel_hint(&keymap, search_hint, /*accept_label*/ None)),
        is_searchable: true,
        search_mode: SearchMode::OnDemand(search),
        search_placeholder: Some("Type to search features".to_string()),
        search_items: statuses
            .iter()
            .map(|status| item(*status, &keymap, search_hint, ItemContext::Search))
            .collect(),
        // Row widths include the two-column selection marker; matching it keeps every tab's
        // value column aligned.
        name_column_width: statuses
            .iter()
            .map(|status| UnicodeWidthStr::width(status.switch.label()) + 2)
            .max(),
        tabs: SettingsSection::ALL
            .iter()
            .map(|section| {
                let title = section.title();
                SelectionTab {
                    id: title.to_string(),
                    label: title.to_string(),
                    header: panel_header("Settings", subtitle.clone()),
                    items: statuses
                        .iter()
                        .filter(|status| status.switch.section() == *section)
                        .map(|status| item(*status, &keymap, search_hint, ItemContext::Group))
                        .collect(),
                }
            })
            .collect(),
        ..Default::default()
    };
    params.initial_selected_idx = state.selected;
    params.initial_search_query = state.query;
    params.initial_tab_id = state.tab;
    ListSelectionView::new(params, app_event_tx, keymap)
}

impl Renderable for SettingsPanel {
    fn desired_height(&self, width: u16) -> u16 {
        self.picker.desired_height(width)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.picker.render(area, buf);
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.picker.cursor_pos(area)
    }
}
