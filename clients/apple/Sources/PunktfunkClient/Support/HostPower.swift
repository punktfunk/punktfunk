// Host actions — sleep, restart or shut down a paired host from this device
// (`design/host-actions.md` §7), the other half of the Wake-on-LAN round trip. The Apple port of
// the Gaming Mode console's `ConsoleCmd::HostAction` (clients/session/src/console.rs), same
// wording on success.
//
// The HOST is the only enforcer: `HostAction.permitted` is what it says about THIS device's
// access, so a device without the Host-power grant is offered nothing rather than shown a row
// that will be refused. Discovery is kept on a slow TTL so a menu's rows are settled before it
// draws — rows that appear under a finger already on its way down would be a hazard when two of
// them end whatever is running on that machine.

import Foundation
import PunktfunkKit

private let log = ClientLog(category: "power")

/// A destructive host action waiting on its confirmation — which host, and which verb.
struct PendingHostAction: Identifiable {
    let host: StoredHost
    let action: HostAction
    var id: String { "\(host.id.uuidString):\(action.id)" }
}

/// The per-host answer cache, shared by the touch card menu and the gamepad options view.
///
/// `@MainActor` and observable: the menus read `actions(for:)` while drawing, and a refresh that
/// lands republishes them. One cache for both surfaces, so the two menus cannot disagree about
/// what a host offers.
@MainActor
final class HostPowerStore: ObservableObject {
    static let shared = HostPowerStore()

    /// How long an answer stays fresh. Long on purpose: what it governs (whether this device
    /// holds the grant, whether the box can suspend) changes when an operator edits access, not
    /// minute to minute, and every refresh is a TLS handshake against an otherwise idle host.
    private static let ttl: TimeInterval = 300

    @Published private var byHost: [String: [HostAction]] = [:]
    private var askedAt: [String: Date] = [:]

    /// What `host` last said this device may do to it. Empty until a refresh answers, and empty
    /// for an older host, an unreachable one, or a device without the grant.
    func actions(for host: StoredHost) -> [HostAction] { byHost[host.id.uuidString] ?? [] }

    /// Ask again unless the cached answer is still fresh. Cheap and idempotent — call it from
    /// whatever the surface already does on appear or on a refresh tick.
    func refresh(_ host: StoredHost) {
        let key = host.id.uuidString
        // Stamp BEFORE the request, so a slow or hanging host cannot make every pass ask again.
        if let at = askedAt[key], Date().timeIntervalSince(at) < Self.ttl { return }
        // The demo host serves no management API.
        guard !DemoMode.isDemo(host), case .success(let target) = MgmtTarget.make(host: host)
        else { return }
        askedAt[key] = Date()
        Task { @MainActor in
            let found = await LibraryClient.actions(target)
            byHost[key] = found
        }
    }

    /// Forget what this host said — call it right after invoking an action, because whatever it
    /// said is about to be wrong. Without this, a menu goes on offering "Sleep Host" for a
    /// machine that is already asleep until the TTL lapses.
    func invalidate(_ host: StoredHost) {
        let key = host.id.uuidString
        byHost[key] = []
        askedAt[key] = nil
    }

    /// Run one action against `host`. Never throws: the caller shows `message` either way.
    ///
    /// Success means the host ACCEPTED it — it now ends every session and acts about a second
    /// later, so this is the last word this device will get on the subject.
    func invoke(_ action: HostAction, on host: StoredHost) async -> (ok: Bool, message: String) {
        do {
            let target = try MgmtTarget.make(host: host).get()
            invalidate(host)
            try await LibraryClient.invokeAction(id: action.id, on: target)
            log.info("host action \(action.id, privacy: .public) accepted by \(host.displayName, privacy: .public)")
            return (true, "\(host.displayName): \(action.label) — on its way")
        } catch let missing as MgmtTargetMissing {
            return (false, missing.sentence)
        } catch {
            let why = (error as? LocalizedError)?.errorDescription ?? error.localizedDescription
            log.warning("host action \(action.id, privacy: .public) refused by \(host.displayName, privacy: .public): \(why, privacy: .public)")
            return (false, "\(action.label) failed — \(why)")
        }
    }
}
