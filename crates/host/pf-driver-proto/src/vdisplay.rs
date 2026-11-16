//! Virtual-monitor mode lists and OS identity for the `pf-vdisplay` driver: what a monitor
//! advertises, which id names it, and the timing numbers behind one `(width, height, refresh)`.
//!
//! Lives here, not in the driver: the driver only builds under the WDK, so this arithmetic had
//! no test on any machine. Integer-only and free of OS types — the driver stamps the results
//! into its `wdk_sys` / `iddcx` structs and keeps nothing else.

use alloc::vec;
use alloc::vec::Vec;

/// One resolution with the refresh rates it supports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_rates: Vec<u32>,
}

/// A single `(width, height, refresh)` tuple — modes flattened across their refresh rates.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ModeItem {
    pub width: u32,
    pub height: u32,
    pub refresh_rate: u32,
}

/// Flatten a mode list into per-refresh-rate tuples (the order the mode DDIs emit).
pub fn flatten(modes: &[Mode]) -> impl Iterator<Item = ModeItem> + '_ {
    modes.iter().flat_map(|m| {
        m.refresh_rates.iter().map(|&rr| ModeItem {
            width: m.width,
            height: m.height,
            refresh_rate: rr,
        })
    })
}

/// How many distinct resolutions a monitor's advertised list may accumulate (the requested
/// head + the host's extras + history). Bounds the union growth across many resizes.
pub const MODE_LIST_CAP: usize = 24;

/// Append `from`'s modes to `into`, skipping resolutions already there, capped at
/// [`MODE_LIST_CAP`] — the accumulate half of the driver's mode-union semantics. The OS pins
/// a monitor's settable set at arrival, so a list may only ever grow.
///
/// Dedupe is by `(width, height)` alone: a duplicate resolution is dropped WHOLE, refresh
/// rates and all, never merged into the entry already present. The cap is checked before
/// each candidate, so reaching it stops the merge rather than skipping one entry.
pub fn union_modes(into: &mut Vec<Mode>, from: &[Mode]) {
    for m in from {
        if into.len() >= MODE_LIST_CAP {
            break;
        }
        if !into
            .iter()
            .any(|e| (e.width, e.height) == (m.width, m.height))
        {
            into.push(m.clone());
        }
    }
}

/// The list a monitor advertises: the requested mode first, then — for a host only — the
/// `extras` that fit inside it, at its refresh rates, and whatever the monitor already offered.
/// A larger extra is skipped: a game that picks the biggest mode would outgrow the stream.
/// Zero entries are unused slots.
///
/// A seat rides a remote-session adapter, which IddCx obliges to declare `USE_SMALLEST_MODE`,
/// so the OS drives the monitor at the SMALLEST mode on the list. A seat therefore offers
/// exactly what the client asked for: one extra, or one stale larger entry surviving a
/// resize, pins that seat to the wrong resolution.
///
/// `history` is the monitor's current list on a re-advertise, empty at create. The OS pins the
/// settable set at arrival, so a host's list may only grow — [`union_modes`] caps that growth.
#[must_use]
pub fn advertised_modes(
    requested: Mode,
    seat: bool,
    extras: &[[u32; 2]],
    history: &[Mode],
) -> Vec<Mode> {
    let fit: Vec<Mode> = extras
        .iter()
        .filter(|&&[w, h]| w > 0 && h > 0 && w <= requested.width && h <= requested.height)
        .map(|&[width, height]| Mode {
            width,
            height,
            refresh_rates: requested.refresh_rates.clone(),
        })
        .collect();
    let mut modes = vec![requested];
    if !seat {
        union_modes(&mut modes, &fit);
    }
    accumulate_modes(&mut modes, seat, history);
    modes
}

/// Merge `history` into `into` — the accumulate half of [`advertised_modes`], for the caller
/// that only learns the history later (the registry resolves the monitor id under its lock).
///
/// A seat never accumulates: every carried-over mode is one the OS can pick INSTEAD of the
/// size the client asked for.
pub fn accumulate_modes(into: &mut Vec<Mode>, seat: bool, history: &[Mode]) {
    if !seat {
        union_modes(into, history);
    }
}

/// The numbers of a `DISPLAYCONFIG_VIDEO_SIGNAL_INFO`, without the OS struct. The hSync
/// rational has denominator 1, and total size equals active size (no fabricated blanking),
/// so the driver stamps one `(width, height)` region into both size fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalInfo {
    /// Pixels per second, `refresh · width · height`. u64 because 8K240 alone is 7.96e9.
    pub pixel_rate: u64,
    /// `hSyncFreq` numerator, `refresh · height`, saturated into u32.
    pub h_sync_num: u32,
    /// `vSyncFreq` numerator: whole hertz over [`Self::v_sync_den`] 1, or millihertz over 1000.
    pub v_sync_num: u32,
    /// `vSyncFreq` denominator.
    pub v_sync_den: u32,
    /// The `AdditionalSignalInfo` union read as `videoStandard`: 255 (other), with the
    /// vSync frequency divider in bits 16..21.
    pub video_standard: u32,
}

/// THE signal description for both mode DDI families (IddSampleDriver-exact): pixel rate =
/// `refresh · width · height`, integer sync rationals, total == active. Monitor (description)
/// and target (scan-out) modes differ ONLY in `v_sync_freq_divider`, which the caller passes
/// (0 / 1 per the DDI contract).
///
/// Until 2026-07 the monitor side used the virtual-display-rs legacy math instead — a
/// WIDTH-LESS pixel rate (`rr·(h+4)²+1000`) and a deliberately fractional vSync — so the OS
/// saw two disagreeing descriptions of the same tuple, one physically meaningless.
///
/// The hSync numerator is computed in u64 and saturated: an unchecked `refresh · height`
/// past u32 would panic → abort the extern-"C" mode DDI in a debug build.
///
/// `mhz` is the refresh in millihertz. A whole rate keeps the integer vSync rational; one
/// with a fraction states it over 1000.
#[must_use]
pub fn signal_info(width: u32, height: u32, mhz: u32, v_sync_freq_divider: u32) -> SignalInfo {
    let lines = u64::from(mhz) * u64::from(height);
    let (v_sync_num, v_sync_den) = match mhz % 1000 {
        0 => (mhz / 1000, 1),
        _ => (mhz, 1000),
    };
    SignalInfo {
        pixel_rate: (lines * u64::from(width) + 500) / 1000,
        h_sync_num: u32::try_from((lines + 500) / 1000).unwrap_or(u32::MAX),
        v_sync_num,
        v_sync_den,
        video_standard: 255 | (v_sync_freq_divider << 16),
    }
}

/// The rate a mode at whole `hz` runs on a monitor whose exact rate is `exact_mhz`, in
/// millihertz. Zero is a monitor with none; a rate that rounds to another `hz` is not this
/// mode's.
#[must_use]
pub fn rate_mhz(exact_mhz: u32, hz: u32) -> u32 {
    if exact_mhz != 0 && exact_mhz.saturating_add(500) / 1000 == hz {
        exact_mhz
    } else {
        hz.saturating_mul(1000)
    }
}

/// Resolve the id to name a new monitor by, given the ids currently `live`: honour the host's
/// per-client `preferred` id when it is in `1..=15` (so the IddCx `ConnectorIndex` = id stays
/// below `MaxMonitorsSupported` = 16) AND not live, so a client keeps a STABLE identity across
/// reconnects and Windows reapplies its saved per-monitor DPI scaling.
///
/// Otherwise fall back to [`alloc_monitor_id`]. A collision NEVER departs the live holder —
/// that would tear down an unrelated client — so live ids stay distinct even against a host
/// bug. `preferred == 0` (anonymous / TOFU / GameStream) always falls through to auto.
#[must_use]
pub fn resolve_id(live: &[u32], preferred: u32) -> u32 {
    if (1..=15).contains(&preferred) && !live.contains(&preferred) {
        preferred
    } else {
        alloc_monitor_id(live)
    }
}

/// The lowest id ≥ 1 not in `live`. Reusing freed ids (rather than a monotonic counter) keeps
/// the connector index / EDID serial / container GUID bounded by the number of CONCURRENT
/// monitors, so a fresh ADD reuses a departed monitor's OS target slot instead of orphaning it
/// — the ghost accumulation that wedges ADD at 0x80070490.
///
/// The search spans `1..=live.len() + 1`, where pigeonhole guarantees a free id. That bound
/// is NOT clamped to 15: with 15 live ids the result is 16, one past the connector range
/// [`resolve_id`] enforces for a preferred id.
#[must_use]
pub fn alloc_monitor_id(live: &[u32]) -> u32 {
    (1u32..=live.len() as u32 + 1)
        .find(|id| !live.contains(id))
        .unwrap_or(1)
}

/// A deterministic, monitor-unique container GUID (which groups targets into one physical
/// device), derived from `id` so it is stable and collision-free without a random source.
/// Returned as `(Data1, Data2, Data3, Data4)` like [`crate::interface_guid_fields`] — this
/// crate is `no_std` and has no `GUID` type.
#[must_use]
pub const fn container_guid(id: u32) -> (u32, u16, u16, [u8; 8]) {
    (
        0x7066_7664u32.wrapping_add(id),
        0x7044,
        0x5350,
        [
            0xa1,
            0xb2,
            0xc3,
            0xd4,
            0xe5,
            0xf6,
            (id >> 8) as u8,
            id as u8,
        ],
    )
}

/// Sanity bounds for a mode the host requests over ADD / UPDATE_MODES — generous (any real
/// client fits) but rejecting the zero and absurd values that would otherwise reach the EDID
/// and [`signal_info`] math unchecked.
#[must_use]
pub fn valid_mode(width: u32, height: u32, refresh_hz: u32) -> bool {
    (1..=16384).contains(&width)
        && (1..=16384).contains(&height)
        && (1..=1000).contains(&refresh_hz)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One advertised resolution, spelled short enough for the mode-list tests to read.
    fn mode(width: u32, height: u32, refresh_rates: &[u32]) -> Mode {
        Mode {
            width,
            height,
            refresh_rates: refresh_rates.to_vec(),
        }
    }

    /// A 4K120 stream offers the extras that fit, at 120 Hz, in the host's order. The larger
    /// one, the repeat of the head, and the unused zero slot all drop out.
    #[test]
    fn extras_fit_inside_the_request_at_its_refresh() {
        let asked = mode(3840, 2160, &[120]);
        let extras = [
            [5120, 2880],
            [2560, 1440],
            [3840, 2160],
            [0, 0],
            [1920, 1080],
        ];
        let host = advertised_modes(asked.clone(), false, &extras, &[]);
        assert_eq!(
            host,
            vec![asked, mode(2560, 1440, &[120]), mode(1920, 1080, &[120])]
        );
        // flatten walks resolutions in list order, refresh rates within each.
        let flat: Vec<_> = flatten(&host)
            .map(|i| (i.width, i.height, i.refresh_rate))
            .collect();
        assert_eq!(
            flat,
            [(3840, 2160, 120), (2560, 1440, 120), (1920, 1080, 120)]
        );
    }

    /// The `USE_SMALLEST_MODE` rule: the OS drives a seat at the smallest advertised mode, so a
    /// seat must advertise its request alone. A fallback or a surviving larger entry would pin it.
    #[test]
    fn a_seat_advertises_only_what_the_client_asked_for() {
        let asked = mode(2560, 1440, &[120]);
        let extras = [[1280, 720]];
        let history = vec![mode(3840, 2160, &[60]), mode(1024, 768, &[60])];

        let seat = advertised_modes(asked.clone(), true, &extras, &history);
        assert_eq!(seat, vec![asked.clone()], "a seat offers one mode");
        let smallest = flatten(&seat)
            .min_by_key(|i| i.width * i.height)
            .expect("a non-empty list");
        assert_eq!((smallest.width, smallest.height), (2560, 1440));

        // A host keeps the request first, then the extras, then its history — and 1024x768
        // proves the history really does ride along.
        let host = advertised_modes(asked.clone(), false, &extras, &history);
        assert_eq!(host[0], asked);
        assert!(host.contains(&mode(1280, 720, &[120])), "extras");
        assert!(host.contains(&mode(1024, 768, &[60])), "history");
        assert!(
            host.len() <= MODE_LIST_CAP,
            "the union is capped: {}",
            host.len()
        );
    }

    /// Create passes no history; the seat rule still holds and an old host's all-zero extras
    /// leave the request alone.
    #[test]
    fn advertised_modes_without_history_is_the_create_path() {
        let asked = mode(800, 600, &[60]);
        assert_eq!(
            advertised_modes(asked.clone(), true, &[[640, 480]], &[]),
            vec![asked.clone()]
        );
        let host = advertised_modes(asked.clone(), false, &[[640, 480]], &[]);
        assert_eq!(host, vec![asked.clone(), mode(640, 480, &[60])]);
        assert_eq!(
            advertised_modes(asked.clone(), false, &[[0, 0]; 16], &[]),
            vec![asked]
        );
    }

    #[test]
    fn union_modes_dedupes_by_resolution_and_stops_at_the_cap() {
        // The head keeps its place, and a duplicate resolution is dropped WHOLE: the incoming
        // 60/120 Hz rates are lost rather than merged into the 144 Hz head already there.
        let mut into = vec![mode(1920, 1080, &[144])];
        union_modes(
            &mut into,
            &[mode(1920, 1080, &[60, 120]), mode(1280, 720, &[60])],
        );
        assert_eq!(into, vec![mode(1920, 1080, &[144]), mode(1280, 720, &[60])]);

        // At the cap nothing is appended, and nothing already accumulated is truncated.
        let cap = MODE_LIST_CAP;
        let mut full: Vec<_> = (0..cap as u32).map(|i| mode(640 + i, 480, &[60])).collect();
        union_modes(&mut full, &[mode(3840, 2160, &[60])]);
        assert_eq!(full.len(), cap);
        assert!(!full.contains(&mode(3840, 2160, &[60])));

        // One slot free: the first new resolution takes it and the rest fall off — the merge
        // stops AT the cap, so it is the later candidates that are lost.
        let mut nearly: Vec<_> = (0..cap as u32 - 1)
            .map(|i| mode(640 + i, 480, &[60]))
            .collect();
        union_modes(
            &mut nearly,
            &[mode(3840, 2160, &[60]), mode(2560, 1440, &[60])],
        );
        assert_eq!(nearly.len(), cap);
        assert_eq!(nearly[cap - 1], mode(3840, 2160, &[60]));
    }

    #[test]
    fn monitor_ids_honour_the_preferred_then_take_the_lowest_free() {
        // A free preferred id inside the connector range wins.
        assert_eq!(resolve_id(&[1, 2], 7), 7);
        // A collision falls back to auto — the live holder is never displaced.
        assert_eq!(resolve_id(&[1, 2], 2), 3);
        // 0 (anonymous / TOFU / GameStream) and 16+ (past MaxMonitorsSupported) fall back too.
        assert_eq!(resolve_id(&[1, 2], 0), 3);
        assert_eq!(resolve_id(&[1, 2], 16), 3);

        // Lowest free, not next-highest: a departed monitor's id is refilled.
        assert_eq!(alloc_monitor_id(&[]), 1);
        assert_eq!(alloc_monitor_id(&[1, 3, 4]), 2);

        // Pigeonhole over `1..=len + 1` always finds one — but is NOT clamped to the 1..=15
        // range `resolve_id` enforces for a preferred id, so a full adapter allocates past it.
        let fifteen: Vec<u32> = (1..=15).collect();
        assert_eq!(alloc_monitor_id(&fifteen), 16);
        let sixteen: Vec<u32> = (1..=16).collect();
        assert_eq!(alloc_monitor_id(&sixteen), 17);
    }

    #[test]
    fn container_guids_are_unique_per_monitor_id() {
        let all: Vec<_> = (1u32..=16).map(container_guid).collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Only Data1 and the last two Data4 bytes carry the id; the rest is a fixed prefix.
        assert_eq!(
            container_guid(1),
            (
                0x7066_7665,
                0x7044,
                0x5350,
                [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x00, 0x01]
            )
        );
        assert_eq!(container_guid(0x1234).3[6..], [0x12, 0x34]);
    }

    #[test]
    fn signal_info_numerics_match_the_ddi_formula() {
        // Monitor modes pass divider 0, target modes 1 — the ONLY difference between the two.
        let m = signal_info(1920, 1080, 60_000, 0);
        assert_eq!(m.pixel_rate, 60 * 1920 * 1080);
        assert_eq!(m.h_sync_num, 60 * 1080);
        assert_eq!((m.v_sync_num, m.v_sync_den), (60, 1));
        assert_eq!(m.video_standard, 255);
        assert_eq!(
            signal_info(1920, 1080, 60_000, 1).video_standard,
            255 | (1 << 16)
        );

        // A rate with a fraction states it over 1000; the other numbers follow it.
        let ntsc = signal_info(1920, 1080, 59_940, 1);
        assert_eq!((ntsc.v_sync_num, ntsc.v_sync_den), (59_940, 1000));
        assert_eq!(ntsc.pixel_rate, 124_291_584);
        assert_eq!(ntsc.h_sync_num, 64_735);
        assert_eq!(rate_mhz(59_940, 60), 59_940);
        assert_eq!(rate_mhz(59_940, 120), 120_000);
        assert_eq!(rate_mhz(0, 60), 60_000);

        // 8K240's pixel rate (7.96e9) is why that field is u64; the hSync numerator still fits
        // a u32 with orders of magnitude to spare.
        let big = signal_info(7680, 4320, 240_000, 1);
        assert_eq!(big.pixel_rate, 7_962_624_000);
        assert!(big.pixel_rate > u64::from(u32::MAX));
        assert_eq!(big.h_sync_num, 240 * 4320);

        // The hSync saturation is unreachable through `valid_mode` (16384 · 1000 fits); it only
        // keeps an unvalidated tuple from panicking inside the extern-"C" mode DDI.
        assert!(signal_info(16384, 16384, 1_000_000, 0).h_sync_num < u32::MAX);
        assert_eq!(signal_info(1, u32::MAX, u32::MAX, 0).h_sync_num, u32::MAX);
    }

    #[test]
    fn valid_mode_takes_the_edges_and_rejects_zero() {
        assert!(valid_mode(1, 1, 1));
        assert!(valid_mode(1920, 1080, 60));
        assert!(valid_mode(16384, 16384, 1000));
        for bad in [
            (0, 1080, 60),
            (1920, 0, 60),
            (1920, 1080, 0),
            (16385, 1080, 60),
            (1920, 16385, 60),
            (1920, 1080, 1001),
        ] {
            assert!(!valid_mode(bad.0, bad.1, bad.2), "{bad:?}");
        }
    }
}
