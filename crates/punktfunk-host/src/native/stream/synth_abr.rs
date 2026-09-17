//! A rate-following synthetic source: the real send path, no GPU and no display.
//!
//! Frames arrive at the negotiated refresh, each sized from the live wire budget by the
//! arithmetic the encode path uses ([`encoder_kbps_for_budget`]), so a `SetBitrate` moves the
//! bytes on the wire within one frame. [`send_loop`] paces them and answers speed-test bursts
//! exactly as it does for a virtual display; only the encoder is arithmetic.

use super::*;

/// A keyframe against an ordinary frame. Ten, because that is the ratio a real inter-coded
/// 4K stream shows; the link sees one frame's worth of overload, which is what a recovery IDR
/// costs a constrained path.
const IDR_PCT: u64 = 1_000;

/// Repeats and the idle keepalive: one shard, the size a host sends when nothing moved.
const REPEAT_SHARDS: u64 = 1;

/// What the source encodes. Mirrors the simulator's `ContentPhase`, minus the phases no
/// real-time run has the wall clock for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    /// Every frame, `fill_pct` of its allowance.
    Steady { fill_pct: u32 },
    /// Ten seconds of host-marked repeats, then [`Content::Steady`].
    IdleThenMotion { fill_pct: u32 },
    /// A source slower than the session: `fps` new frames a second, each still sized from the
    /// session's per-frame allowance, so the budget goes unspent.
    FrameDriven { fps: u32, fill_pct: u32 },
}

impl Content {
    /// `steady`, `idle-then-motion` or `frame-driven:35`, at `fill_pct` of the allowance.
    pub fn parse(spec: &str, fill_pct: u32) -> Option<Content> {
        match spec.split_once(':') {
            Some(("frame-driven", fps)) => Some(Content::FrameDriven {
                fps: fps.parse().ok().filter(|&f| f > 0)?,
                fill_pct,
            }),
            None => match spec {
                "steady" => Some(Content::Steady { fill_pct }),
                "idle-then-motion" => Some(Content::IdleThenMotion { fill_pct }),
                _ => None,
            },
            _ => None,
        }
    }

    fn fill_pct(self) -> u32 {
        match self {
            Content::Steady { fill_pct }
            | Content::IdleThenMotion { fill_pct }
            | Content::FrameDriven { fill_pct, .. } => fill_pct,
        }
    }

    /// What this script produces `elapsed` into the stream, at a session running `fps`.
    /// `None` = nothing this tick; the source skips it.
    fn frame_at(self, elapsed: std::time::Duration, fps: u32, tick: u64) -> Option<Shot> {
        match self {
            Content::Steady { .. } => Some(Shot::New),
            Content::IdleThenMotion { .. } => {
                if elapsed < std::time::Duration::from_secs(10) {
                    Some(Shot::Repeat)
                } else {
                    Some(Shot::New)
                }
            }
            // Integer cadence over the tick count: 35 of every 165 ticks carry content.
            Content::FrameDriven { fps: src, .. } => {
                let want = u64::from(src.min(fps));
                let per = u64::from(fps.max(1));
                (tick * want / per != (tick + 1) * want / per).then_some(Shot::New)
            }
        }
    }
}

/// What one tick puts on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shot {
    New,
    /// The last picture again, marked so the controller reads the window as stillness.
    Repeat,
}

/// Bytes for one frame at an encoder rate.
///
/// The per-frame allowance is the session's refresh, not the rate the source manages: a
/// frame-driven source spends a slice of the budget and the controller must see that. The
/// caller derives `enc_kbps` from the wire budget through [`encoder_kbps_for_budget`], so
/// parity and the audio reservation are already off it.
fn frame_bytes(
    enc_kbps: u32,
    shard_payload: u16,
    fps: u32,
    fill_pct: u32,
    shot: Shot,
    idr: bool,
) -> usize {
    if shot == Shot::Repeat {
        return (REPEAT_SHARDS * u64::from(shard_payload)) as usize;
    }
    let allowance = u64::from(enc_kbps) * 1_000 / 8 / u64::from(fps.max(1));
    let mut b = allowance * u64::from(fill_pct) / 100;
    if idr {
        b = b * IDR_PCT / 100;
    }
    b.max(1) as usize
}

/// Per-session inputs for [`synthetic_abr_stream`]. The display-side half of
/// [`SessionContext`] has no meaning here and is not carried.
pub(crate) struct SynthAbrContext {
    pub(crate) session: Session,
    pub(crate) mode: punktfunk_core::Mode,
    /// `0` = until the client leaves.
    pub(crate) seconds: u32,
    pub(crate) content: Content,
    /// How long after a keyframe ask a decodable frame reaches the wire. `0` = the next one.
    /// A GPU host that answers with a pipeline rebuild takes about a second, and the asks
    /// that pile up in the meantime are what a report window reads as damage.
    pub(crate) recovery: std::time::Duration,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) counters: Arc<crate::session_status::SessionCounters>,
    pub(crate) keyframe: std::sync::mpsc::Receiver<()>,
    pub(crate) rfi: std::sync::mpsc::Receiver<(u32, u32)>,
    pub(crate) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    pub(crate) shard_rx: std::sync::mpsc::Receiver<usize>,
    /// Total wire budget (kbps): video + FEC + framing + the audio reservation.
    pub(crate) bitrate_kbps: u32,
    pub(crate) audio_reserved_kbps: u32,
    pub(crate) shard_payload: u16,
    pub(crate) live_bitrate: Arc<AtomicU32>,
    pub(crate) fec_target: Arc<AtomicU8>,
    pub(crate) probe_rx: std::sync::mpsc::Receiver<ProbeRequest>,
    pub(crate) probe_result_tx: tokio::sync::mpsc::UnboundedSender<ProbeResult>,
    pub(crate) timing_conn: Option<super::super::link::SessionLink>,
    pub(crate) phase: Arc<PhaseCtl>,
    pub(crate) probe_seq: bool,
    pub(crate) stats: Arc<StatsRecorder>,
    pub(crate) client_label: String,
    pub(crate) bringup: Arc<crate::bringup::Trace>,
    pub(crate) wire_sock: Option<std::net::UdpSocket>,
}

/// Stream until the client leaves, `seconds` elapse, or the send thread goes.
pub(crate) fn synthetic_abr_stream(ctx: SynthAbrContext) -> Result<()> {
    boost_thread_priority(true);
    let SynthAbrContext {
        session,
        mode,
        seconds,
        content,
        recovery,
        stop,
        counters,
        keyframe,
        rfi,
        bitrate_rx,
        shard_rx,
        bitrate_kbps,
        audio_reserved_kbps,
        shard_payload,
        live_bitrate,
        fec_target,
        probe_rx,
        probe_result_tx,
        timing_conn,
        phase,
        probe_seq,
        stats,
        client_label,
        bringup,
        wire_sock,
    } = ctx;
    let fps = mode.refresh_hz.max(1);
    let mut budget_kbps = bitrate_kbps;
    live_bitrate.store(budget_kbps, Ordering::Relaxed);

    // Depth 3, as the virtual path: encode blocks on a slow send rather than dropping a frame.
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<SendMsg>(3);
    let live_mode = Arc::new(AtomicU64::new(pack_mode(mode.width, mode.height, fps)));
    let send_stats = SendStats {
        rec: stats,
        mode: live_mode,
        codec: "synthetic-abr",
        client: client_label,
        bitrate_kbps: live_bitrate.clone(),
        bringup: bringup.clone(),
        wire_sock,
        driver_dropped: Arc::new(AtomicU64::new(0)),
        counters: counters.clone(),
    };
    let send_thread = std::thread::Builder::new()
        .name("punktfunk-send".into())
        .spawn({
            let stop = stop.clone();
            let fec_target = fec_target.clone();
            move || {
                send_loop(
                    session,
                    frame_rx,
                    probe_rx,
                    probe_result_tx,
                    stop,
                    pf_host_config::config().perf,
                    Arc::new(AtomicU32::new(0)),
                    Arc::new(AtomicU32::new(0)),
                    false,
                    std::env::var("PUNKTFUNK_PACE_BURST_KB")
                        .ok()
                        .and_then(|s| s.parse::<usize>().ok())
                        .map(|kb| kb * 1024),
                    fec_target,
                    shard_rx,
                    send_stats,
                    timing_conn,
                    phase,
                    probe_seq,
                )
            }
        })
        .context("spawn send thread")?;

    let interval = std::time::Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let started = std::time::Instant::now();
    let deadline = (seconds > 0).then(|| started + std::time::Duration::from_secs(seconds.into()));
    let mut due = started;
    let (mut au_seq, mut tick) = (0u32, 0u64);
    // An IDR at start, then whenever one comes due. `None` = none owed.
    let mut idr_due: Option<std::time::Instant> = Some(started);
    while !stop.load(Ordering::SeqCst) {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            break;
        }
        // Latest ask wins: the client re-targets faster than a frame only while it is probing.
        let mut want = None;
        while let Ok(k) = bitrate_rx.try_recv() {
            want = Some(k);
        }
        if let Some(new_kbps) = want.filter(|&k| k != budget_kbps) {
            tracing::info!(
                from_kbps = budget_kbps,
                to_kbps = new_kbps,
                requested_kbps = new_kbps,
                "encoder bitrate reconfigured in place (adaptive bitrate — no IDR)"
            );
            counters.note_bitrate(new_kbps);
            budget_kbps = new_kbps;
            live_bitrate.store(budget_kbps, Ordering::Relaxed);
        }
        // Both mean the picture needs re-anchoring, and arithmetic has no reference chain to
        // invalidate: an RFI costs the same IDR here. The first ask sets the clock; asks
        // while one is already owed do not move it, as a rebuild in flight does not restart.
        let asked = keyframe.try_iter().count() + rfi.try_iter().count();
        if asked > 0 {
            idr_due.get_or_insert_with(|| std::time::Instant::now() + recovery);
        }

        let elapsed = started.elapsed();
        if let Some(shot) = content.frame_at(elapsed, fps, tick) {
            let idr = idr_due.is_some_and(|t| std::time::Instant::now() >= t);
            if idr {
                idr_due = None;
            }
            // Re-derived per frame: an adaptive-FEC move changes the picture's share of the
            // budget without the budget moving.
            let enc_kbps = encoder_kbps_for_budget(
                budget_kbps,
                audio_reserved_kbps,
                fec_target.load(Ordering::Relaxed),
                shard_payload,
            );
            let len = frame_bytes(enc_kbps, shard_payload, fps, content.fill_pct(), shot, idr);
            let flags = if idr {
                u32::from(FLAG_PIC | FLAG_SOF)
            } else {
                u32::from(FLAG_PIC)
            };
            let msg = FrameMsg {
                data: test_frame(au_seq, len),
                capture_ns: now_ns(),
                flags,
                frame_index: au_seq,
                deadline: due + interval,
                // A plausible encode: half a frame budget, which is what a GPU that is not
                // the bottleneck reads. The client's encode-stage detector needs a number
                // in the right decade, not a model.
                encode_us: (500_000 / fps).max(1),
                queue_us: 0,
                cap_us: 0,
                submit_us: 0,
                wait_us: 0,
                repeat: shot == Shot::Repeat,
                was_measured: true,
                driver: false,
                split: false,
                ipc_us: 0,
            };
            if frame_tx.send(SendMsg::Frame(msg)).is_err() {
                break;
            }
            au_seq = au_seq.wrapping_add(1);
        }
        tick += 1;
        due += interval;
        // A late tick forfeits its sleep rather than its cadence; a very late one re-anchors
        // so the source cannot spend the rest of the session catching up.
        let now = std::time::Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        } else if now - due > interval * 4 {
            due = now;
        }
    }
    drop(frame_tx);
    stop.store(true, Ordering::SeqCst);
    let _ = send_thread.join();
    tracing::info!(
        frames = au_seq,
        budget_kbps,
        "synthetic-abr stream complete"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame's bytes are the encoder's per-frame allowance at the budget in force, so a
    /// second of frames spends exactly what `abr::budget` says the picture may have. The
    /// arithmetic is the encode path's, not a second one that can drift from it.
    #[test]
    fn a_seconds_frames_spend_the_encoder_rate_abr_budget_derives() {
        for budget in [2_000u32, 12_500, 20_000, 171_294] {
            for fec in [5u8, 10, 25] {
                for fps in [30u32, 60, 165] {
                    let enc = encoder_kbps_for_budget(budget, 512, fec, 1408);
                    let one = frame_bytes(enc, 1408, fps, 100, Shot::New, false);
                    let second = one as u64 * u64::from(fps);
                    let want = u64::from(enc) * 1_000 / 8;
                    assert!(
                        second <= want && want - second < u64::from(fps) * 8,
                        "budget {budget} fec {fec} at {fps} fps: {second} bytes a second \
                         against the {want} the encoder rate allows"
                    );
                }
            }
        }
    }

    /// Fill scales the frame, a keyframe is [`IDR_PCT`] of one, and a repeat is one shard
    /// whatever the budget — the three shapes the controller reads differently.
    #[test]
    fn fill_idr_and_repeat_each_size_their_own_frame() {
        let enc = encoder_kbps_for_budget(20_000, 512, 10, 1408);
        let at = |fill, shot, idr| frame_bytes(enc, 1408, 60, fill, shot, idr);
        let full = at(100, Shot::New, false);
        assert_eq!(at(50, Shot::New, false), full / 2, "fill halves the frame");
        assert_eq!(
            at(100, Shot::New, true) as u64,
            full as u64 * IDR_PCT / 100,
            "a keyframe is IDR_PCT of an ordinary frame"
        );
        assert_eq!(at(100, Shot::Repeat, false), 1408, "a repeat is one shard");
        assert_eq!(
            at(100, Shot::Repeat, true),
            1408,
            "even when an IDR is owed"
        );
    }

    /// A frame-driven source produces its own rate, not the session's, and every other
    /// script produces every tick.
    #[test]
    fn the_content_script_decides_which_ticks_carry_a_picture() {
        let steady = Content::parse("steady", 80).expect("steady parses");
        assert_eq!(steady.fill_pct(), 80);
        let d = std::time::Duration::from_secs(30);
        assert!((0..165).all(|t| steady.frame_at(d, 165, t).is_some()));

        let fd = Content::parse("frame-driven:35", 100).expect("frame-driven parses");
        let carried = (0..165)
            .filter(|&t| fd.frame_at(d, 165, t).is_some())
            .count();
        assert_eq!(carried, 35, "35 of 165 ticks carry content");

        let idle = Content::parse("idle-then-motion", 100).expect("idle-then-motion parses");
        assert_eq!(
            idle.frame_at(std::time::Duration::from_secs(1), 60, 0),
            Some(Shot::Repeat)
        );
        assert_eq!(
            idle.frame_at(std::time::Duration::from_secs(11), 60, 0),
            Some(Shot::New)
        );

        assert_eq!(Content::parse("frame-driven:0", 100), None);
        assert_eq!(Content::parse("nonsense", 100), None);
    }
}
