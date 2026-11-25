//! Custom Windows 11 titlebar + seamless tab strip matching `tab_bar.slint`.

use crate::components::{FluentButtonProps, fluent_button, vertical_divider};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph, render_onyx_emblem};
use crate::theme::{FONT_SM, OnyxTheme, RADIUS_SM};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, Orientation, ParentElement,
    Role, StatefulInteractiveElement, Styled, Window, WindowControlArea, div, px,
};
use onyx_core::ViewMode;

/// Renders the 40px Win11-style custom titlebar with tabs, quick toggles, and native window controls.
pub fn render_tab_bar(
    view: &OnyxExplorer,
    window: &Window,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let theme = view.theme;
    let tabs = view.tabs.tabs();
    let active_idx = view.tabs.active_index();
    let tab_count = tabs.len();
    let is_maximized = window.is_maximized();

    let mut tab_strip = div()
        .id("win11-tab-strip")
        .role(Role::TabList)
        .aria_label("Folder tabs")
        .aria_orientation(Orientation::Horizontal)
        .flex()
        .flex_row()
        .items_end()
        .flex_1()
        .min_w(px(0.0))
        .h_full()
        .gap(px(0.0));

    for (idx, tab) in tabs.iter().enumerate() {
        let is_active = idx == active_idx;
        let next_is_active = idx + 1 == active_idx;
        let tab_id = tab.id;
        let drop_path = tab.path.clone();
        let is_drop_hover = view.drag_hover_is(&tab.path);
        let title = if tab.title.is_empty() {
            "New Tab".to_string()
        } else {
            tab.title.clone()
        };

        let mut tab_card = div()
            .id(("win11-tab", tab_id))
            .role(Role::Tab)
            .aria_label(title.clone())
            .aria_selected(is_active)
            .aria_position_in_set(idx + 1)
            .aria_size_of_set(tab_count)
            .relative()
            .flex()
            .flex_row()
            .items_center()
            .flex_1()
            .min_w(px(110.0))
            .max_w(px(236.0))
            .h(if is_active { px(34.0) } else { px(29.0) })
            .mb(if is_active { px(0.0) } else { px(4.0) })
            .mx(px(1.0))
            .px(px(10.0))
            .gap(px(6.0))
            .cursor_pointer()
            .bg(if is_active {
                theme.bg_chrome()
            } else {
                gpui_kit::transparent_black()
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |explorer, _, _, cx| {
                explorer.switch_tab(tab_id, cx);
            }))
            .on_mouse_move(cx.listener(move |explorer, _, _, cx| {
                explorer.set_drop_hover(
                    crate::components::drag_drop::DropTarget::Directory(drop_path.clone()),
                    cx,
                );
            }));

        if is_active {
            tab_card = tab_card
                .rounded_tl(px(8.0))
                .rounded_tr(px(8.0))
                .border_t_1()
                .border_l_1()
                .border_r_1()
                .border_color(theme.border_subtle())
                // Top specular accent matching `tab_bar.slint`
                .child(
                    div()
                        .absolute()
                        .top(px(1.0))
                        .left(px(10.0))
                        .right(px(10.0))
                        .h(px(1.0))
                        .bg(theme.border_specular()),
                )
                // Seamless bottom bridge over the 1px hairline so active tab merges into NavBar
                .child(
                    div()
                        .absolute()
                        .bottom(px(-1.0))
                        .left(px(0.0))
                        .right(px(0.0))
                        .h(px(2.0))
                        .bg(theme.bg_chrome()),
                )
                .child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded_full()
                        .flex_shrink_0()
                        .bg(theme.accent_bright()),
                );
        } else {
            tab_card = tab_card
                .rounded(px(6.0))
                .hover(move |s| s.bg(theme.bg_hover()));
        }

        if is_drop_hover {
            // Drop target highlight while an internal drag hovers this tab.
            tab_card = tab_card
                .bg(theme.accent_subtle())
                .border_1()
                .border_color(theme.accent_bright());
        }

        let mut title_el = div()
            .flex_1()
            .min_w(px(0.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(FONT_SM)
            .text_color(if is_active {
                theme.text_primary()
            } else {
                theme.text_secondary()
            });
        if is_active {
            title_el = title_el.font_weight(FontWeight::SEMIBOLD);
        }
        tab_card = tab_card.child(title_el.child(title.clone()));

        if tab_count > 1 && idx > 0 {
            tab_card = tab_card.child(
                div()
                    .id(("tab-left", tab_id))
                    .role(Role::Button)
                    .aria_label("Move tab left")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(18.0))
                    .h(px(18.0))
                    .rounded(RADIUS_SM)
                    .cursor_pointer()
                    .hover(move |s| s.bg(theme.bg_pressed()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |explorer, _, _, cx| {
                        cx.stop_propagation();
                        explorer.reorder_tab(idx, idx - 1, cx);
                    }))
                    .child(render_glyph(
                        OnyxGlyph::ChevronLeft,
                        px(11.0),
                        theme.text_muted(),
                    )),
            );
        }

        if tab_count > 1 && idx + 1 < tab_count {
            tab_card = tab_card.child(
                div()
                    .id(("tab-right", tab_id))
                    .role(Role::Button)
                    .aria_label("Move tab right")
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(18.0))
                    .h(px(18.0))
                    .rounded(RADIUS_SM)
                    .cursor_pointer()
                    .hover(move |s| s.bg(theme.bg_pressed()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |explorer, _, _, cx| {
                        cx.stop_propagation();
                        explorer.reorder_tab(idx, idx + 1, cx);
                    }))
                    .child(render_glyph(
                        OnyxGlyph::ChevronRight,
                        px(11.0),
                        theme.text_muted(),
                    )),
            );
        }

        // Close tab button
        tab_card = tab_card.child(
            div()
                .id(("tab-close", tab_id))
                .role(Role::Button)
                .aria_label(format!("Close tab {title}"))
                .flex()
                .items_center()
                .justify_center()
                .w(px(20.0))
                .h(px(20.0))
                .rounded(RADIUS_SM)
                .opacity(if is_active { 1.0 } else { 0.6 })
                .cursor_pointer()
                .hover(move |s| s.bg(theme.bg_pressed()).opacity(1.0))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(cx.listener(move |explorer, _, _, cx| {
                    cx.stop_propagation();
                    explorer.close_tab(Some(tab_id), cx);
                }))
                .child(render_glyph(OnyxGlyph::Close, px(11.0), theme.text_muted())),
        );

        let mut tab_slot = div()
            .flex()
            .flex_row()
            .items_end()
            .flex_1()
            .min_w(px(110.0))
            .max_w(px(238.0))
            .h_full()
            .child(tab_card);

        if !is_active && !next_is_active && idx + 1 < tab_count {
            tab_slot = tab_slot.child(
                div()
                    .flex()
                    .items_center()
                    .h(px(34.0))
                    .child(vertical_divider(theme, px(16.0))),
            );
        }

        tab_strip = tab_strip.child(tab_slot);
    }

    // New tab '+' button sitting cleanly right next to the last tab
    tab_strip = tab_strip
        .child(
            div()
                .flex()
                .items_center()
                .h(px(34.0))
                .pl(px(4.0))
                .pb(px(2.0))
                .child(
                    fluent_button(
                        FluentButtonProps {
                            id: "new-tab-btn".into(),
                            glyph: Some(OnyxGlyph::Plus),
                            label: None,
                            active: false,
                            enabled: true,
                            compact: true,
                            min_width: px(28.0),
                        },
                        theme,
                        cx.listener(|explorer, _, _, cx| explorer.new_tab(cx)),
                    )
                    .aria_label("New tab"),
                ),
        )
        // Native Windows 11 draggable caption area filling remaining titlebar width
        .child(
            div()
                .id("titlebar-drag-spacer")
                .flex_1()
                .min_w(px(32.0))
                .h_full()
                .window_control_area(WindowControlArea::Drag)
                .on_click(|ev, window, _| {
                    if ev.click_count() >= 2 {
                        window.zoom_window();
                    }
                }),
        );

    // Quick view/layout/theme controls on right before native Win11 window buttons
    let quick_controls = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(4.0))
        .pr(px(6.0))
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "toggle-sidebar-btn".into(),
                    glyph: Some(OnyxGlyph::PanelLeft),
                    label: None,
                    active: view.sidebar_visible,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_sidebar(cx)),
            )
            .aria_label(if view.sidebar_visible {
                "Hide the sidebar"
            } else {
                "Show the sidebar"
            }),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "toggle-view-btn".into(),
                    glyph: Some(match view.view_mode {
                        ViewMode::Grid => OnyxGlyph::Grid,
                        ViewMode::Details | ViewMode::List => OnyxGlyph::List,
                    }),
                    label: Some(match view.view_mode {
                        ViewMode::Grid => "Grid",
                        ViewMode::Details | ViewMode::List => "Details",
                    }),
                    active: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_view_mode(cx)),
            )
            .aria_label(match view.view_mode {
                ViewMode::Grid => "Switch to details view",
                ViewMode::Details | ViewMode::List => "Switch to grid view",
            }),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "toggle-hidden-btn".into(),
                    glyph: Some(OnyxGlyph::Eye),
                    label: None,
                    active: view.show_hidden,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_hidden(cx)),
            )
            .aria_label(if view.show_hidden {
                "Hide hidden files"
            } else {
                "Show hidden files"
            }),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "toggle-preview-btn".into(),
                    glyph: Some(OnyxGlyph::PanelRight),
                    label: None,
                    active: view.preview_visible,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.toggle_preview_pane(cx)),
            )
            .aria_label("Toggle the preview pane"),
        )
        .child(
            fluent_button(
                FluentButtonProps {
                    id: "toggle-theme-btn".into(),
                    glyph: Some(if theme.dark_mode {
                        OnyxGlyph::Sun
                    } else {
                        OnyxGlyph::Moon
                    }),
                    label: None,
                    active: false,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| explorer.toggle_theme(window, cx)),
            )
            .aria_label(if theme.dark_mode {
                "Switch to the light theme"
            } else {
                "Switch to the dark theme"
            }),
        );

    // Native Windows 11 window control buttons (`WindowControlArea::Min/Max/Close`)
    let window_controls = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h_full()
        .child(win_control_btn(
            "win-min",
            "Minimize",
            OnyxGlyph::WindowMinimize,
            WindowControlArea::Min,
            false,
            theme,
        ))
        .child(win_control_btn(
            "win-max",
            if is_maximized { "Restore" } else { "Maximize" },
            if is_maximized {
                OnyxGlyph::WindowRestore
            } else {
                OnyxGlyph::WindowMaximize
            },
            WindowControlArea::Max,
            false,
            theme,
        ))
        .child(win_control_btn(
            "win-close",
            "Close",
            OnyxGlyph::Close,
            WindowControlArea::Close,
            true,
            theme,
        ));

    div()
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(40.0))
        .flex_shrink_0()
        .bg(theme.bg_tab_strip())
        // Bottom hairline separating inactive tab strip from NavBar below
        .child(
            div()
                .absolute()
                .bottom(px(0.0))
                .left(px(0.0))
                .right(px(0.0))
                .h(px(1.0))
                .bg(theme.border_subtle()),
        )
        // Left Onyx Emblem (draggable caption edge)
        .child(
            div()
                .id("titlebar-emblem-zone")
                .flex()
                .items_center()
                .justify_center()
                .h_full()
                .pl(px(10.0))
                .pr(px(8.0))
                .window_control_area(WindowControlArea::Drag)
                .child(render_onyx_emblem(theme)),
        )
        .child(tab_strip)
        .child(quick_controls)
        .child(window_controls)
}

fn win_control_btn(
    id: &'static str,
    label: &'static str,
    glyph: OnyxGlyph,
    area: WindowControlArea,
    is_close: bool,
    theme: OnyxTheme,
) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .flex()
        .items_center()
        .justify_center()
        .w(px(42.0))
        .h_full()
        .flex_shrink_0()
        .window_control_area(area)
        .text_color(theme.text_secondary())
        .hover(move |s| {
            if is_close {
                s.bg(gpui_kit::rgb(0xc42b1c))
                    .text_color(gpui_kit::rgb(0xffffff))
            } else {
                s.bg(theme.bg_hover()).text_color(theme.text_primary())
            }
        })
        .active(move |s| {
            if is_close {
                s.bg(gpui_kit::rgb(0xb22416))
                    .text_color(gpui_kit::rgb(0xffffff))
            } else {
                s.bg(theme.bg_pressed())
            }
        })
        .child(render_glyph(glyph, px(13.0), theme.text_secondary()))
}
