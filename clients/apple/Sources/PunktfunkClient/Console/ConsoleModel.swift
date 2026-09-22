// The service side of the console: this app's stores pushed in, and what the shell raises
// turned back into the calls the gamepad shell already made. The Kotlin twin is
// `SkiaConsole.kt`; the JSON both write is `ConsoleJSON`.
//
// Everything here runs on the main actor, which is also the console's own thread: the display
// link calls `consoleDidDrawFrame` there, and that is where events and commands are drained.

import Combine
import Foundation
import Metal
import PunktfunkKit
import PunktfunkShared
import SwiftUI

@MainActor
final class ConsoleModel: ObservableObject, ConsoleViewDelegate {
    /// What the console asks the app to do — the closures `GamepadHomeView` already took.
    struct Actions {
        var connect: (StoredHost, PresetSelection) -> Void
        var connectDiscovered: (DiscoveredHost) -> Void
        var launchTitle: (LibraryTarget, String) -> Void
        var connectShelf: (LibraryTarget) -> Void
        var wakeOnly: (StoredHost) -> Void
        var cancelConnect: () -> Void
        var showStream: () -> Void
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
    /// A screen the app owns and the console asked for (`PlatformScreen`), by id.
    @Published var platformScreen: String?
    private let pads = GamepadMenuInput(manager: .shared)
    private var watching: [AnyCancellable] = []
    /// The shelf the console has open, so a fetch knows whose catalog it is filling.
    private var shelf: StoredHost?
    var fetching: Task<Void, Never>?

    init?(entry: StoredHost?, pin: StreamPreset?, store: HostStore, discovery: HostDiscovery,
          presets: PresetStore, power: HostPowerStore, nowPlaying: NowPlayingStore,
          waker: HostWaker, actions: Actions) {
        guard let device = MTLCreateSystemDefaultDevice(), let queue = device.makeCommandQueue(),
            let bridge = ConsoleBridge(
                options: Self.options(entry: entry, pin: pin, presets: presets.presets),
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
        bridge.push(.settings, ConsoleSettings.json())
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
    }

    func detach() {
        watching.removeAll()
        pads.stop()
        fetching?.cancel()
        // The touch UI runs its own browse; two subscribers would keep the radio up between them.
        discovery.stop()
    }

    /// Re-root the console on a shelf — a deep link, or the shelf a game was launched from.
    func navigate(to shelf: LibraryTarget, pin: StreamPreset?) {
        bridge.push(.navigate, ConsoleJSON.entry(shelf.host, pin: pin, presets: presets.presets))
    }

    /// Where the session the console asked for stands, so the takeover can narrate it.
    func session(_ phase: ConsoleBridge.Phase, message: String = "") {
        bridge.phase(phase, message: message)
    }

    // MARK: - ConsoleViewDelegate

    func consoleDidDrawFrame() {
        while let raw = bridge.nextEvent() { handle(event: raw) }
        drainCommands()
    }

    func consoleDidRequestQuit() { actions.quit() }

    // MARK: - what the app pushes

    private static func options(entry: StoredHost?, pin: StreamPreset?, presets: [StreamPreset])
        -> String
    {
        var options: [String: Any] = [
            "device_name": deviceName,
            "gpu_cache_bytes": gpuCacheBytes,
            // Every Apple platform keeps an interface to fall back to, so the console's own
            // off switch always has somewhere to land.
            "fallback_ui": true,
            "av1_ok": AV1.hardwareDecodeSupported,
            "pyrowave_ok": MetalWaveletDecoder.supported,
            "settings": ConsoleSettings.document(),
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

    private func pushKnownHosts() {
        bridge.push(.knownHosts, ConsoleJSON.knownHosts(store.hosts))
    }

    func notice(_ text: String) { bridge.push(.notice, ConsoleJSON.string(text)) }

    // MARK: - input

    /// The pad drives the console through the same poller the SwiftUI shell used: GameController's
    /// handlers do not fire on device outside a stream (`GamepadMenuInput`'s header).
    private func wirePads() {
        pads.onMove = { [weak self] (direction: GamepadMenuInput.Direction) in
            let event: ConsoleBridge.Menu =
                switch direction {
                case .up: .up
                case .down: .down
                case .left: .left
                case .right: .right
                }
            self?.bridge.menu(event, from: .pad)
        }
        pads.onConfirm = { [weak self] in self?.bridge.menu(.confirm, from: .pad) }
        pads.onSecondary = { [weak self] in self?.bridge.menu(.secondary, from: .pad) }
        pads.onTertiary = { [weak self] in self?.bridge.menu(.tertiary, from: .pad) }
        pads.onBack = { [weak self] in
            guard let self else { return }
            // `false` = the shell let it go, which at the root is the system's press.
            if !bridge.menu(.back, from: .pad) { actions.quit() }
        }
        pads.onShoulder = { [weak self] forward in
            self?.bridge.menu(forward ? .jumpForward : .jumpBack, from: .pad)
        }
    }

    /// A remote or keyboard Back (tvOS `.onExitCommand`). `false` = the system's press.
    @discardableResult
    func back() -> Bool { bridge.menu(.back, from: .keys) }

    // MARK: - what the console raises

    private func handle(event raw: String) {
        guard let data = raw.data(using: .utf8),
            let event = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return }
        if let settings = event["settings"] as? [String: Any] {
            ConsoleSettings.apply(settings)
        } else if let text = event["announce"] as? String {
            announce(text)
        } else if let action = event["action"] {
            handle(action: action)
        }
        // `pulse` is the haptic cue and `editing` the shell's own keyboard: neither is ours.
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
        guard let host = host(fp: fp, addr: addr, port: port) else {
            // Not saved yet: the row came from an advert, so dial it as a discovery does.
            if let found = discovery.hosts.first(where: { $0.host == addr && $0.port == port }) {
                actions.connectDiscovered(found)
            }
            return
        }
        if let title = a["launch"] as? String {
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
