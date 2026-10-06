//! Raw passthrough for a Steam Controller 2 held by an SDL slot. SDL's Triton driver still
//! feeds the typed plane (escape chord, ring, menus); a second hidapi handle on the same node
//! forwards every input report verbatim to the host's as-is `28DE:1302` pad and replays Steam's
//! writes on the physical controller. Trackpads, gyro and haptics exist only on this path: the
//! host's typed fallback carries buttons, sticks and triggers.
//!
//! Same contract as Android's `Sc2Capture` and Apple's: the IMU gate, wireless reports kept
//! off the wire, writes forwarded unchanged.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::quic::{RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT, HID_REPORT_MAX};
use sdl3::sys::hidapi as hid;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

/// SDL `ETritonReportIDTypes`.
const ID_STATE: u8 = 0x42;
const ID_STATE_BLE: u8 = 0x45;
const ID_WIRELESS_X: u8 = 0x46;
const ID_STATE_TIMESTAMP: u8 = 0x47;
const ID_WIRELESS: u8 = 0x79;

/// SDL `TritonButtons` bits that [`Gate::system_forward`] keeps local.
const BTN_QAM: u32 = 0x0000_0010;
const BTN_STEAM: u32 = 0x0001_0000;
/// SDL `TritonButtons` bits of the ring chord, Select (View ⧉) then A. SDL's enum calls `0x4000`
/// MENU, but drives BACK from it, as hid-steam does.
const BTN_A: u32 = 0x0000_0001;
const BTN_VIEW: u32 = 0x0000_4000;

/// A queued host write waits at most this long behind a read.
const READ_TIMEOUT_MS: i32 = 4;

/// `SETTING_ENABLE_RAW_JOYSTICK` off: a pad left in raw mode reports ADC stick values that
/// read as a few percent of travel. Steam sends the same at init; SDL does not.
const NORMALIZE_JOYSTICKS: [u8; 64] = {
    let mut r = [0u8; 64];
    r[0] = 0x01; // feature report id
    r[1] = 0x87; // ID_SET_SETTINGS_VALUES
    r[2] = 3; // one {u8 num, u16 value}
    r[3] = 0x2E; // SETTING_ENABLE_RAW_JOYSTICK, value 0
    r
};

/// Wired `1302` and BLE `1303` pads are the controller itself; `1304`/`1305` are Puck dongles.
pub(crate) fn pref_for(vid: u16, pid: u16) -> Option<GamepadPref> {
    match (vid, pid) {
        (0x28DE, 0x1302 | 0x1303) => Some(GamepadPref::SteamController2),
        (0x28DE, 0x1304 | 0x1305) => Some(GamepadPref::SteamController2Puck),
        _ => None,
    }
}

pub(crate) fn is_sc2(pref: GamepadPref) -> bool {
    matches!(
        pref,
        GamepadPref::SteamController2 | GamepadPref::SteamController2Puck
    )
}

#[derive(Clone, Copy)]
pub(crate) struct Gate {
    /// Overlay owns the pad: state reports go out neutral so nothing stays held on the host.
    pub masked: bool,
    /// Off: Steam and QAM stay with the local shell, as on the typed plane.
    pub system_forward: bool,
    /// The typed plane opens the ring on Select+A, so those presses stay off the wire.
    pub chords: bool,
}

enum Cmd {
    Write(u8, Vec<u8>),
    Gate(Gate),
}

/// The reader thread owns the handle; dropping this hangs up and joins it.
pub(crate) struct Sc2Capture {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Sc2Capture {
    /// `path` is the SDL slot's HID path. `None` when the node will not open (not a HIDAPI
    /// device, no permission): the slot keeps the typed plane.
    pub(crate) fn open(
        path: &str,
        client: Arc<NativeClient>,
        pad: u8,
        gate: Gate,
    ) -> Option<Sc2Capture> {
        let Some(dev) = Dev::open(path) else {
            tracing::warn!(path, error = %sdl3::get_error(), "open steam controller 2 hid node");
            return None;
        };
        dev.send_feature(&NORMALIZE_JOYSTICKS);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pf-sc2-raw".into())
            .spawn(move || run(dev, &rx, &client, pad, gate))
            .map_err(|e| tracing::warn!(error = %e, "spawn steam controller 2 reader"))
            .ok()?;
        Some(Sc2Capture {
            tx,
            thread: Some(thread),
        })
    }

    /// Host `HidOutput::HidRaw`: `data` is the full report, id first.
    pub(crate) fn write(&self, kind: u8, data: Vec<u8>) {
        let _ = self.tx.send(Cmd::Write(kind, data));
    }

    pub(crate) fn set_gate(&self, gate: Gate) {
        let _ = self.tx.send(Cmd::Gate(gate));
    }
}

impl Drop for Sc2Capture {
    fn drop(&mut self) {
        // Replacing the sender disconnects the reader's channel; it exits and closes the node.
        self.tx = std::sync::mpsc::channel().0;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Log a HIDAPI pad's report descriptor, once per slot open, so a "Send logs" bundle carries the
/// capture a native host identity is built from. Silent where the node will not open.
pub(crate) fn log_descriptor(path: &str, vid: u16, pid: u16) {
    let Some(dev) = Dev::open(path) else {
        return;
    };
    let mut buf = [0u8; 4096];
    // SAFETY: `dev.0` is open and `buf` is writable for its whole length.
    let n = unsafe { hid::SDL_hid_get_report_descriptor(dev.0, buf.as_mut_ptr(), buf.len()) };
    if n > 0 {
        let rdesc = crate::presets::hex_lower(&buf[..n as usize]);
        tracing::info!(vid, pid, len = n, rdesc, "controller HID descriptor");
    }
}

/// A second hidapi handle on an SDL slot's node.
pub(crate) struct Dev(*mut hid::SDL_hid_device);

// SAFETY: the handle moves into one worker thread once and is used and closed only there.
unsafe impl Send for Dev {}

impl Dev {
    /// `None` when the node will not open (not a HIDAPI device, no permission).
    pub(crate) fn open(path: &str) -> Option<Dev> {
        let c_path = std::ffi::CString::new(path).ok()?;
        // SAFETY: `c_path` is a valid NUL-terminated string that outlives the call.
        let dev = Dev(unsafe { hid::SDL_hid_open_path(c_path.as_ptr()) });
        (!dev.0.is_null()).then_some(dev)
    }

    fn send_feature(&self, r: &[u8]) -> i32 {
        // SAFETY: `self.0` is an open handle only this thread uses; `r` is valid for its length.
        unsafe { hid::SDL_hid_send_feature_report(self.0, r.as_ptr(), r.len()) }
    }

    pub(crate) fn write(&self, kind: u8, r: &[u8]) -> i32 {
        match kind {
            // SAFETY: as in `send_feature`.
            HID_RAW_OUTPUT => unsafe { hid::SDL_hid_write(self.0, r.as_ptr(), r.len()) },
            HID_RAW_FEATURE => self.send_feature(r),
            _ => 0,
        }
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: an open handle, closed once, on the thread that owns it.
            unsafe { hid::SDL_hid_close(self.0) };
        }
    }
}

fn run(dev: Dev, rx: &Receiver<Cmd>, client: &NativeClient, pad: u8, mut gate: Gate) {
    let mut imu = ImuGate::default();
    let mut ring = RingGate::default();
    let mut buf = [0u8; HID_REPORT_MAX];
    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Write(kind, data)) => {
                    if dev.write(kind, &data) < 0 {
                        let id = data.first().copied().unwrap_or(0);
                        tracing::debug!(pad, kind, id, "steam controller 2 write refused");
                    }
                }
                Ok(Cmd::Gate(g)) => gate = g,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        // SAFETY: open handle owned by this thread; `buf` is writable for its full length.
        let n = unsafe {
            hid::SDL_hid_read_timeout(dev.0, buf.as_mut_ptr(), buf.len(), READ_TIMEOUT_MS)
        };
        if n < 0 {
            // Unplug: SDL removes the slot too, which drops the capture.
            tracing::info!(pad, "steam controller 2 hid node closed");
            return;
        }
        let n = n as usize;
        if n == 0 || !filter_report(&mut buf[..n], gate, &mut imu, &mut ring) {
            continue;
        }
        let _ = client.send_rich_input(RichInput::HidReport {
            pad,
            len: n as u8,
            data: buf,
        });
    }
}

/// Gate one report in place. False for a report that stays off the wire: slot lifecycle rides
/// SDL hotplug, and the host queues its own Puck connect edge.
fn filter_report(r: &mut [u8], gate: Gate, imu: &mut ImuGate, ring: &mut RingGate) -> bool {
    match r[0] {
        ID_WIRELESS | ID_WIRELESS_X => return false,
        ID_STATE | ID_STATE_BLE | ID_STATE_TIMESTAMP if r.len() >= 6 => {
            let mut b = ring.apply(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), gate);
            if gate.masked {
                r[2..].fill(0);
            } else {
                if !gate.system_forward {
                    b &= !(BTN_STEAM | BTN_QAM);
                }
                r[2..6].copy_from_slice(&b.to_le_bytes());
            }
        }
        _ => {}
    }
    imu.apply(r);
    true
}

/// Buttons held off the wire until the hardware releases them: the ring chord, decided here
/// because the typed plane's mask lands reports later, and anything held while masked. Their
/// presses never went out, so their releases must not either. Apple's `Sc2RingGate` is the twin.
#[derive(Default)]
struct RingGate {
    held: u32,
    swallow: u32,
}

impl RingGate {
    /// What is left of `buttons` for the host. Select first, then A, as on the typed plane.
    fn apply(&mut self, buttons: u32, gate: Gate) -> u32 {
        let was = std::mem::replace(&mut self.held, buttons);
        self.swallow &= buttons;
        if gate.masked {
            self.swallow |= buttons;
        } else if gate.chords && buttons & !was & BTN_A != 0 && was & BTN_VIEW != 0 {
            self.swallow |= BTN_A | BTN_VIEW;
        }
        buttons & !self.swallow
    }
}

/// The pad streams IMU only after Steam writes `SETTING_IMU_MODE`. Until then the block and its
/// timestamp are a frozen resting sample, which Steam's desktop gyro-mouse reads as constant
/// rotation. Pass it only while the timestamp moves; `0x47` diverges from byte 18 and passes.
#[derive(Default)]
struct ImuGate {
    last: u32,
    seen: bool,
    stale: u8,
}

impl ImuGate {
    /// `TritonMTUNoQuat_t.imu` (struct offset 29 + id byte): u32 timestamp, 3× accel, 3× gyro.
    const OFFSET: usize = 30;
    const LEN: usize = 16;
    /// Three repeats still pass (report rate beats the IMU rate); the fourth freezes.
    const STALE_LIMIT: u8 = 4;

    fn apply(&mut self, r: &mut [u8]) {
        if r.len() < Self::OFFSET + Self::LEN || !matches!(r[0], ID_STATE | ID_STATE_BLE) {
            return;
        }
        let o = Self::OFFSET;
        let ts = u32::from_le_bytes([r[o], r[o + 1], r[o + 2], r[o + 3]]);
        let live = if !std::mem::replace(&mut self.seen, true) {
            self.stale = Self::STALE_LIMIT;
            false
        } else if ts != self.last {
            self.stale = 0;
            true
        } else {
            self.stale = (self.stale + 1).min(Self::STALE_LIMIT);
            self.stale < Self::STALE_LIMIT
        };
        self.last = ts;
        if !live {
            r[o..o + Self::LEN].fill(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Gate = Gate {
        masked: false,
        system_forward: true,
        chords: true,
    };

    fn state(ts: u32, buttons: u32) -> [u8; 54] {
        let mut r = [0u8; 54];
        r[0] = ID_STATE;
        r[1] = 7;
        r[2..6].copy_from_slice(&buttons.to_le_bytes());
        r[10] = 0x40; // left stick x
        r[30..34].copy_from_slice(&ts.to_le_bytes());
        r[36] = 0x11; // gyro
        r
    }

    /// The ring chord's Select is the bit `clients/shared/sc2-vectors.json` maps to wire Back,
    /// so the raw gate holds the same button the typed plane opens the ring on.
    #[test]
    fn ring_select_is_the_shared_back_bit() {
        let raw = include_str!("../../../clients/shared/sc2-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let back = file["buttons"]
            .as_array()
            .expect("buttons")
            .iter()
            .find(|r| r["name"] == "back")
            .expect("back row");
        assert_eq!(u64::from(BTN_VIEW), back["sc2"].as_u64().unwrap());
    }

    /// `clients/shared/sc2-vectors.json`'s trace through one gate; the Swift and Kotlin gates
    /// replay the same file.
    #[test]
    fn imu_gate_matches_the_shared_trace() {
        let raw = include_str!("../../../clients/shared/sc2-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("vector file parses");
        let mut gate = ImuGate::default();
        for (i, step) in file["imu_trace"]
            .as_array()
            .expect("imu_trace")
            .iter()
            .enumerate()
        {
            let len = step["len"].as_u64().unwrap() as usize;
            let mut r = vec![0u8; len];
            r[0] = step["id"].as_u64().unwrap() as u8;
            let ts = step["ts"].as_u64().unwrap() as u32;
            let imu = ImuGate::OFFSET..len.min(ImuGate::OFFSET + ImuGate::LEN);
            r[ImuGate::OFFSET..ImuGate::OFFSET + 4].copy_from_slice(&ts.to_le_bytes());
            r[ImuGate::OFFSET + 4..imu.end].fill(0x11);
            let before = r.clone();
            gate.apply(&mut r);
            if step["pass"].as_bool().unwrap() {
                assert_eq!(r, before, "step {i}");
            } else {
                assert!(r[imu].iter().all(|&b| b == 0), "step {i}");
            }
        }
    }

    #[test]
    fn detects_every_sc2_identity() {
        assert_eq!(
            pref_for(0x28DE, 0x1302),
            Some(GamepadPref::SteamController2)
        );
        assert_eq!(
            pref_for(0x28DE, 0x1303),
            Some(GamepadPref::SteamController2)
        );
        assert_eq!(
            pref_for(0x28DE, 0x1304),
            Some(GamepadPref::SteamController2Puck)
        );
        assert_eq!(
            pref_for(0x28DE, 0x1305),
            Some(GamepadPref::SteamController2Puck)
        );
        assert_eq!(pref_for(0x28DE, 0x1205), None);
    }

    #[test]
    fn frozen_imu_is_zeroed_and_a_moving_one_passes() {
        let mut imu = ImuGate::default();
        let mut r = state(100, 0);
        assert!(filter_report(
            &mut r,
            OPEN,
            &mut imu,
            &mut RingGate::default()
        ));
        assert_eq!(r[36], 0, "first sample is unproven");
        let mut r = state(100, 0);
        filter_report(&mut r, OPEN, &mut imu, &mut RingGate::default());
        assert_eq!(r[36], 0, "frozen timestamp");
        let mut r = state(101, 0);
        filter_report(&mut r, OPEN, &mut imu, &mut RingGate::default());
        assert_eq!(r[36], 0x11, "moving timestamp");
        for _ in 0..3 {
            let mut r = state(101, 0);
            filter_report(&mut r, OPEN, &mut imu, &mut RingGate::default());
            assert_eq!(r[36], 0x11, "short repeats pass");
        }
        let mut r = state(101, 0);
        filter_report(&mut r, OPEN, &mut imu, &mut RingGate::default());
        assert_eq!(r[36], 0, "fourth repeat freezes");
    }

    #[test]
    fn mask_neutralises_state_but_keeps_id_and_seq() {
        let mut r = state(5, 0x1);
        let masked = Gate {
            masked: true,
            ..OPEN
        };
        assert!(filter_report(
            &mut r,
            masked,
            &mut ImuGate::default(),
            &mut RingGate::default()
        ));
        assert_eq!((r[0], r[1]), (ID_STATE, 7));
        assert!(r[2..].iter().all(|&b| b == 0));
    }

    #[test]
    fn local_system_buttons_stay_off_the_wire() {
        let mut r = state(5, BTN_STEAM | BTN_QAM | 0x1);
        let local = Gate {
            system_forward: false,
            ..OPEN
        };
        filter_report(
            &mut r,
            local,
            &mut ImuGate::default(),
            &mut RingGate::default(),
        );
        assert_eq!(u32::from_le_bytes([r[2], r[3], r[4], r[5]]), 0x1);
        assert_eq!(r[10], 0x40);
    }

    #[test]
    fn wireless_status_never_reaches_the_host() {
        let mut imu = ImuGate::default();
        assert!(!filter_report(
            &mut [ID_WIRELESS, 0x01],
            OPEN,
            &mut imu,
            &mut RingGate::default()
        ));
        assert!(!filter_report(
            &mut [ID_WIRELESS_X, 0x02],
            OPEN,
            &mut imu,
            &mut RingGate::default()
        ));
        assert!(filter_report(
            &mut [0x43, 80],
            OPEN,
            &mut imu,
            &mut RingGate::default()
        ));
    }

    /// Buttons one report puts on the wire through `ring`.
    fn sent(ring: &mut RingGate, gate: Gate, buttons: u32) -> u32 {
        let mut r = state(5, buttons);
        filter_report(&mut r, gate, &mut ImuGate::default(), ring);
        u32::from_le_bytes([r[2], r[3], r[4], r[5]])
    }

    #[test]
    fn select_then_a_stays_off_the_wire_until_each_is_released() {
        let mut ring = RingGate::default();
        assert_eq!(sent(&mut ring, OPEN, BTN_VIEW), BTN_VIEW);
        assert_eq!(
            sent(&mut ring, OPEN, BTN_VIEW | BTN_A),
            0,
            "the chord report"
        );
        assert_eq!(sent(&mut ring, OPEN, BTN_VIEW | BTN_A | 0x2), 0x2);
        assert_eq!(sent(&mut ring, OPEN, BTN_A), 0, "A outlives Select");
        assert_eq!(sent(&mut ring, OPEN, 0), 0);
        assert_eq!(
            sent(&mut ring, OPEN, BTN_A),
            BTN_A,
            "a fresh press goes out"
        );
    }

    #[test]
    fn a_then_select_or_no_listener_is_no_chord() {
        let mut ring = RingGate::default();
        sent(&mut ring, OPEN, BTN_A);
        assert_eq!(sent(&mut ring, OPEN, BTN_A | BTN_VIEW), BTN_A | BTN_VIEW);
        let deaf = Gate {
            chords: false,
            ..OPEN
        };
        let mut ring = RingGate::default();
        sent(&mut ring, deaf, BTN_VIEW);
        assert_eq!(sent(&mut ring, deaf, BTN_VIEW | BTN_A), BTN_VIEW | BTN_A);
    }

    /// The ring closes on A; the host must not see that A pressed again.
    #[test]
    fn a_button_held_through_the_mask_stays_off_until_released() {
        let mut ring = RingGate::default();
        let masked = Gate {
            masked: true,
            ..OPEN
        };
        sent(&mut ring, masked, BTN_A);
        assert_eq!(sent(&mut ring, OPEN, BTN_A), 0);
        assert_eq!(sent(&mut ring, OPEN, 0), 0);
        assert_eq!(sent(&mut ring, OPEN, BTN_A), BTN_A);
    }
}
