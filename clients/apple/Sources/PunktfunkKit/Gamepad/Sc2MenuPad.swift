// The Steam Controller 2 read OUTSIDE a session. `Sc2Capture` cannot cover it: that one is built
// from a live `PunktfunkConnection` at stream start.
//
// iOS, visionOS and tvOS surface nothing for an SC2, so there it drives the gamepad UI, over BLE,
// for the app's lifetime. macOS shows it to GameController as a twin without grips or trackpads,
// so there it runs only while the console's input test is up: over USB when a pad is attached and
// Input Monitoring already allows it (a test never prompts), else BLE — `Sc2Capture`'s pick.
//
// One pad, no wire: state reports are parsed to `Sc2Device.State` and kept in `snapshot` for
// `GamepadMenuInput`'s 60 Hz poll and the input test. Nothing here claims a wire slot, forwards
// a raw report, or writes beyond the links' own lizard-off keep-alive. A Puck's first pad to
// report is the one read.
//
// `Sc2Capture` owns the hardware whenever a session captures it, so `GamepadManager` stops this
// pad for the stream. Reports arrive on the link queue and `snapshot` is read from the main
// actor, so the state sits behind a lock; the attach edge hops to main.

import Foundation
#if !os(macOS)
import UIKit
#endif

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
    #if os(macOS)
    private var usbLink: Sc2UsbLink!
    /// The USB source being read, behind `lock`; nil until one reports.
    private var usbSource: UInt64?
    #endif
    private var observers: [NSObjectProtocol] = []

    /// Guards `state` and `listening` (the link queue and the main actor both touch them).
    private let lock = NSLock()
    /// The last parsed state report; nil until the first one and after the link closes, which
    /// is also what "a pad is attached" means here — an idle radio claims nothing.
    private var state: Sc2Device.State?
    /// False from a stop until the next start. The link stops asynchronously and keeps its
    /// connection 150 ms for the lizard restore; a report in that window must not re-attach
    /// the pad, or the menus read its last state for as long as the radio stays released.
    private var listening = false

    /// The live pad's buttons and sticks, or nil when none is delivering. Safe from any thread.
    var snapshot: Sc2Device.State? {
        lock.lock()
        defer { lock.unlock() }
        return state
    }

    /// Whether the delivering pad is a 2015 Steam Controller — the console card's kind.
    var isClassic: Bool {
        #if os(macOS)
        lock.lock()
        let source = usbSource
        lock.unlock()
        if let source { return usbLink.isClassic(source: source) }
        #endif
        return link.isClassic
    }

    init() {
        link = Sc2BleLink(
            queue: queue,
            onReport: { [weak self] report in
                guard let self else { return }
                handle(report, classic: link.isClassic)
            },
            onClosed: { [weak self] in self?.detach(reason: "link closed") })
        #if os(macOS)
        usbLink = Sc2UsbLink(
            queue: queue,
            onReport: { [weak self] source, report in self?.handleUsb(source, report) },
            onSourceClosed: { [weak self] source in self?.usbClosed(source) },
            onRefused: {})
        #endif
    }

    /// Begin acquisition. On iOS, visionOS and tvOS also follow the app's foreground: the radio
    /// is released while inactive (a CoreBluetooth central's recommended backgrounding) and
    /// re-acquired on return. A macOS reader lives only as long as the input test.
    @MainActor
    func start() {
        #if os(macOS)
        listen()
        #else
        guard observers.isEmpty else { return }
        // Both observers run on main, like `start`; the weak reference crosses no thread.
        nonisolated(unsafe) weak let weakSelf = self
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { _ in
            guard let self = weakSelf else { return }
            self.detach(reason: "app inactive", halt: true)
            self.link.stop()
        })
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { _ in
            weakSelf?.listen()
        })
        listen()
        #endif
    }

    /// Release the links and forget the pad. Idempotent.
    @MainActor
    func stop() {
        observers.forEach { NotificationCenter.default.removeObserver($0) }
        observers.removeAll()
        detach(reason: "stop", halt: true)
        link.stop()
        #if os(macOS)
        usbLink.stop()
        #endif
    }

    /// Take reports again and start (or restart) the link.
    private func listen() {
        lock.lock()
        listening = true
        lock.unlock()
        #if os(macOS)
        if Sc2UsbLink.attached() {
            // A refused open stays refused: `Sc2UsbLink.start` would prompt, and a test never does.
            if Sc2UsbLink.canOpenAttached() { usbLink.start() }
            return
        }
        #endif
        link.start()
    }

    #if os(macOS)
    /// One USB report. The first source to report is the pad the test shows.
    private func handleUsb(_ source: UInt64, _ report: [UInt8]) {
        lock.lock()
        let read = (usbSource ?? source) == source
        if read { usbSource = source }
        lock.unlock()
        if read { handle(report, classic: usbLink.isClassic(source: source)) }
    }

    private func usbClosed(_ source: UInt64) {
        lock.lock()
        let read = usbSource == source
        if read { usbSource = nil }
        lock.unlock()
        if read { detach(reason: "unplugged") }
    }
    #endif

    /// One report off the link queue. Only state reports matter — battery and wireless status
    /// carry nothing a menu can navigate with. A 2015 pad's arrive as wired frames.
    private func handle(_ framed: [UInt8], classic: Bool) {
        var parsed = Sc2Device.State()
        let isState = classic
            ? Sc1Device.parseState(framed, into: &parsed)
            : Sc2Device.parseState(framed, into: &parsed)
        guard isState else { return }
        lock.lock()
        guard listening else {
            lock.unlock()
            return
        }
        let wasAttached = state != nil
        state = parsed
        lock.unlock()
        guard !wasAttached else { return }
        log.info("SC2: menu pad attached")
        Task { @MainActor [weak self] in self?.onAttachChange?(true) }
    }

    /// Drop the pad — link closed, backgrounded, or stopped. Fires the edge only on a real
    /// transition, so a repeated stop stays silent. `halt` also refuses reports until the next
    /// `listen`: the two stops, where the link still delivers for a moment.
    private func detach(reason: String, halt: Bool = false) {
        lock.lock()
        if halt { listening = false }
        let wasAttached = state != nil
        state = nil
        #if os(macOS)
        if halt { usbSource = nil }
        #endif
        lock.unlock()
        guard wasAttached else { return }
        log.info("SC2: menu pad released (\(reason))")
        Task { @MainActor [weak self] in self?.onAttachChange?(false) }
    }
}
