//! Live per-session gamepad tap for the console's Controllers page.
//!
//! The input thread already folds every wire event into the frame it hands the virtual
//! pad ([`crate::native::input`]); this publishes that same frame. Exact by construction:
//! what the host applies, not what a client says it sent.
//!
//! One feed per session, carried on [`crate::session_status::SessionControls`], so a
//! stream opened on one id can never see another session's pads. Deliberately NOT the
//! lifecycle ring: a stick sweep is ~250 Hz and would evict `session.started` and
//! friends out of a 1024-entry ring in seconds.
//!
//! Idle unless a console is attached — with the page closed [`PadFeed::publish`] only
//! reads the channel's receiver count, and nothing is built. The [`PadFeed::roster`] beside it
//! is always kept: an emulator's players are written from it.

use punktfunk_core::config::GamepadPref;
use punktfunk_core::input::MAX_PADS;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use tokio::sync::broadcast;
use utoipa::ToSchema;

/// Per-subscriber depth before a console too slow for the pad's own rate is dropped.
/// Same policy as `GET /events`: lag, never buffer.
const FEED_CAPACITY: usize = 256;

/// One pad's whole state as the host holds it — the `data:` of one SSE frame.
///
/// Whole state, not a delta: a page that attaches mid-press draws what is held without
/// replaying history, and the console derives its event log by diffing consecutive frames.
#[derive(Serialize, ToSchema, Clone, Debug, PartialEq, Eq)]
pub struct PadFrame {
    /// Wire index — the number this client gave the pad.
    pub pad: u8,
    /// Unix milliseconds ([`crate::events::HostEvent`] convention).
    pub ts_ms: u64,
    /// The virtual controller the host built: `xbox360`, `dualsense`, `steamdeck`, …
    pub device: String,
    /// What the client declared at arrival, when it declared one. Differs from `device`
    /// where the build cannot construct that kind and folded it into one it can.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared: Option<String>,
    /// Host-wide OS slot — the identity every per-pad host resource is named by
    /// (mailbox, `SwDeviceCreate` instance, pairing MAC). Absent before the first
    /// frame builds the device.
    #[schema(value_type = u8, required = false)]
    pub slot: Option<u8>,
    /// `false` is the unplug frame: the host holds no device at this index any more.
    pub present: bool,
    /// `punktfunk_core::input::gamepad::BTN_*` mask, as applied.
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    pub ls_x: i16,
    pub ls_y: i16,
    pub rs_x: i16,
    pub rs_y: i16,
}

/// One session's pad tap. Producer is that session's input thread; consumers are
/// `GET /session/{id}/pads` streams.
#[derive(Debug)]
pub struct PadFeed {
    tx: broadcast::Sender<PadFrame>,
    /// A console just attached and wants every live pad re-published.
    resync: AtomicBool,
    /// Per wire pad: its device kind + 1 in the low byte (0 = no pad), its OS slot + 1 in the
    /// high byte (0 = not built yet). One word, so a reader never pairs one pad's kind with
    /// another moment's slot.
    roster: [AtomicU16; MAX_PADS],
}

/// One pad of [`PadFeed::roster`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RosterPad {
    /// Wire index.
    pub pad: u8,
    /// The device the host built, or will build: a declared kind as this host resolves it.
    pub kind: GamepadPref,
    /// Host-wide OS slot, once the first frame built the device.
    pub slot: Option<u8>,
}

impl Default for PadFeed {
    fn default() -> PadFeed {
        PadFeed::new()
    }
}

impl PadFeed {
    pub fn new() -> PadFeed {
        PadFeed {
            tx: broadcast::channel(FEED_CAPACITY).0,
            resync: AtomicBool::new(false),
            roster: std::array::from_fn(|_| AtomicU16::new(0)),
        }
    }

    /// Wire pad `pad` is now `kind` at `slot`, or gone with `None`.
    pub fn note_pad(&self, pad: usize, entry: Option<(GamepadPref, Option<u8>)>) {
        let word = entry.map_or(0, |(kind, slot)| {
            let slot = slot.map_or(0, |s| u16::from(s) + 1);
            (u16::from(kind.to_u8()) + 1) | (slot << 8)
        });
        if let Some(cell) = self.roster.get(pad) {
            cell.store(word, Ordering::Relaxed);
        }
    }

    /// The session's pads in wire order: declared at arrival, or built by a first frame.
    pub fn roster(&self) -> Vec<RosterPad> {
        (0..MAX_PADS)
            .filter_map(|pad| {
                let word = self.roster[pad].load(Ordering::Relaxed);
                let kind = (word & 0xff).checked_sub(1)?;
                Some(RosterPad {
                    pad: pad as u8,
                    kind: GamepadPref::from_u8(kind as u8),
                    slot: (word >> 8).checked_sub(1).map(|s| s as u8),
                })
            })
            .collect()
    }

    /// Attach a console, and ask the input thread for a full picture: a held button
    /// sends no further frames, so without this the page stays blank until the next press.
    pub fn subscribe(&self) -> broadcast::Receiver<PadFrame> {
        let rx = self.tx.subscribe();
        self.resync.store(true, Ordering::Relaxed);
        rx
    }

    /// Whether anyone is on the page. The whole cost of this feature with nobody watching.
    pub fn watching(&self) -> bool {
        self.tx.receiver_count() > 0
    }

    /// Consume a pending resync request. Short-circuits on [`Self::watching`], so a
    /// closed page costs the input thread one receiver-count check per wake.
    pub fn take_resync(&self) -> bool {
        self.watching() && self.resync.swap(false, Ordering::Relaxed)
    }

    /// Publish one pad. The frame is not built with nobody watching, so the `String`
    /// never allocates on an idle host.
    pub fn publish(&self, frame: impl FnOnce() -> PadFrame) {
        if self.watching() {
            let _ = self.tx.send(frame());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(pad: u8, buttons: u32) -> PadFrame {
        PadFrame {
            pad,
            ts_ms: 1,
            device: "xbox360".into(),
            declared: None,
            slot: Some(0),
            present: true,
            buttons,
            left_trigger: 0,
            right_trigger: 0,
            ls_x: 0,
            ls_y: 0,
            rs_x: 0,
            rs_y: 0,
        }
    }

    /// The hard requirement: a console that is not on the Controllers page costs the
    /// input thread nothing — the frame is never even built.
    #[test]
    fn nobody_watching_means_nothing_is_built() {
        let feed = PadFeed::new();
        let mut built = 0;
        feed.publish(|| {
            built += 1;
            frame(0, 1)
        });
        assert_eq!(built, 0, "an unwatched feed must not build a frame");
        assert!(!feed.watching());
        assert!(!feed.take_resync(), "no subscriber, nothing to resync for");

        let _rx = feed.subscribe();
        feed.publish(|| {
            built += 1;
            frame(0, 1)
        });
        assert_eq!(built, 1);
    }

    /// Attaching arms exactly one resync; the input thread takes it and moves on.
    #[test]
    fn a_fresh_subscriber_arms_one_resync() {
        let feed = PadFeed::new();
        let _rx = feed.subscribe();
        assert!(feed.take_resync());
        assert!(!feed.take_resync(), "taken once, not on every wake");
    }

    /// Bounded: past [`FEED_CAPACITY`] the slowest consumer lags and is dropped rather
    /// than the host buffering a stick sweep for it.
    #[tokio::test]
    async fn a_consumer_that_cannot_keep_up_lags_instead_of_growing_the_host() {
        let feed = PadFeed::new();
        let mut rx = feed.subscribe();
        for i in 0..(FEED_CAPACITY + 8) {
            feed.publish(|| frame(0, i as u32));
        }
        assert!(
            matches!(rx.recv().await, Err(broadcast::error::RecvError::Lagged(_))),
            "an overrun consumer must lag, not be buffered"
        );
    }

    /// The roster keeps each pad's kind and slot in wire order, a pad not built yet without a
    /// slot, and drops a pad that left.
    #[test]
    fn the_roster_holds_each_pad_until_it_leaves() {
        let feed = PadFeed::new();
        assert!(feed.roster().is_empty());
        feed.note_pad(3, Some((GamepadPref::SwitchPro, None)));
        feed.note_pad(0, Some((GamepadPref::SteamDeck, Some(2))));
        let pad = |pad, kind, slot| RosterPad { pad, kind, slot };
        assert_eq!(
            feed.roster(),
            [
                pad(0, GamepadPref::SteamDeck, Some(2)),
                pad(3, GamepadPref::SwitchPro, None)
            ]
        );
        feed.note_pad(0, None);
        feed.note_pad(MAX_PADS, Some((GamepadPref::DualSense, Some(0))));
        assert_eq!(feed.roster(), [pad(3, GamepadPref::SwitchPro, None)]);
    }

    /// Two sessions hold two feeds, so a stream on one id cannot see the other's pads.
    #[tokio::test]
    async fn one_session_never_sees_another_sessions_pads() {
        let (a, b) = (PadFeed::new(), PadFeed::new());
        let mut rx_a = a.subscribe();
        let mut rx_b = b.subscribe();
        a.publish(|| frame(0, 0xAA));
        b.publish(|| frame(0, 0xBB));

        assert_eq!(rx_a.recv().await.unwrap().buttons, 0xAA);
        assert_eq!(rx_b.recv().await.unwrap().buttons, 0xBB);
        assert!(rx_a.try_recv().is_err(), "A's stream holds only A's pads");
        assert!(rx_b.try_recv().is_err(), "B's stream holds only B's pads");
    }
}
