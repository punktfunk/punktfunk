// The Steam Controller 2 protocol tables: the per-report characteristic map, the stripped-length
// table against `clients/shared/sc2-vectors.json`, the
// feature-command bytes (hardware-confirmed 2026-06-08), the state parser and the WIRE_MAP —
// all pure statics on `Sc2Device` (the DualSenseHIDTests convention: pin the wire layout
// without a physical pad). Plus the escape-chord invariant mirror (GamepadEscapeChordTests
// pattern): `Sc2Capture` re-declares the chord because the original is main-actor-isolated,
// and the two must not drift.

import XCTest

@testable import PunktfunkKit

final class Sc2DeviceTests: XCTestCase {
    func testOutputCharUUIDIsIdPlus0x35() {
        // Valve routes output report id 0xNN to characteristic 100F6C<NN+0x35> (verified
        // per-actuator on-device 2026-06-09).
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x80), "100f6cb5-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x81), "100f6cb6-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x82), "100f6cb7-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x83), "100f6cb8-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x84), "100f6cb9-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x85), "100f6cba-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x86), "100f6cbb-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x87), "100f6cbc-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x88), "100f6cbd-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0x89), "100f6cbe-1735-4313-b402-38567131e5f3")
        // The +0x35 wraps modulo 256 rather than overflowing.
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0xF0), "100f6c25-1735-4313-b402-38567131e5f3")
    }

    /// `clients/shared/sc2-vectors.json`: the host's id-INCLUDED lengths, which
    /// `pf_driver_proto::triton::out_report_len` and the Kotlin table replay too.
    func testStrippedLenPlusOneMatchesTheSharedVectors() throws {
        for row in try sharedRows("out_report_len") {
            let id = try UInt8(XCTUnwrap(row["id"] as? Int))
            // Undeclared ids answer nil: clamp to what arrived, never guess a length.
            let want = (row["len"] as? Int).map { $0 - 1 }
            XCTAssertEqual(Sc2Device.strippedOutputLen(id: id), want, "id 0x\(String(id, radix: 16))")
        }
        XCTAssertNil(Sc2Device.strippedOutputLen(id: 0x42))
        XCTAssertNil(Sc2Device.strippedOutputLen(id: 0x00))
    }

    // MARK: - USB identity (macOS `Sc2UsbLink`)

    func testUsbPIDsCoverTheWiredPadAndBothPucksButNotBLE() {
        XCTAssertEqual(Sc2Device.vidValve, 0x28DE)
        // Exactly the ids Android's `Sc2Device.USB_PIDS` matches: wired + both dongles.
        XCTAssertEqual(Sc2Device.usbPIDs.sorted(), [0x1302, 0x1304, 0x1305])
        // 0x1303 is the DIRECT BLE identity. Matching it on USB would open a device the BLE link
        // owns, so the two transports would fight over one pad.
        XCTAssertFalse(Sc2Device.usbPIDs.contains(Sc2Device.pidBLE))
        XCTAssertEqual(Sc2Device.pidBLE, 0x1303)
    }

    func testOnlyTheDonglePIDsAreAPuck() {
        // Decides the declared wire kind AND whether wireless-status reports are authoritative —
        // getting this backwards tears the wire slot down on a wired pad (Android's on-glass bug).
        XCTAssertTrue(Sc2Device.isDongle(pid: Sc2Device.pidDongleProteus))
        XCTAssertTrue(Sc2Device.isDongle(pid: Sc2Device.pidDongleNereid))
        XCTAssertFalse(Sc2Device.isDongle(pid: Sc2Device.pidWired))
        XCTAssertFalse(Sc2Device.isDongle(pid: Sc2Device.pidBLE))
        XCTAssertFalse(Sc2Device.isDongle(pid: 0x0000))
    }

    func testTheMatchedUsagePairIsTheVendorCollectionNotAKeyboardOrMouse() {
        // The SC2's controller collection is vendor-defined page 0xFF00 usage 0x01 — the THIRD
        // top-level collection in `pf_driver_proto::triton::RDESC`, behind the lizard mouse
        // (Generic Desktop 01:02) and keyboard (01:06).
        XCTAssertEqual(Sc2Device.usagePageVendor, 0xFF00)
        XCTAssertEqual(Sc2Device.usageController, 0x01)
        // The invariant that matters: macOS surfaces one IOHIDDevice per collection, so matching
        // Generic Desktop (0x01) would open the pad's KEYBOARD — putting the whole app behind the
        // Input Monitoring TCC gate for no benefit. Vendor pages start at 0xFF00.
        XCTAssertGreaterThanOrEqual(Sc2Device.usagePageVendor, 0xFF00)
        XCTAssertNotEqual(Sc2Device.usagePageVendor, 0x01)
        XCTAssertEqual(Sc2Device.dongleIfaces, 2...5)
    }

    func testWirelessStatusPayloadValues() {
        // `idWireless`/`idWirelessX` byte 1 — mirrors Android's WIRELESS_CONNECT/DISCONNECT.
        XCTAssertEqual(Sc2Device.wirelessDisconnect, 1)
        XCTAssertEqual(Sc2Device.wirelessConnect, 2)
    }

    func testEngravedSerialParsing() {
        // Reply shape: report id + binary header, then the engraved serial as printable ASCII.
        var reply = [UInt8](repeating: 0, count: 65)
        reply[0] = Sc2Device.featureSerial
        reply[1] = 0x11
        reply[2] = 0xFE
        let serial = Array("FXA1234567890".utf8)
        reply.replaceSubrange(8 ..< 8 + serial.count, with: serial)
        XCTAssertEqual(Sc2Device.parseSerial(reply), "FXA1234567890")
        // Runs outside 8–20 never qualify — a garbage reply degrades to no serial, not a
        // wrong one: too short (7), and an over-long printable run (an ASCII error string).
        XCTAssertNil(Sc2Device.parseSerial(Array("ABCDEFG".utf8)))
        XCTAssertNil(Sc2Device.parseSerial(Array(repeating: UInt8(ascii: "A"), count: 65)))
        XCTAssertNil(Sc2Device.parseSerial([0x02, 0x41, 0x42, 0x00, 0x43, 0x44]))
        XCTAssertNil(Sc2Device.parseSerial([]))
        // Ties keep the FIRST longest run.
        XCTAssertEqual(Sc2Device.parseSerial(Array("ABCDEFGH:IJKLMNOP".utf8)), "ABCDEFGH")
    }

    func testFeatureCommandBytesVerbatim() {
        // DISABLE_LIZARD: [1][0x87 ID_SET_SETTINGS_VALUES][3][9 SETTING_LIZARD_MODE][0 0 u16],
        // zero-padded to the 64-byte feature size (Android sends the identical frame).
        XCTAssertEqual(Sc2Device.disableLizard.count, 64)
        XCTAssertEqual(
            Array(Sc2Device.disableLizard[0 ..< 6]), [0x01, 0x87, 0x03, 0x09, 0x00, 0x00])
        XCTAssertTrue(Sc2Device.disableLizard[6...].allSatisfy { $0 == 0 })
        // NORMALIZE_JOYSTICKS (USB only): the same framing with SETTING_ENABLE_RAW_JOYSTICK
        // (0x2e) = 0. Android sends the identical 64-byte frame — its `Sc2DeviceTest` pins these
        // same bytes from the other side, so the pair goes red on either edit alone.
        XCTAssertEqual(Sc2Device.normalizeJoysticks.count, 64)
        XCTAssertEqual(
            Array(Sc2Device.normalizeJoysticks[0 ..< 6]), [0x01, 0x87, 0x03, 0x2E, 0x00, 0x00])
        XCTAssertTrue(Sc2Device.normalizeJoysticks[6...].allSatisfy { $0 == 0 })
        // The gyro-enable REFERENCE (WRITE_REGISTER reg 0x30 GYRO_MODE val 0x0018) — kept for
        // logging/tests only; nothing in the client may ever send it unprompted.
        XCTAssertEqual(Sc2Device.gyroEnableReference, [0x01, 0x87, 0x03, 0x30, 0x18, 0x00])
        // SDL's lizard-off refresh cadence.
        XCTAssertEqual(Sc2Device.lizardRefreshSeconds, 3.0)
    }

    /// One 46-byte BLE-shaped state report with the client-consumed fields planted.
    private func stateReport(
        id: UInt8 = Sc2Device.idStateBLE, buttons: UInt32 = 0,
        lt: Int16 = 0, rt: Int16 = 0,
        lsX: Int16 = 0, lsY: Int16 = 0, rsX: Int16 = 0, rsY: Int16 = 0
    ) -> [UInt8] {
        var r = [UInt8](repeating: 0, count: 46)
        r[0] = id
        r[1] = 0x42 // seq — parseState must not read it
        func put32(_ v: UInt32, at o: Int) {
            r[o] = UInt8(v & 0xFF)
            r[o + 1] = UInt8((v >> 8) & 0xFF)
            r[o + 2] = UInt8((v >> 16) & 0xFF)
            r[o + 3] = UInt8((v >> 24) & 0xFF)
        }
        func put16(_ v: Int16, at o: Int) {
            let u = UInt16(bitPattern: v)
            r[o] = UInt8(u & 0xFF)
            r[o + 1] = UInt8(u >> 8)
        }
        put32(buttons, at: 2)
        put16(lt, at: 6)
        put16(rt, at: 8)
        put16(lsX, at: 10)
        put16(lsY, at: 12)
        put16(rsX, at: 14)
        put16(rsY, at: 16)
        return r
    }

    func testParseStateTruthTable() {
        var out = Sc2Device.State()
        // Buttons LE u32 @2; triggers i16 @6/@8 clamped to 0...32767 then >>7; sticks i16
        // @10..16 — Android's parseState, byte for byte.
        let report = stateReport(
            buttons: Sc2Device.btnA | Sc2Device.btnSteam | Sc2Device.btnRPadClick,
            lt: 32767, rt: -100, lsX: -32768, lsY: 32767, rsX: 1234, rsY: -1234)
        XCTAssertTrue(Sc2Device.parseState(report, into: &out))
        XCTAssertEqual(out.buttons, Sc2Device.btnA | Sc2Device.btnSteam | Sc2Device.btnRPadClick)
        XCTAssertEqual(out.lt, 255) // 32767 >> 7
        XCTAssertEqual(out.rt, 0) // negative clamps to 0
        XCTAssertEqual(out.lsX, -32768)
        XCTAssertEqual(out.lsY, 32767)
        XCTAssertEqual(out.rsX, 1234)
        XCTAssertEqual(out.rsY, -1234)
        // All three state shapes parse (identical offsets for everything read here)…
        XCTAssertTrue(Sc2Device.parseState(stateReport(id: Sc2Device.idState), into: &out))
        XCTAssertTrue(
            Sc2Device.parseState(stateReport(id: Sc2Device.idStateTimestamp), into: &out))
        // …and non-state / short reports answer false.
        XCTAssertFalse(Sc2Device.parseState([Sc2Device.idBattery, 0, 0], into: &out))
        var short = stateReport()
        short.removeSubrange(17...)
        XCTAssertFalse(Sc2Device.parseState(short, into: &out))
    }

    /// `clients/shared/sc2-vectors.json` `buttons`: Kotlin's `WIRE_MAP` and the host's typed
    /// fallback replay the same rows.
    func testWireMapMatchesTheSharedVectors() throws {
        let expected = try sharedRows("buttons").map { row -> (UInt32, UInt32) in
            let sc2 = try XCTUnwrap(row["sc2"] as? Int)
            let wire = try XCTUnwrap(row["wire"] as? Int)
            return (UInt32(sc2), UInt32(wire))
        }
        XCTAssertEqual(Sc2Device.wireMap.count, expected.count)
        for (sc2, wire) in expected {
            XCTAssertEqual(Sc2Device.wireButtons(sc2), wire, "sc2 bit 0x\(String(sc2, radix: 16))")
        }
        // Every mapped bit at once, and nothing else.
        let allSc2 = expected.reduce(UInt32(0)) { $0 | $1.0 }
        let allWire = expected.reduce(UInt32(0)) { $0 | $1.1 }
        XCTAssertEqual(Sc2Device.wireButtons(allSc2), allWire)
        // Unmapped SC2 bits translate to nothing.
        XCTAssertEqual(Sc2Device.wireButtons(~allSc2), 0)
        XCTAssertEqual(Sc2Device.wireButtons(0), 0)
    }

    /// The identity rides core's `[len][request][len][reply]` packing, and core lists the queries.
    func testPadIdentityPacksAsCoreReadsIt() {
        let id = PunktfunkConnection.PadIdentity(serial: "FXA0000000001", replies: [
            .init(request: [0x01, 0x83, 0x00], reply: [0x01, 0x83, 0x1E]),
            .init(request: [0x01, 0xAE, 0x15, 0x01], reply: Array(repeating: 0x46, count: 70)),
        ])
        let packed = id.packedReplies
        XCTAssertEqual(Array(packed[..<8]), [3, 0x01, 0x83, 0x00, 3, 0x01, 0x83, 0x1E])
        XCTAssertEqual(packed[8], 4)
        XCTAssertEqual(packed[13], 64, "a part is cut at 64 bytes")
        XCTAssertEqual(packed.count, 8 + 5 + 65)
        XCTAssertEqual(PunktfunkConnection.sc2IdentityRequests(puck: false).count, 8)
        XCTAssertEqual(PunktfunkConnection.sc2IdentityRequests(puck: true).count, 12)
        XCTAssertEqual(PunktfunkConnection.sc2IdentityRequests(puck: false).first, [0x01, 0x83, 0x00])
    }

    /// One section of `clients/shared/sc2-vectors.json`.
    private func sharedRows(_ key: String) throws -> [[String: Any]] {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/sc2-vectors.json")
        let root = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        return try XCTUnwrap(root[key] as? [[String: Any]])
    }

    func testSc2ButtonsInvertsTheWireMap() {
        // A chord is written in wire bits; the parsed state speaks the device's layout, so the
        // ring's swallow needs the map read backwards.
        XCTAssertEqual(
            Sc2Device.sc2Buttons(forWire: GamepadWire.back | GamepadWire.a),
            Sc2Device.btnView | Sc2Device.btnA)
        XCTAssertEqual(Sc2Device.sc2Buttons(forWire: 0), 0)
        // Round-trip: every mapped wire bit comes back as itself.
        for (sc2, wire) in Sc2Device.wireMap {
            XCTAssertEqual(Sc2Device.sc2Buttons(forWire: wire), sc2)
            XCTAssertEqual(Sc2Device.wireButtons(Sc2Device.sc2Buttons(forWire: wire)), wire)
        }
    }
}

/// `Sc2Capture` re-declares the escape chord (the original is `@MainActor`-isolated and the
/// capture reads its mask on the BLE queue) — this pins the two masks and the hold duration
/// together, because the failure of a drift is invisible until someone can't leave a stream
/// with a captured SC2 in their hands (the GamepadEscapeChordTests rationale, one class over).
@MainActor
final class Sc2EscapeChordMirrorTests: XCTestCase {
    func testChordMaskMirrorsGamepadCapture() {
        XCTAssertEqual(Sc2Capture.escapeChord, GamepadCapture.escapeChord)
    }

    func testHoldMirrorsTheCrossClientDisconnectHold() {
        // pf-client-core's DISCONNECT_HOLD — 1.5 s on every client (GamepadCapture's own copy
        // is private; the value is the cross-client contract being pinned).
        XCTAssertEqual(Sc2Capture.disconnectHold, 1.5)
    }

    func testStatsChordMirrorsGamepadCapture() {
        XCTAssertEqual(Sc2RingGate.statsChord, GamepadCapture.statsChord)
    }

    /// The three chords must stay reachable independently: no way of holding one passes through
    /// another on the way, or the dial would open while someone reaches for the exit.
    func testChordsDoNotContainOneAnother() {
        XCTAssertEqual(Sc2RingGate.ringChord, GamepadWire.back | GamepadWire.a)
        let chords = [Sc2RingGate.ringChord, Sc2RingGate.statsChord, Sc2Capture.escapeChord]
        for (i, one) in chords.enumerated() {
            for other in chords[(i + 1)...] {
                XCTAssertNotEqual(one & other, one)
                XCTAssertNotEqual(one & other, other)
            }
        }
    }
}
