//! Resident virtual HID mouse + keyboard via the UMDF minidriver
//! (`packaging/windows/drivers/pf-mouse`).
//!
//! With no pointing device, win32k reports `SM_MOUSEPRESENT` = 0 and DWM never composites a
//! cursor into the pf-vdisplay frame. One `pf_mouse_<index>` devnode for the host process
//! lifetime makes Windows draw it. The console host also sends its mouse and keyboard input
//! through it ([`with_hid`]): a HID report reaches raw input with a device handle and no
//! injected flag, which kernel anti-cheat requires. `SendInput` stays the fallback.
//!
//! Transport is the sealed pad channel ([`PadChannel`], `design/gamepad-channel-sealing.md`):
//! the unnamed `MouseShm` duplicated into WUDFHost, bootstrapped via
//! `Global\pfmouse-boot-<index>` ([`mouse_index_for_slot`] — one host per seat, one index each).
//! [`ensure_resident`] never drops the devnode; it dies with the host service.

use super::gamepad_raii::{
    create_swdevice, DriverAttach, PadChannel, ProofTransport, SwDevice, SwDeviceProfile,
    TRUST_MAILBOX_ENV,
};
use anyhow::{Context, Result};
use pf_driver_proto::mouse::{
    abs_report, keyboard_report, mouse_boot_name, relative_report, ring_slot_off, MouseShm,
    DOORBELL_REPORT_ID, DOORBELL_USAGE_PAGE, KEYBOARD_BITMAP_LEN, MOUSE_FEATURE_RING, MOUSE_MAGIC,
    MOUSE_RING_LEN,
};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};

const SHM_SIZE: usize = core::mem::size_of::<MouseShm>();
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(MouseShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(MouseShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(MouseShm, pad_index);
const OFF_MAGIC: usize = core::mem::offset_of!(MouseShm, magic);
const OFF_FEATURES: usize = core::mem::offset_of!(MouseShm, driver_features);
const OFF_RING_HEAD: usize = core::mem::offset_of!(MouseShm, ring_head);
const OFF_RING_TAIL: usize = core::mem::offset_of!(MouseShm, ring_tail);
const OFF_WHEEL_COUNTS: usize = core::mem::offset_of!(MouseShm, wheel_counts);
const OFF_PAN_COUNTS: usize = core::mem::offset_of!(MouseShm, pan_counts);

const GENERIC_WRITE: u32 = 0x4000_0000;
/// The driver's timer stamps the heartbeat every 8–16 ms; a second without one means it
/// stopped taking reports.
const DRIVER_STALE: Duration = Duration::from_secs(1);
/// Between doorbell lookups while hidclass has not published the collection yet.
const DOORBELL_RETRY: Duration = Duration::from_secs(1);
/// `PUNKTFUNK_HID_INPUT=0` keeps every event on `SendInput`.
const HID_INPUT_ENV: &str = "PUNKTFUNK_HID_INPUT";

/// The reserved display connector a seat host owns; unset on the console.
/// `docs-site/content/docs/developers/multi-seat-contract.md` is the contract of record.
const SEAT_SLOT_ENV: &str = "PUNKTFUNK_SEAT_DISPLAY_SLOT";
const FIRST_SEAT_SLOT: u8 = 12;
const LAST_SEAT_SLOT: u8 = 15;

/// This host's mouse index, from [`SEAT_SLOT_ENV`]. Every OS name the mouse needs — the
/// `Global\pfmouse-boot-<i>` mailbox, the `pf_mouse_<i>` devnode, its container id — is
/// machine-wide, so the several hosts on a seats box must each pick a different `i`. A seat's
/// connector is already unique to it, so it is the index; the console keeps 0. A slot the
/// display plane would refuse reads as the console, which is what an ordinary host has always
/// done.
fn mouse_index_for_slot(raw: Option<&std::ffi::OsStr>) -> u8 {
    raw.and_then(std::ffi::OsStr::to_str)
        .and_then(|slot| slot.parse::<u8>().ok())
        .filter(|slot| (FIRST_SEAT_SLOT..=LAST_SEAT_SLOT).contains(slot))
        .unwrap_or(0)
}

/// Process-lifetime `pf_mouse_<index>` plus sealed `MouseShm`. Dropping it removes the device.
pub struct VirtualMouse {
    /// `None` if `SwDeviceCreate` failed; injection then uses an out-of-band devnode.
    _sw: Option<SwDevice>,
    channel: PadChannel,
    attach: DriverAttach,
    /// The devnode the doorbell collection hangs off. `None` on the out-of-band path.
    instance_id: Option<String>,
    /// Console host with HID input allowed: a seat's HID input would land in the console session.
    route_input: bool,
    /// Write handle on the doorbell collection; `None` until hidclass publishes it.
    doorbell: Option<OwnedHandle>,
    doorbell_next_try: Instant,
    /// Heartbeat last seen, and when it last moved.
    beat: (u32, Instant),
    /// Whether the last [`Self::ready`] said yes, to log each change once.
    was_ready: bool,
    full_warned: bool,
    /// What the device reports held, in HID button order (primary, secondary, middle, X1, X2).
    buttons: u8,
    keys: [u8; KEYBOARD_BITMAP_LEN],
    /// Wheel and pan motion below one count, in 1/120 counts.
    wheel_rem: [i64; 2],
}

// SAFETY: the non-`Send` parts are the mapped section views and the `HSWDEVICE`. Both are
// process-wide, not thread-affine, and every access goes through `RESIDENT`'s mutex or the one
// thread that owns a spike's instance.
unsafe impl Send for VirtualMouse {}

impl VirtualMouse {
    /// Unnamed DATA + `Global\pfmouse-boot-<index>` for this host's [`mouse_index_for_slot`].
    /// Stamp index, then magic LAST.
    pub fn open() -> Result<VirtualMouse> {
        let index = mouse_index_for_slot(std::env::var_os(SEAT_SLOT_ENV).as_deref());
        let boot_name = mouse_boot_name(index);
        let mut channel = PadChannel::create(boot_name.clone(), SHM_SIZE)?;
        // Index first, magic LAST — the same publish order the pads use.
        let shm = channel.data();
        shm.store_u32(OFF_PAD_INDEX, u32::from(index), Ordering::Relaxed);
        shm.store_u32(OFF_MAGIC, MOUSE_MAGIC, Ordering::Relaxed);
        let instance = format!("pf_mouse_{index}");
        let (sw, instance_id) = match create_swdevice(&SwDeviceProfile {
            instance: &instance,
            container_tag: 0x5046_4D4F, // "PFMO" — never grouped with a pad's container
            container_index: index,
            hwid: "pf_mouse",
            // Virtual identity (PF:MO). USB tokens are inert for a mouse; shared profile = one path.
            usb_vid_pid: Some("VID_5046&PID_4D4F"),
            usb_mi: None,
            bluetooth: false,
            description: "Punktfunk Virtual Mouse",
            enumerator: "punktfunk",
            property: None,
        }) {
            Ok((sw, id)) => (Some(sw), id),
            // Without a devnode the sealed channel refuses the mailbox pid, so the mouse would
            // never come up: fail, and the keeper retries. The trusted mailbox can still serve.
            Err(e) if std::env::var_os(TRUST_MAILBOX_ENV).is_none() => {
                return Err(e.context("create the pf_mouse devnode"));
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "SwDeviceCreate failed; falling back to an out-of-band pf_mouse devnode");
                (None, None)
            }
        };
        // Bind to this devnode's serving pid, not the LocalService-writable mailbox.
        channel.bind_devnode(
            u32::from(index),
            instance_id.clone(),
            ProofTransport::HidSerialString,
        );
        channel.deliver_eager(Duration::from_millis(1500));
        Ok(VirtualMouse {
            _sw: sw,
            channel,
            attach: DriverAttach::new(
                "pf_mouse",
                "pf_mouse.inf",
                "C:\\Windows\\ServiceProfiles\\LocalService\\AppData\\Local\\Temp\\pfmouse-driver.log",
                boot_name,
                instance_id.clone(),
            ),
            instance_id,
            route_input: index == 0 && std::env::var_os(HID_INPUT_ENV).is_none_or(|v| v != "0"),
            doorbell: None,
            doorbell_next_try: Instant::now(),
            beat: (0, Instant::now()),
            was_ready: false,
            full_warned: false,
            buttons: 0,
            keys: [0; KEYBOARD_BITMAP_LEN],
            wheel_rem: [0; 2],
        })
    }

    /// Pump sealed-channel delivery and feed the attach watcher (8 ms timer stamps `driver_proto`).
    pub fn service(&mut self) {
        self.channel.pump();
        self.attach.observe(self.driver_proto());
    }

    /// The driver drains the ring and ticked within [`DRIVER_STALE`].
    fn ready(&mut self) -> bool {
        let shm = self.channel.data();
        let ring = shm.load_u32(OFF_FEATURES, Ordering::Acquire) & MOUSE_FEATURE_RING != 0;
        let hb = shm.load_u32(OFF_DRIVER_HEARTBEAT, Ordering::Relaxed);
        if hb != self.beat.0 {
            self.beat = (hb, Instant::now());
        }
        let ready = ring && self.beat.1.elapsed() < DRIVER_STALE;
        if ready != self.was_ready {
            self.was_ready = ready;
            if ready {
                tracing::info!("input: mouse and keyboard go through the virtual HID device");
            } else {
                tracing::warn!(
                    driver_ring = ring,
                    "input: virtual HID device not taking reports — using SendInput"
                );
            }
        }
        ready
    }

    /// Queue one report, ringing the doorbell when the driver may have stopped draining.
    fn push(&mut self, report: &[u8]) {
        let shm = self.channel.data();
        let head = shm.load_u32(OFF_RING_HEAD, Ordering::Relaxed);
        let tail = shm.load_u32(OFF_RING_TAIL, Ordering::SeqCst);
        if head.wrapping_sub(tail) as usize >= MOUSE_RING_LEN {
            if !std::mem::replace(&mut self.full_warned, true) {
                tracing::warn!("virtual HID ring full — the driver stopped taking reports");
            }
            return;
        }
        self.full_warned = false;
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
        if self.doorbell.is_none() && Instant::now() >= self.doorbell_next_try {
            self.doorbell_next_try = Instant::now() + DOORBELL_RETRY;
            self.doorbell = self.open_doorbell();
        }
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
        if let Err(e) = r {
            tracing::debug!(error = %e, "virtual HID doorbell write failed — reopening");
            self.doorbell = None;
        }
    }

    fn open_doorbell(&self) -> Option<OwnedHandle> {
        let path = crate::channel_proof::hid_collection_path(
            self.instance_id.as_deref()?,
            DOORBELL_USAGE_PAGE,
            1,
        )?;
        open_for_write(&path)
            .inspect_err(
                |e| tracing::debug!(error = %format!("{e:#}"), "virtual HID doorbell open"),
            )
            .ok()
    }

    /// Relative pointer motion, split into reports of at most ±32767.
    pub fn move_by(&mut self, mut dx: i32, mut dy: i32) {
        const MAX: i32 = i16::MAX as i32;
        while dx != 0 || dy != 0 {
            let (sx, sy) = (dx.clamp(-MAX, MAX), dy.clamp(-MAX, MAX));
            self.push(&relative_report(self.buttons, sx as i16, sy as i16, 0, 0));
            (dx, dy) = (dx - sx, dy - sy);
        }
    }

    /// Absolute pointer position on the primary monitor, `0..=32767` per axis.
    pub fn move_to(&mut self, x: u16, y: u16) {
        self.push(&abs_report(x, y));
    }

    /// Button `bit` (HID order: primary, secondary, middle, X1, X2) down or up. Buttons travel
    /// the relative collection whichever collection moved the pointer.
    pub fn button(&mut self, bit: u8, down: bool) {
        let held = if down {
            self.buttons | 1 << bit
        } else {
            self.buttons & !(1 << bit)
        };
        if held != self.buttons {
            self.buttons = held;
            self.push(&relative_report(held, 0, 0, 0, 0));
        }
    }

    /// Wheel motion in 1/120 notches, scaled to the counts per notch Windows set up.
    pub fn scroll(&mut self, horizontal: bool, v120: i32) {
        let off = if horizontal {
            OFF_PAN_COUNTS
        } else {
            OFF_WHEEL_COUNTS
        };
        let per_notch = i64::from(self.channel.data().load_u32(off, Ordering::Relaxed).max(1));
        let axis = usize::from(horizontal);
        let total = self.wheel_rem[axis] + i64::from(v120) * per_notch;
        let mut counts = total / 120;
        self.wheel_rem[axis] = total - counts * 120;
        while counts != 0 {
            let c = counts.clamp(-i64::from(i16::MAX), i64::from(i16::MAX));
            let (wheel, pan) = if horizontal { (0, c) } else { (c, 0) };
            self.push(&relative_report(
                self.buttons,
                0,
                0,
                wheel as i16,
                pan as i16,
            ));
            counts -= c;
        }
    }

    /// Key `usage` (keyboard page) down or up. A repeated down changes nothing: Windows
    /// repeats a held HID key itself.
    pub fn key(&mut self, usage: u8, down: bool) {
        let Some(byte) = self.keys.get_mut(usize::from(usage / 8)) else {
            return;
        };
        let bit = 1 << (usage % 8);
        let held = if down { *byte | bit } else { *byte & !bit };
        if held != *byte {
            *byte = held;
            let report = keyboard_report(&self.keys);
            self.push(&report);
        }
    }

    /// Whether the doorbell collection is open, so reports leave without waiting for a tick.
    pub fn doorbell_open(&self) -> bool {
        self.doorbell.is_some()
    }

    fn driver_proto(&self) -> u32 {
        self.channel
            .data()
            .load_u32(OFF_DRIVER_PROTO, Ordering::Relaxed)
    }

    fn driver_heartbeat(&self) -> u32 {
        self.channel
            .data()
            .load_u32(OFF_DRIVER_HEARTBEAT, Ordering::Relaxed)
    }
}

/// `CreateFileW` for writing, as a HID vendor collection allows.
fn open_for_write(path: &str) -> Result<OwnedHandle> {
    let wide = HSTRING::from(path);
    // SAFETY: `wide` is a valid NUL-terminated UTF-16 path for the duration of the call; the
    // returned handle is owned solely by the `OwnedHandle` built from it.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            None,
        )
        .with_context(|| format!("open {path} for write"))?
    };
    // SAFETY: `h` is the fresh handle just opened, moved into a single owner that closes it.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0 as _) })
}

/// The resident device, once the keeper opened it.
static RESIDENT: Mutex<Option<VirtualMouse>> = Mutex::new(None);

/// Run `f` on the resident device when it carries this host's input: the console host,
/// [`HID_INPUT_ENV`] not `0`, and a driver taking reports. `None` = use `SendInput`.
pub(crate) fn with_hid<R>(f: impl FnOnce(&mut VirtualMouse) -> R) -> Option<R> {
    let mut resident = RESIDENT.lock().unwrap_or_else(PoisonError::into_inner);
    let m = resident.as_mut()?;
    (m.route_input && m.ready()).then(|| f(m))
}

/// Ensure the one process-wide virtual mouse exists. Called from
/// [`InjectorService`](crate::InjectorService) start; native + GameStream share it.
/// The keeper thread owns the devnode for the process lifetime.
///
/// `PUNKTFUNK_NO_VIRTUAL_MOUSE=1` opts out.
pub(crate) fn ensure_resident() {
    use std::sync::OnceLock;
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        if std::env::var_os("PUNKTFUNK_NO_VIRTUAL_MOUSE").is_some_and(|v| v != "0") {
            tracing::info!(
                "virtual HID mouse disabled (PUNKTFUNK_NO_VIRTUAL_MOUSE) — with no physical \
                 pointer attached, Windows will not draw a cursor into the stream"
            );
            return;
        }
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-vmouse".into())
            .spawn(keeper_thread)
        {
            tracing::warn!(error = %e, "virtual-mouse keeper thread spawn failed");
        }
    });
}

/// Open-with-retry, then publish the device to [`RESIDENT`] and pump it every 250 ms. Open
/// fails on a mailbox squat (another host); a missing driver is not an open failure
/// (`DriverAttach` diagnoses via the pump).
fn keeper_thread() {
    loop {
        match VirtualMouse::open() {
            Ok(m) => {
                tracing::info!(
                    "resident virtual HID mouse created (pf_mouse — keeps SM_MOUSEPRESENT true \
                     so DWM composites the cursor on headless hosts)"
                );
                *RESIDENT.lock().unwrap_or_else(PoisonError::into_inner) = Some(m);
                loop {
                    if let Some(m) = RESIDENT
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_mut()
                    {
                        m.service();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %format!("{e:#}"),
                    "virtual HID mouse open failed — retrying in 60s (headless hosts stream an \
                     invisible cursor until it exists)"
                );
                std::thread::sleep(Duration::from_secs(60));
            }
        }
    }
}

/// Open a stand-alone device for the devtests and wait up to 10 s for its driver to take
/// reports. Stop the host service first: it owns the mailbox.
pub fn open_for_spike() -> Result<VirtualMouse> {
    let mut m = VirtualMouse::open()?;
    println!("virtual HID devnode up (5046:4D4F) — waiting for the driver to take reports…");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !m.ready() && Instant::now() < deadline {
        m.service();
        std::thread::sleep(Duration::from_millis(50));
    }
    if m.driver_proto() == 0 {
        anyhow::bail!(
            "the pf_mouse driver never attached. Install it: punktfunk-host.exe driver install \
             --gamepad --dir <stage>"
        );
    }
    if !m.ready() {
        anyhow::bail!("the installed pf_mouse driver predates the report ring — reinstall it");
    }
    Ok(m)
}

/// `vmouse-spike`: drive the real cursor through HID reports for `secs`. The sweep glides
/// the pointer left↔right at mid-screen with a wheel notch every second. `relative` instead
/// counts down 5 s to focus a game, then yaws the view and clicks every 2 s.
pub fn spike_hold(secs: u64, relative: bool) -> Result<()> {
    let mut m = open_for_spike()?;
    if relative {
        println!("Focus the game now: relative motion starts in 5 s.");
        for left in (1..=5).rev() {
            println!("  {left}…");
            m.service();
            std::thread::sleep(Duration::from_secs(1));
        }
        println!(
            "Relative reports for {secs}s: the view should yaw left↔right, firing every 2 s. \
             A still view means the game ignores this device."
        );
    } else {
        println!("Sweeping the cursor for {secs}s — it should glide left↔right across mid-screen.");
    }
    let t0 = Instant::now();
    let beat_before = m.driver_heartbeat();
    let mut i: u64 = 0;
    while t0.elapsed() < Duration::from_secs(secs) {
        let phase = (i % 240) as u16; // 240 steps × 16 ms ≈ 4 s per round trip
        if relative {
            m.move_by(if phase < 120 { 6 } else { -6 }, 0);
            m.button(0, (60..66).contains(&(phase % 120)));
        } else {
            let tri = if phase < 120 { phase } else { 240 - phase };
            m.move_to(4096 + tri * (24576 / 120), 0x4000);
            if i % 60 == 0 {
                m.scroll(false, 120);
            }
        }
        m.service();
        i += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    m.button(0, false); // never leave the button down
    let ticks = m.driver_heartbeat().wrapping_sub(beat_before);
    println!(
        "vmouse-spike: done (driver timer ≈{:.0} Hz). Devnode removed on exit.",
        f64::from(ticks) / t0.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Throwaway `pf_mouse_probe` at pad index 9: print which HID IOCTL hidclass
/// forwards to a UMDF minidriver (`HidD_GetIndexedString` vs `IOCTL_HID_GET_STRING`).
/// The mailbox is LocalService-writable so the pad channel trusts the devnode's
/// [`pf_driver_proto::gamepad::ChannelProof`] instead. Needs `pf_mouse` installed.
pub fn channel_proof_probe() -> Result<()> {
    use crate::channel_proof::{self, ProofTransport};

    /// A pad index no real pad uses, so the proof's index check is actually exercised and the
    /// probe can never be confused with the resident mouse at 0.
    const PROBE_INDEX: u8 = 9;

    println!("creating a throwaway pf_mouse devnode (pad index {PROBE_INDEX})…");
    let (_sw, instance_id) = create_swdevice(&SwDeviceProfile {
        instance: "pf_mouse_probe",
        container_tag: 0x5046_4D4F, // "PFMO"
        container_index: PROBE_INDEX,
        hwid: "pf_mouse",
        usb_vid_pid: Some("VID_5046&PID_4D4F"),
        usb_mi: None,
        bluetooth: false,
        description: "Punktfunk Virtual Mouse (channel-proof probe)",
        enumerator: "punktfunk",
        property: None,
    })?;
    let Some(instance_id) = instance_id else {
        anyhow::bail!("SwDeviceCreate reported no instance id to look the devnode up by");
    };

    // Poll: PnP + hidclass publish in tens of ms; a fixed sleep would miss a slow box.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let report = loop {
        let r = channel_proof::diagnose(
            &instance_id,
            ProofTransport::HidSerialString,
            PROBE_INDEX as u32,
        );
        if r.contains("ChannelProof") || std::time::Instant::now() >= deadline {
            break r;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    println!("\n{report}");

    match channel_proof::probe_pid(
        &instance_id,
        ProofTransport::HidSerialString,
        PROBE_INDEX as u32,
    ) {
        Ok(pid) => println!(
            "RESULT: the devnode proved its driver is pid {pid} — the HID channel proof WORKS on \
             this build of Windows, so the pad channel never has to trust the mailbox."
        ),
        Err(e) => println!(
            "RESULT: no usable channel proof ({e:#}).\n\
             If BOTH HID lines above say \"call failed\", hidclass on this build forwards neither \
             IOCTL to a UMDF minidriver and the HID pads/mouse need a different transport (the \
             xusb leg is unaffected — it owns its own device interface). If one says \"answered, \
             but not a proof\", an OLD pf_mouse driver is installed: reinstall with\n\
             \x20  punktfunk-host.exe driver install --gamepad"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    /// Two seat hosts must never derive the same mouse index, and the console keeps 0.
    #[test]
    fn only_a_reserved_seat_connector_moves_the_mouse_off_index_zero() {
        assert_eq!(mouse_index_for_slot(None), 0);
        for refused in [
            "", "0", "11", "16", "255", "999", "12.0", "-1", " 12", "12 ",
        ] {
            assert_eq!(
                mouse_index_for_slot(Some(OsStr::new(refused))),
                0,
                "{refused}"
            );
        }
        let mut seen = Vec::new();
        for slot in FIRST_SEAT_SLOT..=LAST_SEAT_SLOT {
            let index = mouse_index_for_slot(Some(OsStr::new(&slot.to_string())));
            assert_eq!(index, slot);
            assert!(
                !seen.contains(&index),
                "seat slot {slot} reused index {index}"
            );
            seen.push(index);
        }
        assert!(
            !seen.contains(&0),
            "a seat must not land on the console index"
        );
    }
}
