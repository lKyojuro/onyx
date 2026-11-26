//! Per-pane multi-selection, marquee (rubber-band) drag selection, and inline
//! rename helpers for the file panes.

use crate::explorer::{OnyxExplorer, PaneId};
use gpui_kit::{InteractiveElement, IntoElement, Styled, deferred, div, px};
use std::collections::BTreeSet;
use std::ops::Range;
use std::path::PathBuf;

/// Pointer travel in logical pixels after which an empty-area press becomes a marquee drag.
pub const MARQUEE_THRESHOLD_PX: f32 = 4.0;
const MARQUEE_LAYER: usize = 30;

/// Per-pane multi-selection state with deterministic index ordering, range anchor,
/// and active/lead index (matching `selected_index`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaneSelection {
    selected: BTreeSet<usize>,
    anchor: Option<usize>,
    lead: Option<usize>,
}

impl PaneSelection {
    /// Clears all selected entries and resets anchor/lead.
    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.lead = None;
    }

    /// Replaces the selection with a single index `idx` (plain click / arrow nav).
    pub fn select_single(&mut self, idx: usize, len: usize) {
        self.selected.clear();
        if idx < len {
            self.selected.insert(idx);
            self.anchor = Some(idx);
            self.lead = Some(idx);
        } else {
            self.anchor = None;
            self.lead = None;
        }
    }

    /// Toggles `idx` in the selection (`Ctrl+Click`).
    pub fn toggle(&mut self, idx: usize, len: usize) {
        if idx >= len {
            return;
        }
        if self.selected.contains(&idx) {
            self.selected.remove(&idx);
            if self.lead == Some(idx) {
                self.lead = self.selected.iter().next_back().copied();
            }
            if self.anchor == Some(idx) {
                self.anchor = self.lead;
            }
        } else {
            self.selected.insert(idx);
            self.anchor = Some(idx);
            self.lead = Some(idx);
        }
    }

    /// Selects a contiguous range from the anchor to `idx` (`Shift+Click`).
    /// When `additive` is `true` (`Ctrl+Shift+Click`), preserves existing selections outside the range.
    pub fn select_range(&mut self, idx: usize, len: usize, additive: bool) {
        if idx >= len || len == 0 {
            return;
        }
        let anchor = self.anchor.filter(|&a| a < len).unwrap_or(idx);
        let lo = anchor.min(idx);
        let hi = anchor.max(idx);
        if !additive {
            self.selected.clear();
        }
        for i in lo..=hi {
            self.selected.insert(i);
        }
        self.anchor = Some(anchor);
        self.lead = Some(idx);
    }

    /// Selects every entry in `0..len` (`Ctrl+A`).
    pub fn select_all(&mut self, len: usize) {
        self.selected.clear();
        if len == 0 {
            self.anchor = None;
            self.lead = None;
            return;
        }
        for i in 0..len {
            self.selected.insert(i);
        }
        self.anchor = Some(0);
        self.lead = self.lead.filter(|&i| i < len).or(Some(0));
    }

    /// Applies a live marquee hit set, optionally unioned with `base` (`Ctrl+Drag`).
    pub fn apply_marquee(
        &mut self,
        hits: impl IntoIterator<Item = usize>,
        base: &BTreeSet<usize>,
        additive: bool,
    ) {
        self.selected.clear();
        if additive {
            self.selected.extend(base.iter().copied());
        }
        let mut first_hit = None;
        let mut last_hit = None;
        for idx in hits {
            if first_hit.is_none() {
                first_hit = Some(idx);
            }
            last_hit = Some(idx);
            self.selected.insert(idx);
        }
        self.lead = last_hit.or_else(|| self.selected.iter().next_back().copied());
        self.anchor = first_hit.or(self.lead);
    }

    #[must_use]
    pub fn contains(&self, idx: usize) -> bool {
        self.selected.contains(&idx)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.selected.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    #[must_use]
    pub fn lead(&self) -> Option<usize> {
        self.lead
    }

    #[cfg(test)]
    #[must_use]
    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    #[must_use]
    pub fn indices(&self) -> &BTreeSet<usize> {
        &self.selected
    }
}

/// Axis-aligned rectangle in logical window pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectF32 {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl RectF32 {
    #[must_use]
    pub fn from_points(a: (f32, f32), b: (f32, f32)) -> Self {
        Self {
            left: a.0.min(b.0),
            top: a.1.min(b.1),
            right: a.0.max(b.0),
            bottom: a.1.max(b.1),
        }
    }

    #[must_use]
    pub fn from_origin_size(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            left: x,
            top: y,
            right: x + w.max(0.0),
            bottom: y + h.max(0.0),
        }
    }

    #[must_use]
    pub fn width(self) -> f32 {
        (self.right - self.left).max(0.0)
    }

    #[must_use]
    pub fn height(self) -> f32 {
        (self.bottom - self.top).max(0.0)
    }

    #[must_use]
    pub fn clamp_to(self, bounds: Self) -> Option<Self> {
        let left = self.left.max(bounds.left);
        let top = self.top.max(bounds.top);
        let right = self.right.min(bounds.right);
        let bottom = self.bottom.min(bounds.bottom);
        (right > left && bottom > top).then_some(Self {
            left,
            top,
            right,
            bottom,
        })
    }

    #[must_use]
    pub fn intersects(self, other: Self) -> bool {
        self.left < other.right
            && self.right > other.left
            && self.top < other.bottom
            && self.bottom > other.top
    }
}

/// Live state of a marquee (rubber-band) selection drag on a file pane.
#[derive(Debug, Clone)]
pub struct MarqueeState {
    pub pane: PaneId,
    pub origin: (f32, f32),
    pub current: (f32, f32),
    pub active: bool,
    pub additive: bool,
    pub base_selection: BTreeSet<usize>,
}

impl MarqueeState {
    #[must_use]
    pub fn new(
        pane: PaneId,
        origin: (f32, f32),
        additive: bool,
        base_selection: BTreeSet<usize>,
    ) -> Self {
        Self {
            pane,
            origin,
            current: origin,
            active: false,
            additive,
            base_selection,
        }
    }

    /// Updates the pointer position and activates the marquee once travel exceeds threshold.
    pub fn update(&mut self, pointer: (f32, f32)) -> bool {
        self.current = pointer;
        if !self.active {
            let dx = pointer.0 - self.origin.0;
            let dy = pointer.1 - self.origin.1;
            if (dx * dx + dy * dy).sqrt() >= MARQUEE_THRESHOLD_PX {
                self.active = true;
            }
        }
        self.active
    }

    #[must_use]
    pub fn rect(&self) -> RectF32 {
        RectF32::from_points(self.origin, self.current)
    }
}

/// Returns the entry indices whose Details rows intersect `marquee`.
#[must_use]
pub fn marquee_hit_test_details(
    marquee: RectF32,
    list_bounds: RectF32,
    scroll_offset: f32,
    row_height: f32,
    entry_count: usize,
) -> Vec<usize> {
    if entry_count == 0 || row_height <= 0.0 {
        return Vec::new();
    }
    let Some(clamped) = marquee.clamp_to(list_bounds) else {
        return Vec::new();
    };
    let content_top = (clamped.top - list_bounds.top + scroll_offset).max(0.0);
    let content_bottom = (clamped.bottom - list_bounds.top + scroll_offset).max(0.0);
    if content_bottom <= content_top {
        return Vec::new();
    }
    let first = (content_top / row_height).floor().max(0.0) as usize;
    let last = ((content_bottom - f32::EPSILON) / row_height)
        .floor()
        .max(0.0) as usize;
    if first >= entry_count {
        return Vec::new();
    }
    (first..=last.min(entry_count - 1)).collect()
}

/// Returns the entry indices whose Grid cards intersect `marquee`.
#[allow(clippy::too_many_arguments)]
#[must_use]
pub fn marquee_hit_test_grid(
    marquee: RectF32,
    grid_bounds: RectF32,
    scroll_offset: f32,
    cols: usize,
    row_height: f32,
    pad_x: f32,
    gap_x: f32,
    entry_count: usize,
) -> Vec<usize> {
    if entry_count == 0 || cols == 0 || row_height <= 0.0 {
        return Vec::new();
    }
    let Some(clamped) = marquee.clamp_to(grid_bounds) else {
        return Vec::new();
    };
    let inner_w =
        (grid_bounds.width() - pad_x * 2.0 - gap_x * (cols.saturating_sub(1) as f32)).max(1.0);
    let card_w = inner_w / cols as f32;
    let content_top = (clamped.top - grid_bounds.top + scroll_offset).max(0.0);
    let content_bottom = (clamped.bottom - grid_bounds.top + scroll_offset).max(0.0);
    let local_left = clamped.left - grid_bounds.left;
    let local_right = clamped.right - grid_bounds.left;
    let marquee_local = RectF32 {
        left: local_left,
        top: content_top,
        right: local_right,
        bottom: content_bottom,
    };

    let mut hits = Vec::new();
    for idx in 0..entry_count {
        let row = idx / cols;
        let col = idx % cols;
        let card_left = pad_x + col as f32 * (card_w + gap_x);
        let card_top = row as f32 * row_height + 6.0;
        let card_rect = RectF32::from_origin_size(card_left, card_top, card_w, row_height - 12.0);
        if marquee_local.intersects(card_rect) {
            hits.push(idx);
        }
    }
    hits
}

/// Renders the translucent accent marquee box above the active pane while dragging.
pub fn render_marquee_overlay(view: &OnyxExplorer) -> Option<impl IntoElement> {
    let marquee = view.marquee.as_ref()?;
    if !marquee.active {
        return None;
    }
    let rect = match view.pane_content_bounds(marquee.pane) {
        Some(bounds) => marquee.rect().clamp_to(bounds)?,
        None => marquee.rect(),
    };
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return None;
    }
    let theme = view.theme;
    Some(
        deferred(
            div()
                .id("onyx-marquee-box")
                .absolute()
                .left(px(rect.left))
                .top(px(rect.top))
                .w(px(rect.width()))
                .h(px(rect.height()))
                .rounded(px(2.0))
                .bg(theme.accent_subtle())
                .border_1()
                .border_color(theme.accent_bright()),
        )
        .with_priority(MARQUEE_LAYER),
    )
}

/// Active inline rename session in Details view.
#[derive(Debug, Clone)]
pub struct InlineRenameState {
    pub pane: PaneId,
    pub path: PathBuf,
    pub original_name: String,
}

/// Computes the byte range of the filename stem to pre-select during inline rename.
///
/// - Directories select the entire name (e.g. `v1.2.3` -> `0..6`).
/// - Files select up to the last `.` when a non-empty stem and extension both exist
///   (e.g. `report.pdf` -> `0..6`, `archive.tar.gz` -> `0..11`).
/// - Dotfiles with a single leading dot (`.gitignore`), extensionless files (`Makefile`),
///   or trailing-dot names (`foo.`) select the entire name.
#[must_use]
pub fn filename_stem_range(name: &str, is_dir: bool) -> Range<usize> {
    if is_dir || name.is_empty() {
        return 0..name.len();
    }
    if let Some(dot) = name.rfind('.')
        && dot > 0
        && dot + 1 < name.len()
    {
        return 0..dot;
    }
    0..name.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_click_toggles_items_and_updates_lead() {
        let mut sel = PaneSelection::default();
        sel.select_single(1, 5);
        assert_eq!(sel.len(), 1);
        assert!(sel.contains(1));
        assert_eq!(sel.lead(), Some(1));

        sel.toggle(3, 5);
        assert_eq!(sel.len(), 2);
        assert!(sel.contains(1) && sel.contains(3));
        assert_eq!(sel.lead(), Some(3));
        assert_eq!(sel.anchor(), Some(3));

        // Toggling off the lead falls back to the highest remaining index.
        sel.toggle(3, 5);
        assert_eq!(sel.len(), 1);
        assert!(sel.contains(1));
        assert!(!sel.contains(3));
        assert_eq!(sel.lead(), Some(1));

        // Toggling off the last item empties the selection.
        sel.toggle(1, 5);
        assert!(sel.is_empty());
        assert_eq!(sel.lead(), None);
    }

    #[test]
    fn shift_click_selects_range_from_anchor_forward_and_backward() {
        let mut sel = PaneSelection::default();
        sel.select_single(2, 6);

        // Extend forward: 2..=5
        sel.select_range(5, 6, false);
        let forward: Vec<usize> = sel.indices().iter().copied().collect();
        assert_eq!(forward, vec![2, 3, 4, 5]);
        assert_eq!(sel.anchor(), Some(2));
        assert_eq!(sel.lead(), Some(5));

        // Extend backward from the same anchor: 0..=2
        sel.select_range(0, 6, false);
        let backward: Vec<usize> = sel.indices().iter().copied().collect();
        assert_eq!(backward, vec![0, 1, 2]);
        assert_eq!(sel.anchor(), Some(2));
        assert_eq!(sel.lead(), Some(0));
    }

    #[test]
    fn shift_click_without_anchor_selects_clicked_item() {
        let mut sel = PaneSelection::default();
        sel.select_range(3, 6, false);
        let items: Vec<usize> = sel.indices().iter().copied().collect();
        assert_eq!(items, vec![3]);
        assert_eq!(sel.anchor(), Some(3));
        assert_eq!(sel.lead(), Some(3));
    }

    #[test]
    fn ctrl_a_selects_all_and_handles_empty_list() {
        let mut sel = PaneSelection::default();
        sel.select_all(4);
        let all: Vec<usize> = sel.indices().iter().copied().collect();
        assert_eq!(all, vec![0, 1, 2, 3]);
        assert_eq!(sel.lead(), Some(0));

        sel.select_all(0);
        assert!(sel.is_empty());
        assert_eq!(sel.lead(), None);
    }

    #[test]
    fn marquee_hit_test_details_intersects_rows_with_scroll_offset() {
        let list_bounds = RectF32::from_origin_size(200.0, 100.0, 500.0, 350.0);
        // Scroll offset 70px = 2 rows down (row height 35px).
        // Drag from y=110 to y=180 -> content y=80..150 -> rows 2, 3, 4.
        let marquee = RectF32::from_points((250.0, 110.0), (450.0, 180.0));
        let hits = marquee_hit_test_details(marquee, list_bounds, 70.0, 35.0, 20);
        assert_eq!(hits, vec![2, 3, 4]);
    }

    #[test]
    fn marquee_hit_test_details_returns_empty_outside_bounds() {
        let list_bounds = RectF32::from_origin_size(200.0, 100.0, 500.0, 350.0);
        let outside = RectF32::from_points((10.0, 10.0), (150.0, 90.0));
        assert!(marquee_hit_test_details(outside, list_bounds, 0.0, 35.0, 10).is_empty());
    }

    #[test]
    fn marquee_hit_test_grid_selects_cards_in_box() {
        let grid_bounds = RectF32::from_origin_size(100.0, 100.0, 460.0, 400.0);
        // 3 columns, row height 134px, pad_x 10, gap_x 8.
        // Drag across top-left two cards (indices 0 and 1).
        let marquee = RectF32::from_points((120.0, 120.0), (320.0, 190.0));
        let hits = marquee_hit_test_grid(marquee, grid_bounds, 0.0, 3, 134.0, 10.0, 8.0, 6);
        assert_eq!(hits, vec![0, 1]);
    }

    #[test]
    fn marquee_apply_replaces_or_adds_to_base_selection() {
        let mut sel = PaneSelection::default();
        sel.select_single(0, 6);
        let base = sel.indices().clone();

        sel.apply_marquee([2, 3], &base, false);
        assert_eq!(
            sel.indices().iter().copied().collect::<Vec<_>>(),
            vec![2, 3]
        );

        sel.apply_marquee([4, 5], &base, true);
        assert_eq!(
            sel.indices().iter().copied().collect::<Vec<_>>(),
            vec![0, 4, 5]
        );
    }

    #[test]
    fn filename_stem_range_handles_extensions_dotfiles_dirs_and_unicode() {
        assert_eq!(filename_stem_range("report.pdf", false), 0..6);
        assert_eq!(filename_stem_range("archive.tar.gz", false), 0..11);
        assert_eq!(filename_stem_range(".gitignore", false), 0..10);
        assert_eq!(filename_stem_range(".env.local", false), 0..4);
        assert_eq!(filename_stem_range("Makefile", false), 0..8);
        assert_eq!(filename_stem_range("trailing.", false), 0..9);
        assert_eq!(filename_stem_range("v1.2.3", true), 0..6);
        let unicode = "日本語.txt";
        let range = filename_stem_range(unicode, false);
        assert_eq!(&unicode[range], "日本語");
    }
}
