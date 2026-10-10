//! What the stream carries beside the picture: HDR metadata, the cursor, host timing,
//! colour, chroma, codec and shard payload.

#[cfg(feature = "quic")]
use crate::*;

/// Static HDR metadata ([`punktfunk_connection_next_hdr_meta`]): ST.2086 mastering
/// display + CEA-861.3 content light. HDR10 SEI units (primaries/white 1/50000,
/// luminance 0.0001 cd/m²) for DXGI / `CAEDRMetadata` / `KEY_HDR_STATIC_INFO`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHdrMeta {
    /// Display-primaries x-chromaticities, 1/50000 units, ST.2086 order [green, blue, red].
    pub display_primaries_x: [u16; 3],
    /// Display-primaries y-chromaticities, 1/50000 units, ST.2086 order [green, blue, red].
    pub display_primaries_y: [u16; 3],
    /// White-point x-chromaticity, 1/50000 units.
    pub white_point_x: u16,
    /// White-point y-chromaticity, 1/50000 units.
    pub white_point_y: u16,
    /// Max display mastering luminance, 0.0001 cd/m².
    pub max_display_mastering_luminance: u32,
    /// Min display mastering luminance, 0.0001 cd/m².
    pub min_display_mastering_luminance: u32,
    /// Max content light level (MaxCLL), nits. 0 = unknown.
    pub max_cll: u16,
    /// Max frame-average light level (MaxFALL), nits. 0 = unknown.
    pub max_fall: u16,
}

#[cfg(feature = "quic")]
impl PunktfunkHdrMeta {
    fn from_meta(m: &punktfunk_core::quic::HdrMeta) -> PunktfunkHdrMeta {
        PunktfunkHdrMeta {
            display_primaries_x: [
                m.display_primaries[0][0],
                m.display_primaries[1][0],
                m.display_primaries[2][0],
            ],
            display_primaries_y: [
                m.display_primaries[0][1],
                m.display_primaries[1][1],
                m.display_primaries[2][1],
            ],
            white_point_x: m.white_point[0],
            white_point_y: m.white_point[1],
            max_display_mastering_luminance: m.max_display_mastering_luminance,
            min_display_mastering_luminance: m.min_display_mastering_luminance,
            max_cll: m.max_cll,
            max_fall: m.max_fall,
        }
    }
}

/// Host capture→sent duration for one AU ([`punktfunk_connection_next_host_timing`]).
/// Correlate by `pts_ns`; `network = (received + clock_offset − pts_ns) − host_us`.
/// Lost datagram: no sample. See `design/stats-unification.md`.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHostTiming {
    /// AU capture stamp (host capture clock — matches `PunktfunkFrame::pts_ns`).
    pub pts_ns: u64,
    /// Host capture→sent duration, µs.
    pub host_us: u32,
}

/// Pull the next static HDR metadata (ST.2086 + content light) into `*out`.
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// Apply the latest to the display. Only an HDR session (PQ transfer from
/// `punktfunk_connection_color_info`) emits these. One puller.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHdrMeta`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_hdr_meta(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHdrMeta,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_hdr_meta(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(m) => {
                // SAFETY: caller out-param, non-null on this path, written once.
                unsafe { *out = PunktfunkHdrMeta::from_meta(&m) };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Forwarded host-cursor shape: straight-alpha RGBA8, no padding, `len == w * h * 4`,
/// hotspot within `w`×`h`. `serial` is the identity [`PunktfunkCursorState`] refers
/// to — cache the built OS cursor by it.
#[repr(C)]
pub struct PunktfunkCursorShape {
    pub serial: u32,
    pub w: u16,
    pub h: u16,
    pub hot_x: u16,
    pub hot_y: u16,
    /// Borrows connection memory until the next cursor-shape call.
    pub rgba: *const u8,
    pub len: usize,
}

/// Per-frame host-cursor state: position in host video pixels, visibility, and
/// relative-mode hint. `flags` bit 0 = visible, bit 1 = relative (host app
/// grabbed/hid the pointer — run captured relative; clear = absolute at `x`/`y`).
#[repr(C)]
pub struct PunktfunkCursorState {
    pub serial: u32,
    pub flags: u8,
    pub x: i32,
    pub y: i32,
}

/// Pull the next forwarded cursor shape (pointer-bitmap change on the control
/// stream; only `PUNKTFUNK_CLIENT_CAP_CURSOR` sessions receive any). On `Ok`,
/// `out->rgba` borrows until the next cursor-shape call. One puller per plane.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one cursor-shape
/// puller; it may run concurrently with every other plane.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_cursor_shape(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkCursorShape,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_cursor_shape(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(shape) => {
                let mut slot = lock_recover(&c.last_cursor_shape);
                let sh = slot.insert(shape);
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkCursorShape {
                        serial: sh.serial,
                        w: sh.w,
                        h: sh.h,
                        hot_x: sh.hot_x,
                        hot_y: sh.hot_y,
                        rgba: sh.rgba.as_ptr(),
                        len: sh.rgba.len(),
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Pull the next cursor state (`0xD0` per host encode tick — latest-wins; drain
/// the queue and apply only the newest). Same negotiation gate as
/// [`punktfunk_connection_next_cursor_shape`].
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable. At most one cursor-state
/// puller; it may run concurrently with every other plane.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_cursor_state(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkCursorState,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_cursor_state(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(st) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkCursorState {
                        serial: st.serial,
                        flags: st.flags,
                        x: st.x,
                        y: st.y,
                    };
                }
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Who draws the pointer (`design/remote-desktop-sweep.md`). `true` = client
/// draws (host forwards shape/state); `false` = host composites. Latest-wins.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_set_cursor_render(
    c: *mut PunktfunkConnection,
    client_draws: bool,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.set_cursor_render(client_draws))
    })
}

/// Pull the next per-AU host timing (0xCF) into `*out`: capture→sent duration,
/// correlated by `pts_ns` (see [`PunktfunkHostTiming`]).
/// [`PunktfunkStatus::NoFrame`] on timeout, [`PunktfunkStatus::Closed`] once ended.
/// Drain non-blockingly (`timeout_ms = 0`). A host that never emits any: keep
/// showing the combined `host+network` stage. One puller.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHostTiming`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_next_host_timing(
    c: *mut PunktfunkConnection,
    out: *mut PunktfunkHostTiming,
    timeout_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        match c
            .inner
            .next_host_timing(std::time::Duration::from_millis(timeout_ms as u64))
        {
            Ok(t) => {
                // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
                unsafe {
                    *out = PunktfunkHostTiming {
                        pts_ns: t.pts_ns,
                        host_us: t.host_us,
                    }
                };
                PunktfunkStatus::Ok
            }
            Err(e) => e.status(),
        }
    })
}

/// Resolved colour signalling + encode bit depth. Each out pointer is filled when
/// non-NULL: `primaries`/`transfer`/`matrix` are CICP (BT.709 = 1; BT.2020 = 9;
/// PQ = 16, HLG = 18; BT.2020-NCL = 9), `full_range` 0/1, `bit_depth` 8 or 10.
/// Transfer 16/18 is HDR — drain [`punktfunk_connection_next_hdr_meta`]. Fixed
/// until a reconfigure.
///
/// # Safety
/// `c` is a valid connection handle; each out pointer is NULL or writable for its scalar.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_color_info(
    c: *mut PunktfunkConnection,
    primaries: *mut u8,
    transfer: *mut u8,
    matrix: *mut u8,
    full_range: *mut u8,
    bit_depth: *mut u8,
) -> PunktfunkStatus {
    with_conn!(c => {
        let color = c.inner.color;
        // SAFETY: the caller passes each out-param null or writable for one value.
        unsafe {
            put(primaries, color.primaries);
            put(transfer, color.transfer);
            put(matrix, color.matrix);
            put(full_range, color.full_range);
            put(bit_depth, c.inner.bit_depth);
        }
        PunktfunkStatus::Ok
    })
}

/// Resolved chroma as HEVC `chroma_format_idc`: `1` = 4:2:0, `3` = 4:4:4.
/// `*out` is filled when non-NULL. In-band SPS is authoritative; this lets the
/// embedder pre-size the decoder. Fixed until a reconfigure.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_chroma_format(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.chroma_format)
}

/// Host-resolved video codec: [`PUNKTFUNK_CODEC_H264`] / [`PUNKTFUNK_CODEC_HEVC`] /
/// [`PUNKTFUNK_CODEC_AV1`]. Build the decoder from this (never assume HEVC).
/// `*out` is filled when non-NULL. A host that did not negotiate reports HEVC.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u8`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_codec(
    c: *mut PunktfunkConnection,
    out: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, out => c.inner.codec)
}

/// Negotiated wire shard payload (Welcome, bytes). Parse-window size of a
/// chunk-aligned AU (PyroWave datagram-aligned, `design/pyrowave-codec-plan.md`):
/// every `shard_payload`-sized window starts a self-delimiting chunk. Other
/// codecs never need this.
///
/// # Safety
/// `c` is a valid connection handle; `out` is NULL or writable for one `u32`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_shard_payload(
    c: *mut PunktfunkConnection,
    out: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, out => u32::from(c.inner.shard_payload))
}
