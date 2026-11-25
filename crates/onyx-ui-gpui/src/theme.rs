//! Exact Onyx design tokens ported 1:1 from `crates/onyx-ui/ui/tokens.slint`.
//!
//! Provides Dark Graphite (default) and Light Theme palettes with the
//! steel-blue / aurora accent (`#9eafc5`), plus warm-amber LED matrix tokens
//! for the hardware-inspired activity pill.

use gpui_kit::{App, Hsla, Pixels, Window, component::Theme, component::ThemeMode, px, rgb, rgba};

/// Default Onyx steel-blue / aurora accent (`#9eafc5` from `tokens.slint`).
pub const DEFAULT_ACCENT_RGB: u32 = 0x9eafc5;

/// Runtime theme state mirroring `OnyxTheme` in `tokens.slint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnyxTheme {
    pub dark_mode: bool,
    pub accent_rgb: u32,
}

impl Default for OnyxTheme {
    fn default() -> Self {
        Self {
            dark_mode: true,
            accent_rgb: DEFAULT_ACCENT_RGB,
        }
    }
}

impl OnyxTheme {
    /// Creates a theme configured for dark or light mode with the default accent.
    #[allow(dead_code)]
    #[must_use]
    pub const fn new(dark_mode: bool) -> Self {
        Self {
            dark_mode,
            accent_rgb: DEFAULT_ACCENT_RGB,
        }
    }

    /// Synchronizes `gpui-component`'s global theme mode (for `Input`, scrollbars, and window controls).
    pub fn sync_gpui_theme(&self, window: Option<&mut Window>, cx: &mut App) {
        let mode = if self.dark_mode {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        Theme::change(mode, window, cx);
    }

    // ---- Core surfaces (window chrome -> raised panels) ----

    #[must_use]
    pub fn bg_base(self) -> Hsla {
        if self.dark_mode {
            rgb(0x101722).into()
        } else {
            rgb(0xedf1f6).into()
        }
    }

    #[must_use]
    pub fn bg_chrome(self) -> Hsla {
        if self.dark_mode {
            rgb(0x141c29).into()
        } else {
            rgb(0xe4eaf2).into()
        }
    }

    #[must_use]
    pub fn bg_sidebar(self) -> Hsla {
        if self.dark_mode {
            rgb(0x182230).into()
        } else {
            rgb(0xe8edf4).into()
        }
    }

    #[must_use]
    pub fn bg_surface(self) -> Hsla {
        if self.dark_mode {
            rgb(0x202a38).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    #[must_use]
    pub fn bg_surface_alt(self) -> Hsla {
        if self.dark_mode {
            rgb(0x1c2634).into()
        } else {
            rgb(0xf5f7fa).into()
        }
    }

    #[must_use]
    pub fn bg_elevated(self) -> Hsla {
        if self.dark_mode {
            rgb(0x263242).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    #[must_use]
    pub fn bg_hover(self) -> Hsla {
        if self.dark_mode {
            rgb(0x2c394b).into()
        } else {
            rgb(0xdfe7f1).into()
        }
    }

    #[must_use]
    pub fn bg_pressed(self) -> Hsla {
        if self.dark_mode {
            rgb(0x37475c).into()
        } else {
            rgb(0xcfdae8).into()
        }
    }

    #[must_use]
    pub fn bg_selected(self) -> Hsla {
        if self.dark_mode {
            rgb(0x35465d).into()
        } else {
            rgb(0xd5e0ee).into()
        }
    }

    #[must_use]
    pub fn bg_input(self) -> Hsla {
        if self.dark_mode {
            rgb(0x141e2b).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn bg_code(self) -> Hsla {
        if self.dark_mode {
            rgb(0x131d2b).into()
        } else {
            rgb(0xeef2f7).into()
        }
    }

    #[must_use]
    pub fn bg_badge(self) -> Hsla {
        if self.dark_mode {
            rgb(0x2b394b).into()
        } else {
            rgb(0xdfe7f1).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn bg_backdrop(self) -> Hsla {
        if self.dark_mode {
            rgba(0x0d1520c4).into()
        } else {
            rgba(0x202a3870).into()
        }
    }

    // ---- Borders & dividers ----

    #[must_use]
    pub fn border_subtle(self) -> Hsla {
        if self.dark_mode {
            rgb(0x334154).into()
        } else {
            rgb(0xcbd5e2).into()
        }
    }

    #[must_use]
    pub fn border_strong(self) -> Hsla {
        if self.dark_mode {
            rgb(0x586b83).into()
        } else {
            rgb(0x8798ac).into()
        }
    }

    #[must_use]
    pub fn border_specular(self) -> Hsla {
        if self.dark_mode {
            rgba(0xffffff12).into()
        } else {
            rgba(0xffffffaa).into()
        }
    }

    #[must_use]
    pub fn border_focus(self) -> Hsla {
        if self.dark_mode {
            self.accent_color()
        } else {
            self.accent_bright()
        }
    }

    // ---- Typography colors ----

    #[must_use]
    pub fn text_primary(self) -> Hsla {
        if self.dark_mode {
            rgb(0xeef3f9).into()
        } else {
            rgb(0x141c29).into()
        }
    }

    #[must_use]
    pub fn text_secondary(self) -> Hsla {
        if self.dark_mode {
            rgb(0xc0ccdc).into()
        } else {
            rgb(0x3f5065).into()
        }
    }

    #[must_use]
    pub fn text_muted(self) -> Hsla {
        if self.dark_mode {
            rgb(0xa4b3c6).into()
        } else {
            rgb(0x516177).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn text_on_accent(self) -> Hsla {
        if self.dark_mode {
            rgb(0x141c29).into()
        } else {
            rgb(0xffffff).into()
        }
    }

    // ---- Accent & semantic colors ----

    #[must_use]
    pub fn accent_color(self) -> Hsla {
        rgb(self.accent_rgb).into()
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn accent_effective(self) -> Hsla {
        if self.dark_mode {
            self.accent_color()
        } else {
            rgb(0x465c78).into()
        }
    }

    #[must_use]
    pub fn accent_bright(self) -> Hsla {
        if self.dark_mode {
            rgb(0xb4c2d4).into()
        } else {
            rgb(0x3b516d).into()
        }
    }

    #[must_use]
    pub fn accent_subtle(self) -> Hsla {
        if self.dark_mode {
            rgba((self.accent_rgb << 8) | 0x29).into()
        } else {
            rgba(0x465c781c).into()
        }
    }

    #[must_use]
    pub fn accent_border(self) -> Hsla {
        if self.dark_mode {
            rgba((self.accent_rgb << 8) | 0x6b).into()
        } else {
            rgba(0x465c7866).into()
        }
    }

    #[must_use]
    pub fn folder_tint(self) -> Hsla {
        if self.dark_mode {
            rgb(0xb4c2d4).into()
        } else {
            rgb(0x465c78).into()
        }
    }

    #[must_use]
    pub fn file_tint(self) -> Hsla {
        if self.dark_mode {
            rgb(0xa4b3c6).into()
        } else {
            rgb(0x516177).into()
        }
    }

    #[must_use]
    pub fn symlink_tint(self) -> Hsla {
        if self.dark_mode {
            rgb(0x7cc8ff).into()
        } else {
            rgb(0x0284c7).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn success(self) -> Hsla {
        if self.dark_mode {
            rgb(0x6ccb5f).into()
        } else {
            rgb(0x15803d).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn warning(self) -> Hsla {
        if self.dark_mode {
            rgb(0xf4c95d).into()
        } else {
            rgb(0xd97706).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn warning_bg(self) -> Hsla {
        if self.dark_mode {
            rgba(0x332714d8).into()
        } else {
            rgb(0xfef3c7).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn warning_text(self) -> Hsla {
        if self.dark_mode {
            rgb(0xfde68a).into()
        } else {
            rgb(0x92400e).into()
        }
    }

    #[must_use]
    pub fn danger(self) -> Hsla {
        if self.dark_mode {
            rgb(0xf06e96).into()
        } else {
            rgb(0xdc2626).into()
        }
    }

    #[must_use]
    pub fn danger_bg(self) -> Hsla {
        if self.dark_mode {
            rgba(0x351626d8).into()
        } else {
            rgb(0xfee2e2).into()
        }
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn danger_hover(self) -> Hsla {
        if self.dark_mode {
            rgb(0xe05382).into()
        } else {
            rgb(0xb91c1c).into()
        }
    }

    // ---- Tab-bar surface matching `tab_bar.slint` ----

    #[must_use]
    pub fn bg_tab_strip(self) -> Hsla {
        if self.dark_mode {
            self.bg_sidebar()
        } else {
            self.bg_base()
        }
    }

    #[must_use]
    pub fn bg_column_header(self) -> Hsla {
        if self.dark_mode {
            self.bg_chrome()
        } else {
            self.bg_surface_alt()
        }
    }

    /// Recessed pill surface for the hardware-inspired 7x7 LED dot-matrix pill.
    #[must_use]
    pub fn bg_led_pill(self) -> Hsla {
        if self.dark_mode {
            rgb(0x0b1018).into()
        } else {
            rgb(0x161f2c).into()
        }
    }
}

/// Warm-amber LED intensity levels (`0` = unlit, `4` = peak incandescent).
#[must_use]
pub fn led_color(intensity: u8) -> Hsla {
    match intensity {
        0 => rgb(0x1b1510).into(),
        1 => rgb(0x4a290b).into(),
        2 => rgb(0x9c5312).into(),
        3 => rgb(0xe8871e).into(),
        _ => rgb(0xffc15e).into(),
    }
}

// ---- Corner radii ----

pub const RADIUS_SM: Pixels = px(5.0);
pub const RADIUS_MD: Pixels = px(8.0);
pub const RADIUS_LG: Pixels = px(12.0);

// ---- Spacing scale ----

#[allow(dead_code)]
pub const SPACE_XS: Pixels = px(4.0);
#[allow(dead_code)]
pub const SPACE_SM: Pixels = px(8.0);
#[allow(dead_code)]
pub const SPACE_MD: Pixels = px(12.0);
#[allow(dead_code)]
pub const SPACE_LG: Pixels = px(16.0);

// ---- Typography scale ----

pub const FONT_XS: Pixels = px(12.0);
pub const FONT_SM: Pixels = px(13.0);
pub const FONT_MD: Pixels = px(14.0);
#[allow(dead_code)]
pub const FONT_LG: Pixels = px(15.0);
#[allow(dead_code)]
pub const FONT_XL: Pixels = px(18.0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_tokens_differ() {
        let dark = OnyxTheme::new(true);
        let light = OnyxTheme::new(false);
        assert_ne!(dark.bg_base(), light.bg_base());
        assert_ne!(dark.bg_chrome(), light.bg_chrome());
        assert_ne!(dark.bg_surface(), light.bg_surface());
        assert_ne!(dark.text_primary(), light.text_primary());
    }
}
