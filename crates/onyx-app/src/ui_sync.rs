use onyx_cache::RgbaImageData;
use onyx_config::AppConfig;
use onyx_core::{
    DriveInfo, FileEntry, KnownFolder, KnownFolderKind, PaneId, SortColumn, SortDirection,
    ThemeMode, ViewMode, build_breadcrumbs, format_bytes, format_timestamp, normalize_display_path,
};
use onyx_operations::ArchiveEntry;
use onyx_preview::{PreviewContent, ThumbnailImage, is_thumbnail_supported};
use onyx_ui::{
    OnyxTheme, OnyxWindow, UiArchiveEntry, UiBreadcrumb, UiDriveItem, UiFileEntry, UiMetadataRow,
    UiSidebarItem, UiTabItem,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{
    ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel,
};
use std::{collections::HashMap, path::Path};

use crate::content::THUMBNAIL_TARGET_PX;
use crate::state::{State, is_zip_path, pane_number};

pub(crate) fn model<T: Clone + 'static>(items: impl IntoIterator<Item = T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from_iter(items))
}

pub(crate) fn display_path(path: &Path) -> SharedString {
    normalize_display_path(path).into()
}

pub(crate) fn accent_color(hex: &str) -> Option<slint::Color> {
    let hex = hex.trim().trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let rgb = u32::from_str_radix(hex, 16).ok()?;
    Some(slint::Color::from_rgb_u8(
        (rgb >> 16) as u8,
        (rgb >> 8) as u8,
        rgb as u8,
    ))
}

pub(crate) fn window_hwnd(ui: &OnyxWindow) -> Option<isize> {
    let window_handle = ui.window().window_handle();
    match window_handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
        _ => None,
    }
}

pub(crate) fn system_fonts() -> [&'static str; 3] {
    fn preferred(preferred: &'static str, fallback: &'static str) -> &'static str {
        if onyx_windows::has_system_font_family(preferred) {
            preferred
        } else {
            fallback
        }
    }

    let ui = preferred("Bahnschrift", "Segoe UI");
    [ui, ui, preferred("Cascadia Mono", "Consolas")]
}

pub(crate) fn apply_window_theme(hwnd: isize, ui: &OnyxWindow) {
    if let Err(error) = onyx_windows::apply_window_backdrop(hwnd, ui.get_dark_mode(), true) {
        tracing::debug!(%error, "could not apply the Windows backdrop");
    }

    let theme = ui.global::<OnyxTheme>();
    let rgb =
        |color: slint::Color| u32::from_be_bytes([0, color.red(), color.green(), color.blue()]);
    if let Err(error) = onyx_windows::apply_titlebar_colors(
        hwnd,
        rgb(theme.get_bg_chrome()),
        rgb(theme.get_text_primary()),
        rgb(theme.get_border_subtle()),
    ) {
        tracing::debug!(%error, "could not apply the Windows title bar colors");
    }
}

pub(crate) fn known_folders(ui: &OnyxWindow, folders: Vec<KnownFolder>) {
    ui.set_known_folders(model(folders.into_iter().map(|folder| {
        UiSidebarItem {
            label: folder.name.into(),
            path: display_path(&folder.path),
            icon_glyph: match folder.kind {
                KnownFolderKind::Home => "home",
                KnownFolderKind::Desktop => "desktop",
                KnownFolderKind::Downloads => "download",
                KnownFolderKind::Documents => "file",
                KnownFolderKind::Pictures => "image",
                KnownFolderKind::Music => "music",
                KnownFolderKind::Videos => "video",
            }
            .into(),
            is_favorite: false,
        }
    })));
}

pub(crate) fn drives(ui: &OnyxWindow, drives: Vec<DriveInfo>) {
    ui.set_drives(model(drives.into_iter().map(|drive| UiDriveItem {
        label: format!("{} ({})", drive.label, normalize_display_path(&drive.root)).into(),
        root: display_path(&drive.root),
        filesystem: drive.filesystem.clone().into(),
        free_label: if drive.total_bytes == 0 {
            "".into()
        } else {
            format!(
                "{} free of {}",
                format_bytes(drive.free_bytes),
                format_bytes(drive.total_bytes)
            )
            .into()
        },
        usage_ratio: drive.usage_ratio(),
    })));
}

pub(crate) fn settings(ui: &OnyxWindow, config: &AppConfig) {
    let theme_mode = match config.theme {
        ThemeMode::Dark => 0,
        ThemeMode::Light => 1,
        ThemeMode::System => 2,
    };
    ui.set_settings_theme_mode(theme_mode);
    ui.set_settings_default_view(i32::from(config.default_view == ViewMode::Grid));
    ui.set_settings_show_hidden(config.show_hidden);
    ui.set_settings_show_system(config.show_system);
    ui.set_settings_show_preview(config.show_preview_pane);
    ui.set_settings_dual_pane_default(config.dual_pane_default);
    ui.set_settings_default_terminal(config.normalized_terminal().into());
    ui.set_settings_excluded_dirs(model(
        config.excluded_index_dirs.iter().map(|p| display_path(p)),
    ));
}

pub(crate) fn chrome(ui: &OnyxWindow, state: &State) {
    ui.set_tabs(model(state.tabs.tabs().iter().enumerate().map(
        |(index, tab)| UiTabItem {
            id: tab.id as i32,
            title: tab.title.as_str().into(),
            path: display_path(&tab.path),
            active: index == state.tabs.active_index(),
        },
    )));
    ui.set_favorites(model(state.config.favorites.iter().map(|item| {
        UiSidebarItem {
            label: item.name.as_str().into(),
            path: display_path(&item.path),
            icon_glyph: "star-filled".into(),
            is_favorite: true,
        }
    })));
    let path = state.path(state.active);
    let tab = state.tab(state.active);
    if !ui.get_address_editing() {
        ui.set_address_text(display_path(path));
    }
    ui.set_breadcrumbs(model(build_breadcrumbs(path).into_iter().map(|crumb| {
        UiBreadcrumb {
            label: crumb.label.into(),
            path: display_path(&crumb.path),
        }
    })));
    ui.set_home_path(display_path(&state.home));
    ui.set_can_go_back(tab.is_some_and(|tab| tab.history.can_go_back()));
    ui.set_can_go_forward(tab.is_some_and(|tab| tab.history.can_go_forward()));
    ui.set_is_current_favorite(
        state
            .config
            .favorites
            .iter()
            .any(|item| same_path(&item.path, path)),
    );
    ui.set_active_pane(pane_number(state.active) as i32);
    ui.set_dual_pane(state.dual);
    ui.set_preview_pane(state.config.show_preview_pane);
    ui.set_quick_look_open(state.quick_look);
    ui.set_view_mode(i32::from(state.config.default_view == ViewMode::Grid));
    ui.set_show_hidden(state.config.show_hidden);
    ui.set_indexed_search(state.indexed_search);
    settings(ui, &state.config);
}

pub(crate) fn same_path(first: &Path, second: &Path) -> bool {
    let normalize = |path: &Path| {
        normalize_display_path(path)
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    first == second || normalize(first) == normalize(second)
}

pub(crate) fn icon_key(entry: &FileEntry) -> String {
    if entry.is_dir() {
        return "directory".into();
    }
    let ext = entry
        .extension
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ext == "exe" || ext == "lnk" || ext == "ico" {
        format!("path:{}", normalize_display_path(&entry.path))
    } else {
        format!("extension:{ext}")
    }
}

pub(crate) fn rgba_image(data: &RgbaImageData) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &data.pixels,
        data.width,
        data.height,
    ))
}

pub(crate) fn thumbnail_image(img: &ThumbnailImage) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &img.rgba, img.width, img.height,
    ))
}

pub(crate) fn rows(state: &State, id: PaneId) {
    let pane = state.pane(id);
    pane.model
        .set_vec(file_rows(state, id, &pane.entries).collect::<Vec<_>>());
}

pub(crate) fn file_rows<'a>(
    state: &'a State,
    id: PaneId,
    entries: &'a [FileEntry],
) -> impl Iterator<Item = UiFileEntry> + 'a {
    let selected = state.tab(id).and_then(|tab| tab.selected_paths.first());
    let grid_mode = state.config.default_view == ViewMode::Grid;
    let mut images = HashMap::new();
    entries.iter().map(move |entry| {
        let key = icon_key(entry);
        let icon = images
            .entry(key.clone())
            .or_insert_with(|| state.icons.get(&key).as_ref().map(rgba_image));
        let cached_thumb = if grid_mode
            && !entry.is_dir()
            && is_thumbnail_supported(&entry.path)
            && let Some(modified_secs) = entry.modified
        {
            state
                .thumbnails
                .get_cached(&entry.path, modified_secs, entry.size, THUMBNAIL_TARGET_PX)
                .map(|img| thumbnail_image(&img))
        } else {
            None
        };
        UiFileEntry {
            path: display_path(&entry.path),
            name: entry.name.as_str().into(),
            modified: format_timestamp(entry.modified).into(),
            type_label: entry.type_label.as_str().into(),
            size_label: if entry.is_dir() {
                "".into()
            } else {
                format_bytes(entry.size).into()
            },
            is_dir: entry.is_dir(),
            is_hidden: entry.attributes.hidden,
            is_symlink: entry.is_symlink,
            selected: selected == Some(&entry.path),
            has_icon: icon.is_some(),
            icon: icon.clone().unwrap_or_default(),
            has_thumbnail: cached_thumb.is_some(),
            thumbnail: cached_thumb.unwrap_or_default(),
        }
    })
}

pub(crate) fn selection_row(state: &State, id: PaneId, index: usize, selected: bool) {
    let model = &state.pane(id).model;
    if let Some(mut row) = model.row_data(index) {
        row.selected = selected;
        model.set_row_data(index, row);
    }
}

pub(crate) fn selected_index(state: &State, id: PaneId) -> Option<usize> {
    let path = state.tab(id)?.selected_paths.first()?;
    state
        .pane(id)
        .entries
        .iter()
        .position(|entry| &entry.path == path)
}

pub(crate) fn pane(ui: &OnyxWindow, state: &State, id: PaneId) {
    let pane = state.pane(id);
    let path = display_path(state.path(id));
    let sort = state.tab(id).map(|tab| tab.sort).unwrap_or_default();
    let column = match sort.column {
        SortColumn::Name => 0,
        SortColumn::Modified => 1,
        SortColumn::Kind => 2,
        SortColumn::Size => 3,
    };
    let index = selected_index(state, id)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(-1);
    let ascending = sort.direction == SortDirection::Ascending;
    match id {
        PaneId::Primary => {
            ui.set_primary_path(path);
            ui.set_primary_sort_column(column);
            ui.set_primary_sort_ascending(ascending);
            ui.set_primary_loading(pane.loading);
            ui.set_primary_error(pane.error.as_str().into());
            ui.set_primary_selected_index(index);
        }
        PaneId::Secondary => {
            ui.set_secondary_path(path);
            ui.set_secondary_sort_column(column);
            ui.set_secondary_sort_ascending(ascending);
            ui.set_secondary_loading(pane.loading);
            ui.set_secondary_error(pane.error.as_str().into());
            ui.set_secondary_selected_index(index);
        }
    }
}

pub(crate) fn status(ui: &OnyxWindow, state: &State) {
    let pane = state.pane(state.active);
    let count = pane.entries.len();
    ui.set_status_items_text(if count == 1 {
        "1 item".into()
    } else {
        format!("{count} items").into()
    });
    let selected = state.selected(state.active);
    ui.set_has_selection(selected.is_some());
    ui.set_has_archive_selection(selected.is_some_and(|e| !e.is_dir() && is_zip_path(&e.path)));
    ui.set_status_selection_text(selected.map_or_else(Default::default, |entry| {
        if entry.is_dir() {
            "1 selected".into()
        } else {
            format!("1 selected · {}", format_bytes(entry.size)).into()
        }
    }));
    ui.set_status_search_text(if pane.query.is_empty() {
        "".into()
    } else if pane.loading {
        "Searching…".into()
    } else {
        format!("{count} matches").into()
    });
}

pub(crate) fn archive_entries(entries: &[ArchiveEntry]) -> ModelRc<UiArchiveEntry> {
    model(entries.iter().map(|entry| {
        let rel = entry.relative_path.to_string_lossy().replace('\\', "/");
        UiArchiveEntry {
            name: entry.name.as_str().into(),
            relative_path: if entry.is_dir {
                format!("{rel}/").into()
            } else {
                rel.into()
            },
            size_label: if entry.is_dir {
                "".into()
            } else {
                format_bytes(entry.size).into()
            },
            compressed_label: if entry.is_dir {
                "".into()
            } else {
                format_bytes(entry.compressed_size).into()
            },
            is_dir: entry.is_dir,
            modified: format_timestamp(entry.modified.map(|dt| dt.timestamp())).into(),
        }
    }))
}

pub(crate) fn reset_preview(ui: &OnyxWindow, entry: Option<&FileEntry>) {
    ui.set_preview_kind(if entry.is_some() {
        "loading".into()
    } else {
        "none".into()
    });
    ui.set_preview_title(entry.map_or_else(Default::default, |entry| entry.name.as_str().into()));
    ui.set_preview_subtitle(entry.map_or_else(Default::default, |entry| display_path(&entry.path)));
    ui.set_preview_image(Image::default());
    ui.set_has_preview_image(false);
    ui.set_preview_text("".into());
    ui.set_preview_language("".into());
    ui.set_preview_truncated(false);
    ui.set_preview_summary("".into());
    ui.set_preview_metadata(ModelRc::default());
    ui.set_preview_error("".into());
}

pub(crate) fn preview(ui: &OnyxWindow, content: PreviewContent) {
    let kind = match content {
        PreviewContent::Text {
            text,
            language,
            line_count,
            truncated,
        } => {
            ui.set_preview_text(text.into());
            ui.set_preview_language(language.into());
            ui.set_preview_truncated(truncated);
            ui.set_preview_summary(format!("{line_count} lines").into());
            "text"
        }
        PreviewContent::Image {
            width,
            height,
            original_width,
            original_height,
            rgba,
            format,
        } => {
            ui.set_preview_image(rgba_image(&RgbaImageData::new(width, height, rgba)));
            ui.set_has_preview_image(true);
            ui.set_preview_summary(
                format!("{format} · {original_width} × {original_height}").into(),
            );
            "image"
        }
        PreviewContent::DocumentMetadata {
            kind,
            summary_fields,
        } => {
            ui.set_preview_summary(kind.into());
            metadata(ui, summary_fields);
            "binary"
        }
        PreviewContent::DirectorySummary {
            item_count,
            dir_count,
            file_count,
            immediate_size_bytes,
            truncated,
        } => {
            ui.set_preview_summary(
                format!(
                    "{item_count} items{}",
                    if truncated { " (partial)" } else { "" }
                )
                .into(),
            );
            ui.set_preview_truncated(truncated);
            metadata(
                ui,
                [
                    ("Folders".into(), dir_count.to_string()),
                    ("Files".into(), file_count.to_string()),
                    ("Size".into(), format_bytes(immediate_size_bytes)),
                ],
            );
            "directory"
        }
        PreviewContent::BinaryMetadata {
            mime_guess,
            size_bytes,
            hex_header,
        } => {
            ui.set_preview_summary(mime_guess.into());
            metadata(
                ui,
                [
                    ("Size".into(), format_bytes(size_bytes)),
                    ("Header".into(), hex_header),
                ],
            );
            "binary"
        }
        PreviewContent::Unavailable { reason } => {
            ui.set_preview_error(reason.into());
            "error"
        }
    };
    ui.set_preview_kind(kind.into());
}

fn metadata(ui: &OnyxWindow, fields: impl IntoIterator<Item = (String, String)>) {
    ui.set_preview_metadata(model(fields.into_iter().map(|(key, value)| {
        UiMetadataRow {
            key: key.into(),
            value: value.into(),
        }
    })));
}
