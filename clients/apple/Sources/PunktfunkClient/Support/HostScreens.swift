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

    /// Each host's answer, nil for one with no screens for this device. 20 s, as the Rust
    /// clients: `in_use_by` moves as other devices start and end streams.
    private var cache = TTLCache<DisplayTargets?>(ttl: 20)

    /// What `host` last answered; nil until it does, and for a host without the route.
    func targets(for host: StoredHost) -> DisplayTargets? { cache[host.id.uuidString] ?? nil }

    /// Ask again unless the last answer is fresh. A miss keeps the rows already shown.
    func refresh(_ host: StoredHost, port: UInt16? = nil) {
        guard !DemoMode.isDemo(host),
              case .success(let target) = MgmtTarget.make(host: host, port: port),
              let ticket = cache.ask(host.id.uuidString, now: Date())
        else { return }
        Task { @MainActor in
            let found = await LibraryClient.targetsIfAnswered(target)
            if found != nil { objectWillChange.send() }
            cache.land(ticket, found)
        }
    }

    /// Write this device's pick on `host`, then ask for the rows again. nil when the host took
    /// it, else the sentence to show; `label` is the row's word.
    func pick(
        _ screen: DisplayTarget, label: String, on host: StoredHost, port: UInt16? = nil
    ) async -> String? {
        defer {
            cache.invalidate(host.id.uuidString, keepValue: true)
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
