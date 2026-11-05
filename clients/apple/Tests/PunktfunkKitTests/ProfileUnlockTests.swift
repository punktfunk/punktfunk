// Profile unlock (CONTRACT §3, §6): the signed message's test vector through the core's builder,
// a software P-256 key standing in for the Secure Enclave, the rejection, and the flow's choices.

import CryptoKit
import PunktfunkCore
@testable import PunktfunkKit
import PunktfunkShared
import XCTest

/// A software P-256 key per (host, profile), in place of the Secure Enclave.
private final class SoftwareUnlockKeys: UnlockKeys, @unchecked Sendable {
    var keys: [String: P256.Signing.PrivateKey] = [:]
    var biometry: Biometry? = .faceID
    var signFails = false
    var signed: [Data] = []

    private func slot(_ host: Data, _ profile: String) -> String {
        host.base64EncodedString() + "/" + profile
    }

    func has(host: Data, profile: String) -> Bool { keys[slot(host, profile)] != nil }

    func create(host: Data, profile: String) throws -> Data {
        let key = P256.Signing.PrivateKey()
        keys[slot(host, profile)] = key
        return key.publicKey.x963Representation
    }

    func sign(_ message: Data, host: Data, profile: String, reason: String) throws -> Data {
        guard !signFails, let key = keys[slot(host, profile)] else { throw CocoaError(.userCancelled) }
        signed.append(message)
        return try key.signature(for: message).derRepresentation
    }

    func delete(host: Data, profile: String) { keys[slot(host, profile)] = nil }

    func publicKey(_ host: Data, _ profile: String) -> P256.Signing.PublicKey? {
        keys[slot(host, profile)]?.publicKey
    }
}

/// What the prompt was shown with, and what it answers in turn; nil is Cancel.
private final class PromptScript: @unchecked Sendable {
    var shown: [String?] = []
    var answers: [ProfileUnlocker.Entry?]

    init(_ answers: [ProfileUnlocker.Entry?]) { self.answers = answers }

    var prompt: ProfileUnlocker.Prompt {
        { error in
            self.shown.append(error)
            return self.answers.isEmpty ? nil : self.answers.removeFirst()
        }
    }
}

final class ProfileUnlockTests: XCTestCase {
    // CONTRACT §3's vector.
    private let challenge = Data(1...32)
    private let seat = "0123456789abcdef0123456789abcdef"
    private let device = Data(repeating: 0xaa, count: 32)
    private let box = Data(repeating: 0xbb, count: 32)

    private func hex(_ data: some Sequence<UInt8>) -> String {
        data.map { String(format: "%02x", $0) }.joined()
    }

    private func unlocker(_ keys: SoftwareUnlockKeys?, challenges: PromptScript? = nil)
        -> ProfileUnlocker
    {
        let (challenge, seat) = (challenge, seat)
        return ProfileUnlocker(keys: keys, host: box, device: device, hostName: "Desk") { id in
            challenges?.shown.append(id)
            return UnlockChallenge(challenge: challenge, seat: seat)
        }
    }

    private func entry(_ password: String, switchOn: Bool = false) -> ProfileUnlocker.Entry {
        ProfileUnlocker.Entry(password: UnlockSecret(password), switchOn: switchOn)
    }

    private func row(_ unlock: UnlockNeed) -> ListedProfile {
        ListedProfile(id: "anna", displayName: "Anna", unlock: unlock)
    }

    private func bytes(_ unlock: ProfileUnlock) -> Data { unlock.secret.withBytes { Data($0) } }

    func testTheMessageMatchesTheContractVector() throws {
        let message = try XCTUnwrap(
            ProfileUnlock.message(challenge: challenge, seat: seat, device: device, box: box))
        XCTAssertEqual(message.count, 152)
        XCTAssertEqual(
            message, Data("punktfunk-seat-unlock-v1".utf8) + challenge + Data(seat.utf8) + device + box)
        XCTAssertEqual(
            hex(SHA256.hash(data: message)),
            "31de46821f3058729b14c3585ad13159085ecb0d18c87194a34b1bddc8ddc31a")
    }

    func testAMisshapenArgumentBuildsNoMessage() {
        XCTAssertNil(ProfileUnlock.message(
            challenge: challenge.prefix(31), seat: seat, device: device, box: box))
        XCTAssertNil(ProfileUnlock.message(
            challenge: challenge, seat: seat.uppercased(), device: device, box: box))
        XCTAssertNil(ProfileUnlock.message(
            challenge: challenge, seat: String(seat.dropLast()), device: device, box: box))
    }

    func testAKeyRowSignsTheChallengeAndItVerifies() async throws {
        let keys = SoftwareUnlockKeys()
        _ = try keys.create(host: box, profile: "anna")
        let asked = PromptScript([])
        let prompt = PromptScript([])
        let outcome = await unlocker(keys, challenges: asked).before(row(.key), prompt: prompt.prompt)
        guard case .dial(let unlock?) = outcome else { return XCTFail("no key proof: \(outcome)") }
        XCTAssertEqual(unlock.kind, .keyProof)
        XCTAssertTrue(prompt.shown.isEmpty, "a key unlock shows no prompt of ours")
        XCTAssertEqual(asked.shown, ["anna"])
        let message = try XCTUnwrap(
            ProfileUnlock.message(challenge: challenge, seat: seat, device: device, box: box))
        XCTAssertEqual(keys.signed, [message])
        let signature = try P256.Signing.ECDSASignature(derRepresentation: bytes(unlock))
        let key = try XCTUnwrap(keys.publicKey(box, "anna"))
        XCTAssertTrue(key.isValidSignature(signature, for: message))
    }

    func testAFailedOrMissingKeyFallsToThePassword() async {
        let keys = SoftwareUnlockKeys()
        let none = PromptScript([entry("hunter2")])
        guard case .dial(let typed?) = await unlocker(keys).before(row(.key), prompt: none.prompt)
        else { return XCTFail("no password unlock") }
        XCTAssertEqual(typed.kind, .password)
        XCTAssertEqual(none.shown, [nil])

        _ = try? keys.create(host: box, profile: "anna")
        keys.signFails = true
        let cancelled = PromptScript([entry("hunter2")])
        guard case .dial(let after?) = await unlocker(keys).before(row(.key), prompt: cancelled.prompt)
        else { return XCTFail("no password unlock") }
        XCTAssertEqual(after.kind, .password)
        XCTAssertTrue(keys.has(host: box, profile: "anna"), "a failed sheet keeps the key")
    }

    func testACancelledPromptDialsNothing() async {
        let keys = SoftwareUnlockKeys()
        let script = PromptScript([nil])
        guard case .cancelled = await unlocker(keys).before(row(.password), prompt: script.prompt)
        else { return XCTFail("a cancel dialed") }
        let again = PromptScript([nil])
        guard case .cancelled = await unlocker(keys).refused(
            profile: "anna", said: "Wrong password.", prompt: again.prompt)
        else { return XCTFail("a cancel dialed") }
        XCTAssertEqual(again.shown, ["Wrong password."])
    }

    func testAPasswordRowDeletesTheStaleKey() async throws {
        let keys = SoftwareUnlockKeys()
        _ = try keys.create(host: box, profile: "anna")
        _ = try keys.create(host: box, profile: "ben")
        let script = PromptScript([nil])
        _ = await unlocker(keys).before(row(.password), prompt: script.prompt)
        XCTAssertFalse(keys.has(host: box, profile: "anna"))
        XCTAssertTrue(keys.has(host: box, profile: "ben"))
        XCTAssertEqual(script.shown, [nil])
    }

    func testAnUnlockedRowDialsWithNothing() async {
        let script = PromptScript([])
        guard case .dial(nil) = await unlocker(SoftwareUnlockKeys()).before(
            row(.none), prompt: script.prompt)
        else { return XCTFail("an open profile asked for something") }
        XCTAssertTrue(script.shown.isEmpty)
    }

    func testTheSwitchEnrolsWhereThereIsBiometry() async throws {
        let keys = SoftwareUnlockKeys()
        let flow = unlocker(keys)
        XCTAssertEqual(flow.switchTitle, "Unlock with Face ID on this device")
        let script = PromptScript([entry("pässwörd ", switchOn: true)])
        guard case .dial(let unlock?) = await flow.before(row(.password), prompt: script.prompt)
        else { return XCTFail("no unlock") }
        XCTAssertEqual(bytes(unlock), Data("pässwörd ".utf8), "sent as typed, never trimmed")
        XCTAssertFalse(unlock.staySignedIn)
        let key = try XCTUnwrap(keys.publicKey(box, "anna"))
        XCTAssertEqual(unlock.enrollKey, key.x963Representation)
        XCTAssertEqual(unlock.enrollKey?.count, 65)
    }

    func testTheSwitchStaysSignedInWithoutBiometry() async {
        let keys = SoftwareUnlockKeys()
        keys.biometry = nil
        for flow in [unlocker(keys), unlocker(nil)] {
            XCTAssertEqual(flow.switchTitle, "Stay signed in on this device")
            let script = PromptScript([entry("hunter2", switchOn: true)])
            guard case .dial(let unlock?) = await flow.before(row(.password), prompt: script.prompt)
            else { return XCTFail("no unlock") }
            XCTAssertTrue(unlock.staySignedIn)
            XCTAssertNil(unlock.enrollKey)
        }
        XCTAssertTrue(keys.keys.isEmpty)
    }

    func testATooLongPasswordAsksAgain() async {
        let long = entry(String(repeating: "x", count: 513))
        let script = PromptScript([long, entry("hunter2")])
        guard case .dial(let unlock?) = await unlocker(nil).before(row(.password), prompt: script.prompt)
        else { return XCTFail("no unlock") }
        XCTAssertEqual(script.shown, [nil, "That password is too long."])
        XCTAssertEqual(long.password.count, 0, "the refused password is wiped")
        XCTAssertEqual(bytes(unlock), Data("hunter2".utf8))
    }

    func testProfileLockedMapsBothWays() {
        let status = PUNKTFUNK_STATUS_REJECTED_PROFILE_LOCKED.rawValue
        XCTAssertEqual(status, -37)
        XCTAssertEqual(HostRejection(status: status), .profileLocked)
        XCTAssertEqual(HostRejection.profileLocked.status, status)
        XCTAssertEqual(
            ConnectOffer.failureMessage(
                PunktfunkClientError.rejected(.profileLocked), hostName: "Desk", pinned: true,
                requestAccess: false, callerRecovers: true),
            "Desk: This profile needs its account password.")
        XCTAssertEqual(
            ConnectOffer.failureMessage(
                PunktfunkClientError.rejected(.profileLocked, said: "Wrong password."),
                hostName: "Desk", pinned: true, requestAccess: false, callerRecovers: false),
            "Desk: Wrong password.")
    }

    func testTheCStructCarriesTheUnlock() throws {
        let point = Data([0x04] + [UInt8](repeating: 7, count: 64))
        let unlock = try XCTUnwrap(
            ProfileUnlock.password(UnlockSecret("hunter2"), enroll: point))
        unlock.withC { c in
            XCTAssertEqual(Int32(c.pointee.kind), PUNKTFUNK_UNLOCK_PASSWORD)
            XCTAssertEqual(c.pointee.stay_signed_in, 0)
            XCTAssertEqual(Int32(c.pointee.enroll_alg), PUNKTFUNK_ENROLL_P256)
            XCTAssertEqual(Data(bytes: c.pointee.secret, count: Int(c.pointee.secret_len)),
                           Data("hunter2".utf8))
            XCTAssertEqual(Data(bytes: c.pointee.enroll_key, count: Int(c.pointee.enroll_key_len)),
                           point)
        }
        let proof = try XCTUnwrap(ProfileUnlock.keyProof(Data([0x30, 0x01])))
        proof.withC { c in
            XCTAssertEqual(Int32(c.pointee.kind), PUNKTFUNK_UNLOCK_KEY_PROOF)
            XCTAssertEqual(Int32(c.pointee.enroll_alg), PUNKTFUNK_ENROLL_NONE)
            XCTAssertNil(c.pointee.enroll_key)
        }
        XCTAssertNil(ProfileUnlock.password(UnlockSecret("")))
        XCTAssertNil(ProfileUnlock.password(UnlockSecret("a\u{0}b")))
        XCTAssertNil(ProfileUnlock.password(UnlockSecret("x"), enroll: point.prefix(64)))
        XCTAssertNil(ProfileUnlock.keyProof(Data(count: 1025)))
    }

    func testAnUnlockDialsOncePinnedAndPrintsNoSecret() throws {
        let unlock = try XCTUnwrap(ProfileUnlock.password(UnlockSecret("hunter2")))
        XCTAssertFalse("\(unlock)".contains("hunter2"))
        XCTAssertFalse(String(reflecting: unlock).contains("hunter2"))
        XCTAssertThrowsError(try PunktfunkConnection(
            host: "192.0.2.1", width: 1, height: 1, refreshHz: 60, unlock: unlock
        )) { error in
            guard case PunktfunkClientError.invalidPin = error else {
                return XCTFail("unpinned unlock: \(error)")
            }
        }
        XCTAssertEqual(unlock.secret.count, 0, "the connect wipes the unlock")
        XCTAssertThrowsError(try PunktfunkConnection(
            host: "192.0.2.1", width: 1, height: 1, refreshHz: 60,
            pinSHA256: Data(repeating: 1, count: 32), unlock: unlock
        )) { error in
            guard case PunktfunkClientError.status(PUNKTFUNK_STATUS_INVALID_ARG.rawValue) = error
            else { return XCTFail("a wiped unlock dialed: \(error)") }
        }
    }

    func testAChallengeParsesOnlyItsShape() {
        func parse(_ challenge: String, _ seat: String) -> UnlockChallenge? {
            UnlockChallenge(json: Data(#"{"challenge":"\#(challenge)","seat":"\#(seat)"}"#.utf8))
        }
        let parsed = parse(hex(challenge), seat)
        XCTAssertEqual(parsed, UnlockChallenge(challenge: challenge, seat: seat))
        XCTAssertNil(parse(hex(challenge).uppercased(), seat))
        XCTAssertNil(parse(String(hex(challenge).dropLast(2)), seat))
        XCTAssertNil(parse(hex(challenge), seat + "00"))
        XCTAssertNil(parse(hex(challenge), "zz" + seat.dropFirst(2)))
        XCTAssertNil(UnlockChallenge(json: Data("[]".utf8)))
    }

    func testARowSaysWhatItNeedsAndALockedSeatDials() throws {
        let raw = """
            [{"id":"a","display_name":"A","unlock":"key","seat":{"state":"stopped"}},
             {"id":"b","display_name":"B","seat":{"state":"stopped"}},
             {"id":"c","display_name":"C","unlock":"retina"}]
            """
        let rows = try JSONDecoder().decode([ListedProfile].self, from: Data(raw.utf8))
        XCTAssertEqual(rows.map(\.unlock), [.key, .none, .none])
        XCTAssertEqual(HostProfiles.seatGate(rows[0]), .dial, "a locked seat starts by unlock")
        XCTAssertEqual(HostProfiles.seatGate(rows[1]), .wake)
    }
}
