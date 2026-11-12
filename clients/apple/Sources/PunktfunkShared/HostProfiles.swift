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
    /// The progress line while starting, or why while unavailable. When the host names the
    /// message (`detail_id`) this is the app's wording of it, else the host's English.
    public var detail: String?
    /// The device playing on it while occupied.
    public var occupant: String?
    /// Its Steam has no account yet. nil where the host can't tell.
    public var steamSignIn: Bool?

    private enum CodingKeys: String, CodingKey {
        case state, detail, occupant
        case steamSignIn = "steam_sign_in"
    }

    /// What the host names beside `detail`; read, never kept.
    private enum MessageKeys: String, CodingKey {
        case detailId = "detail_id"
        case detailParams = "detail_params"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let word = try c.decodeIfPresent(String.self, forKey: .state) ?? "ready"
        state = SeatState(rawValue: word) ?? .other
        let english = try c.decodeIfPresent(String.self, forKey: .detail)
        let message = try decoder.container(keyedBy: MessageKeys.self)
        if let id = try message.decodeIfPresent(String.self, forKey: .detailId) {
            let params = (try? message.decode([String: String].self, forKey: .detailParams)) ?? [:]
            let said = HostText.word(id, params: params, english: english ?? "")
            detail = said.isEmpty ? nil : said
        } else {
            detail = english
        }
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

/// What a profile on a person's own account, or a locked one, needs from this device before it
/// opens. An old host sends none: `.none`. A word this build doesn't know reads as `.password`,
/// the prompt every client has.
public enum UnlockNeed: String, Codable, Sendable {
    case none, password, key, pin
    /// This device's unlock key, else the locked profile's PIN.
    case pinKey = "pin_key"

    /// The prompt asks for the profile's PIN, not an account password.
    public var pin: Bool { self == .pin || self == .pinKey }
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
    public var unlock: UnlockNeed = .none

    private enum CodingKeys: String, CodingKey {
        case id, accent, avatar, owner, seat, unlock
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
        let word = try c.decodeIfPresent(String.self, forKey: .unlock)
        unlock = word.map { UnlockNeed(rawValue: $0) ?? .password } ?? .none
    }

    public init(id: String, displayName: String, accent: String? = nil, avatar: String? = nil,
                owner: Bool = false, seat: ProfileSeat? = nil, legacySeat: Bool = false,
                unlock: UnlockNeed = .none) {
        self.id = id
        self.displayName = displayName
        self.accent = accent
        self.avatar = avatar
        self.owner = owner
        self.seat = seat
        self.legacySeat = legacySeat
        self.unlock = unlock
    }

    public var pick: ProfilePick { ProfilePick(id: id, displayName: displayName, avatar: avatar) }

    /// The one line under a picker card, if any.
    public var note: String? {
        guard let seat else { return nil }
        switch seat.state {
        case .occupied:
            return seat.occupant.map { L10n.profileInUseBy(device: $0) }
                ?? String(localized: L10n.profileInUse)
        case .starting:
            return seat.detail ?? String(localized: L10n.profileGettingReady)
        case .unavailable:
            return seat.detail ?? String(localized: L10n.profileUnavailable)
        default:
            return seat.steamSignIn == true ? String(localized: L10n.profileSteamSignIn) : nil
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

    /// The gate for `p`'s seat, as `enumerate` lists it. A locked profile's unlock starts its
    /// stopped seat; the host refuses it a wake.
    public static func seatGate(_ p: ListedProfile) -> SeatGate {
        guard let seat = p.seat else { return .dial }
        switch seat.state {
        case .ready, .occupied, .other: return .dial
        case .stopped: return p.unlock == .none ? .wake : .dial
        case .starting: return .wait(detail: seat.detail)
        case .unavailable:
            return .refuse(seat.detail ?? String(localized: L10n.rejectSeatUnavailable))
        }
    }

    /// The line while a seat comes up: `Getting Kid's desk ready…`.
    public static func wakingLine(_ displayName: String) -> String {
        L10n.profileWaking(name: displayName)
    }
}
