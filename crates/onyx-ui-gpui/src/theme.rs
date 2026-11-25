//! Obsidian & LED Dot-Matrix design tokens for Onyx's GPUI prototype.

use gpui_kit::{Hsla, rgb, rgba};

/// Deep matte obsidian primary background (`#080808`).
pub fn bg_obsidian() -> Hsla {
    rgb(0x080808).into()
}

/// Secondary matte graphite surface (`#0e0e0e`) for sidebar and top chrome.
pub fn bg_surface() -> Hsla {
    rgb(0x0e0e0e).into()
}

/// Elevated hardware panel surface (`#141414`) for cards, pills, and headers.
pub fn bg_elevated() -> Hsla {
    rgb(0x141414).into()
}

/// Recessed pill interior (`#050505`) matching the LED thinking-pill aesthetic.
pub fn bg_pill() -> Hsla {
    rgb(0x050505).into()
}

/// Hover highlight for interactive rows and buttons (`#181818`).
pub fn bg_hover() -> Hsla {
    rgb(0x181818).into()
}

/// Selected file row surface with subtle warm-amber undertone.
pub fn bg_selected() -> Hsla {
    rgba(0xff9d2e1a).into()
}

/// Subtle structural hairline border (`#1e1e1e`).
pub fn border_subtle() -> Hsla {
    rgb(0x1e1e1e).into()
}

/// Crisp active/control border (`#2c2c2c`).
pub fn border_control() -> Hsla {
    rgb(0x2c2c2c).into()
}

/// Primary crisp foreground text (`#ebebeb`).
pub fn text_primary() -> Hsla {
    rgb(0xebebeb).into()
}

/// Secondary label text (`#929292`).
pub fn text_secondary() -> Hsla {
    rgb(0x929292).into()
}

/// Muted metadata / disabled text (`#5a5a5a`).
pub fn text_muted() -> Hsla {
    rgb(0x5a5a5a).into()
}

/// Folder badge & accent amber (`#f0a43c`).
pub fn folder_amber() -> Hsla {
    rgb(0xf0a43c).into()
}

/// Warm-amber LED intensity levels (0 = unlit, 4 = peak incandescent).
pub fn led_color(intensity: u8) -> Hsla {
    match intensity {
        0 => rgb(0x1b1510).into(),
        1 => rgb(0x4a290b).into(),
        2 => rgb(0x9c5312).into(),
        3 => rgb(0xe8871e).into(),
        _ => rgb(0xffc15e).into(),
    }
}

/// Error accent for inaccessible paths.
pub fn status_error() -> Hsla {
    rgb(0xe05d44).into()
}

/// Subtle dot-matrix accent dot color for header texture.
pub fn matrix_dot() -> Hsla {
    rgb(0x1c1c1c).into()
}
