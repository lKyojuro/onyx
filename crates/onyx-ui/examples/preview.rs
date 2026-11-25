//! Interactive standalone preview of the Onyx Fluent Slint UI.
//!
//! Run with: `cargo run -p onyx-ui --example preview`

use std::rc::Rc;

use onyx_ui::{
    OnyxWindow, UiBreadcrumb, UiCommandItem, UiDriveItem, UiFileEntry, UiMetadataRow,
    UiOperationProgress, UiSidebarItem, UiTabItem,
};
use slint::{ComponentHandle, Image, Model, ModelRc, SharedString, VecModel};

fn sample_entries() -> Vec<UiFileEntry> {
    vec![
        UiFileEntry {
            path: "D:\\Onyx\\crates".into(),
            name: "crates".into(),
            modified: "2026-11-24 17:48".into(),
            type_label: "File folder".into(),
            size_label: "".into(),
            size_ratio: 0.0,
            is_dir: true,
            is_hidden: false,
            is_symlink: false,
            selected: false,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
        UiFileEntry {
            path: "D:\\Onyx\\assets".into(),
            name: "assets".into(),
            modified: "2026-11-24 17:55".into(),
            type_label: "File folder".into(),
            size_label: "".into(),
            size_ratio: 0.0,
            is_dir: true,
            is_hidden: false,
            is_symlink: false,
            selected: false,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
        UiFileEntry {
            path: "D:\\Onyx\\AGENTS.md".into(),
            name: "AGENTS.md".into(),
            modified: "2026-11-24 17:32".into(),
            type_label: "Markdown File".into(),
            size_label: "1.2 KB".into(),
            size_ratio: 0.0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            selected: true,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
        UiFileEntry {
            path: "D:\\Onyx\\Cargo.toml".into(),
            name: "Cargo.toml".into(),
            modified: "2026-11-24 17:45".into(),
            type_label: "TOML File".into(),
            size_label: "3.1 KB".into(),
            size_ratio: 0.0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            selected: false,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
        UiFileEntry {
            path: "D:\\Onyx\\README.md".into(),
            name: "README.md".into(),
            modified: "2026-11-24 17:30".into(),
            type_label: "Markdown File".into(),
            size_label: "4.8 KB".into(),
            size_ratio: 0.0,
            is_dir: false,
            is_hidden: false,
            is_symlink: false,
            selected: false,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
        UiFileEntry {
            path: "D:\\Onyx\\onyx-link".into(),
            name: "onyx-link".into(),
            modified: "2026-11-24 16:10".into(),
            type_label: "Symbolic Link".into(),
            size_label: "".into(),
            size_ratio: 0.0,
            is_dir: true,
            is_hidden: false,
            is_symlink: true,
            selected: false,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
        UiFileEntry {
            path: "D:\\Onyx\\.gitignore".into(),
            name: ".gitignore".into(),
            modified: "2026-11-24 15:20".into(),
            type_label: "GITIGNORE File".into(),
            size_label: "412 B".into(),
            size_ratio: 0.0,
            is_dir: false,
            is_hidden: true,
            is_symlink: false,
            selected: false,
            icon: Image::default(),
            has_icon: false,
            thumbnail: Image::default(),
            has_thumbnail: false,
        },
    ]
}

fn sample_commands() -> Vec<UiCommandItem> {
    vec![
        UiCommandItem {
            id: "toggle-theme".into(),
            title: "Toggle Dark / Light Theme".into(),
            shortcut: "".into(),
            category: "Appearance".into(),
        },
        UiCommandItem {
            id: "toggle-dual".into(),
            title: "Toggle Dual-Pane View".into(),
            shortcut: "".into(),
            category: "Layout".into(),
        },
        UiCommandItem {
            id: "toggle-view".into(),
            title: "Switch Details / Grid View".into(),
            shortcut: "".into(),
            category: "Layout".into(),
        },
        UiCommandItem {
            id: "quick-look".into(),
            title: "Toggle Quick Look Preview".into(),
            shortcut: "Space".into(),
            category: "Preview".into(),
        },
        UiCommandItem {
            id: "new-folder".into(),
            title: "Create New Folder".into(),
            shortcut: "".into(),
            category: "File".into(),
        },
    ]
}

fn main() -> Result<(), slint::PlatformError> {
    let window = OnyxWindow::new()?;

    // Tabs
    let tabs = Rc::new(VecModel::from(vec![
        UiTabItem {
            id: 1,
            title: "Onyx".into(),
            path: "D:\\Onyx".into(),
            active: true,
        },
        UiTabItem {
            id: 2,
            title: "Downloads".into(),
            path: "C:\\Users\\User\\Downloads".into(),
            active: false,
        },
    ]));
    window.set_tabs(ModelRc::from(tabs));

    // Breadcrumbs & Address bar
    window.set_breadcrumbs(ModelRc::from(Rc::new(VecModel::from(vec![
        UiBreadcrumb {
            label: "D:".into(),
            path: "D:\\".into(),
        },
        UiBreadcrumb {
            label: "Onyx".into(),
            path: "D:\\Onyx".into(),
        },
    ]))));
    window.set_address_text("D:\\Onyx".into());
    window.set_home_path("C:\\Users\\User".into());
    window.set_can_go_back(true);
    window.set_is_current_favorite(true);

    // Sidebar
    window.set_favorites(ModelRc::from(Rc::new(VecModel::from(vec![
        UiSidebarItem {
            label: "Onyx".into(),
            path: "D:\\Onyx".into(),
            icon_glyph: "star-filled".into(),
            is_favorite: true,
            icon: Image::default(),
            has_icon: false,
        },
    ]))));

    window.set_known_folders(ModelRc::from(Rc::new(VecModel::from(vec![
        UiSidebarItem {
            label: "Desktop".into(),
            path: "C:\\Users\\User\\Desktop".into(),
            icon_glyph: "desktop".into(),
            is_favorite: false,
            icon: Image::default(),
            has_icon: false,
        },
        UiSidebarItem {
            label: "Downloads".into(),
            path: "C:\\Users\\User\\Downloads".into(),
            icon_glyph: "download".into(),
            is_favorite: false,
            icon: Image::default(),
            has_icon: false,
        },
        UiSidebarItem {
            label: "Documents".into(),
            path: "C:\\Users\\User\\Documents".into(),
            icon_glyph: "file".into(),
            is_favorite: false,
            icon: Image::default(),
            has_icon: false,
        },
        UiSidebarItem {
            label: "Pictures".into(),
            path: "C:\\Users\\User\\Pictures".into(),
            icon_glyph: "image".into(),
            is_favorite: false,
            icon: Image::default(),
            has_icon: false,
        },
    ]))));

    window.set_drives(ModelRc::from(Rc::new(VecModel::from(vec![
        UiDriveItem {
            label: "Windows (C:)".into(),
            root: "C:\\".into(),
            filesystem: "NTFS".into(),
            free_label: "148 GB free of 512 GB".into(),
            usage_ratio: 0.71,
            icon: Image::default(),
            has_icon: false,
        },
        UiDriveItem {
            label: "Data (D:)".into(),
            root: "D:\\".into(),
            filesystem: "NTFS".into(),
            free_label: "620 GB free of 1.0 TB".into(),
            usage_ratio: 0.38,
            icon: Image::default(),
            has_icon: false,
        },
        UiDriveItem {
            label: "Archive (E:)".into(),
            root: "E:\\".into(),
            filesystem: "NTFS".into(),
            free_label: "42 GB free of 500 GB".into(),
            usage_ratio: 0.92,
            icon: Image::default(),
            has_icon: false,
        },
    ]))));

    // File entries
    let primary_model = Rc::new(VecModel::from(sample_entries()));
    let secondary_model = Rc::new(VecModel::from(sample_entries()));
    window.set_primary_entries(ModelRc::from(primary_model.clone()));
    window.set_primary_path("D:\\Onyx".into());
    window.set_primary_selected_index(2);
    window.set_secondary_entries(ModelRc::from(secondary_model));
    window.set_secondary_path("D:\\Onyx\\crates".into());
    window.set_has_selection(true);
    window.set_can_paste(true);
    window.set_can_undo(true);

    // Preview content
    window.set_preview_title("AGENTS.md".into());
    window.set_preview_subtitle("Markdown File • 1.2 KB".into());
    window.set_preview_kind("text".into());
    window.set_preview_language("Markdown".into());
    window.set_preview_text(
        "# AGENTS.md — Onyx Engineering Guidelines\n\n\
         ## Architecture Rules\n\
         1. No UI dependencies in core crates.\n\
         2. Isolate platform Win32/COM calls inside onyx-windows.\n\
         3. Never block the UI thread.\n\
         4. Generation tracking for async navigation & search.\n\
         5. Default delete always uses Windows Recycle Bin."
            .into(),
    );
    window.set_preview_metadata(ModelRc::from(Rc::new(VecModel::from(vec![
        UiMetadataRow {
            key: "Path".into(),
            value: "D:\\Onyx\\AGENTS.md".into(),
        },
        UiMetadataRow {
            key: "Modified".into(),
            value: "2026-11-24 17:32".into(),
        },
        UiMetadataRow {
            key: "Size".into(),
            value: "1,248 bytes".into(),
        },
        UiMetadataRow {
            key: "Encoding".into(),
            value: "UTF-8".into(),
        },
    ]))));

    // Command palette items
    let all_commands = sample_commands();
    window.set_command_items(ModelRc::from(Rc::new(VecModel::from(all_commands.clone()))));

    // Status bar
    window.set_status_items_text("7 items".into());
    window.set_status_selection_text("1 selected (1.2 KB)".into());
    window.set_status_search_text("Preview Mode".into());

    // Interactive callbacks
    let weak = window.as_weak();
    window.on_toggle_theme(move || {
        if let Some(w) = weak.upgrade() {
            w.set_dark_mode(!w.get_dark_mode());
        }
    });

    let weak = window.as_weak();
    window.on_toggle_view_mode(move || {
        if let Some(w) = weak.upgrade() {
            w.set_view_mode(if w.get_view_mode() == 0 { 1 } else { 0 });
        }
    });

    let weak = window.as_weak();
    window.on_toggle_dual_pane(move || {
        if let Some(w) = weak.upgrade() {
            w.set_dual_pane(!w.get_dual_pane());
        }
    });

    let weak = window.as_weak();
    window.on_toggle_preview_pane(move || {
        if let Some(w) = weak.upgrade() {
            w.set_preview_pane(!w.get_preview_pane());
        }
    });

    let weak = window.as_weak();
    window.on_toggle_quick_look(move || {
        if let Some(w) = weak.upgrade() {
            w.set_quick_look_open(!w.get_quick_look_open());
        }
    });

    let weak = window.as_weak();
    window.on_toggle_hidden(move || {
        if let Some(w) = weak.upgrade() {
            w.set_show_hidden(!w.get_show_hidden());
        }
    });

    let weak = window.as_weak();
    window.on_toggle_indexed_search(move || {
        if let Some(w) = weak.upgrade() {
            w.set_indexed_search(!w.get_indexed_search());
        }
    });

    let weak = window.as_weak();
    window.on_toggle_command_palette(move || {
        if let Some(w) = weak.upgrade() {
            w.set_command_palette_open(!w.get_command_palette_open());
        }
    });

    let weak = window.as_weak();
    let cmds_for_filter = all_commands;
    window.on_command_query_changed(move |query| {
        if let Some(w) = weak.upgrade() {
            let q = query.to_lowercase();
            let filtered: Vec<UiCommandItem> = cmds_for_filter
                .iter()
                .filter(|c| {
                    c.title.to_lowercase().contains(&q) || c.category.to_lowercase().contains(&q)
                })
                .cloned()
                .collect();
            w.set_command_items(ModelRc::from(Rc::new(VecModel::from(filtered))));
        }
    });

    let weak = window.as_weak();
    window.on_execute_command(move |id| {
        if let Some(w) = weak.upgrade() {
            w.set_command_palette_open(false);
            match id.as_str() {
                "toggle-theme" => w.set_dark_mode(!w.get_dark_mode()),
                "toggle-dual" => w.set_dual_pane(!w.get_dual_pane()),
                "toggle-view" => w.set_view_mode(if w.get_view_mode() == 0 { 1 } else { 0 }),
                "quick-look" => w.set_quick_look_open(!w.get_quick_look_open()),
                "new-folder" => {
                    w.set_modal_title("New Folder".into());
                    w.set_modal_message("Enter a name for the new folder:".into());
                    w.set_modal_input("New folder".into());
                    w.set_modal_show_input(true);
                    w.set_modal_confirm_label("Create".into());
                    w.set_modal_danger(false);
                    w.set_modal_open(true);
                }
                _ => {}
            }
        }
    });

    let weak = window.as_weak();
    window.on_action_new_folder(move || {
        if let Some(w) = weak.upgrade() {
            w.set_modal_title("New Folder".into());
            w.set_modal_message("Enter a name for the new folder:".into());
            w.set_modal_input("New folder".into());
            w.set_modal_show_input(true);
            w.set_modal_confirm_label("Create".into());
            w.set_modal_danger(false);
            w.set_modal_open(true);
        }
    });

    let weak = window.as_weak();
    window.on_modal_confirm(move |val| {
        if let Some(w) = weak.upgrade() {
            w.set_modal_open(false);
            w.set_status_message(SharedString::from(format!("Confirmed: {val}")));
        }
    });

    let weak = window.as_weak();
    window.on_modal_cancel(move || {
        if let Some(w) = weak.upgrade() {
            w.set_modal_open(false);
        }
    });

    let weak = window.as_weak();
    let entries_model = primary_model;
    window.on_select_entry(move |pane, idx| {
        if let Some(w) = weak.upgrade() {
            w.set_active_pane(pane);
            if pane == 0 {
                let len = entries_model.row_count();
                for i in 0..len {
                    if let Some(mut item) = entries_model.row_data(i) {
                        item.selected = i as i32 == idx;
                        entries_model.set_row_data(i, item);
                    }
                }
                if idx >= 0
                    && let Some(item) = entries_model.row_data(idx as usize)
                {
                    w.set_preview_title(item.name.clone());
                    w.set_preview_subtitle(item.type_label.clone());
                    if item.is_dir {
                        w.set_preview_kind("directory".into());
                        w.set_preview_summary("Folder preview (standalone UI demo)".into());
                    } else {
                        w.set_preview_kind("text".into());
                    }
                    w.set_status_selection_text(SharedString::from(format!(
                        "1 selected ({})",
                        if item.size_label.is_empty() {
                            item.type_label.as_str()
                        } else {
                            item.size_label.as_str()
                        }
                    )));
                }
            }
        }
    });

    let weak = window.as_weak();
    window.on_set_active_pane(move |pane| {
        if let Some(w) = weak.upgrade() {
            w.set_active_pane(pane);
        }
    });

    let weak = window.as_weak();
    window.on_sort_by(move |pane, col| {
        if let Some(w) = weak.upgrade() {
            if pane == 0 {
                if w.get_primary_sort_column() == col {
                    w.set_primary_sort_ascending(!w.get_primary_sort_ascending());
                } else {
                    w.set_primary_sort_column(col);
                    w.set_primary_sort_ascending(true);
                }
            } else if w.get_secondary_sort_column() == col {
                w.set_secondary_sort_ascending(!w.get_secondary_sort_ascending());
            } else {
                w.set_secondary_sort_column(col);
                w.set_secondary_sort_ascending(true);
            }
        }
    });

    let weak = window.as_weak();
    window.on_cancel_operation(move || {
        if let Some(w) = weak.upgrade() {
            w.set_operation_progress(UiOperationProgress::default());
        }
    });

    window.run()
}
