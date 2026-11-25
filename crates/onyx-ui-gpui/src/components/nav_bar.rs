//! Navigation bar with Back/Forward/Up/Refresh, clickable Breadcrumbs,
//! editable Address Bar (%ENV% expansion + async autocomplete), Favorite Star, and Search.

use crate::components::{FluentButtonProps, fluent_button, fluent_chip};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_MD, FONT_SM, FONT_XS, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    Context, Focusable, FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton,
    ParentElement, Role, StatefulInteractiveElement, Styled, Window, component::input::Input,
    deferred, div, px,
};
use onyx_core::build_breadcrumbs;

/// Renders the 44px navigation bar matching `nav_bar.slint`.
pub fn render_nav_bar(
    view: &OnyxExplorer,
    window: &Window,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let theme = view.theme;
    let can_back = view.active_history().is_some_and(|h| h.can_go_back());
    let can_forward = view.active_history().is_some_and(|h| h.can_go_forward());
    let can_up = view
        .current_path()
        .parent()
        .is_some_and(|p| !p.as_os_str().is_empty());
    let is_fav = view.is_current_favorite();
    let search_focused = view
        .search_input
        .read(cx)
        .focus_handle(cx)
        .is_focused(window);

    let nav_buttons = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(2.0))
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "nav-back".into(),
                    glyph: Some(OnyxGlyph::Back),
                    label: None,
                    active: false,
                    enabled: can_back,
                    compact: false,
                    min_width: px(30.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.go_back(cx)),
            )
            .aria_label("Back")
            .aria_keyshortcuts("Alt+Left"),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "nav-forward".into(),
                    glyph: Some(OnyxGlyph::Forward),
                    label: None,
                    active: false,
                    enabled: can_forward,
                    compact: false,
                    min_width: px(30.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.go_forward(cx)),
            )
            .aria_label("Forward")
            .aria_keyshortcuts("Alt+Right"),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "nav-up".into(),
                    glyph: Some(OnyxGlyph::Up),
                    label: None,
                    active: false,
                    enabled: can_up,
                    compact: false,
                    min_width: px(30.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.go_up(cx)),
            )
            .aria_label("Up one level")
            .aria_keyshortcuts("Alt+Up"),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "nav-refresh".into(),
                    glyph: Some(OnyxGlyph::Refresh),
                    label: None,
                    active: false,
                    enabled: true,
                    compact: false,
                    min_width: px(30.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.refresh(cx)),
            )
            .aria_label("Refresh")
            .aria_keyshortcuts("F5"),
        );

    // Breadcrumbs / Editable Address Bar
    let address_box = if view.address_editing {
        div()
            .id("address-box-edit")
            .aria_label("Address bar")
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(240.0))
            .h(px(32.0))
            .px(px(8.0))
            .rounded(RADIUS_MD)
            .border_1()
            .border_color(theme.border_focus())
            .bg(theme.bg_input())
            .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
                explorer.handle_address_key_down(ev, window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .text_size(FONT_MD)
                    .text_color(theme.text_primary())
                    .child(
                        Input::new(&view.address_input)
                            .appearance(false)
                            .aria_label("Address bar")
                            .accessibility_id("address-bar-input"),
                    ),
            )
            .into_any_element()
    } else {
        let breadcrumbs = build_breadcrumbs(view.current_path());
        let crumb_len = breadcrumbs.len();
        let crumb_row_label = format!("Current path, {}", view.current_path().display());
        let mut crumb_row = div()
            .id("address-breadcrumbs")
            .role(Role::Group)
            .aria_label(crumb_row_label)
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(0.0))
            .h(px(32.0))
            .pl(px(8.0))
            .pr(px(6.0))
            .gap(px(2.0))
            .rounded(RADIUS_MD)
            .border_1()
            .border_color(theme.border_subtle())
            .bg(theme.bg_input())
            .overflow_hidden();

        for (idx, seg) in breadcrumbs.into_iter().enumerate() {
            if idx > 0 {
                crumb_row = crumb_row.child(render_glyph(
                    OnyxGlyph::ChevronRight,
                    px(12.0),
                    theme.text_muted(),
                ));
            }
            let is_last = idx + 1 == crumb_len;
            let seg_path = seg.path;
            let crumb_label = format!("Navigate to {}", seg.label);
            let mut seg_el = div()
                .id(("nav-crumb", idx))
                .role(Role::Button)
                .aria_label(crumb_label)
                .flex()
                .items_center()
                .h(px(24.0))
                .px(px(6.0))
                .rounded(RADIUS_SM)
                .cursor_pointer()
                .text_size(FONT_SM)
                .text_color(if is_last {
                    theme.text_primary()
                } else {
                    theme.text_secondary()
                })
                .hover(move |s| s.bg(theme.bg_hover()).text_color(theme.text_primary()))
                .on_click(cx.listener(move |explorer, _, _, cx| {
                    explorer.navigate_to(seg_path.clone(), cx);
                }));
            if is_last {
                seg_el = seg_el.font_weight(FontWeight::SEMIBOLD);
            }
            crumb_row = crumb_row.child(seg_el.child(seg.label));
        }

        // Empty space click enters editable address mode
        crumb_row = crumb_row
            .child(
                div()
                    .id("address-empty-click")
                    .role(Role::Button)
                    .aria_label("Edit address bar")
                    .flex_1()
                    .min_w(px(24.0))
                    .h_full()
                    .cursor_text()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|explorer, _, window, cx| {
                            explorer.begin_address_edit(window, cx);
                        }),
                    ),
            )
            // Pin / unpin favorite star inside address bar
            .child(
                div()
                    .id("address-fav-btn")
                    .role(Role::Button)
                    .aria_label(if is_fav {
                        "Remove folder from favorites"
                    } else {
                        "Pin folder to favorites"
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(24.0))
                    .h(px(24.0))
                    .rounded(RADIUS_SM)
                    .cursor_pointer()
                    .hover(move |s| s.bg(theme.bg_hover()))
                    .on_click(cx.listener(|explorer, _, _, cx| {
                        explorer.toggle_favorite(cx);
                    }))
                    .child(render_glyph(
                        if is_fav {
                            OnyxGlyph::StarFilled
                        } else {
                            OnyxGlyph::Star
                        },
                        px(15.0),
                        if is_fav {
                            theme.accent_bright()
                        } else {
                            theme.text_muted()
                        },
                    )),
            );

        crumb_row.into_any_element()
    };

    // Search Bar (`w(280px)`, matching `nav_bar.slint`)
    let search_box = div()
        .id("nav-search-box")
        .aria_label("Search")
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .w(px(280.0))
        .h(px(32.0))
        .pl(px(8.0))
        .pr(px(5.0))
        .gap(px(5.0))
        .rounded(RADIUS_MD)
        .border_1()
        .border_color(if search_focused {
            theme.border_focus()
        } else {
            theme.border_subtle()
        })
        .bg(theme.bg_input())
        .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
            if ev.keystroke.key == "escape" {
                explorer.clear_search_and_focus_main(window, cx);
                cx.stop_propagation();
            }
        }))
        .child(render_glyph(
            OnyxGlyph::Search,
            px(15.0),
            theme.text_muted(),
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .text_size(FONT_MD)
                .text_color(theme.text_primary())
                .child(
                    Input::new(&view.search_input)
                        .appearance(false)
                        .cleanable(true)
                        .aria_label(if view.indexed_search {
                            "Search the search index, Ctrl+F"
                        } else {
                            "Filter this folder, Ctrl+F"
                        })
                        .accessibility_id("folder-search-input"),
                ),
        )
        .child(
            fluent_chip(
                "search-index-chip".into(),
                "Index",
                view.indexed_search,
                true,
                theme,
                Some(Box::new(cx.listener(|explorer, _, _, cx| {
                    explorer.toggle_indexed_search(cx);
                }))),
            )
            .aria_label("Search the Windows search index instead of the folder"),
        );

    div()
        .id("onyx-nav-bar")
        .role(Role::Toolbar)
        .aria_label("Navigation")
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(44.0))
        .flex_shrink_0()
        .px(px(10.0))
        .py(px(6.0))
        .gap(px(6.0))
        .bg(theme.bg_chrome())
        .border_b_1()
        .border_color(theme.border_subtle())
        .child(nav_buttons)
        .child(address_box)
        .child(search_box)
}

/// Renders the floating autocomplete suggestion dropdown below the address bar when editing.
pub fn render_address_suggestions_overlay(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    if !view.address_editing || view.address_suggestions.is_empty() {
        return None;
    }
    let theme = view.theme;

    let mut list = div()
        .id("address-suggestion-list")
        .role(Role::List)
        .aria_label("Folder suggestions")
        .flex()
        .flex_col()
        .w_full()
        .p(px(4.0))
        .gap(px(2.0))
        .rounded(RADIUS_MD)
        .bg(theme.bg_elevated())
        .border_1()
        .border_color(theme.border_strong());

    for (idx, suggestion) in view.address_suggestions.iter().enumerate() {
        let is_highlighted = view.suggestion_index == Some(idx);
        let target = suggestion.clone();
        list = list.child(
            div()
                .id(("addr-suggestion", idx))
                .role(Role::ListItem)
                .aria_label(format!("Navigate to {suggestion}"))
                .aria_selected(is_highlighted)
                .flex()
                .flex_row()
                .items_center()
                .h(px(28.0))
                .px(px(8.0))
                .gap(px(8.0))
                .rounded(RADIUS_SM)
                .cursor_pointer()
                .bg(if is_highlighted {
                    theme.bg_selected()
                } else {
                    gpui_kit::transparent_black()
                })
                .hover(move |s| s.bg(theme.bg_hover()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |explorer, _, window, cx| {
                        explorer.accept_address_suggestion(&target, window, cx);
                    }),
                )
                .child(render_glyph(
                    OnyxGlyph::Folder,
                    px(14.0),
                    theme.folder_tint(),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_XS)
                        .text_color(theme.text_primary())
                        .child(suggestion.clone()),
                ),
        );
    }

    Some(
        deferred(
            div()
                .absolute()
                .top(px(84.0))
                .left(px(140.0))
                .right(px(296.0))
                .child(list),
        )
        .with_priority(20),
    )
}
