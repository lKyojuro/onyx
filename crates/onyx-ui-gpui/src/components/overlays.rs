//! Modal overlays and floating toasts matching `crates/onyx-ui/ui/components/overlays.slint`:
//! - `PromptDialogModal` (New Folder, Rename, Permanent Delete confirmation)
//! - `OperationProgressCard` (bottom-right floating progress toast with Cancel)

use crate::components::{ActionButtonProps, fluent_action_button};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_LG, FONT_MD, FONT_SM, FONT_XS, RADIUS_LG, RADIUS_MD};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement,
    Styled, component::input::Input, deferred, div, px,
};

const PROGRESS_TRACK_WIDTH: f32 = 306.0;

/// Renders the centered Prompt Dialog modal (New Folder, Rename, Permanent Delete confirmation).
pub fn render_prompt_dialog(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let prompt = view.prompt_state.as_ref()?;
    let theme = view.theme;
    let is_danger = prompt.is_danger;
    let show_input = prompt.show_input;
    let title = prompt.title.clone();
    let message = prompt.message.clone();
    let confirm_label = if prompt.confirm_label.is_empty() {
        "Confirm".to_string()
    } else {
        prompt.confirm_label.clone()
    };

    let mut header = div().flex().flex_row().items_center().gap(px(8.0));
    if is_danger {
        header = header.child(render_glyph(OnyxGlyph::Warning, px(18.0), theme.danger()));
    }
    header = header.child(
        div()
            .flex_1()
            .text_size(FONT_LG)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if is_danger {
                theme.danger()
            } else {
                theme.text_primary()
            })
            .child(title),
    );

    let mut body = div()
        .flex()
        .flex_col()
        .w_full()
        .p(px(20.0))
        .gap(px(14.0))
        .child(header);

    if !message.is_empty() {
        body = body.child(
            div()
                .text_size(FONT_SM)
                .text_color(theme.text_secondary())
                .child(message),
        );
    }

    if show_input {
        body = body.child(
            div()
                .id("prompt-dialog-input-box")
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .h(px(32.0))
                .px(px(8.0))
                .rounded(RADIUS_MD)
                .border_1()
                .border_color(theme.border_focus())
                .bg(theme.bg_input())
                .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        explorer.cancel_prompt(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(FONT_MD)
                        .text_color(theme.text_primary())
                        .child(Input::new(&view.prompt_input).appearance(false)),
                ),
        );
    }

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .child(fluent_action_button(
            ActionButtonProps {
                id: "prompt-cancel-btn".into(),
                glyph: None,
                trailing_glyph: None,
                label: Some("Cancel"),
                active: false,
                primary: false,
                danger: false,
                enabled: true,
                compact: false,
                min_width: px(72.0),
            },
            theme,
            cx.listener(|explorer, _, window, cx| explorer.cancel_prompt(window, cx)),
        ))
        .child(fluent_action_button(
            ActionButtonProps {
                id: "prompt-confirm-btn".into(),
                glyph: None,
                trailing_glyph: None,
                label: Some(&confirm_label),
                active: false,
                primary: true,
                danger: is_danger,
                enabled: true,
                compact: false,
                min_width: px(84.0),
            },
            theme,
            cx.listener(|explorer, _, window, cx| explorer.confirm_prompt(window, cx)),
        ));

    body = body.child(footer);

    let card = div()
        .id("prompt-dialog-card")
        .relative()
        .flex()
        .flex_col()
        .w(px(440.0))
        .max_w(px(440.0))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(if is_danger {
            theme.danger()
        } else {
            theme.border_strong()
        })
        .bg(theme.bg_elevated())
        .track_focus(&view.prompt_focus_handle)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_key_down(cx.listener(move |explorer, ev: &KeyDownEvent, window, cx| {
            match ev.keystroke.key.as_str() {
                "escape" => {
                    explorer.cancel_prompt(window, cx);
                    cx.stop_propagation();
                }
                "enter" if !show_input => {
                    explorer.confirm_prompt(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
        }))
        .child(
            div()
                .absolute()
                .top(px(1.0))
                .left(px(12.0))
                .right(px(12.0))
                .h(px(1.0))
                .bg(theme.border_specular()),
        )
        .child(body);

    Some(
        deferred(
            div()
                .id("prompt-dialog-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|explorer, _, window, cx| {
                        explorer.cancel_prompt(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(card),
        )
        .with_priority(30),
    )
}

/// Renders the bottom-right floating Operation Progress toast with a Cancel button.
pub fn render_operation_progress_toast(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let op = view.active_operation.as_ref()?;
    let theme = view.theme;
    let clamped = op.progress.clamp(0.0, 1.0);
    let pct = (clamped * 100.0).round() as u32;
    let fill_width = px(PROGRESS_TRACK_WIDTH * clamped);
    let title = if op.title.is_empty() {
        "Running operation..."
    } else {
        op.title
    };

    let mut content = div()
        .flex()
        .flex_col()
        .w_full()
        .p(px(14.0))
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .gap(px(8.0))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_SM)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_primary())
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_size(FONT_XS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.accent_bright())
                        .child(format!("{pct}%")),
                ),
        );

    if !op.current_item.is_empty() {
        content = content.child(
            div()
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(op.current_item.clone()),
        );
    }

    let progress_track = div()
        .w_full()
        .h(px(6.0))
        .rounded(px(3.0))
        .bg(theme.border_subtle())
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(fill_width)
                .rounded(px(3.0))
                .bg(theme.accent_effective()),
        );

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(op.detail.clone()),
        )
        .child(fluent_action_button(
            ActionButtonProps {
                id: "op-toast-cancel".into(),
                glyph: None,
                trailing_glyph: None,
                label: Some("Cancel"),
                active: false,
                primary: false,
                danger: true,
                enabled: true,
                compact: true,
                min_width: px(56.0),
            },
            theme,
            cx.listener(|explorer, _, _, cx| explorer.cancel_operation(cx)),
        ));

    content = content.child(progress_track).child(footer);

    Some(
        deferred(
            div()
                .id("op-progress-toast")
                .absolute()
                .right(px(16.0))
                .bottom(px(42.0))
                .w(px(336.0))
                .rounded(RADIUS_LG)
                .border_1()
                .border_color(theme.border_strong())
                .bg(theme.bg_elevated())
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .absolute()
                        .top(px(1.0))
                        .left(px(12.0))
                        .right(px(12.0))
                        .h(px(1.0))
                        .bg(theme.border_specular()),
                )
                .child(content),
        )
        .with_priority(25),
    )
}
