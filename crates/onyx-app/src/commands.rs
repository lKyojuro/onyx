use onyx_ui::{OnyxWindow, UiCommandItem};

use crate::controller::Controller;
use crate::ui_sync;

struct Command {
    id: &'static str,
    title: &'static str,
    shortcut: &'static str,
    category: &'static str,
    run: fn(&Controller, &OnyxWindow),
}

const COMMANDS: &[Command] = &[
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
        id: "terminal",
        title: "Open Terminal",
        shortcut: "",
        category: "Tools",
        run: Controller::terminal,
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

pub(crate) fn filter(ui: &OnyxWindow, query: &str) {
    let query = query.to_lowercase();
    ui.set_command_items(ui_sync::model(
        COMMANDS
            .iter()
            .filter(|command| {
                let title = command.title.to_lowercase();
                let category = command.category.to_lowercase();
                let shortcut = command.shortcut.to_lowercase();
                query.split_whitespace().all(|word| {
                    title.contains(word)
                        || category.contains(word)
                        || shortcut.contains(word)
                        || command.id.contains(word)
                })
            })
            .map(|command| UiCommandItem {
                id: command.id.into(),
                title: command.title.into(),
                shortcut: command.shortcut.into(),
                category: command.category.into(),
            }),
    ));
}

pub(crate) fn execute(app: &Controller, ui: &OnyxWindow, id: &str) {
    if let Some(command) = COMMANDS.iter().find(|command| command.id == id) {
        ui.set_command_palette_open(false);
        (command.run)(app, ui);
    }
}
