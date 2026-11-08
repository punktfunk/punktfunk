// Which host screen this device streams (`design/screen-pick.md`): the rows the host offers it
// and the write of its own pick. The Swift twin of `pf-client-core::display_targets`. The host
// resolves every row for the caller; this client renders them and never builds a label.

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

/// `GET /api/v1/display/targets` for this device.
public struct DisplayTargets: Codable, Hashable, Sendable {
    /// The host's settings file pins the screen: the rows are not offered.
    public var locked: Bool
    public var hostDefault: DisplayTarget
    /// This device's own row; nil follows ``hostDefault``.
    public var yours: DisplayTarget?
    public var targets: [ScreenTarget]

    private enum CodingKeys: String, CodingKey {
        case locked, yours, targets
        case hostDefault = "host_default"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        locked = try c.decodeIfPresent(Bool.self, forKey: .locked) ?? false
        hostDefault = try c.decodeIfPresent(DisplayTarget.self, forKey: .hostDefault) ?? .own
        yours = try c.decodeIfPresent(DisplayTarget.self, forKey: .yours)
        targets = try c.decodeIfPresent([ScreenTarget].self, forKey: .targets) ?? []
    }

    /// The pick a connect gets now: this device's row, else the host's default.
    public var current: DisplayTarget { yours ?? hostDefault }

    /// The card's line: "Streams LG TV" for a monitor, the own row's word for its own screen.
    public var summary: String {
        switch current {
        case .own:
            return String(localized: L10n.screenOwn)
        case .monitor(let connector):
            let row = targets.first { $0.id == connector }
            return L10n.screenStreams(screen: row?.name ?? connector)
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

    /// `PUT /api/v1/display/clients/{this device's fingerprint}` with `{"screen": …}`. The caller
    /// fetches the rows again: the host's answer is the pick.
    public static func setScreen(_ screen: DisplayTarget, on target: MgmtTarget) async throws {
        let identity = ClientIdentity(certPEM: target.certPEM, keyPEM: target.keyPEM)
        guard let device = identity.fingerprint else { throw MgmtTargetMissing.keychain }
        let body = try JSONEncoder().encode(["screen": screen])
        let response = try await send(
            "/api/v1/display/clients/\(MgmtTransport.hex(device))", to: target, method: "PUT",
            body: (body, "application/json"))
        if ![200, 204, 401].contains(response.status), let why = hostReason(response.body) {
            throw ScreenPickRefused(reason: why)
        }
        try expectOK(response, [200, 204])
    }
}
