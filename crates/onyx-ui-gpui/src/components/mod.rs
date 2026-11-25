//! Modular GPUI UI components achieving 1:1 visual parity with `crates/onyx-ui/ui/`.

pub mod archive_overlay;
pub mod command_palette;
pub mod context_menu;
pub mod drag_drop;
pub mod file_pane;
pub mod nav_bar;
pub mod overlays;
pub mod preview_pane;
pub mod settings_overlay;
pub mod sidebar;
pub mod smooth_scroll;
pub mod splitter;
pub mod status_bar;
pub mod tab_bar;
pub mod toolbar;

use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_SM, FONT_XS, OnyxTheme, RADIUS_SM};
use gpui_kit::{
    App, ClickEvent, ElementId, InteractiveElement, IntoElement, MouseButton, MouseMoveEvent,
    ParentElement, Pixels, StatefulInteractiveElement, Styled, Window, div, px,
};

/// Subtle 1px vertical divider matching `VerticalDivider` in `common.slint`.
pub fn vertical_divider(theme: OnyxTheme, height: Pixels) -> impl IntoElement {
    div()
        .w(px(1.0))
        .h(height)
        .flex_shrink_0()
        .bg(theme.border_subtle())
}

/// Configuration for [`fluent_button`].
pub struct FluentButtonProps<'a> {
    pub id: ElementId,
    pub glyph: Option<OnyxGlyph>,
    pub label: Option<&'a str>,
    pub active: bool,
    pub enabled: bool,
    pub compact: bool,
    pub min_width: Pixels,
}

/// Fluent / Nocturne toolbar & navigation button matching `FluentButton` in `common.slint`.
pub fn fluent_button(
    props: FluentButtonProps<'_>,
    theme: OnyxTheme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let height = if props.compact { px(26.0) } else { px(28.0) };
    let fg = if !props.enabled {
        theme.text_muted()
    } else if props.active {
        theme.accent_bright()
    } else {
        theme.text_primary()
    };

    let px_pad = if props.label.is_some() {
        if props.compact { px(8.0) } else { px(10.0) }
    } else {
        px(6.0)
    };

    let mut base = div()
        .id(props.id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(height)
        .min_w(props.min_width)
        .px(px_pad)
        .gap(px(5.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(if props.active {
            theme.accent_border()
        } else {
            gpui_kit::transparent_black()
        })
        .bg(if !props.enabled {
            gpui_kit::transparent_black()
        } else if props.active {
            theme.accent_subtle()
        } else {
            gpui_kit::transparent_black()
        });

    if let Some(glyph) = props.glyph {
        base = base.child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .opacity(if props.enabled { 1.0 } else { 0.38 })
                .child(render_glyph(glyph, px(15.0), fg)),
        );
    }

    if let Some(label) = props.label
        && !label.is_empty()
    {
        let mut text_el = div()
            .text_size(FONT_SM)
            .text_color(fg)
            .opacity(if props.enabled { 1.0 } else { 0.38 });
        if props.active {
            text_el = text_el.font_weight(gpui_kit::FontWeight::SEMIBOLD);
        }
        base = base.child(text_el.child(label.to_string()));
    }

    if props.enabled {
        base.cursor_pointer()
            .hover(move |s| {
                s.bg(if props.active {
                    theme.bg_selected()
                } else {
                    theme.bg_hover()
                })
            })
            .active(move |s| s.bg(theme.bg_pressed()))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(move |ev, window, cx| {
                cx.stop_propagation();
                on_click(ev, window, cx);
            })
    } else {
        base
    }
}

/// Boxed UI click callback used by optional-action widgets.
pub type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// Boxed pointer-move callback used for optional drop-hover tracking.
pub type MouseMoveHandler = Box<dyn Fn(&MouseMoveEvent, &mut Window, &mut App) + 'static>;

/// Configuration for [`fluent_action_button`] supporting primary, danger, and trailing glyphs.
#[derive(Debug)]
pub struct ActionButtonProps<'a> {
    pub id: ElementId,
    pub glyph: Option<OnyxGlyph>,
    pub trailing_glyph: Option<OnyxGlyph>,
    pub label: Option<&'a str>,
    pub active: bool,
    pub primary: bool,
    pub danger: bool,
    pub enabled: bool,
    pub compact: bool,
    pub min_width: Pixels,
}

/// Extended Fluent / Nocturne button matching `FluentButton` variants (`primary`, `danger`,
/// `trailing-icon`) in `common.slint`.
pub fn fluent_action_button(
    props: ActionButtonProps<'_>,
    theme: OnyxTheme,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let height = if props.compact { px(26.0) } else { px(30.0) };
    let fg = if !props.enabled {
        theme.text_muted()
    } else if props.primary && props.danger {
        gpui_kit::rgb(0xffffff).into()
    } else if props.primary {
        theme.text_on_accent()
    } else if props.danger {
        theme.danger()
    } else if props.active {
        theme.accent_bright()
    } else {
        theme.text_primary()
    };

    let bg = if !props.enabled {
        gpui_kit::transparent_black()
    } else if props.primary && props.danger {
        theme.danger()
    } else if props.primary {
        theme.accent_effective()
    } else if props.active {
        theme.accent_subtle()
    } else {
        gpui_kit::transparent_black()
    };

    let border = if !props.enabled {
        gpui_kit::transparent_black()
    } else if props.primary && props.danger {
        theme.danger()
    } else if props.primary || props.active {
        theme.accent_border()
    } else if props.danger && props.label.is_some() {
        theme.danger()
    } else {
        gpui_kit::transparent_black()
    };

    let px_pad = if props.label.is_some() {
        if props.compact { px(8.0) } else { px(12.0) }
    } else {
        px(6.0)
    };

    let mut base = div()
        .id(props.id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(height)
        .min_w(props.min_width)
        .px(px_pad)
        .gap(px(5.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(border)
        .bg(bg);

    if let Some(glyph) = props.glyph {
        base = base.child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .opacity(if props.enabled { 1.0 } else { 0.38 })
                .child(render_glyph(glyph, px(15.0), fg)),
        );
    }

    if let Some(label) = props.label
        && !label.is_empty()
    {
        let mut text_el = div()
            .text_size(FONT_SM)
            .text_color(fg)
            .opacity(if props.enabled { 1.0 } else { 0.38 });
        if props.active || props.primary {
            text_el = text_el.font_weight(gpui_kit::FontWeight::SEMIBOLD);
        }
        base = base.child(text_el.child(label.to_string()));
    }

    if let Some(trailing) = props.trailing_glyph {
        base = base.child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .opacity(if props.enabled { 1.0 } else { 0.38 })
                .child(render_glyph(trailing, px(13.0), fg)),
        );
    }

    if props.enabled {
        let hover_bg = if props.primary && props.danger {
            theme.danger_hover()
        } else if props.primary {
            theme.accent_bright()
        } else if props.danger {
            theme.danger_bg()
        } else if props.active {
            theme.bg_selected()
        } else {
            theme.bg_hover()
        };
        base.cursor_pointer()
            .hover(move |s| s.bg(hover_bg))
            .active(move |s| s.bg(theme.bg_pressed()))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(move |ev, window, cx| {
                cx.stop_propagation();
                on_click(ev, window, cx);
            })
    } else {
        base
    }
}

/// Small pill badge / toggle chip matching `FluentChip` in `common.slint`.
pub fn fluent_chip(
    id: ElementId,
    label: &str,
    active: bool,
    interactive: bool,
    theme: OnyxTheme,
    on_click: Option<ClickHandler>,
) -> impl IntoElement {
    let mut chip = div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .flex_shrink_0()
        .h(px(22.0))
        .px(px(7.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(if active {
            theme.accent_border()
        } else {
            theme.border_subtle()
        })
        .bg(if active {
            theme.accent_subtle()
        } else {
            theme.bg_badge()
        })
        .text_size(FONT_XS)
        .font_weight(if active {
            gpui_kit::FontWeight::SEMIBOLD
        } else {
            gpui_kit::FontWeight::MEDIUM
        })
        .text_color(if active {
            theme.accent_bright()
        } else {
            theme.text_secondary()
        })
        .child(label.to_string());

    if interactive {
        chip = chip
            .cursor_pointer()
            .hover(move |s| if !active { s.bg(theme.bg_hover()) } else { s })
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            });
        if let Some(handler) = on_click {
            chip = chip.on_click(move |ev, window, cx| {
                cx.stop_propagation();
                handler(ev, window, cx);
            });
        }
    }

    chip
}
