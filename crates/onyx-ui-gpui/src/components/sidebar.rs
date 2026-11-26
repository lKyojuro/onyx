//! Sidebar with real Windows Shell icons, Favorites, Quick Access, and Drives with capacity bars.

use crate::components::{ClickHandler, MouseMoveHandler};
use crate::explorer::{OnyxExplorer, same_path};
use crate::icons::{OnyxGlyph, RECYCLE_BIN_ICON_PREFIX, path_icon_key, render_glyph};
use crate::theme::{FONT_SM, FONT_XS, OnyxTheme, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    Context, ExternalPaths, FontWeight, ImageSource, InteractiveElement, IntoElement, MouseButton,
    ParentElement, RenderImage, Role, StatefulInteractiveElement, Styled, div, img, px,
};
use onyx_config::{SIDEBAR_GROUP_DRIVES, SIDEBAR_GROUP_FAVORITES, SIDEBAR_GROUP_QUICK_ACCESS};
use onyx_core::{KnownFolderKind, format_bytes, normalize_display_path};
use std::path::PathBuf;
use std::sync::Arc;

/// Renders the collapsible, resizable sidebar matching `sidebar.slint`.
pub fn render_sidebar(
    view: &OnyxExplorer,
    width: f32,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    if !view.sidebar_visible {
        return None;
    }

    let theme = view.theme;
    let current = view.current_path();
    let home_icon = view
        .icons
        .get(&path_icon_key(&view.home_path))
        .map(Arc::clone);

    // Home item
    let home_active = !view.home_path.as_os_str().is_empty() && same_path(current, &view.home_path);
    let home_target = view.home_path.clone();
    let home_drop_hover = view.drag_hover_is(&view.home_path);
    let home_hover_target = view.home_path.clone();
    let home_external_drop = view.home_path.clone();
    let home_row = sidebar_nav_row(
        "sidebar-home".into(),
        "Home",
        OnyxGlyph::Home,
        home_icon,
        home_active,
        false,
        theme,
        cx.listener(move |explorer, _, _, cx| {
            explorer.navigate_to(home_target.clone(), cx);
        }),
        None,
        home_drop_hover,
        Some(Box::new(cx.listener(move |explorer, _, _, cx| {
            explorer.set_drop_hover(
                crate::components::drag_drop::DropTarget::Directory(home_hover_target.clone()),
                cx,
            );
        }))),
        Some(home_external_drop),
        cx,
    );

    // Favorites section
    let fav_collapsed = view.is_sidebar_group_collapsed(SIDEBAR_GROUP_FAVORITES);
    let mut fav_col = div().flex().flex_col().gap(px(3.0)).child(section_header(
        "hdr-favorites",
        "FAVORITES",
        fav_collapsed,
        theme,
        cx.listener(|explorer, _, _, cx| {
            explorer.toggle_sidebar_group(SIDEBAR_GROUP_FAVORITES, cx);
        }),
    ));

    if !fav_collapsed {
        if view.favorites.is_empty() {
            fav_col = fav_col.child(
                div()
                    .px(px(6.0))
                    .py(px(2.0))
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .child("Pin folders from the address bar"),
            );
        } else {
            for (idx, fav) in view.favorites.iter().enumerate() {
                let is_active = same_path(current, &fav.path);
                let nav_target = fav.path.clone();
                let unpin_target = fav.path.clone();
                let hover_target = fav.path.clone();
                let fav_external_drop = fav.path.clone();
                let fav_drop_hover = view.drag_hover_is(&fav.path);
                let native_icon = view.icons.get(&path_icon_key(&fav.path)).map(Arc::clone);
                fav_col = fav_col.child(sidebar_nav_row(
                    ("sidebar-fav", idx).into(),
                    &fav.name,
                    OnyxGlyph::StarFilled,
                    native_icon,
                    is_active,
                    true,
                    theme,
                    cx.listener(move |explorer, _, _, cx| {
                        explorer.navigate_to(nav_target.clone(), cx);
                    }),
                    Some(Box::new(cx.listener(move |explorer, _, _, cx| {
                        explorer.unpin_favorite(&unpin_target, cx);
                    }))),
                    fav_drop_hover,
                    Some(Box::new(cx.listener(move |explorer, _, _, cx| {
                        explorer.set_drop_hover(
                            crate::components::drag_drop::DropTarget::Directory(
                                hover_target.clone(),
                            ),
                            cx,
                        );
                    }))),
                    Some(fav_external_drop),
                    cx,
                ));
            }
        }
    }

    // Quick Access (Known Folders) section
    let qa_collapsed = view.is_sidebar_group_collapsed(SIDEBAR_GROUP_QUICK_ACCESS);
    let mut qa_col = div().flex().flex_col().gap(px(3.0)).child(section_header(
        "hdr-quick-access",
        "QUICK ACCESS",
        qa_collapsed,
        theme,
        cx.listener(|explorer, _, _, cx| {
            explorer.toggle_sidebar_group(SIDEBAR_GROUP_QUICK_ACCESS, cx);
        }),
    ));

    if !qa_collapsed {
        for (idx, folder) in view.known_folders.iter().enumerate() {
            if folder.kind == KnownFolderKind::Home {
                continue;
            }
            let glyph = match folder.kind {
                KnownFolderKind::Desktop => OnyxGlyph::Desktop,
                KnownFolderKind::Downloads => OnyxGlyph::Download,
                KnownFolderKind::Documents => OnyxGlyph::File,
                KnownFolderKind::Pictures => OnyxGlyph::Image,
                KnownFolderKind::Music => OnyxGlyph::Music,
                KnownFolderKind::Videos => OnyxGlyph::Video,
                KnownFolderKind::Home => OnyxGlyph::Home,
            };
            let is_active = same_path(current, &folder.path);
            let target = folder.path.clone();
            let qa_hover_target = folder.path.clone();
            let qa_external_drop = folder.path.clone();
            let qa_drop_hover = view.drag_hover_is(&folder.path);
            let native_icon = view.icons.get(&path_icon_key(&folder.path)).map(Arc::clone);
            qa_col = qa_col.child(sidebar_nav_row(
                ("sidebar-qa", idx).into(),
                &folder.name,
                glyph,
                native_icon,
                is_active,
                false,
                theme,
                cx.listener(move |explorer, _, _, cx| {
                    explorer.navigate_to(target.clone(), cx);
                }),
                None,
                qa_drop_hover,
                Some(Box::new(cx.listener(move |explorer, _, _, cx| {
                    explorer.set_drop_hover(
                        crate::components::drag_drop::DropTarget::Directory(
                            qa_hover_target.clone(),
                        ),
                        cx,
                    );
                }))),
                Some(qa_external_drop),
                cx,
            ));
        }
    }

    // Recycle Bin: a Shell namespace folder, so it sits beside the drives and not
    // among the known folders that resolve to a filesystem path.
    // The Recycle Bin has no filesystem path, so it uses the Windows stock icon
    // (one per state) instead of a shell-extracted one.
    let recycle_state = if view.recycle_bin_has_items {
        ":full"
    } else {
        ":empty"
    };
    let recycle_icon = view
        .icons
        .get(&format!("{RECYCLE_BIN_ICON_PREFIX}{recycle_state}"))
        .cloned();
    let recycle_row = sidebar_nav_row(
        "sidebar-recycle-bin".into(),
        "Recycle Bin",
        OnyxGlyph::Trash,
        recycle_icon,
        view.is_recycle_bin_mode(),
        false,
        theme,
        cx.listener(|explorer, _, _, cx| {
            explorer.open_recycle_bin(cx);
        }),
        None,
        false,
        None,
        None,
        cx,
    );

    // Drives section
    let drives_collapsed = view.is_sidebar_group_collapsed(SIDEBAR_GROUP_DRIVES);
    let mut drives_col = div().flex().flex_col().gap(px(6.0)).child(section_header(
        "hdr-drives",
        "DRIVES",
        drives_collapsed,
        theme,
        cx.listener(|explorer, _, _, cx| {
            explorer.toggle_sidebar_group(SIDEBAR_GROUP_DRIVES, cx);
        }),
    ));

    if !drives_collapsed {
        for (idx, drive) in view.drives.iter().enumerate() {
            let is_active = same_path(current, &drive.root);
            let target: PathBuf = drive.root.clone();
            let drive_hover_target: PathBuf = drive.root.clone();
            let drive_external_drop: PathBuf = drive.root.clone();
            let drive_drop_hover = view.drag_hover_is(&drive.root);
            let native_icon = view.icons.get(&path_icon_key(&drive.root)).map(Arc::clone);
            let root_display = normalize_display_path(&drive.root);
            let title = if drive.label.trim().is_empty() {
                format!("Local Disk ({root_display})")
            } else {
                format!("{} ({root_display})", drive.label)
            };
            let free_label = if drive.total_bytes == 0 {
                String::new()
            } else {
                format!(
                    "{} free of {}",
                    format_bytes(drive.free_bytes),
                    format_bytes(drive.total_bytes)
                )
            };
            let drive_label = if free_label.is_empty() {
                title.clone()
            } else {
                format!("{title}, {free_label}")
            };
            let ratio = drive.usage_ratio().clamp(0.0, 1.0);
            let bar_color = if ratio > 0.90 {
                theme.danger()
            } else {
                theme.accent_bright()
            };

            let icon_el = if let Some(render_img) = native_icon {
                img(ImageSource::Render(render_img))
                    .w(px(20.0))
                    .h(px(20.0))
                    .flex_shrink_0()
                    .into_any_element()
            } else {
                render_glyph(OnyxGlyph::Drive, px(18.0), theme.accent_bright()).into_any_element()
            };

            let mut top_row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .child(icon_el)
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_SM)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_primary())
                        .child(title),
                );

            if !drive.filesystem.is_empty() {
                top_row = top_row.child(
                    div()
                        .flex()
                        .items_center()
                        .h(px(16.0))
                        .px(px(4.0))
                        .rounded(px(3.0))
                        .bg(theme.bg_badge())
                        .text_size(px(11.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_muted())
                        .child(drive.filesystem.clone()),
                );
            }

            let mut card = div()
                .id(("sidebar-drive", idx))
                .role(Role::Button)
                .aria_label(drive_label)
                .aria_selected(is_active)
                .flex()
                .flex_col()
                .p(px(8.0))
                .gap(px(5.0))
                .rounded(RADIUS_MD)
                .border_1()
                .border_color(if drive_drop_hover {
                    theme.accent_bright()
                } else if is_active {
                    theme.accent_border()
                } else {
                    theme.border_subtle()
                })
                .bg(if drive_drop_hover {
                    theme.accent_subtle()
                } else if is_active {
                    theme.bg_selected()
                } else {
                    theme.bg_surface_alt()
                })
                .cursor_pointer()
                .hover(move |s| {
                    if !is_active {
                        s.bg(theme.bg_hover()).border_color(theme.border_strong())
                    } else {
                        s
                    }
                })
                .on_click(cx.listener(move |explorer, _, _, cx| {
                    explorer.navigate_to(target.clone(), cx);
                }))
                .on_mouse_move(cx.listener(move |explorer, _, _, cx| {
                    explorer.set_drop_hover(
                        crate::components::drag_drop::DropTarget::Directory(
                            drive_hover_target.clone(),
                        ),
                        cx,
                    );
                }))
                .drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style
                        .bg(theme.accent_subtle())
                        .border_color(theme.accent_bright())
                })
                .on_drop(
                    cx.listener(move |explorer, paths: &ExternalPaths, _window, cx| {
                        cx.stop_propagation();
                        explorer.handle_external_paths_drop_to(
                            paths,
                            drive_external_drop.clone(),
                            cx,
                        );
                    }),
                )
                .child(top_row)
                // Usage progress bar
                .child(
                    div()
                        .id(("sidebar-drive-usage", idx))
                        .role(Role::ProgressIndicator)
                        .aria_label("Drive usage")
                        .aria_numeric_value(f64::from(ratio) * 100.0)
                        .aria_min_numeric_value(0.0)
                        .aria_max_numeric_value(100.0)
                        .w_full()
                        .h(px(4.0))
                        .rounded(px(2.0))
                        .bg(theme.border_subtle())
                        .overflow_hidden()
                        .child(
                            div()
                                .h_full()
                                .w(gpui_kit::relative(ratio))
                                .rounded(px(2.0))
                                .bg(bar_color),
                        ),
                );

            if !free_label.is_empty() {
                card = card.child(
                    div()
                        .text_size(FONT_XS)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_secondary())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child(free_label),
                );
            }

            drives_col = drives_col.child(card);
        }
    }

    Some(
        div()
            .id("onyx-sidebar-scroll")
            .role(Role::Navigation)
            .aria_label("Sidebar")
            .flex()
            .flex_col()
            .w(px(width))
            .h_full()
            .flex_shrink_0()
            .overflow_y_scroll()
            .bg(theme.bg_sidebar())
            .p(px(10.0))
            .gap(px(12.0))
            .child(home_row)
            .child(fav_col)
            .child(qa_col)
            .child(recycle_row)
            .child(drives_col),
    )
}

fn section_header(
    id: &'static str,
    label: &'static str,
    collapsed: bool,
    theme: OnyxTheme,
    on_toggle: impl Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(format!("{label} group"))
        .aria_expanded(!collapsed)
        .flex()
        .flex_row()
        .items_center()
        .h(px(24.0))
        .px(px(6.0))
        .gap(px(5.0))
        .rounded(RADIUS_SM)
        .cursor_pointer()
        .hover(move |s| s.bg(theme.bg_hover()))
        .on_click(on_toggle)
        .child(render_glyph(
            if collapsed {
                OnyxGlyph::ChevronRight
            } else {
                OnyxGlyph::ChevronDown
            },
            px(12.0),
            theme.text_muted(),
        ))
        .child(
            div()
                .flex_1()
                .text_size(FONT_XS)
                .font_weight(FontWeight::BOLD)
                .text_color(theme.text_muted())
                .child(label),
        )
}

#[allow(clippy::too_many_arguments)]
fn sidebar_nav_row<
    T: Fn(&gpui_kit::ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static,
>(
    id: gpui_kit::ElementId,
    label: &str,
    fallback_glyph: OnyxGlyph,
    native_icon: Option<Arc<RenderImage>>,
    active: bool,
    removable: bool,
    theme: OnyxTheme,
    on_click: T,
    on_remove: Option<ClickHandler>,
    drop_hover: bool,
    on_hover: Option<MouseMoveHandler>,
    external_drop_dir: Option<PathBuf>,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement + use<T> {
    let icon_el = if let Some(render_img) = native_icon {
        img(ImageSource::Render(render_img))
            .w(px(20.0))
            .h(px(20.0))
            .flex_shrink_0()
            .into_any_element()
    } else {
        render_glyph(
            fallback_glyph,
            px(18.0),
            if active {
                theme.accent_bright()
            } else {
                theme.text_secondary()
            },
        )
        .into_any_element()
    };

    let mut label_el = div()
        .flex_1()
        .min_w(px(0.0))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_size(FONT_SM)
        .text_color(if active {
            theme.text_primary()
        } else {
            theme.text_secondary()
        });
    if active {
        label_el = label_el.font_weight(FontWeight::SEMIBOLD);
    }

    let mut row = div()
        .id(id)
        .role(Role::Button)
        .aria_label(label.to_string())
        .aria_selected(active)
        .relative()
        .flex()
        .flex_row()
        .items_center()
        .h(px(32.0))
        .pl(px(10.0))
        .pr(px(6.0))
        .gap(px(9.0))
        .rounded(RADIUS_SM)
        .border_1()
        .border_color(if drop_hover {
            theme.accent_bright()
        } else if active {
            theme.accent_border()
        } else {
            gpui_kit::transparent_black()
        })
        .bg(if drop_hover {
            theme.accent_subtle()
        } else if active {
            theme.bg_selected()
        } else {
            gpui_kit::transparent_black()
        })
        .cursor_pointer()
        .hover(move |s| if !active { s.bg(theme.bg_hover()) } else { s })
        .on_click(on_click);

    if let Some(hover_cb) = on_hover {
        row = row.on_mouse_move(move |ev, window, cx| hover_cb(ev, window, cx));
    }
    if let Some(drop_dir) = external_drop_dir {
        row = row
            .drag_over::<ExternalPaths>(move |style, _, _, _| {
                style
                    .bg(theme.accent_subtle())
                    .border_color(theme.accent_bright())
            })
            .on_drop(
                cx.listener(move |explorer, paths: &ExternalPaths, _window, cx| {
                    cx.stop_propagation();
                    explorer.handle_external_paths_drop_to(paths, drop_dir.clone(), cx);
                }),
            );
    }

    if active {
        row = row.child(
            div()
                .absolute()
                .left(px(2.0))
                .top(px(7.0))
                .w(px(3.0))
                .h(px(16.0))
                .rounded(px(1.5))
                .bg(theme.accent_bright()),
        );
    }

    row = row.child(icon_el).child(label_el.child(label.to_string()));

    if removable && let Some(remove_cb) = on_remove {
        row = row.child(
            div()
                .id(format!("unpin-fav-{label}"))
                .role(Role::Button)
                .aria_label(format!("Unpin {label}"))
                .flex()
                .items_center()
                .justify_center()
                .w(px(20.0))
                .h(px(20.0))
                .rounded(RADIUS_SM)
                .opacity(0.45)
                .cursor_pointer()
                .hover(move |s| s.bg(theme.bg_pressed()).opacity(1.0))
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .on_click(move |ev, window, cx| {
                    cx.stop_propagation();
                    remove_cb(ev, window, cx);
                })
                .child(render_glyph(OnyxGlyph::Close, px(12.0), theme.text_muted())),
        );
    }

    row
}
