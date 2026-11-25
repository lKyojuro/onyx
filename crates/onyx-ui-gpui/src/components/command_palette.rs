//! Command Palette modal (`Ctrl+K`) matching `CommandPaletteModal` in
//! `crates/onyx-ui/ui/components/overlays.slint`: a top-centered card with a
//! query field, a category-grouped command list with shortcuts, and
//! arrow/Enter/Escape handling. A query that starts with [`FILE_SEARCH_PREFIX`]
//! switches the card to the indexed file search mode.

use crate::components::a11y;
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::search_index::{SearchRun, SearchStatus};
use crate::theme::{FONT_SM, FONT_XS, OnyxTheme, RADIUS_LG, RADIUS_SM};
use gpui_kit::{
    Context, Entity, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, Role,
    StatefulInteractiveElement, Styled,
    component::input::{Input, InputState},
    deferred, div,
    prelude::FluentBuilder,
    px,
};
use onyx_core::{FileEntry, normalize_display_path};

const CARD_WIDTH: f32 = 560.0;
const CARD_HEIGHT: f32 = 420.0;
const CARD_TOP: f32 = 68.0;
const ROW_HEIGHT: f32 = 34.0;
/// Deferred layer for the palette: above the nav suggestions, below the
/// context menu.
const PALETTE_LAYER: usize = 25;

/// Maximum number of matches shown at once; the list scrolls by re-filtering
/// instead of scrolling, which keeps arrow-key navigation unambiguous.
pub const MAX_VISIBLE_MATCHES: usize = 9;

/// Maximum number of file hits the palette renders for one search run.
pub const MAX_VISIBLE_HITS: usize = 12;

/// Query prefix that switches the palette to the indexed file search.
pub const FILE_SEARCH_PREFIX: char = '?';

/// What the palette lists for the current query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PaletteMode {
    #[default]
    Commands,
    /// Indexed file search: every row is a search hit of the open folder tree.
    Files,
}

/// Palette mode selected by `query`'s prefix.
#[must_use]
pub fn palette_mode(query: &str) -> PaletteMode {
    if query.trim_start().starts_with(FILE_SEARCH_PREFIX) {
        PaletteMode::Files
    } else {
        PaletteMode::Commands
    }
}

/// The file query of a [`PaletteMode::Files`] palette text, without the prefix.
#[must_use]
pub fn file_search_query(query: &str) -> &str {
    query
        .trim_start()
        .strip_prefix(FILE_SEARCH_PREFIX)
        .unwrap_or(query)
        .trim()
}

/// Static metadata of one palette entry. The dispatch for `id` lives in
/// [`crate::explorer::OnyxExplorer::run_palette_command`].
#[derive(Debug, Clone, Copy)]
pub struct PaletteCommand {
    pub id: &'static str,
    pub title: &'static str,
    pub shortcut: &'static str,
    pub category: &'static str,
}

/// All palette commands, mirroring `crates/onyx-app/src/commands.rs`.
pub const COMMANDS: &[PaletteCommand] = &[
    PaletteCommand {
        id: "settings",
        title: "Open Settings",
        shortcut: "Ctrl+,",
        category: "Preferences",
    },
    PaletteCommand {
        id: "new-tab",
        title: "New Tab",
        shortcut: "Ctrl+T",
        category: "Navigation",
    },
    PaletteCommand {
        id: "close-tab",
        title: "Close Tab",
        shortcut: "Ctrl+W",
        category: "Navigation",
    },
    PaletteCommand {
        id: "refresh",
        title: "Refresh",
        shortcut: "F5",
        category: "Navigation",
    },
    PaletteCommand {
        id: "up",
        title: "Go Up One Folder",
        shortcut: "Alt+Up",
        category: "Navigation",
    },
    PaletteCommand {
        id: "favorite",
        title: "Pin or Unpin Favorite",
        shortcut: "",
        category: "Navigation",
    },
    PaletteCommand {
        id: "quick-look",
        title: "Quick Look",
        shortcut: "Space",
        category: "View",
    },
    PaletteCommand {
        id: "theme",
        title: "Toggle Dark / Light Theme",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "view-mode",
        title: "Toggle Details / Grid View",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "hidden",
        title: "Toggle Hidden Files",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "system-files",
        title: "Toggle Protected System Files",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "sidebar",
        title: "Toggle Sidebar",
        shortcut: "Ctrl+B",
        category: "View",
    },
    PaletteCommand {
        id: "dual",
        title: "Toggle Dual Pane",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "swap-panes",
        title: "Swap Panes",
        shortcut: "Ctrl+Shift+S",
        category: "View",
    },
    PaletteCommand {
        id: "preview",
        title: "Toggle Preview Pane",
        shortcut: "Alt+P",
        category: "View",
    },
    PaletteCommand {
        id: "folder-sizes",
        title: "Toggle Folder Size Calculation",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "new-folder",
        title: "New Folder",
        shortcut: "Ctrl+Shift+N",
        category: "Files",
    },
    PaletteCommand {
        id: "rename",
        title: "Rename",
        shortcut: "F2",
        category: "Files",
    },
    PaletteCommand {
        id: "duplicate",
        title: "Duplicate",
        shortcut: "",
        category: "Files",
    },
    PaletteCommand {
        id: "copy",
        title: "Copy",
        shortcut: "Ctrl+C",
        category: "Files",
    },
    PaletteCommand {
        id: "cut",
        title: "Cut",
        shortcut: "Ctrl+X",
        category: "Files",
    },
    PaletteCommand {
        id: "paste",
        title: "Paste",
        shortcut: "Ctrl+V",
        category: "Files",
    },
    PaletteCommand {
        id: "recycle",
        title: "Move to Recycle Bin",
        shortcut: "Delete",
        category: "Files",
    },
    PaletteCommand {
        id: "copy-path",
        title: "Copy Path",
        shortcut: "",
        category: "Files",
    },
    PaletteCommand {
        id: "properties",
        title: "Properties",
        shortcut: "Alt+Enter",
        category: "Files",
    },
    PaletteCommand {
        id: "terminal",
        title: "Open Terminal",
        shortcut: "Ctrl+`",
        category: "Tools",
    },
    PaletteCommand {
        id: "reset-layout",
        title: "Reset Layout",
        shortcut: "",
        category: "View",
    },
    PaletteCommand {
        id: "compress-zip",
        title: "Compress to ZIP",
        shortcut: "",
        category: "Archive",
    },
    PaletteCommand {
        id: "extract-here",
        title: "Extract Here",
        shortcut: "",
        category: "Archive",
    },
    PaletteCommand {
        id: "extract-to-folder",
        title: "Extract to Folder",
        shortcut: "",
        category: "Archive",
    },
    PaletteCommand {
        id: "browse-archive",
        title: "Browse Archive",
        shortcut: "",
        category: "Archive",
    },
    PaletteCommand {
        id: "index",
        title: "Toggle Indexed Search",
        shortcut: "",
        category: "Search",
    },
    PaletteCommand {
        id: "index-rebuild",
        title: "Rebuild Search Index",
        shortcut: "",
        category: "Search",
    },
];

/// Returns the indices of all commands matching every whitespace-separated
/// query word, capped at [`MAX_VISIBLE_MATCHES`]. Matches title, category,
/// shortcut and id, case-insensitively.
#[must_use]
pub fn matching_commands(query: &str) -> Vec<usize> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(|word| word.to_lowercase())
        .collect();
    COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            words.iter().all(|word| {
                command.title.to_lowercase().contains(word.as_str())
                    || command.category.to_lowercase().contains(word.as_str())
                    || command.shortcut.to_lowercase().contains(word.as_str())
                    || command.id.contains(word.as_str())
            })
        })
        .map(|(index, _)| index)
        .take(MAX_VISIBLE_MATCHES)
        .collect()
}

/// Query, filtered matches and selection of the palette. Pure state, so the
/// filter and focus behaviour can be unit tested without a window.
#[derive(Debug, Default)]
pub struct PaletteFilter {
    pub query: String,
    pub matches: Vec<usize>,
    pub selected: usize,
}

impl PaletteFilter {
    #[must_use]
    pub fn new() -> Self {
        Self {
            query: String::new(),
            matches: matching_commands(""),
            selected: 0,
        }
    }

    /// Replaces the query and recomputes the match list, keeping the selection
    /// inside the new result set.
    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.matches = matching_commands(&self.query);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
    }

    /// Moves the selection by `delta`, clamping to the visible matches.
    pub fn move_focus(&mut self, delta: isize) {
        if self.matches.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.matches.len() - 1;
        let next = self.selected.saturating_add_signed(delta);
        self.selected = next.min(last);
    }

    /// Id of the currently focused command, if any.
    #[must_use]
    pub fn selected_id(&self) -> Option<&'static str> {
        self.matches
            .get(self.selected)
            .and_then(|index| COMMANDS.get(*index))
            .map(|command| command.id)
    }
}

/// Indexed file search of the palette's `?` mode. Keeps its own run, hits and
/// selection next to the command filter, so switching modes never mixes rows.
#[derive(Debug, Default)]
pub struct PaletteSearch {
    pub run: SearchRun,
    pub hits: Vec<FileEntry>,
    pub selected: usize,
}

impl PaletteSearch {
    /// Replaces the hits of the current run and focuses the first one.
    pub fn set_hits(&mut self, hits: Vec<FileEntry>) {
        self.hits = hits;
        self.selected = 0;
    }

    /// Reports why a file search could not run at all.
    pub fn fail(&mut self, message: String) {
        self.hits.clear();
        self.selected = 0;
        self.run.fail(message);
    }

    /// Cancels the run in flight and clears the rows.
    pub fn cancel(&mut self) {
        self.run.cancel();
        self.hits.clear();
        self.selected = 0;
    }

    /// Moves the hit selection by `delta`, clamping to the visible hits.
    pub fn move_focus(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.hits.len().min(MAX_VISIBLE_HITS) - 1;
        let next = self.selected.saturating_add_signed(delta);
        self.selected = next.min(last);
    }

    #[must_use]
    pub fn selected_hit(&self) -> Option<&FileEntry> {
        self.hits.get(self.selected)
    }

    /// Empty-list text for the current run state.
    #[must_use]
    pub fn empty_note(&self) -> &'static str {
        match self.run.status() {
            SearchStatus::Running => "Searching files…",
            SearchStatus::Failed { .. } => "File search unavailable",
            SearchStatus::Done { .. } | SearchStatus::Idle => "No matching files",
        }
    }
}

/// Open state of the command palette modal.
pub struct CommandPaletteState {
    pub filter: PaletteFilter,
    pub input: Entity<InputState>,
    pub search: PaletteSearch,
}

impl CommandPaletteState {
    #[must_use]
    pub fn new(input: Entity<InputState>) -> Self {
        Self {
            filter: PaletteFilter::new(),
            input,
            search: PaletteSearch::default(),
        }
    }

    /// Mode of the current query.
    #[must_use]
    pub fn mode(&self) -> PaletteMode {
        palette_mode(&self.filter.query)
    }
}

/// Centered placeholder shown when the current mode has no rows.
fn empty_row(text: &str, theme: OnyxTheme) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .justify_center()
        .w_full()
        .h(px(64.0))
        .text_size(FONT_SM)
        .text_color(theme.text_muted())
        .child(text.to_string())
}

/// One command row: category, title and shortcut.
fn render_command_row(
    command: &PaletteCommand,
    position: usize,
    size: usize,
    is_selected: bool,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let id = command.id;
    let title = command.title;
    let shortcut = command.shortcut;
    let category = command.category;

    div()
        .id(("palette-row", position))
        .role(Role::Button)
        .aria_label(title.to_string())
        .aria_description(format!("{category} command"))
        .aria_keyshortcuts(shortcut.to_string())
        .aria_selected(is_selected)
        .aria_position_in_set(position + 1)
        .aria_size_of_set(size.max(1))
        .when(is_selected, |row| row.aria_active_descendant())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .h(px(ROW_HEIGHT))
        .flex_shrink_0()
        .px(px(10.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(if is_selected {
            theme.accent_border()
        } else {
            gpui_kit::transparent_black()
        })
        .bg(if is_selected {
            theme.bg_selected()
        } else {
            gpui_kit::transparent_black()
        })
        .cursor_pointer()
        .hover(move |style| {
            if is_selected {
                style
            } else {
                style.bg(theme.bg_hover())
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |explorer, _, window, cx| {
                explorer.run_palette_command(id, window, cx);
            }),
        )
        .child(
            div()
                .w(px(92.0))
                .flex_shrink_0()
                .text_size(FONT_XS)
                .font_weight(FontWeight::MEDIUM)
                .text_color(if is_selected {
                    theme.accent_bright()
                } else {
                    theme.text_muted()
                })
                .child(category),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_SM)
                .text_color(theme.text_primary())
                .child(title),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(shortcut),
        )
}

/// One file-search hit: icon, file name and the folder that contains it.
fn render_hit_row(
    entry: &FileEntry,
    position: usize,
    size: usize,
    is_selected: bool,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let name = entry.name.clone();
    let location = entry
        .path
        .parent()
        .map_or_else(String::new, normalize_display_path);

    let hit_description = if location.is_empty() {
        "Search hit".to_string()
    } else {
        format!("Search hit in {location}")
    };

    div()
        .id(("palette-hit", position))
        .role(Role::Button)
        .aria_label(name.clone())
        .aria_description(hit_description)
        .aria_selected(is_selected)
        .aria_position_in_set(position + 1)
        .aria_size_of_set(size.max(1))
        .when(is_selected, |row| row.aria_active_descendant())
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .h(px(ROW_HEIGHT))
        .flex_shrink_0()
        .px(px(10.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(if is_selected {
            theme.accent_border()
        } else {
            gpui_kit::transparent_black()
        })
        .bg(if is_selected {
            theme.bg_selected()
        } else {
            gpui_kit::transparent_black()
        })
        .cursor_pointer()
        .hover(move |style| {
            if is_selected {
                style
            } else {
                style.bg(theme.bg_hover())
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |explorer, _, window, cx| {
                explorer.open_palette_hit(position, window, cx);
            }),
        )
        .child(render_glyph(
            if entry.is_dir() {
                OnyxGlyph::Folder
            } else {
                OnyxGlyph::File
            },
            px(14.0),
            if entry.is_dir() {
                theme.folder_tint()
            } else {
                theme.text_muted()
            },
        ))
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(200.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_SM)
                .text_color(theme.text_primary())
                .child(name),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(if is_selected {
                    theme.text_secondary()
                } else {
                    theme.text_muted()
                })
                .child(location),
        )
}

/// Renders the deferred command palette overlay when open.
pub fn render_command_palette(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let state = view.command_palette.as_ref()?;
    let theme = view.theme;
    let query = state.filter.query.clone();
    let mode = state.mode();

    // Tab order inside the palette: the result list, then back to the query field.
    view.modals.palette.begin();
    let list_focus = view.modals.palette.slot();

    let (list_label, list_description) = match mode {
        PaletteMode::Commands => (
            "Command palette results",
            format!(
                "{} of {} commands",
                state.filter.matches.len(),
                COMMANDS.len()
            ),
        ),
        PaletteMode::Files => (
            "File search results",
            format!("{} files", state.search.hits.len().min(MAX_VISIBLE_HITS)),
        ),
    };

    let mut list = div()
        .id("palette-list")
        .role(Role::List)
        .aria_label(list_label)
        .aria_description(list_description)
        .track_focus(&list_focus)
        .focus_visible(|style| a11y::focus_ring_style(theme, style))
        .flex()
        .flex_col()
        .w_full()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .py(px(6.0));

    let (section_label, header_note) = match mode {
        PaletteMode::Commands => {
            let selected = state.filter.selected;
            let matches: Vec<&'static PaletteCommand> = state
                .filter
                .matches
                .iter()
                .filter_map(|index| COMMANDS.get(*index))
                .collect();
            let count = matches.len();
            if matches.is_empty() {
                list = list.child(empty_row("No matching commands", theme));
            }
            for (position, command) in matches.iter().enumerate() {
                list = list.child(render_command_row(
                    command,
                    position,
                    count,
                    position == selected,
                    theme,
                    cx,
                ));
            }
            let note = if count > 0 {
                format!("{count} of {} commands", COMMANDS.len())
            } else {
                "0 commands".to_string()
            };
            ("COMMANDS", note)
        }
        PaletteMode::Files => {
            let selected = state.search.selected;
            let hits: Vec<(usize, &FileEntry)> = state
                .search
                .hits
                .iter()
                .take(MAX_VISIBLE_HITS)
                .enumerate()
                .collect();
            let count = hits.len();
            if hits.is_empty() {
                list = list.child(empty_row(state.search.empty_note(), theme));
            }
            for (position, entry) in hits {
                list = list.child(render_hit_row(
                    entry,
                    position,
                    count,
                    position == selected,
                    theme,
                    cx,
                ));
            }
            let note = match state.search.run.status() {
                SearchStatus::Running => "searching…".to_string(),
                SearchStatus::Failed { message } => message.clone(),
                SearchStatus::Done { .. } | SearchStatus::Idle if count > 0 => {
                    format!("{count} files")
                }
                SearchStatus::Done { .. } | SearchStatus::Idle => "0 files".to_string(),
            };
            ("FILES", note)
        }
    };

    let card = div()
        .id("palette-card")
        .role(Role::Dialog)
        .aria_label("Command palette")
        .aria_description("Type to filter commands, Enter runs the selected command")
        .flex()
        .flex_col()
        .w(px(CARD_WIDTH))
        .h(px(CARD_HEIGHT))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(theme.border_strong())
        .bg(theme.bg_elevated())
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .w_full()
                .h(px(38.0))
                .flex_shrink_0()
                .px(px(10.0))
                .border_b_1()
                .border_color(theme.border_subtle())
                .child(render_glyph(
                    OnyxGlyph::Search,
                    px(15.0),
                    theme.accent_bright(),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(FONT_SM)
                        .text_color(theme.text_primary())
                        .child(
                            Input::new(&state.input)
                                .appearance(false)
                                .aria_label("Search commands")
                                .accessibility_id("command-palette-input"),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(FONT_XS)
                        .text_color(theme.text_muted())
                        .child(header_note),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .w_full()
                .h(px(24.0))
                .flex_shrink_0()
                .px(px(12.0))
                .border_b_1()
                .border_color(theme.border_subtle())
                .child(render_glyph(
                    OnyxGlyph::Command,
                    px(12.0),
                    theme.text_muted(),
                ))
                .child(
                    div()
                        .text_size(FONT_XS)
                        .text_color(theme.accent_bright())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(section_label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_XS)
                        .text_color(theme.text_muted())
                        .child(query),
                ),
        )
        .child(div().px(px(8.0)).flex_1().min_h(px(0.0)).child(list));

    Some(
        deferred(
            div()
                .id("command-palette-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .bg(theme.bg_backdrop())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|explorer, _, window, cx| {
                        explorer.close_command_palette(window, cx);
                    }),
                )
                .child(div().mt(px(CARD_TOP)).child(card)),
        )
        .with_priority(PALETTE_LAYER),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_lists_the_leading_commands() {
        let ids: Vec<&str> = matching_commands("")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert_eq!(ids.len(), MAX_VISIBLE_MATCHES);
        assert_eq!(ids.first().copied(), Some("settings"));
    }

    #[test]
    fn archive_and_layout_commands_are_registered() {
        // Mirrors `command_palette_includes_settings_archive_and_dnd_actions`
        // in `crates/onyx-app/src/commands.rs`.
        let ids: Vec<&str> = COMMANDS.iter().map(|command| command.id).collect();
        for required in [
            "compress-zip",
            "extract-here",
            "extract-to-folder",
            "browse-archive",
            "reset-layout",
        ] {
            assert!(
                ids.contains(&required),
                "expected command '{required}' to be registered"
            );
        }

        let archive_hits: Vec<&str> = matching_commands("archive")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert!(archive_hits.contains(&"browse-archive"));
    }

    #[test]
    fn query_matches_title_category_shortcut_and_id() {
        let by_title: Vec<&str> = matching_commands("open settings")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert_eq!(by_title, vec!["settings"]);

        let by_category: Vec<&str> = matching_commands("preferences")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert_eq!(by_category, vec!["settings"]);

        let by_shortcut: Vec<&str> = matching_commands("f5")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert!(by_shortcut.contains(&"refresh"));

        let by_id: Vec<&str> = matching_commands("swap-panes")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert_eq!(by_id, vec!["swap-panes"]);
    }

    #[test]
    fn query_words_all_have_to_match() {
        assert!(matching_commands("toggle nonexistent").is_empty());
        let ids: Vec<&str> = matching_commands("toggle sidebar")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert_eq!(ids, vec!["sidebar"]);
    }

    #[test]
    fn selection_clamps_to_the_match_list() {
        let mut state = PaletteFilter::new();
        state.move_focus(-1);
        assert_eq!(state.selected, 0);

        state.set_query("toggle".to_string());
        let last = state.matches.len() - 1;
        state.move_focus(50);
        assert_eq!(state.selected, last);

        state.set_query("zzz".to_string());
        assert!(state.matches.is_empty());
        assert_eq!(state.selected, 0);
        assert_eq!(state.selected_id(), None);
    }

    #[test]
    fn refilter_keeps_selection_valid() {
        let mut state = PaletteFilter::new();
        state.move_focus(5);
        let before = state.selected;
        assert_eq!(before, 5);

        state.set_query("files".to_string());
        assert!(state.selected < state.matches.len().max(1));
        assert!(state.selected_id().is_some());
    }

    #[test]
    fn question_mark_switches_to_the_file_search_mode() {
        assert_eq!(palette_mode(""), PaletteMode::Commands);
        assert_eq!(palette_mode("toggle sidebar"), PaletteMode::Commands);
        assert_eq!(palette_mode("?"), PaletteMode::Files);
        assert_eq!(palette_mode("  ?report"), PaletteMode::Files);

        assert_eq!(file_search_query("?report"), "report");
        assert_eq!(file_search_query("  ?  report "), "report");
        assert_eq!(file_search_query("?"), "");
        // Without the prefix the text is returned unchanged.
        assert_eq!(file_search_query("report"), "report");
    }

    #[test]
    fn index_commands_are_reachable_from_the_palette() {
        let ids: Vec<&str> = matching_commands("search index")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .map(|command| command.id)
            .collect();
        assert!(ids.contains(&"index"));
        assert!(ids.contains(&"index-rebuild"));

        let by_category: Vec<&str> = matching_commands("search")
            .iter()
            .filter_map(|index| COMMANDS.get(*index))
            .filter(|command| command.category == "Search")
            .map(|command| command.id)
            .collect();
        assert_eq!(by_category, vec!["index", "index-rebuild"]);
    }

    #[test]
    fn hit_selection_stays_inside_the_visible_hits() {
        let entry = |name: &str| FileEntry {
            path: std::path::PathBuf::from(format!(r"C:\data\{name}")),
            name: name.to_string(),
            extension: None,
            kind: onyx_core::FileKind::File,
            size: 0,
            modified: None,
            created: None,
            attributes: onyx_core::FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            type_label: "File".to_string(),
        };

        let mut search = PaletteSearch::default();
        search.move_focus(3);
        assert_eq!(search.selected, 0);
        assert_eq!(search.empty_note(), "No matching files");

        search.set_hits(vec![entry("a"), entry("b")]);
        assert_eq!(search.selected, 0);
        assert_eq!(
            search.selected_hit().map(|hit| hit.name.as_str()),
            Some("a")
        );
        search.move_focus(5);
        assert_eq!(search.selected, 1);
        search.move_focus(-9);
        assert_eq!(search.selected, 0);

        search.cancel();
        assert!(search.hits.is_empty());
        assert_eq!(search.selected_hit(), None);
    }
}
