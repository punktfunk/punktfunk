//! A/V sync: the depth the audio ring should aim for so audio plays with its picture.

use super::*;

/// Lock-free hand-off between the decode/pull thread (timestamps) and the realtime callback
/// (ring + [`JitterPolicy`]). The callback must not block, so they trade two words.
///
/// `usize::MAX` encodes "no target", not `0` — `0` is a valid depth and would silently drain
/// the ring.
#[derive(Debug)]
pub struct AudioSyncCell {
    depth: std::sync::atomic::AtomicUsize,
    target: std::sync::atomic::AtomicUsize,
    /// Concealment the decode side has synthesized this session, ms. Produced on decode, read
    /// from the callback's 10 s playback line.
    plc_ms: std::sync::atomic::AtomicU64,
    /// Device output latency past the ring, ns ([`AvSyncObservation::output_latency_ns`]).
    /// Produced by the backend, read on decode.
    output_latency_ns: std::sync::atomic::AtomicU64,
}

impl Default for AudioSyncCell {
    fn default() -> Self {
        AudioSyncCell {
            depth: std::sync::atomic::AtomicUsize::new(0),
            target: std::sync::atomic::AtomicUsize::new(usize::MAX),
            plc_ms: std::sync::atomic::AtomicU64::new(0),
            output_latency_ns: std::sync::atomic::AtomicU64::new(0),
        }
    }
}

impl AudioSyncCell {
    pub fn publish_depth(&self, depth: usize) {
        self.depth
            .store(depth, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn depth(&self) -> usize {
        self.depth.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn publish_plc_ms(&self, ms: u64) {
        self.plc_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn plc_ms(&self) -> u64 {
        self.plc_ms.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn publish_output_latency_ns(&self, ns: u64) {
        self.output_latency_ns
            .store(ns, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn output_latency_ns(&self) -> u64 {
        self.output_latency_ns
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Decode side: ask the ring to aim for this depth (`None` = unsynchronised).
    pub fn set_target(&self, target: Option<usize>) {
        self.target.store(
            target.unwrap_or(usize::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn target(&self) -> Option<usize> {
        match self.target.load(std::sync::atomic::Ordering::Relaxed) {
            usize::MAX => None,
            t => Some(t),
        }
    }
}

/// Smoothing time constant for the measured A/V offset, in ms of consumed audio. Long enough
/// that network jitter and a single late datagram do not move it; short enough to track real
/// drift.
const AV_EWMA_TAU_MS: u32 = 2_000;
/// Offsets inside this band are left alone. Correcting a few ms costs a real discontinuity and
/// buys nothing a listener can perceive; the deadband stops the loop hunting around zero.
pub(super) const AV_DEADBAND_MS: u32 = 10;
/// Audio observed before the first correction is offered, in [`FRAME_MS`] frames (500 ms). The
/// offset is derived from a clock skew estimate and a video figure that both need a moment to
/// settle after connect; acting on the first sample would chase the handshake, not the stream.
const AV_MIN_OBSERVATIONS: u32 = 100;
/// An offset larger than this is not believed. A wall-clock step, a paused host, or a stale
/// video figure can all produce an enormous apparent misalignment, and steering the ring by
/// it would empty or overfill it. Beyond this the loop reports and waits rather than acting.
const AV_SANE_LIMIT_MS: u32 = 1_000;

/// Turns "when will this audio play" and "when did its picture reach the glass" into a ring
/// depth [`JitterPolicy`] should aim for.
///
/// Video is the master: the video leg is the input-feel budget and must not inflate to satisfy
/// the audio clock. Audio moves, via small crossfaded corrections ([`crossfade_drop`]).
/// Continuity outranks sync: this type only proposes a depth; [`JitterPolicy`] clamps it to
/// the underrun-driven floor. See [`JitterPolicy::set_sync_target`].
#[derive(Clone, Debug)]
pub struct AvSync {
    /// Negotiated layout, same two numbers [`JitterPolicy`] keeps, so a millisecond agrees down
    /// to the sample.
    rate_hz: u32,
    channels: u8,
    /// EWMA of the measured offset in ns. Positive = audio is scheduled to play late relative
    /// to the picture it belongs with.
    offset_avg_ns: f32,
    observations: u32,
    /// Audio the observations cover: one frame each.
    observed_us: u64,
    /// One observation's frame, for the EWMA weight; see [`AvSync::set_frame_us`].
    frame_us: u32,
    implausible: bool,
    /// Last depth offered outside the deadband; what the deadband keeps asking for.
    held: Option<usize>,
}

/// One measurement for [`AvSync::observe`]. Each field is in the units its source already produces.
#[derive(Clone, Copy, Debug)]
pub struct AvSyncObservation {
    pub pts_ns: u64,
    /// Local wall-clock now, same basis the client's video latency math uses (CLOCK_REALTIME).
    pub now_local_ns: i128,
    /// Host clock minus client clock, from the skew handshake (`clock_offset_now_ns`).
    pub clock_offset_ns: i64,
    /// How much audio is already queued ahead of this frame, in interleaved samples.
    pub buffered_ahead: usize,
    /// Device output latency past the ring: from a sample leaving it to the speaker (the
    /// graph or endpoint buffer, a Bluetooth link). `0` = unknown.
    pub output_latency_ns: u64,
    /// Video end-to-end in ns: `displayed + clock_offset − pts`. `None` until a frame is presented.
    pub video_e2e_ns: Option<u64>,
}

impl AvSync {
    /// `channels` is 2/6/8 at [`SAMPLE_RATE_HZ`]. Hi-res uses [`new_at_rate`](Self::new_at_rate).
    pub fn new(channels: u8) -> AvSync {
        Self::new_at_rate(channels, SAMPLE_RATE_HZ)
    }

    /// As [`new`](Self::new), at an explicitly negotiated `rate_hz`. Multiply-before-divide so
    /// the 44.1 kHz family is representable and the proposed depth is in the ring's units.
    pub fn new_at_rate(channels: u8, rate_hz: u32) -> AvSync {
        AvSync {
            rate_hz: rate_hz.max(1),
            channels: channels.max(1),
            offset_avg_ns: 0.0,
            observations: 0,
            observed_us: 0,
            frame_us: FRAME_MS * 1000,
            implausible: false,
            held: None,
        }
    }

    /// The negotiated frame, one per [`Self::observe`] call. Default [`FRAME_MS`]; a lossless
    /// plane sends 1–4 ms frames, and the weights are in audio time, not calls.
    pub fn set_frame_us(&mut self, frame_us: u32) {
        self.frame_us = frame_us.max(1);
    }

    fn samples_ms(&self, samples: usize) -> u32 {
        samples_to_ms(self.rate_hz, self.channels, samples)
    }

    /// Fold one measurement. Smoothed offset in ns once there is enough evidence (positive =
    /// audio late), or `None` while settling. Rejects the implausible rather than clamping it:
    /// a clamped wrong value would be acted on as a small real one.
    pub fn observe(&mut self, o: AvSyncObservation) -> Option<i64> {
        // No frame on the glass yet: no reference to align against.
        let video_e2e_ns = o.video_e2e_ns?;
        // Play-at in the host capture clock, same shape as the video figure. Rounded to whole
        // milliseconds; ≤ 1 ms is inside [`AV_DEADBAND_MS`]. The conversion itself is exact at
        // every rate.
        let buffered_ns = self.samples_ms(o.buffered_ahead) as i128 * 1_000_000;
        let play_at_host =
            o.now_local_ns + buffered_ns + o.output_latency_ns as i128 + o.clock_offset_ns as i128;
        let audio_e2e_ns = play_at_host - o.pts_ns as i128;
        let offset_ns = audio_e2e_ns - video_e2e_ns as i128;

        if offset_ns.unsigned_abs() > (AV_SANE_LIMIT_MS as u128) * 1_000_000 {
            self.implausible = true;
            return None;
        }
        self.implausible = false;

        // Weight by this plane's frame: the caller observes once per packet, so the time
        // constant stays in audio time whatever the frame length.
        let alpha = (self.frame_us as f32 / (AV_EWMA_TAU_MS * 1000) as f32).clamp(0.0, 1.0);
        if self.observations == 0 {
            self.offset_avg_ns = offset_ns as f32;
        } else {
            self.offset_avg_ns += (offset_ns as f32 - self.offset_avg_ns) * alpha;
        }
        self.observations = self.observations.saturating_add(1);
        self.observed_us = self.observed_us.saturating_add(u64::from(self.frame_us));
        self.settled().then_some(self.offset_avg_ns as i64)
    }

    pub fn settled(&self) -> bool {
        self.observed_us >= u64::from(AV_MIN_OBSERVATIONS * FRAME_MS * 1000)
    }

    /// Smoothed offset in ms (positive = audio late), for the HUD. Reported while still settling
    /// so the operator can watch it converge.
    pub fn offset_ms(&self) -> i32 {
        (self.offset_avg_ns / 1_000_000.0) as i32
    }

    pub fn implausible(&self) -> bool {
        self.implausible
    }

    /// The ring depth that would place audio with the picture, given where the ring is now.
    /// `None` while unsettled: the caller runs unsynchronised. Inside the deadband, the last
    /// request again: a ring that reached its depth stays there, where `None` would drop it to
    /// the floor and shed what the insert just built.
    ///
    /// Audio late (offset > 0) means there is too much queued: aim shallower. Audio early means
    /// aim deeper.
    pub fn desired_depth(&mut self, current_depth: usize) -> Option<usize> {
        if !self.settled() {
            self.held = None;
            return None;
        }
        let offset_ms = self.offset_avg_ns / 1_000_000.0;
        if offset_ms.abs() < AV_DEADBAND_MS as f32 {
            return self.held;
        }
        // One millisecond of samples as a float, so a fractional offset scales smoothly.
        // Divide the constant, not the product: `x * 96000.0 / 1000.0` can land one sample off
        // the `x * 96.0` every 48 kHz session computes. This way 44 100 Hz stereo is 88.2, not 88.
        let per_ms = interleaved_per_sec(self.rate_hz, self.channels) as f32 / 1000.0;
        let delta = (offset_ms * per_ms) as i64;
        self.held = Some((current_depth as i64 - delta).max(0) as usize);
        self.held
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn per_ms(channels: u8) -> usize {
        (SAMPLE_RATE_HZ / 1000) as usize * channels as usize
    }

    // ---- A/V sync ------------------------------------------------------------------------

    fn obs(offset_ms: i64, depth: usize, per_ms: usize) -> AvSyncObservation {
        // audio_e2e = buffered + (now + skew - pts). Pin now/skew/pts so the only free term is
        // the buffered depth, then choose video_e2e so the difference lands on `offset_ms`.
        let buffered_ms = (depth / per_ms) as i64;
        let audio_e2e_ms = buffered_ms + 40; // 40 ms of transport, arbitrary but fixed
        let video_e2e_ms = audio_e2e_ms - offset_ms;
        AvSyncObservation {
            pts_ns: 1_000_000_000,
            now_local_ns: 1_000_000_000i128 + 40 * 1_000_000,
            clock_offset_ns: 0,
            buffered_ahead: depth,
            output_latency_ns: 0,
            video_e2e_ns: Some((video_e2e_ms.max(0) as u64) * 1_000_000),
        }
    }

    fn settle(sync: &mut AvSync, offset_ms: i64, depth: usize, per_ms: usize, n: u32) {
        for _ in 0..n {
            sync.observe(obs(offset_ms, depth, per_ms));
        }
    }

    /// `new` is exactly `new_at_rate` at the protocol default.
    #[test]
    fn the_default_constructor_is_the_default_rate() {
        let x = AvSync::new(2);
        let y = AvSync::new_at_rate(2, SAMPLE_RATE_HZ);
        assert_eq!((x.rate_hz, x.channels), (y.rate_hz, y.channels));
    }

    #[test]
    fn av_sync_needs_evidence_before_acting() {
        let pm = per_ms(2);
        let mut s = AvSync::new(2);
        assert!(s.observe(obs(50, 30 * pm, pm)).is_none());
        assert!(!s.settled());
        assert!(s.desired_depth(30 * pm).is_none());
        settle(&mut s, 50, 30 * pm, pm, AV_MIN_OBSERVATIONS);
        assert!(s.settled(), "should act once the evidence is in");
    }

    /// A 2 ms lossless plane observes 2.5× as often; settling and smoothing stay in audio time.
    #[test]
    fn short_frames_settle_on_the_same_span_of_audio() {
        let pm = per_ms(2);
        let mut s = AvSync::new(2);
        s.set_frame_us(2_000);
        settle(&mut s, 50, 30 * pm, pm, AV_MIN_OBSERVATIONS);
        assert!(
            !s.settled(),
            "100 × 2 ms is 200 ms of audio, not the 500 ms gate"
        );
        settle(&mut s, 50, 30 * pm, pm, AV_MIN_OBSERVATIONS * 3 / 2);
        assert!(s.settled());
        // One 2 ms observation moves the average by 2/2000 of the step, not 5/2000.
        s.observe(obs(70, 30 * pm, pm));
        assert!(
            (s.offset_avg_ns / 1e6 - 50.02).abs() < 0.001,
            "{}",
            s.offset_avg_ns
        );
    }

    #[test]
    fn av_sync_aims_shallower_when_audio_is_late() {
        let pm = per_ms(2);
        let depth = 60 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, 40, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let want = s
            .desired_depth(depth)
            .expect("a 40 ms offset is actionable");
        assert!(
            want < depth,
            "audio late must aim shallower: {want} vs {depth}"
        );
        let shed_ms = (depth - want) / pm;
        assert!(
            (35..=45).contains(&shed_ms),
            "should aim to shed ~40 ms, got {shed_ms}"
        );
    }

    #[test]
    fn av_sync_aims_deeper_when_audio_is_early() {
        let pm = per_ms(2);
        let depth = 20 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, -30, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let want = s
            .desired_depth(depth)
            .expect("a 30 ms offset is actionable");
        assert!(
            want > depth,
            "audio early must aim deeper: {want} vs {depth}"
        );
    }

    #[test]
    fn av_sync_deadbands_what_no_one_can_hear() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        settle(
            &mut s,
            (AV_DEADBAND_MS - 2) as i64,
            depth,
            pm,
            AV_MIN_OBSERVATIONS * 4,
        );
        assert!(
            s.desired_depth(depth).is_none(),
            "an offset inside the deadband must not provoke a (real, if crossfaded) discontinuity"
        );
    }

    /// Audio that leaves the ring still has the device to cross. A 150 ms Bluetooth link on a
    /// ring that alone looks 30 ms early is audio 120 ms late: aim shallower, never deeper.
    #[test]
    fn av_sync_counts_the_device_behind_the_ring() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        for _ in 0..AV_MIN_OBSERVATIONS * 4 {
            s.observe(AvSyncObservation {
                output_latency_ns: 150_000_000,
                ..obs(-30, depth, pm)
            });
        }
        assert_eq!(s.offset_ms(), 120);
        let want = s
            .desired_depth(depth)
            .expect("a 120 ms offset is actionable");
        assert!(
            want < depth,
            "late audio must aim shallower: {want} vs {depth}"
        );
    }

    #[test]
    fn av_sync_rejects_the_implausible_instead_of_clamping_it() {
        let pm = per_ms(2);
        let depth = 30 * pm;
        let mut s = AvSync::new(2);
        settle(&mut s, 30, depth, pm, AV_MIN_OBSERVATIONS * 4);
        let before = s.offset_ms();
        // A wall-clock step / stale video figure. Built directly rather than through `obs`:
        // that helper floors the video figure at zero, which would cap the offset at a merely
        // large value and let this test pass without ever exercising the rejection.
        let wild = AvSyncObservation {
            pts_ns: 0,
            now_local_ns: 5_000_000_000,
            clock_offset_ns: 0,
            buffered_ahead: depth,
            output_latency_ns: 0,
            video_e2e_ns: Some(40_000_000),
        };
        assert!(s.observe(wild).is_none());
        assert!(s.implausible(), "a ~5 s offset must be refused, not folded");
        assert_eq!(
            before,
            s.offset_ms(),
            "an implausible sample must be discarded, not folded in"
        );
    }

    /// Closed loop, as every client wires it: observe per packet, hand `desired_depth` to the
    /// policy. Video sits a steady `early_ms` behind the ring's own audio. Once the ring is deep
    /// enough the offset enters the deadband; the loop must hold there, not hunt.
    #[test]
    fn sync_steering_settles_instead_of_hunting() {
        let pm = per_ms(2);
        let want = 5 * pm;
        for early_ms in [40u64, 60, 100] {
            let mut p = JitterPolicy::new(JitterTuning::PIPEWIRE, 2);
            let mut av = AvSync::new(2);
            let (mut depth, mut sheds, mut inserts, mut trims) = (0usize, 0u32, 0u32, 0u32);
            for cb in 0..24_000u32 {
                depth += want;
                // Video end-to-end is fixed; only the ring moves the audio side.
                av.observe(AvSyncObservation {
                    video_e2e_ns: Some((40 + early_ms) * 1_000_000),
                    ..obs(0, depth, pm)
                });
                p.set_sync_target(av.desired_depth(depth));
                let s = p.step(depth, want);
                depth -= s.drop_front.min(depth);
                depth += s.insert_front;
                // Converging costs a handful of inserts; count only what follows the first minute.
                if cb >= 12_000 {
                    sheds += (s.drop_front > 0 && !s.hard_trim) as u32;
                    trims += s.hard_trim as u32;
                    inserts += (s.insert_front > 0) as u32;
                }
                let short = !s.silence && depth < want;
                if !s.silence {
                    depth -= want.min(depth);
                }
                p.note_read(short);
            }
            assert_eq!(
                (sheds, trims, inserts),
                (0, 0, 0),
                "{early_ms} ms early: still hunting in the second minute (sheds, trims, inserts)"
            );
        }
    }
}
