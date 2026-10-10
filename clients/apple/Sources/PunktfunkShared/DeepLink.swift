// The `punktfunk://` URL grammar — the single builder/parser shared by the widget (which emits
// links via `widgetURL`/`Link`), the App Intents (which round-trip through it rather than opening
// a second connect path) and the app (`ContentView.onOpenURL`, which routes them into the existing
// connect path). Keeping every side on one type means the wire format can't drift.
//
//   punktfunk://connect/<host-ref>[?fp=<64-hex>][&host=<addr[:port]>][&launch=<id>]
//                                 [&preset=<ref>][&as=<profile>][&name=<label>]
//
// The invariant the grammar exists to keep: **a URL may only ever do what a click on an existing
// card could do, minus trust decisions.** So it carries *references* to things that already exist
// on this device — a host record, a settings preset, a library id — and never values: no
// resolution, no bitrate, no codec. A web page must not be able to shape a session beyond picking
// among the user's own configurations. `pair` is deliberately not a route and never will be;
// pairing stays an interactive ceremony.
//
// `pf://` parses as an alias so a hand-typed or legacy link still works, but nothing ever *emits*
// or registers it (design/client-deep-links.md §2).
//
// This is a port of `crates/client/pf-client-core/src/deeplink.rs`, and the two are held together by
// `clients/shared/deeplink-vectors.json` — 44 cases including every refusal code, run verbatim by
// both test suites. Change one parser and the other's suite fails, which is the point: three
// parsers that drift are three different security postures.

import Foundation

/// Hostile-input caps (design §8). The total is generous for a real link and small enough that a
/// pasted megabyte never reaches the decoder.
public enum DeepLinkLimits {
    public static let url = 2048
    public static let hostRef = 128
    public static let launch = 128
    public static let preset = 64
    public static let name = 64
    public static let profile = 64
}

/// The default native port, as everywhere else in the clients.
public let punktfunkDefaultPort: UInt16 = 9777

/// What the URL asks for. `wake`/`browse` are reserved in the grammar and parse today; a front-end
/// that hasn't implemented them refuses with a notice rather than silently connecting — the
/// grammar is the contract, per-platform support is not.
public enum DeepLinkRoute: String, Equatable, Sendable {
    case connect, wake, browse
}

/// An `addr[:port]` pair carried by a link (the `host=` recovery parameter, or an address-shaped
/// reference).
public struct DeepLinkAddress: Equatable, Sendable {
    public var address: String
    public var port: UInt16

    public init(_ address: String, _ port: UInt16) {
        self.address = address
        self.port = port
    }
}

/// Why a URL was rejected. The `code` strings are the cross-language contract (the vector file
/// names them) — Rust and Kotlin report the same code for the same input.
public enum DeepLinkError: Error, Equatable, Sendable {
    /// Not a `punktfunk://` (or `pf://`) URL at all — the caller should ignore it, not warn.
    case notOurScheme
    case tooLong
    /// A route this grammar doesn't define.
    case unknownRoute(String)
    /// `punktfunk://pair/…` — pairing is an interactive ceremony, never a link.
    case pairRefused
    case missingHostRef
    /// A `%` escape that isn't two hex digits, or a decode that isn't UTF-8.
    case badEscape
    /// A control character survived decoding — no legitimate field contains one.
    case controlChar
    /// A parameter past its cap; carries the parameter name.
    case paramTooLong(String)
    /// `fp=` that isn't 64 hex characters.
    case badFingerprint
    /// `host=` that isn't `addr[:port]` with a parsable port.
    case badHostParam
    /// `launch=` outside the printable, shell-safe id charset the host and Decky agree on.
    case badLaunchID

    /// The stable code shared with the Rust/Kotlin ports and the vector file.
    public var code: String {
        switch self {
        case .notOurScheme: return "not-our-scheme"
        case .tooLong: return "too-long"
        case .unknownRoute: return "unknown-route"
        case .pairRefused: return "pair-refused"
        case .missingHostRef: return "missing-host-ref"
        case .badEscape: return "bad-escape"
        case .controlChar: return "control-char"
        case .paramTooLong: return "param-too-long"
        case .badFingerprint: return "bad-fingerprint"
        case .badHostParam: return "bad-host-param"
        case .badLaunchID: return "bad-launch-id"
        }
    }

    /// A sentence for the notice a refusing front-end shows. Deliberately names the failing
    /// reference: a shortcut that can't honor what it asked for says so instead of streaming with
    /// the wrong settings.
    public var message: String {
        switch self {
        case .notOurScheme: return "That isn't a Punktfunk link."
        case .tooLong: return "That link is too long to be genuine."
        case .unknownRoute(let r): return "Punktfunk links can't do “\(r)”."
        case .pairRefused:
            return "Pairing can't be done from a link — pair the host in Punktfunk first."
        case .missingHostRef: return "That link doesn't say which host to use."
        case .badEscape, .controlChar: return "That link is malformed and was ignored."
        case .paramTooLong(let p): return "That link's “\(p)” value is too long."
        case .badFingerprint: return "That link's host fingerprint isn't a valid one."
        case .badHostParam: return "That link's host address isn't valid."
        case .badLaunchID: return "That link's game id isn't a valid one."
        }
    }
}

/// A parsed, validated link. Every field is already length- and charset-checked, so a consumer
/// never has to re-validate hostile input; what it still has to do is *resolve* — the references
/// may name things that don't exist here.
public struct DeepLink: Equatable, Sendable {
    public var route: DeepLinkRoute = .connect
    /// The host reference as written: a stable record id, a host name, or `addr[:port]`.
    public var hostRef: String
    /// Expected host certificate fingerprint, lowercase hex (64 chars).
    public var fp: String?
    /// Recovery address for a stable id that no longer resolves (store wiped, reinstall).
    public var host: DeepLinkAddress?
    /// A store-qualified library id (`steam:570`) for the host to launch on arrival.
    public var launch: String?
    /// A settings-preset reference (id, or a unique name) — one-off, never rebinding.
    public var preset: String?
    /// The host profile to play as (id, or a unique name): this connect only, never saved.
    public var asProfile: String?
    /// Display label for the unknown-host confirmation sheet (external emitters).
    public var name: String?

    public static let scheme = "punktfunk"

    public init(
        route: DeepLinkRoute = .connect, hostRef: String, fp: String? = nil,
        host: DeepLinkAddress? = nil, launch: String? = nil, preset: String? = nil,
        asProfile: String? = nil, name: String? = nil
    ) {
        self.route = route
        self.hostRef = hostRef
        self.fp = fp
        self.host = host
        self.launch = launch
        self.preset = preset
        self.asProfile = asProfile
        self.name = name
    }

    // MARK: - Emission

    /// The canonical URL for this link — always `punktfunk://`, never the `pf://` alias.
    public var urlString: String {
        var s = "punktfunk://\(route.rawValue)/\(Self.encode(hostRef))"
        var separator = "?"
        func push(_ key: String, _ value: String) {
            s += separator + key + "=" + Self.encode(value)
            separator = "&"
        }
        if let fp { push("fp", fp) }
        if let host {
            let text: String
            if host.port == punktfunkDefaultPort {
                text = host.address
            } else if host.address.contains(":") {
                text = "[\(host.address)]:\(host.port)" // literal IPv6 needs its brackets back
            } else {
                text = "\(host.address):\(host.port)"
            }
            push("host", text)
        }
        if let launch { push("launch", launch) }
        // `profile=` as well, for clients that predate `preset=`.
        if let preset {
            push("preset", preset)
            push("profile", preset)
        }
        if let asProfile { push("as", asProfile) }
        if let name { push("name", name) }
        return s
    }

    /// Non-optional: every segment is percent-encoded on the way out, so the emitted string is
    /// always a valid URL — force-unwrapping a value we fully control.
    public var url: URL { URL(string: urlString)! }

    /// A plain connect link for a saved host — the shape the widget and the Connect intent emit.
    public static func connect(host: UUID, launchID: String? = nil, preset: String? = nil)
        -> DeepLink {
        DeepLink(
            hostRef: host.uuidString,
            launch: (launchID?.isEmpty ?? true) ? nil : launchID,
            preset: (preset?.isEmpty ?? true) ? nil : preset)
    }

    /// A library link for a saved host — the shape the library widget and the Open Library intent
    /// emit. Opens the host's game library without starting a session; a session begins only when
    /// the user picks a title there, through the normal connect path.
    public static func browse(host: UUID) -> DeepLink {
        DeepLink(route: .browse, hostRef: host.uuidString)
    }

    /// The self-emitted form for a saved host: id first (address-independent), with the address
    /// and pin alongside so the link degrades to a confirmation sheet instead of a dead click when
    /// the record is gone ("Copy link", and any shortcut written from a card).
    public static func forHost(
        _ host: StoredHost, launch: String? = nil, preset: String? = nil
    ) -> DeepLink {
        DeepLink(
            hostRef: host.id.uuidString,
            fp: host.pinnedSHA256.map(Self.hexLower),
            host: DeepLinkAddress(host.address, host.port),
            launch: (launch?.isEmpty ?? true) ? nil : launch,
            preset: (preset?.isEmpty ?? true) ? nil : preset)
    }

    // MARK: - Parsing

    /// Parse an incoming URL. Everything hostile is rejected here, once, for every front-end.
    public init(url: URL) throws {
        self = try DeepLink.parse(url.absoluteString)
    }

    public static func parse(_ text: String) throws -> DeepLink {
        guard text.utf8.count <= DeepLinkLimits.url else { throw DeepLinkError.tooLong }
        guard let separator = text.range(of: "://") else { throw DeepLinkError.notOurScheme }
        let scheme = text[text.startIndex..<separator.lowerBound].lowercased()
        guard scheme == "punktfunk" || scheme == "pf" else { throw DeepLinkError.notOurScheme }
        // A fragment is never part of this grammar; drop it rather than folding it into the last
        // parameter (where it would smuggle unvalidated text past the caps).
        let rest = String(text[separator.upperBound...].prefix { $0 != "#" })
        let path: Substring
        let query: Substring
        if let q = rest.firstIndex(of: "?") {
            path = rest[rest.startIndex..<q]
            query = rest[rest.index(after: q)...]
        } else {
            path = rest[...]
            query = ""
        }
        var trimmed = String(path)
        while trimmed.hasSuffix("/") { trimmed.removeLast() }

        let routeWord: String
        let hostRefRaw: String
        if let slash = trimmed.firstIndex(of: "/") {
            routeWord = String(trimmed[trimmed.startIndex..<slash])
            hostRefRaw = String(trimmed[trimmed.index(after: slash)...])
        } else if isRouteWord(trimmed) {
            // A single segment: a bare reference is unambiguous as long as it isn't one of the
            // route words — those stay routes (with a missing reference), so `punktfunk://pair`
            // refuses instead of hunting for a host called "pair".
            routeWord = trimmed
            hostRefRaw = ""
        } else {
            routeWord = "connect"
            hostRefRaw = trimmed
        }
        let lowered = routeWord.lowercased()
        if lowered == "pair" { throw DeepLinkError.pairRefused }
        guard let route = DeepLinkRoute(rawValue: lowered) else {
            throw DeepLinkError.unknownRoute(lowered)
        }

        let hostRef = try decode(hostRefRaw)
        guard !hostRef.isEmpty else { throw DeepLinkError.missingHostRef }
        guard hostRef.unicodeScalars.count <= DeepLinkLimits.hostRef else {
            throw DeepLinkError.paramTooLong("host-ref")
        }

        var link = DeepLink(route: route, hostRef: hostRef)
        var legacyPreset: String?
        for pair in query.split(separator: "&", omittingEmptySubsequences: true) {
            let rawKey: Substring
            let rawValue: Substring
            if let eq = pair.firstIndex(of: "=") {
                rawKey = pair[pair.startIndex..<eq]
                rawValue = pair[pair.index(after: eq)...]
            } else {
                rawKey = pair
                rawValue = ""
            }
            let key = try decode(String(rawKey)).lowercased()
            let value = try decode(String(rawValue))
            // `?launch=` with nothing after it is "not given", not an error.
            if value.isEmpty { continue }
            // First occurrence wins, and unknown keys are ignored: a newer emitter's parameter
            // must not turn an otherwise valid link into a refusal, and appending a second `fp=`
            // must not be able to override the first.
            switch key {
            case "fp" where link.fp == nil:
                let fp = value.lowercased()
                guard fp.count == 64, fp.allSatisfy({ $0.isASCII && $0.isHexDigit }) else {
                    throw DeepLinkError.badFingerprint
                }
                link.fp = fp
            case "host" where link.host == nil:
                guard let parsed = parseAddressPort(value) else { throw DeepLinkError.badHostParam }
                link.host = parsed
            case "launch" where link.launch == nil:
                guard value.utf8.count <= DeepLinkLimits.launch else {
                    throw DeepLinkError.paramTooLong("launch")
                }
                guard isSafeLaunchID(value) else { throw DeepLinkError.badLaunchID }
                link.launch = value
            case "preset" where link.preset == nil:
                guard value.unicodeScalars.count <= DeepLinkLimits.preset else {
                    throw DeepLinkError.paramTooLong("preset")
                }
                link.preset = value
            // The pre-rename spelling: `preset` wins wherever it sits.
            case "profile" where legacyPreset == nil:
                guard value.unicodeScalars.count <= DeepLinkLimits.preset else {
                    throw DeepLinkError.paramTooLong("profile")
                }
                legacyPreset = value
            case "as" where link.asProfile == nil:
                guard value.unicodeScalars.count <= DeepLinkLimits.profile else {
                    throw DeepLinkError.paramTooLong("as")
                }
                link.asProfile = value
            case "name" where link.name == nil:
                guard value.unicodeScalars.count <= DeepLinkLimits.name else {
                    throw DeepLinkError.paramTooLong("name")
                }
                link.name = value
            default:
                break
            }
        }
        if link.preset == nil { link.preset = legacyPreset }
        return link
    }

    // MARK: - Resolution

    /// True when this link's `fp` contradicts what we have pinned for that host — the link is
    /// stale or lying, and the only safe answer is a hard refusal (design §3.1).
    public func pinConflict(with host: StoredHost) -> Bool {
        guard let fp, let pin = host.pinnedSHA256 else { return false }
        return fp.lowercased() != Self.hexLower(pin)
    }

    /// Resolve this link's host reference against the local store, in the documented order:
    /// stable record id → unique case-insensitive name → `addr[:port]` literal, then the `host=`
    /// recovery path — a self-emitted shortcut that outlived the record it was written from still
    /// lands on the right box.
    ///
    /// Only the record id is UNGUESSABLE, so only the record id resolves to `.known`, the silent
    /// one-click contract. A display name is an mDNS instance name or a user label ("Gaming PC")
    /// and an address is a LAN address: anything that can open a URL can guess those, and a guess
    /// must not be able to start a stream (or launch a title). They resolve to `.confirm` — the
    /// same host, behind the user's OK.
    public func resolveHost(in hosts: [StoredHost]) -> HostResolution {
        let reference = hostRef.lowercased()
        if let match = hosts.first(where: { $0.id.uuidString.lowercased() == reference }) {
            return .known(match)
        }
        let byName = hosts.filter { !$0.name.isEmpty && $0.name.lowercased() == reference }
        if byName.count == 1 { return .confirm(byName[0]) }
        if byName.count > 1 { return .ambiguous }
        // `addr[:port]` literal, then the `host=` recovery parameter. At that address the link's
        // `fp` picks its record — a dual-boot box answers there with two pins — else only an
        // unpinned one stands in. The literal is only considered when the reference could BE an
        // address: a stale record id must fall through to `host=` (or to a refusal), never dial.
        let literal = Self.looksLikeAddress(hostRef) ? Self.parseAddressPort(hostRef) : nil
        let pin = fp?.lowercased()
        for candidate in [literal, host].compactMap({ $0 }) {
            let atAddress = hosts.filter {
                $0.address == candidate.address && $0.port == candidate.port
            }
            let match: StoredHost?
            if let pin {
                match = atAddress.first(where: { $0.pinnedSHA256.map(Self.hexLower) == pin })
                    ?? atAddress.first(where: { $0.pinnedSHA256 == nil })
            } else {
                match = atAddress.first
            }
            if let match { return .confirm(match) }
        }
        guard let target = literal ?? host else { return .unresolvable }
        return .unknown(address: target.address, port: target.port, name: name, fp: fp)
    }

    /// What the local host store made of a link's references.
    public enum HostResolution: Equatable, Sendable {
        /// A record we already have, named by its stable (unguessable) id: the one-click contract
        /// (subject to `pinConflict`).
        case known(StoredHost)
        /// The same record, named by something GUESSABLE — its display name, its address, or the
        /// `host=` recovery parameter. A link may not act on a guess, so this one waits for the
        /// user's confirmation; past that it is the `.known` path exactly.
        case confirm(StoredHost)
        /// No record, but the link says where to dial: the confirmation sheet's input, from which
        /// the normal pairing flow proceeds under the user's eyes. Never an auto-connect.
        case unknown(address: String, port: UInt16, name: String?, fp: String?)
        /// The name matched more than one saved host — refuse with a notice, never guess.
        case ambiguous
        /// A reference that resolves to nothing and carries no address to fall back on.
        case unresolvable
    }

    // MARK: - Grammar helpers (ports of the Rust originals — keep them in step)

    /// Could this reference be a network address (an IP literal or a host name) rather than a
    /// record id or a display name? Only then may an unmatched reference become "an unknown host
    /// at this address". A stable id that no longer resolves is NOT an address: offering to dial a
    /// UUID as a hostname would turn a wiped store into a dead end instead of `host=` recovery.
    static func looksLikeAddress(_ s: String) -> Bool {
        let chars = Array(s)
        let uuidShaped = chars.count == 36 && chars.enumerated().allSatisfy { i, c in
            [8, 13, 18, 23].contains(i) ? c == "-" : c.isHexDigit
        }
        guard !uuidShaped, !s.isEmpty else { return false }
        return s.allSatisfy { c in
            c.isASCII && (c.isLetter || c.isNumber || ".-_:[]".contains(c))
        }
    }

    /// The reserved first path segments — everything the grammar routes on, plus `pair`, which is
    /// reserved precisely so it can be refused rather than mistaken for a host name.
    private static func isRouteWord(_ s: String) -> Bool {
        ["connect", "wake", "browse", "pair"].contains(s.lowercased())
    }

    /// `addr`, `addr:port`, `[v6]`, `[v6]:port` — nil when the port isn't a number. A bare IPv6
    /// literal (`::1`) keeps its colons and takes the default port; anything else splits at the
    /// last colon, like every other host-parsing site in the clients.
    static func parseAddressPort(_ s: String) -> DeepLinkAddress? {
        guard !s.isEmpty else { return nil }
        if s.hasPrefix("[") {
            let body = s.dropFirst()
            guard let close = body.firstIndex(of: "]") else { return nil }
            let address = String(body[body.startIndex..<close])
            guard !address.isEmpty else { return nil }
            let tail = String(body[body.index(after: close)...])
            if tail.isEmpty { return DeepLinkAddress(address, punktfunkDefaultPort) }
            guard tail.hasPrefix(":"), let port = UInt16(tail.dropFirst()) else { return nil }
            return DeepLinkAddress(address, port)
        }
        guard let colon = s.lastIndex(of: ":") else {
            return DeepLinkAddress(s, punktfunkDefaultPort)
        }
        let head = String(s[s.startIndex..<colon])
        // `::1` and friends: the head still has a colon, so this isn't a port separator.
        if head.contains(":") { return DeepLinkAddress(s, punktfunkDefaultPort) }
        guard !head.isEmpty, let port = UInt16(s[s.index(after: colon)...]) else { return nil }
        return DeepLinkAddress(head, port)
    }

    /// The launch-id charset the whole product already agrees on: printable, non-space ASCII with
    /// no shell metacharacters (Decky rides ids through Steam launch options as an env token, so a
    /// quote or a backtick genuinely breaks something downstream). Validation only — the id is
    /// opaque and the host matches it verbatim against its own library.
    static func isSafeLaunchID(_ id: String) -> Bool {
        // `"`, `'`, `\`, `$`, backtick — spelled as bytes because a Swift literal holding all five
        // is its own little escaping puzzle, and this list must read exactly like the Rust one.
        let forbidden: Set<UInt8> = [0x22, 0x27, 0x5C, 0x24, 0x60]
        // No leading `-`: the id rides session argv, where `--browse` would read as a flag.
        return !id.isEmpty && !id.hasPrefix("-") && id.utf8.allSatisfy { (0x21...0x7e).contains($0) && !forbidden.contains($0) }
    }

    /// Strict percent-decoding: `%` must be followed by exactly two hex digits, the result must be
    /// UTF-8, and no control character may survive. Lenient decoders are how `%00`, a stray `\n`
    /// or a half-escape end up inside a filename or a log line.
    static func decode(_ s: String) throws -> String {
        let bytes = Array(s.utf8)
        var out: [UInt8] = []
        out.reserveCapacity(bytes.count)
        var i = 0
        while i < bytes.count {
            guard bytes[i] == UInt8(ascii: "%") else {
                out.append(bytes[i])
                i += 1
                continue
            }
            guard i + 2 < bytes.count,
                  let hi = Character(UnicodeScalar(bytes[i + 1])).hexDigitValue,
                  let lo = Character(UnicodeScalar(bytes[i + 2])).hexDigitValue
            else { throw DeepLinkError.badEscape }
            out.append(UInt8(hi << 4 | lo))
            i += 3
        }
        guard let text = String(bytes: out, encoding: .utf8) else { throw DeepLinkError.badEscape }
        if text.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) {
            throw DeepLinkError.controlChar
        }
        return text
    }

    /// Percent-encode for emission: unreserved characters plus `:` (legal in a query value and
    /// left alone by `URLComponents`, so the three emitters agree on `steam:570`).
    static func encode(_ s: String) -> String {
        var out = ""
        for b in s.utf8 {
            let c = Character(UnicodeScalar(b))
            if c.isASCII, c.isLetter || c.isNumber || "-._~:".contains(c) {
                out.append(c)
            } else {
                out += String(format: "%%%02X", b)
            }
        }
        return out
    }

    static func hexLower(_ data: Data) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }
}
