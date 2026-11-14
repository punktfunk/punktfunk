// SC2 framing: the report-id prepend on the way up (the 2026-06-09 root cause — a GATT
// characteristic VALUE carries no id byte, and without re-prepending 0x45 an id-keyed consumer
// drops ~every frame), the OUTPUT strip+trim and FEATURE 0x01-strip on the way down, and the
// wire-constant pins against the regenerated C header (GamepadWireTests pattern: a Swift-side
// edit that drifts from the Rust contract fails CI).

import PunktfunkCore
import XCTest

@testable import PunktfunkKit

final class Sc2FramingTests: XCTestCase {
    // MARK: - Incoming (up-path)

    func testStateSizedPayloadGetsThe0x45Prepend() {
        // The live shape: a 45-byte TritonMTUNoQuat_t payload → a 46-byte id-first frame.
        var payload = [UInt8](repeating: 0, count: 45)
        payload[0] = 0xE5 // seq — the byte the queue used to mistake for a report id
        let framed = Sc2Device.frameIncoming(payload)
        XCTAssertEqual(framed.count, 46)
        XCTAssertEqual(framed[0], Sc2Device.idStateBLE)
        XCTAssertEqual(Array(framed[1...]), payload)
        // The rule's floor: exactly 40 bytes still counts as state-sized.
        XCTAssertEqual(Sc2Device.frameIncoming([UInt8](repeating: 1, count: 40)).count, 41)
    }

    func testShortPayloadPassesThroughUnmodified() {
        // Battery/status payloads keep whatever framing the firmware gave them — the host's
        // virtual pad handles the rest. (No 0x45→0x42 rewrite and no 54-byte pad on ANY path:
        // that belongs to a synthetic-USB queue contract, not punktfunk's.)
        let battery: [UInt8] = [Sc2Device.idBattery, 0x64, 0x01]
        XCTAssertEqual(Sc2Device.frameIncoming(battery), battery)
        XCTAssertEqual(
            Sc2Device.frameIncoming([UInt8](repeating: 2, count: 39)).count, 39)
    }

    // MARK: - Outgoing OUTPUT (kind 0): id selects the char, is stripped, payload trimmed

    func testOutputStripAndTrimPerId() {
        // A native-length 0x80 grip rumble (10 B wire = 1 id + 9 payload) → char B5, 9 bytes.
        let rumble: [UInt8] = [0x80, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]
        let write = Sc2Device.outputWrite(frame: rumble)
        XCTAssertEqual(write?.charUUID, "100f6cb5-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(write?.payload, Array(rumble[1...]))
        // 0x82 — Steam's ping/test buzz — rides B7 with a 3-byte payload (the historical
        // mis-route regression: it failed while pointed at B5).
        let buzz: [UInt8] = [0x82, 0x03, 0x01, 0xFF]
        let buzzWrite = Sc2Device.outputWrite(frame: buzz)
        XCTAssertEqual(buzzWrite?.charUUID, "100f6cb7-1735-4313-b402-38567131e5f3")
        XCTAssertEqual(buzzWrite?.payload, [0x03, 0x01, 0xFF])
    }

    func testOutputToleratesAnOldHosts64BytePadding() {
        // Current Windows hosts pre-trim OUTPUT frames to out_report_len(id); an older host
        // pads to 64 B. The client clamp is therefore redundant-but-kept — the GATT write must
        // carry exactly the declared stripped length either way.
        var padded = [UInt8](repeating: 0, count: 64)
        padded[0] = 0x80
        for i in 1 ... 9 { padded[i] = UInt8(i) }
        padded[10] = 0xEE // padding garbage past the declared length — must be dropped
        let write = Sc2Device.outputWrite(frame: padded)
        XCTAssertEqual(write?.payload, [1, 2, 3, 4, 5, 6, 7, 8, 9])
    }

    func testOutputClampsToWhatArrived() {
        // A frame shorter than the declared length clamps to what arrived (never over-reads).
        let short: [UInt8] = [0x80, 0x01, 0x02]
        XCTAssertEqual(Sc2Device.outputWrite(frame: short)?.payload, [0x01, 0x02])
        // Too short to carry any payload → nil (the len<2 guard).
        XCTAssertNil(Sc2Device.outputWrite(frame: [0x80]))
        XCTAssertNil(Sc2Device.outputWrite(frame: []))
    }

    func testOutputIdOutsideTheBlockIsNeverWritten() {
        // 0xFF + 0x35 wraps to 0x34, the feature characteristic: a host must not reach it this way.
        XCTAssertEqual(Sc2Device.outputCharUUID(id: 0xFF), Sc2Device.reportCharUUID.lowercased())
        XCTAssertNil(Sc2Device.outputWrite(frame: [0xFF, 0x8E, 0x00]))
        XCTAssertNil(Sc2Device.outputWrite(frame: [0x90, 1, 2, 3, 4, 5]))
        XCTAssertNil(Sc2Device.outputWrite(frame: [0x7F, 1]))
    }

    // MARK: - Outgoing FEATURE (kind 1): strip the 0x01 channel id, pass the rest whole

    func testFeatureStripsTheChannelIdAndKeepsThePadding() {
        // FEATURE frames deliberately arrive WHOLE from the host (64 B, un-trimmed): strip the
        // 0x01, keep everything else — the firmware accepts the zero-padded form (Android sends
        // DISABLE_LIZARD padded to 64 B the same way).
        var lizard = [UInt8](repeating: 0, count: 64)
        lizard.replaceSubrange(0 ..< 6, with: [0x01, 0x87, 0x03, 0x09, 0x00, 0x00])
        let payload = Sc2Device.featurePayload(frame: lizard)
        XCTAssertEqual(payload?.count, 63)
        XCTAssertEqual(Array(payload?.prefix(5) ?? []), [0x87, 0x03, 0x09, 0x00, 0x00])
        // The short (un-padded) form works too — what the client's own keepalive would look
        // like before padding.
        XCTAssertEqual(
            Sc2Device.featurePayload(frame: [0x01, 0x87, 0x03, 0x30, 0x18, 0x00]),
            [0x87, 0x03, 0x30, 0x18, 0x00])
        // Too short to carry a command → nil.
        XCTAssertNil(Sc2Device.featurePayload(frame: [0x01]))
        XCTAssertNil(Sc2Device.featurePayload(frame: []))
    }

    func testLizardOffKeepaliveLeadsWithTheSettingsCommand() {
        // The client's own keepalive (`Sc2BleLink.sendLizardOff`) writes EXACTLY this frame:
        // `featurePayload(disableLizard)` — the same framing as a host-forwarded feature write,
        // so BOTH 100F6C34 writers share one code path. The characteristic VALUE carries no
        // 0x01 channel report-id (the firmware parses byte 0 as the settings-command id), so
        // the write MUST lead with 0x87 (ID_SET_SETTINGS_VALUES): unstripped, the frame reads
        // as command 0x01 and lizard-off silently fails for the whole pre-claim window.
        let write = Sc2Device.featurePayload(frame: Sc2Device.disableLizard)
        XCTAssertEqual(write?.first, 0x87) // ID_SET_SETTINGS_VALUES — never the 0x01 channel id
        XCTAssertEqual(write, Array(Sc2Device.disableLizard.dropFirst()))
        XCTAssertEqual(write?.count, 63) // the 64-byte frame minus the channel id, padding kept
        XCTAssertEqual(
            Array(write?.prefix(5) ?? []),
            [0x87, 0x03, 0x09, 0x00, 0x00]) // SET_SETTINGS len=3 {LIZARD_MODE, OFF u16}
    }

    func testLizardOnRestoreIsTheDisableFrameWithTheValueSet() {
        // What both links write on stop, before they let the pad go: the same
        // ID_SET_SETTINGS_VALUES / SETTING_LIZARD_MODE report, u16 value 1. Without it the pad
        // stays in Steam-Input HID until the firmware watchdog fires and drives nothing
        // meanwhile.
        let write = Sc2Device.featurePayload(frame: Sc2Device.enableLizard)
        XCTAssertEqual(
            Array(write?.prefix(5) ?? []),
            [0x87, 0x03, 0x09, 0x01, 0x00]) // SET_SETTINGS len=3 {LIZARD_MODE, ON u16 LE}
        XCTAssertEqual(write?.count, 63) // the 64-byte frame minus the channel id, padding kept
        // One byte apart from the disable frame — the value's low half, nothing else.
        XCTAssertEqual(
            zip(Sc2Device.enableLizard, Sc2Device.disableLizard).filter { $0 != $1 }.count, 1)
    }

    // MARK: - Wire-constant pins against the regenerated C header (ABI v27)

    func testWireConstantsMatchTheCABIVerbatim() {
        // The up-path datagram: [0xCC][0x04][pad][len][data…].
        XCTAssertEqual(Int(PUNKTFUNK_RICH_INPUT_MAGIC), 0xCC)
        XCTAssertEqual(Int(PUNKTFUNK_RICH_HID_REPORT), 0x04)
        // The down-path plane tag ([0xCD]; the HidRaw wire kind byte 0x05 is core-internal —
        // the ABI surfaces it as the struct kind below, which is what Swift consumes).
        XCTAssertEqual(Int(PUNKTFUNK_HIDOUT_MAGIC), 0xCD)
        XCTAssertEqual(Int(PUNKTFUNK_HIDOUT_HID_RAW), 6)
        // The device-channel kinds Sc2BleLink branches on, and the report bound.
        XCTAssertEqual(Int(PUNKTFUNK_HID_RAW_OUTPUT), 0)
        XCTAssertEqual(Int(PUNKTFUNK_HID_RAW_FEATURE), 1)
        XCTAssertEqual(Int(PUNKTFUNK_HID_REPORT_MAX), 64)
        // The pad kind the capture declares in its arrival (GamepadPref::SteamController2).
        XCTAssertEqual(Int(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2), 9)
        XCTAssertEqual(PunktfunkConnection.GamepadType.steamController2.rawValue, 9)
        // The Puck dongle is its own host backend (native seven-interface topology, four
        // controller slots), declared by the macOS USB capture when it owns the physical dongle.
        XCTAssertEqual(Int(PUNKTFUNK_GAMEPAD_STEAMCONTROLLER2_PUCK), 10)
        XCTAssertEqual(PunktfunkConnection.GamepadType.steamController2Puck.rawValue, 10)
        // Both SC2 kinds carry motion (the IMU rides inside the opaque raw report), and both
        // parse from their names — the env/dev hook the host's `GamepadPref::from_name` mirrors.
        XCTAssertTrue(PunktfunkConnection.GamepadType.steamController2Puck.hasMotion)
        XCTAssertEqual(PunktfunkConnection.GamepadType(name: "ibexpuck"), .steamController2Puck)
        XCTAssertEqual(PunktfunkConnection.GamepadType(name: "sc2puck"), .steamController2Puck)
        XCTAssertEqual(PunktfunkConnection.GamepadType(name: "sc2"), .steamController2)
    }
}
