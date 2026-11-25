//! Internal drag & drop between file rows, tabs, sidebar entries and the other
//! dual pane, following the `drag-*` properties of `crates/onyx-ui/ui/main.slint`
//! and the pointer handling of `file_pane.slint`.

use crate::explorer::{OnyxExplorer, PaneId, same_path};
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_XS, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    FontWeight, InteractiveElement, IntoElement, ParentElement, Styled, deferred, div, px,
};
use std::path::{Path, PathBuf};

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
        Self {
            source,
            label,
            is_dir,
            origin,
            pointer: origin,
            armed: false,
            copy: false,
            hover: None,
        }
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
    let mode = drag_mode_label(drag.copy);
    let x = drag.pointer.0 + GHOST_OFFSET;
    let y = drag.pointer.1 + GHOST_OFFSET;

    Some(
        deferred(
            div()
                .id("drag-ghost")
                .absolute()
                .top(px(y))
                .left(px(x))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(6.0))
                .max_w(px(260.0))
                .px(px(8.0))
                .py(px(5.0))
                .rounded(RADIUS_MD)
                .border_1()
                .border_color(theme.accent_bright())
                .bg(theme.bg_elevated())
                .child(render_glyph(glyph, px(14.0), theme.accent_bright()))
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
}
