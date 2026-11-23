// Everything the panel shows and does. A live store polls the host's loopback API; a store
// without an API holds canned state for the preview and its actions do nothing.
import AppKit
import Observation

public struct HostContext: Sendable {
    public var configDir: URL
    public var version: String
    public var launchedByLoginAgent: Bool
    /// The punktfunk-host binary, for `ctl` and for quitting through its signal handler. Nil in
    /// the preview, which runs without the host.
    public var hostExecutable: URL?
    public var bundle: URL

    public init(configDir: URL, version: String, launchedByLoginAgent: Bool = false,
                hostExecutable: URL? = nil, bundle: URL = Bundle.main.bundleURL)
    {
        self.configDir = configDir
        self.version = version
        self.launchedByLoginAgent = launchedByLoginAgent
        self.hostExecutable = hostExecutable
        self.bundle = bundle
    }

    /// `pf_paths::config_dir()`: `$PUNKTFUNK_CONFIG_DIR`, else `$XDG_CONFIG_HOME/punktfunk`,
    /// else `~/.config/punktfunk`.
    public static func defaultConfigDir() -> URL {
        let env = ProcessInfo.processInfo.environment
        if let dir = env["PUNKTFUNK_CONFIG_DIR"], !dir.isEmpty { return URL(fileURLWithPath: dir) }
        if let xdg = env["XDG_CONFIG_HOME"], !xdg.isEmpty {
            return URL(fileURLWithPath: xdg).appendingPathComponent("punktfunk")
        }
        return FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".config/punktfunk")
    }
}

@MainActor @Observable
public final class HostStore {
    enum Confirm: Equatable {
        case endSession(id: Int, name: String)
        case release
        /// By id: a refresh replaces the device's row, its age with it.
        case approve(id: Int)
    }

    let context: HostContext
    let api: HostAPI?
    var state = HostState.starting
    var consoleUp = true
    var runtime: RuntimeStatus?
    var pairing: NativePairStatus?
    var pairDeadline: Date?
    var pairBoundTo: PendingDevice?
    var pending: [PendingDevice] = []
    var update: UpdateStatus?
    var grants = Grants.all
    var login = LoginItem.State.off
    var inApplications = true
    var confirm: Confirm?
    var error: String?
    var busy = false
    var panelOpen = false {
        didSet { if panelOpen, !oldValue { error = nil; refreshNow() } }
    }

    /// Called after every change the menu bar glyph may show.
    @ObservationIgnored var onChange: (() -> Void)?
    @ObservationIgnored var onShowOnboarding: (() -> Void)?
    @ObservationIgnored private var unreachableSince: Date?
    @ObservationIgnored private var consoleMisses = 0
    @ObservationIgnored private var edge = ConnectEdge()
    @ObservationIgnored private var updateCheckedAt: Date?
    @ObservationIgnored private let console = ConsoleProbe()

    public init(context: HostContext, live: Bool) {
        self.context = context
        api = live ? HostAPI(files: HostFiles(dir: context.configDir)) : nil
        if live {
            grants = Permissions.read()
            login = LoginItem.state()
            inApplications = Relocation.isInApplications(context.bundle)
        }
    }

    /// Whether `device`'s approval is open, whichever refresh its row came from.
    func isApproving(_ device: PendingDevice) -> Bool { confirm == .approve(id: device.id) }

    var needsAttention: Bool { state.pairingAttention || grants.missingRequired }
    var glyph: GlyphState { .of(state, needsAttention: needsAttention) }

    func start() {
        guard let api else { return }
        Task { [weak self] in
            while let self {
                await self.tick(api)
                try? await Task.sleep(for: .seconds(self.panelOpen ? 2 : 3))
            }
        }
    }

    func refreshNow() {
        guard let api else { return }
        Task { await tick(api) }
    }

    private func tick(_ api: HostAPI) async {
        let summary = try? await api.summary()
        if summary != nil {
            unreachableSince = nil
        } else if unreachableSince == nil {
            unreachableSince = .now
        }
        let next = HostState.map(summary, unreachableSince: unreachableSince, now: .now)
        if edge.fires(streaming: next.isStreaming), let s = next.summary { Notify.connected(s) }
        state = next
        // One miss is not down: a cold console can outrun the timeout.
        consoleMisses = await console.responds() ? 0 : consoleMisses + 1
        consoleUp = consoleMisses < 2
        grants = Permissions.read()
        login = LoginItem.state()
        if panelOpen { await refreshDetails(api) }
        onChange?()
    }

    private func refreshDetails(_ api: HostAPI) async {
        runtime = try? await api.status()
        apply(try? await api.pairing())
        pending = (try? await api.pending()) ?? []
        if updateCheckedAt.map({ Date.now.timeIntervalSince($0) > 600 }) ?? true {
            update = try? await api.update()
            updateCheckedAt = .now
        }
    }

    private func apply(_ status: NativePairStatus?) {
        pairing = status
        if status?.armed == true, let secs = status?.expiresInSecs {
            pairDeadline = .now.addingTimeInterval(TimeInterval(secs))
        } else if status?.armed != true {
            pairDeadline = nil
            pairBoundTo = nil
        }
    }

    /// Runs one action; a failure shows `failed(reason)` until the panel next opens.
    private func act(_ failed: @escaping (String) -> String, _ body: @escaping (HostAPI) async throws -> Void) {
        guard let api else { return }
        busy = true
        error = nil
        Task {
            do {
                try await body(api)
                confirm = nil
            } catch {
                self.error = failed((error as? HostError)?.reason ?? error.localizedDescription)
            }
            busy = false
            await tick(api)
        }
    }

    func pair(for device: PendingDevice? = nil) {
        act({ L10n.trayMacPairFailed(why: $0) }) { api in
            let status = try await api.arm(boundTo: device?.fingerprint, access: device == nil ? .standard : .remote)
            self.apply(status)
            self.pairBoundTo = device
        }
    }

    func cancelPairing() {
        act({ L10n.trayMacPairFailed(why: $0) }) { api in try await api.disarm() }
    }

    func approve(_ device: PendingDevice, guest: Bool) {
        act({ L10n.trayMacPendingApproveFailed(name: device.name, why: $0) }) { api in
            try await api.approve(device.id, access: guest ? .guest : .standard)
        }
    }

    func deny(_ device: PendingDevice) {
        act({ L10n.trayMacPendingDenyFailed(name: device.name, why: $0) }) { api in try await api.deny(device.id) }
    }

    func endSession(_ id: Int) {
        act({ L10n.trayMacSessionEndFailed(why: $0) }) { api in try await api.endSession(id) }
    }

    func releaseDisplays() {
        act({ L10n.trayMacReleaseFailed(why: $0) }) { api in try await api.releaseDisplays() }
    }

    func restart() {
        act({ L10n.trayMacRestartFailed(why: $0) }) { api in try await api.restart() }
    }

    /// The host's SIGTERM handler ends sessions, restores displays and audio, then exits 0,
    /// which the login agent does not restart.
    func quit() {
        if context.hostExecutable != nil {
            kill(getpid(), SIGTERM)
        } else {
            NSApp.terminate(nil)
        }
    }

    /// Signed in, through the host's `ctl console-url --open`.
    func openConsole() {
        guard let exe = context.hostExecutable else { return }
        let ctl = Process()
        ctl.executableURL = exe
        ctl.arguments = ["ctl", "console-url", "--open"]
        do {
            try ctl.run()
        } catch {
            self.error = L10n.trayMacConsoleFailed(why: error.localizedDescription)
        }
    }

    func openLogs() { NSWorkspace.shared.open(HostFiles(dir: context.configDir).logsDir) }

    func openUpdate() {
        if let page = update?.manifest.flatMap({ URL(string: $0.notesUrl) }) { NSWorkspace.shared.open(page) }
    }

    func showOnboarding() { onShowOnboarding?() }

    func setLogin(_ on: Bool) {
        guard api != nil else { return }
        Task {
            do {
                if on {
                    try LoginItem.register()
                    login = LoginItem.state()
                    if login == .on { await handOverToAgent() }
                } else {
                    // launchd ends the agent it unregisters; a fresh instance takes over.
                    if context.launchedByLoginAgent { Reopen.afterExit(context.bundle) }
                    try await LoginItem.unregister()
                    login = LoginItem.state()
                }
            } catch {
                self.error = L10n.trayMacLoginFailed(why: error.localizedDescription)
            }
        }
    }

    /// The agent's instance waits for this one's lock. With nothing streaming, make way for it.
    func handOverToAgent() async {
        guard context.hostExecutable != nil, !context.launchedByLoginAgent, let api,
              let status = try? await api.status(), status.activeSessions == 0
        else { return }
        kill(getpid(), SIGTERM)
    }
}
