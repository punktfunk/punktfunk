// What a saved host's card and page offer. `HostActions` builds a card's acts from
// `HostOffers.resolve`, so the grid, the host page and the Mac's host window offer one set. Pure,
// so `swift test` checks every row (`HostOffersTests`).

import PunktfunkShared

/// The acts a saved host offers beyond connect, pair, forget and remove.
public struct HostOffers: OptionSet, Sendable {
    public let rawValue: Int
    public init(rawValue: Int) { self.rawValue = rawValue }

    public static let browse = HostOffers(rawValue: 1 << 0)
    public static let speedTest = HostOffers(rawValue: 1 << 1)
    public static let sendLogs = HostOffers(rawValue: 1 << 2)
    public static let wake = HostOffers(rawValue: 1 << 3)
    public static let switchProfile = HostOffers(rawValue: 1 << 4)
    public static let details = HostOffers(rawValue: 1 << 5)
    public static let power = HostOffers(rawValue: 1 << 6)

    /// What `host` offers. A pinned card is a shortcut, not a second host: it carries no host
    /// acts. Library, speed test and logs dial with the pinned identity, so they wait for a
    /// pairing; the demo host has no speed test, log inbox or profiles. `canWake` is
    /// `WakePolicy.canWake` for the host's MACs.
    public static func resolve(
        host: StoredHost, pinned: Bool, online: Bool, isDemo: Bool, canWake: Bool
    ) -> HostOffers {
        let paired = host.pinnedSHA256 != nil
        var offers: HostOffers = []
        if paired { offers.insert(.browse) }
        if paired && !isDemo { offers.formUnion([.speedTest, .sendLogs]) }
        guard !pinned else { return offers }
        offers.formUnion([.details, .power])
        if !online && canWake { offers.insert(.wake) }
        if paired && !isDemo && host.pickedProfile != nil { offers.insert(.switchProfile) }
        return offers
    }
}
