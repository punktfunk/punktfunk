//! sRGB colour arithmetic every client theme shares: mixes and WCAG contrast. Plain `f64`,
//! no toolkit types, so the Omarchy reader, the GTK shell and the Skia console run the same
//! maths on every platform.

/// sRGB 0..1. Numbers, not strings: mixes need arithmetic, and a value that
/// survived a hex parse cannot smuggle stylesheet syntax.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Rgb(pub f64, pub f64, pub f64);

impl Rgb {
    pub fn hex(self) -> String {
        let c = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        format!("#{:02x}{:02x}{:02x}", c(self.0), c(self.1), c(self.2))
    }

    /// sRGB lerp, `t` of the way toward `other`. At the few-percent mixes consumers use,
    /// oklab is indistinguishable.
    pub fn mix(self, other: Rgb, t: f64) -> Rgb {
        let m = |a: f64, b: f64| a + (b - a) * t;
        Rgb(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    /// WCAG relative luminance.
    pub fn luminance(self) -> f64 {
        let lin = |v: f64| {
            if v <= 0.040_45 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }
}

/// WCAG contrast ratio: 1.0 for two identical colours, 21.0 for black on white.
pub fn contrast(a: Rgb, b: Rgb) -> f64 {
    let (x, y) = (a.luminance(), b.luminance());
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

/// Accent mixed toward `fg` until contrast with `bg` is ≥ 3:1 (WCAG large-text
/// floor). An accent already equal to `fg` has nowhere to go; returns the best mix.
pub fn readable(accent: Rgb, bg: Rgb, fg: Rgb) -> Rgb {
    let mut c = accent;
    for _ in 0..10 {
        if contrast(c, bg) >= 3.0 {
            break;
        }
        c = c.mix(fg, 0.15);
    }
    c
}

/// Black or white, whichever reads better on `accent`.
pub fn on_accent(accent: Rgb) -> Rgb {
    let (black, white) = (Rgb(0.0, 0.0, 0.0), Rgb(1.0, 1.0, 1.0));
    if contrast(accent, white) >= contrast(accent, black) {
        white
    } else {
        black
    }
}
