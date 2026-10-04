//! punktfunk virtual HID mouse + keyboard — UMDF2 HID minidriver, identity PF:MO (obviously
//! virtual).
//!
//! A resident pointer keeps `SM_MOUSEPRESENT` true, so DWM draws the cursor on a headless host.
//! The host's input arrives as HID reports and reaches raw input like a USB device's: absolute
//! pointer (0x01), relative pointer (0x02), keyboard (0x03). Layout and descriptor:
//! `pf_driver_proto::mouse`.
//!
//! The host channel is the sealed pad channel (design/gamepad-channel-sealing.md); its unsafe
//! surface lives in `pf_umdf_util`, so the only `unsafe` here is WDF setup FFI.
//!
//! Delivery is event-driven: reports wait in the shared ring until a READ_REPORT takes them,
//! drained when a read arrives, when the host rings the doorbell, and on each timer tick. An
//! idle device sends nothing — a constant stream would read as user activity.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]
// Every remaining `unsafe {}` (all WDF setup FFI) must carry a `// SAFETY:` proof.

use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use std::sync::Mutex;

use pf_driver_proto::gamepad::ChannelProof;
use pf_driver_proto::mouse::{
    DOORBELL_REPORT_LEN, MOUSE_FEATURE_RING, MOUSE_PID, MOUSE_RDESC, MOUSE_RDESC_LEN,
    MOUSE_REL_FEATURE_LEN, MOUSE_REL_REPORT_ID, MOUSE_REPORT_ID, MOUSE_REPORT_LEN, MOUSE_RING_LEN,
    MOUSE_RING_SLOT, MOUSE_SHM_LEGACY_SIZE, MOUSE_VER, MOUSE_VID, MouseShm, WHEEL_MULTIPLIER,
    input_report_len, ring_slot_off,
};
use pf_umdf_util::channel::{ChannelClient, ChannelConfig};
use pf_umdf_util::hid::{
    IOCTL_HID_GET_DEVICE_ATTRIBUTES, IOCTL_HID_GET_DEVICE_DESCRIPTOR,
    IOCTL_HID_GET_REPORT_DESCRIPTOR, IOCTL_HID_GET_STRING, IOCTL_HID_READ_REPORT,
    IOCTL_HID_WRITE_REPORT, IOCTL_UMDF_HID_GET_FEATURE, IOCTL_UMDF_HID_GET_INPUT_REPORT,
    IOCTL_UMDF_HID_SET_FEATURE, IOCTL_UMDF_HID_SET_OUTPUT_REPORT, string_request_id,
};
use pf_umdf_util::skeleton::{
    self, STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED, STATUS_SUCCESS,
};
use pf_umdf_util::wdf::{self, Request};
use pf_umdf_util::{dbglog, nt_success};
use wdk_sys::{
    NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG, WDF_NO_OBJECT_ATTRIBUTES,
    WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFQUEUE__, WDFREQUEST, WDFTIMER,
    call_unsafe_wdf_function_binding,
};

// HID descriptor (9 bytes, packed): len, type=0x21, bcdHID=0x0100, country=0, numDesc=1, then
// {reportType=0x22, wReportLength}, computed from MOUSE_RDESC so the two never drift.
static HID_DESC: [u8; 9] = {
    let [lo, hi] = (MOUSE_RDESC_LEN as u16).to_le_bytes();
    [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, lo, hi]
};

// HID_DEVICE_ATTRIBUTES (32 bytes): Size(u32)=32, VendorID, ProductID, VersionNumber, Reserved[11].
fn hid_attrs() -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&MOUSE_VID.to_le_bytes());
    a[6..8].copy_from_slice(&MOUSE_PID.to_le_bytes());
    a[8..10].copy_from_slice(&MOUSE_VER.to_le_bytes());
    a
}

/// The absolute report before the host published one: id + all-zero state. Only ever a
/// GET_INPUT_REPORT answer, never fed into the input stream.
const NEUTRAL_ABS: [u8; MOUSE_REPORT_LEN] = {
    let mut r = [0u8; MOUSE_REPORT_LEN];
    r[0] = MOUSE_REPORT_ID;
    r
};

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The last absolute report handed to Windows, for GET_INPUT_REPORT. Relative and keyboard
/// queries read zero: a delta must not replay, and held keys are nobody else's business.
static LAST_ABS: Mutex<[u8; MOUSE_REPORT_LEN]> = Mutex::new(NEUTRAL_ABS);
/// The relative collection's feature byte as Windows last set it (Resolution Multiplier bits).
static MULTIPLIERS: AtomicU8 = AtomicU8::new(0);
/// One drain at a time: READ_REPORT, the doorbell and the timer all drain.
static DRAIN: Mutex<()> = Mutex::new(());

// ---- the sealed host channel: layouts + offsets from pf_driver_proto (drift = compile error) ----
const SHM_MAGIC: u32 = pf_driver_proto::mouse::MOUSE_MAGIC; // "PFMO"
const SHM_SIZE: usize = core::mem::size_of::<MouseShm>();
const GAMEPAD_PROTO_VERSION: u32 = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION;

// MouseShm field offsets (the driver drains the ring, writes the health marks).
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(MouseShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(MouseShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(MouseShm, pad_index);
const OFF_FEATURES: usize = core::mem::offset_of!(MouseShm, driver_features);
const OFF_RING_HEAD: usize = core::mem::offset_of!(MouseShm, ring_head);
const OFF_RING_TAIL: usize = core::mem::offset_of!(MouseShm, ring_tail);
const OFF_WHEEL_COUNTS: usize = core::mem::offset_of!(MouseShm, wheel_counts);
const OFF_PAN_COUNTS: usize = core::mem::offset_of!(MouseShm, pan_counts);

/// The sealed-channel client (`ProcessSharingDisabled` gives the mouse its own WUDFHost, so this
/// static is per-device). The handshake/adoption/validation state machine lives in `pf_umdf_util`.
static CHANNEL: ChannelClient = ChannelClient::new();

/// This device's channel config (magic/size/index offset + our logger).
fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-mouse",
        boot_name_prefix: "Global\\pfmouse-boot-",
        data_magic: SHM_MAGIC,
        data_size: SHM_SIZE,
        min_data_size: MOUSE_SHM_LEGACY_SIZE, // a host that predates the ring
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
}

// The bring-up file log. OPT-IN — debug builds, or the `PFMOUSE_DEBUG_LOG` env var — so a RELEASE
// driver never writes the file and never traps into the debugger. Path, sink and the gate live
// in `pf_umdf_util::log`, one copy for all four drivers.
pf_umdf_util::file_log!("pfmouse-driver.log", "PFMOUSE_DEBUG_LOG");

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log("[pf-mouse] DriverEntry");
    // SAFETY: `driver`/`registry_path` are the loader's DriverEntry arguments.
    unsafe { skeleton::driver_create(driver, registry_path, Some(evt_device_add)) }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    log("[pf-mouse] EvtDeviceAdd");

    // Mark as a filter (HID minidriver sits below mshidumdf.sys).
    // SAFETY: device_init is provided by the framework and non-null.
    unsafe { call_unsafe_wdf_function_binding!(WdfFdoInitSetFilter, device_init) };

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
        dbglog!("[pf-mouse] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let shm_idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(shm_idx);
    dbglog!("[pf-mouse] shm index = {shm_idx}");

    // Default parallel queue handling all IOCTLs.
    // SAFETY: `device` is the live device just created.
    if let Err(st) = unsafe { skeleton::create_default_queue(device, Some(evt_io_device_control)) }
    {
        dbglog!(
            "[pf-mouse] default WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
        return st;
    }

    // Manual queue: pended READ_REPORT requests wait here for a ring report.
    // SAFETY: `device` is the live device just created.
    let manual_queue = match unsafe { skeleton::create_manual_queue(device) } {
        Ok(q) => q,
        Err(st) => {
            dbglog!(
                "[pf-mouse] manual WdfIoQueueCreate failed 0x{:08x}",
                st as u32
            );
            return st;
        }
    };
    MANUAL_QUEUE.store(manual_queue, Ordering::SeqCst);

    // Periodic timer (parent = manual queue): sealed-channel pump, health marks, and a drain that
    // backs up the doorbell. 8 ms — the proven pf-gamepad cadence.
    // SAFETY: `manual_queue` is the live queue just created.
    let timer = unsafe { skeleton::create_periodic_timer(manual_queue.cast(), Some(evt_timer), 8) };
    if let Err(st) = timer {
        dbglog!("[pf-mouse] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }

    log("[pf-mouse] device ready (HID mouse + keyboard 5046:4D4F)");
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

    // Skip the READ_REPORT and doorbell cadence so the log stays readable.
    if !matches!(
        ioctl,
        IOCTL_HID_READ_REPORT | IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT
    ) {
        dbglog!("[pf-mouse] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
    }

    // READ_REPORT parks in the manual queue until a ring report answers it — this CONSUMES the
    // request token, so it's handled apart from the status-and-complete paths below.
    if ioctl == IOCTL_HID_READ_REPORT {
        let mq: WDFQUEUE = MANUAL_QUEUE.load(Ordering::SeqCst);
        // SAFETY: `mq` is the manual queue created in EvtDeviceAdd (a live WDFQUEUE of this device).
        match unsafe { request.forward_to_queue(mq) } {
            // A report may already wait in the ring: answer now, not at the next tick.
            Ok(()) => drain(mq),
            Err((req, st)) => req.complete(st), // forward failed → complete with the error
        }
        return;
    }

    let status: NTSTATUS = match ioctl {
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => request.copy_to_output(&HID_DESC),
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => request.copy_to_output(&hid_attrs()),
        IOCTL_HID_GET_REPORT_DESCRIPTOR => request.copy_to_output(&MOUSE_RDESC),
        IOCTL_UMDF_HID_GET_INPUT_REPORT => on_get_input_report(&request),
        // The doorbell (and any other write): drain the ring now. The bytes carry nothing.
        IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT => {
            drain(MANUAL_QUEUE.load(Ordering::SeqCst));
            let n = request
                .input_bytes(DOORBELL_REPORT_LEN)
                .map_or(0, |(_, n)| n);
            request.set_information(n as u64);
            STATUS_SUCCESS
        }
        IOCTL_UMDF_HID_SET_FEATURE => on_set_feature(&request),
        IOCTL_UMDF_HID_GET_FEATURE => on_get_feature(&request),
        // The channel proof (see `pf_umdf_util::hid`): the host asks THIS devnode which process
        // serves it, and duplicates the DATA section into the answer — so it never has to trust the
        // LocalService-writable bootstrap mailbox to name its target.
        IOCTL_HID_GET_STRING => on_get_string(&request),
        _ => STATUS_NOT_IMPLEMENTED,
    };

    dbglog!("[pf-mouse] ioctl 0x{ioctl:08x} -> 0x{:08x}", status as u32);
    request.complete(status);
}

/// GET_INPUT_REPORT for the id in input byte 0: the last absolute report, or a zeroed report
/// of the asked id.
fn on_get_input_report(request: &Request) -> NTSTATUS {
    let id = request
        .input_bytes(1)
        .ok()
        .and_then(|(b, _)| b.first().copied())
        .unwrap_or(MOUSE_REPORT_ID);
    if id == MOUSE_REPORT_ID {
        let report = LAST_ABS.lock().map(|g| *g).unwrap_or(NEUTRAL_ABS);
        return request.copy_to_output(&report);
    }
    let Some(len) = input_report_len(id) else {
        return STATUS_INVALID_PARAMETER;
    };
    let mut report = [0u8; MOUSE_RING_SLOT];
    report[0] = id;
    request.copy_to_output(&report[..len])
}

/// SET_FEATURE on the relative collection: Windows enabling the wheel/pan Resolution
/// Multipliers. The report arrives id-first; a bare value byte is taken too (it is never 2).
fn on_set_feature(request: &Request) -> NTSTATUS {
    let Ok((bytes, _)) = request.input_bytes(MOUSE_REL_FEATURE_LEN) else {
        return STATUS_INVALID_PARAMETER;
    };
    let value = match bytes[..] {
        [MOUSE_REL_REPORT_ID, v, ..] | [v] => v,
        _ => return STATUS_INVALID_PARAMETER,
    };
    dbglog!("[pf-mouse] resolution multipliers set: 0x{value:02x}");
    MULTIPLIERS.store(value, Ordering::Relaxed);
    STATUS_SUCCESS
}

/// GET_FEATURE for the id in input byte 0: only the relative collection's multiplier report.
fn on_get_feature(request: &Request) -> NTSTATUS {
    match request.input_bytes(1) {
        Ok((b, _)) if b.first() == Some(&MOUSE_REL_REPORT_ID) => {
            request.copy_to_output(&[MOUSE_REL_REPORT_ID, MULTIPLIERS.load(Ordering::Relaxed)])
        }
        _ => STATUS_INVALID_PARAMETER,
    }
}

// IOCTL_HID_GET_STRING: the input is a ULONG whose low word is the string id and whose high word
// is the language id. Windows polls ids 0x0E/0x0F/0x10 (manufacturer/product/serial) as well as
// the 0/1/2 HID_STRING_ID_* constants — serve both (the pf-gamepad finding).
fn on_get_string(request: &Request) -> NTSTATUS {
    let (bytes, _) = match request.input_bytes(4) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let (_, string_id) = string_request_id(&bytes);
    let s: String = match string_id {
        0 | 0x000E => "Punktfunk".into(),
        // (2) The SERIAL carries the channel proof — the one transport measured to reach a UMDF HID
        // minidriver from user mode (`HidD_GetSerialNumberString`, zero-access handle, verified on
        // .173). Safe HERE and only here: nothing reads the virtual mouse's serial, whereas the pads'
        // serials are what SDL and Steam dedup controllers on. The old value was the inert
        // "PFMOUSE00"; the proof text is just as inert and does the security work.
        2 | 0x0010 => ChannelProof::new(CHANNEL.index(), std::process::id()).to_hid_string(),
        _ => "Punktfunk Virtual Mouse".into(),
    };
    request.copy_utf16z_to_output(&s)
}

/// Wheel counts per notch for one Resolution Multiplier field.
fn counts_per_notch(bits: u8) -> u32 {
    if bits == 0 { 1 } else { WHEEL_MULTIPLIER }
}

extern "C" fn evt_timer(timer: WDFTIMER) {
    // One sealed-channel tick: publish our pid / adopt a delivery / detect host-gone (all safe,
    // via pf_umdf_util), then stamp the health marks the host watches.
    let Some(view) = CHANNEL.pump(&channel_cfg()) else {
        return; // host gone or not attached — nothing to deliver, nothing to mark
    };
    view.write_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION);
    let hb = view.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
    view.write_u32(OFF_DRIVER_HEARTBEAT, hb);
    if view.mapped_len() >= SHM_SIZE {
        let m = MULTIPLIERS.load(Ordering::Relaxed);
        view.write_u32(OFF_WHEEL_COUNTS, counts_per_notch(m & 0b11));
        view.write_u32(OFF_PAN_COUNTS, counts_per_notch((m >> 2) & 0b11));
        // Release: a host that sees the feature also sees the counts above.
        view.store_u32(OFF_FEATURES, MOUSE_FEATURE_RING, Ordering::Release);
    }
    // SAFETY: WdfTimerGetParentObject on the framework-provided live timer; its parent is the
    // manual queue (set in EvtDeviceAdd).
    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer) } as WDFQUEUE;
    drain(queue);
}

/// Hand ring reports to pended READ_REPORTs, in order, until one side runs out. A report waits
/// in the ring while no read is pended, so nothing is dropped while hidclass re-pends.
fn drain(queue: WDFQUEUE) {
    let Some(view) = CHANNEL.data() else {
        return;
    };
    if queue.is_null() || view.mapped_len() < SHM_SIZE {
        return; // a host that predates the ring never publishes into it
    }
    let Ok(_one_drain) = DRAIN.lock() else {
        return;
    };
    loop {
        // SeqCst pairs with the host's head store and tail load: a report published as this
        // drain stops is either seen here or makes the host ring the doorbell.
        let head = view.load_u32(OFF_RING_HEAD, Ordering::SeqCst);
        let tail = view.load_u32(OFF_RING_TAIL, Ordering::Relaxed);
        let pending = head.wrapping_sub(tail);
        if pending == 0 {
            return;
        }
        if pending as usize > MOUSE_RING_LEN {
            // The host broke its own bound: drop the backlog rather than replay stale slots.
            view.store_u32(OFF_RING_TAIL, head, Ordering::Release);
            return;
        }
        let mut slot = [0u8; MOUSE_RING_SLOT];
        view.read_bytes(ring_slot_off(tail), &mut slot);
        let Some(len) = input_report_len(slot[0]) else {
            view.store_u32(OFF_RING_TAIL, tail.wrapping_add(1), Ordering::Release);
            continue; // malformed slot: skip it
        };
        // SAFETY: `queue` is this device's live manual queue — the contract
        // `retrieve_next_request` needs.
        let Some(request) = (unsafe { wdf::retrieve_next_request(queue) }) else {
            return; // no read pended: the report waits for the next one
        };
        if slot[0] == MOUSE_REPORT_ID
            && let Ok(mut g) = LAST_ABS.lock()
        {
            g.copy_from_slice(&slot[..MOUSE_REPORT_LEN]);
        }
        let st = request.copy_to_output(&slot[..len]);
        request.complete(st);
        // The host may reuse the slot only after it is read.
        view.store_u32(OFF_RING_TAIL, tail.wrapping_add(1), Ordering::SeqCst);
    }
}
