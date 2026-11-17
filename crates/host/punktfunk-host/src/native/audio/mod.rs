//! Native audio plane: desktop capture → Opus (`AUDIO_MAGIC` / `AUDIO_RED_MAGIC`) or lossless
//! PCM (`AUDIO_PCM_MAGIC`) at the negotiated channel count. Opus is 48 kHz, 5 ms, constrained
//! VBR at [`AudioTier`](punktfunk_core::audio::AudioTier). PCM covers both rate families
//! (44.1/48/88.2/96/176.4 kHz), 16/24-bit, and a negotiated frame duration — see
//! `design/hi-res-audio.md`. The third plane is AC-3 / E-AC-3 (`AUDIO_BITSTREAM_MAGIC`) for a
//! client that passes it through to a receiver.
//!
//! The plane is chosen once at handshake and never switches: the client's output device is
//! open at a fixed rate. [`native_encoder`] and [`audio_thread`] build on every target (libopus
//! is vendored); off Linux and Windows `pf_audio` has no capturer, so a dev build streams
//! video only.
//! Unlike GameStream this path has no FEC, so it uses constrained VBR rather than hard CBR —
//! see [`native_encoder`].

use super::*;

use crate::audio::{CaptureLease, CaptureRoute, Ready};
use punktfunk_core::audio::{bitstream, pcm};

mod frame;
use frame::FrameAssembler;

/// Wire clock: `pts_ns` is an anchor plus a running total of interleaved samples.
///
/// `frame_us` is a label. `pcm::samples_per_frame` floors, so a rung is a real duration only
/// when the rate divides it — every rung divides the 48 kHz family; none of the seven divides
/// 44 100 Hz. A "5 ms" frame at 44 100 Hz is 220 samples/ch = 4 988 662 ns. Stamping
/// `pts += frame_us * 1000` runs **2 268 ppm fast** (2.3 ms/s invented) for the session.
///
/// [`reanchor`](Self::reanchor) only moves FORWARD (must not un-send frames already on the
/// wire), so a fast clock is never pulled back. Counting samples cannot drift. A running
/// total also beats summing per-frame `frame_duration_ns` (~1 ns/frame remainder); this
/// accumulates zero.
struct PtsClock {
    base_ns: u64,
    samples: usize,
    rate_hz: u32,
    channels: u8,
    /// Fold factor for [`advance`](Self::advance): interleaved samples in one second.
    samples_per_sec: usize,
}

impl PtsClock {
    fn new(rate_hz: u32, channels: u8) -> PtsClock {
        PtsClock {
            base_ns: 0,
            samples: 0,
            rate_hz,
            channels,
            samples_per_sec: rate_hz as usize * channels as usize,
        }
    }

    /// Pts of the NEXT frame to leave, real or synthesized.
    fn pts_ns(&self) -> u64 {
        self.base_ns + pcm::frame_duration_ns(self.samples, self.rate_hz, self.channels)
    }

    fn advance(&mut self, samples: usize) {
        self.samples += samples;
        // Fold whole seconds into the base: `frame_duration_ns` takes a `usize` (32-bit on
        // some targets; 176 400 Hz 7.1 is 1.4 M samples/s). Exact — `rate_hz × channels`
        // interleaved samples are 1 000 000 000 ns at every rate this plane carries, so the
        // fold is invisible to `pts_ns`.
        if self.samples_per_sec > 0 && self.samples >= self.samples_per_sec {
            let secs = self.samples / self.samples_per_sec;
            self.base_ns += secs as u64 * 1_000_000_000;
            self.samples -= secs * self.samples_per_sec;
        }
    }

    /// Re-anchor on a capture arrival — **forward only**.
    ///
    /// Infill may have already advanced the wire clock; an arrival-derived anchor can land at
    /// or before the last sent pts. Moving back would re-issue a pts the client has played.
    /// Restarts the running total: the new base already covers the old base plus its samples.
    fn reanchor(&mut self, anchor_ns: u64) {
        if anchor_ns > self.pts_ns() {
            self.base_ns = anchor_ns;
            self.samples = 0;
        }
    }
}

/// Opus encoder for `channels` (2/6/8) at `tier`, constrained VBR.
///
/// GameStream uses hard CBR because its audio FEC needs fixed-size packets. This plane has no
/// FEC (`punktfunk_core::audio::AudioGapTracker` exists because a lost packet has nothing to
/// rebuild it from), so CBR would be a quality tax. Constrained VBR keeps the same average
/// bitrate and a bounded packet size.
fn native_encoder(
    channels: u8,
    tier: punktfunk_core::audio::AudioTier,
    layout: punktfunk_core::audio::AudioLayout,
) -> Result<crate::audio::OpusEnc, opus::Error> {
    let l = punktfunk_core::audio::layout_for(channels, layout);
    crate::audio::OpusEnc::new(
        l,
        l.bitrate_for(tier),
        crate::audio::RateControl::ConstrainedVbr,
    )
}

/// Ceiling on a single pacing sleep. The capture channel is finite and `next_chunk` has to be
/// serviced; sleeping past a couple of frames would trade a burst on the wire for a drop at the
/// capturer, which is strictly worse (a drop is a click AND a permanent shift).
const PACE_MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(10);

/// The longest capture wait with nothing owed: ten wakes a second, and session teardown waits
/// at most this long for the audio thread.
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(100);
/// How far behind schedule the pacer may fall before it stops trying to catch up and simply
/// re-anchors. Chasing an old schedule after a stall would send a burst — the exact thing
/// pacing exists to prevent — so past this point the debt is forgiven, not repaid.
const PACE_REANCHOR: std::time::Duration = std::time::Duration::from_millis(100);
/// How long a stretch of capture with no hole in it times the capture clock. Arrival jitter at
/// its two ends moves one estimate by jitter / window, and the next window takes it back.
const RATE_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);
/// How far a slot may stray from one nominal frame to follow the capture clock. Real clocks sit
/// within ±100 ppm; past this the rate is wrong, and the hole and bonus rules carry it.
const RATE_FOLLOW_PPM: u128 = 500;

/// What the wire does in the slot [`Pacer::release`] was asked about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    /// Nothing is owed yet, or the hole is too young (or too old) to cover.
    Wait,
    /// A frame of real audio.
    Frame,
    /// Silence over a capture hole; `first` is the hole's first frame, which fades.
    Silence { first: bool },
}

#[derive(Debug, PartialEq, Eq)]
struct Release {
    slot: Slot,
    /// How far past its slot this frame leaves, measured before a re-anchor forgives it.
    late: std::time::Duration,
    /// The schedule fell more than [`PACE_REANCHOR`] behind and restarted.
    reanchored: bool,
}

/// The send schedule. A capture quantum is not a frame: draining it into back-to-back datagrams
/// bursts 4–5 frames then silence. One frame leaves per slot, and a slot lasts one frame of the
/// capture clock ([`Pacer::follow_capture`]), so a capture clock off the host's neither holds
/// surplus nor runs dry. A source behind the schedule is covered with silence once the hole
/// reaches [`InfillPolicy::after`], or the schedule falls behind forever; a source ahead
/// (backlog past one chunk plus one frame) sends a second frame in the same slot, so surplus
/// drains at 2× instead of growing latency. Time is passed in.
///
/// [`InfillPolicy::after`]: crate::audio::capture_policy::InfillPolicy::after
struct Pacer {
    due: Option<std::time::Instant>,
    /// One slot: `nominal` until a [`RATE_WINDOW`] has timed the capture clock.
    interval: std::time::Duration,
    /// One frame at the plane's nominal rate.
    nominal: std::time::Duration,
    /// Start of the current stretch of steady capture, and the samples since.
    window: Option<(std::time::Instant, usize)>,
    /// Interleaved samples in one protocol frame.
    frame_len: usize,
    infill: crate::audio::capture_policy::InfillPolicy,
    /// Recent capture quantum, the bonus-send threshold with one frame on top.
    max_chunk: crate::audio::capture_policy::RecentMax<usize>,
    last_chunk_at: std::time::Instant,
    /// Nothing is synthesized before the first real frame: there is no continuity to protect
    /// yet, and the wire clock has no anchor to continue from.
    sent_any: bool,
    bonus_taken: bool,
}

impl Pacer {
    /// Holes are sized from this session's `frame_us`: PCM frames can be 1 ms, so Opus's 5 ms
    /// constants would be off by up to 5×.
    fn new(
        frame_us: u32,
        frame_len: usize,
        interval: std::time::Duration,
        now: std::time::Instant,
    ) -> Pacer {
        Pacer {
            due: None,
            interval,
            nominal: interval,
            window: None,
            frame_len,
            infill: crate::audio::capture_policy::InfillPolicy::new(frame_us),
            max_chunk: Default::default(),
            last_chunk_at: now,
            sent_any: false,
            bonus_taken: false,
        }
    }

    /// How long capture may block: until the next owed slot, or for a due slot with no audio,
    /// until the hole is old enough to cover. With nothing owed (before the first send, or with
    /// the infill budget spent) it is [`STOP_POLL`]: a silent loopback pushes no chunk at all,
    /// and the loop has to see `stop` before teardown's join gives up on it.
    fn wake_budget(&self, now: std::time::Instant, queued: usize) -> std::time::Duration {
        if self.infill.exhausted() || !self.sent_any {
            return STOP_POLL;
        }
        let ready_at = match self.due {
            Some(due) if queued >= self.frame_len => due,
            Some(due) => due.max(self.last_chunk_at + self.infill.after()),
            None => now + self.interval,
        };
        ready_at.saturating_duration_since(now).min(PACE_MAX_SLEEP)
    }

    /// A capture chunk of `len` samples lasting `quantum`, which also times the capture clock.
    /// `true` = the wire fell silent across the hole it ends: the partial frame and the
    /// redundancy predecessor both describe audio from before a discontinuity.
    fn on_chunk(
        &mut self,
        now: std::time::Instant,
        len: usize,
        quantum: std::time::Duration,
    ) -> bool {
        let broke = self.infill.chunk_arrived();
        // On time is one chunk after the last, a missed graph cycle two: past one and a half, the
        // clock window restarts. A late chunk misread as a miss only delays the estimate.
        if now.duration_since(self.last_chunk_at) > quantum * 3 / 2 {
            self.window = None;
        }
        self.last_chunk_at = now;
        self.max_chunk.note(len, now);
        // The graph's real buffer size, so the infill threshold is one chunk plus one frame and
        // never the middle of a legitimately long cycle.
        self.infill.note_quantum(quantum, now);
        self.follow_capture(now, len);
        broke
    }

    /// Once [`RATE_WINDOW`] of steady capture has arrived, a slot lasts the time `frame_len` of
    /// its samples took, within [`RATE_FOLLOW_PPM`] of nominal. A covered hole or a missed cycle
    /// restarts the window and keeps the slot: the audio lost would read as a slow clock, and
    /// its lag stays for the hole rule to cover.
    fn follow_capture(&mut self, now: std::time::Instant, len: usize) {
        let Some((start, samples)) = self.window else {
            self.window = Some((now, 0));
            return;
        };
        let samples = samples + len;
        let span = now.duration_since(start);
        if span < RATE_WINDOW {
            self.window = Some((start, samples));
            return;
        }
        let samples = (samples as u128).max(1);
        let ns = (span.as_nanos() * self.frame_len as u128 + samples / 2) / samples;
        let nominal = self.nominal.as_nanos();
        let slack = nominal * RATE_FOLLOW_PPM / 1_000_000;
        let ns = ns.clamp(nominal - slack, nominal + slack);
        self.interval = std::time::Duration::from_nanos(ns as u64);
        self.window = Some((now, 0));
    }

    /// Start one wake's releases: at most one bonus frame per wake.
    fn wake(&mut self) {
        self.bonus_taken = false;
    }

    /// The slot due at `now`, with `queued` samples waiting. Silence consumes infill budget and
    /// restarts the capture-clock window.
    fn release(&mut self, now: std::time::Instant, queued: usize) -> Release {
        let mut late = std::time::Duration::ZERO;
        let mut reanchored = false;
        match self.due {
            Some(due) if due > now => {
                return Release {
                    slot: Slot::Wait,
                    late,
                    reanchored,
                }
            }
            Some(due) if now.duration_since(due) > PACE_REANCHOR => {
                reanchored = true;
                self.due = None;
            }
            Some(due) => late = now.duration_since(due),
            None => {}
        }
        let slot = if queued >= self.frame_len {
            Slot::Frame
        } else if !self.sent_any {
            Slot::Wait
        } else {
            // The hole is the longer of the time since the last chunk (the graph stopped) and
            // the schedule lag (the graph fed less than the schedule).
            let hole = now.duration_since(self.last_chunk_at).max(late);
            match self.infill.decide(hole) {
                crate::audio::capture_policy::Infill::Silence => {
                    self.window = None;
                    Slot::Silence {
                        first: self.infill.covered() == self.infill.frame(),
                    }
                }
                _ => Slot::Wait,
            }
        };
        if slot != Slot::Wait {
            // A second frame in this slot when what is left after this one exceeds one chunk
            // plus one frame. Never twice in a row, never for silence.
            let left = queued.saturating_sub(self.frame_len);
            let bonus = slot == Slot::Frame
                && !self.bonus_taken
                && left >= self.max_chunk.get() + self.frame_len;
            self.due = match self.due {
                Some(due) if bonus => Some(due),
                other => Some(other.unwrap_or(now) + self.interval),
            };
            self.bonus_taken = bonus;
        }
        Release {
            slot,
            late,
            reanchored,
        }
    }

    /// A frame reached the wire.
    fn sent(&mut self) {
        self.sent_any = true;
    }
}

/// How one plane's run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlaneEnd {
    /// The session stopped, or the plane cannot go on.
    Done,
    /// A lossless plane's capture left the rate it promised.
    RateLost,
}

/// What the session hands its audio plane, built once at the spawn.
pub(super) struct PlaneArgs {
    pub conn: super::link::SessionLink,
    pub stop: Arc<AtomicBool>,
    pub audio_cap: AudioCapSlot,
    /// Negotiated: 2 / 6 = 5.1 / 8 = 7.1, wire order FL FR FC LFE RL RR SL SR.
    pub channels: u8,
    pub budget: punktfunk_core::audio::AudioBudget,
    /// The format `Welcome` stated — read back by the caller, never recomputed, so the
    /// promised wire and the sent wire cannot disagree.
    pub plane: super::handshake::AudioPlane,
    /// Isolated session sink (`design/gamescope-multiuser.md`): open this exact sink and skip
    /// the shared park slot. A parked shared capturer has the wrong sink; parking an isolated
    /// one would hand this session's name to the next. `None` = shared path. Linux-only.
    pub sink: Option<String>,
    /// A `join` session taps the owner's sink instead of minting a second one of that name.
    pub tap: bool,
    /// This session's live-display record: the sink name goes here on every open, so a later
    /// joiner finds the one to tap.
    pub published: Arc<std::sync::Mutex<Option<String>>>,
    /// The owner's live-display slot, for a joiner on the shared path: the sink to tap appears
    /// there once the owner's capturer is open, which can be after this spawn.
    pub tap_from: Option<Arc<std::sync::Mutex<Option<String>>>>,
    /// Operator mute for THIS session (mgmt `PUT /session/{id}/audio`). Read per frame; the
    /// capturer and the sink stay up so the session sharing them keeps hearing.
    pub muted: Arc<AtomicBool>,
    /// Session totals for the summary. The per-window `SendStats` keeps resetting; these do
    /// not, because the thread outlives the summary that wants them.
    pub counters: Arc<crate::session_status::SessionCounters>,
    /// Where a lossless plane that lost its rate announces its Opus successor.
    pub audio_epochs:
        Option<tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::v2::msg::AudioConfig>>,
    /// The session runs `FEATURE_PCM_PARTS`: a PCM frame over one datagram leaves as parts.
    pub pcm_parts: bool,
}

/// [`run_plane`] on the session's plane. A lossless plane whose capture loses its rate runs on
/// as 48 kHz Opus when the client reopens on an `AudioConfig` (`audio_epochs`, which carries
/// it to the control stream before the first such datagram); else it ends, video going on.
/// `seq` runs on across the switch: the config anchors there.
pub(super) fn audio_thread(args: PlaneArgs) {
    args.counters.note_audio_started();
    let (mut plane, mut seq, mut epoch) = (args.plane, 0u32, 0u8);
    loop {
        let end = run_plane(&args, plane, &mut seq);
        let Some(tx) = args
            .audio_epochs
            .as_ref()
            .filter(|_| end == PlaneEnd::RateLost)
        else {
            return;
        };
        plane = plane.opus_fallback();
        epoch = epoch.wrapping_add(1);
        tracing::info!(
            first_seq = seq,
            "the lossless plane's capture changed rate — the session goes on as 48 kHz Opus"
        );
        if tx
            .send(plane.config(args.channels, epoch, seq, args.budget.redundancy))
            .is_err()
        {
            return;
        }
    }
}

/// One run's wire format and the frame geometry derived from it.
struct PlaneFormat {
    /// The plane's `Welcome::audio_codec`.
    codec: u8,
    pcm: bool,
    /// Interleaved channels, normalised (2 / 6 / 8).
    channels: u8,
    rate_hz: u32,
    bits: u8,
    /// Opus is the fixed 5 ms of `0xC9`; PCM's duration was negotiated from datagram size;
    /// `0xD4`'s is its access unit's label.
    frame_us: u32,
    /// Interleaved samples in one protocol frame (`pcm::samples_per_frame`). Exact on the
    /// 48 kHz family; floors on 44.1 / 88.2 / 176.4, so a frame is up to one sample/ch short
    /// of its label. Safe for sizing; wrong for timing — stamp pts from `frame_duration_ns`,
    /// not `frame_us`. On `0xD4`, whole blocks ([`bitstream::frame_samples`]).
    frame_len: usize,
    /// One protocol frame of wall time, from the real sample count so this clock and pts
    /// agree (0.23 % apart on 44.1 kHz).
    interval: std::time::Duration,
}

impl PlaneFormat {
    fn new(plane: super::handshake::AudioPlane, channels: u8) -> PlaneFormat {
        const OPUS_FRAME_US: u32 = 5_000;
        let channels = punktfunk_core::audio::normalize_channels(channels);
        let pcm = plane.is_pcm();
        let rate_hz = if pcm {
            plane.rate_hz
        } else {
            crate::audio::SAMPLE_RATE
        };
        // `max(1)`: a zero frame would divide the pacer by nothing and emit empty frames at
        // infinite rate.
        let frame_us = if pcm || plane.is_bitstream() {
            (plane.frame_us as u32).max(1)
        } else {
            OPUS_FRAME_US
        };
        let frame_len = if plane.is_bitstream() {
            bitstream::frame_samples(plane.codec, frame_us) * usize::from(channels)
        } else {
            pcm::samples_per_frame(rate_hz, frame_us, channels)
        };
        // `max(1)`: a zero interval would spin the pacer; shortest ladder rung is 1 000 µs.
        let interval = std::time::Duration::from_nanos(
            pcm::frame_duration_ns(frame_len, rate_hz, channels).max(1),
        );
        PlaneFormat {
            codec: plane.codec,
            pcm,
            channels,
            rate_hz,
            bits: plane.bits,
            frame_us,
            frame_len,
            interval,
        }
    }

    fn bitstream(&self) -> bool {
        bitstream::is_bitstream(self.codec)
    }

    /// Wall time of `len` interleaved samples.
    fn duration(&self, len: usize) -> std::time::Duration {
        std::time::Duration::from_nanos(pcm::frame_duration_ns(len, self.rate_hz, self.channels))
    }

    /// Whether the plane goes on at the capture's `live_rate`. Never send a rate we did not get
    /// (`design/hi-res-audio.md`): probe-then-open is a race (hotplug, format change,
    /// renegotiate). PCM ends: the client is open at `rate_hz` and the plane cannot switch.
    /// Opus and `0xD4` warn once through `warned` and go on — 48 kHz, the capturer resamples.
    fn rate_holds(&self, live_rate: u32, warned: &std::sync::Once) -> bool {
        if live_rate == self.rate_hz {
            return true;
        }
        if self.pcm {
            tracing::warn!(
                promised_hz = self.rate_hz,
                capture_hz = live_rate,
                "the capture path changed rate under a session the hi-res gate had already \
                 vetted — ending the lossless audio plane rather than sending samples under \
                 a label that is not theirs (video continues). Reconnect: the gate re-runs \
                 against the capture path as it now is, and resolves either the real rate or \
                 Opus 48 kHz"
            );
            return false;
        }
        warned.call_once(|| {
            tracing::warn!(
                promised_hz = self.rate_hz,
                capture_hz = live_rate,
                "the capture path reports a rate other than the session's — the 48 kHz plane \
                 continues (it is 48 kHz by definition and the capturer resamples), but the \
                 sample clock and the wire clock will not agree if this is real"
            );
        });
        true
    }

    /// The run's opening line. Redundancy is on Opus only, whatever the budget (see [`WireEnc`]).
    fn log_streaming(
        &self,
        plane: super::handshake::AudioPlane,
        budget: punktfunk_core::audio::AudioBudget,
    ) {
        let opus = !self.pcm && !self.bitstream();
        tracing::info!(
            channels = self.channels,
            plane = match self.codec {
                punktfunk_core::quic::AUDIO_CODEC_AC3 => "0xD4 AC-3",
                punktfunk_core::quic::AUDIO_CODEC_EAC3 => "0xD4 E-AC-3",
                _ if self.pcm => "0xD3 PCM",
                _ => "0xC9 Opus",
            },
            lossless = self.pcm,
            rate_hz = self.rate_hz,
            bits = self.bits,
            frame_us = self.frame_us,
            // Meaningful only on the Opus plane — PCM has no tier and no rate control; its cost
            // is exactly `rate × bits × channels`. `0xD4` runs a fixed rate per layout.
            tier = budget.tier.as_str(),
            kbps = if self.bitstream() {
                bitstream::bitrate_kbps(self.codec, self.channels)
            } else if self.pcm {
                pcm::bitrate_kbps(self.rate_hz, self.bits, self.channels)
            } else {
                budget.kbps
            },
            redundancy = budget.redundancy && opus,
            layout = ?plane.layout,
            "punktfunk/1 audio streaming"
        );
    }
}

/// Operator capture gain (`PUNKTFUNK_AUDIO_GAIN`, default 1.0). WASAPI loopback taps upstream
/// of the endpoint master volume, so this is the host-side lift. Read once per run: an operator
/// setting, not a live control.
fn operator_gain() -> f32 {
    let gain = crate::audio::capture_gain();
    if gain != 1.0 {
        tracing::info!(
            gain,
            "audio: applying operator capture gain (soft-limited above \
             {}; headroom, not loudness)",
            punktfunk_core::audio::SOFT_LIMIT_KNEE
        );
    }
    gain
}

/// The omur stream for a `0xD4` plane coding `channels`: codec and blocks from the plane's
/// label, the rate from core's table. `Err` for a plane the handshake gate never grants.
pub(super) fn omur_config(
    plane: super::handshake::AudioPlane,
    channels: u8,
) -> Result<omur::Config, omur::Error> {
    use omur::{Blocks, Codec, Layout};
    let blocks = match bitstream::blocks_for(plane.codec, plane.frame_us.into()) {
        1 => Blocks::One,
        2 => Blocks::Two,
        3 => Blocks::Three,
        _ => Blocks::Six,
    };
    let codec = if plane.codec == punktfunk_core::quic::AUDIO_CODEC_AC3 {
        Codec::Ac3
    } else {
        Codec::Eac3 { blocks }
    };
    let layout = match channels {
        2 => Layout::Stereo,
        6 => Layout::Surround51,
        8 => Layout::Surround71,
        _ => return Err(omur::Error::Layout),
    };
    omur::Config::new(
        codec,
        layout,
        bitstream::bitrate_kbps(plane.codec, channels),
    )
}

/// How a frame becomes datagrams. Redundancy (`0xD2`) exists only on Opus: it is not defined
/// for `0xD3` or `0xD4`, there is no decoder for it there, and doubling a 1.4–33.9 Mbps plane
/// is not a budget. Handshake already refuses the bits; this is the send-side lock.
enum WireEnc {
    /// `payload` is sized exactly — `from_f32` appends, so it is cleared per frame. A 4 KB
    /// Opus guess is too small for 96/24 at the longest rungs. `parts`: the client joins
    /// `0xD5` parts, so a frame over one datagram is split, not refused.
    Pcm {
        bits: u8,
        payload: Vec<u8>,
        parts: bool,
    },
    Opus {
        enc: crate::audio::OpusEnc,
        /// 4096 covers 7.1 HQ ≈ 1.3 KB at 5 ms.
        out: Vec<u8>,
        redundancy: bool,
        /// The previous Opus bytes for the redundant plane. Cleared when continuity breaks,
        /// so it never advertises a predecessor the client's seq does not agree with.
        prev: Vec<u8>,
        /// A stuck encoder fails on every 5 ms frame (~200/s); the warn fires on powers of two.
        errs: u64,
    },
    /// One access unit per frame, a frame late: omur holds each frame for its lookahead.
    Bitstream {
        enc: Box<omur::Encoder>,
        config: omur::Config,
        /// Pts of the frame the encoder holds, which the next pull returns.
        pending_pts: Option<u64>,
        /// Rebuild the encoder before the next frame ([`WireEnc::forget_predecessor`]).
        reset: bool,
        errs: u64,
    },
}

impl WireEnc {
    /// No Opus encoder at all on the PCM or `0xD4` plane — there is nothing for it to do, and
    /// building one would make a libopus failure able to kill a session that does not use it.
    /// `pcm_parts` is the session's `FEATURE_PCM_PARTS`.
    fn open(
        fmt: &PlaneFormat,
        plane: super::handshake::AudioPlane,
        budget: punktfunk_core::audio::AudioBudget,
        pcm_parts: bool,
    ) -> Result<WireEnc> {
        if fmt.pcm {
            let bytes = pcm::frame_payload_bytes(fmt.rate_hz, fmt.bits, fmt.channels, fmt.frame_us);
            return Ok(WireEnc::Pcm {
                bits: fmt.bits,
                payload: Vec::with_capacity(bytes),
                parts: pcm_parts,
            });
        }
        if fmt.bitstream() {
            let config = omur_config(plane, fmt.channels).context("ac-3 encoder config")?;
            return Ok(WireEnc::Bitstream {
                enc: Box::new(omur::Encoder::new(config).context("ac-3 encoder")?),
                config,
                pending_pts: None,
                reset: false,
                errs: 0,
            });
        }
        Ok(WireEnc::Opus {
            enc: native_encoder(fmt.channels, budget.tier, plane.layout).context("opus encoder")?,
            out: vec![0u8; 4096],
            redundancy: budget.redundancy,
            prev: Vec::new(),
            errs: 0,
        })
    }

    /// `samples` as the datagrams of `seq` into `out`: one on Opus; on PCM one, or `0xD5` parts
    /// when the frame is over `max_datagram` and the client joins them; on `0xD4` an access
    /// unit's parts, or none while omur fills its lookahead. An encode error writes none and is
    /// counted; PCM cannot fail. `false`: the unit does not fit 15 parts of `max_datagram`.
    fn datagrams(
        &mut self,
        seq: u32,
        pts_ns: u64,
        samples: &[f32],
        max_datagram: usize,
        out: &mut Vec<Vec<u8>>,
    ) -> bool {
        use punktfunk_core::quic;
        out.clear();
        let (enc, buf, redundancy, prev, errs) = match self {
            WireEnc::Pcm {
                bits,
                payload,
                parts,
            } => {
                payload.clear();
                pcm::from_f32(samples, *bits, payload);
                if *parts && quic::AUDIO_PCM_HEADER + payload.len() > max_datagram {
                    let part = quic::encode_audio_pcm_part_datagram;
                    return split_unit(part, seq, pts_ns, payload, max_datagram, out);
                }
                out.push(quic::encode_audio_pcm_datagram(seq, pts_ns, payload));
                return true;
            }
            WireEnc::Bitstream {
                enc,
                config,
                pending_pts,
                reset,
                errs,
            } => {
                if std::mem::take(reset) {
                    // Never `Err`: `config` was built, so it constructs.
                    if let Ok(fresh) = omur::Encoder::new(*config) {
                        **enc = fresh;
                    }
                    *pending_pts = None;
                }
                // Capture order FL FR FC LFE RL RR SL SR is omur's WAV order (RL RR = BL BR).
                if let Err(e) = enc.push(samples) {
                    *errs += 1;
                    if errs.is_power_of_two() {
                        tracing::warn!(
                            error = %e,
                            count = *errs,
                            "ac-3 encode refused the frame — dropping it"
                        );
                    }
                    *reset = true;
                    return true;
                }
                let held = pending_pts.replace(pts_ns);
                let (Some(unit), Some(unit_pts)) = (enc.pull(), held) else {
                    return true;
                };
                let part = quic::encode_audio_bitstream_datagram;
                return split_unit(part, seq, unit_pts, unit.bytes, max_datagram, out);
            }
            WireEnc::Opus {
                enc,
                out: buf,
                redundancy,
                prev,
                errs,
            } => (enc, buf, *redundancy, prev, errs),
        };
        let n = match enc.encode_float(samples, buf) {
            Ok(n) => n,
            Err(e) => {
                *errs += 1;
                if errs.is_power_of_two() {
                    tracing::warn!(
                        error = %e,
                        count = *errs,
                        "opus encode failed — dropping audio frame"
                    );
                }
                return true;
            }
        };
        let opus = &buf[..n];
        if !redundancy {
            out.push(punktfunk_core::quic::encode_audio_datagram(
                seq, pts_ns, opus,
            ));
            return true;
        }
        out.push(punktfunk_core::quic::encode_audio_red_datagram(
            seq, pts_ns, opus, prev,
        ));
        // Recorded before send so the drop arm can clear it: a frame that never left must not
        // be advertised as `seq`'s predecessor.
        prev.clear();
        prev.extend_from_slice(opus);
        true
    }

    /// Continuity broke. Opus: the next redundant frame carries no predecessor. `0xD4`: the
    /// encoder is rebuilt before the next frame. It holds the previous frame for lookahead,
    /// which would leave after the hole under an old pts, and a passthrough sink would play it
    /// as stale audio. Lazy, since mute calls this every frame.
    fn forget_predecessor(&mut self) {
        match self {
            WireEnc::Opus { prev, .. } => prev.clear(),
            WireEnc::Bitstream { reset, .. } => *reset = true,
            WireEnc::Pcm { .. } => {}
        }
    }
}

/// One part of a unit as its datagram: `(seq, pts_ns, index, count, bytes)`, the `0xD4` or
/// `0xD5` encoder.
type PartEncoder = fn(u32, u64, u8, u8, &[u8]) -> Vec<u8>;

/// `unit` as `encode`'s parts of near-equal size into `out`, each with `seq` and `pts_ns`.
/// `max_datagram` is net of the `punktfunk/2` kind byte ([`super::link::SessionLink`] takes
/// it off). `false` when 15 parts cannot carry it.
fn split_unit(
    encode: PartEncoder,
    seq: u32,
    pts_ns: u64,
    unit: &[u8],
    max_datagram: usize,
    out: &mut Vec<Vec<u8>>,
) -> bool {
    let room = max_datagram.saturating_sub(punktfunk_core::quic::AUDIO_BITSTREAM_HEADER);
    if room == 0 {
        return false;
    }
    let parts = unit.len().div_ceil(room);
    if !(1..=15).contains(&parts) {
        return false;
    }
    // At most `parts` chunks of at most `room` bytes.
    let part = unit.len().div_ceil(parts);
    let count = unit.len().div_ceil(part) as u8;
    for (index, bytes) in unit.chunks(part).enumerate() {
        out.push(encode(seq, pts_ns, index as u8, count, bytes));
    }
    true
}

/// Sleep between checks of a full datagram buffer. Each check takes quinn's connection lock,
/// which the driver needs to drain the buffer.
const ROOM_POLL: std::time::Duration = std::time::Duration::from_micros(100);

/// Whether the datagram buffer (`space`) has room for all of `parts`, checked again every
/// [`ROOM_POLL`] for up to `wait`. The buffer evicts its oldest datagram once full, which after
/// a back-to-back frame is a part of that frame, so on `false` the caller drops this frame
/// whole.
fn parts_have_room(
    space: impl Fn() -> Option<usize>,
    parts: &[Vec<u8>],
    wait: std::time::Duration,
) -> bool {
    // Each part leaves under a one-byte kind and costs the buffer its own record; the room
    // the connection reports is already net of one record.
    let record = punktfunk_core::config::DATAGRAM_QUEUE_OVERHEAD;
    let need = (parts.iter().map(|d| d.len() + 1 + record).sum::<usize>()).saturating_sub(record);
    let t0 = std::time::Instant::now();
    while space().is_some_and(|free| free < need) {
        if t0.elapsed() >= wait {
            return false;
        }
        std::thread::sleep(ROOM_POLL);
    }
    true
}

/// The wire side of one run: the sequence, departures scored per stats window, refused drops.
struct Egress {
    /// Survives reopens and a plane switch, so the client sees a gap, not a restart.
    seq: u32,
    /// `late` is "missed the slot by a whole protocol frame"; PCM frames can be 1 ms, so an
    /// Opus 5 ms constant would miss every slot and report zero.
    stats: crate::audio::capture_policy::SendStats,
    frame_us: u32,
    window_at: std::time::Instant,
    last_departure: Option<std::time::Instant>,
    /// Datagrams the wire refused, for the session. Counted: PCM has no PLC to hide a drop.
    oversized_drops: u64,
    /// Frames in parts the datagram buffer had no room for, for the session.
    congested_drops: u64,
    /// The last frame in parts found no room; until one fits, a frame checks once.
    stalled: bool,
}

impl Egress {
    fn new(seq: u32, frame_us: u32) -> Egress {
        Egress {
            seq,
            stats: crate::audio::capture_policy::SendStats::new(frame_us),
            frame_us,
            window_at: std::time::Instant::now(),
            last_departure: None,
            oversized_drops: 0,
            congested_drops: 0,
            stalled: false,
        }
    }

    /// Datagram `seq` left at `now`: scored against its slot and the previous departure.
    fn departed(
        &mut self,
        now: std::time::Instant,
        late: std::time::Duration,
        infilled: bool,
        counters: &crate::session_status::SessionCounters,
    ) {
        self.seq = self.seq.wrapping_add(1);
        let was_late = self.stats.observe_departure(
            late,
            self.last_departure.map(|t| now.duration_since(t)),
            infilled,
        );
        counters.note_audio_frame(late, infilled, was_late);
        self.last_departure = Some(now);
    }

    /// One oversized frame, not the plane: `seq` advances so the client sees a gap and conceals
    /// it. Persistent means `audio_frame_us` was sized against a datagram budget this path
    /// lacks (see `handshake::pcm_datagram`).
    fn refused(&mut self, fmt: &PlaneFormat, conn: &super::link::SessionLink) {
        self.oversized_drops += 1;
        if self.oversized_drops.is_power_of_two() {
            tracing::warn!(
                count = self.oversized_drops,
                frame_us = fmt.frame_us,
                rate_hz = fmt.rate_hz,
                bits = fmt.bits,
                max_datagram = conn.max_datagram_size(),
                "audio datagram rejected as too large — dropping the frame and \
                 continuing (the session's negotiated audio frame does not fit this \
                 path's datagram size)"
            );
        }
        self.seq = self.seq.wrapping_add(1);
    }

    /// Whether a frame's `parts` fit the datagram buffer (`space`). The first frame of a stall
    /// waits up to `interval` for room; while the buffer stays full, each later frame checks
    /// once. One that does not fit is dropped through [`Self::congested`].
    fn parts_fit(
        &mut self,
        space: impl Fn() -> Option<usize>,
        parts: &[Vec<u8>],
        interval: std::time::Duration,
    ) -> bool {
        let wait = if self.stalled {
            std::time::Duration::ZERO
        } else {
            interval
        };
        self.stalled = !parts_have_room(space, parts, wait);
        if self.stalled {
            self.congested();
        }
        !self.stalled
    }

    /// A frame in parts the datagram buffer had no room for within one frame: the connection
    /// is not draining. `seq` advances so the client sees a gap and conceals it.
    fn congested(&mut self) {
        self.congested_drops += 1;
        if self.congested_drops.is_power_of_two() {
            tracing::warn!(
                count = self.congested_drops,
                "no room in the datagram buffer for an audio frame's parts — dropping the \
                 frame and continuing (the connection is not draining)"
            );
        }
        self.seq = self.seq.wrapping_add(1);
    }

    /// Every `STATS_EVERY`, one line for the window, then a fresh window. Same window as the
    /// capture line so they read as a pair: holes at the tap with clean departures means the
    /// host delivered everything it had.
    fn log_window(&mut self) {
        if self.window_at.elapsed() < crate::audio::capture_policy::STATS_EVERY {
            return;
        }
        tracing::info!(
            sent = self.stats.sent,
            infilled = self.stats.infilled,
            late = self.stats.late,
            max_late_ms = self.stats.max_late_ms(),
            max_spacing_ms = self.stats.max_spacing_ms(),
            reanchors = self.stats.reanchors,
            // Cumulative for the session (the rest of this line resets per window):
            // a total answers "did this ever happen". Zero on a healthy session.
            oversized_drops = self.oversized_drops,
            congested_drops = self.congested_drops,
            "audio egress"
        );
        // Keep this session's `frame_us`. `Default` would zero it, and every
        // departure would then count as late by a whole frame.
        self.stats = crate::audio::capture_policy::SendStats::new(self.frame_us);
        self.window_at = std::time::Instant::now();
    }
}

/// Desktop capture → `plane` (Opus on `AUDIO_MAGIC`/`AUDIO_RED_MAGIC`, PCM on
/// `AUDIO_PCM_MAGIC`, AC-3 / E-AC-3 on `AUDIO_BITSTREAM_MAGIC`) at the negotiated channel count.
/// Capturer comes from and returns to [`AudioCapSlot`] through a [`CaptureLease`]; [`Pacer`]
/// decides when each frame leaves, [`FrameAssembler`] builds it, [`WireEnc`] encodes it and
/// [`Egress`] scores its departure. `seq` is the wire sequence, left where this run stopped:
/// one per frame, or per access unit on `0xD4`.
fn run_plane(args: &PlaneArgs, plane: super::handshake::AudioPlane, seq_io: &mut u32) -> PlaneEnd {
    let fmt = PlaneFormat::new(plane, args.channels);
    // Same boost as the video loop; this thread needs it more: a stall on 5 ms datagrams
    // is a click, not a presentation slip.
    pf_frame::thread_qos::boost_thread_priority(true);
    let route = CaptureRoute {
        sink: args.sink.clone(),
        tap: args.tap,
        tap_from: args.tap_from.clone(),
        published: args.published.clone(),
    };
    let mut lease = CaptureLease::new(fmt.channels as u32, fmt.rate_hz, route);
    let stopped = || args.stop.load(Ordering::SeqCst);
    lease.open(&args.audio_cap, stopped);
    let mut wire = match WireEnc::open(&fmt, plane, args.budget, args.pcm_parts) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                error = %format!("{e:#}"),
                "audio encoder did not open — the session continues without audio"
            );
            lease.park(&args.audio_cap);
            return PlaneEnd::Done;
        }
    };
    let mut datagrams: Vec<Vec<u8>> = Vec::new();
    let mut asm = FrameAssembler::new(fmt.rate_hz, fmt.channels, fmt.frame_len, operator_gain());
    let mut acc: Vec<f32> = Vec::with_capacity(fmt.frame_len * 4);
    let mut pacer = Pacer::new(
        fmt.frame_us,
        fmt.frame_len,
        fmt.interval,
        std::time::Instant::now(),
    );
    // Sample clock (see [`PtsClock`]); the pacer keeps the seed off the wire until the first
    // real send.
    let mut clock = PtsClock::new(fmt.rate_hz, fmt.channels);
    let mut egress = Egress::new(*seq_io, fmt.frame_us);
    // The Opus plane's capture-rate warning fires at most once per session — the condition is
    // re-tested a few hundred times a second, and it is a statement about the capturer, not an
    // event.
    let rate_mismatch_warned = std::sync::Once::new();
    if lease.is_live() {
        fmt.log_streaming(plane, args.budget);
    }
    let mut end = PlaneEnd::Done;
    'session: while !args.stop.load(Ordering::SeqCst) {
        match lease.ready(stopped) {
            Ready::Live => {}
            Ready::Down => continue,
            Ready::Reopened => {
                // The partial frame straddles the gap, and the predecessor is invalid across
                // it: the client would splice pre-gap audio onto the new stream. The new
                // capture starts mid-waveform, so it fades in as after a hole.
                acc.clear();
                wire.forget_predecessor();
                asm.fade_in_next();
            }
        }
        // Checked every pass: PipeWire may not know its rate at open.
        if !fmt.rate_holds(lease.live().sample_rate(), &rate_mismatch_warned) {
            end = PlaneEnd::RateLost;
            break;
        }
        // Wake on a chunk or the next owed slot, whichever first. Waiting only on capture
        // would make a hole cost more than the audio it swallowed — see [`InfillPolicy`].
        let wake = pacer.wake_budget(std::time::Instant::now(), acc.len());
        let Some(chunk) = lease.next_chunk(Some(wake)) else {
            continue;
        };
        if !chunk.is_empty() {
            let quantum = fmt.duration(chunk.len());
            if pacer.on_chunk(std::time::Instant::now(), chunk.len(), quantum) {
                acc.clear();
                wire.forget_predecessor();
            }
            // Anchor on this chunk's arrival. PipeWire hands already-captured audio, so
            // the newest sample is ~now. Re-deriving each chunk ties pts to the device
            // cadence instead of accumulating graph drift.
            let arrival_ns = now_ns();
            acc.extend_from_slice(&chunk);
            let queued_frames = (acc.len() / fmt.channels as usize) as u64;
            // Session rate, not the 48 kHz module constant: at 96 kHz that divisor would
            // put the anchor a whole buffer occupancy too early.
            let anchor =
                arrival_ns.saturating_sub(queued_frames * 1_000_000_000 / fmt.rate_hz as u64);
            // Never step backwards — and see [`PtsClock::reanchor`] for why that asymmetry is
            // exactly what makes a fast clock unrecoverable and the sample-exact stamp mandatory.
            clock.reanchor(anchor);
        }
        pacer.wake();
        loop {
            let now = std::time::Instant::now();
            let Release {
                slot,
                late,
                reanchored,
            } = pacer.release(now, acc.len());
            if reanchored {
                egress.stats.observe_reanchor();
                args.counters.note_audio_reanchor();
            }
            let Some(frame) = asm.next(slot, &mut acc) else {
                break;
            };
            let infilled = frame.infilled;
            // Charge the frame's real sample count, never `frame_us` (a label on 44.1 kHz
            // that would run 2.3 ms/s fast), so the stamp matches the buffer we send. See
            // [`PtsClock`].
            let pts_ns = clock.pts_ns();
            clock.advance(frame.samples.len());
            // Muted: drop the frame before it costs an encode or a datagram. The clock
            // advanced, so unmute resumes at the right pts; `seq` does not, so the client
            // reads a pause rather than loss. The predecessor a RED frame would advertise
            // is from before the silence — clear it, and fade back in like a capture hole.
            if args.muted.load(Ordering::SeqCst) {
                wire.forget_predecessor();
                asm.fade_in_next();
                continue;
            }
            // No datagrams for the rest of the connection: the send arm below would say so
            // once a datagram is built, but `0xD4` builds none without a size.
            let Some(max_datagram) = args.conn.max_datagram_size() else {
                tracing::info!(
                    "the datagram path is unavailable — ending the audio plane for this \
                     session (video continues)"
                );
                break 'session;
            };
            let mut refused = !wire.datagrams(
                egress.seq,
                pts_ns,
                frame.samples,
                max_datagram,
                &mut datagrams,
            );
            if datagrams.is_empty() && !refused {
                continue;
            }
            // A PCM frame's parts queue together or not at all, within one frame's time.
            if fmt.pcm
                && datagrams.len() > 1
                && !egress.parts_fit(|| args.conn.datagram_space(), &datagrams, fmt.interval)
            {
                datagrams.clear();
                continue;
            }
            // Three `DatagramSend` outcomes; only `Unavailable` ends the audio plane. A
            // unit's parts share `seq`: one refused part drops the rest of the unit.
            for d in datagrams.drain(..) {
                match args.conn.send_datagram(d) {
                    super::link::DatagramSend::Sent => {}
                    super::link::DatagramSend::TooLarge => {
                        refused = true;
                        break;
                    }
                    // Datagrams disabled for the rest of the connection. End the audio plane
                    // (capturer parked below; video continues) rather than pacing a wire that
                    // cannot take it. Logged once: this arm breaks.
                    super::link::DatagramSend::Unavailable => {
                        tracing::info!(
                            "the datagram path is unavailable — ending the audio plane for \
                             this session (video continues)"
                        );
                        break 'session;
                    }
                }
            }
            if refused {
                egress.refused(&fmt, &args.conn);
                wire.forget_predecessor();
                continue;
            }
            // `now` is from the top of this iteration — one clock read cheaper, ~200/s.
            egress.departed(now, late, infilled, &args.counters);
            // From here there is a continuity worth protecting, and `clock` has a real
            // anchor to continue from — both preconditions for synthesizing anything.
            pacer.sent();
        }
        egress.log_window();
    }
    lease.park(&args.audio_cap);
    *seq_io = egress.seq;
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const MS: fn(u64) -> Duration = Duration::from_millis;

    /// One frame per slot. A backlog past one chunk plus one frame drains two per slot, never
    /// three; nothing is owed before the first send.
    #[test]
    fn the_pacer_releases_a_frame_a_slot_and_drains_a_backlog_at_twice_that() {
        let t0 = Instant::now();
        let frame = 480; // 5 ms of 48 kHz stereo
        let mut p = Pacer::new(5_000, frame, MS(5), t0);
        assert_eq!(
            p.wake_budget(t0, 0),
            STOP_POLL,
            "nothing owed before the first send, but stop is still read"
        );
        assert_eq!(p.release(t0, 0).slot, Slot::Wait);
        p.on_chunk(t0, frame, MS(5));
        p.wake();
        assert_eq!(p.release(t0, frame).slot, Slot::Frame);
        p.sent();
        assert_eq!(
            p.release(t0, frame).slot,
            Slot::Wait,
            "the next slot is 5 ms out"
        );
        assert_eq!(p.wake_budget(t0, frame), MS(5));

        let t1 = t0 + MS(5);
        p.wake();
        assert_eq!(p.release(t1, 10 * frame).slot, Slot::Frame);
        assert_eq!(
            p.release(t1, 9 * frame).slot,
            Slot::Frame,
            "the bonus frame"
        );
        assert_eq!(
            p.release(t1, 8 * frame).slot,
            Slot::Wait,
            "never three in a slot"
        );
    }

    /// A hole is covered once it outlasts `after()` (10 ms here), fading on its first frame
    /// only; a stall past `PACE_REANCHOR` is forgiven, not repaid; past the 500 ms budget the
    /// wire goes quiet and the next chunk starts a new continuity.
    #[test]
    fn the_pacer_covers_a_hole_then_goes_quiet() {
        let t0 = Instant::now();
        let frame = 480;
        let mut p = Pacer::new(5_000, frame, MS(5), t0);
        p.on_chunk(t0, frame, MS(5));
        p.wake();
        assert_eq!(p.release(t0, frame).slot, Slot::Frame);
        p.sent();
        assert_eq!(
            p.release(t0 + MS(5), 0).slot,
            Slot::Wait,
            "a 5 ms hole is too young"
        );
        let first = Release {
            slot: Slot::Silence { first: true },
            late: MS(5),
            reanchored: false,
        };
        assert_eq!(p.release(t0 + MS(10), 0), first);
        let next = p.release(t0 + MS(10), 0).slot;
        assert_eq!(next, Slot::Silence { first: false });

        assert!(
            !p.on_chunk(t0 + MS(200), 10 * frame, MS(5)),
            "covered: no break"
        );
        p.wake();
        let r = p.release(t0 + MS(200), 10 * frame);
        assert!(r.reanchored, "a 185 ms stall re-anchors");
        assert_eq!((r.slot, r.late), (Slot::Frame, Duration::ZERO));

        let mut t = t0 + MS(200);
        let mut silent = 0;
        for _ in 0..200 {
            t += MS(5);
            if let Slot::Silence { .. } = p.release(t, 0).slot {
                silent += 1;
            }
        }
        assert_eq!(silent, 100, "500 ms of 5 ms frames");
        assert_eq!(
            p.wake_budget(t, 0),
            STOP_POLL,
            "a spent budget waits on capture, and still reads stop"
        );
        assert!(
            p.on_chunk(t, frame, MS(5)),
            "the next chunk starts a new continuity"
        );
    }

    /// What a drift replay measured: silence frames over the whole run, the rest over its
    /// second half.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct DriftRow {
        /// Time a frame waits after its last sample arrived.
        mean_hold_us: u64,
        max_hold_us: u64,
        infills: u32,
        bonuses: u32,
        frames: u64,
    }

    /// The audio thread's wake loop on simulated time: 48 kHz stereo in `chunk`-sample chunks
    /// from a capture clock `ppm` off the host's, paced into 5 ms frames for `secs`. `miss`: the
    /// graph misses one cycle about every 30 s, and that chunk's audio is lost. `follow: false`
    /// keeps every slot nominal, the fixed wall-clock grid.
    fn drift_replay(chunk: usize, ppm: i64, secs: u64, miss: bool, follow: bool) -> DriftRow {
        const PER_SEC: u128 = 96_000;
        let frame = 480;
        let t0 = Instant::now();
        let arrival = |k: u64| {
            let ns = k as u128 * chunk as u128 * 1_000_000_000_000_000
                / (PER_SEC * (1_000_000 + ppm) as u128);
            t0 + Duration::from_nanos(ns as u64)
        };
        let quantum = Duration::from_nanos((chunk as u128 * 1_000_000_000 / PER_SEC) as u64);
        let period = 30 * PER_SEC as u64 / chunk as u64;
        let missed = |k: u64| miss && k > 0 && k % period == 0;
        let mut p = Pacer::new(5_000, frame, MS(5), t0);
        // Each queued chunk's arrival and the samples left of it, oldest first.
        let mut queue = std::collections::VecDeque::<(Instant, usize)>::new();
        let (mut now, mut next, mut queued) = (t0, 0u64, 0usize);
        let steady = t0 + Duration::from_secs(secs / 2);
        let end = t0 + Duration::from_secs(secs);
        let (mut row, mut held_ns) = (DriftRow::default(), 0u128);
        while now < end {
            if missed(next) {
                next += 1;
            }
            let at = arrival(next);
            let wake = p.wake_budget(now, queued);
            if at <= now + wake {
                now = now.max(at);
                next += 1;
                let broke = p.on_chunk(now, chunk, quantum);
                if !follow {
                    p.window = None;
                }
                if broke {
                    queue.clear();
                    queued = 0;
                }
                queue.push_back((now, chunk));
                queued += chunk;
            } else {
                now += wake;
            }
            p.wake();
            loop {
                match p.release(now, queued).slot {
                    Slot::Wait => break,
                    Slot::Frame => {
                        let (mut need, mut last) = (frame, now);
                        while need > 0 {
                            let (at, left) = queue.front_mut().unwrap();
                            let take = need.min(*left);
                            *left -= take;
                            need -= take;
                            last = *at;
                            if *left == 0 {
                                queue.pop_front();
                            }
                        }
                        queued -= frame;
                        if now >= steady {
                            let hold = now - last;
                            held_ns += hold.as_nanos();
                            row.max_hold_us = row.max_hold_us.max(hold.as_micros() as u64);
                            row.frames += 1;
                            row.bonuses += u32::from(p.bonus_taken);
                        }
                    }
                    Slot::Silence { .. } => {
                        queue.clear();
                        queued = 0;
                        row.infills += 1;
                    }
                }
                p.sent();
            }
        }
        row.mean_hold_us = (held_ns / u128::from(row.frames.max(1)) / 1_000) as u64;
        row
    }

    /// A capture clock 50 ppm off the host's either way, at a 5 ms and a 21 ms chunk, over 20
    /// minutes: no silence, and frames wait no longer than on time plus the half millisecond the
    /// first window lets build up. With a graph cycle missed every 30 s, an on-time clock paces
    /// exactly as the fixed grid, so each miss's lag is covered as before; drift adds or saves
    /// at most a few frames of silence on top, where the fixed grid moves it by twelve.
    #[test]
    fn a_drifting_capture_clock_neither_holds_surplus_nor_dips() {
        for chunk in [480, 2048] {
            let on_time = drift_replay(chunk, 0, 1200, false, true);
            assert_eq!(on_time.infills, 0, "{on_time:?}");
            for ppm in [50, -50] {
                let row = drift_replay(chunk, ppm, 1200, false, true);
                assert_eq!(row.infills, 0, "{chunk}/{ppm} ppm: {row:?}");
                assert!(
                    row.mean_hold_us <= on_time.mean_hold_us + 1_000,
                    "{chunk}/{ppm} ppm: {row:?}, on time {on_time:?}"
                );
            }
            let missing = drift_replay(chunk, 0, 1200, true, true);
            assert_eq!(
                missing,
                drift_replay(chunk, 0, 1200, true, false),
                "{chunk}"
            );
            for ppm in [50, -50] {
                let row = drift_replay(chunk, ppm, 1200, true, true);
                assert!(
                    row.infills.abs_diff(missing.infills) <= 3,
                    "{chunk}/{ppm} ppm with misses: {row:?}, on time {missing:?}"
                );
            }
        }
    }

    /// An E-AC-3 5.1 unit leaves a frame late, in parts under one seq and the pts of the frame
    /// it codes. A broken continuity restarts that lag; 15 parts is the ceiling.
    #[test]
    fn a_bitstream_unit_leaves_a_frame_late_in_parts() {
        use punktfunk_core::audio::bitstream::AUDIO_BITSTREAM_EAC3;
        use punktfunk_core::quic::{decode_audio_bitstream_datagram, AUDIO_BITSTREAM_HEADER};
        const DGRAM: usize = 600;
        const MS_NS: u64 = 1_000_000;
        let (plane, channels) = super::super::handshake::resolve_bitstream_plane(
            AUDIO_BITSTREAM_EAC3,
            6,
            40_000,
            Some(DGRAM),
        )
        .unwrap();
        let fmt = PlaneFormat::new(plane, channels);
        assert_eq!((fmt.frame_len, fmt.interval), (768 * 6, MS(16)));
        let budget = punktfunk_core::audio::AudioBudget {
            tier: Default::default(),
            redundancy: true,
            kbps: 0,
        };
        let mut wire = WireEnc::open(&fmt, plane, budget, false).unwrap();
        let unit_bytes = match &wire {
            WireEnc::Bitstream { config, .. } => config.frame_bytes(),
            _ => panic!("not the 0xD4 arm"),
        };
        let parts = unit_bytes.div_ceil(DGRAM - AUDIO_BITSTREAM_HEADER);
        let unit = |out: &[Vec<u8>], seq: u32, pts: u64| {
            assert_eq!(out.len(), parts);
            let mut au = Vec::new();
            for (i, d) in out.iter().enumerate() {
                assert!(d.len() <= DGRAM);
                let (s, p, index, count, bytes) = decode_audio_bitstream_datagram(d).unwrap();
                assert_eq!((s, p, index as usize, count as usize), (seq, pts, i, parts));
                au.extend_from_slice(bytes);
            }
            au
        };
        let silence = vec![0.0f32; fmt.frame_len];
        let mut out = Vec::new();
        assert!(wire.datagrams(0, 0, &silence, DGRAM, &mut out));
        assert!(out.is_empty(), "the first frame fills the lookahead");
        assert!(wire.datagrams(0, 16 * MS_NS, &silence, DGRAM, &mut out));
        let au = unit(&out, 0, 0);
        assert_eq!(au.len(), unit_bytes);
        assert_eq!(au[..2], [0x0B, 0x77], "the sync word");
        assert!(wire.datagrams(1, 32 * MS_NS, &silence, DGRAM, &mut out));
        unit(&out, 1, 16 * MS_NS);
        wire.forget_predecessor();
        assert!(wire.datagrams(2, 48 * MS_NS, &silence, DGRAM, &mut out));
        assert!(
            out.is_empty(),
            "a rebuilt encoder fills its lookahead again"
        );

        let part: PartEncoder = punktfunk_core::quic::encode_audio_bitstream_datagram;
        assert!(!split_unit(part, 0, 0, &au, 50, &mut out), "36 parts");
        assert!(!split_unit(
            part,
            0,
            0,
            &au,
            AUDIO_BITSTREAM_HEADER,
            &mut out
        ));
        assert!(out.is_empty());
    }

    /// The plane `resolve_audio_plane` grants 96 kHz/24-bit 5.1 to a client that joins parts.
    fn parted_plane(max_datagram: usize) -> (PlaneFormat, super::super::handshake::AudioPlane) {
        let plane = super::super::handshake::resolve_audio_plane(
            true,
            true,
            96_000,
            pcm::BITS_24,
            6,
            crate::audio::CaptureRate::Declared,
            200_000,
            Some(max_datagram),
            true,
        );
        assert_eq!((plane.is_pcm(), plane.frame_us), (true, 2000));
        (PlaneFormat::new(plane, 6), plane)
    }

    fn budget() -> punktfunk_core::audio::AudioBudget {
        punktfunk_core::audio::AudioBudget {
            tier: Default::default(),
            redundancy: false,
            kbps: 0,
        }
    }

    /// A PCM frame over one datagram leaves as `0xD5` parts that join into the `0xD3`
    /// payload. One that fits stays `0xD3`; without the client's bit nothing is split.
    #[test]
    fn a_pcm_frame_over_one_datagram_leaves_in_parts() {
        use punktfunk_core::quic::{decode_audio_pcm_datagram, decode_audio_pcm_part_datagram};
        const DGRAM: usize = 1433;
        let (fmt, plane) = parted_plane(DGRAM);
        let samples: Vec<f32> = (0..fmt.frame_len)
            .map(|i| (i as f32 * 0.01).sin() * 0.5)
            .collect();
        let mut whole = Vec::new();
        pcm::from_f32(&samples, pcm::BITS_24, &mut whole);
        assert_eq!(whole.len(), 3456);

        let mut wire = WireEnc::open(&fmt, plane, budget(), true).unwrap();
        let mut out = Vec::new();
        assert!(wire.datagrams(9, 77, &samples, DGRAM, &mut out));
        assert_eq!(out.len(), 3);
        let mut joined = Vec::new();
        for (i, d) in out.iter().enumerate() {
            assert!(d.len() <= DGRAM);
            let (seq, pts, index, count, bytes) = decode_audio_pcm_part_datagram(d).unwrap();
            assert_eq!((seq, pts, index as usize, count), (9, 77, i, 3));
            joined.extend_from_slice(bytes);
        }
        assert_eq!(joined, whole, "the parts are the 0xD3 payload");

        // A path that carries the frame whole gets the plain datagram.
        assert!(wire.datagrams(10, 78, &samples, 4000, &mut out));
        assert_eq!(out.len(), 1);
        assert_eq!(decode_audio_pcm_datagram(&out[0]).unwrap().2, &whole[..]);

        // An older client: one datagram, which the wire then refuses.
        let mut plain = WireEnc::open(&fmt, plane, budget(), false).unwrap();
        assert!(plain.datagrams(11, 79, &samples, DGRAM, &mut out));
        assert_eq!(out.len(), 1);
        assert!(out[0].len() > DGRAM);
    }

    /// Frames sent back to back over a real connection all leave whole: the sender waits for
    /// buffer room, where the buffer would otherwise evict the frame before. Counted at the
    /// sender: a datagram the network loses is not this test's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn back_to_back_parted_frames_all_leave() {
        use punktfunk_core::quic::endpoint;
        const FRAMES: u32 = 40;
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let conn = server.accept().await.unwrap().await.unwrap();
            (server, conn)
        });
        let client = endpoint::client_insecure().unwrap();
        let _client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (_server, conn) = accept.await.unwrap();
        let sender = conn.clone();
        let socket = Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap());
        let link = super::super::link::SessionLink::QuicV2(
            conn.clone(),
            Arc::new(super::super::link::V2Link::new(conn, socket)),
        );

        // The audio thread is a plain thread; so is this sender.
        let queued = tokio::task::spawn_blocking(move || {
            // A loaded test box drains slowly; the wait is the thing under test.
            let wait = std::time::Duration::from_secs(30);
            let max_datagram = link.max_datagram_size().unwrap();
            let (fmt, plane) = parted_plane(max_datagram);
            let mut wire = WireEnc::open(&fmt, plane, budget(), true).unwrap();
            let silence = vec![0.0f32; fmt.frame_len];
            let (mut out, mut queued) = (Vec::new(), 0u64);
            for seq in 0..FRAMES {
                assert!(wire.datagrams(seq, 0, &silence, max_datagram, &mut out));
                assert!(out.len() > 1, "{max_datagram} B a datagram");
                queued += out.len() as u64;
                assert!(
                    parts_have_room(|| link.datagram_space(), &out, wait),
                    "the connection drains"
                );
                for d in out.drain(..) {
                    assert!(matches!(
                        link.send_datagram(d),
                        super::super::link::DatagramSend::Sent
                    ));
                }
            }
            // Empty again: every part left, or was evicted. An empty buffer reports its size
            // less one record, which also pins quinn's accounting the sizing relies on.
            use punktfunk_core::config::{DATAGRAM_QUEUE_OVERHEAD, DATAGRAM_SEND_BUFFER};
            let empty = Some(DATAGRAM_SEND_BUFFER - DATAGRAM_QUEUE_OVERHEAD);
            let t0 = std::time::Instant::now();
            while link.datagram_space() != empty {
                assert!(t0.elapsed() < wait, "the connection drains");
                std::thread::yield_now();
            }
            queued
        })
        .await
        .unwrap();
        assert_eq!(
            sender.stats().frame_tx.datagram,
            queued,
            "a part the buffer evicted never leaves"
        );
    }

    /// A full datagram buffer is re-checked after a sleep, not in a spin, for one frame. While
    /// it stays full each later frame checks once; a frame that fits ends the stall.
    #[test]
    fn a_full_datagram_buffer_costs_one_check_per_frame_once_stalled() {
        use std::cell::Cell;
        let (full, checks) = (Cell::new(true), Cell::new(0u32));
        let space = || {
            checks.set(checks.get() + 1);
            Some(if full.get() { 0 } else { 1 << 20 })
        };
        let parts = vec![vec![0u8; 1200]; 3];
        let wait = MS(50);
        let mut egress = Egress::new(0, 2_000);
        let frame = |e: &mut Egress| {
            checks.set(0);
            (e.parts_fit(space, &parts, wait), checks.get())
        };
        let (fit, n) = frame(&mut egress);
        // A sleep never ends early: 50 ms of 100 µs steps is at most 500 more checks.
        assert!(!fit && (2..=502).contains(&n), "{n} checks");
        assert_eq!(frame(&mut egress), (false, 1), "stalled: one check");
        full.set(false);
        assert_eq!(frame(&mut egress), (true, 1));
        full.set(true);
        let (fit, n) = frame(&mut egress);
        assert!(!fit && n >= 2, "a new stall waits again: {n} checks");
        assert_eq!((egress.congested_drops, egress.seq), (3, 3));
    }

    /// The number this clock exists for, pinned exactly.
    ///
    /// One second of "5 ms" frames at 44 100 Hz stereo: 200 frames × 440 interleaved
    /// samples = 88 000 samples = 997 732 426 ns. The rung is labelled 1 000 000 000 ns
    /// because 44 100 divides none of the seven ladder rungs and `samples_per_frame`
    /// floors 220.5 to 220 per channel. Advancing `frame_us * 1000` instead invents
    /// 2 267 574 ns every second (2 272 ppm).
    #[test]
    fn the_clock_counts_samples_and_not_nominal_frames() {
        let (rate, us, ch) = (44_100u32, 5_000u32, 2u8);
        let n = pcm::samples_per_frame(rate, us, ch);
        assert_eq!(n, 440, "220 samples per channel, not 220.5");

        let mut c = PtsClock::new(rate, ch);
        let frames = 1_000_000 / us as u64; // 200
        for _ in 0..frames {
            c.advance(n);
        }
        let real_ns = c.pts_ns();
        assert_eq!(real_ns, 997_732_426, "88 000 samples at 44 100 Hz stereo");

        // What the nominal advance would have claimed, and the gap between the two.
        let nominal_ns = frames * us as u64 * 1_000;
        assert_eq!(nominal_ns - real_ns, 2_267_574, "invented ns per second");
        let fast_ppm = (nominal_ns - real_ns) * 1_000_000 / real_ns;
        assert_eq!(fast_ppm, 2_272, "the nominal clock runs this many ppm fast");

        // Drift is cumulative: an hour is 8.2 s of invented time. Re-anchor only moves
        // forward, so the client's A/V sync chases it to the end.
        let hour = 3_600 * (nominal_ns - real_ns) / 1_000_000;
        assert_eq!(hour, 8_163, "ms of drift over an hour");
    }

    /// Summing floored per-frame durations is also fine; this pins the difference so the
    /// choice stays on the record. Under 1 ns/frame against the running total — four
    /// orders of magnitude below what the nominal advance invents.
    #[test]
    fn a_running_total_beats_summing_floored_frames_by_a_hair() {
        let (rate, us, ch) = (44_100u32, 5_000u32, 2u8);
        let n = pcm::samples_per_frame(rate, us, ch);
        let frames = 200u64;
        let mut c = PtsClock::new(rate, ch);
        for _ in 0..frames {
            c.advance(n);
        }
        let summed = frames * pcm::frame_duration_ns(n, rate, ch);
        let total = c.pts_ns();
        assert!(
            total >= summed && total - summed < frames,
            "{total} vs {summed}"
        );
    }

    /// On the 48 kHz family a rung IS a duration, so this clock and `frame_us * 1000` are
    /// bit-identical. An Opus session's timestamps must not move by a nanosecond.
    #[test]
    fn the_48k_family_is_bit_identical_to_the_nominal_advance() {
        for (rate, ch) in [(48_000u32, 2u8), (48_000, 6), (48_000, 8), (96_000, 2)] {
            for us in pcm::FRAME_US_LADDER {
                let n = pcm::samples_per_frame(rate, us, ch);
                let mut c = PtsClock::new(rate, ch);
                for i in 1..=400u64 {
                    c.advance(n);
                    assert_eq!(
                        c.pts_ns(),
                        i * us as u64 * 1_000,
                        "{rate} Hz/{ch}ch at {us} µs, frame {i}"
                    );
                }
            }
        }
    }

    /// The fold that keeps the running total bounded must be invisible: it moves whole seconds
    /// from the sample count into the base, and `rate × channels` samples are exactly 1e9 ns at
    /// every rate the plane carries. Run long enough to fold many times, on a rate that divides
    /// nothing.
    #[test]
    fn folding_whole_seconds_does_not_move_the_clock() {
        for (rate, ch) in [(44_100u32, 2u8), (176_400, 8), (88_200, 6), (96_000, 2)] {
            let n = pcm::samples_per_frame(rate, 1_000, ch);
            let mut c = PtsClock::new(rate, ch);
            let mut unfolded: u128 = 0;
            for _ in 0..5_000 {
                c.advance(n);
                unfolded += n as u128;
                // The clock must always equal the duration of every sample ever charged to it,
                // computed in one shot from zero — the property the fold could silently break.
                assert!(
                    c.samples < c.samples_per_sec,
                    "{rate}/{ch}ch: the total was not folded"
                );
                assert_eq!(
                    c.pts_ns(),
                    (unfolded * 1_000_000_000 / (rate as u128 * ch as u128)) as u64,
                    "{rate} Hz/{ch}ch after {unfolded} samples"
                );
            }
        }
    }

    /// The re-anchor is forward-only, and it restarts the running total rather than adding to it.
    /// Both halves matter: moving back would re-issue a pts the client has already played, and
    /// keeping the count across a re-anchor would charge the same span twice.
    #[test]
    fn the_reanchor_only_ever_moves_the_clock_forward() {
        let (rate, ch) = (44_100u32, 2u8);
        let n = pcm::samples_per_frame(rate, 5_000, ch);
        let mut c = PtsClock::new(rate, ch);
        c.reanchor(1_000_000_000);
        assert_eq!(c.pts_ns(), 1_000_000_000);
        c.advance(n);
        let after = c.pts_ns();
        assert_eq!(after, 1_000_000_000 + 4_988_662);
        // An anchor behind the wire clock — capture returning after infilled frames advanced it —
        // is ignored outright.
        c.reanchor(1_000_000_000);
        assert_eq!(c.pts_ns(), after, "a late anchor must not rewind the wire");
        c.reanchor(after);
        assert_eq!(c.pts_ns(), after, "an equal anchor is not a step either");
        // Forward is taken, and the total restarts from it rather than compounding.
        c.reanchor(after + 1_000_000);
        assert_eq!(c.pts_ns(), after + 1_000_000);
        c.advance(n);
        assert_eq!(c.pts_ns(), after + 1_000_000 + 4_988_662);
    }
}
