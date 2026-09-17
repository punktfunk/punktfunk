//! Shared binary contract between the punktfunk host and the `pf-vdisplay` IddCx driver.
//!
//! Two planes:
//! * [`control`] — `DeviceIoControl` (add/remove, adapter pin, keepalive, info, clear-all,
//!   cursor delivery). Owned and versioned — not the SudoVDA ABI.
//! * [`encode`] — the video transport. The host creates an unnamed section plus a ready event,
//!   duplicates the handles into WUDFHost over [`encode::IOCTL_SET_ENCODE`], and the driver's
//!   encoder publishes access units into it. No object-name scheme: unnamed objects cannot be
//!   enumerated, opened by name, or squatted. This crate owns [`encode::au::AuHeader`], the
//!   [`encode::FrameToken`] publish cell and the status codes.
//!   Evidence: `design/idd-push-security.md`.
//!
//! GUID and LUID travel as integers; each side converts to its own `windows` / bindgen types.
//! `Pod` + `offset_of!` asserts make a one-sided layout edit a compile error.
#![forbid(unsafe_code)]
#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// Device-interface GUID `{70667664-7044-5350-a1b2-c3d4e5f60001}`.
/// Not SudoVDA's `{e5bcc234-…}`: a private GUID so a real SudoVDA install cannot bind.
/// Construct via `GUID::from_u128(PF_VDISPLAY_INTERFACE_GUID_U128)`.
pub const PF_VDISPLAY_INTERFACE_GUID_U128: u128 = 0x7066_7664_7044_5350_a1b2_c3d4_e5f6_0001;

/// `(Data1, Data2, Data3, Data4)` of [`PF_VDISPLAY_INTERFACE_GUID_U128`].
/// This crate is `no_std` and has no `GUID` type; callers rebuild theirs from these fields.
#[must_use]
pub const fn interface_guid_fields() -> (u32, u16, u16, [u8; 8]) {
    let g = PF_VDISPLAY_INTERFACE_GUID_U128;
    (
        (g >> 96) as u32,
        (g >> 80) as u16,
        (g >> 64) as u16,
        (g as u64).to_be_bytes(),
    )
}

/// Bumped on any incompatible change to either plane. Exchanged via [`control::IOCTL_GET_INFO`];
/// host and driver assert a match at startup.
///
/// v9 widens [`encode::au::AuSlot`] from 32 to 48 bytes with the driver's encode-submit and
/// publish QPC, so the host can split present → arrival into pool, encode and hand-off. A layout
/// change, so not additive; host and driver ship in one installer.
///
/// v8 scopes the driver to the process that asks: a monitor, its encoder and its cursor channel
/// answer only to the owner whose `IOCTL_ADD` created them, `IOCTL_CLEAR_ALL` departs the
/// caller's own, and an owner's monitors depart when its last control handle closes or it goes
/// silent for the watchdog window. `IOCTL_SET_RENDER_ADAPTER` stays adapter-wide by IddCx
/// design. v7 replaced the pixel ring with driver-side encode into the host's section
/// ([`encode::IOCTL_SET_ENCODE`]); `IOCTL_SET_FRAME_CHANNEL` no longer exists.
///
/// [`control::AddRequest`] luminance tail and [`control::AddReply::cursor_excluded`] are
/// prefix-compatible (no bump): a short read/write sees zeros = unknown. A hardware-cursor
/// declare is irrevocable on the adapter — DWM excludes the pointer from every later monitor
/// until the adapter resets — so the driver blends the pointer when the client draws none.
pub const PROTOCOL_VERSION: u32 = 9;

/// Oldest driver this host still drives. Equal to [`PROTOCOL_VERSION`]: a v8 driver writes
/// 32-byte slots into a table this host lays out at 48, v7 lets a second host reach this
/// host's monitors, and v6 speaks a video transport that no longer exists.
pub const MIN_DRIVER_PROTOCOL_VERSION: u32 = 9;

/// `CTL_CODE(FILE_DEVICE_UNKNOWN = 0x22, func, METHOD_BUFFERED = 0, FILE_ANY_ACCESS = 0)`.
pub const fn ctl_code(func: u32) -> u32 {
    (0x22u32 << 16) | (func << 2)
}

/// Control (`DeviceIoControl`) plane: add/remove, adapter pin, keepalive, frame-channel delivery.
pub mod control {
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
}

/// The 256-byte EDID the `pf-vdisplay` driver hands IddCx for each virtual monitor: an EDID 1.4
/// base block plus a CTA-861.3 extension carrying a BT.2020 Colorimetry block and an HDR Static
/// Metadata block declaring the SMPTE ST 2084 (PQ) EOTF. Windows reads a display's HDR capability
/// from that CTA block; without it the monitor is SDR-only whatever the IddCx adapter's FP16 /
/// wide-gamut / 10-bit caps say.
///
/// Identity: manufacturer "PNK", product name "Punktfunk" (the 0xFC descriptor Windows shows), and
/// a per-monitor serial at base offset 0x0C that [`get_serial`] reads back out of the EDID the OS
/// hands to the mode callbacks. No HDMI Vendor-Specific Data Block: a VSDB carries physical-sink
/// facts (CEC address, TMDS limits) a virtual display does not have, and Windows drives the
/// monitor without one.
///
/// Lives here, not in the driver: the driver only builds under the WDK, and one wrong byte drops
/// HDR silently. `no_std` + integer-only, so it drops into the driver unchanged.
pub mod edid {
    /// `2^(k/32)` for `k = 0..32` in Q16 fixed point (`round(2^(k/32) * 65536)`) — the fractional
    /// step table for the CTA-861.3 luminance exponent.
    const POW2_Q16: [u32; 32] = [
        65536, 66971, 68438, 69936, 71468, 73032, 74632, 76266, 77936, 79642, 81386, 83169, 84990,
        86851, 88752, 90696, 92682, 94711, 96785, 98905, 101070, 103283, 105545, 107856, 110218,
        112631, 115098, 117618, 120194, 122825, 125515, 128263,
    ];

    /// Decode a CTA-861.3 max / frame-average luminance code to MILLI-nits:
    /// `L = 50 * 2^(CV/32)` cd/m², so `L_millinits = 50_000 * 2^(CV/32)`.
    /// (`CV = 255` ≈ 12_525 nits — comfortably inside u64 at Q16.)
    pub const fn cta_max_millinits(code: u8) -> u64 {
        let whole = code as u32 / 32;
        let frac = code as u32 % 32;
        ((50_000u64 << whole) * POW2_Q16[frac as usize] as u64) >> 16
    }

    /// Largest CTA-861.3 code whose decoded luminance does not exceed `nits` — never advertise
    /// brighter than the glass. Clamped to `1..=255`: `0` is "no data" on the wire; callers gate
    /// on `nits > 0`. A sub-51-nit request (no real HDR panel) still codes as 1.
    pub fn cta_max_luminance_code(nits: u32) -> u8 {
        let target = nits as u64 * 1000;
        let mut code = 1u8;
        while code < 255 && cta_max_millinits(code + 1) <= target {
            code += 1;
        }
        code
    }

    /// Floor integer square root (Newton). `u64::isqrt` needs Rust 1.84, above this crate's 1.82
    /// MSRV. Converges in ≤ 6 iterations from the power-of-two seed.
    fn isqrt_u64(x: u64) -> u64 {
        if x == 0 {
            return 0;
        }
        // Seed strictly above sqrt(x): 2^(ceil(bits/2)).
        let mut r = 1u64 << (64 - x.leading_zeros()).div_ceil(2);
        loop {
            let next = (r + x / r) / 2;
            if next >= r {
                return r;
            }
            r = next;
        }
    }

    /// Code a display's min luminance (MILLI-nits) as the CTA-861.3 min-luminance value, which is
    /// relative to the block's coded max: `L_min = L_max * (CV/255)^2 / 100`, so
    /// `CV = 255 * sqrt(100 * L_min / L_max)` — rounded to nearest. `max_code` is the byte
    /// produced by [`cta_max_luminance_code`]; a result of `0` (a true-black panel, or
    /// `millinits = 0` = unknown) is valid on the wire.
    pub fn cta_min_luminance_code(millinits: u32, max_code: u8) -> u8 {
        let max_millinits = cta_max_millinits(max_code);
        if millinits == 0 || max_millinits == 0 {
            return 0;
        }
        // CV = sqrt(100 * 255^2 * L_min / L_max); round to nearest by comparing the two flanking
        // squares (the integer sqrt floors).
        let x = (100u64 * 255 * 255).saturating_mul(millinits as u64) / max_millinits;
        let floor = isqrt_u64(x);
        let cv = if (floor + 1) * (floor + 1) - x <= x - floor * floor {
            floor + 1
        } else {
            floor
        };
        cv.min(255) as u8
    }

    /// Fixed reduced-blanking geometry for [`dtd`] (CVT-RBv2-shaped): 80 px of horizontal and 45
    /// lines of vertical blanking, front-porch/sync splits within them. A virtual display has no
    /// real scan-out, so the blanking only has to be self-consistent — the pixel clock is derived
    /// from these same totals.
    const DTD_H_BLANK: u32 = 80;
    const DTD_V_BLANK: u32 = 45;
    const DTD_H_SYNC_OFFSET: u32 = 8;
    const DTD_H_SYNC_WIDTH: u32 = 32;
    const DTD_V_SYNC_OFFSET: u32 = 3;
    const DTD_V_SYNC_WIDTH: u32 = 5;

    /// 18-byte EDID detailed timing descriptor for `width`×`height`@`refresh_hz` with the fixed
    /// reduced blanking above. `None` when the mode does not fit: pixel clock above 655.35 MHz
    /// (u16 10 kHz field — 4K120-class) or active dimensions above the 12-bit fields. Flags byte
    /// 0x1E: digital separate sync, +H/+V.
    pub fn dtd(width: u32, height: u32, refresh_hz: u32) -> Option<[u8; 18]> {
        if width == 0 || height == 0 || refresh_hz == 0 || width > 4095 || height > 4095 {
            return None;
        }
        let h_total = u64::from(width + DTD_H_BLANK);
        let v_total = u64::from(height + DTD_V_BLANK);
        let clock_10khz = h_total * v_total * u64::from(refresh_hz) / 10_000;
        let clock_10khz = u16::try_from(clock_10khz).ok()?;
        let mut d = [0u8; 18];
        d[0..2].copy_from_slice(&clock_10khz.to_le_bytes());
        d[2] = (width & 0xFF) as u8;
        d[3] = (DTD_H_BLANK & 0xFF) as u8;
        d[4] = (((width >> 8) & 0x0F) << 4) as u8 | ((DTD_H_BLANK >> 8) & 0x0F) as u8;
        d[5] = (height & 0xFF) as u8;
        d[6] = (DTD_V_BLANK & 0xFF) as u8;
        d[7] = (((height >> 8) & 0x0F) << 4) as u8 | ((DTD_V_BLANK >> 8) & 0x0F) as u8;
        d[8] = (DTD_H_SYNC_OFFSET & 0xFF) as u8;
        d[9] = (DTD_H_SYNC_WIDTH & 0xFF) as u8;
        d[10] = (((DTD_V_SYNC_OFFSET & 0x0F) << 4) | (DTD_V_SYNC_WIDTH & 0x0F)) as u8;
        d[11] = ((((DTD_H_SYNC_OFFSET >> 8) & 0x03) << 6)
            | (((DTD_H_SYNC_WIDTH >> 8) & 0x03) << 4)
            | (((DTD_V_SYNC_OFFSET >> 4) & 0x03) << 2)
            | ((DTD_V_SYNC_WIDTH >> 4) & 0x03)) as u8;
        // Bytes 12..16 (image size mm, borders) stay 0 = undefined.
        d[17] = 0x1E;
        Some(d)
    }

    /// Per-monitor serial number: base-block offset 0x0C, little-endian u32.
    const SERIAL_OFFSET: usize = 0x0C;

    /// EDID 1.4 base block. Differs from a plain SDR virtual EDID by revision 1.4 (byte 19),
    /// 10-bit digital video input (byte 20 = 0xB0) and one extension present (byte 126 = 0x01).
    /// The checksum (byte 127), the serial at 0x0C and the preferred DTD are patched in
    /// [`generate`], so editing the name here needs no hand-computed checksum.
    #[rustfmt::skip]
    const BASE: [u8; 128] = [
        0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, // fixed header
        0x41, 0xCB, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, // mfr "PNK", product code 1 (0 = "unset" to EDID tooling), serial (patched)
        0xFF, 0x21, 0x01, 0x04, 0xB0, 0x32, 0x1F, 0x78, // week/year, EDID 1.4, 10-bit digital, size, gamma
        0x03, 0x78, 0xB1, 0xB5, 0x4A, 0x2B, 0xCC, 0x21, // feature (sRGB-default CLEARED), BT.2020 primaries...
        0x0B, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01, // ...BT.2020 primaries, established timings, std timings
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
        0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3A, // std timings, DTD 1 (placeholder preferred timing)
        0x80, 0x18, 0x71, 0x38, 0x2D, 0x40, 0x58, 0x2C,
        0x45, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E,
        0x00, 0x00, 0x00, 0xFD, 0x08, 0x17, 0xF0, 0x0F, // range-limits: offsets H-max+255, 23-240 Hz, min-H 15 kHz...
        0xFF, 0xFF, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20, // ...max-H 510 kHz, max clock 2550 MHz (150 was below the driver's own 1080p120 default)
        0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x50, // name descriptor "Punktfunk"
        0x75, 0x6E, 0x6B, 0x74, 0x66, 0x75, 0x6E, 0x6B,
        0x0A, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00, // empty 4th descriptor...
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, // ...byte 126 = 1 extension, byte 127 = checksum
    ];

    /// CTA-861.3 extension block header (block 1, bytes 0..4). What follows is a Data Block
    /// Collection holding the Colorimetry and HDR Static Metadata blocks; the rest of the block is
    /// padding up to the checksum at byte 255.
    #[rustfmt::skip]
    const CTA_HEADER: [u8; 4] = [
        0x02, // CTA Extension tag
        0x03, // revision 3 (CTA-861.3 — required for the extended-tag data blocks below)
        0x0F, // D = 15: the (empty) DTD region starts at block byte 15, i.e. data blocks occupy bytes 4..15
        0x00, // 0 native DTDs; no basic audio; no YCbCr 4:4:4/4:2:2 (RGB-only, matching the wire format)
    ];

    /// Colorimetry Data Block (CTA extended tag 0x05): declare BT.2020 RGB. YCbCr variants stay
    /// clear — the IddCx wire format is RGB-only — and the gamut-metadata flags are 0.
    #[rustfmt::skip]
    const COLORIMETRY_DB: [u8; 4] = [
        0xE3, // tag 0b111 (use-extended-tag) | length 3
        0x05, // extended tag: Colorimetry
        0x80, // BT2020RGB (bit 7); xvYCC/sYCC/opRGB/BT2020 YCC/cYCC all clear
        0x00, // gamut metadata profiles MD0..MD3: none
    ];

    /// HDR Static Metadata Data Block (CTA extended tag 0x06): EOTFs = Traditional SDR (ET_0) plus
    /// SMPTE ST 2084 / PQ (ET_2), Static Metadata Type 1 (SM_0). The desired-content luminance tail
    /// holds the BUILT-IN defaults, used when the host reported no client volume; [`generate`]
    /// overwrites bytes 4..7 with the client display's coded volume otherwise.
    #[rustfmt::skip]
    const HDR_STATIC_METADATA_DB: [u8; 7] = [
        0xE6, // tag 0b111 (use-extended-tag) | length 6
        0x06, // extended tag: HDR Static Metadata
        0x05, // Supported EOTFs: ET_0 (traditional SDR) | ET_2 (SMPTE ST 2084 / PQ)
        0x01, // Supported Static Metadata Descriptors: SM_0 (Static Metadata Type 1)
        0x8A, // Desired Content Max Luminance      (code 138 ≈ 993 nits)
        0x60, // Desired Content Max Frame-avg Lum. (code  96 = 400 nits)
        0x12, // Desired Content Min Luminance      (code  18 ≈ 0.05 nits)
    ];

    /// The client display's luminance volume for the CTA HDR block — the
    /// [`crate::control::AddRequest`] luminance tail, same units. `max_nits == 0` means unknown (an
    /// SDR client, or an un-upgraded host whose short ADD zero-fills the tail) and keeps the
    /// built-in defaults.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct ClientLuminance {
        /// Peak luminance, nits. `0` = unknown → keep the built-in default block.
        pub max_nits: u32,
        /// Max frame-average luminance, nits. `0` = unknown ("no data" on the wire).
        pub max_frame_avg_nits: u32,
        /// Min luminance, milli-nits. `0` = unknown/true black ("no data" on the wire).
        pub min_millinits: u32,
    }

    /// EDID screen-size bytes 0x15/0x16 hold the image size in CENTIMETRES, and Windows derives a
    /// display's DPI from resolution over that size. A FIXED size therefore makes the virtual
    /// display's scale ride its mode: the 50 cm this EDID used to declare reads as ~97 DPI at
    /// 1080p but ~260 DPI at 5120 px wide, so the OS quite correctly scales the desktop up and
    /// hands out a cursor to match. Sizing from the mode pins the display near 96 DPI, which
    /// leaves scaling where it belongs — the client's own choice, not an artefact of our EDID.
    ///
    /// One byte each, so `1..=255`: 0 means "undefined" (projectors) and would put the DPI
    /// decision back with the OS.
    fn size_cm(px: u32) -> u8 {
        (u64::from(px) * 254 / 9600).clamp(1, 255) as u8
    }

    /// Base-block offsets of the horizontal and vertical image size.
    const H_SIZE_CM_OFFSET: usize = 0x15;
    const V_SIZE_CM_OFFSET: usize = 0x16;

    /// Build the 256-byte EDID for the monitor identified by `serial`, with both block checksums
    /// recomputed — the serial patch at 0x0C and the CTA edits below both invalidate them.
    ///
    /// `lum` is the CLIENT display's luminance volume, coded into the HDR block's desired-content
    /// bytes so apps tone-map to the panel the stream lands on; all-zero keeps the built-in
    /// ~993-nit defaults. `preferred` is the session's `(width, height, refresh)`: it replaces the
    /// hard-coded 1080p60 preferred-timing DTD when it fits the encoding (4K120-class does not).
    /// The modes the OS OFFERS still come from the IddCx mode list, not this descriptor.
    #[must_use]
    pub fn generate(
        serial: u32,
        lum: ClientLuminance,
        preferred: Option<(u32, u32, u32)>,
    ) -> [u8; 256] {
        let mut edid = [0u8; 256];
        edid[..128].copy_from_slice(&BASE);
        edid[SERIAL_OFFSET..SERIAL_OFFSET + 4].copy_from_slice(&serial.to_le_bytes());
        if let Some(d) = preferred.and_then(|(w, h, r)| dtd(w, h, r)) {
            edid[54..72].copy_from_slice(&d);
        }
        // Declare a size that puts this mode near 96 DPI, or the OS scales the desktop for a
        // panel we only claimed to be.
        if let Some((w, h, _)) = preferred {
            edid[H_SIZE_CM_OFFSET] = size_cm(w);
            edid[V_SIZE_CM_OFFSET] = size_cm(h);
        }
        edid[128..132].copy_from_slice(&CTA_HEADER);
        edid[132..136].copy_from_slice(&COLORIMETRY_DB);
        let mut hdr_db = HDR_STATIC_METADATA_DB;
        if lum.max_nits > 0 {
            let max_code = cta_max_luminance_code(lum.max_nits);
            hdr_db[4] = max_code;
            hdr_db[5] = if lum.max_frame_avg_nits > 0 {
                cta_max_luminance_code(lum.max_frame_avg_nits)
            } else {
                0 // "no data" — valid per CTA-861.3
            };
            hdr_db[6] = cta_min_luminance_code(lum.min_millinits, max_code);
        }
        edid[136..143].copy_from_slice(&hdr_db);
        fix_block_checksum(&mut edid, 0);
        fix_block_checksum(&mut edid, 128);
        edid
    }

    /// Read the per-monitor serial (base offset 0x0C, little-endian) out of an EDID the OS handed
    /// back, so a monitor-description callback can find the monitor it belongs to. Takes the full
    /// 256-byte EDID or just the 128-byte base block, and errors rather than panics on a short
    /// buffer so the caller can reject a malformed descriptor.
    pub fn get_serial(edid: &[u8]) -> Result<u32, core::array::TryFromSliceError> {
        let bytes: [u8; 4] = edid
            .get(SERIAL_OFFSET..SERIAL_OFFSET + 4)
            .unwrap_or(&[])
            .try_into()?;
        Ok(u32::from_le_bytes(bytes))
    }

    /// Set the trailing byte of the 128-byte block at `start` so the block's bytes sum to 0
    /// (mod 256) — the standard EDID block checksum, without which a parser rejects the block.
    fn fix_block_checksum(edid: &mut [u8], start: usize) {
        let sum = edid[start..start + 127]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_add(b));
        edid[start + 127] = 0u8.wrapping_sub(sum);
    }
}

/// Virtual-monitor mode lists and OS identity for the `pf-vdisplay` driver: what a monitor
/// advertises, which id names it, and the timing numbers behind one `(width, height, refresh)`.
///
/// Lives here, not in the driver: the driver only builds under the WDK, so this arithmetic had
/// no test on any machine. Integer-only and free of OS types — the driver stamps the results
/// into its `wdk_sys` / `iddcx` structs and keeps nothing else.
pub mod vdisplay {
    use alloc::vec;
    use alloc::vec::Vec;

    /// One resolution with the refresh rates it supports.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Mode {
        pub width: u32,
        pub height: u32,
        pub refresh_rates: Vec<u32>,
    }

    /// A single `(width, height, refresh)` tuple — modes flattened across their refresh rates.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct ModeItem {
        pub width: u32,
        pub height: u32,
        pub refresh_rate: u32,
    }

    /// Flatten a mode list into per-refresh-rate tuples (the order the mode DDIs emit).
    pub fn flatten(modes: &[Mode]) -> impl Iterator<Item = ModeItem> + '_ {
        modes.iter().flat_map(|m| {
            m.refresh_rates.iter().map(|&rr| ModeItem {
                width: m.width,
                height: m.height,
                refresh_rate: rr,
            })
        })
    }

    /// How many distinct resolutions a monitor's advertised list may accumulate (the requested
    /// head + history + the built-in fallbacks). Bounds the union growth across many resizes.
    pub const MODE_LIST_CAP: usize = 12;

    /// Append `from`'s modes to `into`, skipping resolutions already there, capped at
    /// [`MODE_LIST_CAP`] — the accumulate half of the driver's mode-union semantics. The OS pins
    /// a monitor's settable set at arrival, so a list may only ever grow.
    ///
    /// Dedupe is by `(width, height)` alone: a duplicate resolution is dropped WHOLE, refresh
    /// rates and all, never merged into the entry already present. The cap is checked before
    /// each candidate, so reaching it stops the merge rather than skipping one entry.
    pub fn union_modes(into: &mut Vec<Mode>, from: &[Mode]) {
        for m in from {
            if into.len() >= MODE_LIST_CAP {
                break;
            }
            if !into
                .iter()
                .any(|e| (e.width, e.height) == (m.width, m.height))
            {
                into.push(m.clone());
            }
        }
    }

    /// The list a monitor advertises: the requested mode first, then — for a host only — the
    /// fallbacks and whatever the monitor already offered.
    ///
    /// A seat rides a remote-session adapter, which IddCx obliges to declare `USE_SMALLEST_MODE`,
    /// so the OS drives the monitor at the SMALLEST mode on the list. A seat therefore offers
    /// exactly what the client asked for: one fallback, or one stale larger entry surviving a
    /// resize, pins that seat to the wrong resolution.
    ///
    /// `history` is the monitor's current list on a re-advertise, empty at create. The OS pins the
    /// settable set at arrival, so a host's list may only grow — [`union_modes`] caps that growth.
    #[must_use]
    pub fn advertised_modes(requested: Mode, seat: bool, history: &[Mode]) -> Vec<Mode> {
        let mut modes = vec![requested];
        if !seat {
            modes.extend(default_modes());
        }
        accumulate_modes(&mut modes, seat, history);
        modes
    }

    /// Merge `history` into `into` — the accumulate half of [`advertised_modes`], for the caller
    /// that only learns the history later (the registry resolves the monitor id under its lock).
    ///
    /// A seat never accumulates: every carried-over mode is one the OS can pick INSTEAD of the
    /// size the client asked for.
    pub fn accumulate_modes(into: &mut Vec<Mode>, seat: bool, history: &[Mode]) {
        if !seat {
            union_modes(into, history);
        }
    }

    /// Fallback modes appended after the requested mode, so a topology change still has options.
    #[must_use]
    pub fn default_modes() -> Vec<Mode> {
        vec![
            Mode {
                width: 1920,
                height: 1080,
                refresh_rates: vec![60, 120],
            },
            Mode {
                width: 1280,
                height: 720,
                refresh_rates: vec![60],
            },
        ]
    }

    /// The numbers of a `DISPLAYCONFIG_VIDEO_SIGNAL_INFO`, without the OS struct. Both sync
    /// rationals have denominator 1, and total size equals active size (no fabricated blanking),
    /// so the driver stamps one `(width, height)` region into both size fields.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SignalInfo {
        /// Pixels per second, `refresh · width · height`. u64 because 8K240 alone is 7.96e9.
        pub pixel_rate: u64,
        /// `hSyncFreq` numerator, `refresh · height`, saturated into u32.
        pub h_sync_num: u32,
        /// `vSyncFreq` numerator — the refresh rate itself.
        pub v_sync_num: u32,
        /// The `AdditionalSignalInfo` union read as `videoStandard`: 255 (other), with the
        /// vSync frequency divider in bits 16..21.
        pub video_standard: u32,
    }

    /// THE signal description for both mode DDI families (IddSampleDriver-exact): pixel rate =
    /// `refresh · width · height`, integer sync rationals, total == active. Monitor (description)
    /// and target (scan-out) modes differ ONLY in `v_sync_freq_divider`, which the caller passes
    /// (0 / 1 per the DDI contract).
    ///
    /// Until 2026-07 the monitor side used the virtual-display-rs legacy math instead — a
    /// WIDTH-LESS pixel rate (`rr·(h+4)²+1000`) and a deliberately fractional vSync — so the OS
    /// saw two disagreeing descriptions of the same tuple, one physically meaningless.
    ///
    /// The hSync numerator is computed in u64 and saturated: an unchecked `refresh · height`
    /// past u32 would panic → abort the extern-"C" mode DDI in a debug build.
    #[must_use]
    pub fn signal_info(
        width: u32,
        height: u32,
        refresh_rate: u32,
        v_sync_freq_divider: u32,
    ) -> SignalInfo {
        SignalInfo {
            pixel_rate: u64::from(refresh_rate) * u64::from(width) * u64::from(height),
            h_sync_num: u32::try_from(u64::from(refresh_rate) * u64::from(height))
                .unwrap_or(u32::MAX),
            v_sync_num: refresh_rate,
            video_standard: 255 | (v_sync_freq_divider << 16),
        }
    }

    /// Resolve the id to name a new monitor by, given the ids currently `live`: honour the host's
    /// per-client `preferred` id when it is in `1..=15` (so the IddCx `ConnectorIndex` = id stays
    /// below `MaxMonitorsSupported` = 16) AND not live, so a client keeps a STABLE identity across
    /// reconnects and Windows reapplies its saved per-monitor DPI scaling.
    ///
    /// Otherwise fall back to [`alloc_monitor_id`]. A collision NEVER departs the live holder —
    /// that would tear down an unrelated client — so live ids stay distinct even against a host
    /// bug. `preferred == 0` (anonymous / TOFU / GameStream) always falls through to auto.
    #[must_use]
    pub fn resolve_id(live: &[u32], preferred: u32) -> u32 {
        if (1..=15).contains(&preferred) && !live.contains(&preferred) {
            preferred
        } else {
            alloc_monitor_id(live)
        }
    }

    /// The lowest id ≥ 1 not in `live`. Reusing freed ids (rather than a monotonic counter) keeps
    /// the connector index / EDID serial / container GUID bounded by the number of CONCURRENT
    /// monitors, so a fresh ADD reuses a departed monitor's OS target slot instead of orphaning it
    /// — the ghost accumulation that wedges ADD at 0x80070490.
    ///
    /// The search spans `1..=live.len() + 1`, where pigeonhole guarantees a free id. That bound
    /// is NOT clamped to 15: with 15 live ids the result is 16, one past the connector range
    /// [`resolve_id`] enforces for a preferred id.
    #[must_use]
    pub fn alloc_monitor_id(live: &[u32]) -> u32 {
        (1u32..=live.len() as u32 + 1)
            .find(|id| !live.contains(id))
            .unwrap_or(1)
    }

    /// A deterministic, monitor-unique container GUID (which groups targets into one physical
    /// device), derived from `id` so it is stable and collision-free without a random source.
    /// Returned as `(Data1, Data2, Data3, Data4)` like [`crate::interface_guid_fields`] — this
    /// crate is `no_std` and has no `GUID` type.
    #[must_use]
    pub const fn container_guid(id: u32) -> (u32, u16, u16, [u8; 8]) {
        (
            0x7066_7664u32.wrapping_add(id),
            0x7044,
            0x5350,
            [
                0xa1,
                0xb2,
                0xc3,
                0xd4,
                0xe5,
                0xf6,
                (id >> 8) as u8,
                id as u8,
            ],
        )
    }

    /// Sanity bounds for a mode the host requests over ADD / UPDATE_MODES — generous (any real
    /// client fits) but rejecting the zero and absurd values that would otherwise reach the EDID
    /// and [`signal_info`] math unchecked.
    #[must_use]
    pub fn valid_mode(width: u32, height: u32, refresh_hz: u32) -> bool {
        (1..=16384).contains(&width)
            && (1..=16384).contains(&height)
            && (1..=1000).contains(&refresh_hz)
    }
}

/// Protocol v7: the driver encodes, and the host reads access units instead of pixels.
///
/// It replaced the pixel ring rather than joining it — one transport, as
/// `design/windows-video-plane-overhaul.md` §1.1 decided. The host still owns the memory: it
/// creates an unnamed section plus a ready event, duplicates both into WUDFHost and delivers the
/// values over [`IOCTL_SET_ENCODE`], which also carries codec, mode, bitrate, HDR metadata and an
/// ordered backend preference list. The driver opens the first backend that works and answers with
/// [`SetEncodeReply`] — the backend that took, its [`EncoderCapsWire`] and the applied bitrate, or
/// a named failure. No silent fallback.
///
/// Steady state lives in [`au`]: a 128-byte header, a 16-entry slot table and a bitstream heap the
/// encode thread writes into. Publishing goes through [`FrameToken`], so the host takes a slot only
/// under a generation check. Runtime control — keyframe, RFI, bitrate, HDR metadata, reset, flush —
/// travels as one-shot [`IOCTL_ENCODE_CTL`] calls on the framework queue that already carries
/// `PING`.
pub mod encode {
    use super::ctl_code;
    use bytemuck::{Pod, Zeroable};

    /// Encoder backend ids, as they travel in [`SetEncodeRequest::backends`],
    /// [`SetEncodeReply::backend_opened`] and
    /// [`EncodeProbeRequest::backend`](crate::control::EncodeProbeRequest::backend).
    ///
    /// The host picks by these numbers, the driver opens by them and names them back, so both
    /// sides read the table here rather than restating it. A doc that restated it had already
    /// drifted: Media Foundation was missing from two of them while the host was sending it.
    pub mod backend {
        pub const NVENC: u32 = 1;
        pub const AMF: u32 = 2;
        pub const QSV: u32 = 3;
        pub const PYROWAVE: u32 = 4;
        pub const MEDIA_FOUNDATION: u32 = 5;

        /// Indexed by `id - 1`; also the stage tag a `SET_ENCODE` reply carries.
        pub const NAMES: [&str; 5] = ["nvenc", "amf", "qsv", "pyrowave", "mf"];

        #[must_use]
        pub fn name(id: u32) -> Option<&'static str> {
            NAMES.get(id.checked_sub(1)? as usize).copied()
        }

        /// Whether `id` may appear in [`SetEncodeRequest::backends`]. `0` terminates the list, so
        /// it passes here and is simply never opened.
        #[must_use]
        pub const fn listed(id: u32) -> bool {
            (id as usize) <= NAMES.len()
        }
    }

    /// Codec ids for [`SetEncodeRequest::codec`] and
    /// [`EncodeProbeRequest::codec`](crate::control::EncodeProbeRequest::codec). PyroWave is a
    /// codec AND a backend, and only ever pairs with itself.
    pub mod codec {
        pub const H264: u32 = 1;
        pub const HEVC: u32 = 2;
        pub const AV1: u32 = 3;
        pub const PYROWAVE: u32 = 4;

        pub const NAMES: [&str; 4] = ["h264", "hevc", "av1", "pyrowave"];

        #[must_use]
        pub fn name(id: u32) -> Option<&'static str> {
            NAMES.get(id.checked_sub(1)? as usize).copied()
        }

        /// Whether `id` names a codec. Unlike a backend list, `0` is not legal here.
        #[must_use]
        pub const fn valid(id: u32) -> bool {
            id != 0 && (id as usize) <= NAMES.len()
        }
    }

    /// The publish cell of [`au::AuHeader::latest`]: `(generation << 40) | (seq << 8) | slot`,
    /// with `generation` 24-bit, `seq` 32-bit and `slot` 8-bit. `generation` is bumped on every
    /// [`IOCTL_SET_ENCODE`], so a publish an old encoder left behind is rejected, never consumed.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct FrameToken {
        pub generation: u32,
        pub seq: u32,
        pub slot: u8,
    }

    impl FrameToken {
        /// Low 24 bits of `generation` are significant.
        pub const GENERATION_MASK: u32 = 0x00FF_FFFF;

        #[must_use]
        pub const fn pack(self) -> u64 {
            (((self.generation & Self::GENERATION_MASK) as u64) << 40)
                | (((self.seq as u64) & 0xFFFF_FFFF) << 8)
                | (self.slot as u64)
        }

        #[must_use]
        pub const fn unpack(v: u64) -> Self {
            Self {
                generation: ((v >> 40) as u32) & Self::GENERATION_MASK,
                seq: ((v >> 8) & 0xFFFF_FFFF) as u32,
                slot: (v & 0xFF) as u8,
            }
        }
    }

    /// [`au::AuHeader::driver_status`] values. UMDF hides `OutputDebugString` and the restricted
    /// token blocks file writes, so this word is how a driver with no debugger reports state.
    pub const DRV_STATUS_NONE: u32 = 0;
    /// An encoder is open on this section.
    pub const DRV_STATUS_OPENED: u32 = 1;

    /// Open the encoder for one monitor and adopt its AU section + ready event. Input
    /// [`SetEncodeRequest`], output [`SetEncodeReply`]. A resolution, codec or HDR change is a new
    /// SET_ENCODE — the same tear-down-and-rebuild the host does today.
    pub const IOCTL_SET_ENCODE: u32 = ctl_code(0x90C);
    /// One-shot control on the live encoder. Input [`EncodeCtlRequest`], no output.
    pub const IOCTL_ENCODE_CTL: u32 = ctl_code(0x90D);

    /// [`IOCTL_SET_ENCODE`] input. `section` and `event` are handle VALUES already duplicated into
    /// WUDFHost, adopt-on-success-only exactly as
    /// [`SetFrameChannelRequest`](crate::control::SetFrameChannelRequest): the driver owns and
    /// closes them IFF the IOCTL succeeds, and the host reaps with `DUPLICATE_CLOSE_SOURCE` on any
    /// error. Closing on error double-closes a possibly-reused handle value.
    ///
    /// Everything the backend needs to open travels in one request, so opening is not a
    /// negotiation: the driver walks `backends` in order and reports what took.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct SetEncodeRequest {
        /// OS target id of the monitor to encode (the [`AddReply`](crate::control::AddReply) one).
        pub target_id: u32,
        /// Aligns `section` to 8 (Pod forbids implicit padding).
        pub _pad: u32,
        /// AU section mapping handle VALUE; the driver maps it and writes [`au::AuHeader`].
        pub section: u64,
        /// Event handle VALUE the driver signals after each publish.
        pub event: u64,
        /// Bytes the host allocated for the section — [`au::section_bytes`].
        pub section_bytes: u32,
        /// A [`codec`] id, the same numbering
        /// [`EncodeProbeRequest`](crate::control::EncodeProbeRequest) uses.
        pub codec: u32,
        /// `0` = 4:2:0, `1` = 4:4:4. Not the H.264/HEVC `chroma_format_idc`.
        pub chroma: u32,
        /// Bits per component the encoder emits: 8 or 10.
        pub bit_depth: u32,
        pub width: u32,
        pub height: u32,
        /// Target frame rate; with `bitrate_kbps` it sizes the heap ([`au::heap_bytes_for`]).
        pub fps: u32,
        pub bitrate_kbps: u32,
        /// `1` = PQ BT.2020 stream and `hdr_meta` is valid.
        pub hdr: u32,
        /// `pf_frame::HdrMeta` as its 28 raw bytes — opaque here, this crate has no HDR types.
        pub hdr_meta: [u8; 28],
        /// Slice-chunk target for `Encoder::set_wire_chunking`; `0` = publish whole AUs.
        pub wire_chunk_bytes: u32,
        /// First `wire_seq` the driver stamps, so the host's `au_seq` domain survives a DriverCycle.
        pub wire_seq_base: u32,
        /// Ordered preference, 0-terminated. [`backend`] ids.
        pub backends: [u32; 4],
        /// Reserved; send `0`.
        pub flags: u32,
        /// Pads the prefix to its 8-byte alignment (Pod forbids implicit tail padding).
        pub _pad_tail: u32,
        /// Encoder knobs from the host's `host.env`. Prefix-compatible after
        /// [`SET_ENCODE_REQUEST_LEGACY_SIZE`]: an old host sends none and the driver zero-fills,
        /// which is every backend's default; an old driver reads the prefix and ignores them.
        pub knobs: EncodeKnobs,
    }

    /// Bytes of [`SetEncodeRequest`] before [`SetEncodeRequest::knobs`]: what a host older than
    /// the knobs sends, and what a driver older than them reads.
    pub const SET_ENCODE_REQUEST_LEGACY_SIZE: usize = 120;

    /// Encoder tuning the host resolves once from `host.env` and hands to the driver in the
    /// request, so a knob takes effect on the next session instead of after `setx /M` plus a
    /// driver restart. Zero is every backend's own default. Each field names the environment
    /// variable it replaces; [`EncodeKnobs::apply_env`] is the one parser for those, and the
    /// encoder crate still reads the same variables inside WUDFHost as a dev override.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, Default, PartialEq, Eq)]
    pub struct EncodeKnobs {
        /// `PUNKTFUNK_IR_PERIOD_FRAMES`: intra-refresh wave length in frames, `>= 2`; `0` = half
        /// a second of frames.
        pub ir_period_frames: u16,
        /// `PUNKTFUNK_LTR_INTERVAL_FRAMES`: frames between LTR marks; `0` = the backend's tuning.
        pub ltr_interval_frames: u16,
        /// `PUNKTFUNK_LTR_FORCE_AT`: spike-only self-triggered RFI at this frame; `0` = off.
        pub ltr_force_at: u16,
        /// `PUNKTFUNK_SPLIT_ENCODE`: `0` = by pixel rate, `1` = disable, `2` = auto-forced,
        /// `3` = two engines, `4` = three engines.
        pub split_encode: u8,
        /// `PUNKTFUNK_NVENC_ASYNC`: `1` = the two-thread retrieve.
        pub nvenc_async: u8,
        /// `PUNKTFUNK_NVENC_ASYNC_DEPTH`: in-flight encodes in async mode; `0` = 4.
        pub nvenc_async_depth: u8,
        /// `PUNKTFUNK_NVENC_SLICES`: H.264/HEVC slices, `1..=32`; `0` = the session's default.
        pub nvenc_slices: u8,
        /// `PUNKTFUNK_NVENC_SUBFRAME`: `0` = the GPU's cap decides, `1` = never, `2` = force.
        pub nvenc_subframe: u8,
        /// `PUNKTFUNK_NVENC_MAX_SESSIONS`: concurrent-session budget; `0` = 8.
        pub nvenc_max_sessions: u8,
        /// `PUNKTFUNK_NVENC_SPLIT_ARBITRATE`: `1` = arm the live split experiment.
        pub nvenc_split_arbitrate: u8,
        /// `PUNKTFUNK_INTRA_REFRESH`: `0` = the on-demand wave only, `1` = the periodic wave on
        /// AMF/QSV too, `2` = no wave, IDR on every loss.
        pub intra_refresh: u8,
        /// `PUNKTFUNK_AMF_USAGE`: `0` = ultralowlatency, `1` = lowlatency,
        /// `2` = lowlatency_high_quality, `3` = transcoding, `4` = highquality.
        pub amf_usage: u8,
        /// `PUNKTFUNK_NO_AMF_LTR`: `1` = IDR-only loss recovery on AMF.
        pub no_amf_ltr: u8,
        /// `PUNKTFUNK_NO_QSV_LTR`: `1` = IDR-only loss recovery on QSV.
        pub no_qsv_ltr: u8,
        /// `PUNKTFUNK_VBV_FRAMES` in tenths of a frame interval; `0` = 10 (one frame).
        pub vbv_tenths: u8,
        /// `PUNKTFUNK_PYROWAVE_STREAMED_AU`: `1` = arm streamed PyroWave AUs.
        pub pyrowave_streamed_au: u8,
        /// `PUNKTFUNK_PYROWAVE_CHUNK_KIB` / 64: streamed-AU chunk target; `0` = 256 KiB.
        pub pyrowave_chunk_64kib: u8,
        /// Reserved; send `0`.
        pub _reserved: [u8; 4],
    }

    /// The trimmed, case-folded truthy set every `PUNKTFUNK_*` flag shares.
    #[must_use]
    pub fn truthy(v: &str) -> bool {
        let v = v.trim();
        v == "1"
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes")
            || v.eq_ignore_ascii_case("on")
    }

    impl EncodeKnobs {
        /// Every variable [`Self::apply_env`] knows, for a caller that walks the environment.
        pub const ENV_NAMES: [&'static str; 17] = [
            "PUNKTFUNK_SPLIT_ENCODE",
            "PUNKTFUNK_NVENC_ASYNC",
            "PUNKTFUNK_NVENC_ASYNC_DEPTH",
            "PUNKTFUNK_NVENC_SLICES",
            "PUNKTFUNK_NVENC_SUBFRAME",
            "PUNKTFUNK_NVENC_MAX_SESSIONS",
            "PUNKTFUNK_NVENC_SPLIT_ARBITRATE",
            "PUNKTFUNK_INTRA_REFRESH",
            "PUNKTFUNK_IR_PERIOD_FRAMES",
            "PUNKTFUNK_LTR_INTERVAL_FRAMES",
            "PUNKTFUNK_LTR_FORCE_AT",
            "PUNKTFUNK_AMF_USAGE",
            "PUNKTFUNK_NO_AMF_LTR",
            "PUNKTFUNK_NO_QSV_LTR",
            "PUNKTFUNK_VBV_FRAMES",
            "PUNKTFUNK_PYROWAVE_STREAMED_AU",
            "PUNKTFUNK_PYROWAVE_CHUNK_KIB",
        ];

        /// Set the knob `name` names from its environment spelling. Unparseable values leave
        /// the field alone, as the encoders always did. `false` = not a knob name.
        pub fn apply_env(&mut self, name: &str, value: &str) -> bool {
            let v = value.trim();
            let num = |lo: u32, hi: u32| v.parse::<u32>().ok().filter(|n| (lo..=hi).contains(n));
            match name {
                "PUNKTFUNK_SPLIT_ENCODE" => {
                    self.split_encode = match v {
                        "0" | "disable" => 1,
                        "1" | "auto" => 2,
                        "2" => 3,
                        "3" => 4,
                        _ => self.split_encode,
                    }
                }
                "PUNKTFUNK_NVENC_ASYNC" => self.nvenc_async = truthy(v) as u8,
                "PUNKTFUNK_NVENC_ASYNC_DEPTH" => {
                    if let Some(n) = num(1, 255) {
                        self.nvenc_async_depth = n as u8;
                    }
                }
                "PUNKTFUNK_NVENC_SLICES" => {
                    if let Some(n) = num(1, 32) {
                        self.nvenc_slices = n as u8;
                    }
                }
                "PUNKTFUNK_NVENC_SUBFRAME" => {
                    self.nvenc_subframe = match v {
                        "0" => 1,
                        "1" => 2,
                        _ => self.nvenc_subframe,
                    }
                }
                "PUNKTFUNK_NVENC_MAX_SESSIONS" => {
                    if let Some(n) = num(1, 255) {
                        self.nvenc_max_sessions = n as u8;
                    }
                }
                "PUNKTFUNK_NVENC_SPLIT_ARBITRATE" => self.nvenc_split_arbitrate = (v == "1") as u8,
                "PUNKTFUNK_INTRA_REFRESH" => {
                    self.intra_refresh = if v == "0" {
                        2
                    } else if truthy(v) {
                        1
                    } else {
                        0
                    }
                }
                "PUNKTFUNK_IR_PERIOD_FRAMES" => {
                    if let Some(n) = num(2, u16::MAX as u32) {
                        self.ir_period_frames = n as u16;
                    }
                }
                "PUNKTFUNK_LTR_INTERVAL_FRAMES" => {
                    if let Some(n) = num(1, u16::MAX as u32) {
                        self.ltr_interval_frames = n as u16;
                    }
                }
                "PUNKTFUNK_LTR_FORCE_AT" => {
                    if let Some(n) = num(1, u16::MAX as u32) {
                        self.ltr_force_at = n as u16;
                    }
                }
                "PUNKTFUNK_AMF_USAGE" => {
                    self.amf_usage = match v {
                        "ultralowlatency" => 0,
                        "lowlatency" => 1,
                        "lowlatency_high_quality" => 2,
                        "transcoding" => 3,
                        "highquality" | "high_quality" => 4,
                        _ => self.amf_usage,
                    }
                }
                "PUNKTFUNK_NO_AMF_LTR" => self.no_amf_ltr = truthy(v) as u8,
                "PUNKTFUNK_NO_QSV_LTR" => self.no_qsv_ltr = truthy(v) as u8,
                "PUNKTFUNK_VBV_FRAMES" => {
                    // Tenths, so `1.5` survives the byte; `0` and garbage keep the default.
                    if let Some(t) = parse_tenths(v).filter(|t| (1..=255).contains(t)) {
                        self.vbv_tenths = t as u8;
                    }
                }
                "PUNKTFUNK_PYROWAVE_STREAMED_AU" => self.pyrowave_streamed_au = (v == "1") as u8,
                "PUNKTFUNK_PYROWAVE_CHUNK_KIB" => {
                    // 4..=8192 KiB in the encoder; the byte carries 64 KiB steps, so the floor
                    // rounds up to one step.
                    if let Some(k) = num(4, 8192) {
                        self.pyrowave_chunk_64kib = k.div_ceil(64) as u8;
                    }
                }
                _ => return false,
            }
            true
        }

        /// `PUNKTFUNK_VBV_FRAMES` as the encoders read it: frame intervals, default one.
        #[must_use]
        pub fn vbv_frames(&self) -> f64 {
            if self.vbv_tenths == 0 {
                1.0
            } else {
                f64::from(self.vbv_tenths) / 10.0
            }
        }
    }

    /// `"1.5"` → `15`; integers and one decimal only, no exponent. Negative or empty = `None`.
    fn parse_tenths(v: &str) -> Option<u32> {
        let (whole, frac) = match v.split_once('.') {
            Some((w, f)) => (w, f),
            None => (v, ""),
        };
        let whole: u32 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        let tenth: u32 = match frac.as_bytes().first() {
            None => 0,
            Some(d) if d.is_ascii_digit() => u32::from(d - b'0'),
            Some(_) => return None,
        };
        if !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(whole.checked_mul(10)? + tenth)
    }

    /// The encoder input one [`SetEncodeRequest`] resolves to, so the chroma the reply promises
    /// and the pixels the backend is handed come from the same decision. The driver owns the
    /// D3D targets behind each variant; this crate owns only the choice and what it can carry.
    ///
    /// [`Self::full_chroma`] is the honest ceiling for [`EncoderCapsWire::chroma_444`]: a
    /// subsampled input cannot become 4:4:4 downstream, whatever the request asked for.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum EncodeInput {
        /// BGRA straight into the backend, which does the RGB→YUV CSC.
        Bgra,
        /// Video-engine BGRA→NV12, 8-bit 4:2:0.
        Nv12,
        /// Shader FP16 scRGB→P010 PQ, 10-bit 4:2:0.
        P010,
        /// Video-engine BGRA→P010, 10-bit 4:2:0 BT.709 (10-bit SDR): an 8-bit capture widened to a
        /// Main10 stream under BT.709, no HDR volume. AMF only — NVENC widens from `Bgra` itself.
        P010Sdr,
        /// Shader FP16 scRGB→packed `R10G10B10A2` PQ BT.2020; the backend CSCs to 4:4:4 itself.
        Rgb10,
        /// Shareable Y + CbCr planes plus a fence, as PyroWave's own Vulkan device imports them.
        Planar { hdr: bool, chroma444: bool },
    }

    impl EncodeInput {
        /// The input for `backend` (the [`SetEncodeRequest::backends`] numbering) under the
        /// request's HDR, depth and 4:4:4 flags. Only NVENC ingests packed RGB, so only it can
        /// pair HDR with full chroma; AMF and QSV take P010 and encode 4:2:0. `ten_bit` without
        /// `hdr` is 10-bit SDR: NVENC still widens from `Bgra`, AMF takes a BT.709 P010
        /// (`P010Sdr`). Media Foundation takes NV12 whatever was asked for — no vendor's MFT
        /// accepts P010, so an HDR request that reaches it encodes 8-bit rather than failing.
        #[must_use]
        pub const fn choose(backend: u32, hdr: bool, ten_bit: bool, chroma444: bool) -> Self {
            match (backend, hdr, chroma444) {
                (backend::PYROWAVE, _, _) => Self::Planar { hdr, chroma444 },
                (backend::MEDIA_FOUNDATION, _, _) => Self::Nv12,
                (backend::NVENC, true, true) => Self::Rgb10,
                (_, true, _) => Self::P010,
                (backend::NVENC, false, _) => Self::Bgra,
                (backend::AMF, false, _) if ten_bit => Self::P010Sdr,
                _ => Self::Nv12,
            }
        }

        /// Full chroma reaches the backend: packed RGB, or planes built at full resolution.
        #[must_use]
        pub const fn full_chroma(self) -> bool {
            matches!(
                self,
                Self::Bgra
                    | Self::Rgb10
                    | Self::Planar {
                        chroma444: true,
                        ..
                    }
            )
        }
    }
    /// `pf_encode_win::EncoderCaps` as plain integers — this crate cannot depend on the encoder
    /// crate, which builds only in the driver's graph. Each `bool` there is `0`/`1` here; the
    /// driver fills this from `Encoder::caps()` after the open and the host maps it straight back,
    /// so the session routes by query rather than by a `false` default.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct EncoderCapsWire {
        /// `supports_rfi`: `invalidate_ref_frames` can succeed; else the host keyframes on loss.
        pub supports_rfi: u32,
        /// `chroma_444`: the opened encoder emits 4:4:4. Cross-check against the request's `chroma`.
        pub chroma_444: u32,
        /// `intra_refresh`: a moving intra band instead of periodic IDRs.
        pub intra_refresh: u32,
        /// `intra_refresh_recovery`: the wave has a decoder-visible clean point, so freezes lift.
        pub intra_refresh_recovery: u32,
        /// `intra_refresh_period`: wave length in frames; `0` when the wave is off.
        pub intra_refresh_period: u32,
        /// `blends_cursor`: the encoder composited the pointer, so the host must not.
        pub blends_cursor: u32,
    }

    /// [`SetEncodeReply::status`]: an encoder is open.
    pub const SET_ENCODE_OK: u32 = 0;
    /// No arrived monitor has the request's `target_id`.
    pub const SET_ENCODE_NO_MONITOR: u32 = 1;
    /// The section did not pass [`au::au_readable`], or is smaller than its header claims.
    pub const SET_ENCODE_BAD_SECTION: u32 = 2;
    /// The monitor has no render device yet: no swap-chain has been assigned since arrival.
    pub const SET_ENCODE_NO_DEVICE: u32 = 3;
    /// Every backend in the list refused; `error` and `name` are the last one's.
    pub const SET_ENCODE_NO_BACKEND: u32 = 4;
    /// The pool or its converters could not be built on the render device.
    pub const SET_ENCODE_POOL: u32 = 5;
    /// The encode thread could not be started.
    pub const SET_ENCODE_THREAD: u32 = 6;
    /// The open did not finish within the driver's bound; the thread was abandoned.
    pub const SET_ENCODE_TIMEOUT: u32 = 7;

    /// [`IOCTL_SET_ENCODE`] output. `status` is the driver's own failure domain, not an HRESULT:
    /// [`SET_ENCODE_OK`] means an encoder is open, anything else means none is and the session
    /// ends with a structured error — `error` carries the backend's raw code and `name` a short
    /// NUL-padded tag (the driver log has the rest). `backend_opened` names the entry from
    /// [`SetEncodeRequest::backends`] that took, never a guess.
    ///
    /// The IOCTL itself completes successfully whenever the request was well-formed and named a
    /// monitor; from that point the driver owns the two handles and closes them itself when
    /// `status` is non-zero. Only an NTSTATUS failure leaves them for the host to reap.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct SetEncodeReply {
        /// One of the `SET_ENCODE_*` codes.
        pub status: u32,
        /// The [`backend`] id that opened; `0` when `status` is non-zero.
        pub backend_opened: u32,
        /// What the opened backend can actually do.
        pub caps: EncoderCapsWire,
        /// Bitrate the backend accepted — may differ from what was asked.
        pub applied_bitrate_kbps: u32,
        /// Raw backend code for a non-zero `status` (HRESULT, NVENC status, AMF result).
        pub error: i32,
        /// Short NUL-padded tag: the backend name on success, the failing stage otherwise.
        pub name: [u8; 32],
    }

    /// [`EncodeCtlRequest::op`]: force the next submitted frame to an IDR.
    pub const ENCODE_CTL_REQUEST_KEYFRAME: u32 = 1;
    /// Invalidate reference frames `arg0..=arg1` in the host's wire-index domain (RFI).
    pub const ENCODE_CTL_INVALIDATE_REF_FRAMES: u32 = 2;
    /// Distrust every reference; the next AU is a clean recovery anchor.
    pub const ENCODE_CTL_DISTRUST_REFERENCES: u32 = 3;
    /// Reconfigure the bitrate to `arg0` kbps without reopening the backend.
    pub const ENCODE_CTL_RECONFIGURE_BITRATE: u32 = 4;
    /// Replace the HDR mastering metadata from `payload` (28 `pf_frame::HdrMeta` bytes).
    pub const ENCODE_CTL_SET_HDR_META: u32 = 5;
    /// Detach the wedged encode thread, open a fresh encoder, restart `wire_seq` at `arg0`.
    pub const ENCODE_CTL_RESET: u32 = 6;
    /// Push the encoder's in-flight AUs into the section.
    pub const ENCODE_CTL_FLUSH: u32 = 7;
    /// Stop the session whose `generation` is `arg0` — the host's proxy going away. A
    /// generation that is not the live one is a stale proxy and a no-op, so a dropped
    /// predecessor never stops its successor.
    pub const ENCODE_CTL_CLOSE: u32 = 8;

    /// `SET_ENCODE` completed with fewer reply bytes than [`SetEncodeReply`]. The IOCTL itself
    /// succeeded, so the driver adopted the host's handles: the host must not close them too.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct ReplyTooShort {
        pub got: usize,
        pub want: usize,
    }

    impl core::fmt::Display for ReplyTooShort {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(
                f,
                "SET_ENCODE: short reply ({} of {} bytes) — driver predates proto v7",
                self.got, self.want
            )
        }
    }

    impl core::error::Error for ReplyTooShort {}

    /// Which pool slot a keyframe request re-encodes when the desktop composed nothing:
    /// `stash`, the newest slot the encode thread took, but only while `queued` is 0 — a
    /// composed frame already carries the IDR — and the slot sits in `idle`, so no drain pass
    /// can be writing the pixels the encoder is about to read. `None` means do nothing.
    ///
    /// The driver's pool is Windows-only; the rule lives here so it is covered everywhere.
    #[must_use]
    pub fn republish_slot(stash: Option<usize>, queued: usize, idle: &[usize]) -> Option<usize> {
        stash.filter(|s| queued == 0 && idle.contains(s))
    }

    /// Where the drain worker's pass writes. A free slot always wins; with none left the
    /// oldest queued frame is overwritten, so the encoder takes the freshest composed picture
    /// under back-pressure rather than the incoming one being thrown away. `lost` is whether a
    /// consumer was there to miss the recycled frame.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum OfferSlot {
        Free(usize),
        Recycle { slot: usize, lost: bool },
    }

    /// [`OfferSlot`] for a pool with `free` (any free slot) and `oldest_full` (the front of the
    /// queue), `live` while an encode thread is consuming. `None` means no slot at all.
    ///
    /// The driver's pool is Windows-only; the rule lives here so it is covered everywhere.
    #[must_use]
    pub fn offer_slot(
        free: Option<usize>,
        oldest_full: Option<usize>,
        live: bool,
    ) -> Option<OfferSlot> {
        match (free, oldest_full) {
            (Some(slot), _) => Some(OfferSlot::Free(slot)),
            (None, Some(slot)) => Some(OfferSlot::Recycle { slot, lost: live }),
            (None, None) => None,
        }
    }

    /// [`IOCTL_ENCODE_CTL`] input: one op against one monitor's live encoder. Unused `arg*` /
    /// `payload` bytes are zero. The ops are the `Encoder` trait calls the stream loop already
    /// makes locally on Linux, forwarded by a control proxy — so the wire shape is deliberately
    /// flat. A command ring with a doorbell is the upgrade path if measured IOCTL latency hurts
    /// RFI recovery; not before.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct EncodeCtlRequest {
        /// OS target id of the monitor whose encoder this addresses.
        pub target_id: u32,
        /// One `ENCODE_CTL_*` op.
        pub op: u32,
        /// First argument: reference-range start, bitrate kbps, or new `wire_seq_base`.
        pub arg0: u32,
        /// Second argument: reference-range end (inclusive) for
        /// [`ENCODE_CTL_INVALIDATE_REF_FRAMES`].
        pub arg1: u32,
        /// [`ENCODE_CTL_SET_HDR_META`] payload: 28 `pf_frame::HdrMeta` bytes.
        pub payload: [u8; 28],
    }

    /// The AU section: header, a fixed slot table, and a bitstream heap, in one host-created
    /// mapping the driver writes and the host reads.
    ///
    /// The encode thread is the only writer. It copies an access unit — or one slice chunk of one,
    /// when the host asked for wire chunking — into the heap, fills a [`FREE`] slot, publishes by
    /// storing a packed [`FrameToken`] into [`AuHeader::latest`], and signals the ready event. The
    /// host loads `latest`, checks the generation, takes the slot and releases it back to `FREE`.
    /// Sixteen slots is where back-pressure lands: with none free the encode thread skips the next
    /// pool slot, so the drop falls on pixels, which are free to drop, and never on an access unit,
    /// which is not.
    ///
    /// The host sizes the heap from the session's peak bitrate ([`heap_bytes_for`]) and the section
    /// from the heap ([`section_bytes`]), both before
    /// [`IOCTL_SET_ENCODE`](super::IOCTL_SET_ENCODE). A reader trusts no field here until
    /// [`au_readable`] passes.
    pub mod au {
        use bytemuck::{Pod, Zeroable};

        /// Header magic (`"PFAU"` LE), stamped by the host before the handle is delivered.
        pub const AU_MAGIC: u32 = 0x5541_4650;
        /// AU-section layout version; moved with `PROTOCOL_VERSION` v7 and v9.
        pub const AU_VERSION: u32 = 8;
        /// Slots in the table. Fixed: the host allocates exactly this many.
        pub const AU_SLOTS: u32 = 16;
        /// [`AuHeader`] size, and therefore where the slot table starts.
        pub const AU_HEADER_SIZE: usize = 128;
        /// [`AuSlot`] size.
        pub const AU_SLOT_SIZE: usize = 48;
        /// Byte offset of the slot table inside the section.
        pub const SLOT_TABLE_OFFSET: usize = AU_HEADER_SIZE;
        /// Byte offset of the heap: past the header and the whole table, still 64-byte aligned so
        /// heap writes never share a cache line with a slot record the host is polling.
        pub const HEAP_OFFSET: usize = SLOT_TABLE_OFFSET + AU_SLOTS as usize * AU_SLOT_SIZE;

        /// Heap sizes round up to this.
        pub const HEAP_GRANULE: u32 = 64 * 1024;
        /// Heap floor: 16 slots need room for 16 access units whatever the bitrate implies.
        pub const HEAP_MIN_BYTES: u32 = 1024 * 1024;
        /// Heap cap. Past this the session is misconfigured, not bandwidth-hungry.
        pub const HEAP_MAX_BYTES: u32 = 64 * 1024 * 1024;
        /// Section page alignment ([`section_bytes`]).
        pub const SECTION_ALIGN: u32 = 4096;

        /// [`AuHeader::encoder_state`]: no encoder — before the first SET_ENCODE, or between
        /// sessions. The pool keeps its stashed slot; nothing is published.
        pub const ENCODER_CLOSED: u32 = 0;
        /// A backend is open and the encode thread is waiting on pool slots.
        pub const ENCODER_OPEN: u32 = 1;
        /// Access units are flowing.
        pub const ENCODER_ENCODING: u32 = 2;
        /// A backend call has not returned. `source_seq` advances, `last_au_qpc` does not; the host
        /// answers with [`ENCODE_CTL_RESET`](super::ENCODE_CTL_RESET) and `detached` gains one.
        pub const ENCODER_WEDGED: u32 = 3;

        /// [`AuSlot::flags`], mirroring `pf_encode_win::AuChunk`: opens an access unit. AU metadata
        /// is authoritative on this slot — the host opens the wire frame from it.
        pub const AU_FIRST: u32 = 1 << 0;
        /// Closes the access unit and releases the encoder's in-flight slot.
        pub const AU_LAST: u32 = 1 << 1;
        /// IDR; sets the client's SOF/keyframe wire flags.
        pub const AU_KEYFRAME: u32 = 1 << 2;
        /// A clean picture after RFI — the client lifts its freeze here without waiting for an IDR.
        pub const AU_RECOVERY_ANCHOR: u32 = 1 << 3;
        /// The AU's chunks are cut on the codec's own window boundaries (PyroWave); the host
        /// forwards it as the wire's chunk-aligned user flag.
        pub const AU_CHUNK_ALIGNED: u32 = 1 << 4;
        /// Start or close of an encoder-driven intra refresh wave; the host forwards it as the
        /// wire's recovery-point user flag. A driver that never waves leaves it clear.
        pub const AU_RECOVERY_POINT: u32 = 1 << 5;
        /// The wave's close, beside [`AU_RECOVERY_POINT`]; the host forwards it as the wire's
        /// recovery-close user flag.
        pub const AU_RECOVERY_CLOSE: u32 = 1 << 6;

        /// [`AuSlot::state`]: the encode thread may take this slot. There is no WRITING state —
        /// the encode thread fills the heap before it claims a slot.
        pub const FREE: u32 = 0;
        /// Written and published; the host may take it.
        pub const PUBLISHED: u32 = 1;
        /// The host is copying the bytes out; the encode thread must not touch it.
        pub const READING: u32 = 2;

        /// Section header. The host stamps the layout fields and the magic last, before delivering
        /// the handle; the driver owns everything from `latest` down and writes it through atomic
        /// views over the mapping, `latest` with Release after the slot it names is complete.
        /// Telemetry here is what the classifier reads instead of inferring a stall from timers:
        /// `source_seq` moving while `last_au_qpc` stands still is a wedged encoder, and both
        /// standing still is a display composing nothing.
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
        pub struct AuHeader {
            /// [`AU_MAGIC`], host-stamped last.
            pub magic: u32,
            /// [`AU_VERSION`], host-stamped.
            pub version: u32,
            /// Host-stamped [`HEAP_OFFSET`].
            pub heap_offset: u32,
            /// Host-stamped heap size — [`heap_bytes_for`].
            pub heap_bytes: u32,
            /// Host-stamped [`SLOT_TABLE_OFFSET`].
            pub slot_table_offset: u32,
            /// Host-stamped [`AU_SLOTS`].
            pub slot_count: u32,
            /// The publish cell: a packed [`FrameToken`](super::FrameToken).
            pub latest: u64,
            /// Bumped by the driver on every SET_ENCODE; a publish carries it.
            pub generation: u32,
            /// Echo of [`SetEncodeRequest::wire_seq_base`](super::SetEncodeRequest::wire_seq_base).
            pub wire_seq_base: u32,
            /// One of the `ENCODER_*` states.
            pub encoder_state: u32,
            /// Encode threads abandoned after a wedge. Two is the `DriverCycle` threshold.
            pub detached: u32,
            /// QPC of the most recent publish — the stall clock the classifier reads.
            pub last_au_qpc: u64,
            /// QPC of the drain worker's most recent pass, stored after `FinishedProcessingFrame`.
            pub drain_heartbeat_qpc: u64,
            /// Frames the drain worker handed the pool — DWM's cadence, ahead of the encoder.
            pub source_seq: u64,
            /// Frames dropped at the pool or skipped for a full slot table.
            pub dropped_total: u64,
            /// Access units published.
            pub published_total: u64,
            /// One of the `DRV_STATUS_*` words — how a driver with no debugger reports.
            pub driver_status: u32,
            /// Raw detail for `driver_status`.
            pub driver_status_detail: u32,
            /// Rate the backend is actually encoding at, kbps, rewritten every time the driver
            /// drains an [`ENCODE_CTL_RECONFIGURE_BITRATE`](super::ENCODE_CTL_RECONFIGURE_BITRATE).
            /// The ctl is queued for the encode thread and has no reply, so a backend that
            /// declines or clamps one is otherwise invisible: the host would go on reporting a
            /// rate nothing encodes. Occupies the old `_reserved` at offset 96; `0` is a driver
            /// that predates the stamp, and the host keeps the rate it asked for.
            pub applied_bitrate_kbps: u32,
            /// Pads the header to [`AU_HEADER_SIZE`]; zero.
            pub _reserved: [u8; 28],
        }

        /// One slot: where an access unit (or one chunk of one) sits in the heap, and what the host
        /// stamps on the wire frame. `offset`/`len` are the driver's to choose, so a reader
        /// bounds-checks them against `heap_bytes` before copying. `wire_seq` continues the host's
        /// `au_seq` domain from
        /// [`SetEncodeRequest::wire_seq_base`](super::SetEncodeRequest::wire_seq_base);
        /// `source_seq` names the frame it encodes, which is how a dropped frame stays visible.
        /// `qpc_submit` and `qpc_published` are the driver's own clocks on the way through: with
        /// `qpc_pts` they split present → arrival into the pool wait, the encode and the hand-off.
        #[repr(C)]
        #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
        pub struct AuSlot {
            /// Byte offset from the start of the section (not from `heap_offset`).
            pub offset: u32,
            /// Bytes of bitstream.
            pub len: u32,
            /// Wire frame index the packetizer stamps.
            pub wire_seq: u32,
            /// Low half of the source frame counter this AU encodes.
            pub source_seq: u32,
            /// Source present QPC — the ground truth the classifier reads, not a timer inference.
            pub qpc_pts: u64,
            /// `AU_*` flag bits.
            pub flags: u32,
            /// [`FREE`], [`PUBLISHED`] or [`READING`].
            pub state: u32,
            /// QPC when the driver handed the frame to its encoder. `0` from a driver that
            /// predates the stamp.
            pub qpc_submit: u64,
            /// QPC when the driver wrote this record.
            pub qpc_published: u64,
        }

        /// Heap bytes for a session: four frames at `max_bitrate_kbps`, doubled for burst, rounded
        /// up to [`HEAP_GRANULE`] and clamped into [`HEAP_MIN_BYTES`]..=[`HEAP_MAX_BYTES`].
        ///
        /// Four frames is the slot table's working set while the host drains; the doubling pays for
        /// an IDR several times the average AU. 400 Mbps at 60 fps lands at 6.4 MiB, inside the
        /// 8 MiB the plan budgets, and the same bitrate at 120 fps needs half of it. `fps == 0`
        /// reads as 1 rather than dividing by zero.
        #[must_use]
        pub const fn heap_bytes_for(max_bitrate_kbps: u32, fps: u32) -> u32 {
            let fps = if fps == 0 { 1 } else { fps as u64 };
            let per_second = (max_bitrate_kbps as u64) * 1000 / 8;
            let granule = HEAP_GRANULE as u64;
            let rounded = (8 * (per_second / fps)).div_ceil(granule) * granule;
            if rounded < HEAP_MIN_BYTES as u64 {
                HEAP_MIN_BYTES
            } else if rounded > HEAP_MAX_BYTES as u64 {
                HEAP_MAX_BYTES
            } else {
                rounded as u32
            }
        }

        /// Section bytes for a heap: header, slot table and heap, rounded up to a 4 KiB page. The
        /// host allocates exactly this and sends it as
        /// [`SetEncodeRequest::section_bytes`](super::SetEncodeRequest::section_bytes).
        #[must_use]
        pub const fn section_bytes(heap_bytes: u32) -> u32 {
            let align = SECTION_ALIGN as u64;
            let total = HEAP_OFFSET as u64 + heap_bytes as u64;
            (total.div_ceil(align) * align) as u32
        }

        /// Byte offset of slot `i`'s record inside the section.
        #[must_use]
        pub const fn slot_offset(i: usize) -> usize {
            SLOT_TABLE_OFFSET + i * AU_SLOT_SIZE
        }

        /// Whether a mapped header may be read past its magic: the host's magic, this version, the
        /// layout constants both sides already agree on, and a heap within the caps. Every other
        /// field is an index a writer chose, so `false` means touch nothing — the fail-closed rule
        /// the ring's `check_attach` follows.
        #[must_use]
        pub const fn au_readable(header: &AuHeader) -> bool {
            header.magic == AU_MAGIC
                && header.version == AU_VERSION
                && header.slot_count == AU_SLOTS
                && header.slot_table_offset as usize == SLOT_TABLE_OFFSET
                && header.heap_offset as usize == HEAP_OFFSET
                && header.heap_bytes >= HEAP_MIN_BYTES
                && header.heap_bytes <= HEAP_MAX_BYTES
        }

        /// The writer's bookkeeping over the heap and the slot table: which slot an access
        /// unit (or one chunk of it) goes into and where its bytes land. Pure over a snapshot
        /// of the slot states the caller loads, so it runs under `cargo test` anywhere.
        ///
        /// Bytes are placed by a bump pointer that wraps at the heap's end and never straddles
        /// it. A placement that overlaps the recorded range of a slot the host still holds
        /// ([`PUBLISHED`] or [`READING`]) is refused, never moved past it: the writer then waits
        /// or drops at the pool. One access unit's chunks therefore sit at ascending offsets
        /// except across a single wrap — the order the host reads a multi-chunk AU in.
        #[derive(Clone, Debug)]
        pub struct HeapRing {
            heap_offset: u32,
            heap_bytes: u32,
            /// Next write offset, section-relative.
            head: u32,
            /// Round-robin start for the slot pick.
            next_slot: usize,
            /// `(offset, len)` last handed out per slot; `len == 0` = nothing recorded.
            ranges: [(u32, u32); AU_SLOTS as usize],
        }

        impl HeapRing {
            #[must_use]
            pub const fn new(heap_offset: u32, heap_bytes: u32) -> Self {
                Self {
                    heap_offset,
                    heap_bytes,
                    head: heap_offset,
                    next_slot: 0,
                    ranges: [(0, 0); AU_SLOTS as usize],
                }
            }

            /// A [`FREE`] slot and a heap range of `len` bytes for it, recorded as the slot's.
            /// `None` when no slot is free, `len` exceeds the heap, or both placements — in
            /// place and after a wrap — overlap a range the host still holds.
            pub fn take(
                &mut self,
                len: u32,
                states: &[u32; AU_SLOTS as usize],
            ) -> Option<(usize, u32)> {
                if len > self.heap_bytes {
                    return None;
                }
                let n = AU_SLOTS as usize;
                let slot = (0..n)
                    .map(|k| (self.next_slot + k) % n)
                    .find(|&i| states[i] == FREE)?;
                let end = self.heap_offset + self.heap_bytes;
                let in_place = (self.head + len <= end).then_some(self.head);
                let offset = in_place
                    .into_iter()
                    .chain(core::iter::once(self.heap_offset))
                    .find(|&at| !self.overlaps(at, len, states))?;
                self.ranges[slot] = (offset, len);
                self.head = offset + len;
                self.next_slot = (slot + 1) % n;
                Some((slot, offset))
            }

            /// Whether `[at, at + len)` touches a range a non-[`FREE`] slot still names.
            fn overlaps(&self, at: u32, len: u32, states: &[u32; AU_SLOTS as usize]) -> bool {
                self.ranges
                    .iter()
                    .zip(states)
                    .any(|(&(off, held), &state)| {
                        state != FREE && held != 0 && at < off + held && off < at + len
                    })
            }
        }

        // Layout crosses the process boundary; Pod rejects internal padding and these pin the
        // externally-visible sizes, so a same-size field reorder is a compile error.
        const _: () = {
            use core::mem::{offset_of, size_of};

            assert!(size_of::<AuHeader>() == AU_HEADER_SIZE);
            assert!(offset_of!(AuHeader, magic) == 0);
            assert!(offset_of!(AuHeader, version) == 4);
            assert!(offset_of!(AuHeader, heap_offset) == 8);
            assert!(offset_of!(AuHeader, heap_bytes) == 12);
            assert!(offset_of!(AuHeader, slot_table_offset) == 16);
            assert!(offset_of!(AuHeader, slot_count) == 20);
            assert!(offset_of!(AuHeader, latest) == 24);
            assert!(offset_of!(AuHeader, generation) == 32);
            assert!(offset_of!(AuHeader, wire_seq_base) == 36);
            assert!(offset_of!(AuHeader, encoder_state) == 40);
            assert!(offset_of!(AuHeader, detached) == 44);
            assert!(offset_of!(AuHeader, last_au_qpc) == 48);
            assert!(offset_of!(AuHeader, drain_heartbeat_qpc) == 56);
            assert!(offset_of!(AuHeader, source_seq) == 64);
            assert!(offset_of!(AuHeader, dropped_total) == 72);
            assert!(offset_of!(AuHeader, published_total) == 80);
            assert!(offset_of!(AuHeader, driver_status) == 88);
            assert!(offset_of!(AuHeader, driver_status_detail) == 92);
            assert!(offset_of!(AuHeader, applied_bitrate_kbps) == 96);
            assert!(offset_of!(AuHeader, _reserved) == 100);

            assert!(size_of::<AuSlot>() == AU_SLOT_SIZE);
            assert!(offset_of!(AuSlot, offset) == 0);
            assert!(offset_of!(AuSlot, len) == 4);
            assert!(offset_of!(AuSlot, wire_seq) == 8);
            assert!(offset_of!(AuSlot, source_seq) == 12);
            assert!(offset_of!(AuSlot, qpc_pts) == 16);
            assert!(offset_of!(AuSlot, flags) == 24);
            assert!(offset_of!(AuSlot, state) == 28);
            assert!(offset_of!(AuSlot, qpc_submit) == 32);
            assert!(offset_of!(AuSlot, qpc_published) == 40);

            assert!(HEAP_OFFSET == 896 && HEAP_OFFSET % 64 == 0);
            assert!(SLOT_TABLE_OFFSET % 8 == 0);
        };
    }

    // Same reason as the ring's asserts: the IOCTL buffers are raw bytes on the far side.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<SetEncodeRequest>() == 144);
        assert!(offset_of!(SetEncodeRequest, target_id) == 0);
        assert!(offset_of!(SetEncodeRequest, section) == 8);
        assert!(offset_of!(SetEncodeRequest, event) == 16);
        assert!(offset_of!(SetEncodeRequest, hdr_meta) == 60);
        assert!(offset_of!(SetEncodeRequest, backends) == 96);
        assert!(offset_of!(SetEncodeRequest, flags) == 112);
        assert!(offset_of!(SetEncodeRequest, knobs) == SET_ENCODE_REQUEST_LEGACY_SIZE);
        assert!(size_of::<EncodeKnobs>() == 24);
        assert!(offset_of!(EncodeKnobs, split_encode) == 6);
        assert!(offset_of!(EncodeKnobs, _reserved) == 20);

        assert!(size_of::<EncoderCapsWire>() == 24);
        assert!(size_of::<SetEncodeReply>() == 72);
        assert!(offset_of!(SetEncodeReply, caps) == 8);
        assert!(offset_of!(SetEncodeReply, applied_bitrate_kbps) == 32);
        assert!(offset_of!(SetEncodeReply, error) == 36);
        assert!(offset_of!(SetEncodeReply, name) == 40);

        assert!(size_of::<EncodeCtlRequest>() == 44);
        assert!(offset_of!(EncodeCtlRequest, op) == 4);
        assert!(offset_of!(EncodeCtlRequest, arg0) == 8);
        assert!(offset_of!(EncodeCtlRequest, arg1) == 12);
        assert!(offset_of!(EncodeCtlRequest, payload) == 16);
    };
}

/// Gamepad shared-memory layouts (host ↔ UMDF drivers `pf_xusb` / `pf_gamepad`).
///
/// Sealed channel (`design/gamepad-channel-sealing.md`): the host creates the DATA section
/// ([`XusbShm`]/[`PadShm`]) unnamed (SYSTEM-only DACL) and duplicates its handle into WUDFHost;
/// only the tiny [`PadBootstrap`] mailbox stays named. `Pod` + `offset_of!` asserts pin the
/// historical `OFF_*` / `view.add(N)` layout. Layout only; the sections are host-created.
pub mod gamepad {
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};

    /// XUSB section magic (loosely "PFXU").
    pub const XUSB_MAGIC: u32 = 0x5558_4650;
    /// Pad section magic (loosely "PFDS"). The two magics use opposite byte-order mnemonics;
    /// only the u32 value is the contract.
    pub const PAD_MAGIC: u32 = 0x5046_4453;

    /// `device_type` DualSense. The section is zeroed, so `0` is the default; one driver serves
    /// every identity.
    pub const DEVTYPE_DUALSENSE: u8 = 0;
    /// DualShock 4 (`VID_054C&PID_09CC`).
    pub const DEVTYPE_DUALSHOCK4: u8 = 1;
    /// DualSense Edge (`VID_054C&PID_0DF2`) — DualSense report codec plus the four back/Fn bits.
    pub const DEVTYPE_DUALSENSE_EDGE: u8 = 2;
    /// Steam Deck (`VID_28DE&PID_1205`). Steam Input promotes it on Windows when the synthesized
    /// USB hardware ids carry `&MI_02` (wired controller interface).
    pub const DEVTYPE_STEAMDECK: u8 = 3;
    /// Xbox Wireless Controller (`VID_045E&PID_0B13` — Bluetooth Xbox is a real HID device;
    /// wired `045E:028E`/`045E:02EA` are not). `pf-xusb` registers only `GUID_DEVINTERFACE_XUSB`
    /// and has no HID collection, so Steam/WGI/GameInput never see it.
    ///
    /// Unlike its siblings the Xbox input report is not 64 bytes — it is `XBOX_INPUT_REPORT_LEN`
    /// (16). hidclass sizes its buffer from the descriptor and refuses an over-long source.
    pub const DEVTYPE_XBOX: u8 = 4;
    /// Xbox One S over Bluetooth (`VID_045E&PID_02FD`).
    /// Shares [`DEVTYPE_XBOX`]'s report descriptor byte-for-byte. All three Xbox identities are
    /// the same pad in HID terms and differ only in VID/PID, product string, and INF model line.
    /// Do not hand-write a per-identity descriptor — the shape is shared; identity is VID/PID.
    pub const DEVTYPE_XBOX_ONE_S: u8 = 5;
    /// Xbox Elite Wireless Controller Series 2 (`VID_045E&PID_0B22`).
    /// The four paddles are not in this identity's report yet. The descriptor is shared (see
    /// [`DEVTYPE_XBOX_ONE_S`]); `xinputhid` may claim the HID collection exclusively anyway.
    pub const DEVTYPE_XBOX_ELITE: u8 = 6;
    /// Steam Controller 2 (Triton): wired identity `28DE:1302`. Raw-passthrough — host feeds
    /// captured reports; the driver answers Steam's feature query-dance (see [`crate::triton`]).
    pub const DEVTYPE_TRITON: u8 = 7;

    /// Written into the section's `driver_proto` on attach. The section starts zeroed, so `0`
    /// means no driver has attached. Bump on a gamepad-layout change.
    ///
    /// v3: sealed DATA section + [`ChannelProof`]. The host learns the duplication target over
    /// the device stack, not the mailbox's `driver_pid`. Mixed pairings fail closed both ways.
    /// Evidence: `design/gamepad-channel-sealing.md`.
    pub const GAMEPAD_PROTO_VERSION: u32 = 3;

    // Channel proof: who to hand the DATA section to. Do not take the duplication target from
    // the mailbox's `driver_pid` — LocalService can spawn a world-executable WUDFHost and publish
    // that pid. Ask the devnode the host created (`SwDeviceCreate` instance id). `pf_xusb` answers
    // via IOCTL; `pf_gamepad`/`pf_mouse` have no control device (hidclass owns the stack).

    /// Proof magic ("PFCP"), and the `PFCP` prefix of the text form.
    pub const PROOF_MAGIC: u32 = 0x5043_4650;

    /// HID string index the minidrivers answer with [`ChannelProof`]. 16-bit on purpose: both
    /// `IOCTL_HID_GET_INDEXED_STRING` and `IOCTL_HID_GET_STRING` pack `(language_id << 16) |
    /// string_index`, so only the low word survives. `0x5046` ("PF") is outside USB's 1..=255
    /// string-descriptor range. hidclass currently does not forward an arbitrary indexed-string
    /// request to a UMDF HID minidriver; kept as the first ask. Working transports:
    /// [`proof_is_serial_string`] (`pf_mouse`) and [`HID_FEATURE_REPORT_CHANNEL_PROOF`] (PS pads).
    pub const HID_STRING_INDEX_CHANNEL_PROOF: u32 = 0x5046;

    // Do not retry `WdfDeviceCreateDeviceInterface` for `pf_gamepad`/`pf_mouse`: hidclass owns
    // `IRP_MJ_CREATE` on a devnode it is the FDO for, so `CreateFile` returns ERROR_GEN_FAILURE.

    /// `CTL_CODE(0x8000, 0x0FE0, METHOD_BUFFERED, FILE_ANY_ACCESS)`: function code no xusb22 IOCTL
    /// uses; `FILE_ANY_ACCESS` so the host can ask over a `CreateFile` handle opened with no access
    /// rights (the same way it must open a HID collection).
    pub const IOCTL_PF_GET_CHANNEL_PROOF: u32 = 0x8000_3F80;

    /// Whether a driver serves its channel proof as its HID serial-number string.
    /// `true` for `pf_mouse` only — its serial (`PFMOUSE00`) is inert. Pad serials are what SDL
    /// and Steam dedup on; Steam mangles a pad's displayed name over serial format alone.
    pub const fn proof_is_serial_string(pad_kind_is_mouse: bool) -> bool {
        pad_kind_is_mouse
    }

    /// Feature report the PS pad identities (DualSense / DualShock 4 / Edge) answer the proof on.
    /// `0x85` is already declared as Feature in all three captured descriptors, so this needs no
    /// report-descriptor change — Steam/SDL fingerprint VID/PID, layout, serial, product string.
    pub const HID_FEATURE_REPORT_CHANNEL_PROOF: u8 = 0x85;

    /// Steam Deck private proof command. The Deck descriptor declares one unnumbered feature
    /// report; Steam drives it as `0x83`/`0xAE`. Two bytes, not one, so a Steam command byte we
    /// have not catalogued cannot be mistaken for it. No descriptor change.
    pub const DECK_PROOF_CMD: [u8; 2] = [0xF9, 0x50];

    /// Driver's answer over the device stack: who it is, which pad, which WUDFHost pid.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct ChannelProof {
        pub magic: u32,
        pub proto: u32,
        /// Pad index from the devnode Location — cross-checked so a mis-resolved devnode cannot
        /// cross-wire two pads.
        pub pad_index: u32,
        /// `GetCurrentProcessId()` of the driver's WUDFHost: the duplication target.
        pub wudf_pid: u32,
    }

    impl ChannelProof {
        pub fn new(pad_index: u32, wudf_pid: u32) -> ChannelProof {
            ChannelProof {
                magic: PROOF_MAGIC,
                proto: GAMEPAD_PROTO_VERSION,
                pad_index,
                wudf_pid,
            }
        }

        /// Validate against the pad the host is delivering. `Err` is the operator-facing reason;
        /// every rejection is a refusal to deliver — do not fall back to an untrusted pid.
        pub fn check(&self, expect_pad_index: u32) -> Result<u32, &'static str> {
            if self.magic != PROOF_MAGIC {
                return Err(
                    "the devnode's answer is not a punktfunk channel proof (bad magic) — \
                            some other driver is bound to this device",
                );
            }
            if self.proto != GAMEPAD_PROTO_VERSION {
                return Err(
                    "the driver bound to this devnode speaks a different gamepad protocol \
                            — update the host and the drivers together",
                );
            }
            if self.pad_index != expect_pad_index {
                return Err(
                    "the devnode answered for a DIFFERENT pad index — the interface lookup \
                            resolved the wrong device",
                );
            }
            if self.wudf_pid == 0 {
                return Err("the driver reported pid 0");
            }
            Ok(self.wudf_pid)
        }

        /// 16 wire bytes of the `pf_xusb` IOCTL answer. Driver crates need no `bytemuck`; both
        /// sides go through one length-checked pair with [`from_bytes`](Self::from_bytes).
        pub fn to_bytes(self) -> [u8; 16] {
            let mut out = [0u8; 16];
            out.copy_from_slice(bytemuck::bytes_of(&self));
            out
        }

        /// Parse [`to_bytes`](Self::to_bytes). `None` on a short read — never zero-extend into a pid.
        ///
        /// `pod_read_unaligned`, not `from_bytes`: the feature-report form offsets the proof by one
        /// byte (report id at 0), so the slice is not 4-aligned. Device I/O buffers have no alignment.
        pub fn from_bytes(b: &[u8]) -> Option<ChannelProof> {
            (b.len() >= 16).then(|| bytemuck::pod_read_unaligned::<ChannelProof>(&b[..16]))
        }

        /// HID feature report of exactly `len` bytes: `[report_id, proof(16), 0…]`. Byte 0 is the
        /// report id; the driver pads to the descriptor length. `None` if `len` cannot hold id+proof.
        pub fn to_feature_report(self, report_id: u8, len: usize) -> Option<alloc::vec::Vec<u8>> {
            if len < 17 {
                return None;
            }
            let mut out = alloc::vec![0u8; len];
            out[0] = report_id;
            out[1..17].copy_from_slice(&self.to_bytes());
            Some(out)
        }

        /// Parse [`to_feature_report`](Self::to_feature_report); skips the leading report id.
        pub fn from_feature_report(b: &[u8]) -> Option<ChannelProof> {
            Self::from_bytes(b.get(1..)?)
        }

        /// HID indexed-string form: `PFCP:<proto>:<pad_index>:<wudf_pid>`.
        /// `HidD_GetIndexedString` is a string channel.
        pub fn to_hid_string(self) -> String {
            alloc::format!("PFCP:{}:{}:{}", self.proto, self.pad_index, self.wudf_pid)
        }

        /// Parse [`to_hid_string`](Self::to_hid_string). `None` on any deviation — refuse delivery
        /// rather than guess a pid.
        pub fn from_hid_string(s: &str) -> Option<ChannelProof> {
            let rest = s.strip_prefix("PFCP:")?;
            let mut it = rest.split(':');
            let proto = it.next()?.parse::<u32>().ok()?;
            let pad_index = it.next()?.parse::<u32>().ok()?;
            let wudf_pid = it.next()?.parse::<u32>().ok()?;
            if it.next().is_some() {
                return None; // trailing field: not a shape we mint
            }
            Some(ChannelProof {
                magic: PROOF_MAGIC,
                proto,
                pad_index,
                wudf_pid,
            })
        }
    }

    /// Bootstrap-mailbox magic (`"PFBT"` LE). The host stamps it last (after `host_proto`) so a
    /// driver only trusts a fully-initialized mailbox.
    pub const BOOT_MAGIC: u32 = 0x5442_4650;

    /// `Global\pfxusb-boot-<index>` — Xbox 360 pad bootstrap mailbox ([`PadBootstrap`]).
    pub fn xusb_boot_name(index: u8) -> String {
        alloc::format!("Global\\pfxusb-boot-{index}")
    }
    /// `Global\pfds-boot-<index>` — DualSense / DualShock 4 bootstrap mailbox ([`PadBootstrap`]).
    pub fn pad_boot_name(index: u8) -> String {
        alloc::format!("Global\\pfds-boot-{index}")
    }

    /// Per-pad bootstrap mailbox (32 B, named `Global\pf…-boot-<index>`, SY+LS DACL) — the only
    /// named object on the gamepad channel. UMDF HID minidrivers have no control device (hidclass
    /// owns the stack), so this is the late-bound handshake: host stamps `host_proto` then `magic`;
    /// driver writes `driver_proto`/`driver_pid`; host asks the **devnode** ([`ChannelProof`]) who
    /// the driver is, duplicates the unnamed DATA section, then writes `data_handle`/`handle_pid`
    /// and bumps `handle_seq` last. `driver_pid` is advisory; the mailbox does not choose the
    /// duplication target. Evidence: `design/gamepad-channel-sealing.md`.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct PadBootstrap {
        /// [`BOOT_MAGIC`], host-stamped last at creation.
        pub magic: u32,
        /// Host's [`GAMEPAD_PROTO_VERSION`]. A driver whose version differs must not publish its
        /// pid (fail closed); it still writes `driver_proto` so the host can log the mismatch.
        pub host_proto: u32,
        /// Driver's WUDFHost pid (`0` = none yet). Advisory liveness hint — not the duplication
        /// target; that comes from [`ChannelProof`].
        pub driver_pid: u32,
        /// Driver's [`GAMEPAD_PROTO_VERSION`] (diagnostics only).
        pub driver_proto: u32,
        /// DATA-section handle VALUE duplicated into `handle_pid`'s table; valid only in that process.
        pub data_handle: u64,
        /// Pid `data_handle` was duplicated for — a driver whose pid differs ignores the delivery.
        pub handle_pid: u32,
        /// Host-global monotonic, never 0. Bumped AFTER `data_handle`/`handle_pid` — new-delivery trigger.
        pub handle_seq: u32,
    }

    /// Virtual Xbox 360 (XInput) shared section (64 B). Host writes XInput state; driver answers
    /// `XInputGetState`. Driver writes `XInputSetState` into `rumble_*` (bumping `rumble_seq`).
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct XusbShm {
        pub magic: u32,
        /// XInput `dwPacketNumber` — bumped on every state change.
        pub packet: u32,
        pub buttons: u16,
        pub left_trigger: u8,
        pub right_trigger: u8,
        pub thumb_lx: i16,
        pub thumb_ly: i16,
        pub thumb_rx: i16,
        pub thumb_ry: i16,
        pub _reserved0: u32,
        /// Bumped on a new force-feedback packet.
        pub rumble_seq: u32,
        pub rumble_large: u8,
        pub rumble_small: u8,
        pub _pad0: [u8; 2],
        /// [`GAMEPAD_PROTO_VERSION`] while attached. `0` = no driver — the host health check keys off it.
        pub driver_proto: u32,
        /// Bumped on every serviced XInput IOCTL. Only advances while something polls the slot, so a
        /// static value is not an error.
        pub driver_heartbeat: u32,
        /// Pad index (host-stamped before the magic). The driver checks it against
        /// `pszDeviceLocation` so a cross-pad delivery is rejected. Carved from v1 reserved space.
        pub pad_index: u32,
        pub _reserved1: [u8; 20],
    }

    /// Pre-ring [`PadShm`] size. Every field old binaries know sits below this offset; the ring
    /// keeps bytes `0..256` identical. Pagefile-backed sections are page-granular, so either
    /// generation's view maps against either generation's section — a driver must still fall
    /// back to this size if the full-size map is refused (`ChannelConfig::min_data_size`).
    pub const PAD_SHM_LEGACY_SIZE: usize = 256;

    /// v2.1 output-report ring depth — hardcoded `%` in every pre-v2.2 driver, and the drain
    /// length whenever [`PadShm::out_ring_len`] reads 0. Eight slots at a ~4 ms poll overflow
    /// under a sustained >2 kHz writer (DS5 compat-vibration re-sends per audio quantum).
    pub const OUT_RING_LEN: u32 = 8;
    pub const OUT_RING_LEN_USIZE: usize = OUT_RING_LEN as usize;

    /// v2.2 ring depth: every slot that fits the one-page section ([`PAD_SHM_SIZE`] = 4096).
    /// 56 slots at ~4 ms poll ≈ 14 kHz. Used only when both sides negotiated it (`out_ring_ver
    /// >= 2` and the driver echoed the length in [`PadShm::out_ring_len`]).
    pub const OUT_RING_LEN_V22: u32 = 56;
    pub const OUT_RING_LEN_V22_USIZE: usize = OUT_RING_LEN_V22 as usize;

    /// v2.1 [`PadShm`] size. A v2.1 driver maps this much and gates 8-slot ring writes on
    /// `mapped_len() >= 1024`; v2.2 keeps bytes `0..1024` identical.
    pub const PAD_SHM_V21_SIZE: usize = 1024;

    /// Full [`PadShm`] size — exactly one page. Hard ceiling: pagefile-backed sections round up
    /// to page granularity, which is what lets every generation map its own size against any
    /// other generation's section. Growing past 4096 needs a new negotiation.
    pub const PAD_SHM_SIZE: usize = 4096;

    /// One slot of the lossless output-report ring: report bytes as the game wrote them (id
    /// first), with the exact length. The legacy latest-report slot's fixed 64-byte copy can
    /// carry a stale tail from a previous longer report.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct OutSlot {
        /// Valid bytes in `data` (`0..=64`). `0` = never written.
        pub len: u32,
        pub data: [u8; 64],
    }

    /// DualSense / DualShock 4 shared section ([`PAD_SHM_SIZE`] = 4096). Bytes `0..256` are the
    /// v2 layout ([`PAD_SHM_LEGACY_SIZE`]); `0..1024` are v2.1 ([`PAD_SHM_V21_SIZE`]). Host writes
    /// `input`; driver publishes output into the legacy `output` slot (every host generation reads
    /// it) and, when `out_ring_ver` is stamped, into the lossless `out_ring`. The single slot
    /// coalesces — a rumble-stop overwritten inside one poll is gone (`design/rumble-root-fix.md`).
    ///
    /// Tail extension, not a [`GAMEPAD_PROTO_VERSION`] bump: bootstrap fails closed on a version
    /// mismatch (no pad at all), the wrong failure for a feedback-quality fix. An old host never
    /// stamps `out_ring_ver`, so a new driver stays on the legacy slot.
    ///
    /// Ring-length: each side declares, the shorter wins. Host stamps `out_ring_ver = 2`; the
    /// driver picks (`>= 2` + full map → 56, `1` → 8, `0` → no ring) and echoes into
    /// `out_ring_len` before every `ring_head` bump. Drain keys off the echo (`0` = 8). Store
    /// order slot-bytes → echo → head-bump: an Acquire-observed head bump always has that length.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct PadShm {
        pub magic: u32,
        pub _reserved0: u32,
        /// Host-written HID input (≤ 64 B). Spans `magic`+pad .. `out_seq`.
        pub input: [u8; 64],
        /// Bumped when the driver publishes a new `output` report.
        pub out_seq: u32,
        /// Driver-written output: rumble / lightbar / player-LEDs / adaptive triggers.
        pub output: [u8; 64],
        /// HID identity — [`DEVTYPE_DUALSENSE`] / [`DEVTYPE_DUALSHOCK4`].
        pub device_type: u8,
        pub _pad0: [u8; 3],
        /// [`GAMEPAD_PROTO_VERSION`] while mapped. `0` = no driver — the host health check keys off it.
        pub driver_proto: u32,
        /// Bumped by the driver's ~125 Hz timer each tick — advances whenever loaded, game or not.
        pub driver_heartbeat: u32,
        /// Pad index (host-stamped before the magic) — see [`XusbShm::pad_index`].
        pub pad_index: u32,
        /// Host-stamped `1` ⇔ this section carries `out_ring` and the host drains it. Zeroed
        /// section + old host never writes it, so `0` tells a new driver to stay legacy-only.
        pub out_ring_ver: u32,
        /// Driver-bumped AFTER writing `out_ring[ring_head % len]`. Overflow is `head - tail > len`.
        /// Same publish-then-bump order as `out_seq` (host Acquire load).
        pub ring_head: u32,
        /// Ring length the driver's slot math is using, (re-)stamped before every `ring_head`
        /// bump. `0` = pre-v2.2 driver = [`OUT_RING_LEN`].
        pub out_ring_len: u32,
        /// Seqlock over [`PadShm::input`]: odd while mid-copy, even when the slot holds a whole
        /// report. Host: bump odd, Release-fence, write 64 bytes, Release-store even. Driver
        /// samples before and after and retries on a write in flight. An old host leaves this 0
        /// (constant even), so a new driver's re-check always passes. Inside the v2 legacy region.
        pub input_gen: u32,
        pub _reserved1: [u8; 84],
        /// Lossless output ring. [`OUT_RING_LEN`] under v2.1, [`OUT_RING_LEN_V22`] under v2.2
        /// (slots 8.. overlay what v2.1 called `_reserved2`, which no shipped binary touched).
        pub out_ring: [OutSlot; OUT_RING_LEN_V22_USIZE],
        pub _reserved2: [u8; 32],
    }

    // Offsets are the wire contract the shipped drivers already read by hand. A failing assert
    // means the struct no longer matches the historical `OFF_*` / `view.add(N)` layout.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<XusbShm>() == 64);
        assert!(offset_of!(XusbShm, magic) == 0);
        assert!(offset_of!(XusbShm, packet) == 4);
        assert!(offset_of!(XusbShm, buttons) == 8);
        assert!(offset_of!(XusbShm, left_trigger) == 10);
        assert!(offset_of!(XusbShm, right_trigger) == 11);
        assert!(offset_of!(XusbShm, thumb_lx) == 12);
        assert!(offset_of!(XusbShm, thumb_ly) == 14);
        assert!(offset_of!(XusbShm, thumb_rx) == 16);
        assert!(offset_of!(XusbShm, thumb_ry) == 18);
        assert!(offset_of!(XusbShm, rumble_seq) == 24);
        assert!(offset_of!(XusbShm, rumble_large) == 28);
        assert!(offset_of!(XusbShm, rumble_small) == 29);
        assert!(offset_of!(XusbShm, driver_proto) == 32);
        assert!(offset_of!(XusbShm, driver_heartbeat) == 36);
        assert!(offset_of!(XusbShm, pad_index) == 40);

        assert!(size_of::<PadShm>() == PAD_SHM_SIZE);
        assert!(offset_of!(PadShm, magic) == 0);
        assert!(offset_of!(PadShm, input) == 8);
        assert!(offset_of!(PadShm, out_seq) == 72);
        assert!(offset_of!(PadShm, output) == 76);
        assert!(offset_of!(PadShm, device_type) == 140);
        assert!(offset_of!(PadShm, driver_proto) == 144);
        assert!(offset_of!(PadShm, driver_heartbeat) == 148);
        assert!(offset_of!(PadShm, pad_index) == 152);
        // Ring extension — everything below PAD_SHM_LEGACY_SIZE is the v2 layout verbatim.
        assert!(offset_of!(PadShm, out_ring_ver) == 156);
        assert!(offset_of!(PadShm, ring_head) == 160);
        assert!(offset_of!(PadShm, out_ring) == PAD_SHM_LEGACY_SIZE);
        assert!(size_of::<OutSlot>() == 68);
        // Echo field in v2.1 reserved space; slot k stays at 256 + k*68; struct is one page.
        assert!(offset_of!(PadShm, out_ring_len) == 164);
        // Input seqlock: 4-aligned (atomic accessors check it) and inside the v2 legacy region.
        assert!(offset_of!(PadShm, input_gen) == 168);
        assert!(offset_of!(PadShm, input_gen) % 4 == 0);
        assert!(offset_of!(PadShm, input_gen) < PAD_SHM_LEGACY_SIZE);
        assert!(
            PAD_SHM_LEGACY_SIZE + OUT_RING_LEN_USIZE * size_of::<OutSlot>() <= PAD_SHM_V21_SIZE
        );
        assert!(PAD_SHM_SIZE == 4096);

        assert!(size_of::<ChannelProof>() == 16);
        assert!(offset_of!(ChannelProof, magic) == 0);
        assert!(offset_of!(ChannelProof, proto) == 4);
        assert!(offset_of!(ChannelProof, pad_index) == 8);
        assert!(offset_of!(ChannelProof, wudf_pid) == 12);

        assert!(size_of::<PadBootstrap>() == 32);
        assert!(offset_of!(PadBootstrap, magic) == 0);
        assert!(offset_of!(PadBootstrap, host_proto) == 4);
        assert!(offset_of!(PadBootstrap, driver_pid) == 8);
        assert!(offset_of!(PadBootstrap, driver_proto) == 12);
        assert!(offset_of!(PadBootstrap, data_handle) == 16);
        assert!(offset_of!(PadBootstrap, handle_pid) == 24);
        assert!(offset_of!(PadBootstrap, handle_seq) == 28);
    };

    /// How often a real pad on USB sends an input report: `DualSense`, DualShock 4 and the Deck
    /// (its 4 ms connection interval) alike. Every identity but the Triton is served at this rate.
    ///
    /// A game polls the stream, not the state: Sony's libScePad hands a frame no sample when no
    /// report arrived since its last read, so a pad slower than the game's frame rate reads as a
    /// held input released and pressed again.
    pub const REPORT_PERIOD_US: u64 = 4_000;

    /// Whether a report is due at `now_us`, given the slot `due_us` it was scheduled for.
    ///
    /// `Some(next)` means serve now and schedule the next slot one period on. A tick more than a
    /// period late restarts the schedule from now rather than catching up, since a burst of
    /// back-dated reports is exactly the cadence a game must not see.
    pub fn serve_due(now_us: u64, due_us: u64) -> Option<u64> {
        if now_us < due_us {
            return None;
        }
        let from = if now_us - due_us >= REPORT_PERIOD_US {
            now_us
        } else {
            due_us
        };
        Some(from + REPORT_PERIOD_US)
    }

    /// Write the pad's own clocks into a Sony report about to be served.
    ///
    /// A USB `DualSense` advances four every report: the 8-bit counter (byte 7), a 32-bit packet
    /// sequence (12–15), `sensor_timestamp` (28–31) and a second 32-bit timer (49–52) about 2 ms
    /// after it. Motion code integrates gyro over `sensor_timestamp`, so it has to advance by the
    /// real time between the reports a game receives. The host publishes at its client's rate and
    /// the driver serves at the hardware's, so the driver owns every clock. `serial` is this
    /// report's index, `elapsed_us` the time since the first report; every field wraps as hardware
    /// does. Returns `false`, and leaves the report alone, for an identity that has no such fields.
    pub fn stamp_sony_clock(
        device_type: u8,
        report: &mut [u8; 64],
        serial: u32,
        elapsed_us: u64,
    ) -> bool {
        match device_type {
            DEVTYPE_DUALSENSE | DEVTYPE_DUALSENSE_EDGE => {
                report[7] = serial as u8;
                report[12..16].copy_from_slice(&serial.to_le_bytes());
                // 1/3 µs ticks (hid-playstation's DIV_ROUND_CLOSEST(delta, 3)).
                let ticks = elapsed_us * 3;
                report[28..32].copy_from_slice(&(ticks as u32).to_le_bytes());
                // A real pad stamps this ~5 900 ticks (≈2 ms) after the sensor sample.
                report[49..53].copy_from_slice(&((ticks + 5_900) as u32).to_le_bytes());
                true
            }
            DEVTYPE_DUALSHOCK4 => {
                // The counter is the top six bits; the low two are PS and touchpad click.
                report[7] = (report[7] & 0x03) | (((serial as u8) & 0x3F) << 2);
                // 16/3 µs ticks, mirrored into the one touch frame's own timestamp byte.
                let ts = (elapsed_us * 3 / 16) as u16;
                report[10..12].copy_from_slice(&ts.to_le_bytes());
                report[34] = ts as u8;
                true
            }
            _ => false,
        }
    }
}

/// Steam Controller 2 (Triton) wire tables: UMDF driver (answers Steam synchronously) and
/// host/inject (Linux usbip + tests). Pure byte-packing so it tests on any host.
pub mod triton {
    /// Feature-1 command bytes of the Valve query dance.
    pub const ID_GET_ATTRIBUTES_VALUES: u8 = 0x83;
    pub const ID_GET_STRING_ATTRIBUTE: u8 = 0xAE;
    pub const ID_GET_FIRMWARE_INFO: u8 = 0xF2;
    /// Output report id Steam rumbles with (`80 | type | intensity16 | Lspeed16 Lgain | Rspeed16 Rgain`).
    pub const ID_OUT_REPORT_HAPTIC_RUMBLE: u8 = 0x80;

    /// Wired Steam Controller 2 identity (`28DE:1302`) — Triton half of the `0x83` attributes reply.
    const WIRED_PRODUCT: u32 = 0x1302;

    /// Firmware build time (unix epoch) as attribute tag `4` (`ATTRIB_FIRMWARE_BUILD_TIME`) in
    /// the `0x83` reply, mirrored at bytes 4..8 of the `0xF2` firmware-info reply — the two must
    /// agree. `0x6A6D_3700` = 2026-08-01T00:00:00Z. An older synthetic epoch (Feb 2016) made
    /// Steam offer to "update" the virtual pad, forwarding SET_REPORTs toward a real controller.
    /// Bump when Steam learns a newer shipping firmware and starts prompting again.
    pub const FW_BUILD_TIME: u32 = 0x6A6D_3700;

    /// Bit 31 of an out-ring slot's `len` marks a FEATURE set (vs interrupt/output). Only Triton's
    /// producer/consumer interpret it; other devtypes write plain lengths, so the bit is additive.
    pub const OUT_FEATURE_BIT: u32 = 0x8000_0000;
    #[inline]
    pub const fn out_len(raw: u32) -> u32 {
        raw & !OUT_FEATURE_BIT
    }
    #[inline]
    pub const fn out_is_feature(raw: u32) -> bool {
        raw & OUT_FEATURE_BIT != 0
    }

    /// Wire length (id byte included) of each input report the wired descriptor declares.
    /// hidclass sizes its read buffer from the largest (0x42 → 54) and refuses over-long
    /// completions. `None` = undeclared id, drop it (0x47 is BLE-only, not in the 372-byte descriptor).
    pub const fn input_len(report_id: u8) -> Option<usize> {
        match report_id {
            0x42 => Some(54),
            0x45 => Some(46),
            0x43 => Some(15),
            0x44 => Some(6),
            0x79 => Some(2),
            0x7B => Some(13),
            _ => None,
        }
    }

    /// Declared wire length (id byte included) of each OUTPUT report. hidclass pads every write
    /// to `OutputReportByteLength` (64), so the host trims before forwarding — a 0x80 rumble is
    /// 10 bytes on GATT, not 64. Unknown id returns 64: no trim, never guess a length.
    /// Hand-mirrored on the Apple client as `Sc2Device.strippedOutputLen` (id-excluded, so
    /// `stripped + 1` == the value here). Edit this table and that one together.
    pub const fn out_report_len(id: u8) -> usize {
        match id {
            0x80 => 10,
            0x81 => 8,
            0x82 => 4,
            0x83 => 10,
            0x84 => 9,
            0x85 => 4,
            0x86 => 4,
            // 0x87/0x88/0x89 are declared full-length (63-byte payload) blobs.
            _ => 64,
        }
    }

    /// Per-pad unit id (`"TRI\0" | index` — same value the Linux leg uses).
    pub const fn unit_id(index: u8) -> u32 {
        0x5452_4900 | index as u32
    }

    /// ASCII serial `FVPF1302<idx:02>D03`. Steam rejects a "PF"-leading serial; the FVPF prefix
    /// is what the host's physical-conflict gate excludes.
    pub fn serial(index: u8, out: &mut [u8; 13]) {
        const D: &[u8; 10] = b"0123456789";
        out.copy_from_slice(b"FVPF130200D03");
        out[8] = D[(index / 10 % 10) as usize];
        out[9] = D[(index % 10) as usize];
    }

    /// Wired Triton's captured 372-byte report descriptor. Byte-identical to the sysfs capture;
    /// do not re-derive.
    #[rustfmt::skip]
    pub static RDESC: [u8; 372] = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x40, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
        0x95, 0x02, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01, 0x81, 0x01, 0x05, 0x01,
        0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02,
        0x81, 0x06, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, 0x05, 0x0C, 0x0A, 0x38,
        0x02, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0, 0x05, 0x01, 0x09, 0x06, 0xA1,
        0x01, 0x85, 0x41, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25,
        0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x81, 0x01, 0x19, 0x00, 0x29,
        0x65, 0x15, 0x00, 0x25, 0x65, 0x75, 0x08, 0x95, 0x06, 0x81, 0x00, 0xC0,
        0x06, 0x00, 0xFF, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x42, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x35, 0x09, 0x42, 0x81, 0x02, 0x85, 0x44,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x05, 0x09, 0x44, 0x81,
        0x02, 0x85, 0x79, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x01,
        0x09, 0x79, 0x81, 0x02, 0x85, 0x43, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x0E, 0x09, 0x43, 0x81, 0x02, 0x85, 0x7B, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x0C, 0x09, 0x7B, 0x81, 0x02, 0x85, 0x45,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x2D, 0x09, 0x45, 0x81,
        0x02, 0x85, 0x80, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09,
        0x09, 0x80, 0x91, 0x02, 0x85, 0x81, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x07, 0x09, 0x81, 0x91, 0x02, 0x85, 0x82, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x09, 0x82, 0x91, 0x02, 0x85, 0x83,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x09, 0x09, 0x83, 0x91,
        0x02, 0x85, 0x84, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x08,
        0x09, 0x84, 0x91, 0x02, 0x85, 0x85, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x03, 0x09, 0x85, 0x91, 0x02, 0x85, 0x86, 0x15, 0x00, 0x26,
        0xFF, 0x00, 0x75, 0x08, 0x95, 0x03, 0x09, 0x86, 0x91, 0x02, 0x85, 0x87,
        0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x3F, 0x09, 0x87, 0x91,
        0x02, 0x85, 0x89, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x3F,
        0x09, 0x89, 0x91, 0x02, 0x85, 0x88, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x75,
        0x08, 0x95, 0x3F, 0x09, 0x88, 0x91, 0x02, 0x85, 0x01, 0x95, 0x3F, 0x09,
        0x01, 0xB1, 0x02, 0x85, 0x02, 0x95, 0x3F, 0x09, 0x01, 0xB1, 0x02, 0xC0,
    ];

    /// Feature GET_REPORT reply for Steam's `GetControllerInfo` query dance. The reply's command
    /// byte must echo the last SET's command or Steam never adopts the pad. Frame is feature
    /// report id 1 (`[0x01][cmd][len][payload…]`, matching SDL). `last_set` is id-first
    /// (`[0x01, cmd, …]`); a stack that already stripped the id (`[cmd, …]`, cmd ≥ 0x80) works too.
    pub fn feature_reply(last_set: &[u8], serial: &str, unit_id: u32) -> [u8; 64] {
        const ATTRIB_STR_UNIT_SERIAL: u8 = 0x01;

        let body = match last_set {
            [0x01, rest @ ..] => rest,
            d => d,
        };
        let cmd = body.first().copied().unwrap_or(ID_GET_STRING_ATTRIBUTE);

        let mut r = [0u8; 64];
        r[0] = 0x01;
        match cmd {
            ID_GET_ATTRIBUTES_VALUES => {
                // Captured controller response: 25-byte payload, five id/u32 attributes.
                r[1] = ID_GET_ATTRIBUTES_VALUES;
                r[2] = 0x19;
                let attrs = [
                    (0x01, WIRED_PRODUCT),
                    (0x02, 0),
                    (0x0A, unit_id),
                    // Tag 4 = ATTRIB_FIRMWARE_BUILD_TIME. See [`FW_BUILD_TIME`].
                    (0x04, FW_BUILD_TIME),
                    (0x09, 0x49),
                ];
                let mut o = 3;
                for (id, val) in attrs {
                    r[o] = id;
                    r[o + 1..o + 5].copy_from_slice(&val.to_le_bytes());
                    o += 5;
                }
            }
            ID_GET_STRING_ATTRIBUTE => {
                // Captured replies always declare 20 bytes: attribute id plus a 19-byte padded string.
                let attr = body.get(2).copied().unwrap_or(ATTRIB_STR_UNIT_SERIAL);
                let b = serial.as_bytes();
                let len = b.len().min(19);
                r[..4].copy_from_slice(&[0x01, ID_GET_STRING_ATTRIBUTE, 0x14, attr]);
                r[4..4 + len].copy_from_slice(&b[..len]);
            }
            ID_GET_FIRMWARE_INFO => {
                let index = body.get(2).copied().unwrap_or(0);
                r[1] = ID_GET_FIRMWARE_INFO;
                r[3] = index;
                match index {
                    0 => {
                        r[2] = 0x29;
                        // Must agree with the 0x83 reply's tag-4 attribute (Steam may cross-check).
                        r[4..8].copy_from_slice(&FW_BUILD_TIME.to_le_bytes());
                        r[8] = 0x49;
                        r[12..24].copy_from_slice(b"603f69218a85");
                        let b = serial.as_bytes();
                        let len = b.len().min(16);
                        r[28..28 + len].copy_from_slice(&b[..len]);
                    }
                    1 => {
                        r[2] = 0x22;
                        r[4..37].copy_from_slice(&[
                            0x00, 0x57, 0xD0, 0x18, 0x6A, 0x37, 0x30, 0x35, 0x34, 0x32, 0x35, 0x37,
                            0x64, 0x32, 0x64, 0x61, 0x37, 0x00, 0x00, 0x00, 0x00, 0x23, 0x00, 0x00,
                            0x00, 0x00, 0x00, 0x00, 0x00, 0x33, 0x6D, 0x02, 0x00,
                        ]);
                    }
                    _ => {
                        r[2] = 0x09;
                        r[4..12].copy_from_slice(&[0x7C, 0x4F, 0x01, 0x00, 0x01, 0, 0, 0]);
                    }
                }
            }
            _ => {
                let n = body.len().min(63);
                r[1..1 + n].copy_from_slice(&body[..n]);
            }
        }
        r
    }
}

/// Virtual-pointer shared-memory layout (host ↔ UMDF HID-mouse minidriver `pf_mouse`).
///
/// With no pointing device, win32k reports the cursor absent (`SM_MOUSEPRESENT` = 0) and DWM
/// never composites a cursor into the pf-vdisplay frame — `SendInput` still moves it, but the
/// stream shows no pointer. A resident HID mouse devnode makes Windows consider a pointer
/// present. Injection stays `SendInput`; the report path is the higher-fidelity route.
///
/// Same sealed-pad handshake as [`gamepad`] (`design/gamepad-channel-sealing.md`):
/// [`gamepad::PadBootstrap`], [`mouse_boot_name`], mouse DATA magic, `pad_index` 0. Reusing
/// the handshake means `pf-umdf-util`'s `ChannelClient`/`PadChannel` serve the mouse unchanged.
pub mod mouse {
    use alloc::string::String;
    use bytemuck::{Pod, Zeroable};

    /// Mouse DATA-section magic ("PFMO" LE) — distinct from the pad magics so a cross-wire fails.
    pub const MOUSE_MAGIC: u32 = 0x4F4D_4650;

    /// `Global\pfmouse-boot-<index>` — mouse bootstrap mailbox ([`crate::gamepad::PadBootstrap`]).
    pub fn mouse_boot_name(index: u8) -> String {
        alloc::format!("Global\\pfmouse-boot-{index}")
    }

    /// HID identity ("PF" / "MO") — obviously virtual; no software matches on it, unlike the
    /// pads' cloned Sony/Valve ids.
    pub const MOUSE_VID: u16 = 0x5046;
    pub const MOUSE_PID: u16 = 0x4D4F;
    pub const MOUSE_VER: u16 = 0x0100;

    /// Input report id `0x01`: `[id, buttons(5 bits), x_lo, x_hi, y_lo, y_hi, wheel, pan]` —
    /// absolute X/Y over `0..=`[`MOUSE_ABS_MAX`], relative wheel/pan.
    pub const MOUSE_REPORT_ID: u8 = 0x01;
    pub const MOUSE_REPORT_LEN: usize = 8;
    /// Logical maximum of the absolute X/Y axes (15-bit, HID-descriptor convention).
    pub const MOUSE_ABS_MAX: u16 = 0x7FFF;

    /// Build the 8-byte input report. Pure so the layout is unit-tested here (the driver
    /// workspace is `panic = "abort"`); the driver only ferries these bytes.
    #[must_use]
    pub fn input_report(buttons: u8, x: u16, y: u16, wheel: i8, pan: i8) -> [u8; MOUSE_REPORT_LEN] {
        let x = x.min(MOUSE_ABS_MAX);
        let y = y.min(MOUSE_ABS_MAX);
        [
            MOUSE_REPORT_ID,
            buttons & 0x1F,
            (x & 0xFF) as u8,
            (x >> 8) as u8,
            (y & 0xFF) as u8,
            (y >> 8) as u8,
            wheel as u8,
            pan as u8,
        ]
    }

    /// Virtual-mouse shared section (64 B). Host writes a report then bumps `in_seq` (Release);
    /// the driver's timer Acquire-loads it and completes a pended `READ_REPORT`. Idle generates
    /// no HID traffic — a constant report stream would read as user activity to the OS.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug)]
    pub struct MouseShm {
        pub magic: u32,
        /// Bumped AFTER `report` is in place (Release). `0` = nothing published yet.
        pub in_seq: u32,
        pub report: [u8; MOUSE_REPORT_LEN],
        /// [`crate::gamepad::GAMEPAD_PROTO_VERSION`] while attached. `0` = no driver.
        pub driver_proto: u32,
        /// Bumped each timer tick — advances whether or not input flows.
        pub driver_heartbeat: u32,
        /// Device index (host-stamped before the magic); driver checks it against the devnode Location.
        pub pad_index: u32,
        pub _reserved: [u8; 36],
    }

    // Offsets are the cross-process wire contract — pin every one.
    const _: () = {
        use core::mem::{offset_of, size_of};

        assert!(size_of::<MouseShm>() == 64);
        assert!(offset_of!(MouseShm, magic) == 0);
        assert!(offset_of!(MouseShm, in_seq) == 4);
        assert!(offset_of!(MouseShm, report) == 8);
        assert!(offset_of!(MouseShm, driver_proto) == 16);
        assert!(offset_of!(MouseShm, driver_heartbeat) == 20);
        assert!(offset_of!(MouseShm, pad_index) == 24);
    };
}

/// Hardware-cursor channel: one unnamed file mapping per monitor, delivered by handle value
/// ([`control::IOCTL_SET_CURSOR_CHANNEL`]). The driver's cursor thread seqlock-writes shape +
/// position; the host reads at encode-tick pace — no event crosses the boundary. Writer: bump
/// [`CursorShm::seq`] odd, write, bump even. Reader: retry while odd, copy, re-read — unchanged
/// ⇒ consistent snapshot. Position-only updates never touch shape bytes, so a reader that
/// skips unchanged `shape_id`s never copies torn pixels.
pub mod cursor {
    use bytemuck::{Pod, Zeroable};

    /// [`CursorShm`] magic (`b"PFCU"` LE); anything else = not attached yet.
    pub const CURSOR_MAGIC: u32 = u32::from_le_bytes(*b"PFCU");

    /// Max cursor side (px) declared to the OS (`IDDCX_CURSOR_CAPS::MaxX/MaxY`). Windows XL
    /// accessibility cursors top out here; the host's wire forwarder downscales anyway.
    pub const CURSOR_SHAPE_MAX: u32 = 256;

    /// Shape-buffer bytes: 32-bpp at the declared max.
    pub const CURSOR_SHAPE_BYTES: usize = (CURSOR_SHAPE_MAX * CURSOR_SHAPE_MAX * 4) as usize;

    /// Byte offset of the shape pixels (64-byte header).
    pub const CURSOR_SHAPE_OFFSET: usize = 64;

    pub const CURSOR_SHM_SIZE: usize = CURSOR_SHAPE_OFFSET + CURSOR_SHAPE_BYTES;

    /// `IDDCX_CURSOR_SHAPE_TYPE` values. The driver writes the OS value into [`CursorShm::cursor_type`].
    pub const CURSOR_TYPE_MASKED_COLOR: u32 = 1;
    pub const CURSOR_TYPE_ALPHA: u32 = 2;

    /// Section header; shape pixels follow at [`CURSOR_SHAPE_OFFSET`]. `x`/`y` are the shape's
    /// top-left in desktop coordinates (IddCx `IDARG_OUT_QUERY_HWCURSOR::X/Y` — position −
    /// hotspot, can be negative); both readers subtract the host-stamped `origin_*`.
    /// `shape_id` bumps on every shape set. Pixels are 32-bpp rows at `pitch` (BGRA for
    /// ALPHA; color+mask for MASKED_COLOR); [`shape_rgba`] converts them on either side.
    #[repr(C)]
    #[derive(Clone, Copy, Pod, Zeroable, Debug, PartialEq, Eq)]
    pub struct CursorShm {
        pub magic: u32,
        /// Seqlock: odd = writer mid-update.
        pub seq: u32,
        pub visible: u32,
        pub cursor_type: u32,
        pub x: i32,
        pub y: i32,
        pub shape_id: u32,
        pub width: u32,
        pub height: u32,
        pub pitch: u32,
        pub hot_x: u32,
        pub hot_y: u32,
        /// Host-stamped before the magic: the monitor's top-left on the desktop.
        pub origin_x: i32,
        pub origin_y: i32,
        /// Host-stamped `f32` bits: where the HDR desktop puts SDR white (1.0 = 80 nits), for
        /// the driver's blend onto an FP16 frame. `0` = not stamped, the driver uses 1.0.
        pub sdr_white_scale: u32,
        pub _reserved: u32,
    }

    /// One shape as straight-alpha RGBA, `w * h * 4` bytes.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct ShapeRgba {
        pub rgba: alloc::vec::Vec<u8>,
        pub w: u32,
        pub h: u32,
        pub hot_x: u32,
        pub hot_y: u32,
    }

    /// The part of a `w`×`h` cursor shape drawn at `(x, y)` that lands on a `width`×`height`
    /// target: `(x, y, w, h)` clipped to it, or `None` when none of it does. The shape's
    /// top-left is in target coordinates and may be negative — the pointer half off an edge.
    ///
    /// What a save-under of the blend has to copy, and put back. The driver's blend is
    /// Windows-only; the rule lives here so it is covered everywhere.
    #[must_use]
    pub fn clip_rect(
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        width: u32,
        height: u32,
    ) -> Option<(u32, u32, u32, u32)> {
        let (x0, y0) = (i64::from(x).max(0), i64::from(y).max(0));
        let x1 = (i64::from(x) + i64::from(w)).min(i64::from(width));
        let y1 = (i64::from(y) + i64::from(h)).min(i64::from(height));
        (x1 > x0 && y1 > y0).then(|| (x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32))
    }

    /// `(width, rows, pitch)` of the shape bytes a reader copies out for `hdr`, clamped to the
    /// section so a corrupt header can never index past it.
    #[must_use]
    pub fn shape_extent(hdr: &CursorShm) -> (usize, usize, usize) {
        let rows = hdr.height.min(CURSOR_SHAPE_MAX) as usize;
        let width = hdr.width.min(CURSOR_SHAPE_MAX) as usize;
        let pitch = (hdr.pitch as usize).min(CURSOR_SHAPE_BYTES / rows.max(1));
        (width, rows, pitch)
    }

    /// Pack the pitch-strided 32-bpp rows of `raw` (at least `rows * pitch` bytes, see
    /// [`shape_extent`]) into straight RGBA. ALPHA is BGRA (swap R↔B). MASKED_COLOR: `alpha ==
    /// 0` is opaque color; `0xFF` XORs the screen with the color. XOR with black is the
    /// transparent field around a monochrome shape (the I-beam is mostly that); XOR with
    /// anything else is an inversion, which no blend can honor — mid-gray keeps it visible.
    #[must_use]
    pub fn shape_rgba(hdr: &CursorShm, raw: &[u8]) -> ShapeRgba {
        let (width, rows, pitch) = shape_extent(hdr);
        let masked = hdr.cursor_type == CURSOR_TYPE_MASKED_COLOR;
        let mut rgba = alloc::vec::Vec::with_capacity(width * rows * 4);
        for y in 0..rows {
            let row = raw.get(y * pitch..).unwrap_or(&[]);
            for x in 0..width {
                let o = x * 4;
                let Some(px) = row.get(o..o + 4) else {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                };
                let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
                if masked {
                    if a == 0 {
                        rgba.extend_from_slice(&[r, g, b, 0xFF]);
                    } else if (r, g, b) == (0, 0, 0) {
                        rgba.extend_from_slice(&[0, 0, 0, 0]);
                    } else {
                        rgba.extend_from_slice(&[0x80, 0x80, 0x80, 0xB4]);
                    }
                } else {
                    rgba.extend_from_slice(&[r, g, b, a]);
                }
            }
        }
        ShapeRgba {
            rgba,
            w: width as u32,
            h: rows as u32,
            hot_x: hdr.hot_x.min(width.saturating_sub(1) as u32),
            hot_y: hdr.hot_y.min(rows.saturating_sub(1) as u32),
        }
    }

    // Layout is load-bearing across the process boundary — pin it.
    const _: () = {
        use core::mem::{offset_of, size_of};
        assert!(size_of::<CursorShm>() == 64);
        assert!(size_of::<CursorShm>() <= CURSOR_SHAPE_OFFSET);
        assert!(offset_of!(CursorShm, magic) == 0);
        assert!(offset_of!(CursorShm, seq) == 4);
        assert!(offset_of!(CursorShm, visible) == 8);
        assert!(offset_of!(CursorShm, cursor_type) == 12);
        assert!(offset_of!(CursorShm, x) == 16);
        assert!(offset_of!(CursorShm, y) == 20);
        assert!(offset_of!(CursorShm, shape_id) == 24);
        assert!(offset_of!(CursorShm, width) == 28);
        assert!(offset_of!(CursorShm, height) == 32);
        assert!(offset_of!(CursorShm, pitch) == 36);
        assert!(offset_of!(CursorShm, hot_x) == 40);
        assert!(offset_of!(CursorShm, hot_y) == 44);
        assert!(offset_of!(CursorShm, origin_x) == 48);
        assert!(offset_of!(CursorShm, origin_y) == 52);
        assert!(offset_of!(CursorShm, sdr_white_scale) == 56);
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    /// A pad served at the hardware cadence advances its counter by one and its timestamps by the
    /// real period per report.
    #[test]
    fn sony_clock_advances_like_hardware() {
        use gamepad::*;
        let mut ds = [0xAAu8; 64];
        assert!(stamp_sony_clock(DEVTYPE_DUALSENSE, &mut ds, 5, 8_000));
        assert_eq!(ds[7], 5);
        let le = |r: &[u8; 64], at: usize| u32::from_le_bytes(r[at..at + 4].try_into().unwrap());
        assert_eq!(le(&ds, 12), 5, "packet sequence");
        assert_eq!(le(&ds, 28), 24_000);
        assert_eq!(le(&ds, 49), 29_900, "second timer");
        assert_eq!(
            ds[27], 0xAA,
            "the gyro/accel bytes before the timestamp stay the host's"
        );
        assert_eq!(ds[53], 0xAA, "battery byte stays the host's");

        // Every clock advances on every report, as on hardware.
        let mut next = ds;
        stamp_sony_clock(DEVTYPE_DUALSENSE, &mut next, 6, 12_000);
        for at in [12, 28, 49] {
            assert!(le(&next, at) > le(&ds, at), "field at {at} did not advance");
        }

        let mut edge = [0u8; 64];
        assert!(stamp_sony_clock(
            DEVTYPE_DUALSENSE_EDGE,
            &mut edge,
            256 + 3,
            4_000
        ));
        assert_eq!(edge[7], 3, "the counter wraps at a byte");

        let mut ds4 = [0u8; 64];
        ds4[7] = 0x03; // PS + touchpad click held
        assert!(stamp_sony_clock(
            DEVTYPE_DUALSHOCK4,
            &mut ds4,
            64 + 2,
            16_000
        ));
        assert_eq!(
            ds4[7],
            0x03 | (2 << 2),
            "counter wraps at six bits, buttons survive"
        );
        assert_eq!(u16::from_le_bytes([ds4[10], ds4[11]]), 3_000);
        assert_eq!(ds4[34], 3_000u16 as u8);

        let mut xbox = [0x11u8; 64];
        assert!(!stamp_sony_clock(DEVTYPE_XBOX, &mut xbox, 1, 4_000));
        assert_eq!(xbox, [0x11u8; 64]);
    }

    /// Serves land one period apart on a fine timer, and a coarse timer restarts the schedule
    /// instead of bursting reports to catch up.
    #[test]
    fn serves_at_the_hardware_period() {
        use gamepad::*;
        let p = REPORT_PERIOD_US;
        assert_eq!(serve_due(0, 0), Some(p));
        assert_eq!(
            serve_due(2_000, p),
            None,
            "a 2 ms tick between slots serves nothing"
        );
        assert_eq!(
            serve_due(p + 100, p),
            Some(2 * p),
            "a slightly late tick keeps the grid"
        );
        assert_eq!(
            serve_due(p + 15_600, p),
            Some(p + 15_600 + p),
            "a coarse tick restarts it"
        );
    }

    #[test]
    fn dtd_encodes_the_session_mode() {
        // 1920×1080@60 with the fixed RB blanking: totals 2000×1125 → 135.00 MHz = 13500 × 10 kHz.
        let d = edid::dtd(1920, 1080, 60).expect("1080p60 fits the DTD encoding");
        assert_eq!(u16::from_le_bytes([d[0], d[1]]), 13_500);
        // Active dimensions round-trip through the split 8+4-bit fields.
        assert_eq!(u32::from(d[2]) | (u32::from(d[4] >> 4) << 8), 1920);
        assert_eq!(u32::from(d[5]) | (u32::from(d[7] >> 4) << 8), 1080);
        // Blanking fields carry the fixed geometry; flags match the legacy descriptor.
        assert_eq!(u32::from(d[3]) | (u32::from(d[4] & 0x0F) << 8), 80);
        assert_eq!(u32::from(d[6]) | (u32::from(d[7] & 0x0F) << 8), 45);
        assert_eq!(d[17], 0x1E);
    }

    #[test]
    fn dtd_rejects_what_the_encoding_cannot_carry() {
        // 4K120: (3840+80)·(2160+45)·120 ≈ 1.037 GHz — past the u16 10 kHz pixel-clock field.
        assert_eq!(edid::dtd(3840, 2160, 120), None);
        // 4K60 fits (≈518 MHz).
        assert!(edid::dtd(3840, 2160, 60).is_some());
        // Degenerate and over-wide modes are refused, not mis-encoded.
        assert_eq!(edid::dtd(0, 1080, 60), None);
        assert_eq!(edid::dtd(5000, 1080, 10), None);
    }

    /// Sum of a 128-byte EDID block: the trailing checksum byte is what drives this to 0.
    fn block_sum(block: &[u8]) -> u8 {
        block.iter().fold(0u8, |acc, &b| acc.wrapping_add(b))
    }

    #[test]
    fn edid_matches_the_golden_bytes() {
        // Byte-for-byte capture of what the driver shipped before this code moved here. One byte
        // out of place and Windows drops HDR, so nothing below may drift silently. Two bytes DO
        // differ from that capture on purpose: 0x15/0x16 now size the panel from the mode
        // (2560x1440 -> 67x38 cm), which moves the checksum at 0x7F by the same 24.
        #[rustfmt::skip]
        const GOLDEN: [u8; 256] = [
            0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00,
            0x41, 0xCB, 0x01, 0x00, 0x07, 0x00, 0x00, 0x00,
            0xFF, 0x21, 0x01, 0x04, 0xB0, 0x43, 0x26, 0x78,
            0x03, 0x78, 0xB1, 0xB5, 0x4A, 0x2B, 0xCC, 0x21,
            0x0B, 0x50, 0x54, 0x00, 0x00, 0x00, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0xC4, 0xB7,
            0x00, 0x50, 0xA0, 0xA0, 0x2D, 0x50, 0x08, 0x20,
            0x35, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E,
            0x00, 0x00, 0x00, 0xFD, 0x08, 0x17, 0xF0, 0x0F,
            0xFF, 0xFF, 0x00, 0x0A, 0x20, 0x20, 0x20, 0x20,
            0x20, 0x20, 0x00, 0x00, 0x00, 0xFC, 0x00, 0x50,
            0x75, 0x6E, 0x6B, 0x74, 0x66, 0x75, 0x6E, 0x6B,
            0x0A, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x27,
            0x02, 0x03, 0x0F, 0x00, 0xE3, 0x05, 0x80, 0x00,
            0xE6, 0x06, 0x05, 0x01, 0x72, 0x52, 0x0F, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xBF,
        ];
        let lum = edid::ClientLuminance {
            max_nits: 600,
            max_frame_avg_nits: 300,
            min_millinits: 20,
        };
        assert_eq!(edid::generate(7, lum, Some((2560, 1440, 120))), GOLDEN);
    }

    /// The declared panel size must track the mode, or the display's DPI rides its resolution:
    /// a fixed 50 cm reads as ~97 DPI at 1080p but ~260 at 5120 wide, and the OS then scales the
    /// desktop (and the cursor) for a panel we only claimed to be.
    #[test]
    fn the_declared_panel_size_keeps_every_mode_near_96_dpi() {
        for (w, h) in [(1920u32, 1080u32), (2560, 1440), (3840, 2160), (5120, 1440)] {
            let e = edid::generate(1, edid::ClientLuminance::default(), Some((w, h, 60)));
            let (cm_w, cm_h) = (u32::from(e[0x15]), u32::from(e[0x16]));
            assert!(cm_w > 0 && cm_h > 0, "{w}x{h}: zero means undefined");
            // dpi = px / (cm / 2.54). Rounding to whole centimetres is the only error here.
            let dpi_w = w * 254 / (cm_w * 100);
            let dpi_h = h * 254 / (cm_h * 100);
            assert!(
                (92..=100).contains(&dpi_w) && (92..=100).contains(&dpi_h),
                "{w}x{h} declared {cm_w}x{cm_h} cm = {dpi_w}x{dpi_h} DPI, wanted ~96"
            );
        }
    }

    #[test]
    fn edid_blocks_checksum_to_zero() {
        let lum = edid::ClientLuminance {
            max_nits: 1000,
            max_frame_avg_nits: 400,
            min_millinits: 50,
        };
        for (serial, l, mode) in [
            (0, edid::ClientLuminance::default(), None),
            (1, lum, Some((1920, 1080, 60))),
            (u32::MAX, lum, Some((3840, 2160, 120))),
        ] {
            let e = edid::generate(serial, l, mode);
            assert_eq!(block_sum(&e[..128]), 0, "base block, serial {serial}");
            assert_eq!(block_sum(&e[128..]), 0, "CTA block, serial {serial}");
        }
    }

    #[test]
    fn edid_serial_lands_at_0x0c_and_rechecksums() {
        for serial in [0u32, 1, 7, 0x00FF_00FF, u32::MAX] {
            let e = edid::generate(serial, edid::ClientLuminance::default(), None);
            assert_eq!(e[0x0C..0x10], serial.to_le_bytes());
            assert_eq!(edid::get_serial(&e).unwrap(), serial);
            // The base block alone is what the mode callbacks sometimes get handed back.
            assert_eq!(edid::get_serial(&e[..128]).unwrap(), serial);
            assert_eq!(block_sum(&e[..128]), 0);
        }
        // A short descriptor is rejected, not read out of bounds.
        assert!(edid::get_serial(&[0u8; 8]).is_err());
    }

    #[test]
    fn edid_swaps_the_preferred_dtd_only_when_the_mode_fits() {
        let lum = edid::ClientLuminance::default();
        let placeholder = edid::generate(1, lum, None);
        // The stock descriptor is the 148.50 MHz 1080p60 timing baked into the base block.
        assert_eq!(
            u16::from_le_bytes([placeholder[54], placeholder[55]]),
            14_850
        );
        // A mode that fits replaces all 18 bytes; 4K120 does not fit, so the stock one stays.
        let swapped = edid::generate(1, lum, Some((2560, 1440, 120)));
        assert_eq!(swapped[54..72], edid::dtd(2560, 1440, 120).unwrap());
        let too_fast = edid::generate(1, lum, Some((3840, 2160, 120)));
        assert_eq!(too_fast[54..72], placeholder[54..72]);
    }

    #[test]
    fn edid_hdr_block_tracks_the_client_volume() {
        // No client volume reported: the built-in ~993 / 400 / 0.05 nit defaults stay.
        let stock = edid::generate(1, edid::ClientLuminance::default(), None);
        assert_eq!(stock[136..143], [0xE6, 0x06, 0x05, 0x01, 0x8A, 0x60, 0x12]);
        // A known peak overrides all three; the EOTF and descriptor bytes never move.
        let lum = edid::ClientLuminance {
            max_nits: 400,
            max_frame_avg_nits: 400,
            min_millinits: 50,
        };
        let coded = edid::generate(1, lum, None);
        assert_eq!(coded[136..140], [0xE6, 0x06, 0x05, 0x01]);
        assert_eq!(coded[140], edid::cta_max_luminance_code(400));
        // Unknown frame-average and unknown min both code as 0 = "no data" on the wire.
        let partial = edid::generate(
            1,
            edid::ClientLuminance {
                max_nits: 400,
                ..Default::default()
            },
            None,
        );
        assert_eq!(partial[141], 0);
        assert_eq!(partial[142], 0);
    }

    /// The wire numbering, which the host writes and the driver dispatches on. A silent
    /// renumber swaps one encoder for another on a shipped driver, so pin it here.
    #[test]
    fn backend_and_codec_ids_are_the_wire_numbering() {
        use encode::{backend as be, codec as cc};
        assert_eq!(
            [
                be::NVENC,
                be::AMF,
                be::QSV,
                be::PYROWAVE,
                be::MEDIA_FOUNDATION
            ],
            [1, 2, 3, 4, 5]
        );
        assert_eq!([cc::H264, cc::HEVC, cc::AV1, cc::PYROWAVE], [1, 2, 3, 4]);
        // The driver indexes NAMES by `id - 1` and replies with the entry it found.
        assert_eq!(be::name(be::MEDIA_FOUNDATION), Some("mf"));
        assert_eq!(be::NAMES[be::NVENC as usize - 1], "nvenc");
        assert_eq!(cc::name(cc::AV1), Some("av1"));
        assert_eq!(be::name(0), None, "0 terminates a list, it names nothing");
        assert_eq!(be::name(6), None);
    }

    /// `listed` guards a 0-terminated preference list; `valid` guards a single required field.
    /// Reading either as the other would let a `SET_ENCODE` through with no codec at all.
    #[test]
    fn a_terminator_is_listable_but_never_a_valid_codec() {
        use encode::{backend as be, codec as cc};
        assert!(be::listed(0), "the list terminator");
        assert!(be::listed(be::MEDIA_FOUNDATION), "the widest id");
        assert!(!be::listed(6));
        assert!(!cc::valid(0), "a codec field is required");
        assert!(cc::valid(cc::PYROWAVE));
        assert!(!cc::valid(5));
    }

    /// One advertised resolution, spelled short enough for the mode-list tests to read.
    fn mode(width: u32, height: u32, refresh_rates: &[u32]) -> vdisplay::Mode {
        vdisplay::Mode {
            width,
            height,
            refresh_rates: refresh_rates.to_vec(),
        }
    }

    #[test]
    fn default_modes_lead_with_1080p_then_720p() {
        let d = vdisplay::default_modes();
        assert_eq!(
            d,
            vec![mode(1920, 1080, &[60, 120]), mode(1280, 720, &[60])]
        );
        // flatten walks resolutions in list order, refresh rates within each.
        let flat: Vec<_> = vdisplay::flatten(&d)
            .map(|i| (i.width, i.height, i.refresh_rate))
            .collect();
        assert_eq!(flat, [(1920, 1080, 60), (1920, 1080, 120), (1280, 720, 60)]);
    }

    /// The `USE_SMALLEST_MODE` rule: the OS drives a seat at the smallest advertised mode, so a
    /// seat must advertise its request alone. A fallback or a surviving larger entry would pin it.
    #[test]
    fn a_seat_advertises_only_what_the_client_asked_for() {
        let asked = mode(2560, 1440, &[120]);
        let history = vec![mode(3840, 2160, &[60]), mode(1024, 768, &[60])];

        let seat = vdisplay::advertised_modes(asked.clone(), true, &history);
        assert_eq!(seat, vec![asked.clone()], "a seat offers one mode");
        let smallest = vdisplay::flatten(&seat)
            .min_by_key(|i| i.width * i.height)
            .expect("a non-empty list");
        assert_eq!((smallest.width, smallest.height), (2560, 1440));

        // A host keeps the request first, then the fallbacks, then its history — and 1024x768
        // proves the history really does ride along.
        let host = vdisplay::advertised_modes(asked.clone(), false, &history);
        assert_eq!(host[0], asked);
        assert!(host.contains(&mode(1280, 720, &[60])), "fallbacks");
        assert!(host.contains(&mode(1024, 768, &[60])), "history");
        assert!(
            host.len() <= vdisplay::MODE_LIST_CAP,
            "the union is capped: {}",
            host.len()
        );
    }

    /// Create passes no history; the seat rule still holds and the host still gets its fallbacks.
    #[test]
    fn advertised_modes_without_history_is_the_create_path() {
        let asked = mode(800, 600, &[60]);
        assert_eq!(
            vdisplay::advertised_modes(asked.clone(), true, &[]),
            vec![asked.clone()]
        );
        let host = vdisplay::advertised_modes(asked.clone(), false, &[]);
        assert_eq!(host, [vec![asked], vdisplay::default_modes()].concat());
    }

    #[test]
    fn union_modes_dedupes_by_resolution_and_stops_at_the_cap() {
        // The head keeps its place, and a duplicate resolution is dropped WHOLE: the incoming
        // 60/120 Hz rates are lost rather than merged into the 144 Hz head already there.
        let mut into = vec![mode(1920, 1080, &[144])];
        vdisplay::union_modes(
            &mut into,
            &[mode(1920, 1080, &[60, 120]), mode(1280, 720, &[60])],
        );
        assert_eq!(into, vec![mode(1920, 1080, &[144]), mode(1280, 720, &[60])]);

        // At the cap nothing is appended, and nothing already accumulated is truncated.
        let cap = vdisplay::MODE_LIST_CAP;
        let mut full: Vec<_> = (0..cap as u32).map(|i| mode(640 + i, 480, &[60])).collect();
        vdisplay::union_modes(&mut full, &[mode(3840, 2160, &[60])]);
        assert_eq!(full.len(), cap);
        assert!(!full.contains(&mode(3840, 2160, &[60])));

        // One slot free: the first new resolution takes it and the rest fall off — the merge
        // stops AT the cap, so it is the later candidates that are lost.
        let mut nearly: Vec<_> = (0..cap as u32 - 1)
            .map(|i| mode(640 + i, 480, &[60]))
            .collect();
        vdisplay::union_modes(
            &mut nearly,
            &[mode(3840, 2160, &[60]), mode(2560, 1440, &[60])],
        );
        assert_eq!(nearly.len(), cap);
        assert_eq!(nearly[cap - 1], mode(3840, 2160, &[60]));
    }

    #[test]
    fn monitor_ids_honour_the_preferred_then_take_the_lowest_free() {
        // A free preferred id inside the connector range wins.
        assert_eq!(vdisplay::resolve_id(&[1, 2], 7), 7);
        // A collision falls back to auto — the live holder is never displaced.
        assert_eq!(vdisplay::resolve_id(&[1, 2], 2), 3);
        // 0 (anonymous / TOFU / GameStream) and 16+ (past MaxMonitorsSupported) fall back too.
        assert_eq!(vdisplay::resolve_id(&[1, 2], 0), 3);
        assert_eq!(vdisplay::resolve_id(&[1, 2], 16), 3);

        // Lowest free, not next-highest: a departed monitor's id is refilled.
        assert_eq!(vdisplay::alloc_monitor_id(&[]), 1);
        assert_eq!(vdisplay::alloc_monitor_id(&[1, 3, 4]), 2);

        // Pigeonhole over `1..=len + 1` always finds one — but is NOT clamped to the 1..=15
        // range `resolve_id` enforces for a preferred id, so a full adapter allocates past it.
        let fifteen: Vec<u32> = (1..=15).collect();
        assert_eq!(vdisplay::alloc_monitor_id(&fifteen), 16);
        let sixteen: Vec<u32> = (1..=16).collect();
        assert_eq!(vdisplay::alloc_monitor_id(&sixteen), 17);
    }

    #[test]
    fn container_guids_are_unique_per_monitor_id() {
        let all: Vec<_> = (1u32..=16).map(vdisplay::container_guid).collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Only Data1 and the last two Data4 bytes carry the id; the rest is a fixed prefix.
        assert_eq!(
            vdisplay::container_guid(1),
            (
                0x7066_7665,
                0x7044,
                0x5350,
                [0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x00, 0x01]
            )
        );
        assert_eq!(vdisplay::container_guid(0x1234).3[6..], [0x12, 0x34]);
    }

    #[test]
    fn signal_info_numerics_match_the_ddi_formula() {
        // Monitor modes pass divider 0, target modes 1 — the ONLY difference between the two.
        let m = vdisplay::signal_info(1920, 1080, 60, 0);
        assert_eq!(m.pixel_rate, 60 * 1920 * 1080);
        assert_eq!(m.h_sync_num, 60 * 1080);
        assert_eq!(m.v_sync_num, 60);
        assert_eq!(m.video_standard, 255);
        assert_eq!(
            vdisplay::signal_info(1920, 1080, 60, 1).video_standard,
            255 | (1 << 16)
        );

        // 8K240's pixel rate (7.96e9) is why that field is u64; the hSync numerator still fits
        // a u32 with orders of magnitude to spare.
        let big = vdisplay::signal_info(7680, 4320, 240, 1);
        assert_eq!(big.pixel_rate, 7_962_624_000);
        assert!(big.pixel_rate > u64::from(u32::MAX));
        assert_eq!(big.h_sync_num, 240 * 4320);

        // The hSync saturation is unreachable through `valid_mode` (16384 · 1000 fits); it only
        // keeps an unvalidated tuple from panicking inside the extern-"C" mode DDI.
        assert!(vdisplay::signal_info(16384, 16384, 1000, 0).h_sync_num < u32::MAX);
        assert_eq!(
            vdisplay::signal_info(1, u32::MAX, u32::MAX, 0).h_sync_num,
            u32::MAX
        );
    }

    #[test]
    fn valid_mode_takes_the_edges_and_rejects_zero() {
        assert!(vdisplay::valid_mode(1, 1, 1));
        assert!(vdisplay::valid_mode(1920, 1080, 60));
        assert!(vdisplay::valid_mode(16384, 16384, 1000));
        for bad in [
            (0, 1080, 60),
            (1920, 0, 60),
            (1920, 1080, 0),
            (16385, 1080, 60),
            (1920, 16385, 60),
            (1920, 1080, 1001),
        ] {
            assert!(!vdisplay::valid_mode(bad.0, bad.1, bad.2), "{bad:?}");
        }
    }

    #[test]
    fn frame_token_roundtrips() {
        for (g, s, slot) in [
            (1u32, 0u32, 0u8),
            (5, 12_345, 3),
            (encode::FrameToken::GENERATION_MASK, 0xFFFF_FFFF, 5),
            (0, 1, 255),
        ] {
            let t = encode::FrameToken {
                generation: g,
                seq: s,
                slot,
            };
            assert_eq!(encode::FrameToken::unpack(t.pack()), t);
        }
    }

    #[test]
    fn frame_token_packing_matches_legacy_layout() {
        // Packing was `(gen<<40)|(seq<<8)|slot` by hand; lock the bit positions.
        let t = encode::FrameToken {
            generation: 7,
            seq: 42,
            slot: 3,
        };
        assert_eq!(t.pack(), (7u64 << 40) | (42u64 << 8) | 3u64);
    }

    #[test]
    fn control_structs_roundtrip_through_bytes() {
        let req = control::AddRequest {
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
        assert_eq!(*bytemuck::from_bytes::<control::AddRequest>(bytes), req);
        // preferred_monitor_id occupies the old `_reserved` slot at offset 20.
        assert_eq!(bytes[20..24], 7u32.to_le_bytes());
        // Luminance tail rides after the legacy boundary; a zero-filled tail decodes as unknown.
        assert_eq!(bytes[24..28], 800u32.to_le_bytes());
        let mut legacy = [0u8; 40];
        legacy[..control::ADD_REQUEST_LEGACY_SIZE]
            .copy_from_slice(&bytes[..control::ADD_REQUEST_LEGACY_SIZE]);
        // `pod_read_unaligned`, not `from_bytes`: `legacy` is `[u8; 40]` (align 1) but
        // `AddRequest` is align 8. `from_bytes` panics unless the buffer happens to be 8-aligned.
        let old = bytemuck::pod_read_unaligned::<control::AddRequest>(&legacy);
        assert_eq!(old.preferred_monitor_id, 7);
        assert_eq!(
            (
                old.max_luminance_nits,
                old.max_frame_avg_nits,
                old.min_luminance_millinits
            ),
            (0, 0, 0)
        );

        let reply = control::AddReply {
            adapter_luid_low: 0x1234_5678,
            adapter_luid_high: -2,
            target_id: 262,
            resolved_monitor_id: 7,
            wudf_pid: 4242,
            cursor_excluded: 1,
        };
        let rbytes = bytemuck::bytes_of(&reply);
        assert_eq!(rbytes.len(), 24);
        assert_eq!(*bytemuck::from_bytes::<control::AddReply>(rbytes), reply);
        // resolved_monitor_id occupies the old `_reserved` slot at offset 12.
        assert_eq!(rbytes[12..16], 7u32.to_le_bytes());
        // Duplication-target pid trails at offset 16.
        assert_eq!(rbytes[16..20], 4242u32.to_le_bytes());
        // cursor_excluded rides after the legacy boundary; a zero-filled tail reads as unknown.
        assert_eq!(rbytes[20..24], 1u32.to_le_bytes());
        assert_eq!(control::ADD_REPLY_LEGACY_SIZE, 20);
    }

    #[test]
    fn update_modes_request_roundtrips_and_versions_cohere() {
        let req = control::UpdateModesRequest {
            session_id: 42,
            width: 2560,
            height: 1409, // arbitrary — the in-place path serves window-drag modes
            refresh_hz: 120,
            _reserved: 0,
        };
        let bytes = bytemuck::bytes_of(&req);
        assert_eq!(bytes.len(), 24);
        assert_eq!(
            *bytemuck::from_bytes::<control::UpdateModesRequest>(bytes),
            req
        );
        assert_eq!(bytes[8..12], 2560u32.to_le_bytes());
        // v9 widened the AU slot, v8 scoped the driver to its owners, v7 replaced the video
        // transport; each makes the floor the version itself.
        assert_eq!(PROTOCOL_VERSION, 9);
        assert_eq!(MIN_DRIVER_PROTOCOL_VERSION, PROTOCOL_VERSION);
    }

    #[test]
    fn cursor_shm_layout_is_pinned() {
        use cursor::*;
        // Header must leave the shape offset intact whatever grows inside `_reserved`.
        assert_eq!(core::mem::size_of::<CursorShm>(), 64);
        assert_eq!(CURSOR_SHM_SIZE, 64 + 256 * 256 * 4);
        assert_eq!(CURSOR_MAGIC, u32::from_le_bytes(*b"PFCU"));
        let hdr = CursorShm {
            magic: CURSOR_MAGIC,
            seq: 2,
            visible: 1,
            cursor_type: CURSOR_TYPE_ALPHA,
            x: -3,
            y: 7,
            shape_id: 42,
            width: 32,
            height: 32,
            pitch: 128,
            hot_x: 4,
            hot_y: 5,
            origin_x: -1920,
            origin_y: 0,
            sdr_white_scale: 2.5f32.to_bits(),
            _reserved: 0,
        };
        let bytes = bytemuck::bytes_of(&hdr);
        assert_eq!(*bytemuck::from_bytes::<CursorShm>(bytes), hdr);
        assert_eq!(bytes[16..20], (-3i32).to_le_bytes());
        assert_eq!(bytes[48..52], (-1920i32).to_le_bytes());
        assert_eq!(f32::from_bits(hdr.sdr_white_scale), 2.5);
    }

    /// Both readers of the cursor section share one conversion: ALPHA swaps B↔R, MASKED
    /// turns the mask into opaque colour, the transparent field, or the mid-gray XOR stand-in,
    /// and a header whose extent exceeds the section is clamped rather than indexed.
    #[test]
    fn cursor_shape_converts_alpha_and_masked_rows() {
        use cursor::*;
        let hdr = CursorShm {
            cursor_type: CURSOR_TYPE_ALPHA,
            width: 2,
            height: 1,
            pitch: 16,
            hot_x: 9,
            hot_y: 9,
            ..CursorShm::zeroed()
        };
        // Two BGRA pixels, then pitch padding.
        let raw = [1u8, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        let s = shape_rgba(&hdr, &raw);
        assert_eq!(s.rgba, [3, 2, 1, 4, 7, 6, 5, 8]);
        assert_eq!((s.w, s.h, s.hot_x, s.hot_y), (2, 1, 1, 0));
        let masked = CursorShm {
            cursor_type: CURSOR_TYPE_MASKED_COLOR,
            width: 3,
            ..hdr
        };
        // Opaque colour, an inversion pixel, and the XOR-with-black field around a
        // monochrome shape — which must stay transparent, or an I-beam is a gray block.
        let raw = [1u8, 2, 3, 0, 5, 6, 7, 0xFF, 0, 0, 0, 0xFF];
        assert_eq!(
            shape_rgba(&masked, &raw).rgba,
            [3, 2, 1, 0xFF, 0x80, 0x80, 0x80, 0xB4, 0, 0, 0, 0]
        );
        // Short rows read as transparent; an oversized header stays inside the section.
        assert_eq!(
            shape_rgba(&hdr, &[9, 9, 9, 9]).rgba,
            [9, 9, 9, 9, 0, 0, 0, 0]
        );
        let huge = CursorShm {
            width: u32::MAX,
            height: u32::MAX,
            pitch: u32::MAX,
            ..hdr
        };
        let (w, rows, pitch) = shape_extent(&huge);
        assert_eq!((w, rows), (256, 256));
        assert!(rows * pitch <= CURSOR_SHAPE_BYTES);
    }

    #[test]
    fn gamepad_names_and_magics_are_stable() {
        assert_eq!(gamepad::xusb_boot_name(0), "Global\\pfxusb-boot-0");
        assert_eq!(gamepad::pad_boot_name(2), "Global\\pfds-boot-2");
        // Lock the exact u32 magics the shipped host/drivers use.
        assert_eq!(gamepad::XUSB_MAGIC, 0x5558_4650);
        assert_eq!(gamepad::PAD_MAGIC, 0x5046_4453);
        // "PFBT" little-endian.
        assert_eq!(gamepad::BOOT_MAGIC.to_le_bytes(), *b"PFBT");
    }

    #[test]
    fn pad_bootstrap_roundtrips_through_bytes() {
        let b = gamepad::PadBootstrap {
            magic: gamepad::BOOT_MAGIC,
            host_proto: gamepad::GAMEPAD_PROTO_VERSION,
            driver_pid: 1234,
            driver_proto: gamepad::GAMEPAD_PROTO_VERSION,
            data_handle: 0x0000_0000_0000_2a4c,
            handle_pid: 1234,
            handle_seq: 7,
        };
        let bytes = bytemuck::bytes_of(&b);
        assert_eq!(bytes.len(), 32);
        assert_eq!(*bytemuck::from_bytes::<gamepad::PadBootstrap>(bytes), b);
        // Handle value rides 8-aligned at offset 16; seq trails at 28 (written last).
        assert_eq!(bytes[16..24], 0x2a4cu64.to_le_bytes());
        assert_eq!(bytes[28..32], 7u32.to_le_bytes());
    }

    #[test]
    fn ctl_codes_are_contiguous_and_distinct() {
        assert_eq!(control::IOCTL_ADD, ctl_code(0x900));
        let all = [
            control::IOCTL_ADD,
            control::IOCTL_REMOVE,
            control::IOCTL_SET_RENDER_ADAPTER,
            control::IOCTL_PING,
            control::IOCTL_GET_INFO,
            control::IOCTL_CLEAR_ALL,
            control::IOCTL_UPDATE_MODES,
            control::IOCTL_SET_CURSOR_CHANNEL,
            control::IOCTL_SET_CURSOR_FORWARD,
            control::IOCTL_ENCODE_PROBE_ARM,
            control::IOCTL_ENCODE_PROBE_STATUS,
            encode::IOCTL_SET_ENCODE,
            encode::IOCTL_ENCODE_CTL,
            control::IOCTL_DRAIN_LOG,
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(encode::IOCTL_ENCODE_CTL, ctl_code(0x90D));
        assert_eq!(control::IOCTL_DRAIN_LOG, ctl_code(0x90E));
    }

    /// The driver writes these records and the host parses them in another process: a one-sided
    /// change to either half silently loses the encoder's diagnostics.
    #[test]
    fn log_records_roundtrip_and_stay_one_line_each() {
        let mut buf = alloc::vec::Vec::new();
        control::write_log_record(&mut buf, control::LOG_WARN, "AMF rejected 4:4:4");
        control::write_log_record(&mut buf, control::LOG_INFO, "two\nlines");
        control::write_log_record(&mut buf, control::LOG_ERROR, "");
        let got: alloc::vec::Vec<_> = control::log_lines(&buf).collect();
        assert_eq!(
            got,
            [
                (control::LOG_WARN, "AMF rejected 4:4:4"),
                (control::LOG_INFO, "two lines"),
                (control::LOG_ERROR, ""),
            ]
        );
        // Framing holds only while every record is exactly one line.
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 3);
    }

    #[test]
    fn encode_probe_structs_roundtrip_through_bytes() {
        let req = control::EncodeProbeRequest {
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
            bytemuck::pod_read_unaligned::<control::EncodeProbeRequest>(bytes),
            req
        );
        let mut name = [0u8; 32];
        name[..4].copy_from_slice(b"open");
        let reply = control::EncodeProbeReply {
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
            bytemuck::pod_read_unaligned::<control::EncodeProbeReply>(bytes),
            reply
        );
    }

    #[test]
    fn cta_luminance_codes_hit_the_reference_points() {
        // Historical built-in EDID block: 0x8A ≈ 993 nits, 0x60 = 400 nits (exact), 0x12 ≈ 0.05 nit.
        assert_eq!(edid::cta_max_millinits(0x60), 400_000); // 50·2^3 exactly
        assert_eq!(edid::cta_max_millinits(0x8A) / 1000, 993);
        assert_eq!(edid::cta_max_luminance_code(400), 0x60);
        // 0x8A decodes to 993.481 nits; 994 is the smallest whole-nit input that reaches it.
        assert_eq!(edid::cta_max_luminance_code(994), 0x8A);
        assert_eq!(edid::cta_min_luminance_code(50, 0x8A), 0x12); // 0.05 nits @ a 993-nit max
                                                                  // Never advertise brighter than the panel. 1000 nits sits between 138 (993) and 139 (~1015).
        assert_eq!(edid::cta_max_luminance_code(1000), 138);
        assert!(edid::cta_max_millinits(edid::cta_max_luminance_code(1000)) <= 1_000_000);
        // Every real code decodes at or below its input, within one step (~2.2%).
        // Starts above code 1's 51.094 nits — beneath that the documented clamp-to-1 wins.
        for nits in [52u32, 80, 120, 250, 400, 604, 800, 1_499, 4_000, 10_000] {
            let c = edid::cta_max_luminance_code(nits);
            let dec = edid::cta_max_millinits(c);
            assert!(dec <= nits as u64 * 1000, "{nits} → {c} decoded {dec}");
            assert!(
                dec * 1023 / 1000 >= nits as u64 * 1000,
                "{nits} → {c} more than a step low"
            );
        }
        // 0/tiny stays a valid on-wire code (callers gate on nits > 0); the ceiling saturates at 255.
        assert_eq!(edid::cta_max_luminance_code(0), 1);
        assert_eq!(edid::cta_max_luminance_code(u32::MAX), 255);
        // Min-luminance: 0 = unknown/true black stays 0; a floor brighter than the max clamps.
        assert_eq!(edid::cta_min_luminance_code(0, 0x8A), 0);
        assert_eq!(edid::cta_min_luminance_code(u32::MAX, 1), 255);
        // HDR400: max 400 nits / min 0.4 nits.
        let max_c = edid::cta_max_luminance_code(400);
        let min_c = edid::cta_min_luminance_code(400, max_c);
        // L_min = L_max·(cv/255)²/100 — must come back within ~10% of 0.4 nits.
        let back =
            edid::cta_max_millinits(max_c) * (min_c as u64 * min_c as u64) / (255 * 255) / 100;
        assert!((360..=440).contains(&back), "min decoded {back} millinits");
    }

    #[test]
    fn mouse_report_and_names_are_stable() {
        assert_eq!(mouse::mouse_boot_name(0), "Global\\pfmouse-boot-0");
        // "PFMO" LE, and never colliding with a pad magic.
        assert_eq!(mouse::MOUSE_MAGIC.to_le_bytes(), *b"PFMO");
        assert_ne!(mouse::MOUSE_MAGIC, gamepad::XUSB_MAGIC);
        assert_ne!(mouse::MOUSE_MAGIC, gamepad::PAD_MAGIC);
        let r = mouse::input_report(0b0000_0101, 0x1234, 0x7FFF, -3, 7);
        assert_eq!(r, [0x01, 0x05, 0x34, 0x12, 0xFF, 0x7F, 0xFD, 0x07]);
        // Axes clamp to the 15-bit logical max; buttons to the declared 5.
        let r = mouse::input_report(0xFF, 0xFFFF, 0, 0, 0);
        assert_eq!((r[1], r[2], r[3]), (0x1F, 0xFF, 0x7F));
        // A zeroed section reads as nothing published (`in_seq` 0).
        let shm = mouse::MouseShm::zeroed();
        assert_eq!(shm.in_seq, 0);
        assert_eq!(bytemuck::bytes_of(&shm).len(), 64);
    }

    #[test]
    fn guid_is_not_sudovda() {
        const SUDOVDA: u128 = 0xE5BC_C234_1E0C_418A_A0D4_EF8B_7501_414D;
        assert_ne!(PF_VDISPLAY_INTERFACE_GUID_U128, SUDOVDA);
    }

    /// Both wire forms (IOCTL struct and HID indexed-string) round-trip; malformed shapes refuse
    /// rather than half-parse into a pid.
    #[test]
    fn channel_proof_round_trips_in_both_wire_forms() {
        use gamepad::*;
        let proof = ChannelProof::new(2, 4242);
        assert_eq!(proof.magic, PROOF_MAGIC);
        assert_eq!(PROOF_MAGIC, u32::from_le_bytes(*b"PFCP"));
        assert_eq!(proof.proto, GAMEPAD_PROTO_VERSION);

        let bytes = bytemuck::bytes_of(&proof);
        assert_eq!(bytes.len(), 16);
        assert_eq!(*bytemuck::from_bytes::<ChannelProof>(bytes), proof);

        let s = proof.to_hid_string();
        assert_eq!(s, alloc::format!("PFCP:{GAMEPAD_PROTO_VERSION}:2:4242"));
        assert_eq!(ChannelProof::from_hid_string(&s), Some(proof));

        // Every malformed shape parses to None.
        for bad in [
            "",
            "PFCP",
            "PFCP:",
            "PFCP:3:0",        // truncated read
            "PFCP:3:0:4242:9", // trailing field we never mint
            "PFCP:3:0:-1",     // not a u32
            "PFCP:3:0:0x10",   // not decimal
            "PFCP:3:0: 4242",  // whitespace is not trimmed away into a valid pid
            "NOPE:3:0:4242",   // another driver answered this string index
            "pfcp:3:0:4242",   // prefix is case-sensitive
        ] {
            assert_eq!(
                ChannelProof::from_hid_string(bad),
                None,
                "malformed proof {bad:?} must not parse"
            );
        }
    }

    /// Pin each `check` refusal: foreign driver, version skew, wrong-devnode (would cross-wire pads).
    #[test]
    fn channel_proof_check_refuses_everything_it_should() {
        use gamepad::*;
        assert_eq!(ChannelProof::new(0, 1234).check(0), Ok(1234));
        assert_eq!(ChannelProof::new(3, 1234).check(3), Ok(1234));

        // Right shape, wrong pad: the interface lookup resolved another pad's devnode.
        assert!(ChannelProof::new(1, 1234).check(0).is_err());
        let mut foreign = ChannelProof::new(0, 1234);
        foreign.magic = 0xDEAD_BEEF;
        assert!(foreign.check(0).is_err());
        // Version skew must fail closed, not "probably compatible".
        let mut old = ChannelProof::new(0, 1234);
        old.proto = GAMEPAD_PROTO_VERSION - 1;
        assert!(old.check(0).is_err());
        // pid 0 is never a duplication target.
        assert!(ChannelProof::new(0, 0).check(0).is_err());
    }

    /// v2 driver answers no proof and a v2 host never asks — version must have moved.
    #[test]
    fn gamepad_proto_is_at_the_channel_proof_version() {
        assert_eq!(gamepad::GAMEPAD_PROTO_VERSION, 3);
    }

    /// Feature-report framing: report id in byte 0, proof in 1..17, zero pad; short reads refuse.
    #[test]
    fn channel_proof_feature_report_round_trips_and_refuses_short_reads() {
        use gamepad::*;
        let proof = ChannelProof::new(1, 4242);
        let rep = proof
            .to_feature_report(HID_FEATURE_REPORT_CHANNEL_PROOF, 64)
            .expect("64 bytes is plenty");
        assert_eq!(rep.len(), 64);
        assert_eq!(
            rep[0], 0x85,
            "byte 0 is the report id, as every HID feature reply is"
        );
        assert!(rep[17..].iter().all(|&b| b == 0), "tail is zero padding");
        assert_eq!(ChannelProof::from_feature_report(&rep), Some(proof));

        assert!(proof.to_feature_report(0x85, 17).is_some());
        assert!(proof.to_feature_report(0x85, 16).is_none());
        // A truncated read must not be zero-extended into a pid.
        assert_eq!(ChannelProof::from_feature_report(&rep[..16]), None);
        assert_eq!(ChannelProof::from_feature_report(&[]), None);

        // Two bytes so a stray Steam command cannot collide.
        assert_eq!(DECK_PROOF_CMD.len(), 2);
        assert!(!DECK_PROOF_CMD.starts_with(&[0x83]) && !DECK_PROOF_CMD.starts_with(&[0xAE]));
        assert!(!DECK_PROOF_CMD.starts_with(&[0xEB]) && !DECK_PROOF_CMD.starts_with(&[0x8F]));
    }

    #[test]
    fn triton_devtype_is_the_next_free_slot() {
        assert_eq!(gamepad::DEVTYPE_TRITON, 7);
    }

    /// GET reply echoes the last SET's command; a mismatch makes Steam drop the pad.
    #[test]
    fn triton_feature_reply_echoes_the_queried_command() {
        // Settings write (lizard-off) reads back as a mirror.
        let set = [0x01, 0x87, 0x03, 0x09, 0x00, 0x00];
        let r = triton::feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(r[0], 0x01);
        assert_eq!(&r[1..6], &[0x87, 0x03, 0x09, 0x00, 0x00]);
    }

    #[test]
    fn triton_feature_reply_synthesizes_attributes_for_0x83() {
        let set = [0x01, 0x83, 0x00];
        let r = triton::feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(&r[..3], &[0x01, 0x83, 0x19]); // 25-byte TLV payload
        assert_eq!(r[3], 0x01); // first attribute id: product id
                                // Tag-4 TLV carries FW_BUILD_TIME; a stale epoch makes Steam prompt to update firmware.
        assert_eq!(r[18], 0x04);
        assert_eq!(r[19..23], triton::FW_BUILD_TIME.to_le_bytes());
    }

    #[test]
    fn triton_firmware_info_build_time_agrees_with_the_attributes_reply() {
        let set = [0x01, 0xF2, 0x00, 0x00];
        let r = triton::feature_reply(&set, "FVPF130200D03", 0x5452_4900);
        assert_eq!(&r[..4], &[0x01, 0xF2, 0x29, 0x00]);
        // Bytes 4..8 mirror the 0x83 reply's tag-4 build time — Steam may cross-check.
        assert_eq!(r[4..8], triton::FW_BUILD_TIME.to_le_bytes());
    }

    #[test]
    fn triton_input_len_matches_the_descriptor() {
        assert_eq!(triton::input_len(0x42), Some(54));
        assert_eq!(triton::input_len(0x45), Some(46));
        assert_eq!(triton::input_len(0x43), Some(15));
        assert_eq!(triton::input_len(0x44), Some(6));
        assert_eq!(triton::input_len(0x79), Some(2));
        assert_eq!(triton::input_len(0x7B), Some(13));
        assert_eq!(triton::input_len(0x47), None); // BLE-only id, not in the wired descriptor
        assert_eq!(triton::input_len(0x01), None);
    }

    #[test]
    fn triton_out_report_len_matches_the_descriptor_and_bench_table() {
        assert_eq!(triton::out_report_len(0x80), 10);
        assert_eq!(triton::out_report_len(0x81), 8);
        assert_eq!(triton::out_report_len(0x82), 4);
        assert_eq!(triton::out_report_len(0x83), 10);
        assert_eq!(triton::out_report_len(0x84), 9);
        assert_eq!(triton::out_report_len(0x85), 4);
        assert_eq!(triton::out_report_len(0x86), 4);
        assert_eq!(triton::out_report_len(0x87), 64);
        assert_eq!(triton::out_report_len(0x88), 64);
        assert_eq!(triton::out_report_len(0x89), 64);
        // Undeclared ids stay whole (64 = no trim) — never guess a length.
        assert_eq!(triton::out_report_len(0x00), 64);
        assert_eq!(triton::out_report_len(0x8A), 64);
    }

    #[test]
    fn triton_serial_shape_dodges_the_pf_prefix_rejection() {
        let mut s = [0u8; 13];
        triton::serial(3, &mut s);
        assert_eq!(&s, b"FVPF130203D03");
    }

    #[test]
    fn triton_rdesc_is_the_372_byte_capture() {
        assert_eq!(triton::RDESC.len(), 372);
        // Mouse TLC opens it: Usage Page Generic Desktop, Usage Mouse, Collection App, Report ID 0x40.
        assert_eq!(
            &triton::RDESC[..8],
            &[0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x40]
        );
    }

    #[test]
    fn out_feature_bit_round_trips() {
        let tagged = 64u32 | triton::OUT_FEATURE_BIT;
        assert_eq!(triton::out_len(tagged), 64);
        assert!(triton::out_is_feature(tagged));
        assert!(!triton::out_is_feature(64));
        assert_eq!(triton::out_len(64), 64);
    }

    #[test]
    fn encode_ioctls_follow_the_probe_pair() {
        println!(
            "SET_ENCODE {:#x}, ENCODE_CTL {:#x} (after probe {:#x})",
            encode::IOCTL_SET_ENCODE,
            encode::IOCTL_ENCODE_CTL,
            control::IOCTL_ENCODE_PROBE_STATUS,
        );
        assert_eq!(encode::IOCTL_SET_ENCODE, ctl_code(0x90C));
        assert_eq!(encode::IOCTL_ENCODE_CTL, ctl_code(0x90D));
        // METHOD_BUFFERED on FILE_DEVICE_UNKNOWN, like every other op in the space.
        assert_eq!(encode::IOCTL_SET_ENCODE & 0b11, 0);
        assert_eq!(encode::IOCTL_SET_ENCODE >> 16, 0x22);
    }

    #[test]
    fn set_encode_request_layout_is_pinned() {
        use core::mem::{offset_of, size_of};
        use encode::SetEncodeRequest;

        println!(
            "SetEncodeRequest {} bytes: target_id@{} section@{} event@{} hdr_meta@{} \
             backends@{} flags@{}",
            size_of::<SetEncodeRequest>(),
            offset_of!(SetEncodeRequest, target_id),
            offset_of!(SetEncodeRequest, section),
            offset_of!(SetEncodeRequest, event),
            offset_of!(SetEncodeRequest, hdr_meta),
            offset_of!(SetEncodeRequest, backends),
            offset_of!(SetEncodeRequest, flags),
        );
        assert_eq!(size_of::<SetEncodeRequest>(), 144);
        assert_eq!(
            offset_of!(SetEncodeRequest, knobs),
            encode::SET_ENCODE_REQUEST_LEGACY_SIZE
        );
        assert_eq!(offset_of!(SetEncodeRequest, target_id), 0);
        assert_eq!(offset_of!(SetEncodeRequest, section), 8);
        assert_eq!(offset_of!(SetEncodeRequest, event), 16);
        assert_eq!(offset_of!(SetEncodeRequest, hdr_meta), 60);
        assert_eq!(offset_of!(SetEncodeRequest, backends), 96);
        assert_eq!(offset_of!(SetEncodeRequest, flags), 112);
        // The HDR blob is exactly `pf_frame::HdrMeta`, which the driver copies through untouched.
        assert_eq!(size_of::<[u8; 28]>(), 28);
    }

    #[test]
    fn set_encode_reply_and_ctl_layouts_are_pinned() {
        use core::mem::{offset_of, size_of};
        use encode::{EncodeCtlRequest, EncoderCapsWire, SetEncodeReply};

        println!(
            "SetEncodeReply {} bytes (caps {} bytes): caps@{} applied@{} error@{} name@{}",
            size_of::<SetEncodeReply>(),
            size_of::<EncoderCapsWire>(),
            offset_of!(SetEncodeReply, caps),
            offset_of!(SetEncodeReply, applied_bitrate_kbps),
            offset_of!(SetEncodeReply, error),
            offset_of!(SetEncodeReply, name),
        );
        assert_eq!(size_of::<EncoderCapsWire>(), 24);
        assert_eq!(size_of::<SetEncodeReply>(), 72);
        assert_eq!(offset_of!(SetEncodeReply, status), 0);
        assert_eq!(offset_of!(SetEncodeReply, backend_opened), 4);
        assert_eq!(offset_of!(SetEncodeReply, caps), 8);
        assert_eq!(offset_of!(SetEncodeReply, applied_bitrate_kbps), 32);
        assert_eq!(offset_of!(SetEncodeReply, error), 36);
        assert_eq!(offset_of!(SetEncodeReply, name), 40);

        println!(
            "EncodeCtlRequest {} bytes: op@{} arg0@{} arg1@{} payload@{}",
            size_of::<EncodeCtlRequest>(),
            offset_of!(EncodeCtlRequest, op),
            offset_of!(EncodeCtlRequest, arg0),
            offset_of!(EncodeCtlRequest, arg1),
            offset_of!(EncodeCtlRequest, payload),
        );
        assert_eq!(size_of::<EncodeCtlRequest>(), 44);
        assert_eq!(offset_of!(EncodeCtlRequest, target_id), 0);
        assert_eq!(offset_of!(EncodeCtlRequest, op), 4);
        assert_eq!(offset_of!(EncodeCtlRequest, arg0), 8);
        assert_eq!(offset_of!(EncodeCtlRequest, arg1), 12);
        assert_eq!(offset_of!(EncodeCtlRequest, payload), 16);
    }

    /// A `SET_ENCODE` reply may promise only the chroma its chosen input can carry. The two
    /// backends the host ever negotiates 4:4:4 for must therefore land on a full-chroma input at
    /// both depths: HDR once picked P010 here, so the encoder emitted 4:2:0 while the reply — and
    /// the client's Welcome — still said 4:4:4.
    #[test]
    fn a_444_request_picks_a_full_chroma_input() {
        use encode::EncodeInput::{self, Bgra, Nv12, P010Sdr, Planar, Rgb10, P010};

        // (backend, hdr, ten_bit, chroma444) -> input. HDR implies ten_bit; 10-bit SDR is
        // ten_bit without hdr.
        let table = [
            ((1, false, false, false), Bgra),
            ((1, false, true, false), Bgra), // NVENC widens SDR-10 from BGRA itself
            ((1, false, false, true), Bgra),
            ((1, true, true, false), P010),
            ((1, true, true, true), Rgb10),
            ((2, true, true, true), P010),
            ((2, false, true, false), P010Sdr), // AMF 10-bit SDR: BT.709 P010
            ((2, false, false, false), Nv12),   // AMF 8-bit SDR
            ((3, false, false, true), Nv12),
            ((3, false, true, true), Nv12), // QSV 10-bit SDR not wired: 8-bit NV12
            (
                (4, true, true, true),
                Planar {
                    hdr: true,
                    chroma444: true,
                },
            ),
        ];
        for ((backend, hdr, ten_bit, chroma444), want) in table {
            let got = EncodeInput::choose(backend, hdr, ten_bit, chroma444);
            assert_eq!(
                got, want,
                "backend {backend} hdr {hdr} 10bit {ten_bit} 444 {chroma444}"
            );
        }
        for backend in [1, 4] {
            for hdr in [false, true] {
                assert!(
                    EncodeInput::choose(backend, hdr, hdr, true).full_chroma(),
                    "backend {backend} hdr {hdr} asked 4:4:4 and got a subsampled input"
                );
            }
        }
    }

    #[test]
    fn encode_ctl_ops_are_distinct() {
        use encode::*;
        let ops = [
            ENCODE_CTL_REQUEST_KEYFRAME,
            ENCODE_CTL_INVALIDATE_REF_FRAMES,
            ENCODE_CTL_DISTRUST_REFERENCES,
            ENCODE_CTL_RECONFIGURE_BITRATE,
            ENCODE_CTL_SET_HDR_META,
            ENCODE_CTL_RESET,
            ENCODE_CTL_FLUSH,
            ENCODE_CTL_CLOSE,
        ];
        println!("ENCODE_CTL ops: {ops:?}");
        assert_eq!(ops, [1, 2, 3, 4, 5, 6, 7, 8]);
        // `0` stays unassigned: a zeroed request is not a silent keyframe.
        assert!(!ops.contains(&0));
    }

    #[test]
    fn keyframe_republishes_the_stash_only_when_nothing_composed() {
        use encode::republish_slot;
        // Idle desktop, slot back in the free list: the request re-encodes the stash as an IDR.
        assert_eq!(republish_slot(Some(1), 0, &[1, 2]), Some(1));
        // A composed frame is queued — the ordinary path already produces the IDR.
        assert_eq!(republish_slot(Some(1), 1, &[1, 2]), None);
        // The slot is in use again (a drain pass or an AU still owed on it).
        assert_eq!(republish_slot(Some(1), 0, &[2]), None);
        // Nothing was ever encoded on this pool.
        assert_eq!(republish_slot(None, 0, &[1, 2]), None);
    }

    #[test]
    fn a_full_pool_recycles_the_oldest_frame_not_the_new_one() {
        use encode::{offer_slot, OfferSlot};
        // A free slot is always taken, whatever is queued behind it.
        assert_eq!(offer_slot(Some(2), Some(0), true), Some(OfferSlot::Free(2)));
        // No free slot: the oldest queued frame goes, and a consumer lost it.
        assert_eq!(
            offer_slot(None, Some(0), true),
            Some(OfferSlot::Recycle {
                slot: 0,
                lost: true
            })
        );
        // Between sessions nobody is reading, so the same recycle costs nothing.
        assert_eq!(
            offer_slot(None, Some(0), false),
            Some(OfferSlot::Recycle {
                slot: 0,
                lost: false
            })
        );
        // Every slot is out at the encoder: this frame has nowhere to land.
        assert_eq!(offer_slot(None, None, true), None);
    }

    #[test]
    fn a_cursor_save_under_covers_only_what_the_target_holds() {
        use cursor::clip_rect;
        // Wholly inside: the shape's own box.
        assert_eq!(
            clip_rect(100, 50, 32, 32, 1920, 1080),
            Some((100, 50, 32, 32))
        );
        // Half off each edge in turn; the origin moves only where the shape starts negative.
        assert_eq!(
            clip_rect(-10, 50, 32, 32, 1920, 1080),
            Some((0, 50, 22, 32))
        );
        assert_eq!(
            clip_rect(100, -10, 32, 32, 1920, 1080),
            Some((100, 0, 32, 22))
        );
        assert_eq!(
            clip_rect(1900, 50, 32, 32, 1920, 1080),
            Some((1900, 50, 20, 32))
        );
        assert_eq!(
            clip_rect(100, 1060, 32, 32, 1920, 1080),
            Some((100, 1060, 32, 20))
        );
        // Fully off, in both directions: nothing to save.
        assert_eq!(clip_rect(-40, 50, 32, 32, 1920, 1080), None);
        assert_eq!(clip_rect(1920, 50, 32, 32, 1920, 1080), None);
        // A shape with no pixels covers nothing.
        assert_eq!(clip_rect(100, 50, 0, 32, 1920, 1080), None);
        assert_eq!(clip_rect(100, 50, 32, 0, 1920, 1080), None);
    }

    #[test]
    fn au_section_layout_is_pinned() {
        use core::mem::{offset_of, size_of};
        use encode::au::{self, AuHeader, AuSlot};

        println!(
            "AuHeader {} bytes: latest@{} generation@{} encoder_state@{} last_au_qpc@{} \
             driver_status@{}; AuSlot {} bytes; slot table @{}, heap @{}",
            size_of::<AuHeader>(),
            offset_of!(AuHeader, latest),
            offset_of!(AuHeader, generation),
            offset_of!(AuHeader, encoder_state),
            offset_of!(AuHeader, last_au_qpc),
            offset_of!(AuHeader, driver_status),
            size_of::<AuSlot>(),
            au::SLOT_TABLE_OFFSET,
            au::HEAP_OFFSET,
        );
        assert_eq!(size_of::<AuHeader>(), 128);
        assert_eq!(offset_of!(AuHeader, magic), 0);
        assert_eq!(offset_of!(AuHeader, version), 4);
        assert_eq!(offset_of!(AuHeader, heap_offset), 8);
        assert_eq!(offset_of!(AuHeader, heap_bytes), 12);
        assert_eq!(offset_of!(AuHeader, slot_table_offset), 16);
        assert_eq!(offset_of!(AuHeader, slot_count), 20);
        assert_eq!(offset_of!(AuHeader, latest), 24);
        assert_eq!(offset_of!(AuHeader, generation), 32);
        assert_eq!(offset_of!(AuHeader, wire_seq_base), 36);
        assert_eq!(offset_of!(AuHeader, encoder_state), 40);
        assert_eq!(offset_of!(AuHeader, detached), 44);
        assert_eq!(offset_of!(AuHeader, last_au_qpc), 48);
        assert_eq!(offset_of!(AuHeader, drain_heartbeat_qpc), 56);
        assert_eq!(offset_of!(AuHeader, source_seq), 64);
        assert_eq!(offset_of!(AuHeader, dropped_total), 72);
        assert_eq!(offset_of!(AuHeader, published_total), 80);
        assert_eq!(offset_of!(AuHeader, driver_status), 88);
        assert_eq!(offset_of!(AuHeader, driver_status_detail), 92);
        // Carved out of `_reserved`, which the host still zeroes: an older driver leaves it 0.
        assert_eq!(offset_of!(AuHeader, applied_bitrate_kbps), 96);
        assert_eq!(offset_of!(AuHeader, _reserved), 100);

        assert_eq!(size_of::<AuSlot>(), 48);
        assert_eq!(offset_of!(AuSlot, offset), 0);
        assert_eq!(offset_of!(AuSlot, len), 4);
        assert_eq!(offset_of!(AuSlot, wire_seq), 8);
        assert_eq!(offset_of!(AuSlot, source_seq), 12);
        assert_eq!(offset_of!(AuSlot, qpc_pts), 16);
        assert_eq!(offset_of!(AuSlot, flags), 24);
        assert_eq!(offset_of!(AuSlot, state), 28);
        assert_eq!(offset_of!(AuSlot, qpc_submit), 32);
        assert_eq!(offset_of!(AuSlot, qpc_published), 40);

        assert_eq!(au::slot_offset(0), 128);
        assert_eq!(au::slot_offset(15), 128 + 15 * 48);
        assert_eq!(au::slot_offset(au::AU_SLOTS as usize), au::HEAP_OFFSET);
        assert_eq!(au::HEAP_OFFSET, 896);
        assert_eq!(&au::AU_MAGIC.to_le_bytes(), b"PFAU");
        // The retired ring header's magic — a v6 section must never read as an AU one.
        assert_ne!(au::AU_MAGIC, 0x4456_4650);
    }

    #[test]
    fn au_flag_bits_and_slot_states_are_distinct() {
        use encode::au::*;
        let flags = [
            AU_FIRST,
            AU_LAST,
            AU_KEYFRAME,
            AU_RECOVERY_ANCHOR,
            AU_CHUNK_ALIGNED,
            AU_RECOVERY_POINT,
            AU_RECOVERY_CLOSE,
        ];
        println!("AU flags: {flags:?}; states: {FREE} {PUBLISHED} {READING}");
        assert_eq!(flags, [1, 2, 4, 8, 16, 32, 64]);
        assert_eq!(flags.iter().fold(0, |a, b| a | b), 0b111_1111);
        // A whole non-key AU is FIRST|LAST — the two must not alias.
        assert_eq!(AU_FIRST | AU_LAST, 3);
        assert_eq!([FREE, PUBLISHED, READING], [0, 1, 2]);
        assert_eq!(
            [
                ENCODER_CLOSED,
                ENCODER_OPEN,
                ENCODER_ENCODING,
                ENCODER_WEDGED
            ],
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn heap_and_section_sizing_match_the_plan() {
        use encode::au::{self, heap_bytes_for, section_bytes};

        let mib = 1024 * 1024;
        for (kbps, fps) in [(20_000, 60), (400_000, 60), (400_000, 120)] {
            println!(
                "{kbps} kbps @ {fps} fps -> heap {} B, section {} B",
                heap_bytes_for(kbps, fps),
                section_bytes(heap_bytes_for(kbps, fps)),
            );
        }
        // 20 Mbps is far under the floor — a 16-slot table still needs room.
        assert_eq!(heap_bytes_for(20_000, 60), au::HEAP_MIN_BYTES);
        assert_eq!(au::HEAP_MIN_BYTES, mib);
        // The plan's worst case: 400 Mbps at 60 fps must fit the 8 MiB budget.
        let big = heap_bytes_for(400_000, 60);
        assert!(big <= 8 * mib, "400 Mbps/60 wanted {big} B, over 8 MiB");
        assert!(big > 4 * mib, "and it must not be a token allocation");
        // More frames per second means less bitstream per frame.
        assert!(heap_bytes_for(400_000, 120) < big);
        // Everything lands on the granule, is clamped, and survives absurd inputs.
        assert_eq!(big % au::HEAP_GRANULE, 0);
        assert_eq!(heap_bytes_for(u32::MAX, 1), au::HEAP_MAX_BYTES);
        assert_eq!(heap_bytes_for(400_000, 0), heap_bytes_for(400_000, 1));

        assert_eq!(section_bytes(0), 4096);
        for heap in [au::HEAP_MIN_BYTES, big, au::HEAP_MAX_BYTES] {
            let s = section_bytes(heap);
            assert_eq!(s % au::SECTION_ALIGN, 0);
            assert!(s as usize >= au::HEAP_OFFSET + heap as usize);
            assert!((s as usize) < au::HEAP_OFFSET + heap as usize + 4096);
        }
    }

    #[test]
    fn au_readable_is_fail_closed() {
        use encode::au::{self, au_readable, AuHeader};

        let good = AuHeader {
            magic: au::AU_MAGIC,
            version: au::AU_VERSION,
            heap_offset: au::HEAP_OFFSET as u32,
            heap_bytes: au::heap_bytes_for(50_000, 60),
            slot_table_offset: au::SLOT_TABLE_OFFSET as u32,
            slot_count: au::AU_SLOTS,
            ..AuHeader::zeroed()
        };
        println!("readable header: {good:?}");
        assert!(au_readable(&good));
        // A zeroed section is what an unmapped or half-created one looks like.
        assert!(!au_readable(&AuHeader::zeroed()));
        for bad in [
            AuHeader {
                magic: 0x4456_4650,
                ..good
            },
            AuHeader { version: 6, ..good },
            AuHeader {
                heap_bytes: au::HEAP_MAX_BYTES + 1,
                ..good
            },
            AuHeader {
                heap_bytes: 4096,
                ..good
            },
            AuHeader {
                slot_count: 8,
                ..good
            },
            AuHeader {
                slot_table_offset: 64,
                ..good
            },
            AuHeader {
                heap_offset: 128,
                ..good
            },
        ] {
            assert!(!au_readable(&bad), "accepted {bad:?}");
        }
    }

    #[test]
    fn au_publish_token_round_trips_every_slot() {
        use encode::{au::AU_SLOTS, FrameToken};

        for slot in 0..AU_SLOTS as u8 {
            let t = FrameToken {
                generation: 0x00AB_CDEF,
                seq: 0xDEAD_BEEF,
                slot,
            };
            let packed = t.pack();
            assert_eq!(FrameToken::unpack(packed), t, "slot {slot} did not survive");
        }
        let t = FrameToken {
            generation: 7,
            seq: 15,
            slot: 15,
        };
        println!("token {t:?} packs to {:#x}", t.pack());
        // 16 slots need 4 bits of the 8 the token carries: the table can double without a repack.
        assert_eq!(FrameToken::unpack(t.pack()).slot, 15);
    }

    #[test]
    fn set_encode_request_round_trips_through_bytes() {
        use encode::SetEncodeRequest;

        let mut req = SetEncodeRequest::zeroed();
        req.target_id = 0x1234;
        req.section = 0x0BAD_C0DE_0BAD_C0DE;
        req.event = 0xFEED_FACE_FEED_FACE;
        req.section_bytes = encode::au::section_bytes(encode::au::heap_bytes_for(80_000, 60));
        req.codec = 2;
        req.chroma = 1;
        req.bit_depth = 10;
        req.width = 3840;
        req.height = 2160;
        req.fps = 120;
        req.bitrate_kbps = 80_000;
        req.hdr = 1;
        req.hdr_meta[27] = 0xAB;
        req.wire_chunk_bytes = 0;
        req.wire_seq_base = 0x7FFF_FFFF;
        req.backends = [1, 2, 0, 0];

        req.knobs.apply_env("PUNKTFUNK_NVENC_SLICES", "4");

        let bytes = bytemuck::bytes_of(&req);
        println!("SetEncodeRequest on the wire: {} bytes", bytes.len());
        assert_eq!(bytes.len(), 144);
        let back: SetEncodeRequest = bytemuck::pod_read_unaligned(bytes);
        assert_eq!(back, req);
        // A pre-knobs driver reads the first 120 bytes and sees the same request minus knobs;
        // a pre-knobs host's 120 bytes zero-fill to every backend's default.
        let mut prefix = [0u8; 144];
        prefix[..encode::SET_ENCODE_REQUEST_LEGACY_SIZE]
            .copy_from_slice(&bytes[..encode::SET_ENCODE_REQUEST_LEGACY_SIZE]);
        let old: SetEncodeRequest = bytemuck::pod_read_unaligned(&prefix);
        assert_eq!(old.backends, req.backends);
        assert_eq!(old.knobs, encode::EncodeKnobs::default());
        // The two handle values sit where the driver reads them, byte for byte.
        assert_eq!(
            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
            req.section
        );
        assert_eq!(
            u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
            req.event
        );
        // A zeroed request is inert: no target, no handles, no backend.
        let zero = SetEncodeRequest::zeroed();
        assert_eq!(zero.backends, [0; 4]);
        assert_eq!(bytemuck::bytes_of(&zero), [0u8; 144]);
    }

    #[test]
    fn heap_ring_back_pressures_on_the_slot_table() {
        use encode::au::{self, HeapRing};

        let n = au::AU_SLOTS as usize;
        let mut ring = HeapRing::new(au::HEAP_OFFSET as u32, au::HEAP_MIN_BYTES);
        let mut states = [au::FREE; au::AU_SLOTS as usize];
        // Sixteen publishes fill the table; the writer claims by storing PUBLISHED itself.
        let mut offsets = Vec::new();
        for k in 0..n {
            let (slot, offset) = ring.take(1000, &states).expect("a free slot");
            assert_eq!(slot, k, "slots hand out round-robin");
            states[slot] = au::PUBLISHED;
            offsets.push(offset);
        }
        assert!(offsets.windows(2).all(|w| w[1] == w[0] + 1000));
        assert_eq!(offsets[0], au::HEAP_OFFSET as u32);
        println!(
            "16 slots placed from {} to {}",
            offsets[0],
            offsets[n - 1] + 1000
        );
        // All unread: the seventeenth is refused — the drop lands on the pool, not on an AU.
        assert_eq!(ring.take(1000, &states), None);
        // The host takes one READING then frees it; only then is a slot available again.
        states[5] = au::READING;
        assert_eq!(ring.take(1000, &states), None);
        states[5] = au::FREE;
        let (slot, offset) = ring.take(1000, &states).expect("the freed slot");
        assert_eq!(slot, 5);
        assert_eq!(
            offset,
            offsets[n - 1] + 1000,
            "bytes keep bumping, slots recycle"
        );
        // Zero-length chunks and over-heap chunks are answered, never placed wrongly.
        assert_eq!(ring.take(au::HEAP_MIN_BYTES + 1, &states), None);
    }

    #[test]
    fn heap_ring_wraps_without_overwriting_held_bytes() {
        use encode::au::{self, HeapRing};

        let heap = au::HEAP_MIN_BYTES;
        let base = au::HEAP_OFFSET as u32;
        let mut ring = HeapRing::new(base, heap);
        let mut states = [au::FREE; au::AU_SLOTS as usize];
        let chunk = heap / 4 + 1;
        // Three chunks fit; the fourth does not fit at the end and must wrap, never straddle.
        let mut placed = Vec::new();
        for _ in 0..3 {
            let (slot, off) = ring.take(chunk, &states).unwrap();
            states[slot] = au::PUBLISHED;
            placed.push((slot, off));
            assert!(off + chunk <= base + heap);
        }
        // The wrap target overlaps slot 0's bytes while the host holds them.
        assert_eq!(ring.take(chunk, &states), None);
        states[placed[0].0] = au::FREE;
        let (slot, off) = ring.take(chunk, &states).unwrap();
        println!("wrapped to slot {slot} at {off} after {:?}", placed);
        assert_eq!(
            off, base,
            "a chunk that does not fit at the end starts the heap over"
        );
        states[slot] = au::PUBLISHED;
        // The next one continues after the wrap and still refuses slot 1's held bytes.
        assert_eq!(ring.take(chunk, &states), None);
        states[placed[1].0] = au::FREE;
        let (_, off2) = ring.take(chunk, &states).unwrap();
        assert_eq!(off2, base + chunk);
        // A freed slot's stale range never blocks: FREE ranges are ignored.
        states = [au::FREE; au::AU_SLOTS as usize];
        assert!(
            ring.take(heap, &states).is_some(),
            "a whole-heap AU fits an empty heap"
        );
    }

    #[test]
    fn encode_knobs_parse_their_env_spellings() {
        use encode::{truthy, EncodeKnobs};
        let mut k = EncodeKnobs::default();
        assert!(!k.apply_env("PUNKTFUNK_NOT_A_KNOB", "1"));
        for name in EncodeKnobs::ENV_NAMES {
            assert!(k.apply_env(name, ""), "{name} must be a knob name");
        }
        assert_eq!(
            k,
            EncodeKnobs::default(),
            "empty values leave every default"
        );

        assert!(k.apply_env("PUNKTFUNK_SPLIT_ENCODE", "disable"));
        assert_eq!(k.split_encode, 1);
        k.apply_env("PUNKTFUNK_SPLIT_ENCODE", "3");
        assert_eq!(k.split_encode, 4);
        k.apply_env("PUNKTFUNK_SPLIT_ENCODE", "garbage");
        assert_eq!(k.split_encode, 4, "garbage keeps the last value");
        k.apply_env("PUNKTFUNK_NVENC_ASYNC", " yes ");
        assert_eq!(k.nvenc_async, 1);
        k.apply_env("PUNKTFUNK_NVENC_SUBFRAME", "0");
        assert_eq!(k.nvenc_subframe, 1);
        k.apply_env("PUNKTFUNK_NVENC_SLICES", "33");
        assert_eq!(k.nvenc_slices, 0, "out of range is ignored");
        k.apply_env("PUNKTFUNK_INTRA_REFRESH", "0");
        assert_eq!(k.intra_refresh, 2);
        k.apply_env("PUNKTFUNK_INTRA_REFRESH", "on");
        assert_eq!(k.intra_refresh, 1);
        k.apply_env("PUNKTFUNK_INTRA_REFRESH", "maybe");
        assert_eq!(k.intra_refresh, 0);
        k.apply_env("PUNKTFUNK_IR_PERIOD_FRAMES", "1");
        assert_eq!(k.ir_period_frames, 0, "a one-frame wave is not a wave");
        k.apply_env("PUNKTFUNK_IR_PERIOD_FRAMES", "60");
        assert_eq!(k.ir_period_frames, 60);
        k.apply_env("PUNKTFUNK_AMF_USAGE", "highquality");
        assert_eq!(k.amf_usage, 4);
        k.apply_env("PUNKTFUNK_VBV_FRAMES", "1.5");
        assert_eq!(k.vbv_tenths, 15);
        assert_eq!(k.vbv_frames(), 1.5);
        k.apply_env("PUNKTFUNK_VBV_FRAMES", "-1");
        assert_eq!(k.vbv_tenths, 15);
        k.apply_env("PUNKTFUNK_PYROWAVE_CHUNK_KIB", "4");
        assert_eq!(k.pyrowave_chunk_64kib, 1, "the floor rounds up to one step");
        k.apply_env("PUNKTFUNK_PYROWAVE_CHUNK_KIB", "8192");
        assert_eq!(k.pyrowave_chunk_64kib, 128);
        assert!(truthy("TRUE") && truthy(" 1") && !truthy("2") && !truthy(""));
    }
}
