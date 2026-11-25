import io

p = 'src/explorer.rs'
s = io.open(p, encoding='utf-8', newline='').read()

anchor = "    // ---- Command Palette & Settings overlays (Stage 3A) ----"
block = '''    // ---- Archive browser & ZIP actions (Stage 3B) ----

    /// Opens the archive browser for `path` and lists its entries asynchronously.
    pub fn open_archive_browser(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.archive_cancel.cancel();
        let cancel = CancellationToken::new();
        self.archive_cancel = cancel.clone();
        self.archive_generation = self.archive_generation.next();
        let generation = self.archive_generation;

        self.command_palette = None;
        self.settings_open = false;
        self.quick_look_open = false;
        self.context_menu = None;
        self.prompt_state = None;
        self.drag = None;
        self.archive = Some(ArchiveViewState::new(path.clone(), generation, cancel.clone()));
        cx.notify();

        let runtime = Arc::clone(&self.runtime);
        let (tx, rx) = oneshot::channel();
        runtime.spawn(async move {
            let result = list_archive_entries(path, cancel).await;
            let _ = tx.send(result);
        });

        cx.spawn(async move |this, cx| {
            let Ok(result) = rx.await else {
                return;
            };
            let _ = this.update(cx, |explorer, cx| {
                if explorer.archive_generation != generation {
                    return;
                }
                match result {
                    Ok(entries) => {
                        if let Some(archive) = &mut explorer.archive {
                            archive.apply_entries(entries);
                        }
                    }
                    Err(OnyxError::Cancelled) => return,
                    Err(err) => {
                        if let Some(archive) = &mut explorer.archive {
                            archive.apply_error(err.to_string());
                        }
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn close_archive(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.archive.take().is_some() {
            self.archive_cancel.cancel();
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    #[must_use]
    pub fn archive_is_open(&self) -> bool {
        self.archive.is_some()
    }

    /// Extracts the open archive into its own folder.
    pub fn extract_archive_here(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.archive.as_ref() else {
            return;
        };
        let archive_path = state.path.clone();
        let Some(parent) = archive_path.parent().map(Path::to_path_buf) else {
            return;
        };
        self.archive = None;
        self.archive_cancel.cancel();
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir: parent,
            },
            cx,
        );
    }

    /// Extracts the open archive into `<parent>/<stem>`.
    pub fn extract_archive_to_folder(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.archive.as_ref() else {
            return;
        };
        let archive_path = state.path.clone();
        let dest_dir = extract_folder_for(&archive_path);
        self.archive = None;
        self.archive_cancel.cancel();
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir,
            },
            cx,
        );
    }

    /// Compresses the selected item into a ZIP next to it.
    pub fn compress_selection_to_zip(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            self.status_message = Some("Select a file or folder to compress.".to_string());
            cx.notify();
            return;
        };
        if active_is_archive(entry) {
            self.status_message = Some("Archives cannot be compressed again.".to_string());
            cx.notify();
            return;
        }
        let sources = vec![entry.path.clone()];
        let current_dir = self.active_directory();
        self.start_operation(
            FileOperation::CreateArchive {
                sources,
                current_dir,
            },
            cx,
        );
    }

    /// Extracts the selected archive into the active directory.
    pub fn extract_selected_archive(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self.selected_entry() else {
            return;
        };
        if !active_is_archive(entry) {
            self.status_message =
                Some("Select a .zip, .tar.gz or .tgz archive first.".to_string());
            cx.notify();
            return;
        }
        let archive_path = entry.path.clone();
        let dest_dir = self.active_directory();
        self.start_operation(
            FileOperation::ExtractArchive {
                archive_path,
                dest_dir,
            },
            cx,
        );
    }

    #[must_use]
    pub fn selection_is_archive(&self) -> bool {
        self.selected_entry().is_some_and(active_is_archive)
    }

    // ---- Internal drag & drop (Stage 3B) ----

    /// Starts a potential drag on mouse-down; it only arms after the threshold.
    pub fn begin_drag(
        &mut self,
        source: PathBuf,
        is_dir: bool,
        origin: (f32, f32),
        cx: &mut Context<Self>,
    ) {
        let label = source.file_name().map_or_else(
            || source.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        self.drag = Some(DragState::new(source, label, is_dir, origin));
        cx.notify();
    }

    /// Tracks the pointer while a drag is in flight.
    pub fn update_drag(&mut self, pointer: (f32, f32), copy: bool, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        drag.track(pointer, copy);
        if drag.armed {
            cx.notify();
        }
    }

    /// Records the drop target under the pointer while dragging.
    pub fn set_drop_hover(&mut self, target: DropTarget, cx: &mut Context<Self>) {
        let Some(drag) = &mut self.drag else {
            return;
        };
        if !drag.armed || drag.hover.as_ref() == Some(&target) {
            return;
        }
        drag.hover = Some(target);
        cx.notify();
    }

    /// Aborts the drag without touching the file system.
    pub fn cancel_drag(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
        }
    }

    /// Completes the drag: copies or moves the source into the hovered folder.
    pub fn finish_drag(&mut self, cx: &mut Context<Self>) {
        let Some(drag) = self.drag.take() else {
            return;
        };
        cx.notify();
        if !drag.armed {
            return;
        }
        let Some(target) = drag.hover else {
            self.status_message = Some("Drop onto a folder, tab, or the other pane.".to_string());
            cx.notify();
            return;
        };
        let destination_dir = match &target {
            DropTarget::Directory(path) => path.clone(),
            DropTarget::Pane(pane) => self.pane_path(*pane).to_path_buf(),
        };
        match validate_drop(&drag.source, &destination_dir) {
            Ok(destination) => {
                let operation = if drag.copy {
                    FileOperation::CopyItems {
                        sources: vec![drag.source],
                        destination,
                    }
                } else {
                    FileOperation::MoveItems {
                        sources: vec![drag.source],
                        destination,
                    }
                };
                self.start_operation(operation, cx);
            }
            Err(DropReject::SameItem | DropReject::IntoItself | DropReject::AlreadyThere) => {
                self.status_message = Some(drag_reject_message(&target).to_string());
                cx.notify();
            }
        }
    }

    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.drag.as_ref().is_some_and(|drag| drag.armed)
    }

    /// Whether `path` is the folder currently highlighted as a drop target.
    #[must_use]
    pub fn drag_hover_is(&self, path: &Path) -> bool {
        self.drag.as_ref().is_some_and(|drag| {
            drag.armed
                && matches!(
                    drag.hover.as_ref(),
                    Some(DropTarget::Directory(hovered)) if same_path(hovered, path)
                )
        })
    }

    /// Whether the given pane background is the highlighted drop target.
    #[must_use]
    pub fn drag_hover_is_pane(&self, pane: PaneId) -> bool {
        self.drag.as_ref().is_some_and(|drag| {
            drag.armed && drag.hover.as_ref() == Some(&DropTarget::Pane(pane))
        })
    }

'''
assert s.count(anchor) == 1
s = s.replace(anchor, block + anchor)

# overlay_open also reports the archive browser
a = """    pub fn overlay_open(&self) -> bool {
        self.command_palette.is_some() || self.settings_open
    }"""
b = """    pub fn overlay_open(&self) -> bool {
        self.command_palette.is_some() || self.settings_open || self.archive.is_some()
    }"""
assert s.count(a) == 1
s = s.replace(a, b)

# .zip opens the archive browser instead of the shell association
a = """        if entry.is_dir() {
            match pane {
                PaneId::Primary => self.load_directory(entry.path, true, true, cx),
                PaneId::Secondary => self.load_secondary_directory(entry.path, true, cx),
            }
        } else {"""
b = """        if entry.is_dir() {
            match pane {
                PaneId::Primary => self.load_directory(entry.path, true, true, cx),
                PaneId::Secondary => self.load_secondary_directory(entry.path, true, cx),
            }
        } else if active_is_archive(&entry) {
            self.open_archive_browser(entry.path, cx);
        } else {"""
assert s.count(a) == 1
s = s.replace(a, b)

# archive overlay modality + Escape cancels a drag
a = """        if self.settings_open {
            if key == "escape" || (mods.control && key == ",") {"""
b = """        if self.archive.is_some() {
            if key == "escape" {
                self.close_archive(window, cx);
                cx.stop_propagation();
            }
            return;
        }

        if self.drag.is_some() {
            if key == "escape" {
                self.cancel_drag(cx);
                cx.stop_propagation();
                return;
            }
        }

        if self.settings_open {
            if key == "escape" || (mods.control && key == ",") {"""
assert s.count(a) == 1
s = s.replace(a, b)

# render: archive overlay + drag ghost
a = """        if let Some(palette_overlay) = command_palette::render_command_palette(self, cx) {
            root = root.child(palette_overlay);
        }"""
b = """        if let Some(palette_overlay) = command_palette::render_command_palette(self, cx) {
            root = root.child(palette_overlay);
        }
        if let Some(archive_element) = archive_overlay::render_archive_overlay(self, window, cx) {
            root = root.child(archive_element);
        }"""
assert s.count(a) == 1
s = s.replace(a, b)

a = """        if let Some(settings_element) = settings_overlay::render_settings_overlay(self, window, cx) {
            root = root.child(settings_element);
        }"""
b = """        if let Some(settings_element) = settings_overlay::render_settings_overlay(self, window, cx) {
            root = root.child(settings_element);
        }
        if let Some(ghost) = drag_drop::render_drag_ghost(self) {
            root = root.child(ghost);
        }"""
assert s.count(a) == 1
s = s.replace(a, b)

# pointer tracking: drag next to splitter drag
a = """            .on_mouse_move(cx.listener(|explorer, ev: &MouseMoveEvent, _, cx| {
                if explorer.splitter_drag.is_some() {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        explorer.update_splitter_drag(f32::from(ev.position.x), cx);
                    } else {
                        explorer.end_splitter_drag(cx);
                    }
                }
            }))"""
b = """            .on_mouse_move(cx.listener(|explorer, ev: &MouseMoveEvent, _, cx| {
                let pointer = (f32::from(ev.position.x), f32::from(ev.position.y));
                if explorer.splitter_drag.is_some() {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        explorer.update_splitter_drag(pointer.0, cx);
                    } else {
                        explorer.end_splitter_drag(cx);
                    }
                } else if explorer.drag.is_some() {
                    if ev.pressed_button == Some(MouseButton::Left) {
                        explorer.update_drag(pointer, ev.modifiers.control, cx);
                    } else {
                        explorer.cancel_drag(cx);
                    }
                }
            }))"""
assert s.count(a) == 1
s = s.replace(a, b)

a = """                cx.listener(|explorer, _: &MouseUpEvent, _, cx| {
                    explorer.end_splitter_drag(cx);
                }),"""
b = """                cx.listener(|explorer, _: &MouseUpEvent, _, cx| {
                    explorer.end_splitter_drag(cx);
                    explorer.finish_drag(cx);
                }),"""
assert s.count(a) == 2
s = s.replace(a, b)

io.open(p, 'w', encoding='utf-8', newline='').write(s)
print('ok')
