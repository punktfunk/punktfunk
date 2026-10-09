//! The pad-audio pipeline shared by both OSes: de-interleave the quad capture, gate silence,
//! Opus-encode each lane at its wire cadence, send `0xD1` datagrams. Capture is the caller's:
//! [`pad_audio_thread`] opens it lazily and reopens with backoff.

use super::*;

/// Bit N of `kinds` is wire kind N — same packing as the arrival's audio-caps.
pub(in crate::native) const KIND_BIT_HAPTICS: u8 =
    1 << punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS;
pub(in crate::native) const KIND_BIT_SPEAKER: u8 =
    1 << punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER;

/// 5 ms: haptics are felt latency. Speaker is 10 ms (coding efficiency). Wire cadences.
const HAPTICS_FRAME_MS: u32 = 5;
const SPEAKER_FRAME_MS: u32 = 10;
const HAPTICS_FRAME_SAMPLES: usize =
    crate::audio::SAMPLE_RATE as usize * HAPTICS_FRAME_MS as usize / 1000;
const SPEAKER_FRAME_SAMPLES: usize =
    crate::audio::SAMPLE_RATE as usize * SPEAKER_FRAME_MS as usize / 1000;
/// Quad: FL FR = speaker, BL BR = voice coils. Own copy: `pad_capture::PAD_CHANNELS` is Windows-gated.
const CAP_CHANNELS: usize = 4;

/// ≈ −60 dBFS. Opens on the first frame at this peak — haptics are felt latency.
const GATE_OPEN_PEAK: f32 = 1e-3;
/// 250 ms: long enough for a decaying haptic tail (and the decoder's), short enough idle is free.
const GATE_HANGOVER_MS: u32 = 250;

/// Band-limited rumble; 64 kbps CBR stays under one MTU.
const HAPTICS_BITRATE: i32 = 64_000;
/// Programme audio: 64 kbps CELT-only at 10 ms is artifacty; 96 kbps CBR is still ~120 B/frame.
const SPEAKER_BITRATE: i32 = 96_000;

/// Opens on the first signal frame; closes after [`GATE_HANGOVER_MS`] of quiet. Idle pad → no datagrams.
struct SilenceGate {
    hangover_frames: u32,
    quiet: u32,
    /// Starts closed so a pad that never renders never sends.
    open: bool,
}

impl SilenceGate {
    fn new(frame_ms: u32) -> SilenceGate {
        SilenceGate {
            hangover_frames: (GATE_HANGOVER_MS / frame_ms).max(1),
            quiet: 0,
            open: false,
        }
    }

    /// Signal opens on this frame. The hangover-completing quiet frame is suppressed.
    fn feed(&mut self, frame: &[f32]) -> bool {
        if frame.iter().any(|s| s.abs() >= GATE_OPEN_PEAK) {
            self.open = true;
            self.quiet = 0;
        } else if self.open {
            self.quiet += 1;
            if self.quiet >= self.hangover_frames {
                self.open = false;
                self.quiet = 0;
            }
        }
        self.open
    }
}

/// Seq is frozen while gated (client tells silence from loss by continuity) and kept across
/// capture reopens (gap, not restart).
struct LaneCtl {
    gate: SilenceGate,
    seq: u32,
}

impl LaneCtl {
    fn new(frame_ms: u32) -> LaneCtl {
        LaneCtl {
            gate: SilenceGate::new(frame_ms),
            seq: 0,
        }
    }

    /// `None` = gated: do not send, do not advance. Encode failure after this leaves a one-frame seq gap.
    fn admit(&mut self, frame: &[f32]) -> Option<u32> {
        if !self.gate.feed(frame) {
            return None;
        }
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Some(seq)
    }
}

/// FL FR → speaker, BL BR → haptics. A ragged tail (not a multiple of 4) is dropped, never smeared.
fn split_quad(block: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let mut front = Vec::with_capacity(block.len() / 2);
    let mut back = Vec::with_capacity(block.len() / 2);
    for s in block.chunks_exact(CAP_CHANNELS) {
        front.extend_from_slice(&s[..2]);
        back.extend_from_slice(&s[2..4]);
    }
    (front, back)
}

/// Disabled kinds are not split out, so they never reach an encoder.
struct PadFramer {
    kinds: u8,
    acc: Vec<f32>,
    front: Vec<f32>,
}

impl PadFramer {
    fn new(kinds: u8) -> PadFramer {
        PadFramer {
            kinds,
            acc: Vec::with_capacity(HAPTICS_FRAME_SAMPLES * CAP_CHANNELS * 4),
            front: Vec::new(),
        }
    }

    /// Haptics emit first — felt latency.
    fn feed(&mut self, chunk: &[f32], mut emit: impl FnMut(u8, &[f32])) {
        self.acc.extend_from_slice(chunk);
        let block_len = HAPTICS_FRAME_SAMPLES * CAP_CHANNELS;
        while self.acc.len() >= block_len {
            let block: Vec<f32> = self.acc.drain(..block_len).collect();
            let (front, back) = split_quad(&block);
            if self.kinds & KIND_BIT_HAPTICS != 0 {
                emit(punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS, &back);
            }
            if self.kinds & KIND_BIT_SPEAKER != 0 {
                self.front.extend_from_slice(&front);
                let frame_len = SPEAKER_FRAME_SAMPLES * 2;
                while self.front.len() >= frame_len {
                    let frame: Vec<f32> = self.front.drain(..frame_len).collect();
                    emit(punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER, &frame);
                }
            }
        }
    }

    /// Drop partials across a capture gap. Seq/gate live on [`LaneCtl`], which survives reopens.
    fn clear(&mut self) {
        self.acc.clear();
        self.front.clear();
    }
}

/// `encode_errs` is a power-of-two throttle (~200 fails/s unthrottled).
struct Lane {
    kind: u8,
    ctl: LaneCtl,
    enc: opus::Encoder,
    encode_errs: u64,
}

/// Stereo 48 kHz hard-CBR. Haptics: LowDelay (CELT-only, 2.5 ms lookahead) at 64 kbps.
/// Speaker: `Application::Audio` at 96 kbps — extra ~4 ms is inaudible; CELT-only artifacts were not.
fn build_lanes(kinds: u8) -> Result<Vec<Lane>, opus::Error> {
    let mut lanes = Vec::new();
    for (bit, kind, frame_ms, app, bitrate) in [
        (
            KIND_BIT_HAPTICS,
            punktfunk_core::quic::PAD_AUDIO_KIND_HAPTICS,
            HAPTICS_FRAME_MS,
            opus::Application::LowDelay,
            HAPTICS_BITRATE,
        ),
        (
            KIND_BIT_SPEAKER,
            punktfunk_core::quic::PAD_AUDIO_KIND_SPEAKER,
            SPEAKER_FRAME_MS,
            opus::Application::Audio,
            SPEAKER_BITRATE,
        ),
    ] {
        if kinds & bit == 0 {
            continue;
        }
        let mut enc = opus::Encoder::new(crate::audio::SAMPLE_RATE, opus::Channels::Stereo, app)?;
        enc.set_bitrate(opus::Bitrate::Bits(bitrate)).ok();
        enc.set_vbr(false).ok();
        lanes.push(Lane {
            kind,
            ctl: LaneCtl::new(frame_ms),
            enc,
            encode_errs: 0,
        });
    }
    Ok(lanes)
}

/// Capture death reopens after [`crate::audio::REOPEN_BACKOFF`] (encoders + seq kept).
/// ConnectionLost or a gone datagram path ends the thread; one TooLarge costs that frame only.
pub(super) fn pad_audio_thread<C: crate::audio::AudioCapturer>(
    conn: super::link::SessionLink,
    pad: u8,
    kinds: u8,
    open: impl Fn() -> anyhow::Result<C>,
    stop: Arc<AtomicBool>,
) {
    // Same boost as session send: live pad audio is a ≤10 ms cadence.
    pf_frame::thread_qos::boost_thread_priority(false);
    let mut lanes = match build_lanes(kinds) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(pad, error = %e, "pad-audio opus encoder init failed — pad continues without audio");
            return;
        }
    };
    if lanes.is_empty() {
        return; // spawn() refuses kinds == 0
    }
    let mut framer = PadFramer::new(kinds);
    // One Opus frame; 96 kbps CBR at ≤10 ms is ~120 bytes. 1500 is session-plane slack.
    let mut opus_buf = vec![0u8; 1500];
    // Capture death reopens instead of muting the pad for the session. First open rides this too.
    let mut capturer: Option<C> = None;
    let mut last_failed: Option<std::time::Instant> = None;
    let mut oversized_drops: u64 = 0;
    // Consecutive open failures. The third one warns: a first open can lose a race with the
    // endpoint's own re-activate, a streak means no controller audio for the session.
    let mut open_failures: u32 = 0;
    tracing::info!(
        pad,
        haptics = kinds & KIND_BIT_HAPTICS != 0,
        speaker = kinds & KIND_BIT_SPEAKER != 0,
        "pad audio streaming (0xD1, Opus 48 kHz, silence-gated)"
    );
    'session: while !stop.load(Ordering::SeqCst) {
        if capturer.is_none() {
            if last_failed.is_some_and(|t| t.elapsed() < crate::audio::REOPEN_BACKOFF) {
                std::thread::sleep(std::time::Duration::from_millis(200));
                continue;
            }
            match open() {
                Ok(c) => {
                    if last_failed.take().is_some() {
                        tracing::info!(pad, "pad-audio capture reopened");
                    }
                    open_failures = 0;
                    capturer = Some(c);
                    framer.clear();
                }
                Err(e) => {
                    open_failures += 1;
                    if open_failures == 3 {
                        tracing::warn!(
                            pad,
                            error = %format!("{e:#}"),
                            "pad-audio capture did not open — no controller audio until it does, \
                             retrying every 2 s"
                        );
                    } else {
                        tracing::debug!(pad, error = %format!("{e:#}"), "pad-audio open failed — will retry");
                    }
                    last_failed = Some(std::time::Instant::now());
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    continue;
                }
            }
        }
        // Empty chunk = quiet endpoint (idle timeout), not death. Only Err drops the capturer.
        let chunk = match capturer.as_mut().unwrap().next_chunk() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(pad, error = %format!("{e:#}"), "pad-audio capture lost — reopening");
                capturer = None;
                last_failed = Some(std::time::Instant::now());
                continue;
            }
        };
        let mut end_plane = false;
        framer.feed(&chunk, |kind, frame| {
            if end_plane {
                return;
            }
            let Some(lane) = lanes.iter_mut().find(|l| l.kind == kind) else {
                return; // framer emits only enabled kinds
            };
            let Some(seq) = lane.ctl.admit(frame) else {
                return;
            };
            let pts_ns = now_ns();
            match lane.enc.encode_float(frame, &mut opus_buf) {
                Ok(n) => {
                    let d = punktfunk_core::quic::encode_pad_audio_datagram(
                        pad,
                        kind,
                        seq,
                        pts_ns,
                        &opus_buf[..n],
                    );
                    match conn.send_datagram(d) {
                        super::link::DatagramSend::Sent => {}
                        // One frame, not the plane. seq already advanced; client conceals the gap.
                        super::link::DatagramSend::TooLarge => {
                            oversized_drops += 1;
                            if oversized_drops.is_power_of_two() {
                                tracing::warn!(
                                    pad,
                                    kind,
                                    count = oversized_drops,
                                    opus_bytes = n,
                                    "pad-audio datagram rejected as too large — dropping the \
                                     frame and continuing"
                                );
                            }
                        }
                        // Datagrams are gone for this connection. Next frame will not land.
                        super::link::DatagramSend::Unavailable => {
                            tracing::warn!(
                                pad,
                                "the datagram path is unavailable — ending this pad's audio"
                            );
                            end_plane = true;
                        }
                    }
                }
                Err(e) => {
                    lane.encode_errs += 1;
                    if lane.encode_errs.is_power_of_two() {
                        tracing::warn!(
                            pad,
                            kind,
                            error = %e,
                            count = lane.encode_errs,
                            "pad-audio opus encode failed — dropping frame"
                        );
                    }
                }
            }
        });
        if end_plane {
            break 'session;
        }
    }
    // Dropping the capturer stops its WASAPI thread. No cross-session park: pad capture is per-pad.
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};

    fn frame(level: f32, n: usize) -> Vec<f32> {
        vec![level; n * 2]
    }

    #[test]
    fn gate_opens_immediately_and_closes_after_hangover() {
        let mut g = SilenceGate::new(HAPTICS_FRAME_MS);
        // 250 ms / 5 ms.
        assert_eq!(g.hangover_frames, 50);
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // Threshold opens on this frame (haptics are felt latency).
        assert!(g.feed(&frame(GATE_OPEN_PEAK, HAPTICS_FRAME_SAMPLES)));
        // 49 quiet frames ride the hangover; the 50th completes 250 ms and is suppressed.
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        // Sub-threshold does not reopen; negative peaks count as signal.
        assert!(!g.feed(&frame(9e-4, HAPTICS_FRAME_SAMPLES)));
        assert!(g.feed(&frame(-0.5, HAPTICS_FRAME_SAMPLES)));
        // A loud frame mid-hangover rearms the full 250 ms.
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(g.feed(&frame(0.02, HAPTICS_FRAME_SAMPLES)));
        for _ in 0..49 {
            assert!(g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, HAPTICS_FRAME_SAMPLES)));
    }

    #[test]
    fn gate_hangover_scales_with_frame_ms() {
        let mut g = SilenceGate::new(SPEAKER_FRAME_MS);
        assert_eq!(g.hangover_frames, 25); // 250 ms / 10 ms
        assert!(g.feed(&frame(0.1, SPEAKER_FRAME_SAMPLES)));
        for _ in 0..24 {
            assert!(g.feed(&frame(0.0, SPEAKER_FRAME_SAMPLES)));
        }
        assert!(!g.feed(&frame(0.0, SPEAKER_FRAME_SAMPLES)));
    }

    #[test]
    fn seq_freezes_while_gated_and_survives_reopen() {
        let mut lane = LaneCtl::new(HAPTICS_FRAME_MS);
        assert_eq!(lane.admit(&frame(0.5, HAPTICS_FRAME_SAMPLES)), Some(0));
        assert_eq!(lane.admit(&frame(0.5, HAPTICS_FRAME_SAMPLES)), Some(1));
        // Hangover is still sent (seq advances); then the gate closes and seq freezes.
        for i in 0..49u32 {
            assert_eq!(lane.admit(&frame(0.0, HAPTICS_FRAME_SAMPLES)), Some(2 + i));
        }
        for _ in 0..500 {
            assert_eq!(lane.admit(&frame(0.0, HAPTICS_FRAME_SAMPLES)), None);
        }
        // Reopen resets only the framer — LaneCtl is untouched, so the next audible frame continues.
        assert_eq!(lane.admit(&frame(0.9, HAPTICS_FRAME_SAMPLES)), Some(51));
    }

    #[test]
    fn splitter_exact_pairs() {
        let quad = [0.0, 1.0, 2.0, 3.0, 10.0, 11.0, 12.0, 13.0];
        let (front, back) = split_quad(&quad);
        assert_eq!(front, [0.0, 1.0, 10.0, 11.0]);
        assert_eq!(back, [2.0, 3.0, 12.0, 13.0]);
        // A ragged tail (never produced by the capturer) is dropped, not smeared.
        let (front, back) = split_quad(&quad[..7]);
        assert_eq!((front.len(), back.len()), (2, 2));
    }

    #[test]
    fn framer_cuts_the_wire_cadence() {
        let mut f = PadFramer::new(KIND_BIT_HAPTICS | KIND_BIT_SPEAKER);
        let mut got: Vec<(u8, usize, f32)> = Vec::new();
        // 10 ms of capture: two 5 ms haptics frames from the back pair, then one 10 ms speaker frame.
        let mut quad = Vec::new();
        for _ in 0..2 * HAPTICS_FRAME_SAMPLES {
            quad.extend_from_slice(&[0.25, 0.25, -0.5, -0.5]);
        }
        for chunk in quad.chunks(101) {
            f.feed(chunk, |kind, frame| got.push((kind, frame.len(), frame[0])));
        }
        assert_eq!(
            got,
            vec![
                (PAD_AUDIO_KIND_HAPTICS, 2 * HAPTICS_FRAME_SAMPLES, -0.5),
                (PAD_AUDIO_KIND_HAPTICS, 2 * HAPTICS_FRAME_SAMPLES, -0.5),
                (PAD_AUDIO_KIND_SPEAKER, 2 * SPEAKER_FRAME_SAMPLES, 0.25),
            ]
        );
    }

    #[test]
    fn framer_masks_disabled_kinds() {
        // 20 ms of all-ones: 4 potential haptics frames, 2 potential speaker frames.
        let quad = vec![1.0f32; 4 * HAPTICS_FRAME_SAMPLES * CAP_CHANNELS];
        let mut kinds_seen = Vec::new();
        // Haptics-only: the front pair is never split out.
        let mut f = PadFramer::new(KIND_BIT_HAPTICS);
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert_eq!(kinds_seen, vec![PAD_AUDIO_KIND_HAPTICS; 4]);
        let mut f = PadFramer::new(KIND_BIT_SPEAKER);
        kinds_seen.clear();
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert_eq!(kinds_seen, vec![PAD_AUDIO_KIND_SPEAKER; 2]);
        // kinds = 0 is never spawned, but the framer must still be total.
        let mut f = PadFramer::new(0);
        kinds_seen.clear();
        f.feed(&quad, |kind, _| kinds_seen.push(kind));
        assert!(kinds_seen.is_empty());
    }

    #[test]
    fn framer_clear_drops_partials_only() {
        let mut f = PadFramer::new(KIND_BIT_HAPTICS | KIND_BIT_SPEAKER);
        let mut emitted = 0;
        // 100 samples: no frame boundary yet.
        f.feed(&vec![0.1; 100 * CAP_CHANNELS], |_, _| emitted += 1);
        assert_eq!(emitted, 0);
        f.clear();
        // After the gap: one haptics frame from 240 fresh samples — the 100 stale would skew later boundaries.
        f.feed(
            &vec![0.2; HAPTICS_FRAME_SAMPLES * CAP_CHANNELS],
            |kind, frame| {
                emitted += 1;
                assert_eq!(
                    (kind, frame.len()),
                    (PAD_AUDIO_KIND_HAPTICS, 2 * HAPTICS_FRAME_SAMPLES)
                );
            },
        );
        assert_eq!(emitted, 1);
    }
}
