//! The `pf-driver-proto` control plane (`EvtIddCxDeviceIoControl`). The host opens the device
//! interface (`PF_VDISPLAY_INTERFACE_GUID`) and drives the low-frequency IOCTLs: GET_INFO (version
//! handshake), PING (watchdog keepalive), ADD/REMOVE/CLEAR_ALL (virtual monitors),
//! SET_RENDER_ADAPTER, UPDATE_MODES, the cursor channel, the encoder verbs, and DRAIN_LOG (this
//! process's diagnostic lines, on their way to `host.log`).
//!
//! [`dispatch`] wraps the raw `WDFREQUEST` in a [`Request`] token once and hands it to a handler BY
//! VALUE; completing consumes the token, so "every path completes exactly once" (the
//! `EVT_IDD_CX_DEVICE_IO_CONTROL` shape returns `()`, leaving the framework no status to act on) is
//! a type-level fact. Buffer I/O rides the token's methods over `bytemuck` casts of the Pod wire
//! structs — which leaves the control plane one `unsafe` block, the token construction.
//!
//! Every verb is scoped to the calling process (v8): a monitor answers only to the owner whose
//! ADD created it, and CLEAR_ALL departs the caller's own. Two hosts on one box never meet.

use bytemuck::Pod;
use pf_driver_proto::control;
use pf_driver_proto::vdisplay::valid_mode;
use pf_umdf_util::wdf::Request;
use wdk_sys::WDFREQUEST;

use crate::{
    STATUS_ACCESS_DENIED, STATUS_BUFFER_TOO_SMALL, STATUS_INVALID_PARAMETER, STATUS_NOT_FOUND,
    STATUS_SUCCESS,
};

/// Dispatch one control IOCTL and complete the request.
///
/// # Safety
/// `request` is the framework-provided `WDFREQUEST` for an `EvtIddCxDeviceIoControl` call.
pub unsafe fn dispatch(request: WDFREQUEST, ioctl_code: u32) {
    // SAFETY: `request` is the live request for THIS EvtIddCxDeviceIoControl invocation — exactly
    // the contract `Request::new` requires. Everything below is safe: the token owns completion.
    let request = unsafe { Request::new(request) };
    // The calling process owns what this IOCTL creates and reaches only what it owns. Every
    // IOCTL is liveness for that owner, so its watchdog fires only once it has gone silent.
    let owner = request.requestor_pid();
    // A seat device admits NETWORK SERVICE for the remoting stack's transport. Every NS service
    // lives in session 0 and the seat host runs inside its seat session, so punktfunk verbs on
    // that device answer only a requestor outside session 0.
    if crate::adapter::is_seat_role()
        && ioctl_code != IOCTL_RDPIDD_TRANSPORT
        && session_of(owner) == Some(0)
    {
        dbglog!("[pf-vd] seat: refusing IOCTL {ioctl_code:#010x} from session-0 pid {owner}");
        request.complete(STATUS_ACCESS_DENIED);
        return;
    }
    crate::watchdog::ping(owner, request.file_object());
    match ioctl_code {
        control::IOCTL_GET_INFO => {
            let reply = control::InfoReply {
                protocol_version: pf_driver_proto::PROTOCOL_VERSION,
                watchdog_timeout_s: crate::watchdog::WATCHDOG_TIMEOUT_S,
            };
            write_output_prefix_complete(request, &reply, size_of::<control::InfoReply>());
        }
        control::IOCTL_PING => request.complete(STATUS_SUCCESS),
        control::IOCTL_DRAIN_LOG => {
            let lines = crate::log::drain(request.output_buffer_len());
            let status = request.copy_to_output(&lines);
            request.complete(status);
        }
        control::IOCTL_ADD => add(owner, request),
        control::IOCTL_REMOVE => remove(owner, request),
        control::IOCTL_CLEAR_ALL => {
            crate::monitor::clear_all(owner);
            request.complete(STATUS_SUCCESS);
        }
        control::IOCTL_SET_RENDER_ADAPTER => set_render_adapter(owner, request),
        control::IOCTL_UPDATE_MODES => update_modes(owner, request),
        control::IOCTL_SET_CURSOR_CHANNEL => set_cursor_channel(owner, request),
        control::IOCTL_SET_CURSOR_FORWARD => set_cursor_forward(owner, request),
        pf_driver_proto::encode::IOCTL_SET_ENCODE => set_encode(owner, request),
        pf_driver_proto::encode::IOCTL_ENCODE_CTL => encode_ctl(owner, request),
        #[cfg(feature = "encode-probe")]
        control::IOCTL_ENCODE_PROBE_ARM => encode_probe_arm(request),
        #[cfg(feature = "encode-probe")]
        control::IOCTL_ENCODE_PROBE_STATUS => {
            let reply = crate::encode_probe::status();
            write_output_prefix_complete(request, &reply, size_of::<control::EncodeProbeReply>());
        }
        // The remoting stack drives a seat display over RdpIdd's own private opcodes and treats
        // STATUS_NOT_FOUND as "this is not a display driver" (`RDPIDD_OPCODE_TYPE_BIND_DRIVER` ->
        // `IddInterfaceArrivalFailure`). Log what it asks for and answer, so the protocol it needs
        // can be learnt from the trace rather than guessed. The console device still refuses.
        IOCTL_RDPIDD_TRANSPORT if crate::adapter::is_seat_role() => rdpidd_transport(request),
        _ => {
            if crate::adapter::is_seat_role() {
                dbglog!("[pf-vd] seat: refusing unknown IOCTL {ioctl_code:#010x}");
            }
            request.complete(STATUS_NOT_FOUND)
        }
    }
}

/// Session id of `pid`, `None` when the lookup fails (pid gone). No handle is opened, so
/// this works from WUDFHost's LocalService token against a SYSTEM requestor.
fn session_of(pid: u32) -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    let mut session = 0u32;
    // SAFETY: `session` is a live local out-param; the call reads a table by pid and touches no
    // memory of ours.
    unsafe { ProcessIdToSessionId(pid, &mut session) }
        .ok()
        .map(|()| session)
}

/// The private transport the remoting stack drives a seat display over.
///
/// One buffered IOCTL carries every message, and none of it is documented — this layout is read off
/// the wire and off RdpIdd's own `ProcessIoctl`:
///
/// ```text
/// +0x00  u64  total message length (equals the input buffer size)
/// +0x08  u32  opcode
/// +0x0c  ..   opcode-specific payload
/// ```
///
/// Observed: `0x408` binds the driver and is the only one wanting output (12 bytes); `0x409`,
/// `0x40a` and `0x40b` follow it and want none. `STATUS_NOT_FOUND` reads as "not a display driver"
/// and an empty output reads as a refusal, so the reply is a zeroed buffer of the size asked for.
///
/// This is ANSWERED, not implemented: the payloads are not understood, so an opcode outside the
/// observed set is logged rather than silently accepted as understood.
const IOCTL_RDPIDD_TRANSPORT: u32 = 0x8000_0040;
const RDPIDD_OPCODES_SEEN: core::ops::RangeInclusive<u32> = 0x408..=0x40b;

fn rdpidd_transport(request: Request) {
    let (input, in_len) = request.input_bytes(16).unwrap_or_default();
    let opcode = input
        .get(8..12)
        .and_then(|s| s.try_into().ok())
        .map_or(0, u32::from_le_bytes);
    let out_len = request.output_buffer_len();
    if !RDPIDD_OPCODES_SEEN.contains(&opcode) {
        dbglog!("[pf-vd] seat: unknown rdpidd opcode {opcode:#x} in={in_len} out={out_len}");
    }
    let status = if out_len > 0 {
        request.copy_to_output(&vec![0u8; out_len])
    } else {
        STATUS_SUCCESS
    };
    request.complete(status);
}

/// `IOCTL_SET_ENCODE` (v7): open an encoder on the delivered AU section. A well-formed request
/// for a live monitor of `owner`'s always completes successfully with the structured reply —
/// the driver owns the two handles from there — and only a malformed or unmatched one fails
/// the IOCTL with nothing adopted (`SetEncodeReply` docs).
fn set_encode(owner: u32, request: Request) {
    use pf_driver_proto::encode::{
        SET_ENCODE_REQUEST_LEGACY_SIZE, SetEncodeReply, SetEncodeRequest,
    };
    // Sized before the handles are adopted: on an IOCTL error the host reaps its duplicates,
    // and the driver must not have taken them as well.
    if request.output_buffer_len() < size_of::<SetEncodeReply>() {
        request.complete(STATUS_BUFFER_TOO_SMALL);
        return;
    }
    let Some(req) = read_input_prefix::<SetEncodeRequest>(&request, SET_ENCODE_REQUEST_LEGACY_SIZE)
    else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    match crate::encode::set_encode(owner, &req) {
        Ok(reply) => write_output_prefix_complete(request, &reply, size_of::<SetEncodeReply>()),
        Err(st) => request.complete(st),
    }
}

/// `IOCTL_ENCODE_CTL` (v7): one control op on `owner`'s monitor's live encoder; `reset`
/// reopens it.
fn encode_ctl(owner: u32, request: Request) {
    let Some(req) = read_input::<pf_driver_proto::encode::EncodeCtlRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    request.complete(crate::encode::encode_ctl(owner, &req));
}

/// `IOCTL_ENCODE_PROBE_ARM`: start an in-process encode run on one monitor's frames.
#[cfg(feature = "encode-probe")]
fn encode_probe_arm(request: Request) {
    let Some(req) = read_input::<control::EncodeProbeRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    request.complete(crate::encode_probe::arm(&req));
}

/// `IOCTL_SET_RENDER_ADAPTER`: pin the IddCx render adapter (hybrid-GPU IDD-push). Adapter-wide,
/// so `owner` may not move it from under another owner's live monitors.
fn set_render_adapter(owner: u32, request: Request) {
    let Some(req) = read_input::<control::SetRenderAdapterRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    let st = crate::adapter::set_render_adapter(owner, req.luid_low, req.luid_high);
    request.complete(st);
}

/// `IOCTL_ADD`: create `owner`'s virtual monitor at the requested mode and reply with its OS
/// target id and LUID, or complete with the status class `create_monitor` returns.
fn add(owner: u32, request: Request) {
    // Sized before the monitor exists: a short buffer fails with nothing created, or the host
    // reads the error as "nothing to remove" and the monitor holds its slot until the owner goes.
    if request.output_buffer_len() < control::ADD_REPLY_LEGACY_SIZE {
        request.complete(STATUS_BUFFER_TOO_SMALL);
        return;
    }
    let Some(req) = read_add_request(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    if !valid_mode(req.width, req.height, req.refresh_hz) {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    }
    // A refused adapter init is never retried by PnP; the host's ADD retries are the only
    // second chance, so use them.
    if crate::adapter::adapter().is_none() {
        crate::adapter::retry_init();
    }
    let (monitor_id, target_id, luid_low, luid_high) =
        match crate::monitor::create_monitor(owner, &req) {
            Ok(made) => made,
            Err(st) => {
                request.complete(st);
                return;
            }
        };
    let reply = control::AddReply {
        adapter_luid_low: luid_low,
        adapter_luid_high: luid_high,
        target_id,
        resolved_monitor_id: monitor_id,
        // This WUDFHost's pid — where the host duplicates the sealed frame channel's handles INTO
        // (`ProcessSharingDisabled`: this process is exclusively ours and dies with the device).
        wudf_pid: std::process::id(),
        // An irrevocable hardware-cursor declare from an EARLIER session excludes the pointer
        // ADAPTER-wide, not just on the declaring target, so a channel-less session on this
        // adapter must self-composite the pointer (design/remote-desktop-sweep.md §8.6).
        cursor_excluded: crate::registry::any_declared() as u32,
    };
    // Dual-size reply (the `cursor_excluded` tail ext): an un-upgraded host retrieves only the
    // legacy-size buffer — write the prefix it asked for instead of failing its ADD.
    write_output_prefix_complete(request, &reply, control::ADD_REPLY_LEGACY_SIZE);
}

/// `IOCTL_SET_CURSOR_CHANNEL` (v5): adopt `owner`'s monitor's hardware-cursor section, declare
/// the hardware cursor to the OS, start the query→publish worker.
fn set_cursor_channel(owner: u32, request: Request) {
    let Some(req) = read_input::<control::SetCursorChannelRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    let Some(ch) = crate::cursor::CursorChannel::from_request(&req) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    match crate::cursor::set_cursor_channel(owner, req.target_id, ch) {
        Ok(()) => request.complete(STATUS_SUCCESS),
        Err(ch) => {
            dbglog!(
                "[pf-vd] SET_CURSOR_CHANNEL: channel for target_id {} not adopted — rejecting",
                req.target_id
            );
            // NOT adopted: the host's error path reaps the duplicated handle remotely.
            ch.into_unowned();
            request.complete(STATUS_NOT_FOUND);
        }
    }
}

/// `IOCTL_SET_CURSOR_FORWARD` (v6): the mid-stream cursor-render flip — (un)declare `owner`'s
/// LIVE monitor's hardware cursor as the client's mouse model demands.
fn set_cursor_forward(owner: u32, request: Request) {
    let Some(req) = read_input::<control::SetCursorForwardRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    if crate::cursor::set_cursor_forward(owner, req.target_id, req.enable != 0) {
        request.complete(STATUS_SUCCESS);
    } else {
        dbglog!(
            "[pf-vd] SET_CURSOR_FORWARD: no cursor-channel monitor with target_id {} — rejecting",
            req.target_id
        );
        request.complete(STATUS_NOT_FOUND);
    }
}

/// `IOCTL_UPDATE_MODES` (v4): refresh a LIVE monitor's target-mode list to a new preferred mode —
/// the in-place mid-stream resize (`design/first-frame-and-resize-latency.md` P2). The monitor is
/// NOT departed: its OS identity, swap-chain machinery and encode session all survive; the host
/// force-sets the freshly-advertised mode afterwards. Only `owner`'s monitor answers.
fn update_modes(owner: u32, request: Request) {
    let Some(req) = read_input::<control::UpdateModesRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    if !valid_mode(req.width, req.height, req.refresh_hz) {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    }
    let st = crate::monitor::update_monitor_modes(
        owner,
        req.session_id,
        req.width,
        req.height,
        req.refresh_hz,
        req.refresh_mhz,
    );
    request.complete(st);
}

/// `IOCTL_REMOVE`: depart + drop `owner`'s monitor for the given session id.
fn remove(owner: u32, request: Request) {
    let Some(req) = read_input::<control::RemoveRequest>(&request) else {
        request.complete(STATUS_INVALID_PARAMETER);
        return;
    };
    crate::monitor::remove_monitor(owner, req.session_id);
    request.complete(STATUS_SUCCESS);
}

/// Read an [`control::AddRequest`], accepting BOTH wire sizes: the full struct, or an un-upgraded
/// host's [`ADD_REQUEST_LEGACY_SIZE`](control::ADD_REQUEST_LEGACY_SIZE)-byte prefix (no client-HDR
/// luminance tail), whose missing tail zero-fills to "unknown" — so a new driver keeps serving an
/// old host (see the `AddRequest` size-compatibility docs).
fn read_add_request(request: &Request) -> Option<control::AddRequest> {
    read_input_prefix(request, control::ADD_REQUEST_LEGACY_SIZE)
}

/// Read a Pod input struct that grew prefix-compatibly: the full struct, or at least
/// `legacy_size` bytes of it from an older host, whose missing tail zero-fills to the
/// Zeroable contract's "unknown"/default for every field past what the host sent.
fn read_input_prefix<T: Pod>(request: &Request, legacy_size: usize) -> Option<T> {
    let (bytes, _) = request.input_bytes(size_of::<T>()).ok()?;
    if bytes.len() < legacy_size {
        return None;
    }
    let mut out = T::zeroed();
    bytemuck::bytes_of_mut(&mut out)[..bytes.len()].copy_from_slice(&bytes);
    Some(out)
}

/// Read a Pod input struct from the request's input buffer. `None` if the host sent fewer bytes
/// than the struct (or no input buffer at all) — every caller answers that with
/// `STATUS_INVALID_PARAMETER`.
fn read_input<T: Pod>(request: &Request) -> Option<T> {
    // `input_bytes` caps its copy at `size_of::<T>()`, so a full-length result IS the "host sent at
    // least the whole struct" check — and it keeps `pod_read_unaligned` off its panic path.
    let (bytes, _) = request.input_bytes(size_of::<T>()).ok()?;
    (bytes.len() == size_of::<T>()).then(|| bytemuck::pod_read_unaligned(&bytes))
}

/// Copy a Pod reply into the output buffer and complete with the byte count.
///
/// `min_size` is the SHORTEST reply the caller will serve; anything from there up to
/// `size_of::<T>()` is written as a prefix of the struct. That is the dual-size discipline behind
/// [`control::AddReply`]'s appended `cursor_excluded`: a host that retrieved only the legacy prefix
/// gets exactly that prefix instead of a failed IOCTL. A buffer shorter than `min_size` cannot
/// carry a usable reply and completes `STATUS_BUFFER_TOO_SMALL`.
fn write_output_prefix_complete<T: Pod>(request: Request, value: &T, min_size: usize) {
    let out_len = request.output_buffer_len();
    if out_len < min_size {
        request.complete(STATUS_BUFFER_TOO_SMALL);
        return;
    }
    let take = out_len.min(size_of::<T>());
    let st = request.copy_to_output(&bytemuck::bytes_of(value)[..take]);
    request.complete(st);
}
