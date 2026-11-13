//! Raw passthrough for a Steam Controller held by an SDL slot, either generation ([`Model`]).
//! SDL's own driver still feeds the typed plane (escape chord, ring, menus); a second hidapi
//! handle on the same node forwards every input report to the host's as-is pad (`28DE:1302`,
//! or `28DE:1102` for the 2015 one) and replays Steam's writes on the physical controller.
//! Trackpads, gyro and haptics exist only on this path: the host's typed fallback carries
//! buttons, sticks and triggers. Core's send path gates each report
//! ([`NativeClient::set_sc2_gate`]), as it does for the Apple and Android captures.

use punktfunk_core::client::sc1_ble;
use punktfunk_core::client::NativeClient;
use punktfunk_core::config::GamepadPref;
use punktfunk_core::quic::{RichInput, HID_RAW_FEATURE, HID_RAW_OUTPUT, HID_REPORT_MAX};
use sdl3::sys::hidapi as hid;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

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

/// Which Steam Controller a capture holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Model {
    /// The 2026 pad: on a cable, on Bluetooth, or one slot of a Puck.
    Triton { puck: bool },
    /// The 2015 pad on a cable or its dongle. Its reports and feature frames carry no id.
    Classic,
    /// The 2015 pad on Bluetooth: segments both ways, which core turns into wired frames.
    ClassicBle,
}

/// The capture a slot gets: the pad in hand and the kind declared to the host are the same
/// Steam Controller.
pub(crate) fn raw_model(pref: GamepadPref, declared: GamepadPref, pid: u16) -> Option<Model> {
    match (pref, declared) {
        (p, d) if is_sc2(p) && is_sc2(d) => Some(Model::Triton {
            puck: p == GamepadPref::SteamController2Puck,
        }),
        (GamepadPref::SteamController, GamepadPref::SteamController)
            if matches!(pid, 0x1102 | 0x1142) =>
        {
            Some(Model::Classic)
        }
        (GamepadPref::SteamController, GamepadPref::SteamController)
            if matches!(pid, 0x1105 | 0x1106) =>
        {
            Some(Model::ClassicBle)
        }
        _ => None,
    }
}

/// The feature commands a host may send a 2015 pad, after the report id 0: settings, haptic
/// pulses, the jingles, power-off and the IMU reset. Mirrors the host's `sc_forwards_to_pad`
/// (`pf-inject` `steam_proto.rs`), which keeps every other command on its virtual pad.
const CLASSIC_HOST_COMMANDS: [u8; 6] = [0x87, 0x8F, 0x9F, 0xB6, 0xC1, 0xCE];

/// Feature commands the host never sends a Triton: lizard mode off and on, and the factory
/// settings. Mirrors `pf_driver_proto::triton::forwards_to_pad`. The repo names no other
/// command that rewrites the pad, so nothing else is held back.
const TRITON_HELD_BACK: [u8; 3] = [0x81, 0x85, 0x8E];

/// Why a report from the host must not reach the pad, or `None` to write it. A host that
/// follows the rules above never sends what this refuses.
fn refused(model: Model, kind: u8, data: &[u8]) -> Option<&'static str> {
    match (model, kind) {
        (Model::Triton { .. }, HID_RAW_FEATURE) => {
            // Id first, or a bare command (`0x80` and above), as the host reads it.
            let cmd = match data {
                [first, ..] if *first >= 0x80 => first,
                [_, cmd, ..] => cmd,
                _ => return None,
            };
            TRITON_HELD_BACK
                .contains(cmd)
                .then_some("lizard mode and factory settings stay on the host")
        }
        (Model::Triton { .. }, _) => None,
        (Model::Classic | Model::ClassicBle, HID_RAW_FEATURE) => match data {
            [0, cmd, ..] if CLASSIC_HOST_COMMANDS.contains(cmd) => None,
            [0, _, ..] => Some("command is not one the host forwards"),
            _ => Some("not a feature frame with report id 0"),
        },
        (Model::Classic | Model::ClassicBle, _) => Some("a 2015 pad takes feature reports only"),
    }
}

enum Cmd {
    Write(u8, Vec<u8>),
}

/// The reader thread owns the handle; dropping this hangs up and joins it.
pub(crate) struct Sc2Capture {
    tx: Sender<Cmd>,
    /// Raised by `Drop` so an identity read in progress ends before the join.
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Sc2Capture {
    /// `path` is the SDL slot's HID path. `None` when the node will not open (not a HIDAPI
    /// device, no permission): the slot keeps the typed plane.
    /// `serial` is the pad's USB serial. The reader sends the pad's identity
    /// ([`NativeClient::send_pad_identity`]) before its first report.
    pub(crate) fn open(
        path: &str,
        client: Arc<NativeClient>,
        pad: u8,
        serial: Option<String>,
        model: Model,
    ) -> Option<Sc2Capture> {
        let Some(dev) = Dev::open(path) else {
            tracing::warn!(path, error = %sdl3::get_error(), "open steam controller hid node");
            return None;
        };
        if matches!(model, Model::Triton { .. }) {
            dev.send_feature(&NORMALIZE_JOYSTICKS);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = stop.clone();
        let thread = std::thread::Builder::new()
            .name("pf-sc2-raw".into())
            .spawn(move || {
                send_identity(
                    &dev,
                    &client,
                    pad,
                    serial.unwrap_or_default(),
                    model,
                    &stop_t,
                );
                run(dev, &rx, &client, pad, model)
            })
            .map_err(|e| tracing::warn!(error = %e, "spawn steam controller reader"))
            .ok()?;
        Some(Sc2Capture {
            tx,
            stop,
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
        // The flag ends an identity read; the hung-up channel ends the report loop. Either
        // way the reader closes the node.
        self.stop.store(true, Ordering::Relaxed);
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

    /// The USB interface this node is; 0 when SDL cannot say.
    fn interface(&self) -> u8 {
        // SAFETY: `self.0` is an open device; SDL keeps the info alive while the handle is open.
        let info = unsafe { hid::SDL_hid_get_device_info(self.0) };
        if info.is_null() {
            return 0;
        }
        // SAFETY: non-null, owned by SDL, read before the handle can close.
        u8::try_from(unsafe { (*info).interface_number }).unwrap_or(0)
    }

    /// SET one feature query, then GET until the reply echoes its command: a Puck stalls the
    /// GET until its pad answers over the radio, so the newer pad gets a second. The 2015 pad
    /// gets 200 ms: its three queries must end inside the host's one-second wait. A raised
    /// `stop` ends the wait.
    fn exchange(&self, request: &[u8], model: Model, stop: &AtomicBool) -> Option<Vec<u8>> {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let (sent, tries) = match model {
            Model::Triton { .. } => {
                let mut frame = [0u8; 64];
                frame[..request.len()].copy_from_slice(request);
                (self.send_feature(&frame), 50)
            }
            Model::Classic => (self.send_classic_feature(request), 10),
            Model::ClassicBle => return self.exchange_ble(request, stop),
        };
        if sent < 0 {
            return None;
        }
        poll_until(tries, Duration::from_millis(20), stop, || {
            let mut buf = [0u8; 65];
            buf[0] = request[0];
            // SAFETY: `self.0` is open and used only on this thread; `buf` is writable.
            let n = unsafe { hid::SDL_hid_get_feature_report(self.0, buf.as_mut_ptr(), buf.len()) };
            (n > 1 && buf[1] == request[1]).then(|| buf[..n as usize].to_vec())
        })
    }

    /// [`Self::exchange`] for a 2015 pad on Bluetooth: the reply comes one segment per GET.
    /// Windows and macOS hand the report id back twice, so a segment starts one byte in there
    /// (SDL `GetFeatureReport`).
    fn exchange_ble(&self, request: &[u8], stop: &AtomicBool) -> Option<Vec<u8>> {
        if self.send_ble_feature(request) < 0 {
            return None;
        }
        let skip = usize::from(cfg!(any(target_os = "windows", target_os = "macos")));
        let mut read = Vec::new();
        let mut reply = [0u8; 73];
        for _ in 0..12 {
            if stop.load(Ordering::Relaxed) {
                return None;
            }
            let mut buf = [0u8; sc1_ble::SEGMENT + 1];
            buf[0] = 0x03;
            // SAFETY: `self.0` is open and used only on this thread; `buf` is writable.
            let n = unsafe {
                hid::SDL_hid_get_feature_report(self.0, buf.as_mut_ptr(), sc1_ble::SEGMENT + skip)
            };
            if n <= skip as i32 {
                continue;
            }
            read.extend_from_slice(&buf[skip..skip + sc1_ble::SEGMENT]);
            if let Some(len) = sc1_ble::classic_ble_reply(&read, &mut reply) {
                return (reply.get(1) == request.get(1)).then(|| reply[..len].to_vec());
            }
        }
        None
    }

    /// A wired feature frame to a 2015 pad on Bluetooth, as its segments.
    fn send_ble_feature(&self, frame: &[u8]) -> i32 {
        let mut sent = -1;
        for seg in sc1_ble::classic_ble_segments(frame) {
            sent = self.send_feature(&seg);
            if sent < 0 {
                break;
            }
        }
        sent
    }

    fn send_feature(&self, r: &[u8]) -> i32 {
        // SAFETY: `self.0` is an open handle only this thread uses; `r` is valid for its length.
        unsafe { hid::SDL_hid_send_feature_report(self.0, r.as_ptr(), r.len()) }
    }

    /// A 2015 pad takes a feature report only as 65 bytes, id 0 first, and its dongle refuses
    /// one while the radio is busy: retry for 25 ms, as SDL does.
    fn send_classic_feature(&self, r: &[u8]) -> i32 {
        let mut frame = [0u8; 65];
        let n = r.len().min(frame.len());
        frame[..n].copy_from_slice(&r[..n]);
        for _ in 0..50 {
            let sent = self.send_feature(&frame);
            if sent >= 0 {
                return sent;
            }
            std::thread::sleep(std::time::Duration::from_micros(500));
        }
        -1
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

/// Run `get` once per `period`, up to `tries` times, until it answers. `None` when it never
/// does, or once `stop` is raised.
fn poll_until<T>(
    tries: u32,
    period: Duration,
    stop: &AtomicBool,
    mut get: impl FnMut() -> Option<T>,
) -> Option<T> {
    for _ in 0..tries {
        std::thread::sleep(period);
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        if let Some(reply) = get() {
            return Some(reply);
        }
    }
    None
}

/// Read the pad's replies to core's feature queries and send them with its serial, so the host
/// builds its virtual pad as this one. A raised `stop` ends the read and sends nothing.
fn send_identity(
    dev: &Dev,
    client: &NativeClient,
    pad: u8,
    serial: String,
    model: Model,
    stop: &AtomicBool,
) {
    use punktfunk_core::client::sc2;
    let requests: Vec<&[u8]> = match model {
        Model::Triton { puck } => sc2::identity_requests(puck).collect(),
        Model::Classic | Model::ClassicBle => sc2::classic_identity_requests().collect(),
    };
    let answered: Vec<(&[u8], Vec<u8>)> = requests
        .into_iter()
        .filter_map(|req| dev.exchange(req, model, stop).map(|reply| (req, reply)))
        .collect();
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let replies = punktfunk_core::quic::pack_identity_replies(
        answered.iter().map(|(req, reply)| (*req, reply.as_slice())),
    );
    tracing::info!(
        pad,
        serial,
        ?model,
        replies = answered.len(),
        "steam controller identity read"
    );
    // A Puck's pad slots are its interfaces 2–5.
    let slot = if model == (Model::Triton { puck: true }) {
        dev.interface().saturating_sub(2).min(3)
    } else {
        0
    };
    let id = punktfunk_core::quic::PadIdentity {
        pad,
        serial,
        replies,
        slot,
    };
    if let Err(error) = client.send_pad_identity(id) {
        tracing::warn!(pad, %error, "steam controller identity not sent");
    }
}

fn run(dev: Dev, rx: &Receiver<Cmd>, client: &NativeClient, pad: u8, model: Model) {
    let mut buf = [0u8; HID_REPORT_MAX];
    let mut ble = [0u8; sc1_ble::CLASSIC_BLE_STATE];
    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Write(kind, data)) => {
                    if let Some(reason) = refused(model, kind, &data) {
                        let head = data.get(..2);
                        tracing::debug!(pad, kind, ?head, reason, "steam controller write refused");
                        continue;
                    }
                    let sent = match (model, kind) {
                        (Model::Classic, HID_RAW_FEATURE) => dev.send_classic_feature(&data),
                        (Model::ClassicBle, HID_RAW_FEATURE) => dev.send_ble_feature(&data),
                        (Model::ClassicBle, _) => 0,
                        _ => dev.write(kind, &data),
                    };
                    if sent < 0 {
                        // Report id, then the command of a feature frame.
                        let head = data.get(..2);
                        tracing::debug!(pad, kind, ?head, "steam controller write refused");
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
            tracing::info!(pad, "steam controller hid node closed");
            return;
        }
        if n > 0 && model == Model::ClassicBle {
            let mut frame = [0u8; HID_REPORT_MAX];
            if sc1_ble::classic_ble_input(&mut ble, &buf[..n as usize], &mut frame) {
                let _ = client.send_rich_input(RichInput::HidReport {
                    pad,
                    len: HID_REPORT_MAX as u8,
                    data: frame,
                });
            }
        } else if n > 0 {
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

    /// A raised stop ends a 50-try wait at once instead of after its full second.
    #[test]
    fn a_raised_stop_ends_the_wait() {
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        let reply = poll_until(50, Duration::ZERO, &stop, || {
            calls += 1;
            if calls == 2 {
                stop.store(true, Ordering::Relaxed);
            }
            None::<u8>
        });
        assert_eq!(reply, None);
        assert_eq!(calls, 2);

        let mut calls = 0;
        let reply = poll_until(50, Duration::ZERO, &stop, || {
            calls += 1;
            Some(7)
        });
        assert_eq!((reply, calls), (None, 0), "a stopped wait never asks");
    }

    #[test]
    fn a_wait_returns_the_first_answer() {
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        let reply = poll_until(50, Duration::ZERO, &stop, || {
            calls += 1;
            (calls == 3).then_some(calls)
        });
        assert_eq!(reply, Some(3));
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

    /// A raw capture needs the pad in hand and the declared kind to be the same Steam
    /// Controller.
    #[test]
    fn raw_capture_follows_the_pad_and_the_declared_kind() {
        use GamepadPref as P;
        let puck = P::SteamController2Puck;
        assert_eq!(
            raw_model(P::SteamController2, P::SteamController2, 0x1302),
            Some(Model::Triton { puck: false })
        );
        assert_eq!(
            raw_model(puck, puck, 0x1304),
            Some(Model::Triton { puck: true })
        );
        for pid in [0x1102, 0x1142] {
            assert_eq!(
                raw_model(P::SteamController, P::SteamController, pid),
                Some(Model::Classic)
            );
        }
        for pid in [0x1105, 0x1106] {
            assert_eq!(
                raw_model(P::SteamController, P::SteamController, pid),
                Some(Model::ClassicBle)
            );
        }
        assert_eq!(
            raw_model(P::SteamController, P::Xbox360, 0x1102),
            None,
            "a pad the player declared as something else stays typed"
        );
        assert_eq!(raw_model(P::Xbox360, P::SteamController, 0x028E), None);
    }

    /// A 2015 pad takes the commands the host forwards, framed with report id 0, and nothing
    /// else: the host's own list of refusals stays refused, so does an output report.
    #[test]
    fn a_2015_pad_takes_only_the_commands_the_host_forwards() {
        for model in [Model::Classic, Model::ClassicBle] {
            for cmd in [0x87, 0x8F, 0x9F, 0xB6, 0xC1, 0xCE] {
                assert_eq!(refused(model, HID_RAW_FEATURE, &[0, cmd, 3, 1]), None);
            }
            for cmd in [0x81, 0x85, 0x86, 0x8E, 0x90, 0xA7, 0xA9, 0xAD, 0xB3, 0xBF] {
                assert!(refused(model, HID_RAW_FEATURE, &[0, cmd, 0]).is_some());
            }
            assert!(refused(model, HID_RAW_FEATURE, &[1, 0x87, 0]).is_some());
            assert!(refused(model, HID_RAW_FEATURE, &[0x87, 0]).is_some());
            assert!(refused(model, HID_RAW_FEATURE, &[0]).is_some());
            assert!(refused(model, HID_RAW_OUTPUT, &[0, 0x87, 0]).is_some());
        }
    }

    /// A Triton refuses lizard mode and the factory settings in either framing, and takes
    /// settings, other feature reads and every output report.
    #[test]
    fn a_triton_keeps_lizard_mode_and_the_factory_settings_off_the_pad() {
        let triton = Model::Triton { puck: false };
        for cmd in [0x81, 0x85, 0x8E] {
            assert!(refused(triton, HID_RAW_FEATURE, &[1, cmd, 0]).is_some());
            assert!(refused(Model::Triton { puck: true }, HID_RAW_FEATURE, &[2, cmd]).is_some());
            assert!(refused(triton, HID_RAW_FEATURE, &[cmd, 0]).is_some());
        }
        for frame in [
            &[1, 0x87, 3, 0x2E, 0][..],
            &[1, 0xAE, 0x15, 1],
            &[0x87, 3],
            &[],
        ] {
            assert_eq!(refused(triton, HID_RAW_FEATURE, frame), None);
        }
        assert_eq!(refused(triton, HID_RAW_OUTPUT, &[0x80, 1, 0, 0]), None);
        assert_eq!(refused(triton, HID_RAW_OUTPUT, &[0x8E, 0]), None);
    }
}
