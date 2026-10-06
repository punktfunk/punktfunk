// Steam Controller 2 (2026, Valve "Ibex" / SDL "Triton", wired 28DE:1302) protocol constants +
// every piece of pure SC2 logic the client needs — framing, the per-report characteristic map,
// the feature commands, and the light state parser. Cross-client parity: this file is the Apple
// sibling of Android's `Sc2Device.kt`, and the transport facts are the ones proven against
// real hardware on the bench (on-glass 2026-06-08/09: live input end-to-end
// after the report-id prepend fix, gyro-enable forwarded over GATT, and full multi-actuator
// haptics via the per-report characteristic map).
//
// The GATT service is Valve's CUSTOM vendor service, NOT standard HID-over-GATT (0x1812) —
// which is exactly why a third-party app may read it raw: iOS keeps its own 0x1812 binding (the
// lizard-mode keyboard/mouse) while we open a second handle to the vendor service, the same
// coexistence Steam Link relies on. The full report rides the punktfunk wire verbatim
// (`PunktfunkConnection.sendHidReport` → the host's as-is virtual 28DE:1302 pad); the parser
// here extracts only what the client itself consumes — the button word for the typed mirror +
// exit chord, and sticks/triggers for the degrade path.
//
// Everything in this file is device-free by design (no CoreBluetooth import), so the tables and
// framing rules are pinned by unit tests that run on any Mac — `Sc2DeviceTests` /
// `Sc2FramingTests` — the same convention as `DualSenseHID`'s report builders.

import Foundation

enum Sc2Device {
    // MARK: - USB identity (macOS `Sc2UsbLink`; cf. Android's `Sc2Device.kt`)

    static let vidValve = 0x28DE
    /// Wired controller.
    static let pidWired = 0x1302
    /// Direct BLE identity — that transport is `Sc2BleLink`'s, never USB's.
    static let pidBLE = 0x1303
    /// The wireless Puck dongles (Proteus / Nereid).
    static let pidDongleProteus = 0x1304
    static let pidDongleNereid = 0x1305
    /// Everything `Sc2UsbLink` matches. `pidBLE` is deliberately absent.
    static let usbPIDs = [pidWired, pidDongleProteus, pidDongleNereid]

    /// Whether a matched USB product id is a Puck dongle rather than a directly-attached pad.
    /// Decides the declared wire kind (`.steamController2Puck` vs `.steamController2`) AND
    /// whether wireless-status reports are authoritative — see `Sc2Capture`.
    static func isDongle(pid: Int) -> Bool {
        pid == pidDongleProteus || pid == pidDongleNereid
    }

    /// The HID usage pair of the SC2's CONTROLLER collection (vendor-defined page 0xFF00,
    /// usage 0x01) — declared in `pf_driver_proto::triton::RDESC` beside the lizard-mode mouse
    /// (Generic Desktop 01:02, report 0x40) and keyboard (01:06, report 0x41).
    ///
    /// ⚠ Load-bearing as a DEVICE-usage-pair match (`kIOHIDDeviceUsagePageKey`), never a
    /// primary-usage one: on real hardware (Puck, macOS 27, 2026-08-31) each controller
    /// interface is ONE `IOHIDDevice` carrying all its collections with primary usage
    /// `0001:0002`, so a primary match finds nothing. The pair match selects the four controller
    /// slots and excludes the Puck's management interface, whose only pair is `ff00:02` —
    /// opening that one is actively wrong (feature-report-2 queries; see
    /// `pf_driver_proto::triton`).
    static let usagePageVendor = 0xFF00
    static let usageController = 0x01

    /// The Puck hosts up to four pads on USB interfaces 2…5, one HID interface each, and there
    /// is no way to know in advance which slot a controller bonded to. The link therefore opens
    /// EVERY matched collection and lets whichever one streams state become the write target —
    /// Android learned this on glass (claiming only interface 2 read silence). Kept as
    /// documentation of the dongle's layout; the usage-pair match already excludes its CDC pair
    /// on interfaces 0/1, which is not HID at all.
    static let dongleIfaces = 2...5

    // MARK: - GATT topology (Valve vendor service; cf. Android's `Sc2BleLink.kt`)

    /// The custom Valve service every SC2 exposes over BLE.
    static let serviceUUID = "100F6C32-1735-4313-B402-38567131E5F3"
    /// Input characteristic (notify): report 0x45, a bare 45-byte state payload — the
    /// characteristic VALUE carries NO report-id byte (the id is implied by the UUID), which is
    /// the framing root cause debugged on-glass 2026-06-09: without re-prepending 0x45 an
    /// id-keyed consumer drops ~every frame. See `frameIncoming`.
    static let inputCharUUID = "100F6C7A-1735-4313-B402-38567131E5F3"
    /// Timestamp characteristic (notify): report 0x47. Deliberately NOT subscribed — the bench
    /// stack subscribed it and then ignored every 0x47, Android never subscribes it, and the gyro is
    /// hardware-proven NOT to ride it (the IMU streams inside the SAME 0x45 report once enabled,
    /// bench 2026-06-08). The constant stays for the connect-time characteristic census log.
    static let timestampCharUUID = "100F6C7C-1735-4313-B402-38567131E5F3"
    /// Report/feature characteristic (write/read; notify on some firmware): where FEATURE
    /// commands land — the hardware-proven gyro-enable/lizard-off path. Props 0x0a on-device.
    static let reportCharUUID = "100F6C34-1735-4313-B402-38567131E5F3"

    /// Per-OUTPUT-report characteristic: Valve routes each output report id 0xNN to its OWN
    /// characteristic `100F6C<NN+0x35>` (the id only SELECTS the characteristic and is stripped
    /// from the written payload). Lowercase, the form CoreBluetooth's `CBUUID.uuidString` is
    /// compared against case-insensitively. Verified per-actuator on-device 2026-06-09 — the
    /// 0x82 test buzz failed while mis-routed to the 0x80 characteristic.
    static func outputCharUUID(id: UInt8) -> String {
        String(format: "100f6c%02x-1735-4313-b402-38567131e5f3", (Int(id) + 0x35) & 0xFF)
    }

    /// Declared STRIPPED payload length per output report id (wire length = stripped + 1). The
    /// hardware-verified table; its id-INCLUDED twin is the host's
    /// `pf_driver_proto::triton::out_report_len`, and `clients/shared/sc2-vectors.json` holds
    /// both (and Android's) to the same rows. `nil` = unknown id: clamp to what arrived minus
    /// the id byte, never guess-trim beyond that.
    static func strippedOutputLen(id: UInt8) -> Int? {
        switch id {
        case 0x80: return 9 // grip rumble    → 100F6CB5 (left/right motor fields)
        case 0x81: return 7 // trackpad pulse → 100F6CB6 (side byte 01=L 02=R 03=both; one char)
        case 0x82: return 3 // haptic command → 100F6CB7 (Steam's ping/test buzz)
        case 0x83: return 9 // LFO tone       → 100F6CB8
        case 0x84: return 8 // log sweep      → 100F6CB9
        case 0x85: return 3 // script         → 100F6CBA
        case 0x86: return 3 // vendor         → 100F6CBB
        case 0x87, 0x88, 0x89: return 63 // vendor big → 100F6CBC/BD/BE
        default: return nil
        }
    }

    // MARK: - Input report ids (`ETritonReportIDTypes`)

    static let idState: UInt8 = 0x42
    static let idBattery: UInt8 = 0x43
    /// The WIRELESS state shape, not a BLE-only one: the Puck dongle delivers `0x45` (46 B,
    /// id-first) over USB too — measured on-glass 2026-08-31. Only a cabled pad emits `0x42`.
    static let idStateBLE: UInt8 = 0x45
    static let idWirelessX: UInt8 = 0x46
    static let idStateTimestamp: UInt8 = 0x47
    static let idWireless: UInt8 = 0x79

    // MARK: - Feature commands (Sc2Device.kt; hardware-confirmed 2026-06-08)

    /// The feature report that turns lizard mode (built-in keyboard/mouse emulation) off:
    /// `[report id 1][ID_SET_SETTINGS_VALUES 0x87][length 3][SETTING_LIZARD_MODE 9]
    /// [LIZARD_MODE_OFF u16]`, zero-padded to the 64-byte feature size (the firmware accepts the
    /// padded form — it is exactly what a Windows host's hidclass sends). The firmware watchdog
    /// re-enables lizard mode after a few seconds of silence, so this is re-sent every
    /// `lizardRefreshSeconds` (SDL's cadence) — and the host's Steam sends its own through the
    /// raw plane once it grabs the virtual pad, which lands on the same characteristic.
    static let disableLizard: [UInt8] = {
        var b = [UInt8](repeating: 0, count: 64)
        b[0] = 0x01 // feature report id
        b[1] = 0x87 // ID_SET_SETTINGS_VALUES
        b[2] = 3 // one ControllerSetting {u8 num, u16 value}
        b[3] = 9 // SETTING_LIZARD_MODE
        // [4..6] = LIZARD_MODE_OFF (0) — already zero
        return b
    }()

    /// The same settings write with `LIZARD_MODE` back ON — what both links send on stop, before
    /// they let the pad go. Without it the controller stays in Steam-Input HID until the firmware
    /// watchdog times out, so for those seconds it drives nothing: no keyboard, no mouse, no
    /// focus. The keep-alive is cancelled first, or its next disable would land after this.
    static let enableLizard: [UInt8] = {
        var b = disableLizard
        b[4] = 1 // LIZARD_MODE_ON, low half of the u16
        return b
    }()

    /// Force firmware-calibrated signed i16 stick coordinates (`SETTING_ENABLE_RAW_JOYSTICK`
    /// 0x2e, value 0) — Steam sends this during physical-controller initialization. Without it a
    /// controller previously opened in raw mode reports ADC coordinates around 0…3200, which a
    /// Triton consumer reads as a few percent of full travel: the sticks barely move.
    ///
    /// USB only, deliberately. The BLE link never sent it and is hardware-proven without it; the
    /// raw-mode state it corrects is left behind by a USB host that opened the pad, so it
    /// belongs with the transport that can inherit that state. Same 64-byte zero-padded framing
    /// as `disableLizard`, and re-sent on the same keep-alive tick.
    static let normalizeJoysticks: [UInt8] = {
        var b = [UInt8](repeating: 0, count: 64)
        b[0] = 0x01 // feature report id
        b[1] = 0x87 // ID_SET_SETTINGS_VALUES
        b[2] = 3 // one ControllerSetting {u8 num, u16 value}
        b[3] = 0x2E // SETTING_ENABLE_RAW_JOYSTICK
        // [4..6] = disabled (0) — firmware emits calibrated signed i16 values
        return b
    }()

    // MARK: - Wireless status payload (`idWireless` / `idWirelessX` byte 1)

    /// The Puck reports the controller powered off / out of range.
    static let wirelessDisconnect: UInt8 = 1
    /// The Puck reports a controller bonded to one of its slots.
    static let wirelessConnect: UInt8 = 2

    // MARK: - Engraved serial (feature report 2 on a controller slot; macOS `Sc2UsbLink` only)

    /// The feature report whose plain GET a slot node answers with the pad's ENGRAVED serial —
    /// no SET first, unlike the `0xAE` GET_STRING_ATTRIBUTE dance every other string query
    /// rides on feature report 1 (`steam_proto::ID_GET_STRING_ATTRIBUTE`). The reply is a
    /// small binary header followed by the serial as printable ASCII (13 chars, `FXA…` —
    /// `triton_proto.rs`), the only per-unit identity a Puck exposes: the dongle's own USB
    /// serial is shared by every slot. Feature report 2 is declared by the controller
    /// descriptor itself, for the Puck's connection/bond queries (`triton_proto.rs`); this GET
    /// is one of them, proven on real hardware by the splitscreen project's bench. Read once
    /// per live pad, never per report (the blocking-GET rule `HidUsbLink.kt` pins).
    static let featureSerial: UInt8 = 0x02

    /// Pull the serial out of a feature-2 reply: the longest ASCII-alphanumeric run, accepted
    /// only at 8–20 characters (the header is binary; the engraved serial is the one printable
    /// token that length; ties keep the first). Nil otherwise — a reply this cannot read
    /// degrades to no-serial, never to a garbage identity, which is why an over-long run (an
    /// all-ASCII error string, say) is rejected whole rather than truncated into one.
    static func parseSerial(_ reply: [UInt8]) -> String? {
        var best: ArraySlice<UInt8> = []
        var runStart: Int?
        for (i, b) in reply.enumerated() {
            let alnum = (0x30 ... 0x39).contains(b) || (0x41 ... 0x5A).contains(b)
                || (0x61 ... 0x7A).contains(b)
            if alnum {
                if runStart == nil { runStart = i }
            } else if let s = runStart {
                if i - s > best.count { best = reply[s ..< i] }
                runStart = nil
            }
        }
        if let s = runStart, reply.count - s > best.count { best = reply[s...] }
        guard (8 ... 20).contains(best.count) else { return nil }
        return String(decoding: best, as: UTF8.self)
    }

    /// The frame `Sc2Capture` replays onto a fresh wire slot for a wireless edge the Puck
    /// emitted before that slot existed — always id `0x79`, whichever of `0x79`/`0x46` arrived.
    /// The virtual identity's report descriptor declares `0x79` but not `0x46`
    /// (`pf_driver_proto::triton::RDESC`), and the Windows driver drops undeclared input ids,
    /// so a `0x46`-shaped replay would silently vanish on one host and land on the other.
    static func wirelessReplay(_ framed: [UInt8]) -> [UInt8] {
        [idWireless, framed.count >= 2 ? framed[1] : 0]
    }

    /// The gyro-enable Steam itself sends — WRITE_REGISTER, reg 0x30 (GYRO_MODE), value 0x0018
    /// (raw accel | raw gyro); confirmed both ways on real hardware 2026-06-08. Kept ONLY for
    /// logging and tests: the client must NEVER self-enable the gyro (a permanent enable re-flies
    /// the desktop cursor) — Steam's own forwarded write is what opens `Sc2ImuGate`.
    static let gyroEnableReference: [UInt8] = [0x01, 0x87, 0x03, 0x30, 0x18, 0x00]

    /// SDL's lizard-off refresh cadence.
    static let lizardRefreshSeconds: TimeInterval = 3.0

    // MARK: - Button bits in the state report's u32 (SDL `TritonButtons`)
    // SDL's enum swaps the View and Menu names; its mapping and hid-steam agree with these values.

    static let btnA: UInt32 = 0x0000_0001
    static let btnB: UInt32 = 0x0000_0002
    static let btnX: UInt32 = 0x0000_0004
    static let btnY: UInt32 = 0x0000_0008
    static let btnQAM: UInt32 = 0x0000_0010
    static let btnR3: UInt32 = 0x0000_0020
    static let btnMenu: UInt32 = 0x0000_0040
    static let btnR4: UInt32 = 0x0000_0080
    static let btnR5: UInt32 = 0x0000_0100
    static let btnRB: UInt32 = 0x0000_0200
    static let btnDpadDown: UInt32 = 0x0000_0400
    static let btnDpadRight: UInt32 = 0x0000_0800
    static let btnDpadLeft: UInt32 = 0x0000_1000
    static let btnDpadUp: UInt32 = 0x0000_2000
    static let btnView: UInt32 = 0x0000_4000
    static let btnL3: UInt32 = 0x0000_8000
    static let btnSteam: UInt32 = 0x0001_0000
    static let btnL4: UInt32 = 0x0002_0000
    static let btnL5: UInt32 = 0x0004_0000
    static let btnLB: UInt32 = 0x0008_0000
    static let btnRPadClick: UInt32 = 0x0040_0000

    /// Wire mapping: SC2 button bit → punktfunk `GamepadWire` bit, the inverse of the host's
    /// typed-fallback mapping (`triton_proto::from_gamepad`): paddles R4/L4/R5/L5 =
    /// PADDLE1/2/3/4, QAM = MISC1, right-pad click = the touchpad wire bit. Same pairs, same
    /// order, as Android's `Sc2Device.WIRE_MAP`.
    static let wireMap: [(sc2: UInt32, wire: UInt32)] = [
        (btnA, GamepadWire.a),
        (btnB, GamepadWire.b),
        (btnX, GamepadWire.x),
        (btnY, GamepadWire.y),
        (btnLB, GamepadWire.leftShoulder),
        (btnRB, GamepadWire.rightShoulder),
        (btnView, GamepadWire.back),
        (btnMenu, GamepadWire.start),
        (btnSteam, GamepadWire.guide),
        (btnL3, GamepadWire.leftStickClick),
        (btnR3, GamepadWire.rightStickClick),
        (btnDpadUp, GamepadWire.dpadUp),
        (btnDpadDown, GamepadWire.dpadDown),
        (btnDpadLeft, GamepadWire.dpadLeft),
        (btnDpadRight, GamepadWire.dpadRight),
        (btnQAM, GamepadWire.misc1),
        (btnR4, GamepadWire.paddle1),
        (btnL4, GamepadWire.paddle2),
        (btnR5, GamepadWire.paddle3),
        (btnL5, GamepadWire.paddle4),
        (btnRPadClick, GamepadWire.touchpadClick),
    ]

    /// Translate an SC2 button word into the wire `GamepadWire` bitmask.
    static func wireButtons(_ sc2: UInt32) -> UInt32 {
        var out: UInt32 = 0
        for (bit, wire) in wireMap where sc2 & bit != 0 {
            out |= wire
        }
        return out
    }

    // MARK: - State parser (typed mirror + exit chord only; the raw report is the product)

    /// The typed-mirror fields of one state report (buttons/sticks/triggers only).
    struct State: Equatable {
        var buttons: UInt32 = 0 // SC2 bit layout
        var lsX: Int32 = 0 // i16, +y = up (device convention = wire convention)
        var lsY: Int32 = 0
        var rsX: Int32 = 0
        var rsY: Int32 = 0
        var lt: Int32 = 0 // 0...255 (device 0...32767 scaled down)
        var rt: Int32 = 0
    }

    /// Parse the client-consumed fields out of a state report (`0x42`/`0x45`/`0x47` — identical
    /// offsets for everything read here) into `out`. Returns false for non-state/short reports.
    /// Offsets are id-first wire offsets: buttons u32 @2, triggers i16 @6/@8 (`>>7` → 0...255),
    /// sticks i16 @10/@12/@14/@16 — Android's `parseState`, byte for byte.
    static func parseState(_ report: [UInt8], into out: inout State) -> Bool {
        guard report.count >= 18 else { return false }
        switch report[0] {
        case idState, idStateBLE, idStateTimestamp: break
        default: return false
        }
        func i16(_ o: Int) -> Int32 {
            Int32(Int16(bitPattern: UInt16(report[o]) | (UInt16(report[o + 1]) << 8)))
        }
        out.buttons = UInt32(report[2]) | (UInt32(report[3]) << 8)
            | (UInt32(report[4]) << 16) | (UInt32(report[5]) << 24)
        out.lt = min(max(i16(6), 0), 32767) >> 7
        out.rt = min(max(i16(8), 0), 32767) >> 7
        out.lsX = i16(10)
        out.lsY = i16(12)
        out.rsX = i16(14)
        out.rsY = i16(16)
        return true
    }

    /// The SC2 bits behind a wire mask — `wireButtons` read backwards, so a chord expressed in
    /// `GamepadWire` terms can be cleared from a raw report and a parsed state, which both speak
    /// the device's own layout.
    static func sc2Buttons(forWire wire: UInt32) -> UInt32 {
        var out: UInt32 = 0
        for (sc2, w) in wireMap where wire & w != 0 {
            out |= sc2
        }
        return out
    }

    /// Clear input the host must not see from a state report, in place: the buttons in `clear`
    /// (device layout), plus every stick and trigger when `zeroAxes`. What the quick-action ring
    /// forwards while it owns the pad — a well-formed report at the same cadence, so the host's
    /// virtual pad stays live instead of freezing on whatever was held when the dial opened.
    /// Same id-first offsets `parseState` reads, and identical in all three state shapes.
    static func maskInputs(_ report: inout [UInt8], clear: UInt32, zeroAxes: Bool) {
        guard report.count >= 18 else { return }
        switch report[0] {
        case idState, idStateBLE, idStateTimestamp: break
        default: return
        }
        if clear != 0 {
            let held = (UInt32(report[2]) | (UInt32(report[3]) << 8)
                | (UInt32(report[4]) << 16) | (UInt32(report[5]) << 24)) & ~clear
            report[2] = UInt8(held & 0xFF)
            report[3] = UInt8((held >> 8) & 0xFF)
            report[4] = UInt8((held >> 16) & 0xFF)
            report[5] = UInt8((held >> 24) & 0xFF)
        }
        if zeroAxes {
            for i in 6 ..< 18 {
                report[i] = 0
            }
        }
    }

    // MARK: - Framing (pure; the BLE shim calls these — see Sc2FramingTests)

    /// Incoming (up-path): a GATT characteristic VALUE is the raw payload with NO HID report-id
    /// byte, so re-prepend `0x45` for state-sized (≥ 40 B) payloads — the wire then carries the
    /// same id-first framing as USB, which is punktfunk's contract (the host's virtual pad does
    /// the rest; no 0x45→0x42 rewrite and no 54-byte zero-pad — those belong to a
    /// synthetic-USB queue contract, not ours). Short payloads (battery/status) pass through
    /// unmodified. Observed live rate ~66 Hz, len 45.
    static func frameIncoming(_ payload: [UInt8]) -> [UInt8] {
        guard payload.count >= 40 else { return payload }
        return [idStateBLE] + payload
    }

    /// One resolved OUTPUT write: which per-report characteristic, and the bare payload to put
    /// on it.
    struct OutputWrite: Equatable {
        /// Lowercase characteristic UUID (`outputCharUUID`).
        let charUUID: String
        let payload: [UInt8]
    }

    /// Outgoing OUTPUT (`kind == 0`, HID_RAW_OUTPUT): the frame arrives id-first
    /// `[0xNN][payload…]`; the id SELECTS the per-report characteristic and is STRIPPED, and the
    /// payload is trimmed to the declared stripped length, clamped to what arrived. The clamp is
    /// redundant-but-kept: current Windows hosts already trim each drained OUTPUT frame to
    /// `out_report_len(id)` before the HidRaw push, but older hosts pad to 64 B — and the GATT
    /// write must carry exactly the declared length either way. Unknown id: the whole id-stripped
    /// payload (never guess-trim). `nil` for a frame too short to carry a payload.
    static func outputWrite(frame: [UInt8]) -> OutputWrite? {
        guard frame.count >= 2 else { return nil }
        let id = frame[0]
        let declared = strippedOutputLen(id: id) ?? frame.count - 1
        let n = min(declared, frame.count - 1)
        return OutputWrite(charUUID: outputCharUUID(id: id), payload: Array(frame[1 ..< 1 + n]))
    }

    /// Outgoing FEATURE (`kind == 1`, HID_RAW_FEATURE): the frame is `[0x01][0x87 …]` — strip
    /// the leading 0x01 channel report-id and write the remainder to `100F6C34` (the
    /// hardware-proven gyro/lizard path). FEATURE frames deliberately arrive WHOLE from the host
    /// (64 B, un-trimmed), so trailing zero-padding is passed through — the firmware accepts the
    /// zero-padded form (Android sends `disableLizard` padded to 64 B the same way). NO 0xC0
    /// segment wrapper: proven unnecessary on-device for both feature and output writes.
    /// `nil` for a frame too short to carry a command.
    static func featurePayload(frame: [UInt8]) -> [UInt8]? {
        guard frame.count >= 2 else { return nil }
        return Array(frame.dropFirst())
    }
}
