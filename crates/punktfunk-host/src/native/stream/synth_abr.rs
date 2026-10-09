//! A rate-following synthetic source: the real send path, no GPU and no display.
//!
//! Frames arrive at the negotiated refresh, each sized from the live wire budget by the
//! arithmetic the encode path uses ([`encoder_kbps_for_budget`]), so a `SetBitrate` moves the
//! bytes on the wire within one frame. [`send_loop`] paces them and answers speed-test bursts
//! exactly as it does for a virtual display; only the encoder is arithmetic.

use super::recovery::{KeyframeGate, KeyframeVerdict, IDR_COOLDOWN_FULL};
use super::*;
use crate::session_status::pack_mode;
use punktfunk_core::abr::budget::encoder_kbps_for_budget;

/// A keyframe against an ordinary frame, percent, when `--idr-pct` says nothing. Ten times,
/// which is what rounds 4–7 measured against; a hardware encoder runs `PUNKTFUNK_VBV_FRAMES`
/// = 1.0 and fits its keyframe near one frame's share, so this is the pessimistic end.
pub const DEFAULT_IDR_PCT: u32 = 1_000;

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
    /// A minute of [`Content::Steady`], then a desktop gone still: `fps` new frames a second
    /// and host-marked repeats between them.
    MotionThenStill { fps: u32, fill_pct: u32 },
}

/// How long [`Content::MotionThenStill`] moves before it goes still.
const STILL_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

impl Content {
    /// `steady`, `idle-then-motion`, `frame-driven:35` or `motion-then-still:5`, at
    /// `fill_pct` of the allowance.
    pub fn parse(spec: &str, fill_pct: u32) -> Option<Content> {
        match spec.split_once(':') {
            Some(("frame-driven", fps)) => Some(Content::FrameDriven {
                fps: fps.parse().ok().filter(|&f| f > 0)?,
                fill_pct,
            }),
            Some(("motion-then-still", fps)) => Some(Content::MotionThenStill {
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
            | Content::FrameDriven { fill_pct, .. }
            | Content::MotionThenStill { fill_pct, .. } => fill_pct,
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
            Content::FrameDriven { fps: src, .. } => cadence(src, fps, tick).then_some(Shot::New),
            Content::MotionThenStill { .. } if elapsed < STILL_AFTER => Some(Shot::New),
            Content::MotionThenStill { fps: src, .. } => Some(if cadence(src, fps, tick) {
                Shot::New
            } else {
                Shot::Repeat
            }),
        }
    }
}

/// Whether tick `tick` of a session at `fps` carries one of `src` new frames a second.
fn cadence(src: u32, fps: u32, tick: u64) -> bool {
    let want = u64::from(src.min(fps));
    let per = u64::from(fps.max(1));
    tick * want / per != (tick + 1) * want / per
}

/// How the source answers a keyframe request.
///
/// The field shows both: `host173` 09-17 09:53 logged `keyframe_req=9 idr=2 rfi=8` in one
/// minute — most asks answered with an intra-refresh wave, which does not re-anchor a client
/// that lost its reference, and only some with a real IDR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyframeAnswer {
    /// Every ask gets an IDR.
    Idr,
    /// Every n-th ask gets an IDR; the rest get an intra-refresh wave, which this source
    /// sends as nothing at all. A wave costs bits a real encoder spends and this one does
    /// not — the conservative direction, since extra bytes would only damage the link more.
    Wave(u32),
}

impl KeyframeAnswer {
    /// `idr` or `wave:<n>`.
    pub fn parse(spec: &str) -> Option<KeyframeAnswer> {
        match spec.split_once(':') {
            None if spec == "idr" => Some(KeyframeAnswer::Idr),
            Some(("wave", n)) => n.parse().ok().filter(|&n| n > 0).map(KeyframeAnswer::Wave),
            _ => None,
        }
    }

    /// Whether the `n`th ask of this session is answered with an IDR.
    fn answers(self, asks: u32) -> bool {
        match self {
            KeyframeAnswer::Idr => true,
            KeyframeAnswer::Wave(every) => asks % every == 0,
        }
    }
}

/// How a `synthetic-abr` session behaves, as the command line sets it.
///
/// One value on [`Punktfunk1Source`](crate::native::Punktfunk1Source) rather
/// than four: the source is the only thing that reads any of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SynthAbrShape {
    pub content: Content,
    /// How long a keyframe ask takes to reach the wire.
    pub recovery: std::time::Duration,
    pub answer: KeyframeAnswer,
    /// A keyframe's size as a percent of an ordinary frame ([`DEFAULT_IDR_PCT`]).
    pub idr_pct: u32,
    /// How long the first frame is held back, as a pipeline build holds it.
    pub bringup: std::time::Duration,
    /// Advertise `HOST_CAP2_RAMP`. `false` is the old-host control: the client
    /// falls back to the legacy in-session burst.
    pub serve_ramp: bool,
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
    idr_pct: u32,
) -> usize {
    if shot == Shot::Repeat {
        return (REPEAT_SHARDS * u64::from(shard_payload)) as usize;
    }
    let allowance = u64::from(enc_kbps) * 1_000 / 8 / u64::from(fps.max(1));
    let mut b = allowance * u64::from(fill_pct) / 100;
    if idr {
        b = b * u64::from(idr_pct) / 100;
    }
    b.max(1) as usize
}

/// When the next IDR is owed after one ask, with production's gate in front of it.
///
/// [`KeyframeGate`] and [`IDR_COOLDOWN_FULL`] are `recovery.rs`'s own, so the rig cannot
/// drift from the host it models. A coalesced ask owes the IDR the cooldown ends on:
/// production sends nothing and a client asking at 10 Hz forces one the moment the gate
/// opens, which puts the same frame on the same wire.
fn owe_idr(
    gate: &mut KeyframeGate,
    now: std::time::Instant,
    recovery: std::time::Duration,
    last_idr: Option<std::time::Instant>,
    idr_due: Option<std::time::Instant>,
) -> Option<std::time::Instant> {
    match gate.decide(now, IDR_COOLDOWN_FULL, last_idr, None, false) {
        KeyframeVerdict::Force { .. } => Some(idr_due.unwrap_or(now + recovery)),
        KeyframeVerdict::Coalesced { cooldown, .. } => {
            idr_due.or_else(|| last_idr.map(|t| t + cooldown))
        }
        // Unreachable: this source has no RFI to echo, so it never passes one in.
        KeyframeVerdict::RfiEcho { .. } => idr_due,
    }
}

/// Per-session inputs for [`synthetic_abr_stream`]: the [`StreamCommon`] plus the shape it
/// encodes. The display-side half of [`SessionContext`] has no meaning here and is not carried.
pub(crate) struct SynthAbrContext {
    pub(crate) common: StreamCommon,
    pub(crate) content: Content,
    /// How long after a keyframe ask a decodable frame reaches the wire. `0` = the next one.
    /// A GPU host that answers with a pipeline rebuild takes about a second, and the asks
    /// that pile up in the meantime are what a report window reads as damage.
    pub(crate) recovery: std::time::Duration,
    pub(crate) answer: KeyframeAnswer,
    /// A keyframe's size as a percent of an ordinary frame ([`DEFAULT_IDR_PCT`]).
    pub(crate) idr_pct: u32,
    /// How long the first frame is held back, the way a display session's
    /// pipeline build holds it. The client's bring-up ramp is served on the
    /// idle data plane for exactly this long.
    pub(crate) bringup_delay: std::time::Duration,
    /// Explicit-rate PyroWave: the client's ramp closes with one lower pin, so
    /// the window lingers a bounded grace past the fake bring-up for it to cross.
    pub(crate) fit_pin: bool,
    /// What [`crate::session_status::register`] needs and this source can't derive: which
    /// plane carries it, and the client's address, which is how the shared-path governor
    /// groups sessions.
    pub(crate) plane: crate::events::Plane,
    pub(crate) peer: std::net::IpAddr,
}

/// Stream until the client leaves, `seconds` (`0` = until the client leaves) elapse, or the
/// send thread goes.
pub(crate) fn synthetic_abr_stream(ctx: SynthAbrContext) -> Result<()> {
    pf_frame::thread_qos::boost_thread_priority(true);
    let SynthAbrContext {
        common:
            StreamCommon {
                session,
                mode,
                seconds,
                stop,
                quit,
                end_reason,
                counters,
                ends:
                    StreamEnds {
                        keyframe,
                        rfi,
                        bitrate_rx,
                        shard_rx,
                        probe_rx,
                        probe_result_tx,
                        ..
                    },
                shared:
                    SessionShared {
                        live_bitrate,
                        fec_target,
                        shape,
                        ports,
                        ramp_open,
                        resend,
                        link_kbps,
                        ..
                    },
                bitrate_kbps,
                audio_reserved_kbps,
                shard_payload,
                timing_conn,
                probe_seq,
                stats,
                client_label,
                bringup,
                wire_sock,
                codec,
                controls,
                client_name,
                hdr,
                bit_depth,
                chroma,
            },
        content,
        recovery,
        answer,
        idr_pct,
        bringup_delay,
        fit_pin,
        plane,
        peer,
    } = ctx;
    let fps = mode.refresh_hz.max(1);
    let mut budget_kbps = bitrate_kbps;
    live_bitrate.store(budget_kbps, Ordering::Relaxed);

    // The bring-up a display session spends on its pipeline, without one. The
    // client measures the link here, on a data plane with no video to damage,
    // and the session below opens at what it found.
    let ramp = ramp::RampServer::start(
        session,
        probe_rx,
        probe_result_tx.clone(),
        probe_seq,
        stop.clone(),
        ramp_open,
        fit_pin,
    );
    let build_until = std::time::Instant::now() + bringup_delay;
    while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < build_until {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let (session, probe_rx) = ramp.finish();
    tracing::info!(
        bringup_ms = bringup_delay.as_millis() as u64,
        "synthetic-abr bring-up done — the ramp window is closed"
    );

    // Depth 3, as the virtual path: encode blocks on a slow send rather than dropping a frame.
    let (frame_tx, frame_rx) = std::sync::mpsc::sync_channel::<SendMsg>(3);
    // A client's NACK reaches the send thread through a weak handle, as on the virtual path.
    let frame_tx = Arc::new(frame_tx);
    let nack_tx = Arc::downgrade(&frame_tx);
    *resend.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(move |frame, shards| {
        nack_tx
            .upgrade()
            .is_some_and(|tx| tx.try_send(SendMsg::Resend { frame, shards }).is_ok())
    }));
    let live_mode = Arc::new(AtomicU64::new(pack_mode(mode.width, mode.height, fps)));
    let send_stats = SendStats {
        rec: stats,
        mode: live_mode.clone(),
        codec: "synthetic-abr",
        client: client_label.clone(),
        plane,
        bitrate_kbps: live_bitrate.clone(),
        link_kbps,
        ports,
        link_paced: false,
        shape,
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
                    fec_target,
                    shard_rx,
                    send_stats,
                    timing_conn,
                    probe_seq,
                )
            }
        })
        .context("spawn send thread")?;

    let interval = std::time::Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let started = std::time::Instant::now();
    // The ramp's opening `SetBitrate` lands while the pipeline is still
    // building, so the first frame must already be sized by it.
    let mut opening = None;
    while let Ok(k) = bitrate_rx.try_recv() {
        opening = Some(k);
    }
    if let Some(k) = opening.filter(|&k| k != budget_kbps) {
        tracing::info!(
            from_kbps = budget_kbps,
            to_kbps = k,
            "encoder opened at the bring-up ramp's rate"
        );
        counters.note_bitrate(k);
        budget_kbps = k;
        live_bitrate.store(budget_kbps, Ordering::Relaxed);
    }
    // Published where the display path publishes, once the encoder has its opening rate:
    // registration latches the id the control task asks the shared-path governor with
    // ([`crate::session_status::share_for`]). The guard retires the entry on every exit below.
    let _live_session = crate::session_status::register(crate::session_status::Registration {
        mode: live_mode,
        bitrate_kbps: live_bitrate.clone(),
        codec,
        stop: stop.clone(),
        quit,
        // Nothing drains it here: a console force-keyframe is a no-op on this source.
        force_idr: Arc::new(AtomicBool::new(false)),
        client: client_label,
        client_name,
        plane,
        hdr,
        ttff_ms: bringup.total_slot(),
        // Never written: a source that cannot reconfigure never resizes.
        last_resize_ms: Arc::new(AtomicU32::new(0)),
        // No display, so no launch and no lease: this session plays no title.
        game: None,
        // No capturer to classify, which the governor reads as a session that is not idle.
        capture_health: Arc::new(std::sync::Mutex::new(None)),
        join: false,
        controls,
        bit_depth,
        chroma,
        end_reason,
        counters: counters.clone(),
        peer: Some(peer),
    });
    let deadline = (seconds > 0).then(|| started + std::time::Duration::from_secs(seconds.into()));
    let mut due = started;
    let (mut au_seq, mut tick, mut asks) = (0u32, 0u64, 0u32);
    // An IDR at start, then whenever one comes due. `None` = none owed.
    let mut idr_due: Option<std::time::Instant> = Some(started);
    // `recovery.rs`'s gate, so a burst of asks costs what it costs a real host.
    let (mut kf_gate, mut last_idr) = (KeyframeGate::default(), None);
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
        // invalidate: an RFI costs the same IDR here. An answered ask goes through the same
        // cooldown a real host applies ([`owe_idr`]); asks while one is already owed do not
        // move it, as a rebuild in flight does not restart.
        for _ in 0..(keyframe.try_iter().count() + rfi.try_iter().count()) {
            asks += 1;
            if answer.answers(asks) {
                let now = std::time::Instant::now();
                idr_due = owe_idr(&mut kf_gate, now, recovery, last_idr, idr_due);
            }
        }

        let elapsed = started.elapsed();
        if let Some(shot) = content.frame_at(elapsed, fps, tick) {
            let now = std::time::Instant::now();
            let idr = idr_due.is_some_and(|t| now >= t);
            if idr {
                idr_due = None;
                last_idr = Some(now);
                // The `link health` line's `idr=` counter, so a run's repairs are in the log.
                counters.link.note_idr();
            }
            // Re-derived per frame: an adaptive-FEC move changes the picture's share of the
            // budget without the budget moving.
            let enc_kbps = encoder_kbps_for_budget(
                budget_kbps,
                audio_reserved_kbps,
                fec_target.load(Ordering::Relaxed),
                shard_payload,
            );
            let len = frame_bytes(
                enc_kbps,
                shard_payload,
                fps,
                content.fill_pct(),
                shot,
                idr,
                idr_pct,
            );
            let flags = if idr {
                u32::from(FLAG_PIC | FLAG_SOF)
            } else {
                u32::from(FLAG_PIC)
            };
            let msg = FrameMsg {
                data: test_frame(au_seq, len),
                meta: AuMeta {
                    capture_ns: now_ns(),
                    epoch: 0,
                    flags,
                    frame_index: au_seq,
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
                    driver: None,
                },
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
        keyframe_asks = asks,
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
                    let one = frame_bytes(enc, 1408, fps, 100, Shot::New, false, DEFAULT_IDR_PCT);
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

    /// Fill scales the frame, a keyframe is `idr_pct` of one, and a repeat is one shard
    /// whatever the budget — the three shapes the controller reads differently.
    #[test]
    fn fill_idr_and_repeat_each_size_their_own_frame() {
        let enc = encoder_kbps_for_budget(20_000, 512, 10, 1408);
        let at = |fill, shot, idr| frame_bytes(enc, 1408, 60, fill, shot, idr, DEFAULT_IDR_PCT);
        let full = at(100, Shot::New, false);
        assert_eq!(at(50, Shot::New, false), full / 2, "fill halves the frame");
        assert_eq!(
            at(100, Shot::New, true) as u64,
            full as u64 * u64::from(DEFAULT_IDR_PCT) / 100,
            "a keyframe is idr_pct of an ordinary frame"
        );
        assert_eq!(at(100, Shot::Repeat, false), 1408, "a repeat is one shard");
        assert_eq!(
            at(100, Shot::Repeat, true),
            1408,
            "even when an IDR is owed"
        );
        // A VBV-bounded encoder fits its keyframe near one frame's share.
        assert_eq!(
            frame_bytes(enc, 1408, 60, 100, Shot::New, true, 100),
            full,
            "100 % is a keyframe the size of the frame it replaces"
        );
    }

    /// A burst of asks costs one IDR, not one each: production gates them behind
    /// [`IDR_COOLDOWN_FULL`], and the ones that land inside it collapse into the IDR the
    /// cooldown ends on. Round 7's 111 asks became 111 bursts without this.
    #[test]
    fn a_burst_of_asks_costs_one_idr_per_cooldown() {
        let mut gate = KeyframeGate::default();
        let t0 = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        let (mut due, mut last_idr) = (None, None);
        let mut sent = Vec::new();
        // A client asking at 10 Hz for 3 s, the way `Hold` does behind a lost frame.
        for step in 0..30u64 {
            let now = t0 + ms(step * 100);
            due = owe_idr(&mut gate, now, std::time::Duration::ZERO, last_idr, due);
            if due.is_some_and(|t| now >= t) {
                due = None;
                last_idr = Some(now);
                sent.push(step * 100);
            }
        }
        assert_eq!(
            sent,
            [0, 800, 2300],
            "one IDR, then one per cooldown — 750 ms, then doubled while the asks keep coming"
        );
        // A lone ask well past the cooldown is answered at once.
        let quiet = t0 + ms(10_000);
        due = owe_idr(&mut gate, quiet, std::time::Duration::ZERO, last_idr, None);
        assert_eq!(due, Some(quiet), "a new episode is not held back");
    }

    /// A host that answers with an intra-refresh wave re-anchors only every n-th ask; the
    /// client holding its picture through the others is what C6 is about.
    #[test]
    fn a_wave_answers_only_every_nth_ask() {
        let idr = KeyframeAnswer::parse("idr").expect("idr parses");
        assert!((1..=8).all(|n| idr.answers(n)), "every ask gets one");
        let wave = KeyframeAnswer::parse("wave:4").expect("wave parses");
        assert_eq!(
            (1..=8).filter(|&n| wave.answers(n)).collect::<Vec<_>>(),
            [4, 8],
            "every fourth ask, and no other"
        );
        assert_eq!(KeyframeAnswer::parse("wave:0"), None);
        assert_eq!(KeyframeAnswer::parse("wave"), None);
        assert_eq!(KeyframeAnswer::parse("nothing"), None);
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

        // A minute of motion, then 5 new frames a second among repeats.
        let still = Content::parse("motion-then-still:5", 100).expect("motion-then-still parses");
        assert!((0..60)
            .all(|t| still.frame_at(std::time::Duration::from_secs(1), 60, t) == Some(Shot::New)));
        let late = std::time::Duration::from_secs(61);
        let new = (0..60)
            .filter(|&t| still.frame_at(late, 60, t) == Some(Shot::New))
            .count();
        assert_eq!(new, 5, "five new frames a second once still");
        assert_eq!(still.frame_at(late, 60, 1), Some(Shot::Repeat));

        assert_eq!(Content::parse("frame-driven:0", 100), None);
        assert_eq!(Content::parse("nonsense", 100), None);
    }
}
