//! The desktop scale that fits the client's glass. A Linux compositor's virtual output takes a
//! scale and no physical size, so the host works the scale out here. Windows and macOS are
//! handed the size instead and pick their own.
//!
//! The rule is the one Mutter and niri apply to a real monitor: the picture's density over
//! the density that reads as 100 %, to the nearest quarter. A 27-inch 4K panel gets 1.5.

/// A screen with a shorter diagonal is held close: a phone, a tablet, a laptop.
const DESK_FROM_INCHES: f64 = 20.0;
/// Pixels per inch that read as 100 % on a screen held close.
const NEAR_DPI: f64 = 135.0;
/// Pixels per inch that read as 100 % at a desk, where the eye is further away.
const DESK_DPI: f64 = 110.0;
/// The fewest logical pixels a scaled desktop keeps: 800x480.
const MIN_LOGICAL_AREA: f64 = 800.0 * 480.0;

/// The scale for a `px` picture that is `mm` large on the client. `None` leaves the screen
/// at 1: the client states no size, or the picture reads right unscaled.
pub(crate) fn for_panel(px: (u32, u32), mm: Option<(u16, u16)>) -> Option<f64> {
    let (w_mm, h_mm) = mm.filter(|&(w, h)| w > 0 && h > 0)?;
    let (w, h) = (f64::from(px.0), f64::from(px.1));
    let diag_inches = f64::from(w_mm).hypot(f64::from(h_mm)) / 25.4;
    let reads_as_100 = if diag_inches < DESK_FROM_INCHES {
        NEAR_DPI
    } else {
        DESK_DPI
    };
    let ideal = w.hypot(h) / diag_inches / reads_as_100;
    (4..=16)
        .map(|quarters| f64::from(quarters) / 4.0)
        .filter(|scale| w * h / (scale * scale) >= MIN_LOGICAL_AREA)
        .min_by(|a, b| (a - ideal).abs().total_cmp(&(b - ideal).abs()))
        .filter(|scale| *scale > 1.0)
}

/// Whether two scales are the same setting. Compositors report a scale in 1/120 or 1/256 steps.
#[cfg(target_os = "linux")]
pub(crate) fn same(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.01
}

#[cfg(test)]
mod tests {
    use super::*;

    /// niri's table for real panels, which follows Mutter's.
    #[test]
    fn real_panels_get_the_scale_a_desktop_gives_them() {
        let cases = [
            ((65, 129), (720, 1440), 1.5), // Librem 5: 2.0 leaves too little desktop
            ((64, 142), (1080, 2400), 2.5), // Pixel 6a
            ((286, 179), (2560, 1600), 1.75), // 13-inch MacBook
            ((303, 202), (2400, 1600), 1.5), // Surface Laptop Studio
            ((290, 180), (3840, 2400), 2.5), // XPS 9320
            ((509, 286), (3840, 2160), 1.75), // 23-inch 4K
            ((598, 336), (3840, 2160), 1.5), // 27-inch 4K
            ((708, 398), (3840, 2160), 1.25), // 32-inch 4K
            ((554, 312), (3840, 2160), 1.5), // 25-inch 4K, ideal 1.60
            ((260, 170), (2736, 1824), 2.0), // Surface Pro 6
        ];
        for (mm, px, want) in cases {
            assert_eq!(for_panel(px, Some(mm)), Some(want), "{px:?} on {mm:?}");
        }
    }

    #[test]
    fn a_size_holds_while_the_mode_changes() {
        let panel = Some((598, 336));
        assert_eq!(for_panel((3840, 2160), panel), Some(1.5));
        assert_eq!(for_panel((1920, 1080), panel), None);
    }

    #[test]
    fn a_screen_that_needs_no_scale_gets_none() {
        assert_eq!(for_panel((3840, 2160), None), None);
        assert_eq!(for_panel((3840, 2160), Some((0, 336))), None);
        assert_eq!(for_panel((1920, 1080), Some((509, 286))), None); // 23-inch 1080p
        assert_eq!(for_panel((2560, 1440), Some((700, 390))), None); // 31.5-inch 1440p
        assert_eq!(for_panel((640, 480), Some((65, 49))), None); // too small to scale
    }
}
