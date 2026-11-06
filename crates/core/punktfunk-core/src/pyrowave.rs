//! PyroWave's rate rule: bits per pixel times pixel rate. The host's pin and the client's
//! Automatic bounds both come from [`kbps_for`], so the two ends agree on a mode's rate
//! without a message for it. The player picks the bits per pixel as PyroWave quality, sees it
//! as the rate it needs ([`rate`]), and is warned when this device's link is short of it
//! ([`link_shortfall`]).

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

/// A rate as the number a player reads, before its unit is worded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Whole megabits per second, below 1 Gbit/s.
    Mbit(u64),
    /// Gigabits per second to a tenth, without a trailing `.0`: `"1.3"`, `"2"`.
    Gbit(String),
}

/// `kbps` as a player reads it: 940 Mbit/s, 1.3 Gbit/s.
pub fn rate(kbps: u32) -> Rate {
    let mbps = (u64::from(kbps) + 500) / 1000;
    if mbps < 1000 {
        return Rate::Mbit(mbps);
    }
    let gbps = format!("{:.1}", f64::from(kbps) / 1e6);
    Rate::Gbit(gbps.trim_end_matches(".0").to_string())
}

/// Why this device's link falls short of a PyroWave rate ([`link_shortfall`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkShortfall {
    /// Wi-Fi: PyroWave is a wired codec.
    Wifi,
    /// An Ethernet port of this many Mbit/s.
    Port(u32),
    /// Nothing known: the rate is past what a 1 GbE link carries.
    Gbe,
}

/// What a player whose PyroWave quality needs `required_kbps` over this device's link is
/// warned of; `None` when it fits. Wi-Fi always warns. An Ethernet port of known speed warns
/// past 90 % of it; with nothing known, past 900 Mbit/s.
pub fn link_shortfall(required_kbps: u32, facts: LinkFacts) -> Option<LinkShortfall> {
    let over = |port_mbps: u32| u64::from(required_kbps) * 10 > u64::from(port_mbps) * 9_000;
    match facts.kind {
        IFACE_KIND_WIFI => Some(LinkShortfall::Wifi),
        IFACE_KIND_ETHERNET if facts.mbps > 0 => {
            over(facts.mbps).then_some(LinkShortfall::Port(facts.mbps))
        }
        _ => over(1000).then_some(LinkShortfall::Gbe),
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

/// Bytes before each window's body in a windowed AU: `u16` used, `u16` kind.
const WINDOW_PREFIX: usize = 4;
const WIN_PACKED: u16 = 0;
const WIN_FRAG_FIRST: u16 = 1;

/// The leading data shards of a windowed AU (`shard`-byte windows,
/// [`USER_FLAG_CHUNK_ALIGNED`](crate::packet::USER_FLAG_CHUNK_ALIGNED)) that carry its sequence
/// header and wavelet level 4, whose loss blurs up to half the picture. The host's encoder packs
/// those blocks first, so the head ends at the first window that opens with a finer block; an
/// AU packed in block order ends it one window late, never early. `None` when window 0 holds
/// no sequence header.
pub fn head_shards(au: &[u8], shard: usize) -> Option<usize> {
    if shard <= WINDOW_PREFIX + 8 {
        return None;
    }
    let sof = body(au.get(..shard)?).get(..4)?;
    let word = u32::from_le_bytes([sof[0], sof[1], sof[2], sof[3]]);
    if word >> 31 == 0 {
        return None;
    }
    let head_blocks = level4_blocks((word & 0x3fff) + 1, ((word >> 14) & 0x3fff) + 1);
    let mut chain = 0;
    for (i, w) in au.chunks_exact(shard).enumerate() {
        let opens = match u16::from_le_bytes([w[2], w[3]]) {
            WIN_PACKED => first_block(body(w)),
            WIN_FRAG_FIRST => {
                chain = first_block(body(w)).unwrap_or(chain);
                Some(chain)
            }
            _ => Some(chain),
        };
        if opens.is_some_and(|index| index >= head_blocks) {
            return Some(i);
        }
    }
    Some(au.len() / shard)
}

/// A window's used bytes, past its prefix.
fn body(window: &[u8]) -> &[u8] {
    let used = usize::from(u16::from_le_bytes([window[0], window[1]]));
    window
        .get(WINDOW_PREFIX..WINDOW_PREFIX + used)
        .unwrap_or(&[])
}

/// The index of the first block in a window body, past any sequence header.
fn first_block(body: &[u8]) -> Option<u32> {
    let mut at = 0;
    while let Some(h) = body.get(at..at + 8) {
        if h[3] & 0x80 == 0 {
            return Some(u32::from_le_bytes([h[4], h[5], h[6], h[7]]) >> 8);
        }
        at += 8;
    }
    None
}

/// Level-4 blocks of a `width`×`height` frame: four bands of 32×32 coefficients for each of three
/// planes, at the size pyrowave aligns the frame to (`WaveletBuffers::init_block_meta`).
fn level4_blocks(width: u32, height: u32) -> u32 {
    let side = |v: u32| {
        ((((v + 31) & !31).max(128) / 2) >> 4)
            .div_ceil(8)
            .div_ceil(4)
    };
    side(width) * side(height) * 4 * 3
}

/// A 1080p windowed AU of `shard`-byte windows, one block filling each: `head` level-4 windows
/// (the first opens with the sequence header), then `tail` finer ones. Bodies differ per window.
#[cfg(test)]
pub(crate) fn windowed_au(shard: usize, head: usize, tail: usize) -> Vec<u8> {
    let mut au = vec![0u8; (head + tail) * shard];
    for (w, win) in au.chunks_mut(shard).enumerate() {
        win[..2].copy_from_slice(&((shard - WINDOW_PREFIX) as u16).to_le_bytes());
        let mut at = WINDOW_PREFIX;
        if w == 0 {
            let word: u32 = 1919 | (1079 << 14) | (1 << 31);
            win[at..at + 4].copy_from_slice(&word.to_le_bytes());
            at += 8;
        }
        let index = if w < head { w as u32 } else { 48 + w as u32 };
        win[at + 2..at + 4].copy_from_slice(&(((shard - at) / 4) as u16).to_le_bytes());
        win[at + 4..at + 8].copy_from_slice(&(index << 8).to_le_bytes());
        for (j, b) in win[at + 8..].iter_mut().enumerate() {
            *b = (w * 7 + j) as u8;
        }
    }
    au
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A windowed AU of `shard`-byte windows from `(kind, body)` pairs.
    fn windows(shard: usize, wins: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut au = Vec::new();
        for (kind, body) in wins {
            let mut w = vec![0u8; shard];
            w[..2].copy_from_slice(&(body.len() as u16).to_le_bytes());
            w[2..4].copy_from_slice(&kind.to_le_bytes());
            w[4..4 + body.len()].copy_from_slice(body);
            au.extend_from_slice(&w);
        }
        au
    }

    fn sof(width: u32, height: u32) -> Vec<u8> {
        let word = (width - 1) | ((height - 1) << 14) | (1 << 31);
        let mut s = word.to_le_bytes().to_vec();
        s.extend_from_slice(&[0; 4]);
        s
    }

    fn block(index: u32, len: usize) -> Vec<u8> {
        let mut b = vec![0u8; len];
        b[2..4].copy_from_slice(&((len / 4) as u16).to_le_bytes());
        b[4..8].copy_from_slice(&(index << 8).to_le_bytes());
        b
    }

    #[test]
    fn level_four_is_twelve_bands_of_the_aligned_frame() {
        assert_eq!(level4_blocks(1920, 1080), 48);
        assert_eq!(level4_blocks(3840, 2160), 144);
        assert_eq!(level4_blocks(256, 144), 12);
    }

    /// The head ends at the first window that opens with a block past level 4; FRAG pieces
    /// belong to their chain, and a mixed window from in-order packing stays in the head.
    #[test]
    fn the_head_ends_where_finer_blocks_start() {
        let shard = 64;
        let au = windows(
            shard,
            &[
                (
                    WIN_PACKED,
                    [sof(1920, 1080), block(0, 16), block(3, 16)].concat(),
                ),
                (WIN_FRAG_FIRST, block(7, 60)),
                (3, vec![1; 20]),
                (WIN_PACKED, [block(47, 24), block(48, 24)].concat()),
                (WIN_PACKED, block(52, 32)),
                (WIN_FRAG_FIRST, block(60, 60)),
            ],
        );
        assert_eq!(head_shards(&au, shard), Some(4));
        assert_eq!(head_shards(&windowed_au(64, 11, 20), 64), Some(11));
        let only_head = windows(
            shard,
            &[(WIN_PACKED, [sof(1920, 1080), block(5, 8)].concat())],
        );
        assert_eq!(head_shards(&only_head, shard), Some(1));
        let no_sof = windows(shard, &[(WIN_PACKED, block(0, 16))]);
        assert_eq!(head_shards(&no_sof, shard), None);
        assert_eq!(head_shards(&au[..10], shard), None);
    }

    #[test]
    fn a_quality_is_held_inside_its_bounds() {
        assert_eq!(bpp_of_x100(0), None);
        assert_eq!(bpp_of_x100(160), Some(1.6));
        assert_eq!(bpp_of_x100(10), Some(BPP_FLOOR));
        assert_eq!(bpp_of_x100(u16::MAX), Some(BPP_MAX));
    }

    #[test]
    fn rates_read_in_mbit_then_gbit() {
        assert_eq!(rate(940_000), Rate::Mbit(940));
        assert_eq!(rate(999_600), Rate::Gbit("1".into()));
        assert_eq!(rate(1_327_000), Rate::Gbit("1.3".into()));
        assert_eq!(rate(2_000_000), Rate::Gbit("2".into()));
        assert_eq!(rate(10_000_000), Rate::Gbit("10".into()));
    }

    #[test]
    fn the_link_warning_follows_what_the_os_says() {
        let eth = |mbps| LinkFacts {
            kind: IFACE_KIND_ETHERNET,
            mbps,
        };
        // 90 % of the port is the line: at it the rate fits, past it the player is told.
        assert_eq!(link_shortfall(900_000, eth(1000)), None);
        assert_eq!(
            link_shortfall(900_001, eth(1000)),
            Some(LinkShortfall::Port(1000))
        );
        assert_eq!(link_shortfall(2_000_000, eth(2500)), None);
        let wifi = LinkFacts {
            kind: IFACE_KIND_WIFI,
            mbps: 2400,
        };
        assert_eq!(link_shortfall(100_000, wifi), Some(LinkShortfall::Wifi));
        // Nothing known, or a wire with no speed: a 1 GbE link is assumed.
        for unknown in [LinkFacts::default(), eth(0)] {
            assert_eq!(link_shortfall(900_000, unknown), None);
            assert_eq!(link_shortfall(1_327_000, unknown), Some(LinkShortfall::Gbe));
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
