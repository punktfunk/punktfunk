// The service side of the console: this app's stores pushed in, and what the shell raises
// turned back into the calls the gamepad shell already made. The Kotlin twin is
// `SkiaConsole.kt`; the JSON both write is `ConsoleJSON`.
//
// Everything here runs on the main actor, which is also the console's own thread: the display
// link calls `consoleDidDrawFrame` there, and that is where events and commands are drained.

import Combine
import Foundation
import GameController
import Metal
import PunktfunkKit
import PunktfunkShared
import SwiftUI

@MainActor
final class ConsoleModel: ObservableObject, ConsoleViewDelegate {
    /// What the console asks the app to do: connect, launch, wake, pair.
    struct Actions {
        var connect: (StoredHost, PresetSelection) -> Void
        var connectDiscovered: (DiscoveredHost) -> Void
        /// The Pair screen's "Request access": the long approval dial. A discovered host is
        /// saved first.
        var requestAccess: (StoredHost) -> Void
        var requestAccessDiscovered: (DiscoveredHost) -> Void
        var launchTitle: (LibraryTarget, String) -> Void
        var connectShelf: (LibraryTarget) -> Void
        var wakeOnly: (StoredHost) -> Void
        var cancelConnect: () -> Void
        var showStream: () -> Void
        /// The console's launch hold went up (`true`) or let go.
        var holding: (Bool) -> Void
        var paired: (StoredHost, Data) -> Void
        /// A Back the shell did not take: on a TV that press belongs to the system.
        var quit: () -> Void
    }

    let bridge: ConsoleBridge
    let device: MTLDevice
    let queue: MTLCommandQueue

    let store: HostStore
    let discovery: HostDiscovery
    let presets: PresetStore
    let power: HostPowerStore
    let nowPlaying: NowPlayingStore
    let waker: HostWaker
    let actions: Actions
    /// The pairing ceremony the console's PIN screen drives.
    let ceremony = PairCeremony()
    private let pads = GamepadMenuInput(manager: .shared)
    private let haptics = MenuHaptics(manager: .shared)
    /// When the pad last drove the console. A pulse from a remote, keyboard or finger is
    /// felt by nobody holding the pad, so it stays silent.
    private var padInputAt: TimeInterval = 0
    /// A console field the system keyboard is typing into (Apple TV). The pad rests meanwhile:
    /// on a TV it drives the keyboard through the focus engine.
    @Published var systemEntry: SystemEntry? {
        didSet {
            guard (systemEntry == nil) != (oldValue == nil) else { return }
            if systemEntry == nil { pads.start() } else { pads.stop() }
        }
    }
    /// The field the console named just before it raised `editing`.
    private var openedField: SystemEntry?
    /// Sends the pad's reading while the console's input test is up; the menu poller rests.
    private var padTestTimer: Timer?
    /// The launch hold as last reported through `actions.holding`.
    private var holding = false

    struct SystemEntry: Identifiable, Equatable {
        let id = UUID()
        let label: String
        let text: String
        let digits: Bool
    }

    /// Open prompts by id, each with what its answer does.
    private var prompts: [String: (Int?) -> Void] = [:]
    private var watching: [AnyCancellable] = []
    /// The shelf the console has open, so a fetch knows whose catalog it is filling.
    private var shelf: StoredHost?
    var fetching: Task<Void, Never>?
    /// The posters of the last list fetch; a new fetch cancels it.
    var artTask: Task<Void, Never>?

    init?(entry: StoredHost?, pin: StreamPreset?, store: HostStore, discovery: HostDiscovery,
          presets: PresetStore, power: HostPowerStore, nowPlaying: NowPlayingStore,
          waker: HostWaker, actions: Actions) {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue(),
            let bridge = ConsoleBridge(
                options: Self.options(
                    entry: entry, pin: pin, presets: presets.presets, hosts: store.hosts),
                device: device, queue: queue)
        else { return nil }
        self.device = device
        self.queue = queue
        self.bridge = bridge
        self.store = store
        self.discovery = discovery
        self.presets = presets
        self.power = power
        self.nowPlaying = nowPlaying
        self.waker = waker
        self.actions = actions
        self.shelf = entry
        wirePads()
    }

    // MARK: - lifecycle

    /// The console is on screen: push what it starts from and follow the stores.
    func attach() {
        pushHosts()
        pushPresets()
        pushKnownHosts()
        bridge.push(.settings, ConsoleJSON.string(Self.settings(store.hosts)))
        discovery.start()
        pads.start()
        for object in [store.objectWillChange, presets.objectWillChange, power.objectWillChange,
                       nowPlaying.objectWillChange, discovery.objectWillChange] {
            watching.append(
                object.receive(on: RunLoop.main).sink { [weak self] _ in
                    self?.pushHosts()
                })
        }
        watching.append(
            store.objectWillChange.receive(on: RunLoop.main).sink { [weak self] _ in
                self?.pushKnownHosts()
            })
        watching.append(
            presets.objectWillChange.receive(on: RunLoop.main).sink { [weak self] _ in
                self?.pushPresets()
            })
        pushPads()
        watching.append(
            GamepadManager.shared.objectWillChange.receive(on: RunLoop.main).sink { [weak self] _ in
                self?.pushPads()
            })
        let plugs: [Notification.Name] = [
            .GCKeyboardDidConnect, .GCKeyboardDidDisconnect, .GCMouseDidConnect,
            .GCMouseDidDisconnect, .GCControllerDidConnect, .GCControllerDidDisconnect,
        ]
        for name in plugs {
            watching.append(
                NotificationCenter.default.publisher(for: name).receive(on: RunLoop.main)
                    .sink { [weak self] _ in self?.pushPads() })
        }
    }

    func detach() {
        // A console off screen holds nothing.
        report(holding: false)
        watching.removeAll()
        padTest(false)
        pads.stop()
        haptics.stop()
        fetching?.cancel()
        // The touch UI runs its own browse; two subscribers would keep the radio up between them.
        discovery.stop()
    }

    /// Re-root the console on a shelf — a deep link, or the shelf a game was launched from.
    func navigate(to shelf: LibraryTarget, pin: StreamPreset?) {
        bridge.push(.navigate, ConsoleJSON.entry(shelf.host, pin: pin, presets: presets.presets))
    }

    /// Open the console's Pair screen for `host`: a pairing the app was asked for elsewhere.
    func pair(_ host: StoredHost) {
        bridge.push(.navigate, ConsoleJSON.pairEntry(host, presets: presets.presets))
    }

    /// Ask in the console instead of a system alert a pad cannot answer. `answer` gets the
    /// chosen index, or nil for Back.
    func prompt(
        id: String, title: String, message: String, choices: [String],
        answer: @escaping (Int?) -> Void
    ) {
        prompts[id] = answer
        bridge.push(.prompt, ConsoleJSON.prompt(id: id, title: title, message: message, choices: choices))
    }

    func answerPrompt(id: String, choice: Int?) {
        prompts.removeValue(forKey: id)?(choice)
    }

    /// Where the session the console asked for stands, so the takeover can narrate it.
    func session(_ phase: ConsoleBridge.Phase, message: String = "") {
        bridge.phase(phase, message: message)
    }

    // MARK: - ConsoleViewDelegate

    func consoleDidDrawFrame() {
        while let raw = bridge.nextEvent() { handle(event: raw) }
        drainCommands()
        report(holding: bridge.holdsLaunch)
    }

    private func report(holding now: Bool) {
        guard now != holding else { return }
        holding = now
        actions.holding(now)
    }

    func consoleDidRequestQuit() { actions.quit() }

    // MARK: - what the app pushes

    /// The settings document with each saved host's favorites under `favorites.<fp>`, where
    /// the console reads them; `LibraryFavorites` keeps them by host record.
    private static func settings(_ hosts: [StoredHost]) -> [String: Any] {
        var doc = ConsoleSettings.document()
        for host in hosts {
            guard let fp = host.pinnedSHA256?.map({ String(format: "%02x", $0) }).joined()
            else { continue }
            let ids = LibraryFavorites.shared.ids(for: host.id.uuidString)
            doc["favorites.\(fp)"] = ids.isEmpty ? nil : ids
        }
        return doc
    }

    /// The console saved favorites: back into `LibraryFavorites`, by record.
    private func applyFavorites(_ doc: [String: Any]) {
        for host in store.hosts {
            guard let fp = host.pinnedSHA256?.map({ String(format: "%02x", $0) }).joined()
            else { continue }
            let ids = doc["favorites.\(fp)"] as? [String] ?? []
            LibraryFavorites.shared.set(ids, host: host.id.uuidString)
        }
    }

    private static func options(
        entry: StoredHost?, pin: StreamPreset?, presets: [StreamPreset], hosts: [StoredHost]
    ) -> String {
        var options: [String: Any] = [
            "device_name": deviceName,
            "gpu_cache_bytes": gpuCacheBytes,
            // Every Apple platform keeps an interface to fall back to, so the console's own
            // off switch always has somewhere to land.
            "fallback_ui": true,
            "tv": isTV,
            // tvOS types through its own keyboard, where iPhone typing and dictation live.
            "system_keyboard": isTV,
            "av1_ok": AV1.hardwareDecodeSupported,
            "pyrowave_ok": MetalWaveletDecoder.supported,
            "settings": settings(hosts),
            "presets": presets.map { ["id": $0.id, "name": $0.name, "overrides": [:] as [String: Any]] },
        ]
        if let screen = screenSize {
            options["screen"] = [screen.0, screen.1]
            options["safe_area"] = [screen.0, screen.1]
        }
        if let entry, let data = ConsoleJSON.entry(entry, pin: pin, presets: presets)
            .data(using: .utf8),
            let row = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        {
            options["entry"] = row
        }
        return ConsoleJSON.string(options)
    }

    private static var isTV: Bool {
        #if os(tvOS)
        return true
        #else
        return false
        #endif
    }

    private static var deviceName: String {
        #if canImport(UIKit)
        return UIDevice.current.name
        #else
        return Host.current().localizedName ?? "Mac"
        #endif
    }

    /// Skia's budget for posters and glyph atlases. A share of this device's memory, floored
    /// so a TV still holds a shelf and capped so a Mac does not hoard.
    private static var gpuCacheBytes: Int {
        let share = Int(ProcessInfo.processInfo.physicalMemory / 32)
        return min(max(share, 64 << 20), 256 << 20)
    }

    /// This device's panel and its safe area, landscape, for the Aspect row. A Mac's window is
    /// not a panel, and a TV's is standard, so neither sends one.
    private static var screenSize: (Int, Int)? {
        #if os(iOS)
        let bounds = UIScreen.main.nativeBounds
        let long = Int(max(bounds.width, bounds.height))
        let short = Int(min(bounds.width, bounds.height))
        return (long, short)
        #else
        return nil
        #endif
    }

    func pushHosts() {
        var running: [String: String] = [:]
        var actions: [String: [HostAction]] = [:]
        for host in store.hosts {
            guard let fp = host.pinnedSHA256?.map({ String(format: "%02x", $0) }).joined()
            else { continue }
            if let title = nowPlaying.title(for: host) { running[fp] = title }
            let list = power.actions(for: host)
            if !list.isEmpty { actions[fp] = list }
        }
        bridge.push(
            .hosts,
            ConsoleJSON.hostRows(
                saved: store.hosts, discovered: discovery.hosts, online: store.probedOnline,
                presets: presets.presets, actions: actions, running: running))
    }

    private func pushPresets() { bridge.push(.presets, ConsoleJSON.presets(presets.presets)) }

    /// The connected pads, for the Players tab and the legend's chip.
    private func pushPads() {
        let m = GamepadManager.shared
        let forwarded = Set(m.forwarded.map(\.id))
        let pad = { (c: GamepadManager.DiscoveredController) in
            ConsoleJSON.Pad(c, forwarded: forwarded.contains(c.id))
        }
        var others: [(name: String, kind: String)] = []
        if let keyboard = GCKeyboard.coalesced {
            others.append((keyboard.vendorName ?? "Keyboard", "keyboard"))
        }
        others += GCMouse.mice().map { ($0.vendorName ?? "Mouse", "mouse") }
        // A controller with no extended profile is a remote: the Siri Remote on a TV.
        others += GCController.controllers()
            .filter { $0.extendedGamepad == nil && $0.microGamepad != nil }
            .map { ($0.vendorName ?? "Remote", "remote") }
        bridge.push(
            .pads,
            ConsoleJSON.pads(m.controllers.map(pad), active: m.active.map(pad), others: others))
    }

    /// The console's input test is up (`true`) or gone. While up, the pad's every button and
    /// axis goes to the console at 30 Hz, and the pad moves no menu.
    func padTest(_ on: Bool) {
        padTestTimer?.invalidate()
        padTestTimer = nil
        guard on else {
            if systemEntry == nil { pads.start() }
            return
        }
        pads.stop()
        let timer = Timer(timeInterval: 1.0 / 30.0, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.pushPadTest() }
        }
        RunLoop.main.add(timer, forMode: .common)
        padTestTimer = timer
    }

    private func pushPadTest() {
        guard let g = GamepadManager.shared.active?.controller.extendedGamepad else { return }
        let buttons: [(String, GCControllerButtonInput?)] = [
            ("A", g.buttonA), ("B", g.buttonB), ("X", g.buttonX), ("Y", g.buttonY),
            ("LB", g.leftShoulder), ("RB", g.rightShoulder),
            ("LT", g.leftTrigger), ("RT", g.rightTrigger),
            ("Back", g.buttonOptions), ("Start", g.buttonMenu), ("Guide", g.buttonHome),
            ("LS", g.leftThumbstickButton), ("RS", g.rightThumbstickButton),
            ("Up", g.dpad.up), ("Down", g.dpad.down), ("Left", g.dpad.left),
            ("Right", g.dpad.right),
        ]
        // GameController's +y is up; the console's is down.
        let axes: [[Any]] = [
            ["LX", g.leftThumbstick.xAxis.value], ["LY", -g.leftThumbstick.yAxis.value],
            ["RX", g.rightThumbstick.xAxis.value], ["RY", -g.rightThumbstick.yAxis.value],
            ["LT", g.leftTrigger.value], ["RT", g.rightTrigger.value],
        ]
        let held = buttons.filter { $0.1?.isPressed == true }.map(\.0)
        bridge.push(.padTest, ConsoleJSON.string(["held": held, "axes": axes]))
    }

    private func pushKnownHosts() {
        bridge.push(.knownHosts, ConsoleJSON.knownHosts(store.hosts))
    }

    func notice(_ text: String) { bridge.push(.notice, ConsoleJSON.string(text)) }

    // MARK: - input

    /// The pad drives the console through `GamepadMenuInput`'s poller: GameController's handlers
    /// do not fire on device outside a stream (its header).
    private func wirePads() {
        pads.onMove = { [weak self] (direction: GamepadMenuInput.Direction) in
            let event: ConsoleBridge.Menu =
                switch direction {
                case .up: .up
                case .down: .down
                case .left: .left
                case .right: .right
                }
            self?.fromPad(event)
        }
        pads.onConfirm = { [weak self] in self?.fromPad(.confirm) }
        pads.onSecondary = { [weak self] in self?.fromPad(.secondary) }
        pads.onTertiary = { [weak self] in self?.fromPad(.tertiary) }
        pads.onBack = { [weak self] in
            guard let self else { return }
            // `false` = the shell is at its root. ConsoleView keeps the pad's B from tvOS.
            if !fromPad(.back) { actions.quit() }
        }
        pads.onShoulder = { [weak self] forward in
            self?.fromPad(forward ? .jumpForward : .jumpBack)
        }
    }

    @discardableResult
    private func fromPad(_ event: ConsoleBridge.Menu) -> Bool {
        padInputAt = ProcessInfo.processInfo.systemUptime
        return bridge.menu(event, from: .pad)
    }

    /// The shell's haptic cue, on the pad, when the pad caused it.
    private func pulse(_ kind: String) {
        guard ProcessInfo.processInfo.systemUptime - padInputAt < 0.5 else { return }
        switch kind {
        case "move": haptics.move()
        case "confirm": haptics.confirm()
        case "boundary": haptics.boundary()
        default: break
        }
    }

    // MARK: - what the console raises

    private func handle(event raw: String) {
        guard let data = raw.data(using: .utf8),
            let event = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        if let settings = event["settings"] as? [String: Any] {
            ConsoleSettings.apply(settings)
            applyFavorites(settings)
        } else if let text = event["announce"] as? String {
            announce(text)
        } else if let action = event["action"] {
            handle(action: action)
        } else if let kind = event["pulse"] as? String {
            pulse(kind)
        } else if let field = event["edit_text"] as? [String: Any] {
            openedField = SystemEntry(
                label: field["label"] as? String ?? "", text: field["text"] as? String ?? "",
                digits: field["digits"] as? Bool ?? false)
        } else if let editing = event["editing"] as? Bool {
            #if os(tvOS)
            systemEntry = editing ? openedField : nil
            #endif
            openedField = nil
        }
    }

    /// The system keyboard closed: its text replaces the field's, and the field closes.
    func finishEntry(_ text: String) {
        guard let entry = systemEntry else { return }
        systemEntry = nil
        for _ in entry.text {
            bridge.key(.backspace)
        }
        bridge.text(text)
        bridge.key(.return)
    }

    private func announce(_ text: String) {
        #if canImport(UIKit)
        UIAccessibility.post(notification: .announcement, argument: text)
        #endif
    }

    private func handle(action: Any) {
        if let name = action as? String {
            switch name {
            case "CancelConnect": actions.cancelConnect()
            case "ShowStream": actions.showStream()
            case "Quit": actions.quit()
            default: break
            }
            return
        }
        guard let action = action as? [String: Any] else { return }
        if let launch = action["Launch"] as? [String: Any] {
            self.launch(launch)
        } else if let text = action["CopyText"] as? String {
            LinkClipboard.copy(text)
        }
    }

    private func launch(_ a: [String: Any]) {
        let fp = a["fp_hex"] as? String ?? ""
        let addr = a["addr"] as? String ?? ""
        let port = UInt16(a["port"] as? Int ?? 0)
        let preset: PresetSelection = (a["preset"] as? String).map { .preset($0) } ?? .inherit
        let requestAccess = a["request_access"] as? Bool ?? false
        guard let host = host(fp: fp, addr: addr, port: port) else {
            // Not saved yet: the row came from an advert, so dial it as a discovery does.
            if let found = discovery.hosts.first(where: { $0.host == addr && $0.port == port }) {
                if requestAccess {
                    actions.requestAccessDiscovered(found)
                } else {
                    actions.connectDiscovered(found)
                }
            }
            return
        }
        if requestAccess {
            actions.requestAccess(host)
        } else if let title = a["launch"] as? String {
            actions.launchTitle(LibraryTarget(host: host, preset: preset), title)
        } else {
            actions.connect(host, preset)
        }
    }

    func host(fp: String, addr: String, port: UInt16) -> StoredHost? {
        store.hosts.first {
            let own = $0.pinnedSHA256?.map { String(format: "%02x", $0) }.joined() ?? ""
            return (!fp.isEmpty && own == fp) || ($0.address == addr && $0.port == port)
        }
    }

    /// A row key is the fingerprint or `addr:port`, with a pinned card's preset id behind a NUL.
    func host(key: String) -> StoredHost? {
        let base = key.split(separator: "\u{0}").first.map(String.init) ?? key
        if let host = store.hosts.first(where: {
            ($0.pinnedSHA256?.map { String(format: "%02x", $0) }.joined() ?? "") == base
        }) { return host }
        let parts = base.split(separator: ":")
        guard parts.count == 2, let port = UInt16(parts[1]) else { return nil }
        return store.hosts.first { $0.address == String(parts[0]) && $0.port == port }
    }
}
