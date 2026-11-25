use onyx_ui::{OnyxWindow, UiCommandItem};

use crate::controller::Controller;
use crate::ui_sync;

pub(crate) struct Command {
    pub id: &'static str,
    pub title: &'static str,
    pub shortcut: &'static str,
    pub category: &'static str,
    pub run: fn(&Controller, &OnyxWindow),
}

pub(crate) const COMMANDS: &[Command] = &[
    Command {
        id: "settings",
        title: "Open Settings",
        shortcut: "Ctrl+,",
        category: "Preferences",
        run: Controller::toggle_settings,
    },
    Command {
        id: "new-tab",
        title: "New Tab",
        shortcut: "Ctrl+T",
        category: "Navigation",
        run: Controller::new_tab,
    },
    Command {
        id: "close-tab",
        title: "Close Tab",
        shortcut: "Ctrl+W",
        category: "Navigation",
        run: |app, ui| app.close_tab(ui, -1),
    },
    Command {
        id: "refresh",
        title: "Refresh",
        shortcut: "F5",
        category: "Navigation",
        run: Controller::refresh,
    },
    Command {
        id: "favorite",
        title: "Pin or Unpin Favorite",
        shortcut: "",
        category: "Navigation",
        run: Controller::toggle_favorite,
    },
    Command {
        id: "dual",
        title: "Toggle Dual Pane",
        shortcut: "",
        category: "View",
        run: Controller::toggle_dual,
    },
    Command {
        id: "preview",
        title: "Toggle Preview Pane",
        shortcut: "",
        category: "View",
        run: Controller::toggle_preview,
    },
    Command {
        id: "quick-look",
        title: "Quick Look",
        shortcut: "Space",
        category: "View",
        run: Controller::toggle_quick_look,
    },
    Command {
        id: "theme",
        title: "Toggle Dark / Light Theme",
        shortcut: "",
        category: "View",
        run: Controller::toggle_theme,
    },
    Command {
        id: "hidden",
        title: "Toggle Hidden Files",
        shortcut: "",
        category: "View",
        run: Controller::toggle_hidden,
    },
    Command {
        id: "new-folder",
        title: "New Folder",
        shortcut: "",
        category: "Files",
        run: Controller::prompt_new_folder,
    },
    Command {
        id: "rename",
        title: "Rename",
        shortcut: "F2",
        category: "Files",
        run: Controller::prompt_rename,
    },
    Command {
        id: "duplicate",
        title: "Duplicate",
        shortcut: "",
        category: "Files",
        run: Controller::duplicate,
    },
    Command {
        id: "copy",
        title: "Copy",
        shortcut: "Ctrl+C",
        category: "Files",
        run: |app, ui| app.copy_selection(ui, false),
    },
    Command {
        id: "cut",
        title: "Cut",
        shortcut: "Ctrl+X",
        category: "Files",
        run: |app, ui| app.copy_selection(ui, true),
    },
    Command {
        id: "paste",
        title: "Paste",
        shortcut: "Ctrl+V",
        category: "Files",
        run: Controller::paste,
    },
    Command {
        id: "recycle",
        title: "Move to Recycle Bin",
        shortcut: "Delete",
        category: "Files",
        run: Controller::recycle,
    },
    Command {
        id: "undo",
        title: "Undo",
        shortcut: "Ctrl+Z",
        category: "Files",
        run: Controller::undo,
    },
    Command {
        id: "external-drag",
        title: "Drag to External App",
        shortcut: "",
        category: "Files",
        run: Controller::external_drag,
    },
    Command {
        id: "compress-zip",
        title: "Compress to ZIP",
        shortcut: "",
        category: "Archive",
        run: Controller::compress_zip,
    },
    Command {
        id: "extract-here",
        title: "Extract Here",
        shortcut: "",
        category: "Archive",
        run: Controller::extract_here,
    },
    Command {
        id: "extract-to-folder",
        title: "Extract to Folder",
        shortcut: "",
        category: "Archive",
        run: Controller::extract_to_folder,
    },
    Command {
        id: "browse-archive",
        title: "Browse Archive",
        shortcut: "",
        category: "Archive",
        run: Controller::browse_archive,
    },
    Command {
        id: "terminal",
        title: "Open Terminal",
        shortcut: "Ctrl+` / Alt+T",
        category: "Tools",
        run: Controller::terminal,
    },
    Command {
        id: "powershell",
        title: "Open PowerShell here",
        shortcut: "",
        category: "Tools",
        run: |app, _| app.terminal_with_profile(onyx_windows::TerminalProfile::PowerShell),
    },
    Command {
        id: "command-prompt",
        title: "Open Command Prompt here",
        shortcut: "",
        category: "Tools",
        run: |app, _| app.terminal_with_profile(onyx_windows::TerminalProfile::Cmd),
    },
    Command {
        id: "index",
        title: "Index Current Folder",
        shortcut: "",
        category: "Search",
        run: Controller::index_current,
    },
];

pub(crate) fn toggle(ui: &OnyxWindow) {
    let open = !ui.get_command_palette_open();
    if open {
        ui.set_command_query("".into());
        filter(ui, "");
    }
    ui.set_command_palette_open(open);
}

pub(crate) fn matching_commands(query: &str) -> impl Iterator<Item = &'static Command> + '_ {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    COMMANDS.iter().filter(move |command| {
        let title = command.title.to_lowercase();
        let category = command.category.to_lowercase();
        let shortcut = command.shortcut.to_lowercase();
        words.iter().all(|word| {
            title.contains(word.as_str())
                || category.contains(word.as_str())
                || shortcut.contains(word.as_str())
                || command.id.contains(word.as_str())
        })
    })
}

pub(crate) fn filter(ui: &OnyxWindow, query: &str) {
    ui.set_command_items(ui_sync::model(matching_commands(query).map(|command| {
        UiCommandItem {
            id: command.id.into(),
            title: command.title.into(),
            shortcut: command.shortcut.into(),
            category: command.category.into(),
        }
    })));
}

pub(crate) fn execute(app: &Controller, ui: &OnyxWindow, id: &str) {
    if let Some(command) = COMMANDS.iter().find(|command| command.id == id) {
        ui.set_command_palette_open(false);
        (command.run)(app, ui);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_palette_includes_settings_archive_and_dnd_actions() {
        let ids: Vec<&str> = matching_commands("").map(|c| c.id).collect();
        for required in [
            "settings",
            "compress-zip",
            "extract-here",
            "extract-to-folder",
            "browse-archive",
            "external-drag",
        ] {
            assert!(
                ids.contains(&required),
                "expected command '{required}' to be registered"
            );
        }

        let settings_hits: Vec<&str> = matching_commands("open settings").map(|c| c.id).collect();
        assert_eq!(settings_hits, vec!["settings"]);

        let archive_hits: Vec<&str> = matching_commands("archive").map(|c| c.id).collect();
        assert!(archive_hits.contains(&"compress-zip"));
        assert!(archive_hits.contains(&"extract-here"));
        assert!(archive_hits.contains(&"extract-to-folder"));
        assert!(archive_hits.contains(&"browse-archive"));
    }
}
