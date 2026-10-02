import AVFoundation
import os

// MARK: - the ms ⇄ interleaved-sample conversion both the ring and the sync loop run on
//
// **Multiply first, divide last.** This is the whole of design/hi-res-audio.md §4.1, and it is the
// Swift half of the same fix core just took (`punktfunk_core::audio::ms_to_samples`). Both types
// below used to precompute `perMS = (rateHz / 1000) * channels` and express every figure they own
// as `ms * perMS`. That division happens FIRST, so 44 100 Hz became 44 samples per millisecond and
// every depth, target, shed threshold, hard cap, de-prime fuse and reported `bufferedMS`/`targetMS`
// came out **2.3 % low** — quietly, permanently, and in the one subsystem two previous programs
// spent their time making trustworthy. 48 000 and 96 000 were exact only because they happen to
// divide.
//
// Keeping `rateHz` and `channels` as the two numbers they are, and dividing last, is exact at every
// rate on the ladder (`pcm::rate_is_supported`: 44 100 / 48 000 / 88 200 / 96 000 / 176 400). It
// costs one integer division per conversion and buys three more rates.
//
// **Why `Int` is enough here, where core needed an explicit `u64`.** Core's conversions run against
// `usize`, which is 32 bits on some embedder targets, so they widen and saturate by hand. This
// package builds for macOS 14 / iOS 17 / tvOS 17 only (Package.swift) — arm64 and x86_64, where
// `Int` is 64-bit — and the largest product any caller can reach is the longest span this file
// names against the top of the ladder: `syncBackoffMaxMS` (480 000 ms) × 176 400 Hz × 8 ch =
// 6.8 × 10¹¹, forty bits, before the divide brings it back to 6.8 × 10⁸. That is five orders of
// magnitude inside `Int.max`. The samples → ms direction is the one that takes a caller-supplied
// count and is guarded, because Swift TRAPS on overflow rather than wrapping and that trap would
// land in a realtime render callback.

/// Interleaved samples per second at a negotiated layout — the denominator both conversions share.
/// `max(1)` on both: a degenerate layout must not divide by zero in a render callback.
func audioInterleavedPerSec(rateHz: Int, channels: Int) -> Int {
    max(rateHz, 1) * max(channels, 1)
}

/// `ms` milliseconds of audio, in interleaved samples. Mirrors `ms_to_samples`.
func audioMsToSamples(rateHz: Int, channels: Int, ms: Int) -> Int {
    ms * audioInterleavedPerSec(rateHz: rateHz, channels: channels) / 1_000
}

/// Interleaved samples back to whole milliseconds — the exact inverse of [`audioMsToSamples`], and
/// the reason `depthMS(target)` round-trips to `targetMS` at every rate on the ladder. §4.1 names
/// that round trip as the tell that this rework is incomplete, so it is also the shape of the test
/// that guards it (`testTheShippingRateLadderRoundTripsMsToSamplesExactly`).
///
/// `samples` arrives from a caller and nothing bounds it — `setSyncTarget(Int.max / 2)` is a real
/// call this file's own tests make — and `samples * 1_000` on that would TRAP, taking the process
/// down from wherever it was asked (the drain thread, or the render callback). Core widens to u128
/// for the same reason; Swift has no u128, so the multiply reports its overflow and saturates.
/// Saturating rather than wrapping, because a wrapped duration is a tiny one: a fuse that blows
/// instantly instead of one that never blows.
func audioSamplesToMs(rateHz: Int, channels: Int, samples: Int) -> Int {
    let (scaled, overflow) = samples.multipliedReportingOverflow(by: 1_000)
    guard !overflow else { return Int.max }
    return scaled / audioInterleavedPerSec(rateHz: rateHz, channels: channels)
}

/// Interleaved samples in one `frameUs` frame — **per channel × channels**. Mirrors
/// `punktfunk_core::audio::pcm::samples_per_frame`, which is the single source of truth for how
/// long a frame is: the host fills a buffer of this size and this ring drains one, so the two agree
/// by construction rather than by both re-deriving `rate × µs` and hoping they round the same way.
///
/// ⚠ **Not the same question as "how many samples is `frameUs` of audio", and at 44.1 kHz not the
/// same answer.** The divide is per channel and FLOORS, because 220.5 samples do not exist: 5 ms of
/// 44.1 kHz stereo audio is 441 interleaved samples, but a 5 ms FRAME of it carries 440. Both the
/// shed size and the near-miss margin mean *exactly one packet*, so computing the first where the
/// wire delivers the second would describe a packet that does not exist. Multiply first here too —
/// `rateHz / 1_000_000` is 0 for every rate below a megahertz.
func audioSamplesPerFrame(rateHz: Int, frameUs: Int, channels: Int) -> Int {
    (max(rateHz, 1) * max(frameUs, 0) / 1_000_000) * max(channels, 1)
}

/// SPSC-ish jitter ring (interleaved float, `channels` per frame), drain thread → render
/// callback, under an unfair lock held for microseconds. `JitterPolicy` decides each callback:
/// priming, drift sheds, cap trims, sync inserts. The ring applies it, crossfading every seam.
/// All counts stay whole frames, so the interleave can never slip.
final class AudioRing: @unchecked Sendable {
    private var buf: [Float]
    private var readIdx = 0
    private var writeIdx = 0
    private var policy: JitterPolicy
    /// Reported, not acted on: short reads that starved the callback, smooth drift sheds, and
    /// sync-driven inserts. Concealment in either direction must be visible.
    private var underrunCount = 0
    private var shedCount = 0
    private var insertCount = 0
    /// The sync loop's smoothed offset in ms, stored for reporting: the drain thread has the
    /// timestamps, the ring has the depth, and a stats line reads both under one lock.
    private var avOffsetMS = 0
    /// Device output latency behind the ring, ns (`noteOutputLatency`).
    private var outputLatencyNsValue: Int64 = 0
    /// Drought concealment the drain thread has synthesized this session, ms (`notePlcMS`).
    private var plcMS = 0
    private let channels: Int
    private let rateHz: Int
    private let lock = OSAllocatedUnfairLock()

    /// A ring holding `seconds` of audio at the negotiated format, sized in time: a sample count
    /// is `rateHz × channels`, and 96 kHz must not get half the headroom 48 kHz does. Zero rates
    /// and channel counts clamp to 1 rather than fault in a render callback.
    init(seconds: Int, channels: Int, rateHz: Int) {
        self.channels = max(channels, 1)
        self.rateHz = max(rateHz, 1)
        buf = [Float](repeating: 0, count: max(seconds, 1) * self.rateHz * self.channels)
        policy = JitterPolicy(channels: self.channels, rateHz: self.rateHz)
    }

    /// The negotiated frame length (`punktfunk_connection_audio_frame_us`). The shed, the
    /// near-miss margin and the priming lift are one frame; left at the 5 ms default, a 96 kHz
    /// session would shed two and a half. Idempotent, so an engine rebuild may call it again.
    func setFrameUs(_ us: Int) {
        lock.lock()
        defer { lock.unlock() }
        policy.setFrameUs(us)
    }

    /// The frame-denominated quantities, taken under the lock — for `AudioRingDriftTests`.
    var frameGeometry: (frame: Int, crossfade: Int) {
        lock.lock()
        defer { lock.unlock() }
        return (policy.frameSamples, policy.crossfadeSamples)
    }

    /// Hand the ring the depth the A/V sync loop wants (`AvSync.desiredDepth`), in interleaved
    /// samples, or `nil` to run unsynchronised. A request, clamped by the policy.
    func setSyncTarget(_ samples: Int?) {
        lock.lock()
        defer { lock.unlock() }
        policy.setSyncTarget(samples)
    }

    /// Store the sync loop's smoothed A/V offset for reporting (positive = audio behind the
    /// picture).
    func noteAvOffset(_ ms: Int) {
        lock.lock()
        defer { lock.unlock() }
        avOffsetMS = ms
    }

    /// What the output device adds after a sample leaves the ring (HAL latency, a Bluetooth
    /// link). Set by the engine side on every start; the drain counts it as audio already queued.
    func noteOutputLatency(ns: Int64) {
        lock.lock()
        defer { lock.unlock() }
        outputLatencyNsValue = max(0, ns)
    }

    var outputLatencyNs: Int64 {
        lock.lock()
        defer { lock.unlock() }
        return outputLatencyNsValue
    }

    /// Store the drain thread's running drought concealment (`DroughtConceal.totalMS`). A
    /// healthy `underruns` bought with a climbing `plc_ms` is a link in trouble.
    func notePlcMS(_ ms: Int) {
        lock.lock()
        defer { lock.unlock() }
        plcMS = ms
    }

    /// Buffered depth in interleaved samples — what the sync loop measures against.
    var bufferedSamples: Int {
        lock.lock()
        defer { lock.unlock() }
        return writeIdx - readIdx
    }

    /// Queue decoded audio. On overflow the oldest goes; every other trim is the policy's, on
    /// the read side.
    func write(_ samples: UnsafePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        let capacity = buf.count
        // One write past the whole ring would push readIdx past writeIdx.
        guard count <= capacity else { return }
        if writeIdx + count - readIdx > capacity {
            readIdx = writeIdx + count - capacity
        }
        for i in 0..<count {
            buf[(writeIdx + i) % capacity] = samples[i]
        }
        writeIdx += count
    }

    /// Fill `out` completely: silence while priming, and beyond what is buffered.
    func read(into out: UnsafeMutablePointer<Float>, count: Int) {
        lock.lock()
        defer { lock.unlock() }
        let step = policy.step(depth: writeIdx - readIdx, want: count)
        if step.dropFront > 0 {
            dropFront(step.dropFront, fade: step.crossfade)
            if !step.hardTrim { shedCount += 1 }
        }
        if step.insertFront > 0 {
            insertFront(step.insertFront, fade: step.crossfade)
            insertCount += 1
        }
        let n = step.silence ? 0 : min(writeIdx - readIdx, count)
        let capacity = buf.count
        for i in 0..<n {
            out[i] = buf[(readIdx + i) % capacity]
        }
        readIdx += n
        for i in n..<count { out[i] = 0 }
        let ranShort = !step.silence && n < count
        if ranShort { underrunCount += 1 }
        policy.noteRead(ranShort: ranShort)
    }

    /// Drop `drop` interleaved samples from the front, crossfading the seam over `fade`
    /// (core's `crossfade_drop`). The fade-out is the head of what goes, the continuation of
    /// the sample just played. Caller holds the lock.
    private func dropFront(_ drop: Int, fade: Int) {
        let available = writeIdx - readIdx
        guard drop > 0, available >= drop else { return }
        let fade = min(fade, min(drop, available - drop))
        let capacity = buf.count
        for i in 0..<fade {
            let old = buf[(readIdx + i) % capacity]
            let new = buf[(readIdx + drop + i) % capacity]
            let t = Float(i + 1) / Float(fade + 1)
            buf[(readIdx + drop + i) % capacity] = old * (1 - t) + new * t
        }
        readIdx += drop
    }

    /// Duplicate the first `insert` interleaved samples at the front, crossfading the seam over
    /// `fade` (core's `crossfade_insert`). Caller holds the lock.
    ///
    /// The copy lands in the `insert` slots before `readIdx`, free when the ring has that much
    /// spare capacity. When `readIdx` is too small to step back, both indices shift up one whole
    /// capacity first: every position they name is unchanged and neither goes negative.
    private func insertFront(_ insert: Int, fade: Int) {
        let available = writeIdx - readIdx
        let capacity = buf.count
        guard insert > 0, available >= insert, available + insert <= capacity else { return }
        let fade = min(fade, min(insert, available - insert))
        if readIdx < insert {
            readIdx += capacity
            writeIdx += capacity
        }
        for i in 0..<insert {
            buf[(readIdx - insert + i) % capacity] = buf[(readIdx + i) % capacity]
        }
        // The seam: what would have followed the copy fades into the original's head.
        for i in 0..<fade {
            let old = buf[(readIdx + insert + i) % capacity]
            let new = buf[(readIdx + i) % capacity]
            let t = Float(i + 1) / Float(fade + 1)
            buf[(readIdx + i) % capacity] = old * (1 - t) + new * t
        }
        readIdx -= insert
    }

    /// Current buffered depth in milliseconds — for the stats overlay and the drain's log.
    var bufferedMS: Int {
        lock.lock()
        defer { lock.unlock() }
        return audioSamplesToMs(rateHz: rateHz, channels: channels, samples: writeIdx - readIdx)
    }

    /// One consistent snapshot of the ring's vitals, under a single lock.
    struct Stats {
        let bufferedMS: Int
        /// The depth the ring aims for, sync request and device quantum included.
        let targetMS: Int
        let underruns: Int
        let sheds: Int
        /// Sync-driven inserts, one duplicated frame each: the shed's other direction.
        let inserts: Int
        /// The A/V sync loop's smoothed offset (ms): positive = audio behind the picture. `0`
        /// before the loop has evidence, or with sync off.
        let avOffsetMS: Int
        /// Audio synthesized for packet droughts this session (`DroughtConceal`), ms.
        let plcMS: Int
    }

    var stats: Stats {
        lock.lock()
        defer { lock.unlock() }
        return Stats(
            bufferedMS: audioSamplesToMs(
                rateHz: rateHz, channels: channels, samples: writeIdx - readIdx),
            targetMS: policy.effectiveTargetMS,
            underruns: underrunCount,
            sheds: shedCount,
            inserts: insertCount,
            avOffsetMS: avOffsetMS,
            plcMS: plcMS)
    }
}

// MARK: - A/V sync

/// The A/V synchronisation controller: turns "when will this audio actually play" and "when did
/// the picture it belongs with reach the glass" into a ring depth `AudioRing` should aim for.
/// The Swift twin of `punktfunk_core::audio::AvSync`, held to it by `jitter-vectors.json`.
///
/// **The defect it exists to fix.** The host stamps `pts_ns` on every audio datagram and the
/// client decoded it into `AudioPCM` — and then never read it. Video's `pts_ns`, by contrast, is
/// used end to end (`LatencyMeter` computes a true glass-to-glass `displayed + clockOffset − pts`
/// per presented frame). So audio free-ran at whatever depth its jitter ring happened to settle
/// at, video was presented on a wholly independent path, and nothing ever compared them: the A/V
/// offset was an accident of buffer depths. It moved whenever the ring ratcheted under underrun
/// pressure, and — the way this surfaced in the field — it got WORSE every time video got faster,
/// because a quicker decoder lowers the video leg while leaving the audio leg exactly where it was.
///
/// **Video is the master.** In a game streamer the video leg is the input-feel budget and must
/// never be inflated to satisfy the audio clock; audio tolerates small, crossfaded, rate-limited
/// corrections that are inaudible, and `JitterPolicy` already applies them. So audio moves.
///
/// **Continuity outranks sync.** This type only ever PROPOSES a depth. `JitterPolicy` clamps the
/// proposal to its own underrun-driven floor, so a link whose jitter
/// genuinely needs more buffer than the picture is away keeps its buffer and the residual is
/// reported instead of being taken out of the listener's stream.
///
/// Not a class and not locked: it is owned outright by the drain thread that observes packets.
struct AvSync {
    /// Smoothing time constant for the measured offset, in ms of consumed audio. Long enough that
    /// network jitter and a single late datagram do not move it; short enough to track real drift.
    private static let ewmaTauMS = 2_000
    /// Offsets inside this band are left alone. Correcting a few ms costs a (crossfaded, but real)
    /// discontinuity and buys nothing a listener can perceive — detectability for A/V misalignment
    /// sits an order of magnitude above it. The deadband is what keeps the loop from hunting
    /// forever around zero, which would be audible in a way the misalignment it chased was not.
    static let deadbandMS = 10
    /// Audio observed before the first correction is offered, in `frameMS` frames (500 ms). The
    /// offset is derived from a clock skew estimate and a video figure that both need a moment to
    /// settle after connect; acting on the first sample would chase the handshake, not the stream.
    private static let minObservations = 100
    /// An offset larger than this is not believed. A wall-clock step, a paused host, or a stale
    /// video figure can all produce an enormous apparent misalignment, and steering the ring by it
    /// would empty or overfill it outright. Beyond this the loop reports and waits rather than acts.
    private static let saneLimitMS = 1_000
    /// The protocol's default frame, in ms. `init` takes the negotiated one.
    private static let frameMS = JitterPolicy.frameMS

    /// The negotiated layout, in the same two numbers `AudioRing` keeps and for the same reason —
    /// this type's proposal is denominated in the ring's own units, so the two have to agree about
    /// what a millisecond is down to the sample.
    private let rateHz: Int
    private let channels: Int
    /// EWMA of the measured offset in ns. Positive = audio is scheduled to play LATE relative to
    /// the picture it belongs with.
    private var offsetAvgNs: Float = 0
    private var observations = 0
    /// Audio the observations cover: one frame each.
    private var observedUs = 0
    /// One observation's frame, for the EWMA weight. The caller observes once per packet, so
    /// weights are in audio time, not calls.
    private let frameUs: Int
    /// Set once an observation lands outside `saneLimitMS`, for reporting.
    private(set) var implausible = false
    /// Last depth offered outside the deadband; what the deadband keeps asking for.
    private var held: Int?

    /// `channels` is the negotiated interleaved channel count (2/6/8), `rateHz` the negotiated
    /// sample rate — every rate on the lossless ladder, exactly, for the reason `AudioRing.init`
    /// gives: the ms ⇄ sample conversion multiplies before it divides, so the 44.1 kHz family is
    /// representable here too and the depth this type proposes lands in the units the ring measures
    /// itself in. Mirrors `AvSync::new_at_rate`.
    init(channels: Int, rateHz: Int, frameUs: Int = frameMS * 1_000) {
        self.rateHz = max(rateHz, 1)
        self.channels = max(channels, 1)
        self.frameUs = max(frameUs, 1)
    }

    /// Interleaved samples to whole milliseconds — see `audioSamplesToMs`.
    private func samplesMs(_ samples: Int) -> Int {
        audioSamplesToMs(rateHz: rateHz, channels: channels, samples: samples)
    }

    /// One measurement handed to `observe`. Every field is in the units its source already
    /// produces, so no caller has to do clock arithmetic to use it correctly.
    struct Observation {
        /// The host capture timestamp carried by the audio frame being queued (host clock).
        let ptsNs: UInt64
        /// Local `CLOCK_REALTIME` now — the same basis `LatencyMeter` stamps video in.
        let nowLocalNs: Int64
        /// Host clock minus client clock, from the skew handshake (`clockOffsetNs`).
        ///
        /// It very nearly CANCELS: the video figure this is differenced against was computed with
        /// the same offset and the same sign, so as long as both terms use one value the skew
        /// drops out of the result entirely. That is what makes the connect-time offset good
        /// enough here even though the absolute legs would prefer a re-synced one.
        let clockOffsetNs: Int64
        /// How much audio is already queued AHEAD of this frame, in interleaved samples —
        /// everything that must play before it does.
        let bufferedAhead: Int
        /// Device output latency past the ring, ns (`AudioRing.outputLatencyNs`). 0 = unknown.
        var outputLatencyNs: Int64 = 0
        /// The video plane's current end-to-end figure in ns: `displayed + clockOffset − pts`, as
        /// `LatencyMeter` already computes it per presented frame. `nil` while nothing has reached
        /// the glass recently — no reference, no correction.
        let videoE2eNs: Int64?
    }

    /// Fold one measurement. Returns the smoothed offset in ns once there is enough evidence to
    /// believe it (positive = audio late), or `nil` while still settling.
    ///
    /// Rejecting the implausible rather than clamping it is deliberate: a wall-clock step or a
    /// stale video figure produces a huge apparent offset, and a clamped-but-wrong value would be
    /// acted on as though it were a small real one.
    @discardableResult
    mutating func observe(_ o: Observation) -> Int64? {
        // No frame on the glass yet ⇒ no reference to align against, so nothing to say.
        guard let videoE2eNs = o.videoE2eNs else { return nil }
        // When these samples reach the speaker, in the host's capture clock like the video
        // figure. Buffered audio counts in whole ms, as core counts it: the ≤ 1 ms dropped is
        // well inside `deadbandMS`.
        let bufferedNs = Int64(samplesMs(o.bufferedAhead)) * 1_000_000 + max(0, o.outputLatencyNs)
        // Overflow-reporting, never wrapping: a garbage `pts_ns` must not wrap round into a
        // small, plausible offset. An overflow takes the sanity limit's exit.
        let (playAtLocal, o1) = o.nowLocalNs.addingReportingOverflow(bufferedNs)
        let (playAtHost, o2) = playAtLocal.addingReportingOverflow(o.clockOffsetNs)
        let (audioE2eNs, o3) = playAtHost.subtractingReportingOverflow(Int64(bitPattern: o.ptsNs))
        let (offsetNs, o4) = audioE2eNs.subtractingReportingOverflow(videoE2eNs)
        guard !o1, !o2, !o3, !o4, abs(offsetNs) <= Int64(Self.saneLimitMS) * 1_000_000 else {
            implausible = true
            return nil
        }
        implausible = false

        // `Float`, like core's `f32`, so both sides round the same way.
        let alpha = min(max(Float(frameUs) / Float(Self.ewmaTauMS * 1_000), 0), 1)
        if observations == 0 {
            offsetAvgNs = Float(offsetNs)
        } else {
            offsetAvgNs += (Float(offsetNs) - offsetAvgNs) * alpha
        }
        observations += 1
        observedUs += frameUs
        return settled ? Int64(offsetAvgNs) : nil
    }

    /// Enough evidence folded to act on.
    var settled: Bool { observedUs >= Self.minObservations * Self.frameMS * 1_000 }

    /// The smoothed offset in ms (positive = audio late), for the HUD. Reported as soon as it is
    /// measured, including while still settling — a number the operator can watch converge is more
    /// useful than a blank that hides whether the loop is working at all.
    var offsetMS: Int { Int(offsetAvgNs / 1_000_000) }

    /// The ring depth that would place audio with the picture, given where the ring is now.
    /// `nil` while unsettled: the caller runs unsynchronised. Inside the deadband, the last
    /// request again: a ring that reached its depth stays there, where `nil` would drop it to
    /// the floor and shed what the insert just built.
    ///
    /// Audio late (offset > 0) means there is too much queued: aim shallower. Audio early means
    /// aim deeper.
    mutating func desiredDepth(currentDepth: Int) -> Int? {
        guard settled else {
            held = nil
            return nil
        }
        let offsetMs = offsetAvgNs / 1_000_000
        guard abs(offsetMs) >= Float(Self.deadbandMS) else { return held }
        // One ms of samples as a float, the constant divided rather than the product: one
        // rounding, so 48 kHz stays exactly 96.0 and 44.1 kHz stereo is 88.2.
        let perMs = Float(audioInterleavedPerSec(rateHz: rateHz, channels: channels)) / 1_000
        let delta = Int(offsetMs * perMs)
        held = max(0, currentDepth - delta)
        return held
    }
}

// MARK: - Drought concealment

/// Bounded concealment of a packet DROUGHT — the Apple leg of the policy the three Rust clients
/// share (`punktfunk_core::audio::DroughtConceal`; design/host-source-stutter-fixes.md, WP-C1).
///
/// The decode path already conceals a SEQ GAP: core's in-ABI decoder synthesizes the packets the
/// sequence says went missing before the one that arrived (`nextAudioPcm`). But that only fires
/// when a LATER packet arrives to reveal the gap. When the wire simply goes quiet — a delivery
/// stall on a bunching Wi-Fi link, or a host whose capture stalled — nothing arrives to reveal
/// anything: `AudioRing` drains to empty, the render callback runs short, and `noteRead` de-primes
/// and then re-primes a whole target's worth of fresh silence. The artifact is far longer than the
/// audio actually missing, and this is the shape the 2026-08-15 field session spent 3–16 % of its
/// wall-clock in.
///
/// So a drought that is draining the ring gets concealed too, from the same decoder state
/// (`PunktfunkConnection.audioPlc`), for a bounded time. The BOUND is denominated in TIME, never in
/// frames or callbacks: that is the recorded lesson from the very fuse this protects, where a count
/// gave an iPad a third of a Mac's slack (`JitterPolicy.deprimeMS`, and
/// `testDeprimeFuseIsADurationNotACallbackCount`). What it COUNTS is frames — one per synthesized
/// packet, which is what the drain thread actually produces — and the resolved frame length is what
/// converts between the two. Those are the same discipline, not opposite ones: the policy is stated
/// in time and the conversion is exact, instead of a frame being assumed to be 5 ms.
///
/// Time is passed IN, so the policy stays as deterministic as the ring's own.
struct DroughtConceal {
    /// Frames concealed since the last real packet. Counted in FRAMES rather than milliseconds
    /// because that is what the drain thread actually does — one `audioPlc()` frame per `conceal()`
    /// that says yes — and because the frame is no longer a fixed 5 ms; see `init(maxMS:frameUs:)`.
    private var concealed = 0
    private let maxMS: Int
    /// One frame, in MICROSECONDS. Everything time-denominated here derives from it.
    private let frameUs: Int
    /// Concealed over the session, in FRAMES.
    private var total = 0

    /// At the protocol's default frame (`JitterPolicy.frameMS`) — every Opus session, and every test
    /// that pins the pre-hi-res numbers.
    init(maxMS: Int) {
        self.init(maxMS: maxMS, frameUs: JitterPolicy.frameMS * 1_000)
    }

    /// At an explicitly negotiated frame length (`punktfunk_connection_audio_frame_us`).
    ///
    /// This type charges one frame per concealed frame and bounds itself in WALL-CLOCK
    /// milliseconds, so both must agree how long a frame is: a 2 ms lossless frame costed at 5 ms
    /// spends the budget in two fifths of the time it promises and over-reports `plc_ms` by the
    /// same factor. Mirrors `DroughtConceal::new_at_frame_us`.
    init(maxMS: Int, frameUs: Int) {
        self.maxMS = maxMS
        self.frameUs = max(frameUs, 1)
    }

    /// How long a drought must last before it is concealed at all — TWO FRAMES, so an ordinary
    /// inter-packet gap is never mistaken for a stall. It was a fixed `2 × frameMS`, which on a 2 ms
    /// lossless frame waits five frames instead of two before conceding there is a stall.
    ///
    /// ⚠ In whole milliseconds, because that is the granularity the caller measures the quiet wire
    /// at (core compares `Duration`s in µs). Every rung on the ladder is a multiple of 500 µs, so
    /// `2 × frameUs` is always a whole number of ms and nothing truncates; the floor of 1 exists
    /// only so a degenerate `frameUs` cannot produce a zero-length tolerance, which would conceal
    /// ordinary jitter as though it were a stall.
    private var afterMS: Int { max(2 * frameUs / 1_000, 1) }

    /// Ring depth below which a drought is worth concealing, in ms — also two frames. A drought a
    /// deep ring can cover is not audible, and concealing it would synthesize audio the late packets
    /// are about to duplicate, pushing the whole stream later and handing the drift shed a mess to
    /// clean up audibly. Rounds UP, like core's `div_ceil`, so the floor is never *less* than the
    /// two frames it promises.
    private var floorMS: Int { (2 * frameUs + 999) / 1_000 }

    /// Concealed since the last real packet, in ms — the figure the `maxMS` budget bounds.
    private var concealedMS: Int { concealed * frameUs / 1_000 }

    /// Concealment over the session, ms — what the 10 s `plc_ms=` line reports. Concealment must be
    /// visible: a policy that quietly papers over a failing link is a policy that hides the bug.
    var totalMS: Int { total * frameUs / 1_000 }

    /// A packet arrived, ending any drought — the next one starts from a full budget.
    ///
    /// Nothing to divide: the run is a frame count, so ending it is one assignment. The Rust twin
    /// hands that count BACK, for its caller to subtract from the loss concealment the seq path is
    /// about to ask for. Here that subtraction is core's, on the far side of the ABI, because that
    /// is where the gap tracker lives (see `punktfunk_connection_audio_plc`) — a packet genuinely
    /// lost inside a covered drought must not be concealed twice either way.
    mutating func packet() {
        concealed = 0
    }

    /// Should one more frame be concealed? `depthMS` is the playout ring as the render callback
    /// last left it.
    mutating func conceal(sinceLastPacketMS: Int, depthMS: Int) -> Bool {
        if sinceLastPacketMS < afterMS || depthMS > floorMS || concealedMS >= maxMS {
            return false
        }
        concealed += 1
        total += 1
        return true
    }
}

/// CoreAudio channel layout for the canonical wire order FL FR FC LFE RL RR [SL SR]. nil for
/// stereo (the standard layout is correct). For 5.1/7.1 we list explicit channel labels via
/// `kAudioChannelLayoutTag_UseChannelDescriptions` — preset tags (DTS_5_1 etc.) don't reliably
/// match Moonlight's order. 7.1 follows `kAudioChannelLayoutTag_WAVE_7_1` (WASAPI 0x63F):
/// wire 4-5, the WAVE *back* pair, are RearSurround*; 6-7, the *side* pair, are Left/RightSurround.
/// 5.1 keeps Left/RightSurround for its back pair: that is where a 5.1 device has speakers.
func wireChannelLayout(channels: Int) -> AVAudioChannelLayout? {
    let labels: [AudioChannelLabel]
    switch channels {
    case 6:
        labels = [
            kAudioChannelLabel_Left, kAudioChannelLabel_Right, kAudioChannelLabel_Center,
            kAudioChannelLabel_LFEScreen, kAudioChannelLabel_LeftSurround,
            kAudioChannelLabel_RightSurround,
        ]
    case 8:
        labels = [
            kAudioChannelLabel_Left, kAudioChannelLabel_Right, kAudioChannelLabel_Center,
            kAudioChannelLabel_LFEScreen,
            kAudioChannelLabel_RearSurroundLeft, kAudioChannelLabel_RearSurroundRight, // wire RL/RR (back)
            kAudioChannelLabel_LeftSurround, kAudioChannelLabel_RightSurround, // wire SL/SR (side)
        ]
    default:
        return nil
    }
    let size = MemoryLayout<AudioChannelLayout>.size
        + (labels.count - 1) * MemoryLayout<AudioChannelDescription>.stride
    let raw = UnsafeMutableRawPointer.allocate(byteCount: size, alignment: 16)
    defer { raw.deallocate() }
    let layout = raw.bindMemory(to: AudioChannelLayout.self, capacity: 1)
    layout.pointee.mChannelLayoutTag = kAudioChannelLayoutTag_UseChannelDescriptions
    layout.pointee.mChannelBitmap = AudioChannelBitmap(rawValue: 0)
    layout.pointee.mNumberChannelDescriptions = UInt32(labels.count)
    // `mChannelDescriptions` is the C variable-length tail array (declared `[1]`, over-allocated
    // above). Scope the pointer with `withUnsafeMutablePointer` — taking `&…mChannelDescriptions`
    // inline yields a pointer valid only for that expression, so building a buffer from it that
    // outlives the call is a dangling-pointer bug. Inside the closure it stays valid while we fill it.
    withUnsafeMutablePointer(to: &layout.pointee.mChannelDescriptions) { tail in
        let descs = UnsafeMutableBufferPointer(start: tail, count: labels.count)
        for (i, lbl) in labels.enumerated() {
            descs[i] = AudioChannelDescription(
                mChannelLabel: lbl, mChannelFlags: AudioChannelFlags(rawValue: 0),
                mCoordinates: (0, 0, 0))
        }
    }
    return AVAudioChannelLayout(layout: layout)
}
