// punktfunk virtual Xbox 360 XUSB companion — UMDF2 driver presenting the XUSB device interface so
// classic XInput (XInputGetState) reads the pad with no kernel bus driver (the HIDMaestro approach).
//
// xinput1_4.dll enumerates GUID_DEVINTERFACE_XUSB, opens the Nth instance (= player slot), and polls
// it with buffered IOCTLs. We register the interface and answer those IOCTLs from controller state the
// host publishes into a shared DATA section; a game's rumble (SET_STATE) is published back for the
// host to forward. Byte formats are the source-verified xusb22 wire layout (HIDMaestro
// driver/companion.c + nefarius/XInputHooker XUSB.h + ViGEm XUSB_REPORT).
//
// The host channel is the **sealed pad channel** (design/gamepad-channel-sealing.md, proto v2): the
// DATA section (`pf_driver_proto::gamepad::XusbShm`) is UNNAMED — we reach it only through a handle
// the SYSTEM host duplicated into this WUDFHost, bootstrapped over the named `Global\pfxusb-boot-<i>`
// mailbox. The whole handshake + all shared-memory access lives in `pf_umdf_util` (audited unsafe
// layer): this crate's channel/IOCTL/state logic is 100% SAFE Rust. The only `unsafe` here is the
// unavoidable WDF setup FFI in DriverEntry/EvtDeviceAdd, each with a `// SAFETY:` proof.
//
// We answer the WAIT_* IOCTLs with STATUS_INVALID_DEVICE_REQUEST, which makes xinput1_4 fall back to
// synchronous GET_STATE polling — so no manual queue is needed for classic XInput. A periodic WDF
// timer (the pf-gamepad pattern) owns the sealed-channel pump: adoption, re-delivery and host-gone
// detection all happen there, so the IOCTL path reads the CACHED view instead of re-opening the
// bootstrap mailbox (open+map+close+unmap + 2 allocs) on EVERY XInput poll.

#![allow(non_snake_case, non_upper_case_globals, clippy::missing_safety_doc)]
// Every remaining `unsafe {}` (all WDF setup FFI) must carry a `// SAFETY:` proof.

use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};
use pf_driver_proto::gamepad::XusbShm;
use pf_umdf_util::channel::{ChannelClient, ChannelConfig};
use pf_umdf_util::section::MappedView;
use pf_umdf_util::skeleton::{self, STATUS_INVALID_DEVICE_REQUEST, STATUS_SUCCESS};
use pf_umdf_util::wdf::{self, Request};
use pf_umdf_util::{dbglog, nt_success};
use wdk_sys::{
    GUID, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, PWDFDEVICE_INIT, ULONG,
    WDF_NO_OBJECT_ATTRIBUTES, WDFDEVICE, WDFDRIVER, WDFQUEUE, WDFQUEUE__, WDFREQUEST, WDFTIMER,
    call_unsafe_wdf_function_binding,
};

// GUID_DEVINTERFACE_XUSB {EC87F1E3-C13B-4100-B5F7-8B84D54260CB} — what xinput1_4 enumerates + opens.
const GUID_DEVINTERFACE_XUSB: GUID = GUID {
    Data1: 0xEC87_F1E3,
    Data2: 0xC13B,
    Data3: 0x4100,
    Data4: [0xB5, 0xF7, 0x8B, 0x84, 0xD5, 0x42, 0x60, 0xCB],
};

// ---- XUSB IOCTLs (METHOD_BUFFERED) ----
const IOCTL_XUSB_GET_INFORMATION: u32 = 0x8000_6000;
const IOCTL_XUSB_GET_CAPABILITIES: u32 = 0x8000_E004;
const IOCTL_XUSB_GET_LED_STATE: u32 = 0x8000_E008;
const IOCTL_XUSB_GET_STATE: u32 = 0x8000_E00C;
const IOCTL_XUSB_SET_STATE: u32 = 0x8000_A010;
const IOCTL_XUSB_WAIT_GUIDE_BUTTON: u32 = 0x8000_E014;
const IOCTL_XUSB_GET_BATTERY_INFORMATION: u32 = 0x8000_E018;
const IOCTL_XUSB_POWER_DOWN: u32 = 0x8000_A01C;
const IOCTL_XUSB_GET_XINPUT_MANAGEMENT_DRIVER: u32 = 0x8000_6380;
const IOCTL_XUSB_WAIT_FOR_INPUT: u32 = 0x8000_E3AC;
const IOCTL_XUSB_GET_INFORMATION_EX: u32 = 0x8000_E3FC;

/// Our own private IOCTL (NOT part of xusb22): answer the host's **channel proof** — which process
/// is serving this devnode — so the host learns its duplication target from the device stack instead
/// of from the LocalService-writable bootstrap mailbox (see `pf_driver_proto::gamepad::ChannelProof`
/// for why that distinction is the whole security property). Any local caller may ask; the answer is
/// a pid and two version numbers, none of them secret, and it is only worth anything to a process
/// that can already duplicate handles into us.
const IOCTL_PF_GET_CHANNEL_PROOF: u32 = pf_driver_proto::gamepad::IOCTL_PF_GET_CHANNEL_PROOF;

// Xbox 360 wired identity (what GET_INFORMATION reports). 0x0103 unblocks SET_STATE (vibration).
const XUSB_VID: u16 = 0x045E;
const XUSB_PID: u16 = 0x028E;
const XUSB_VERSION: u16 = 0x0103;

/// Manual queue holding pended [`IOCTL_XUSB_WAIT_FOR_INPUT`] requests; the periodic timer completes
/// them when the host publishes a new packet. See [`evt_timer`].
static WAIT_QUEUE: AtomicPtr<WDFQUEUE__> = AtomicPtr::new(core::ptr::null_mut());
/// The `dwPacketNumber` the last completed wait reported — the edge the timer compares against, so
/// a waiter is only released when the state actually MOVED (that is the contract of an async wait;
/// completing it unconditionally would spin the caller at timer rate).
static WAIT_LAST_PACKET: AtomicU32 = AtomicU32::new(0);
// ---- the sealed host channel: layouts + offsets from pf_driver_proto (drift = compile error) ----
const SHM_MAGIC: u32 = pf_driver_proto::gamepad::XUSB_MAGIC; // "PFXU"
const SHM_SIZE: usize = core::mem::size_of::<XusbShm>();
const GAMEPAD_PROTO_VERSION: u32 = pf_driver_proto::gamepad::GAMEPAD_PROTO_VERSION;

// XusbShm field offsets (host writes state, we answer XInput; we write rumble + health marks).
const OFF_PACKET: usize = core::mem::offset_of!(XusbShm, packet);
const OFF_BUTTONS: usize = core::mem::offset_of!(XusbShm, buttons);
const OFF_LT: usize = core::mem::offset_of!(XusbShm, left_trigger);
const OFF_RT: usize = core::mem::offset_of!(XusbShm, right_trigger);
const OFF_LX: usize = core::mem::offset_of!(XusbShm, thumb_lx);
const OFF_LY: usize = core::mem::offset_of!(XusbShm, thumb_ly);
const OFF_RX: usize = core::mem::offset_of!(XusbShm, thumb_rx);
const OFF_RY: usize = core::mem::offset_of!(XusbShm, thumb_ry);
const OFF_RUMBLE_SEQ: usize = core::mem::offset_of!(XusbShm, rumble_seq);
const OFF_RUMBLE_LARGE: usize = core::mem::offset_of!(XusbShm, rumble_large);
const OFF_RUMBLE_SMALL: usize = core::mem::offset_of!(XusbShm, rumble_small);
const OFF_DRIVER_PROTO: usize = core::mem::offset_of!(XusbShm, driver_proto);
const OFF_DRIVER_HEARTBEAT: usize = core::mem::offset_of!(XusbShm, driver_heartbeat);
const OFF_PAD_INDEX: usize = core::mem::offset_of!(XusbShm, pad_index);

/// The sealed-channel client (per-pad: `ProcessSharingDisabled` gives each pad its own WUDFHost, so
/// this static is per-pad). All shared-memory access + the bootstrap handshake live in `pf_umdf_util`.
static CHANNEL: ChannelClient = ChannelClient::new();

/// Host liveness as observed by the timer's last pump: the mailbox-name existence IS the signal
/// (`pf_umdf_util::channel`). The IOCTL path consults this instead of pumping, so a vanished host
/// still reads as a neutral pad within one timer tick (≤8 ms) — the detach semantics the per-IOCTL
/// pump used to provide instantly.
static HOST_LIVE: AtomicBool = AtomicBool::new(false);

/// This pad's channel config (magic/size/pad_index offset + our logger).
fn channel_cfg() -> ChannelConfig {
    ChannelConfig {
        tag: "pf-xusb",
        boot_name_prefix: "Global\\pfxusb-boot-",
        data_magic: SHM_MAGIC,
        data_size: SHM_SIZE,
        min_data_size: SHM_SIZE, // layout never grew — no fallback size
        pad_index_off: OFF_PAD_INDEX,
        log,
    }
}

// The bring-up file log. OPT-IN — debug builds, or the `PFXUSB_DEBUG_LOG` env var — so a RELEASE
// driver never writes the file and never traps into the debugger. Path, sink and the gate live
// in `pf_umdf_util::log`, one copy for all four drivers.
pf_umdf_util::file_log!("pfxusb-driver.log", "PFXUSB_DEBUG_LOG");

#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    log("[pf-xusb] DriverEntry");
    // SAFETY: `driver`/`registry_path` are the loader's DriverEntry arguments.
    unsafe { skeleton::driver_create(driver, registry_path, Some(evt_device_add)) }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, mut device_init: PWDFDEVICE_INIT) -> NTSTATUS {
    log("[pf-xusb] EvtDeviceAdd");

    let mut device: WDFDEVICE = core::ptr::null_mut();
    // SAFETY: `device_init` is the framework-provided init; attributes null; `device` receives it.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device
        )
    };
    if !nt_success(st) {
        dbglog!("[pf-xusb] WdfDeviceCreate failed 0x{:08x}", st as u32);
        return st;
    }

    // SAFETY: `device` is the live device just created — the exact contract `query_location_index`
    // requires.
    let idx = unsafe { wdf::query_location_index(device) };
    CHANNEL.set_index(idx);
    dbglog!("[pf-xusb] shm index = {idx}");

    // Register the XUSB device interface (no reference string) — what xinput1_4 enumerates + opens.
    // SAFETY: `device` is live; the GUID is a static; null reference string.
    let st = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &GUID_DEVINTERFACE_XUSB,
            core::ptr::null()
        )
    };
    if !nt_success(st) {
        dbglog!(
            "[pf-xusb] WdfDeviceCreateDeviceInterface failed 0x{:08x}",
            st as u32
        );
        return st;
    }

    // Default parallel queue: all the XUSB IOCTLs land here.
    // SAFETY: `device` is the live device just created.
    let queue = match unsafe { skeleton::create_default_queue(device, Some(evt_io_device_control)) }
    {
        Ok(q) => q,
        Err(st) => {
            dbglog!("[pf-xusb] WdfIoQueueCreate failed 0x{:08x}", st as u32);
            return st;
        }
    };

    // Manual queue for the ASYNC input wait (`IOCTL_XUSB_WAIT_FOR_INPUT`), completed by the timer.
    // Classic XInput falls back to GET_STATE polling when it is declined; WGI/GameInput poll
    // asynchronously and never admit a device that declines it.
    // SAFETY: `device` is the live device just created.
    let wait_queue = match unsafe { skeleton::create_manual_queue(device) } {
        Ok(q) => q,
        Err(st) => {
            dbglog!("[pf-xusb] wait WdfIoQueueCreate failed 0x{:08x}", st as u32);
            return st;
        }
    };
    WAIT_QUEUE.store(wait_queue, Ordering::SeqCst);

    // Periodic sealed-channel tick, parented to the default queue. It is the only mailbox pump
    // (adoption, re-delivery, host-gone), so the XInput IOCTL path never re-opens the mailbox and
    // nothing outlives the device to stamp a later session's mailbox.
    // SAFETY: `queue` is the live default queue just created.
    let timer = unsafe { skeleton::create_periodic_timer(queue.cast(), Some(evt_timer), 8) };
    if let Err(st) = timer {
        dbglog!("[pf-xusb] WdfTimerCreate failed 0x{:08x}", st as u32);
        return st;
    }

    log("[pf-xusb] device ready (XUSB interface registered)");
    STATUS_SUCCESS
}

/// One sealed-channel tick: pump the bootstrap mailbox (adopt a delivery / detect host-gone) and
/// refresh [`HOST_LIVE`]. This is the ONLY steady-state pump — the IOCTL path reads the cached
/// view. All safe; the channel state machine lives in pf_umdf_util.
extern "C" fn evt_timer(_timer: WDFTIMER) {
    let live = CHANNEL.pump(&channel_cfg()).is_some();
    HOST_LIVE.store(live, Ordering::Relaxed);

    // On a new `dwPacketNumber`, release every pended `WAIT_FOR_INPUT`: each client (WGI,
    // GameInput, Steam) parks its own, and releasing one per packet starves the rest. An unchanged
    // packet releases none, or the waiters would spin at timer rate.
    let data = CHANNEL.data();
    let (packet, ..) = read_state(data);
    if packet == WAIT_LAST_PACKET.load(Ordering::Relaxed) {
        return;
    }
    let wq: WDFQUEUE = WAIT_QUEUE.load(Ordering::SeqCst);
    if wq.is_null() {
        return;
    }
    let state = build_wait_state(data);
    // SAFETY: `wq` is the live manual queue created in EvtDeviceAdd — the contract
    // `retrieve_next_request` requires. `None` simply means nobody is waiting.
    while let Some(request) = unsafe { wdf::retrieve_next_request(wq) } {
        WAIT_LAST_PACKET.store(packet, Ordering::Relaxed);
        let st = request.copy_to_output(&state);
        request.complete(st);
    }
}

/// The `GET_STATE` bytes plus the two WGI's XUSB parser gates on: `[2] = 3` (resumed; until one
/// arrives WGI drops vibration) and `[10] = 0x14` (payload marker; zero skips the reading).
/// Layout from HIDMaestro's decomp of `XusbDevice::ProcessInput`.
fn build_wait_state(data: Option<&MappedView>) -> [u8; 29] {
    let mut s = build_get_state(data);
    s[2] = 0x03;
    s[10] = 0x14;
    s
}

/// The current controller state from the attached DATA section (zeros / neutral when unattached).
/// Returns `(dwPacketNumber, wButtons, lt, rt, lx, ly, rx, ry)`.
fn read_state(data: Option<&MappedView>) -> (u32, u16, u8, u8, i16, i16, i16, i16) {
    match data {
        Some(v) => (
            v.read_u32(OFF_PACKET),
            v.read_u16(OFF_BUTTONS),
            v.read_u8(OFF_LT),
            v.read_u8(OFF_RT),
            v.read_i16(OFF_LX),
            v.read_i16(OFF_LY),
            v.read_i16(OFF_RX),
            v.read_i16(OFF_RY),
        ),
        None => (0, 0, 0, 0, 0, 0, 0, 0),
    }
}

/// Stamp the driver health marks the host watches: `driver_proto` (the attach signal, idempotent)
/// and `driver_heartbeat` (+1). Called once the channel attaches and on every serviced IOCTL, so the
/// host can tell "driver bound and alive" apart from "driver package missing/failed to bind" and see
/// the game-visible polling path advance.
fn touch_driver_marks(data: &MappedView) {
    let _marks = SECTION_PUBLISH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    data.write_u32(OFF_DRIVER_PROTO, GAMEPAD_PROTO_VERSION);
    let hb = data.read_u32(OFF_DRIVER_HEARTBEAT).wrapping_add(1);
    data.write_u32(OFF_DRIVER_HEARTBEAT, hb);
}

/// Publish a game's rumble (from SET_STATE) into the DATA section for the host to forward.
///
/// Serialized and Release-published, because IOCTLs arrive concurrently and neither property held
/// before. `seq` was a read-modify-write across the two motor bytes: two `SET_STATE` calls could
/// both read the same value and both write back `seq + 1`, so the host — which treats an unchanged
/// seq as "nothing new" — saw one bump for two writes and skipped a level entirely. A skipped
/// **stop** is the one that hurts: the pad keeps buzzing until the host's ~2.5 s idle force-off
/// notices the game went quiet, which is where the bound on this bug comes from.
///
/// The seq store is Release for the same reason as `pf-gamepad`'s `out_seq`: the host loads it with
/// Acquire and documents that as ordering its read of the motor bytes ("the driver bumps
/// `rumble_seq` AFTER writing the rumble bytes", `gamepad_windows.rs`). A plain write gives that
/// Acquire nothing to pair with, so the guarantee the host's comment claims did not exist in either
/// direction — the host could read a fresh seq against stale motor levels on a weakly-ordered core.
fn publish_rumble(data: Option<&MappedView>, large: u8, small: u8) {
    let Some(v) = data else { return };
    let _publish = SECTION_PUBLISH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    v.write_u8(OFF_RUMBLE_LARGE, large);
    v.write_u8(OFF_RUMBLE_SMALL, small);
    let seq = v.read_u32(OFF_RUMBLE_SEQ).wrapping_add(1);
    v.store_u32(OFF_RUMBLE_SEQ, seq, Ordering::Release);
}

/// Serializes the section's read-modify-write publishes ([`publish_rumble`], [`touch_driver_marks`])
/// against each other. One lock rather than one per field: they are all short byte writes into the
/// same mapped view, and the contention is nil compared to the IOCTL round trip that reaches them.
///
/// Poison-tolerant deliberately — poison is sticky, so bailing out on it would silently stop
/// forwarding rumble for the rest of the process. The protected state is bytes in a shared section,
/// not an invariant a panic elsewhere could have violated.
static SECTION_PUBLISH: std::sync::Mutex<()> = std::sync::Mutex::new(());

// Build the 29-byte GET_STATE buffer (the layout xinput1_4 parses).
fn build_get_state(data: Option<&MappedView>) -> [u8; 29] {
    let (packet, buttons, lt, rt, lx, ly, rx, ry) = read_state(data);
    let mut s = [0u8; 29];
    s[0..2].copy_from_slice(&XUSB_VERSION.to_le_bytes());
    s[2] = 0x01; // device count
    s[5..9].copy_from_slice(&packet.to_le_bytes());
    s[0x0B..0x0D].copy_from_slice(&buttons.to_le_bytes());
    s[0x0D] = lt;
    s[0x0E] = rt;
    s[0x0F..0x11].copy_from_slice(&lx.to_le_bytes());
    s[0x11..0x13].copy_from_slice(&ly.to_le_bytes());
    s[0x13..0x15].copy_from_slice(&rx.to_le_bytes());
    s[0x15..0x17].copy_from_slice(&ry.to_le_bytes());
    s
}

// GET_INFORMATION: 12 bytes — version, device count, VID/PID. Marks the slot connected.
fn build_information() -> [u8; 12] {
    let mut info = [0u8; 12];
    info[0..2].copy_from_slice(&XUSB_VERSION.to_le_bytes());
    info[2] = 0x01; // one device/port
    info[8..10].copy_from_slice(&XUSB_VID.to_le_bytes());
    info[10..12].copy_from_slice(&XUSB_PID.to_le_bytes());
    info
}

// GET_CAPABILITIES V1 (24 bytes): Type=0x03 SubType=0x01 (gamepad), button/stick masks, motor max
// = 0xFFFF (advertise rumble). The V2 (36-byte) form prepends a 16-byte header when WGI asks for 36.
#[rustfmt::skip]
const CAPS_V1: [u8; 24] = [
    0x03, 0x01, 0x00, 0x01, 0xFF, 0xF7, 0xFF, 0xFF,
    0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF, 0xC0, 0xFF,
    0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF,
];

fn build_caps_v2() -> [u8; 36] {
    let mut c = [0u8; 36];
    c[0..6].copy_from_slice(&[0x03, 0x01, 0x01, 0x01, 0x0C, 0x00]);
    c[6..8].copy_from_slice(&XUSB_VID.to_le_bytes());
    c[8..10].copy_from_slice(&XUSB_PID.to_le_bytes());
    c[10..16].copy_from_slice(&[0x10, 0x01, 0x00, 0xFA, 0x34, 0x22]);
    c[16..36].copy_from_slice(&CAPS_V1[4..24]); // the XINPUT_CAPABILITIES struct body
    c
}

extern "C" fn evt_io_device_control(
    _queue: WDFQUEUE,
    request: WDFREQUEST,
    output_len: usize,
    input_len: usize,
    ioctl: ULONG,
) {
    // SAFETY: `request` is the live request for THIS EvtIoDeviceControl invocation — exactly the
    // contract `Request::new` requires. From here everything is safe (the token owns completion).
    let request = unsafe { Request::new(request) };

    // The CACHED data view, gated on the timer's host-liveness read. This path used to call
    // `CHANNEL.pump()` — a mailbox open+map+close+unmap plus two heap allocs — on EVERY XInput
    // poll, per pad; the periodic timer owns the pump now (adoption, re-delivery, host-gone), and
    // a vanished host reads as a neutral pad within one 8 ms tick. The heartbeat mark still
    // advances per serviced IOCTL, so the host keeps seeing the GAME-visible polling path move,
    // not the timer.
    let data = if HOST_LIVE.load(Ordering::Relaxed) {
        CHANNEL.data()
    } else {
        None
    };
    if let Some(v) = data {
        touch_driver_marks(v);
    }

    let status: NTSTATUS = match ioctl {
        IOCTL_XUSB_GET_INFORMATION => request.copy_to_output(&build_information()),
        IOCTL_XUSB_GET_INFORMATION_EX => {
            let mut ex = [0u8; 64];
            ex[0..2].copy_from_slice(&XUSB_VERSION.to_le_bytes());
            ex[2] = 0x01;
            ex[3] = 0x01;
            ex[8..10].copy_from_slice(&XUSB_VID.to_le_bytes());
            ex[10..12].copy_from_slice(&XUSB_PID.to_le_bytes());
            let n = output_len.min(64);
            request.copy_to_output(&ex[..n])
        }
        IOCTL_XUSB_GET_CAPABILITIES => {
            if output_len >= 36 {
                request.copy_to_output(&build_caps_v2())
            } else {
                request.copy_to_output(&CAPS_V1)
            }
        }
        IOCTL_XUSB_GET_STATE => request.copy_to_output(&build_get_state(data)),
        // The channel proof — deliberately answered from THIS process's own identity, never from
        // anything a caller supplied, so the only way to make this devnode name a process is to BE
        // the driver bound to it.
        IOCTL_PF_GET_CHANNEL_PROOF => {
            let proof =
                pf_driver_proto::gamepad::ChannelProof::new(CHANNEL.index(), std::process::id());
            request.copy_to_output(&proof.to_bytes())
        }
        IOCTL_XUSB_GET_LED_STATE => request.copy_to_output(&[0x00, 0x00, 0x06]),
        IOCTL_XUSB_GET_BATTERY_INFORMATION => request.copy_to_output(&[0x00, 0x01, 0x03, 0x00]),
        IOCTL_XUSB_SET_STATE => on_set_state(&request, data),
        IOCTL_XUSB_POWER_DOWN | IOCTL_XUSB_GET_XINPUT_MANAGEMENT_DRIVER => STATUS_SUCCESS,
        // The async input wait is PENDED on the manual queue and completed by the timer when the
        // packet number moves (see `evt_timer`) — WGI/GameInput poll this way and will not admit a
        // device that refuses it. Classic `xinput1_4` never issues it (it polls GET_STATE), so this
        // costs the working path nothing. A forward failure completes the request with its error.
        IOCTL_XUSB_WAIT_FOR_INPUT => {
            let wq: WDFQUEUE = WAIT_QUEUE.load(Ordering::SeqCst);
            if wq.is_null() {
                STATUS_INVALID_DEVICE_REQUEST
            } else {
                // SAFETY: `wq` is the live manual queue created in EvtDeviceAdd; `request` is this
                // dispatch's request and is CONSUMED by the forward (hence the early return).
                match unsafe { request.forward_to_queue(wq) } {
                    Ok(()) => return,
                    Err((req, st)) => req.complete(st),
                }
                return;
            }
        }
        // Still declined: the guide-button wait has no state of ours to signal on.
        IOCTL_XUSB_WAIT_GUIDE_BUTTON => STATUS_INVALID_DEVICE_REQUEST,
        other => {
            dbglog!("[pf-xusb] unhandled IOCTL 0x{other:08x} in={input_len} out={output_len}");
            STATUS_INVALID_DEVICE_REQUEST
        }
    };
    request.complete(status);
}

// SET_STATE: the rumble packet. Classic xusb22 layout is small; the motor bytes sit near the end.
// We publish a best-effort (large = byte 2, small = byte 3 for the 5-byte form) and log the raw bytes
// so the exact offsets can be confirmed against a real pad.
fn on_set_state(request: &Request, data: Option<&MappedView>) -> NTSTATUS {
    if let Ok((bytes, len)) = request.input_bytes(8)
        && len >= 2
    {
        dbglog!(
            "[pf-xusb] SET_STATE len={len} data: {}",
            pf_umdf_util::hid::hex_dump(&bytes, bytes.len())
        );
        // Observed 5-byte form {00, led, largeMotor, smallMotor, subcmd}: subcmd 0x02 = rumble
        // (large/low-freq at [2], small/high-freq at [3]); 0x01 = player-LED set (ignored).
        // 4-byte = raw XINPUT_VIBRATION → the two motor hi bytes.
        if len >= 5 && bytes[4] == 0x02 {
            publish_rumble(data, bytes[2], bytes[3]);
        } else if len == 4 {
            publish_rumble(data, bytes[1], bytes[3]);
        }
    }
    STATUS_SUCCESS
}
