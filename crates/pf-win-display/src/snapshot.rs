//! The cached display snapshot (vdisplay immunity plan WP8, decision D6 "one display actor").
//!
//! Every CCD/GDI/SetupAPI read of the display topology is meant to go through ONE actor — the
//! [`display_events`](crate::display_events) pump thread on Windows — which publishes an immutable,
//! generation-stamped [`DisplaySnapshot`]. Hot readers (cursor poll, descriptor poller, input
//! mapping, stall reports) take the snapshot and never the display-config lock. This module is the
//! platform-neutral half: the identity and inventory types, the [`SnapshotCache`] bookkeeping
//! (generation, last-known-good, failure backoff) and the pure geometry over an inventory, so the
//! rules are unit-tested on every platform. The OS glue lives in `win_display` /
//! `display_events`.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Complete Windows display-target identity. Target ids are only unique PER ADAPTER, so a helper
/// that selects a path from a bare `u32` can resolve, isolate, move, or change HDR on a different
/// adapter's same-numbered target on a hybrid box. Every public helper that picks a path takes
/// this key. The packed LUID matches `pf_frame::dxgi::pack_luid` (asserted by a cross-crate test
/// in pf-capture).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CcdTargetKey {
    /// Packed adapter LUID (`(HighPart << 32) | (LowPart & 0xffff_ffff)`).
    pub adapter_luid: i64,
    pub target_id: u32,
}

impl CcdTargetKey {
    pub fn new(adapter_luid: i64, target_id: u32) -> Self {
        Self {
            adapter_luid,
            target_id,
        }
    }

    /// Build from the split LUID the OS structs carry.
    pub fn from_luid_parts(low: u32, high: i32, target_id: u32) -> Self {
        Self {
            adapter_luid: pack_luid_parts(low, high),
            target_id,
        }
    }
}

impl std::fmt::Display for CcdTargetKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{:x}", self.target_id, self.adapter_luid)
    }
}

/// The LUID packing every key uses — one formula, identical to `pf_frame::dxgi::pack_luid`.
pub fn pack_luid_parts(low: u32, high: i32) -> i64 {
    ((high as i64) << 32) | (low as i64 & 0xffff_ffff)
}

/// One connected target from a `QDC_ALL_PATHS` sweep. `external_physical` and
/// `internal_panel` are the disturbance suspects (standby TV link-probe, dark
/// eDP servicing). Indirect/virtual targets, including ours, are never suspects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetInventory {
    /// Complete identity — target ids are only unique per adapter, so every selector keys on this.
    pub key: CcdTargetKey,
    /// The bare target id, for LOGS and display only — never select a path from it.
    pub target_id: u32,
    pub active: bool,
    /// HDMI/DP/DVI/… — candidate for standby link-probe churn.
    pub external_physical: bool,
    /// eDP/LVDS/embedded — candidate for dark-head servicing when exclusive
    /// isolate leaves the laptop panel connected-but-inactive.
    pub internal_panel: bool,
    pub tech: &'static str,
    /// Empty when the EDID carries none.
    pub friendly: String,
    /// Maps to the PnP instance id (`monitor_devnode`).
    pub monitor_device_path: String,
    /// Ours (the `PNK` EDID manufacturer) — the connector class cannot tell.
    pub ours: bool,
    /// Empty when inactive (no source).
    pub gdi_name: String,
    /// Pixels. All zero when inactive: CCD mode indices are only valid for
    /// active paths.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// mHz (60000 = 60 Hz) from the path's `refreshRate` rational. 0 when
    /// the path reports no rate.
    pub refresh_mhz: u32,
    /// Desktop origin sits on this head — Windows' "primary".
    pub primary: bool,
    /// HDR active on the target; `None` when inactive or the query failed.
    pub hdr: Option<bool>,
    /// `SDRWhiteLevel` (1000 = 80 nits): where DWM puts SDR white on an HDR desktop. `None` when
    /// inactive or not reported.
    pub sdr_white_level: Option<u32>,
    /// The SOURCE side of the active path (VidPn source id + its adapter) — what `D3DKMTGetScanLine`
    /// and the scanline probe address. Zero when inactive.
    pub source_id: u32,
    pub source_adapter_luid: i64,
}

impl TargetInventory {
    /// `(x, y, w, h)` of the active source, `None` when inactive.
    pub fn source_rect(&self) -> Option<(i32, i32, i32, i32)> {
        self.active
            .then_some((self.x, self.y, self.width as i32, self.height as i32))
    }
}

/// An immutable, generation-stamped view of every connected target. Readers clone the `Arc`;
/// the actor replaces the whole value, never a field.
#[derive(Clone, Debug)]
pub struct DisplaySnapshot {
    /// Bumps on every PUBLISHED snapshot (a fresh query, or a last-known-good re-stamp after a
    /// failed one). A reader that saw generation `g` knows nothing changed while it stays `g`.
    pub generation: u64,
    /// When the inventory this carries was actually read from the OS.
    pub taken_at: Instant,
    /// Consecutive failed refreshes since `taken_at`. Zero = fresh; non-zero = last-known-good,
    /// labelled with its age — never presented as a fresh verification.
    pub failures: u32,
    pub targets: Arc<[TargetInventory]>,
}

impl DisplaySnapshot {
    /// An empty snapshot before the first successful read (generation 0).
    pub fn empty(now: Instant) -> Self {
        Self {
            generation: 0,
            taken_at: now,
            failures: 0,
            targets: Arc::from(Vec::new()),
        }
    }

    /// How old the underlying OS read is.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.taken_at)
    }

    /// Whether this is the result of a successful read (not a re-stamped last-known-good).
    pub fn is_fresh(&self) -> bool {
        self.failures == 0 && self.generation > 0
    }

    pub fn target(&self, key: CcdTargetKey) -> Option<&TargetInventory> {
        self.targets.iter().find(|t| t.key == key)
    }

    /// `(x, y, w, h)` of `key`'s active source.
    pub fn source_rect(&self, key: CcdTargetKey) -> Option<(i32, i32, i32, i32)> {
        self.target(key)?.source_rect()
    }

    /// Union of every active source rect, `(x, y, w, h)` — the console desktop bounds.
    pub fn desktop_bounds(&self) -> Option<(i32, i32, i32, i32)> {
        let mut acc: Option<(i32, i32, i32, i32)> = None; // (x0, y0, x1, y1) exclusive end
        for (x0, y0, w, h) in self.targets.iter().filter_map(TargetInventory::source_rect) {
            let (x1, y1) = (x0 + w, y0 + h);
            acc = Some(match acc {
                None => (x0, y0, x1, y1),
                Some((ax0, ay0, ax1, ay1)) => (ax0.min(x0), ay0.min(y0), ax1.max(x1), ay1.max(y1)),
            });
        }
        acc.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
    }

    /// Active paths whose target is not in `keep` — what would still be lit besides the managed
    /// virtual set.
    pub fn count_other_active(&self, keep: &[CcdTargetKey]) -> u32 {
        self.targets
            .iter()
            .filter(|t| t.active && !keep.contains(&t.key))
            .count() as u32
    }

    /// `(source adapter low, high, source id, physical?)` of an active path, preferring a physical
    /// head — the scanline probe needs a real scan-out; an exclusive topology falls back to ours.
    pub fn scanline_target(&self) -> Option<(u32, i32, u32, bool)> {
        let parts = |t: &TargetInventory| {
            (
                t.source_adapter_luid as u32,
                (t.source_adapter_luid >> 32) as i32,
                t.source_id,
            )
        };
        let active = self.targets.iter().filter(|t| t.active);
        if let Some(t) = active.clone().find(|t| !t.ours) {
            let (lo, hi, id) = parts(t);
            return Some((lo, hi, id, true));
        }
        active.take(1).next().map(|t| {
            let (lo, hi, id) = parts(t);
            (lo, hi, id, false)
        })
    }

    /// External standby sinks and the laptop panel Exclusive isolate deactivated. Both get the same
    /// ~2 s driver-level probe. Virtual/indirect targets stay out.
    pub fn connected_inactive_physicals(&self) -> Vec<String> {
        self.targets
            .iter()
            .filter(|t| (t.external_physical || t.internal_panel) && !t.active)
            .map(|t| {
                let name = if t.friendly.is_empty() && t.internal_panel {
                    "laptop panel"
                } else {
                    &t.friendly
                };
                format!("{} ({})", name, t.tech)
            })
            .collect()
    }
}

/// The actor's bookkeeping: what to publish after a read, and when to try again after a failure.
/// Pure so the rules — last-known-good keeps its age, backoff doubles to a cap, a burst coalesces —
/// are testable without a window procedure.
#[derive(Clone, Debug)]
pub struct SnapshotCache {
    current: Arc<DisplaySnapshot>,
    generation: u64,
    failures: u32,
}

/// Events arriving inside this window collapse into one refresh (a hot-plug or mode set fires
/// several broadcasts back to back).
pub const COALESCE: Duration = Duration::from_millis(150);
/// The slow safety refresh when no event arrives — a missed broadcast costs at most this.
pub const SAFETY_REFRESH: Duration = Duration::from_secs(15);
/// First retry after a failed read; doubles per consecutive failure up to [`SAFETY_REFRESH`].
pub const RETRY_BASE: Duration = Duration::from_secs(1);

impl SnapshotCache {
    pub fn new(now: Instant) -> Self {
        Self {
            current: Arc::new(DisplaySnapshot::empty(now)),
            generation: 0,
            failures: 0,
        }
    }

    pub fn current(&self) -> Arc<DisplaySnapshot> {
        self.current.clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A read succeeded: publish it as a fresh snapshot (even if empty — zero targets is an answer).
    pub fn publish(&mut self, targets: Vec<TargetInventory>, now: Instant) -> Arc<DisplaySnapshot> {
        self.generation += 1;
        self.failures = 0;
        self.current = Arc::new(DisplaySnapshot {
            generation: self.generation,
            taken_at: now,
            failures: 0,
            targets: Arc::from(targets),
        });
        self.current.clone()
    }

    /// A read FAILED: keep the last-known-good targets and their `taken_at`, stamp the failure
    /// count so no reader mistakes them for a fresh verification, and say when to retry.
    pub fn fail(&mut self) -> Duration {
        self.failures = self.failures.saturating_add(1);
        self.generation += 1;
        let prev = &self.current;
        self.current = Arc::new(DisplaySnapshot {
            generation: self.generation,
            taken_at: prev.taken_at,
            failures: self.failures,
            targets: prev.targets.clone(),
        });
        Self::retry_delay(self.failures)
    }

    /// Exponential backoff after `failures` consecutive failed reads, capped at the safety period.
    pub fn retry_delay(failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(8);
        (RETRY_BASE * (1u32 << shift)).min(SAFETY_REFRESH)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: u32, active: bool, x: i32, y: i32, w: u32, h: u32, ours: bool) -> TargetInventory {
        TargetInventory {
            key: CcdTargetKey::new(0x1f, key),
            target_id: key,
            active,
            external_physical: !ours,
            internal_panel: false,
            tech: if ours { "punktfunk-virtual" } else { "HDMI" },
            friendly: if ours { String::new() } else { "TV".into() },
            monitor_device_path: String::new(),
            ours,
            gdi_name: if active {
                format!(r"\\.\DISPLAY{key}")
            } else {
                String::new()
            },
            x,
            y,
            width: w,
            height: h,
            refresh_mhz: 60_000,
            primary: active && x == 0 && y == 0,
            hdr: active.then_some(false),
            sdr_white_level: None,
            source_id: key,
            source_adapter_luid: 0x1f,
        }
    }

    #[test]
    fn key_packing_matches_the_dxgi_formula() {
        assert_eq!(
            pack_luid_parts(0xdead_beef, -1),
            -0x1_0000_0000 + 0xdead_beef
        );
        assert_eq!(pack_luid_parts(7, 2), (2 << 32) | 7);
        let k = CcdTargetKey::from_luid_parts(7, 2, 4352);
        assert_eq!(k, CcdTargetKey::new((2 << 32) | 7, 4352));
        assert_eq!(k.to_string(), "4352@200000007");
    }

    #[test]
    fn same_target_id_on_two_adapters_are_distinct_targets() {
        let a = TargetInventory {
            key: CcdTargetKey::new(1, 4352),
            ..t(4352, true, 0, 0, 1920, 1080, false)
        };
        let b = TargetInventory {
            key: CcdTargetKey::new(2, 4352),
            ..t(4352, true, 1920, 0, 2560, 1440, true)
        };
        let snap = DisplaySnapshot {
            targets: Arc::from(vec![a, b]),
            ..DisplaySnapshot::empty(Instant::now())
        };
        assert_eq!(
            snap.source_rect(CcdTargetKey::new(1, 4352)),
            Some((0, 0, 1920, 1080))
        );
        assert_eq!(
            snap.source_rect(CcdTargetKey::new(2, 4352)),
            Some((1920, 0, 2560, 1440))
        );
        assert_eq!(snap.source_rect(CcdTargetKey::new(3, 4352)), None);
        assert_eq!(snap.count_other_active(&[CcdTargetKey::new(2, 4352)]), 1);
    }

    #[test]
    fn desktop_bounds_is_the_union_of_active_sources_only() {
        let snap = DisplaySnapshot {
            targets: Arc::from(vec![
                t(1, true, 0, 0, 1920, 1080, false),
                t(2, true, 1920, -200, 1280, 720, true),
                t(3, false, 9000, 9000, 4096, 2160, false), // inactive: excluded
            ]),
            ..DisplaySnapshot::empty(Instant::now())
        };
        assert_eq!(snap.desktop_bounds(), Some((0, -200, 3200, 1280)));
        let empty = DisplaySnapshot::empty(Instant::now());
        assert_eq!(empty.desktop_bounds(), None);
    }

    #[test]
    fn scanline_target_prefers_a_physical_head() {
        let snap = DisplaySnapshot {
            targets: Arc::from(vec![
                t(1, true, 0, 0, 1920, 1080, true),
                t(2, true, 1920, 0, 1920, 1080, false),
            ]),
            ..DisplaySnapshot::empty(Instant::now())
        };
        assert_eq!(snap.scanline_target(), Some((0x1f, 0, 2, true)));
        // Exclusive topology: only ours is lit — fall back to it, flagged non-physical.
        let ours_only = DisplaySnapshot {
            targets: Arc::from(vec![t(1, true, 0, 0, 1920, 1080, true)]),
            ..DisplaySnapshot::empty(Instant::now())
        };
        assert_eq!(ours_only.scanline_target(), Some((0x1f, 0, 1, false)));
    }

    #[test]
    fn inactive_physicals_are_named_and_ours_never_is() {
        let mut panel = t(9, false, 0, 0, 0, 0, false);
        panel.external_physical = false;
        panel.internal_panel = true;
        panel.tech = "internal-panel";
        panel.friendly = String::new();
        let snap = DisplaySnapshot {
            targets: Arc::from(vec![
                t(1, true, 0, 0, 1920, 1080, true),
                t(2, false, 0, 0, 0, 0, false),
                t(3, false, 0, 0, 0, 0, true),
                panel,
            ]),
            ..DisplaySnapshot::empty(Instant::now())
        };
        assert_eq!(
            snap.connected_inactive_physicals(),
            vec![
                "TV (HDMI)".to_string(),
                "laptop panel (internal-panel)".to_string()
            ]
        );
    }

    #[test]
    fn publish_bumps_generation_and_a_failure_keeps_last_known_good_with_its_age() {
        let t0 = Instant::now();
        let mut c = SnapshotCache::new(t0);
        assert_eq!(c.current().generation, 0);
        assert!(!c.current().is_fresh());
        let s1 = c.publish(
            vec![t(1, true, 0, 0, 1920, 1080, false)],
            t0 + Duration::from_secs(1),
        );
        assert!(s1.is_fresh());
        assert_eq!((s1.generation, s1.targets.len()), (1, 1));
        // The query fails twice: readers still see the one target, stamped stale, and the
        // generation still moves so a waiter wakes up and can read the failure count.
        let d1 = c.fail();
        let d2 = c.fail();
        let s = c.current();
        assert_eq!((s.generation, s.failures, s.targets.len()), (3, 2, 1));
        assert!(!s.is_fresh());
        assert_eq!(
            s.taken_at,
            t0 + Duration::from_secs(1),
            "age is the OS read, not the retry"
        );
        assert_eq!((d1, d2), (Duration::from_secs(1), Duration::from_secs(2)));
        // A success clears the failure count.
        let s = c.publish(Vec::new(), t0 + Duration::from_secs(9));
        assert_eq!((s.generation, s.failures, s.targets.len()), (4, 0, 0));
        assert!(s.is_fresh(), "zero targets is an answer, not a failure");
    }

    #[test]
    fn retry_backoff_doubles_to_the_safety_cap() {
        assert_eq!(SnapshotCache::retry_delay(1), Duration::from_secs(1));
        assert_eq!(SnapshotCache::retry_delay(3), Duration::from_secs(4));
        assert_eq!(SnapshotCache::retry_delay(4), Duration::from_secs(8));
        assert_eq!(SnapshotCache::retry_delay(5), SAFETY_REFRESH);
        assert_eq!(
            SnapshotCache::retry_delay(40),
            SAFETY_REFRESH,
            "no overflow far out"
        );
        assert!(COALESCE < RETRY_BASE && RETRY_BASE < SAFETY_REFRESH);
    }
}
