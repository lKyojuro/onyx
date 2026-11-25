//! Resizable workspace splitters with window-relative drag tracking and responsive clamping
//! backed by `onyx-config` bounds (`sidebar`: 180–400px, `dual_pane`: 0.15–0.85, `preview`: 220–480px).

use crate::explorer::OnyxExplorer;
use crate::theme::OnyxTheme;
use gpui_kit::{
    Context, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, Styled,
    div, px,
};
use onyx_config::{
    DEFAULT_PREVIEW_WIDTH_DUAL, DEFAULT_PREVIEW_WIDTH_SINGLE, MIN_PREVIEW_WIDTH, MIN_SIDEBAR_WIDTH,
    clamp_pane_split_ratio, clamp_preview_width, clamp_sidebar_width,
};

/// Logical width of a vertical resize splitter in pixels (matches `splitter.slint`).
pub const SPLITTER_WIDTH: f32 = 6.0;

/// Minimum logical width of a single file pane in pixels (matches `main.slint`).
pub const FILE_PANE_MIN_WIDTH: f32 = 240.0;

/// Identifies which workspace boundary a resize splitter controls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SplitterKind {
    Sidebar,
    DualPane,
    Preview,
}

impl SplitterKind {
    #[must_use]
    const fn element_id(self) -> &'static str {
        match self {
            Self::Sidebar => "splitter-sidebar",
            Self::DualPane => "splitter-dual-pane",
            Self::Preview => "splitter-preview",
        }
    }
}

/// Snapshot captured on splitter mouse-down for jitter-free window-relative dragging
/// and Escape-key cancellation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveSplitterDrag {
    pub kind: SplitterKind,
    pub start_x: f32,
    pub start_sidebar_w: f32,
    pub start_split_ratio: f32,
    pub start_preview_w: f32,
    pub start_preview_pref: Option<f32>,
    pub files_area_w: f32,
}

/// Computed logical widths of workspace regions for the current viewport width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorkspaceLayout {
    pub sidebar_w: f32,
    pub primary_w: f32,
    pub secondary_w: f32,
    pub preview_w: f32,
    pub files_area_w: f32,
}

/// Computes responsive workspace column widths matching `main.slint` (lines 348–406)
/// using the clamping bounds from `onyx-config`.
#[must_use]
pub fn compute_workspace_layout(
    workspace_w: f32,
    sidebar_visible: bool,
    sidebar_pref_w: f32,
    dual_pane: bool,
    split_ratio: f32,
    preview_visible: bool,
    preview_pref_w: Option<f32>,
) -> WorkspaceLayout {
    let workspace_w = if workspace_w.is_finite() {
        workspace_w.max(0.0)
    } else {
        0.0
    };

    let splitters_total_w = (if sidebar_visible { SPLITTER_WIDTH } else { 0.0 })
        + (if dual_pane { SPLITTER_WIDTH } else { 0.0 })
        + (if preview_visible { SPLITTER_WIDTH } else { 0.0 });

    let min_files_total_w = if dual_pane {
        FILE_PANE_MIN_WIDTH * 2.0
    } else {
        FILE_PANE_MIN_WIDTH
    };
    let side_budget_w = (workspace_w - splitters_total_w - min_files_total_w).max(0.0);

    let pref_sidebar_w = if sidebar_visible {
        clamp_sidebar_width(sidebar_pref_w)
    } else {
        0.0
    };

    let raw_pref_preview_w = clamp_preview_width(preview_pref_w).unwrap_or(if dual_pane {
        DEFAULT_PREVIEW_WIDTH_DUAL
    } else {
        DEFAULT_PREVIEW_WIDTH_SINGLE
    });
    let pref_preview_w = if preview_visible {
        raw_pref_preview_w
    } else {
        0.0
    };

    let pref_sides_sum = pref_sidebar_w + pref_preview_w;
    let min_sides_sum = (if sidebar_visible {
        MIN_SIDEBAR_WIDTH
    } else {
        0.0
    }) + (if preview_visible {
        MIN_PREVIEW_WIDTH
    } else {
        0.0
    });
    let sidebar_pref_excess = (pref_sidebar_w - MIN_SIDEBAR_WIDTH).max(0.0);
    let preview_pref_excess = (pref_preview_w - MIN_PREVIEW_WIDTH).max(0.0);
    let total_pref_excess = sidebar_pref_excess + preview_pref_excess;

    let sidebar_w = if !sidebar_visible {
        0.0
    } else if pref_sides_sum <= side_budget_w {
        pref_sidebar_w
    } else if !preview_visible {
        pref_sidebar_w.min(side_budget_w).max(0.0)
    } else if side_budget_w >= min_sides_sum && total_pref_excess > 0.0 {
        MIN_SIDEBAR_WIDTH
            + (side_budget_w - min_sides_sum) * (sidebar_pref_excess / total_pref_excess)
    } else if min_sides_sum > 0.0 {
        side_budget_w * (MIN_SIDEBAR_WIDTH / min_sides_sum)
    } else {
        0.0
    };

    let preview_w = if !preview_visible {
        0.0
    } else if pref_sides_sum <= side_budget_w {
        pref_preview_w
    } else if !sidebar_visible {
        pref_preview_w.min(side_budget_w).max(0.0)
    } else {
        (side_budget_w - sidebar_w).max(0.0)
    };

    let files_area_w = (workspace_w - splitters_total_w - sidebar_w - preview_w).max(0.0);
    let effective_min_pane_w = FILE_PANE_MIN_WIDTH.min(files_area_w / 2.0);
    let clamped_ratio = clamp_pane_split_ratio(split_ratio);

    let primary_w = if dual_pane {
        let max_primary = (files_area_w - effective_min_pane_w).max(effective_min_pane_w);
        (files_area_w * clamped_ratio).clamp(effective_min_pane_w, max_primary)
    } else {
        files_area_w
    };
    let secondary_w = if dual_pane {
        (files_area_w - primary_w).max(0.0)
    } else {
        0.0
    };

    WorkspaceLayout {
        sidebar_w,
        primary_w,
        secondary_w,
        preview_w,
        files_area_w,
    }
}

/// Applies a horizontal drag delta to the sidebar width, clamped to `180..=400px`.
#[must_use]
pub fn apply_sidebar_drag(start_w: f32, delta_x: f32) -> f32 {
    clamp_sidebar_width(start_w + delta_x)
}

/// Applies a horizontal drag delta to the dual-pane split ratio, clamped to `0.15..=0.85`.
#[must_use]
pub fn apply_dual_pane_drag(start_ratio: f32, files_area_w: f32, delta_x: f32) -> f32 {
    if !files_area_w.is_finite() || files_area_w <= 1.0 {
        return clamp_pane_split_ratio(start_ratio);
    }
    clamp_pane_split_ratio(start_ratio + delta_x / files_area_w)
}

/// Applies a horizontal drag delta to the preview pane width, clamped to `220..=480px`.
#[must_use]
pub fn apply_preview_drag(start_w: f32, delta_x: f32, preview_on_left: bool) -> f32 {
    let candidate = if preview_on_left {
        start_w + delta_x
    } else {
        start_w - delta_x
    };
    clamp_preview_width(Some(candidate)).unwrap_or(MIN_PREVIEW_WIDTH)
}

/// Renders a 6px vertical drag splitter matching `ResizeSplitter` in `splitter.slint`.
pub fn render_splitter(
    kind: SplitterKind,
    is_dragging: bool,
    theme: OnyxTheme,
    cx: &mut Context<OnyxExplorer>,
) -> impl IntoElement {
    let hairline_w = if is_dragging { px(2.0) } else { px(1.0) };
    let hairline_bg = if is_dragging {
        theme.accent_bright()
    } else {
        theme.border_subtle()
    };

    div()
        .id(kind.element_id())
        .flex()
        .items_center()
        .justify_center()
        .w(px(SPLITTER_WIDTH))
        .h_full()
        .flex_shrink_0()
        .cursor_col_resize()
        .bg(if is_dragging {
            theme.accent_subtle()
        } else {
            gpui_kit::transparent_black()
        })
        .hover(move |s| s.bg(theme.accent_subtle()))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |explorer, ev: &MouseDownEvent, window, cx| {
                explorer.begin_splitter_drag(kind, ev.position.x.into(), window, cx);
                cx.stop_propagation();
            }),
        )
        .child(div().w(hairline_w).h_full().bg(hairline_bg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use onyx_config::{
        MAX_PANE_SPLIT_RATIO, MAX_PREVIEW_WIDTH, MAX_SIDEBAR_WIDTH, MIN_PANE_SPLIT_RATIO,
        MIN_PREVIEW_WIDTH, MIN_SIDEBAR_WIDTH,
    };

    #[test]
    fn drag_helpers_clamp_to_onyx_config_bounds() {
        assert!((apply_sidebar_drag(226.0, -500.0) - MIN_SIDEBAR_WIDTH).abs() < 1e-4);
        assert!((apply_sidebar_drag(226.0, 500.0) - MAX_SIDEBAR_WIDTH).abs() < 1e-4);
        assert!((apply_sidebar_drag(226.0, 24.0) - 250.0).abs() < 1e-4);

        assert!((apply_dual_pane_drag(0.5, 1000.0, -900.0) - MIN_PANE_SPLIT_RATIO).abs() < 1e-4);
        assert!((apply_dual_pane_drag(0.5, 1000.0, 900.0) - MAX_PANE_SPLIT_RATIO).abs() < 1e-4);
        assert!((apply_dual_pane_drag(0.5, 1000.0, 100.0) - 0.6).abs() < 1e-4);

        assert!((apply_preview_drag(310.0, 500.0, false) - MIN_PREVIEW_WIDTH).abs() < 1e-4);
        assert!((apply_preview_drag(310.0, -500.0, false) - MAX_PREVIEW_WIDTH).abs() < 1e-4);
        assert!((apply_preview_drag(310.0, 50.0, true) - 360.0).abs() < 1e-4);
    }

    #[test]
    fn workspace_layout_distributes_dual_pane_and_sidebar() {
        let layout = compute_workspace_layout(1200.0, true, 240.0, true, 0.5, false, None);
        assert!((layout.sidebar_w - 240.0).abs() < 1e-4);
        assert!((layout.preview_w - 0.0).abs() < 1e-4);
        let expected_files = 1200.0 - 240.0 - SPLITTER_WIDTH * 2.0;
        assert!((layout.files_area_w - expected_files).abs() < 1e-4);
        assert!((layout.primary_w - expected_files * 0.5).abs() < 1e-4);
        assert!((layout.secondary_w - expected_files * 0.5).abs() < 1e-4);
    }

    #[test]
    fn workspace_layout_respects_min_pane_width_under_extreme_ratio() {
        let layout = compute_workspace_layout(800.0, false, 226.0, true, 0.15, false, None);
        assert!(layout.primary_w >= FILE_PANE_MIN_WIDTH);
        assert!(layout.secondary_w >= FILE_PANE_MIN_WIDTH);
    }
}
