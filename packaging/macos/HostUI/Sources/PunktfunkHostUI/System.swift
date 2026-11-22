// What the UI asks of macOS beyond the privacy grants: the login agent, the app's place on
// disk, reopening after an exit, and the connect notification.
import AppKit
import Security
import ServiceManagement
import UserNotifications

/// The bundled launchd agent (Contents/Library/LaunchAgents/io.unom.punktfunk.host.plist) that
/// starts `punktfunk-host serve` at login and restarts it after a crash.
enum LoginItem {
    static let label = "io.unom.punktfunk.host"

    enum State: Equatable {
        case on, off, needsApproval, unavailable
    }

    static var service: SMAppService { .agent(plistName: label + ".plist") }

    static func state() -> State {
        switch service.status {
        case .enabled: .on
        case .requiresApproval: .needsApproval
        // macOS reports an agent that was never registered as not found.
        case .notRegistered, .notFound: .off
        @unknown default: .unavailable
        }
    }

    /// launchd starts the agent's instance at once; it waits for this one's lock.
    static func register() throws { try service.register() }
    static func unregister() async throws { try await service.unregister() }

    /// A manual launch while the agent is registered: start the agent and leave serving to it.
    static func handOffToAgent() -> Bool {
        guard service.status == .enabled else { return false }
        HandOver.mark()
        let kick = Process()
        kick.executableURL = URL(fileURLWithPath: "/bin/launchctl")
        kick.arguments = ["kickstart", "gui/\(getuid())/\(label)"]
        guard (try? kick.run()) != nil else {
            HandOver.clear()
            return false
        }
        kick.waitUntilExit()
        if kick.terminationStatus != 0 { HandOver.clear() }
        return kick.terminationStatus == 0
    }

    static func openSettings() { SMAppService.openSystemSettingsLoginItems() }
}

/// The note a hand-over leaves for the agent's instance. The host's SIGUSR1 reaches only an
/// instance that already holds the lock; one that kickstart has just started reads this note
/// at launch instead, and shows its panel.
enum HandOver {
    static let note = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("io.unom.punktfunk.host.handover")
    /// An agent start this long after the note is a later one, not the hand-over's.
    static let window: TimeInterval = 30

    static func mark(_ url: URL = note) { FileManager.default.createFile(atPath: url.path, contents: nil) }
    static func clear(_ url: URL = note) { try? FileManager.default.removeItem(at: url) }

    /// True once for a note left within `window`; the note is gone afterwards either way.
    static func take(_ url: URL = note, now: Date = .now) -> Bool {
        defer { clear(url) }
        let made = (try? FileManager.default.attributesOfItem(atPath: url.path))?[.modificationDate] as? Date
        return made.map { now.timeIntervalSince($0) < window } ?? false
    }
}

/// Whether the app runs from an Applications folder, and moving it there.
enum Relocation {
    static func isInApplications(_ bundle: URL) -> Bool {
        let path = bundle.resolvingSymlinksInPath().path
        let homeApps = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Applications").path
        return path.hasPrefix("/Applications/") || path.hasPrefix(homeApps + "/")
    }

    /// Copies the bundle into /Applications (or ~/Applications when that is not writable),
    /// replacing an older copy, and checks the copy's signature. Returns the copy.
    static func copyToApplications(_ bundle: URL) throws -> URL {
        let fm = FileManager.default
        var folder = URL(fileURLWithPath: "/Applications", isDirectory: true)
        if !fm.isWritableFile(atPath: folder.path) {
            folder = fm.homeDirectoryForCurrentUser.appendingPathComponent("Applications", isDirectory: true)
            try fm.createDirectory(at: folder, withIntermediateDirectories: true)
        }
        let target = folder.appendingPathComponent(bundle.lastPathComponent)
        if fm.fileExists(atPath: target.path) { try fm.trashItem(at: target, resultingItemURL: nil) }
        try fm.copyItem(at: bundle, to: target)
        // Copied by code, the quarantine flag would send the copy back into translocation.
        removexattr(target.path, "com.apple.quarantine", XATTR_NOFOLLOW)
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(target as CFURL, [], &code) == errSecSuccess, let code,
              SecStaticCodeCheckValidity(code, [], nil) == errSecSuccess
        else {
            try? fm.trashItem(at: target, resultingItemURL: nil)
            throw CocoaError(.fileWriteUnknown)
        }
        return target
    }
}

/// Opens `app` once this process has exited, so the new instance finds no lock held. A process
/// still alive after 30 s keeps running and nothing opens.
enum Reopen {
    static func afterExit(_ app: URL) {
        // Its own session: a launchd bootout ends the job's process group, and this outlives it.
        let pid = getpid()
        let script = "i=0; while kill -0 \(pid) 2>/dev/null && [ $i -lt 150 ]; do sleep 0.2; i=$((i+1)); done; "
            + "kill -0 \(pid) 2>/dev/null || exec /usr/bin/open \"$0\""
        var attr: posix_spawnattr_t?
        posix_spawnattr_init(&attr)
        defer { posix_spawnattr_destroy(&attr) }
        posix_spawnattr_setflags(&attr, Int16(POSIX_SPAWN_SETSID))
        let argv = ["/bin/sh", "-c", script, app.path].map { strdup($0) } + [nil]
        defer { argv.forEach { free($0) } }
        var child: pid_t = 0
        posix_spawn(&child, "/bin/sh", nil, &attr, argv, environ)
    }
}

/// The connect notification. Only an app bundle has a notification identity.
enum Notify {
    static var available: Bool { Bundle.main.bundleURL.pathExtension == "app" }

    static func requestPermission() {
        guard available else { return }
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    static func connected(_ summary: LocalSummary) {
        guard available else { return }
        let content = UNMutableNotificationContent()
        content.title = summary.clientName.map { L10n.trayToastConnected(name: $0) }
            ?? String(localized: L10n.trayToastClientConnected)
        content.body = summary.session.map {
            L10n.trayToastStreamingMode(width: "\($0.width)", height: "\($0.height)", fps: "\($0.fps)")
        } ?? String(localized: L10n.trayToastStreaming)
        let request = UNNotificationRequest(identifier: "connected", content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }
}
