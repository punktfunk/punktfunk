// Where a management API call goes and as whom. Every call needs the same two things — this
// device's mTLS identity and the host's pinned fingerprint — so `MgmtTarget.make` checks them once
// and names what is missing in one sentence the caller can show.

import Foundation
import Security

private let log = ClientLog(category: "mgmt")

/// A paired host's management API, reached as this device.
///
/// `@unchecked Sendable` for `identity` alone: a vended `SecIdentity` is immutable.
public struct MgmtTarget: @unchecked Sendable {
    public let address: String
    public let port: UInt16
    let identity: SecIdentity
    /// SHA-256 of the host's certificate; the transport refuses any other.
    public let pin: Data

    /// The target for `host` as `identity`. `port` overrides the saved management port; nil or 0
    /// keeps it. An unpaired host fails here rather than at the transport, so every caller gets
    /// the same remedy. Builds the TLS identity: blocking Keychain work on its first call.
    public static func make(
        host: StoredHost, port: UInt16? = nil, identity: ClientIdentity?
    ) -> Result<MgmtTarget, MgmtTargetMissing> {
        guard let identity else { return .failure(.identity) }
        guard let pin = host.pinnedSHA256 else { return .failure(.pairing(host.displayName)) }
        let secIdentity: SecIdentity
        do {
            secIdentity = try ClientTLS.makeIdentity(
                certPEM: identity.certPEM, keyPEM: identity.keyPEM)
        } catch {
            log.warning("client identity unreadable: \(error.localizedDescription)")
            return .failure(.keychain)
        }
        let mgmt = port.flatMap { $0 > 0 ? $0 : nil } ?? host.effectiveMgmtPort
        return .success(MgmtTarget(address: host.address, port: mgmt, identity: secIdentity, pin: pin))
    }
}

public extension Result where Success == MgmtTarget, Failure == MgmtTargetMissing {
    /// The target, or nil once `say` has the sentence naming what is missing.
    func target(orSay say: (String) -> Void) -> MgmtTarget? {
        switch self {
        case .success(let target): return target
        case .failure(let missing):
            say(missing.sentence)
            return nil
        }
    }
}

/// Why there is no `MgmtTarget`, as the one sentence to show for it.
public enum MgmtTargetMissing: Error, Equatable, Sendable {
    /// No client identity yet; the first connect mints it.
    case identity
    /// The host (named) has no pinned fingerprint, so nothing it presents can be verified.
    case pairing(String)
    /// The Keychain wouldn't build the identity from its stored halves.
    case keychain

    public var sentence: String {
        switch self {
        case .identity:
            return "This device has no identity yet. Connect to a host once to create it."
        case .pairing(let name):
            return "This device isn't paired with \(name). Pair with it first."
        case .keychain:
            return "Couldn't read this device's identity from the Keychain. Try again."
        }
    }
}
