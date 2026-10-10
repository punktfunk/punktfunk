//! Host-wide OS-level virtual-pad slots ([`PadSlotPool`]) and the per-session
//! wire-index → slot map ([`PadSlotMap`]).
//!
//! Every OS name a pad needs is derived from a pad index: the
//! `Global\pfxusb-boot-<i>` / `Global\pfds-boot-<i>` mailboxes, `SwDeviceCreate`
//! instance ids, DualSense pairing MAC, Deck serial, and Switch Pro MAC.
//! `hid-playstation` uses the MAC as HID `uniq`; SDL/Steam dedup on that serial.
//! Clients each number their first pad 0, and the host serves several sessions.
//!
//! A session's wire index stays its own; the OS slot is claimed on first frame
//! and released when the pad (or this map) goes away. Name format is unchanged,
//! so drivers that parse the index need no change.
//!
//! The slot is the player number a co-op game reads, so it is also a preference:
//! [`PadSlotPool::reserve`] names one and [`PadIdentity`] keys it to the pairing
//! fingerprint. It only ORDERS the search — one session still reaches [`MAX_PADS`].
//!
//! The bitmap is per process but the names are per MACHINE, so on a multi-seat
//! box every host would otherwise start at 0 and all but one would be refused
//! [`crate::pad_slots::PadCreateFault::IndexOwnedElsewhere`] — and the wire→slot
//! map memoizes, so that retry never advances. [`PadSlotPool::claim`] therefore
//! skips an index whose mailbox another process serves.

use punktfunk_core::input::MAX_PADS;
use std::sync::Mutex;

/// Stable pad identity for one session.
///
/// `owner` keys a DEVICE, so a reconnect reclaims the slots it had: it is derived
/// from the pairing fingerprint, never from an address. `0` is not minted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PadIdentity {
    pub owner: u64,
    /// Player slot the operator picked, 0-based. `None` = today's lazy claim.
    pub preferred: Option<u8>,
}

impl PadIdentity {
    /// Key this device's pads by its pairing fingerprint. A session with none
    /// (`--open`) gets a per-session key: no identity, so nothing to remember.
    pub fn new(fingerprint_hex: Option<&str>, preferred: Option<u8>) -> PadIdentity {
        PadIdentity {
            owner: owner_key(fingerprint_hex),
            preferred,
        }
    }

    /// A session that has no device record: never equal to another session's.
    pub fn anonymous() -> PadIdentity {
        PadIdentity::new(None, None)
    }
}

/// Hash of the fingerprint, or a fresh per-process number when there is none.
/// Never `0` — that would make two identity-less sessions look like one device.
pub fn owner_key(fingerprint_hex: Option<&str>) -> u64 {
    use std::hash::{Hash, Hasher};
    match fingerprint_hex {
        Some(fp) => {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            fp.to_ascii_lowercase().hash(&mut h);
            h.finish() | 1
        }
        None => {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            // Even, so it can never collide with a hashed fingerprint above.
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed) << 1
        }
    }
}

/// Who wants a slot. Set on every claim and by [`PadSlotPool::reserve`].
#[derive(Clone, Copy, Debug)]
struct Prefer {
    owner: u64,
    /// The owner's session is gone (or its pad unplugged). An idle preference
    /// still brings that owner back to this slot, and costs another owner
    /// nothing — it ranks exactly like a slot no one has ever used.
    idle: bool,
}

#[derive(Debug)]
struct PoolState {
    /// Bit `i` set = OS slot `i` is claimed. `MAX_PADS <= 16` is asserted in [`crate::pad_slots`].
    taken: u16,
    /// Who prefers slot `i`. A hint for [`PadSlotPool::claim`]'s search order.
    prefer: [Option<Prefer>; MAX_PADS],
}

#[derive(Debug)]
pub struct PadSlotPool {
    state: Mutex<PoolState>,
}

impl Default for PadSlotPool {
    fn default() -> Self {
        Self::new()
    }
}

impl PadSlotPool {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(PoolState {
                taken: 0,
                prefer: [None; MAX_PADS],
            }),
        }
    }

    /// Lowest slot free in this process AND unclaimed on the machine, searched in
    /// `owner`'s preferred order. Not round-robin: a lone host still maps wire 0 →
    /// slot 0.
    ///
    /// The preference only sorts the candidates, so every free slot stays
    /// reachable and a claim never fails while one is free.
    pub fn claim(&self, owner: u64) -> Option<u8> {
        let mut st = self.lock();
        let mut order = [0usize; MAX_PADS];
        for (i, slot) in order.iter_mut().enumerate() {
            *slot = i;
        }
        // Stable sort, so within a rank the lowest index still wins.
        order.sort_by_key(|&i| rank(st.prefer[i], owner));
        let pick = order
            .into_iter()
            .find(|&i| st.taken & (1 << i) == 0 && !owned_elsewhere(i as u8))?;
        st.taken |= 1 << pick;
        st.prefer[pick] = Some(Prefer { owner, idle: false });
        Some(pick as u8)
    }

    /// Name the slot `owner` wants before its first frame claims one. `false` =
    /// another live owner already asked for it, so the first asker keeps it and
    /// this session stays on the lazy path.
    ///
    /// One pick per session: the owner's other unheld preferences go, or a claim
    /// would land on the slot it used to want.
    pub fn reserve(&self, slot: u8, owner: u64) -> bool {
        if slot as usize >= MAX_PADS {
            return false;
        }
        let mut st = self.lock();
        if st.prefer[slot as usize].is_some_and(|p| p.owner != owner && !p.idle) {
            return false;
        }
        let taken = st.taken;
        for (i, p) in st.prefer.iter_mut().enumerate() {
            if i != slot as usize && taken & (1 << i) == 0 && p.is_some_and(|p| p.owner == owner) {
                *p = None;
            }
        }
        st.prefer[slot as usize] = Some(Prefer { owner, idle: false });
        true
    }

    /// No-op if `slot` was never claimed, so a double release cannot free another
    /// session's pad. The claim on it stays as an idle preference: a re-plug lands
    /// back here, and anyone else reads the slot as free.
    pub fn release(&self, slot: u8) {
        if (slot as usize) < MAX_PADS {
            let mut st = self.lock();
            st.taken &= !(1u16 << slot);
            if let Some(p) = &mut st.prefer[slot as usize] {
                p.idle = true;
            }
        }
    }

    /// Session end: everything `owner` still wants goes idle, including a slot it
    /// reserved and never plugged a pad into.
    pub fn retire(&self, owner: u64) {
        let mut st = self.lock();
        for p in st.prefer.iter_mut().flatten() {
            if p.owner == owner {
                p.idle = true;
            }
        }
    }

    /// Recover from poison. The state has no torn form, and a panic in one session
    /// must not block every future pad on the host.
    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    fn taken_mask(&self) -> u16 {
        self.lock().taken
    }
}

/// Search order for `owner`: its own slots, then anything free for the taking,
/// then a slot another live owner is on. An idle foreign preference ranks with
/// the never-used, so yesterday's guest does not push today's player off P1.
fn rank(p: Option<Prefer>, owner: u64) -> u8 {
    match p {
        Some(p) if p.owner == owner => 0,
        None => 1,
        Some(p) if p.idle => 1,
        Some(_) => 2,
    }
}

/// Whether another live process on this machine already serves pad index `i`.
///
/// The bootstrap mailboxes are `Global\\` names, so they are the machine-wide
/// record of who owns an index — cheaper and more honest than asking the driver.
/// A mailbox this process created is not another owner: it is our own pad, maybe
/// inside its unplug grace, and a re-plug must land back on it or the host shows
/// two controllers. A losing create is unrecoverable within a session (the
/// wire→slot map keeps the slot it was given), so this is checked BEFORE
/// claiming. Two hosts claiming in the same instant can still both pick one; that
/// create fails and heals on the next claim, when the winner's mailbox shows.
#[cfg(windows)]
fn owned_elsewhere(i: u8) -> bool {
    use crate::gamepad_raii::{created_here, named_section_exists};
    use pf_driver_proto::gamepad::{pad_boot_name, xusb_boot_name};
    [xusb_boot_name(i), pad_boot_name(i)].iter().any(|name| {
        !created_here(name) && named_section_exists(&windows::core::HSTRING::from(name))
    })
}

/// Other platforms name pads per process, so an index is this host's to take.
#[cfg(not(windows))]
fn owned_elsewhere(_i: u8) -> bool {
    false
}

pub fn global() -> &'static PadSlotPool {
    static POOL: PadSlotPool = PadSlotPool::new();
    &POOL
}

/// Per-session wire-index → OS-slot map over a [`PadSlotPool`].
///
/// Drop releases every slot still held, so a panicking input thread cannot
/// strand an OS name for the life of the host.
#[derive(Debug)]
pub struct PadSlotMap<'a> {
    pool: &'a PadSlotPool,
    /// Whose pads these are ([`PadIdentity::owner`]). The pool searches this
    /// owner's slots first, so a reconnect of the same device lands where it was.
    owner: u64,
    slot: [Option<u8>; MAX_PADS],
}

impl PadSlotMap<'static> {
    /// The host-wide pool, keyed to this session's device.
    pub fn new(id: PadIdentity) -> Self {
        Self::with_identity(global(), id)
    }
}

impl Default for PadSlotMap<'static> {
    fn default() -> Self {
        Self::new(PadIdentity::anonymous())
    }
}

impl<'a> PadSlotMap<'a> {
    /// Caller-supplied pool so tests do not touch the process-wide bitmap. No
    /// device behind it: a fresh owner every time, remembering nothing.
    pub fn with_pool(pool: &'a PadSlotPool) -> Self {
        Self::with_identity(pool, PadIdentity::anonymous())
    }

    /// The operator's pick is reserved here, ahead of any first-frame claim. A
    /// refused reservation is not an error: the session falls back to lazy claim.
    pub fn with_identity(pool: &'a PadSlotPool, id: PadIdentity) -> Self {
        if let Some(slot) = id.preferred {
            pool.reserve(slot, id.owner);
        }
        Self {
            pool,
            owner: id.owner,
            slot: [None; MAX_PADS],
        }
    }

    pub fn claim_for(&mut self, wire: usize) -> Option<u8> {
        if wire >= MAX_PADS {
            return None;
        }
        if let Some(slot) = self.slot[wire] {
            return Some(slot);
        }
        let slot = self.pool.claim(self.owner)?;
        self.slot[wire] = Some(slot);
        Some(slot)
    }

    pub fn slot_of(&self, wire: usize) -> Option<u8> {
        self.slot.get(wire).copied().flatten()
    }

    /// Reverse map. Backends tag rumble / HID output with the OS slot they
    /// created; the client only knows its wire index.
    pub fn wire_of(&self, slot: u8) -> Option<usize> {
        self.slot.iter().position(|s| *s == Some(slot))
    }

    /// Every OS slot this session holds, as a bitmask. What `/status` reports and
    /// the client is told, so both name the same player numbers.
    pub fn held_mask(&self) -> u16 {
        self.slot
            .iter()
            .flatten()
            .fold(0u16, |m, slot| m | (1u16 << slot))
    }

    pub fn release(&mut self, wire: usize) {
        if let Some(slot) = self.slot.get_mut(wire).and_then(Option::take) {
            self.pool.release(slot);
        }
    }

    /// Wire-space active mask in OS-slot space. The unplug sweep walks created
    /// slots, so this must use the numbering the devices were created under.
    pub fn os_mask(&self, wire_mask: u16) -> u16 {
        (0..MAX_PADS)
            .filter(|w| wire_mask & (1 << w) != 0)
            .filter_map(|w| self.slot[w])
            .fold(0u16, |m, slot| m | (1u16 << slot))
    }
}

impl Drop for PadSlotMap<'_> {
    /// Slots go back, but this device's claim on them lingers as an idle
    /// preference: a reconnect lands on the same player numbers, and a slot it
    /// only reserved stops holding anyone else back.
    fn drop(&mut self) {
        for wire in 0..MAX_PADS {
            self.release(wire);
        }
        self.pool.retire(self.owner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_sessions_numbering_their_first_pad_zero_get_different_os_slots() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(b.claim_for(0), Some(1), "session B must not reuse slot 0");
        assert_eq!(a.claim_for(1), Some(2));
        assert_eq!(b.claim_for(1), Some(3));

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(pool.taken_mask(), 0b1111);
    }

    #[test]
    fn one_session_still_reaches_every_pad() {
        let pool = PadSlotPool::new();
        let mut only = PadSlotMap::with_pool(&pool);
        for wire in 0..MAX_PADS {
            assert_eq!(only.claim_for(wire), Some(wire as u8), "wire {wire}");
        }
        assert_eq!(pool.taken_mask(), u16::MAX);
    }

    #[test]
    fn a_released_slot_goes_back_to_the_pool() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);

        assert_eq!(a.claim_for(0), Some(0));
        assert_eq!(b.claim_for(0), Some(1));
        a.release(0);
        assert_eq!(a.slot_of(0), None);
        assert_eq!(b.claim_for(1), Some(0));

        // Double-release must not clear a slot another session now holds.
        a.release(0);
        assert_eq!(pool.taken_mask(), 0b11);
    }

    #[test]
    fn dropping_a_session_returns_every_slot_it_held() {
        let pool = PadSlotPool::new();
        {
            let mut s = PadSlotMap::with_pool(&pool);
            s.claim_for(0);
            s.claim_for(3);
            s.claim_for(7);
            assert_eq!(pool.taken_mask(), 0b111);
        }
        assert_eq!(
            pool.taken_mask(),
            0,
            "a dropped session must free its slots"
        );
    }

    #[test]
    fn an_exhausted_pool_refuses_rather_than_colliding() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        for wire in 0..MAX_PADS {
            assert!(a.claim_for(wire).is_some());
        }
        let mut b = PadSlotMap::with_pool(&pool);
        assert_eq!(b.claim_for(0), None, "no slot left, and none may be shared");
    }

    #[test]
    fn an_out_of_range_wire_index_claims_nothing() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        assert_eq!(a.claim_for(MAX_PADS), None);
        assert_eq!(a.claim_for(usize::MAX), None);
        assert_eq!(
            pool.taken_mask(),
            0,
            "a rejected index must not consume a slot"
        );
    }

    #[test]
    fn the_active_mask_is_translated_into_slot_space() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);
        a.claim_for(0);
        b.claim_for(0);
        b.claim_for(1);

        assert_eq!(b.os_mask(0b11), 0b110);
        assert_eq!(a.os_mask(0b11), 0b1);
        assert_eq!(a.os_mask(0), 0);
    }

    #[test]
    fn feedback_maps_back_to_the_wire_pad_that_owns_it() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);
        a.claim_for(0);
        b.claim_for(0);

        assert_eq!(a.wire_of(0), Some(0));
        assert_eq!(
            a.wire_of(1),
            None,
            "B's pad must not resolve to a wire pad of A's - that is rumble on the wrong client"
        );
        assert_eq!(b.wire_of(1), Some(0));
    }

    /// A session that named a player gets it, ahead of the lazy claim that would
    /// have handed it slot 0.
    #[test]
    fn a_reserved_slot_goes_to_the_session_that_asked_for_it() {
        let pool = PadSlotPool::new();
        let mut p2 = PadSlotMap::with_identity(
            &pool,
            PadIdentity {
                owner: 7,
                preferred: Some(1),
            },
        );
        assert_eq!(p2.claim_for(0), Some(1), "wire 0 lands on the picked slot");
        // Slot 0 was never taken, so the unpicked session still gets it.
        let mut p1 = PadSlotMap::with_pool(&pool);
        assert_eq!(p1.claim_for(0), Some(0));
        // A second pad of the picking session has no pick left; lowest free wins.
        assert_eq!(p2.claim_for(1), Some(2));
    }

    /// Two sessions asking for the same player: the first keeps it, the second is
    /// told so and stays on the lazy path. Order decides, not whoever plugs first.
    #[test]
    fn two_sessions_preferring_one_slot_resolve_by_who_asked_first() {
        let pool = PadSlotPool::new();
        assert!(pool.reserve(1, 7), "first asker takes the preference");
        assert!(!pool.reserve(1, 8), "second asker is refused");
        assert!(pool.reserve(1, 7), "the holder may re-assert its own");

        let mut first = PadSlotMap::with_identity(
            &pool,
            PadIdentity {
                owner: 7,
                preferred: Some(1),
            },
        );
        let mut second = PadSlotMap::with_identity(
            &pool,
            PadIdentity {
                owner: 8,
                preferred: Some(1),
            },
        );
        assert_eq!(first.claim_for(0), Some(1));
        assert_eq!(second.claim_for(0), Some(0), "refused pick, lazy claim");
    }

    /// A reservation is a hint, not a lock: a session that reserves a player and
    /// never sends a pad frame must not deny that slot to anyone.
    #[test]
    fn a_reservation_never_used_still_yields_the_slot() {
        let pool = PadSlotPool::new();
        let _idle = PadSlotMap::with_identity(
            &pool,
            PadIdentity {
                owner: 7,
                preferred: Some(0),
            },
        );
        // Everything but the reserved slot goes first — it is only an ordering.
        let mut others = Vec::new();
        for _ in 0..MAX_PADS - 1 {
            let mut m = PadSlotMap::with_pool(&pool);
            assert!(m.claim_for(0).is_some());
            others.push(m);
        }
        let mut last = PadSlotMap::with_pool(&pool);
        assert_eq!(
            last.claim_for(0),
            Some(0),
            "the last free slot is handed out however it is spoken for"
        );
        assert_eq!(pool.taken_mask(), u16::MAX);
    }

    /// Reconnect is keyed by the device, not the address: the same fingerprint
    /// comes back to the slot it had, while a stranger gets the next one.
    #[test]
    fn the_same_device_reconnects_onto_the_slot_it_held() {
        let pool = PadSlotPool::new();
        let fp = Some("aabbccddeeff00112233445566778899");
        let first = PadIdentity::new(fp, None);
        let mut host_pad = PadSlotMap::with_pool(&pool);
        assert_eq!(host_pad.claim_for(0), Some(0));
        {
            let mut guest = PadSlotMap::with_identity(&pool, first);
            assert_eq!(guest.claim_for(0), Some(1));
        }
        // Same fingerprint, new session: slot 1 again, not the freed-up lowest.
        let mut again = PadSlotMap::with_identity(&pool, PadIdentity::new(fp, None));
        assert_eq!(again.claim_for(0), Some(1));
        // A different device has no claim on it and takes the next free slot.
        let mut stranger = PadSlotMap::with_identity(
            &pool,
            PadIdentity::new(Some("00112233445566778899aabbccddeeff"), None),
        );
        assert_eq!(stranger.claim_for(0), Some(2));
    }

    /// The identity is the fingerprint. Two sessions with no pairing record are
    /// two devices, and an address never keys either of them.
    #[test]
    fn sessions_without_a_fingerprint_never_share_an_identity() {
        assert_ne!(PadIdentity::anonymous(), PadIdentity::anonymous());
        assert_eq!(
            owner_key(Some("AABB")),
            owner_key(Some("aabb")),
            "a fingerprint is hex, and its case is not part of the device"
        );
        assert_ne!(owner_key(Some("aabb")), 0);
    }

    /// What `/status` and the client are both told: the slots this session holds.
    #[test]
    fn the_held_mask_names_every_slot_this_session_has() {
        let pool = PadSlotPool::new();
        let mut a = PadSlotMap::with_pool(&pool);
        let mut b = PadSlotMap::with_pool(&pool);
        assert_eq!(a.held_mask(), 0);
        a.claim_for(0);
        b.claim_for(0);
        a.claim_for(1);
        assert_eq!(a.held_mask(), 0b101);
        assert_eq!(b.held_mask(), 0b010);
        a.release(0);
        assert_eq!(a.held_mask(), 0b100);
    }

    /// Only a mailbox another process holds skips a slot. Our own pad's must not, or a
    /// re-plug inside the unplug grace lands on a second slot beside the live pad.
    #[cfg(windows)]
    #[test]
    fn our_own_mailbox_never_reads_as_owned_elsewhere() {
        use pf_driver_proto::gamepad::pad_boot_name;
        let i = (MAX_PADS - 1) as u8;
        let ours = crate::gamepad_raii::PadChannel::create(pad_boot_name(i), 4096)
            .expect("create a Global\\ mailbox (needs SeCreateGlobalPrivilege)");
        assert!(crate::gamepad_raii::named_section_exists(
            &windows::core::HSTRING::from(pad_boot_name(i))
        ));
        assert!(!owned_elsewhere(i), "our own pad skipped its slot");
        drop(ours);
        // Same name, created outside `PadChannel`: stands in for another host's pad.
        let _foreign = crate::gamepad_raii::Shm::create_named(
            &windows::core::HSTRING::from(pad_boot_name(i)),
            64,
        )
        .expect("recreate the mailbox");
        assert!(
            owned_elsewhere(i),
            "another holder's mailbox must still skip the slot"
        );
    }
}
