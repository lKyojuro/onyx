//! Docked Preview Pane and Quick Look Modal matching `preview_pane.slint`.
//!
//! Renders Text/Code previews, BGRA `RenderImage` image previews, Directory summaries,
//! Binary/Document metadata with hex headers, Loading/Error states, and key-value details.

use crate::components::{FluentButtonProps, fluent_button, fluent_chip};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph, rgba_pixels_to_render_image};
use crate::theme::{FONT_MD, FONT_SM, FONT_XS, OnyxTheme, RADIUS_LG, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    AnyElement, Context, FontWeight, ImageSource, InteractiveElement, IntoElement, MouseButton,
    ObjectFit, ParentElement, RenderImage, StatefulInteractiveElement, Styled, StyledImage,
    deferred, div, img, px, relative,
};
use onyx_core::{FileEntry, format_bytes, format_timestamp, normalize_display_path};
use onyx_preview::PreviewContent;
use std::path::PathBuf;
use std::sync::Arc;

/// Active visual state of the Preview Pane / Quick Look modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PreviewKind {
    #[default]
    None,
    Loading,
    Image,
    Text,
    Directory,
    Binary,
    Error,
}

/// Preview payload prepared on a background worker so BGRA image conversion
/// never blocks the GPUI UI thread.
pub enum DecodedPreview {
    /// Text, Directory, Document, Binary, or Unavailable content.
    Content(PreviewContent),
    /// Image content already converted to BGRA `RenderImage` on a worker thread.
    Image {
        original_width: u32,
        original_height: u32,
        render_image: Option<Arc<RenderImage>>,
        format: String,
    },
}

impl std::fmt::Debug for DecodedPreview {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Content(content) => f.debug_tuple("Content").field(content).finish(),
            Self::Image {
                original_width,
                original_height,
                render_image,
                format,
            } => f
                .debug_struct("Image")
                .field("original_width", original_width)
                .field("original_height", original_height)
                .field("has_render_image", &render_image.is_some())
                .field("format", format)
                .finish(),
        }
    }
}

impl DecodedPreview {
    /// Converts a [`PreviewContent`] into a [`DecodedPreview`], performing RGBA->BGRA
    /// conversion eagerly on the calling (background) thread.
    #[must_use]
    pub fn from_content(content: PreviewContent) -> Self {
        match content {
            PreviewContent::Image {
                width,
                height,
                original_width,
                original_height,
                rgba,
                format,
            } => Self::Image {
                original_width,
                original_height,
                render_image: rgba_pixels_to_render_image(width, height, &rgba),
                format,
            },
            other => Self::Content(other),
        }
    }
}

/// Current preview state displayed by both the docked Preview Pane and Quick Look modal.
#[derive(Default)]
pub struct PreviewState {
    pub path: Option<PathBuf>,
    pub title: String,
    pub subtitle: String,
    pub kind: PreviewKind,
    pub language: String,
    pub truncated: bool,
    pub text: String,
    pub summary: String,
    pub error: String,
    pub image: Option<Arc<RenderImage>>,
    pub metadata: Vec<(String, String)>,
}

impl std::fmt::Debug for PreviewState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewState")
            .field("path", &self.path)
            .field("title", &self.title)
            .field("kind", &self.kind)
            .field("has_image", &self.image.is_some())
            .field("metadata_len", &self.metadata.len())
            .finish()
    }
}

impl PreviewState {
    /// Resets the preview state for a newly selected entry (or clears it when `None`).
    pub fn reset_for_entry(&mut self, entry: Option<&FileEntry>) {
        self.language.clear();
        self.truncated = false;
        self.text.clear();
        self.summary.clear();
        self.error.clear();
        self.image = None;
        self.metadata.clear();

        let Some(entry) = entry else {
            self.path = None;
            self.title.clear();
            self.subtitle.clear();
            self.kind = PreviewKind::None;
            return;
        };

        self.path = Some(entry.path.clone());
        self.title = entry.name.clone();
        self.subtitle = if entry.is_dir() {
            "Folder".to_string()
        } else {
            format_bytes(entry.size)
        };
        self.kind = PreviewKind::Loading;

        self.metadata.push(("Name".to_string(), entry.name.clone()));
        if !entry.type_label.is_empty() {
            self.metadata
                .push(("Type".to_string(), entry.type_label.clone()));
        }
        if !entry.is_dir() {
            self.metadata
                .push(("Size".to_string(), format_bytes(entry.size)));
        }
        let modified = format_timestamp(entry.modified);
        if !modified.is_empty() {
            self.metadata.push(("Modified".to_string(), modified));
        }
        self.metadata
            .push(("Path".to_string(), normalize_display_path(&entry.path)));
    }

    /// Applies decoded preview output from `PreviewEngine`.
    pub fn apply_decoded(&mut self, decoded: DecodedPreview) {
        match decoded {
            DecodedPreview::Image {
                original_width,
                original_height,
                render_image,
                format,
            } => {
                self.kind = PreviewKind::Image;
                self.image = render_image;
                self.language = format.clone();
                self.metadata.push(("Format".to_string(), format));
                self.metadata.push((
                    "Dimensions".to_string(),
                    format!("{original_width} × {original_height}"),
                ));
            }
            DecodedPreview::Content(content) => match content {
                PreviewContent::Text {
                    text,
                    language,
                    line_count,
                    truncated,
                } => {
                    self.kind = PreviewKind::Text;
                    self.text = text;
                    self.language = language.clone();
                    self.truncated = truncated;
                    self.metadata.push(("Language".to_string(), language));
                    self.metadata
                        .push(("Lines".to_string(), line_count.to_string()));
                }
                PreviewContent::Image {
                    width,
                    height,
                    original_width,
                    original_height,
                    rgba,
                    format,
                } => {
                    self.kind = PreviewKind::Image;
                    self.image = rgba_pixels_to_render_image(width, height, &rgba);
                    self.language = format.clone();
                    self.metadata.push(("Format".to_string(), format));
                    self.metadata.push((
                        "Dimensions".to_string(),
                        format!("{original_width} × {original_height}"),
                    ));
                }
                PreviewContent::DocumentMetadata {
                    kind,
                    summary_fields,
                } => {
                    self.kind = PreviewKind::Binary;
                    self.summary = kind.clone();
                    self.metadata.push(("Kind".to_string(), kind));
                    for (k, v) in summary_fields {
                        self.metadata.push((k, v));
                    }
                }
                PreviewContent::DirectorySummary {
                    item_count,
                    dir_count,
                    file_count,
                    immediate_size_bytes,
                    truncated,
                } => {
                    let suffix = if truncated { "+" } else { "" };
                    self.kind = PreviewKind::Directory;
                    self.summary = format!(
                        "{item_count}{suffix} items ({dir_count} folders, {file_count} files)"
                    );
                    self.metadata
                        .push(("Items".to_string(), format!("{item_count}{suffix}")));
                    self.metadata
                        .push(("Folders".to_string(), dir_count.to_string()));
                    self.metadata
                        .push(("Files".to_string(), file_count.to_string()));
                    self.metadata.push((
                        "Immediate Size".to_string(),
                        format_bytes(immediate_size_bytes),
                    ));
                }
                PreviewContent::BinaryMetadata {
                    mime_guess,
                    size_bytes,
                    hex_header,
                } => {
                    self.kind = PreviewKind::Binary;
                    self.summary = format!("{mime_guess} ({})", format_bytes(size_bytes));
                    self.text = hex_header;
                    self.metadata.push(("MIME".to_string(), mime_guess));
                    self.metadata
                        .push(("Size".to_string(), format_bytes(size_bytes)));
                }
                PreviewContent::Unavailable { reason } => {
                    self.kind = PreviewKind::Error;
                    self.error = reason;
                }
            },
        }
    }

    /// Transitions the preview into the Error state.
    pub fn apply_error(&mut self, error: String) {
        self.kind = PreviewKind::Error;
        self.error = error;
        self.image = None;
    }
}

/// Renders the docked Preview Pane when enabled at the given logical width.
pub fn render_preview_pane(
    view: &OnyxExplorer,
    width: f32,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    if !view.preview_visible {
        return None;
    }
    let theme = view.theme;

    Some(
        div()
            .flex()
            .flex_col()
            .w(px(width))
            .flex_shrink_0()
            .h_full()
            .bg(theme.bg_chrome())
            .overflow_hidden()
            .child(render_preview_surface(&view.preview, false, theme, cx)),
    )
}

/// Renders the centered Quick Look modal overlay (`Space` shortcut) when open.
pub fn render_quick_look_modal(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    if !view.quick_look_open {
        return None;
    }
    let theme = view.theme;

    let modal_card = div()
        .id("quick-look-card")
        .relative()
        .flex()
        .flex_col()
        .w(px(760.0))
        .max_w(relative(0.88))
        .h(px(580.0))
        .max_h(relative(0.84))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(theme.border_strong())
        .bg(theme.bg_elevated())
        .overflow_hidden()
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        // Subtle specular top edge matching `preview_pane.slint`
        .child(
            div()
                .absolute()
                .top(px(1.0))
                .left(px(10.0))
                .right(px(10.0))
                .h(px(1.0))
                .bg(theme.border_specular()),
        )
        .child(render_preview_surface(&view.preview, true, theme, cx))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_center()
                .w_full()
                .h(px(26.0))
                .flex_shrink_0()
                .px(px(12.0))
                .bg(theme.bg_chrome())
                .border_t_1()
                .border_color(theme.border_subtle())
                .text_size(px(11.0))
                .text_color(theme.text_muted())
                .child("Arrows: Navigate   •   Enter: Open   •   Space / Esc: Close"),
        );

    Some(
        deferred(
            div()
                .id("quick-look-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|explorer, _, _, cx| {
                        explorer.close_quick_look(cx);
                    }),
                )
                .child(modal_card),
        )
        .with_priority(30),
    )
}

fn render_preview_surface(
    preview: &PreviewState,
    is_quick_look: bool,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let title = if !preview.title.is_empty() {
        preview.title.clone()
    } else if is_quick_look {
        "Quick Look".to_string()
    } else {
        "Preview".to_string()
    };

    let mut header_left = div()
        .flex()
        .flex_col()
        .justify_center()
        .flex_1()
        .min_w(px(0.0))
        .gap(px(2.0))
        .child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_MD)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_primary())
                .child(title),
        );

    if !preview.subtitle.is_empty() {
        header_left = header_left.child(
            div()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(preview.subtitle.clone()),
        );
    }

    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .w_full()
        .h(px(48.0))
        .flex_shrink_0()
        .pl(px(14.0))
        .pr(px(10.0))
        .gap(px(8.0))
        .bg(theme.bg_surface_alt())
        .border_b_1()
        .border_color(theme.border_subtle())
        .child(header_left);

    if !preview.language.is_empty() {
        header = header.child(fluent_chip(
            if is_quick_look {
                "ql-lang-chip".into()
            } else {
                "preview-lang-chip".into()
            },
            &preview.language,
            true,
            false,
            theme,
            None,
        ));
    }

    if is_quick_look && preview.kind != PreviewKind::None {
        header = header.child(fluent_button(
            FluentButtonProps {
                id: "ql-open-btn".into(),
                glyph: None,
                label: Some("Open"),
                active: true,
                enabled: true,
                compact: true,
                min_width: px(52.0),
            },
            theme,
            cx.listener(|explorer, _, _, cx| {
                explorer.open_selected_from_quick_look(cx);
            }),
        ));
    }

    header = header.child(fluent_button(
        FluentButtonProps {
            id: if is_quick_look {
                "ql-close-btn".into()
            } else {
                "preview-close-btn".into()
            },
            glyph: Some(OnyxGlyph::Close),
            label: None,
            active: false,
            enabled: true,
            compact: true,
            min_width: px(28.0),
        },
        theme,
        cx.listener(move |explorer, _, _, cx| {
            if is_quick_look {
                explorer.close_quick_look(cx);
            } else {
                explorer.close_preview_pane(cx);
            }
        }),
    ));

    let body: AnyElement = match preview.kind {
        PreviewKind::None => div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(20.0))
            .gap(px(8.0))
            .child(render_glyph(
                OnyxGlyph::PanelRight,
                px(30.0),
                theme.text_muted(),
            ))
            .child(
                div()
                    .text_size(FONT_MD)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.text_secondary())
                    .text_center()
                    .child("No preview available"),
            )
            .child(
                div()
                    .text_size(FONT_XS)
                    .text_color(theme.text_muted())
                    .text_center()
                    .child("Select a file or press Space for Quick Look."),
            )
            .into_any_element(),

        PreviewKind::Loading => div()
            .flex()
            .flex_col()
            .flex_1()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .child(render_glyph(
                OnyxGlyph::Refresh,
                px(24.0),
                theme.accent_bright(),
            ))
            .child(
                div()
                    .text_size(FONT_SM)
                    .text_color(theme.text_secondary())
                    .child("Loading preview..."),
            )
            .into_any_element(),

        PreviewKind::Error => div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p(px(16.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .max_w(px(420.0))
                    .p(px(12.0))
                    .gap(px(4.0))
                    .rounded(RADIUS_MD)
                    .border_1()
                    .border_color(theme.danger())
                    .bg(theme.danger_bg())
                    .child(
                        div()
                            .text_size(FONT_SM)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.danger())
                            .child("Preview failed"),
                    )
                    .child(
                        div()
                            .text_size(FONT_XS)
                            .text_color(theme.text_primary())
                            .child(preview.error.clone()),
                    ),
            )
            .into_any_element(),

        PreviewKind::Image => {
            let content = if let Some(render_img) = preview.image.clone() {
                img(ImageSource::Render(render_img))
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element()
            } else {
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .size_full()
                    .text_size(FONT_SM)
                    .text_color(theme.text_muted())
                    .child("Unable to render image")
                    .into_any_element()
            };

            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .p(px(12.0))
                .child(
                    div()
                        .flex()
                        .flex_1()
                        .min_h(px(0.0))
                        .items_center()
                        .justify_center()
                        .p(px(8.0))
                        .rounded(RADIUS_MD)
                        .border_1()
                        .border_color(theme.border_subtle())
                        .bg(theme.bg_code())
                        .overflow_hidden()
                        .child(content),
                )
                .into_any_element()
        }

        PreviewKind::Text => {
            let mut container = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .p(px(12.0))
                .gap(px(8.0));

            if preview.truncated {
                container = container.child(
                    div()
                        .flex()
                        .items_center()
                        .w_full()
                        .h(px(28.0))
                        .flex_shrink_0()
                        .px(px(10.0))
                        .rounded(RADIUS_SM)
                        .bg(theme.warning_bg())
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_XS)
                        .text_color(theme.warning_text())
                        .child(
                            "Preview truncated for performance — open file to view full contents.",
                        ),
                );
            }

            let scroll_id = if is_quick_look {
                "ql-text-scroll"
            } else {
                "preview-text-scroll"
            };

            container
                .child(
                    div()
                        .id(scroll_id)
                        .flex_1()
                        .min_h(px(0.0))
                        .w_full()
                        .p(px(12.0))
                        .rounded(RADIUS_MD)
                        .border_1()
                        .border_color(theme.border_subtle())
                        .bg(theme.bg_code())
                        .overflow_y_scroll()
                        .font_family("Consolas")
                        .text_size(FONT_XS)
                        .text_color(theme.text_primary())
                        .child(preview.text.clone()),
                )
                .into_any_element()
        }

        PreviewKind::Directory | PreviewKind::Binary => {
            let is_dir = preview.kind == PreviewKind::Directory;
            let mut box_el = div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .items_center()
                .justify_center()
                .p(px(18.0))
                .gap(px(10.0))
                .child(render_glyph(
                    if is_dir {
                        OnyxGlyph::Folder
                    } else {
                        OnyxGlyph::File
                    },
                    px(48.0),
                    if is_dir {
                        theme.folder_tint()
                    } else {
                        theme.file_tint()
                    },
                ))
                .child(
                    div()
                        .max_w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_MD)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_primary())
                        .child(preview.title.clone()),
                );

            if !preview.summary.is_empty() {
                box_el = box_el.child(
                    div()
                        .max_w_full()
                        .text_size(FONT_SM)
                        .text_color(theme.text_secondary())
                        .text_center()
                        .child(preview.summary.clone()),
                );
            }

            if !is_dir && !preview.text.is_empty() {
                let hex_scroll_id = if is_quick_look {
                    "ql-hex-scroll"
                } else {
                    "preview-hex-scroll"
                };
                box_el = box_el.child(
                    div()
                        .id(hex_scroll_id)
                        .w_full()
                        .max_h(px(160.0))
                        .p(px(10.0))
                        .rounded(RADIUS_MD)
                        .border_1()
                        .border_color(theme.border_subtle())
                        .bg(theme.bg_code())
                        .overflow_y_scroll()
                        .font_family("Consolas")
                        .text_size(px(11.0))
                        .text_color(theme.text_secondary())
                        .child(preview.text.clone()),
                );
            }

            box_el.into_any_element()
        }
    };

    let mut root = div()
        .flex()
        .flex_col()
        .flex_1()
        .min_h(px(0.0))
        .w_full()
        .child(header)
        .child(
            div()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.0))
                .w_full()
                .overflow_hidden()
                .child(body),
        );

    if !preview.metadata.is_empty() {
        let meta_scroll_id = if is_quick_look {
            "ql-meta-scroll"
        } else {
            "preview-meta-scroll"
        };
        let mut meta_list = div()
            .id(meta_scroll_id)
            .flex()
            .flex_col()
            .w_full()
            .max_h(if is_quick_look { px(180.0) } else { px(220.0) })
            .flex_shrink_0()
            .p(px(12.0))
            .gap(px(6.0))
            .bg(theme.bg_surface_alt())
            .border_t_1()
            .border_color(theme.border_subtle())
            .overflow_y_scroll()
            .child(
                div()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.text_muted())
                    .child("DETAILS"),
            );

        for (key, value) in &preview.metadata {
            meta_list = meta_list.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .w_full()
                    .gap(px(8.0))
                    .child(
                        div()
                            .w(px(92.0))
                            .flex_shrink_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(FONT_XS)
                            .text_color(theme.text_muted())
                            .child(key.clone()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.0))
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_size(FONT_XS)
                            .text_color(theme.text_primary())
                            .child(value.clone()),
                    ),
            );
        }

        root = root.child(meta_list);
    }

    root
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_core::{FileAttributes, FileKind};

    fn sample_file_entry(name: &str, kind: FileKind, size: u64) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: PathBuf::from(format!("C:\\temp\\{name}")),
            kind,
            size,
            modified: Some(1_700_000_000),
            created: None,
            attributes: FileAttributes::default(),
            is_symlink: false,
            symlink_target: None,
            extension: None,
            type_label: if matches!(kind, FileKind::Directory) {
                "Folder".to_string()
            } else {
                "File".to_string()
            },
        }
    }

    #[test]
    fn preview_state_transitions_cover_all_variants() {
        let mut state = PreviewState::default();
        assert_eq!(state.kind, PreviewKind::None);

        let entry = sample_file_entry("main.rs", FileKind::File, 128);
        state.reset_for_entry(Some(&entry));
        assert_eq!(state.kind, PreviewKind::Loading);
        assert_eq!(state.title, "main.rs");

        state.apply_decoded(DecodedPreview::from_content(PreviewContent::Text {
            text: "fn main() {}".to_string(),
            language: "Rust".to_string(),
            line_count: 1,
            truncated: true,
        }));
        assert_eq!(state.kind, PreviewKind::Text);
        assert_eq!(state.language, "Rust");
        assert!(state.truncated);

        state.reset_for_entry(Some(&sample_file_entry("docs", FileKind::Directory, 0)));
        state.apply_decoded(DecodedPreview::from_content(
            PreviewContent::DirectorySummary {
                item_count: 10,
                dir_count: 2,
                file_count: 8,
                immediate_size_bytes: 4096,
                truncated: false,
            },
        ));
        assert_eq!(state.kind, PreviewKind::Directory);
        assert!(state.summary.contains("10 items"));

        state.reset_for_entry(Some(&sample_file_entry("img.png", FileKind::File, 16)));
        state.apply_decoded(DecodedPreview::from_content(PreviewContent::Image {
            width: 2,
            height: 2,
            original_width: 200,
            original_height: 100,
            rgba: vec![255; 16],
            format: "PNG".to_string(),
        }));
        assert_eq!(state.kind, PreviewKind::Image);
        assert!(state.image.is_some());

        state.apply_error("Access denied".to_string());
        assert_eq!(state.kind, PreviewKind::Error);
        assert_eq!(state.error, "Access denied");
        assert!(state.image.is_none());

        state.reset_for_entry(None);
        assert_eq!(state.kind, PreviewKind::None);
        assert!(state.path.is_none());
    }
}
