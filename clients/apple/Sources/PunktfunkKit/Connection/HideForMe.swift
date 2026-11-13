// "Hide for me": a player takes a title off the shelf of the locked profile this device is let
// into, and brings it back. A port of `pf_client_core::hide`; both replay
// `clients/shared/hide-for-me-vectors.json`. A host that never marks a row `person` has no action.

import Foundation
import PunktfunkShared

public enum HideForMe {
    /// The row of the profile this device plays as when it may hide titles there: `listed` is
    /// what `enumerate` answered (nil: the host has no profiles), `pick` the saved pick's id.
    public static func offer(listed: [ListedProfile]?, pick: String?) -> ListedProfile? {
        guard let pick else { return nil }
        return listed?.first { $0.id == pick && $0.person }
    }

    /// The label of the library's "Hidden for <name>" button for `offer`, or nil: no button.
    public static func hiddenEntry(_ offer: ListedProfile?) -> String? {
        offer.map { L10n.libHiddenFor(name: $0.displayName) }
    }

    /// The path and JSON body of the `PUT` that hides (`hidden`) or shows title `id` for
    /// `profile`. The id is one path segment (``LibraryClient/pathSegment(_:)``).
    static func request(profile: String, id: String, hidden: Bool) -> (path: String, body: Data) {
        let escaped = LibraryClient.pathSegment(id)
        let profile = LibraryClient.queryValue(profile) ?? profile
        return ("/api/v1/library/hidden/\(escaped)?as=\(profile)", Data(#"{"hidden":\#(hidden)}"#.utf8))
    }

    /// What a hide or a show came to.
    public enum Outcome: Equatable, Sendable {
        case done
        /// The host's sentence, worded by its code where this build has the message.
        case refused(String)
        /// No answer to word: a lowercase cause.
        case failed(String)

        public static func from(status: Int, body: Data) -> Outcome {
            if (200..<300).contains(status) { return .done }
            if let said = HostText.apiError(body) { return .refused(said) }
            return .failed(L10n.errorMgmtRefused(code: String(status)))
        }

        /// The line a player reads after hiding (`hidden`) or showing `title` for profile `name`.
        public func notice(hidden: Bool, title: String, name: String) -> String {
            switch (self, hidden) {
            case (.done, true): L10n.libHiddenDone(title: title, name: name)
            case (.done, false): L10n.libShownDone(title: title, name: name)
            case (.refused(let said), _): said
            case (.failed(let why), true): L10n.libHideFailed(title: title, why: why)
            case (.failed(let why), false): L10n.libShowFailed(title: title, why: why)
            }
        }
    }

    /// Hide (`hidden`) or show title `id` for `profile` on `target`. Never throws.
    public static func setHidden(
        _ id: String, hidden: Bool, profile: String, on target: MgmtTarget
    ) async -> Outcome {
        let (path, body) = request(profile: profile, id: id, hidden: hidden)
        do {
            let response = try await LibraryClient.send(
                path, to: target, method: "PUT", body: (body, "application/json"))
            return .from(status: response.status, body: response.body)
        } catch {
            return .failed(error.localizedDescription)
        }
    }

    /// One title on the hidden list, as the page marks it.
    struct Listed: Decodable {
        var entry: GameEntry
        var hidden: Bool

        private enum Keys: String, CodingKey { case hidden }

        init(from decoder: Decoder) throws {
            entry = try GameEntry(from: decoder)
            hidden = try decoder.container(keyedBy: Keys.self)
                .decodeIfPresent(Bool.self, forKey: .hidden) ?? false
        }
    }

    /// What `profile` hides, its person asking: every page of `hidden=true`, keeping only the
    /// rows marked hidden, so a host that ignores the flag lists nothing. A failed page fails
    /// the list. `get` answers one path.
    static func fetchHidden(
        profile: String, _ get: (String) async throws -> HTTPResponse
    ) async throws -> [GameEntry] {
        let profile = LibraryClient.queryValue(profile) ?? profile
        let rows = try await LibraryClient.walkPages(of: Listed.self) { cursor in
            var path = "/api/v1/library/page?limit=\(LibraryClient.pageLimit)&hidden=true"
            if let next = cursor.flatMap(LibraryClient.queryValue) { path += "&cursor=\(next)" }
            let response = try await get(path + "&as=\(profile)")
            try LibraryClient.expectOK(response)
            return response.body
        }
        return rows.filter(\.hidden).map(\.entry)
    }

    /// ``fetchHidden(profile:_:)`` over `target`'s lanes. Fetched when the player asks, never
    /// cached.
    public static func hidden(profile: String, on target: MgmtTarget) async throws -> [GameEntry] {
        try await fetchHidden(profile: profile) { path in try await LibraryClient.send(path, to: target) }
    }
}
