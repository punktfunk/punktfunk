// Profiles on a box: what `GET /api/v1/profiles/enumerate` lists, when a connect shows the picker,
// and which id its hello carries. A port of `crates/client/pf-client-core/src/profiles.rs`; both run
// `clients/shared/profile-picker-vectors.json`, so `pickerDecision` cannot drift from the rule
// the other clients follow. The saved pick is `StoredHost.pickedProfile`: shown on the card,
// never applied unseen.

import Foundation

/// The pick a device saves for a box. Shown on the card, so it carries the name and the picture's
/// URL. `remember`: the player asked this device to keep it; the console applies only such a pick.
public struct ProfilePick: Codable, Hashable, Sendable {
    public var id: String
    public var displayName: String
    /// Host-relative URL of the picture.
    public var avatar: String?
    public var remember: Bool

    public init(
        id: String, displayName: String = "", avatar: String? = nil, remember: Bool = false
    ) {
        self.id = id
        self.displayName = displayName
        self.avatar = avatar
        self.remember = remember
    }

    private enum CodingKeys: String, CodingKey {
        case id, avatar
        case displayName = "display_name"
        case remember
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        displayName = try c.decodeIfPresent(String.self, forKey: .displayName) ?? ""
        avatar = try c.decodeIfPresent(String.self, forKey: .avatar)
        remember = try c.decodeIfPresent(Bool.self, forKey: .remember) ?? false
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(displayName, forKey: .displayName)
        try c.encodeIfPresent(avatar, forKey: .avatar)
        if remember { try c.encode(true, forKey: .remember) }
    }

    /// The console bridge's `{id, display_name, avatar, remember}`; the last two only when set.
    public var bridgeJSON: [String: Any] {
        var json: [String: Any] = ["id": id, "display_name": displayName]
        if let avatar { json["avatar"] = avatar }
        if remember { json["remember"] = true }
        return json
    }
}

/// A seat profile's state right now. A word the host adds later reads as `.other`.
public enum SeatState: String, Codable, Sendable {
    case ready, starting, stopped, occupied, unavailable, other
}

/// A profile's own seat. Absent for one that plays on the box's own session.
public struct ProfileSeat: Codable, Hashable, Sendable {
    public var state: SeatState = .ready
    /// The progress line while starting, or why while unavailable.
    public var detail: String?
    /// The device playing on it while occupied.
    public var occupant: String?
    /// Its Steam has no account yet. nil where the host can't tell.
    public var steamSignIn: Bool?

    private enum CodingKeys: String, CodingKey {
        case state, detail, occupant
        case steamSignIn = "steam_sign_in"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let word = try c.decodeIfPresent(String.self, forKey: .state) ?? "ready"
        state = SeatState(rawValue: word) ?? .other
        detail = try c.decodeIfPresent(String.self, forKey: .detail)
        occupant = try c.decodeIfPresent(String.self, forKey: .occupant)
        steamSignIn = try c.decodeIfPresent(Bool.self, forKey: .steamSignIn)
    }

    public init(state: SeatState = .ready, detail: String? = nil, occupant: String? = nil,
                steamSignIn: Bool? = nil) {
        self.state = state
        self.detail = detail
        self.occupant = occupant
        self.steamSignIn = steamSignIn
    }
}

/// One row of `enumerate`. Every field defaults, so a host that adds one never fails the list.
public struct ListedProfile: Codable, Hashable, Sendable, Identifiable {
    public var id: String = ""
    public var displayName: String = ""
    /// `#RRGGBB` behind the initials.
    public var accent: String?
    /// Host-relative URL of the picture.
    public var avatar: String?
    public var owner: Bool = false
    public var seat: ProfileSeat?
    /// This device's old seat became this profile.
    public var legacySeat: Bool = false

    private enum CodingKeys: String, CodingKey {
        case id, accent, avatar, owner, seat
        case displayName = "display_name"
        case legacySeat = "legacy_seat"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decodeIfPresent(String.self, forKey: .id) ?? ""
        displayName = try c.decodeIfPresent(String.self, forKey: .displayName) ?? ""
        accent = try c.decodeIfPresent(String.self, forKey: .accent)
        avatar = try c.decodeIfPresent(String.self, forKey: .avatar)
        owner = try c.decodeIfPresent(Bool.self, forKey: .owner) ?? false
        seat = try c.decodeIfPresent(ProfileSeat.self, forKey: .seat)
        legacySeat = try c.decodeIfPresent(Bool.self, forKey: .legacySeat) ?? false
    }

    public init(id: String, displayName: String, accent: String? = nil, avatar: String? = nil,
                owner: Bool = false, seat: ProfileSeat? = nil, legacySeat: Bool = false) {
        self.id = id
        self.displayName = displayName
        self.accent = accent
        self.avatar = avatar
        self.owner = owner
        self.seat = seat
        self.legacySeat = legacySeat
    }

    public var pick: ProfilePick { ProfilePick(id: id, displayName: displayName, avatar: avatar) }

    /// The one line under a picker card, if any.
    public var note: String? {
        guard let seat else { return nil }
        switch seat.state {
        case .occupied:
            return seat.occupant.map { "In use by \($0)" } ?? "In use"
        case .starting:
            return seat.detail ?? "Getting ready…"
        case .unavailable:
            return seat.detail ?? "Unavailable"
        default:
            return seat.steamSignIn == true ? "Steam sign-in once" : nil
        }
    }
}

public enum HostProfiles {
    /// Initials for a profile without a picture: the first letters of its first two words.
    public static func initials(_ name: String) -> String {
        name.split(whereSeparator: \.isWhitespace).prefix(2)
            .compactMap { $0.unicodeScalars.first.map { String($0).uppercased() } }
            .joined()
    }

    /// The profile `wanted` names in `listed`: its id, else its name in any ASCII case.
    public static func find(_ listed: [ListedProfile], _ wanted: String) -> ListedProfile? {
        if let byID = listed.first(where: { $0.id == wanted }) { return byID }
        let named = listed.filter { asciiLower($0.displayName) == asciiLower(wanted) }
        // Two profiles can't share a name, but a hand-edited file could: refuse to guess.
        return named.count == 1 ? named[0] : nil
    }

    private static func asciiLower(_ s: String) -> String {
        String(String.UnicodeScalarView(s.unicodeScalars.map {
            ("A"..."Z").contains($0) ? Unicode.Scalar($0.value + 32)! : $0
        }))
    }

    /// What a client does before it dials.
    public struct Decision: Equatable, Sendable {
        /// Show the picker; the connect waits for a pick.
        public var picker = false
        /// The id the hello carries. nil sends none.
        public var send: String?
        /// The name of a remembered profile the box no longer lists, for the picker's line.
        public var gone: String?
        /// The saved pick afterwards.
        public var remember: ProfilePick?
    }

    /// `listed` is nil for a box without profiles. `link` is a link's `as=`: it wins for this
    /// connect and leaves the saved pick alone.
    public static func pickerDecision(
        listed: [ListedProfile]?, remembered: ProfilePick?, link: String?
    ) -> Decision {
        guard let listed else {
            return Decision(send: link, remember: remembered)
        }
        let still = remembered.flatMap { r in listed.first { $0.id == r.id } }
        // A kept pick keeps `remember`: saving it back never clears the console's.
        let kept = still.map { p -> ProfilePick in
            var pick = p.pick
            pick.remember = remembered?.remember ?? false
            return pick
        }
        if let wanted = link {
            guard let p = find(listed, wanted) else {
                return Decision(picker: true, remember: kept)
            }
            return Decision(send: p.id, remember: kept)
        }
        if listed.count == 1 {
            return Decision(send: listed[0].id, remember: kept)
        }
        if let p = still {
            return Decision(send: p.id, remember: kept)
        }
        if remembered == nil, let p = listed.first(where: \.legacySeat) {
            return Decision(send: p.id, remember: p.pick)
        }
        return Decision(picker: true, gone: remembered?.displayName)
    }

    /// What a client does with a picked profile's seat before it dials.
    public enum SeatGate: Equatable, Sendable {
        /// Dial now. The host places the connect, or says why it can't.
        case dial
        /// `POST /api/v1/profiles/{id}/wake`, then wait.
        case wake
        /// The seat is coming up. Poll the list every 2 s, show `detail`, offer Cancel; there is
        /// no timeout.
        case wait(detail: String?)
        /// The seat can't play now; the line says why. Don't dial.
        case refuse(String)
    }

    /// The gate for `p`'s seat, as `enumerate` lists it.
    public static func seatGate(_ p: ListedProfile) -> SeatGate {
        guard let seat = p.seat else { return .dial }
        switch seat.state {
        case .ready, .occupied, .other: return .dial
        case .stopped: return .wake
        case .starting: return .wait(detail: seat.detail)
        case .unavailable:
            return .refuse(seat.detail ?? "That profile can't play on this host right now.")
        }
    }

    /// The line while a seat comes up: `Getting Kid's desk ready…`.
    public static func wakingLine(_ displayName: String) -> String {
        "Getting \(displayName)'s desk ready…"
    }
}
