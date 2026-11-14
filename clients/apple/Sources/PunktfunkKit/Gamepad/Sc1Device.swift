// The 2015 Steam Controller (SDL "D0G") as `Sc2UsbLink` (Mac) and `Sc2BleLink` capture it:
// frames carry no report id, just a version word of 1 and the type. The parser feeds only the
// typed mirror and local chords, in the Steam Controller 2's layout so `Sc2Capture` serves both
// pads. Android's `Sc1Device.kt` is its sibling; `clients/shared/sc1-vectors.json` pins both.

import Foundation

enum Sc1Device {
    /// The wired pad; its controller is interface 2, beside a lizard keyboard and mouse.
    static let pidWired = 0x1102
    /// The wireless dongle: one pad per interface 1…4.
    static let pidDongle = 0x1142
    static let usbPIDs = [pidWired, pidDongle]

    /// The Bluetooth input characteristic: 19-byte segments, `[header, 18 bytes]`, which the
    /// HID report id `0x03` turns into the segments core decodes.
    static let bleInputCharUUID = "100F6C33-1735-4313-B402-38567131E5F3"
    static let bleReport: UInt8 = 0x03

    static let typeState: UInt8 = 0x01
    static let typeWireless: UInt8 = 0x03
    /// Wireless event byte: the pad behind a dongle slot powered off.
    static let wirelessDisconnect: UInt8 = 1

    private static let lpadClick: UInt32 = 0x02_0000
    private static let lpadTouch: UInt32 = 0x08_0000
    private static let rpadTouch: UInt32 = 0x10_0000
    /// Left pad and stick share 16..20; with this set the pad holds them and the stick last reported.
    private static let lpadAndStick: UInt32 = 0x80_0000

    /// 2015 bit → Steam Controller 2 bit. Grips land on L4/R4, the right-pad click on its own.
    private static let toSc2: [(sc1: UInt32, sc2: UInt32)] = [
        (0x00_0004, Sc2Device.btnRB),
        (0x00_0008, Sc2Device.btnLB),
        (0x00_0010, Sc2Device.btnY),
        (0x00_0020, Sc2Device.btnB),
        (0x00_0040, Sc2Device.btnX),
        (0x00_0080, Sc2Device.btnA),
        (0x00_0100, Sc2Device.btnDpadUp),
        (0x00_0200, Sc2Device.btnDpadRight),
        (0x00_0400, Sc2Device.btnDpadLeft),
        (0x00_0800, Sc2Device.btnDpadDown),
        (0x00_1000, Sc2Device.btnView),
        (0x00_2000, Sc2Device.btnSteam),
        (0x00_4000, Sc2Device.btnMenu),
        (0x00_8000, Sc2Device.btnL4),
        (0x01_0000, Sc2Device.btnR4),
        (0x04_0000, Sc2Device.btnRPadClick),
        (0x40_0000, Sc2Device.btnL3),
    ]

    /// The 2015 button word in the Steam Controller 2's layout.
    static func sc2Buttons(_ word: UInt32) -> UInt32 {
        var out: UInt32 = 0
        for (bit, sc2) in toSc2 where word & bit != 0 { out |= sc2 }
        // Older firmware reports a stick click as a left-pad click with no finger on the pad.
        if word & lpadClick != 0, word & lpadTouch == 0 { out |= Sc2Device.btnL3 }
        return out
    }

    /// Whether `r` is a frame of `type`: version word 1, then the type.
    static func isFrame(_ r: [UInt8], type: UInt8) -> Bool {
        r.count >= 3 && r[0] == 1 && r[1] == 0 && r[2] == type
    }

    static func isWirelessDisconnect(_ r: [UInt8]) -> Bool {
        r.count >= 5 && isFrame(r, type: typeWireless) && r[4] == wirelessDisconnect
    }

    /// The typed-mirror fields of a state report into `out`; false for any other frame. A touched
    /// left pad leaves the stick where it last was when the pad interleaves both, else centred;
    /// the right pad is the right stick while touched.
    static func parseState(_ r: [UInt8], into out: inout Sc2Device.State) -> Bool {
        guard r.count >= 24, isFrame(r, type: typeState) else { return false }
        func i16(_ o: Int) -> Int32 { Int32(Int16(bitPattern: UInt16(r[o]) | UInt16(r[o + 1]) << 8)) }
        let word = UInt32(r[8]) | UInt32(r[9]) << 8 | UInt32(r[10]) << 16
        out.buttons = sc2Buttons(word)
        if word & lpadTouch == 0 {
            out.lsX = i16(16)
            out.lsY = i16(18)
        } else if word & lpadAndStick == 0 {
            out.lsX = 0
            out.lsY = 0
        }
        let rpad = word & rpadTouch != 0
        out.rsX = rpad ? i16(20) : 0
        out.rsY = rpad ? i16(22) : 0
        out.lt = trigger(r[11])
        out.rt = trigger(r[12])
        return true
    }

    /// SDL's scale: `(v << 7) | v` reaches full at 26000, so a pull of 202 is full.
    private static func trigger(_ v: UInt8) -> Int32 { min(Int32(v) * 129 * 255 / 26000, 255) }

    /// A feature frame as the pad takes it: report id 0, then `body`, zero-padded to 65 bytes.
    private static func frame(_ body: [UInt8]) -> [UInt8] {
        [0] + body + [UInt8](repeating: 0, count: 64 - body.count)
    }

    /// Lizard mode off, as SDL's driver sends it at open: digital mappings cleared, settings
    /// reset, then packet version 2 and both trackpads emulating nothing. The pad has no
    /// watchdog, so once per pad.
    static let lizardOff: [[UInt8]] = [
        frame([0x81]),
        frame([0x8E, 0]),
        frame([0x87, 12, 0x31, 2, 0, 7, 7, 0, 8, 7, 0, 24, 0, 0]),
    ]

    /// Lizard mode back on as the link lets go, as SDL's driver closes: the Mac gets its mouse.
    static let lizardOn: [[UInt8]] = [
        frame([0x85]),
        frame([0x8E, 0]),
        frame([0x87, 3, 8, 0, 0]),
    ]
}
