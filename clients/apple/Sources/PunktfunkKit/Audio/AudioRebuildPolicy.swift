// The two policy decisions of the device-change recovery, where a unit test can reach them
// without a mic or a session: a voice-processing start that failed on an input device is not
// retried on it, and rebuilds that keep retriggering each other back off. The wiring lives in
// `SessionAudio`.

#if os(macOS)
import CoreAudio
#endif
import Foundation

#if os(macOS)
/// Should a rebuild try the combined (voice-processing) topology again?
///
/// A VPIO start failure is a property of the INPUT DEVICE (its channel count and format), not of
/// the moment: retrying it on the same device fails the same way, and the attempt is not free —
/// engaging and abandoning the voice processor churns the HAL hard enough to stop the healthy
/// fallback engines. So a failure latches until the default input actually changes; a new device
/// earns exactly one fresh attempt (it may well support VPIO), and its own failure latches again.
struct CombinedTopologyGate {
    private var failed = false
    /// The default input device the failure was observed on — nil is a real value here ("failed
    /// with no resolvable input device"), which is why `failed` is tracked separately.
    private var failedInput: AudioDeviceID?

    /// The combined topology failed with `input` as the default input device.
    mutating func noteFailure(input: AudioDeviceID?) {
        failed = true
        failedInput = input
    }

    /// True when the combined topology is worth attempting with `input` as the default input
    /// device. A device change clears the latch — the answer is about the CURRENT hardware, and
    /// coming back to a device that failed before earns a fresh attempt too (the failure may have
    /// been the mid-transition kind, and one attempt per device change cannot loop).
    mutating func shouldTry(input: AudioDeviceID?) -> Bool {
        guard failed else { return true }
        guard input == failedInput else {
            failed = false
            failedInput = nil
            return true
        }
        return false
    }
}
#endif

/// The delay before the next engine rebuild — the base debounce/floor behaviour, plus an
/// escalating floor when rebuilds CHAIN (each one retriggered by its predecessor's own fallout).
///
/// One device switch produces one rebuild: its trigger burst is coalesced upstream, so the next
/// trigger normally arrives minutes later and gets the base floor. A trigger that arrives hard on
/// the heels of the last rebuild, again and again, is a rebuild answering itself — and since the
/// recovery cannot always identify its own echo, the backstop is to keep answering but at a
/// doubling floor, so an unforeseen feedback shape costs one audio blip per half-minute instead
/// of a metronome. A quiet stretch resets the ladder to full responsiveness.
struct RebuildBackoff {
    /// Let the burst of triggers from one switch land before rebuilding.
    static let debounce: TimeInterval = 0.15
    /// Floor between two rebuilds.
    static let floor: TimeInterval = 0.5
    /// The escalated floor's cap: looping recoveries settle at one attempt per this interval.
    static let floorCap: TimeInterval = 30
    /// A trigger this long after the last rebuild is unrelated to it — the chain resets.
    static let chainWindow: TimeInterval = 10

    /// Consecutive rebuilds whose trigger arrived within `chainWindow` of the previous rebuild.
    private(set) var chain = 0
    private var lastRebuildAt: TimeInterval = -.infinity

    /// The delay to schedule the next rebuild with, for a trigger arriving at `now`
    /// (`systemUptime`). Mutates the chain accounting: call once per SCHEDULED rebuild, not per
    /// coalesced trigger.
    mutating func delay(now: TimeInterval) -> TimeInterval {
        let since = now - lastRebuildAt
        chain = since < Self.chainWindow ? chain + 1 : 0
        let floor = min(Self.floor * pow(2, Double(min(chain, 6))), Self.floorCap)
        return max(Self.debounce, floor - since)
    }

    /// The rebuild actually ran at `now` — the reference the next trigger's `delay` measures from.
    mutating func noteRebuild(at now: TimeInterval) {
        lastRebuildAt = now
    }
}
