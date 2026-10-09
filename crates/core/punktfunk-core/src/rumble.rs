//! Shared rumble policy: seq-gated wire updates in, effective actuator commands out.
//!
//! Embedders never see a TTL, a deadline, or a staleness constant. A platform
//! keeps *how to vibrate* plus [`ActuatorQuirks`]. Native clients poll
//! [`RumbleShared`] from one thread; the browser drives [`RumbleEngine`] from
//! its frame loop. Priority: lease-expiry zero,
//! legacy-staleness zero, current level on every wire update (renewals re-emit
//! so duration APIs re-arm), then quirk keepalives. Close drains one zero per
//! still-buzzing pad.
//!
//! Four motors ([`Levels`]) share one lease and one seq; a trigger-only rumble
//! is live. Pin in this module's tests. Evidence: `design/rumble-root-fix.md`,
//! `design/trigger-rumble-plane.md`.

use crate::input::MAX_PADS;
#[cfg(feature = "quic")]
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Two missed 500 ms legacy refreshes. A quieter host is treated as gone.
pub const LEGACY_STALE_MS: u64 = 1000;

/// Hardware-level net if the engine stalls: twice [`LEGACY_STALE_MS`].
const BACKSTOP_LEGACY_MS: u32 = 2000;

/// Cap whatever the envelope claims. Matches the host `RUMBLE_TTL_CEIL_MS`.
/// Not `pub`: every `pub` const here is an unprefixed `#define` in the C header.
const MAX_LEASE_MS: u16 = 5_000;

/// Four motors, one instant. All-zero is stop now; those carry `backstop_ms == 0`.
/// `backstop_ms` is a duration-API net if the embedder thread stalls.
/// Triggers (`design/trigger-rumble-plane.md`) share the handle scale. Do not
/// fold them onto handles — a continuous trigger stream would drone a motor.
/// A pad without trigger motors ignores them; `(low, high)` then reads silent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RumbleCommand {
    pub pad: u16,
    pub low: u16,
    pub high: u16,
    pub left_trigger: u16,
    pub right_trigger: u16,
    pub backstop_ms: u32,
}

/// Wire order: `(low, high, left_trigger, right_trigger)`. Handles first so a
/// pre-trigger `(low, high)` read is a literal prefix.
type Levels = (u16, u16, u16, u16);

/// All four motors off. Liveness is against this tuple, not the handles: a
/// trigger-only rumble is live.
const SILENT: Levels = (0, 0, 0, 0);

/// How a platform parameterizes the shared policy. Defaults mean no quirks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActuatorQuirks {
    /// Re-emit an unchanged non-zero level every this many ms when hardware
    /// decays between renewals. `0` = none. Useless if the renderer below
    /// skips identical-level writes — pair with `dedup_jitter` when the skip
    /// is at this command layer.
    pub keepalive_ms: u16,
    /// Floor for `backstop_ms` on non-zero commands. C ABI
    /// (`punktfunk_connection_set_rumble_quirks`) for duration APIs that reject
    /// short values.
    pub min_pulse_ms: u16,
    /// Flip the low motor's LSB on re-emits so an SDL-class layer that no-ops
    /// identical values still writes the device.
    pub dedup_jitter: bool,
}

#[derive(Clone, Copy)]
struct PadState {
    level: Levels,
    /// v2 lease expiry. `None` for a zero level or a legacy pad.
    deadline: Option<Instant>,
    /// Last v2 TTL (drives the backstop). `0` means legacy.
    ttl_ms: u16,
    /// Staleness clock. Set only for a no-TTL host.
    legacy_wire: Option<Instant>,
    /// Set on every wire update, including same-level renewals.
    dirty: bool,
    next_keepalive: Option<Instant>,
    /// Last value handed to an embedder. [`SILENT`] means the engine believes
    /// the actuators are off. Keying jitter and redundant-stop on this, not a
    /// free-running phase, keeps every emit path consistent.
    last_emit: Levels,
    quirks: ActuatorQuirks,
}

impl PadState {
    const NEUTRAL: PadState = PadState {
        level: SILENT,
        deadline: None,
        ttl_ms: 0,
        legacy_wire: None,
        dirty: false,
        next_keepalive: None,
        last_emit: SILENT,
        quirks: ActuatorQuirks {
            keepalive_ms: 0,
            min_pulse_ms: 0,
            dedup_jitter: false,
        },
    };

    fn backstop(&self) -> u32 {
        let b = if self.ttl_ms > 0 {
            (2 * self.ttl_ms as u32).clamp(500, 5000)
        } else {
            BACKSTOP_LEGACY_MS
        };
        b.max(self.quirks.min_pulse_ms as u32)
    }

    /// Stop all four motors and clear timers. Triggers share the handle lease.
    fn silence(&mut self, pad: u16) -> RumbleCommand {
        self.level = SILENT;
        self.deadline = None;
        self.legacy_wire = None;
        self.next_keepalive = None;
        self.dirty = false;
        self.last_emit = SILENT;
        RumbleCommand {
            pad,
            low: 0,
            high: 0,
            left_trigger: 0,
            right_trigger: 0,
            backstop_ms: 0,
        }
    }

    /// Record and return the current level. On `dedup_jitter`, a re-send of
    /// `last_emit` is a no-op write, so the low LSB is flipped. Keying on
    /// `last_emit` covers every emit path, including renewals. The flip is
    /// refused at `(1, 0, 0, 0)` — the only tuple that would become [`SILENT`]
    /// — and `low` steps up instead (`1 ↔ 3`). The nudge stays on `low` even
    /// when that lifts a resting handle; moving it to a live motor would make
    /// the phase depend on which motors this command drives.
    fn emit(&mut self, pad: u16) -> RumbleCommand {
        let (mut low, high, lt, rt) = self.level;
        if self.quirks.dedup_jitter && self.level == self.last_emit {
            let alt = low ^ 1;
            low = if (alt, high, lt, rt) == SILENT {
                low | 0b10
            } else {
                alt
            };
        }
        self.last_emit = (low, high, lt, rt);
        RumbleCommand {
            pad,
            low,
            high,
            left_trigger: lt,
            right_trigger: rt,
            backstop_ms: self.backstop(),
        }
    }
}

/// Per-connection policy. `now` is injected so tests pin time; [`RumbleShared`]
/// owns the real clock.
pub struct RumbleEngine {
    pads: [PadState; MAX_PADS],
}

impl Default for RumbleEngine {
    fn default() -> Self {
        Self::new()
    }
}

fn merge_wake(wake: &mut Option<Instant>, t: Instant) {
    *wake = Some(wake.map_or(t, |w| w.min(t)));
}

impl RumbleEngine {
    pub fn new() -> RumbleEngine {
        RumbleEngine {
            pads: [PadState::NEUTRAL; MAX_PADS],
        }
    }

    /// Fold one seq-gated update. Every arrival dirties the pad — renewals
    /// re-emit so duration APIs re-arm. A v2 TTL replaces the lease; a missing
    /// TTL refreshes the staleness clock. `lt`/`rt` are zero on v1/v2: an
    /// absent field on a level-triggered plane means off now, never keep.
    // Grouping the four levels would add a hop at the two call sites for nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn wire_update(
        &mut self,
        now: Instant,
        pad: u16,
        low: u16,
        high: u16,
        lt: u16,
        rt: u16,
        ttl_ms: Option<u16>,
    ) {
        let Some(p) = self.pads.get_mut(pad as usize) else {
            return;
        };
        p.level = (low, high, lt, rt);
        p.dirty = true;
        match ttl_ms {
            Some(t) => {
                let t = t.min(MAX_LEASE_MS);
                p.ttl_ms = t;
                p.legacy_wire = None;
                // Trigger-only is live; a handle-only test would expire it now.
                p.deadline = if p.level != SILENT {
                    Some(now + Duration::from_millis(t as u64))
                } else {
                    None
                };
            }
            None => {
                p.ttl_ms = 0;
                p.deadline = None;
                p.legacy_wire = Some(now);
            }
        }
    }

    pub fn set_quirks(&mut self, pad: u16, q: ActuatorQuirks) {
        if let Some(p) = self.pads.get_mut(pad as usize) {
            p.quirks = q;
            // A stale kick from the old cadence is harmless; clearing the
            // quirk must cancel it.
            if q.keepalive_ms == 0 {
                p.next_keepalive = None;
            }
        }
    }

    /// Next due command at `now`, plus the earliest future wake. `None` wake
    /// means wait for wire input.
    pub fn poll(&mut self, now: Instant) -> (Option<RumbleCommand>, Option<Instant>) {
        let mut wake: Option<Instant> = None;
        for i in 0..MAX_PADS {
            let p = &mut self.pads[i];
            let pad = i as u16;
            if p.level != SILENT {
                // Unrenewed expiry is a host-side bug; log at info.
                if let Some(d) = p.deadline {
                    if now >= d {
                        tracing::info!(pad, "rumble: envelope expired unrenewed — silencing");
                        return (Some(p.silence(pad)), None);
                    }
                    merge_wake(&mut wake, d);
                }
                if let Some(lw) = p.legacy_wire {
                    let stale_at = lw + Duration::from_millis(LEGACY_STALE_MS);
                    if now >= stale_at {
                        tracing::debug!(pad, "rumble: legacy host went quiet — silencing");
                        return (Some(p.silence(pad)), None);
                    }
                    merge_wake(&mut wake, stale_at);
                }
            }
            if p.dirty {
                p.dirty = false;
                if p.level == SILENT {
                    // Drop a stop if `last_emit` is already silent. A lost
                    // first stop leaves the pad buzzing, so the burst still emits.
                    if p.last_emit != SILENT {
                        return (Some(p.silence(pad)), None);
                    }
                    continue;
                }
                if p.quirks.keepalive_ms > 0 {
                    p.next_keepalive =
                        Some(now + Duration::from_millis(p.quirks.keepalive_ms as u64));
                }
                return (Some(p.emit(pad)), None);
            }
            // Keepalive. Expiry and staleness returned above, so this cannot
            // sustain a level the policy has ended.
            if p.level != SILENT && p.quirks.keepalive_ms > 0 {
                let ka = Duration::from_millis(p.quirks.keepalive_ms as u64);
                let due = *p.next_keepalive.get_or_insert(now + ka);
                if now >= due {
                    p.next_keepalive = Some(now + ka);
                    return (Some(p.emit(pad)), None);
                }
                merge_wake(&mut wake, due);
            }
        }
        (None, wake)
    }

    /// One stop per still-buzzing pad. Call until `None`.
    pub fn close_drain(&mut self) -> Option<RumbleCommand> {
        for i in 0..MAX_PADS {
            if self.pads[i].level != SILENT {
                return Some(self.pads[i].silence(i as u16));
            }
        }
        None
    }
}

/// Engine behind a lock + condvar. Demux feeds; one embedder thread polls.
#[cfg(feature = "quic")]
pub(crate) struct RumbleShared {
    inner: Mutex<SharedState>,
    cv: Condvar,
}

#[cfg(feature = "quic")]
struct SharedState {
    engine: RumbleEngine,
    closed: bool,
}

/// Held by the datagram demux. Drop sets `closed` and wakes the poller.
#[cfg(feature = "quic")]
pub(crate) struct RumbleFeed(pub(crate) std::sync::Arc<RumbleShared>);

#[cfg(feature = "quic")]
impl RumbleFeed {
    pub(crate) fn wire_update(
        &self,
        pad: u16,
        low: u16,
        high: u16,
        lt: u16,
        rt: u16,
        ttl_ms: Option<u16>,
    ) {
        let mut g = self.0.inner.lock().unwrap();
        g.engine
            .wire_update(Instant::now(), pad, low, high, lt, rt, ttl_ms);
        drop(g);
        self.0.cv.notify_all();
    }
}

#[cfg(feature = "quic")]
impl Drop for RumbleFeed {
    fn drop(&mut self) {
        self.0.inner.lock().unwrap().closed = true;
        self.0.cv.notify_all();
    }
}

#[cfg(feature = "quic")]
impl RumbleShared {
    pub(crate) fn new() -> RumbleShared {
        RumbleShared {
            inner: Mutex::new(SharedState {
                engine: RumbleEngine::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn set_quirks(&self, pad: u16, q: ActuatorQuirks) {
        self.inner.lock().unwrap().engine.set_quirks(pad, q);
        self.cv.notify_all();
    }

    /// Block up to `timeout`. `Ok(None)` is timeout; `Err(Closed)` means the
    /// connection ended and every close-drain stop was delivered.
    pub(crate) fn next_command(&self, timeout: Duration) -> Result<Option<RumbleCommand>, Closed> {
        let overall = Instant::now() + timeout;
        let mut g = self.inner.lock().unwrap();
        loop {
            let now = Instant::now();
            let (cmd, wake) = g.engine.poll(now);
            if let Some(c) = cmd {
                return Ok(Some(c));
            }
            if g.closed {
                return match g.engine.close_drain() {
                    Some(c) => Ok(Some(c)),
                    None => Err(Closed),
                };
            }
            let until = wake.map_or(overall, |w| w.min(overall));
            if until <= now {
                if now >= overall {
                    return Ok(None);
                }
                continue;
            }
            let (guard, _) = self.cv.wait_timeout(g, until - now).unwrap();
            g = guard;
        }
    }
}

/// The connection ended and every close-drain stop has been delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "quic")]
pub(crate) struct Closed;

#[cfg(test)]
mod tests {
    use super::*;

    /// 40 ms keepalive + LSB jitter — the in-tree decay actuator.
    const DECK: ActuatorQuirks = ActuatorQuirks {
        keepalive_ms: 40,
        min_pulse_ms: 0,
        dedup_jitter: true,
    };

    /// Handle-only update (triggers = 0). Trigger cases use [`wire4`].
    fn wire(e: &mut RumbleEngine, t: Instant, pad: u16, low: u16, high: u16, ttl: Option<u16>) {
        e.wire_update(t, pad, low, high, 0, 0, ttl);
    }

    #[allow(clippy::too_many_arguments)]
    fn wire4(
        e: &mut RumbleEngine,
        t: Instant,
        pad: u16,
        low: u16,
        high: u16,
        lt: u16,
        rt: u16,
        ttl: Option<u16>,
    ) {
        e.wire_update(t, pad, low, high, lt, rt, ttl);
    }

    /// Poll until idle; collect `(low, high)`. [`drain4`] is all four levels.
    fn drain(e: &mut RumbleEngine, t: Instant) -> Vec<(u16, u16)> {
        let mut out = Vec::new();
        while let (Some(c), _) = e.poll(t) {
            out.push((c.low, c.high));
        }
        out
    }

    fn drain4(e: &mut RumbleEngine, t: Instant) -> Vec<Levels> {
        let mut out = Vec::new();
        while let (Some(c), _) = e.poll(t) {
            out.push((c.low, c.high, c.left_trigger, c.right_trigger));
        }
        out
    }

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }

    /// Handle-only expected command (triggers = 0).
    fn cmd(pad: u16, low: u16, high: u16, backstop_ms: u32) -> RumbleCommand {
        cmd4(pad, low, high, 0, 0, backstop_ms)
    }

    fn cmd4(pad: u16, low: u16, high: u16, lt: u16, rt: u16, backstop_ms: u32) -> RumbleCommand {
        RumbleCommand {
            pad,
            low,
            high,
            left_trigger: lt,
            right_trigger: rt,
            backstop_ms,
        }
    }

    #[test]
    fn v2_level_emits_and_expires_at_the_lease() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 0x4000, 0x8000, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 0x4000, 0x8000, 800))); // backstop = 2×ttl
        let (c, wake) = e.poll(t0 + ms(200));
        assert_eq!(c, None);
        assert_eq!(wake, Some(t0 + ms(400)));
        assert_eq!(e.poll(t0 + ms(400)).0, Some(cmd(0, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(500)), (None, None));
    }

    #[test]
    fn renewal_re_emits_and_extends_the_deadline() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 0, Some(400));
        assert!(e.poll(t0).0.is_some());
        wire(&mut e, t0 + ms(300), 0, 100, 0, Some(400));
        assert_eq!(e.poll(t0 + ms(300)).0, Some(cmd(0, 100, 0, 800)));
        assert_eq!(e.poll(t0 + ms(500)).0, None);
        assert_eq!(e.poll(t0 + ms(700)).0, Some(cmd(0, 0, 0, 0)));
    }

    #[test]
    fn explicit_stop_is_immediate_and_cancels_the_lease() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 2, 500, 500, Some(400));
        assert!(e.poll(t0).0.is_some());
        wire(&mut e, t0 + ms(50), 2, 0, 0, Some(0));
        assert_eq!(e.poll(t0 + ms(50)).0, Some(cmd(2, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(600)), (None, None));
    }

    #[test]
    fn legacy_host_gets_the_uniform_staleness_bound() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 300, 0, None);
        assert_eq!(e.poll(t0).0, Some(cmd(0, 300, 0, 2000)));
        wire(&mut e, t0 + ms(500), 0, 300, 0, None);
        assert_eq!(e.poll(t0 + ms(500)).0, Some(cmd(0, 300, 0, 2000)));
        assert_eq!(e.poll(t0 + ms(1400)).0, None); // 900 ms since last wire, inside 1 s
        assert_eq!(e.poll(t0 + ms(1500)).0, Some(cmd(0, 0, 0, 0)));
    }

    #[test]
    fn keepalive_rekicks_with_jitter_and_never_outlives_the_lease() {
        let mut e = RumbleEngine::new();
        e.set_quirks(
            0,
            ActuatorQuirks {
                keepalive_ms: 40,
                min_pulse_ms: 0,
                dedup_jitter: true,
            },
        );
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 200, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 200, 800)));
        assert_eq!(e.poll(t0 + ms(40)).0, Some(cmd(0, 101, 200, 800)));
        assert_eq!(e.poll(t0 + ms(80)).0, Some(cmd(0, 100, 200, 800)));
        assert_eq!(e.poll(t0 + ms(400)).0, Some(cmd(0, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(440)), (None, None));
    }

    #[test]
    fn quirk_registered_mid_rumble_starts_keepalives() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 0, Some(400));
        assert!(e.poll(t0).0.is_some());
        e.set_quirks(
            0,
            ActuatorQuirks {
                keepalive_ms: 40,
                min_pulse_ms: 0,
                dedup_jitter: false,
            },
        );
        // Schedules from this `now`; first kick at now+40.
        let (c, wake) = e.poll(t0 + ms(10));
        assert_eq!(c, None);
        assert_eq!(wake, Some(t0 + ms(50)));
        assert_eq!(e.poll(t0 + ms(50)).0, Some(cmd(0, 100, 0, 800)));
    }

    #[test]
    fn min_pulse_floors_the_backstop() {
        let mut e = RumbleEngine::new();
        e.set_quirks(
            0,
            ActuatorQuirks {
                keepalive_ms: 0,
                min_pulse_ms: 5000,
                dedup_jitter: false,
            },
        );
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 0, Some(100));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 0, 5000)));
    }

    #[test]
    fn close_drain_silences_every_buzzing_pad_once() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 0, Some(400));
        wire(&mut e, t0, 3, 0, 900, Some(400));
        let _ = e.poll(t0);
        let _ = e.poll(t0);
        let a = e.close_drain().unwrap();
        let b = e.close_drain().unwrap();
        assert_eq!((a.pad, a.low, a.high), (0, 0, 0));
        assert_eq!((b.pad, b.low, b.high), (3, 0, 0));
        assert_eq!(e.close_drain(), None);
    }

    #[test]
    fn stalled_embedder_wakes_to_one_current_command_not_a_backlog() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        for k in 0..20u64 {
            wire(&mut e, t0 + ms(k * 120), 0, 100 + k as u16, 0, Some(400));
        }
        let t = t0 + ms(20 * 120);
        assert_eq!(e.poll(t).0, Some(cmd(0, 119, 0, 800)));
        assert_eq!(e.poll(t).0, None);
    }

    #[cfg(feature = "quic")]
    #[test]
    fn shared_close_delivers_drain_zero_then_closed() {
        let shared = std::sync::Arc::new(RumbleShared::new());
        let feed = RumbleFeed(shared.clone());
        feed.wire_update(1, 100, 0, 0, 0, Some(400));
        assert_eq!(
            shared.next_command(ms(100)).unwrap().unwrap(),
            cmd(1, 100, 0, 800)
        );
        drop(feed);
        assert_eq!(
            shared.next_command(ms(100)).unwrap().unwrap(),
            cmd(1, 0, 0, 0)
        );
        assert_eq!(shared.next_command(ms(10)), Err(Closed));
    }

    /// A renewal must not repeat the last device write; an SDL-class layer
    /// would swallow it.
    #[test]
    fn renewal_keeps_the_dedupe_jitter_alternating() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0), vec![(100, 200)]);
        assert_eq!(drain(&mut e, t0 + ms(40)), vec![(101, 200)]);
        assert_eq!(drain(&mut e, t0 + ms(80)), vec![(100, 200)]);
        // Default 120 ms renewal: same level, must still be a distinct write.
        wire(&mut e, t0 + ms(120), 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0 + ms(120)), vec![(101, 200)]);
        assert_eq!(drain(&mut e, t0 + ms(160)), vec![(100, 200)]);
    }

    /// At the 60 ms TTL floor, distinct writes stay within the 40 ms keepalive.
    #[test]
    fn renewal_never_gaps_distinct_writes_at_the_60ms_floor() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        let (mut last, mut last_write, mut worst) = ((0u16, 0u16), 0u64, 0u64);
        for tick in 0..=360u64 {
            let t = t0 + ms(tick);
            if tick % 60 == 0 {
                wire(&mut e, t, 0, 100, 200, Some(400));
            }
            for v in drain(&mut e, t) {
                assert_ne!(v, (0, 0), "a live lease must never emit the stop sentinel");
                if v != last {
                    worst = worst.max(tick - last_write);
                    last_write = tick;
                    last = v;
                }
            }
        }
        assert!(
            worst <= 41,
            "worst distinct-write gap {worst} ms exceeds the 40 ms declared cadence"
        );
    }

    /// Default quirks must not nudge: an off-by-one amplitude would hit
    /// identical-target compares and one-shot duration APIs.
    #[test]
    fn default_quirks_pads_get_the_level_verbatim_on_every_renewal() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 200, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 200, 800)));
        wire(&mut e, t0 + ms(120), 0, 100, 200, Some(400));
        assert_eq!(e.poll(t0 + ms(120)).0, Some(cmd(0, 100, 200, 800)));
    }

    /// `(1, 0)` is the one handle pair whose LSB flip is the stop sentinel.
    #[test]
    fn jitter_never_synthesizes_the_stop_sentinel() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 1, 0, Some(400));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 1, 0, 800)));
        assert_eq!(e.poll(t0 + ms(40)).0, Some(cmd(0, 3, 0, 800)));
        assert_eq!(e.poll(t0 + ms(80)).0, Some(cmd(0, 1, 0, 800)));
    }

    /// Redundant stops drop; a lost first stop leaves the pad buzzing so the
    /// burst re-send still emits.
    #[test]
    fn a_redundant_stop_is_dropped_but_the_burst_still_heals_a_lost_one() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0), vec![(100, 200)]);
        wire(&mut e, t0 + ms(10), 0, 0, 0, Some(0));
        assert_eq!(drain(&mut e, t0 + ms(10)), vec![(0, 0)]);
        wire(&mut e, t0 + ms(20), 0, 0, 0, Some(0));
        wire(&mut e, t0 + ms(30), 0, 0, 0, Some(0));
        assert_eq!(drain(&mut e, t0 + ms(30)), Vec::new());

        wire(&mut e, t0 + ms(40), 0, 100, 200, Some(400));
        assert_eq!(drain(&mut e, t0 + ms(40)), vec![(100, 200)]);
        wire(&mut e, t0 + ms(50), 0, 0, 0, Some(0));
        assert_eq!(drain(&mut e, t0 + ms(50)), vec![(0, 0)]);
    }

    /// Sender-side `RUMBLE_TTL_CEIL_MS` is not a receiver bound; clamp here.
    #[test]
    fn an_overlong_lease_is_clamped_to_the_ceiling() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 200, Some(u16::MAX));
        assert_eq!(e.poll(t0).0, Some(cmd(0, 100, 200, 5000)));
        // Ceiling, not the ~65 s `u16::MAX` asked for.
        assert!(e.poll(t0 + ms(MAX_LEASE_MS as u64 - 1)).0.is_none());
        assert_eq!(
            e.poll(t0 + ms(MAX_LEASE_MS as u64)).0,
            Some(cmd(0, 0, 0, 0)),
            "the lease must end at the ceiling"
        );
    }

    /// `ttl_ms == 0` on a live level is an already-expired lease, not the
    /// legacy sentinel. Expiry runs before relay, so this poll silences.
    #[test]
    fn a_zero_ttl_envelope_silences_rather_than_taking_the_legacy_backstop() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire(&mut e, t0, 0, 100, 200, Some(0));
        assert_eq!(
            e.poll(t0).0,
            Some(cmd(0, 0, 0, 0)),
            "a zero-length lease must expire immediately, not emit with a legacy backstop"
        );
    }

    /// Trigger-only must be live. A two-field test would drop it as a
    /// redundant stop (`design/trigger-rumble-plane.md`).
    #[test]
    fn a_trigger_only_rumble_is_a_live_level_not_a_stop() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire4(&mut e, t0, 0, 0, 0, 0x8000, 0, Some(400));
        assert_eq!(
            e.poll(t0).0,
            Some(cmd4(0, 0, 0, 0x8000, 0, 800)),
            "a trigger-only level must emit with a live backstop"
        );
        assert_eq!(e.poll(t0 + ms(200)), (None, Some(t0 + ms(400))));
        assert_eq!(e.poll(t0 + ms(400)).0, Some(cmd(0, 0, 0, 0)));
        assert_eq!(e.poll(t0 + ms(500)), (None, None));
    }

    /// Pre-trigger C API (`punktfunk_connection_next_rumble_cmd`) has no
    /// trigger slots. Silent handles is correct for that actuator. Those
    /// commands are not [`SILENT`], so redundant-stop suppression does not
    /// drop them.
    #[test]
    fn the_old_two_field_view_of_a_trigger_command_is_silent_handles() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire4(&mut e, t0, 0, 0x1111, 0, 0x8000, 0x4000, Some(400));
        let c = e.poll(t0).0.unwrap();
        assert_eq!((c.pad, c.low, c.high, c.backstop_ms), (0, 0x1111, 0, 800));
        assert_eq!((c.left_trigger, c.right_trigger), (0x8000, 0x4000));
        // Handles off, triggers live: old view is `(0, 0)`, not a stop command.
        wire4(&mut e, t0 + ms(50), 0, 0, 0, 0x8000, 0x4000, Some(400));
        let c = e.poll(t0 + ms(50)).0.unwrap();
        assert_eq!((c.low, c.high), (0, 0));
        assert_eq!((c.left_trigger, c.right_trigger), (0x8000, 0x4000));
        assert_ne!(
            c.backstop_ms, 0,
            "not a stop command — the pad is still live"
        );
    }

    #[test]
    fn keepalives_carry_the_trigger_levels_and_only_nudge_low() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        wire4(&mut e, t0, 0, 100, 200, 300, 400, Some(400));
        assert_eq!(drain4(&mut e, t0), vec![(100, 200, 300, 400)]);
        assert_eq!(drain4(&mut e, t0 + ms(40)), vec![(101, 200, 300, 400)]);
        assert_eq!(drain4(&mut e, t0 + ms(80)), vec![(100, 200, 300, 400)]);
    }

    /// Refuse the LSB flip only at `(1, 0, 0, 0)`. At `(1, 0, lt, rt)` the
    /// flip to `low == 0` is safe — triggers keep the command live. Testing
    /// `(alt, high)` against `(0, 0)` would emit a stop nobody ordered.
    #[test]
    fn the_jitter_never_synthesizes_the_four_field_stop_sentinel() {
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        let t0 = Instant::now();
        // `(1, 0, 0, 0)`: LSB flip is the stop sentinel — step up instead.
        wire(&mut e, t0, 0, 1, 0, Some(400));
        assert_eq!(drain4(&mut e, t0), vec![(1, 0, 0, 0)]);
        assert_eq!(drain4(&mut e, t0 + ms(40)), vec![(3, 0, 0, 0)]);
        assert_eq!(drain4(&mut e, t0 + ms(80)), vec![(1, 0, 0, 0)]);
        // `(1, 0, lt, 0)`: live triggers, so the flip to `low == 0` is taken.
        let mut e = RumbleEngine::new();
        e.set_quirks(0, DECK);
        wire4(&mut e, t0, 0, 1, 0, 0x8000, 0, Some(400));
        assert_eq!(drain4(&mut e, t0), vec![(1, 0, 0x8000, 0)]);
        assert_eq!(drain4(&mut e, t0 + ms(40)), vec![(0, 0, 0x8000, 0)]);
        assert_eq!(drain4(&mut e, t0 + ms(80)), vec![(1, 0, 0x8000, 0)]);
    }

    #[test]
    fn close_drain_silences_a_trigger_only_pad() {
        let mut e = RumbleEngine::new();
        let t0 = Instant::now();
        wire4(&mut e, t0, 2, 0, 0, 0, 0x9000, Some(400));
        assert!(e.poll(t0).0.is_some());
        assert_eq!(e.close_drain(), Some(cmd(2, 0, 0, 0)));
        assert_eq!(e.close_drain(), None);
    }
}
