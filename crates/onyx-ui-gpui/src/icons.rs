//! Vector icons ported from `crates/onyx-ui/ui/components/icons.slint`
//! and asynchronous Windows Shell icon decoding into `RenderImage` (BGRA).

use gpui_kit::{Hsla, IntoElement, Pixels, RenderImage, Styled, svg};
use image::{Frame, RgbaImage};
use onyx_cache::{IconCache, RgbaImageData};
use onyx_core::{FileEntry, normalize_display_path};
use onyx_windows::RgbaIcon;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

/// Standard Onyx vector icon glyphs matching `icons.slint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OnyxGlyph {
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    SortAscending,
    SortDescending,
    ArrowUpRight,
    Back,
    Forward,
    Up,
    Refresh,
    Plus,
    Close,
    Search,
    Folder,
    File,
    Home,
    Star,
    StarFilled,
    Drive,
    List,
    Grid,
    Eye,
    PanelLeft,
    PanelRight,
    Columns,
    SwapHorizontal,
    Sun,
    Moon,
    Desktop,
    Download,
    Image,
    Music,
    Video,
    WindowMinimize,
    WindowMaximize,
    WindowRestore,
    FolderPlus,
    Cut,
    Copy,
    Paste,
    Rename,
    Duplicate,
    Trash,
    Undo,
    Terminal,
    Info,
    Warning,
    Settings,
    Command,
    Archive,
}

impl OnyxGlyph {
    /// Returns raw SVG bytes with `currentColor` stroke/fill on a `24x24` viewBox.
    #[must_use]
    pub const fn svg_bytes(self) -> &'static [u8] {
        match self {
            Self::ChevronLeft => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M15 5l-7 7 7 7"/></svg>"#,
            Self::ChevronRight => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 5l7 7-7 7"/></svg>"#,
            Self::ChevronDown => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 9l7 7 7-7"/></svg>"#,
            Self::SortAscending => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 15l7-7 7 7"/></svg>"#,
            Self::SortDescending => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 9l7 7 7-7"/></svg>"#,
            Self::ArrowUpRight => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M4 20L20 4M7 4h13v13"/></svg>"#,
            Self::Back => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M19 12H5M12 19l-7-7 7-7"/></svg>"#,
            Self::Forward => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M5 12h14M12 5l7 7-7 7"/></svg>"#,
            Self::Up => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5M5 12l7-7 7 7"/></svg>"#,
            Self::Refresh => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 0-2.6 6.4M21 5v6h-6"/></svg>"#,
            Self::Plus => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>"#,
            Self::Close => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M18 6L6 18M6 6l12 12"/></svg>"#,
            Self::Search => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="11" cy="11" r="7"/><path d="M20.5 20.5L16 16"/></svg>"#,
            Self::Folder => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" fill-opacity="0.22" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h6.5l2 2.5H21V19H3z"/></svg>"#,
            Self::File => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" fill-opacity="0.14" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3h8l5 5v13H6z"/><path d="M14 3v5h5"/></svg>"#,
            Self::Home => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 10.5L12 3l9 7.5M5 9.5V20h14V9.5M9.5 20v-6h5v6"/></svg>"#,
            Self::Star => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l2.9 5.8 6.3.9-4.6 4.5 1.1 6.3L12 17.5l-5.7 3 1.1-6.3L2.8 9.7l6.3-.9z"/></svg>"#,
            Self::StarFilled => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3l2.9 5.8 6.3.9-4.6 4.5 1.1 6.3L12 17.5l-5.7 3 1.1-6.3L2.8 9.7l6.3-.9z"/></svg>"#,
            Self::Drive => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="14" width="18" height="5"/><path d="M3 14l3-8h12l3 8M16.5 16.5h1"/></svg>"#,
            Self::List => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 6h16M4 12h16M4 18h16"/></svg>"#,
            Self::Grid => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="4" width="6" height="6"/><rect x="14" y="4" width="6" height="6"/><rect x="4" y="14" width="6" height="6"/><rect x="14" y="14" width="6" height="6"/></svg>"#,
            Self::Eye => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M2 12s4-7 10-7 10 7 10 7-4 7-10 7-10-7-10-7z"/><circle cx="12" cy="12" r="3"/></svg>"#,
            Self::PanelLeft => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16"/><path d="M9 4v16"/></svg>"#,
            Self::Columns => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="1"/><path d="M12 4v16"/></svg>"#,
            Self::SwapHorizontal => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M7 10l-4-4 4-4"/><path d="M3 6h14"/><path d="M17 14l4 4-4 4"/><path d="M21 18H7"/></svg>"#,
            Self::PanelRight => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16"/><path d="M15 4v16"/></svg>"#,
            Self::Sun => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="4"/><path d="M12 2v2.5M12 19.5V22M2 12h2.5M19.5 12H22M4.9 4.9l1.8 1.8M17.3 17.3l1.8 1.8M19.1 4.9l-1.8 1.8M6.7 17.3l-1.8 1.8"/></svg>"#,
            Self::Moon => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20 14.5A8.5 8.5 0 1 1 9.5 4 6.5 6.5 0 0 0 20 14.5z"/></svg>"#,
            Self::Desktop => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="12"/><path d="M8 20h8M12 16v4"/></svg>"#,
            Self::Download => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M4 17v3h16v-3"/></svg>"#,
            Self::Image => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16"/><path d="M4 17l5.5-5.5L14 16l2.5-2.5 4 4"/><circle cx="9.5" cy="8.5" r="0.8" fill="currentColor"/></svg>"#,
            Self::Music => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><path d="M9 18V5l11-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="17" cy="16" r="3"/></svg>"#,
            Self::Video => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="14"/><path d="M10 9l5 3-5 3z"/></svg>"#,
            Self::WindowMinimize => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linecap="round"><path d="M6 12h12"/></svg>"#,
            Self::WindowMaximize => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round"><rect x="6" y="6" width="12" height="12"/></svg>"#,
            Self::WindowRestore => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.75" stroke-linejoin="round"><rect x="5" y="8" width="10" height="10"/><path d="M9 8V5h10v10h-3"/></svg>"#,
            Self::FolderPlus => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h6.5l2 2.5H21V19H3z"/><path d="M12 11v5M9.5 13.5h5"/></svg>"#,
            Self::Cut => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M20 4L8.12 15.88M14.47 14.48L20 20M8.12 8.12L12 12"/></svg>"#,
            Self::Copy => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>"#,
            Self::Paste => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/><rect x="8" y="2" width="8" height="4" rx="1"/></svg>"#,
            Self::Rename => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/></svg>"#,
            Self::Duplicate => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="8" y="8" width="12" height="12" rx="2"/><path d="M4 16V6a2 2 0 0 1 2-2h10"/><path d="M14 11v6M11 14h6"/></svg>"#,
            Self::Trash => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 6h18"/><path d="M8 6V4h8v2"/><path d="M19 6l-1 14H6L5 6"/><path d="M10 11v6M14 11v6"/></svg>"#,
            Self::Undo => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 14L4 9l5-5"/><path d="M4 9h10.5a5.5 5.5 0 0 1 0 11H11"/></svg>"#,
            Self::Terminal => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M7 9l4 3-4 3M13 15h4"/></svg>"#,
            Self::Info => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 16v-4M12 8h.01"/></svg>"#,
            Self::Warning => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><path d="M12 9v4M12 17h.01"/></svg>"#,
            Self::Settings => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3.2"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06A1.7 1.7 0 0 0 15 19.4a1.7 1.7 0 0 0-1 1.55V21a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 8.9 19.4a1.7 1.7 0 0 0-1.87.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-1.55-1H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.6 8.9a1.7 1.7 0 0 0-.33-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-1.55V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 1 1.51 1.7 1.7 0 0 0 1.87-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 9v.09a1.7 1.7 0 0 0 1.55 1H21a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.51 1z"/></svg>"#,
            Self::Command => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M9 6.5a3.5 3.5 0 1 0-3.5 3.5H9V6.5zM9 10h6M15 6.5A3.5 3.5 0 1 1 18.5 10H15V6.5zM9 14v3.5A3.5 3.5 0 1 1 5.5 14H9zm6 0h3.5A3.5 3.5 0 1 1 15 17.5V14z"/></svg>"#,
            Self::Archive => br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="4" width="18" height="4.5" rx="1.2"/><path d="M4.5 8.5V19a1.5 1.5 0 0 0 1.5 1.5h12a1.5 1.5 0 0 0 1.5-1.5V8.5"/><path d="M10 12h4"/></svg>"#,
        }
    }
}

/// Renders an `OnyxGlyph` SVG element at `size` with `color`.
pub fn render_glyph(glyph: OnyxGlyph, size: Pixels, color: Hsla) -> impl IntoElement {
    svg()
        .data(glyph.svg_bytes())
        .flex_shrink_0()
        .w(size)
        .h(size)
        .text_color(color)
}

/// Converts straight RGBA8 pixel bytes into a GPUI `RenderImage` (BGRA pixel order).
#[must_use]
pub fn rgba_pixels_to_render_image(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Option<Arc<RenderImage>> {
    if width == 0 || height == 0 {
        return None;
    }
    let expected = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(4)?;
    if rgba.len() != expected {
        return None;
    }
    // GPUI's `RenderImage` stores frames in BGRA pixel order for DX11 / Metal upload.
    let mut bgra = rgba.to_vec();
    for px_chunk in bgra.chunks_exact_mut(4) {
        px_chunk.swap(0, 2);
    }
    let img_buf = RgbaImage::from_raw(width, height, bgra)?;
    let frame = Frame::new(img_buf);
    Some(Arc::new(RenderImage::new([frame])))
}

/// Converts a Windows Shell `RgbaIcon` (straight RGBA) into a GPUI `RenderImage` (BGRA).
#[must_use]
pub fn rgba_icon_to_render_image(icon: &RgbaIcon) -> Option<Arc<RenderImage>> {
    rgba_pixels_to_render_image(icon.width, icon.height, &icon.pixels)
}

/// Cache key prefix for the Windows stock Recycle Bin icons. The suffix is the
/// state (`:empty` or `:full`); there is no filesystem path to key on.
pub const RECYCLE_BIN_ICON_PREFIX: &str = "shell:recycle-bin";

/// Computes the cache key for a filesystem path (sidebar folders, drives, executables).
#[must_use]
pub fn path_icon_key(path: &Path) -> String {
    format!("path:{}", normalize_display_path(path))
}

/// Computes the cache key for a `FileEntry` (matches `onyx-app/src/ui_sync.rs`).
#[must_use]
pub fn entry_icon_key(entry: &FileEntry) -> String {
    if entry.is_dir() {
        return "directory".to_string();
    }
    let ext = entry
        .extension
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext == "exe" || ext == "lnk" || ext == "ico" {
        path_icon_key(&entry.path)
    } else {
        format!("extension:{ext}")
    }
}

/// Deduplicated Windows Shell icon store bridging `onyx-cache::IconCache` and GPUI `RenderImage`.
#[derive(Default)]
pub struct ShellIconStore {
    raw_cache: IconCache,
    gpui_images: HashMap<String, Arc<RenderImage>>,
    pending: HashSet<String>,
    missing: HashSet<String>,
}

impl std::fmt::Debug for ShellIconStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShellIconStore")
            .field("cached_images", &self.gpui_images.len())
            .field("pending", &self.pending.len())
            .finish()
    }
}

impl ShellIconStore {
    /// Returns a cached GPUI `RenderImage` if already decoded.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Arc<RenderImage>> {
        self.gpui_images.get(key)
    }

    /// Returns `true` if `key` needs background Windows Shell extraction.
    #[must_use]
    pub fn needs_fetch(&self, key: &str) -> bool {
        !self.gpui_images.contains_key(key)
            && !self.pending.contains(key)
            && !self.missing.contains(key)
    }

    /// Marks `key` as queued for background extraction.
    pub fn mark_pending(&mut self, key: String) {
        self.pending.insert(key);
    }

    /// Records a batch of decoded icons from a blocking worker.
    pub fn insert_batch(&mut self, batch: Vec<(String, Option<RgbaIcon>)>) {
        for (key, icon_opt) in batch {
            self.pending.remove(&key);
            if let Some(icon) = icon_opt {
                if let Some(render_img) = rgba_icon_to_render_image(&icon) {
                    self.gpui_images.insert(key.clone(), render_img);
                }
                self.raw_cache.put(
                    key,
                    RgbaImageData::new(icon.width, icon.height, icon.pixels),
                );
            } else if self.missing.len() < self.raw_cache.capacity() {
                self.missing.insert(key);
            }
        }
    }
}
