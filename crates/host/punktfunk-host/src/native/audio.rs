//! Native audio plane: desktop capture → Opus (`AUDIO_MAGIC` / `AUDIO_RED_MAGIC`) or lossless
//! PCM (`AUDIO_PCM_MAGIC`) at the negotiated channel count. Opus is 48 kHz, 5 ms, constrained
//! VBR at [`AudioTier`](punktfunk_core::audio::AudioTier). PCM covers both rate families
//! (44.1/48/88.2/96/176.4 kHz), 16/24-bit, and a negotiated frame duration — see
//! `design/hi-res-audio.md`.
//!
//! The plane is chosen once at handshake and never switches: the client's output device is
//! open at a fixed rate. [`native_encoder`] and [`audio_thread`] compile on linux/windows
//! (libopus + a real capturer); other targets get the stub so a dev build streams video-only.
//! Unlike GameStream this path has no FEC, so it uses constrained VBR rather than hard CBR —
//! see [`native_encoder`].

use super::*;

use crate::audio::{CaptureLease, CaptureRoute, Ready};
use punktfunk_core::audio::pcm;

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
/// How far behind schedule the pacer may fall before it stops trying to catch up and simply
/// re-anchors. Chasing an old schedule after a stall would send a burst — the exact thing
/// pacing exists to prevent — so past this point the debt is forgiven, not repaid.
const PACE_REANCHOR: std::time::Duration = std::time::Duration::from_millis(100);

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

/// The wall-clock send schedule. A capture quantum is not a frame: draining it into
/// back-to-back datagrams bursts 4–5 frames then silence. One frame leaves per slot; a source
/// behind the schedule is covered with silence once the hole reaches [`InfillPolicy::after`],
/// or the schedule falls behind forever; a source ahead (backlog past one chunk plus one
/// frame) sends a second frame in the same slot, so surplus drains at 2× instead of growing
/// latency. Time is passed in.
///
/// [`InfillPolicy::after`]: crate::audio::capture_policy::InfillPolicy::after
struct Pacer {
    due: Option<std::time::Instant>,
    interval: std::time::Duration,
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
            frame_len,
            infill: crate::audio::capture_policy::InfillPolicy::new(frame_us),
            max_chunk: Default::default(),
            last_chunk_at: now,
            sent_any: false,
            bonus_taken: false,
        }
    }

    /// How long capture may block: until the next owed slot, or for a due slot with no audio,
    /// until the hole is old enough to cover. `None` blocks for a chunk: nothing is owed before
    /// the first send, and a spent infill budget should not wake hundreds of times a second to
    /// stay silent.
    fn wake_budget(&self, now: std::time::Instant, queued: usize) -> Option<std::time::Duration> {
        if self.infill.exhausted() || !self.sent_any {
            return None;
        }
        let ready_at = match self.due {
            Some(due) if queued >= self.frame_len => due,
            Some(due) => due.max(self.last_chunk_at + self.infill.after()),
            None => now + self.interval,
        };
        Some(ready_at.saturating_duration_since(now).min(PACE_MAX_SLEEP))
    }

    /// A capture chunk of `len` samples lasting `quantum`. `true` = the wire fell silent across
    /// the hole it ends: the partial frame and the redundancy predecessor both describe audio
    /// from before a discontinuity.
    fn on_chunk(
        &mut self,
        now: std::time::Instant,
        len: usize,
        quantum: std::time::Duration,
    ) -> bool {
        let broke = self.infill.chunk_arrived();
        self.last_chunk_at = now;
        self.max_chunk.note(len, now);
        // The graph's real buffer size, so the infill threshold is one chunk plus one frame and
        // never the middle of a legitimately long cycle.
        self.infill.note_quantum(quantum, now);
        broke
    }

    /// Start one wake's releases: at most one bonus frame per wake.
    fn wake(&mut self) {
        self.bonus_taken = false;
    }

    /// The slot due at `now`, with `queued` samples waiting. Consumes infill budget on silence.
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
            // the schedule lag (the graph fed less than the wall clock).
            let hole = now.duration_since(self.last_chunk_at).max(late);
            match self.infill.decide(hole) {
                crate::audio::capture_policy::Infill::Silence => Slot::Silence {
                    first: self.infill.covered() == self.infill.frame(),
                },
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

/// Desktop capture → the session's resolved plane (Opus on `AUDIO_MAGIC`/`AUDIO_RED_MAGIC`,
/// or PCM on `AUDIO_PCM_MAGIC`) at negotiated `channels` (2 / 6 = 5.1 / 8 = 7.1, wire order
/// FL FR FC LFE RL RR SL SR). Capturer comes from and returns to [`AudioCapSlot`] through a
/// [`CaptureLease`]; [`Pacer`] decides when each frame leaves.
///
/// `plane` is the format `Welcome` stated — read back by the caller, never recomputed, so
/// the promised wire and the sent wire cannot disagree.
#[allow(clippy::too_many_arguments)]
pub(super) fn audio_thread(
    conn: super::link::SessionLink,
    stop: Arc<AtomicBool>,
    audio_cap: AudioCapSlot,
    channels: u8,
    budget: punktfunk_core::audio::AudioBudget,
    plane: super::handshake::AudioPlane,
    // Isolated session sink (`design/gamescope-multiuser.md`): open this exact sink and skip
    // the shared park slot. A parked shared capturer has the wrong sink; parking an isolated
    // one would hand this session's name to the next. `None` = shared path. Linux-only.
    sink: Option<String>,
    // A `join` session taps the owner's sink instead of minting a second one of that name.
    tap: bool,
    // This session's live-display record: the sink name goes here on every open, so a
    // later joiner finds the one to tap.
    published: Arc<std::sync::Mutex<Option<String>>>,
    // The owner's live-display slot, for a joiner on the shared path: the sink to tap
    // appears there once the owner's capturer is open, which can be after this spawn.
    tap_from: Option<Arc<std::sync::Mutex<Option<String>>>>,
    // Operator mute for THIS session (mgmt `PUT /session/{id}/audio`). Read per frame;
    // the capturer and the sink stay up so the session sharing them keeps hearing.
    muted: Arc<AtomicBool>,
    // Session totals for the summary. The per-window `SendStats` below keeps resetting; these
    // do not, because the thread outlives the summary that wants them.
    counters: Arc<crate::session_status::SessionCounters>,
) {
    counters.note_audio_started();
    use crate::audio::SAMPLE_RATE;
    const FRAME_MS: usize = 5;
    /// Fade at each edge of a capture hole, in µs. 1 ms is a slope, not an edge, and too
    /// short to read as a swell. See `last_real` / `resume_fade`.
    const EDGE_FADE_US: u64 = 1_000;
    let want = punktfunk_core::audio::normalize_channels(channels);
    let pcm_plane = plane.is_pcm();
    let rate_hz = if pcm_plane {
        plane.rate_hz
    } else {
        SAMPLE_RATE
    };
    let bits = plane.bits;
    // Opus is the fixed 5 ms of `0xC9`; PCM's duration was negotiated from datagram size.
    // `max(1)`: a zero frame would divide the pacer by nothing and emit empty frames at
    // infinite rate.
    let frame_us: u32 = if pcm_plane {
        (plane.frame_us as u32).max(1)
    } else {
        FRAME_MS as u32 * 1000
    };
    // Interleaved samples in one protocol frame (`pcm::samples_per_frame`). Exact on the
    // 48 kHz family; floors on 44.1 / 88.2 / 176.4, so a frame is up to one sample/ch short
    // of its label. Safe for sizing; wrong for timing — stamp pts from `frame_duration_ns`,
    // not `frame_us`.
    let frame_len = pcm::samples_per_frame(rate_hz, frame_us, want);
    // One protocol frame of wall time, from the real sample count so this clock and pts
    // agree (0.23 % apart on 44.1 kHz). `max(1)`: a zero interval would spin the pacer;
    // shortest ladder rung is 1 000 µs.
    let frame_interval =
        std::time::Duration::from_nanos(pcm::frame_duration_ns(frame_len, rate_hz, want).max(1));
    // Same boost as the video loop; this thread needs it more: a stall on 5 ms datagrams
    // is a click, not a presentation slip.
    pf_frame::thread_qos::boost_thread_priority(true);
    // Tier and redundancy come from `handshake::audio_budget`. Forced off on PCM: `0xD2`
    // is not defined for `0xD3`, there is no PCM decoder for it, and doubling a 1.4–33.9
    // Mbps plane is not a budget. Handshake already refuses both bits; this is the send-side
    // lock so a budget-ladder change cannot turn redundancy on here.
    let (tier, redundancy) = (budget.tier, budget.redundancy && !pcm_plane);

    let route = CaptureRoute {
        sink,
        tap,
        tap_from,
        published,
    };
    let mut lease = CaptureLease::new(want as u32, rate_hz, route);
    let stopped = || stop.load(Ordering::SeqCst);
    lease.open(&audio_cap, stopped);
    // No Opus encoder at all on the PCM plane — there is nothing for it to do, and building one
    // would make a libopus failure able to kill a session that does not use libopus.
    let mut enc = if pcm_plane {
        None
    } else {
        match native_encoder(want, tier, plane.layout) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::warn!(error = %e, "opus encoder init failed — session continues without audio");
                lease.park(&audio_cap);
                return;
            }
        }
    };

    // Operator capture gain (`PUNKTFUNK_AUDIO_GAIN`, default 1.0). WASAPI loopback taps
    // upstream of the endpoint master volume, so this is the host-side lift. Read once:
    // operator setting, not a live control.
    let gain = crate::audio::capture_gain();
    if gain != 1.0 {
        tracing::info!(
            gain,
            "audio: applying operator capture gain (soft-limited above \
             {}; headroom, not loudness)",
            punktfunk_core::audio::SOFT_LIMIT_KNEE
        );
    }
    let mut acc: Vec<f32> = Vec::with_capacity(frame_len * 4);
    let mut frame_buf: Vec<f32> = Vec::with_capacity(frame_len);
    // 4096 covers 7.1 HQ ≈ 1.3 KB at 5 ms. Empty on PCM: no Opus encoder writes here.
    let mut opus_buf = vec![0u8; if pcm_plane { 0 } else { 4096 }];
    // Exact payload size — `from_f32` appends, so clear per frame. A 4 KB Opus guess is
    // too small for 96/24 at the longest rungs.
    let mut pcm_wire: Vec<u8> = if pcm_plane {
        Vec::with_capacity(pcm::frame_payload_bytes(rate_hz, bits, want, frame_us))
    } else {
        Vec::new()
    };
    // Survives reopens so the client sees a gap, not a restart.
    let mut seq: u32 = 0;
    let mut pacer = Pacer::new(
        frame_us,
        frame_len,
        frame_interval,
        std::time::Instant::now(),
    );
    // Fade out into a hole and fade in after it (`EDGE_FADE_US`, raised cosine). Digital
    // zero is a step from the last real sample — a click. `last_real` is the fade-out
    // source when the hole opens on an empty partial.
    let mut last_real: Vec<f32> = Vec::with_capacity(frame_len);
    let mut resume_fade = false;
    // Interleaved: `frames × channels` so the curve spans whole frames.
    let edge_fade_samples = (rate_hz as u64 * EDGE_FADE_US / 1_000_000) as usize * want as usize;
    // A stuck Opus encoder would fail on every 5 ms frame (~200/s); power-of-two throttle the
    // warn so it can't flood stderr + the log ring while still surfacing that it's failing.
    let mut opus_encode_errs: u64 = 0;
    // Previous Opus bytes for the redundant `0xD2` plane. Cleared when continuity breaks
    // so we never advertise a predecessor the client's seq does not agree with.
    let mut prev_frame: Vec<u8> = Vec::new();
    // Sample clock (see [`PtsClock`]); the pacer keeps the seed off the wire until the first
    // real send.
    let mut clock = PtsClock::new(rate_hz, want);
    // Wire-side counters. `late` is "missed the slot by a whole protocol frame"; PCM
    // frames can be 1 ms, so an Opus 5 ms constant would miss every slot and report zero.
    let mut send_stats = crate::audio::capture_policy::SendStats::new(frame_us);
    let mut last_send_stats = std::time::Instant::now();
    let mut last_departure: Option<std::time::Instant> = None;
    // Datagrams the wire refused. Counted: PCM has no PLC to hide a drop.
    let mut oversized_drops: u64 = 0;
    // The Opus plane's capture-rate warning fires at most once per session — the condition is
    // re-tested a few hundred times a second, and it is a statement about the capturer, not an
    // event.
    let rate_mismatch_warned = std::sync::Once::new();
    if lease.is_live() {
        tracing::info!(
            channels = want,
            plane = if pcm_plane { "0xD3 PCM" } else { "0xC9 Opus" },
            lossless = pcm_plane,
            rate_hz,
            bits,
            frame_us,
            // Meaningful only on the Opus plane — PCM has no tier and no rate control; its cost
            // is exactly `rate × bits × channels`.
            tier = tier.as_str(),
            kbps = if pcm_plane {
                pcm::bitrate_kbps(rate_hz, bits, want)
            } else {
                budget.kbps
            },
            redundancy,
            layout = ?plane.layout,
            "punktfunk/1 audio streaming"
        );
    }
    'session: while !stop.load(Ordering::SeqCst) {
        match lease.ready(stopped) {
            Ready::Live => {}
            Ready::Down => continue,
            Ready::Reopened => {
                // The partial frame straddles the gap, and the predecessor is invalid across
                // it: the client would splice pre-gap audio onto the new stream.
                acc.clear();
                prev_frame.clear();
            }
        }
        // Never send a rate we did not get (`design/hi-res-audio.md`). Probe-then-open is a
        // race (hotplug, format change, renegotiate). PCM ends: the client is open at
        // `rate_hz` and the plane cannot switch. Opus only warns — 48 kHz, capturer resamples.
        // Checked every iteration: PipeWire may not know its rate at open.
        let live_rate = lease.live().sample_rate();
        if live_rate != rate_hz {
            if pcm_plane {
                tracing::warn!(
                    promised_hz = rate_hz,
                    capture_hz = live_rate,
                    "the capture path changed rate under a session the hi-res gate had already \
                     vetted — ending the lossless audio plane rather than sending samples under \
                     a label that is not theirs (video continues). Reconnect: the gate re-runs \
                     against the capture path as it now is, and resolves either the real rate or \
                     Opus 48 kHz"
                );
                break 'session;
            }
            rate_mismatch_warned.call_once(|| {
                tracing::warn!(
                    promised_hz = rate_hz,
                    capture_hz = live_rate,
                    "the capture path reports a rate other than the session's — the Opus plane \
                     continues (it is 48 kHz by definition and the capturer resamples), but the \
                     sample clock and the wire clock will not agree if this is real"
                );
            });
        }
        // Wake on a chunk or the next owed slot, whichever first. Waiting only on capture
        // made a hole cost more than the audio it swallowed — see [`InfillPolicy`].
        let wake = pacer.wake_budget(std::time::Instant::now(), acc.len());
        let Some(chunk) = lease.next_chunk(wake) else {
            continue;
        };
        if !chunk.is_empty() {
            let quantum =
                std::time::Duration::from_nanos(pcm::frame_duration_ns(chunk.len(), rate_hz, want));
            if pacer.on_chunk(std::time::Instant::now(), chunk.len(), quantum) {
                acc.clear();
                prev_frame.clear();
            }
            // Anchor on this chunk's arrival. PipeWire hands already-captured audio, so
            // the newest sample is ~now. Re-deriving each chunk ties pts to the device
            // cadence instead of accumulating graph drift.
            let arrival_ns = now_ns();
            acc.extend_from_slice(&chunk);
            let queued_frames = (acc.len() / want as usize) as u64;
            // Session rate, not the 48 kHz module constant: at 96 kHz that divisor would
            // put the anchor a whole buffer occupancy too early.
            let anchor = arrival_ns.saturating_sub(queued_frames * 1_000_000_000 / rate_hz as u64);
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
                send_stats.observe_reanchor();
                counters.note_audio_reanchor();
            }
            frame_buf.clear();
            let infilled = match slot {
                Slot::Wait => break,
                Slot::Frame => {
                    frame_buf.extend(acc.drain(..frame_len));
                    false
                }
                Slot::Silence { first } => {
                    // Send the partial padded with silence; do not wait for post-gap samples
                    // to complete it (that frame would straddle the hole). Dropping it would
                    // come back as schedule lag.
                    let partial = acc.len();
                    frame_buf.append(&mut acc);
                    if first {
                        // Fade the tail we have — the partial, or `last_real` if empty — so
                        // the hole is a slope from the listener's level, not a step.
                        if partial == 0 && !last_real.is_empty() {
                            let n = edge_fade_samples.min(last_real.len());
                            frame_buf.extend_from_slice(&last_real[..n]);
                        }
                        let n = frame_buf.len().min(edge_fade_samples);
                        pcm::raised_cosine_tail(&mut frame_buf, n);
                    }
                    frame_buf.resize(frame_len, 0.0);
                    // Whatever follows this hole starts mid-waveform.
                    resume_fade = true;
                    true
                }
            };
            if !infilled {
                if gain != 1.0 {
                    punktfunk_core::audio::apply_gain(&mut frame_buf, gain);
                }
                if std::mem::take(&mut resume_fade) {
                    // First real frame after a hole: fade it in from silence, the mirror of the
                    // fade the hole started with.
                    pcm::raised_cosine_head(&mut frame_buf, edge_fade_samples);
                }
                // What the next hole's fade is built from when it opens on an empty partial —
                // recorded post-gain, so the fade sits at the level the listener was hearing.
                last_real.clear();
                last_real.extend_from_slice(&frame_buf);
            }
            // Charge the frame's real sample count, never `frame_us` (a label on 44.1 kHz
            // that would run 2.3 ms/s fast). `frame_buf.len()` so the stamp matches the
            // buffer we send. See [`PtsClock`].
            let pts_ns = clock.pts_ns();
            clock.advance(frame_buf.len());
            // Muted: drop the frame before it costs an encode or a datagram. The clock
            // advanced, so unmute resumes at the right pts; `seq` does not, so the client
            // reads a pause rather than loss. The predecessor a RED frame would advertise
            // is from before the silence — clear it, and fade back in like a capture hole.
            if muted.load(Ordering::SeqCst) {
                prev_frame.clear();
                resume_fade = true;
                continue;
            }
            // One send path for both planes. `None` = Opus encode error (already counted).
            // PCM cannot fail: `from_f32` is scale-and-clamp over a known length.
            let datagram: Option<Vec<u8>> = if pcm_plane {
                pcm_wire.clear();
                pcm::from_f32(&frame_buf, bits, &mut pcm_wire);
                Some(punktfunk_core::quic::encode_audio_pcm_datagram(
                    seq, pts_ns, &pcm_wire,
                ))
            } else {
                // Bind the encode result first: the match arms take an immutable slice of
                // `opus_buf`, and a `&mut opus_buf` in the scrutinee would live for the
                // whole match.
                let encoded = enc
                    .as_mut()
                    .expect("opus plane has an encoder")
                    .encode_float(&frame_buf, &mut opus_buf);
                match encoded {
                    Ok(n) => {
                        let opus = &opus_buf[..n];
                        let d = if redundancy {
                            punktfunk_core::quic::encode_audio_red_datagram(
                                seq,
                                pts_ns,
                                opus,
                                &prev_frame,
                            )
                        } else {
                            punktfunk_core::quic::encode_audio_datagram(seq, pts_ns, opus)
                        };
                        if redundancy {
                            // Recorded before send so the drop arm can clear it: a frame
                            // that never left must not be advertised as `seq`'s predecessor.
                            prev_frame.clear();
                            prev_frame.extend_from_slice(opus);
                        }
                        Some(d)
                    }
                    Err(e) => {
                        opus_encode_errs += 1;
                        if opus_encode_errs.is_power_of_two() {
                            tracing::warn!(
                                error = %e,
                                count = opus_encode_errs,
                                "opus encode failed — dropping audio frame"
                            );
                        }
                        None
                    }
                }
            };
            let Some(d) = datagram else { continue };
            // Four `SendDatagramError` outcomes; only `ConnectionLost` ends the session.
            match conn.send_datagram(d) {
                super::link::DatagramSend::Sent => {
                    seq = seq.wrapping_add(1);
                    // Score against the slot and the previous departure. `now` is from the
                    // top of this iteration — one clock read cheaper, ~200/s.
                    let was_late = send_stats.observe_departure(
                        late,
                        last_departure.map(|t| now.duration_since(t)),
                        infilled,
                    );
                    counters.note_audio_frame(late, infilled, was_late);
                    last_departure = Some(now);
                    // From here there is a continuity worth protecting, and `clock` has a real
                    // anchor to continue from — both preconditions for synthesizing anything.
                    pacer.sent();
                }
                // One oversized frame, not the plane. Advance `seq` so the client sees a
                // gap and conceals it. Warn on powers of two. Persistent means
                // `audio_frame_us` was sized against a datagram budget this path lacks
                // (see MTU note in `handshake::negotiate`).
                super::link::DatagramSend::TooLarge => {
                    oversized_drops += 1;
                    if oversized_drops.is_power_of_two() {
                        tracing::warn!(
                            count = oversized_drops,
                            frame_us,
                            rate_hz,
                            bits,
                            max_datagram = conn.max_datagram_size(),
                            "audio datagram rejected as too large — dropping the frame and \
                             continuing (the session's negotiated audio frame does not fit this \
                             path's datagram size)"
                        );
                    }
                    seq = seq.wrapping_add(1);
                    prev_frame.clear();
                }
                // Datagrams disabled for the rest of the connection. End the audio plane
                // (capturer parked below; video continues) rather than pacing a wire that
                // cannot take it. Logged once: this arm breaks.
                super::link::DatagramSend::Unavailable => {
                    tracing::info!(
                        "the datagram path is unavailable — ending the audio plane for this \
                         session (video continues)"
                    );
                    break 'session;
                }
            }
        }
        if last_send_stats.elapsed() >= crate::audio::capture_policy::STATS_EVERY {
            // Same window as the capture line so they read as a pair: holes at the tap
            // with clean departures means the host delivered everything it had.
            tracing::info!(
                sent = send_stats.sent,
                infilled = send_stats.infilled,
                late = send_stats.late,
                max_late_ms = send_stats.max_late_ms(),
                max_spacing_ms = send_stats.max_spacing_ms(),
                reanchors = send_stats.reanchors,
                // Cumulative for the session (the rest of this line resets per window):
                // a total answers "did this ever happen". Zero on a healthy session.
                oversized_drops,
                "audio egress"
            );
            // Keep this session's `frame_us`. `Default` would zero it, and every
            // departure would then count as late by a whole frame.
            send_stats = crate::audio::capture_policy::SendStats::new(frame_us);
            last_send_stats = std::time::Instant::now();
        }
    }
    lease.park(&audio_cap);
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
            None,
            "blocks for capture before the first send"
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
        assert_eq!(p.wake_budget(t0, frame), Some(MS(5)));

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
            None,
            "a spent budget blocks for capture"
        );
        assert!(
            p.on_chunk(t, frame, MS(5)),
            "the next chunk starts a new continuity"
        );
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
