//! ZIP archive browser modal matching `ArchiveBrowserModal` in
//! `crates/onyx-ui/ui/components/archive_overlay.slint`: header with title,
//! path and summary, a virtualized entry table (name, size, compressed, date),
//! loading/error states, and "Extract here" / "Extract to Folder" actions.

use crate::components::{ActionButtonProps, fluent_action_button, vertical_divider};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_LG, FONT_SM, FONT_XS, RADIUS_LG, RADIUS_SM};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement, Styled,
    UniformListScrollHandle, Window, deferred, div, px, uniform_list,
};
use onyx_core::{format_bytes, normalize_display_path};
use onyx_operations::ArchiveEntry;
use std::path::{Path, PathBuf};

const CARD_WIDTH: f32 = 720.0;
const CARD_HEIGHT: f32 = 540.0;
/// Deferred layer of the archive browser: above palette/settings, below the
/// context menu.
const ARCHIVE_LAYER: usize = 23;
const ROW_HEIGHT: f32 = 24.0;
const EMPTY_DATE: &str = "—";

/// One rendered table row of the archive browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRow {
    pub name: String,
    pub size: String,
    pub compressed: String,
    pub modified: String,
    pub is_dir: bool,
}

/// Formats an entry's timestamp as `YYYY-MM-DD HH:MM`, without pulling a date
/// crate into the UI layer.
#[must_use]
fn date_label(entry: &ArchiveEntry) -> String {
    entry.modified.map_or_else(
        || EMPTY_DATE.to_string(),
        |stamp| stamp.to_string().chars().take(16).collect(),
    )
}

/// Maps archive entries onto display rows.
#[must_use]
pub fn build_rows(entries: &[ArchiveEntry]) -> Vec<ArchiveRow> {
    entries
        .iter()
        .map(|entry| ArchiveRow {
            name: entry.name.clone(),
            size: if entry.is_dir {
                String::new()
            } else {
                format_bytes(entry.size)
            },
            compressed: if entry.is_dir {
                String::new()
            } else {
                format_bytes(entry.compressed_size)
            },
            modified: date_label(entry),
            is_dir: entry.is_dir,
        })
        .collect()
}

/// Header summary line: entry count, packed and unpacked size.
#[must_use]
pub fn summary_line(entries: &[ArchiveEntry]) -> String {
    let count = entries.len();
    let total: u64 = entries.iter().map(|entry| entry.size).sum();
    let packed: u64 = entries.iter().map(|entry| entry.compressed_size).sum();
    let items = if count == 1 {
        "1 entry".to_string()
    } else {
        format!("{count} entries")
    };
    if total == 0 {
        items
    } else {
        let ratio = if packed > 0 {
            format!(" · {:.0}% packed", (packed as f64 / total as f64) * 100.0)
        } else {
            String::new()
        };
        format!(
            "{items} · {} → {}{ratio}",
            format_bytes(total),
            format_bytes(packed)
        )
    }
}

/// Archive title derived from the file name.
#[must_use]
pub fn archive_title(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "Archive".to_string(), ToString::to_string)
}

/// Open state of the archive browser.
pub struct ArchiveViewState {
    pub path: PathBuf,
    pub entries: Vec<ArchiveEntry>,
    pub rows: Vec<ArchiveRow>,
    pub is_loading: bool,
    pub error: Option<String>,
    pub scroll: UniformListScrollHandle,
}

impl ArchiveViewState {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            entries: Vec::new(),
            rows: Vec::new(),
            is_loading: true,
            error: None,
            scroll: UniformListScrollHandle::new(),
        }
    }

    /// Applies a successful listing result.
    pub fn apply_entries(&mut self, entries: Vec<ArchiveEntry>) {
        self.rows = build_rows(&entries);
        self.entries = entries;
        self.error = None;
        self.is_loading = false;
    }

    /// Applies a failed listing result.
    pub fn apply_error(&mut self, message: String) {
        self.entries.clear();
        self.rows.clear();
        self.error = Some(message);
        self.is_loading = false;
    }

    #[must_use]
    pub fn summary(&self) -> String {
        if self.is_loading {
            "Reading archive…".to_string()
        } else if let Some(error) = &self.error {
            error.clone()
        } else {
            summary_line(&self.entries)
        }
    }
}

/// Renders the deferred archive browser overlay when open.
pub fn render_archive_overlay(
    view: &OnyxExplorer,
    window: &Window,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let state = view.archive.as_ref()?;
    let theme = view.theme;
    let title = archive_title(&state.path);
    let path_label = normalize_display_path(&state.path);
    let summary = state.summary();
    let is_loading = state.is_loading;
    let error = state.error.clone();
    let row_count = state.rows.len();
    let rows = state.rows.clone();
    let is_extracting = view.active_operation.is_some();

    let mut body = div().flex().flex_col().flex_1().min_h(px(0.0)).w_full();

    if let Some(message) = error {
        body = body.child(
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .max_w(px(460.0))
                        .p(px(14.0))
                        .gap(px(6.0))
                        .rounded(RADIUS_LG)
                        .border_1()
                        .border_color(theme.danger())
                        .bg(theme.danger_bg())
                        .child(
                            div()
                                .text_size(FONT_SM)
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme.danger())
                                .child("Unable to read archive"),
                        )
                        .child(
                            div()
                                .text_size(FONT_XS)
                                .text_color(theme.text_primary())
                                .child(message),
                        ),
                ),
        );
    } else if is_loading {
        body = body.child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(8.0))
                .child(render_glyph(
                    OnyxGlyph::Refresh,
                    px(24.0),
                    theme.accent_bright(),
                ))
                .child(
                    div()
                        .text_size(FONT_SM)
                        .text_color(theme.text_secondary())
                        .child("Reading archive…"),
                ),
        );
    } else {
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(24.0))
            .flex_shrink_0()
            .px(px(12.0))
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.border_subtle())
            .bg(theme.bg_column_header())
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child("Name"),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_shrink_0()
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child("Size"),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_shrink_0()
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child("Packed"),
            )
            .child(
                div()
                    .w(px(112.0))
                    .flex_shrink_0()
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child("Modified"),
            );

        let mut list = div()
            .id("archive-entry-list-host")
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .overflow_hidden();

        if row_count == 0 {
            list = list.child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_size(FONT_SM)
                    .text_color(theme.text_muted())
                    .child("This archive is empty"),
            );
        } else {
            let rows_for_list = rows.clone();
            list = list.child(
                uniform_list(
                    "archive-entry-list",
                    row_count,
                    cx.processor(move |_explorer, range: std::ops::Range<usize>, _, _cx| {
                        let mut out = Vec::with_capacity(range.len());
                        for index in range {
                            let Some(row) = rows_for_list.get(index) else {
                                continue;
                            };
                            let mut name_el = div()
                                .flex_1()
                                .min_w(px(0.0))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_size(FONT_XS)
                                .text_color(if row.is_dir {
                                    theme.folder_tint()
                                } else {
                                    theme.text_primary()
                                });
                            if row.is_dir {
                                name_el = name_el.font_weight(FontWeight::MEDIUM);
                            }
                            out.push(
                                div()
                                    .id(("archive-row", index))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .w_full()
                                    .h(px(ROW_HEIGHT))
                                    .px(px(12.0))
                                    .gap(px(8.0))
                                    .rounded(RADIUS_SM)
                                    .child(name_el.child(row.name.clone()))
                                    .child(
                                        div()
                                            .w(px(88.0))
                                            .flex_shrink_0()
                                            .text_size(FONT_XS)
                                            .text_color(theme.text_secondary())
                                            .child(row.size.clone()),
                                    )
                                    .child(
                                        div()
                                            .w(px(88.0))
                                            .flex_shrink_0()
                                            .text_size(FONT_XS)
                                            .text_color(theme.text_muted())
                                            .child(row.compressed.clone()),
                                    )
                                    .child(
                                        div()
                                            .w(px(112.0))
                                            .flex_shrink_0()
                                            .text_size(FONT_XS)
                                            .text_color(theme.text_muted())
                                            .child(row.modified.clone()),
                                    ),
                            );
                        }
                        out
                    }),
                )
                .track_scroll(&state.scroll)
                .flex_1()
                .h_full(),
            );
        }

        body = body.child(header).child(list);
    }

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .flex_shrink_0()
        .px(px(18.0))
        .py(px(12.0))
        .border_b_1()
        .border_color(theme.border_subtle())
        .child(render_glyph(
            OnyxGlyph::Archive,
            px(20.0),
            theme.accent_bright(),
        ))
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(FONT_LG)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_primary())
                        .child(title),
                )
                .child(
                    div()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_XS)
                        .text_color(theme.text_muted())
                        .child(path_label),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(summary),
        )
        .child(vertical_divider(theme, px(20.0)))
        .child(fluent_action_button(
            ActionButtonProps {
                id: "archive-close".into(),
                glyph: Some(OnyxGlyph::Close),
                trailing_glyph: None,
                label: None,
                active: false,
                primary: false,
                danger: false,
                enabled: true,
                compact: true,
                min_width: px(28.0),
            },
            theme,
            cx.listener(|explorer, _, window, cx| explorer.close_archive(window, cx)),
        ));

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .w_full()
        .flex_shrink_0()
        .gap(px(8.0))
        .px(px(18.0))
        .py(px(12.0))
        .border_t_1()
        .border_color(theme.border_subtle())
        .child(
            div()
                .flex_1()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(if is_extracting {
                    "An operation is running…"
                } else {
                    "Enter on a .zip, .tar.gz or .tgz opens this browser"
                }),
        )
        .child(fluent_action_button(
            ActionButtonProps {
                id: "archive-extract-folder".into(),
                glyph: Some(OnyxGlyph::FolderPlus),
                trailing_glyph: None,
                label: Some("Extract to Folder"),
                active: false,
                primary: false,
                danger: false,
                enabled: !is_loading && error_is_none(view) && !is_extracting,
                compact: false,
                min_width: px(96.0),
            },
            theme,
            cx.listener(|explorer, _, _, cx| explorer.extract_archive_to_folder(cx)),
        ))
        .child(fluent_action_button(
            ActionButtonProps {
                id: "archive-extract-here".into(),
                glyph: Some(OnyxGlyph::Download),
                trailing_glyph: None,
                label: Some("Extract here"),
                active: false,
                primary: true,
                danger: false,
                enabled: !is_loading && error_is_none(view) && !is_extracting,
                compact: false,
                min_width: px(96.0),
            },
            theme,
            cx.listener(|explorer, _, _, cx| explorer.extract_archive_here(cx)),
        ));

    let card = div()
        .id("archive-card")
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
        .child(header)
        .child(body)
        .child(footer);

    let viewport = window.viewport_size();
    let card_w = CARD_WIDTH.min((f32::from(viewport.width) - 56.0).max(360.0));
    let card_h = CARD_HEIGHT.min((f32::from(viewport.height) - 72.0).max(280.0));

    Some(
        deferred(
            div()
                .id("archive-backdrop")
                .absolute()
                .top(px(0.0))
                .left(px(0.0))
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|explorer, _, window, cx| explorer.close_archive(window, cx)),
                )
                .child(card.w(px(card_w)).h(px(card_h))),
        )
        .with_priority(ARCHIVE_LAYER),
    )
}

fn error_is_none(view: &OnyxExplorer) -> bool {
    view.archive
        .as_ref()
        .is_none_or(|state| state.error.is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, size: u64, packed: u64, is_dir: bool) -> ArchiveEntry {
        ArchiveEntry {
            name: name.to_string(),
            relative_path: PathBuf::from(name),
            size,
            compressed_size: packed,
            is_dir,
            modified: None,
        }
    }

    #[test]
    fn rows_map_sizes_and_skip_directory_sizes() {
        let entries = vec![
            entry("docs/", 0, 0, true),
            entry("docs/report.txt", 2048, 512, false),
        ];
        let rows = build_rows(&entries);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].name, "docs/");
        assert!(rows[0].is_dir);
        assert!(rows[0].size.is_empty());
        assert!(rows[0].compressed.is_empty());
        assert_eq!(rows[0].modified, EMPTY_DATE);
        assert_eq!(rows[1].size, format_bytes(2048));
        assert_eq!(rows[1].compressed, format_bytes(512));
        assert!(!rows[1].is_dir);
    }

    #[test]
    fn summary_reports_counts_and_ratio() {
        let entries = vec![entry("a", 1000, 250, false), entry("b", 1000, 250, false)];
        let summary = summary_line(&entries);
        assert!(summary.starts_with("2 entries"));
        assert!(summary.contains(&format_bytes(2000)));
        assert!(summary.contains(&format_bytes(500)));
        assert!(summary.contains("25% packed"));

        assert_eq!(summary_line(&[]), "0 entries");
        assert_eq!(summary_line(&[entry("only", 0, 0, true)]), "1 entry");
    }

    #[test]
    fn title_falls_back_without_file_name() {
        assert_eq!(
            archive_title(Path::new("C:\\tmp\\photos.zip")),
            "photos.zip"
        );
        assert_eq!(archive_title(Path::new("")), "Archive");
    }

    #[test]
    fn state_transitions_cover_loading_success_and_error() {
        let mut state = ArchiveViewState::new(PathBuf::from(r"C:\tmp\a.zip"));
        assert!(state.is_loading);
        assert_eq!(state.summary(), "Reading archive…");

        state.apply_entries(vec![entry("a.txt", 10, 5, false)]);
        assert!(!state.is_loading);
        assert_eq!(state.rows.len(), 1);
        assert!(state.error.is_none());
        assert!(state.summary().starts_with("1 entry"));

        state.apply_error("boom".to_string());
        assert!(state.rows.is_empty());
        assert_eq!(state.summary(), "boom");
    }
}
