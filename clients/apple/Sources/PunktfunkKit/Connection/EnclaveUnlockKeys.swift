// Unlock keys in the Secure Enclave (CONTRACT §6.6): P-256, usable only after a match of the
// biometrics enrolled when the key was made, never exported, never synced, and with no fallback
// to the device passcode. tvOS has no biometry and no LocalAuthentication, so it gets none.

#if !os(tvOS)
import CryptoKit
import Foundation
import LocalAuthentication
import Security

public struct EnclaveUnlockKeys: UnlockKeys {
    public init() {}

    public var biometry: Biometry? {
        guard SecureEnclave.isAvailable else { return nil }
        let context = LAContext()
        var error: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthenticationWithBiometrics, error: &error)
        else { return nil }
        switch context.biometryType {
        case .faceID: return .faceID
        case .touchID: return .touchID
        case .opticID: return .opticID
        default: return nil
        }
    }

    static func tag(host: Data, profile: String) -> Data {
        let hex = host.map { String(format: "%02x", $0) }.joined()
        return Data("io.unom.punktfunk.unlock-key.\(hex).\(profile)".utf8)
    }

    private static func query(_ host: Data, _ profile: String) -> [CFString: Any] {
        [
            kSecClass: kSecClassKey,
            kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeyClass: kSecAttrKeyClassPrivate,
            kSecAttrApplicationTag: tag(host: host, profile: profile),
            kSecUseDataProtectionKeychain: true,
        ]
    }

    public func has(host: Data, profile: String) -> Bool {
        var query = Self.query(host, profile)
        query[kSecMatchLimit] = kSecMatchLimitOne
        return SecItemCopyMatching(query as CFDictionary, nil) == errSecSuccess
    }

    public func create(host: Data, profile: String) throws -> Data {
        delete(host: host, profile: profile)
        var error: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(
            kCFAllocatorDefault, kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly,
            [.privateKeyUsage, .biometryCurrentSet], &error)
        else { throw Self.failure(error) }
        let attributes: [CFString: Any] = [
            kSecAttrKeyType: kSecAttrKeyTypeECSECPrimeRandom,
            kSecAttrKeySizeInBits: 256,
            kSecAttrTokenID: kSecAttrTokenIDSecureEnclave,
            kSecUseDataProtectionKeychain: true,
            kSecPrivateKeyAttrs: [
                kSecAttrIsPermanent: true,
                kSecAttrApplicationTag: Self.tag(host: host, profile: profile),
                kSecAttrAccessControl: access,
            ] as [CFString: Any],
        ]
        guard let key = SecKeyCreateRandomKey(attributes as CFDictionary, &error) else {
            throw Self.failure(error)
        }
        guard let pub = SecKeyCopyPublicKey(key),
              let point = SecKeyCopyExternalRepresentation(pub, &error) as Data?,
              point.count == 65, point.first == 0x04
        else {
            delete(host: host, profile: profile)
            throw Self.failure(error)
        }
        return point
    }

    public func sign(_ message: Data, host: Data, profile: String, reason: String) throws -> Data {
        let context = LAContext()
        context.localizedReason = reason
        // Empty hides "Use Password…": the account password is the fallback, in our prompt.
        context.localizedFallbackTitle = ""
        var query = Self.query(host, profile)
        query[kSecReturnRef] = true
        query[kSecUseAuthenticationContext] = context
        var found: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &found)
        guard status == errSecSuccess, let found, CFGetTypeID(found) == SecKeyGetTypeID() else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status))
        }
        let key = found as! SecKey // swiftlint:disable:this force_cast
        var error: Unmanaged<CFError>?
        guard let signature = SecKeyCreateSignature(
            key, .ecdsaSignatureMessageX962SHA256, message as CFData, &error) as Data?
        else { throw Self.failure(error) }
        return signature
    }

    public func delete(host: Data, profile: String) {
        SecItemDelete(Self.query(host, profile) as CFDictionary)
    }

    private static func failure(_ error: Unmanaged<CFError>?) -> Error {
        error?.takeRetainedValue() ?? NSError(domain: NSOSStatusErrorDomain, code: Int(errSecParam))
    }
}
#endif
