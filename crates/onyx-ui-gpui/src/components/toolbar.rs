//! 40px action & view toolbar matching `crates/onyx-ui/ui/components/toolbar.slint`.

use crate::components::{ActionButtonProps, fluent_action_button, vertical_divider};
use crate::explorer::OnyxExplorer;
use crate::icons::OnyxGlyph;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, Orientation, ParentElement, Role,
    StatefulInteractiveElement, Styled, Window, div, px,
};
use onyx_core::{SortColumn, SortDirection, ViewMode};

fn sort_trailing_glyph(view: &OnyxExplorer, column: SortColumn) -> Option<OnyxGlyph> {
    let sort_spec = view.pane_sort_spec(view.active_pane);
    if sort_spec.column == column {
        Some(match sort_spec.direction {
            SortDirection::Ascending => OnyxGlyph::SortAscending,
            SortDirection::Descending => OnyxGlyph::SortDescending,
        })
    } else {
        None
    }
}

/// Renders the 40px toolbar with file operations, sort controls, layout/view toggles,
/// terminal, properties, and theme switcher.
pub fn render_toolbar(
    view: &OnyxExplorer,
    window: &Window,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let theme = view.theme;
    let has_selection = view.has_selection();
    let selection_is_archive = view.selection_is_archive();
    let can_paste = view.can_paste();
    let can_undo = view.can_undo();
    let viewport_w: f32 = window.viewport_size().width.into();
    let compact_labels = viewport_w < 1020.0;

    let left_group = div()
        .id("tb-file-actions")
        .role(Role::Toolbar)
        .aria_label("File actions")
        .aria_orientation(Orientation::Horizontal)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.0))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-new-folder".into(),
                    glyph: Some(OnyxGlyph::FolderPlus),
                    trailing_glyph: None,
                    label: if compact_labels { None } else { Some("New") },
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| explorer.prompt_new_folder(window, cx)),
            )
            .aria_label("New folder"),
        )
        .child(vertical_divider(theme, px(16.0)))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-cut".into(),
                    glyph: Some(OnyxGlyph::Cut),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.cut_selection(cx)),
            )
            .aria_label("Cut"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-copy".into(),
                    glyph: Some(OnyxGlyph::Copy),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.copy_selection(cx)),
            )
            .aria_label("Copy"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-paste".into(),
                    glyph: Some(OnyxGlyph::Paste),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: can_paste,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.paste(cx)),
            )
            .aria_label("Paste"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-rename".into(),
                    glyph: Some(OnyxGlyph::Rename),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| explorer.prompt_rename(window, cx)),
            )
            .aria_label("Rename"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-duplicate".into(),
                    glyph: Some(OnyxGlyph::Duplicate),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.duplicate_selection(cx)),
            )
            .aria_label("Duplicate"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-recycle".into(),
                    glyph: Some(OnyxGlyph::Trash),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: true,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.recycle_selection(cx)),
            )
            .aria_label("Move to Recycle Bin"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-undo".into(),
                    glyph: Some(OnyxGlyph::Undo),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: can_undo,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.undo(cx)),
            )
            .aria_label("Undo"),
        )
        .child(vertical_divider(theme, px(16.0)))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-compress".into(),
                    glyph: Some(OnyxGlyph::Archive),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.compress_selection_to_zip(cx)),
            )
            .aria_label("Compress to ZIP"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-extract".into(),
                    glyph: Some(OnyxGlyph::Download),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: selection_is_archive,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.extract_selected_archive(cx)),
            )
            .aria_label("Extract archive here"),
        )
        .child(vertical_divider(theme, px(16.0)))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-sort-name".into(),
                    glyph: None,
                    trailing_glyph: sort_trailing_glyph(view, SortColumn::Name),
                    label: Some("Name"),
                    active: view.pane_sort_spec(view.active_pane).column == SortColumn::Name,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| {
                    explorer.sort_by_in(explorer.active_pane, SortColumn::Name, cx);
                }),
            )
            .aria_label("Sort by name"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-sort-date".into(),
                    glyph: None,
                    trailing_glyph: sort_trailing_glyph(view, SortColumn::Modified),
                    label: Some("Date"),
                    active: view.pane_sort_spec(view.active_pane).column == SortColumn::Modified,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| {
                    explorer.sort_by_in(explorer.active_pane, SortColumn::Modified, cx);
                }),
            )
            .aria_label("Sort by date modified"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-sort-type".into(),
                    glyph: None,
                    trailing_glyph: sort_trailing_glyph(view, SortColumn::Kind),
                    label: Some("Type"),
                    active: view.pane_sort_spec(view.active_pane).column == SortColumn::Kind,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| {
                    explorer.sort_by_in(explorer.active_pane, SortColumn::Kind, cx);
                }),
            )
            .aria_label("Sort by type"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-sort-size".into(),
                    glyph: None,
                    trailing_glyph: sort_trailing_glyph(view, SortColumn::Size),
                    label: Some("Size"),
                    active: view.pane_sort_spec(view.active_pane).column == SortColumn::Size,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| {
                    explorer.sort_by_in(explorer.active_pane, SortColumn::Size, cx);
                }),
            )
            .aria_label("Sort by size"),
        );

    let is_grid = view.view_mode == ViewMode::Grid;
    let mut right_group = div()
        .id("tb-view-actions")
        .role(Role::Toolbar)
        .aria_label("View and layout actions")
        .aria_orientation(Orientation::Horizontal)
        .flex()
        .flex_row()
        .items_center()
        .gap(px(3.0))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-view-mode".into(),
                    glyph: Some(if is_grid {
                        OnyxGlyph::Grid
                    } else {
                        OnyxGlyph::List
                    }),
                    trailing_glyph: None,
                    label: if compact_labels {
                        None
                    } else if is_grid {
                        Some("Grid")
                    } else {
                        Some("Details")
                    },
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_view_mode(cx)),
            )
            .aria_label("Toggle view mode"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-hidden".into(),
                    glyph: Some(OnyxGlyph::Eye),
                    trailing_glyph: None,
                    label: if compact_labels { None } else { Some("Hidden") },
                    active: view.show_hidden,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_hidden(cx)),
            )
            .aria_label("Toggle hidden files"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-sidebar".into(),
                    glyph: Some(OnyxGlyph::PanelLeft),
                    trailing_glyph: None,
                    label: if compact_labels {
                        None
                    } else {
                        Some("Sidebar")
                    },
                    active: view.sidebar_visible,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_sidebar(cx)),
            )
            .aria_label("Show or hide the sidebar"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-preview".into(),
                    glyph: Some(OnyxGlyph::PanelRight),
                    trailing_glyph: None,
                    label: if compact_labels {
                        None
                    } else {
                        Some("Preview")
                    },
                    active: view.preview_visible,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_preview_pane(cx)),
            )
            .aria_label("Toggle preview pane"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-dual-pane".into(),
                    glyph: Some(OnyxGlyph::Columns),
                    trailing_glyph: None,
                    label: if compact_labels { None } else { Some("Dual") },
                    active: view.dual_pane,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_dual_pane(cx)),
            )
            .aria_label("Toggle dual-pane layout"),
        );

    if view.dual_pane {
        right_group = right_group.child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-swap-panes".into(),
                    glyph: Some(OnyxGlyph::SwapHorizontal),
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
                cx.listener(|explorer, _, _, cx| explorer.swap_panes(cx)),
            )
            .aria_label("Swap file panes"),
        );
    }

    let right_group = right_group
        .child(vertical_divider(theme, px(16.0)))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-terminal".into(),
                    glyph: Some(OnyxGlyph::Terminal),
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
                cx.listener(|explorer, _, _, cx| explorer.open_terminal(cx)),
            )
            .aria_label("Open terminal here"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-properties".into(),
                    glyph: Some(OnyxGlyph::Info),
                    trailing_glyph: None,
                    label: None,
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: has_selection,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.show_properties(cx)),
            )
            .aria_label("Properties"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "tb-theme".into(),
                    glyph: Some(if theme.dark_mode {
                        OnyxGlyph::Sun
                    } else {
                        OnyxGlyph::Moon
                    }),
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
                cx.listener(|explorer, _, window, cx| explorer.toggle_theme(window, cx)),
            )
            .aria_label("Switch light or dark theme"),
        );

    let bg = if theme.dark_mode {
        theme.bg_chrome()
    } else {
        theme.bg_surface_alt()
    };

    div()
        .id("onyx-toolbar")
        .role(Role::Toolbar)
        .aria_label("Toolbar")
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(40.0))
        .flex_shrink_0()
        .px(px(10.0))
        .py(px(6.0))
        .gap(px(4.0))
        .bg(bg)
        .border_b_1()
        .border_color(theme.border_subtle())
        .overflow_hidden()
        .child(left_group)
        .child(right_group)
}
