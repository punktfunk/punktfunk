//! DualSense pad audio (`0xD1`), the platform-free half: the per-kind Opus decode stage with
//! seq-gap PLC, the mixer that interleaves the haptics and speaker lanes into the pad's
//! four-channel frame, and the liveness clock that hands the coils between haptics and wire
//! rumble. The sink, pad latching and tallies stay in each client.

use crate::audio::{AudioGapTracker, SAMPLE_RATE_HZ};
use crate::quic::{PadAudioFrame, PAD_AUDIO_KIND_HAPTICS, PAD_AUDIO_KIND_SPEAKER};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Speaker FL/FR on 0/1, voice coils on 2/3: the DualSense USB audio function's layout.
pub const PAD_CHANNELS: usize = 4;

/// A kind silent this long stops holding the other back: the host gate closed its lane.
/// 25 ms = 2.5 speaker frames.
pub const LANE_LIVE: Duration = Duration::from_millis(25);

/// Interleave the two stereo lanes into one four-channel stream.
///
/// The kinds arrive on different cadences (haptics 5 ms, speaker 10 ms), so each has its own
/// write cursor and [`pop`](Self::pop) emits what every live kind has written. A kind that has
/// not pushed for [`LANE_LIVE`] reads silence, so a haptics-only session plays the coils with a
/// silent speaker pair instead of stalling. `S` is the sink's sample type.
pub struct QuadMixer<S> {
    /// Interleaved four-channel samples; the front is the next frame out. Always
    /// `ready_frames() * PAD_CHANNELS` long.
    ring: VecDeque<S>,
    /// Per-kind write cursor in frames from the ring front, indexed by wire `kind`.
    written: [usize; 2],
    /// Per-kind last push; live within [`LANE_LIVE`].
    pushed: [Option<Instant>; 2],
    /// Frames dropped to the ceiling: a stalled sink.
    dropped: u64,
    max_frames: usize,
}

impl<S: Copy + Default> QuadMixer<S> {
    /// `max_frames` caps decoder backlog when the sink stalls; overflow drops the oldest.
    pub fn new(max_frames: usize) -> QuadMixer<S> {
        QuadMixer {
            ring: VecDeque::new(),
            written: [0; 2],
            pushed: [None; 2],
            dropped: 0,
            max_frames,
        }
    }

    /// Write one decoded stereo chunk for `kind` at that kind's cursor. Both cursors shift
    /// together on overflow, so the kinds never skew. An unknown kind is dropped: folding it into
    /// the coil pair would play an unknown stream on the actuators.
    pub fn push(&mut self, kind: u8, stereo: &[S], now: Instant) {
        let (k, off) = match kind {
            PAD_AUDIO_KIND_HAPTICS => (0usize, 2usize),
            PAD_AUDIO_KIND_SPEAKER => (1, 0),
            _ => return,
        };
        self.pushed[k] = Some(now);
        let frames = stereo.len() / 2;
        let base = self.written[k];
        let need = (base + frames) * PAD_CHANNELS;
        if self.ring.len() < need {
            self.ring.resize(need, S::default());
        }
        for (i, fr) in stereo.chunks_exact(2).enumerate() {
            let at = (base + i) * PAD_CHANNELS + off;
            self.ring[at] = fr[0];
            self.ring[at + 1] = fr[1];
        }
        self.written[k] = base + frames;
        let over = self.ready_frames().saturating_sub(self.max_frames);
        if over > 0 {
            self.dropped += over as u64;
            self.drop_front(over);
        }
    }

    /// Frames ready to output: the further-ahead kind's cursor.
    pub fn ready_frames(&self) -> usize {
        self.written[0].max(self.written[1])
    }

    /// Frames dropped to the ceiling since construction.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped
    }

    /// Append the frames every live kind has written to `out`, or every ready frame once none is
    /// live; returns the frame count. Popping past a live kind's cursor zeros its pair for that
    /// span. Both cursors move back; a lagging kind resumes at the new front.
    pub fn pop(&mut self, out: &mut Vec<S>, now: Instant) -> usize {
        let frames = (0..2)
            .filter(|&k| self.pushed[k].is_some_and(|t| now.duration_since(t) < LANE_LIVE))
            .map(|k| self.written[k])
            .min()
            .unwrap_or_else(|| self.ready_frames());
        let n = frames * PAD_CHANNELS;
        out.extend(self.ring.drain(..n.min(self.ring.len())));
        for w in &mut self.written {
            *w = w.saturating_sub(frames);
        }
        frames
    }

    /// Throw the ready frames away: no sink to render them on right now.
    pub fn discard(&mut self) {
        let f = self.ready_frames();
        self.drop_front(f);
    }

    fn drop_front(&mut self, frames: usize) {
        let n = (frames * PAD_CHANNELS).min(self.ring.len());
        self.ring.drain(..n);
        let f = n / PAD_CHANNELS;
        for w in &mut self.written {
            *w = w.saturating_sub(f);
        }
    }
}

/// Concealment frames to synthesise before decoding `seq`, capped at 50 ms of `frame_samples`
/// (per channel at 48 kHz; speaker frames are 10 ms, haptics 5 ms). 0 until a first decode, but
/// the tracker is always fed so a gap before it cannot replay later.
pub fn plc_frames(gaps: &mut AudioGapTracker, seq: u32, frame_samples: usize) -> u32 {
    if frame_samples == 0 {
        gaps.missing_before(seq);
        return 0;
    }
    gaps.set_frame_us((frame_samples as u64 * 1_000_000 / SAMPLE_RATE_HZ as u64) as u32);
    gaps.missing_before(seq)
}

/// Largest Opus frame per channel: 120 ms at 48 kHz. [`PadDecode::decode_frame`]'s `pcm` holds
/// this many stereo frames.
pub const MAX_FRAME_SAMPLES: usize = 5_760;

/// A sample type libopus decodes into: `i16` for a usbfs sink, `f32` for PipeWire and WASAPI.
#[cfg(feature = "quic")]
pub trait PadSample: Copy + Default {
    fn decode(dec: &mut opus::Decoder, opus: &[u8], pcm: &mut [Self]) -> opus::Result<usize>;
}

#[cfg(feature = "quic")]
impl PadSample for i16 {
    fn decode(dec: &mut opus::Decoder, opus: &[u8], pcm: &mut [i16]) -> opus::Result<usize> {
        dec.decode(opus, pcm, false)
    }
}

#[cfg(feature = "quic")]
impl PadSample for f32 {
    fn decode(dec: &mut opus::Decoder, opus: &[u8], pcm: &mut [f32]) -> opus::Result<usize> {
        dec.decode_float(opus, pcm, false)
    }
}

/// One kind's stereo 48 kHz decoder, its seq-gap tracker, and the last decoded frame size, the
/// unit PLC synthesises in.
#[cfg(feature = "quic")]
struct KindStream {
    dec: opus::Decoder,
    gaps: AudioGapTracker,
    frame_samples: usize,
}

/// The decode stage of one rendered pad: both kinds' Opus streams into a [`QuadMixer`]. The
/// player's settings gate each kind; a kind's decoder is created on its first frame.
#[cfg(feature = "quic")]
pub struct PadDecode {
    haptics: bool,
    speaker: bool,
    kinds: [Option<KindStream>; 2],
}

#[cfg(feature = "quic")]
impl PadDecode {
    pub fn new(haptics: bool, speaker: bool) -> PadDecode {
        PadDecode {
            haptics,
            speaker,
            kinds: [None, None],
        }
    }

    /// Whether the settings render `kind`. The host sends only the kinds this client declared;
    /// the re-check keeps a stale host from forcing one.
    pub fn wants(&self, kind: u8) -> bool {
        match kind {
            PAD_AUDIO_KIND_HAPTICS => self.haptics,
            PAD_AUDIO_KIND_SPEAKER => self.speaker,
            _ => false,
        }
    }

    /// Conceal the seq gap before `frame`, then decode it, all into `mixer`. A frozen seq (host
    /// gate closed) sends nothing and conceals nothing. Returns the samples per channel `frame`
    /// decoded to, at the front of `pcm`; `None` for an unwanted kind, an empty (DTX) payload or
    /// a decode error. `pcm` holds [`MAX_FRAME_SAMPLES`] stereo frames.
    pub fn decode_frame<S: PadSample>(
        &mut self,
        frame: &PadAudioFrame,
        pcm: &mut [S],
        mixer: &mut QuadMixer<S>,
    ) -> Option<usize> {
        if !self.wants(frame.kind) {
            return None;
        }
        let st = match &mut self.kinds[usize::from(frame.kind)] {
            Some(st) => st,
            slot @ None => match opus::Decoder::new(SAMPLE_RATE_HZ, opus::Channels::Stereo) {
                Ok(dec) => slot.insert(KindStream {
                    dec,
                    gaps: AudioGapTracker::new(),
                    frame_samples: 0,
                }),
                Err(e) => {
                    tracing::warn!(error = %e, kind = frame.kind, "pad-audio opus decoder");
                    return None;
                }
            },
        };
        let plc = st.frame_samples * 2;
        for _ in 0..plc_frames(&mut st.gaps, frame.seq, st.frame_samples) {
            match S::decode(&mut st.dec, &[], &mut pcm[..plc]) {
                Ok(n) => mixer.push(frame.kind, &pcm[..n * 2], Instant::now()),
                Err(_) => break,
            }
        }
        if frame.opus.is_empty() {
            return None;
        }
        match S::decode(&mut st.dec, &frame.opus, pcm) {
            Ok(n) => {
                st.frame_samples = n;
                mixer.push(frame.kind, &pcm[..n * 2], Instant::now());
                Some(n)
            }
            Err(e) => {
                tracing::debug!(error = %e, kind = frame.kind, "pad-audio opus decode");
                None
            }
        }
    }
}

/// Whether `frame` hands the coils to haptics: a haptics payload with a sink open to play it.
/// An empty keep-alive is no evidence, so it never takes the coils from rumble.
pub fn is_haptics_evidence(frame: &PadAudioFrame, sink_open: bool) -> bool {
    frame.kind == PAD_AUDIO_KIND_HAPTICS && !frame.opus.is_empty() && sink_open
}

/// The host gates haptics at −60 dBFS with a 250 ms hangover, so a title that only rumbles sends
/// no frames at all. Twice the hangover covers wire jitter.
pub const HAPTICS_IDLE_MS: u64 = 500;

/// Per-wire-pad stamp of the last haptics frame, the evidence that the game drives the coils.
/// Haptics own the coils only while frames arrive, so a rumble-only title keeps its rumble.
/// Written by the render thread, read by the rumble path.
pub struct HapticsLiveness([AtomicU64; 16]);

impl HapticsLiveness {
    pub const fn new() -> HapticsLiveness {
        HapticsLiveness([const { AtomicU64::new(0) }; 16])
    }

    /// Stamp a haptics frame for `pad`. Concealment must not: filling a gap is no evidence.
    pub fn note(&self, pad: u8) {
        self.0[usize::from(pad & 0x0f)].store(seen_clock(), Ordering::Relaxed);
    }

    /// Slot teardown: wire indices are reused, and a stale stamp would take the next pad's rumble.
    pub fn clear(&self, pad: u8) {
        self.0[usize::from(pad & 0x0f)].store(0, Ordering::Relaxed);
    }

    /// Whether `pad`'s haptics frames are arriving right now.
    pub fn live(&self, pad: u8) -> bool {
        live_at(
            self.0[usize::from(pad & 0x0f)].load(Ordering::Relaxed),
            seen_clock(),
        )
    }
}

impl Default for HapticsLiveness {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-clock ms, 1-based so 0 stays "never stamped": a frame in the first millisecond
/// must still read as live.
fn seen_clock() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1
}

/// Never-stamped is never live; a stamp ahead of `now` is live, not wrapped.
fn live_at(seen_ms: u64, now_ms: u64) -> bool {
    seen_ms != 0 && now_ms.saturating_sub(seen_ms) < HAPTICS_IDLE_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: usize = 2_880;

    #[test]
    fn speaker_lands_on_the_front_pair_and_haptics_on_the_coils() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[100, 200], t);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[300, 400], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![100, 200, 300, 400]);
    }

    /// `pad_speaker = "off"` must not stall the coils waiting for a kind that never arrives.
    #[test]
    fn a_missing_kind_plays_as_a_silent_pair() {
        let t = Instant::now();
        let mut m = QuadMixer::<f32>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[0.5, -0.5, 0.25, -0.25], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 2);
        assert_eq!(out, vec![0.0, 0.0, 0.5, -0.5, 0.0, 0.0, 0.25, -0.25]);
        let mut m = QuadMixer::<f32>::new(CAP);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[0.5, -0.5], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![0.5, -0.5, 0.0, 0.0]);
    }

    /// With both live, a pop takes only what both have written; a lane gone quiet stops holding
    /// the other back.
    #[test]
    fn interleaving_survives_uneven_cadences() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 1, 2, 2], t);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[9, 9], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![9, 9, 1, 1]);
        out.clear();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[5, 5], t);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[6, 6], t);
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![5, 5, 2, 2]);
        out.clear();
        assert_eq!(m.pop(&mut out, t), 0, "the speaker is live and behind");
        assert_eq!(m.pop(&mut out, t + LANE_LIVE), 1);
        assert_eq!(out, vec![0, 0, 6, 6]);
    }

    /// The loop pops after every datagram. With both lanes live that must play wall time once:
    /// popping the further-ahead kind played 20 ms per 10 ms and zeroed each pair in turn.
    #[test]
    fn both_live_lanes_play_in_real_time() {
        let t0 = Instant::now();
        let mut m = QuadMixer::<f32>::new(4_800);
        let (hap, spk) = (vec![0.5f32; 240 * 2], vec![1.0f32; 480 * 2]);
        let mut out = Vec::new();
        for ms in (0..1_000u64).step_by(5) {
            let now = t0 + Duration::from_millis(ms);
            m.push(PAD_AUDIO_KIND_HAPTICS, &hap, now);
            m.pop(&mut out, now);
            if ms % 10 == 0 {
                m.push(PAD_AUDIO_KIND_SPEAKER, &spk, now);
                m.pop(&mut out, now);
            }
        }
        let frames = out.len() / PAD_CHANNELS;
        assert!(
            (47_500..=48_000).contains(&frames),
            "{frames} frames for 1 s"
        );
        // Past the first speaker frame, every frame carries both pairs.
        assert!(out[480 * PAD_CHANNELS..]
            .chunks_exact(4)
            .all(|f| f[0] == 1.0 && f[2] == 0.5));
    }

    /// A stalled sink cannot grow the ring past the cap; the oldest frames drop and both cursors
    /// shift, so a late marker on the other kind still lands at the front.
    #[test]
    fn the_ceiling_drops_the_oldest_without_skewing_the_kinds() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &vec![1i16; (CAP + 500) * 2], t);
        assert_eq!(m.dropped_frames(), 500);
        assert_eq!(m.ready_frames(), CAP);
        m.push(PAD_AUDIO_KIND_SPEAKER, &[42, 43], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t + LANE_LIVE), CAP);
        assert_eq!(&out[..4], &[42, 43, 1, 1]);
    }

    #[test]
    fn discard_empties_without_disturbing_alignment() {
        let t = Instant::now();
        let mut m = QuadMixer::<i16>::new(CAP);
        m.push(PAD_AUDIO_KIND_HAPTICS, &[1, 2, 3, 4], t);
        m.discard();
        assert_eq!(m.ready_frames(), 0);
        let mut out = Vec::new();
        m.push(PAD_AUDIO_KIND_SPEAKER, &[8, 9], t);
        assert_eq!(m.pop(&mut out, t + LANE_LIVE), 1);
        assert_eq!(out, vec![8, 9, 0, 0]);
    }

    /// An unknown kind never reaches a channel pair, least of all the coils.
    #[test]
    fn an_unknown_kind_is_dropped_rather_than_rendered_into_the_coils() {
        let t = Instant::now();
        let mut m = QuadMixer::<f32>::new(CAP);
        m.push(2, &[0.9, 0.9], t);
        assert_eq!(m.ready_frames(), 0, "an unknown kind occupies no pair");
        m.push(PAD_AUDIO_KIND_SPEAKER, &[0.1, 0.2], t);
        let mut out = Vec::new();
        assert_eq!(m.pop(&mut out, t), 1);
        assert_eq!(out, vec![0.1, 0.2, 0.0, 0.0]);
    }

    /// 0 for first/in-order, the exact gap for a loss, 50 ms of the stream's own frames for a
    /// burst. A gap before the first decode is consumed, not replayed.
    #[test]
    fn plc_counts_gaps_in_the_streams_own_frames() {
        let mut g = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut g, 0, 480), 0);
        assert_eq!(plc_frames(&mut g, 1, 480), 0);
        assert_eq!(plc_frames(&mut g, 5, 480), 3);
        assert_eq!(plc_frames(&mut g, 5, 480), 0);
        assert_eq!(plc_frames(&mut g, 1000, 480), 5, "10 ms speaker frames");
        assert_eq!(plc_frames(&mut g, 2000, 240), 10, "5 ms haptics frames");
        let mut g = AudioGapTracker::new();
        assert_eq!(plc_frames(&mut g, 7, 0), 0);
        assert_eq!(plc_frames(&mut g, 12, 0), 0);
        assert_eq!(plc_frames(&mut g, 13, 480), 0);
    }

    fn frame(kind: u8, seq: u32, opus: &[u8]) -> PadAudioFrame {
        let wire = crate::quic::encode_pad_audio_datagram(0, kind, seq, 0, opus);
        crate::quic::decode_pad_audio_datagram(&wire).expect("a whole pad-audio datagram")
    }

    /// An off kind decodes nothing; a seq gap conceals in the stream's own frames before the
    /// frame that revealed it; DTX decodes nothing. Both sample types share one decoder.
    #[cfg(feature = "quic")]
    #[test]
    fn the_stage_gates_kinds_and_conceals_gaps_before_decoding() {
        let mut enc = opus::Encoder::new(48_000, opus::Channels::Stereo, opus::Application::Audio)
            .expect("opus encoder");
        let packet = enc
            .encode_vec_float(&[0.0f32; 480 * 2], 4_000)
            .expect("encode one 10 ms stereo frame");
        let mut stage = PadDecode::new(false, true);
        let mut pcm = vec![0f32; MAX_FRAME_SAMPLES * 2];
        let mut m = QuadMixer::<f32>::new(4_800);
        let hap = frame(PAD_AUDIO_KIND_HAPTICS, 0, &packet);
        assert_eq!(stage.decode_frame(&hap, &mut pcm, &mut m), None);
        assert_eq!(m.ready_frames(), 0, "haptics are off");
        let spk = |seq| frame(PAD_AUDIO_KIND_SPEAKER, seq, &packet);
        assert_eq!(stage.decode_frame(&spk(0), &mut pcm, &mut m), Some(480));
        assert_eq!(stage.decode_frame(&spk(3), &mut pcm, &mut m), Some(480));
        assert_eq!(m.ready_frames(), 480 * 4, "seq 1 and 2 concealed");
        let dtx = frame(PAD_AUDIO_KIND_SPEAKER, 4, &[]);
        assert_eq!(stage.decode_frame(&dtx, &mut pcm, &mut m), None);
        assert_eq!(m.ready_frames(), 480 * 4);
        let mut pcm16 = vec![0i16; MAX_FRAME_SAMPLES * 2];
        let mut m16 = QuadMixer::<i16>::new(4_800);
        assert_eq!(stage.decode_frame(&spk(5), &mut pcm16, &mut m16), Some(480));
    }

    #[test]
    fn only_a_rendered_haptics_payload_is_evidence() {
        let hap = frame(PAD_AUDIO_KIND_HAPTICS, 0, &[1]);
        assert!(is_haptics_evidence(&hap, true));
        assert!(!is_haptics_evidence(&hap, false), "no sink renders it");
        let keep_alive = frame(PAD_AUDIO_KIND_HAPTICS, 0, &[]);
        assert!(!is_haptics_evidence(&keep_alive, true));
        let speaker = frame(PAD_AUDIO_KIND_SPEAKER, 0, &[1]);
        assert!(!is_haptics_evidence(&speaker, true));
    }

    #[test]
    fn haptics_own_the_coils_only_while_frames_arrive() {
        assert!(!live_at(0, 10_000), "never stamped");
        assert!(live_at(10_000, 10_000));
        assert!(live_at(10_000, 10_000 + HAPTICS_IDLE_MS - 1));
        assert!(!live_at(10_000, 10_000 + HAPTICS_IDLE_MS));
        assert!(live_at(10_000, 9_000), "a stamp ahead of now is live");
        assert!(live_at(1, 1), "a frame in the first millisecond is live");
    }

    #[test]
    fn clearing_a_pad_hands_its_coils_back() {
        let l = HapticsLiveness::new();
        l.note(0x12);
        assert!(l.live(2), "the pad index wraps into the 16 wire slots");
        assert!(!l.live(3));
        l.clear(2);
        assert!(!l.live(2));
    }
}
