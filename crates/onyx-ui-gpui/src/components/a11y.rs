//! Accessibility plumbing shared by the components: reading-order focus rings for
//! modal surfaces and the labels spoken for file entries and list state.
//!
//! Roles, labels and states are applied through the `aria_*` API of `gpui-fast`
//! (AccessKit), which mirrors the `accessible-*` properties of the Slint UI.

use crate::explorer::OnyxExplorer;
use crate::theme::OnyxTheme;
use gpui_kit::{Context, FocusHandle, StyleRefinement, Styled, Window};
use onyx_core::{FileEntry, SortColumn, SortDirection, SortSpec, ViewMode};
use std::cell::Cell;

/// Focus slots reserved for the rename / new-folder / delete prompt.
const PROMPT_SLOTS: usize = 2;
/// Focus slots reserved for the command palette (its result list).
const PALETTE_SLOTS: usize = 1;
/// Focus slots reserved for the settings overlay: two header controls plus one
/// slot per selectable chip and one per excluded folder.
const SETTINGS_SLOTS: usize = 36;
/// Focus slots reserved for the context menu (the menu card itself).
const MENU_SLOTS: usize = 1;
/// Focus slots reserved for the archive browser: close, entry list, both actions.
const ARCHIVE_SLOTS: usize = 4;
/// Focus slots reserved for Quick Look: open, close.
const QUICK_LOOK_SLOTS: usize = 2;

/// One focusable stop inside a modal surface.
///
/// Handles are allocated up front so the surface can hand them out while it
/// renders from a shared `&OnyxExplorer`. A surface visits every stop once per
/// frame in reading order, which keeps the `Tab` sequence stable across frames
/// even when entries appear or disappear.
#[derive(Debug)]
pub(crate) struct FocusRing {
    handles: Vec<FocusHandle>,
    cursor: Cell<usize>,
    used: Cell<usize>,
    return_to: Option<FocusHandle>,
}

impl FocusRing {
    fn new(capacity: usize, cx: &mut Context<OnyxExplorer>) -> Self {
        Self {
            handles: (0..capacity).map(|_| cx.focus_handle()).collect(),
            cursor: Cell::new(0),
            used: Cell::new(0),
            return_to: None,
        }
    }

    /// Starts a new reading-order pass; call once at the top of the render.
    pub(crate) fn begin(&self) {
        self.cursor.set(0);
        self.used.set(0);
    }

    /// Takes the next focus handle in reading order.
    pub(crate) fn slot(&self) -> FocusHandle {
        let index = self.cursor.get();
        self.cursor.set(index + 1);
        self.used.set(self.used.get().max(index + 1));
        self.handles
            .get(index)
            .or_else(|| self.handles.last())
            .cloned()
            .unwrap_or_else(|| self.handles[0].clone())
    }

    /// Number of stops visited during the last render.
    fn count(&self) -> usize {
        self.used.get().min(self.handles.len())
    }

    /// Remembers the element that owned the keyboard focus before the surface opened.
    fn remember_focus(&mut self, window: &Window, cx: &Context<OnyxExplorer>) {
        self.return_to = window.focused(cx);
    }

    /// Moves the keyboard focus to the stop at `index`.
    pub(crate) fn focus_index(
        &self,
        index: usize,
        window: &mut Window,
        cx: &mut Context<OnyxExplorer>,
    ) {
        if let Some(handle) = self.handles.get(index) {
            window.focus(handle, cx);
        }
    }

    /// Index of the stop that currently owns the keyboard focus.
    fn focused_index(&self, window: &Window) -> Option<usize> {
        self.handles
            .iter()
            .position(|handle| handle.is_focused(window))
    }

    /// Moves the keyboard focus one stop forward or backward, wrapping inside the
    /// surface so `Tab` can never leave an open modal.
    fn step(&self, forward: bool, window: &mut Window, cx: &mut Context<OnyxExplorer>) {
        let next = next_stop(self.focused_index(window), forward, self.count().max(1));
        self.focus_index(next, window, cx);
    }

    /// Returns the keyboard focus to the element that opened the surface.
    pub(crate) fn restore_focus(
        &mut self,
        fallback: &FocusHandle,
        window: &mut Window,
        cx: &mut Context<OnyxExplorer>,
    ) {
        let target = self.return_to.take().unwrap_or_else(|| fallback.clone());
        window.focus(&target, cx);
    }
}

/// Focus rings of every modal surface, created once with the explorer.
#[derive(Debug)]
pub(crate) struct Modals {
    pub(crate) prompt: FocusRing,
    pub(crate) palette: FocusRing,
    pub(crate) settings: FocusRing,
    pub(crate) menu: FocusRing,
    pub(crate) archive: FocusRing,
    pub(crate) quick_look: FocusRing,
}

impl Modals {
    pub(crate) fn new(cx: &mut Context<OnyxExplorer>) -> Self {
        Self {
            prompt: FocusRing::new(PROMPT_SLOTS, cx),
            palette: FocusRing::new(PALETTE_SLOTS, cx),
            settings: FocusRing::new(SETTINGS_SLOTS, cx),
            menu: FocusRing::new(MENU_SLOTS, cx),
            archive: FocusRing::new(ARCHIVE_SLOTS, cx),
            quick_look: FocusRing::new(QUICK_LOOK_SLOTS, cx),
        }
    }
}

/// The focus ring of the modal surface that currently owns the keyboard.
impl Modals {
    pub(crate) fn ring_for(&self, surface: ModalSurface) -> &FocusRing {
        match surface {
            ModalSurface::ContextMenu => &self.menu,
            ModalSurface::Prompt => &self.prompt,
            ModalSurface::Palette => &self.palette,
            ModalSurface::Archive => &self.archive,
            ModalSurface::Settings => &self.settings,
            ModalSurface::QuickLook => &self.quick_look,
        }
    }

    pub(crate) fn ring_for_mut(&mut self, surface: ModalSurface) -> &mut FocusRing {
        match surface {
            ModalSurface::ContextMenu => &mut self.menu,
            ModalSurface::Prompt => &mut self.prompt,
            ModalSurface::Palette => &mut self.palette,
            ModalSurface::Archive => &mut self.archive,
            ModalSurface::Settings => &mut self.settings,
            ModalSurface::QuickLook => &mut self.quick_look,
        }
    }
}

/// Modal surfaces that trap the keyboard focus while open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalSurface {
    ContextMenu,
    Prompt,
    Palette,
    Archive,
    Settings,
    QuickLook,
}

/// Focus ring styling for keyboard-driven stops inside a modal.
pub(crate) fn focus_ring_style(theme: OnyxTheme, style: StyleRefinement) -> StyleRefinement {
    style
        .border_color(theme.border_focus())
        .bg(theme.accent_subtle())
}

/// Remembers the element that owns the keyboard focus before a modal surface
/// opens, so [`close_modal`] can hand it back.
pub(crate) fn remember_modal_focus(
    explorer: &mut OnyxExplorer,
    surface: ModalSurface,
    window: &Window,
    cx: &Context<OnyxExplorer>,
) {
    explorer
        .modals
        .ring_for_mut(surface)
        .remember_focus(window, cx);
}

/// Opens a modal surface: remembers the focus to return to and moves the
/// keyboard focus to `start`.
pub(crate) fn open_modal(
    explorer: &mut OnyxExplorer,
    surface: ModalSurface,
    start: usize,
    window: &mut Window,
    cx: &mut Context<OnyxExplorer>,
) {
    remember_modal_focus(explorer, surface, window, cx);
    explorer
        .modals
        .ring_for(surface)
        .focus_index(start, window, cx);
}

/// Moves the keyboard focus to one stop of a modal surface.
pub(crate) fn focus_modal_slot(
    explorer: &OnyxExplorer,
    surface: ModalSurface,
    index: usize,
    window: &mut Window,
    cx: &mut Context<OnyxExplorer>,
) {
    explorer
        .modals
        .ring_for(surface)
        .focus_index(index, window, cx);
}

/// Index of the modal stop that currently owns the keyboard focus.
pub(crate) fn modal_focused_slot(
    explorer: &OnyxExplorer,
    surface: ModalSurface,
    window: &Window,
) -> Option<usize> {
    explorer.modals.ring_for(surface).focused_index(window)
}

/// Whether a modal stop currently owns the keyboard focus.
pub(crate) fn modal_slot_is_focused(
    explorer: &OnyxExplorer,
    surface: ModalSurface,
    index: usize,
    window: &Window,
) -> bool {
    explorer.modals.ring_for(surface).focused_index(window) == Some(index)
}

/// Closes a modal surface and hands the keyboard focus back to the element that
/// opened it, falling back to the explorer root when that element is gone.
pub(crate) fn close_modal(
    explorer: &mut OnyxExplorer,
    surface: ModalSurface,
    window: &mut Window,
    cx: &mut Context<OnyxExplorer>,
) {
    let fallback = explorer.focus_handle.clone();
    explorer
        .modals
        .ring_for_mut(surface)
        .restore_focus(&fallback, window, cx);
}

/// Moves the keyboard focus inside the open modal surface, wrapping at the ends.
pub(crate) fn step_modal_focus(
    explorer: &OnyxExplorer,
    surface: ModalSurface,
    forward: bool,
    window: &mut Window,
    cx: &mut Context<OnyxExplorer>,
) {
    explorer.modals.ring_for(surface).step(forward, window, cx);
}

/// Index of the next focus stop, wrapping at both ends of `count`. Without a
/// current stop, forward starts at the first and backward at the last.
#[must_use]
pub(crate) fn next_stop(current: Option<usize>, forward: bool, count: usize) -> usize {
    let count = count.max(1);
    match (current, forward) {
        (Some(index), true) => (index + 1) % count,
        (Some(index), false) => (index + count - 1) % count,
        (None, true) => 0,
        (None, false) => count - 1,
    }
}

/// Label of a sort column as spoken to assistive technology.
#[must_use]
pub(crate) const fn sort_column_label(column: SortColumn) -> &'static str {
    match column {
        SortColumn::Name => "name",
        SortColumn::Modified => "date modified",
        SortColumn::Kind => "type",
        SortColumn::Size => "size",
    }
}

/// Current sort order, for the list and column-header descriptions.
#[must_use]
pub(crate) fn sort_description(spec: SortSpec) -> String {
    let direction = match spec.direction {
        SortDirection::Ascending => "ascending",
        SortDirection::Descending => "descending",
    };
    format!("sorted by {} {direction}", sort_column_label(spec.column))
}

/// View mode as spoken to assistive technology.
#[must_use]
pub(crate) const fn view_mode_label(mode: ViewMode) -> &'static str {
    match mode {
        ViewMode::Grid => "grid",
        ViewMode::Details | ViewMode::List => "details",
    }
}

/// Label of one file-list row: name, kind, size and modification date.
#[must_use]
pub(crate) fn entry_label(entry: &FileEntry, size_label: &str, modified: &str) -> String {
    let mut label = entry.name.clone();
    let kind = if entry.is_dir() {
        "folder".to_string()
    } else if entry.type_label.is_empty() {
        "file".to_string()
    } else {
        entry.type_label.clone()
    };
    label.push_str(", ");
    label.push_str(&kind);
    if !size_label.is_empty() {
        label.push_str(", ");
        label.push_str(size_label);
    }
    if !modified.is_empty() {
        label.push_str(", modified ");
        label.push_str(modified);
    }
    if entry.is_symlink {
        label.push_str(", shortcut");
    }
    if entry.attributes.hidden {
        label.push_str(", hidden");
    }
    label
}

/// Description of a file list: how many entries it holds, what is selected and
/// how it is presented.
#[must_use]
pub(crate) fn list_state_description(
    count: usize,
    selection: Option<&FileEntry>,
    spec: SortSpec,
    mode: ViewMode,
) -> String {
    let items = if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    };
    let selected = selection.map_or_else(
        || "nothing selected".to_string(),
        |entry| format!("selected {}", entry.name),
    );
    format!(
        "{items}, {selected}, {}, {} view",
        sort_description(spec),
        view_mode_label(mode)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_core::{FileAttributes, FileKind};
    use std::path::PathBuf;

    fn entry(name: &str, kind: FileKind, size: u64, type_label: &str) -> FileEntry {
        FileEntry {
            path: PathBuf::from(name),
            name: name.to_string(),
            extension: None,
            kind,
            size,
            modified: None,
            created: None,
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: type_label.to_string(),
        }
    }

    #[test]
    fn entry_labels_name_kind_size_and_date() {
        let file = entry("report.txt", FileKind::File, 1024, "Text Document");
        assert_eq!(
            entry_label(&file, "1.0 KB", "21.11.2025 10:04"),
            "report.txt, Text Document, 1.0 KB, modified 21.11.2025 10:04"
        );

        let folder = entry("Projects", FileKind::Directory, 0, "");
        assert_eq!(entry_label(&folder, "", ""), "Projects, folder");
    }

    #[test]
    fn list_state_reports_count_selection_sort_and_view() {
        let spec = SortSpec {
            column: SortColumn::Size,
            direction: SortDirection::Descending,
            folders_first: true,
        };
        assert_eq!(
            list_state_description(42, None, spec, ViewMode::Grid),
            "42 items, nothing selected, sorted by size descending, grid view"
        );

        let selected = entry("notes.md", FileKind::File, 12, "Markdown");
        assert_eq!(
            list_state_description(1, Some(&selected), SortSpec::default(), ViewMode::Details),
            "1 item, selected notes.md, sorted by name ascending, details view"
        );
    }

    #[test]
    fn focus_stops_wrap_and_start_at_both_ends() {
        assert_eq!(next_stop(None, true, 3), 0);
        assert_eq!(next_stop(None, false, 3), 2);
        assert_eq!(next_stop(Some(0), true, 3), 1);
        assert_eq!(next_stop(Some(2), true, 3), 0);
        assert_eq!(next_stop(Some(0), false, 3), 2);
        assert_eq!(next_stop(Some(1), false, 2), 0);
        // A surface with a single stop keeps the focus on it.
        assert_eq!(next_stop(Some(0), true, 1), 0);
        assert_eq!(next_stop(None, true, 0), 0);
    }

    #[test]
    fn sort_and_view_labels_cover_every_variant() {
        assert_eq!(sort_column_label(SortColumn::Name), "name");
        assert_eq!(sort_column_label(SortColumn::Modified), "date modified");
        assert_eq!(sort_column_label(SortColumn::Kind), "type");
        assert_eq!(sort_column_label(SortColumn::Size), "size");
        assert_eq!(view_mode_label(ViewMode::Details), "details");
        assert_eq!(view_mode_label(ViewMode::List), "details");
        assert_eq!(view_mode_label(ViewMode::Grid), "grid");
    }
}
