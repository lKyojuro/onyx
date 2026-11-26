//! Details & Grid file list with real Windows Shell icons, sortable column headers,
//! proportional folder/file size bars, and virtualized scrolling (`uniform_list`).

use crate::components::a11y;
use crate::components::drag_drop::DropTarget;
use crate::components::smooth_scroll::{
    DETAILS_ROW_HEIGHT, VIEWPORT_FALLBACK_PX, content_height, visible_rows,
};
use crate::explorer::{OnyxExplorer, PaneId};
use crate::icons::{OnyxGlyph, entry_icon_key, render_glyph};
use crate::theme::{FONT_MD, FONT_SM, FONT_XS, OnyxTheme, RADIUS_LG, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    Context, Entity, ExternalPaths, FontWeight, ImageSource, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, ParentElement, Pixels,
    RenderImage, Role, ScrollEvent, StatefulInteractiveElement, Styled, StyledImage,
    component::input::{Input, InputState},
    div, img,
    prelude::FluentBuilder,
    px, relative, uniform_list,
};
use onyx_core::{
    FileEntry, GroupByMode, SortColumn, SortDirection, ViewMode, format_bytes, format_timestamp,
    group_entries,
};
use std::sync::Arc;

/// Computes responsive grid column count matching `file_pane.slint` (`140px` min card width).
#[must_use]
pub fn compute_grid_columns(pane_width: f32) -> usize {
    (((pane_width - 24.0) / 140.0).floor() as usize).max(1)
}

/// Renders a file pane (`PaneId::Primary` or `PaneId::Secondary`) in Details or Grid mode.
pub fn render_file_pane(
    view: &OnyxExplorer,
    pane_id: PaneId,
    pane_w: f32,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let theme = view.theme;
    let pane_w = pane_w.max(200.0);
    let show_date_col = pane_w >= 370.0;
    let show_type_col = pane_w >= 520.0;
    let size_col_w = if pane_w >= 520.0 { 92.0 } else { 76.0 };
    let grid_cols = compute_grid_columns(pane_w);

    let is_dual = view.dual_pane;
    let is_active_pane = !is_dual || view.active_pane == pane_id;
    let entries = view.pane_entries(pane_id);
    let is_loading = view.pane_is_loading(pane_id);
    let error_message = view.pane_error_message(pane_id);
    let sort_spec = view.pane_sort_spec(pane_id);
    let entry_count = entries.len();
    let selection = view
        .pane_selected_index(pane_id)
        .and_then(|index| entries.get(index));
    // Spoken list state: how many entries, what is selected, sort, view mode.
    let list_description =
        a11y::list_state_description(entry_count, selection, sort_spec, view.view_mode);
    let list_label = if is_dual {
        match pane_id {
            PaneId::Primary => "Directory contents, primary pane",
            PaneId::Secondary => "Directory contents, secondary pane",
        }
    } else {
        "Directory contents"
    };

    let pane_container_id = match pane_id {
        PaneId::Primary => "onyx-pane-primary",
        PaneId::Secondary => "onyx-pane-secondary",
    };
    let pane_index = match pane_id {
        PaneId::Primary => 0usize,
        PaneId::Secondary => 1usize,
    };

    let mut pane = div()
        .id(pane_container_id)
        .flex()
        .flex_col()
        .h_full()
        .bg(theme.bg_surface())
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |explorer, _, window, cx| {
                explorer.set_active_pane(pane_id, window, cx);
            }),
        );

    if is_dual {
        pane = pane.w(px(pane_w)).flex_shrink_0();
    } else {
        pane = pane.flex_1().min_w(px(0.0));
    }

    // Dual-pane active focus indicator + compact pane path header
    if is_dual {
        let display_path = view.pane_location_label(pane_id);
        let pane_label = match pane_id {
            PaneId::Primary => "Primary",
            PaneId::Secondary => "Secondary",
        };
        let count_label = if entries.len() == 1 {
            "1 item".to_string()
        } else {
            format!("{} items", entries.len())
        };
        let header_id = match pane_id {
            PaneId::Primary => "pane-hdr-primary",
            PaneId::Secondary => "pane-hdr-secondary",
        };

        pane = pane
            .child(
                div()
                    .w_full()
                    .h(px(2.0))
                    .flex_shrink_0()
                    .bg(if is_active_pane {
                        theme.accent_bright()
                    } else {
                        gpui_kit::transparent_black()
                    }),
            )
            .child(
                div()
                    .id(header_id)
                    .role(Role::Group)
                    .aria_label(format!(
                        "{} pane, {display_path}, {count_label}",
                        pane_label
                    ))
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .h(px(28.0))
                    .flex_shrink_0()
                    .px(px(10.0))
                    .gap(px(6.0))
                    .bg(if is_active_pane {
                        theme.bg_chrome()
                    } else {
                        theme.bg_surface_alt()
                    })
                    .border_b_1()
                    .border_color(if is_active_pane {
                        theme.accent_border()
                    } else {
                        theme.border_subtle()
                    })
                    .cursor_pointer()
                    .child(
                        div()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded_full()
                            .flex_shrink_0()
                            .bg(if is_active_pane {
                                theme.accent_bright()
                            } else {
                                theme.border_strong()
                            }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(FONT_XS)
                            .font_weight(if is_active_pane {
                                FontWeight::SEMIBOLD
                            } else {
                                FontWeight::MEDIUM
                            })
                            .text_color(if is_active_pane {
                                theme.accent_bright()
                            } else {
                                theme.text_muted()
                            })
                            .child(pane_label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(FONT_XS)
                            .text_color(if is_active_pane {
                                theme.text_primary()
                            } else {
                                theme.text_secondary()
                            })
                            .child(display_path),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(FONT_XS)
                            .text_color(theme.text_muted())
                            .child(count_label),
                    ),
            );
    }

    // Non-blocking top 2px loading bar when refreshing a populated folder
    if is_loading && !entries.is_empty() {
        pane = pane.child(
            div()
                .w_full()
                .h(px(2.0))
                .bg(theme.accent_bright())
                .opacity(0.85),
        );
    }

    // Details column header row
    if view.view_mode != ViewMode::Grid && error_message.is_none() {
        let mut header_row = div()
            .id(("col-header-row", pane_index))
            .role(Role::Toolbar)
            .aria_label("Column headers, sort the list")
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(30.0))
            .flex_shrink_0()
            .pl(px(10.0))
            .pr(px(18.0))
            .gap(px(4.0))
            .bg(theme.bg_column_header())
            .border_b_1()
            .border_color(theme.border_subtle())
            .child(column_header_btn(
                pane_id,
                "Name",
                SortColumn::Name,
                sort_spec.column,
                sort_spec.direction,
                false,
                None,
                theme,
                cx,
            ));

        let recycle_bin = view.pane_shows_recycle_bin(pane_id);
        if show_date_col {
            header_row = header_row.child(column_header_btn(
                pane_id,
                if recycle_bin {
                    "Date deleted"
                } else {
                    "Date modified"
                },
                SortColumn::Modified,
                sort_spec.column,
                sort_spec.direction,
                false,
                Some(px(144.0)),
                theme,
                cx,
            ));
        }
        if show_type_col {
            // A recycled item has no type label to show; the folder it is restored to
            // is what the column reports instead.
            header_row = header_row.child(column_header_btn(
                pane_id,
                if recycle_bin {
                    "Original location"
                } else {
                    "Type"
                },
                SortColumn::Kind,
                sort_spec.column,
                sort_spec.direction,
                false,
                Some(px(120.0)),
                theme,
                cx,
            ));
        }
        header_row = header_row.child(column_header_btn(
            pane_id,
            "Size",
            SortColumn::Size,
            sort_spec.column,
            sort_spec.direction,
            true,
            Some(px(size_col_w)),
            theme,
            cx,
        ));

        pane = pane.child(header_row);
    }

    // Body: Error / Initial Loading / Empty / Details List / Grid List
    let body = if let Some(err) = error_message {
        div()
            .id(("pane-error", pane_index))
            .role(Role::Alert)
            .aria_label("Unable to open location")
            .aria_description(err.to_string())
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .max_w(px(440.0))
                    .p(px(16.0))
                    .gap(px(6.0))
                    .rounded(RADIUS_LG)
                    .border_1()
                    .border_color(theme.danger())
                    .bg(theme.danger_bg())
                    .child(
                        div()
                            .text_size(FONT_MD)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.danger())
                            .child("Unable to open location"),
                    )
                    .child(
                        div()
                            .text_size(FONT_SM)
                            .text_color(theme.text_primary())
                            .child(err.to_string()),
                    ),
            )
            .into_any_element()
    } else if is_loading && entries.is_empty() {
        div()
            .id(("pane-loading", pane_index))
            .role(Role::Status)
            .aria_label("Loading folder")
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .child(render_glyph(
                OnyxGlyph::Refresh,
                px(26.0),
                theme.accent_bright(),
            ))
            .child(
                div()
                    .text_size(FONT_MD)
                    .text_color(theme.text_secondary())
                    .child("Loading folder..."),
            )
            .into_any_element()
    } else if entries.is_empty() {
        let has_query = !view.search_query_text().trim().is_empty();
        let recycle_bin = view.pane_shows_recycle_bin(pane_id);
        let (empty_title, empty_hint) = if has_query {
            (
                "No matching items",
                "Try a different search term or clear the search box.",
            )
        } else if recycle_bin {
            (
                "Recycle Bin is empty",
                "Deleted files stay here until you empty the Recycle Bin.",
            )
        } else {
            (
                "This folder is empty",
                "Navigate using the sidebar or address bar.",
            )
        };
        div()
            .id(("pane-empty", pane_index))
            .role(Role::Status)
            .aria_label(empty_title)
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .child(render_glyph(
                if has_query {
                    OnyxGlyph::Search
                } else if recycle_bin {
                    OnyxGlyph::Trash
                } else {
                    OnyxGlyph::Folder
                },
                px(36.0),
                theme.text_muted(),
            ))
            .child(
                div()
                    .text_size(FONT_MD)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary())
                    .child(empty_title),
            )
            .child(
                div()
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child(empty_hint),
            )
            .into_any_element()
    } else if view.view_mode == ViewMode::Grid {
        let total = entries.len();
        let row_count = total.div_ceil(grid_cols);
        let grid_id = match pane_id {
            PaneId::Primary => "onyx-file-grid-primary",
            PaneId::Secondary => "onyx-file-grid-secondary",
        };
        let grid_list = uniform_list(
            grid_id,
            row_count,
            cx.processor(move |explorer, range: std::ops::Range<usize>, _, cx| {
                let mut out = Vec::with_capacity(range.len());
                for row_idx in range {
                    let mut row_el = div()
                        .flex()
                        .flex_row()
                        .w_full()
                        .h(px(134.0))
                        .px(px(10.0))
                        .py(px(6.0))
                        .gap(px(8.0));

                    for col_idx in 0..grid_cols {
                        let item_idx = row_idx * grid_cols + col_idx;
                        if let Some(entry) = explorer.pane_entries(pane_id).get(item_idx) {
                            let is_selected = explorer.pane_is_selected(pane_id, item_idx);
                            let is_lead = explorer.pane_selected_index(pane_id) == Some(item_idx);
                            let is_drop_hover = explorer.drag_hover_is(&entry.path);
                            let native_icon =
                                explorer.icons.get(&entry_icon_key(entry)).map(Arc::clone);
                            let thumbnail = explorer.thumbnails.get(&entry.path).map(Arc::clone);
                            let (size_label, _) = explorer.entry_size_info_for(pane_id, entry);
                            row_el = row_el.child(render_grid_card(
                                pane_id,
                                item_idx,
                                total,
                                entry,
                                is_selected,
                                is_lead,
                                is_drop_hover,
                                native_icon,
                                thumbnail,
                                size_label,
                                explorer.theme,
                                cx,
                            ));
                        } else {
                            row_el = row_el.child(div().flex_1().h(px(122.0)));
                        }
                    }

                    out.push(row_el);
                }
                out
            }),
        )
        .track_scroll(view.pane_grid_scroll(pane_id))
        .flex_1()
        .h_full();
        div()
            .id(("onyx-file-grid-host", pane_index))
            .role(Role::List)
            .aria_label(list_label)
            .aria_description(list_description.clone())
            .track_focus(view.pane_focus_handle(pane_id))
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .child(grid_list)
            .into_any_element()
    } else {
        let count = entries.len();
        let list_id = match pane_id {
            PaneId::Primary => "onyx-file-details-primary",
            PaneId::Secondary => "onyx-file-details-secondary",
        };
        // Owned scroll container: a tracked `ScrollHandle` lets the wheel ease
        // the offset (`smooth_scroll`) instead of jumping item by item.
        let offset = view.pane_list_offset(pane_id);
        let measured = view.pane_list_viewport(pane_id);
        let viewport = if measured > 0.0 {
            measured
        } else {
            VIEWPORT_FALLBACK_PX
        };

        let (total_visual_rows, row_els) = if view.group_by == GroupByMode::None {
            let rows = visible_rows(offset, viewport, DETAILS_ROW_HEIGHT, count);
            let mut els = Vec::with_capacity(rows.len());
            for idx in rows {
                let Some(entry) = view.pane_entries(pane_id).get(idx) else {
                    continue;
                };
                let is_selected = view.pane_is_selected(pane_id, idx);
                let is_lead = view.pane_selected_index(pane_id) == Some(idx);
                let is_drop_hover = view.drag_hover_is(&entry.path);
                let is_insert_line = !entry.is_dir() && view.drag_hover_is_pane_row(pane_id, idx);
                let rename_input = view
                    .is_inline_renaming(pane_id, &entry.path)
                    .then(|| view.inline_rename_input.clone());
                let native_icon = view.icons.get(&entry_icon_key(entry)).map(Arc::clone);
                let (size_label, size_ratio) = view.entry_size_info_for(pane_id, entry);
                els.push(
                    render_details_row(
                        pane_id,
                        idx,
                        count,
                        entry,
                        is_selected,
                        is_lead,
                        is_drop_hover,
                        is_insert_line,
                        rename_input,
                        native_icon,
                        size_label,
                        size_ratio,
                        show_date_col,
                        show_type_col,
                        size_col_w,
                        view.theme,
                        cx,
                    )
                    .into_any_element(),
                );
            }
            (count, els)
        } else {
            enum GroupedRow {
                Header {
                    key: String,
                    label: String,
                    count: usize,
                    collapsed: bool,
                },
                Entry(usize),
            }
            let groups = group_entries(entries, view.group_by);
            let mut flat: Vec<GroupedRow> = Vec::with_capacity(count + groups.len());
            let mut entry_cursor = 0usize;
            for group in groups {
                let collapsed = view.is_group_collapsed(&group.key);
                let group_len = group.len();
                flat.push(GroupedRow::Header {
                    key: group.key,
                    label: group.label,
                    count: group_len,
                    collapsed,
                });
                if !collapsed {
                    for _ in 0..group_len {
                        flat.push(GroupedRow::Entry(entry_cursor));
                        entry_cursor += 1;
                    }
                } else {
                    entry_cursor += group_len;
                }
            }
            let total_rows = flat.len();
            let rows = visible_rows(offset, viewport, DETAILS_ROW_HEIGHT, total_rows);
            let mut els = Vec::with_capacity(rows.len());
            for row_idx in rows {
                match &flat[row_idx] {
                    GroupedRow::Header {
                        key,
                        label,
                        count: group_count,
                        collapsed,
                    } => {
                        let toggle_key = key.clone();
                        let header_id = format!("grp-hdr-{pane_index}-{key}");
                        let count_badge = if *group_count == 1 {
                            "1 item".to_string()
                        } else {
                            format!("{group_count} items")
                        };
                        els.push(
                            div()
                                .id(header_id)
                                .role(Role::Button)
                                .aria_label(format!("{label}, {count_badge}"))
                                .aria_expanded(!collapsed)
                                .flex()
                                .flex_row()
                                .items_center()
                                .w_full()
                                .h(px(DETAILS_ROW_HEIGHT))
                                .px(px(10.0))
                                .gap(px(6.0))
                                .bg(theme.bg_surface_alt())
                                .border_b_1()
                                .border_color(theme.border_subtle())
                                .cursor_pointer()
                                .hover(move |s| s.bg(theme.bg_hover()))
                                .on_click(cx.listener(move |explorer, _, _, cx| {
                                    explorer.toggle_group_collapsed(&toggle_key, cx);
                                }))
                                .child(render_glyph(
                                    if *collapsed {
                                        OnyxGlyph::ChevronRight
                                    } else {
                                        OnyxGlyph::ChevronDown
                                    },
                                    px(12.0),
                                    theme.text_muted(),
                                ))
                                .child(
                                    div()
                                        .text_size(FONT_XS)
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme.text_primary())
                                        .child(label.clone()),
                                )
                                .child(
                                    div()
                                        .px(px(6.0))
                                        .py(px(1.0))
                                        .rounded(RADIUS_SM)
                                        .bg(theme.bg_badge())
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted())
                                        .child(count_badge),
                                )
                                .into_any_element(),
                        );
                    }
                    GroupedRow::Entry(idx) => {
                        let Some(entry) = view.pane_entries(pane_id).get(*idx) else {
                            continue;
                        };
                        let is_selected = view.pane_is_selected(pane_id, *idx);
                        let is_lead = view.pane_selected_index(pane_id) == Some(*idx);
                        let is_drop_hover = view.drag_hover_is(&entry.path);
                        let is_insert_line =
                            !entry.is_dir() && view.drag_hover_is_pane_row(pane_id, *idx);
                        let rename_input = view
                            .is_inline_renaming(pane_id, &entry.path)
                            .then(|| view.inline_rename_input.clone());
                        let native_icon = view.icons.get(&entry_icon_key(entry)).map(Arc::clone);
                        let (size_label, size_ratio) = view.entry_size_info_for(pane_id, entry);
                        els.push(
                            render_details_row(
                                pane_id,
                                *idx,
                                count,
                                entry,
                                is_selected,
                                is_lead,
                                is_drop_hover,
                                is_insert_line,
                                rename_input,
                                native_icon,
                                size_label,
                                size_ratio,
                                show_date_col,
                                show_type_col,
                                size_col_w,
                                view.theme,
                                cx,
                            )
                            .into_any_element(),
                        );
                    }
                }
            }
            (total_rows, els)
        };

        let first_row = (offset / DETAILS_ROW_HEIGHT).floor().max(0.0) as usize;
        div()
            .id(list_id)
            .role(Role::List)
            .aria_label(list_label)
            .aria_description(list_description)
            .track_focus(view.pane_focus_handle(pane_id))
            .flex_1()
            .h_full()
            .overflow_hidden()
            .track_scroll(view.pane_list_scroll(pane_id))
            .on_scroll(cx.listener(move |explorer, ev: &ScrollEvent, window, cx| {
                cx.stop_propagation();
                explorer.wheel_list(pane_id, ev, f32::from(window.line_height()), cx);
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .h(px(content_height(total_visual_rows, DETAILS_ROW_HEIGHT)))
                    .pt(px(first_row as f32 * DETAILS_ROW_HEIGHT))
                    .children(row_els),
            )
            .into_any_element()
    };

    let body_id = match pane_id {
        PaneId::Primary => "onyx-file-pane-body-primary",
        PaneId::Secondary => "onyx-file-pane-body-secondary",
    };

    let pane_drop_hover = view.drag_hover_is_pane(pane_id);
    pane.child(
        div()
            .id(body_id)
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.0))
            .w_full()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                    explorer.begin_marquee(pane_id, ev, window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                    explorer.set_active_pane(pane_id, window, cx);
                    explorer.open_background_context_menu(ev.position, window, cx);
                }),
            )
            .on_mouse_move(cx.listener(move |explorer, _, _, cx| {
                explorer.set_drop_hover(DropTarget::Pane(pane_id), cx);
            }))
            .border_2()
            .border_color(if pane_drop_hover {
                theme.accent_bright()
            } else {
                gpui_kit::transparent_black()
            })
            .when(pane_drop_hover, |el| el.bg(theme.accent_subtle()))
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style
                    .border_2()
                    .border_color(theme.accent_bright())
                    .bg(theme.accent_subtle())
            })
            .on_drop(
                cx.listener(move |explorer, paths: &ExternalPaths, _window, cx| {
                    cx.stop_propagation();
                    let dest = explorer.pane_path(pane_id).to_path_buf();
                    explorer.handle_external_paths_drop_to(paths, dest, cx);
                }),
            )
            .child(body),
    )
}

#[allow(clippy::too_many_arguments)]
fn column_header_btn(
    pane_id: PaneId,
    label: &'static str,
    column: SortColumn,
    active_column: SortColumn,
    direction: SortDirection,
    align_right: bool,
    width: Option<Pixels>,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement + use<> {
    let is_active = column == active_column;
    let col_prefix = match pane_id {
        PaneId::Primary => "p1-col-hdr",
        PaneId::Secondary => "p2-col-hdr",
    };
    let direction_label = match direction {
        SortDirection::Ascending => "ascending",
        SortDirection::Descending => "descending",
    };
    let a11y_label = if is_active {
        format!("Sort by {label}, currently {direction_label}")
    } else {
        format!("Sort by {label}")
    };
    let mut btn = div()
        .id((col_prefix, column as usize))
        .role(Role::ColumnHeader)
        .aria_label(a11y_label)
        .aria_selected(is_active)
        .flex()
        .flex_row()
        .items_center()
        .h(px(26.0))
        .px(px(8.0))
        .gap(px(3.0))
        .rounded(RADIUS_SM)
        .cursor_pointer()
        .hover(move |s| s.bg(theme.bg_hover()))
        .on_click(cx.listener(move |explorer, _, _, cx| {
            explorer.sort_by_in(pane_id, column, cx);
        }));

    if align_right {
        btn = btn.justify_end();
    } else {
        btn = btn.justify_start();
    }
    if let Some(w) = width {
        btn = btn.w(w).flex_shrink_0();
    } else {
        btn = btn.flex_1().min_w(px(0.0));
    }

    btn = btn.child(
        div()
            .text_size(FONT_XS)
            .font_weight(if is_active {
                FontWeight::SEMIBOLD
            } else {
                FontWeight::MEDIUM
            })
            .text_color(if is_active {
                theme.accent_bright()
            } else {
                theme.text_muted()
            })
            .overflow_hidden()
            .whitespace_nowrap()
            .child(label),
    );

    if is_active {
        btn = btn.child(render_glyph(
            match direction {
                SortDirection::Ascending => OnyxGlyph::SortAscending,
                SortDirection::Descending => OnyxGlyph::SortDescending,
            },
            px(12.0),
            theme.accent_bright(),
        ));
    }

    btn
}

fn entry_glyph(
    entry: &FileEntry,
    native_icon: Option<Arc<RenderImage>>,
    size: Pixels,
    theme: OnyxTheme,
) -> impl IntoElement {
    let is_dir = entry.is_dir();
    let main_icon = if let Some(render_img) = native_icon {
        img(ImageSource::Render(render_img))
            .w(size)
            .h(size)
            .flex_shrink_0()
            .into_any_element()
    } else {
        render_glyph(
            if is_dir {
                OnyxGlyph::Folder
            } else {
                OnyxGlyph::File
            },
            size,
            if is_dir {
                theme.folder_tint()
            } else {
                theme.file_tint()
            },
        )
        .into_any_element()
    };

    let mut box_el = div()
        .relative()
        .flex()
        .items_center()
        .justify_center()
        .w(size)
        .h(size)
        .flex_shrink_0()
        .child(main_icon);

    if entry.is_symlink {
        box_el = box_el.child(
            div()
                .absolute()
                .right(px(0.0))
                .bottom(px(0.0))
                .flex()
                .items_center()
                .justify_center()
                .w(px(10.0))
                .h(px(10.0))
                .rounded(px(2.0))
                .bg(theme.bg_surface())
                .border_1()
                .border_color(theme.symlink_tint())
                .child(render_glyph(
                    OnyxGlyph::ArrowUpRight,
                    px(7.0),
                    theme.symlink_tint(),
                )),
        );
    }

    box_el
}

#[allow(clippy::too_many_arguments)]
fn render_details_row(
    pane_id: PaneId,
    idx: usize,
    count: usize,
    entry: &FileEntry,
    is_selected: bool,
    is_lead: bool,
    is_drop_hover: bool,
    is_insert_line: bool,
    rename_input: Option<Entity<InputState>>,
    native_icon: Option<Arc<RenderImage>>,
    size_label: String,
    size_ratio: f32,
    show_date_col: bool,
    show_type_col: bool,
    size_col_w: f32,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement + use<> {
    let is_dir = entry.is_dir();
    let is_renaming = rename_input.is_some();
    let drag_path = entry.path.clone();
    let hover_path = entry.path.clone();
    let external_drop_path = entry.path.clone();
    let modified_text = format_timestamp(entry.modified);
    let hidden_opacity = if entry.attributes.hidden { 0.62 } else { 1.0 };
    let row_prefix = match pane_id {
        PaneId::Primary => "p1-details-row",
        PaneId::Secondary => "p2-details-row",
    };

    let a11y_label = a11y::entry_label(entry, &size_label, &modified_text);
    let mut inner = div()
        .id((row_prefix, idx))
        .role(Role::ListItem)
        .aria_label(a11y_label)
        .aria_selected(is_selected)
        .aria_position_in_set(idx + 1)
        .aria_size_of_set(count.max(1))
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(33.0))
        .pl(px(10.0))
        .pr(px(12.0))
        .gap(px(4.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(if is_drop_hover {
            theme.accent_bright()
        } else if is_selected {
            theme.accent_border()
        } else {
            gpui_kit::transparent_black()
        })
        .bg(if is_drop_hover {
            theme.accent_subtle()
        } else if is_selected {
            theme.bg_selected()
        } else {
            gpui_kit::transparent_black()
        })
        .opacity(hidden_opacity)
        .cursor_pointer()
        .hover(move |s| {
            if !is_selected {
                s.bg(theme.bg_hover())
            } else {
                s
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if is_renaming {
                    return;
                }
                explorer.handle_entry_mouse_down(
                    pane_id,
                    idx,
                    ev,
                    drag_path.clone(),
                    is_dir,
                    window,
                    cx,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                explorer.set_active_pane(pane_id, window, cx);
                explorer.open_item_context_menu(idx, ev.position, window, cx);
            }),
        )
        .on_mouse_move(cx.listener(move |explorer, ev: &MouseMoveEvent, _, cx| {
            if explorer.is_drag_armed() {
                let pointer = (f32::from(ev.position.x), f32::from(ev.position.y));
                explorer.update_drag(pointer, ev.modifiers.control, cx);
                if is_dir {
                    explorer.set_drop_hover(DropTarget::Directory(hover_path.clone()), cx);
                } else {
                    explorer.set_drop_hover(
                        DropTarget::PaneRow {
                            pane: pane_id,
                            row_index: idx,
                        },
                        cx,
                    );
                }
                cx.stop_propagation();
            }
        }));

    if is_dir {
        inner = inner
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style
                    .bg(theme.accent_subtle())
                    .border_color(theme.accent_bright())
            })
            .on_drop(
                cx.listener(move |explorer, paths: &ExternalPaths, _window, cx| {
                    cx.stop_propagation();
                    explorer.handle_external_paths_drop_to(paths, external_drop_path.clone(), cx);
                }),
            );
    }

    if is_selected {
        inner = inner.child(
            div()
                .absolute()
                .left(px(2.0))
                .top(px(7.0))
                .w(px(3.0))
                .h(px(17.0))
                .rounded(px(1.5))
                .bg(theme.accent_bright()),
        );
    }

    let name_content = if let Some(input_state) = rename_input {
        div()
            .flex_1()
            .min_w(px(0.0))
            .h(px(24.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .rounded(RADIUS_SM)
            .border_1()
            .border_color(theme.border_focus())
            .bg(theme.bg_input())
            .text_size(FONT_SM)
            .text_color(theme.text_primary())
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
                if ev.keystroke.key.as_str() == "escape" {
                    explorer.cancel_inline_rename(window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(
                Input::new(&input_state)
                    .appearance(false)
                    .aria_label("Rename item")
                    .accessibility_id("inline-rename-input"),
            )
            .into_any_element()
    } else {
        let mut name_text = div()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(FONT_SM)
            .text_color(theme.text_primary());
        if is_selected {
            name_text = name_text.font_weight(FontWeight::SEMIBOLD);
        } else if is_dir {
            name_text = name_text.font_weight(FontWeight::MEDIUM);
        }
        name_text.child(entry.name.clone()).into_any_element()
    };

    inner = inner.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(0.0))
            .gap(px(10.0))
            .child(entry_glyph(entry, native_icon, px(22.0), theme))
            .child(name_content),
    );

    if show_date_col {
        inner = inner.child(
            div()
                .w(px(144.0))
                .flex_shrink_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(modified_text),
        );
    }

    if show_type_col {
        inner = inner.child(
            div()
                .w(px(120.0))
                .flex_shrink_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(entry.type_label.clone()),
        );
    }

    // Size + proportional right-aligned fill bar (`file_pane.slint`)
    let mut size_cell = div()
        .relative()
        .flex()
        .items_center()
        .justify_end()
        .w(px(size_col_w))
        .h(px(28.0))
        .flex_shrink_0();

    if size_ratio > 0.0 {
        let clamped = size_ratio.clamp(0.04, 1.0);
        size_cell = size_cell.child(
            div()
                .absolute()
                .right(px(0.0))
                .top(px(4.0))
                .bottom(px(4.0))
                .w(relative(clamped))
                .rounded(px(3.0))
                .bg(theme.accent_subtle()),
        );
    }

    size_cell = size_cell.child(
        div()
            .text_size(FONT_XS)
            .text_color(theme.text_muted())
            .overflow_hidden()
            .whitespace_nowrap()
            .child(size_label),
    );

    inner = inner.child(size_cell);
    if is_lead {
        inner = inner.aria_active_descendant();
    }

    let mut row_outer = div()
        .relative()
        .flex()
        .items_center()
        .w_full()
        .h(px(35.0))
        .px(px(6.0))
        .child(inner);

    if !is_dir {
        row_outer = row_outer
            .border_b_2()
            .border_color(gpui_kit::transparent_black())
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style.border_b_2().border_color(theme.accent_bright())
            })
            .on_drop(
                cx.listener(move |explorer, paths: &ExternalPaths, _window, cx| {
                    cx.stop_propagation();
                    let dest = explorer.pane_path(pane_id).to_path_buf();
                    explorer.handle_external_paths_drop_to(paths, dest, cx);
                }),
            );
        if is_insert_line {
            row_outer = row_outer.child(
                div()
                    .absolute()
                    .left(px(8.0))
                    .right(px(8.0))
                    .bottom(px(0.0))
                    .h(px(2.0))
                    .rounded_full()
                    .bg(theme.accent_bright()),
            );
        }
    }

    row_outer
}

#[allow(clippy::too_many_arguments)]
fn render_grid_card(
    pane_id: PaneId,
    idx: usize,
    count: usize,
    entry: &FileEntry,
    is_selected: bool,
    is_lead: bool,
    is_drop_hover: bool,
    native_icon: Option<Arc<RenderImage>>,
    thumbnail: Option<Arc<RenderImage>>,
    size_label: String,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement + use<> {
    let sub_label = if !size_label.is_empty() {
        size_label
    } else if !entry.type_label.is_empty() {
        entry.type_label.clone()
    } else if entry.is_dir() {
        "Folder".to_string()
    } else {
        format_bytes(entry.size)
    };
    let card_prefix = match pane_id {
        PaneId::Primary => "p1-grid-card",
        PaneId::Secondary => "p2-grid-card",
    };
    let is_dir = entry.is_dir();
    let drag_path = entry.path.clone();
    let hover_path = entry.path.clone();
    let a11y_label = a11y::entry_label(entry, &sub_label, "");

    let mut name_el = div()
        .w_full()
        .text_center()
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(FONT_XS)
        .text_color(theme.text_primary());
    if is_selected {
        name_el = name_el.font_weight(FontWeight::SEMIBOLD);
    }

    div()
        .id((card_prefix, idx))
        .role(Role::ListItem)
        .aria_label(a11y_label)
        .aria_selected(is_selected)
        .aria_position_in_set(idx + 1)
        .aria_size_of_set(count.max(1))
        .when(is_lead, |card| card.aria_active_descendant())
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .flex_1()
        .min_w(px(0.0))
        .h(px(122.0))
        .p(px(8.0))
        .gap(px(6.0))
        .rounded(RADIUS_MD)
        .border_1()
        .border_color(if is_drop_hover {
            theme.accent_bright()
        } else if is_selected {
            theme.accent_border()
        } else {
            theme.border_subtle()
        })
        .bg(if is_selected {
            theme.bg_selected()
        } else {
            theme.bg_surface_alt()
        })
        .opacity(if entry.attributes.hidden { 0.62 } else { 1.0 })
        .cursor_pointer()
        .hover(move |s| {
            if !is_selected {
                s.bg(theme.bg_hover()).border_color(theme.border_strong())
            } else {
                s
            }
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                explorer.handle_entry_mouse_down(
                    pane_id,
                    idx,
                    ev,
                    drag_path.clone(),
                    is_dir,
                    window,
                    cx,
                );
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                explorer.set_active_pane(pane_id, window, cx);
                explorer.open_item_context_menu(idx, ev.position, window, cx);
            }),
        )
        .on_mouse_move(cx.listener(move |explorer, ev: &MouseMoveEvent, _, cx| {
            if is_dir && explorer.is_drag_armed() {
                let pointer = (f32::from(ev.position.x), f32::from(ev.position.y));
                explorer.update_drag(pointer, ev.modifiers.control, cx);
                explorer.set_drop_hover(DropTarget::Directory(hover_path.clone()), cx);
                cx.stop_propagation();
            }
        }))
        .when(is_dir, |card| {
            let drop_dir = entry.path.clone();
            card.drag_over::<ExternalPaths>(move |style, _, _, _| {
                style
                    .bg(theme.accent_subtle())
                    .border_color(theme.accent_bright())
            })
            .on_drop(cx.listener(
                move |explorer, paths: &ExternalPaths, _window, cx| {
                    cx.stop_propagation();
                    explorer.handle_external_paths_drop_to(paths, drop_dir.clone(), cx);
                },
            ))
        })
        .child(if let Some(thumb_img) = thumbnail {
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(88.0))
                .h(px(58.0))
                .rounded(RADIUS_SM)
                .border_1()
                .border_color(theme.border_subtle())
                .bg(theme.bg_code())
                .overflow_hidden()
                .child(
                    img(ImageSource::Render(thumb_img))
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                )
        } else {
            div()
                .flex()
                .items_center()
                .justify_center()
                .h(px(58.0))
                .child(entry_glyph(entry, native_icon, px(42.0), theme))
        })
        .child(name_el.child(entry.name.clone()))
        .child(
            div()
                .w_full()
                .text_center()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(11.0))
                .text_color(theme.text_muted())
                .child(sub_label),
        )
}
