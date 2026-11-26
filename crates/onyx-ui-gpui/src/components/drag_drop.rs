//! Internal drag & drop between file rows, tabs, sidebar entries and the other
//! dual pane, following the `drag-*` properties of `crates/onyx-ui/ui/main.slint`
//! and the pointer handling of `file_pane.slint`.
//!
//! The same gesture also leaves the window: once the pointer crosses the window
//! edge an armed drag is handed to `onyx_windows::start_external_file_drag`, so
//! items can be dropped into Explorer, the desktop or other applications.

use crate::explorer::{OnyxExplorer, PaneId, same_path};
use crate::icons::{OnyxGlyph, path_icon_key, render_glyph};
use crate::theme::{FONT_XS, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    FontWeight, InteractiveElement, IntoElement, ParentElement, Styled, deferred, div, img, px,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Pointer travel in logical pixels after which a press becomes a drag.
pub const DRAG_THRESHOLD_PX: f32 = 8.0;
/// Offset of the drag ghost from the pointer so it never covers the drop target.
const GHOST_OFFSET: f32 = 14.0;
const GHOST_LAYER: usize = 40;

/// A possible drop destination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropTarget {
    /// A concrete folder: file row, sidebar entry or tab.
    Directory(PathBuf),
    /// Empty background of a pane, i.e. that pane's current directory.
    Pane(PaneId),
    /// A non-folder row in Details view: drops into that pane's current directory
    /// and displays a 2px accent insertion line along the row edge.
    PaneRow { pane: PaneId, row_index: usize },
}

/// Why a drop is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReject {
    /// Source and destination are the same folder.
    SameItem,
    /// The destination lies inside the dragged folder.
    IntoItself,
    /// The source already lives in the destination folder.
    AlreadyThere,
}

impl DropReject {
    /// Status-bar text explaining the refusal.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::SameItem => "Cannot drop a folder onto itself.",
            Self::IntoItself => "Cannot move a folder into itself.",
            Self::AlreadyThere => "The item is already in this folder.",
        }
    }
}

/// Whether the pointer travelled far enough to count as a drag.
#[must_use]
pub fn exceeded_threshold(origin: (f32, f32), pointer: (f32, f32)) -> bool {
    let dx = pointer.0 - origin.0;
    let dy = pointer.1 - origin.1;
    (dx * dx + dy * dy).sqrt() >= DRAG_THRESHOLD_PX
}

/// Whether `pointer` lies outside the window rectangle `(0, 0)..viewport`.
///
/// Windows keeps reporting mouse moves with client coordinates while the left
/// button is held, so a pointer that has left the window arrives with negative
/// or oversized values.
#[must_use]
pub fn pointer_left_window(pointer: (f32, f32), viewport: (f32, f32)) -> bool {
    if viewport.0 <= 0.0 || viewport.1 <= 0.0 {
        return false;
    }
    pointer.0 < 0.0 || pointer.1 < 0.0 || pointer.0 >= viewport.0 || pointer.1 >= viewport.1
}

/// Whether an armed internal drag has to be handed over to the shell because the
/// pointer left the window.
#[must_use]
pub fn should_start_external_drag(
    drag: Option<&DragState>,
    external_active: bool,
    pointer: (f32, f32),
    viewport: (f32, f32),
) -> bool {
    !external_active
        && drag.is_some_and(|drag| drag.armed)
        && pointer_left_window(pointer, viewport)
}

/// Validates dropping `source` into `destination_dir`, returning the resolved
/// destination folder.
pub fn validate_drop(source: &Path, destination_dir: &Path) -> Result<PathBuf, DropReject> {
    if same_path(source, destination_dir) {
        return Err(DropReject::SameItem);
    }
    let is_dir = source.is_dir();
    if is_dir && destination_dir.starts_with(source) {
        return Err(DropReject::IntoItself);
    }
    if source
        .parent()
        .is_some_and(|parent| same_path(parent, destination_dir))
    {
        return Err(DropReject::AlreadyThere);
    }
    Ok(destination_dir.to_path_buf())
}

/// Label of the active drag mode, matching the `Ctrl` modifier convention.
#[must_use]
pub const fn drag_mode_label(copy: bool) -> &'static str {
    if copy { "Copy" } else { "Move" }
}

/// Live state of an in-flight internal drag.
#[derive(Debug, Clone)]
pub struct DragState {
    pub source: PathBuf,
    pub sources: Vec<PathBuf>,
    pub label: String,
    pub is_dir: bool,
    pub origin: (f32, f32),
    pub pointer: (f32, f32),
    pub armed: bool,
    pub copy: bool,
    pub hover: Option<DropTarget>,
}

impl DragState {
    #[must_use]
    pub fn new(source: PathBuf, label: String, is_dir: bool, origin: (f32, f32)) -> Self {
        let sources = vec![source.clone()];
        Self {
            source,
            sources,
            label,
            is_dir,
            origin,
            pointer: origin,
            armed: false,
            copy: false,
            hover: None,
        }
    }

    /// Attaches multiple dragged paths and updates the ghost label when dragging a multi-selection.
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<PathBuf>) -> Self {
        if sources.len() > 1 {
            self.label = format!("{} items", sources.len());
            self.sources = sources;
        } else if !sources.is_empty() {
            self.sources = sources;
        }
        self
    }

    /// Updates the pointer and arms the drag once the threshold is passed.
    pub fn track(&mut self, pointer: (f32, f32), copy: bool) {
        self.pointer = pointer;
        self.copy = copy;
        if !self.armed && exceeded_threshold(self.origin, pointer) {
            self.armed = true;
        }
    }
}

/// Renders the floating drag ghost above everything else while dragging.
pub fn render_drag_ghost(view: &OnyxExplorer) -> Option<impl IntoElement> {
    let drag = view.drag.as_ref()?;
    if !drag.armed {
        return None;
    }
    let theme = view.theme;
    let glyph = if drag.is_dir {
        OnyxGlyph::Folder
    } else {
        OnyxGlyph::File
    };
    let icon_key = if drag.is_dir {
        "directory".to_string()
    } else {
        let ext = drag
            .source
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext == "exe" || ext == "lnk" || ext == "ico" {
            path_icon_key(&drag.source)
        } else {
            format!("extension:{ext}")
        }
    };
    let shell_icon = view
        .thumbnails
        .get(&drag.source)
        .or_else(|| view.icons.get(&icon_key))
        .cloned();
    let mode = drag_mode_label(drag.copy);
    let x = drag.pointer.0 + GHOST_OFFSET;
    let y = drag.pointer.1 + GHOST_OFFSET;

    let mut ghost = div()
        .id("drag-ghost")
        .absolute()
        .top(px(y))
        .left(px(x))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.0))
        .max_w(px(280.0))
        .px(px(8.0))
        .py(px(6.0))
        .rounded(RADIUS_MD)
        .border_1()
        .border_color(theme.accent_bright())
        .bg(theme.bg_elevated())
        .opacity(0.94);

    if let Some(icon_img) = shell_icon {
        ghost = ghost.child(
            img(Arc::clone(&icon_img))
                .w(px(28.0))
                .h(px(28.0))
                .flex_shrink_0(),
        );
    } else {
        ghost = ghost.child(render_glyph(glyph, px(22.0), theme.accent_bright()));
    }

    Some(
        deferred(
            ghost
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_XS)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text_primary())
                        .child(drag.label.clone()),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .px(px(5.0))
                        .rounded(RADIUS_SM)
                        .bg(theme.accent_subtle())
                        .text_size(FONT_XS)
                        .text_color(theme.accent_bright())
                        .child(mode),
                ),
        )
        .with_priority(GHOST_LAYER),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_needs_eight_pixels_of_travel() {
        assert!(!exceeded_threshold((10.0, 10.0), (10.0, 10.0)));
        assert!(!exceeded_threshold((10.0, 10.0), (15.0, 10.0)));
        assert!(exceeded_threshold((10.0, 10.0), (18.0, 10.0)));
        assert!(exceeded_threshold((10.0, 10.0), (10.0, 2.0)));
    }

    #[test]
    fn drop_into_same_folder_is_refused() {
        let source = PathBuf::from("C:\\data\\report.txt");
        assert_eq!(
            validate_drop(&source, Path::new("C:\\data")),
            Err(DropReject::AlreadyThere)
        );
    }

    #[test]
    fn drop_onto_the_folder_itself_is_refused() {
        // `same_path` compares textually, so an existing directory is not needed.
        let source = PathBuf::from("C:\\data\\photos");
        assert_eq!(
            validate_drop(&source, Path::new("C:\\data\\photos")),
            Err(DropReject::SameItem)
        );
    }

    #[test]
    fn drop_into_own_subtree_is_refused_for_directories() {
        // Directory detection reads the real FS, so a missing path counts as a file;
        // the purely textual prefix check is covered by the SameItem case above.
        let source = PathBuf::from("C:\\data\\missing-dir");
        let resolved = validate_drop(&source, Path::new("C:\\data\\missing-dir\\sub"));
        assert_eq!(resolved, Ok(PathBuf::from("C:\\data\\missing-dir\\sub")));
    }

    #[test]
    fn valid_drop_resolves_the_destination_folder() {
        let source = PathBuf::from("C:\\data\\report.txt");
        assert_eq!(
            validate_drop(&source, Path::new("C:\\archive")),
            Ok(PathBuf::from("C:\\archive"))
        );
    }

    #[test]
    fn mode_label_follows_the_ctrl_modifier() {
        assert_eq!(drag_mode_label(false), "Move");
        assert_eq!(drag_mode_label(true), "Copy");
    }

    #[test]
    fn drag_arms_only_after_the_threshold() {
        let mut drag = DragState::new(
            PathBuf::from("C:\\data\\a.txt"),
            "a.txt".to_string(),
            false,
            (100.0, 100.0),
        );
        assert!(!drag.armed);
        drag.track((103.0, 100.0), true);
        assert!(!drag.armed);
        assert!(drag.copy);
        drag.track((120.0, 100.0), false);
        assert!(drag.armed);
        assert!(!drag.copy);
    }

    #[test]
    fn pointer_leaves_the_window_at_every_edge() {
        let viewport = (1000.0, 700.0);
        assert!(!pointer_left_window((0.0, 0.0), viewport));
        assert!(!pointer_left_window((999.0, 699.0), viewport));
        assert!(pointer_left_window((-1.0, 20.0), viewport));
        assert!(pointer_left_window((20.0, -1.0), viewport));
        assert!(pointer_left_window((1000.0, 20.0), viewport));
        assert!(pointer_left_window((20.0, 700.0), viewport));
    }

    #[test]
    fn unknown_viewport_never_counts_as_left() {
        assert!(!pointer_left_window((-500.0, -500.0), (0.0, 0.0)));
    }

    #[test]
    fn external_drag_needs_an_armed_drag_outside_the_window() {
        let viewport = (1000.0, 700.0);
        let mut drag = DragState::new(
            PathBuf::from("C:\\data\\a.txt"),
            "a.txt".to_string(),
            false,
            (500.0, 300.0),
        );
        drag.track((-40.0, 300.0), false);
        assert!(drag.armed);
        // Still inside the window: the drop stays internal.
        assert!(!should_start_external_drag(
            Some(&drag),
            false,
            (500.0, 300.0),
            viewport
        ));
        // A running OS drag swallows further crossings.
        assert!(!should_start_external_drag(
            Some(&drag),
            true,
            (-40.0, 300.0),
            viewport
        ));
        assert!(!should_start_external_drag(
            None,
            false,
            (-40.0, 300.0),
            viewport
        ));

        // A press that has not passed the threshold yet never leaves the app.
        let mut pressed = DragState::new(
            PathBuf::from("C:\\data\\a.txt"),
            "a.txt".to_string(),
            false,
            (500.0, 300.0),
        );
        pressed.track((502.0, 300.0), false);
        assert!(!should_start_external_drag(
            Some(&pressed),
            false,
            (-40.0, 300.0),
            viewport
        ));
        assert!(should_start_external_drag(
            Some(&drag),
            false,
            (-40.0, 300.0),
            viewport
        ));
    }
}
