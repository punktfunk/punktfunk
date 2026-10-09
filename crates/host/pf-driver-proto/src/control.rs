//! Control (`DeviceIoControl`) plane: add/remove, adapter pin, keepalive, frame-channel delivery.

use super::ctl_code;
use bytemuck::{Pod, Zeroable};

// Contiguous op space at 0x900 — distinct from SudoVDA's gappy 0x800/0x888/0x8FF numbering.
/// Add a virtual monitor at a mode → [`AddReply`]. Input [`AddRequest`].
pub const IOCTL_ADD: u32 = ctl_code(0x900);
/// Remove a virtual monitor by session id. Input [`RemoveRequest`].
pub const IOCTL_REMOVE: u32 = ctl_code(0x901);
/// Pin the IddCx render adapter (hybrid-GPU IDD-push). Input [`SetRenderAdapterRequest`].
pub const IOCTL_SET_RENDER_ADAPTER: u32 = ctl_code(0x902);
/// Keepalive (resets the driver watchdog). No payload.
pub const IOCTL_PING: u32 = ctl_code(0x903);
/// Version + watchdog handshake → [`InfoReply`]. No input.
pub const IOCTL_GET_INFO: u32 = ctl_code(0x904);
/// Tear down every virtual monitor (host-startup orphan reap). First-class op — not the
/// SudoVDA "send-and-hope-it's-ignored" hack.
pub const IOCTL_CLEAR_ALL: u32 = ctl_code(0x905);
// 0x906 was `IOCTL_SET_FRAME_CHANNEL` (the pixel ring, retired at v7); never reuse it —
// a v6 driver still answers it.
/// Refresh a LIVE monitor's target-mode list via `IddCxMonitorUpdateModes2`. Input
/// [`UpdateModesRequest`]. CCD then forces the new mode on the same monitor — no REMOVE→ADD,
/// so OS identity and the driver's swap-chain survive. A v3 driver fails the unknown IOCTL;
/// the host falls back to re-arrival.
pub const IOCTL_UPDATE_MODES: u32 = ctl_code(0x907);
/// Deliver the unnamed [`cursor::CursorShm`](crate::cursor) mapping (handle VALUE duplicated
/// into WUDFHost). No event — the host polls the seqlock. Sent once after ADD when
/// [`AddRequest::hw_cursor`] was set. Input [`SetCursorChannelRequest`].
pub const IOCTL_SET_CURSOR_CHANNEL: u32 = ctl_code(0x908);
/// Flip a LIVE monitor's hardware-cursor declaration. `enable = 1` re-declares
/// (`IddCxMonitorSetupHardwareCursor`); `enable = 0` un-declares so DWM composites the
/// pointer into the frame. Only meaningful after [`IOCTL_SET_CURSOR_CHANNEL`]. Input
/// [`SetCursorForwardRequest`].
pub const IOCTL_SET_CURSOR_FORWARD: u32 = ctl_code(0x909);
/// Spike S5, `encode-probe` driver builds only: run the encoder backends inside WUDFHost on
/// one monitor's frames. Input [`EncodeProbeRequest`]. `STATUS_DEVICE_BUSY` while a run is
/// still going; `STATUS_NOT_FOUND` from a driver built without the feature.
pub const IOCTL_ENCODE_PROBE_ARM: u32 = ctl_code(0x90A);
/// The probe's tally → [`EncodeProbeReply`]. No input.
pub const IOCTL_ENCODE_PROBE_STATUS: u32 = ctl_code(0x90B);

/// Take the driver's pending diagnostic lines. No input; the driver fills the output buffer
/// with whole [`log_lines`] records, completes with the bytes written, and keeps whatever did
/// not fit for the next call. The encoder runs inside WUDFHost, so this is the only way its
/// backend rejections, retargets and wedges reach `host.log`.
///
/// Additive at v8: a driver built before it answers `STATUS_NOT_FOUND`, which reads as
/// "no lines" — the host never depends on the reply.
pub const IOCTL_DRAIN_LOG: u32 = ctl_code(0x90E);

/// Severity byte a [`log_lines`] record opens with. Unknown bytes read as [`LOG_INFO`].
pub const LOG_ERROR: u8 = b'E';
/// See [`LOG_ERROR`].
pub const LOG_WARN: u8 = b'W';
/// See [`LOG_ERROR`].
pub const LOG_INFO: u8 = b'I';
/// See [`LOG_ERROR`].
pub const LOG_DEBUG: u8 = b'D';

/// Append one record: the severity byte, the text, `\n`. Embedded newlines become spaces —
/// the separator IS the framing, so a record is exactly one line. Byte-wise is safe: `\n`
/// never appears in a UTF-8 continuation byte.
pub fn write_log_record(out: &mut alloc::vec::Vec<u8>, level: u8, text: &str) {
    out.push(level);
    out.extend(
        text.as_bytes()
            .iter()
            .map(|&b| if b == b'\n' { b' ' } else { b }),
    );
    out.push(b'\n');
}

/// The records in a drained buffer as `(severity, text)`. A record that is empty or not UTF-8
/// is skipped, never guessed at.
pub fn log_lines(buf: &[u8]) -> impl Iterator<Item = (u8, &str)> {
    buf.split(|&b| b == b'\n')
        .filter(|r| !r.is_empty())
        .filter_map(|r| Some((r[0], core::str::from_utf8(&r[1..]).ok()?)))
}

/// `IOCTL_ADD` input. `session_id` keys the monitor (host refcount owns collisions).
/// The driver advertises this mode as preferred; the host still CCD-forces the active mode.
///
/// Size: the luminance + `hw_cursor` tail after `preferred_monitor_id` is prefix-compatible
/// (no protocol bump). An old driver reads [`ADD_REQUEST_LEGACY_SIZE`] bytes; an old host
/// sends that prefix. Zero tail = unknown / off. Further fields must follow the same rule.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct AddRequest {
    pub session_id: u64,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Host-preferred per-client monitor id (`1..=15`) — EDID serial / IddCx `ConnectorIndex` /
    /// `ContainerId`. Stable across reconnects so Windows reapplies per-monitor DPI. `0` = AUTO
    /// (lowest-free id). Occupies the old `_reserved` at offset 20: an old driver ignores it.
    pub preferred_monitor_id: u32,
    /// Client display peak luminance in nits → EDID CTA-861.3 Desired Content Max Luminance.
    /// `0` = unknown → the driver keeps its built-in ~1000-nit block.
    pub max_luminance_nits: u32,
    /// Client max frame-average luminance in nits. `0` = unknown.
    pub max_frame_avg_nits: u32,
    /// Client min luminance in milli-nits (0.001 cd/m² — CTA min lives well below 1 nit).
    /// `0` = unknown.
    pub min_luminance_millinits: u32,
    /// Non-zero = declare an IddCx hardware cursor: DWM excludes the pointer from the frame.
    /// Occupies the old tail `_reserved` at offset 36: an old driver ignores it (stays composited).
    pub hw_cursor: u32,
}

/// [`AddRequest`] size before the luminance tail — prefix an old driver reads / old host sends.
pub const ADD_REQUEST_LEGACY_SIZE: usize = 24;

/// `IOCTL_ADD` reply: the OS target id + the adapter LUID the IDD landed on (split low/high to
/// match `windows` `LUID { LowPart: u32, HighPart: i32 }`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct AddReply {
    pub adapter_luid_low: u32,
    pub adapter_luid_high: i32,
    pub target_id: u32,
    /// Monitor id the driver actually used. Occupies the old `_reserved` at offset 12: an old
    /// driver leaves it `0`, so the host can tell the preference was ignored.
    pub resolved_monitor_id: u32,
    /// WUDFHost pid — duplication target for unnamed frame-object handles. Reported per-ADD,
    /// not per-open, so a WUDFHost restart cannot leave the host duplicating into a dead process.
    pub wudf_pid: u32,
    /// Non-zero = this adapter already carries an irrevocable hardware-cursor declare.
    /// Exclusion is adapter-wide until the adapter resets; sessions without the cursor channel
    /// must self-composite. Prefix-compatible after [`ADD_REPLY_LEGACY_SIZE`]: zeros = unknown.
    pub cursor_excluded: u32,
}

/// [`AddReply`] size before `cursor_excluded` — prefix an old driver writes / old host retrieves.
pub const ADD_REPLY_LEGACY_SIZE: usize = 20;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct RemoveRequest {
    pub session_id: u64,
}

/// `IOCTL_UPDATE_MODES` input: live monitor (ADD `session_id`) and the new preferred mode.
/// The driver replaces the stored list (new mode first, then built-in fallbacks) and pushes
/// it via `IddCxMonitorUpdateModes2`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct UpdateModesRequest {
    pub session_id: u64,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    /// Pads the `u64`-aligned struct to a multiple of 8 (Pod forbids implicit tail padding).
    pub _reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SetRenderAdapterRequest {
    pub luid_low: u32,
    pub luid_high: i32,
}

/// `IOCTL_GET_INFO` reply. `protocol_version` is asserted against [`super::PROTOCOL_VERSION`].
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct InfoReply {
    pub protocol_version: u32,
    pub watchdog_timeout_s: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SetCursorChannelRequest {
    pub target_id: u32,
    pub _pad: u32,
    /// [`cursor::CursorShm`](crate::cursor) mapping handle VALUE, already duplicated into WUDFHost.
    pub header_handle: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct SetCursorForwardRequest {
    pub target_id: u32,
    /// `1` = declare (exclude + forward), `0` = un-declare (DWM composites).
    pub enable: u32,
}

/// [`IOCTL_ENCODE_PROBE_ARM`] input. Zero `frames` / `bitrate_kbps` / `fps` take the
/// driver's defaults (300 / 20000 / 60).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct EncodeProbeRequest {
    /// OS target of the monitor to tap; `0` = whichever drain worker offers first.
    pub target_id: u32,
    /// A [`backend`](crate::encode::backend) id.
    pub backend: u32,
    /// A [`codec`](crate::encode::codec) id. PyroWave pairs only with the PyroWave backend.
    pub codec: u32,
    /// `0` = the backend's own input for `flags`, as `SET_ENCODE` would choose it.
    /// `1` = force BGRA→NV12 on the video engine (the NVENC colour A/B); PyroWave ignores it.
    pub input: u32,
    pub frames: u32,
    pub bitrate_kbps: u32,
    pub fps: u32,
    /// [`PROBE_FLAG_HDR`] | [`PROBE_FLAG_444`]; `0` is an 8-bit 4:2:0 SDR run.
    pub flags: u32,
}

/// [`EncodeProbeRequest::flags`]: 10-bit BT.2020 PQ. The ring carries DWM's own surface, so
/// the desktop must already be in advanced colour or the run fails at `fmt`.
pub const PROBE_FLAG_HDR: u32 = 1 << 0;
/// [`EncodeProbeRequest::flags`]: full chroma. With [`PROBE_FLAG_HDR`] this is the packed
/// 10-bit RGB input NVENC CSCs under FREXT — the pairing `SET_ENCODE` silently lost once.
pub const PROBE_FLAG_444: u32 = 1 << 1;

/// [`IOCTL_ENCODE_PROBE_STATUS`] reply. `mean_*` / `max_*` cover every AU after the first;
/// `first_au_us` alone carries the backend's lazy session init. `name` is a short NUL-padded
/// tag: once open, the chosen input paired with the chroma the encoder reports (`Rgb10+444`);
/// on failure, the failing stage (the driver log has the full error).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
pub struct EncodeProbeReply {
    /// 0 idle, 1 armed, 2 running, 3 done, 4 failed.
    pub state: u32,
    pub backend_opened: u32,
    pub frames_submitted: u32,
    pub aus: u32,
    pub bytes: u64,
    pub open_us: u32,
    pub first_au_us: u32,
    pub mean_submit_to_au_us: u32,
    pub max_submit_to_au_us: u32,
    /// Frames the drain worker had no free pool slot for.
    pub drops: u32,
    pub error: i32,
    pub name: [u8; 32],
}

// Layout is load-bearing across the process boundary. Pod rejects internal padding; these
// assert the externally-visible sizes. `offset_of!` catches a same-size field reorder.
const _: () = {
    use core::mem::{offset_of, size_of};

    assert!(size_of::<AddRequest>() == 40);
    assert!(offset_of!(AddRequest, session_id) == 0);
    assert!(offset_of!(AddRequest, width) == 8);
    assert!(offset_of!(AddRequest, height) == 12);
    assert!(offset_of!(AddRequest, refresh_hz) == 16);
    assert!(offset_of!(AddRequest, preferred_monitor_id) == 20);
    // Luminance tail starts at the legacy boundary (prefix-compat).
    assert!(offset_of!(AddRequest, max_luminance_nits) == ADD_REQUEST_LEGACY_SIZE);
    assert!(offset_of!(AddRequest, max_frame_avg_nits) == 28);
    assert!(offset_of!(AddRequest, min_luminance_millinits) == 32);
    // Former tail `_reserved` — same offset, same total size (rename-only).
    assert!(offset_of!(AddRequest, hw_cursor) == 36);
    assert!(size_of::<AddRequest>() == 40);

    assert!(size_of::<AddReply>() == 24);
    assert!(offset_of!(AddReply, adapter_luid_low) == 0);
    assert!(offset_of!(AddReply, adapter_luid_high) == 4);
    assert!(offset_of!(AddReply, target_id) == 8);
    assert!(offset_of!(AddReply, resolved_monitor_id) == 12);
    assert!(offset_of!(AddReply, wudf_pid) == 16);
    // cursor_excluded starts at the legacy boundary (prefix-compat).
    assert!(offset_of!(AddReply, cursor_excluded) == ADD_REPLY_LEGACY_SIZE);

    assert!(size_of::<RemoveRequest>() == 8);
    assert!(offset_of!(RemoveRequest, session_id) == 0);

    assert!(size_of::<SetCursorChannelRequest>() == 16);
    assert!(offset_of!(SetCursorChannelRequest, target_id) == 0);
    assert!(offset_of!(SetCursorChannelRequest, header_handle) == 8);
    assert!(size_of::<SetCursorForwardRequest>() == 8);
    assert!(offset_of!(SetCursorForwardRequest, target_id) == 0);
    assert!(offset_of!(SetCursorForwardRequest, enable) == 4);

    assert!(size_of::<UpdateModesRequest>() == 24);
    assert!(offset_of!(UpdateModesRequest, session_id) == 0);
    assert!(offset_of!(UpdateModesRequest, width) == 8);
    assert!(offset_of!(UpdateModesRequest, height) == 12);
    assert!(offset_of!(UpdateModesRequest, refresh_hz) == 16);

    assert!(size_of::<SetRenderAdapterRequest>() == 8);
    assert!(offset_of!(SetRenderAdapterRequest, luid_low) == 0);
    assert!(offset_of!(SetRenderAdapterRequest, luid_high) == 4);

    assert!(size_of::<InfoReply>() == 8);
    assert!(offset_of!(InfoReply, protocol_version) == 0);
    assert!(offset_of!(InfoReply, watchdog_timeout_s) == 4);

    assert!(size_of::<EncodeProbeRequest>() == 32);
    assert!(offset_of!(EncodeProbeRequest, target_id) == 0);
    assert!(offset_of!(EncodeProbeRequest, backend) == 4);
    assert!(offset_of!(EncodeProbeRequest, frames) == 16);
    assert!(offset_of!(EncodeProbeRequest, flags) == 28);
    assert!(size_of::<EncodeProbeReply>() == 80);
    assert!(offset_of!(EncodeProbeReply, state) == 0);
    assert!(offset_of!(EncodeProbeReply, bytes) == 16);
    assert!(offset_of!(EncodeProbeReply, open_us) == 24);
    assert!(offset_of!(EncodeProbeReply, error) == 44);
    assert!(offset_of!(EncodeProbeReply, name) == 48);
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MIN_DRIVER_PROTOCOL_VERSION, PROTOCOL_VERSION};

    #[test]
    fn control_structs_roundtrip_through_bytes() {
        let req = AddRequest {
            session_id: 0xDEAD_BEEF_CAFE_F00D,
            width: 3840,
            height: 2160,
            refresh_hz: 120,
            preferred_monitor_id: 7,
            max_luminance_nits: 800,
            max_frame_avg_nits: 400,
            min_luminance_millinits: 50, // 0.05 nits
            hw_cursor: 1,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 40);
        assert_eq!(*bytemuck::from_bytes::<AddRequest>(bytes), req);
        // preferred_monitor_id occupies the old `_reserved` slot at offset 20.
        assert_eq!(bytes[20..24], 7u32.to_le_bytes());
        // Luminance tail rides after the legacy boundary; a zero-filled tail decodes as unknown.
        assert_eq!(bytes[24..28], 800u32.to_le_bytes());
        let mut legacy = [0u8; 40];
        legacy[..ADD_REQUEST_LEGACY_SIZE].copy_from_slice(&bytes[..ADD_REQUEST_LEGACY_SIZE]);
        // `pod_read_unaligned`, not `from_bytes`: `legacy` is `[u8; 40]` (align 1) but
        // `AddRequest` is align 8. `from_bytes` panics unless the buffer happens to be 8-aligned.
        let old = bytemuck::pod_read_unaligned::<AddRequest>(&legacy);
        assert_eq!(old.preferred_monitor_id, 7);
        assert_eq!(
            (
                old.max_luminance_nits,
                old.max_frame_avg_nits,
                old.min_luminance_millinits
            ),
            (0, 0, 0)
        );

        let reply = AddReply {
            adapter_luid_low: 0x1234_5678,
            adapter_luid_high: -2,
            target_id: 262,
            resolved_monitor_id: 7,
            wudf_pid: 4242,
            cursor_excluded: 1,
        };
        let rbytes = bytemuck::bytes_of(&reply);
        assert_eq!(rbytes.len(), 24);
        assert_eq!(*bytemuck::from_bytes::<AddReply>(rbytes), reply);
        // resolved_monitor_id occupies the old `_reserved` slot at offset 12.
        assert_eq!(rbytes[12..16], 7u32.to_le_bytes());
        // Duplication-target pid trails at offset 16.
        assert_eq!(rbytes[16..20], 4242u32.to_le_bytes());
        // cursor_excluded rides after the legacy boundary; a zero-filled tail reads as unknown.
        assert_eq!(rbytes[20..24], 1u32.to_le_bytes());
        assert_eq!(ADD_REPLY_LEGACY_SIZE, 20);
    }

    #[test]
    fn update_modes_request_roundtrips_and_versions_cohere() {
        let req = UpdateModesRequest {
            session_id: 42,
            width: 2560,
            height: 1409, // arbitrary — the in-place path serves window-drag modes
            refresh_hz: 120,
            _reserved: 0,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 24);
        assert_eq!(*bytemuck::from_bytes::<UpdateModesRequest>(bytes), req);
        assert_eq!(bytes[8..12], 2560u32.to_le_bytes());
        // v9 widened the AU slot, v8 scoped the driver to its owners, v7 replaced the video
        // transport; each makes the floor the version itself.
        assert_eq!(PROTOCOL_VERSION, 9);
        assert_eq!(MIN_DRIVER_PROTOCOL_VERSION, PROTOCOL_VERSION);
    }

    /// The driver writes these records and the host parses them in another process: a one-sided
    /// change to either half silently loses the encoder's diagnostics.
    #[test]
    fn log_records_roundtrip_and_stay_one_line_each() {
        let mut buf = alloc::vec::Vec::new();
        write_log_record(&mut buf, LOG_WARN, "AMF rejected 4:4:4");
        write_log_record(&mut buf, LOG_INFO, "two\nlines");
        write_log_record(&mut buf, LOG_ERROR, "");
        let got: alloc::vec::Vec<_> = log_lines(&buf).collect();
        assert_eq!(
            got,
            [
                (LOG_WARN, "AMF rejected 4:4:4"),
                (LOG_INFO, "two lines"),
                (LOG_ERROR, ""),
            ]
        );
        // Framing holds only while every record is exactly one line.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 3);
    }

    #[test]
    fn encode_probe_structs_roundtrip_through_bytes() {
        let req = EncodeProbeRequest {
            target_id: 7,
            backend: 1,
            codec: 2,
            input: 0,
            frames: 300,
            bitrate_kbps: 20_000,
            fps: 60,
            flags: 0,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 32);
        assert_eq!(
            bytemuck::pod_read_unaligned::<EncodeProbeRequest>(bytes),
            req
        );
        let mut name = [0u8; 32];
        name[..4].copy_from_slice(b"open");
        let reply = EncodeProbeReply {
            state: 4,
            backend_opened: 0,
            frames_submitted: 0,
            aus: 0,
            bytes: 0x1_0000_0001,
            open_us: 1234,
            first_au_us: 0,
            mean_submit_to_au_us: 0,
            max_submit_to_au_us: 0,
            drops: 3,
            error: -1,
            name,
        };
        let bytes = bytemuck::bytes_of(&reply);
        assert_eq!(bytes.len(), 80);
        // `bytes` rides 8-aligned at 16; `error` at 44; the tag opens at 48.
        assert_eq!(bytes[16..24], 0x1_0000_0001u64.to_le_bytes());
        assert_eq!(bytes[44..48], (-1i32).to_le_bytes());
        assert_eq!(&bytes[48..52], b"open");
        assert_eq!(
            bytemuck::pod_read_unaligned::<EncodeProbeReply>(bytes),
            reply
        );
    }
}
