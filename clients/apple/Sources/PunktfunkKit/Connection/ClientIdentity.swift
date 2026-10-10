// The client's persistent identity + the SPAKE2 PIN pairing ceremony — the trust
// bootstrap that precedes any pinned PunktfunkConnection.

import Foundation
import PunktfunkCore

/// This client's persistent self-signed identity. Generate ONCE with `generateIdentity()`,
/// store both PEMs (Keychain), present on every connect — the certificate's fingerprint is
/// how hosts recognize this client after pairing.
public struct ClientIdentity: Sendable {
    public let certPEM: String
    public let keyPEM: String
    public init(certPEM: String, keyPEM: String) {
        self.certPEM = certPEM
        self.keyPEM = keyPEM
    }
}

/// Generate a fresh client identity (self-signed cert + key, PEM).
public func generateIdentity() throws -> ClientIdentity {
    var cert = [CChar](repeating: 0, count: 4096)
    var key = [CChar](repeating: 0, count: 4096)
    let rc = punktfunk_generate_identity(&cert, UInt(cert.count), &key, UInt(key.count))
    guard rc == PUNKTFUNK_STATUS_OK.rawValue else {
        throw PunktfunkClientError.status(rc)
    }
    return ClientIdentity(certPEM: String(cString: cert), keyPEM: String(cString: key))
}

/// Run the PIN pairing ceremony: the host displays a 4-digit PIN (its log/UI), the user
/// types it here. On success the host stores this client's identity and the returned
/// fingerprint is the host's now-VERIFIED identity — persist it and pass it as `pinSHA256`
/// to every later connect. Throws `.wrongPIN` when the proof is rejected.
public func pair(
    host: String, port: UInt16 = 9777,
    identity: ClientIdentity, pin: String, name: String,
    timeoutMs: UInt32 = 90_000
) throws -> Data {
    var observed = [UInt8](repeating: 0, count: 32)
    // The C header types PunktfunkStatus as a bare int32 (C17, no enum import), so the ABI
    // functions return Int32 directly — compare against the enum constants' rawValue, the
    // same bridging the connection methods use (statusOK etc.).
    let rc = host.withCString { cs in
        identity.certPEM.withCString { cert in
            identity.keyPEM.withCString { key in
                pin.withCString { p in
                    name.withCString { n in
                        punktfunk_pair(cs, port, cert, key, p, n, &observed, timeoutMs)
                    }
                }
            }
        }
    }
    switch rc {
    case PUNKTFUNK_STATUS_OK.rawValue: return Data(observed)
    // Both mean the SPAKE2 proof did not verify. The host answers a bad PIN with the typed
    // REJECTED_SETUP_FAILED; CRYPTO is what an older one sent. Either way the user typed the
    // wrong number, and saying "status -29" instead of that is the whole difference between an
    // actionable message and a dead end.
    case PUNKTFUNK_STATUS_CRYPTO.rawValue,
         PUNKTFUNK_STATUS_REJECTED_SETUP_FAILED.rawValue:
        throw PunktfunkClientError.wrongPIN
    default:
        // A typed host rejection (pairing not armed / rate-limited / armed for another
        // device) carries its own reason — never report it as a bad PIN or dead network.
        if let rejection = HostRejection(status: rc) {
            throw PunktfunkClientError.rejected(rejection)
        }
        throw PunktfunkClientError.status(rc)
    }
}

/// Request access without a PIN: blocks until the host's operator approves this device (or
/// refuses it), then returns the host's fingerprint to persist as paired. Never streams.
/// `pinSHA256` is the advertised fingerprint; nil trusts on first use. A refusal throws
/// `.rejected`; an unreachable host or a mismatched fingerprint throws `.connectFailed`.
public func requestAccess(
    host: String, port: UInt16 = 9777,
    identity: ClientIdentity, pinSHA256: Data?, name: String,
    timeoutMs: UInt32 = 185_000
) throws -> Data {
    if let pin = pinSHA256, pin.count != 32 { throw PunktfunkClientError.invalidPin }
    var observed = [UInt8](repeating: 0, count: 32)
    let pin = pinSHA256.map { [UInt8]($0) }
    let rc = host.withCString { cs in
        identity.certPEM.withCString { cert in
            identity.keyPEM.withCString { key in
                name.withCString { n in
                    if let pin {
                        return punktfunk_request_access(
                            cs, port, cert, key, pin, n, &observed, timeoutMs)
                    }
                    return punktfunk_request_access(
                        cs, port, cert, key, nil, n, &observed, timeoutMs)
                }
            }
        }
    }
    if rc == PUNKTFUNK_STATUS_OK.rawValue { return Data(observed) }
    if let rejection = HostRejection(status: rc) {
        throw PunktfunkClientError.rejected(rejection)
    }
    throw PunktfunkClientError.connectFailed
}
