// What a user typed into an add-or-edit-host form, and the one set of rules for turning it into a
// stored record.
//
// The pointer sheet, the gamepad screen and the tvOS commit path all take their answers from here,
// so the same typed text saves the same record on each, and the rules test without a view.

import Foundation

public struct HostFormDraft: Equatable, Sendable {
    /// The punktfunk/1 data plane's default. A blank port field means this, not zero.
    public static let defaultPort: UInt16 = 9777

    public var name: String
    public var address: String
    /// The port field's raw text. Blank is legal and means the default.
    public var port: String
    /// The Wake-on-LAN field's raw text (`parseMacs`).
    public var macs: String

    public init(name: String = "", address: String = "", port: String = "", macs: String = "") {
        self.name = name
        self.address = address
        self.port = port
        self.macs = macs
    }

    /// The fields as a form shows `record`.
    public init(record: StoredHost) {
        self.init(
            name: record.name, address: record.address, port: String(record.port),
            macs: record.wakeMacs.joined(separator: ", "))
    }

    /// What is wrong with the draft, in the order a form should send the user to fix it.
    public enum Problem: Equatable, Sendable {
        case emptyAddress
        /// Present but not a port: non-numeric, zero, or past 65535.
        case badPort
    }

    public var trimmedName: String { name.trimmingCharacters(in: .whitespacesAndNewlines) }

    /// The address with surrounding whitespace gone and any `:port` suffix split off — a user who
    /// pastes what the cards RENDER (`192.168.1.5:9777`) typed a valid thing, and storing it whole
    /// makes the core build `[192.168.1.5:9777]:9777`, which never resolves and fails every dial
    /// with a generic "could not connect". An IPv6 literal keeps its colons and loses its
    /// brackets (`[fd7a::1]:9777` stores `fd7a::1`): a stored address is bare, and a deep link
    /// puts the brackets back itself.
    public var trimmedAddress: String { Self.split(address).host }

    /// The resolved port: the address's suffix if it carried one, else the typed field, else the
    /// default.
    public var resolvedPort: UInt16? {
        let split = Self.split(address)
        if let fromAddress = split.port { return fromAddress }
        let typed = port.trimmingCharacters(in: .whitespacesAndNewlines).withASCIIDigits
        if typed.isEmpty { return Self.defaultPort }
        guard let value = UInt16(typed), value > 0 else { return nil }
        return value
    }

    /// nil when the draft is savable; otherwise what to fix first.
    public var problem: Problem? {
        if trimmedAddress.isEmpty { return .emptyAddress }
        if resolvedPort == nil { return .badPort }
        return nil
    }

    public var canSave: Bool { problem == nil }

    /// Apply the draft to a record, leaving every field the form does not show — the pinned
    /// fingerprint, the wake MACs, preset bindings, `addedAt` — exactly as it was.
    public func apply(to host: inout StoredHost) {
        host.name = trimmedName
        host.address = trimmedAddress
        host.port = resolvedPort ?? Self.defaultPort
    }

    /// A host page's save over `host`, which can move while the form is open (a sweep
    /// re-addresses it, an advert teaches it a MAC). Only a field that differs from `loaded`, the
    /// form as it last loaded the record, is written. Name, address and port go in together when
    /// the result can save, the MACs when they parse or the field is blank. What the rules
    /// refuse keeps the record's value.
    public func commit(to host: inout StoredHost, changedFrom loaded: HostFormDraft) {
        var merged = HostFormDraft(record: host)
        if name != loaded.name { merged.name = name }
        if address != loaded.address { merged.address = address }
        if port != loaded.port { merged.port = port }
        if merged != HostFormDraft(record: host), merged.canSave { merged.apply(to: &host) }
        guard macs != loaded.macs else { return }
        let parsed = Self.parseMacs(macs)
        if parsed != nil || macs.allSatisfy(\.isWhitespace) { host.macAddresses = parsed }
    }

    /// The Wake-on-LAN field's MACs: split on commas, semicolons and whitespace, keeping the
    /// well-formed ones as lower-case `aa:bb:cc:dd:ee:ff`. A hyphen separates octets as a colon
    /// does. nil when none is valid, so clearing the field clears the stored MACs.
    public static func parseMacs(_ s: String) -> [String]? {
        let macs = s
            .split(whereSeparator: { ",; \n\t".contains($0) })
            .map { $0.lowercased().replacingOccurrences(of: "-", with: ":") }
            .filter { m in
                let parts = m.split(separator: ":", omittingEmptySubsequences: false)
                return parts.count == 6
                    && parts.allSatisfy { $0.count == 2 && $0.allSatisfy(\.isHexDigit) }
            }
        return macs.isEmpty ? nil : macs
    }

    /// Split a trailing `:port` off an address, after mapping what a CJK keyboard types (full-width
    /// forms, `。`) to ASCII. Bare IPv6 (more than one colon, unbracketed) is returned whole;
    /// brackets around the host are dropped.
    static func split(_ raw: String) -> (host: String, port: UInt16?) {
        let text = raw.withASCIIAddress.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let colon = text.lastIndex(of: ":") else { return (text, nil) }
        let head = String(text[text.startIndex..<colon])
        let tail = String(text[text.index(after: colon)...])
        // `[fd7a::1]:9777` splits; `fd7a::1` does not.
        let bracketed = head.hasPrefix("[") && head.hasSuffix("]")
        let looksIPv6 = head.contains(":") && !bracketed
        guard !looksIPv6, !head.isEmpty, let value = UInt16(tail.withASCIIDigits), value > 0 else {
            return (unbracketed(text), nil)
        }
        return (unbracketed(head), value)
    }

    private static func unbracketed(_ host: String) -> String {
        guard host.hasPrefix("["), host.hasSuffix("]"), host.count > 2 else { return host }
        return String(host.dropFirst().dropLast())
    }
}
