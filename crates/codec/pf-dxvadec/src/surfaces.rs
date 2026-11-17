//! Decode surfaces a presenter samples in place: where each DPB slot's picture lives, and
//! when a surface may be written again.
//!
//! A pool of exactly one surface per slot names surfaces by slot ([`Surfaces::Identity`]).
//! A pool the presenter samples has [`pf_bitstream::slots::HOLD_HEADROOM`] more surfaces
//! ([`crate::sampled_pool_size`]), and a slot binds to whichever surface its picture was
//! decoded into ([`Surfaces::Bound`]): `Index7Bits` then names the surface, not the slot.
//!
//! [`SurfaceLedger`] is the binding plus what still holds each surface: a delivered frame
//! (until its guard returns) and the presenter's `read` fence value for its last read. A
//! surface is free when unbound, not pending in this access unit, and not held. Before a
//! write the decoder waits the surface's `read` value on its GPU context, once per value.
//! The same ledger with no slots covers a copy ring. Pinned by the tests below.

use crate::SlotMap;
use pf_bitstream::h264::PicId;

/// How a conversion turns a DPB slot into the `Index7Bits` surface index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surfaces<'a> {
    /// Surface `i` is slot `i`: a pool of one surface per slot.
    Identity,
    /// `table[slot]` is the slot's surface before this access unit's removals; `target`
    /// is the surface the current picture decodes into.
    Bound { table: &'a [Option<u8>], target: u8 },
}

impl Surfaces<'_> {
    /// The surface a reference in `slot` lives in; `None` when the slot is unbound.
    pub fn of(self, slot: u8) -> Option<u8> {
        match self {
            Surfaces::Identity => Some(slot),
            Surfaces::Bound { table, .. } => table.get(usize::from(slot)).copied().flatten(),
        }
    }

    /// The surface the current picture, assigned `setup_slot`, decodes into.
    pub fn target(self, setup_slot: u8) -> u8 {
        match self {
            Surfaces::Identity => setup_slot,
            Surfaces::Bound { target, .. } => target,
        }
    }
}

/// Slot bindings, holds and read values over one pool generation.
#[derive(Debug, Clone)]
pub struct SurfaceLedger {
    /// DPB slot → surface; `None` while the slot holds nothing. Empty over a copy ring.
    slot_surface: Vec<Option<u8>>,
    /// Delivered frames still holding each surface. An AV1 `show_existing_frame` can
    /// deliver one surface twice.
    held: Vec<u32>,
    /// Decoded in this access unit and neither bound nor delivered yet.
    pending: Vec<u8>,
    /// The presenter's `read` value of each surface's last returned frame.
    read: Vec<u64>,
    /// Highest `read` value the decode context has waited.
    waited: u64,
    generation: u64,
}

impl SurfaceLedger {
    /// `slots` DPB slots (0 over a copy ring) over `surfaces` surfaces of pool `generation`.
    pub fn new(slots: usize, surfaces: usize, generation: u64) -> SurfaceLedger {
        SurfaceLedger {
            slot_surface: vec![None; slots],
            held: vec![0; surfaces],
            pending: Vec::new(),
            read: vec![0; surfaces],
            waited: 0,
            generation,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// A free surface for the next write, marked pending. `None` is an exhausted pool.
    pub fn acquire(&mut self) -> Option<u8> {
        // `sampled_pool_size` caps a pool at 127 surfaces, so every index fits a `u8`.
        let free = (0..self.held.len() as u8).find(|&i| {
            self.held[usize::from(i)] == 0
                && !self.slot_surface.contains(&Some(i))
                && !self.pending.contains(&i)
        })?;
        self.pending.push(free);
        Some(free)
    }

    /// The conversion's view of the binding, with `target` as the current picture's surface.
    pub fn surfaces(&self, target: u8) -> Surfaces<'_> {
        Surfaces::Bound {
            table: &self.slot_surface,
            target,
        }
    }

    /// The surface `slot` is bound to.
    pub fn surface_of(&self, slot: u8) -> Option<u8> {
        self.slot_surface.get(usize::from(slot)).copied().flatten()
    }

    /// Drop the bindings of slots `slots` no longer holds.
    pub fn sync_slot_bindings(&mut self, slots: &SlotMap) {
        let mut live = vec![false; self.slot_surface.len()];
        for (slot, _) in slots.held() {
            if let Some(l) = live.get_mut(usize::from(slot)) {
                *l = true;
            }
        }
        for (bound, live) in self.slot_surface.iter_mut().zip(live) {
            if !live {
                *bound = None;
            }
        }
    }

    /// Bind picture `id`'s slot to `surface` after its conversion. A picture stored and
    /// evicted in one plan holds no slot; `pending` keeps its surface until the AU ends.
    pub fn bind_setup(&mut self, slots: &SlotMap, id: PicId, surface: u8) {
        self.sync_slot_bindings(slots);
        if let Some(bound) = slots
            .slot_of(id)
            .and_then(|slot| self.slot_surface.get_mut(usize::from(slot)))
        {
            *bound = Some(surface);
        }
    }

    /// The access unit is done: its pictures are bound, delivered, or free.
    pub fn end_au(&mut self) {
        self.pending.clear();
    }

    /// A frame of `surface` goes to the presenter; its guard returns it through [`Self::release`].
    pub fn hold(&mut self, surface: u8) {
        if let Some(h) = self.held.get_mut(usize::from(surface)) {
            *h += 1;
        }
    }

    /// A frame's guard came back, read at `read` (0: never read). `false` for a frame of
    /// another generation or surface, which frees nothing here.
    pub fn release(&mut self, surface: u32, generation: u64, read: u64) -> bool {
        let i = surface as usize;
        if generation != self.generation || i >= self.held.len() || self.held[i] == 0 {
            return false;
        }
        self.held[i] -= 1;
        self.read[i] = self.read[i].max(read);
        true
    }

    /// The `read` value to wait on the decode context before writing `surface`, recorded as
    /// waited. `None` when an earlier wait already covers it.
    pub fn wait_before_write(&mut self, surface: u8) -> Option<u64> {
        let r = *self.read.get(usize::from(surface))?;
        (r > self.waited).then(|| {
            self.waited = r;
            r
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger(slots: usize, surfaces: usize) -> SurfaceLedger {
        SurfaceLedger::new(slots, surfaces, 1)
    }

    #[test]
    fn identity_names_the_slot_and_a_binding_names_the_surface() {
        assert_eq!(Surfaces::Identity.of(3), Some(3));
        assert_eq!(Surfaces::Identity.target(3), 3);
        let table = [Some(7), None, Some(2)];
        let bound = Surfaces::Bound {
            table: &table,
            target: 9,
        };
        assert_eq!(bound.of(0), Some(7));
        assert_eq!(bound.of(1), None, "an unbound slot names no surface");
        assert_eq!(bound.of(5), None, "a slot past the table names no surface");
        assert_eq!(
            bound.target(1),
            9,
            "the target is the ledger's, not the slot"
        );
    }

    /// The ledger's rows, named as the VAAPI session's.
    #[test]
    fn a_surface_is_free_only_when_unbound_not_pending_and_not_held() {
        let mut slots = SlotMap::new(1);
        let mut l = ledger(2, 4);

        // "slot 0 is held by picture 11; slot 1's picture is gone"
        slots.assign(11).expect("a free slot");
        let a = l.acquire().expect("a fresh pool");
        l.bind_setup(&slots, 11, a);
        slots.assign(12).expect("a free slot");
        let b = l.acquire().expect("the pool has spares");
        l.bind_setup(&slots, 12, b);
        slots.release(12);
        l.sync_slot_bindings(&slots);
        assert_eq!(l.surface_of(0), Some(a));
        assert_eq!(l.surface_of(1), None);
        assert_eq!(l.acquire(), Some(2), "b is still pending in this AU");
        l.end_au();

        // "the pool has spares": held surfaces stay out of reach past the DPB.
        l.hold(b);
        assert_eq!(l.acquire(), Some(2), "a bound, b held");
        l.end_au();
        l.hold(2);
        assert_eq!(l.acquire(), Some(3));
        l.end_au();
        l.hold(3);
        assert_eq!(l.acquire(), None, "an exhausted pool says so");
        assert!(l.release(3, 1, 0));
        assert_eq!(l.acquire(), Some(3));
    }

    #[test]
    fn a_late_guard_frees_after_a_rebuild_only_in_its_own_pool() {
        let mut old = ledger(2, 4);
        old.hold(2);
        let mut new = SurfaceLedger::new(2, 4, 2);
        new.hold(2);
        assert!(
            !new.release(2, 1, 5),
            "the old pool's guard frees nothing here"
        );
        assert_eq!(new.held[2], 1);
        assert!(old.release(2, 1, 5));
        assert!(!new.release(9, 2, 0), "a surface past the pool");
        assert!(!new.release(1, 2, 0), "a surface nothing holds");
    }

    /// The sync ledger: the decoder waits `read` on the GPU, once per value.
    #[test]
    fn a_surface_read_by_present_n_is_reusable_once_read_reaches_n() {
        let mut l = ledger(0, 3);
        l.hold(0);
        assert!(l.release(0, 1, 5));
        assert_eq!(l.wait_before_write(0), Some(5), "wait present 5's read");
        assert_eq!(l.wait_before_write(0), None, "read >= 5 is already waited");
        l.hold(1);
        assert!(l.release(1, 1, 4));
        assert_eq!(l.wait_before_write(1), None, "the wait for 5 covers 4");
        assert_eq!(l.wait_before_write(2), None, "never read, no wait");
    }

    #[test]
    fn a_dropped_frame_pins_its_surface_until_the_guard_drops() {
        let mut l = ledger(0, 2);
        let s = l.acquire().expect("free");
        l.hold(s);
        l.end_au();
        assert_eq!(l.acquire(), Some(1));
        l.end_au();
        l.hold(1);
        assert_eq!(l.acquire(), None, "both frames still out");
        assert!(l.release(u32::from(s), 1, 0));
        assert_eq!(l.acquire(), Some(s));
    }

    #[test]
    fn an_unreleased_guard_frees_at_once() {
        let mut l = ledger(0, 1);
        l.hold(0);
        assert!(l.release(0, 1, 0));
        assert_eq!(l.acquire(), Some(0));
        assert_eq!(l.wait_before_write(0), None, "never read: nothing to wait");
    }

    #[test]
    fn a_surface_delivered_twice_frees_after_both_guards() {
        let mut l = ledger(0, 1);
        l.hold(0);
        l.hold(0);
        assert!(l.release(0, 1, 3));
        assert_eq!(l.acquire(), None);
        assert!(l.release(0, 1, 4));
        assert_eq!(l.acquire(), Some(0));
        assert_eq!(l.wait_before_write(0), Some(4), "the later read");
    }
}
