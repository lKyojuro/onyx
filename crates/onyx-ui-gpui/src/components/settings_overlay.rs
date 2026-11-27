//! Settings overlay (`Ctrl+,`) matching `SettingsOverlayModal` in
//! `crates/onyx-ui/ui/components/settings_overlay.slint`: appearance, browsing
//! behavior, tooling and search-indexing rows that apply immediately and
//! persist atomically through `ConfigStore`.

use crate::components::a11y;
use crate::components::{ActionButtonProps, fluent_action_button, fluent_chip, vertical_divider};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_LG, FONT_SM, FONT_XS, OnyxTheme, RADIUS_LG, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, MouseButton, ParentElement,
    Role, StatefulInteractiveElement, Styled, Window, deferred, div, px,
};
use onyx_core::{ThemeMode, ViewMode};
use std::path::{Path, PathBuf};

const CARD_WIDTH: f32 = 640.0;
const CARD_HEIGHT: f32 = 620.0;
const SETTINGS_LAYER: usize = 24;
/// Number of excluded index folders rendered before the list is elided.
pub const MAX_VISIBLE_EXCLUDED_DIRS: usize = 3;

/// Terminal profiles offered by the settings overlay, in display order.
pub const TERMINAL_CHOICES: [(&str, &str); 3] = [
    ("wt", "Windows Terminal"),
    ("pwsh", "PowerShell"),
    ("cmd", "Command Prompt"),
];

/// Returns the terminal profile id that follows `current` in [`TERMINAL_CHOICES`].
#[must_use]
pub fn next_terminal(current: &str) -> &'static str {
    let normalized = match current.trim().to_ascii_lowercase().as_str() {
        "pwsh" | "powershell" => "pwsh",
        "cmd" | "command-prompt" => "cmd",
        _ => "wt",
    };
    let index = TERMINAL_CHOICES
        .iter()
        .position(|(id, _)| *id == normalized)
        .unwrap_or(0);
    let next = (index + 1) % TERMINAL_CHOICES.len();
    TERMINAL_CHOICES.get(next).map_or("wt", |(id, _)| *id)
}

/// Maps a [`ThemeMode`] onto the palette's dark-mode flag. `System` resolves via
/// the Windows app theme.
#[must_use]
pub fn resolve_dark_mode(mode: ThemeMode) -> bool {
    match mode {
        ThemeMode::Dark | ThemeMode::Oled => true,
        ThemeMode::Light => false,
        ThemeMode::System => onyx_windows::detect_system_theme() == ThemeMode::Dark,
    }
}

/// Maps a [`ThemeMode`] onto the palette's pure-black OLED flag.
#[must_use]
pub const fn resolve_oled_mode(mode: ThemeMode) -> bool {
    matches!(mode, ThemeMode::Oled)
}

/// Short label of a theme mode for the segmented control.
#[must_use]
pub const fn theme_label(mode: ThemeMode) -> &'static str {
    match mode {
        ThemeMode::Dark => "Dark",
        ThemeMode::Oled => "OLED",
        ThemeMode::Light => "Light",
        ThemeMode::System => "System",
    }
}

/// Short label of the On/Off switch chips.
#[must_use]
pub const fn toggle_label(on: bool) -> &'static str {
    if on { "On" } else { "Off" }
}

/// Display labels of the excluded index folders, elided to
/// [`MAX_VISIBLE_EXCLUDED_DIRS`] entries.
#[must_use]
pub fn excluded_dir_labels(dirs: &[PathBuf]) -> Vec<String> {
    dirs.iter()
        .take(MAX_VISIBLE_EXCLUDED_DIRS)
        .map(|path| path.display().to_string())
        .collect()
}

/// Count of excluded directories hidden by [`MAX_VISIBLE_EXCLUDED_DIRS`].
#[must_use]
pub fn hidden_excluded_count(dirs: &[PathBuf]) -> usize {
    dirs.len().saturating_sub(MAX_VISIBLE_EXCLUDED_DIRS)
}

/// The view mode that the "Default view" control offers next to `current`.
#[must_use]
pub const fn view_label(mode: ViewMode) -> &'static str {
    match mode {
        ViewMode::Grid => "Grid",
        ViewMode::Details | ViewMode::List => "Details",
    }
}

fn section(title: &str, theme: OnyxTheme) -> impl IntoElement {
    div()
        .w_full()
        .pt(px(6.0))
        .text_size(FONT_XS)
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.accent_bright())
        .child(title.to_string())
}

fn settings_row(
    title: &str,
    description: &str,
    theme: OnyxTheme,
    controls: Vec<AnyElement>,
) -> impl IntoElement {
    let mut value_group = div()
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .gap(px(6.0));
    for control in controls {
        value_group = value_group.child(control);
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.0))
        .w_full()
        .min_h(px(44.0))
        .px(px(14.0))
        .py(px(8.0))
        .rounded(RADIUS_MD)
        .border_1()
        .border_color(theme.border_subtle())
        .bg(theme.bg_surface_alt())
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(2.0))
                .child(
                    div()
                        .text_size(FONT_SM)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_primary())
                        .child(title.to_string()),
                )
                .child(
                    div()
                        .text_size(FONT_XS)
                        .text_color(theme.text_muted())
                        .child(description.to_string()),
                ),
        )
        .child(value_group)
}

/// Renders the deferred settings overlay when open.
pub fn render_settings_overlay(
    view: &OnyxExplorer,
    window: &Window,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    if !view.settings_open {
        return None;
    }
    let theme = view.theme;
    let config = &view.config;
    let theme_mode = if config.theme == ThemeMode::System {
        ThemeMode::System
    } else if theme.oled_mode {
        ThemeMode::Oled
    } else if theme.dark_mode {
        ThemeMode::Dark
    } else {
        ThemeMode::Light
    };
    let default_view = config.default_view;
    let show_hidden = config.show_hidden;
    let show_system = config.show_system;
    let preview_visible = view.preview_visible;
    let dual_pane_default = config.dual_pane_default;
    let calculate_folder_sizes = view.calculate_folder_sizes;
    let terminal = config.normalized_terminal().to_string();
    let excluded_labels = excluded_dir_labels(&config.excluded_index_dirs);
    let hidden_excluded = hidden_excluded_count(&config.excluded_index_dirs);
    let excluded_count = config.excluded_index_dirs.len();

    // Reading order: the header acts first, then every control row top to bottom.
    let ring = &view.modals.settings;
    ring.begin();
    let esc_focus = ring.slot();
    let close_focus = ring.slot();

    let theme_controls: Vec<AnyElement> = [
        ThemeMode::Dark,
        ThemeMode::Oled,
        ThemeMode::Light,
        ThemeMode::System,
    ]
    .into_iter()
    .map(|mode| {
        let handle = ring.slot();
        fluent_chip(
            format!("settings-theme-{}", theme_label(mode)).into(),
            theme_label(mode),
            theme_mode == mode,
            true,
            theme,
            Some(Box::new(cx.listener(move |explorer, _, window, cx| {
                explorer.set_theme_mode(mode, window, cx);
            }))),
        )
        .role(Role::RadioButton)
        .aria_label(format!("Theme: {}", theme_label(mode)))
        .aria_selected(theme_mode == mode)
        .track_focus(&handle)
        .focus_visible(move |style| a11y::focus_ring_style(theme, style))
        .into_any_element()
    })
    .collect();

    let view_controls: Vec<AnyElement> = [ViewMode::Details, ViewMode::Grid]
        .into_iter()
        .map(|mode| {
            let handle = ring.slot();
            let is_active = matches!(
                (default_view, mode),
                (ViewMode::Grid, ViewMode::Grid)
                    | (ViewMode::Details | ViewMode::List, ViewMode::Details)
            );
            fluent_chip(
                format!("settings-view-{}", view_label(mode)).into(),
                view_label(mode),
                is_active,
                true,
                theme,
                Some(Box::new(cx.listener(move |explorer, _, _, cx| {
                    explorer.set_default_view(mode, cx);
                }))),
            )
            .role(Role::RadioButton)
            .aria_label(format!("Default view: {}", view_label(mode)))
            .aria_selected(is_active)
            .track_focus(&handle)
            .focus_visible(move |style| a11y::focus_ring_style(theme, style))
            .into_any_element()
        })
        .collect();

    let boolean_controls = |id: &'static str,
                            title: &'static str,
                            on: bool,
                            set: fn(&mut OnyxExplorer, bool, &mut Context<OnyxExplorer>)|
     -> Vec<AnyElement> {
        [false, true]
            .into_iter()
            .map(|value| {
                let handle = ring.slot();
                fluent_chip(
                    format!("{id}-{}", toggle_label(value)).into(),
                    toggle_label(value),
                    on == value,
                    true,
                    theme,
                    Some(Box::new(cx.listener(move |explorer, _, _, cx| {
                        set(explorer, value, cx);
                    }))),
                )
                .role(Role::CheckBox)
                .aria_label(format!("{title}: {}", toggle_label(value)))
                .track_focus(&handle)
                .focus_visible(move |style| a11y::focus_ring_style(theme, style))
                .into_any_element()
            })
            .collect()
    };

    // Tab order follows the reading order, so the boolean rows claim their
    // focus stops before the terminal and exclusion controls are built.
    let hidden_controls = boolean_controls(
        "settings-hidden",
        "Hidden files",
        show_hidden,
        OnyxExplorer::set_show_hidden,
    );
    let system_controls = boolean_controls(
        "settings-system",
        "Protected system files",
        show_system,
        OnyxExplorer::set_show_system,
    );
    let preview_controls = boolean_controls(
        "settings-preview",
        "Preview pane",
        preview_visible,
        OnyxExplorer::set_preview_visible,
    );
    let dual_controls = boolean_controls(
        "settings-dual",
        "Dual pane by default",
        dual_pane_default,
        OnyxExplorer::set_dual_pane_default,
    );
    let size_controls = boolean_controls(
        "settings-sizes",
        "Calculate folder sizes",
        calculate_folder_sizes,
        OnyxExplorer::set_calculate_folder_sizes,
    );

    let terminal_controls: Vec<AnyElement> = TERMINAL_CHOICES
        .iter()
        .map(|(id, label)| {
            let profile = *id;
            let handle = ring.slot();
            fluent_chip(
                format!("settings-terminal-{profile}").into(),
                label,
                terminal == profile,
                true,
                theme,
                Some(Box::new(cx.listener(move |explorer, _, _, cx| {
                    explorer.set_default_terminal(profile, cx);
                }))),
            )
            .role(Role::RadioButton)
            .aria_label(format!("Default terminal: {label}"))
            .aria_selected(terminal == profile)
            .track_focus(&handle)
            .focus_visible(move |style| a11y::focus_ring_style(theme, style))
            .into_any_element()
        })
        .collect();

    let terminal_next_focus = ring.slot();
    let terminal_cycle_controls: Vec<AnyElement> = vec![
        fluent_action_button(
            ActionButtonProps {
                id: "settings-terminal-next".into(),
                glyph: None,
                trailing_glyph: Some(OnyxGlyph::ChevronRight),
                label: Some("Next"),
                active: false,
                primary: false,
                danger: false,
                enabled: true,
                compact: true,
                min_width: px(28.0),
            },
            theme,
            cx.listener(|explorer, _, _, cx| {
                let next = next_terminal(explorer.config.normalized_terminal());
                explorer.set_default_terminal(next, cx);
            }),
        )
        .aria_label("Next terminal profile")
        .track_focus(&terminal_next_focus)
        .focus_visible(move |style| a11y::focus_ring_style(theme, style))
        .into_any_element(),
    ];

    let add_excluded_focus = ring.slot();
    let mut excluded_controls: Vec<AnyElement> = vec![
        fluent_action_button(
            ActionButtonProps {
                id: "settings-add-excluded".into(),
                glyph: Some(OnyxGlyph::Plus),
                trailing_glyph: None,
                label: Some("Add current folder"),
                active: false,
                primary: false,
                danger: false,
                enabled: true,
                compact: true,
                min_width: px(28.0),
            },
            theme,
            cx.listener(|explorer, _, _, cx| {
                let path = explorer.active_directory();
                explorer.add_excluded_index_dir(path, cx);
            }),
        )
        .aria_label("Add the current folder to the excluded index folders")
        .track_focus(&add_excluded_focus)
        .focus_visible(move |style| a11y::focus_ring_style(theme, style))
        .into_any_element(),
    ];
    if excluded_count > 0 {
        let clear_excluded_focus = ring.slot();
        excluded_controls.push(
            fluent_action_button(
                ActionButtonProps {
                    id: "settings-clear-excluded".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some("Clear"),
                    active: false,
                    primary: false,
                    danger: true,
                    enabled: true,
                    compact: true,
                    min_width: px(28.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.clear_excluded_index_dirs(cx)),
            )
            .aria_label("Clear all excluded index folders")
            .track_focus(&clear_excluded_focus)
            .focus_visible(move |style| a11y::focus_ring_style(theme, style))
            .into_any_element(),
        );
    }

    let mut body = div()
        .id("settings-body")
        .flex()
        .flex_col()
        .w_full()
        .flex_1()
        .min_h(px(0.0))
        .overflow_hidden()
        .p(px(18.0))
        .gap(px(10.0))
        .child(section("APPEARANCE", theme))
        .child(settings_row(
            "Theme",
            "Choose Dark, Light, or follow the Windows app theme.",
            theme,
            theme_controls,
        ))
        .child(settings_row(
            "Default view",
            "Layout used when a folder opens.",
            theme,
            view_controls,
        ))
        .child(section("BROWSING", theme))
        .child(settings_row(
            "Hidden files",
            "Show files and folders marked hidden.",
            theme,
            hidden_controls,
        ))
        .child(settings_row(
            "Protected system files",
            "Also list protected operating-system files.",
            theme,
            system_controls,
        ))
        .child(settings_row(
            "Preview pane",
            "Dock the preview beside the file panes.",
            theme,
            preview_controls,
        ))
        .child(settings_row(
            "Dual pane by default",
            "Start every window with two file panes.",
            theme,
            dual_controls,
        ))
        .child(settings_row(
            "Calculate folder sizes",
            "Recursively size folders in the background.",
            theme,
            size_controls,
        ))
        .child(section("TOOLS", theme))
        .child(settings_row(
            "Default terminal",
            "Profile used by Open Terminal and the context menu.",
            theme,
            terminal_controls
                .into_iter()
                .chain(terminal_cycle_controls)
                .collect(),
        ))
        .child(section("SEARCH INDEXING", theme))
        .child(settings_row(
            "Indexed search",
            "Keep a persistent index in the folder tree and search beyond the open folder.",
            theme,
            boolean_controls(
                "settings-indexed-search",
                "Indexed search",
                view.indexed_search,
                OnyxExplorer::set_indexed_search,
            ),
        ))
        .child(settings_row(
            "Excluded index folders",
            "Folders the background indexer must skip.",
            theme,
            excluded_controls,
        ))
        .child(
            div()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(view.index_status.label()),
        );

    if !excluded_labels.is_empty() {
        let mut list = div()
            .id("settings-excluded-list")
            .role(Role::List)
            .aria_label("Excluded index folders")
            .flex()
            .flex_col()
            .w_full()
            .gap(px(4.0))
            .pl(px(4.0));
        for label in excluded_labels {
            let remove_focus = ring.slot();
            list = list.child(
                div()
                    .id(format!("settings-excluded-{label}"))
                    .role(Role::ListItem)
                    .aria_label(format!("Excluded folder {label}"))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .w_full()
                    .h(px(22.0))
                    .px(px(6.0))
                    .rounded(RADIUS_SM)
                    .bg(theme.bg_input())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(FONT_XS)
                            .text_color(theme.text_secondary())
                            .child(label.clone()),
                    )
                    .child(
                        div()
                            .id(format!("settings-remove-excluded-{label}"))
                            .role(Role::Button)
                            .aria_label(format!("Remove excluded folder {label}"))
                            .track_focus(&remove_focus)
                            .focus_visible(move |style| a11y::focus_ring_style(theme, style))
                            .flex()
                            .items_center()
                            .justify_center()
                            .w(px(18.0))
                            .h(px(18.0))
                            .rounded(RADIUS_SM)
                            .cursor_pointer()
                            .hover(move |style| style.bg(theme.danger_bg()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |explorer, _, _, cx| {
                                    explorer.remove_excluded_index_dir(Path::new(&label), cx);
                                }),
                            )
                            .child(render_glyph(OnyxGlyph::Close, px(10.0), theme.text_muted())),
                    ),
            );
        }
        if hidden_excluded > 0 {
            list = list.child(
                div()
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child(format!("+{hidden_excluded} more")),
            );
        }
        body = body.child(list);
    }

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .flex_shrink_0()
        .px(px(18.0))
        .py(px(14.0))
        .border_b_1()
        .border_color(theme.border_subtle())
        .child(render_glyph(
            OnyxGlyph::Settings,
            px(18.0),
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
                        .child("Settings"),
                )
                .child(
                    div()
                        .text_size(FONT_XS)
                        .text_color(theme.text_muted())
                        .child("Appearance, browsing behavior, terminal, and search indexing"),
                ),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_shrink_0()
                .gap(px(8.0))
                .child(
                    fluent_chip(
                        "settings-esc-chip".into(),
                        "Esc",
                        false,
                        true,
                        theme,
                        Some(Box::new(cx.listener(|explorer, _, window, cx| {
                            explorer.close_settings(window, cx);
                        }))),
                    )
                    .aria_label("Close settings")
                    .aria_keyshortcuts("Escape")
                    .track_focus(&esc_focus)
                    .focus_visible(|style| a11y::focus_ring_style(theme, style)),
                )
                .child(vertical_divider(theme, px(14.0)))
                .child(
                    fluent_action_button(
                        ActionButtonProps {
                            id: "settings-close".into(),
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
                        cx.listener(|explorer, _, window, cx| explorer.close_settings(window, cx)),
                    )
                    .aria_label("Close settings")
                    .track_focus(&close_focus)
                    .focus_visible(|style| a11y::focus_ring_style(theme, style)),
                ),
        );

    let viewport = window.viewport_size();
    let width = f32::from(viewport.width);
    let height = f32::from(viewport.height);
    let card_w = CARD_WIDTH.min((width - 48.0).max(320.0));
    let card_h = CARD_HEIGHT.min((height - 64.0).max(260.0));

    let card = div()
        .id("settings-card")
        .role(Role::Dialog)
        .aria_label("Settings")
        .aria_description(format!(
            "Appearance, browsing behavior, terminal and search indexing, {excluded_count} excluded index folders"
        ))
        .flex()
        .flex_col()
        .w(px(card_w))
        .h(px(card_h))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(theme.border_strong())
        .bg(theme.bg_elevated())
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .child(header)
        .child(body);

    Some(
        deferred(
            div()
                .id("settings-backdrop")
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
                    cx.listener(|explorer, _, window, cx| {
                        explorer.close_settings(window, cx);
                    }),
                )
                .child(card),
        )
        .with_priority(SETTINGS_LAYER),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_cycling_wraps_through_all_profiles() {
        assert_eq!(next_terminal("wt"), "pwsh");
        assert_eq!(next_terminal("pwsh"), "cmd");
        assert_eq!(next_terminal("cmd"), "wt");
        assert_eq!(next_terminal("  POWERSHELL "), "cmd");
        assert_eq!(next_terminal("unknown"), "pwsh");
    }

    #[test]
    fn dark_mode_resolution_covers_explicit_modes() {
        assert!(resolve_dark_mode(ThemeMode::Dark));
        assert!(resolve_dark_mode(ThemeMode::Oled));
        assert!(!resolve_dark_mode(ThemeMode::Light));
        assert!(resolve_oled_mode(ThemeMode::Oled));
        assert!(!resolve_oled_mode(ThemeMode::Dark));
        assert!(!resolve_oled_mode(ThemeMode::Light));
    }

    #[test]
    fn labels_match_the_slint_settings_copy() {
        assert_eq!(theme_label(ThemeMode::Dark), "Dark");
        assert_eq!(theme_label(ThemeMode::Oled), "OLED");
        assert_eq!(theme_label(ThemeMode::Light), "Light");
        assert_eq!(theme_label(ThemeMode::System), "System");
        assert_eq!(view_label(ViewMode::Details), "Details");
        assert_eq!(view_label(ViewMode::List), "Details");
        assert_eq!(view_label(ViewMode::Grid), "Grid");
        assert_eq!(toggle_label(true), "On");
        assert_eq!(toggle_label(false), "Off");
    }

    #[test]
    fn excluded_dir_labels_elide_beyond_the_visible_limit() {
        let dirs: Vec<PathBuf> = (0..5)
            .map(|index| PathBuf::from(format!("C:\\excluded{index}")))
            .collect();
        let labels = excluded_dir_labels(&dirs);
        assert_eq!(labels.len(), MAX_VISIBLE_EXCLUDED_DIRS);
        assert_eq!(labels.first().map(String::as_str), Some("C:\\excluded0"));
        assert_eq!(hidden_excluded_count(&dirs), 2);

        let empty: Vec<PathBuf> = Vec::new();
        assert!(excluded_dir_labels(&empty).is_empty());
        assert_eq!(hidden_excluded_count(&empty), 0);
    }
}
