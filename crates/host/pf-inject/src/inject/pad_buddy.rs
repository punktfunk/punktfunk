//! Buddy pads: several sessions playing one OS pad slot ([`BuddyHub`]).
//!
//! The session whose pad built the slot's device leads it. A session the operator shares
//! into that slot builds no device: it posts each pad state here, and the lead folds the
//! posts into its own state before every write ([`merge`]). The lead posts the device's
//! rumble back, and every buddy plays it.
//!
//! The merge: buttons OR, extra bits (paddles, touchpad click) included; each trigger takes
//! the larger value; each stick takes the input furthest from centre, x and y together.
//! Gyro, touchpad contacts, raw reports, HID output and pad audio stay with the lead pad.
//! A buddy that leaves withdraws its posts, so the lead's next write lets go of them.

use punktfunk_core::input::{GamepadFrame, MAX_PADS};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

/// Low motor, high motor, left trigger, right trigger.
pub type RumbleLevels = (u16, u16, u16, u16);

/// `lead` with every buddy state folded in. The lead's index and active mask stand, and a
/// stick tie keeps the earlier input.
pub fn merge(lead: GamepadFrame, buddies: impl IntoIterator<Item = GamepadFrame>) -> GamepadFrame {
    buddies.into_iter().fold(lead, |mut m, b| {
        m.buttons |= b.buttons;
        m.left_trigger = m.left_trigger.max(b.left_trigger);
        m.right_trigger = m.right_trigger.max(b.right_trigger);
        (m.ls_x, m.ls_y) = further((m.ls_x, m.ls_y), (b.ls_x, b.ls_y));
        (m.rs_x, m.rs_y) = further((m.rs_x, m.rs_y), (b.rs_x, b.rs_y));
        m
    })
}

fn further(a: (i16, i16), b: (i16, i16)) -> (i16, i16) {
    let r = |(x, y): (i16, i16)| i64::from(x).pow(2) + i64::from(y).pow(2);
    if r(b) > r(a) {
        b
    } else {
        a
    }
}

struct Share {
    /// Each buddy pad's latest state: member, its wire pad, the state.
    posts: Vec<(u64, u8, GamepadFrame)>,
    rumble: RumbleLevels,
}

/// Host-wide buddy posts and lead rumble, one entry per OS slot.
pub struct BuddyHub {
    shares: Mutex<[Share; MAX_PADS]>,
    /// Bumped on every change to a slot's posts: the lead re-writes its device on a move.
    posts_gen: [AtomicU32; MAX_PADS],
    rumble_gen: [AtomicU32; MAX_PADS],
}

impl Default for BuddyHub {
    fn default() -> Self {
        Self::new()
    }
}

impl BuddyHub {
    pub const fn new() -> Self {
        BuddyHub {
            shares: Mutex::new(
                [const {
                    Share {
                        posts: Vec::new(),
                        rumble: (0, 0, 0, 0),
                    }
                }; MAX_PADS],
            ),
            posts_gen: [const { AtomicU32::new(0) }; MAX_PADS],
            rumble_gen: [const { AtomicU32::new(0) }; MAX_PADS],
        }
    }

    /// A fresh member id: one per session input thread, never reused.
    pub fn member() -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    /// `member`'s wire pad `wire` now reads `state` on `slot`.
    pub fn post(&self, slot: u8, member: u64, wire: u8, state: GamepadFrame) {
        let Some(i) = index(slot) else { return };
        let mut shares = self.lock();
        let posts = &mut shares[i].posts;
        match posts.iter_mut().find(|p| p.0 == member && p.1 == wire) {
            Some(p) if p.2 == state => return,
            Some(p) => p.2 = state,
            None => posts.push((member, wire, state)),
        }
        self.posts_gen[i].fetch_add(1, Ordering::Release);
    }

    /// Drop `member`'s posts on every slot: one wire pad, or all of them with `None`.
    pub fn withdraw(&self, member: u64, wire: Option<u8>) {
        let mut shares = self.lock();
        for (i, share) in shares.iter_mut().enumerate() {
            let before = share.posts.len();
            share
                .posts
                .retain(|p| p.0 != member || wire.is_some_and(|w| p.1 != w));
            if share.posts.len() != before {
                self.posts_gen[i].fetch_add(1, Ordering::Release);
            }
        }
    }

    /// Changes whenever a post on `slot` does.
    pub fn posts_gen(&self, slot: u8) -> u32 {
        index(slot).map_or(0, |i| self.posts_gen[i].load(Ordering::Acquire))
    }

    /// [`merge`] of `lead` and every post on `slot`.
    pub fn merge_into(&self, slot: u8, lead: GamepadFrame) -> GamepadFrame {
        let Some(i) = index(slot) else { return lead };
        merge(lead, self.lock()[i].posts.iter().map(|p| p.2))
    }

    /// The lead device's rumble on `slot`.
    pub fn set_rumble(&self, slot: u8, lv: RumbleLevels) {
        let Some(i) = index(slot) else { return };
        let mut shares = self.lock();
        if shares[i].rumble != lv {
            shares[i].rumble = lv;
            self.rumble_gen[i].fetch_add(1, Ordering::Release);
        }
    }

    /// `slot`'s rumble, when it moved since `seen`. `None` in `seen` takes the current level.
    pub fn rumble_since(&self, slot: u8, seen: &mut Option<u32>) -> Option<RumbleLevels> {
        let i = index(slot)?;
        let stamp = self.rumble_gen[i].load(Ordering::Acquire);
        if *seen == Some(stamp) {
            return None;
        }
        *seen = Some(stamp);
        Some(self.lock()[i].rumble)
    }

    /// Recover from poison: every entry stays whole under the lock.
    fn lock(&self) -> std::sync::MutexGuard<'_, [Share; MAX_PADS]> {
        self.shares.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn index(slot: u8) -> Option<usize> {
    (usize::from(slot) < MAX_PADS).then_some(usize::from(slot))
}

pub fn global() -> &'static BuddyHub {
    static HUB: BuddyHub = BuddyHub::new();
    &HUB
}

/// One session's place in a [`BuddyHub`]. Drop withdraws its posts and silences the rumble
/// it led, so a session that ends or unwinds leaves nothing held on another's device.
pub struct BuddyMember {
    hub: &'static BuddyHub,
    pub id: u64,
    /// Slots this session's devices posted rumble on.
    led: u16,
}

impl BuddyMember {
    pub fn new(hub: &'static BuddyHub) -> Self {
        BuddyMember {
            hub,
            id: BuddyHub::member(),
            led: 0,
        }
    }

    pub fn hub(&self) -> &'static BuddyHub {
        self.hub
    }

    /// This session's device on `slot` rumbles at `lv`.
    pub fn lead_rumble(&mut self, slot: u8, lv: RumbleLevels) {
        if index(slot).is_some() {
            self.led |= 1 << slot;
            self.hub.set_rumble(slot, lv);
        }
    }

    /// This session's device on `slot` is going: its buddies stop buzzing.
    pub fn stop_leading(&mut self, slot: u8) {
        if index(slot).is_some() && self.led & (1 << slot) != 0 {
            self.led &= !(1 << slot);
            self.hub.set_rumble(slot, (0, 0, 0, 0));
        }
    }
}

impl Drop for BuddyMember {
    fn drop(&mut self) {
        self.hub.withdraw(self.id, None);
        for slot in 0..MAX_PADS as u8 {
            self.stop_leading(slot);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::input::gamepad::{BTN_A, BTN_B, BTN_PADDLE1};

    fn frame() -> GamepadFrame {
        GamepadFrame {
            index: 0,
            active_mask: 1,
            ..Default::default()
        }
    }

    #[test]
    fn buttons_or_and_triggers_take_the_larger_value() {
        let lead = GamepadFrame {
            buttons: BTN_A,
            left_trigger: 200,
            right_trigger: 10,
            ..frame()
        };
        let buddy = GamepadFrame {
            buttons: BTN_B | BTN_PADDLE1,
            left_trigger: 40,
            right_trigger: 255,
            ..frame()
        };
        let m = merge(lead, [buddy]);
        assert_eq!(m.buttons, BTN_A | BTN_B | BTN_PADDLE1);
        assert_eq!((m.left_trigger, m.right_trigger), (200, 255));
        assert_eq!((m.index, m.active_mask), (lead.index, lead.active_mask));
    }

    #[test]
    fn each_stick_takes_the_input_furthest_from_centre() {
        let lead = GamepadFrame {
            ls_x: 30_000,
            ls_y: 0,
            rs_x: 1_000,
            rs_y: -1_000,
            ..frame()
        };
        let buddy = GamepadFrame {
            ls_x: -10_000,
            ls_y: 10_000,
            rs_x: 0,
            rs_y: i16::MIN,
            ..frame()
        };
        let m = merge(lead, [buddy]);
        assert_eq!(
            (m.ls_x, m.ls_y),
            (30_000, 0),
            "the lead's stick is further out"
        );
        assert_eq!((m.rs_x, m.rs_y), (0, i16::MIN), "x and y move together");
        // A tie keeps the lead.
        let tie = GamepadFrame {
            ls_x: 0,
            ls_y: 30_000,
            ..frame()
        };
        assert_eq!(
            (merge(lead, [tie]).ls_x, merge(lead, [tie]).ls_y),
            (30_000, 0)
        );
    }

    #[test]
    fn a_held_button_lets_go_when_its_buddy_leaves() {
        static HUB: BuddyHub = BuddyHub::new();
        let buddy = BuddyMember::new(&HUB);
        HUB.post(
            2,
            buddy.id,
            0,
            GamepadFrame {
                buttons: BTN_A,
                right_trigger: 255,
                ..frame()
            },
        );
        let stamp = HUB.posts_gen(2);
        assert_eq!(HUB.merge_into(2, frame()).buttons, BTN_A);
        assert_eq!(
            HUB.merge_into(1, frame()).buttons,
            0,
            "another slot sees nothing"
        );
        drop(buddy);
        assert_ne!(HUB.posts_gen(2), stamp, "the lead must hear the change");
        let m = HUB.merge_into(2, frame());
        assert_eq!((m.buttons, m.right_trigger), (0, 0));
    }

    #[test]
    fn two_buddies_on_one_button_hold_it_until_both_let_go() {
        static HUB: BuddyHub = BuddyHub::new();
        let (a, b) = (BuddyMember::new(&HUB), BuddyMember::new(&HUB));
        let pressed = GamepadFrame {
            buttons: BTN_A,
            ..frame()
        };
        HUB.post(0, a.id, 0, pressed);
        HUB.post(0, b.id, 0, pressed);
        HUB.post(0, a.id, 0, frame());
        assert_eq!(
            HUB.merge_into(0, frame()).buttons,
            BTN_A,
            "b still holds it"
        );
        HUB.post(0, b.id, 0, frame());
        assert_eq!(HUB.merge_into(0, frame()).buttons, 0);
        // An unplugged pad withdraws alone; the member's other pad stays.
        HUB.post(0, a.id, 1, pressed);
        HUB.post(
            0,
            a.id,
            0,
            GamepadFrame {
                buttons: BTN_B,
                ..frame()
            },
        );
        HUB.withdraw(a.id, Some(1));
        assert_eq!(HUB.merge_into(0, frame()).buttons, BTN_B);
    }

    #[test]
    fn an_unchanged_post_does_not_wake_the_lead() {
        static HUB: BuddyHub = BuddyHub::new();
        let a = BuddyMember::new(&HUB);
        HUB.post(3, a.id, 0, frame());
        let stamp = HUB.posts_gen(3);
        HUB.post(3, a.id, 0, frame());
        assert_eq!(HUB.posts_gen(3), stamp);
    }

    #[test]
    fn a_leaving_lead_silences_its_buddies() {
        static HUB: BuddyHub = BuddyHub::new();
        let mut lead = BuddyMember::new(&HUB);
        let mut seen = None;
        assert_eq!(
            HUB.rumble_since(1, &mut seen),
            Some((0, 0, 0, 0)),
            "a joiner reads the level"
        );
        lead.lead_rumble(1, (9, 9, 0, 0));
        assert_eq!(HUB.rumble_since(1, &mut seen), Some((9, 9, 0, 0)));
        assert_eq!(HUB.rumble_since(1, &mut seen), None, "once per change");
        drop(lead);
        assert_eq!(HUB.rumble_since(1, &mut seen), Some((0, 0, 0, 0)));
    }
}
