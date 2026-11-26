//! Desk pads lent to a seat on Windows: which session may open each lent USB controller.
//!
//! The ledger says which row holds a pad. The supervisor says which session each seat runs in.
//! [`Lent`] combines the two into one [`Holder`] per USB device and has the service's
//! [`Hardware`] apply it:
//!
//! - the seat's session while the seat runs,
//! - [`Holder::Parked`] while it doesn't,
//! - the desk once no row holds the pad.
//!
//! A change of holder restarts the device, which closes whatever the last holder had open. One
//! change happens in place: a parked pad going live, because nobody could list a parked pad.
//!
//! A pad's key is its USB ids plus the serial or port id Windows names it by, so the key names
//! the same pad after a replug and after a reboot. Which pads may be lent is read from the
//! hardware, never taken from the caller. Nothing here uses an OS type, so the tests run on
//! every target.

// Off Windows only the tests read this module.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::backend::{no_lending, pad_handover, pad_not_a_controller, pad_not_present};
use crate::backend::{BackendError, PresentPad};
use crate::model::{pad_interface_allowed, pad_name, valid_pad_key, Ledger, Seat};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

/// Whose listings a lent USB device shows up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    Desk,
    /// Nobody's: its seat doesn't run.
    Parked,
    /// The seat that runs in this session.
    Session(u32),
}

/// A USB device plugged into the box, as [`Hardware`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbPad {
    /// `USB\VID_…&PID_…\<serial or port id>`.
    pub instance: String,
    pub vendor_id: u16,
    pub product_id: u16,
    /// What the device calls itself on the bus.
    pub name: String,
    /// Class, subclass and protocol of each interface.
    pub interfaces: Vec<[u8; 3]>,
    /// A game controller's HID collection or an XUSB interface lies under it.
    pub controller: bool,
}

/// The box's USB devices and the fence over them. The service supplies it.
pub trait Hardware: Send + Sync + 'static {
    fn present(&self) -> Vec<UsbPad>;

    /// Fences the device `instance` to `holder`. With `restart` the device restarts first, so
    /// every handle on it dies. One that isn't plugged in is the holder's when it arrives.
    fn hand(&self, instance: &str, holder: Holder, restart: bool) -> Result<(), String>;
}

/// The lends as last handed to the hardware, and each running seat's session.
#[derive(Default)]
pub struct Lent {
    hardware: OnceLock<Arc<dyn Hardware>>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Each lent key's seat, and the holder its device was last handed to.
    lends: BTreeMap<String, (String, Holder)>,
    /// The session of each seat that runs, by seat id.
    sessions: BTreeMap<String, u32>,
}

impl State {
    fn holder(&self, seat: &str) -> Holder {
        match self.sessions.get(seat) {
            Some(&session) => Holder::Session(session),
            None => Holder::Parked,
        }
    }
}

impl Lent {
    /// Gives lending its hardware and parks every pad `ledger` lends: no seat runs yet, and
    /// the desk may have opened a pad while the service was down, so each one restarts.
    pub fn equip(&self, hardware: Arc<dyn Hardware>, ledger: &Ledger) {
        if self.hardware.set(hardware).is_err() {
            return;
        }
        let mut state = self.lock();
        for seat in &ledger.seats {
            for pad in &seat.lent {
                let id = seat.id.to_string();
                self.give_anyway(&mut state, &pad.key, &id, Holder::Parked, true);
            }
        }
    }

    /// [`Self::give`], and in place when the restart is refused: the listings still change
    /// hands, and only a handle the last holder already had open stays.
    fn give_anyway(&self, state: &mut State, key: &str, seat: &str, holder: Holder, restart: bool) {
        if !self.give(state, key, seat, holder, restart) && restart {
            self.give(state, key, seat, holder, false);
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn hardware(&self) -> Result<&Arc<dyn Hardware>, BackendError> {
        self.hardware.get().ok_or_else(no_lending)
    }

    /// Hands `key`'s device to `holder` and notes it for `seat`. A refusal is logged and
    /// answered `false`, with the note left as it was.
    fn give(
        &self,
        state: &mut State,
        key: &str,
        seat: &str,
        holder: Holder,
        restart: bool,
    ) -> bool {
        let (Some(hardware), Some(instance)) = (self.hardware.get(), instance_of(key)) else {
            return false;
        };
        if let Err(error) = hardware.hand(&instance, holder, restart) {
            tracing::warn!(key, %error, ?holder, "desk pad not handed over");
            return false;
        }
        match holder {
            Holder::Desk => state.lends.remove(key),
            _ => state
                .lends
                .insert(key.to_owned(), (seat.to_owned(), holder)),
        };
        true
    }

    /// Every plugged-in device that may be lent, and every plugged-in one a row of `ledger`
    /// lends, each under the key its instance id makes.
    pub fn desk_pads(&self, ledger: &Ledger) -> Result<Vec<PresentPad>, BackendError> {
        let listed = |usb: &UsbPad| {
            let key = key_of(&usb.instance)?;
            (ledger.holder_of(&key).is_some() || lendable(usb)).then(|| pad(usb, key))
        };
        Ok(self
            .hardware()?
            .present()
            .iter()
            .filter_map(listed)
            .collect())
    }

    /// The plugged-in pad `key` names, through the gate.
    pub fn lendable_pad(&self, key: &str) -> Result<PresentPad, BackendError> {
        let present = self.hardware()?.present();
        let named = |usb: &&UsbPad| key_of(&usb.instance).as_deref() == Some(key);
        let Some(usb) = present.iter().find(named) else {
            return Err(pad_not_present());
        };
        if !lendable(usb) {
            return Err(pad_not_a_controller());
        }
        Ok(pad(usb, key.to_owned()))
    }

    /// Hands every pad whose row in `ledger` differs from the last hand-over to its holder
    /// now: its seat's session, parked, or the desk when no row lends it. Each one restarts.
    pub fn settle(&self, ledger: &Ledger) -> Result<(), BackendError> {
        self.hardware()?;
        let mut state = self.lock();
        let want: BTreeMap<&str, String> = ledger
            .seats
            .iter()
            .flat_map(|seat| {
                let id = seat.id.to_string();
                seat.lent
                    .iter()
                    .map(move |pad| (pad.key.as_str(), id.clone()))
            })
            .collect();
        let gone: Vec<String> = (state.lends.keys())
            .filter(|key| !want.contains_key(key.as_str()))
            .cloned()
            .collect();
        for key in gone {
            if !self.give(&mut state, &key, "", Holder::Desk, true) {
                return Err(pad_handover());
            }
        }
        for (key, seat) in want {
            let holder = state.holder(&seat);
            let as_handed = state.lends.get(key) == Some(&(seat.clone(), holder));
            if !as_handed && !self.give(&mut state, key, &seat, holder, true) {
                return Err(pad_handover());
            }
        }
        Ok(())
    }

    /// `seat` runs in `session` now, or no longer runs. Its pads follow: live in place when
    /// they were parked, parked with a restart when the seat ends.
    pub fn seat_session(&self, seat: &str, session: Option<u32>) {
        let mut state = self.lock();
        match session {
            Some(session) => state.sessions.insert(seat.to_owned(), session),
            None => state.sessions.remove(seat),
        };
        let holder = state.holder(seat);
        let stale: Vec<(String, Holder)> = (state.lends.iter())
            .filter(|(_, (of, was))| of == seat && *was != holder)
            .map(|(key, (_, was))| (key.clone(), *was))
            .collect();
        for (key, was) in stale {
            let restart = was != Holder::Parked;
            self.give_anyway(&mut state, &key, seat, holder, restart);
        }
    }

    /// `seat`'s row is gone: its pads are the desk's again.
    pub fn release(&self, seat: &Seat) {
        let mut state = self.lock();
        for pad in &seat.lent {
            if state.lends.contains_key(&pad.key) {
                self.give_anyway(&mut state, &pad.key, "", Holder::Desk, true);
            }
        }
    }
}

/// Whether `usb` may be lent: a game controller whose every interface is a pad's
/// ([`pad_interface_allowed`]). A hub, a disk and a network card fail the second half.
fn lendable(usb: &UsbPad) -> bool {
    let pads_only = !usb.interfaces.is_empty()
        && (usb.interfaces.iter()).all(|&[c, s, p]| pad_interface_allowed(c, s, p));
    usb.controller && pads_only
}

fn pad(usb: &UsbPad, key: String) -> PresentPad {
    PresentPad {
        key,
        name: pad_name(&usb.name),
        vendor_id: usb.vendor_id,
        product_id: usb.product_id,
        port: None,
    }
}

fn hex4(text: &str) -> bool {
    text.len() == 4 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The key of the USB device `instance`, which has the form `USB\VID_vvvv&PID_pppp\<tail>`. The
/// key is the device's ids, followed by one of:
///
/// - `s` and the tail, when the tail is a serial that a key can carry
/// - `p` and the tail, when the tail is the port id that Windows made. Each `&` in it is
///   written as `.`.
///
/// A tail that is neither gives no key. The tail is upper-cased, because Windows reports one id
/// in either case and finds it by both.
fn key_of(instance: &str) -> Option<String> {
    let mut parts = instance.split('\\');
    let (bus, ids, tail) = (parts.next()?, parts.next()?, parts.next()?);
    let ids = ids.to_ascii_lowercase();
    let (vendor, product) = ids.strip_prefix("vid_")?.split_once("&pid_")?;
    let plain = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-');
    let (kind, tail) = if tail.bytes().all(|b| plain(b) || b == b'.') {
        ('s', tail.to_ascii_uppercase())
    } else if tail.bytes().all(|b| plain(b) || b == b'&') {
        ('p', tail.replace('&', ".").to_ascii_uppercase())
    } else {
        return None;
    };
    let device = bus.eq_ignore_ascii_case("USB") && parts.next().is_none() && !tail.is_empty();
    let key = format!("usb-{vendor}-{product}-{kind}-{tail}");
    (device && hex4(vendor) && hex4(product) && valid_pad_key(&key)).then_some(key)
}

/// The instance id [`key_of`] made `key` from.
fn instance_of(key: &str) -> Option<String> {
    let mut parts = key.strip_prefix("usb-")?.splitn(4, '-');
    let (vendor, product) = (parts.next()?, parts.next()?);
    let tail = match (parts.next()?, parts.next()?) {
        ("s", tail) => tail.to_owned(),
        ("p", tail) => tail.replace('.', "&"),
        _ => return None,
    };
    let (vendor, product) = (vendor.to_ascii_uppercase(), product.to_ascii_uppercase());
    (hex4(&vendor) && hex4(&product) && !tail.is_empty())
        .then(|| format!("USB\\VID_{vendor}&PID_{product}\\{tail}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{LentPad, RowKind, RuntimeStatus, SeatId};

    const PUCK: &str = "USB\\VID_28DE&PID_1304\\FXB995480095E";
    const PUCK_KEY: &str = "usb-28de-1304-s-FXB995480095E";
    const DUALSENSE: &str = "USB\\VID_054C&PID_0CE6\\7&3F94D01&0&2";
    const DUALSENSE_KEY: &str = "usb-054c-0ce6-p-7.3F94D01.0.2";

    /// Hardware that lists what it is given and writes every hand-over down.
    #[derive(Default)]
    struct Desk {
        present: Vec<UsbPad>,
        handed: Mutex<Vec<(String, Holder, bool)>>,
        refuse: bool,
    }

    impl Hardware for Desk {
        fn present(&self) -> Vec<UsbPad> {
            self.present.clone()
        }

        fn hand(&self, instance: &str, holder: Holder, restart: bool) -> Result<(), String> {
            if self.refuse && restart {
                return Err("removal refused".into());
            }
            let mut handed = self.handed.lock().unwrap();
            handed.push((instance.to_owned(), holder, restart));
            Ok(())
        }
    }

    fn usb(instance: &str, interfaces: &[[u8; 3]], controller: bool) -> UsbPad {
        UsbPad {
            instance: instance.to_owned(),
            vendor_id: 0x28DE,
            product_id: 0x1304,
            name: " Steam Controller Puck\u{0} ".to_owned(),
            interfaces: interfaces.to_vec(),
            controller,
        }
    }

    fn seat(keys: &[&str]) -> Seat {
        let lent = |key: &&str| LentPad {
            key: (*key).to_owned(),
            name: "Pad".to_owned(),
            vendor_id: 0,
            product_id: 0,
            port: None,
        };
        Seat {
            id: SeatId::random(),
            name: "test".into(),
            account: "pfs-test".into(),
            display_slot: 12,
            native_port: 9779,
            mgmt_port: 47995,
            autostart: false,
            runtime: RuntimeStatus::stopped(),
            kind: RowKind::made(),
            fingerprint: None,
            lent: keys.iter().map(lent).collect(),
        }
    }

    fn ledger(seats: Vec<Seat>) -> Ledger {
        let mut ledger = Ledger::new();
        ledger.seats = seats;
        ledger
    }

    fn lent(desk: Desk, ledger: &Ledger) -> (Lent, Arc<Desk>) {
        let desk = Arc::new(desk);
        let lent = Lent::default();
        lent.equip(desk.clone(), ledger);
        (lent, desk)
    }

    fn handed(desk: &Desk) -> Vec<(String, Holder, bool)> {
        std::mem::take(&mut *desk.handed.lock().unwrap())
    }

    #[test]
    fn a_key_is_the_ids_and_the_serial_or_the_port_id_and_reads_back() {
        assert_eq!(key_of(PUCK).as_deref(), Some(PUCK_KEY));
        assert_eq!(key_of(DUALSENSE).as_deref(), Some(DUALSENSE_KEY));
        assert_eq!(instance_of(PUCK_KEY).as_deref(), Some(PUCK));
        assert_eq!(instance_of(DUALSENSE_KEY).as_deref(), Some(DUALSENSE));
        assert_eq!(
            key_of("usb\\vid_045e&pid_0b12\\3032363030303631").as_deref(),
            Some("usb-045e-0b12-s-3032363030303631")
        );
        // One device, reported in either case, has one key.
        let lower = key_of("USB\\VID_054C&PID_0CE6\\7&3f94d01&0&2");
        assert_eq!(lower.as_deref(), Some(DUALSENSE_KEY));
        assert_eq!(
            key_of("USB\\VID_28DE&PID_1304\\fxb995480095e").as_deref(),
            Some(PUCK_KEY)
        );
        for none in [
            "USB\\VID_054C&PID_0CE6&MI_03\\8&185D5297&4&0003",
            "USB\\ROOT_HUB30\\9&2EC1B4C8&0&0",
            "HID\\VID_054C&PID_0CE6\\9&25E0AEA4&0&0000",
            "USB\\VID_054C&PID_0CE6\\",
            "USB\\VID_054C&PID_0CE6\\a b",
            "USB\\VID_054C&PID_0CE6\\a.b&c",
            "USB\\VID_054C&PID_0CE6\\A\\B",
        ] {
            assert_eq!(key_of(none), None, "{none}");
        }
        for none in [
            "usb-054c-0ce6-x-1",
            "usb-054c-0ce6-s-",
            "usb-54c-0ce6-s-1",
            "hid-1",
        ] {
            assert_eq!(instance_of(none), None, "{none}");
        }
    }

    #[test]
    fn only_a_controller_with_a_pad_s_interfaces_is_listed_and_lendable() {
        let hid = [3, 0, 0];
        let desk = Desk {
            present: vec![
                usb(PUCK, &[[2, 2, 1], [0x0a, 0, 0], hid], true),
                usb("USB\\VID_0DB0&PID_0076\\7E7025010901", &[hid], false),
                usb("USB\\VID_0781&PID_5581\\DISK1", &[hid, [8, 6, 0x50]], true),
                usb("USB\\VID_05E3&PID_0610\\HUB1", &[[9, 0, 1]], true),
                usb("USB\\VID_1234&PID_5678\\NONE", &[], true),
            ],
            ..Desk::default()
        };
        let empty = ledger(Vec::new());
        let (lent, _) = lent(desk, &empty);
        let listed = lent.desk_pads(&empty).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].key, PUCK_KEY);
        assert_eq!(listed[0].name, "Steam Controller Puck");
        assert_eq!(listed[0].port, None);
        assert_eq!(lent.lendable_pad(PUCK_KEY).unwrap().key, PUCK_KEY);
        let refused = |key: &str| lent.lendable_pad(key).unwrap_err().code;
        assert_eq!(
            refused("usb-0db0-0076-s-7E7025010901"),
            "pad_not_a_controller"
        );
        assert_eq!(refused("usb-0781-5581-s-DISK1"), "pad_not_a_controller");
        assert_eq!(refused("usb-05e3-0610-s-HUB1"), "pad_not_a_controller");
        assert_eq!(refused(DUALSENSE_KEY), "pad_not_present");
    }

    #[test]
    fn a_lent_pad_stays_listed_when_it_no_longer_passes_the_gate() {
        let desk = Desk {
            present: vec![usb(PUCK, &[[3, 0, 0]], false)],
            ..Desk::default()
        };
        let holds = ledger(vec![seat(&[PUCK_KEY])]);
        let (lent, _) = lent(desk, &holds);
        assert_eq!(lent.desk_pads(&holds).unwrap().len(), 1);
        assert!(lent.desk_pads(&ledger(Vec::new())).unwrap().is_empty());
    }

    #[test]
    fn without_hardware_lending_is_unsupported() {
        let lent = Lent::default();
        let empty = ledger(Vec::new());
        assert_eq!(lent.desk_pads(&empty).unwrap_err().code, "pads_unsupported");
        assert_eq!(
            lent.lendable_pad(PUCK_KEY).unwrap_err().code,
            "pads_unsupported"
        );
        assert_eq!(lent.settle(&empty).unwrap_err().code, "pads_unsupported");
        lent.seat_session("a", Some(3));
    }

    #[test]
    fn the_service_s_start_parks_every_lent_pad_with_a_restart() {
        let holds = ledger(vec![seat(&[PUCK_KEY, DUALSENSE_KEY])]);
        let (_, desk) = lent(Desk::default(), &holds);
        let mut got = handed(&desk);
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            [
                (DUALSENSE.to_owned(), Holder::Parked, true),
                (PUCK.to_owned(), Holder::Parked, true),
            ]
        );
    }

    #[test]
    fn a_lend_a_move_and_a_take_back_each_restart_the_pad() {
        let (a, b) = (seat(&[]), seat(&[]));
        let (ida, idb) = (a.id.to_string(), b.id.to_string());
        let (lent, desk) = lent(Desk::default(), &ledger(vec![a.clone(), b.clone()]));
        assert!(handed(&desk).is_empty());

        // Lent to a stopped seat: parked.
        let mut holder = a.clone();
        holder.lent = seat(&[PUCK_KEY]).lent;
        let first = ledger(vec![holder.clone(), b.clone()]);
        lent.settle(&first).unwrap();
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Parked, true)]);
        // The same ledger again changes nothing.
        lent.settle(&first).unwrap();
        assert!(handed(&desk).is_empty());

        // Its seat starts: live in place. It ends: parked, with a restart.
        lent.seat_session(&ida, Some(7));
        assert_eq!(
            handed(&desk),
            [(PUCK.to_owned(), Holder::Session(7), false)]
        );
        lent.seat_session(&ida, Some(7));
        assert!(handed(&desk).is_empty());
        lent.seat_session(&ida, None);
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Parked, true)]);

        // Moved to a running seat: that seat's session, with a restart.
        lent.seat_session(&idb, Some(9));
        assert!(handed(&desk).is_empty(), "the other seat holds nothing");
        let mut taker = b.clone();
        taker.lent = holder.lent.clone();
        let moved = ledger(vec![a.clone(), taker.clone()]);
        lent.settle(&moved).unwrap();
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Session(9), true)]);

        // A new session of the same seat: its pad restarts into it.
        lent.seat_session(&idb, Some(11));
        assert_eq!(
            handed(&desk),
            [(PUCK.to_owned(), Holder::Session(11), true)]
        );

        // Taken back: the desk's, with a restart.
        lent.settle(&ledger(vec![a, b])).unwrap();
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Desk, true)]);
        lent.seat_session(&idb, None);
        assert!(handed(&desk).is_empty());
    }

    #[test]
    fn a_removed_row_s_pads_go_back_to_the_desk() {
        let holder = seat(&[PUCK_KEY]);
        let (lent, desk) = lent(Desk::default(), &ledger(vec![holder.clone()]));
        handed(&desk);
        lent.release(&holder);
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Desk, true)]);
        lent.release(&holder);
        assert!(handed(&desk).is_empty());
    }

    #[test]
    fn a_refused_hand_over_fails_the_settle_and_is_tried_again() {
        let refusing = Desk {
            refuse: true,
            ..Desk::default()
        };
        let (lent, desk) = lent(refusing, &ledger(Vec::new()));
        let holds = ledger(vec![seat(&[PUCK_KEY])]);
        assert_eq!(lent.settle(&holds).unwrap_err().code, "pad_handover");
        assert!(handed(&desk).is_empty());
        // Nothing was noted, so putting the ledger back asks for nothing.
        lent.settle(&ledger(Vec::new())).unwrap();
        assert!(handed(&desk).is_empty());
    }

    #[test]
    fn a_seat_s_pads_follow_it_in_place_when_the_restart_is_refused() {
        let refusing = Desk {
            refuse: true,
            ..Desk::default()
        };
        let holder = seat(&[PUCK_KEY]);
        let id = holder.id.to_string();
        // The service's start parks it in place.
        let (lent, desk) = lent(refusing, &ledger(vec![holder.clone()]));
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Parked, false)]);
        lent.seat_session(&id, Some(4));
        assert_eq!(
            handed(&desk),
            [(PUCK.to_owned(), Holder::Session(4), false)]
        );
        lent.seat_session(&id, None);
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Parked, false)]);
        lent.release(&holder);
        assert_eq!(handed(&desk), [(PUCK.to_owned(), Holder::Desk, false)]);
    }
}
