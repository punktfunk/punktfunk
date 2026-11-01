//! punktfunk virtual gamepad — UMDF2 HID minidriver, a port of the WDK `vhidmini2` sample.
//!
//! One driver presents the pad the host's `device_type` stamp names, answering from that kind's
//! row in `pf_driver_proto::pad_kind` and the feature blobs the host's `inject/` ships. The
//! host streams input in and reads output (rumble, lightbar, triggers) back.
//!
//! A singleton virtual device: no WDF object contexts, per-device state lives in statics.
//! The host channel is the sealed pad channel (design/gamepad-channel-sealing.md); its unsafe
//! surface lives in `pf_umdf_util`, so the only `unsafe` here is WDF setup FFI.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};

use pf_driver_proto::gamepad::{
    DEVTYPE_STEAMCONTROLLER, DEVTYPE_STEAMDECK, DEVTYPE_TRITON, PadShm, pad_serial,
};
use pf_driver_proto::pad_kind::{Family, PadKind, pad_kind, served_kind};
use pf_driver_proto::{deck, dualsense, dualshock4, eightbitdo};
use pf_umdf_util::channel::{ChannelClient, ChannelConfig};
use pf_umdf_util::hid::{
    IOCTL_HID_GET_DEVICE_ATTRIBUTES, IOCTL_HID_GET_DEVICE_DESCRIPTOR,
    IOCTL_HID_GET_REPORT_DESCRIPTOR, IOCTL_HID_GET_STRING, IOCTL_HID_READ_REPORT,
    IOCTL_HID_WRITE_REPORT, IOCTL_UMDF_HID_GET_FEATURE, IOCTL_UMDF_HID_GET_INPUT_REPORT,
    IOCTL_UMDF_HID_SET_FEATURE, IOCTL_UMDF_HID_SET_OUTPUT_REPORT, hex_dump, string_request_id,
};
use pf_umdf_util::skeleton::{
    self, STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS,
};
use pf_umdf_util::wdf::{self, Request};
use pf_umdf_util::{dbglog, nt_success};
use wdk_sys::{
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_PNPPOWER_EVENT_CALLBACKS, WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFQUEUE__, WDFREQUEST,
    call_unsafe_wdf_function_binding,
};

/// `HID_DEVICE_ATTRIBUTES` (32 bytes): Size, VendorID, ProductID, VersionNumber, Reserved[11].
fn hid_attrs(kind: &PadKind) -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&kind.vid.to_le_bytes());
    a[6..8].copy_from_slice(&kind.pid.to_le_bytes());
    a[8..10].copy_from_slice(&kind.version.to_le_bytes());
    a
}

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The latest input report the host pushed (report `0x01`) via shared memory; the timer delivers it
/// to pended game READ_REPORTs. Defaults to neutral until the host connects.
static INPUT_REPORT: std::sync::Mutex<[u8; 64]> = std::sync::Mutex::new(dualsense::NEUTRAL_REPORT);
/// Whether [`INPUT_REPORT`] holds a value no pended READ_REPORT has been completed with yet. Set
/// only when the latch actually CHANGES, cleared only when a request is actually completed, so a
/// tick that finds no read pended leaves the report undelivered rather than losing it. Consulted
/// by the Triton identity alone — see the delivery gate in [`tick`].
static INPUT_DIRTY: AtomicBool = AtomicBool::new(true);

// ---- the sealed pad channel: layouts + offsets from pf_driver_proto (drift = compile error) ----
// UMDF runs in WUDFHost.exe (user-mode) and hidclass blocks a control channel on the device stack
// (custom interface CreateFile → err 31; custom IOCTL on the HID handle → err 1) and UMDF has no
// control device. So the DATA section (`PadShm` — input report @8, output seq @72, output
// report @76, device_type @140, health marks @144/@148, pad_index @152, output-report ring
// @156..) is UNNAMED and reached only
// through a handle the SYSTEM host duplicated into this WUDFHost, bootstrapped over the named mailbox
// `Global\pfds-boot-<index>`. The handshake + all shared-memory access live in `pf_umdf_util`.
const SHM_MAGIC: u32 = pf_driver_proto::gamepad::PAD_MAGIC; // "PFDS"
const SHM_SIZE: usize = core::mem::size_of::<PadShm>();
const GAMEPAD_PROTO_VERSION: u32 = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION;

// PadShm field offsets (the driver reads input + device_type, writes output + health marks).
const OFF_INPUT: usize = core::mem::offset_of!(PadShm, input);
const OFF_OUT_SEQ: usize = core::mem::offset_of!(PadShm, out_seq);
const OFF_OUTPUT: usize = core::mem::offset_of!(PadShm, output);
const OFF_DEVICE_TYPE: usize = core::mem::offset_of!(PadShm, device_type);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(PadShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(PadShm, driver_heartbeat);
const OFF_DRIVER_REV: usize = core::mem::offset_of!(PadShm, driver_rev);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(PadShm, pad_index);
// v2.1/v2.2 output-report ring (see PadShm docs in pf_driver_proto).
const OFF_OUT_RING_VER: usize = core::mem::offset_of!(PadShm, out_ring_ver);
const OFF_RING_HEAD: usize = core::mem::offset_of!(PadShm, ring_head);
const OFF_OUT_RING_LEN: usize = core::mem::offset_of!(PadShm, out_ring_len);
const OFF_OUT_RING: usize = core::mem::offset_of!(PadShm, out_ring);
const OFF_INPUT_GEN: usize = core::mem::offset_of!(PadShm, input_gen);

/// How many timer ticks separate two runs of the channel/health housekeeping. The tick itself is
/// [`TIMER_PERIOD_MS`]; the pump, the `driver_proto` stamp and the heartbeat keep their historical
/// ~8 ms cadence so nothing that watches them changes rate — only the input path got faster.
const PUMP_EVERY_N_TICKS: u32 = 4;
/// Tick period. One pended READ_REPORT completes per tick, so this caps what a game observes: 2 ms
/// is about a real DualShock 4's Bluetooth cadence and leaves headroom above a Sony pad's 250 Hz.
/// The extra ticks only do the cheap half (read the input slot, complete one pended read), see
/// [`PUMP_EVERY_N_TICKS`].
const TIMER_PERIOD_MS: u32 = 2;

static TICKER_STOP: AtomicBool = AtomicBool::new(false);
static TICKER: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> = std::sync::Mutex::new(None);

const CREATE_WAITABLE_TIMER_HIGH_RESOLUTION: u32 = 0x2;
const TIMER_ALL_ACCESS: u32 = 0x001F_0003;
const INFINITE: u32 = u32::MAX;
const STATUS_INSUFFICIENT_RESOURCES: NTSTATUS = 0xC000_009A_u32 as NTSTATUS;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateWaitableTimerExW(
        attributes: *const c_void,
        name: *const u16,
        flags: u32,
        access: u32,
    ) -> *mut c_void;
    fn SetWaitableTimer(
        timer: *mut c_void,
        due: *const i64,
        period: i32,
        routine: *const c_void,
        context: *const c_void,
        resume: i32,
    ) -> i32;
    fn WaitForSingleObject(handle: *mut c_void, ms: u32) -> u32;
    fn CloseHandle(handle: *mut c_void) -> i32;
    fn GetCurrentThread() -> *mut c_void;
    fn SetThreadPriority(thread: *mut c_void, priority: i32) -> i32;
}

const THREAD_PRIORITY_TIME_CRITICAL: i32 = 15;

/// Runs [`tick`] every [`TIMER_PERIOD_MS`] until [`TICKER_STOP`]. WUDFHost stays on the 15.6 ms
/// system tick, and a WDF timer or a plain wait quantises to it (~64 reports/s, where a Sony pad
/// sends 250); a high-resolution waitable timer does not. Deadlines are absolute, so a late wake
/// never pushes the next one back. Time-critical: a tick is one copy and at most one completion,
/// and a game saturating the CPU must not stretch the report period.
fn tick_loop() {
    // SAFETY: the calling thread's own pseudo-handle and a plain priority constant.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) };
    // SAFETY: an unnamed timer with default security. Null (pre-1803 Windows has no such flag)
    // falls back to sleeping at the system tick.
    let timer = unsafe {
        CreateWaitableTimerExW(
            core::ptr::null(),
            core::ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    let period_ns = u64::from(TIMER_PERIOD_MS) * 1_000_000;
    let start = std::time::Instant::now();
    let mut n: u64 = 0;
    while !TICKER_STOP.load(Ordering::Relaxed) {
        let queue = MANUAL_QUEUE.load(Ordering::SeqCst);
        if !queue.is_null() {
            tick(queue);
        }
        let now_ns = start.elapsed().as_nanos() as u64;
        n = (n + 1).max(now_ns / period_ns + 1);
        let wait_ns = n * period_ns - now_ns;
        let due = -((wait_ns / 100).max(1) as i64);
        // SAFETY: our own timer handle; `due` outlives the call; no completion routine.
        let armed = !timer.is_null()
            && unsafe { SetWaitableTimer(timer, &due, 0, core::ptr::null(), core::ptr::null(), 0) }
                != 0;
        if armed {
            // SAFETY: the timer handle is live until the CloseHandle below.
            unsafe { WaitForSingleObject(timer, INFINITE) };
        } else {
            std::thread::sleep(std::time::Duration::from_nanos(wait_ns));
        }
    }
    if !timer.is_null() {
        // SAFETY: created above, closed once.
        unsafe { CloseHandle(timer) };
    }
}

extern "C" fn evt_self_managed_io_init(_device: WDFDEVICE) -> NTSTATUS {
    TICKER_STOP.store(false, Ordering::SeqCst);
    match std::thread::Builder::new()
        .name("pf-gamepad-tick".into())
        .spawn(tick_loop)
    {
        Ok(handle) => {
            *TICKER.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
            STATUS_SUCCESS
        }
        Err(e) => {
            dbglog!("[pf-gamepad] tick thread did not start: {e}");
            STATUS_INSUFFICIENT_RESOURCES
        }
    }
}

/// Joins the ticker before the framework deletes the manual queue it completes reads on.
extern "C" fn evt_self_managed_io_cleanup(_device: WDFDEVICE) {
    TICKER_STOP.store(true, Ordering::SeqCst);
    if let Some(handle) = TICKER.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = handle.join();
    }
}

/// Read the host's input report out of the section under the v2.3 seqlock, so a report caught
/// mid-copy is retried instead of handed to a game.
///
/// The host takes `input_gen` odd before writing the 64 bytes and even after, so an odd sample or
/// a changed one means the read straddled a write. One retry: the host publishes in microseconds
/// and this runs on a 2 ms timer, so a second collision is not a thing that happens, and if it did,
/// re-serving the previous whole report beats serving a torn one.
///
/// Against a pre-v2.3 host the field is never written, so it reads 0 — constant and even — and
/// this accepts on the first pass, exactly as the driver behaved before the seqlock existed.
/// `false` means "no whole report available"; the caller keeps what it had.
fn read_input_report(view: &pf_umdf_util::section::MappedView, buf: &mut [u8; 64]) -> bool {
    for _ in 0..2 {
        let before = view.load_u32(OFF_INPUT_GEN, Ordering::Acquire);
        if !before.is_multiple_of(2) {
            continue; // a write is in flight right now
        }
        view.read_bytes(OFF_INPUT, buf);
        // Acquire: the body reads above must not sink below this sample of the generation.
        if view.load_u32(OFF_INPUT_GEN, Ordering::Acquire) == before {
            return true;
        }
    }
    false
}
const OUT_SLOT_SIZE: usize = core::mem::size_of::<pf_driver_proto::gamepad::OutSlot>();
const OUT_RING_LEN: u32 = pf_driver_proto::gamepad::OUT_RING_LEN;
const OUT_RING_LEN_V22: u32 = pf_driver_proto::gamepad::OUT_RING_LEN_V22;

/// The output-ring length this side's slot math uses against the attached section — the driver's
/// half of the v2.2 negotiation (PadShm docs): the host's `out_ring_ver` stamp declares what it
/// can drain, the mapped length proves the slots exist in OUR view, and the shorter understanding
/// wins. `0` = no ring (pre-v2.1 host, or a fallback-size map too small to hold one) — legacy
/// latest-slot only. Constant per attachment (both inputs are fixed once the view exists).
fn ring_len(view: &pf_umdf_util::section::MappedView) -> u32 {
    if view.read_u32(OFF_OUT_RING_VER) == 0
        || view.mapped_len() < pf_driver_proto::gamepad::PAD_SHM_V21_SIZE
    {
        return 0;
    }
    if view.read_u32(OFF_OUT_RING_VER) >= 2
        && view.mapped_len() >= pf_driver_proto::gamepad::PAD_SHM_SIZE
    {
        OUT_RING_LEN_V22
    } else {
        OUT_RING_LEN
    }
}

/// Publish one game output report to the host: the legacy latest-report slot + `out_seq` bump
/// (every host generation reads this), and — when the host stamped `out_ring_ver` (it created the
/// ring region) and our view maps it — the lossless report ring: slot bytes first, then the
/// [`ring_len`] echo (the v2.2 length negotiation — a pre-v2.2 host never reads it), then the
/// `ring_head` bump with `Release` so the host's Acquire load can never observe the bump without
/// the slot bytes and the length that indexed them. The ring is what stops a rumble-STOP report
/// from being coalesced away by a following LED/trigger report inside one host poll window (the
/// confirmed stuck-rumble path).
///
/// `feature` ORs [`pf_driver_proto::triton::OUT_FEATURE_BIT`] (bit 31) into the ring slot's len —
/// the Triton identity's FEATURE/OUTPUT kind tag, stripped back out by the host's `drain_tagged`.
/// Only the ring carries the tag; the legacy latest-slot has no length field to tag, which is fine
/// because the one consumer that needs the split (triton_windows) always drains the ring. Every
/// pre-Triton call site passes `false` (the Deck host expects untagged frames), so plain lengths
/// are bit-identical to before the parameter existed.
fn publish_output(view: &pf_umdf_util::section::MappedView, bytes: &[u8], feature: bool) {
    // Serialized: the whole publish is a read-modify-write (read the cursor, write the slot it
    // names, then advance it) and the framework dispatches output callbacks in PARALLEL, so two
    // can be inside this at once. Unsynchronized, both read the same `ring_head`, both write the
    // SAME slot — tearing one report's bytes across the other's — and both store head+1, so the
    // cursor advances once for two reports and the host sees a single torn entry.
    //
    // An atomic `fetch_add` on the head does not fix it. That hands each writer a distinct slot,
    // but it advances the cursor BEFORE the slot bytes exist, so the host can read a slot that is
    // still being filled — trading a torn slot for a torn slot the host is invited to read. Making
    // the head-advance mean "the slot below is complete" is exactly what the lock buys.
    //
    // Poison-tolerant on purpose. Poison is sticky, so the repo's usual `if let Ok(g) = lock()`
    // would skip the publish for the REST OF THE PROCESS after a single panic elsewhere — silently
    // ending game output. Recovering the guard is safe here: the protected state is bytes in a
    // shared section, not an invariant a panic could have broken.
    let _publish = RING_PUBLISH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    view.write_bytes(OFF_OUTPUT, bytes);
    let seq = view.read_u32(OFF_OUT_SEQ).wrapping_add(1);
    // Release pairs with the host's Acquire load of `out_seq` (`pad_shm_ring.rs`), which orders
    // its copy of the report bytes after it. A plain write lets an ARM64 host see the new seq
    // before the bytes it announces.
    view.store_u32(OFF_OUT_SEQ, seq, Ordering::Release);
    let len = ring_len(view);
    if len != 0 {
        let head = view.read_u32(OFF_RING_HEAD);
        let slot = OFF_OUT_RING + (head % len) as usize * OUT_SLOT_SIZE;
        let n = bytes.len().min(64);
        let tag = if feature {
            pf_driver_proto::triton::OUT_FEATURE_BIT
        } else {
            0
        };
        view.write_u32(slot, n as u32 | tag);
        view.write_bytes(slot + 4, &bytes[..n]);
        view.write_u32(OFF_OUT_RING_LEN, len);
        view.store_u32(OFF_RING_HEAD, head.wrapping_add(1), Ordering::Release);
    }
}

/// Serializes [`publish_output`] against itself — see the note there for why an atomic cursor is
/// not enough. Uncontended in the common case: one output report at a time is the norm, and the
/// critical section is a few dozen bytes of memcpy into an already-mapped view.
static RING_PUBLISH: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The sealed-channel client (per-pad: `ProcessSharingDisabled` gives each pad its own WUDFHost, so
/// this static is per-pad). The handshake/adoption/validation state machine lives in `pf_umdf_util`.
static CHANNEL: ChannelClient = ChannelClient::new();
/// The last observed `device_type`: the neutral-report shape when the channel detaches, and the
/// fallback identity while unattached.
static LAST_DEVTYPE: AtomicU32 = AtomicU32::new(0);
/// The identity resolved from the devnode's PnP hardware ids at `EvtDeviceAdd`
/// ([`pf_driver_proto::gamepad::devtype_from_hwids`]); `u32::MAX` = not resolved. See
/// [`device_type`] for why this exists.
static PNP_DEVTYPE: AtomicU32 = AtomicU32::new(u32::MAX);
/// Timer ticks since load — picks the [`PUMP_EVERY_N_TICKS`] ticks that also do the channel
/// handshake and health marks. Wrapping is fine: only its residue matters.
static TICK: AtomicU32 = AtomicU32::new(0);

/// The pad's own clock: when it started (the first report served), the slot the next report is
/// due at in µs since then, and the index of the next report. See
/// [`pf_driver_proto::gamepad::serve_due`] and [`pf_driver_proto::gamepad::stamp_report_clock`].
static PAD_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static SERVE_DUE_US: AtomicU64 = AtomicU64::new(0);
static REPORT_SERIAL: AtomicU32 = AtomicU32::new(0);

fn pad_elapsed_us() -> u64 {
    PAD_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_micros() as u64
}

/// Last pump verdict, as in pf-xusb. `data()` returns the adopted view whatever the mailbox
/// says, so the three ticks between pumps would otherwise keep serving a departed host's last
/// report — a detached pad frozen mid-input instead of neutral.
static HOST_LIVE: AtomicBool = AtomicBool::new(false);

/// This pad's channel config (magic/size/pad_index offset + our logger).
fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-gamepad",
        boot_name_prefix: "Global\\pfds-boot-",
        data_magic: SHM_MAGIC,
        data_size: SHM_SIZE,
        // The v2.1 layout grew by tail extension (the output-report ring); against an old host's
        // 256-byte section the full-size map still succeeds (sections are page-granular), but if
        // it is ever refused, mapping the legacy size keeps the pad alive with the ring disabled.
        min_data_size: pf_driver_proto::gamepad::PAD_SHM_LEGACY_SIZE,
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
}

/// This pad's index, from its devnode Location at `EvtDeviceAdd`. Keys every per-pad identity
/// surface: the Deck unit id + serial, the PS pairing MAC (feature 0x09/0x12) and USB serial
/// string. SDL and hidapi read those at arrival, before the channel attaches, and dedup pads by
/// serial, so it cannot wait for the section. `adopt` refuses a section whose index differs.
fn pad_index() -> u8 {
    (CHANNEL.index() & 0xFF) as u8
}

// The bring-up file log. OPT-IN — debug builds, or the `PFGAMEPAD_DEBUG_LOG` env var — so a RELEASE
// driver never writes the file and never traps into the debugger. Path, sink and the gate live
// in `pf_umdf_util::log`, one copy for all four drivers.
pf_umdf_util::file_log!("pf_gamepad-driver.log", "PFGAMEPAD_DEBUG_LOG");

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log("[pf-gamepad] DriverEntry");
    // SAFETY: `driver`/`registry_path` are the loader's DriverEntry arguments.
    unsafe { skeleton::driver_create(driver, registry_path, Some(evt_device_add)) }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    log("[pf-gamepad] EvtDeviceAdd");

    // Mark as a filter (HID minidriver sits below mshidumdf.sys).
    // SAFETY: device_init is provided by the framework and non-null.
    unsafe { call_unsafe_wdf_function_binding!(WdfFdoInitSetFilter, device_init) };

    // The ticker starts once the device is up and is joined at removal, before its queue goes.
    let mut pnp = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: core::mem::size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG,
        EvtDeviceSelfManagedIoInit: Some(evt_self_managed_io_init),
        EvtDeviceSelfManagedIoCleanup: Some(evt_self_managed_io_cleanup),
        ..Default::default()
    };
    // SAFETY: device_init is the framework's live init struct, not yet consumed by WdfDeviceCreate.
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks,
            device_init,
            &mut pnp
        )
    };

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: device_init valid; attributes allowed null; device receives the handle.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device
        )
    };
    if !nt_success(st) {
        dbglog!("[pf-gamepad] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let shm_idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(shm_idx);
    dbglog!("[pf-gamepad] shm index = {shm_idx}");

    // Settle WHICH controller we are before hidclass asks (see `device_type`): the PnP hardware ids
    // are the only identity available this early, and every descriptor/attribute answer depends on it.
    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let hwids = unsafe { wdf::query_hardware_ids(device) };
    match pf_driver_proto::gamepad::devtype_from_hwids(&hwids) {
        Some(t) => {
            PNP_DEVTYPE.store(t as u32, Ordering::Relaxed);
            LAST_DEVTYPE.store(t as u32, Ordering::Relaxed);
            dbglog!("[pf-gamepad] identity from PnP hardware ids: device_type={t} ({hwids})");
        }
        // No pf_* id: a devnode this driver cannot name, or a failed property query. Refuse it,
        // so the devnode shows a PnP problem instead of a pad that claims to be a DualSense.
        None => {
            log(&format!(
                "[pf-gamepad] no pf_* hardware id in ({hwids}); refusing the device"
            ));
            return pf_driver_proto::gamepad::STATUS_NO_PAD_IDENTITY as NTSTATUS;
        }
    }

    let pnp = pad_kind(PNP_DEVTYPE.load(Ordering::Relaxed) as u8).map(|k| k.family);
    if matches!(pnp, Some(Family::Triton | Family::SteamController)) {
        // SAFETY: `device` is the live device just created — the exact contract this fn requires.
        let blob = unsafe {
            wdf::query_binary_property(
                device,
                pf_driver_proto::triton::IDENTITY_PROPKEY_FMTID,
                pf_driver_proto::triton::IDENTITY_PROPKEY_PID,
            )
        };
        if let Ok(mut g) = MIRROR_IDENTITY.lock() {
            *g = blob.unwrap_or_default();
        }
    }

    // Default parallel queue handling all IOCTLs.
    // SAFETY: `device` is the live device just created.
    if let Err(st) = unsafe { skeleton::create_default_queue(device, Some(evt_io_device_control)) }
    {
        dbglog!(
            "[pf-gamepad] default WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
        return st;
    }

    // Manual queue: pended READ_REPORT requests are completed by the timer.
    // SAFETY: `device` is the live device just created.
    let manual_queue = match unsafe { skeleton::create_manual_queue(device) } {
        Ok(q) => q,
        Err(st) => {
            dbglog!(
                "[pf-gamepad] manual WdfIoQueueCreate failed 0x{:08x}",
                st as u32
            );
            return st;
        }
    };
    MANUAL_QUEUE.store(manual_queue, Ordering::SeqCst);

    log("[pf-gamepad] device ready");
    STATUS_SUCCESS
}

extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    _output_len: usize,
    _input_len: usize,
    ioctl: ULONG,
) {
    // SAFETY: `request` is the live request for THIS EvtIoDeviceControl invocation — exactly the
    // contract `Request::new` requires. Everything after is safe (the token owns completion).
    let request = unsafe { Request::new(request) };

    // Skip the 8ms READ_REPORT cadence so the log stays readable during a game test;
    // the 0x02 OUTPUT report (the gate) and the descriptor handshake still log.
    if ioctl != IOCTL_HID_READ_REPORT {
        dbglog!("[pf-gamepad] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
    }

    // READ_REPORT forwards to the manual queue (the timer completes it) — this CONSUMES the request
    // token, so it's handled apart from the status-and-complete paths below.
    if ioctl == IOCTL_HID_READ_REPORT {
        let mq: WDFQUEUE = MANUAL_QUEUE.load(Ordering::SeqCst);
        // SAFETY: `mq` is the manual queue created in EvtDeviceAdd (a live WDFQUEUE of this device).
        match unsafe { request.forward_to_queue(mq) } {
            Ok(()) => {}                        // framework owns it now (completed by the timer)
            Err((req, st)) => req.complete(st), // forward failed → complete with the error
        }
        return;
    }

    let status: NTSTATUS = match ioctl {
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => {
            request.copy_to_output(&presented_kind().hid_descriptor())
        }
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => request.copy_to_output(&hid_attrs(presented_kind())),
        IOCTL_HID_GET_REPORT_DESCRIPTOR => request.copy_to_output(presented_kind().rdesc),
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => {
            on_output_report(&request, ioctl)
        }
        IOCTL_UMDF_HID_SET_FEATURE => on_set_feature(&request),
        IOCTL_UMDF_HID_GET_FEATURE => on_get_feature(&request),
        // Sliced to the kind's input length as in `tick`: a longer source is refused, not
        // truncated. Serves the current latch, so a reader that opens mid-session sees the true
        // state, and leaves `INPUT_DIRTY` alone: the interrupt pipeline still owes that report.
        IOCTL_UMDF_HID_GET_INPUT_REPORT => {
            let dt = device_type();
            let kind = served_kind(dt);
            let mut report = INPUT_REPORT
                .lock()
                .map(|g| *g)
                .unwrap_or(dualsense::NEUTRAL_REPORT);
            // The pad's clock as of now, not the host's stamp: a poll must agree with the stream.
            // The counter is not advanced — a poll is not a report in the interrupt pipeline.
            pf_driver_proto::gamepad::stamp_report_clock(
                dt,
                &mut report,
                REPORT_SERIAL.load(Ordering::Relaxed),
                pad_elapsed_us(),
            );
            let served: &[u8] = if kind.family == Family::Triton {
                // Same per-id trim as the timer's completion: Triton input reports are
                // variable-length and id-first; an undeclared latched id falls back to neutral.
                match pf_driver_proto::triton::input_len(report[0]) {
                    Some(len) => &report[..len],
                    None => &kind.neutral[..kind.input_len],
                }
            } else {
                &report[..kind.input_len]
            };
            request.copy_to_output(served)
        }
        IOCTL_HID_GET_STRING => on_get_string(&request),
        // The channel proof (see `pf_umdf_util::hid`): the host asks THIS devnode which process
        // serves it, and duplicates the DATA section into the answer — so it never has to trust the
        // LocalService-writable bootstrap mailbox to name its target.
        _ => STATUS_NOT_IMPLEMENTED,
    };

    dbglog!(
        "[pf-gamepad] ioctl 0x{ioctl:08x} -> 0x{:08x}",
        status as u32
    );
    request.complete(status);
}

// The 0x02 gate: a game writing an output report (rumble / lightbar / ADAPTIVE TRIGGERS). Per the
// UMDF marshalling convention the report data is the *input* buffer and the report id is carried in
// the *output* buffer length. We log it, then publish it to the DATA section for the host.
fn on_output_report(request: &Request, ioctl: ULONG) -> NTSTATUS {
    let (bytes, inlen) = match request.input_bytes(64) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let report_id = request.output_buffer_len() as u32; // report id, UMDF convention

    let kind = if ioctl == IOCTL_HID_WRITE_REPORT {
        "WRITE_REPORT"
    } else {
        "SET_OUTPUT_REPORT"
    };
    dbglog!(
        "[pf-gamepad] *** OUTPUT {kind} reportId={report_id} len={inlen} data: {}",
        hex_dump(&bytes, 48)
    );

    if presented_kind().family == Family::Switch {
        queue_switch_reply(&bytes);
    }

    // Publish the game's 0x02 output report to the sealed DATA section for the host (rumble /
    // lightbar / player-LEDs / adaptive triggers): legacy slot + seq, plus the v2.1 ring.
    // Triton OUTPUT reports (0x80.. haptics) flow through here too, untagged = OUTPUT kind; the
    // largest declared ones (0x87/0x88/0x89, 1 id + 63 payload = 64) exactly fit the 64-byte
    // ring slot, so nothing is ever truncated.
    if !bytes.is_empty() {
        match CHANNEL.data() {
            Some(view) => publish_output(view, &bytes, false),
            // Before the DATA section attaches (the host probes every 250 ms) a title's first
            // lightbar / trigger write would vanish, and a one-shot arm may never recur.
            None => {
                let n = bytes.len().min(64);
                let mut latched = [0u8; 64];
                latched[..n].copy_from_slice(&bytes[..n]);
                *PENDING_OUTPUT.lock().unwrap_or_else(|e| e.into_inner()) = Some((n, latched));
            }
        }
    }

    request.set_information(inlen as u64);
    STATUS_SUCCESS
}

/// Switch handshake replies waiting for a pended READ_REPORT, oldest first. A reader that
/// stops reading must not grow it without bound, so the oldest is dropped past eight.
static SWITCH_REPLIES: std::sync::Mutex<std::collections::VecDeque<[u8; 64]>> =
    std::sync::Mutex::new(std::collections::VecDeque::new());

/// Answer a Switch `0x80` command or `0x01` subcommand as the pad would, on the latched `0x30`
/// header. The host never sees the handshake; it reads the same report for rumble.
fn queue_switch_reply(output: &[u8]) {
    let latched = INPUT_REPORT
        .lock()
        .map(|g| *g)
        .unwrap_or(dualsense::NEUTRAL_REPORT);
    let Some(reply) = pf_driver_proto::switch::reply(&latched, output, device_type(), pad_index())
    else {
        return;
    };
    let mut q = SWITCH_REPLIES.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() == 8 {
        q.pop_front();
    }
    q.push_back(reply);
}

/// Complete the next pended READ_REPORT with the oldest queued Switch reply, ahead of the
/// periodic `0x30`. `true` when one went out. The reply takes the next timer value.
fn serve_switch_reply(queue: WDFQUEUE, now: u64) -> bool {
    let mut q = SWITCH_REPLIES.lock().unwrap_or_else(|e| e.into_inner());
    if q.is_empty() {
        return false;
    }
    // SAFETY: `queue` is the manual queue from EvtDeviceAdd, live until the ticker is joined.
    let Some(request) = (unsafe { wdf::retrieve_next_request(queue) }) else {
        return false;
    };
    let mut report = q.pop_front().unwrap_or(dualsense::NEUTRAL_REPORT);
    let serial = REPORT_SERIAL.fetch_add(1, Ordering::Relaxed);
    pf_driver_proto::gamepad::stamp_report_clock(device_type(), &mut report, serial, now);
    let st = request.copy_to_output(&report);
    request.complete(st);
    true
}

/// The last output report written before the DATA section attached, replayed once on attach.
static PENDING_OUTPUT: std::sync::Mutex<Option<(usize, [u8; 64])>> = std::sync::Mutex::new(None);

/// Deck identity: the last SET_FEATURE payload (the Steam command byte + args, minus the
/// report-id prefix). Steam's Deck contract is command-in-SET_FEATURE → answer-in-GET_FEATURE
/// on the one unnumbered feature report; the PS identities ignore this (their SET_FEATUREs are
/// fire-and-forget) — acking them is all they need.
static LAST_SET_FEATURE: std::sync::Mutex<[u8; 64]> = std::sync::Mutex::new([0; 64]);

/// Triton identity: the last SET_FEATURE frame, WHOLE (id-first, exactly as marshalled) plus its
/// true length — `pf_driver_proto::triton::feature_reply` wants the frame as SET, and the host's
/// drain replays the same bytes verbatim. Separate from [`LAST_SET_FEATURE`] because that latch
/// strips a leading `0x00` (the Deck's unnumbered-report marshalling), which would mangle a
/// numbered Triton frame. Per-pad like every static here: `ProcessSharingDisabled` gives each pad
/// its own WUDFHost (see [`INPUT_REPORT`]).
static TRITON_LAST_SET: std::sync::Mutex<([u8; 64], usize)> = std::sync::Mutex::new(([0; 64], 0));

/// The client's identity the host stored on this devnode (`triton::identity_blob`), read at
/// `EvtDeviceAdd` for a Steam Controller of either generation. Empty when the host set none.
static MIRROR_IDENTITY: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

/// The engraved serial of the pad this one mirrors, else the per-index one for `devtype`.
fn mirror_serial(devtype: u8) -> String {
    let blob = MIRROR_IDENTITY
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();
    match pf_driver_proto::triton::Identity::parse(&blob) {
        Some(id) if !id.serial.is_empty() => id.serial.into(),
        _ => pad_serial(devtype, pad_index()),
    }
}

/// The mirrored pad's own reply to `last_set` (id-first), when its client recorded one.
fn recorded_reply(last_set: &[u8]) -> Option<[u8; 64]> {
    let blob = MIRROR_IDENTITY.lock().ok()?;
    pf_driver_proto::triton::Identity::parse(&blob)?.reply(last_set)
}

/// Whether a latched Triton SET_FEATURE frame is the host's channel-proof command — the SAME
/// two-byte [`pf_driver_proto::gamepad::DECK_PROOF_CMD`] the Deck identity answers, riding the
/// same SET→GET feature contract. The `[0x00, cmd, …]` shape is the Deck/UNNUMBERED-report
/// marshalling of `channel_proof::ask_feature`; a numbered collection like this one never sees
/// it — hidclass rejects a feature buffer whose byte 0 is not a declared nonzero report id — so
/// the host's numbered leg frames the proof id-first, `[0x01, cmd, …]`. The driver accepts the
/// bare, `0x00`- and `0x01`-prefixed shapes alike to cover every sender rather than pinning one
/// marshalling (mirroring `triton::feature_reply`'s tolerance).
fn triton_proof_requested(frame: &[u8]) -> bool {
    let body = match frame {
        [0x00 | 0x01, rest @ ..] => rest,
        d => d,
    };
    body.starts_with(&pf_driver_proto::gamepad::DECK_PROOF_CMD)
}

// SET_FEATURE: ack (the PS identities' contract), latch the payload for the Deck's/Triton's
// GET_FEATURE answer, and — the Deck + Triton feedback paths — publish Steam's commands to the
// host. Per the UMDF marshalling convention the report data is the input buffer.
fn on_set_feature(request: &Request) -> NTSTATUS {
    let family = pad_kind(device_type()).map(|k| k.family);
    if let Ok((bytes, _)) = request.input_bytes(64) {
        if family == Some(Family::Triton) {
            // Latch the WHOLE id-first frame (see TRITON_LAST_SET), then republish it to the
            // host FEATURE-tagged — Steam's SET_REPORT features (lizard-off / IMU-enable /
            // settings) must reach the physical pad, and the tag is how the host's
            // `drain_tagged` tells them from interrupt OUTPUT reports.
            let n = bytes.len().min(64);
            if let Ok(mut g) = TRITON_LAST_SET.lock() {
                g.0.fill(0);
                g.0[..n].copy_from_slice(&bytes[..n]);
                g.1 = n;
            }
            if triton_proof_requested(&bytes[..n]) {
                // The channel-proof exchange is host↔driver plumbing; the client must never
                // see it — latched for the GET answer, NOT republished.
            } else if let Some(view) = CHANNEL.data() {
                publish_output(view, &bytes[..n], true);
            }
        } else {
            // The wire carries [report-id 0, cmd, …] for the unnumbered Steam report; store the
            // command-first view. (PS set-features carry their own report id first — harmless.)
            let src: &[u8] = if bytes.first() == Some(&0x00) && bytes.len() > 1 {
                &bytes[1..]
            } else {
                &bytes
            };
            if let Ok(mut g) = LAST_SET_FEATURE.lock() {
                g.fill(0);
                let n = src.len().min(64);
                g[..n].copy_from_slice(&src[..n]);
            }
            // Deck feedback: Steam drives rumble (0xEB) and trackpad haptic pulses (0x8F) via
            // SET_FEATURE on the unnumbered report — the PS identities get theirs as OUTPUT
            // reports instead. Publish them to the host through the same output slot + seq the
            // output path uses, re-prefixed with the report-id 0 byte so the host's
            // `parse_steam_output` sees the exact wire shape the Linux UHID path delivers.
            // Untagged: the Deck host expects plain frames.
            if family == Some(Family::SteamDeck)
                && matches!(src.first(), Some(&0xEB) | Some(&0x8F))
                && let Some(view) = CHANNEL.data()
            {
                let mut out = [0u8; 64];
                let n = src.len().min(63);
                out[1..1 + n].copy_from_slice(&src[..n]);
                publish_output(view, &out, false);
            }
            // 2015 Steam Controller: every SET but the proof goes to the host FEATURE-tagged,
            // re-prefixed with report id 0; the host decides which reach the physical pad.
            if family == Some(Family::SteamController)
                && !src.starts_with(&pf_driver_proto::gamepad::DECK_PROOF_CMD)
                && let Some(view) = CHANNEL.data()
            {
                let mut out = [0u8; 64];
                let n = src.len().min(63);
                out[1..1 + n].copy_from_slice(&src[..n]);
                publish_output(view, &out, true);
            }
        }
    }
    dbglog!("[pf-gamepad] SET_FEATURE (acked, latched for GET)");
    STATUS_SUCCESS
}

/// Deck identity: the GET_FEATURE reply to the latched SET_FEATURE command. The channel proof
/// rides the same command-and-response contract under [`DECK_PROOF_CMD`]'s two command bytes, so
/// it needs no descriptor change; every other command gets the shared [`deck::feature_reply`].
///
/// [`DECK_PROOF_CMD`]: pf_driver_proto::gamepad::DECK_PROOF_CMD
fn deck_feature_reply() -> [u8; 64] {
    let last = LAST_SET_FEATURE.lock().map(|g| *g).unwrap_or([0u8; 64]);
    if last.starts_with(&pf_driver_proto::gamepad::DECK_PROOF_CMD) {
        return proof_reply();
    }
    deck::feature_reply(&last, &pad_serial(DEVTYPE_STEAMDECK, pad_index()))
}

/// 2015 Steam Controller identity: the Deck's contract and proof, answered as the pad its
/// client captured where the host stored that pad's identity on the devnode.
fn sc_feature_answer() -> [u8; 64] {
    let last = LAST_SET_FEATURE.lock().map(|g| *g).unwrap_or([0u8; 64]);
    if last.starts_with(&pf_driver_proto::gamepad::DECK_PROOF_CMD) {
        return proof_reply();
    }
    deck::sc_mirror_reply(
        recorded_reply,
        &last,
        &mirror_serial(DEVTYPE_STEAMCONTROLLER),
    )
}

/// The channel-proof GET_FEATURE answer both command-driven identities (Deck + Triton) serve:
/// `[DECK_PROOF_CMD, ChannelProof(16 bytes), zeros…]`.
///
/// ⚠️ Security-load-bearing input: the proof carries `CHANNEL.index()`, the pad index this driver
/// read from its OWN devnode Location at `EvtDeviceAdd`, never a value read from a section. The
/// host cross-checks the proof's index against the pad it is about to deliver because it does not
/// yet trust any section; a section-derived index would let a forged delivery vouch for itself.
fn proof_reply() -> [u8; 64] {
    let proof = pf_driver_proto::gamepad::ChannelProof::new(CHANNEL.index(), std::process::id());
    let mut r = [0u8; 64];
    r[..2].copy_from_slice(&pf_driver_proto::gamepad::DECK_PROOF_CMD);
    r[2..18].copy_from_slice(&proof.to_bytes());
    r
}

// GET_FEATURE: report id from the input buffer; reply with the matching DualSense/DualShock 4 blob
// (the Deck identity instead answers the latched Steam command — its one feature report is
// unnumbered; the Triton identity answers its latched command through the shared
// `triton::feature_reply` machine).
fn on_get_feature(request: &Request) -> NTSTATUS {
    let devtype = device_type();
    let family = pad_kind(devtype).map(|k| k.family);
    if family == Some(Family::Triton) {
        let (last, len) = TRITON_LAST_SET.lock().map(|g| *g).unwrap_or(([0u8; 64], 0));
        let is_proof = triton_proof_requested(&last[..len]);
        let mut reply = if is_proof {
            proof_reply()
        } else if let Some(recorded) = recorded_reply(&last[..len]) {
            recorded
        } else {
            // The query dance (0x83 attributes / 0xAE string / 0xF2 firmware) + echo fallback —
            // and feature report 2 rides the SAME machine (mirror semantics, no special table).
            pf_driver_proto::triton::feature_reply(
                &last[..len],
                &mirror_serial(DEVTYPE_TRITON),
                pf_driver_proto::triton::unit_id(pad_index()),
            )
        };
        // A real pad echoes the feature id it was asked for, and this collection declares TWO
        // (0x01/0x02) while `triton::feature_reply` stamps every answer 0x01 — so a GET of
        // declared report 0x02 came back stamped 0x01. Read the requested id the way the PS arm
        // below does (input-buffer byte 0) and stamp it over the reply when it names a different
        // nonzero report. The proof reply is exempt: it is host↔driver plumbing framed
        // `[DECK_PROOF_CMD, proof…]`, and the host matches that command prefix — an id stamp
        // would destroy it.
        if !is_proof
            && let Ok((req, _)) = request.input_bytes(1)
            && let Some(&id) = req.first()
            && id != 0
            && id != reply[0]
        {
            reply[0] = id;
        }
        // The UMDF request's output-buffer length is authoritative: Steam asks with wLength 64
        // AND 65 (Phase-0 bench log — the 63-byte declared reports marshal as either), so serve
        // min(buffer_len, 64) zero-padded bytes and complete with that count.
        let n = request.output_buffer_len().min(64);
        return request.copy_to_output(&reply[..n]);
    }
    if family == Some(Family::SteamDeck) {
        return request.copy_to_output(&deck_feature_reply());
    }
    if family == Some(Family::SteamController) {
        return request.copy_to_output(&sc_feature_answer());
    }
    let (bytes, _) = match request.input_bytes(1) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let Some(&report_id) = bytes.first() else {
        return STATUS_INVALID_PARAMETER;
    };
    // The CHANNEL PROOF (security-review 2026-07-28): tell the host which process serves this
    // devnode, so it never has to trust the LocalService-writable bootstrap mailbox to name its
    // duplication target. `0x85` is already declared as a Feature report in all three captured
    // descriptors and was previously answered with STATUS_INVALID_PARAMETER, so this costs NO
    // report-descriptor change — the identity Steam and SDL fingerprint is untouched. Derived only
    // from our own devnode Location + pid: nothing a caller supplies feeds into it.
    if report_id == pf_driver_proto::gamepad::HID_FEATURE_REPORT_CHANNEL_PROOF {
        let len = request.output_buffer_len();
        return match pf_driver_proto::gamepad::ChannelProof::new(
            CHANNEL.index(),
            std::process::id(),
        )
        .to_feature_report(report_id, len)
        {
            Some(rep) => request.copy_to_output(&rep),
            None => STATUS_INVALID_PARAMETER, // caller's buffer can't hold id + proof
        };
    }
    // DualSense + Edge use feature ids 0x05/0x09/0x20 (same blobs — SDL forces enhanced-rumble
    // for the Edge PID regardless of the firmware version at 0x20[44..46]); DualShock 4 uses
    // 0x02/0x12/0xa3.
    // The pairing MAC is per pad and ends the GET_STRING serial in `on_get_string`.
    let ds_pairing = dualsense::pairing_reply(devtype, pad_index());
    let ds4_pairing = dualshock4::pairing_reply(pad_index());
    let caps = eightbitdo::caps_reply(devtype, pad_index());
    let blob: &[u8] = match (family, report_id) {
        (Some(Family::DualSense), 0x05) => &dualsense::FEATURE_CALIBRATION,
        (Some(Family::DualSense), 0x09) => &ds_pairing,
        (Some(Family::DualSense), 0x20) => &dualsense::FEATURE_FIRMWARE,
        (Some(Family::DualShock4), 0x02) => &dualshock4::FEATURE_CALIBRATION,
        (Some(Family::DualShock4), 0x12) => &ds4_pairing,
        (Some(Family::DualShock4), 0xA3) => &dualshock4::FEATURE_FIRMWARE,
        (_, eightbitdo::FEATURE_CAPS) if eightbitdo::has_caps(devtype) => &caps,
        (_, other) => {
            dbglog!("[pf-gamepad] GET_FEATURE unknown report id 0x{other:02x}");
            return STATUS_INVALID_PARAMETER;
        }
    };
    request.copy_to_output(blob)
}

// IOCTL_HID_GET_STRING: the input is a ULONG whose low word is the string id and whose high word is
// the language id. Reply with the requested device string as a NUL-terminated UTF-16 buffer. Native
// PS5 / Steam code reads these (HidD_GetProductString / HidD_GetSerialNumberString — the serial is one
// way they tell USB from BT). Observed live: Windows polls ids 0x0E/0x0F/0x10 (lang 0x0409)
// cyclically — the manufacturer/product/serial slots — NOT the 0/1/2 HID_STRING_ID_* constants; both.
fn on_get_string(request: &Request) -> NTSTATUS {
    let (bytes, _) = match request.input_bytes(4) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let (id_val, string_id) = string_request_id(&bytes);
    let devtype = device_type();
    dbglog!("[pf-gamepad] GET_STRING id=0x{string_id:04x} (raw 0x{id_val:08x}) devtype={devtype}");
    let kind = served_kind(devtype);
    let s: String = match string_id {
        0 | 0x000e => kind.manufacturer.into(),
        // Per-pad serials: SDL reads this via HidD_GetSerialNumberString and Steam dedups pads
        // by it. The PS serials end in the pairing MAC's low octet (`on_get_feature`); the Deck
        // and Triton serials match their 0xAE answers. Steam reads both.
        2 | 0x0010 if matches!(kind.family, Family::Triton | Family::SteamController) => {
            mirror_serial(devtype)
        }
        2 | 0x0010 => pad_serial(devtype, pad_index()),
        _ => kind.product.into(),
    };
    request.copy_utf16z_to_output(&s)
}

/// The `device_type` this pad presents. The attached section wins; until it attaches, the kind
/// the PnP hardware ids named at `EvtDeviceAdd` ([`PNP_DEVTYPE`]). hidclass asks for the
/// descriptors while it starts the device, before the host can deliver the section over the HID
/// interface those answers create, so the hardware ids are the only identity there is then.
/// Server's unfiltered Xbox line names one Xbox kind for all three; the section corrects it.
fn device_type() -> u8 {
    if let Some(view) = CHANNEL.data() {
        let t = view.read_u8(OFF_DEVICE_TYPE);
        LAST_DEVTYPE.store(t as u32, Ordering::Relaxed);
        return t;
    }
    let pnp = PNP_DEVTYPE.load(Ordering::Relaxed);
    if pnp != u32::MAX {
        return pnp as u8;
    }
    LAST_DEVTYPE.load(Ordering::Relaxed) as u8
}

/// The pad kind [`device_type`] names; a value this build does not know answers as a DualSense.
fn presented_kind() -> &'static PadKind {
    served_kind(device_type())
}

/// Latch the host's report when it is one this identity serves and it changed. The dirty
/// flag means "new state", not "the host published again": an unchanged republish carries
/// nothing a game can act on, and treating it as fresh would put the Triton path back on
/// the host's publish rate.
fn latch_published(view: &pf_umdf_util::section::MappedView) {
    let mut buf = [0u8; 64];
    // A torn read (host mid-publish) is dropped and the previous whole report stays. The
    // id check runs AFTER the read fills `buf`: hoisted, it would test zeros, fail for
    // every identity and serve neutral forever — the same look as a Steam-claim failure.
    if read_input_report(view, &mut buf)
        && pf_driver_proto::gamepad::report_declared(presented_kind(), &buf)
        && let Ok(mut g) = INPUT_REPORT.lock()
        && *g != buf
    {
        *g = buf;
        INPUT_DIRTY.store(true, Ordering::Relaxed);
    }
}

/// The health marks the host watches, on the ~8 ms cadence. The fallback identity first:
/// `device_type()`'s last resort (channel detached, no PnP match) reads LAST_DEVTYPE, and this
/// is the one place that always sees the attached section. Then driver_rev, driver_proto (the
/// attach signal; Release, so a host that sees it sees the revision) and driver_heartbeat
/// (+1 per ~8 ms = liveness): tells "driver bound and alive" from "package missing".
fn mark_health(view: &pf_umdf_util::section::MappedView) {
    LAST_DEVTYPE.store(view.read_u8(OFF_DEVICE_TYPE) as u32, Ordering::Relaxed);
    view.write_u32(OFF_DRIVER_REV, pf_driver_proto::gamepad::GAMEPAD_DRIVER_REV);
    view.store_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION, Ordering::Release);
    let hb = view.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
    view.write_u32(OFF_DRIVER_HEARTBEAT, hb);
}

/// Host gone (mailbox name vanished) or channel not attached yet: feed games the neutral
/// report instead of a frozen last state (matters for the persistent out-of-band devnode,
/// which outlives host sessions).
fn latch_neutral() {
    if let Ok(mut g) = INPUT_REPORT.lock() {
        let neutral = served_kind(LAST_DEVTYPE.load(Ordering::Relaxed) as u8).neutral;
        if *g != neutral {
            *g = neutral;
            INPUT_DIRTY.store(true, Ordering::Relaxed);
        }
    }
}

fn tick(queue: WDFQUEUE) {
    // Two cadences on one tick. Every tick ([`TIMER_PERIOD_MS`]) reads the report slot; the
    // identity's delivery gate below decides whether it also completes a pended READ_REPORT. The
    // handshake and health marks stay on ~8 ms ([`PUMP_EVERY_N_TICKS`]): the heartbeat's "+1 per
    // ~8 ms" is what the host reads as liveness.
    let tick = TICK.fetch_add(1, Ordering::Relaxed);
    let housekeeping = tick.is_multiple_of(PUMP_EVERY_N_TICKS);
    let view = if housekeeping {
        // Publish our pid / adopt a delivery / detect host-gone.
        let was_live = HOST_LIVE.load(Ordering::Relaxed);
        let v = CHANNEL.pump(&channel_cfg());
        HOST_LIVE.store(v.is_some(), Ordering::Relaxed);
        if let Some(view) = v
            && !was_live
            && let Some((n, bytes)) = PENDING_OUTPUT
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take()
        {
            publish_output(view, &bytes[..n], false);
        }
        v
    } else if HOST_LIVE.load(Ordering::Relaxed) {
        CHANNEL.data()
    } else {
        // Host gone at the last pump: serve neutral until one says otherwise, rather than
        // re-latching its final report for the next three ticks.
        None
    };
    match view {
        Some(view) => {
            latch_published(view);
            if housekeeping {
                mark_health(view);
            }
        }
        None => latch_neutral(),
    }

    // Serve by identity (`serve_tick`): the Switch reply first, since it answers a handshake
    // rather than the latch.
    let dt = device_type();
    let pad = served_kind(dt);
    let now = pad_elapsed_us();
    if pad.family == Family::Switch && serve_switch_reply(queue, now) {
        return;
    }
    match pf_driver_proto::gamepad::serve_tick(
        pad,
        INPUT_DIRTY.load(Ordering::Relaxed),
        now,
        SERVE_DUE_US.load(Ordering::Relaxed),
    ) {
        pf_driver_proto::gamepad::Serve::Skip => return,
        pf_driver_proto::gamepad::Serve::Now => {}
        pf_driver_proto::gamepad::Serve::NowThen(next) => {
            SERVE_DUE_US.store(next, Ordering::Relaxed)
        }
    }

    // Complete the next pended READ_REPORT with the current input report (safe queue/request API).
    // SAFETY: `queue` is the manual queue from EvtDeviceAdd, live until the ticker is joined in
    // EvtDeviceSelfManagedIoCleanup — the exact contract `retrieve_next_request` needs.
    if let Some(request) = unsafe { wdf::retrieve_next_request(queue) } {
        let mut report = INPUT_REPORT
            .lock()
            .map(|g| *g)
            .unwrap_or(dualsense::NEUTRAL_REPORT);
        let serial = REPORT_SERIAL.fetch_add(1, Ordering::Relaxed);
        pf_driver_proto::gamepad::stamp_report_clock(dt, &mut report, serial, now);
        // `served_bytes` is exactly what the descriptor declares: `copy_to_output` refuses a
        // longer source rather than truncating it. `complete(self, …)` consumes the request on
        // every path below (no Drop impl, so a dequeued READ_REPORT must not be left). The flag
        // clears HERE, not at the gate: a tick with nothing pended leaves the report undelivered.
        INPUT_DIRTY.store(false, Ordering::Relaxed);
        let st = request.copy_to_output(pf_driver_proto::gamepad::served_bytes(pad, &report));
        request.complete(st);
    }
}
