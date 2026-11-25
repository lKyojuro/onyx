//! Status bar matching `status_bar.slint` + animated 7x7 Amber LED Dot-Matrix Pill.

use crate::components::a11y;
use crate::components::{fluent_chip, vertical_divider};
use crate::explorer::OnyxExplorer;
use crate::led_pill::render_led_status_pill;
use crate::theme::FONT_XS;
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Role,
    StatefulInteractiveElement, Styled, div, px,
};
use onyx_core::{ViewMode, normalize_display_path};

/// Renders the 30px bottom status bar with counts, selection info, path, mode badges, and LED pill.
pub fn render_status_bar(view: &OnyxExplorer, cx: &mut Context<OnyxExplorer>) -> impl IntoElement {
    let theme = view.theme;
    let active = view.active_pane_state();
    let count = active.entries.len();
    let items_text = if count == 1 {
        "1 item".to_string()
    } else {
        format!("{count} items")
    };

    let selection_text = active
        .selected_index
        .and_then(|idx| active.entries.get(idx))
        .map(|entry| {
            let (size_label, _) = view.entry_size_info_for(view.active_pane, entry);
            if size_label.is_empty() {
                "1 selected".to_string()
            } else {
                format!("1 selected · {size_label}")
            }
        });

    let display_path = normalize_display_path(view.pane_path(view.active_pane));
    let search_status = view.search_status_label();

    // One spoken sentence for the whole bar: counts, selection, path and modes.
    let summary = format!(
        "{items_text}, {}, {display_path}, {} view{}",
        selection_text.as_deref().unwrap_or("nothing selected"),
        a11y::view_mode_label(view.view_mode),
        if view.dual_pane { ", dual pane" } else { "" },
    );

    let mut left_group = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(8.0))
        .child(
            div()
                .flex_shrink_0()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(items_text),
        );

    if let Some(sel) = selection_text {
        left_group = left_group.child(vertical_divider(theme, px(12.0))).child(
            div()
                .flex_shrink_0()
                .text_size(FONT_XS)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.accent_bright())
                .child(sel),
        );
    }

    if !display_path.is_empty() {
        left_group = left_group.child(vertical_divider(theme, px(12.0))).child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(display_path),
        );
    }

    let mut right_group = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(6.0));

    if let Some(msg) = view.status_message.as_deref()
        && !msg.is_empty()
    {
        right_group = right_group.child(
            div()
                .max_w(px(340.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(msg.to_string()),
        );
    }

    if let Some(search) = search_status {
        right_group = right_group.child(
            div()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(search),
        );
    }

    right_group = right_group
        .child(fluent_chip(
            "status-index-chip".into(),
            "Indexed",
            view.indexed_search,
            true,
            theme,
            Some(Box::new(cx.listener(|explorer, _, _, cx| {
                explorer.toggle_indexed_search(cx);
            }))),
        ))
        .child(fluent_chip(
            "status-dual-chip".into(),
            "Dual",
            view.dual_pane,
            true,
            theme,
            Some(Box::new(cx.listener(|explorer, _, _, cx| {
                explorer.toggle_dual_pane(cx);
            }))),
        ))
        .child(
            fluent_chip(
                "status-dual-chip".into(),
                "Dual",
                view.dual_pane,
                true,
                theme,
                Some(Box::new(cx.listener(|explorer, _, _, cx| {
                    explorer.toggle_dual_pane(cx);
                }))),
            )
            .aria_label("Dual-pane layout"),
        )
        .child(fluent_chip(
            "status-view-chip".into(),
            match view.view_mode {
                ViewMode::Grid => "Grid",
                ViewMode::Details | ViewMode::List => "Details",
            },
            false,
            false,
            theme,
            None,
        ))
        .child(render_led_status_pill(
            &view.led_label(),
            view.anim_frame,
            view.is_activity_active(),
            theme,
        ));

    div()
        .id("onyx-status-bar")
        .role(Role::Status)
        .aria_label(summary)
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .w_full()
        .h(px(30.0))
        .flex_shrink_0()
        .pl(px(12.0))
        .pr(px(10.0))
        .gap(px(8.0))
        .bg(theme.bg_chrome())
        .border_t_1()
        .border_color(theme.border_subtle())
        .child(left_group)
        .child(right_group)
}
