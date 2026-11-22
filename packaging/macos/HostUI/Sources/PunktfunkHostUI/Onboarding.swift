// The setup window: one page per grant, each with why, the live state and one button. Shown at
// first launch, when a required grant is missing at launch, and from the panel. The step logic
// is plain functions of the facts so tests pin it without macOS answering.
import AppKit
import Observation

enum Step: Equatable, CaseIterable {
    case welcome, move, screen, input, audio, login, done

    /// Moving is offered only to a copy outside the Applications folders.
    static func all(inApplications: Bool) -> [Step] {
        allCases.filter { $0 != .move || !inApplications }
    }

    /// First launch opens on the welcome page; later the first grant still missing.
    static func start(in steps: [Step], firstLaunch: Bool, grants: Grants) -> Int {
        if firstLaunch { return 0 }
        let missing: Step? = steps.contains(.move) ? .move : !grants.screen ? .screen : !grants.input ? .input : nil
        return missing.flatMap { steps.firstIndex(of: $0) } ?? 0
    }
}

/// What a grant page's button does: ask once, then send the person to System Settings.
enum GrantAction: Equatable {
    case ask, openSettings, none

    static func of(granted: Bool, asked: Bool) -> GrantAction {
        granted ? .none : asked ? .openSettings : .ask
    }
}

/// How finishing ends. Registering hands over to the agent's fresh process, where a screen
/// grant from this run applies; otherwise such a grant needs a restart.
enum Finish: Equatable {
    case register, unregister, restart, close

    static func of(want: Bool, isOn: Bool, available: Bool, screenAskedThisRun: Bool) -> Finish {
        if available, want, !isOn { return .register }
        if available, !want, isOn { return .unregister }
        return screenAskedThisRun ? .restart : .close
    }
}

@MainActor @Observable
final class Onboarding {
    let store: HostStore
    let steps: [Step]
    var index: Int
    var grants: Grants
    var screenAsked = false
    var inputAsked = false
    var audioAsked = false
    var startAtLogin = true
    var moveFailed = false
    @ObservationIgnored var onClose: (() -> Void)?
    @ObservationIgnored private var polling: Task<Void, Never>?
    @ObservationIgnored private var observer: NSObjectProtocol?

    init(store: HostStore, firstLaunch: Bool) {
        self.store = store
        steps = Step.all(inApplications: store.inApplications)
        grants = store.grants
        index = Step.start(in: steps, firstLaunch: firstLaunch, grants: store.grants)
        // On by default the first time; later the person's own choice stands.
        startAtLogin = firstLaunch || store.login == .on || store.login == .needsApproval
    }

    var step: Step { steps[index] }
    var live: Bool { store.api != nil }

    /// Reads the grants twice a second while the window shows, and at once when the
    /// accessibility list changes.
    func watch() {
        guard live, polling == nil else { return }
        polling = Task { [weak self] in
            while let self, !Task.isCancelled {
                self.grants = Permissions.read()
                try? await Task.sleep(for: .milliseconds(500))
            }
        }
        observer = DistributedNotificationCenter.default().addObserver(
            forName: Notification.Name("com.apple.accessibility.api"), object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.grants = Permissions.read() }
        }
    }

    func stopWatching() {
        polling?.cancel()
        polling = nil
        if let observer { DistributedNotificationCenter.default().removeObserver(observer) }
        observer = nil
    }

    func next() {
        if index < steps.count - 1 { index += 1 }
        if step == .done { Notify.requestPermission() }
    }

    func back() { if index > 0 { index -= 1 } }

    func screenAction() {
        guard live else { return }
        switch GrantAction.of(granted: grants.screen, asked: screenAsked) {
        case .ask: Permissions.requestScreen()
        case .openSettings: Permissions.open(.screen)
        case .none: break
        }
        screenAsked = true
    }

    func inputAction() {
        guard live else { return }
        switch GrantAction.of(granted: grants.input, asked: inputAsked) {
        case .ask: Permissions.requestInput()
        case .openSettings: Permissions.open(.input)
        case .none: break
        }
        inputAsked = true
    }

    func audioAction() {
        guard live else { return }
        if GrantAction.of(granted: grants.audio == .allowed, asked: audioAsked) == .ask {
            Task {
                await Permissions.askForAudio()
                grants = Permissions.read()
            }
        } else {
            Permissions.open(.audio)
        }
        audioAsked = true
    }

    func move() {
        guard live else { return }
        do {
            let copy = try Relocation.copyToApplications(store.context.bundle)
            Reopen.afterExit(copy)
            store.quit()
        } catch {
            moveFailed = true
        }
    }

    func reopen() { store.restart() }

    var loginOn: Bool { store.login == .on || store.login == .needsApproval }

    func finish() {
        let plan = Finish.of(want: startAtLogin, isOn: loginOn,
                             available: store.inApplications && store.login != .unavailable,
                             screenAskedThisRun: screenAsked)
        onClose?()
        guard live else { return }
        switch plan {
        case .register: store.setLogin(true)
        case .unregister: store.setLogin(false)
        case .restart: store.restart()
        case .close: break
        }
    }
}
