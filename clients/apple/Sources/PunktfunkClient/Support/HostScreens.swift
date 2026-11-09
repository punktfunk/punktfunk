// Which host screen this device streams, per paired host (`GET /display/targets`), for the
// card menu, the host page and the console. The pick lives on the host, so nothing here is
// persisted: a refresh is the only way an answer arrives.

import Foundation
import PunktfunkKit

private let log = ClientLog(category: "screens")

/// The per-host answer cache, shared by every surface so no two disagree about the pick.
@MainActor
final class HostScreenStore: ObservableObject {
    static let shared = HostScreenStore()

    /// 20 s, as the Rust clients: `in_use_by` moves as other devices start and end streams.
    private static let ttl: TimeInterval = 20

    @Published private var byHost: [String: DisplayTargets] = [:]
    private var askedAt: [String: Date] = [:]

    /// What `host` last answered; nil until it does, and for a host without the route.
    func targets(for host: StoredHost) -> DisplayTargets? { byHost[host.id.uuidString] }

    /// Ask again unless the last answer is fresh. A miss keeps the rows already shown.
    func refresh(_ host: StoredHost, port: UInt16? = nil) {
        let key = host.id.uuidString
        if let at = askedAt[key], Date().timeIntervalSince(at) < Self.ttl { return }
        guard !DemoMode.isDemo(host), case .success(let target) = MgmtTarget.make(host: host, port: port)
        else { return }
        askedAt[key] = Date()
        Task { @MainActor in
            if let found = await LibraryClient.targets(target) { byHost[key] = found }
        }
    }

    /// Write this device's pick on `host`, then ask for the rows again. nil when the host took
    /// it, else the sentence to show; `label` is the row's word.
    func pick(
        _ screen: DisplayTarget, label: String, on host: StoredHost, port: UInt16? = nil
    ) async -> String? {
        defer {
            askedAt[host.id.uuidString] = nil
            refresh(host, port: port)
        }
        do {
            let target = try MgmtTarget.make(host: host, port: port).get()
            try await LibraryClient.setScreen(screen, on: target)
            return nil
        } catch let missing as MgmtTargetMissing {
            return missing.sentence
        } catch {
            let why = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            log.warning("screen pick refused by \(host.displayName, privacy: .public): \(why, privacy: .public)")
            return L10n.hostcardActionFailed(action: label, reason: why)
        }
    }
}
