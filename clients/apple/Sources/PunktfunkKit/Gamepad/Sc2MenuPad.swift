// The Steam Controller 2 read OUTSIDE a session, so the gamepad UI can be navigated with it.
//
// iOS only, and BLE only. macOS surfaces an SC2 to GameController on its own (`GamepadManager`'s
// shadow suppression exists for exactly that), so the console launcher is already navigable
// there; iOS surfaces nothing for it, which leaves an iPad user unable to reach the launcher
// with the only controller in the room. `Sc2Capture` cannot cover it — that one is built from a
// live `PunktfunkConnection` at stream start, so it does not exist until the session it is
// meant to launch is already running.
//
// One link, one pad, no wire: state reports are parsed to `Sc2Device.State` and kept in
// `snapshot` for `GamepadMenuInput`'s 60 Hz poll. Nothing here claims a wire slot, forwards a
// raw report, or writes to the pad beyond the link's own lizard-off keep-alive.
//
// `Sc2Capture` owns the hardware whenever a session captures it — two centrals subscribed to
// one peripheral would double-feed it — so `GamepadManager.holdSc2Hardware` stops this pad for
// the stream. Either handoff costs at most one 2 s re-acquisition poll.
//
// Threading matches `Sc2Capture`: reports arrive on the link queue, `snapshot` is read from the
// main actor, so the state sits behind a lock; the attach edge hops to main.

#if os(iOS) || os(visionOS)

import Foundation
import UIKit

private let log = ClientLog(category: "gamepad")

final class Sc2MenuPad {
    /// Fired ON MAIN when the pad starts or stops delivering state reports. `GamepadManager`
    /// republishes it, so a lone SC2 counts as a connected controller for `GamepadUIEnvironment`.
    var onAttachChange: ((Bool) -> Void)?

    /// The link's report queue — USER_INTERACTIVE for the same reason `Sc2Capture`'s is: BLE
    /// packets are dropped silently if the consumer stalls.
    private let queue = DispatchQueue(
        label: "io.unom.punktfunk.sc2-menu", qos: .userInteractive)
    private var link: Sc2BleLink!
    private var observers: [NSObjectProtocol] = []

    /// Guards `state` (written on the link queue, read from the main actor).
    private let lock = NSLock()
    /// The last parsed state report; nil until the first one and after the link closes, which
    /// is also what "a pad is attached" means here — an idle radio claims nothing.
    private var state: Sc2Device.State?

    /// The live pad's buttons and sticks, or nil when none is delivering. Safe from any thread.
    var snapshot: Sc2Device.State? {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    /// Whether the delivering pad is a 2015 Steam Controller — the console card's kind.
    var isClassic: Bool { link.isClassic }

    init() {
        link = Sc2BleLink(
            queue: queue,
            onReport: { [weak self] report in self?.handle(report) },
            onClosed: { [weak self] in self?.detach(reason: "link closed") })
    }

    /// Begin acquisition, and follow the app's foreground: the radio is released while inactive
    /// (a CoreBluetooth central's recommended backgrounding) and re-acquired on return.
    @MainActor
    func start() {
        guard observers.isEmpty else { return }
        // Both observers run on main, like `start`; the weak reference crosses no thread.
        nonisolated(unsafe) weak let weakSelf = self
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { _ in
            guard let self = weakSelf else { return }
            self.detach(reason: "app inactive")
            self.link.stop()
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { _ in
            weakSelf?.link.start()
        })
        link.start()
    }

    /// Release the radio and forget the pad. Idempotent.
    @MainActor
    func stop() {
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
        detach(reason: "stop")
        link.stop()
    }

    /// One report off the link queue. Only state reports matter — battery and wireless status
    /// carry nothing a menu can navigate with. A 2015 pad's arrive as wired frames.
    private func handle(_ framed: [UInt8]) {
        var parsed = Sc2Device.State()
        let isState = link.isClassic
            ? Sc1Device.parseState(framed, into: &parsed)
            : Sc2Device.parseState(framed, into: &parsed)
        guard isState else { return }
        lock.lock()
        let wasAttached = state != nil
        state = parsed
        lock.unlock()
        guard !wasAttached else { return }
        log.info("SC2: menu pad attached")
        Task { @MainActor [weak self] in self?.onAttachChange?(true) }
    }

    /// Drop the pad — link closed, backgrounded, or stopped. Fires the edge only on a real
    /// transition, so a repeated stop stays silent.
    private func detach(reason: String) {
        lock.lock()
        let wasAttached = state != nil
        state = nil
        lock.unlock()
        guard wasAttached else { return }
        log.info("SC2: menu pad released (\(reason))")
        Task { @MainActor [weak self] in self?.onAttachChange?(false) }
    }
}

#endif
