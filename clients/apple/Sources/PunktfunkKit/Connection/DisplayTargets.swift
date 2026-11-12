// Which host screen this device streams, and how: the host's answer, the Screen section every
// surface draws from it (`DisplayTargets.rows`), and the write of a row's pick. The Swift twin of
// `pf-client-core::display_targets`; `clients/shared/screen-rows-vectors.json` pins both.

import Foundation

/// What a session streams: this device's own screen, or a host monitor by connector.
/// JSON `"own"` or `{"monitor": "HDMI-A-1"}`, the host's spelling.
public enum DisplayTarget: Hashable, Sendable, Codable {
    case own
    case monitor(String)

    private enum Keys: String, CodingKey { case monitor }

    public init(from decoder: Decoder) throws {
        if let map = try? decoder.container(keyedBy: Keys.self) {
            self = .monitor(try map.decode(String.self, forKey: .monitor))
            return
        }
        let word = try decoder.singleValueContainer().decode(String.self)
        guard word == ScreenTarget.ownID else {
            throw DecodingError.dataCorrupted(
                .init(codingPath: decoder.codingPath, debugDescription: "unknown screen \(word)"))
        }
        self = .own
    }

    public func encode(to encoder: Encoder) throws {
        switch self {
        case .own:
            var one = encoder.singleValueContainer()
            try one.encode(ScreenTarget.ownID)
        case .monitor(let connector):
            var map = encoder.container(keyedBy: Keys.self)
            try map.encode(connector, forKey: .monitor)
        }
    }
}

/// One row of the picker. Every field defaults, so a host that adds or drops one never hides
/// the section.
public struct ScreenTarget: Codable, Hashable, Sendable, Identifiable {
    static let ownID = "own"

    /// `own`, else the connector.
    public var id: String
    /// The host's words: a monitor's description, or its English for the own row.
    public var label: String
    public var connector: String?
    /// `3840x2160@60`, as the host sends it.
    public var mode: String?
    public var primary: Bool
    /// False when the compositor reports the head off.
    public var enabled: Bool
    /// The pairing name of the device streaming this head now.
    public var inUseBy: String?

    private enum CodingKeys: String, CodingKey {
        case id, label, connector, mode, primary, enabled
        case inUseBy = "in_use_by"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decodeIfPresent(String.self, forKey: .id) ?? ""
        label = try c.decodeIfPresent(String.self, forKey: .label) ?? ""
        connector = try c.decodeIfPresent(String.self, forKey: .connector)
        mode = try c.decodeIfPresent(String.self, forKey: .mode)
        primary = try c.decodeIfPresent(Bool.self, forKey: .primary) ?? false
        enabled = try c.decodeIfPresent(Bool.self, forKey: .enabled) ?? true
        inUseBy = try c.decodeIfPresent(String.self, forKey: .inUseBy)
    }

    /// What picking this row writes.
    public var target: DisplayTarget { id == Self.ownID ? .own : .monitor(id) }

    /// The row's word: this app's wording for the own row, the host's label for a monitor.
    public var name: String {
        if id == Self.ownID { return String(localized: L10n.screenOwn) }
        return label.isEmpty ? id : label
    }

    /// Connector and mode, for the row's second line; the connector drops out when it is the label.
    public var detail: String {
        [connector.flatMap { $0 == label ? nil : $0 }, mode].compactMap { $0 }
            .joined(separator: " \u{b7} ")
    }

    /// Why the row can't be picked now, shown under it; nil for a pickable row.
    public var reason: String? {
        if let inUseBy { return L10n.screenInUse(device: inUseBy) }
        return enabled ? nil : String(localized: L10n.screenOff)
    }
}

/// How this device uses the host's screens when it streams its own: the host's monitors off,
/// on with apps opening here, or on with this one more screen beside them.
public enum ScreenRole: String, Codable, CaseIterable, Hashable, Sendable {
    case only, main, second

    /// The `topology` a pick of this role writes.
    public var topology: String {
        switch self {
        case .only: "exclusive"
        case .main: "primary"
        case .second: "extend"
        }
    }

    public var label: String {
        switch self {
        case .only: String(localized: L10n.screenRoleOnly)
        case .main: String(localized: L10n.screenRoleMain)
        case .second: String(localized: L10n.screenRoleSecond)
        }
    }

    public var hint: String {
        switch self {
        case .only: String(localized: L10n.screenRoleOnlyHint)
        case .main: String(localized: L10n.screenRoleMainHint)
        case .second: String(localized: L10n.screenRoleSecondHint)
        }
    }
}

/// The answer's `role`: present only where this device's own screen can take a role. It never
/// fails the answer: a word this build doesn't know drops out of `offered`, reads as nil in
/// `yours` and as ``ScreenRole/only`` in `hostDefault`.
public struct RoleInfo: Codable, Hashable, Sendable {
    /// The role of this device's own pick; nil follows ``hostDefault``.
    public var yours: ScreenRole?
    public var hostDefault: ScreenRole
    /// The roles this host can give, in any order.
    public var offered: [ScreenRole]

    private enum CodingKeys: String, CodingKey {
        case yours, offered
        case hostDefault = "host_default"
    }

    /// One word of the answer; nil when it is not one this build knows.
    private struct Word: Decodable {
        let role: ScreenRole?
        init(from decoder: Decoder) throws {
            role = (try? decoder.singleValueContainer().decode(String.self))
                .flatMap(ScreenRole.init(rawValue:))
        }
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        yours = (try? c.decodeIfPresent(Word.self, forKey: .yours))?.role
        hostDefault = (try? c.decodeIfPresent(Word.self, forKey: .hostDefault))?.role ?? .only
        offered = (try? c.decodeIfPresent([Word].self, forKey: .offered))?.compactMap(\.role) ?? []
    }

    /// The role a connect on the own screen gets now.
    public var current: ScreenRole { yours ?? hostDefault }
}

/// The body of `PUT /display/clients/{fp}`: the screen, and for a role its topology.
public struct ScreenPick: Codable, Hashable, Sendable {
    public var screen: DisplayTarget
    /// Left out of the body when nil: the host keeps the row's topology.
    public var topology: String?

    public init(screen: DisplayTarget, topology: String? = nil) {
        self.screen = screen
        self.topology = topology
    }
}

/// One row of the Screen section, worded. A surface draws `label`, and `reason`, else `hint`,
/// under it. `header`, when set, heads a group from this row on.
public struct ScreenRow: Hashable, Sendable, Identifiable {
    public enum Kind: Hashable, Sendable {
        case role(ScreenRole)
        case monitor
        /// The own-screen row of a host that sends no `role`.
        case own
    }

    public var kind: Kind
    /// `only` · `main` · `second`, a connector, or `own`: stable across refreshes.
    public var id: String
    public var label: String
    public var hint: String?
    /// A monitor's connector and mode; empty for the rest.
    public var detail: String
    public var header: String?
    /// What a connect gets now.
    public var checked: Bool
    /// False draws the row greyed: a head off or taken, or the host decides.
    public var pickable: Bool
    /// Why a monitor can't be streamed now.
    public var reason: String?
    /// What picking the row writes; nil when it is checked or not pickable.
    public var pick: ScreenPick?

    init(role: ScreenRole, checked: Bool) {
        kind = .role(role)
        id = role.rawValue
        label = role.label
        hint = role.hint
        detail = ""
        self.checked = checked
        pickable = true
        pick = ScreenPick(screen: .own, topology: role.topology)
    }

    init(_ t: ScreenTarget, current: DisplayTarget) {
        kind = t.id == ScreenTarget.ownID ? .own : .monitor
        id = t.id
        label = t.name
        detail = t.detail
        checked = t.target == current
        reason = t.reason
        pickable = reason == nil
        pick = ScreenPick(screen: t.target)
    }
}

/// `GET /api/v1/display/targets` for this device.
public struct DisplayTargets: Codable, Hashable, Sendable {
    /// The host's settings file pins the screen: the rows are not offered.
    public var locked: Bool
    /// This device may pick. A host that omits it predates the grant and lets every device.
    public var mayChoose: Bool
    public var hostDefault: DisplayTarget
    /// This device's own row; nil follows ``hostDefault``.
    public var yours: DisplayTarget?
    public var targets: [ScreenTarget]
    public var role: RoleInfo?

    private enum CodingKeys: String, CodingKey {
        case locked, yours, targets, role
        case mayChoose = "may_choose"
        case hostDefault = "host_default"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        locked = try c.decodeIfPresent(Bool.self, forKey: .locked) ?? false
        mayChoose = try c.decodeIfPresent(Bool.self, forKey: .mayChoose) ?? true
        hostDefault = try c.decodeIfPresent(DisplayTarget.self, forKey: .hostDefault) ?? .own
        yours = try c.decodeIfPresent(DisplayTarget.self, forKey: .yours)
        targets = try c.decodeIfPresent([ScreenTarget].self, forKey: .targets) ?? []
        // Anything but an object is no role.
        role = try? c.decodeIfPresent(RoleInfo.self, forKey: .role)
    }

    /// The pick a connect gets now: this device's row, else the host's default.
    public var current: DisplayTarget { yours ?? hostDefault }

    /// Why nothing in ``rows`` can be picked: the settings file, else the missing grant.
    public var note: String? {
        if locked { return String(localized: L10n.screenLockedHint) }
        return mayChoose ? nil : String(localized: L10n.screenHostDecides)
    }

    /// The Screen section. With `role`: the offered roles as only, main, second, then the
    /// monitors under "Show a monitor", the own row left out. Without: one row per target in the
    /// host's order. Checked is the monitor this device streams, else its role. Under a ``note``
    /// only the checked row stays, greyed.
    public var rows: [ScreenRow] {
        let current = self.current
        var rows: [ScreenRow]
        if let role {
            let roles = ScreenRole.allCases.filter { role.offered.contains($0) }.map {
                ScreenRow(role: $0, checked: current == .own && $0 == role.current)
            }
            let monitors = targets.filter { $0.id != ScreenTarget.ownID }.enumerated().map {
                i, t -> ScreenRow in
                var row = ScreenRow(t, current: current)
                if i == 0 { row.header = String(localized: L10n.screenShowMonitor) }
                return row
            }
            rows = roles + monitors
        } else {
            rows = targets.map { ScreenRow($0, current: current) }
        }
        let decides = note != nil
        if decides { rows.removeAll { !$0.checked } }
        for i in rows.indices {
            if decides { (rows[i].pickable, rows[i].header) = (false, nil) }
            if rows[i].checked || !rows[i].pickable { rows[i].pick = nil }
        }
        return rows
    }

    /// The card's line: "Streams LG TV" for a monitor, the role's name for a main or second
    /// screen. nil for the only screen and an older host's own screen: the default is silent.
    public var cardLine: String? {
        switch current {
        case .monitor(let connector):
            let row = targets.first { $0.id == connector }
            return L10n.screenStreams(screen: row?.name ?? connector)
        case .own:
            guard let role = role?.current, role != .only else { return nil }
            return role.label
        }
    }
}

/// The host's own sentence for a refused pick.
struct ScreenPickRefused: LocalizedError {
    let reason: String
    var errorDescription: String? { reason }
}

extension LibraryClient {
    /// `GET /api/v1/display/targets`. nil on any miss, like ``actions(_:)`` is empty: a host
    /// without the route has no Screen section.
    public static func targets(_ target: MgmtTarget) async -> DisplayTargets? {
        guard let response = try? await send("/api/v1/display/targets", to: target),
              response.status == 200
        else { return nil }
        return try? JSONDecoder().decode(DisplayTargets.self, from: response.body)
    }

    /// `PUT /api/v1/display/clients/{this device's fingerprint}` with a row's ``ScreenPick``. The
    /// caller fetches the rows again: the host's answer is the pick.
    public static func setScreen(_ pick: ScreenPick, on target: MgmtTarget) async throws {
        let identity = ClientIdentity(certPEM: target.certPEM, keyPEM: target.keyPEM)
        guard let device = identity.fingerprint else { throw MgmtTargetMissing.keychain }
        let body = try JSONEncoder().encode(pick)
        let response = try await send(
            "/api/v1/display/clients/\(MgmtTransport.hex(device))", to: target, method: "PUT",
            body: (body, "application/json"))
        if ![200, 204, 401].contains(response.status), let why = hostReason(response.body) {
            throw ScreenPickRefused(reason: why)
        }
        try expectOK(response, [200, 204])
    }
}
