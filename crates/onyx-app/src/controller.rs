use onyx_config::ConfigStore;
use onyx_core::{FileEntry, KnownFolderKind, PaneId, ThemeMode, ViewMode};
use onyx_fs::DirectoryWatcher;
use onyx_ui::{OnyxTheme, OnyxWindow};
use slint::{ComponentHandle, ModelRc, PhysicalSize, Weak};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak as RcWeak};
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::state::{State, effective_theme, pane_id};
use crate::{AppResult, commands, is_directory, ui_sync};

thread_local! {
    static CONTROLLER: RefCell<RcWeak<Controller>> = const { RefCell::new(RcWeak::new()) };
}

pub(crate) struct Controller {
    pub ui: Weak<OnyxWindow>,

    pub state: RefCell<State>,
    pub clipboard: mpsc::UnboundedSender<crate::operations::ClipboardCommand>,
    pub shutdown: CancellationToken,
    watched: watch::Sender<Vec<PathBuf>>,
    pub runtime: Arc<Runtime>,
}

pub(crate) fn run(runtime: Arc<Runtime>, path: Option<PathBuf>) -> AppResult<()> {
    let startup = runtime.block_on(async {
        tokio::task::spawn_blocking(move || -> AppResult<_> {
            let store = ConfigStore::default_location();
            let config = store.load_config();
            let mut session = store.load_session();
            let cwd = std::env::current_dir()?;
            let folders = onyx_windows::enumerate_known_folders();
            let home = folders
                .iter()
                .find(|folder| folder.kind == KnownFolderKind::Home && is_directory(&folder.path))
                .map_or(cwd, |folder| folder.path.clone());
            let path = path.and_then(|path| {
                let valid = std::path::absolute(&path)
                    .ok()
                    .filter(|path| is_directory(path));
                if valid.is_none() {
                    tracing::warn!(path = %path.display(), "startup directory is unavailable");
                }
                valid
            });
            for tab in &mut session.tabs {
                if !is_directory(tab) {
                    tab.clone_from(&home);
                }
            }
            session.secondary_path = session.secondary_path.filter(|path| is_directory(path));
            let dark = effective_theme(config.theme) == ThemeMode::Dark;
            let accent = config
                .accent_hex
                .as_deref()
                .and_then(ui_sync::accent_color)
                .or_else(|| {
                    onyx_windows::detect_system_accent_hex()
                        .as_deref()
                        .and_then(ui_sync::accent_color)
                });
            let fonts = ui_sync::system_fonts();
            Ok((
                store, config, session, home, path, dark, accent, folders, fonts,
            ))
        })
        .await
    })??;
    let (store, config, session, home, path, dark, accent, folders, fonts) = startup;
    let ui = OnyxWindow::new()?;
    let [ui_font, brand_font, mono_font] = fonts;
    let theme = ui.global::<OnyxTheme>();
    theme.set_font_family(ui_font.into());
    theme.set_font_brand(brand_font.into());
    theme.set_font_mono(mono_font.into());
    ui.window().set_size(PhysicalSize::new(
        session.window_width,
        session.window_height,
    ));
    ui.set_dark_mode(dark);
    if let Some(color) = accent {
        ui.set_accent_color(color);
    }
    ui_sync::known_folders(&ui, folders);
    let state = State::new(store, config, session, home, path);
    ui.set_primary_entries(ModelRc::from(state.panes[0].model.clone()));
    ui.set_secondary_entries(ModelRc::from(state.panes[1].model.clone()));
    let (watched, watch_requests) = watch::channel(Vec::new());
    let (clipboard, clipboard_commands) = mpsc::unbounded_channel();
    let controller = Rc::new(Controller {
        ui: ui.as_weak(),
        runtime,
        state: RefCell::new(state),
        clipboard,
        shutdown: CancellationToken::new(),
        watched,
    });
    CONTROLLER.with(|active| *active.borrow_mut() = Rc::downgrade(&controller));
    controller.bind(&ui);
    controller.start_watcher(watch_requests);
    controller.start_clipboard_monitor(clipboard_commands);
    controller.runtime.spawn_blocking({
        let weak = controller.ui.clone();
        move || {
            let drives = onyx_windows::enumerate_drives();
            post(&weak, move |_, ui| ui_sync::drives(ui, drives));
        }
    });
    ui.show()?;
    // Winit creates the native window when the event loop starts.
    post(&controller.ui, |controller, ui| {
        if let Some(hwnd) = ui_sync::window_hwnd(ui) {
            ui_sync::apply_window_theme(hwnd, ui);
            controller.runtime.spawn_blocking(move || {
                if let Err(error) = onyx_windows::set_window_icon_from_exe(hwnd) {
                    tracing::warn!(%error, "could not set the Windows window icon");
                }
            });
        }
    });
    controller.sync_chrome(&ui);
    controller.update_watcher();
    let visible: Vec<_> = controller.state.borrow().visible_panes().collect();
    for pane in visible {
        controller.load_directory(&ui, pane, true);
    }
    let event_result = slint::run_event_loop();
    controller.shutdown.cancel();
    let size = ui.window().size();
    let (store, config, session, operation) = {
        let mut state = controller.state.borrow_mut();
        for pane in &state.panes {
            pane.cancel.cancel();
        }
        state.preview_cancel.cancel();
        if let Some((_, cancel)) = state.indexing.take() {
            cancel.cancel();
        }
        (
            state.store.clone(),
            state.config.clone(),
            state.session(size.width, size.height),
            state.operation.take(),
        )
    };
    let completed = operation.map(|(cancel, operation)| {
        cancel.cancel();
        controller.runtime.block_on(operation)
    });
    let saved = controller.runtime.block_on(async {
        tokio::task::spawn_blocking(move || {
            let config_result = store.save_config(&config);
            let session_result = store.save_session(&session);
            config_result.and(session_result)
        })
        .await
    });
    CONTROLLER.with(|active| *active.borrow_mut() = RcWeak::new());
    event_result?;
    saved??;
    completed.transpose()?;
    Ok(())
}

pub(crate) fn post(
    weak: &Weak<OnyxWindow>,
    apply: impl FnOnce(&Controller, &OnyxWindow) + Send + 'static,
) {
    let weak = weak.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade()
            && let Some(controller) = CONTROLLER.with(|active| active.borrow().upgrade())
            && !controller.shutdown.is_cancelled()
        {
            apply(&controller, &ui);
        }
    });
}

impl Controller {
    fn bind(&self, ui: &OnyxWindow) {
        macro_rules! bind {
            ($callback:ident, |$app:ident, $ui:ident $(, $arg:ident)*| $body:expr) => {
                ui.$callback({
                    let weak = self.ui.clone();
                    move |$($arg),*| {
                        if let Some($ui) = weak.upgrade()
                            && let Some($app) = CONTROLLER.with(|active| active.borrow().upgrade())
                        {
                            $body
                        }
                    }
                });
            };
        }

        bind!(on_new_tab, |app, ui| app.new_tab(&ui));
        bind!(on_close_tab, |app, ui, id| app.close_tab(&ui, id));
        bind!(on_switch_tab, |app, ui, id| app.switch_tab(&ui, id));
        bind!(on_reorder_tab, |app, ui, from, to| app
            .reorder_tab(&ui, from, to));
        bind!(on_nav_back, |app, ui| app.history(&ui, false));
        bind!(on_nav_forward, |app, ui| app.history(&ui, true));
        bind!(on_nav_up, |app, ui| app.go_up(&ui));
        bind!(on_nav_refresh, |app, ui| app.refresh(&ui));
        bind!(on_navigate_to, |app, ui, pane, path| app.navigate_to(
            &ui,
            pane_id(pane),
            PathBuf::from(path.as_str())
        ));
        bind!(on_submit_address, |app, ui, path| app
            .submit_address(&ui, path.as_str()));
        bind!(on_search_changed, |app, ui, query| app.search_changed(
            &ui,
            query.as_str(),
            true
        ));
        bind!(on_search_submitted, |app, ui, query| app.search_changed(
            &ui,
            query.as_str(),
            false
        ));
        bind!(on_toggle_indexed_search, |app, ui| app.toggle_index(&ui));
        bind!(on_select_entry, |app, ui, pane, index| app.select(
            &ui,
            pane_id(pane),
            index
        ));
        bind!(on_activate_entry, |app, ui, pane, index| app.activate(
            &ui,
            pane_id(pane),
            index
        ));
        bind!(on_context_menu_requested, |app, ui, pane, index| app
            .context_menu(&ui, pane_id(pane), index));
        bind!(on_set_active_pane, |app, ui, pane| app
            .activate_pane(&ui, pane_id(pane)));
        bind!(on_sort_by, |app, ui, pane, column| app.sort_by(
            &ui,
            pane_id(pane),
            column
        ));
        bind!(on_toggle_view_mode, |app, ui| app.toggle_view(&ui));
        bind!(on_toggle_dual_pane, |app, ui| app.toggle_dual(&ui));
        bind!(on_toggle_preview_pane, |app, ui| app.toggle_preview(&ui));
        bind!(on_toggle_quick_look, |app, ui| app.toggle_quick_look(&ui));
        bind!(on_toggle_theme, |app, ui| app.toggle_theme(&ui));
        bind!(on_toggle_hidden, |app, ui| app.toggle_hidden(&ui));
        bind!(on_toggle_favorite, |app, ui| app.toggle_favorite(&ui));
        bind!(on_unpin_favorite, |app, ui, path| app
            .unpin_favorite(&ui, Path::new(path.as_str())));
        bind!(on_action_new_folder, |app, ui| app.prompt_new_folder(&ui));
        bind!(on_action_rename, |app, ui| app.prompt_rename(&ui));
        bind!(on_action_duplicate, |app, ui| app.duplicate(&ui));
        bind!(on_action_cut, |app, ui| app.copy_selection(&ui, true));
        bind!(on_action_copy, |app, ui| app.copy_selection(&ui, false));
        bind!(on_action_paste, |app, ui| app.paste(&ui));
        bind!(on_action_recycle, |app, ui| app.recycle(&ui));
        bind!(on_action_permanent_delete, |app, ui| app.prompt_delete(&ui));
        bind!(on_action_undo, |app, ui| app.undo(&ui));
        bind!(on_action_properties, |app, ui| app.properties(&ui));
        bind!(on_action_open_with, |app, ui| app.open_with(&ui));
        bind!(on_action_open_terminal, |app, ui| app.terminal(&ui));
        bind!(on_cancel_operation, |app, ui| app.cancel_operation(&ui));
        bind!(on_toggle_command_palette, |_app, ui| commands::toggle(&ui));
        bind!(
            on_command_query_changed,
            |_app, ui, query| commands::filter(&ui, query.as_str())
        );
        bind!(on_execute_command, |app, ui, command| commands::execute(
            app.as_ref(),
            &ui,
            command.as_str()
        ));
        bind!(on_modal_confirm, |app, ui, input| app
            .confirm_prompt(&ui, input.as_str()));
        bind!(on_modal_cancel, |app, ui| app.cancel_prompt(&ui));
        bind!(on_move_selection, |app, ui, offset| app
            .move_selection(&ui, offset));
    }

    fn start_watcher(&self, mut requests: watch::Receiver<Vec<PathBuf>>) {
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let runtime = self.runtime.handle().clone();
        let stop_watcher = self.shutdown.clone();
        self.runtime.spawn_blocking(move || {
            let mut watcher = match DirectoryWatcher::new(events) {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::warn!(%error, "live directory refresh unavailable");
                    return;
                }
            };
            while runtime.block_on(async {
                tokio::select! {
                    _ = stop_watcher.cancelled() => false,
                    changed = requests.changed() => changed.is_ok(),
                }
            }) {
                let paths = requests.borrow_and_update().clone();
                for removed in watcher
                    .watched_paths()
                    .iter()
                    .filter(|path| !paths.contains(path))
                {
                    if let Err(error) = watcher.unwatch(removed) {
                        tracing::debug!(%error, "could not remove directory watch");
                    }
                }
                for path in &paths {
                    if let Err(error) = watcher.watch(path) {
                        tracing::debug!(%error, "could not watch directory");
                    }
                }
            }
        });
        let weak = self.ui.clone();
        let shutdown = self.shutdown.clone();
        self.runtime.spawn(async move {
            while let Some(event) = tokio::select! {
                _ = shutdown.cancelled() => None,
                event = receiver.recv() => event,
            } {
                post(&weak, move |app, ui| app.refresh_paths(ui, &[event.path]));
            }
        });
    }

    pub(crate) fn update_watcher(&self) {
        let state = self.state.borrow();
        self.watched.send_replace(
            state
                .visible_panes()
                .map(|pane| state.path(pane).to_path_buf())
                .collect(),
        );
    }

    pub(crate) fn sync_chrome(&self, ui: &OnyxWindow) {
        let state = self.state.borrow();
        ui_sync::chrome(ui, &state);
        let query = state.pane(state.active).query.as_str();
        if ui.get_search_text().as_str() != query {
            ui.set_search_text(query.into());
        }
        ui_sync::status(ui, &state);
    }

    pub(crate) fn sync_pane(&self, ui: &OnyxWindow, pane: PaneId) {
        let state = self.state.borrow();
        ui_sync::pane(ui, &state, pane);
        if state.active == pane {
            ui_sync::status(ui, &state);
        }
    }

    pub(crate) fn active(&self) -> PaneId {
        self.state.borrow().active
    }

    pub(crate) fn selected(&self) -> Option<FileEntry> {
        let state = self.state.borrow();
        state.selected(state.active).cloned()
    }

    pub(crate) fn activate_pane(&self, ui: &OnyxWindow, pane: PaneId) {
        {
            let mut state = self.state.borrow_mut();
            if pane == state.active || (pane == PaneId::Secondary && !state.dual) {
                return;
            }
            state.active = pane;
        }
        self.sync_chrome(ui);
        self.update_preview(ui);
    }

    fn toggle_view(&self, ui: &OnyxWindow) {
        let mut state = self.state.borrow_mut();
        state.config.default_view = if state.config.default_view == ViewMode::Grid {
            ViewMode::Details
        } else {
            ViewMode::Grid
        };
        ui.set_view_mode(i32::from(state.config.default_view == ViewMode::Grid));
    }

    pub(crate) fn toggle_dual(&self, ui: &OnyxWindow) {
        let enabled = {
            let mut state = self.state.borrow_mut();
            state.dual = !state.dual;
            state.config.dual_pane_default = state.dual;
            if !state.dual {
                state.active = PaneId::Primary;
                state.pane_mut(PaneId::Secondary).cancel.cancel();
            }
            state.dual
        };
        self.sync_chrome(ui);
        self.update_watcher();
        if enabled {
            self.reload(ui, PaneId::Secondary, true);
        } else {
            self.update_preview(ui);
        }
    }

    pub(crate) fn toggle_preview(&self, ui: &OnyxWindow) {
        let enabled = !self.state.borrow().config.show_preview_pane;
        self.state.borrow_mut().config.show_preview_pane = enabled;
        ui.set_preview_pane(enabled);
        self.update_preview(ui);
    }

    pub(crate) fn toggle_quick_look(&self, ui: &OnyxWindow) {
        let enabled = !self.state.borrow().quick_look && self.selected().is_some();
        self.state.borrow_mut().quick_look = enabled;
        ui.set_quick_look_open(enabled);
        self.update_preview(ui);
    }

    pub(crate) fn toggle_theme(&self, ui: &OnyxWindow) {
        let dark = !ui.get_dark_mode();
        self.state.borrow_mut().config.theme = if dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        ui.set_dark_mode(dark);
        if let Some(hwnd) = ui_sync::window_hwnd(ui) {
            ui_sync::apply_window_theme(hwnd, ui);
        }
    }
}
