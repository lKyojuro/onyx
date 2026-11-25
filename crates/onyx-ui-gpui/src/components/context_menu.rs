//! Custom theme-aware right-click context menu overlay (deferred layer)
//! matching `crates/onyx-ui/ui/components/context_menu.slint` with a bridge
//! to the native Windows Shell menu (`onyx_windows::show_context_menu`).

use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_SM, FONT_XS, OnyxTheme, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    StatefulInteractiveElement, Styled, Window, deferred, div, px,
};
use std::path::PathBuf;

const MENU_WIDTH: f32 = 248.0;
const ROW_HEIGHT: f32 = 30.0;
const SEPARATOR_HEIGHT: f32 = 9.0;
const MENU_PADDING: f32 = 10.0;
const VIEWPORT_MARGIN: f32 = 8.0;

/// Target of an open context menu (file/folder item or empty pane background).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextMenuTarget {
    Item {
        index: usize,
        path: PathBuf,
        is_dir: bool,
    },
    Background {
        dir_path: PathBuf,
    },
}

/// Actions exposed by the custom context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenuAction {
    Open,
    OpenInNewTab,
    OpenWith,
    Cut,
    Copy,
    Paste,
    CopyPath,
    Rename,
    Duplicate,
    OpenInTerminal,
    Properties,
    RecycleBin,
    NewFolder,
    Refresh,
    ShowMoreOptions,
}

/// Runtime state for the currently displayed context menu overlay.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextMenuState {
    pub target: ContextMenuTarget,
    pub anchor_x: f32,
    pub anchor_y: f32,
    pub can_paste: bool,
    pub focused_index: Option<usize>,
}

impl ContextMenuState {
    /// Creates a new context menu state at the given window coordinates.
    #[must_use]
    pub const fn new(
        target: ContextMenuTarget,
        anchor_x: f32,
        anchor_y: f32,
        can_paste: bool,
    ) -> Self {
        Self {
            target,
            anchor_x,
            anchor_y,
            can_paste,
            focused_index: None,
        }
    }

    /// Returns the ordered list of actionable entries for keyboard navigation.
    #[must_use]
    pub fn actionable_items(&self) -> Vec<MenuItemSpec> {
        build_menu_entries(&self.target, self.can_paste)
            .into_iter()
            .filter_map(|entry| match entry {
                MenuEntry::Item(spec) => Some(spec),
                MenuEntry::Separator => None,
            })
            .collect()
    }

    /// Moves keyboard focus forward or backward through menu items.
    pub fn move_focus(&mut self, delta: isize) {
        let count = self.actionable_items().len();
        if count == 0 {
            self.focused_index = None;
            return;
        }
        let next = match self.focused_index {
            Some(current) => {
                if delta >= 0 {
                    (current + delta as usize) % count
                } else {
                    let step = delta.unsigned_abs() % count;
                    (current + count - step) % count
                }
            }
            None => {
                if delta < 0 {
                    count - 1
                } else {
                    0
                }
            }
        };
        self.focused_index = Some(next);
    }

    /// Returns the currently focused enabled action, if any.
    #[must_use]
    pub fn focused_action(&self) -> Option<ContextMenuAction> {
        let idx = self.focused_index?;
        let items = self.actionable_items();
        let item = items.get(idx)?;
        item.enabled.then_some(item.action)
    }
}

/// Visual specification for a single row in the context menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItemSpec {
    pub action: ContextMenuAction,
    pub label: &'static str,
    pub glyph: OnyxGlyph,
    pub shortcut: Option<&'static str>,
    pub enabled: bool,
    pub danger: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuEntry {
    Item(MenuItemSpec),
    Separator,
}

fn build_menu_entries(target: &ContextMenuTarget, can_paste: bool) -> Vec<MenuEntry> {
    match target {
        ContextMenuTarget::Item { is_dir, .. } => {
            let mut entries = Vec::with_capacity(16);
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Open,
                label: "Open",
                glyph: OnyxGlyph::ArrowUpRight,
                shortcut: Some("Enter"),
                enabled: true,
                danger: false,
            }));
            if *is_dir {
                entries.push(MenuEntry::Item(MenuItemSpec {
                    action: ContextMenuAction::OpenInNewTab,
                    label: "Open in New Tab",
                    glyph: OnyxGlyph::Plus,
                    shortcut: None,
                    enabled: true,
                    danger: false,
                }));
            }
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::OpenWith,
                label: "Open With...",
                glyph: OnyxGlyph::File,
                shortcut: None,
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Cut,
                label: "Cut",
                glyph: OnyxGlyph::Cut,
                shortcut: Some("Ctrl+X"),
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Copy,
                label: "Copy",
                glyph: OnyxGlyph::Copy,
                shortcut: Some("Ctrl+C"),
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Paste,
                label: "Paste",
                glyph: OnyxGlyph::Paste,
                shortcut: Some("Ctrl+V"),
                enabled: can_paste,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::CopyPath,
                label: "Copy Path",
                glyph: OnyxGlyph::Copy,
                shortcut: None,
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Rename,
                label: "Rename",
                glyph: OnyxGlyph::Rename,
                shortcut: Some("F2"),
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Duplicate,
                label: "Duplicate",
                glyph: OnyxGlyph::Duplicate,
                shortcut: None,
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::OpenInTerminal,
                label: "Open in Terminal",
                glyph: OnyxGlyph::Terminal,
                shortcut: None,
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Properties,
                label: "Properties",
                glyph: OnyxGlyph::Info,
                shortcut: Some("Alt+Enter"),
                enabled: true,
                danger: false,
            }));
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::RecycleBin,
                label: "Move to Recycle Bin",
                glyph: OnyxGlyph::Trash,
                shortcut: Some("Del"),
                enabled: true,
                danger: true,
            }));
            entries.push(MenuEntry::Separator);
            entries.push(MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::ShowMoreOptions,
                label: "Show more options",
                glyph: OnyxGlyph::ChevronRight,
                shortcut: None,
                enabled: true,
                danger: false,
            }));
            entries
        }
        ContextMenuTarget::Background { .. } => vec![
            MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Paste,
                label: "Paste",
                glyph: OnyxGlyph::Paste,
                shortcut: Some("Ctrl+V"),
                enabled: can_paste,
                danger: false,
            }),
            MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::NewFolder,
                label: "New Folder",
                glyph: OnyxGlyph::FolderPlus,
                shortcut: None,
                enabled: true,
                danger: false,
            }),
            MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::OpenInTerminal,
                label: "Open in Terminal",
                glyph: OnyxGlyph::Terminal,
                shortcut: None,
                enabled: true,
                danger: false,
            }),
            MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::Refresh,
                label: "Refresh",
                glyph: OnyxGlyph::Refresh,
                shortcut: Some("F5"),
                enabled: true,
                danger: false,
            }),
            MenuEntry::Separator,
            MenuEntry::Item(MenuItemSpec {
                action: ContextMenuAction::ShowMoreOptions,
                label: "Show more options",
                glyph: OnyxGlyph::ChevronRight,
                shortcut: None,
                enabled: true,
                danger: false,
            }),
        ],
    }
}

fn estimate_menu_height(entries: &[MenuEntry]) -> f32 {
    let mut height = MENU_PADDING;
    for entry in entries {
        height += match entry {
            MenuEntry::Item(_) => ROW_HEIGHT + 1.0,
            MenuEntry::Separator => SEPARATOR_HEIGHT + 1.0,
        };
    }
    height
}

/// Clamps the floating context menu inside the window viewport with an 8px safety margin.
#[must_use]
pub fn clamp_menu_position(
    anchor_x: f32,
    anchor_y: f32,
    menu_w: f32,
    menu_h: f32,
    viewport_w: f32,
    viewport_h: f32,
) -> (f32, f32) {
    let max_x = (viewport_w - menu_w - VIEWPORT_MARGIN).max(VIEWPORT_MARGIN);
    let max_y = (viewport_h - menu_h - VIEWPORT_MARGIN).max(VIEWPORT_MARGIN);
    (
        anchor_x.clamp(VIEWPORT_MARGIN, max_x),
        anchor_y.clamp(VIEWPORT_MARGIN, max_y),
    )
}

/// Renders the deferred custom context menu overlay when open.
pub fn render_context_menu_overlay(
    view: &OnyxExplorer,
    window: &Window,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let state = view.context_menu.as_ref()?;
    let theme = view.theme;
    let entries = build_menu_entries(&state.target, state.can_paste);
    let menu_h = estimate_menu_height(&entries);

    let viewport = window.viewport_size();
    let viewport_w: f32 = viewport.width.into();
    let viewport_h: f32 = viewport.height.into();
    let (left_x, top_y) = clamp_menu_position(
        state.anchor_x,
        state.anchor_y,
        MENU_WIDTH,
        menu_h,
        viewport_w,
        viewport_h,
    );

    let mut card = div()
        .id("onyx-context-menu-card")
        .absolute()
        .left(px(left_x))
        .top(px(top_y))
        .w(px(MENU_WIDTH))
        .flex()
        .flex_col()
        .p(px(5.0))
        .gap(px(1.0))
        .rounded(RADIUS_MD)
        .border_1()
        .border_color(theme.border_strong())
        .bg(theme.bg_elevated())
        .shadow_lg()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_mouse_down(MouseButton::Right, |_, _, cx| {
            cx.stop_propagation();
        });

    let mut action_idx = 0usize;
    for (entry_idx, entry) in entries.into_iter().enumerate() {
        match entry {
            MenuEntry::Separator => {
                card = card.child(
                    div()
                        .id(("ctx-sep", entry_idx))
                        .w_full()
                        .h(px(SEPARATOR_HEIGHT))
                        .flex()
                        .items_center()
                        .px(px(8.0))
                        .child(div().w_full().h(px(1.0)).bg(theme.border_subtle())),
                );
            }
            MenuEntry::Item(spec) => {
                let is_highlighted = state.focused_index == Some(action_idx);
                card = card.child(render_menu_row(action_idx, spec, is_highlighted, theme, cx));
                action_idx += 1;
            }
        }
    }

    // Full-viewport transparent backdrop closes the menu on any outside click.
    let overlay = div()
        .id("onyx-context-menu-backdrop")
        .absolute()
        .inset_0()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|explorer, _, _, cx| {
                explorer.close_context_menu(cx);
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(
            MouseButton::Right,
            cx.listener(|explorer, _, _, cx| {
                explorer.close_context_menu(cx);
                cx.stop_propagation();
            }),
        )
        .on_mouse_down(
            MouseButton::Middle,
            cx.listener(|explorer, _, _, cx| {
                explorer.close_context_menu(cx);
                cx.stop_propagation();
            }),
        )
        .child(card);

    Some(deferred(overlay).with_priority(30))
}

fn render_menu_row(
    idx: usize,
    spec: MenuItemSpec,
    is_highlighted: bool,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement + use<> {
    let icon_color = if spec.danger && is_highlighted && spec.enabled {
        theme.danger()
    } else if is_highlighted && spec.enabled {
        theme.accent_bright()
    } else {
        theme.text_secondary()
    };

    let label_color = if spec.danger && is_highlighted && spec.enabled {
        theme.danger()
    } else {
        theme.text_primary()
    };

    let base_bg = if !spec.enabled {
        gpui_kit::transparent_black()
    } else if spec.danger && is_highlighted {
        theme.danger_bg()
    } else if is_highlighted {
        theme.bg_hover()
    } else {
        gpui_kit::transparent_black()
    };

    let mut row = div()
        .id(("ctx-menu-row", idx))
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(ROW_HEIGHT))
        .px(px(10.0))
        .gap(px(9.0))
        .rounded(RADIUS_SM)
        .bg(base_bg)
        .opacity(if spec.enabled { 1.0 } else { 0.42 });

    if spec.enabled {
        let action = spec.action;
        row = row
            .cursor_pointer()
            .hover(move |s| {
                if spec.danger {
                    s.bg(theme.danger_bg())
                } else {
                    s.bg(theme.bg_hover())
                }
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(move |explorer, _, window, cx| {
                cx.stop_propagation();
                explorer.execute_context_menu_action(action, window, cx);
            }));
    }

    row = row
        .child(
            div()
                .flex()
                .items_center()
                .justify_center()
                .w(px(16.0))
                .flex_shrink_0()
                .child(render_glyph(spec.glyph, px(15.0), icon_color)),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_SM)
                .font_weight(if is_highlighted && spec.enabled {
                    FontWeight::MEDIUM
                } else {
                    FontWeight::NORMAL
                })
                .text_color(label_color)
                .child(spec.label),
        );

    if let Some(shortcut) = spec.shortcut {
        row = row.child(
            div()
                .flex_shrink_0()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(shortcut),
        );
    }

    row
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_menu_inside_viewport() {
        let (x, y) = clamp_menu_position(1200.0, 780.0, 248.0, 320.0, 1280.0, 800.0);
        assert!((x - (1280.0 - 248.0 - 8.0)).abs() < 1e-4);
        assert!((y - (800.0 - 320.0 - 8.0)).abs() < 1e-4);

        let (x2, y2) = clamp_menu_position(-20.0, 2.0, 248.0, 320.0, 1280.0, 800.0);
        assert!((x2 - 8.0).abs() < 1e-4);
        assert!((y2 - 8.0).abs() < 1e-4);
    }

    #[test]
    fn item_and_background_menus_contain_required_actions() {
        let dir_state = ContextMenuState::new(
            ContextMenuTarget::Item {
                index: 0,
                path: PathBuf::from(r"C:\Temp\Folder"),
                is_dir: true,
            },
            40.0,
            40.0,
            true,
        );
        let dir_actions: Vec<ContextMenuAction> = dir_state
            .actionable_items()
            .into_iter()
            .map(|item| item.action)
            .collect();
        assert!(dir_actions.contains(&ContextMenuAction::Open));
        assert!(dir_actions.contains(&ContextMenuAction::OpenInNewTab));
        assert!(dir_actions.contains(&ContextMenuAction::Cut));
        assert!(dir_actions.contains(&ContextMenuAction::Copy));
        assert!(dir_actions.contains(&ContextMenuAction::Paste));
        assert!(dir_actions.contains(&ContextMenuAction::Rename));
        assert!(dir_actions.contains(&ContextMenuAction::Duplicate));
        assert!(dir_actions.contains(&ContextMenuAction::CopyPath));
        assert!(dir_actions.contains(&ContextMenuAction::RecycleBin));
        assert!(dir_actions.contains(&ContextMenuAction::Properties));
        assert!(dir_actions.contains(&ContextMenuAction::ShowMoreOptions));

        let file_state = ContextMenuState::new(
            ContextMenuTarget::Item {
                index: 1,
                path: PathBuf::from(r"C:\Temp\file.txt"),
                is_dir: false,
            },
            40.0,
            40.0,
            false,
        );
        let file_actions: Vec<ContextMenuAction> = file_state
            .actionable_items()
            .into_iter()
            .map(|item| item.action)
            .collect();
        assert!(!file_actions.contains(&ContextMenuAction::OpenInNewTab));
        assert!(file_actions.contains(&ContextMenuAction::ShowMoreOptions));

        let bg_state = ContextMenuState::new(
            ContextMenuTarget::Background {
                dir_path: PathBuf::from(r"C:\Temp"),
            },
            40.0,
            40.0,
            false,
        );
        let bg_actions: Vec<ContextMenuAction> = bg_state
            .actionable_items()
            .into_iter()
            .map(|item| item.action)
            .collect();
        assert!(bg_actions.contains(&ContextMenuAction::Paste));
        assert!(bg_actions.contains(&ContextMenuAction::NewFolder));
        assert!(bg_actions.contains(&ContextMenuAction::Refresh));
        assert!(bg_actions.contains(&ContextMenuAction::ShowMoreOptions));
    }

    #[test]
    fn keyboard_focus_wraps_cleanly() {
        let mut state = ContextMenuState::new(
            ContextMenuTarget::Background {
                dir_path: PathBuf::from(r"C:\Temp"),
            },
            10.0,
            10.0,
            true,
        );
        assert_eq!(state.focused_index, None);
        state.move_focus(1);
        assert_eq!(state.focused_index, Some(0));
        assert_eq!(state.focused_action(), Some(ContextMenuAction::Paste));
        state.move_focus(-1);
        assert_eq!(
            state.focused_action(),
            Some(ContextMenuAction::ShowMoreOptions)
        );
    }
}
