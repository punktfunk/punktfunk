// Opening a profile on a person's own account (CONTRACT §6) or a locked one: the secret a connect
// carries, the message an unlock key signs, the host's challenge, and the flow that chooses
// between the key and the password or PIN prompt. `clients/shared/profile-unlock-vectors.json`
// pins the rules. The Secure Enclave keys live in EnclaveUnlockKeys.swift.

import Darwin
import Foundation
import PunktfunkCore
import PunktfunkShared

/// The bytes of a password or a key proof. Wiped on release and by `wipe()`; prints its length
/// only. One owner at a time: a connect reads it on one thread, then wipes it.
public final class UnlockSecret: @unchecked Sendable, CustomStringConvertible,
    CustomDebugStringConvertible
{
    private let storage: UnsafeMutableBufferPointer<UInt8>
    private var live: Int

    /// The UTF-8 of `text` as typed, never trimmed.
    public convenience init(_ text: String) {
        var text = text
        let count = text.utf8.count
        self.init(capacity: count) { out in
            text.withUTF8 { _ = out.initialize(from: $0) }
            return count
        }
    }

    public convenience init(_ bytes: Data) {
        self.init(capacity: bytes.count) { out in
            _ = out.initialize(from: bytes)
            return bytes.count
        }
    }

    /// `capacity` zeroed bytes that `fill` writes into, answering how many it wrote. An answer
    /// outside `0...capacity` wipes them, and the secret reads as empty.
    init(capacity: Int, fill: (UnsafeMutableBufferPointer<UInt8>) -> Int) {
        storage = .allocate(capacity: max(capacity, 1))
        storage.initialize(repeating: 0)
        live = 0
        let wrote = fill(UnsafeMutableBufferPointer(rebasing: storage[0..<capacity]))
        if (0...capacity).contains(wrote) { live = wrote } else { wipe() }
    }

    deinit {
        wipe()
        storage.deallocate()
    }

    /// Zero the bytes. A wiped secret reads as empty, so no connect can send it again.
    public func wipe() {
        _ = memset_s(storage.baseAddress, storage.count, 0, storage.count)
        live = 0
    }

    public var count: Int { live }

    func withBytes<R>(_ body: (UnsafeBufferPointer<UInt8>) throws -> R) rethrows -> R {
        try body(UnsafeBufferPointer(rebasing: storage[0..<live]))
    }

    public var description: String { "UnlockSecret(\(live) bytes)" }
    public var debugDescription: String { description }
}

/// What one connect sends to open a locked profile. The connect wipes it, so it dials once.
public struct ProfileUnlock: Sendable, CustomStringConvertible {
    public enum Kind: Sendable, Equatable { case password, keyProof }

    public let kind: Kind
    let secret: UnlockSecret
    public let staySignedIn: Bool
    /// The P-256 key to enrol: its 65-byte X9.63 point `04‖X‖Y`.
    public let enrollKey: Data?

    /// Whether `secret` can be a password on the wire: 1 to 512 bytes, no NUL.
    public static func fits(_ secret: UnlockSecret) -> Bool {
        (1...512).contains(secret.count) && !secret.withBytes { $0.contains(0) }
    }

    public static func password(
        _ secret: UnlockSecret, staySignedIn: Bool = false, enroll: Data? = nil
    ) -> ProfileUnlock? {
        guard fits(secret) else { return nil }
        if let enroll, enroll.count != 65 || enroll.first != 0x04 { return nil }
        return ProfileUnlock(
            kind: .password, secret: secret, staySignedIn: staySignedIn, enrollKey: enroll)
    }

    public static func keyProof(_ signature: Data) -> ProfileUnlock? {
        guard (1...1024).contains(signature.count) else { return nil }
        return ProfileUnlock(
            kind: .keyProof, secret: UnlockSecret(signature), staySignedIn: false, enrollKey: nil)
    }

    public var description: String {
        "ProfileUnlock(\(kind), \(secret), stay=\(staySignedIn), enroll=\(enrollKey != nil))"
    }

    /// The C view of this unlock, valid inside `body` only.
    func withC<R>(_ body: (UnsafePointer<PunktfunkProfileUnlock>) -> R) -> R {
        secret.withBytes { bytes in
            withOptionalBytes(enrollKey) { key in
                var c = PunktfunkProfileUnlock()
                c.kind = UInt8(
                    kind == .password ? PUNKTFUNK_UNLOCK_PASSWORD : PUNKTFUNK_UNLOCK_KEY_PROOF)
                c.stay_signed_in = staySignedIn ? 1 : 0
                c.enroll_alg = UInt8(key == nil ? PUNKTFUNK_ENROLL_NONE : PUNKTFUNK_ENROLL_P256)
                c.secret = bytes.baseAddress
                c.secret_len = UInt(bytes.count)
                c.enroll_key = key?.baseAddress
                c.enroll_key_len = UInt(key?.count ?? 0)
                return body(&c)
            }
        }
    }

    /// The 152 bytes a key proof signs (CONTRACT §3), built by the core:
    /// `"punktfunk-seat-unlock-v1" ‖ challenge ‖ seat ‖ device ‖ box`. nil for a wrong shape.
    public static func message(challenge: Data, seat: String, device: Data, box: Data) -> Data? {
        guard challenge.count == 32, device.count == 32, box.count == 32 else { return nil }
        var out = [UInt8](repeating: 0, count: Int(PUNKTFUNK_UNLOCK_MESSAGE_LEN))
        let rc = challenge.withUnsafeBytes { c in
            device.withUnsafeBytes { d in
                box.withUnsafeBytes { b in
                    seat.withCString { s in
                        punktfunk_profile_unlock_message(
                            c.bindMemory(to: UInt8.self).baseAddress, s,
                            d.bindMemory(to: UInt8.self).baseAddress,
                            b.bindMemory(to: UInt8.self).baseAddress, &out)
                    }
                }
            }
        }
        return rc == PUNKTFUNK_STATUS_OK.rawValue ? Data(out) : nil
    }

    /// The 187 bytes a locked profile's key proof signs, built by the core:
    /// `"punktfunk-profile-unlock-v1" ‖ challenge ‖ profile id, zero-padded to 64 ‖ device ‖ box`.
    /// nil for a wrong shape.
    public static func message(challenge: Data, profile: String, device: Data, box: Data) -> Data? {
        // Checked here too: a C string ends at a NUL the id carries.
        guard isProfileID(profile), challenge.count == 32, device.count == 32, box.count == 32
        else { return nil }
        var out = [UInt8](repeating: 0, count: Int(PUNKTFUNK_LOCKED_PROFILE_UNLOCK_MESSAGE_LEN))
        let rc = challenge.withUnsafeBytes { c in
            device.withUnsafeBytes { d in
                box.withUnsafeBytes { b in
                    profile.withCString { p in
                        punktfunk_locked_profile_unlock_message(
                            c.bindMemory(to: UInt8.self).baseAddress, p,
                            d.bindMemory(to: UInt8.self).baseAddress,
                            b.bindMemory(to: UInt8.self).baseAddress, &out)
                    }
                }
            }
        }
        return rc == PUNKTFUNK_STATUS_OK.rawValue ? Data(out) : nil
    }

    /// An id a host mints: 1 to 64 of `[A-Za-z0-9_-]`.
    public static func isProfileID(_ id: String) -> Bool {
        (1...64).contains(id.utf8.count) && id.utf8.allSatisfy {
            switch $0 {
            case UInt8(ascii: "0")...UInt8(ascii: "9"), UInt8(ascii: "a")...UInt8(ascii: "z"),
                 UInt8(ascii: "A")...UInt8(ascii: "Z"), UInt8(ascii: "_"), UInt8(ascii: "-"):
                return true
            default: return false
            }
        }
    }

    /// Why the PIN field's text is no PIN.
    public enum PinProblem: Error, Equatable {
        case empty, shape

        /// The prompt's error line.
        public var line: String {
            String(localized: self == .empty ? L10n.unlockPinErrorEmpty : L10n.unlockPinErrorShape)
        }
    }

    /// `typed` as the PIN the wire carries: 4 to 8 ASCII digits, a full-width digit read as its
    /// ASCII one. Read in place, so no copy of the digits outlives the secret.
    public static func pin(_ typed: UnlockSecret) -> Result<UnlockSecret, PinProblem> {
        guard typed.count > 0 else { return .failure(.empty) }
        let pin = UnlockSecret(capacity: 8) { out in
            typed.withBytes { b in
                var (i, n) = (0, 0)
                while i < b.count {
                    guard n < out.count else { return -1 }
                    if (0x30...0x39).contains(b[i]) {
                        out[n] = b[i]
                        i += 1
                    } else if i + 2 < b.count, b[i] == 0xEF, b[i + 1] == 0xBC,
                              (0x90...0x99).contains(b[i + 2]) {
                        // U+FF10..FF19 in UTF-8.
                        out[n] = b[i + 2] - 0x90 + 0x30
                        i += 3
                    } else {
                        return -1
                    }
                    n += 1
                }
                return n >= 4 ? n : -1
            }
        }
        return pin.count > 0 ? .success(pin) : .failure(.shape)
    }
}

private func withOptionalBytes<R>(_ data: Data?, _ body: (UnsafeBufferPointer<UInt8>?) -> R) -> R {
    guard let data else { return body(nil) }
    return data.withUnsafeBytes { body($0.bindMemory(to: UInt8.self)) }
}

/// `POST /api/v1/profiles/{id}/unlock-challenge`: 32 fresh bytes from the host, and what the
/// signed message names.
public struct UnlockChallenge: Sendable, Equatable {
    public enum Names: Sendable, Equatable {
        /// A person's own account: the seat id, 32 lowercase hex characters.
        case seat(String)
        /// A locked profile: its id.
        case profile(String)
    }

    public let challenge: Data
    public let names: Names

    /// The host's answer, `{challenge, seat}` or `{challenge, profile}`, each field exactly its
    /// shape. Anything else is refused: both, neither.
    public init?(json: Data) {
        guard json.count <= 4096,
              let object = try? JSONSerialization.jsonObject(with: json) as? [String: Any],
              let hex = object["challenge"] as? String,
              let challenge = Self.lowerHex(hex, bytes: 32)
        else { return nil }
        let field: (String) -> Any? = { key in object[key].flatMap { $0 is NSNull ? nil : $0 } }
        switch (field("seat"), field("profile")) {
        case (.some(let seat as String), .none) where Self.lowerHex(seat, bytes: 16) != nil:
            names = .seat(seat)
        case (.none, .some(let id as String)) where ProfileUnlock.isProfileID(id):
            names = .profile(id)
        default:
            return nil
        }
        self.challenge = challenge
    }

    public init(challenge: Data, names: Names) {
        self.challenge = challenge
        self.names = names
    }

    /// What the key signs to open profile `id`: the message this challenge names. nil for one
    /// named for another profile.
    public func message(for id: String, device: Data, box: Data) -> Data? {
        switch names {
        case .seat(let seat):
            return ProfileUnlock.message(challenge: challenge, seat: seat, device: device, box: box)
        case .profile(let named) where named == id:
            return ProfileUnlock.message(
                challenge: challenge, profile: named, device: device, box: box)
        case .profile:
            return nil
        }
    }

    static func lowerHex(_ text: String, bytes: Int) -> Data? {
        let digits = Array(text.utf8)
        guard digits.count == bytes * 2 else { return nil }
        func nibble(_ c: UInt8) -> UInt8? {
            switch c {
            case UInt8(ascii: "0")...UInt8(ascii: "9"): return c - UInt8(ascii: "0")
            case UInt8(ascii: "a")...UInt8(ascii: "f"): return c - UInt8(ascii: "a") + 10
            default: return nil
            }
        }
        var out = Data(capacity: bytes)
        for i in stride(from: 0, to: digits.count, by: 2) {
            guard let hi = nibble(digits[i]), let lo = nibble(digits[i + 1]) else { return nil }
            out.append(hi << 4 | lo)
        }
        return out
    }
}

/// The biometry that guards this device's unlock keys.
public enum Biometry: Sendable, Equatable {
    case faceID, touchID, opticID

    public var name: String {
        switch self {
        case .faceID: return "Face ID"
        case .touchID: return "Touch ID"
        case .opticID: return "Optic ID"
        }
    }
}

/// This device's unlock keys, one per (host fingerprint, profile id). P-256, never exported.
public protocol UnlockKeys: Sendable {
    /// nil where this device can't hold a key.
    var biometry: Biometry? { get }
    func has(host: Data, profile: String) -> Bool
    /// A new key in place of any old one: its 65-byte X9.63 public point.
    func create(host: Data, profile: String) throws -> Data
    /// ECDSA over SHA-256 of `message`, X9.62 DER. Blocks while the system asks for the person.
    func sign(_ message: Data, host: Data, profile: String, reason: String) throws -> Data
    func delete(host: Data, profile: String)
}

/// The key, or the password or PIN prompt, for one profile on one pinned host. A cancelled prompt
/// dials nothing, and nothing here retries on its own.
public struct ProfileUnlocker: Sendable {
    /// What the player typed, password or PIN, and the prompt's one switch.
    public struct Entry: Sendable {
        public let password: UnlockSecret
        public let switchOn: Bool

        public init(password: UnlockSecret, switchOn: Bool) {
            self.password = password
            self.switchOn = switchOn
        }
    }

    /// Shows the prompt with `error` and answers what was entered; nil is Cancel.
    public typealias Prompt = @Sendable (_ error: String?) async -> Entry?

    public enum Outcome: Sendable {
        /// Dial, with this unlock or none.
        case dial(ProfileUnlock?)
        case cancelled
    }

    public let keys: (any UnlockKeys)?
    /// The host's pinned certificate fingerprint.
    public let host: Data
    /// This device's certificate fingerprint; nil leaves only the prompt.
    public let device: Data?
    public let hostName: String
    public let challenge: @Sendable (_ profile: String) async throws -> UnlockChallenge
    /// What the switch enrols: nil offers "Stay signed in" instead.
    public let biometry: Biometry?

    public init(
        keys: (any UnlockKeys)?, host: Data, device: Data?, hostName: String,
        challenge: @escaping @Sendable (_ profile: String) async throws -> UnlockChallenge
    ) {
        self.keys = keys
        self.host = host
        self.device = device
        self.hostName = hostName
        self.challenge = challenge
        biometry = keys?.biometry
    }

    /// The prompt's switch, worded for this device.
    public var switchTitle: String {
        biometry.map { L10n.unlockWithBiometry(method: $0.name) }
            ?? String(localized: L10n.unlockStaySignedIn)
    }

    /// What a dial does first.
    public enum Gate: Sendable, Equatable {
        case dial
        /// Sign a fresh challenge with this device's key; no proof falls to the prompt.
        case key
        /// The password prompt, or the PIN prompt where `UnlockNeed.pin`.
        case prompt
    }

    /// The gate for a profile `enumerate` lists with `need`. `holdsKey`: this device has its
    /// unlock key for the profile.
    public static func gate(_ need: UnlockNeed, holdsKey: Bool) -> Gate {
        switch need {
        case .none: return .dial
        case .key, .pinKey: return holdsKey ? .key : .prompt
        case .password, .pin: return .prompt
        }
    }

    /// The host asks for the password or the PIN from a device that holds a key: it signed the
    /// device out, and the key goes.
    public static func staleKey(_ need: UnlockNeed, holdsKey: Bool) -> Bool {
        holdsKey && (need == .password || need == .pin)
    }

    /// After a `ProfileLocked` close. `sent`: the dial carried an unlock, so the prompt opens.
    /// A dial that carried none tries the key first. Never a dial on its own.
    public static func afterRefusal(_ need: UnlockNeed, holdsKey: Bool, sent: Bool) -> Gate {
        !sent && gate(need, holdsKey: holdsKey) == .key ? .key : .prompt
    }

    /// Before the dial, from the row's `unlock`.
    public func before(_ row: ListedProfile, prompt: Prompt) async -> Outcome {
        let gate = Self.gate(row.unlock, holdsKey: holdsKey(row))
        if gate == .dial { return .dial(nil) }
        if gate == .key, let proof = await keyProof(row) { return .dial(proof) }
        return await ask(row, error: nil, prompt: prompt)
    }

    /// After a `ProfileLocked` close, from the profile's fresh row: the key or the prompt, with
    /// the host's sentence as it arrived. `sent`: the refused dial carried an unlock.
    public func refused(
        _ row: ListedProfile, sent: Bool, said: String?, prompt: Prompt
    ) async -> Outcome {
        let gate = Self.afterRefusal(row.unlock, holdsKey: holdsKey(row), sent: sent)
        if gate == .key, let proof = await keyProof(row) { return .dial(proof) }
        return await ask(row, error: said, prompt: prompt)
    }

    /// This device holds `row`'s key, once a key the host signed out is deleted. The store is
    /// asked only for a profile that needs an unlock.
    func holdsKey(_ row: ListedProfile) -> Bool {
        guard row.unlock != .none, let keys, keys.has(host: host, profile: row.id) else {
            return false
        }
        guard Self.staleKey(row.unlock, holdsKey: true) else { return true }
        keys.delete(host: host, profile: row.id)
        return false
    }

    /// The signed challenge, or nil for anything that falls to the prompt: no key, no
    /// challenge, one named for another profile, a cancelled sheet, a key the system invalidated.
    func keyProof(_ row: ListedProfile) async -> ProfileUnlock? {
        guard let keys, let device, keys.has(host: host, profile: row.id),
              let fresh = try? await challenge(row.id),
              let message = fresh.message(for: row.id, device: device, box: host),
              let signature = try? keys.sign(
                  message, host: host, profile: row.id,
                  reason: L10n.appleUnlockBiometryReason(name: row.displayName, host: hostName))
        else { return nil }
        return ProfileUnlock.keyProof(signature)
    }

    /// The prompt `row` needs until it holds what the wire takes: a password, or a PIN sent as
    /// its ASCII digits. What was typed is wiped once read.
    func ask(_ row: ListedProfile, error: String?, prompt: Prompt) async -> Outcome {
        var error = error
        while true {
            guard let entry = await prompt(error) else { return .cancelled }
            var secret: UnlockSecret? = entry.password
            if row.unlock.pin {
                switch ProfileUnlock.pin(entry.password) {
                case .success(let digits): secret = digits
                case .failure(let problem): (secret, error) = (nil, problem.line)
                }
                entry.password.wipe()
            } else if !ProfileUnlock.fits(entry.password) {
                entry.password.wipe()
                (secret, error) = (nil, String(localized: L10n.unlockErrorTooLong))
            }
            guard let secret else { continue }
            // Key creation failing sends the secret without a key, and says nothing.
            var enroll: Data?
            if entry.switchOn, biometry != nil {
                enroll = try? keys?.create(host: host, profile: row.id)
            }
            let stay = entry.switchOn && biometry == nil
            return .dial(
                ProfileUnlock.password(secret, staySignedIn: stay, enroll: enroll)
                    ?? ProfileUnlock.password(secret, staySignedIn: stay))
        }
    }
}
