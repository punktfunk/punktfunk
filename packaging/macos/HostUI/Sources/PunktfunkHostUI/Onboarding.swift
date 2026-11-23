// The setup window: welcome, the move to Applications, one page for the three grants, start at
// login, and a last page with the console password. Shown until it has been finished once, when
// a required grant is missing at launch, and from the panel. The step logic is plain functions
// of the facts so tests pin it without macOS answering.
import AppKit
import Observation

enum Step: String, Equatable, CaseIterable {
    case welcome, move, permissions, login, done

    /// Moving is offered only to a copy outside the Applications folders.
    static func all(inApplications: Bool) -> [Step] {
        allCases.filter { $0 != .move || !inApplications }
    }

    /// Where the window opens. A grant or the move restarts the app mid-way, so an unfinished
    /// setup resumes on the page it reached; a finished one opens on what is missing.
    static func start(in steps: [Step], reached: Step?, finished: Bool, grants: Grants) -> Int {
        if finished {
            let missing: Step? = steps.contains(.move) ? .move : grants.missingRequired ? .permissions : nil
            return missing.flatMap { steps.firstIndex(of: $0) } ?? 0
        }
        guard let reached else { return 0 }
        // The move page is gone once the app runs from Applications: the next page takes over.
        let resume = allCases.drop { $0 != reached }.first { steps.contains($0) }
        return resume.flatMap { steps.firstIndex(of: $0) } ?? 0
    }
}

/// What the setup remembers across the restarts it causes.
enum SetupProgress {
    private static let finishedKey = "SetupFinished"
    private static let reachedKey = "SetupReached"

    /// True once the window was closed by its Done button or by hand.
    static var finished: Bool {
        get { UserDefaults.standard.bool(forKey: finishedKey) }
        set { UserDefaults.standard.set(newValue, forKey: finishedKey) }
    }

    static var reached: Step? {
        get { UserDefaults.standard.string(forKey: reachedKey).flatMap(Step.init(rawValue:)) }
        set { UserDefaults.standard.set(newValue?.rawValue, forKey: reachedKey) }
    }
}

/// What a grant's button does: ask once, then send the person to System Settings.
enum GrantAction: Equatable {
    case ask, openSettings, none

    static func of(granted: Bool, asked: Bool) -> GrantAction {
        granted ? .none : asked ? .openSettings : .ask
    }
}

/// How finishing ends. macOS applies a screen recording or control grant only to a process
/// started after it. Registering hands over to the agent's fresh process; otherwise a grant
/// asked for in this run needs a restart.
enum Finish: Equatable {
    case register, unregister, restart, close

    static func of(want: Bool, isOn: Bool, available: Bool, grantAskedThisRun: Bool) -> Finish {
        if available, want, !isOn { return .register }
        if available, !want, isOn { return .unregister }
        return grantAskedThisRun ? .restart : .close
    }
}

@MainActor @Observable
final class Onboarding {
    let store: HostStore
    let steps: [Step]
    var index: Int {
        didSet { if live { SetupProgress.reached = step } }
    }
    /// Which way the last page change went, for the page transition.
    var forward = true
    var grants: Grants
    var screenAsked = false
    var inputAsked = false
    var audioAsked = false
    var startAtLogin = true
    var moveFailed = false
    /// The host's controller verdict for the last page; nil shows nothing.
    var controllers: ControllerVerdict?
    /// The console's first password while its file still holds it in clear; nil afterwards.
    var consolePassword: String?
    @ObservationIgnored var onClose: (() -> Void)?
    @ObservationIgnored private var polling: Task<Void, Never>?
    @ObservationIgnored private var observer: NSObjectProtocol?

    init(store: HostStore, finished: Bool, reached: Step?) {
        self.store = store
        steps = Step.all(inApplications: store.inApplications)
        grants = store.grants
        index = Step.start(in: steps, reached: reached, finished: finished, grants: store.grants)
        // On by default until the setup was finished once; later the person's own choice stands.
        startAtLogin = !finished || store.login == .on || store.login == .needsApproval
    }

    var step: Step { steps[index] }
    var live: Bool { store.api != nil }

    /// A screen recording or control grant asked for in this run reads as allowed only after a
    /// restart, so the page offers one.
    var needsReopen: Bool { (screenAsked && !grants.screen) || (inputAsked && !grants.input) }

    /// Reads the grants twice a second while the window shows, and at once when the
    /// accessibility list changes.
    func watch() {
        guard live, polling == nil else { return }
        SetupProgress.reached = step
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
        forward = true
        if index < steps.count - 1 { index += 1 }
        if step == .done { arrive() }
    }

    /// The last page asks for notifications, reads the console password and the host's
    /// controller verdict.
    func arrive() {
        Notify.requestPermission()
        guard let api = store.api else { return }
        consolePassword = api.files.consolePassword
        Task { controllers = (try? await api.diagnostics()).flatMap(ControllerVerdict.of) }
    }

    func back() {
        forward = false
        if index > 0 { index -= 1 }
    }

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
                             grantAskedThisRun: screenAsked || inputAsked)
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
