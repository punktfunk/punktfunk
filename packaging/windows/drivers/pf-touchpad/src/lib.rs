//! punktfunk virtual Precision Touchpad: a UMDF2 HID minidriver, identity PF:TP.
//!
//! The host presents a client's touchpad contacts here, one devnode per session, and Windows
//! runs pointer motion, taps, scrolling and gestures with the user's own touchpad settings.
//! Layout, descriptor and features: `pf_driver_proto::touchpad`.
//!
//! The host channel is the sealed pad channel (design/gamepad-channel-sealing.md) through
//! `pf_umdf_util`, so the only `unsafe` here is WDF setup FFI. Delivery is event-driven like
//! `pf-mouse`: reports wait in the shared ring until a READ_REPORT takes them, drained when a
//! read arrives, when the host rings the doorbell, and on each timer tick. An idle pad sends
//! nothing.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]

use core::sync::atomic::{AtomicPtr, AtomicU8, Ordering};
use std::sync::Mutex;

use pf_driver_proto::gamepad::ChannelProof;
use pf_driver_proto::touchpad::{
    CAPS_FEATURE, CAPS_REPORT_ID, DOORBELL_REPORT_LEN, HQA_REPORT_ID, INPUT_MODE_REPORT_ID,
    SWITCH_REPORT_ID, TOUCHPAD_PID, TOUCHPAD_RDESC, TOUCHPAD_RDESC_LEN, TOUCHPAD_REPORT_ID,
    TOUCHPAD_REPORT_LEN, TOUCHPAD_RING_LEN, TOUCHPAD_RING_SLOT, TOUCHPAD_VER, TOUCHPAD_VID,
    TouchpadShm, hqa_feature, ring_slot_off,
};
use pf_umdf_util::channel::{ChannelClient, ChannelConfig};
use pf_umdf_util::hid::{
    IOCTL_HID_GET_DEVICE_ATTRIBUTES, IOCTL_HID_GET_DEVICE_DESCRIPTOR,
    IOCTL_HID_GET_REPORT_DESCRIPTOR, IOCTL_HID_GET_STRING, IOCTL_HID_READ_REPORT,
    IOCTL_HID_WRITE_REPORT, IOCTL_UMDF_HID_GET_FEATURE, IOCTL_UMDF_HID_GET_INPUT_REPORT,
    IOCTL_UMDF_HID_SET_FEATURE, IOCTL_UMDF_HID_SET_OUTPUT_REPORT, declared_len, string_request_id,
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
// {reportType=0x22, wReportLength}, computed from TOUCHPAD_RDESC so the two never drift.
static HID_DESC: [u8; 9] = {
    let [lo, hi] = (TOUCHPAD_RDESC_LEN as u16).to_le_bytes();
    [0x09, 0x21, 0x00, 0x01, 0x00, 0x01, 0x22, lo, hi]
};
const _: () = assert!(declared_len(&HID_DESC) == TOUCHPAD_RDESC_LEN);

// HID_DEVICE_ATTRIBUTES (32 bytes): Size(u32)=32, VendorID, ProductID, VersionNumber, Reserved[11].
fn hid_attrs() -> [u8; 32] {
    let mut a = [0u8; 32];
    a[0..4].copy_from_slice(&32u32.to_le_bytes());
    a[4..6].copy_from_slice(&TOUCHPAD_VID.to_le_bytes());
    a[6..8].copy_from_slice(&TOUCHPAD_PID.to_le_bytes());
    a[8..10].copy_from_slice(&TOUCHPAD_VER.to_le_bytes());
    a
}

static MANUAL_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// Input Mode as Windows last set it; 0 until it does.
static INPUT_MODE: AtomicU8 = AtomicU8::new(0);
/// Surface and button switches as Windows last set them; both on until it does.
static SWITCHES: AtomicU8 = AtomicU8::new(0b11);
/// One drain at a time: READ_REPORT, the doorbell and the timer all drain.
static DRAIN: Mutex<()> = Mutex::new(());

const SHM_SIZE: usize = core::mem::size_of::<TouchpadShm>();
const GAMEPAD_PROTO_VERSION: u32 = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION;
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(TouchpadShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(TouchpadShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(TouchpadShm, pad_index);
const OFF_RING_HEAD: usize = core::mem::offset_of!(TouchpadShm, ring_head);
const OFF_RING_TAIL: usize = core::mem::offset_of!(TouchpadShm, ring_tail);
const OFF_INPUT_MODE: usize = core::mem::offset_of!(TouchpadShm, input_mode);

/// Per device: `ProcessSharingDisabled` gives each touchpad its own WUDFHost.
static CHANNEL: ChannelClient = ChannelClient::new();

fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-touchpad",
        boot_name_prefix: "Global\\pftouchpad-boot-",
        data_magic: pf_driver_proto::touchpad::TOUCHPAD_MAGIC,
        data_size: SHM_SIZE,
        min_data_size: SHM_SIZE,
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
}

// The opt-in bring-up file log: debug builds, or `PFTOUCHPAD_DEBUG_LOG` set.
pf_umdf_util::file_log!("pftouchpad-driver.log", "PFTOUCHPAD_DEBUG_LOG");

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log("[pf-touchpad] DriverEntry");
    // SAFETY: `driver`/`registry_path` are the loader's DriverEntry arguments.
    unsafe { skeleton::driver_create(driver, registry_path, Some(evt_device_add)) }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    log("[pf-touchpad] EvtDeviceAdd");

    // A HID minidriver sits below mshidumdf.sys as a filter.
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
        dbglog!("[pf-touchpad] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // SAFETY: `device` is the live device just created — the exact contract this fn requires.
    let shm_idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(shm_idx);
    dbglog!("[pf-touchpad] shm index = {shm_idx}");

    // SAFETY: `device` is the live device just created.
    if let Err(st) = unsafe { skeleton::create_default_queue(device, Some(evt_io_device_control)) }
    {
        dbglog!(
            "[pf-touchpad] default WdfIoQueueCreate failed 0x{:08x}",
            st as u32
        );
        return st;
    }

    // Pended READ_REPORT requests wait here for a ring report.
    // SAFETY: `device` is the live device just created.
    let manual_queue = match unsafe { skeleton::create_manual_queue(device) } {
        Ok(q) => q,
        Err(st) => {
            dbglog!(
                "[pf-touchpad] manual WdfIoQueueCreate failed 0x{:08x}",
                st as u32
            );
            return st;
        }
    };
    MANUAL_QUEUE.store(manual_queue, Ordering::SeqCst);

    // 8 ms: the sealed-channel pump, the health marks, and a drain behind the doorbell.
    // SAFETY: `manual_queue` is the live queue just created.
    let timer = unsafe { skeleton::create_periodic_timer(manual_queue.cast(), Some(evt_timer), 8) };
    if let Err(st) = timer {
        dbglog!("[pf-touchpad] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }

    log("[pf-touchpad] device ready (Precision Touchpad 5046:5450)");
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

    if !matches!(
        ioctl,
        IOCTL_HID_READ_REPORT | IOCTL_HID_WRITE_REPORT | IOCTL_UMDF_HID_SET_OUTPUT_REPORT
    ) {
        dbglog!("[pf-touchpad] ioctl 0x{ioctl:08x} out={_output_len} in={_input_len}");
    }

    // READ_REPORT parks in the manual queue until a ring report answers it; this consumes the
    // request token.
    if ioctl == IOCTL_HID_READ_REPORT {
        let mq: WDFQUEUE = MANUAL_QUEUE.load(Ordering::SeqCst);
        // SAFETY: `mq` is the manual queue created in EvtDeviceAdd (a live WDFQUEUE of this device).
        match unsafe { request.forward_to_queue(mq) } {
            Ok(()) => drain(mq),
            Err((req, st)) => req.complete(st),
        }
        return;
    }

    let status: NTSTATUS = match ioctl {
        IOCTL_HID_GET_DEVICE_DESCRIPTOR => request.copy_to_output(&HID_DESC),
        IOCTL_HID_GET_DEVICE_ATTRIBUTES => request.copy_to_output(&hid_attrs()),
        IOCTL_HID_GET_REPORT_DESCRIPTOR => request.copy_to_output(&TOUCHPAD_RDESC),
        // No contact replays: a query reads an empty pad.
        IOCTL_UMDF_HID_GET_INPUT_REPORT => {
            let mut report = [0u8; TOUCHPAD_REPORT_LEN];
            report[0] = TOUCHPAD_REPORT_ID;
            request.copy_to_output(&report)
        }
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
        // The channel proof is the serial string, as on pf-mouse (`pf_umdf_util::hid`).
        IOCTL_HID_GET_STRING => on_get_string(&request),
        _ => STATUS_NOT_IMPLEMENTED,
    };

    dbglog!(
        "[pf-touchpad] ioctl 0x{ioctl:08x} -> 0x{:08x}",
        status as u32
    );
    request.complete(status);
}

/// SET_FEATURE, id first: Windows setting the Input Mode or the surface and button switches.
fn on_set_feature(request: &Request) -> NTSTATUS {
    let Ok((bytes, _)) = request.input_bytes(2) else {
        return STATUS_INVALID_PARAMETER;
    };
    let cell = match bytes[..] {
        [INPUT_MODE_REPORT_ID, _] => &INPUT_MODE,
        [SWITCH_REPORT_ID, _] => &SWITCHES,
        _ => return STATUS_INVALID_PARAMETER,
    };
    dbglog!(
        "[pf-touchpad] feature 0x{:02x} set to {}",
        bytes[0],
        bytes[1]
    );
    cell.store(bytes[1], Ordering::Relaxed);
    STATUS_SUCCESS
}

/// GET_FEATURE for the id in input byte 0.
fn on_get_feature(request: &Request) -> NTSTATUS {
    let id = match request.input_bytes(1) {
        Ok((b, _)) => b.first().copied(),
        Err(st) => return st,
    };
    match id {
        Some(CAPS_REPORT_ID) => request.copy_to_output(&CAPS_FEATURE),
        Some(HQA_REPORT_ID) => request.copy_to_output(&hqa_feature()),
        Some(INPUT_MODE_REPORT_ID) => {
            request.copy_to_output(&[INPUT_MODE_REPORT_ID, INPUT_MODE.load(Ordering::Relaxed)])
        }
        Some(SWITCH_REPORT_ID) => {
            request.copy_to_output(&[SWITCH_REPORT_ID, SWITCHES.load(Ordering::Relaxed)])
        }
        _ => STATUS_INVALID_PARAMETER,
    }
}

// IOCTL_HID_GET_STRING: the low word of the input ULONG names the string. Windows asks for
// 0x0E/0x0F/0x10 as well as the 0/1/2 HID_STRING_ID_* constants, so both are served.
fn on_get_string(request: &Request) -> NTSTATUS {
    let (bytes, _) = match request.input_bytes(4) {
        Ok(v) => v,
        Err(st) => return st,
    };
    let (_, string_id) = string_request_id(&bytes);
    let s: String = match string_id {
        0 | 0x000E => "Punktfunk".into(),
        // Nothing reads a touchpad's serial, so it carries the channel proof.
        2 | 0x0010 => ChannelProof::new(CHANNEL.index(), std::process::id()).to_hid_string(),
        _ => "Punktfunk Virtual Touchpad".into(),
    };
    request.copy_utf16z_to_output(&s)
}

extern "C" fn evt_timer(timer: WDFTIMER) {
    // One sealed-channel tick, then the health marks the host watches.
    let Some(view) = CHANNEL.pump(&channel_cfg()) else {
        return;
    };
    view.write_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION);
    let hb = view.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
    view.write_u32(OFF_DRIVER_HEARTBEAT, hb);
    view.write_u32(
        OFF_INPUT_MODE,
        u32::from(INPUT_MODE.load(Ordering::Relaxed)),
    );
    // SAFETY: WdfTimerGetParentObject on the framework-provided live timer; its parent is the
    // manual queue (set in EvtDeviceAdd).
    let queue =
        unsafe { call_unsafe_wdf_function_binding!(WdfTimerGetParentObject, timer) } as WDFQUEUE;
    drain(queue);
}

/// Hand ring reports to pended READ_REPORTs, in order, until one side runs out. A report waits
/// in the ring while no read is pended, so nothing drops while hidclass re-pends.
fn drain(queue: WDFQUEUE) {
    let Some(view) = CHANNEL.data() else {
        return;
    };
    if queue.is_null() {
        return;
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
        if pending as usize > TOUCHPAD_RING_LEN {
            // The host broke its own bound: drop the backlog rather than replay stale slots.
            view.store_u32(OFF_RING_TAIL, head, Ordering::Release);
            return;
        }
        let mut slot = [0u8; TOUCHPAD_RING_SLOT];
        view.read_bytes(ring_slot_off(tail), &mut slot);
        if slot[0] != TOUCHPAD_REPORT_ID {
            view.store_u32(OFF_RING_TAIL, tail.wrapping_add(1), Ordering::Release);
            continue;
        }
        // SAFETY: `queue` is this device's live manual queue — the contract
        // `retrieve_next_request` needs.
        let Some(request) = (unsafe { wdf::retrieve_next_request(queue) }) else {
            return;
        };
        let st = request.copy_to_output(&slot[..TOUCHPAD_REPORT_LEN]);
        request.complete(st);
        // The host may reuse the slot only after it is read.
        view.store_u32(OFF_RING_TAIL, tail.wrapping_add(1), Ordering::SeqCst);
    }
}
