//! macOS displays through CoreGraphics: the online heads a mirror streams. A macOS host creates
//! no display, so every head listed is the user's.
//!
//! A head's connector is its `CGDirectDisplayID` in decimal: what a pin names and what capture
//! opens. Geometry follows [`PhysicalMonitor`]: origin in global points, size in the current
//! mode's pixels, `scale` the ratio between them.

use crate::monitors::PhysicalMonitor;
use anyhow::{bail, Result};
use objc2_core_graphics::{
    CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsActive, CGDisplayMode, CGError,
    CGGetOnlineDisplayList, CGMainDisplayID,
};

/// More heads than a Mac drives.
const MAX_DISPLAYS: usize = 32;

/// Every online display, sorted by `(x, y, connector)`. One asleep or mirroring another is
/// listed disabled: it shows nothing of its own to capture.
pub(crate) fn list_monitors() -> Result<Vec<PhysicalMonitor>> {
    let mut ids = [0u32; MAX_DISPLAYS];
    let mut count = 0u32;
    // SAFETY: `ids` has room for the `MAX_DISPLAYS` entries the call may write, and `count` is a
    // live local it writes once. Neither pointer is kept past the call.
    let err = unsafe { CGGetOnlineDisplayList(MAX_DISPLAYS as u32, ids.as_mut_ptr(), &mut count) };
    if err != CGError::Success {
        bail!("list online displays: CGError {}", err.0);
    }
    let main = CGMainDisplayID();
    let mut heads: Vec<_> = ids
        .iter()
        .take(count as usize)
        .map(|&id| read(id, main))
        .collect();
    heads.sort_by(|a, b| (a.x, a.y, &a.connector).cmp(&(b.x, b.y, &b.connector)));
    Ok(heads)
}

/// The main display's connector: what a session with no monitor of its own streams.
pub fn main_display() -> String {
    CGMainDisplayID().to_string()
}

fn read(id: u32, main: u32) -> PhysicalMonitor {
    let origin = CGDisplayBounds(id).origin;
    let mode = CGDisplayCopyDisplayMode(id);
    let mode = mode.as_deref();
    head(
        id,
        (origin.x, origin.y),
        (
            CGDisplayMode::pixel_width(mode),
            CGDisplayMode::pixel_height(mode),
        ),
        CGDisplayMode::width(mode),
        CGDisplayMode::refresh_rate(mode),
        id == main,
        CGDisplayIsActive(id),
    )
}

/// One head from its CoreGraphics reads. A display with no mode reads zero pixels and scale 1;
/// a panel that reports no rate reads 0 mHz.
fn head(
    id: u32,
    origin: (f64, f64),
    pixels: (usize, usize),
    points_wide: usize,
    refresh_hz: f64,
    primary: bool,
    enabled: bool,
) -> PhysicalMonitor {
    let connector = id.to_string();
    PhysicalMonitor {
        description: connector.clone(),
        connector,
        width: pixels.0 as u32,
        height: pixels.1 as u32,
        refresh_mhz: (refresh_hz * 1000.0).round() as u32,
        x: origin.0 as i32,
        y: origin.1 as i32,
        scale: if points_wide > 0 {
            pixels.0 as f64 / points_wide as f64
        } else {
            1.0
        },
        primary,
        enabled,
        managed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_head_keeps_points_for_its_origin_and_pixels_for_its_size() {
        let retina = head(1, (-1512.0, 0.0), (3024, 1964), 1512, 120.0, true, true);
        assert_eq!(retina.connector, "1");
        assert_eq!((retina.x, retina.y), (-1512, 0));
        assert_eq!((retina.width, retina.height), (3024, 1964));
        assert_eq!(retina.scale, 2.0);
        assert_eq!(retina.mode_label(), "3024x1964@120");
        assert!(retina.primary && retina.enabled && !retina.managed);

        let ntsc = head(
            722475312,
            (0.0, 0.0),
            (1920, 1080),
            1920,
            59.94,
            false,
            true,
        );
        assert_eq!(ntsc.refresh_mhz, 59_940);
        assert_eq!(ntsc.scale, 1.0);

        let modeless = head(3, (0.0, 0.0), (0, 0), 0, 0.0, false, false);
        assert_eq!((modeless.scale, modeless.refresh_mhz), (1.0, 0));
    }

    /// Reads this Mac's displays: no permission, no change to any of them.
    #[test]
    #[ignore = "needs a Mac with a display"]
    fn the_main_display_is_listed_once_and_primary() {
        let heads = list_monitors().expect("list displays");
        let main = main_display();
        let mains: Vec<_> = heads.iter().filter(|h| h.connector == main).collect();
        assert_eq!(mains.len(), 1, "{heads:?}");
        assert!(mains[0].primary);
    }
}
