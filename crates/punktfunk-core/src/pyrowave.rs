//! PyroWave's rate rule: bits per pixel times pixel rate. The host's pin and the client's
//! Automatic bounds both come from [`kbps_for`], so the two ends agree on a mode's rate
//! without a message for it. The player picks the bits per pixel as PyroWave quality, sees it
//! as the rate it needs ([`rate_label`]), and is warned when this device's link is short of
//! it ([`link_warning`]).

use crate::config::Mode;
use crate::transport::{LinkFacts, IFACE_KIND_ETHERNET, IFACE_KIND_WIFI};

/// The fewest bits per pixel Automatic takes PyroWave to. A session held there tells the
/// player the network is weak.
pub const BPP_FLOOR: f64 = 0.5;

/// The most bits per pixel a client may ask. Past it a frame costs bandwidth nobody sees.
pub const BPP_MAX: f64 = 2.0;

/// The client's quality before the player moves it: the codec author's clean point.
pub const BPP_DEFAULT: f64 = 1.6;

/// A client's asked quality in hundredths, held inside `[BPP_FLOOR, BPP_MAX]`. `0` asks
/// nothing: the host uses its own.
pub fn bpp_of_x100(x100: u16) -> Option<f64> {
    (x100 != 0).then(|| (f64::from(x100) / 100.0).clamp(BPP_FLOOR, BPP_MAX))
}

/// `kbps` as a player reads it: `940 Mbit/s`, `1.3 Gbit/s`.
pub fn rate_label(kbps: u32) -> String {
    let mbps = (u64::from(kbps) + 500) / 1000;
    if mbps < 1000 {
        return format!("{mbps} Mbit/s");
    }
    let gbps = format!("{:.1}", f64::from(kbps) / 1e6);
    format!("{} Gbit/s", gbps.trim_end_matches(".0"))
}

/// What to tell a player whose PyroWave quality needs `required_kbps` over this device's
/// link; `None` when it fits. Wi-Fi always warns: PyroWave is a wired codec. An Ethernet port
/// of known speed warns past 90 % of it; with nothing known, past 900 Mbit/s.
pub fn link_warning(required_kbps: u32, facts: LinkFacts) -> Option<String> {
    let need = rate_label(required_kbps);
    let over = |port_mbps: u32| u64::from(required_kbps) * 10 > u64::from(port_mbps) * 9_000;
    match facts.kind {
        IFACE_KIND_WIFI => Some("PyroWave needs a wired link; this device is on Wi-Fi.".into()),
        IFACE_KIND_ETHERNET if facts.mbps > 0 => over(facts.mbps).then(|| {
            let port = rate_label(facts.mbps.saturating_mul(1000));
            format!("Needs {need} \u{2014} this device's port carries {port}.")
        }),
        _ => over(1000).then(|| format!("Needs {need} \u{2014} more than a 1 GbE link carries.")),
    }
}

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
    fn a_quality_is_held_inside_its_bounds() {
        assert_eq!(bpp_of_x100(0), None);
        assert_eq!(bpp_of_x100(160), Some(1.6));
        assert_eq!(bpp_of_x100(10), Some(BPP_FLOOR));
        assert_eq!(bpp_of_x100(u16::MAX), Some(BPP_MAX));
    }

    #[test]
    fn rates_read_in_mbit_then_gbit() {
        assert_eq!(rate_label(940_000), "940 Mbit/s");
        assert_eq!(rate_label(999_600), "1 Gbit/s");
        assert_eq!(rate_label(1_327_000), "1.3 Gbit/s");
        assert_eq!(rate_label(2_000_000), "2 Gbit/s");
        assert_eq!(rate_label(10_000_000), "10 Gbit/s");
    }

    #[test]
    fn the_link_warning_follows_what_the_os_says() {
        let eth = |mbps| LinkFacts {
            kind: IFACE_KIND_ETHERNET,
            mbps,
        };
        // 90 % of the port is the line: at it the rate fits, past it the player is told.
        assert_eq!(link_warning(900_000, eth(1000)), None);
        assert_eq!(
            link_warning(900_001, eth(1000)).as_deref(),
            Some("Needs 900 Mbit/s \u{2014} this device's port carries 1 Gbit/s.")
        );
        assert_eq!(link_warning(2_000_000, eth(2500)), None);
        let wifi = LinkFacts {
            kind: IFACE_KIND_WIFI,
            mbps: 2400,
        };
        assert_eq!(
            link_warning(100_000, wifi).as_deref(),
            Some("PyroWave needs a wired link; this device is on Wi-Fi.")
        );
        // Nothing known, or a wire with no speed: a 1 GbE link is assumed.
        for unknown in [LinkFacts::default(), eth(0)] {
            assert_eq!(link_warning(900_000, unknown), None);
            assert_eq!(
                link_warning(1_327_000, unknown).as_deref(),
                Some("Needs 1.3 Gbit/s \u{2014} more than a 1 GbE link carries.")
            );
        }
    }

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
