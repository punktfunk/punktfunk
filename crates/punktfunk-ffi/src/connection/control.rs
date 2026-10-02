//! Steering a live session: compositor and bitrate, clock offset, mode, keyframe and
//! RFI requests, frame accounting, the stats HUD, decode-latency reports, the speed
//! test and teardown.

#[cfg(feature = "quic")]
use crate::*;

/// Compositor the host resolved (`PUNKTFUNK_COMPOSITOR_*`; Welcome echo of
/// [`punktfunk_connect_ex`]). `AUTO` = a host that didn't say. Gamescope PipeWire
/// capture carries no cursor — default to a client-side cursor there.
///
/// # Safety
/// `c` is a valid connection handle; `compositor` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_compositor(
    c: *const PunktfunkConnection,
    compositor: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, compositor => c.inner.resolved_compositor.to_u8() as u32)
}

/// Video encoder bitrate (kbps) the host configured — the [`punktfunk_connect_ex3`]
/// request clamped to the host range, or its default when `0` was requested.
/// `0` = a host that didn't report it. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `bitrate_kbps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_bitrate(
    c: *const PunktfunkConnection,
    bitrate_kbps: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, bitrate_kbps => c.inner.resolved_bitrate_kbps)
}

/// Connect-time wall-clock offset, ns, host minus client. Add to a local
/// realtime stamp to express it in the host capture clock (`pts_ns`). `0` = none.
///
/// # Safety
/// `c` is a valid connection handle; `offset_ns` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clock_offset_ns(
    c: *const PunktfunkConnection,
    offset_ns: *mut i64,
) -> PunktfunkStatus {
    conn_out!(c, offset_ns => c.inner.clock_offset_ns)
}

/// Live wall-clock offset (updated by mid-stream re-sync). Use this for ongoing
/// latency math, not the frozen connect-time value.
///
/// # Safety
/// `c` is a valid connection handle; `offset_ns` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_clock_offset_now_ns(
    c: *const PunktfunkConnection,
    offset_ns: *mut i64,
) -> PunktfunkStatus {
    conn_out!(c, offset_ns => c.inner.clock_offset_now_ns())
}

/// Request a live mode switch. On accept, the first new-mode AU is an IDR with
/// in-band parameter sets — rebuild the decoder from it.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_mode(
    c: *const PunktfunkConnection,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.request_mode(punktfunk_core::config::Mode {
            width,
            height,
            refresh_hz,
        }))
    })
}

/// Request an IDR now. Infinite GOP has one opening IDR; throttle — a wedged
/// decoder stays stuck for several frames, so per-frame requests flood control.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_keyframe(
    c: *const PunktfunkConnection,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.request_keyframe())
    })
}

/// Ask the host to recover `[first_frame, last_frame]` by RFI (P-frame tagged
/// `USER_FLAG_RECOVERY_ANCHOR`) instead of a full IDR. Throttle; keyframe is the backstop.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_request_rfi(
    c: *const PunktfunkConnection,
    first_frame: u32,
    last_frame: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.request_rfi(first_frame, last_frame))
    })
}

/// Note each received `frame_index`. A forward gap fires throttled RFI; keyframe
/// on `frames_dropped` is the backstop. `gap_out` (nullable) is whether a gap was seen.
///
/// # Safety
/// `c` is a valid connection handle; `gap_out` is writable or NULL.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_note_frame_index(
    c: *const PunktfunkConnection,
    frame_index: u32,
    gap_out: *mut bool,
) -> PunktfunkStatus {
    with_conn!(c => {
        let gap = c.inner.note_frame_index(frame_index);
        // SAFETY: the caller passes `gap_out` null or writable for one value.
        unsafe { put(gap_out, gap > 0) };
        PunktfunkStatus::Ok
    })
}

/// [`punktfunk_connection_note_frame_index`] with the gap width: writes how many
/// frames this arrival revealed as missing (0 = contiguous/straggler). Pass the
/// width to [`punktfunk_reanchor_gate_arm_expecting_drops`] so a later
/// `frames_dropped` climb for the same loss cannot re-freeze a healed stream.
///
/// # Safety
/// `c` is a valid connection handle; `gap_width_out` is writable or NULL.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_note_frame_index_ex(
    c: *const PunktfunkConnection,
    frame_index: u32,
    gap_width_out: *mut u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        let gap = c.inner.note_frame_index(frame_index);
        // SAFETY: the caller passes `gap_width_out` null or writable for one value.
        unsafe { put(gap_width_out, gap) };
        PunktfunkStatus::Ok
    })
}

/// Unrecoverable reassembler drops. Poll and request a keyframe when it climbs —
/// infinite GOP conceals missing refs with no decode error. Writes 0 on NULL.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_frames_dropped(
    c: *const PunktfunkConnection,
    out: *mut u64,
) -> PunktfunkStatus {
    guard(|| {
        // Write 0 on a NULL connection before the handle check (header contract).
        // SAFETY: the caller passes `out` null or writable for one value.
        unsafe { put(out, 0) };
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: the caller passes `out` null or writable for one value.
        unsafe { put(out, c.inner.frames_dropped()) };
        PunktfunkStatus::Ok
    })
}

/// Facts only the embedder knows, for [`punktfunk_connection_hud_text`]. Zero-init, set
/// `struct_size = sizeof(PunktfunkHudFacts)`, then the fields you mean. Append only; bump ABI.
#[cfg(feature = "quic")]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct PunktfunkHudFacts {
    /// `sizeof(PunktfunkHudFacts)` as this caller was compiled.
    pub struct_size: u32,
    /// The displayed stamps are true on-glass instants.
    pub on_glass: bool,
    /// Take the measured OS present floor off end-to-end and display (iOS, tvOS).
    pub shave_os_floor: bool,
    /// Decoded audio queued ahead of the speaker, ms, for an embedder that plays audio itself.
    /// `0` keeps the core's reading.
    pub audio_buffer_ms: u32,
    /// Where that buffer puts audio against the picture, ms; positive = audio behind.
    pub av_offset_ms: i32,
    /// NUL-terminated preset name, or null.
    pub preset: *const c_char,
    /// Embedder-only Advanced Detailed lines, `<role>\t<text>\n` each, or null.
    pub extras: *const c_char,
}

#[cfg(all(feature = "quic", target_pointer_width = "64"))]
const _: () = assert!(core::mem::size_of::<PunktfunkHudFacts>() == 32);

/// [`punktfunk_connection_hud_text`]'s facts applied to a drained window.
///
/// # Safety
/// `facts` is null or points to at least its declared `struct_size` bytes, and its strings are
/// NUL-terminated or null.
#[cfg(feature = "quic")]
unsafe fn hud_with_facts(
    mut s: punktfunk_core::hud::StatsSnapshot,
    facts: *const PunktfunkHudFacts,
) -> Result<punktfunk_core::hud::StatsSnapshot, PunktfunkStatus> {
    if facts.is_null() {
        return Ok(s);
    }
    // SAFETY: `addr_of!` does not form a `&`; a shorter layout is rejected before the full read.
    let declared = unsafe { std::ptr::addr_of!((*facts).struct_size).read_unaligned() } as usize;
    if declared < std::mem::size_of::<PunktfunkHudFacts>() {
        return Err(PunktfunkStatus::InvalidArg);
    }
    // SAFETY: non-null, and `struct_size` covers this type. Fields are read one at a time and
    // the bools as bytes, so a binding that stores 2 cannot produce an invalid `bool`.
    let (on_glass, shave_os_floor, audio_buffer_ms, av_offset_ms, preset, extras) = unsafe {
        use std::ptr::addr_of;
        (
            addr_of!((*facts).on_glass).cast::<u8>().read_unaligned() != 0,
            addr_of!((*facts).shave_os_floor)
                .cast::<u8>()
                .read_unaligned()
                != 0,
            addr_of!((*facts).audio_buffer_ms).read_unaligned(),
            addr_of!((*facts).av_offset_ms).read_unaligned(),
            addr_of!((*facts).preset).read_unaligned(),
            addr_of!((*facts).extras).read_unaligned(),
        )
    };
    s.on_glass = on_glass;
    s.shave_os_floor = shave_os_floor;
    if audio_buffer_ms > 0 {
        s.audio_buffer_ms = audio_buffer_ms;
        s.av_offset_ms = av_offset_ms;
    }
    // SAFETY: caller strings, NUL-terminated or null, borrowed for this call.
    let preset = unsafe { opt_cstr(preset) }.map_err(|()| PunktfunkStatus::InvalidArg)?;
    s.preset = preset.filter(|p| !p.is_empty()).map(str::to_owned);
    // SAFETY: as above.
    let extras = unsafe { opt_cstr(extras) }.map_err(|()| PunktfunkStatus::InvalidArg)?;
    s.extras
        .extend(extras.unwrap_or_default().lines().filter_map(|line| {
            let (code, text) = line.split_once('\t')?;
            Some(punktfunk_core::hud::Extra {
                text: text.to_owned(),
                tier: punktfunk_core::hud::StatsVerbosity::Detailed,
                advanced_only: true,
                role: punktfunk_core::hud::Role::from_code(code.parse().ok()?),
            })
        }));
    Ok(s)
}

/// Stats overlay: one frame left the decoder. `pts_ns` is its capture stamp; `received_ns` (the
/// AU's reassembly stamp) and `decoded_ns` are client `CLOCK_REALTIME`. A zero `received_ns`
/// counts the frame without a decode sample.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_decoded(
    c: *const PunktfunkConnection,
    pts_ns: u64,
    received_ns: u64,
    decoded_ns: u64,
) -> PunktfunkStatus {
    with_conn!(c => {
        let hud = c.inner.hud();
        hud.note_decoded(pts_ns, decoded_ns);
        if received_ns > 0 && decoded_ns >= received_ns {
            hud.note_decode_us((decoded_ns - received_ns) / 1000, false);
        }
        PunktfunkStatus::Ok
    })
}

/// Stats overlay: one frame reached the screen at `displayed_ns`, client `CLOCK_REALTIME`.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_displayed(
    c: *const PunktfunkConnection,
    pts_ns: u64,
    decoded_ns: u64,
    displayed_ns: u64,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner
            .hud()
            .note_displayed(pts_ns, decoded_ns, 0, displayed_ns);
        PunktfunkStatus::Ok
    })
}

/// Stats overlay: one sample of the OS present pipeline's depth, ns: how far ahead of glass the
/// compositor takes a frame. Shaved off the shown figures when the facts ask for it.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_os_floor(
    c: *const PunktfunkConnection,
    floor_ns: u64,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.hud().note_os_floor_us(floor_ns / 1000);
        PunktfunkStatus::Ok
    })
}

/// Close the stats overlay's window, about once a second. [`punktfunk_connection_hud_text`]
/// formats what this kept as often as the tier changes.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_drain(
    c: *const PunktfunkConnection,
) -> PunktfunkStatus {
    with_conn!(c => {
        *lock_recover(&c.hud_snap) = c.inner.hud_snapshot();
        PunktfunkStatus::Ok
    })
}

/// The last drained window as overlay lines, `<role>\t<text>\n` each (role 0 primary, 1 detail,
/// 2 muted, 3 warning), NUL-terminated into `out`. `tier` is 0 off to 3 detailed; `advanced`
/// picks the Advanced vocabulary; `facts` may be null. When `cap` is too small nothing is
/// written and the status is `InvalidArg`; `*needed` (when non-null) always holds the size.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes; `facts` is null or
/// valid per [`PunktfunkHudFacts`]; `needed` is null or writable.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_hud_text(
    c: *const PunktfunkConnection,
    tier: u32,
    advanced: bool,
    facts: *const PunktfunkHudFacts,
    out: *mut c_char,
    cap: usize,
    needed: *mut usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        let snap = lock_recover(&c.hud_snap).clone();
        // SAFETY: the caller's `facts` contract, checked inside.
        let snap = match unsafe { hud_with_facts(snap, facts) } {
            Ok(s) => s,
            Err(status) => return status,
        };
        let tier = punktfunk_core::hud::StatsVerbosity::from_index(tier);
        let text = punktfunk_core::hud::encode_lines(&punktfunk_core::hud::format(&snap, tier, advanced));
        // SAFETY: the caller passes `needed` null or writable for one value.
        unsafe { put(needed, text.len() + 1) };
        // SAFETY: `out` is null or writable for `cap` bytes, per this function's contract.
        if !unsafe { write_cstr(out, cap, &text) } {
            return PunktfunkStatus::InvalidArg;
        }
        PunktfunkStatus::Ok
    })
}

/// Decode-stage latency in µs: AU leave [`next_au`] to decoded output. Include
/// decoder-input backlog; exclude vsync wait. Feeds Automatic bitrate. Skip if
/// [`punktfunk_connection_wants_decode_latency`] is false.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_report_decode_us(
    c: *const PunktfunkConnection,
    us: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.report_decode_us(us);
        PunktfunkStatus::Ok
    })
}

/// Report the display-latch grid (`design/phase-locked-capture.md`).
/// `next_latch_host_ns` is already host clock. ~1 Hz; no-op if unnegotiated.
///
/// # Safety
/// `c` is a caller handle or null (error, not UB).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_report_phase(
    c: *const PunktfunkConnection,
    next_latch_host_ns: u64,
    latch_period_ns: u32,
    uncertainty_ns: u32,
    arrival_lead_ns: u32,
    coherence_milli: u16,
) -> PunktfunkStatus {
    with_conn!(c => {
        c.inner.report_phase(
            next_latch_host_ns,
            latch_period_ns,
            uncertainty_ns,
            arrival_lead_ns,
            coherence_milli,
        );
        PunktfunkStatus::Ok
    })
}

/// Whether [`punktfunk_connection_report_decode_us`] is worth calling: writes true
/// only when Automatic bitrate is armed (non-PyroWave). Skip the per-frame
/// measurement otherwise. Constant for the session. Writes false on a NULL connection.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_wants_decode_latency(
    c: *const PunktfunkConnection,
    out: *mut bool,
) -> PunktfunkStatus {
    guard(|| {
        // Write false on a NULL connection before the handle check (uninitialized is not a bool).
        // SAFETY: the caller passes `out` null or writable for one value.
        unsafe { put(out, false) };
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        let c = match unsafe { c.as_ref() } {
            Some(c) => c,
            None => return PunktfunkStatus::NullPointer,
        };
        // SAFETY: the caller passes `out` null or writable for one value.
        unsafe { put(out, c.inner.wants_decode_latency()) };
        PunktfunkStatus::Ok
    })
}

/// Speed-test measurement from [`punktfunk_connection_probe_result`]. `done` is 0
/// until the host's end-of-burst report, then 1. `throughput_kbps` is delivered
/// wire throughput; `loss_pct` is link loss; `host_drop_pct` is send-buffer drop
/// (raise `net.core.wmem_max`). Measured separately so a host that can't keep up
/// reads differently from a lossy link.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PunktfunkProbeResult {
    /// 1 once the host's end-of-burst report arrived (measurement final); else 0 (partial).
    pub done: u8,
    /// Delivered wire bytes (header + shard) / packets the client received during the burst.
    pub recv_bytes: u64,
    pub recv_packets: u32,
    /// Application goodput bytes / access units the host offered.
    pub host_bytes: u64,
    pub host_packets: u32,
    /// Throughput denominator, ms: client-measured burst receive interval, live
    /// while bursting and final once `done`; host send-window duration when fewer
    /// than two probe packets arrived. Host duration alone overstates throughput —
    /// its window closes while the bottleneck queue is still draining.
    pub elapsed_ms: u32,
    /// Delivered wire throughput = `recv_bytes * 8 / elapsed_ms` (kilobits/second).
    pub throughput_kbps: u32,
    /// Link loss `(wire_packets_sent − recv_packets) / wire_packets_sent` as a percentage.
    pub loss_pct: f32,
    /// Host-side send-buffer drop `send_dropped / (wire_packets_sent + send_dropped)`, percent.
    pub host_drop_pct: f32,
    /// Wire packets the host put on the link, and the ones its send buffer dropped.
    pub wire_packets_sent: u32,
    pub send_dropped: u32,
    /// Probe inter-arrival gap, µs, to a tenth of a millisecond: median and 99th percentile.
    /// Their difference is the path's jitter.
    pub gap_p50_us: u32,
    pub gap_p99_us: u32,
    /// Probe packets that arrived behind a later one.
    pub reorders: u32,
}

/// Start a bandwidth speed test: host bursts filler at `target_kbps` goodput for
/// `duration_ms` (clamped ≤ 10 Gbps / ≤ 5 s), briefly pausing video. Non-blocking —
/// poll [`punktfunk_connection_probe_result`] until `done` is 1. Starting a probe
/// resets any prior measurement.
///
/// # Safety
/// `c` is a valid connection handle.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_speed_test(
    c: *const PunktfunkConnection,
    target_kbps: u32,
    duration_ms: u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        status_of(c.inner.request_probe(target_kbps, duration_ms))
    })
}

/// Read the current speed-test measurement into `*out` (partial until `out->done == 1`).
/// Safe to poll after [`punktfunk_connection_speed_test`]; before any probe it reports zeros.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkProbeResult`
/// (NULL is an error).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_probe_result(
    c: *const PunktfunkConnection,
    out: *mut PunktfunkProbeResult,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let o = c.inner.probe_result();
        // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
        unsafe {
            *out = PunktfunkProbeResult {
                done: o.done as u8,
                recv_bytes: o.recv_bytes,
                recv_packets: o.recv_packets,
                host_bytes: o.host_bytes,
                host_packets: o.host_packets,
                elapsed_ms: o.elapsed_ms,
                throughput_kbps: o.throughput_kbps,
                loss_pct: o.loss_pct,
                host_drop_pct: o.host_drop_pct,
                wire_packets_sent: o.wire_packets_sent,
                send_dropped: o.send_dropped,
                gap_p50_us: o.gap_p50_us,
                gap_p99_us: o.gap_p99_us,
                reorders: o.reorders,
            };
        }
        PunktfunkStatus::Ok
    })
}

/// One finding of the network check: the id names the text the app shows
/// ([`punktfunk_core::client::health::FindingId`] as a byte), `numbers` are its figures,
/// `profile` is the delivery profile that helps (`1` capped, `2` smooth, `0` none).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkHealthFinding {
    pub id: u8,
    pub severity: u8,
    pub profile: u8,
    pub numbers: [u32; 3],
}

/// Most findings one report carries; the rules can produce seven.
pub const PUNKTFUNK_HEALTH_FINDINGS_MAX: usize = 8;

/// The network check's report ([`punktfunk_core::client::health::HealthReport`]), flat.
/// `has_clean` 0 = a host without a ramp (no loss figure is honest); `has_host` 0 = the
/// host sent no facts; a leg or fact that was not sampled reads `0`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PunktfunkHealthReport {
    pub ceiling_kbps: u32,
    pub wall: u8,
    pub has_clean: u8,
    pub clean_rate_kbps: u32,
    pub clean_loss_pct: f32,
    pub clean_jitter_us: u32,
    pub client_iface_kind: u8,
    pub client_link_mbps: u32,
    pub client_rcvbuf_kb: u32,
    pub has_host: u8,
    pub host_iface_kind: u8,
    pub host_link_mbps: u32,
    pub host_sndbuf_kb: u32,
    /// Loss of the bursts leg and the capped leg, percent; `n_legs` says how many ran.
    pub n_legs: u8,
    pub leg_loss_pct: [f32; 2],
    pub n_findings: u8,
    pub findings: [PunktfunkHealthFinding; PUNKTFUNK_HEALTH_FINDINGS_MAX],
}

/// Run the network check over this connection and write its report into `*out`. Blocking
/// for ten to twenty seconds — call it off the main thread. The connection should have been
/// dialled with a delivery ask of probes only and facts; without one the check is the speed
/// test alone. Errors: `Unsupported` when the host declined, `Timeout` when a round never
/// reported.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for one `PunktfunkHealthReport`
/// (NULL is an error).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_network_check(
    c: *const PunktfunkConnection,
    out: *mut PunktfunkHealthReport,
) -> PunktfunkStatus {
    use punktfunk_core::client::health::{self, LegShape, SpeedError};
    with_conn!(c => {
        if out.is_null() {
            return PunktfunkStatus::NullPointer;
        }
        let r = match health::health_check(&c.inner, |_| {}) {
            Ok(r) => r,
            Err(SpeedError::Request(e)) => return status_of(Err::<(), _>(e)),
            Err(SpeedError::Declined) => return PunktfunkStatus::Unsupported,
            Err(SpeedError::Timeout) => return PunktfunkStatus::Timeout,
        };
        let mut rep = PunktfunkHealthReport {
            ceiling_kbps: r.speed.ceiling_kbps,
            wall: r.speed.wall as u8,
            client_iface_kind: r.client.link.kind,
            client_link_mbps: r.client.link.mbps,
            client_rcvbuf_kb: r.client.rcvbuf_kb,
            ..Default::default()
        };
        if let Some(cl) = r.speed.clean {
            rep.has_clean = 1;
            rep.clean_rate_kbps = cl.rate_kbps;
            rep.clean_loss_pct = cl.loss_pct;
            rep.clean_jitter_us = cl.jitter_us;
        }
        if let Some(h) = r.host {
            rep.has_host = 1;
            rep.host_iface_kind = h.iface_kind;
            rep.host_link_mbps = h.link_mbps;
            rep.host_sndbuf_kb = h.sndbuf_kb;
        }
        for leg in &r.legs {
            let slot = match leg.shape {
                LegShape::FrameBursts => 0,
                LegShape::Capped => 1,
            };
            rep.leg_loss_pct[slot] = leg.outcome.loss_pct;
            rep.n_legs = rep.n_legs.max(slot as u8 + 1);
        }
        for (slot, f) in r.findings.iter().take(PUNKTFUNK_HEALTH_FINDINGS_MAX).enumerate() {
            rep.findings[slot] = PunktfunkHealthFinding {
                id: f.id as u8,
                severity: f.severity as u8,
                profile: f.profile.unwrap_or(0),
                numbers: f.numbers,
            };
            rep.n_findings = slot as u8 + 1;
        }
        // SAFETY: `out` is a caller-owned `#[repr(C)]` slot, written once by value.
        unsafe { *out = rep };
        PunktfunkStatus::Ok
    })
}

/// Signal a deliberate quit (user stop, not a network drop) before closing: the
/// connection closes with [`QUIT_CLOSE_CODE`] instead of 0, so the host skips the
/// keep-alive linger. Call before [`punktfunk_connection_close`] on a user disconnect;
/// a plain close leaves the linger intact. NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_connect`] and remains valid until `punktfunk_connection_close`.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_disconnect_quit(c: *mut PunktfunkConnection) {
    guard_void(|| {
        // SAFETY: caller handle or null; `as_mut`/`as_ref` never dereference null.
        if let Some(c) = unsafe { c.as_ref() } {
            c.inner.disconnect_quit();
        }
    });
}

/// Close the connection and free the handle (joins the internal threads). NULL is a no-op.
///
/// # Safety
/// `c` was returned by [`punktfunk_connect`] and is not used after this call.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_close(c: *mut PunktfunkConnection) {
    guard_void(|| {
        if !c.is_null() {
            // SAFETY: pointers are caller-supplied and null-checked on this path.
            drop(unsafe { Box::from_raw(c) });
        }
    });
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::*;

    /// The facts land on the snapshot; a short `struct_size` is a status, not a read.
    #[cfg(feature = "quic")]
    #[test]
    fn hud_facts_apply_to_a_drained_window() {
        let preset = std::ffi::CString::new("Work").unwrap();
        let extras =
            std::ffi::CString::new("2\tlink latency ask 1.00\nno tab\n3\tclock\n").unwrap();
        // SAFETY: an all-zero struct is a valid value (null pointers, zero scalars).
        let mut f: PunktfunkHudFacts = unsafe { std::mem::zeroed() };
        f.struct_size = std::mem::size_of::<PunktfunkHudFacts>() as u32;
        f.on_glass = true;
        f.shave_os_floor = true;
        f.audio_buffer_ms = 28;
        f.av_offset_ms = -3;
        f.preset = preset.as_ptr();
        f.extras = extras.as_ptr();
        // SAFETY: `f` and its strings outlive the call.
        let s = unsafe { hud_with_facts(Default::default(), &f) }.unwrap();
        assert!(s.on_glass && s.shave_os_floor);
        assert_eq!((s.audio_buffer_ms, s.av_offset_ms), (28, -3));
        assert_eq!(s.preset.as_deref(), Some("Work"));
        assert_eq!(s.extras.len(), 2);
        assert_eq!(s.extras[1].role, punktfunk_core::hud::Role::Warn);
        assert!(s.extras.iter().all(|e| e.advanced_only));
        f.struct_size = 8;
        // SAFETY: as above; the short size is the documented rejected case.
        let short = unsafe { hud_with_facts(Default::default(), &f) };
        assert_eq!(short.unwrap_err(), PunktfunkStatus::InvalidArg);
        // SAFETY: null facts are the documented no-op.
        assert!(unsafe { hud_with_facts(Default::default(), std::ptr::null()) }.is_ok());

        // A non-C binding may store 2 in a bool byte; that reads as true, not as UB (Miri).
        f.struct_size = std::mem::size_of::<PunktfunkHudFacts>() as u32;
        let mut bytes = [0u8; std::mem::size_of::<PunktfunkHudFacts>()];
        // SAFETY: a byte copy of an initialised `f` into a buffer of the same size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (&raw const f).cast::<u8>(),
                bytes.as_mut_ptr(),
                bytes.len(),
            )
        };
        bytes[std::mem::offset_of!(PunktfunkHudFacts, on_glass)] = 2;
        // SAFETY: `bytes` holds `struct_size` readable bytes; the strings outlive the call.
        let s = unsafe { hud_with_facts(Default::default(), bytes.as_ptr().cast()) }.unwrap();
        assert!(s.on_glass);
    }

    #[cfg(feature = "quic")]
    #[test]
    fn hud_calls_report_a_null_connection() {
        let null = std::ptr::null();
        let mut buf = [0 as std::os::raw::c_char; 8];
        // SAFETY: null handles are the documented reported-not-UB case.
        let statuses = unsafe {
            [
                punktfunk_connection_hud_decoded(null, 1, 1, 2),
                punktfunk_connection_hud_displayed(null, 1, 2, 3),
                punktfunk_connection_hud_os_floor(null, 1),
                punktfunk_connection_hud_drain(null),
                punktfunk_connection_hud_text(
                    null,
                    3,
                    true,
                    std::ptr::null(),
                    buf.as_mut_ptr(),
                    buf.len(),
                    std::ptr::null_mut(),
                ),
            ]
        };
        assert!(statuses.iter().all(|s| *s == PunktfunkStatus::NullPointer));
    }
}
