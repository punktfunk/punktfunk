// Which present the macOS adaptive-panel latency path issues: one in flight issued at once while
// the layer scans out Direct, the display-link slot while the window server composites it.

#if canImport(Metal) && canImport(QuartzCore)
import Foundation
import QuartzCore

/// The adaptive-panel present mode. `direct` issues a frame at once once the previous present
/// has flipped; `composited` books each on the display-link slot (`AdaptiveSlotRegime`).
///
/// Direct scanout shows a present ~2 ms after issue and calls back within a panel refresh, so one
/// in flight costs nothing and never lands a second present mid-refresh. A composited window holds
/// each present ~1.6 refreshes, at any content rate: at the panel's rate one in flight shows two
/// frames in three, so the slot shows them all, about a refresh later; at half the rate every
/// frame still gets out, a refresh sooner than the slot.
///
/// So a present's hold (issue → callback) against the content interval decides: a median of the
/// last `window` above `blendedHold` intervals switches to `composited`, whose booked presents may
/// keep the window composited themselves. While the window is fullscreen (`setDirectPossible`),
/// `composited` probes `direct` again after `backoff`; a probe that fails within `probeSpan`
/// doubles the backoff, up to `maxBackoff`. A window that is not fullscreen is probed once per
/// state change, since each failed probe costs `window` frames. Lock-guarded: the render thread
/// issues, Metal callback threads report. Internal for tests.
final class ScanoutPacer: @unchecked Sendable {
    enum Mode: Equatable { case direct, composited }

    static let window = 12
    static let blendedHold = 1.3
    static let baseBackoff: CFTimeInterval = 2
    static let maxBackoff: CFTimeInterval = 30
    static let probeSpan: CFTimeInterval = 1
    /// A present whose callback has not come in this long is presumed lost (`PresentGate`).
    static let staleAfter: CFTimeInterval = 0.1

    private struct Present {
        let at: CFTimeInterval
        let direct: Bool
    }

    private let lock = NSLock()
    private var current = Mode.direct
    /// The presents in flight, oldest first, whichever mode issued them.
    private var inFlight: [Present] = []
    private var holds: [CFTimeInterval] = []
    /// The last mode switch; never before the first, so creation does not start a probe span.
    private var switchedAt = -CFTimeInterval.infinity
    private var backoff = ScanoutPacer.baseBackoff
    private var switches = 0
    /// The window is fullscreen; `setDirectPossible` keeps it.
    private var directPossible = true

    /// The mode for a frame issued now; a due probe turns `composited` into `direct` here.
    func mode(now: CFTimeInterval) -> Mode {
        lock.lock()
        defer { lock.unlock() }
        if current == .composited, directPossible, now - switchedAt >= backoff {
            switchLocked(to: .direct, now: now)
        }
        return current
    }

    /// Whether a Direct frame may be issued now. A Direct present counts as flipped one panel
    /// refresh (`minInterval`) after issue even without its callback; a booked slot present
    /// holds until its callback, so a Direct frame never overtakes it.
    func mayIssue(now: CFTimeInterval, minInterval: CFTimeInterval) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        inFlight.removeAll { now - $0.at > ($0.direct ? minInterval : Self.staleAfter) }
        return inFlight.isEmpty
    }

    /// Record a present before it is handed to Metal; `landed` takes it back.
    func issued(at t: CFTimeInterval, direct: Bool) {
        lock.lock()
        inFlight.removeAll { t - $0.at > Self.staleAfter }
        inFlight.append(Present(at: t, direct: direct))
        lock.unlock()
    }

    /// The present issued at `issuedAt` called back, or never registered.
    func landed(issuedAt: CFTimeInterval) {
        lock.lock()
        if let i = inFlight.firstIndex(where: { $0.at == issuedAt }) { inFlight.remove(at: i) }
        lock.unlock()
    }

    /// A Direct present's issue → callback, against `interval`: the content interval, or the
    /// panel's shortest refresh when the content is faster.
    func noteDirect(hold: CFTimeInterval, interval: CFTimeInterval, now: CFTimeInterval) {
        lock.lock()
        defer { lock.unlock() }
        guard current == .direct else { return }
        holds.append(hold)
        if holds.count > Self.window { holds.removeFirst() }
        guard holds.count == Self.window,
            holds.sorted()[Self.window / 2] > Self.blendedHold * interval
        else { return }
        backoff = now - switchedAt < Self.probeSpan
            ? min(backoff * 2, Self.maxBackoff) : Self.baseBackoff
        switchLocked(to: .composited, now: now)
    }

    /// Whether the window is fullscreen, the one state that scans out Direct. A change to
    /// possible probes at once; impossible stops the timed probes and leaves the mode to the
    /// holds, since a slow stream shows every frame blended.
    func setDirectPossible(_ possible: Bool, now: CFTimeInterval = CACurrentMediaTime()) {
        lock.lock()
        defer { lock.unlock() }
        guard possible != directPossible else { return }
        directPossible = possible
        backoff = Self.baseBackoff
        if possible, current == .composited { switchLocked(to: .direct, now: now) }
    }

    /// Mode switches since the last call, for the pf-present line.
    func drainSwitches() -> Int {
        lock.lock()
        defer { lock.unlock() }
        let n = switches
        switches = 0
        return n
    }

    private func switchLocked(to mode: Mode, now: CFTimeInterval) {
        current = mode
        switchedAt = now
        holds.removeAll(keepingCapacity: true)
        switches += 1
    }
}
#endif
