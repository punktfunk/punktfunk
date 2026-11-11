// `/actions` and `/display/targets` as an answer or a miss, for the stores that keep the last
// answer through a miss (`TTLCache`). `actions(_:)` and `targets(_:)` read every miss as an
// empty answer, which a cache would then keep for its whole TTL. Pure halves for
// `HostAnswersTests`.

import Foundation

extension LibraryClient {
    /// The host's word when it has none to give this device: no such route on an older host, or
    /// a refused certificate. Anything else short of a readable 200 is a miss.
    static let refusals: Set<Int> = [401, 403, 404]

    /// ``actions(_:)``, or nil when the host didn't answer.
    public static func actionsIfAnswered(_ target: MgmtTarget) async -> [HostAction]? {
        actionsAnswer(try? await send("/api/v1/actions", to: target))
    }

    /// The permitted actions `response` carries; empty for a refusal; nil for a miss.
    static func actionsAnswer(_ response: HTTPResponse?) -> [HostAction]? {
        guard let response else { return nil }
        if refusals.contains(response.status) { return [] }
        guard response.status == 200,
              let list = try? JSONDecoder().decode(ActionList.self, from: response.body)
        else { return nil }
        return list.actions.filter(\.permitted)
    }

    /// ``targets(_:)``, or nil when the host didn't answer. `.some(nil)`: it answered with no
    /// screens for this device.
    public static func targetsIfAnswered(_ target: MgmtTarget) async -> DisplayTargets?? {
        targetsAnswer(try? await send("/api/v1/display/targets", to: target))
    }

    /// The screens `response` carries; `.some(nil)` for a refusal; nil for a miss.
    static func targetsAnswer(_ response: HTTPResponse?) -> DisplayTargets?? {
        guard let response else { return nil }
        if refusals.contains(response.status) { return .some(nil) }
        guard response.status == 200,
              let targets = try? JSONDecoder().decode(DisplayTargets.self, from: response.body)
        else { return nil }
        return targets
    }

    private struct ActionList: Decodable {
        var actions: [HostAction]
    }
}
