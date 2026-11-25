//! Eased ("smooth") scrolling: pure offset physics plus the window range math
//! used by the hand-rolled, virtualized details list.
//!
//! GPUI's built-in `uniform_list` owns its scroll offset privately, so it can
//! only jump item by item. A plain `ScrollHandle` bound with [`track_scroll`]
//! exposes `set_offset`, which lets a wheel notch glide into place over several
//! frames - the behaviour Slint's `Flickable` gave us for free.
//!
//! The physics mirror Slint's `Flickable` in the `fluent` style we ship (no
//! bounce, hard clamp): a wheel notch covers its distance in a fixed
//! [`WHEEL_SCROLL_DURATION`] along Flutter's power curve `1 - (1 - t)^2.358`
//! (`i-slint-core`, `simulations/android.rs`), and a notch that arrives while a
//! glide is still running absorbs the remaining distance of that glide, so a
//! fast wheel spin becomes one continuous motion instead of stacking up.
//!
//! [`track_scroll`]: gpui_kit::Styled

use std::time::{Duration, Instant};

use gpui_kit::ScrollDelta;

/// Height of one details-list row in pixels.
pub const DETAILS_ROW_HEIGHT: f32 = 33.0;

/// Viewport height assumed on the very first paint, before the tracked handle
/// has reported its bounds. Rendering a few extra rows is harmless because the
/// container clips them.
pub const VIEWPORT_FALLBACK_PX: f32 = 900.0;

/// Time a wheel notch takes to cover its distance, measured from the moment the
/// event arrived. Slint's `Flickable` uses the same 180 ms for wheel scrolling.
pub const WHEEL_SCROLL_DURATION: Duration = Duration::from_millis(180);

/// Time a programmatic reveal takes, e.g. keeping the selected row in view while
/// navigating with the keyboard. Slint uses 300 ms for `scroll-to` there.
pub const SCROLL_TO_DURATION: Duration = Duration::from_millis(300);

/// Exponent of Flutter's clamping-scroll power curve, i.e.
/// `log10(0.78) / log10(0.9)`, the deceleration rate Slint uses for wheel
/// scrolling. Lower values start slower and end flatter.
const WHEEL_DECAY: f32 = 2.358_2;

/// Pixels scrolled per wheel line when the platform reports line deltas.
const WHEEL_LINE_PX: f32 = 53.0;

/// Pixel offset of a vertical wheel event, positive when scrolling down.
#[must_use]
pub fn wheel_pixels(delta: &ScrollDelta, line_height: f32) -> f32 {
    match delta {
        ScrollDelta::Lines(lines) => {
            let unit = if line_height > 0.0 {
                line_height
            } else {
                WHEEL_LINE_PX
            };
            -lines.y * unit
        }
        ScrollDelta::Pixels(pixels) => -f32::from(pixels.y),
    }
}

/// Flutter's clamping-scroll curve: fast start, long tail, exactly `1.0` at
/// `t == 1.0` so the glide ends on a hard stop instead of crawling in.
fn ease_decay(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powf(WHEEL_DECAY)
}

/// One fixed-duration offset animation, sampled against wall-clock time so a
/// late frame catches up instead of stretching the motion.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Glide {
    from: f32,
    to: f32,
    duration: Duration,
    started: Instant,
}

impl Glide {
    fn progress(&self, now: Instant) -> f32 {
        if self.duration.is_zero() {
            return 1.0;
        }
        (now.saturating_duration_since(self.started).as_secs_f32() / self.duration.as_secs_f32())
            .clamp(0.0, 1.0)
    }

    /// Eased offset `now`, in pixels from the top.
    fn at(&self, now: Instant) -> f32 {
        self.from + (self.to - self.from) * ease_decay(self.progress(now))
    }

    /// Distance left to travel, negative when gliding upwards.
    fn remaining(&self, now: Instant) -> f32 {
        self.to - self.at(now)
    }

    fn running(&self, now: Instant) -> bool {
        self.progress(now) < 1.0
    }
}

/// Eased vertical scroll offset in pixels, `0.0` at the top.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct SmoothScroll {
    offset: f32,
    max: f32,
    glide: Option<Glide>,
}

impl SmoothScroll {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Current (eased) offset in pixels.
    #[must_use]
    pub fn offset(&self) -> f32 {
        self.offset
    }

    /// Updates the scrollable range, keeping the offset and any running glide
    /// inside it.
    pub fn set_max(&mut self, max: f32) {
        self.max = max.max(0.0);
        self.offset = self.offset.clamp(0.0, self.max);
        if let Some(glide) = self.glide.as_mut() {
            glide.from = glide.from.clamp(0.0, self.max);
            glide.to = glide.to.clamp(0.0, self.max);
        }
    }

    /// Adds a wheel delta (positive scrolls down) at `now`.
    ///
    /// Whatever the running glide has left to travel is carried into the new
    /// one, so fast wheel spins neither lose distance nor restart from a stale
    /// offset.
    pub fn wheel(&mut self, delta_px: f32, now: Instant) {
        let carry = self.glide.map_or(0.0, |glide| glide.remaining(now));
        self.glide_to(
            self.offset_at(now) + carry + delta_px,
            now,
            WHEEL_SCROLL_DURATION,
        );
    }

    /// Glides to `target`, clamped to the scrollable range.
    pub fn scroll_to(&mut self, target: f32, now: Instant) {
        self.glide_to(target, now, SCROLL_TO_DURATION);
    }

    /// Jumps without animation (folder change, view switch, reduced motion).
    pub fn jump_to(&mut self, target: f32) {
        self.glide = None;
        self.offset = target.clamp(0.0, self.max);
    }

    /// Whether a glide still has distance to cover at `now`.
    #[must_use]
    pub fn is_animating(&self, now: Instant) -> bool {
        self.glide.is_some_and(|glide| glide.running(now))
    }

    /// Advances the glide to `now`. Returns `true` while it is still animating.
    pub fn step(&mut self, now: Instant) -> bool {
        let Some(glide) = self.glide else {
            return false;
        };
        self.offset = glide.at(now).clamp(0.0, self.max);
        if glide.running(now) {
            return true;
        }
        self.glide = None;
        self.offset = glide.to.clamp(0.0, self.max);
        false
    }

    /// Offset the running glide has reached at `now`; the settled offset when
    /// nothing is running.
    fn offset_at(&self, now: Instant) -> f32 {
        self.glide
            .map_or(self.offset, |glide| glide.at(now))
            .clamp(0.0, self.max)
    }

    fn glide_to(&mut self, target: f32, now: Instant, duration: Duration) {
        let from = self.offset_at(now);
        let to = target.clamp(0.0, self.max);
        if duration.is_zero() || (to - from).abs() < f32::EPSILON {
            self.jump_to(to);
            return;
        }
        self.offset = from;
        self.glide = Some(Glide {
            from,
            to,
            duration,
            started: now,
        });
    }
}

/// Index range of the rows worth rendering for a viewport at `offset`.
///
/// Returns half-open `start..end` clamped to `count`; one extra row of
/// headroom keeps the edges filled while the offset eases between frames.
#[must_use]
pub fn visible_rows(
    offset: f32,
    viewport_height: f32,
    row_height: f32,
    count: usize,
) -> std::ops::Range<usize> {
    if count == 0 || row_height <= 0.0 {
        return 0..0;
    }
    let offset = offset.max(0.0);
    let first = (offset / row_height).floor().max(0.0) as usize;
    let start = first.min(count);
    let visible = (viewport_height.max(0.0) / row_height).ceil() as usize;
    let end = start.saturating_add(visible).saturating_add(2).min(count);
    start..end
}

/// Scrollable height of `count` fixed-height rows.
#[must_use]
pub fn content_height(count: usize, row_height: f32) -> f32 {
    count as f32 * row_height
}

/// Offset that keeps row `index` fully visible inside `viewport_height`.
#[must_use]
pub fn keep_visible(
    offset: f32,
    index: usize,
    viewport_height: f32,
    row_height: f32,
    max: f32,
) -> f32 {
    let top = index as f32 * row_height;
    let bottom = top + row_height;
    let candidate = if top < offset {
        top
    } else if bottom > offset + viewport_height {
        bottom - viewport_height
    } else {
        offset
    };
    candidate.clamp(0.0, max.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame time of a 60 Hz display, used to sample the curve like the app.
    const FRAME: Duration = Duration::from_micros(16_667);

    fn scroll_with_max(max: f32) -> (SmoothScroll, Instant) {
        let start = Instant::now();
        let mut scroll = SmoothScroll::new();
        scroll.set_max(max);
        (scroll, start)
    }

    #[test]
    fn wheel_lines_and_pixels_map_to_the_same_direction() {
        let lines = ScrollDelta::Lines(gpui_kit::point(0.0_f32, -3.0_f32));
        assert!(wheel_pixels(&lines, 20.0) > 0.0);
        let pixels = ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.0), gpui_kit::px(-60.0)));
        assert_eq!(wheel_pixels(&pixels, 20.0), 60.0);
        assert_eq!(wheel_pixels(&lines, 20.0), 60.0);
    }

    #[test]
    fn wheel_glide_covers_its_distance_in_the_wheel_duration() {
        let (mut scroll, start) = scroll_with_max(1_000.0);
        scroll.wheel(60.0, start);
        assert_eq!(scroll.offset(), 0.0, "a fresh notch starts where we are");

        assert!(scroll.step(start + WHEEL_SCROLL_DURATION / 2));
        let half = scroll.offset();
        assert!(
            (47.0..=50.0).contains(&half),
            "about 80% of the notch is covered at half time: {half}"
        );

        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION));
        assert_eq!(scroll.offset(), 60.0, "the glide ends exactly on target");
    }

    #[test]
    fn wheel_curve_starts_slower_than_an_exponential_lerp() {
        // Slint reaches ~20% in the first 60 Hz frame; the exponential lerp we
        // used before reached ~35% and then crawled.
        let (mut scroll, start) = scroll_with_max(1_000.0);
        scroll.wheel(100.0, start);
        assert!(scroll.step(start + FRAME));
        let first_frame = scroll.offset();
        assert!(
            (18.0..=23.0).contains(&first_frame),
            "first frame should move about a fifth of the notch: {first_frame}"
        );
    }

    #[test]
    fn a_late_frame_catches_up_instead_of_stretching_the_motion() {
        let (mut scroll, start) = scroll_with_max(1_000.0);
        scroll.wheel(120.0, start);
        // Simulate a 3-frame stall, then a single frame: the offset must reflect
        // the elapsed time, not one frame of progress.
        assert!(scroll.step(start + FRAME * 3));
        assert!(
            scroll.offset() > 50.0,
            "elapsed time drives the curve: {}",
            scroll.offset()
        );
        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION));
        assert_eq!(scroll.offset(), 120.0);
    }

    #[test]
    fn a_new_notch_absorbs_the_remaining_distance_of_the_running_glide() {
        let (mut scroll, start) = scroll_with_max(1_000.0);
        scroll.wheel(60.0, start);
        assert!(scroll.step(start + WHEEL_SCROLL_DURATION / 2));

        // The second notch lands while the first is still gliding: the rest of
        // the first notch plus the new one travel together, so fast wheel spins
        // keep their distance instead of restarting every 16 ms.
        scroll.wheel(60.0, start + WHEEL_SCROLL_DURATION / 2);
        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION * 2));
        assert_eq!(scroll.offset(), 120.0);
    }

    #[test]
    fn wheel_accumulates_while_clamped_to_content() {
        let (mut scroll, start) = scroll_with_max(100.0);
        scroll.wheel(40.0, start);
        scroll.wheel(40.0, start);
        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION));
        assert_eq!(scroll.offset(), 80.0);

        scroll.wheel(-500.0, start);
        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION));
        assert_eq!(scroll.offset(), 0.0);

        scroll.wheel(1_000.0, start);
        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION));
        assert_eq!(scroll.offset(), 100.0);
    }

    #[test]
    fn shrinking_content_clamps_offset_and_target() {
        let (mut scroll, start) = scroll_with_max(500.0);
        scroll.jump_to(400.0);
        scroll.wheel(60.0, start);
        scroll.set_max(120.0);
        assert_eq!(scroll.offset(), 120.0);
        assert!(!scroll.step(start + WHEEL_SCROLL_DURATION));
        assert_eq!(scroll.offset(), 120.0);
    }

    #[test]
    fn reveal_glides_and_reports_animation_state() {
        let (mut scroll, start) = scroll_with_max(1_000.0);
        scroll.scroll_to(300.0, start);
        assert!(scroll.is_animating(start));
        assert!(scroll.step(start + SCROLL_TO_DURATION / 2));
        assert!(!scroll.step(start + SCROLL_TO_DURATION));
        assert_eq!(scroll.offset(), 300.0);
        assert!(!scroll.is_animating(start + SCROLL_TO_DURATION));
    }

    #[test]
    fn jump_to_settles_immediately() {
        let (mut scroll, start) = scroll_with_max(500.0);
        scroll.wheel(60.0, start);
        scroll.jump_to(42.0);
        assert_eq!(scroll.offset(), 42.0);
        assert!(!scroll.is_animating(start));
        assert!(!scroll.step(start));
        assert_eq!(scroll.offset(), 42.0);
    }

    #[test]
    fn visible_rows_cover_the_viewport_with_headroom() {
        let range = visible_rows(0.0, 330.0, DETAILS_ROW_HEIGHT, 1_000);
        assert_eq!(range.start, 0);
        assert_eq!(range.end, 12);

        let range = visible_rows(DETAILS_ROW_HEIGHT * 10.0, 330.0, DETAILS_ROW_HEIGHT, 1_000);
        assert_eq!(range.start, 10);
        assert_eq!(range.end, 22);

        let range = visible_rows(0.0, 330.0, DETAILS_ROW_HEIGHT, 3);
        assert_eq!(range, 0..3);
        assert_eq!(visible_rows(0.0, 100.0, DETAILS_ROW_HEIGHT, 0), 0..0);
    }

    #[test]
    fn keep_visible_scrolls_only_when_needed() {
        let row = DETAILS_ROW_HEIGHT;
        assert_eq!(keep_visible(0.0, 2, 330.0, row, 1_000.0), 0.0);
        assert_eq!(
            keep_visible(0.0, 20, 330.0, row, 1_000.0),
            21.0 * row - 330.0
        );
        assert_eq!(keep_visible(300.0, 2, 330.0, row, 1_000.0), 2.0 * row);
        assert_eq!(keep_visible(0.0, 20, 330.0, row, 100.0), 100.0);
    }

    #[test]
    fn content_height_scales_with_rows() {
        assert_eq!(content_height(10, DETAILS_ROW_HEIGHT), 330.0);
        assert_eq!(content_height(0, DETAILS_ROW_HEIGHT), 0.0);
    }
}
