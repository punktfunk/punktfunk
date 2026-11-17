//! Virtual touchpad: a per-session Precision Touchpad through the `pf-touchpad` UMDF2 HID
//! minidriver, on `pf-mouse`'s sealed channel and session fence. Windows takes touchpad input
//! only from a HID device; synthetic pointers cover pen and touch alone.
//!
//! The host's contract, in the order it calls:
//! - [`supported`] and [`taps`] once per session at the handshake.
//! - [`VirtualTouchpad::create`] on the session's first frame. It returns at once; a worker
//!   thread builds the devnode `pf_touchpad_<index>` and waits for its driver.
//! - [`VirtualTouchpad::apply`] with the whole pad after every applied frame, in pad units.
//!   States before the driver takes reports are dropped, and the latest one is presented then.
//! - Dropping the value lifts every contact and removes the devnode.
//!
//! Contacts keep a report slot for their life; the slot is the Contact ID. A contact that left
//! is reported once more with Tip clear. One arriving while five are down stays out until it
//! lifts, as Microsoft asks of a full pad. Layout: `pf_driver_proto::touchpad`. Design:
//! `design/touchpad-passthrough.md` §3.

use crate::gamepad_raii::{
    create_swdevice, named_section_exists, verify_fence, DriverAttach, FenceAt, PadChannel,
    ProofTransport, SwDevice, SwDeviceProfile,
};
use crate::pad_slots::PadCreateFault;
use anyhow::{Context, Result};
use pf_driver_proto::touchpad::{
    ptp_report, ring_slot_off, touchpad_boot_name, PtpContact, TouchpadShm, DOORBELL_REPORT_ID,
    DOORBELL_USAGE_PAGE, PAD_MAX_X, PAD_MAX_Y, TOUCHPAD_MAGIC, TOUCHPAD_MAX_CONTACTS,
    TOUCHPAD_REPORT_LEN, TOUCHPAD_RING_LEN,
};
use punktfunk_core::quic::{PadState, TOUCHPAD_PAD_MAX_X, TOUCHPAD_PAD_MAX_Y};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{Owned, HSTRING, PWSTR};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TokenUser, PSID, SID_AND_ATTRIBUTES, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::WriteFile;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_USERS, RRF_RT_REG_DWORD};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;

const _: () = assert!(PAD_MAX_X == TOUCHPAD_PAD_MAX_X && PAD_MAX_Y == TOUCHPAD_PAD_MAX_Y);
// `session_user_sid` takes the SID's address from the first word of a TOKEN_USER.
const _: () = assert!(
    core::mem::offset_of!(TOKEN_USER, User) == 0
        && core::mem::offset_of!(SID_AND_ATTRIBUTES, Sid) == 0
);

const SHM_SIZE: usize = core::mem::size_of::<TouchpadShm>();
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(TouchpadShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(TouchpadShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(TouchpadShm, pad_index);
const OFF_MAGIC: usize = core::mem::offset_of!(TouchpadShm, magic);
const OFF_RING_HEAD: usize = core::mem::offset_of!(TouchpadShm, ring_head);
const OFF_RING_TAIL: usize = core::mem::offset_of!(TouchpadShm, ring_tail);
const OFF_INPUT_MODE: usize = core::mem::offset_of!(TouchpadShm, input_mode);

/// `PUNKTFUNK_TOUCHPAD=0` keeps the feature off.
const TOUCHPAD_ENV: &str = "PUNKTFUNK_TOUCHPAD";
/// Set on a seat host, whose HID input would land in the console session. Same reading as
/// `mouse_windows::mouse_index_for_slot`: only a reserved connector is a seat.
const SEAT_SLOT_ENV: &str = "PUNKTFUNK_SEAT_DISPLAY_SLOT";
const SEAT_SLOTS: std::ops::RangeInclusive<u8> = 12..=15;
/// The session user's touchpad settings, under `HKEY_USERS\<sid>`.
const SETTINGS_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\PrecisionTouchPad";
/// Touchpads at once in this process, one per session.
const MAX_DEVICES: u8 = 16;
/// The driver's timer stamps the heartbeat every 8–16 ms.
const DRIVER_STALE: Duration = Duration::from_secs(1);
/// The worker's tick: channel pump, fence read, readiness, held repeats.
const SERVICE_TICK: Duration = Duration::from_millis(20);
/// Windows 11 (26200) lifts a contact it hears nothing of for 150 ms, and a still one then
/// clicks as a tap; 100 ms of silence is safe. A held pad goes out again after this long.
const HOLD_REPEAT: Duration = Duration::from_millis(50);
const DOORBELL_RETRY: Duration = Duration::from_secs(1);

/// The console host, with the driver staged, the kill switch not `0`, and the session user's
/// "leave touchpad on when a mouse is connected" not off: `pf_mouse` is always connected.
pub fn supported() -> bool {
    std::env::var_os(TOUCHPAD_ENV).is_none_or(|v| v != "0")
        && !is_seat(std::env::var(SEAT_SLOT_ENV).ok().as_deref())
        && driver_staged()
        && user_setting("LeaveOnWithMouse") != Some(0)
}

/// Whether "tap with a single finger to single-click" is on for the session user. Unset is
/// on, the Windows default; Settings writes `0` or `0xFFFFFFFF`.
pub fn taps() -> bool {
    user_setting("TapsEnabled") != Some(0)
}

fn is_seat(slot: Option<&str>) -> bool {
    slot.and_then(|s| s.parse::<u8>().ok())
        .is_some_and(|s| SEAT_SLOTS.contains(&s))
}

/// A `pf_touchpad.inf_*` package in the driver store.
fn driver_staged() -> bool {
    std::fs::read_dir(pf_paths::system32(r"DriverStore\FileRepository")).is_ok_and(|dir| {
        dir.flatten().any(|e| {
            e.file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with("pf_touchpad.inf_")
        })
    })
}

/// A DWORD of the session user's touchpad settings. `None` when unset, or when no user is
/// signed in to this host's session.
fn user_setting(name: &str) -> Option<u32> {
    let subkey = HSTRING::from(format!(r"{}\{SETTINGS_KEY}", session_user_sid()?));
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both strings are NUL-terminated HSTRINGs; `value`/`size` are live out-params
    // sized for the DWORD the flags demand.
    let rc = unsafe {
        RegGetValueW(
            HKEY_USERS,
            &subkey,
            &HSTRING::from(name),
            RRF_RT_REG_DWORD,
            None,
            Some(std::ptr::addr_of_mut!(value).cast()),
            Some(&mut size),
        )
    };
    (rc == ERROR_SUCCESS).then_some(value)
}

/// `S-1-…` of the user signed in to this process's session. The host is SYSTEM, so its HKCU
/// is not the player's; `WTSQueryUserToken` needs SE_TCB and fails for anyone else.
fn session_user_sid() -> Option<String> {
    let session = pf_win_display::own_session_id()?;
    let mut token = HANDLE::default();
    // SAFETY: `token` is a live out-param.
    unsafe { WTSQueryUserToken(session, &mut token) }.ok()?;
    // SAFETY: the query succeeded, so `token` is a handle this frame alone owns.
    let token = unsafe { Owned::new(token) };
    let mut needed = 0u32;
    // SAFETY: a size probe: a null buffer of length zero fails and sets `needed`.
    let _ = unsafe { GetTokenInformation(*token, TokenUser, None, 0, &mut needed) };
    let mut buf = vec![0u8; needed as usize];
    // SAFETY: `buf` is `needed` bytes, what the probe asked for.
    unsafe {
        GetTokenInformation(
            *token,
            TokenUser,
            Some(buf.as_mut_ptr().cast()),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    // On success `buf` holds a TOKEN_USER, which begins with its SID's address (asserted
    // above); the SID lies in `buf` too.
    let sid = PSID(usize::from_ne_bytes(*buf.first_chunk()?) as *mut core::ffi::c_void);
    let mut out = PWSTR::null();
    // SAFETY: `sid` points into `buf`, alive for the call; `out` is a live out-param.
    unsafe { ConvertSidToStringSidW(sid, &mut out) }.ok()?;
    // SAFETY: `out` is the LocalAlloc'd string the call returned; `Owned` frees it once.
    let _free = unsafe { Owned::new(HLOCAL(out.0.cast())) };
    // SAFETY: `out` is a NUL-terminated wide string from the successful call above.
    unsafe { out.to_string() }.ok()
}

/// A process-unique touchpad index, released on drop. One a live mailbox of another process
/// names (a devtest beside the service) is skipped.
struct Index(u8);

static INDICES: Mutex<u16> = Mutex::new(0);

impl Index {
    fn claim() -> Option<Index> {
        let mut used = INDICES.lock().unwrap_or_else(PoisonError::into_inner);
        let i = (0..MAX_DEVICES).find(|&i| {
            *used & 1 << i == 0
                && !named_section_exists(&HSTRING::from(touchpad_boot_name(i).as_str()))
        })?;
        *used |= 1 << i;
        Some(Index(i))
    }
}

impl Drop for Index {
    fn drop(&mut self) {
        *INDICES.lock().unwrap_or_else(PoisonError::into_inner) &= !(1 << self.0);
    }
}

/// One session's touchpad. The worker owns the devnode; `apply` writes reports straight into
/// its ring once the driver takes them.
pub struct VirtualTouchpad {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct State {
    slots: Slots,
    scan: ScanTime,
    dev: Option<Device>,
    was_ready: bool,
    shutdown: bool,
}

impl State {
    /// The device when its driver takes reports. The first time it does, the current pad goes
    /// out, so a state dropped while it came up is not lost.
    fn ready_device(&mut self) -> Option<&mut Device> {
        let dev = self.dev.as_mut()?;
        let ready = dev.ready();
        if ready != self.was_ready {
            self.was_ready = ready;
            if ready {
                tracing::info!(
                    devnode = %dev.instance_id,
                    input_mode = dev.load(OFF_INPUT_MODE),
                    "virtual touchpad takes reports"
                );
                if let Some(contacts) = self.slots.current() {
                    let scan = self.scan.stamp(self.scan.client, false);
                    dev.push(&ptp_report(&contacts, scan, self.slots.button));
                }
            } else {
                tracing::warn!(devnode = %dev.instance_id, "virtual touchpad stopped taking reports");
            }
        }
        ready.then_some(dev)
    }

    /// Re-send what is held when nothing went out for [`HOLD_REPEAT`]: a still finger sends
    /// no frames. The repeat is one Scan Time tick on; it moves nothing, so its speed is moot.
    fn repeat_held(&mut self) {
        let Some(contacts) = self.slots.current() else {
            let _ = self.ready_device();
            return;
        };
        if self
            .ready_device()
            .is_some_and(|dev| dev.pushed.elapsed() >= HOLD_REPEAT)
        {
            let scan = self.scan.stamp(self.scan.client, true);
            let report = ptp_report(&contacts, scan, self.slots.button);
            if let Some(dev) = self.dev.as_mut() {
                dev.push(&report);
            }
        }
    }

    fn present(&mut self, pad: &PadState) {
        self.scan.client = pad.scan_time;
        let touching = self.slots.held;
        let Some(contacts) = self.slots.update(pad) else {
            return;
        };
        let scan = self.scan.stamp(pad.scan_time, touching);
        let button = self.slots.button;
        if let Some(dev) = self.ready_device() {
            dev.push(&ptp_report(&contacts, scan, button));
        }
    }
}

impl VirtualTouchpad {
    /// Claims an index and starts the worker; the devnode follows within a second or two.
    /// Refuses on a seat host, as [`supported`] does.
    pub fn create() -> Result<VirtualTouchpad> {
        anyhow::ensure!(
            !is_seat(std::env::var(SEAT_SLOT_ENV).ok().as_deref()),
            "no virtual touchpad on a seat host"
        );
        let index = Index::claim().context("claim a touchpad index: all are in use")?;
        let shared = Arc::new(Shared {
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
        });
        let worker = std::thread::Builder::new()
            .name(format!("pf-touchpad-{}", index.0))
            .spawn({
                let shared = Arc::clone(&shared);
                move || worker(&shared, index)
            })
            .context("spawn the touchpad worker")?;
        Ok(VirtualTouchpad {
            shared,
            worker: Some(worker),
        })
    }

    pub fn apply(&mut self, pad: &PadState) {
        self.shared.lock().present(pad);
    }
}

impl Drop for VirtualTouchpad {
    fn drop(&mut self) {
        let opened = {
            let mut state = self.shared.lock();
            state.shutdown = true;
            state.dev.is_some()
        };
        self.shared.wake.notify_all();
        // A worker still opening the devnode finishes on its own and removes it then.
        if let Some(worker) = self.worker.take().filter(|_| opened) {
            let _ = worker.join();
        }
    }
}

fn worker(shared: &Shared, index: Index) {
    let dev = match Device::open(&index) {
        Ok(dev) => dev,
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "virtual touchpad not created");
            return;
        }
    };
    let mut state = shared.lock();
    state.dev = Some(dev);
    while !state.shutdown {
        if state.dev.as_mut().is_some_and(|d| !d.service()) {
            tracing::warn!("virtual touchpad left this session's fence; removing it");
            break;
        }
        state.repeat_held();
        state = shared
            .wake
            .wait_timeout(state, SERVICE_TICK)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    let mut lifted = PadState::default();
    lifted.scan_time = state.scan.client;
    state.present(&lifted);
    let dev = state.dev.take();
    drop(state);
    drop(dev);
    drop(index);
}

/// Contacts in report slots. A slot's index is the Contact ID Windows sees for the contact's
/// whole life.
#[derive(Default)]
struct Slots {
    /// `(client id, last reported contact)`.
    slots: [Option<(u8, PtpContact)>; TOUCHPAD_MAX_CONTACTS],
    /// Client ids that found the pad full; out until they lift.
    suppressed: u32,
    button: bool,
    /// The last report carried a contact still down, or the button.
    held: bool,
}

impl Slots {
    /// The report for `pad`, then the slots as it leaves them: a contact gone from `pad` goes
    /// out once with Tip clear at its last position and confidence, then frees its slot.
    /// `None` when nothing is down now or was before.
    fn update(&mut self, pad: &PadState) -> Option<Vec<PtpContact>> {
        let mut out = Vec::with_capacity(TOUCHPAD_MAX_CONTACTS);
        for entry in &mut self.slots {
            let Some((client, last)) = entry else {
                continue;
            };
            if let Some(c) = pad.contacts().iter().find(|c| c.id == *client) {
                (last.x, last.y, last.confident) = (c.x, c.y, c.confident);
                out.push(*last);
            } else {
                out.push(PtpContact {
                    tip: false,
                    ..*last
                });
                *entry = None;
            }
        }
        self.suppressed &= pad.contacts().iter().fold(0, |m, c| m | 1 << c.id);
        for c in pad.contacts() {
            let known = self.slots.iter().flatten().any(|(id, _)| *id == c.id);
            if known || self.suppressed & 1 << c.id != 0 {
                continue;
            }
            let free = (0..TOUCHPAD_MAX_CONTACTS as u8)
                .find(|&s| self.slots[usize::from(s)].is_none() && !out.iter().any(|o| o.id == s));
            let Some(s) = free else {
                self.suppressed |= 1 << c.id;
                continue;
            };
            let contact = PtpContact {
                id: s,
                tip: true,
                confident: c.confident,
                x: c.x,
                y: c.y,
            };
            self.slots[usize::from(s)] = Some((c.id, contact));
            out.push(contact);
        }
        let held = out.iter().any(|c| c.tip) || pad.button;
        let report = (held || self.held).then_some(out);
        (self.held, self.button) = (held, pad.button);
        report
    }

    /// The contacts down now, for a device that just came up. `None` when nothing is held.
    fn current(&self) -> Option<Vec<PtpContact>> {
        let out: Vec<_> = self.slots.iter().flatten().map(|(_, c)| *c).collect();
        (!out.is_empty() || self.button).then_some(out)
    }
}

/// Scan Time: the client's capture time in 100 µs, wrapping. Windows 11 (26200) takes finger
/// speed from its steps (measured: arrival spacing changes nothing), so a report never repeats
/// the last one, and inside a touch never undercuts it: such a stamp (a doubted state and its
/// lift share one) goes out one tick later. A touch after an idle pad starts from the client's
/// time.
#[derive(Default)]
struct ScanTime {
    /// The latest state's capture time.
    client: u16,
    last: Option<u16>,
}

/// How far a stamp inside a touch may trail the last one sent and still read as not later.
/// Anything further back is a forward step that wrapped.
const SCAN_SLACK: u16 = 100;

impl ScanTime {
    /// `touching`: the last report held a contact or the button.
    fn stamp(&mut self, want: u16, touching: bool) -> u16 {
        let slack = if touching { SCAN_SLACK } else { 0 };
        let t = match self.last {
            Some(last) if want.wrapping_sub(last).wrapping_add(slack) <= slack => {
                last.wrapping_add(1)
            }
            _ => want,
        };
        self.last = Some(t);
        t
    }
}

/// The devnode, its sealed channel and doorbell.
struct Device {
    _sw: SwDevice,
    channel: PadChannel,
    attach: DriverAttach,
    instance_id: String,
    /// The HID children carry this host's session. No report goes out before.
    fenced: bool,
    doorbell: Option<OwnedHandle>,
    doorbell_next_try: Instant,
    /// Heartbeat last seen, and when it last moved.
    beat: (u32, Instant),
    full_warned: bool,
    /// When the last report went into the ring.
    pushed: Instant,
}

// SAFETY: the non-`Send` parts are the mapped section views and the `HSWDEVICE`. Both are
// process-wide, not thread-affine, and every access goes through `Shared`'s mutex.
unsafe impl Send for Device {}

impl Device {
    /// Mailbox and DATA first (index, then magic last), then the devnode, then the eager
    /// delivery: up to 1.5 s for the driver to answer, on the worker thread.
    fn open(index: &Index) -> Result<Device> {
        let boot_name = touchpad_boot_name(index.0);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        let shm = channel.data();
        shm.store_u32(OFF_PAD_INDEX, u32::from(index.0), Ordering::Relaxed);
        shm.store_u32(OFF_MAGIC, TOUCHPAD_MAGIC, Ordering::Release);
        let (sw, instance_id) = create_swdevice(&SwDeviceProfile {
            instance: &format!("pf_touchpad_{}", index.0),
            container_tag: 0x5046_5450, // "PFTP"
            container_index: index.0,
            hwid: "pf_touchpad",
            usb_vid_pid: Some("VID_5046&PID_5450"),
            usb_mi: None,
            bluetooth: false,
            description: "Punktfunk Virtual Touchpad",
            enumerator: "punktfunk",
            property: None,
        })
        .context("create the pf_touchpad devnode")?;
        let instance_id = instance_id.context("SwDeviceCreate named no pf_touchpad instance")?;
        channel.bind_devnode(
            u32::from(index.0),
            Some(instance_id.clone()),
            ProofTransport::HidSerialString,
        );
        channel.deliver_eager(Duration::from_millis(1500));
        tracing::info!(devnode = %instance_id, "virtual touchpad devnode created");
        Ok(Device {
            _sw: sw,
            channel,
            attach: DriverAttach::new(
                "pf_touchpad",
                "pf_touchpad.inf",
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pftouchpad-driver.log",
                boot_name,
                Some(instance_id.clone()),
            ),
            instance_id,
            fenced: false,
            doorbell: None,
            doorbell_next_try: Instant::now(),
            beat: (0, Instant::now()),
            full_warned: false,
            pushed: Instant::now(),
        })
    }

    /// Pump the channel, feed the attach watcher, open the doorbell, and read the fence once
    /// the driver made the HID children. `false`: they carry another session; drop the device.
    fn service(&mut self) -> bool {
        self.channel.pump();
        self.attach.observe(self.load(OFF_DRIVER_PROTO));
        if self.doorbell.is_none() && Instant::now() >= self.doorbell_next_try {
            self.doorbell_next_try = Instant::now() + DOORBELL_RETRY;
            self.doorbell = crate::channel_proof::hid_collection_path(
                &self.instance_id,
                DOORBELL_USAGE_PAGE,
                1,
            )
            .and_then(|path| crate::mouse_windows::open_for_write(&path).ok());
        }
        if self.fenced
            || crate::channel_proof::child_device_ids(&self.instance_id)
                .map_or(true, |kids| kids.is_empty())
        {
            return true;
        }
        match verify_fence(Some(&self.instance_id), FenceAt::HidChildren { pad: false }) {
            Ok(()) => self.fenced = true,
            Err(e) if PadCreateFault::broke_fence(&e) => return false,
            Err(_) => {}
        }
        true
    }

    /// Fenced, and the driver ticked within [`DRIVER_STALE`].
    fn ready(&mut self) -> bool {
        let hb = self.load(OFF_DRIVER_HEARTBEAT);
        if hb != self.beat.0 {
            self.beat = (hb, Instant::now());
        }
        self.fenced && self.load(OFF_DRIVER_PROTO) != 0 && self.beat.1.elapsed() < DRIVER_STALE
    }

    fn load(&self, off: usize) -> u32 {
        self.channel.data().load_u32(off, Ordering::Acquire)
    }

    /// Queue one report, ringing the doorbell when the driver may have stopped draining.
    fn push(&mut self, report: &[u8; TOUCHPAD_REPORT_LEN]) {
        let shm = self.channel.data();
        let head = shm.load_u32(OFF_RING_HEAD, Ordering::Relaxed);
        let tail = shm.load_u32(OFF_RING_TAIL, Ordering::SeqCst);
        if head.wrapping_sub(tail) as usize >= TOUCHPAD_RING_LEN {
            if !std::mem::replace(&mut self.full_warned, true) {
                tracing::warn!("virtual touchpad ring full; the driver stopped taking reports");
            }
            return;
        }
        self.full_warned = false;
        self.pushed = Instant::now();
        shm.write_bytes(ring_slot_off(head), report);
        shm.store_u32(OFF_RING_HEAD, head.wrapping_add(1), Ordering::SeqCst);
        // SeqCst against the driver's tail store and head load: its drain either sees this
        // report or stopped where this load sees it.
        if shm.load_u32(OFF_RING_TAIL, Ordering::SeqCst) == head {
            self.ring_doorbell();
        }
    }

    /// Make the driver drain now. Without a doorbell its timer drains within a tick.
    fn ring_doorbell(&mut self) {
        let Some(h) = &self.doorbell else {
            return;
        };
        let mut written = 0u32;
        // SAFETY: `h` is a live write handle owned by `self`; the buffer and `written` outlive
        // this synchronous call.
        let r = unsafe {
            WriteFile(
                HANDLE(h.as_raw_handle()),
                Some(&[DOORBELL_REPORT_ID, 0]),
                Some(&mut written),
                None,
            )
        };
        if r.is_err() {
            self.doorbell = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{TouchpadContact, TouchpadFrame};

    fn pad(button: bool, fingers: &[(u8, bool, bool, u16)]) -> PadState {
        let contacts: Vec<_> = fingers
            .iter()
            .map(|&(id, tip, confident, x)| TouchpadContact {
                id,
                tip,
                confident,
                x,
                y: 0,
            })
            .collect();
        TouchpadFrame::new(0, button, 3200, 2400, &contacts)
            .unwrap()
            .to_pad()
    }

    fn ids(c: &[PtpContact]) -> Vec<(u8, bool, bool)> {
        c.iter().map(|c| (c.id, c.tip, c.confident)).collect()
    }

    #[test]
    fn a_contact_keeps_its_slot_and_lifts_once_at_its_last_position() {
        let mut s = Slots::default();
        assert!(
            s.update(&pad(false, &[])).is_none(),
            "an idle pad sends nothing"
        );
        let down = s.update(&pad(false, &[(7, true, true, 0), (3, true, true, 65535)]));
        assert_eq!(ids(&down.unwrap()), [(0, true, true), (1, true, true)]);
        let moved = s.update(&pad(false, &[(3, true, true, 100)])).unwrap();
        assert_eq!(ids(&moved), [(0, false, true), (1, true, true)]);
        assert_eq!(moved[0].x, 0, "the lift keeps the last tip-down position");
        let back = s.update(&pad(false, &[(3, true, true, 100), (9, true, true, 5)]));
        assert_eq!(ids(&back.unwrap()), [(1, true, true), (0, true, true)]);
        assert_eq!(
            s.slots[0].unwrap().0,
            9,
            "slot 0 is free again for a new contact"
        );
        let up = s.update(&pad(false, &[])).unwrap();
        assert_eq!(ids(&up), [(0, false, true), (1, false, true)]);
        assert!(s.update(&pad(false, &[])).is_none());
    }

    #[test]
    fn taking_a_contact_back_keeps_confidence_clear_through_the_lift() {
        let mut s = Slots::default();
        s.update(&pad(false, &[(0, true, true, 10)]));
        let unsure = s.update(&pad(false, &[(0, true, false, 10)])).unwrap();
        assert_eq!(ids(&unsure), [(0, true, false)]);
        assert_eq!(
            ids(&s.update(&pad(false, &[])).unwrap()),
            [(0, false, false)]
        );
    }

    #[test]
    fn a_full_pad_keeps_a_late_contact_out_for_its_life() {
        let mut s = Slots::default();
        let five: Vec<_> = (0..5).map(|i| (i, true, true, 0)).collect();
        s.update(&pad(false, &five));
        let mut swap = five.clone();
        swap[0] = (20, true, true, 0);
        let r = s.update(&pad(false, &swap)).unwrap();
        assert_eq!(r.len(), 5, "the lift, four held, and no sixth");
        assert_eq!(ids(&r)[0], (0, false, true));
        let r = s.update(&pad(false, &swap)).unwrap();
        assert!(
            !r.iter().any(|c| c.id == 0),
            "20 stays out though slot 0 is free"
        );
        swap.remove(0);
        s.update(&pad(false, &swap));
        swap.push((20, true, true, 0));
        let r = s.update(&pad(false, &swap)).unwrap();
        assert!(
            r.iter().any(|c| c.id == 0 && c.tip),
            "20 lands after it lifted"
        );
    }

    #[test]
    fn the_button_alone_reports_and_the_present_state_survives_a_late_device() {
        let mut s = Slots::default();
        assert_eq!(s.update(&pad(true, &[])).map(|c| c.len()), Some(0));
        assert!(s.update(&pad(false, &[])).is_some(), "the release goes out");
        assert!(s.current().is_none());
        s.update(&pad(false, &[(4, true, true, 1)]));
        assert_eq!(ids(&s.current().unwrap()), [(0, true, true)]);
    }

    #[test]
    fn scan_time_follows_the_client_and_never_repeats() {
        let mut t = ScanTime::default();
        assert_eq!(t.stamp(500, false), 500);
        assert_eq!(
            t.stamp(500, true),
            501,
            "a doubted state and its lift share a time"
        );
        assert_eq!(t.stamp(500, true), 502);
        assert_eq!(t.stamp(580, true), 580);
        assert_eq!(t.stamp(30, true), 30, "wrapping forward");
        assert_eq!(
            t.stamp(65535, true),
            31,
            "just behind the last reads as not later"
        );
        assert_eq!(
            t.stamp(65530, false),
            65530,
            "a new touch keeps the client's time"
        );
        assert_eq!(t.stamp(65530, false), 65531, "but never repeats it");
    }

    #[test]
    fn only_a_reserved_connector_is_a_seat() {
        assert!(!is_seat(None));
        for console in ["", "0", "11", "16", "12.0", " 12"] {
            assert!(!is_seat(Some(console)), "{console}");
        }
        assert!(is_seat(Some("12")) && is_seat(Some("15")));
    }
}
