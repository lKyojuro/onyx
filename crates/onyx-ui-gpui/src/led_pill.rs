//! Hardware-inspired 7x7 warm-amber LED dot-matrix status pill.
//!
//! Styled after `docs/design-inspo/01-dot-matrix-thinking-pill.png`:
//! calm single-dot standby when idle, sweeping warm-amber wave + subtle
//! typography shimmer when background scanning/loading is active.

use crate::theme;
use gpui_kit::{IntoElement, ParentElement, Styled, div, px, rgb};

/// Side length of the square LED matrix.
pub const MATRIX_DIM: usize = 7;

/// Computes the 0..=4 intensity for cell `(row, col)` at animation `frame`.
#[must_use]
pub fn dot_intensity(row: usize, col: usize, frame: usize, active: bool) -> u8 {
    if !active {
        // Calm tactile standby: warm center pilot dot, faint inner ring.
        if row == 3 && col == 3 {
            return 3;
        }
        let dr = row.abs_diff(3);
        let dc = col.abs_diff(3);
        if dr <= 1 && dc <= 1 {
            return 1;
        }
        return 0;
    }

    // Active animation cycles between a pulsing perimeter/center pattern
    // and a diagonal warm-amber wave sweep (inspired by 01-dot-matrix-thinking-pill.png).
    let phase = frame % 16;
    if phase < 4 {
        let on_edge = row == 0 || row == MATRIX_DIM - 1 || col == 0 || col == MATRIX_DIM - 1;
        if row == 3 && col == 3 {
            return 4;
        }
        if on_edge {
            let corner_dist = (row + col + phase) % 3;
            return match corner_dist {
                0 => 3,
                1 => 2,
                _ => 1,
            };
        }
        return 0;
    }

    let wave_pos = (phase - 4) as isize;
    let diag = (row + col) as isize;
    let dist = (diag - wave_pos).abs();
    match dist {
        0 => 4,
        1 => 3,
        2 => 2,
        3 => 1,
        _ => 0,
    }
}

/// Renders the obsidian LED dot-matrix status pill.
pub fn render_led_status_pill(label: &str, frame: usize, active: bool) -> impl IntoElement {
    let mut matrix_col = div().flex().flex_col().gap(px(1.5));

    for row in 0..MATRIX_DIM {
        let mut matrix_row = div().flex().flex_row().gap(px(1.5));
        for col in 0..MATRIX_DIM {
            let intensity = dot_intensity(row, col, frame, active);
            matrix_row = matrix_row.child(
                div()
                    .w(px(2.5))
                    .h(px(2.5))
                    .rounded(px(0.75))
                    .bg(theme::led_color(intensity)),
            );
        }
        matrix_col = matrix_col.child(matrix_row);
    }

    // Subtle typography shimmer across characters while active.
    let chars: Vec<char> = label.chars().collect();
    let shimmer_idx = if active && !chars.is_empty() {
        Some(frame % (chars.len() + 4))
    } else {
        None
    };

    let mut label_row = div().flex().flex_row().items_center().text_xs();
    for (idx, ch) in chars.iter().enumerate() {
        let color = match shimmer_idx {
            Some(pos) => {
                let dist = idx.abs_diff(pos);
                match dist {
                    0 => rgb(0xffd08a).into(),
                    1 => rgb(0xd9a15b).into(),
                    2 => rgb(0xa38564).into(),
                    _ => theme::text_secondary(),
                }
            }
            None => theme::text_secondary(),
        };
        label_row = label_row.child(div().text_color(color).child(ch.to_string()));
    }

    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(12.0))
        .py(px(6.0))
        .rounded_full()
        .bg(theme::bg_pill())
        .border_1()
        .border_color(if active {
            rgb(0x3a2612).into()
        } else {
            theme::border_control()
        })
        .child(matrix_col)
        .child(label_row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_matrix_lights_center_pilot_dot() {
        assert_eq!(dot_intensity(3, 3, 0, false), 3);
        assert_eq!(dot_intensity(0, 0, 0, false), 0);
        assert_eq!(dot_intensity(2, 3, 0, false), 1);
    }

    #[test]
    fn active_matrix_produces_peak_amber_dots() {
        let has_peak = (0..16).any(|frame| {
            (0..MATRIX_DIM).any(|r| (0..MATRIX_DIM).any(|c| dot_intensity(r, c, frame, true) == 4))
        });
        assert!(has_peak);
    }
}
