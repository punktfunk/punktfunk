//! Display + capture + encoder bring-up for one session: [`build_pipeline`] and its bounded
//! retry, the Welcome-time [`prepare_display`], and [`open_session_encoder`], which every
//! rebuild path (mode switch, capture loss, bitrate fallback) goes through.

use super::*;

/// One built pipeline. `bitrate_kbps` is the rate the encoder actually opened at.
pub(in crate::native) struct Pipeline {
    pub(super) capturer: Box<dyn crate::capture::Capturer>,
    pub(super) enc: Box<dyn crate::encode::Encoder>,
    pub(super) frame: crate::capture::CapturedFrame,
    pub(super) interval: std::time::Duration,
    pub(super) node_id: u32,
    pub(super) display_gen: Option<u64>,
    pub(super) bitrate_kbps: u32,
    /// What the encoder takes of each captured picture.
    pub(super) reframe: punktfunk_core::video_fit::Reframe,
}

/// Display + pipeline built on the prep thread while Start RTT and hole-punch are in flight.
pub(in crate::native) struct PreparedDisplay {
    pub(super) vd: Box<dyn crate::vdisplay::VirtualDisplay>,
    pub(super) pipeline: Pipeline,
}

/// Prep thread: sender delivers [`SessionContext`]; drop un-received aborts into keep-alive.
pub(in crate::native) type PrepHandle = (
    std::sync::mpsc::SyncSender<SessionContext>,
    std::thread::JoinHandle<Result<()>>,
);

/// Build display + pipeline at Welcome time. Same setters as [`StreamState::new`]'s inline arm.
/// Windows-only by policy (`handshake.rs` never spawns it elsewhere), not by construction.
#[allow(clippy::too_many_arguments)]
pub(in crate::native) fn prepare_display(
    compositor: crate::vdisplay::Compositor,
    mode: punktfunk_core::Mode,
    client_identity: Option<[u8; 32]>,
    client_hdr: Option<pf_frame::HdrMeta>,
    cursor_forward: bool,
    multi_slice: bool,
    bitrate_kbps: u32,
    bitrate_auto: bool,
    bit_depth: u8,
    hdr: bool,
    enc_of: super::EncDerive,
    chroma: crate::encode::ChromaFormat,
    codec: crate::encode::Codec,
    shard_payload: u16,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    trace: &crate::bringup::Trace,
) -> Result<PreparedDisplay> {
    let mut plan = crate::session_plan::SessionPlan::resolve(
        bit_depth,
        hdr,
        chroma,
        codec,
        crate::session_plan::cursor_blend_for(
            cursor_forward,
            compositor == pf_vdisplay::Compositor::Gamescope,
            codec,
            bit_depth,
        ),
        cursor_forward,
        multi_slice,
    );
    plan.gamescope_cursor =
        crate::session_plan::gamescope_cursor_for(compositor == pf_vdisplay::Compositor::Gamescope);
    if codec == crate::encode::Codec::PyroWave {
        plan.wire_chunk = Some(shard_payload as usize);
    }
    let mut vd = crate::vdisplay::open(compositor)?;
    vd.set_client_identity(client_identity);
    vd.set_client_hdr(client_hdr);
    vd.set_hdr(hdr);
    vd.set_hw_cursor(cursor_forward);
    vd.set_quit_flag(quit.clone());
    let _idd_setup_guard = crate::windows::idd::setup_guard(
        plan.capture,
        client_identity,
        (mode.width, mode.height),
        stop,
    )?;
    let pipeline = build_pipeline_with_retry(
        &mut vd,
        mode,
        bitrate_kbps,
        bitrate_auto,
        bit_depth,
        enc_of,
        plan,
        quit,
        stop,
        None,
        8,
        Some(trace),
        client_hdr,
        0,
    )?;
    Ok(PreparedDisplay { vd, pipeline })
}

/// Retry transient first-frame races. Permanent errors short-circuit, except a driver with no
/// render device, which gets one adapter reload first ([`cycled_driver_for`]). Each failed
/// attempt drops its capturer so the next create is clean. `supersedes` is the lease this
/// build replaces (create-before-drop): without it the registry counts the old lease as a live
/// sibling and the new display extends the group instead of heading it.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_pipeline_with_retry(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bitrate_auto: bool,
    bit_depth: u8,
    enc_of: super::EncDerive,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    stop: &Arc<AtomicBool>,
    supersedes: Option<u64>,
    max_attempts: u32,
    trace: Option<&crate::bringup::Trace>,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<Pipeline> {
    // IDD-push: hold one lease across attempts so a failed capturer drop does not Lingering-preempt.
    let _retry_hold = if matches!(plan.capture, crate::session_plan::CaptureBackend::IddPush) {
        Some(
            vd.create(display_mode_for(mode))
                .context("acquire virtual output for the session (retry-hold lease)")?,
        )
    } else {
        None
    };
    const FIRST_ATTEMPT_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(2500);
    let mut backoff = std::time::Duration::from_millis(500);
    for attempt in 1..=max_attempts {
        if attempt > 1 && stop.load(Ordering::SeqCst) {
            anyhow::bail!(
                "session ended (client disconnected) during pipeline build — aborting retries \
                 after {} attempt(s)",
                attempt - 1
            );
        }
        let first_frame_budget = (attempt == 1).then_some(FIRST_ATTEMPT_FRAME_BUDGET);
        match build_pipeline(
            vd,
            mode,
            bitrate_kbps,
            bitrate_auto,
            bit_depth,
            enc_of,
            plan,
            quit,
            supersedes,
            first_frame_budget,
            trace,
            client_hdr,
            wire_seq_base,
        ) {
            Ok(pipe) => {
                if attempt > 1 {
                    tracing::info!(attempt, "pipeline up after retry");
                }
                return Ok(pipe);
            }
            Err(e) => {
                let chain = format!("{e:#}");
                let permanent = is_permanent_build_error(&chain) && !cycled_driver_for(&e);
                if permanent || attempt == max_attempts {
                    let why = if permanent {
                        "permanent"
                    } else {
                        "out of retries"
                    };
                    return Err(e).with_context(|| {
                        format!("pipeline build failed ({why}) after {attempt} attempt(s)")
                    });
                }
                tracing::warn!(
                    attempt,
                    max = max_attempts,
                    backoff_ms = backoff.as_millis() as u64,
                    error = %chain,
                    "pipeline build failed — retrying"
                );
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
            }
        }
    }
    unreachable!("the final attempt returns inside the loop")
}

/// A driver that cannot create its render device fails every open until the adapter reloads
/// into a fresh WUDFHost, so that failure earns one reload and a retry. At most once a minute
/// host-wide: the rebuild loop calls this once per build.
#[cfg(target_os = "windows")]
fn cycled_driver_for(e: &anyhow::Error) -> bool {
    const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if !is_driver_no_device(e) {
        return false;
    }
    {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if last.is_some_and(|t| t.elapsed() < COOLDOWN) {
            return false;
        }
        *last = Some(std::time::Instant::now());
    }
    match crate::vdisplay::driver::force_driver_cycle_if_sole() {
        Ok(()) => {
            tracing::warn!("driver adapter reloaded for a missing render device — retrying");
            true
        }
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "driver cycle for a missing render device did not run"
            );
            false
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn cycled_driver_for(_: &anyhow::Error) -> bool {
    false
}

/// `SET_ENCODE` answered `NO_DEVICE`: the driver has no render device for the monitor.
#[cfg(target_os = "windows")]
fn is_driver_no_device(e: &anyhow::Error) -> bool {
    e.downcast_ref::<pf_capture::DriverEncodeOpenError>()
        .is_some_and(|d| d.status == pf_driver_proto::encode::SET_ENCODE_NO_DEVICE)
}

/// Permanent = retrying cannot help this session. Match our English prefix, not KWin's translated payload.
pub(super) fn is_permanent_build_error(chain: &str) -> bool {
    const PERMANENT: &[&str] = &[
        "virtual displays require linux",
        "unknown punktfunk_compositor",
        "compositor not detected",
        "kwin virtual output failed",
        "must be a node id",
        "is it installed",
        "capture/encoder negotiation mismatch",
        "driver encoder open failed",
        // A backend compiled out cannot appear on a retry, and each one costs
        // the user's real monitor a disable/restore cycle.
        "this build left out",
        // A pinned monitor the host does not have. Retrying holds the client on a
        // black screen for the whole backoff before saying so.
        "no monitor named",
    ];
    let lower = chain.to_ascii_lowercase();
    PERMANENT.iter().any(|p| lower.contains(p))
}

/// Session mode with refresh × `PUNKTFUNK_VDISPLAY_HZ_MULT`. Wire rate is still [`pacing_hz`].
pub(super) fn display_mode_for(session: punktfunk_core::Mode) -> punktfunk_core::Mode {
    let mult = pf_host_config::config().vdisplay_hz_mult.max(1);
    punktfunk_core::Mode {
        refresh_hz: session.refresh_hz.saturating_mul(mult).min(0xffff),
        ..session
    }
}

/// Pace and encode at min(session, achieved). Overdrive is display-only.
pub(super) fn pacing_hz(session_hz: u32, achieved_hz: u32) -> u32 {
    achieved_hz.min(session_hz).max(1)
}

/// Open the session's encoder for `frame` — framed for a joiner, or at the client's `negotiated`
/// size when a larger head is mirrored (`session_plan::open_encoder_fitted`) — with the plan's
/// chunking and the capturer's ring depth applied, and return the framing it encodes. `bitrate_bps` is asked
/// for that size. An IDD-push source gets the driver's encoder instead, its wire-index domain
/// continuing at `wire_seq_base` (the loop's `au_seq`).
#[allow(clippy::too_many_arguments)]
pub(super) fn open_session_encoder(
    plan: &crate::session_plan::SessionPlan,
    capturer: &dyn crate::capture::Capturer,
    frame: &crate::capture::CapturedFrame,
    negotiated: (u32, u32),
    hz: u32,
    bitrate_bps: impl Fn(u32, u32) -> u64,
    bit_depth: u8,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<(
    Box<dyn crate::encode::Encoder>,
    punktfunk_core::video_fit::Reframe,
)> {
    if plan.capture == crate::session_plan::CaptureBackend::IddPush {
        return crate::windows::idd::open_driver_encoder(
            plan,
            capturer,
            (frame.width, frame.height),
            hz,
            bitrate_bps(frame.width, frame.height),
            bit_depth,
            client_hdr,
            wire_seq_base,
        )
        .map(|e| {
            (
                e,
                punktfunk_core::video_fit::Reframe::full((frame.width, frame.height)),
            )
        });
    }
    let (mut enc, framed) = crate::session_plan::open_encoder_fitted(
        frame,
        negotiated,
        plan.reframe_to,
        |width, height| {
            crate::encode::open_video(
                plan.codec,
                frame.format,
                width,
                height,
                hz,
                bitrate_bps(width, height),
                frame.is_cuda(),
                bit_depth,
                plan.chroma,
                plan.cursor_blend,
                plan.max_slices,
            )
        },
    )?;
    if let Some(c) = plan.wire_chunk {
        enc.set_wire_chunking(c);
    }
    enc.set_input_ring_depth(capturer.pipeline_depth().max(1));
    Ok((enc, framed))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_pipeline(
    vd: &mut Box<dyn crate::vdisplay::VirtualDisplay>,
    mode: punktfunk_core::Mode,
    bitrate_kbps: u32,
    bitrate_auto: bool,
    bit_depth: u8,
    enc_of: super::EncDerive,
    plan: crate::session_plan::SessionPlan,
    quit: &Arc<AtomicBool>,
    supersedes: Option<u64>,
    first_frame_budget: Option<std::time::Duration>,
    trace: Option<&crate::bringup::Trace>,
    client_hdr: Option<pf_frame::HdrMeta>,
    wire_seq_base: u32,
) -> Result<Pipeline> {
    let display_mode = display_mode_for(mode);
    let vout = crate::vdisplay::registry::acquire(vd, display_mode, quit.clone(), supersedes)
        .context("create virtual output")?;
    if let Some(t) = trace {
        t.mark("display_acquired");
    }
    #[cfg(target_os = "linux")]
    let reused_gen = vout.reused_gen;
    #[cfg(target_os = "linux")]
    let pool_gen = vout.pool_gen;
    #[cfg(not(target_os = "linux"))]
    let pool_gen = None;
    let node_id = vout.node_id;
    #[cfg(target_os = "linux")]
    let cursor_seat = vout.seat.clone();
    let achieved_hz = vout
        .preferred_mode
        .map(|(_, _, hz)| hz)
        .filter(|&hz| hz > 0)
        .unwrap_or(display_mode.refresh_hz);
    if achieved_hz < mode.refresh_hz {
        tracing::warn!(
            requested = display_mode.refresh_hz,
            achieved = achieved_hz,
            session = mode.refresh_hz,
            "compositor did not honor the requested refresh — encoding at the achieved rate"
        );
    } else if achieved_hz < display_mode.refresh_hz {
        tracing::info!(
            requested = display_mode.refresh_hz,
            achieved = achieved_hz,
            session = mode.refresh_hz,
            "compositor did not honor the multiplied display refresh — the session rate is unaffected"
        );
    }
    let effective_hz = pacing_hz(mode.refresh_hz, achieved_hz);
    // A mirrored KWin head is still KWin's stream: its id 0 hides the pointer.
    let cursor_id0_hides = vd.producer() == pf_vdisplay::Compositor::Kwin.id();
    let producer_is_gamescope = vd.name() == pf_vdisplay::Compositor::Gamescope.id();
    let mut capturer = crate::capture::capture_virtual_output(
        vout,
        plan.output_format(),
        plan.capture,
        cursor_id0_hides,
        producer_is_gamescope,
    )
    .context("capture virtual output")?;
    #[cfg(target_os = "linux")]
    if plan.gamescope_cursor {
        capturer.attach_gamescope_cursor(std::sync::Arc::new(move || {
            pf_vdisplay::gamescope_xwayland_cursor_targets(cursor_seat.as_deref())
        }));
    }
    if let Some(t) = trace {
        t.mark("capture_attached");
    }
    capturer.set_active(true);
    let first = match first_frame_budget {
        Some(budget) => capturer.next_frame_within_provisional(budget),
        None => capturer.next_frame(),
    };
    let frame = match first.context("first frame") {
        Ok(f) => f,
        Err(e) => {
            #[cfg(target_os = "linux")]
            if let Some(g) = reused_gen {
                crate::vdisplay::registry::mark_failed(g);
            }
            return Err(e);
        }
    };
    if let Some(t) = trace {
        t.mark("first_frame");
    }
    let negotiated = (mode.width, mode.height);
    // Automatic bitrate follows the pixels actually encoded: a mirrored head's fit, or
    // whatever a virtual display delivered.
    let kbps_for = |w: u32, h: u32| {
        if bitrate_auto && (w, h) != negotiated {
            let encoded = punktfunk_core::Mode {
                width: w,
                height: h,
                ..mode
            };
            resolve_bitrate_kbps_for(plan.codec, 0, &encoded, plan.chroma, bit_depth)
        } else {
            bitrate_kbps
        }
    };
    let (enc, reframe) = open_session_encoder(
        &plan,
        &*capturer,
        &frame,
        negotiated,
        effective_hz,
        |w, h| enc_of.enc_kbps(kbps_for(w, h)) as u64 * 1000,
        bit_depth,
        client_hdr,
        wire_seq_base,
    )
    .context("open video encoder")?;
    let encoded = reframe.out;
    let re = kbps_for(encoded.0, encoded.1);
    if re != bitrate_kbps {
        tracing::info!(
            negotiated = %format!("{}x{}", mode.width, mode.height),
            encoded = %format!("{}x{}", encoded.0, encoded.1),
            from_kbps = bitrate_kbps,
            to_kbps = re,
            "the encoder opened at a size other than the session negotiated — re-resolved the \
             Automatic bitrate for the pixels actually being encoded"
        );
    }
    let bitrate_kbps = re;
    if let Some(t) = trace {
        t.mark("encoder_open");
    }
    let opened_444 = enc.caps().chroma_444;
    if opened_444 != plan.chroma.is_444() {
        tracing::warn!(
            negotiated_444 = plan.chroma.is_444(),
            opened_444,
            "encoder chroma disagrees with the negotiated Welcome — the client was told the other value"
        );
    }
    let interval = std::time::Duration::from_secs_f64(1.0 / effective_hz.max(1) as f64);
    Ok(Pipeline {
        capturer,
        enc,
        frame,
        interval,
        node_id,
        display_gen: pool_gen,
        bitrate_kbps,
        reframe,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pacing_never_exceeds_the_session_rate_or_the_display() {
        assert_eq!(pacing_hz(120, 120), 120);
        assert_eq!(pacing_hz(120, 60), 60);
        assert_eq!(pacing_hz(60, 120), 60);
        assert_eq!(pacing_hz(120, 240), 120);
        assert_eq!(pacing_hz(60, 90), 60);
        assert_eq!(pacing_hz(60, 0), 1);
    }

    #[test]
    fn display_mode_multiplier_scales_only_the_refresh() {
        let session = punktfunk_core::Mode {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
        };
        let display = display_mode_for(session);
        assert_eq!((display.width, display.height), (2560, 1440));
        assert_eq!(
            display.refresh_hz,
            session.refresh_hz * pf_host_config::config().vdisplay_hz_mult.max(1)
        );
    }

    #[test]
    fn permanent_errors_short_circuit_retry() {
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Could not find output"
        ));
        assert!(is_permanent_build_error(
            "create virtual output: KWin virtual output failed: Não foi possível encontrar saída"
        ));
        assert!(is_permanent_build_error(
            "unknown PUNKTFUNK_COMPOSITOR 'foo' (kwin|wlroots|mutter|gamescope)"
        ));
        assert!(is_permanent_build_error(
            "spawn gamescope (is it installed? `apt install gamescope`)"
        ));
        assert!(is_permanent_build_error("virtual displays require Linux"));
        assert!(!is_permanent_build_error(
            "create virtual output: KWin created the virtual output disabled and refused to \
             stream it (stream_virtual_output failed: Não foi possível encontrar saída); enabled \
             it over output management (head Virtual-punktfunk-a1b2) — the retry picks up the \
             configuration KWin just persisted"
        ));
        assert!(!is_permanent_build_error(
            "first frame: no PipeWire frame within 10s (node 42): format negotiation never completed"
        ));
        assert!(!is_permanent_build_error(
            "create virtual output: timed out creating the KWin virtual output"
        ));
        assert!(is_permanent_build_error(
            "open video encoder: hevc on NVIDIA needs the direct-SDK NVENC backend, which this \
             build left out — build with --features punktfunk-host/nvenc"
        ));
        assert!(is_permanent_build_error(
            "create virtual output: no monitor named \"HDMI-A-3\" — this host has: HDMI-A-1"
        ));
        assert!(!is_permanent_build_error("open NVENC: device busy"));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn driver_no_device_is_found_through_the_context_chain() {
        use pf_driver_proto::encode::{SET_ENCODE_NO_BACKEND, SET_ENCODE_NO_DEVICE};
        let open = |status| {
            anyhow::Error::new(pf_capture::DriverEncodeOpenError {
                backends: [1, 0, 0, 0],
                status,
                error: 0x8007_000Eu32 as i32,
                name: "d3d11".into(),
            })
            .context("open video encoder")
        };
        assert!(is_driver_no_device(&open(SET_ENCODE_NO_DEVICE)));
        assert!(!is_driver_no_device(&open(SET_ENCODE_NO_BACKEND)));
    }
}
