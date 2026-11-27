//! Modal overlays and floating toasts matching `crates/onyx-ui/ui/components/overlays.slint`:
//! - `PromptDialogModal` (New Folder, Rename, Permanent Delete confirmation)
//! - `ConflictDialogModal` (Copy/Move destination filename collision resolver)
//! - `OperationProgressCard` (bottom-right floating progress card with loading bar and Cancel)

use crate::components::a11y;
use crate::components::{ActionButtonProps, fluent_action_button};
use crate::explorer::OnyxExplorer;
use crate::icons::{OnyxGlyph, render_glyph};
use crate::theme::{FONT_LG, FONT_MD, FONT_SM, FONT_XS, RADIUS_LG, RADIUS_MD, RADIUS_SM};
use gpui_kit::{
    Context, FontWeight, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ParentElement,
    Role, StatefulInteractiveElement, Styled, Toggled, component::input::Input, deferred, div, px,
};
use onyx_core::{format_bytes, normalize_display_path};
use onyx_operations::{ConflictPolicy, ConflictResolution, OperationKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PROGRESS_CARD_WIDTH: f32 = 368.0;
const PROGRESS_CARD_PADDING: f32 = 16.0;
const PROGRESS_TRACK_WIDTH: f32 = PROGRESS_CARD_WIDTH - PROGRESS_CARD_PADDING * 2.0;

// ---- Operation Progress helpers ----

/// Determinate or indeterminate visual state for the operation loading bar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum ProgressMode {
    Determinate { fraction: f32, percent: u32 },
    Indeterminate,
}

/// Computes the loading bar mode from byte counts (preferred) or item counts,
/// falling back to indeterminate when both totals are 0.
#[must_use]
pub(crate) fn compute_progress_mode(
    bytes_done: u64,
    bytes_total: u64,
    items_done: usize,
    items_total: usize,
) -> ProgressMode {
    let fraction = if bytes_total > 0 {
        (bytes_done as f64 / bytes_total as f64).clamp(0.0, 1.0) as f32
    } else if items_total > 0 {
        (items_done as f64 / items_total as f64).clamp(0.0, 1.0) as f32
    } else {
        return ProgressMode::Indeterminate;
    };
    let percent = (fraction * 100.0).round() as u32;
    ProgressMode::Determinate { fraction, percent }
}

/// Formats the item and byte counter shown beneath the loading bar.
#[must_use]
pub(crate) fn format_operation_counter(
    bytes_done: u64,
    bytes_total: u64,
    items_done: usize,
    items_total: usize,
    error: Option<&str>,
) -> String {
    if let Some(msg) = error
        && !msg.is_empty()
    {
        return msg.to_string();
    }
    let item_word = if items_total == 1 { "item" } else { "items" };
    match (bytes_total > 0, items_total > 0) {
        (true, true) => format!(
            "{items_done} / {items_total} {item_word} · {} / {}",
            format_bytes(bytes_done),
            format_bytes(bytes_total)
        ),
        (true, false) => format!(
            "{} / {}",
            format_bytes(bytes_done),
            format_bytes(bytes_total)
        ),
        (false, true) => format!("{items_done} of {items_total} {item_word}"),
        (false, false) => "Preparing…".to_string(),
    }
}

/// Computes `(left_offset_px, width_px)` for a smooth sliding indeterminate loading bar.
#[must_use]
pub(crate) fn indeterminate_bar_geometry(anim_frame: usize, track_width: f32) -> (f32, f32) {
    const CYCLE_FRAMES: usize = 36;
    let safe_track = track_width.max(1.0);
    let bar_width = (safe_track * 0.34).clamp(24.0, safe_track);
    let phase = (anim_frame % CYCLE_FRAMES) as f32 / CYCLE_FRAMES as f32;
    let eased = 0.5 - 0.5 * (phase * std::f32::consts::PI).cos();
    let offset = -bar_width + eased * (safe_track + bar_width);
    (offset, bar_width)
}

/// Maps an [`OperationKind`] to a concise title for the progress card header.
#[must_use]
pub(crate) const fn operation_kind_title(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Copy => "Copying",
        OperationKind::Move => "Moving",
        OperationKind::Rename => "Renaming",
        OperationKind::CreateDir => "Creating folder",
        OperationKind::Duplicate => "Duplicating",
        OperationKind::Recycle => "Recycling",
        OperationKind::PermanentDelete => "Deleting",
        OperationKind::CreateArchive => "Compressing",
        OperationKind::ExtractArchive => "Extracting",
    }
}

/// Maps an optional [`OperationKind`] to a representative glyph for the progress card.
#[must_use]
const fn operation_glyph(kind: Option<OperationKind>) -> OnyxGlyph {
    match kind {
        Some(OperationKind::Copy | OperationKind::Duplicate) => OnyxGlyph::Copy,
        Some(OperationKind::Move) => OnyxGlyph::SwapHorizontal,
        Some(OperationKind::Recycle | OperationKind::PermanentDelete) => OnyxGlyph::Trash,
        Some(OperationKind::CreateArchive | OperationKind::ExtractArchive) => OnyxGlyph::Archive,
        Some(OperationKind::Rename) => OnyxGlyph::Rename,
        Some(OperationKind::CreateDir) => OnyxGlyph::FolderPlus,
        None => OnyxGlyph::Refresh,
    }
}

// ---- Copy/Move Conflict Dialog state machine & collision detection ----

/// Kind of file transfer guarded by the conflict dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferKind {
    Copy,
    Move,
}

impl TransferKind {
    #[must_use]
    pub(crate) const fn verb(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Move => "Move",
        }
    }

    #[must_use]
    pub(crate) const fn dialog_title(self) -> &'static str {
        match self {
            Self::Copy => "Copy conflict",
            Self::Move => "Move conflict",
        }
    }
}

/// A single destination filename collision detected before starting a transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConflictItem {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub file_name: String,
    pub is_dir: bool,
}

/// Fully resolved transfer ready to be dispatched to `onyx-operations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedTransfer {
    pub kind: TransferKind,
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    pub default_policy: ConflictPolicy,
    /// Per-destination decisions when the user resolves multiple conflicts individually.
    pub per_destination: HashMap<PathBuf, ConflictResolution>,
    pub from_cut_clipboard: bool,
}

/// Result of a user action in the conflict dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConflictOutcome {
    /// More conflicts remain; keep the modal open on the next item.
    NextConflict,
    /// All conflicts are resolved; dispatch the transfer to `onyx-operations`.
    Dispatch(ResolvedTransfer),
    /// User cancelled the transfer.
    Cancelled,
}

/// Maps a [`ConflictPolicy`] to the corresponding [`ConflictResolution`].
#[must_use]
pub(crate) const fn conflict_resolution_from_policy(policy: ConflictPolicy) -> ConflictResolution {
    match policy {
        ConflictPolicy::Overwrite => ConflictResolution::Overwrite,
        ConflictPolicy::Skip => ConflictResolution::Skip,
        ConflictPolicy::AutoRename => ConflictResolution::KeepBoth,
        ConflictPolicy::Cancel | ConflictPolicy::Fail => ConflictResolution::Cancel,
    }
}

/// Detects destination filename collisions for `sources` copied/moved into `destination`.
///
/// Must be called off the UI thread (e.g. inside `tokio::task::spawn_blocking`).
#[must_use]
pub(crate) fn detect_conflicts(sources: &[PathBuf], destination: &Path) -> Vec<ConflictItem> {
    detect_conflicts_with(sources, destination, |target| {
        std::fs::symlink_metadata(target)
            .ok()
            .map(|meta| meta.is_dir())
    })
}

/// Pure testable collision detector parameterized by an existence/is-dir probe.
#[must_use]
pub(crate) fn detect_conflicts_with<F>(
    sources: &[PathBuf],
    destination: &Path,
    mut probe_target: F,
) -> Vec<ConflictItem>
where
    F: FnMut(&Path) -> Option<bool>,
{
    let mut conflicts = Vec::new();
    for source in sources {
        let Some(file_name) = source.file_name() else {
            continue;
        };
        let target = destination.join(file_name);
        if let Some(is_dir) = probe_target(&target) {
            conflicts.push(ConflictItem {
                source: source.clone(),
                destination: target,
                file_name: file_name.to_string_lossy().into_owned(),
                is_dir,
            });
        }
    }
    conflicts
}

/// Active modal state for resolving Copy/Move destination collisions.
#[derive(Debug, Clone)]
pub(crate) struct ConflictDialogState {
    pub kind: TransferKind,
    pub sources: Vec<PathBuf>,
    pub destination: PathBuf,
    pub from_cut_clipboard: bool,
    pub conflicts: Vec<ConflictItem>,
    pub current_index: usize,
    pub apply_to_all: bool,
    pub decisions: HashMap<PathBuf, ConflictResolution>,
    /// Set on creation so the next `render` pass moves keyboard focus into the modal.
    pub needs_focus: bool,
}

impl ConflictDialogState {
    #[must_use]
    pub(crate) fn new(
        kind: TransferKind,
        sources: Vec<PathBuf>,
        destination: PathBuf,
        conflicts: Vec<ConflictItem>,
        from_cut_clipboard: bool,
    ) -> Self {
        Self {
            kind,
            sources,
            destination,
            from_cut_clipboard,
            conflicts,
            current_index: 0,
            apply_to_all: false,
            decisions: HashMap::new(),
            needs_focus: true,
        }
    }

    #[must_use]
    pub(crate) fn current_conflict(&self) -> Option<&ConflictItem> {
        self.conflicts.get(self.current_index)
    }

    #[must_use]
    pub(crate) fn has_multiple_conflicts(&self) -> bool {
        self.conflicts.len() > 1
    }

    #[must_use]
    pub(crate) fn remaining_conflicts(&self) -> usize {
        self.conflicts.len().saturating_sub(self.current_index)
    }

    pub(crate) fn toggle_apply_to_all(&mut self) {
        if self.has_multiple_conflicts() {
            self.apply_to_all = !self.apply_to_all;
        }
    }

    /// Applies the user's chosen policy to the current conflict (or all remaining conflicts
    /// when `apply_to_all` is checked) and advances the modal state machine.
    pub(crate) fn choose(&mut self, policy: ConflictPolicy) -> ConflictOutcome {
        if matches!(policy, ConflictPolicy::Cancel) {
            return ConflictOutcome::Cancelled;
        }
        let resolution = conflict_resolution_from_policy(policy);

        if self.apply_to_all || self.remaining_conflicts() <= 1 {
            // If earlier items were resolved individually, record the final decision for the
            // remaining items too so the resolver has an exact decision for every collision.
            if !self.decisions.is_empty() {
                for item in self.conflicts.iter().skip(self.current_index) {
                    self.decisions.insert(item.destination.clone(), resolution);
                }
            }
            return ConflictOutcome::Dispatch(ResolvedTransfer {
                kind: self.kind,
                sources: self.sources.clone(),
                destination: self.destination.clone(),
                default_policy: policy,
                per_destination: self.decisions.clone(),
                from_cut_clipboard: self.from_cut_clipboard,
            });
        }

        if let Some(current) = self.conflicts.get(self.current_index) {
            self.decisions
                .insert(current.destination.clone(), resolution);
        }
        self.current_index += 1;

        if self.current_index >= self.conflicts.len() {
            ConflictOutcome::Dispatch(ResolvedTransfer {
                kind: self.kind,
                sources: self.sources.clone(),
                destination: self.destination.clone(),
                default_policy: policy,
                per_destination: self.decisions.clone(),
                from_cut_clipboard: self.from_cut_clipboard,
            })
        } else {
            ConflictOutcome::NextConflict
        }
    }
}

// ---- Rendered overlays ----

/// Renders the centered Prompt Dialog modal (New Folder, Rename, Permanent Delete confirmation).
pub fn render_prompt_dialog(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let prompt = view.prompt_state.as_ref()?;
    let theme = view.theme;
    let is_danger = prompt.is_danger;
    let show_input = prompt.show_input;
    let title = prompt.title.clone();
    let message = prompt.message.clone();
    // Name + explanation for the dialog and its input, kept before the visible
    // text nodes take ownership of the strings.
    let a11y_title = title.clone();
    let description = if message.is_empty() {
        title.clone()
    } else {
        format!("{title}. {message}")
    };
    let confirm_label = if prompt.confirm_label.is_empty() {
        "Confirm".to_string()
    } else {
        prompt.confirm_label.clone()
    };

    // Tab order inside the dialog: Cancel, then Confirm.
    let ring = &view.modals.prompt;
    ring.begin();
    let cancel_focus = ring.slot();
    let confirm_focus = ring.slot();

    let mut header = div().flex().flex_row().items_center().gap(px(8.0));
    if is_danger {
        header = header.child(render_glyph(OnyxGlyph::Warning, px(18.0), theme.danger()));
    }
    header = header.child(
        div()
            .flex_1()
            .text_size(FONT_LG)
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if is_danger {
                theme.danger()
            } else {
                theme.text_primary()
            })
            .child(title),
    );

    let mut body = div()
        .flex()
        .flex_col()
        .w_full()
        .p(px(20.0))
        .gap(px(14.0))
        .child(header);

    if !message.is_empty() {
        body = body.child(
            div()
                .text_size(FONT_SM)
                .text_color(theme.text_secondary())
                .child(message),
        );
    }

    if show_input && let Some(prompt_input) = view.prompt_input.as_ref() {
        body = body.child(
            div()
                .id("prompt-dialog-input-box")
                .flex()
                .flex_row()
                .items_center()
                .w_full()
                .h(px(32.0))
                .px(px(8.0))
                .rounded(RADIUS_MD)
                .border_1()
                .border_color(theme.border_focus())
                .bg(theme.bg_input())
                .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key == "escape" {
                        explorer.cancel_prompt(window, cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .text_size(FONT_MD)
                        .text_color(theme.text_primary())
                        .child(
                            Input::new(prompt_input)
                                .appearance(false)
                                .aria_label(a11y_title.clone())
                                .accessibility_id("prompt-name-input"),
                        ),
                ),
        );
    }

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "prompt-cancel-btn".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some("Cancel"),
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: false,
                    min_width: px(72.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| explorer.cancel_prompt(window, cx)),
            )
            .track_focus(&cancel_focus)
            .focus_visible(|style| a11y::focus_ring_style(theme, style))
            .aria_keyshortcuts("Escape"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "prompt-confirm-btn".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some(&confirm_label),
                    active: false,
                    primary: true,
                    danger: is_danger,
                    enabled: true,
                    compact: false,
                    min_width: px(84.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| explorer.confirm_prompt(window, cx)),
            )
            .track_focus(&confirm_focus)
            .focus_visible(|style| a11y::focus_ring_style(theme, style))
            .aria_keyshortcuts("Enter"),
        );

    body = body.child(footer);

    let card = div()
        .id("prompt-dialog-card")
        .role(if is_danger {
            Role::AlertDialog
        } else {
            Role::Dialog
        })
        .aria_label(a11y_title)
        .aria_description(description)
        .relative()
        .flex()
        .flex_col()
        .w(px(440.0))
        .max_w(px(440.0))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(if is_danger {
            theme.danger()
        } else {
            theme.border_strong()
        })
        .bg(theme.bg_elevated())
        .track_focus(&view.prompt_focus_handle)
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_key_down(cx.listener(move |explorer, ev: &KeyDownEvent, window, cx| {
            match ev.keystroke.key.as_str() {
                "escape" => {
                    explorer.cancel_prompt(window, cx);
                    cx.stop_propagation();
                }
                "enter" if !show_input => {
                    explorer.confirm_prompt(window, cx);
                    cx.stop_propagation();
                }
                _ => {}
            }
        }))
        .child(
            div()
                .absolute()
                .top(px(1.0))
                .left(px(12.0))
                .right(px(12.0))
                .h(px(1.0))
                .bg(theme.border_specular()),
        )
        .child(body);

    Some(
        deferred(
            div()
                .id("prompt-dialog-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|explorer, _, window, cx| {
                        explorer.cancel_prompt(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(card),
        )
        .with_priority(30),
    )
}

/// Renders the keyboard-accessible Copy/Move Conflict Dialog modal.
pub fn render_conflict_dialog(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let dialog = view.conflict_dialog.as_ref()?;
    let conflict = dialog.current_conflict()?;
    let theme = view.theme;

    let title = dialog.kind.dialog_title();
    let multi = dialog.has_multiple_conflicts();
    let total_conflicts = dialog.conflicts.len();
    let current_number = dialog.current_index + 1;
    let remaining = dialog.remaining_conflicts();
    let apply_to_all = dialog.apply_to_all;
    let file_name = conflict.file_name.clone();
    let is_dir = conflict.is_dir;
    let dest_display = normalize_display_path(&dialog.destination);
    let item_kind_word = if is_dir { "folder" } else { "file" };

    let description = format!(
        "The destination already contains a {item_kind_word} named {file_name}. Choose whether to replace it, skip it, keep both, or cancel."
    );

    // Tab order inside the dialog:
    // [Apply to all (when multiple)], Cancel, Keep Both, Skip, Replace.
    let ring = &view.modals.conflict;
    ring.begin();
    let apply_all_focus = multi.then(|| ring.slot());
    let cancel_focus = ring.slot();
    let keep_both_focus = ring.slot();
    let skip_focus = ring.slot();
    let replace_focus = ring.slot();

    let mut header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .child(render_glyph(
                    OnyxGlyph::Warning,
                    px(18.0),
                    theme.accent_bright(),
                ))
                .child(
                    div()
                        .text_size(FONT_LG)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_primary())
                        .child(title),
                ),
        );

    if multi {
        header = header.child(
            div()
                .px(px(8.0))
                .py(px(2.0))
                .rounded(RADIUS_SM)
                .bg(theme.bg_badge())
                .border_1()
                .border_color(theme.border_subtle())
                .text_size(FONT_XS)
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.text_secondary())
                .child(format!("Conflict {current_number} of {total_conflicts}")),
        );
    }

    let message = div()
        .text_size(FONT_SM)
        .text_color(theme.text_secondary())
        .child(format!(
            "A {item_kind_word} with the same name already exists in {dest_display}."
        ));

    let item_card = div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .w_full()
        .px(px(12.0))
        .py(px(10.0))
        .rounded(RADIUS_MD)
        .border_1()
        .border_color(theme.border_subtle())
        .bg(theme.bg_surface())
        .child(render_glyph(
            if is_dir {
                OnyxGlyph::Folder
            } else {
                OnyxGlyph::File
            },
            px(18.0),
            theme.accent_bright(),
        ))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_MD)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text_primary())
                .child(file_name),
        );

    let mut body = div()
        .flex()
        .flex_col()
        .w_full()
        .p(px(20.0))
        .gap(px(14.0))
        .child(header)
        .child(message)
        .child(item_card);

    if let Some(checkbox_focus) = apply_all_focus {
        let checkbox_label = format!("Apply to all ({remaining} remaining)");
        let checkbox_row = div()
            .id("conflict-apply-all-checkbox")
            .role(Role::CheckBox)
            .aria_label(checkbox_label.clone())
            .aria_toggled(if apply_to_all {
                Toggled::True
            } else {
                Toggled::False
            })
            .flex()
            .flex_row()
            .items_center()
            .gap(px(8.0))
            .px(px(6.0))
            .py(px(5.0))
            .rounded(RADIUS_SM)
            .border_1()
            .border_color(gpui_kit::transparent_black())
            .cursor_pointer()
            .hover(move |s| s.bg(theme.bg_hover()))
            .track_focus(&checkbox_focus)
            .focus_visible(move |style| a11y::focus_ring_style(theme, style))
            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                cx.stop_propagation();
            })
            .on_click(cx.listener(|explorer, _, _, cx| {
                explorer.toggle_conflict_apply_to_all(cx);
                cx.stop_propagation();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_center()
                    .w(px(16.0))
                    .h(px(16.0))
                    .rounded(px(3.0))
                    .border_1()
                    .border_color(if apply_to_all {
                        theme.accent_border()
                    } else {
                        theme.border_strong()
                    })
                    .bg(if apply_to_all {
                        theme.accent_effective()
                    } else {
                        theme.bg_input()
                    })
                    .text_size(FONT_XS)
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme.text_on_accent())
                    .child(if apply_to_all { "✓" } else { "" }),
            )
            .child(
                div()
                    .text_size(FONT_SM)
                    .text_color(theme.text_primary())
                    .child(checkbox_label),
            );
        body = body.child(checkbox_row);
    }

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_end()
        .gap(px(8.0))
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "conflict-cancel-btn".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some("Cancel"),
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: false,
                    min_width: px(72.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| explorer.cancel_conflict_dialog(window, cx)),
            )
            .track_focus(&cancel_focus)
            .focus_visible(|style| a11y::focus_ring_style(theme, style))
            .aria_keyshortcuts("Escape"),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "conflict-keep-both-btn".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some("Keep Both"),
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: false,
                    min_width: px(88.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| {
                    explorer.resolve_conflict(ConflictPolicy::AutoRename, window, cx);
                }),
            )
            .track_focus(&keep_both_focus)
            .focus_visible(|style| a11y::focus_ring_style(theme, style)),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "conflict-skip-btn".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some("Skip"),
                    active: false,
                    primary: false,
                    danger: false,
                    enabled: true,
                    compact: false,
                    min_width: px(68.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| {
                    explorer.resolve_conflict(ConflictPolicy::Skip, window, cx);
                }),
            )
            .track_focus(&skip_focus)
            .focus_visible(|style| a11y::focus_ring_style(theme, style)),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "conflict-replace-btn".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some("Replace"),
                    active: false,
                    primary: true,
                    danger: false,
                    enabled: true,
                    compact: false,
                    min_width: px(84.0),
                },
                theme,
                cx.listener(|explorer, _, window, cx| {
                    explorer.resolve_conflict(ConflictPolicy::Overwrite, window, cx);
                }),
            )
            .track_focus(&replace_focus)
            .focus_visible(|style| a11y::focus_ring_style(theme, style)),
        );

    body = body.child(footer);

    let card = div()
        .id("conflict-dialog-card")
        .role(Role::AlertDialog)
        .aria_label(title)
        .aria_description(description)
        .relative()
        .flex()
        .flex_col()
        .w(px(480.0))
        .max_w(px(480.0))
        .rounded(RADIUS_LG)
        .border_1()
        .border_color(theme.border_strong())
        .bg(theme.bg_elevated())
        .on_mouse_down(MouseButton::Left, |_, _, cx| {
            cx.stop_propagation();
        })
        .on_key_down(cx.listener(|explorer, ev: &KeyDownEvent, window, cx| {
            if ev.keystroke.key == "escape" {
                explorer.cancel_conflict_dialog(window, cx);
                cx.stop_propagation();
            }
        }))
        .child(
            div()
                .absolute()
                .top(px(1.0))
                .left(px(12.0))
                .right(px(12.0))
                .h(px(1.0))
                .bg(theme.border_specular()),
        )
        .child(body);

    Some(
        deferred(
            div()
                .id("conflict-dialog-backdrop")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme.bg_backdrop())
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|explorer, _, window, cx| {
                        explorer.cancel_conflict_dialog(window, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(card),
        )
        .with_priority(31),
    )
}

/// Renders the bottom-right floating Operation Progress card with a visible loading bar and Cancel.
pub fn render_operation_progress_toast(
    view: &OnyxExplorer,
    cx: &mut Context<OnyxExplorer>,
) -> Option<impl IntoElement> {
    let op = view.active_operation.as_ref()?;
    let theme = view.theme;

    let title = if op.cancelling {
        "Cancelling…"
    } else if op.title.is_empty() {
        "Running operation"
    } else {
        op.title
    };

    let mode = compute_progress_mode(
        op.bytes_done,
        op.bytes_total,
        op.completed_items,
        op.total_items,
    );
    let badge_text = match mode {
        ProgressMode::Determinate { percent, .. } => format!("{percent}%"),
        ProgressMode::Indeterminate => "Working".to_string(),
    };
    let counter_text = format_operation_counter(
        op.bytes_done,
        op.bytes_total,
        op.completed_items,
        op.total_items,
        op.error.as_deref(),
    );
    let status_aria = match mode {
        ProgressMode::Determinate { percent, .. } => format!("{title}, {percent} percent"),
        ProgressMode::Indeterminate => format!("{title}, in progress"),
    };

    let header = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .flex_1()
                .min_w(px(0.0))
                .gap(px(7.0))
                .child(render_glyph(
                    operation_glyph(op.kind),
                    px(15.0),
                    theme.accent_bright(),
                ))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_size(FONT_SM)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme.text_primary())
                        .child(title.to_string()),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .px(px(6.0))
                .py(px(1.0))
                .rounded(RADIUS_SM)
                .bg(theme.accent_subtle())
                .text_size(FONT_XS)
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.accent_bright())
                .child(badge_text),
        );

    let mut content = div()
        .flex()
        .flex_col()
        .w_full()
        .p(px(PROGRESS_CARD_PADDING))
        .gap(px(9.0))
        .child(header);

    if !op.current_item.is_empty() {
        content = content.child(
            div()
                .w_full()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_secondary())
                .child(op.current_item.clone()),
        );
    }

    let progress_track = match mode {
        ProgressMode::Determinate { fraction, .. } => {
            let fill_width = px(PROGRESS_TRACK_WIDTH * fraction);
            div()
                .id("op-progress-bar")
                .role(Role::ProgressIndicator)
                .aria_label(title.to_string())
                .aria_numeric_value(f64::from(fraction) * 100.0)
                .aria_min_numeric_value(0.0)
                .aria_max_numeric_value(100.0)
                .relative()
                .w_full()
                .h(px(8.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.border_subtle())
                .bg(theme.bg_input())
                .overflow_hidden()
                .child(
                    div()
                        .h_full()
                        .w(fill_width)
                        .rounded(px(4.0))
                        .bg(theme.accent_effective()),
                )
        }
        ProgressMode::Indeterminate => {
            let (offset, bar_width) =
                indeterminate_bar_geometry(view.anim_frame, PROGRESS_TRACK_WIDTH);
            div()
                .id("op-progress-bar")
                .role(Role::ProgressIndicator)
                .aria_label(title.to_string())
                .relative()
                .w_full()
                .h(px(8.0))
                .rounded(px(4.0))
                .border_1()
                .border_color(theme.border_subtle())
                .bg(theme.bg_input())
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .top(px(0.0))
                        .bottom(px(0.0))
                        .left(px(offset))
                        .w(px(bar_width))
                        .rounded(px(4.0))
                        .bg(theme.accent_effective()),
                )
        }
    };

    let footer = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_between()
        .gap(px(8.0))
        .child(
            div()
                .flex_1()
                .min_w(px(0.0))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(FONT_XS)
                .text_color(theme.text_muted())
                .child(counter_text),
        )
        .child(
            fluent_action_button(
                ActionButtonProps {
                    id: "op-toast-cancel".into(),
                    glyph: None,
                    trailing_glyph: None,
                    label: Some(if op.cancelling {
                        "Cancelling"
                    } else {
                        "Cancel"
                    }),
                    active: false,
                    primary: false,
                    danger: true,
                    enabled: !op.cancelling,
                    compact: true,
                    min_width: px(60.0),
                },
                theme,
                cx.listener(|explorer, _, _, cx| explorer.cancel_operation(cx)),
            )
            .aria_label("Cancel file operation"),
        );

    content = content.child(progress_track).child(footer);

    Some(
        deferred(
            div()
                .id("op-progress-toast")
                .role(Role::Status)
                .aria_label(status_aria)
                .absolute()
                .right(px(16.0))
                .bottom(px(42.0))
                .w(px(PROGRESS_CARD_WIDTH))
                .rounded(RADIUS_LG)
                .border_1()
                .border_color(theme.border_strong())
                .bg(theme.bg_elevated())
                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .absolute()
                        .top(px(1.0))
                        .left(px(12.0))
                        .right(px(12.0))
                        .h(px(1.0))
                        .bg(theme.border_specular()),
                )
                .child(content),
        )
        .with_priority(25),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn progress_mode_prefers_bytes_falls_back_to_items_and_handles_zero_totals() {
        // Determinate by bytes.
        assert_eq!(
            compute_progress_mode(512, 1024, 0, 4),
            ProgressMode::Determinate {
                fraction: 0.5,
                percent: 50,
            }
        );
        // Clamps above 100%.
        assert_eq!(
            compute_progress_mode(2048, 1024, 1, 1),
            ProgressMode::Determinate {
                fraction: 1.0,
                percent: 100,
            }
        );
        // Falls back to items when byte total is 0.
        assert_eq!(
            compute_progress_mode(0, 0, 3, 4),
            ProgressMode::Determinate {
                fraction: 0.75,
                percent: 75,
            }
        );
        // Indeterminate when both totals are 0.
        assert_eq!(
            compute_progress_mode(0, 0, 0, 0),
            ProgressMode::Indeterminate
        );
    }

    #[test]
    fn operation_counter_formats_bytes_items_error_and_preparing() {
        assert_eq!(format_operation_counter(0, 0, 0, 0, None), "Preparing…");
        assert_eq!(format_operation_counter(0, 0, 1, 1, None), "1 of 1 item");
        assert_eq!(format_operation_counter(0, 0, 2, 5, None), "2 of 5 items");
        assert_eq!(
            format_operation_counter(1024, 2048, 1, 3, None),
            format!(
                "1 / 3 items · {} / {}",
                format_bytes(1024),
                format_bytes(2048)
            )
        );
        assert_eq!(
            format_operation_counter(512, 1024, 0, 0, None),
            format!("{} / {}", format_bytes(512), format_bytes(1024))
        );
        assert_eq!(
            format_operation_counter(512, 1024, 1, 2, Some("Permission denied")),
            "Permission denied"
        );
    }

    #[test]
    fn indeterminate_bar_sweeps_across_track() {
        let (start_x, width) = indeterminate_bar_geometry(0, 300.0);
        let (mid_x, mid_w) = indeterminate_bar_geometry(18, 300.0);
        let (end_x, _) = indeterminate_bar_geometry(35, 300.0);
        assert!((width - mid_w).abs() < f32::EPSILON);
        assert!(start_x < 0.0);
        assert!(mid_x > start_x);
        assert!(end_x > mid_x);
    }

    #[test]
    fn detect_conflicts_finds_existing_files_and_folders_in_temp_dir() -> std::io::Result<()> {
        let src_dir = TempDir::new()?;
        let dst_dir = TempDir::new()?;

        let src_file = src_dir.path().join("report.txt");
        let src_unique = src_dir.path().join("unique.txt");
        let src_folder = src_dir.path().join("docs");
        fs::write(&src_file, b"hello")?;
        fs::write(&src_unique, b"world")?;
        fs::create_dir(&src_folder)?;

        // Pre-create colliding file and folder in destination.
        fs::write(dst_dir.path().join("report.txt"), b"old")?;
        fs::create_dir(dst_dir.path().join("docs"))?;

        let conflicts = detect_conflicts(
            &[src_file.clone(), src_unique, src_folder.clone()],
            dst_dir.path(),
        );
        assert_eq!(conflicts.len(), 2);
        assert_eq!(conflicts[0].file_name, "report.txt");
        assert!(!conflicts[0].is_dir);
        assert_eq!(conflicts[0].source, src_file);
        assert_eq!(conflicts[0].destination, dst_dir.path().join("report.txt"));
        assert_eq!(conflicts[1].file_name, "docs");
        assert!(conflicts[1].is_dir);
        Ok(())
    }

    #[test]
    fn conflict_dialog_single_conflict_dispatches_selected_policy() {
        let dest = PathBuf::from(r"C:\Dest");
        let conflict = ConflictItem {
            source: PathBuf::from(r"C:\Src\a.txt"),
            destination: dest.join("a.txt"),
            file_name: "a.txt".to_string(),
            is_dir: false,
        };
        let mut dialog = ConflictDialogState::new(
            TransferKind::Copy,
            vec![conflict.source.clone()],
            dest.clone(),
            vec![conflict],
            false,
        );
        assert!(!dialog.has_multiple_conflicts());
        dialog.toggle_apply_to_all();
        assert!(!dialog.apply_to_all);

        let outcome = dialog.choose(ConflictPolicy::Overwrite);
        assert_eq!(
            outcome,
            ConflictOutcome::Dispatch(ResolvedTransfer {
                kind: TransferKind::Copy,
                sources: vec![PathBuf::from(r"C:\Src\a.txt")],
                destination: dest,
                default_policy: ConflictPolicy::Overwrite,
                per_destination: HashMap::new(),
                from_cut_clipboard: false,
            })
        );
    }

    #[test]
    fn conflict_dialog_multi_conflict_steps_individually_and_supports_apply_to_all() {
        let dest = PathBuf::from(r"C:\Dest");
        let conflicts = vec![
            ConflictItem {
                source: PathBuf::from(r"C:\Src\a.txt"),
                destination: dest.join("a.txt"),
                file_name: "a.txt".to_string(),
                is_dir: false,
            },
            ConflictItem {
                source: PathBuf::from(r"C:\Src\b.txt"),
                destination: dest.join("b.txt"),
                file_name: "b.txt".to_string(),
                is_dir: false,
            },
            ConflictItem {
                source: PathBuf::from(r"C:\Src\c.txt"),
                destination: dest.join("c.txt"),
                file_name: "c.txt".to_string(),
                is_dir: false,
            },
        ];
        let sources: Vec<PathBuf> = conflicts.iter().map(|c| c.source.clone()).collect();

        // Step 1 individually (Skip), then check Apply to all and choose Keep Both for remaining 2.
        let mut dialog = ConflictDialogState::new(
            TransferKind::Move,
            sources.clone(),
            dest.clone(),
            conflicts,
            true,
        );
        assert!(dialog.has_multiple_conflicts());
        assert_eq!(dialog.remaining_conflicts(), 3);

        assert_eq!(
            dialog.choose(ConflictPolicy::Skip),
            ConflictOutcome::NextConflict
        );
        assert_eq!(dialog.current_index, 1);
        assert_eq!(dialog.remaining_conflicts(), 2);

        dialog.toggle_apply_to_all();
        assert!(dialog.apply_to_all);

        let outcome = dialog.choose(ConflictPolicy::AutoRename);
        match outcome {
            ConflictOutcome::Dispatch(resolved) => {
                assert_eq!(resolved.kind, TransferKind::Move);
                assert!(resolved.from_cut_clipboard);
                assert_eq!(resolved.default_policy, ConflictPolicy::AutoRename);
                assert_eq!(
                    resolved.per_destination.get(&dest.join("a.txt")),
                    Some(&ConflictResolution::Skip)
                );
                assert_eq!(
                    resolved.per_destination.get(&dest.join("b.txt")),
                    Some(&ConflictResolution::KeepBoth)
                );
                assert_eq!(
                    resolved.per_destination.get(&dest.join("c.txt")),
                    Some(&ConflictResolution::KeepBoth)
                );
            }
            other => panic!("expected Dispatch, got {other:?}"),
        }
    }

    #[test]
    fn conflict_dialog_cancel_returns_cancelled() {
        let dest = PathBuf::from(r"C:\Dest");
        let conflict = ConflictItem {
            source: PathBuf::from(r"C:\Src\a.txt"),
            destination: dest.join("a.txt"),
            file_name: "a.txt".to_string(),
            is_dir: false,
        };
        let mut dialog = ConflictDialogState::new(
            TransferKind::Copy,
            vec![conflict.source.clone()],
            dest,
            vec![conflict],
            false,
        );
        assert_eq!(
            dialog.choose(ConflictPolicy::Cancel),
            ConflictOutcome::Cancelled
        );
    }
}
