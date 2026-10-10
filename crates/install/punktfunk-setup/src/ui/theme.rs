//! Brand colour and the capability ladder other UI modules ask before drawing.
//!
//! One terminal violet (`#8678f5`, pf-console-ui dark-appearance brand; readable
//! on light too) with lens highlight `#d2c9fb` as the gradient end. Below
//! truecolor the palette degrades to the 256-colour cube, then to no colour
//! rather than approximate brand paint.
//!
//! `Caps` is a value, not a probe: callers pass it in so intro and theme
//! render the same in a test, `--demo`, and a real terminal.
//!
//! Evidence: `design/installer-v2.md` D7.

use serde::{Deserialize, Serialize};

pub const BRAND: Rgb = Rgb(0x86, 0x78, 0xf5);
/// Wordmark gradient end and the mark's overlap.
pub const LENS: Rgb = Rgb(0xd2, 0xc9, 0xfb);
/// Duller than the nearest cube entry; the recorded brand-violet index.
pub const BRAND_256: u8 = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Nearest entry in the 6×6×6 colour cube (indices 16..232).
    pub fn to_256(self) -> u8 {
        let level = |c: u8| ((f32::from(c) / 255.0) * 5.0).round() as u16;
        (16 + 36 * level(self.0) + 6 * level(self.1) + level(self.2)) as u8
    }
}

/// Ordered: comparisons below rely on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Colors {
    None,
    Ansi16,
    Ansi256,
    Truecolor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    pub tty: bool,
    pub colors: Colors,
    pub width: u16,
}

impl Caps {
    pub fn detect(env: &crate::seam::Env, tty: bool, width: u16) -> Caps {
        let term = env.get("TERM").unwrap_or("");
        let colors = if env.get("NO_COLOR").is_some() || term == "dumb" || !tty {
            Colors::None
        } else if matches!(env.get("COLORTERM"), Some("truecolor" | "24bit")) {
            Colors::Truecolor
        } else if term.contains("256") {
            Colors::Ansi256
        } else {
            Colors::Ansi16
        };
        Caps { tty, colors, width }
    }

    pub fn paint(self, rgb: Rgb, layer: Layer) -> String {
        let ground = match layer {
            Layer::Fg => 38,
            Layer::Bg => 48,
        };
        match self.colors {
            Colors::Truecolor => format!("\x1b[{ground};2;{};{};{}m", rgb.0, rgb.1, rgb.2),
            Colors::Ansi256 => format!("\x1b[{ground};5;{}m", rgb.to_256()),
            _ => String::new(),
        }
    }

    /// Restore this layer's default without touching the other.
    pub fn clear(self, layer: Layer) -> String {
        if self.colors < Colors::Ansi256 {
            return String::new();
        }
        match layer {
            Layer::Fg => "\x1b[39m".to_string(),
            Layer::Bg => "\x1b[49m".to_string(),
        }
    }

    pub fn reset(self) -> String {
        if self.colors == Colors::None {
            String::new()
        } else {
            "\x1b[0m".to_string()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Fg,
    Bg,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::Env;

    #[test]
    fn no_color_beats_a_truecolor_promise() {
        let env = Env::of(&[("COLORTERM", "truecolor"), ("NO_COLOR", "1")]);
        assert_eq!(Caps::detect(&env, true, 100).colors, Colors::None);
    }

    #[test]
    fn a_dumb_terminal_gets_no_escapes() {
        let env = Env::of(&[("TERM", "dumb")]);
        let caps = Caps::detect(&env, true, 100);
        assert_eq!(caps.colors, Colors::None);
        assert_eq!(caps.paint(BRAND, Layer::Fg), "");
        assert_eq!(caps.reset(), "");
    }

    // Without a terminal there is nobody to render escapes for, whatever TERM claims.
    #[test]
    fn no_tty_means_no_colour() {
        let env = Env::of(&[("COLORTERM", "truecolor"), ("TERM", "xterm-256color")]);
        assert_eq!(Caps::detect(&env, false, 100).colors, Colors::None);
    }

    #[test]
    fn colorterm_and_term_pick_the_right_depth() {
        let env = Env::of(&[("COLORTERM", "truecolor"), ("TERM", "xterm-256color")]);
        assert_eq!(Caps::detect(&env, true, 100).colors, Colors::Truecolor);
        let env = Env::of(&[("TERM", "xterm-256color")]);
        assert_eq!(Caps::detect(&env, true, 100).colors, Colors::Ansi256);
        let env = Env::of(&[("TERM", "xterm")]);
        assert_eq!(Caps::detect(&env, true, 100).colors, Colors::Ansi16);
    }

    #[test]
    fn truecolor_and_256_paint_the_documented_escapes() {
        let true_caps = Caps {
            tty: true,
            colors: Colors::Truecolor,
            width: 100,
        };
        assert_eq!(true_caps.paint(BRAND, Layer::Fg), "\x1b[38;2;134;120;245m");
        let cube = Caps {
            tty: true,
            colors: Colors::Ansi256,
            width: 100,
        };
        assert_eq!(
            cube.paint(LENS, Layer::Bg),
            format!("\x1b[48;5;{}m", LENS.to_256())
        );
    }

    #[test]
    fn the_cube_approximations_are_the_expected_indices() {
        assert_eq!(Rgb(0xa7, 0x9f, 0xf8).to_256(), 147);
        assert_eq!(Rgb(0x6c, 0x5b, 0xf3).to_256(), 105);
        assert_eq!(LENS.to_256(), 189);
    }
}
