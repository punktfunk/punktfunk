//! Shared virtual-pad slot table and creation lifecycle ([`PadSlots`]).
//!
//! Backends (Linux uinput/uhid, Windows XUSB/UMDF) share existence — create,
//! `active_mask` sweep, lookup — and the lifecycle logs. Per-controller state
//! and the success log stay in the backend's `open`.
//!
//! Invariants pinned here: a frame only arms [`SWEEP_GRACE`] (the pump's
//! [`PadSlots::reap`] completes the unplug); a bit that returns inside the
//! grace reclaims the live pad without `ensure`; [`PadCreateFault`] downcasts
//! through anyhow context (the real attach is `cfg(windows)`).

use crate::pad_gate::PadGate;
use anyhow::Result;
use punktfunk_core::input::MAX_PADS;
use std::time::{Duration, Instant};

// The unplug sweep walks a u16 `active_mask` (the wire type); every slot must have a bit.
const _: () = assert!(MAX_PADS <= 16);

/// 300 ms ≈ a few dozen frames at 60–240 Hz. A shorter window would flap a
/// PnP teardown on a mask glitch; a real unplug is one grace late.
const SWEEP_GRACE: Duration = Duration::from_millis(300);

/// Named create-failure cause, attached as `anyhow` context so
/// [`PadSlots::ensure`] prints this fault's remedy instead of the backend
/// default `hint`.
///
/// The Windows `hint` assumes a missing UMDF package. When a live sibling
/// already owns the pad index's OS name, that advice is wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadCreateFault {
    /// The OS-level name this pad index needs — on Windows
    /// `Global\pf…-boot-<index>` — is already held by another live process.
    /// Retrying cannot hurry the owner; a driver install does not free it.
    IndexOwnedElsewhere,
}

impl PadCreateFault {
    /// Greppable `fault` log tag; the operator text is [`Self::hint`].
    pub fn as_str(self) -> &'static str {
        match self {
            PadCreateFault::IndexOwnedElsewhere => "index-owned-elsewhere",
        }
    }

    /// Remedy printed instead of the backend's default create-failure `hint`.
    pub fn hint(self) -> &'static str {
        match self {
            PadCreateFault::IndexOwnedElsewhere => {
                " — this pad index is already owned by another LIVE process (on a Windows host \
                 that is the LocalSystem PunktfunkHost service, whose session still holds the \
                 pad). The drivers are not the problem and reinstalling them will not help: the \
                 retry succeeds on its own once that process releases the index (end its session, \
                 or Restart-Service PunktfunkHost), or run against a pad index it does not hold."
            }
        }
    }
}

impl std::fmt::Display for PadCreateFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PadCreateFault::IndexOwnedElsewhere => f.write_str(
                "the OS name this pad index needs is already owned by another live process",
            ),
        }
    }
}

/// Fault a backend attached, found through further `.context()` layers.
/// Split out of [`PadSlots::ensure`] so a test can pin the downcast; the
/// real attach is `cfg(windows)`.
fn create_fault(err: &anyhow::Error) -> Option<PadCreateFault> {
    err.downcast_ref::<PadCreateFault>().copied()
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Sweep {
    /// Grace ran out; the caller resets per-index sibling state.
    pub dropped: u16,
    /// Bit returned inside the grace. The live pad is reused without
    /// [`PadSlots::ensure`], so persisted sibling state (touch, motion) is
    /// the previous controller's and must be cleared.
    pub reclaimed: u16,
}

/// `Vec<Option<P>>` keyed by wire index, with the `active_mask` unplug sweep
/// and a [`PadGate`]-guarded create.
///
/// Per-index sibling state stays in the manager: reset it on the indices
/// [`sweep`](Self::sweep) returns and on a `true` from [`ensure`](Self::ensure).
pub struct PadSlots<P> {
    pads: Vec<Option<P>>,
    /// Links that put slot `i`'s pad in its seat's view. Declared after `pads` so the device
    /// goes before the links that named it. `None` on every unfiltered host.
    exposed: Vec<Option<crate::seat_dev::PadLinks>>,
    /// First instant an allocated slot's `active_mask` bit was seen clear.
    /// `None` = the bit is set, or the slot is empty.
    inactive_since: Vec<Option<Instant>>,
    /// This session's seat: the only directory its pads are exposed in.
    seat: Option<crate::seat_dev::SeatDev>,
    gate: PadGate,
    /// Backend tag in shared lifecycle log lines, e.g. `"DualSense/Windows"`.
    label: &'static str,
    /// Device name in the create-failure line (`virtual <device> creation failed`).
    device: &'static str,
    /// Create-failure suffix: empty on Linux, the driver-install hint on Windows.
    hint: &'static str,
}

impl<P> PadSlots<P> {
    pub fn new(label: &'static str, device: &'static str, hint: &'static str) -> PadSlots<P> {
        PadSlots {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            exposed: (0..MAX_PADS).map(|_| None).collect(),
            inactive_since: (0..MAX_PADS).map(|_| None).collect(),
            seat: None,
            gate: PadGate::new(),
            label,
            device,
            hint,
        }
    }

    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Show this session's pads to one seat alone (WP-S3 of
    /// `design/steam-seats-warm-launch-implementation-plan.md`). `None` — the default, and every
    /// Windows host — leaves them where every process of the user sees them.
    pub fn expose_in(&mut self, dir: Option<std::path::PathBuf>) {
        self.seat = dir.map(crate::seat_dev::SeatDev::new);
    }

    /// Fold one frame's `active_mask` into the grace clocks, then drop whatever
    /// has run out. A frame can only arm the grace — no time has passed at the
    /// instant the clock starts. The producer emits one frame per detach, so
    /// [`Self::reap`] on the periodic pump is what finishes the unplug.
    pub fn sweep(&mut self, active_mask: u16) -> Sweep {
        self.sweep_at(active_mask, Instant::now())
    }

    /// Drop allocated pads whose grace has run out. Never arms a clock, so a
    /// pump cannot invent an unplug however often it calls this.
    pub fn reap(&mut self) -> u16 {
        self.reap_at(Instant::now())
    }

    /// Backdate every armed grace by [`SWEEP_GRACE`] so the next sweep drops
    /// still-clear bits. Test-only: production must not expire the grace.
    #[cfg(test)]
    pub(crate) fn expire_grace(&mut self) {
        for since in self.inactive_since.iter_mut().flatten() {
            *since -= SWEEP_GRACE;
        }
    }

    /// [`Self::sweep`] with an injectable clock so tests drive the grace window.
    fn sweep_at(&mut self, active_mask: u16, now: Instant) -> Sweep {
        let mut reclaimed = 0u16;
        for i in 0..MAX_PADS {
            if active_mask & (1 << i) != 0 {
                // Active again: a glitch never reaches the drop. An armed clock
                // means the live pad was handed to whoever is present now,
                // skipping `ensure` — see `Sweep::reclaimed`.
                if self.inactive_since[i].take().is_some() {
                    reclaimed |= 1 << i;
                }
            } else if self.pads[i].is_some() && self.inactive_since[i].is_none() {
                self.inactive_since[i] = Some(now);
            }
        }
        Sweep {
            dropped: self.reap_at(now),
            reclaimed,
        }
    }

    /// [`Self::reap`] with an injectable clock. Never arms.
    fn reap_at(&mut self, now: Instant) -> u16 {
        let mut swept = 0u16;
        for i in 0..MAX_PADS {
            let Some(since) = self.inactive_since[i] else {
                continue;
            };
            if self.pads[i].is_none() {
                self.inactive_since[i] = None; // pad already gone; do not complete a drop
                continue;
            }
            if now.duration_since(since) >= SWEEP_GRACE {
                tracing::info!(index = i, "controller unplugged ({})", self.label);
                self.pads[i] = None;
                self.exposed[i] = None; // the pad went; so does its seat's view of it
                self.inactive_since[i] = None;
                swept |= 1 << i;
            }
        }
        swept
    }

    /// `true` only on a fresh create (caller resets sibling state). `open`
    /// logs success; failure is logged here.
    pub fn ensure(&mut self, idx: usize, open: impl FnOnce(u8) -> Result<P>) -> bool {
        if idx >= MAX_PADS || self.pads[idx].is_some() || !self.gate.allow(Instant::now()) {
            return false;
        }
        // Serialised host-wide: a seat learns which nodes its pad took from what appeared while
        // it was being made, so no second create may land inside that answer.
        let _serial = crate::seat_dev::create_lock();
        let made = match &self.seat {
            Some(seat) => seat
                .create(|| open(idx as u8))
                .map(|(pad, links)| (pad, Some(links))),
            None => open(idx as u8).map(|pad| (pad, None)),
        };
        match made {
            Ok((p, links)) => {
                self.pads[idx] = Some(p);
                self.exposed[idx] = links;
                self.inactive_since[idx] = None; // unarmed: a new pad is active
                self.gate.on_success();
                true
            }
            Err(e) => {
                // Named fault replaces the backend `hint` (Windows: reinstall
                // the driver). Anonymous failures keep that wording. `index`
                // is on the line so a multi-pad session can tell the slots apart.
                let fault = create_fault(&e);
                tracing::error!(
                    index = idx,
                    error = %format!("{e:#}"),
                    fault = fault.map_or("unclassified", PadCreateFault::as_str),
                    "virtual {} creation failed — retrying with backoff{}",
                    self.device,
                    fault.map_or(self.hint, PadCreateFault::hint)
                );
                self.gate.on_failure(Instant::now());
                false
            }
        }
    }

    /// Live pads, not the fixed [`MAX_PADS`] slots. A failed create leaves the
    /// slot empty; measuring without this count is measuring some other process's pad.
    pub fn live(&self) -> usize {
        self.pads.iter().flatten().count()
    }

    pub fn get(&self, idx: usize) -> Option<&P> {
        self.pads.get(idx).and_then(|s| s.as_ref())
    }

    pub fn get_mut(&mut self, idx: usize) -> Option<&mut P> {
        self.pads.get_mut(idx).and_then(|s| s.as_mut())
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (usize, &mut P)> {
        self.pads
            .iter_mut()
            .enumerate()
            .filter_map(|(i, s)| s.as_mut().map(|p| (i, p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn slots() -> PadSlots<u32> {
        PadSlots::new("Test", "test pad", "")
    }

    #[test]
    fn a_single_frame_plus_a_reap_completes_the_unplug() {
        // Production: one cleared-mask frame, then time, then a reap with no
        // further frame. A frame only arms; reap completes.
        let mut s = slots();
        assert!(s.ensure(2, |i| Ok(i as u32)));
        assert_eq!(
            s.sweep(0b0),
            Sweep::default(),
            "a frame arms the grace but cannot itself drop"
        );
        assert!(s.get(2).is_some());
        s.expire_grace();
        assert_eq!(s.reap(), 1 << 2, "the reap did not complete the unplug");
        assert!(s.get(2).is_none());
        assert_eq!(s.reap(), 0, "nothing left to reap");
    }

    #[test]
    fn reap_never_drops_a_pad_no_frame_ever_deactivated() {
        // Reap completes an unplug; it must never invent one. A pad whose bit
        // never went clear has no clock, even if expire_grace backdates.
        let mut s = slots();
        assert!(s.ensure(0, |i| Ok(i as u32)));
        for _ in 0..10 {
            assert_eq!(s.reap(), 0);
            s.expire_grace();
        }
        assert!(
            s.get(0).is_some(),
            "reap dropped a pad that never went inactive"
        );
    }

    #[test]
    fn a_glitch_that_returns_inside_the_grace_never_drops_the_pad() {
        // A client mask that blips clear and returns must not churn a PnP
        // devnode, even when reaps are frequent.
        let mut s = slots();
        assert!(s.ensure(0, |i| Ok(i as u32)));
        assert_eq!(s.sweep(0b0), Sweep::default()); // bit clears — arms only
        for _ in 0..5 {
            assert_eq!(s.reap(), 0, "dropped a pad inside its grace");
        }
        // Bit returned: disarms and reports reclaim — the pad survived, but
        // whoever drives it now may not be the controller that armed the clock.
        assert_eq!(
            s.sweep(0b1),
            Sweep {
                dropped: 0,
                reclaimed: 1,
            }
        );
        s.expire_grace();
        assert_eq!(s.reap(), 0, "a returned bit must leave nothing armed");
        assert!(s.get(0).is_some());
    }

    #[test]
    fn a_mask_that_never_went_clear_is_not_a_reclaim() {
        // `reclaimed` is "came back inside the grace", not "is present" —
        // a steady stream would otherwise clear touch and motion every frame.
        let mut s = slots();
        assert!(s.ensure(0, |i| Ok(i as u32)));
        for _ in 0..5 {
            assert_eq!(s.sweep(0b1), Sweep::default());
        }
    }

    #[test]
    fn ensure_creates_once_and_reports_freshness() {
        let mut s = slots();
        assert!(s.ensure(3, |i| Ok(i as u32 * 10)));
        assert_eq!(s.get(3), Some(&30));
        assert!(!s.ensure(3, |_| panic!("re-opened an occupied slot")));
        assert!(!s.ensure(MAX_PADS, |_| panic!("opened out of range")));
        assert_eq!(s.get(MAX_PADS), None);
    }

    #[test]
    fn sweep_drops_only_cleared_bits_and_returns_them_once() {
        let mut s = slots();
        assert!(s.ensure(0, |_| Ok(0)));
        assert!(s.ensure(2, |_| Ok(2)));
        assert!(s.ensure(5, |_| Ok(5)));
        // Mask keeps 2, clears 0 and 5; empty slots are non-events. First
        // sweep only arms the grace.
        let t0 = Instant::now();
        assert_eq!(s.sweep_at(0b0000_0100, t0), Sweep::default());
        assert_eq!(s.get(0), Some(&0), "still inside the grace");
        let swept = s.sweep_at(0b0000_0100, t0 + SWEEP_GRACE);
        assert_eq!(
            swept,
            Sweep {
                dropped: 0b0010_0001,
                reclaimed: 0,
            }
        );
        assert_eq!(s.get(0), None);
        assert_eq!(s.get(2), Some(&2));
        assert_eq!(s.get(5), None);
        assert_eq!(
            s.sweep_at(0b0000_0100, t0 + SWEEP_GRACE * 2),
            Sweep::default()
        );
    }

    #[test]
    fn a_mask_glitch_inside_the_grace_never_drops() {
        let mut s = slots();
        assert!(s.ensure(1, |_| Ok(7)));
        let t0 = Instant::now();
        assert_eq!(s.sweep_at(0, t0), Sweep::default());
        // Returns inside the grace: disarmed and reported as reclaim (the pad
        // lives, but its owner may have changed — see `Sweep::reclaimed`).
        assert_eq!(
            s.sweep_at(0b0000_0010, t0 + SWEEP_GRACE / 2),
            Sweep {
                dropped: 0,
                reclaimed: 0b0000_0010,
            }
        );
        assert_eq!(
            s.sweep_at(0b0000_0010, t0 + SWEEP_GRACE * 10),
            Sweep::default()
        );
        assert_eq!(s.get(1), Some(&7), "the glitch never reached the drop");
    }

    /// [`create_fault`] must find the fault through further `.context()`
    /// layers. The Windows attach (`gamepad_raii::create_named`) is
    /// `cfg(windows)` and is not compiled here; this chain is the same shape.
    #[test]
    fn a_named_fault_survives_the_context_layers_wrapped_around_it() {
        let err = anyhow::Error::msg("Zugriff verweigert (0x80070005)")
            .context(PadCreateFault::IndexOwnedElsewhere)
            .context("bootstrap mailbox Global\\pfds-boot-0 already exists");
        assert_eq!(
            create_fault(&err),
            Some(PadCreateFault::IndexOwnedElsewhere)
        );
        // Operator rendering still carries every layer, newest first, so the
        // OS error is not traded for the diagnosis.
        let shown = format!("{err:#}");
        assert!(shown.contains("Global\\pfds-boot-0"), "{shown}");
        assert!(
            shown.contains("already owned by another live process"),
            "{shown}"
        );
        assert!(shown.contains("0x80070005"), "{shown}");
    }

    #[test]
    fn an_unclassified_failure_carries_no_fault() {
        assert_eq!(
            create_fault(&anyhow::Error::msg("SwDeviceCreate failed")),
            None
        );
    }

    #[test]
    fn the_contended_hint_never_tells_the_operator_to_reinstall_drivers() {
        let hint = PadCreateFault::IndexOwnedElsewhere.hint();
        assert!(
            !hint.contains("driver install"),
            "the contended hint must not repeat the driver-repair advice: {hint}"
        );
        assert!(hint.contains("already owned"), "{hint}");
        assert!(hint.contains("Restart-Service"), "{hint}");
    }

    /// A named fault must not become a permanent latch — that is the
    /// `broken: bool` [`PadGate`] removed. Retry stays armed.
    #[test]
    fn a_contended_create_still_backs_off_and_retries_rather_than_latching() {
        let mut s = slots();
        let contended = || {
            Err(anyhow::Error::msg("Zugriff verweigert")
                .context(PadCreateFault::IndexOwnedElsewhere))
        };
        assert!(!s.ensure(0, |_| contended()));
        assert_eq!(s.live(), 0);
        // Backed off, not latched: `ensure` reads the wall clock, so clear
        // the backoff rather than sleeping. Window arithmetic is in `pad_gate`.
        s.gate.on_success();
        let mut ran = false;
        assert!(s.ensure(0, |i| {
            ran = true;
            Ok(i as u32)
        }));
        assert!(ran, "the create was never re-attempted");
        assert_eq!(s.live(), 1);
    }

    #[test]
    fn live_counts_built_pads_not_slots() {
        let mut s = slots();
        assert_eq!(s.live(), 0, "an empty table has no pads, only slots");
        assert!(s.ensure(0, |_| Ok(0)));
        assert!(s.ensure(4, |_| Ok(4)));
        assert_eq!(s.live(), 2);
        let t0 = Instant::now();
        s.sweep_at(0, t0);
        s.sweep_at(0, t0 + SWEEP_GRACE);
        assert_eq!(s.live(), 0);
    }

    #[test]
    fn create_failure_arms_the_gate_and_success_heals_it() {
        let mut s = slots();
        assert!(!s.ensure(1, |_| bail!("transient")));
        assert!(!s.ensure(1, |_| panic!("open during backoff")));
        // Gate is manager-wide, so other indices block too.
        assert!(!s.ensure(2, |_| panic!("open during backoff")));
        assert_eq!(s.get(1), None);
    }

    #[test]
    fn recreate_after_sweep_resets_freshness() {
        let mut s = slots();
        assert!(s.ensure(4, |_| Ok(1)));
        let t0 = Instant::now();
        s.sweep_at(0, t0);
        s.sweep_at(0, t0 + SWEEP_GRACE);
        assert_eq!(s.get(4), None);
        assert!(s.ensure(4, |_| Ok(2)));
        assert_eq!(s.get(4), Some(&2));
    }

    #[test]
    fn iter_mut_yields_live_pads_with_indices() {
        let mut s = slots();
        assert!(s.ensure(1, |_| Ok(10)));
        assert!(s.ensure(6, |_| Ok(60)));
        let seen: Vec<(usize, u32)> = s.iter_mut().map(|(i, p)| (i, *p)).collect();
        assert_eq!(seen, vec![(1, 10), (6, 60)]);
    }
}
