//! PyroWave's rate rule: bits per pixel times pixel rate. The host's pin and the client's
//! Automatic bounds both come from [`kbps_for`], so the two ends agree on a mode's rate
//! without a message for it.

use crate::config::Mode;

/// `bpp` bits per pixel for a 4:2:0 SDR frame at `mode`, kbps. 4:4:4 carries twice the
/// samples but costs ×1.625, since chroma compresses better than luma; 10-bit planes add 15 %.
/// Unclamped: the caller bounds it.
pub fn kbps_for(mode: &Mode, chroma_444: bool, bit_depth: u8, bpp: f64) -> u32 {
    let mut bpp = bpp;
    if chroma_444 {
        bpp *= 1.625;
    }
    if bit_depth >= 10 {
        bpp *= 1.15;
    }
    let px_per_s =
        f64::from(mode.width) * f64::from(mode.height) * f64::from(mode.refresh_hz.max(1));
    // `as` saturates, so a huge mode lands on `u32::MAX`.
    (px_per_s * bpp / 1000.0) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rate_follows_bits_per_pixel() {
        let mode = Mode {
            width: 3840,
            height: 2160,
            refresh_hz: 120,
        };
        let px = 3840 * 2160 * 120;
        // 0.5 bpp is Steam's 500 Mbps ceiling at 4K120.
        assert_eq!(kbps_for(&mode, false, 8, 0.5), px / 2 / 1000);
        // 4:4:4 and 10-bit scale from the asked value, not from 1.6.
        assert_eq!(
            kbps_for(&mode, true, 10, 1.0),
            (f64::from(px) * 1.625 * 1.15 / 1000.0) as u32
        );
    }
}
