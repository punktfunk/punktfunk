//! Raw passthrough for a Steam Controller 2 held by an SDL slot. SDL's Triton driver still
//! feeds the typed plane (escape chord, ring, menus); a second hidapi handle on the same node
//! forwards every input report to the host's as-is `28DE:1302` pad and replays Steam's writes on
//! the physical controller. Trackpads, gyro and haptics exist only on this path: the host's typed
//! fallback carries buttons, sticks and triggers. Core's send path gates each report
//! ([`NativeClient::set_sc2_gate`]), as it does for the Apple and Android captures.

use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::quic::{RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT, HID_REPORT_MAX};
use sdl3::sys::hidapi as hid;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

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

enum Cmd {
    Write(u8, Vec<u8>),
}

/// The reader thread owns the handle; dropping this hangs up and joins it.
pub(crate) struct Sc2Capture {
    tx: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
}

impl Sc2Capture {
    /// `path` is the SDL slot's HID path. `None` when the node will not open (not a HIDAPI
    /// device, no permission): the slot keeps the typed plane.
    pub(crate) fn open(path: &str, client: Arc<NativeClient>, pad: u8) -> Option<Sc2Capture> {
        let Some(dev) = Dev::open(path) else {
            tracing::warn!(path, error = %sdl3::get_error(), "open steam controller 2 hid node");
            return None;
        };
        dev.send_feature(&NORMALIZE_JOYSTICKS);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pf-sc2-raw".into())
            .spawn(move || run(dev, &rx, &client, pad))
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

fn run(dev: Dev, rx: &Receiver<Cmd>, client: &NativeClient, pad: u8) {
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
        if n > 0 {
            let _ = client.send_rich_input(RichInput::HidReport {
                pad,
                len: n as u8,
                data: buf,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
