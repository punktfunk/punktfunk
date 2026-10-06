// The saved-host model and its JSON, shared with the widget, so both live in the dependency-free
// module; `HostStore` and the discovery-join helpers stay in the app. The JSON is a contract the
// `PunktfunkSharedTests` round trip pins: rename a `CodingKeys` case only while still reading
// its old key; never make a stored `Optional` non-optional, or older saved JSON stops decoding.

import Foundation

/// The management-API port default (distinct from the data-plane `port`). Lives here (not in
/// PunktfunkKit's LibraryClient, which re-exports it) so `StoredHost.effectiveMgmtPort` can resolve
/// it without the shared module taking a dependency on the kit.
public let punktfunkDefaultMgmtPort: UInt16 = 47990

public struct StoredHost: Identifiable, Codable, Hashable, Sendable {
    public var id = UUID()
    public var name: String
    public var address: String
    public var port: UInt16 = 9777
    /// SHA-256 of the host's certificate, set after the user explicitly trusted it.
    public var pinnedSHA256: Data?
    /// Last time a streaming session actually started (nil until the first one).
    public var lastConnected: Date?
    /// Management-API port for the library browser (distinct from the data-plane `port`). Optional
    /// (NOT a defaulted non-optional) so older saved hosts — whose JSON lacks this key — still
    /// decode: `init(from:)` reads each Optional with `decodeIfPresent`. Resolve via
    /// `effectiveMgmtPort`. (Auth is mTLS by the pinned identity — no token.)
    public var mgmtPort: UInt16?
    /// Wake-on-LAN MAC address(es) of the host's wake-capable NIC(s), each `aa:bb:cc:dd:ee:ff`.
    /// Learned from the host's mDNS `mac` TXT record while it's awake and persisted here, so the
    /// client can send a magic packet to wake the host later (when it's asleep and no longer
    /// advertising). Optional (same forward-compat reason as `mgmtPort`); nil until first learned.
    public var macAddresses: [String]?
    /// Share the clipboard with this host (macOS and iOS sessions; tvOS has no pasteboard — see
    /// design/clipboard-and-file-transfer.md §5.3). Opt-in per host: nil/false = off (nil also
    /// keeps older saved JSON decoding — same forward-compat reason as `mgmtPort`). Honored only
    /// when the host advertises `HOST_CAP_CLIPBOARD`.
    public var clipboardSync: Bool?
    /// This host's default settings preset (`StreamPreset.id`) — what a plain click/tap uses.
    /// nil, or an id whose preset was deleted, resolves as "Default settings", i.e. exactly
    /// today's behaviour: a dangling binding is never an error and never blocks a connect
    /// (design/client-settings-profiles.md §4.4). Optional and appended last for the same
    /// widget-contract reason as `mgmtPort`.
    public var presetID: String?
    /// Presets pinned as additional cards for this host (design §5.2a), in card order. NOT the
    /// default — that is `presetID`; a pin is presentation only, and duplicates and dangling ids
    /// are dropped when the cards are built. Optional for the same forward-compat reason.
    public var pinnedPresetIDs: [String]?
    /// When this host was saved — what the grid's "Date Added" sort orders by. Optional and
    /// appended last for the same widget-contract reason as the rest; hosts saved before it
    /// existed have none, and keep their stored order, which IS the order they were added in.
    public var addedAt: Date?
    /// The host's OS-identity chain (`windows` | `linux/<family>/<id>`, ...) learned from its
    /// mDNS `os` TXT while online, so the card's OS mark survives the host going to sleep.
    /// Optional and appended last for the same widget-contract reason; nil until first learned.
    public var osChain: String?
    /// Addresses this host was moved away from automatically, newest first, at most
    /// `maxPreviousAddresses`. A host lives at more than one — its LAN lease at home, a
    /// Tailscale address anywhere — so when `address` goes silent the sweep asks these too.
    /// Optional and appended last for the same widget-contract reason; nil until the first move.
    public var previousAddresses: [String]?
    /// The delivery profile to ask this host for (1 capped, 2 smooth), set from a network check's
    /// finding. Per host: a Wi-Fi TV and a wired desk differ. nil asks nothing, and nil is what an
    /// older saved record decodes to.
    public var delivery: Int?
    /// Library title id → preset id: what a launch of that title streams with, beating
    /// `presetID`. A dangling id falls through to `presetID`. nil until the first binding.
    public var gamePresets: [String: String]?

    /// How many left-behind addresses a host keeps.
    public static let maxPreviousAddresses = 3

    public init(
        id: UUID = UUID(), name: String, address: String, port: UInt16 = 9777,
        pinnedSHA256: Data? = nil, lastConnected: Date? = nil, mgmtPort: UInt16? = nil,
        macAddresses: [String]? = nil, clipboardSync: Bool? = nil,
        presetID: String? = nil, pinnedPresetIDs: [String]? = nil, addedAt: Date? = nil,
        osChain: String? = nil, previousAddresses: [String]? = nil,
        gamePresets: [String: String]? = nil
    ) {
        self.id = id
        self.name = name
        self.address = address
        self.port = port
        self.pinnedSHA256 = pinnedSHA256
        self.lastConnected = lastConnected
        self.mgmtPort = mgmtPort
        self.macAddresses = macAddresses
        self.clipboardSync = clipboardSync
        self.presetID = presetID
        self.pinnedPresetIDs = pinnedPresetIDs
        self.addedAt = addedAt
        self.osChain = osChain
        self.previousAddresses = previousAddresses
        self.gamePresets = gamePresets
    }

    private enum CodingKeys: String, CodingKey {
        case id, name, address, port, pinnedSHA256, lastConnected, mgmtPort, macAddresses
        case clipboardSync, presetID, pinnedPresetIDs, addedAt, osChain, previousAddresses
        case gamePresets
        /// Pre-rename keys (design/preset-rename.md): read when the new key is absent, and
        /// written beside it so an older build keeps the bindings.
        case profileID, pinnedProfileIDs
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UUID.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        address = try c.decode(String.self, forKey: .address)
        port = try c.decode(UInt16.self, forKey: .port)
        pinnedSHA256 = try c.decodeIfPresent(Data.self, forKey: .pinnedSHA256)
        lastConnected = try c.decodeIfPresent(Date.self, forKey: .lastConnected)
        mgmtPort = try c.decodeIfPresent(UInt16.self, forKey: .mgmtPort)
        macAddresses = try c.decodeIfPresent([String].self, forKey: .macAddresses)
        clipboardSync = try c.decodeIfPresent(Bool.self, forKey: .clipboardSync)
        presetID = try c.decodeIfPresent(String.self, forKey: .presetID)
            ?? c.decodeIfPresent(String.self, forKey: .profileID)
        pinnedPresetIDs = try c.decodeIfPresent([String].self, forKey: .pinnedPresetIDs)
            ?? c.decodeIfPresent([String].self, forKey: .pinnedProfileIDs)
        addedAt = try c.decodeIfPresent(Date.self, forKey: .addedAt)
        osChain = try c.decodeIfPresent(String.self, forKey: .osChain)
        previousAddresses = try c.decodeIfPresent([String].self, forKey: .previousAddresses)
        gamePresets = try c.decodeIfPresent([String: String].self, forKey: .gamePresets)
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(name, forKey: .name)
        try c.encode(address, forKey: .address)
        try c.encode(port, forKey: .port)
        try c.encodeIfPresent(pinnedSHA256, forKey: .pinnedSHA256)
        try c.encodeIfPresent(lastConnected, forKey: .lastConnected)
        try c.encodeIfPresent(mgmtPort, forKey: .mgmtPort)
        try c.encodeIfPresent(macAddresses, forKey: .macAddresses)
        try c.encodeIfPresent(clipboardSync, forKey: .clipboardSync)
        try c.encodeIfPresent(presetID, forKey: .presetID)
        try c.encodeIfPresent(pinnedPresetIDs, forKey: .pinnedPresetIDs)
        try c.encodeIfPresent(presetID, forKey: .profileID)
        try c.encodeIfPresent(pinnedPresetIDs, forKey: .pinnedProfileIDs)
        try c.encodeIfPresent(addedAt, forKey: .addedAt)
        try c.encodeIfPresent(osChain, forKey: .osChain)
        try c.encodeIfPresent(previousAddresses, forKey: .previousAddresses)
        try c.encodeIfPresent(gamePresets, forKey: .gamePresets)
    }

    public var displayName: String { name.isEmpty ? address : name }
    public var effectiveMgmtPort: UInt16 { mgmtPort ?? punktfunkDefaultMgmtPort }
    /// Wake-capable, in a form the wake helper accepts (empty when none learned yet).
    public var wakeMacs: [String] { macAddresses ?? [] }

    /// Re-point at `address`:`port`, remembering the address it leaves.
    public mutating func move(to address: String, port: UInt16) {
        let old = self.address
        var previous = (previousAddresses ?? []).filter { $0 != address && $0 != old }
        if old != address { previous.insert(old, at: 0) }
        previousAddresses = previous.isEmpty ? nil : Array(previous.prefix(Self.maxPreviousAddresses))
        self.address = address
        self.port = port
    }
}

public extension StoredHost {
    /// Every saved host from the shared suite.
    ///
    /// One decoder for the app, its intents and both widgets: the same four lines existed in four
    /// places. `recentFirst` is explicit at every call site because two of those copies sorted and
    /// two did not — the STORE must keep its own order, which is card order, while a widget picking
    /// "the last host you used" wants recency.
    ///
    /// Decoded PER ELEMENT: one unreadable record would otherwise lose the whole array, and the
    /// caller that reacts by persisting an empty one erases every saved host permanently.
    static func loadAll(
        from defaults: UserDefaults = AppGroup.defaults, recentFirst: Bool
    ) -> [StoredHost] {
        guard let data = defaults.data(forKey: DefaultsKey.hosts) else { return [] }
        let decoder = JSONDecoder()
        let hosts: [StoredHost]
        if let all = try? decoder.decode([StoredHost].self, from: data) {
            hosts = all
        } else if let raw = try? decoder.decode([FailableHost].self, from: data) {
            hosts = raw.compactMap(\.host)
        } else {
            return []
        }
        guard recentFirst else { return hosts }
        return hosts.sorted {
            ($0.lastConnected ?? .distantPast) > ($1.lastConnected ?? .distantPast)
        }
    }
}

/// Decodes a host, or nothing, without failing its container — see `loadAll`.
private struct FailableHost: Decodable {
    let host: StoredHost?
    init(from decoder: Decoder) throws {
        host = try? StoredHost(from: decoder)
    }
}
