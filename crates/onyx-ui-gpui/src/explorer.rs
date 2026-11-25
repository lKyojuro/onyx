//! Primary Onyx GPUI explorer view wired directly to `onyx-fs` and `onyx-core`.

use crate::led_pill::render_led_status_pill;
use crate::theme;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, MouseButton, ParentElement, Render, Styled,
    UniformListScrollHandle, Window, div, px, uniform_list,
};
use onyx_core::{
    FileEntry, FileKind, GenerationId, NavHistory, build_breadcrumbs, format_bytes,
    format_timestamp, normalize_display_path,
};
use onyx_fs::{EnumerationOptions, FsEngine};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

/// Quick-access sidebar bookmark.
#[derive(Debug, Clone)]
struct QuickLink {
    label: String,
    badge: &'static str,
    path: PathBuf,
}

/// Main GPUI view for the Onyx file explorer prototype.
#[derive(Debug)]
pub struct OnyxExplorer {
    runtime: Arc<Runtime>,
    current_path: PathBuf,
    history: NavHistory,
    entries: Vec<FileEntry>,
    selected_index: Option<usize>,
    generation: GenerationId,
    cancel_token: CancellationToken,
    is_loading: bool,
    status_label: String,
    error_message: Option<String>,
    anim_frame: usize,
    quick_links: Vec<QuickLink>,
    scroll_handle: UniformListScrollHandle,
}

impl OnyxExplorer {
    /// Creates the explorer state and kicks off the initial async directory load.
    pub fn new(runtime: Arc<Runtime>, initial_path: PathBuf, cx: &mut Context<Self>) -> Self {
        let quick_links = build_quick_links(&initial_path);
        let mut explorer = Self {
            runtime,
            current_path: initial_path.clone(),
            history: NavHistory::with_initial(initial_path.clone()),
            entries: Vec::new(),
            selected_index: None,
            generation: GenerationId::new(0),
            cancel_token: CancellationToken::new(),
            is_loading: false,
            status_label: "Ready".to_string(),
            error_message: None,
            anim_frame: 0,
            quick_links,
            scroll_handle: UniformListScrollHandle::new(),
        };
        explorer.load_directory(initial_path, false, cx);
        explorer
    }

    fn navigate_to(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        if path == self.current_path && !self.is_loading {
            return;
        }
        self.load_directory(path, true, cx);
    }

    fn go_back(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.history.go_back().cloned() {
            self.load_directory(target, false, cx);
        }
    }

    fn go_forward(&mut self, cx: &mut Context<Self>) {
        if let Some(target) = self.history.go_forward().cloned() {
            self.load_directory(target, false, cx);
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        if let Some(parent) = self.current_path.parent()
            && !parent.as_os_str().is_empty()
        {
            self.load_directory(parent.to_path_buf(), true, cx);
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        let path = self.current_path.clone();
        self.load_directory(path, false, cx);
    }

    fn load_directory(&mut self, path: PathBuf, record_history: bool, cx: &mut Context<Self>) {
        self.cancel_token.cancel();
        let cancel = CancellationToken::new();
        self.cancel_token = cancel.clone();

        self.generation = self.generation.next();
        let generation = self.generation;

        self.current_path = path.clone();
        if record_history {
            self.history.push(path.clone());
        }
        self.selected_index = None;
        self.error_message = None;
        self.is_loading = true;
        self.status_label = "Scanning".to_string();
        cx.notify();

        // Drive LED dot-matrix animation only while loading (zero idle timer wakeups).
        self.spawn_led_animation(generation, cx);

        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = tokio::sync::oneshot::channel();
        runtime.spawn(async move {
            let result = FsEngine::new()
                .enumerate_dir(path, generation, EnumerationOptions::default(), cancel)
                .await;
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |view, cx| {
                if view.generation != generation {
                    return;
                }
                view.is_loading = false;
                match result {
                    Ok(entries) => {
                        let count = entries.len();
                        view.entries = entries;
                        view.error_message = None;
                        view.status_label = format!("{count} items");
                    }
                    Err(err) => {
                        view.entries.clear();
                        view.error_message = Some(err.to_string());
                        view.status_label = "Error".to_string();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn spawn_led_animation(&self, generation: GenerationId, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(65))
                    .await;
                let keep_going = this
                    .update(cx, |view, cx| {
                        if view.generation != generation || !view.is_loading {
                            return false;
                        }
                        view.anim_frame = view.anim_frame.wrapping_add(1);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    break;
                }
            }
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_back = self.history.can_go_back();
        let can_forward = self.history.can_go_forward();
        let can_up = self
            .current_path
            .parent()
            .is_some_and(|p| !p.as_os_str().is_empty());

        let breadcrumbs = build_breadcrumbs(&self.current_path);
        let mut crumb_bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(4.0))
            .flex_1()
            .overflow_hidden()
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(6.0))
            .bg(theme::bg_obsidian())
            .border_1()
            .border_color(theme::border_subtle());

        for (idx, seg) in breadcrumbs.iter().enumerate() {
            if idx > 0 {
                crumb_bar = crumb_bar.child(
                    div()
                        .text_xs()
                        .text_color(theme::text_muted())
                        .px(px(2.0))
                        .child("/"),
                );
            }
            let seg_path = seg.path.clone();
            let is_last = idx + 1 == breadcrumbs.len();
            crumb_bar = crumb_bar.child(
                div()
                    .id(("crumb", idx))
                    .px(px(6.0))
                    .py(px(2.0))
                    .rounded(px(4.0))
                    .text_xs()
                    .text_color(if is_last {
                        theme::text_primary()
                    } else {
                        theme::text_secondary()
                    })
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::bg_hover()).text_color(theme::text_primary()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            view.navigate_to(seg_path.clone(), cx);
                        }),
                    )
                    .child(seg.label.clone()),
            );
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(12.0))
            .px(px(16.0))
            .py(px(10.0))
            .bg(theme::bg_surface())
            .border_b_1()
            .border_color(theme::border_subtle())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(nav_button(
                        "nav-back",
                        "←",
                        can_back,
                        cx.listener(|view, _, _, cx| view.go_back(cx)),
                    ))
                    .child(nav_button(
                        "nav-fwd",
                        "→",
                        can_forward,
                        cx.listener(|view, _, _, cx| view.go_forward(cx)),
                    ))
                    .child(nav_button(
                        "nav-up",
                        "↑",
                        can_up,
                        cx.listener(|view, _, _, cx| view.go_up(cx)),
                    ))
                    .child(nav_button(
                        "nav-refresh",
                        "↻",
                        true,
                        cx.listener(|view, _, _, cx| view.refresh(cx)),
                    )),
            )
            .child(crumb_bar)
            .child(render_led_status_pill(
                &self.status_label,
                self.anim_frame,
                self.is_loading,
            ))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut links_col = div().flex().flex_col().gap(px(2.0)).px(px(8.0));

        for (idx, link) in self.quick_links.iter().enumerate() {
            let is_active = link.path == self.current_path;
            let target = link.path.clone();
            links_col = links_col.child(
                div()
                    .id(("quick-link", idx))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(10.0))
                    .py(px(7.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .bg(if is_active {
                        theme::bg_selected()
                    } else {
                        theme::bg_surface()
                    })
                    .hover(|s| s.bg(theme::bg_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |view, _, _, cx| {
                            view.navigate_to(target.clone(), cx);
                        }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_active {
                                theme::folder_amber()
                            } else {
                                theme::text_muted()
                            })
                            .child(link.badge),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(if is_active {
                                theme::text_primary()
                            } else {
                                theme::text_secondary()
                            })
                            .child(link.label.clone()),
                    ),
            );
        }

        // Subtle decorative dot-matrix grid strip in sidebar header.
        let mut dot_strip = div().flex().flex_row().gap(px(4.0));
        for _ in 0..8 {
            dot_strip = dot_strip.child(
                div()
                    .w(px(2.0))
                    .h(px(2.0))
                    .rounded_full()
                    .bg(theme::matrix_dot()),
            );
        }

        div()
            .flex()
            .flex_col()
            .w(px(208.0))
            .h_full()
            .bg(theme::bg_surface())
            .border_r_1()
            .border_color(theme::border_subtle())
            .py(px(12.0))
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .px(px(14.0))
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted())
                            .child("ONYX // GPUI"),
                    )
                    .child(dot_strip),
            )
            .child(links_col)
    }

    fn render_file_table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let column_header = div()
            .flex()
            .flex_row()
            .items_center()
            .w_full()
            .h(px(30.0))
            .px(px(16.0))
            .bg(theme::bg_elevated())
            .border_b_1()
            .border_color(theme::border_subtle())
            .text_xs()
            .text_color(theme::text_muted())
            .child(div().flex_1().child("NAME"))
            .child(div().w(px(150.0)).child("TYPE"))
            .child(div().w(px(95.0)).child("SIZE"))
            .child(div().w(px(145.0)).child("MODIFIED"));

        let body = if let Some(err) = &self.error_message {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .text_sm()
                .text_color(theme::status_error())
                .child(format!("Unable to open folder: {err}"))
                .into_any_element()
        } else if self.entries.is_empty() && !self.is_loading {
            div()
                .flex()
                .flex_1()
                .items_center()
                .justify_center()
                .p(px(24.0))
                .text_sm()
                .text_color(theme::text_muted())
                .child("This folder is empty")
                .into_any_element()
        } else {
            let count = self.entries.len();
            uniform_list(
                "onyx-file-list",
                count,
                cx.processor(|view, range: std::ops::Range<usize>, _window, cx| {
                    let mut rows = Vec::with_capacity(range.len());
                    for idx in range {
                        let Some(entry) = view.entries.get(idx) else {
                            continue;
                        };
                        let is_selected = view.selected_index == Some(idx);
                        let is_dir = entry.is_dir() || entry.kind == FileKind::ReparsePoint;
                        let entry_path = entry.path.clone();
                        let badge_text = if is_dir {
                            "DIR".to_string()
                        } else {
                            entry
                                .extension
                                .as_deref()
                                .unwrap_or("FILE")
                                .chars()
                                .take(4)
                                .collect::<String>()
                                .to_ascii_uppercase()
                        };
                        let size_text = if is_dir {
                            "—".to_string()
                        } else {
                            format_bytes(entry.size)
                        };
                        let modified_text = format_timestamp(entry.modified);

                        let row = div()
                            .id(("entry-row", idx))
                            .flex()
                            .flex_row()
                            .items_center()
                            .w_full()
                            .h(px(32.0))
                            .px(px(16.0))
                            .border_b_1()
                            .border_color(theme::border_subtle())
                            .bg(if is_selected {
                                theme::bg_selected()
                            } else {
                                theme::bg_obsidian()
                            })
                            .hover(|s| s.bg(theme::bg_hover()))
                            .cursor_pointer()
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(
                                    move |view, event: &gpui_kit::MouseDownEvent, _, cx| {
                                        view.selected_index = Some(idx);
                                        if is_dir && event.click_count >= 2 {
                                            view.navigate_to(entry_path.clone(), cx);
                                        } else {
                                            cx.notify();
                                        }
                                    },
                                ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(10.0))
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .w(px(36.0))
                                            .px(px(4.0))
                                            .py(px(1.5))
                                            .rounded(px(3.0))
                                            .bg(theme::bg_elevated())
                                            .border_1()
                                            .border_color(theme::border_subtle())
                                            .text_xs()
                                            .text_color(if is_dir {
                                                theme::folder_amber()
                                            } else {
                                                theme::text_muted()
                                            })
                                            .child(badge_text),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(if is_dir {
                                                theme::text_primary()
                                            } else {
                                                theme::text_secondary()
                                            })
                                            .overflow_hidden()
                                            .child(entry.name.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .w(px(150.0))
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .overflow_hidden()
                                    .child(entry.type_label.clone()),
                            )
                            .child(
                                div()
                                    .w(px(95.0))
                                    .text_xs()
                                    .text_color(theme::text_secondary())
                                    .child(size_text),
                            )
                            .child(
                                div()
                                    .w(px(145.0))
                                    .text_xs()
                                    .text_color(theme::text_muted())
                                    .child(modified_text),
                            );

                        rows.push(row);
                    }
                    rows
                }),
            )
            .track_scroll(&self.scroll_handle)
            .flex_1()
            .h_full()
            .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .flex_1()
            .h_full()
            .bg(theme::bg_obsidian())
            .child(column_header)
            .child(body)
    }

    fn render_footer(&self) -> impl IntoElement {
        let dir_count = self.entries.iter().filter(|e| e.is_dir()).count();
        let file_count = self.entries.len().saturating_sub(dir_count);
        let display_path = normalize_display_path(&self.current_path);

        let selection_hint = self
            .selected_index
            .and_then(|idx| self.entries.get(idx))
            .map(|entry| {
                if entry.is_dir() {
                    format!("Selected: {} (double-click to open)", entry.name)
                } else {
                    format!("Selected: {} ({})", entry.name, format_bytes(entry.size))
                }
            })
            .unwrap_or_else(|| format!("{dir_count} folders · {file_count} files"));

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(28.0))
            .px(px(16.0))
            .bg(theme::bg_surface())
            .border_t_1()
            .border_color(theme::border_subtle())
            .text_xs()
            .text_color(theme::text_muted())
            .child(selection_hint)
            .child(display_path)
    }
}

impl Render for OnyxExplorer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(theme::bg_obsidian())
            .text_color(theme::text_primary())
            .child(self.render_header(cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h(px(0.0))
                    .child(self.render_sidebar(cx))
                    .child(self.render_file_table(cx)),
            )
            .child(self.render_footer())
    }
}

fn nav_button(
    id: &'static str,
    symbol: &'static str,
    enabled: bool,
    on_click: impl Fn(&gpui_kit::MouseDownEvent, &mut Window, &mut gpui_kit::App) + 'static,
) -> impl IntoElement {
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .w(px(28.0))
        .h(px(28.0))
        .rounded(px(6.0))
        .bg(theme::bg_elevated())
        .border_1()
        .border_color(theme::border_subtle())
        .text_sm()
        .text_color(if enabled {
            theme::text_primary()
        } else {
            theme::text_muted()
        });

    if enabled {
        base.cursor_pointer()
            .hover(|s| {
                s.bg(theme::bg_hover())
                    .border_color(theme::border_control())
            })
            .on_mouse_down(MouseButton::Left, on_click)
            .child(symbol)
    } else {
        base.child(symbol)
    }
}

fn build_quick_links(initial_path: &Path) -> Vec<QuickLink> {
    let mut links = Vec::new();
    let onyx_root = PathBuf::from(r"D:\Onyx");
    if onyx_root.is_dir() {
        links.push(QuickLink {
            label: "Onyx".to_string(),
            badge: "◆",
            path: onyx_root,
        });
    } else if initial_path.is_dir() {
        links.push(QuickLink {
            label: "Workspace".to_string(),
            badge: "◆",
            path: initial_path.to_path_buf(),
        });
    }

    if let Some(home) = dirs::home_dir() {
        links.push(QuickLink {
            label: "Home".to_string(),
            badge: "⌂",
            path: home,
        });
    }
    if let Some(desktop) = dirs::desktop_dir() {
        links.push(QuickLink {
            label: "Desktop".to_string(),
            badge: "▣",
            path: desktop,
        });
    }
    if let Some(downloads) = dirs::download_dir() {
        links.push(QuickLink {
            label: "Downloads".to_string(),
            badge: "↓",
            path: downloads,
        });
    }
    if let Some(docs) = dirs::document_dir() {
        links.push(QuickLink {
            label: "Documents".to_string(),
            badge: "▤",
            path: docs,
        });
    }

    for drive in [r"C:\", r"D:\"] {
        let p = PathBuf::from(drive);
        if p.is_dir() {
            links.push(QuickLink {
                label: format!("Drive {drive}"),
                badge: "▪",
                path: p,
            });
        }
    }

    links
}
