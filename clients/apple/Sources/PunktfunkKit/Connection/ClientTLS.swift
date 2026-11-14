// mTLS for the management REST API. The host serves the API over HTTPS and authorizes a request
// whose client certificate is in its paired store — the SAME identity + trust the QUIC data
// plane uses — so a paired client needs no bearer token.
//
// To present that identity, `MgmtConnection` (Network.framework) needs a SecIdentity (cert +
// private key pair). The client stores its identity as PEM (rcgen ECDSA P-256, PKCS#8 key). We
// rebuild a SecIdentity natively: CryptoKit parses the key → its X9.63 form → a SecKey, the cert
// PEM → a SecCertificate. From there the two platform families diverge because
// `SecIdentityCreateWithCertificate` — the straight-line "pair these two" API — is macOS-only:
//   - macOS: SecIdentityCreateWithCertificate does the pairing directly once the key is in the
//     Keychain (a plain `SecItemAdd`).
//   - iOS/tvOS: that API is unavailable. Instead, add BOTH the key and the certificate to the
//     Keychain (under the same application tag) and query `kSecClassIdentity` — the system
//     correlates a stored cert against a stored key with a matching public key and vends the pair
//     as one `SecIdentity`, no PKCS#12 needed. This is the standard non-macOS technique for
//     "I already have a raw cert + key, not a .p12".

import CryptoKit
import Foundation
import Security

enum ClientTLS {
    enum TLSError: LocalizedError {
        case badKey(String)
        case badCert
        case identity(String)

        var errorDescription: String? {
            switch self {
            case .badKey(let why): return "Couldn't load the client key: \(why)"
            case .badCert: return "Couldn't load the client certificate"
            case .identity(let why): return "Couldn't build the client identity: \(why)"
            }
        }
    }

    /// The first CERTIFICATE block of `pem` → its DER bytes. CryptoKit parses the key's PEM itself.
    static func certificateDER(fromPEM pem: String) -> Data? {
        guard let start = pem.range(of: "-----BEGIN CERTIFICATE-----"),
              let end = pem.range(of: "-----END CERTIFICATE-----", range: start.upperBound..<pem.endIndex)
        else { return nil }
        let b64 = pem[start.upperBound..<end.lowerBound]
            .components(separatedBy: .whitespacesAndNewlines).joined()
        return Data(base64Encoded: b64)
    }

    /// Built identities by PEM pair — a vended SecIdentity is immutable, so repeat calls reuse
    /// the built pair instead of round-tripping the Keychain on every send. `lock` also
    /// serialises the Keychain work in `buildIdentity`, so two builds can't interleave delete/add.
    private static var built: [String: SecIdentity] = [:]
    private static let lock = NSLock()

    /// Build a `SecIdentity` from the client's PEM cert + PKCS#8 P-256 key, cached per PEM pair.
    static func makeIdentity(certPEM: String, keyPEM: String) throws -> SecIdentity {
        let cacheKey = certPEM + "\u{0}" + keyPEM
        return try lock.withLock {
            if let hit = built[cacheKey] { return hit }
            let identity = try buildIdentity(certPEM: certPEM, keyPEM: keyPEM)
            built[cacheKey] = identity
            return identity
        }
    }

    /// Pair the PEM cert and key via the Keychain. Stored once under a stable tag/label — but
    /// dropped and rewritten every call, since a regenerated identity must not keep presenting
    /// a previous pair (`SecItemAdd` refuses to overwrite).
    private static func buildIdentity(certPEM: String, keyPEM: String) throws -> SecIdentity {
        // Key: CryptoKit accepts the SEC1 or PKCS#8 PEM; its x963 form is what SecKey wants.
        let priv: P256.Signing.PrivateKey
        do {
            priv = try P256.Signing.PrivateKey(pemRepresentation: keyPEM)
        } catch {
            throw TLSError.badKey(error.localizedDescription)
        }
        var keyError: Unmanaged<CFError>?
        let attrs: [CFString: Any] = [
            kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeyClass: kSecAttrKeyClassPrivate,
            kSecAttrKeySizeInBits: 256,
        ]
        guard let secKey = SecKeyCreateWithData(
            priv.x963Representation as CFData, attrs as CFDictionary, &keyError)
        else {
            throw TLSError.badKey((keyError?.takeRetainedValue()).map { "\($0)" } ?? "SecKeyCreateWithData")
        }

        guard let certDER = certificateDER(fromPEM: certPEM),
              let cert = SecCertificateCreateWithData(nil, certDER as CFData)
        else { throw TLSError.badCert }

        let tag = Data("io.unom.punktfunk.library-client-key".utf8)
        let certLabel = "io.unom.punktfunk.library-client-cert"
        // The stored key and cert are derived artifacts — the PEM store is the durable state.
        // Drop whatever a previous identity left, or a regenerated identity would keep vending
        // pairs built from the OLD key while QUIC presents the new PEM: one device, two certs.
        SecItemDelete([kSecClass: kSecClassKey, kSecAttrApplicationTag: tag] as CFDictionary)
        SecItemDelete(
            [kSecClass: kSecClassCertificate, kSecAttrLabel: certLabel] as CFDictionary)

        #if os(macOS)
        // The key must live in a Keychain for SecIdentityCreateWithCertificate to pair it with the
        // cert. Add it under a stable tag; the delete above makes a duplicate impossible.
        let add: [CFString: Any] = [
            kSecClass: kSecClassKey,
            kSecAttrApplicationTag: tag,
            kSecValueRef: secKey,
        ]
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess || status == errSecDuplicateItem else {
            throw TLSError.identity("keychain add key failed (OSStatus \(status))")
        }

        var identity: SecIdentity?
        let idStatus = SecIdentityCreateWithCertificate(nil, cert, &identity)
        guard idStatus == errSecSuccess, let identity else {
            throw TLSError.identity("SecIdentityCreateWithCertificate (OSStatus \(idStatus))")
        }
        return identity
        #else
        // Add the key (tagged) and the certificate (labelled, matched to the key by public key)
        // separately; the deletes above make a duplicate impossible. Both carry the class of
        // `ClientIdentityStore`: readable after first unlock, never in a backup or a migration.
        let addKey: [CFString: Any] = [
            kSecClass: kSecClassKey,
            kSecAttrApplicationTag: tag,
            kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueRef: secKey,
        ]
        let keyStatus = SecItemAdd(addKey as CFDictionary, nil)
        guard keyStatus == errSecSuccess || keyStatus == errSecDuplicateItem else {
            throw TLSError.identity("keychain add key failed (OSStatus \(keyStatus))")
        }

        let addCert: [CFString: Any] = [
            kSecClass: kSecClassCertificate,
            kSecAttrLabel: certLabel,
            kSecAttrAccessible: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
            kSecValueRef: cert,
        ]
        let certStatus = SecItemAdd(addCert as CFDictionary, nil)
        guard certStatus == errSecSuccess || certStatus == errSecDuplicateItem else {
            throw TLSError.identity("keychain add certificate failed (OSStatus \(certStatus))")
        }

        // The system correlates the just-added cert against the tagged key (matching public key)
        // and vends the pair as a kSecClassIdentity — the tag filter here matches the KEY half.
        var identityRef: CFTypeRef?
        let query: [CFString: Any] = [
            kSecClass: kSecClassIdentity,
            kSecAttrApplicationTag: tag,
            kSecReturnRef: true,
        ]
        let idStatus = SecItemCopyMatching(query as CFDictionary, &identityRef)
        guard idStatus == errSecSuccess, let identityRef else {
            throw TLSError.identity("SecItemCopyMatching(kSecClassIdentity) (OSStatus \(idStatus))")
        }
        // Safe: a kSecClassIdentity query with kSecReturnRef always vends a SecIdentity.
        return (identityRef as! SecIdentity) // swiftlint:disable:this force_cast
        #endif
    }
}
