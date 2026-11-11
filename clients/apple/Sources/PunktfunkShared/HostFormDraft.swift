// What a user typed into an add-or-edit-host form, and the one set of rules for turning it into a
// stored record.
//
// Three forms take this input — the pointer sheet, the gamepad screen, and the tvOS commit path —
// and each validated it differently: the same typed port became 65535 in one, the default in
// another, and a refusal in the third, so the same typo saved three different records. The rules
// live here so every form answers the same way, and so they can be tested without a view.

import Foundation

public struct HostFormDraft: Equatable, Sendable {
    /// The punktfunk/1 data plane's default. A blank port field means this, not zero.
    public static let defaultPort: UInt16 = 9777

    public var name: String
    public var address: String
    /// The port field's raw text. Blank is legal and means the default.
    public var port: String

    public init(name: String = "", address: String = "", port: String = "") {
        self.name = name
        self.address = address
        self.port = port
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
    /// with a generic "could not connect". An IPv6 literal is left alone unless it is bracketed
    /// with a port after it (`[fd7a::1]:9777`), since a bare one is all colons.
    public var trimmedAddress: String { Self.split(address).host }

    /// The resolved port: the address's suffix if it carried one, else the typed field, else the
    /// default.
    public var resolvedPort: UInt16? {
        let split = Self.split(address)
        if let fromAddress = split.port { return fromAddress }
        let typed = port.trimmingCharacters(in: .whitespacesAndNewlines)
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

    /// The Wake-on-LAN field's MACs: split on commas, semicolons and whitespace, keeping the
    /// well-formed `aa:bb:cc:dd:ee:ff` ones in lower case. nil when none is valid, so clearing
    /// the field clears the stored MACs.
    public static func parseMacs(_ s: String) -> [String]? {
        let macs = s
            .split(whereSeparator: { ",; \n\t".contains($0) })
            .map { $0.trimmingCharacters(in: .whitespaces).lowercased() }
            .filter { m in
                let parts = m.split(separator: ":")
                return parts.count == 6 && parts.allSatisfy { $0.count == 2 && UInt8($0, radix: 16) != nil }
            }
        return macs.isEmpty ? nil : macs
    }

    /// Split a trailing `:port` off an address. Bare IPv6 (more than one colon, unbracketed) is
    /// returned whole.
    static func split(_ raw: String) -> (host: String, port: UInt16?) {
        let text = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let colon = text.lastIndex(of: ":") else { return (text, nil) }
        let head = String(text[text.startIndex..<colon])
        let tail = String(text[text.index(after: colon)...])
        // `[fd7a::1]:9777` splits; `fd7a::1` does not.
        let bracketed = head.hasPrefix("[") && head.hasSuffix("]")
        let looksIPv6 = head.contains(":") && !bracketed
        guard !looksIPv6, !head.isEmpty, let value = UInt16(tail), value > 0 else {
            return (text, nil)
        }
        return (head, value)
    }
}
