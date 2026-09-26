// Raw-HID DualSense rumble and mic-mute LED for macOS.
//
// Apple's GameController/CHHapticEngine path does NOT drive the DualSense's rumble motors on
// macOS — a documented platform gap: adaptive triggers, lightbar and player LEDs all work
// (different APIs), but `CHHapticEngine` output never reaches the motors. So we write the motor
// amplitudes straight into the DualSense HID output report, exactly the way SDL and the Linux
// `hid-playstation` driver do (the same report that already rumbles this pad on a Linux host).
//
// USB (report 0x02, 48 bytes, no CRC) and Bluetooth (report 0x31, 78 bytes, trailing CRC32) are
// both handled. The App Sandbox permits the raw-HID access via the app's `device.usb` +
// `device.bluetooth` entitlements, and this coexists with GameController holding the same device
// (non-seized open). Output-only, so no run-loop scheduling is needed.
//
// macOS-only: IOKit HID device access isn't available to apps on iOS/tvOS.

#if os(macOS)
import Foundation
import IOKit
import IOKit.hid
import os

private let log = ClientLog(category: "gamepad")

/// Opens one connected Sony DualSense and forwards motor rumble and the mic-mute LED to it over
/// raw HID.
///
/// A caller that owns a particular pad passes the location id it wants (see
/// `open(preferringLocationID:)`); the renderer takes that from the `GCController` it is bound to,
/// so with two DualSenses attached each renderer drives its own device. Without a preference the
/// lowest location id wins — an arbitrary but *stable* choice, where `Set.first` was neither.
final class DualSenseHID {
    private let manager: IOHIDManager
    private var device: IOHIDDevice?
    private var bluetooth = false
    private var closed = false
    /// This instance lit the mic LED, so `close` puts it out: the pad latches it.
    private var micLEDLit = false

    private static let vendorSony = 0x054C
    // DualSense (0x0CE6) and DualSense Edge (0x0DF2). The DualShock 4 uses a different report
    // layout and is intentionally not handled here.
    private static let productIDs = [0x0CE6, 0x0DF2]

    /// "USB" or "Bluetooth" — for logs / the debug panel. Valid after a successful `open()`.
    var transport: String { bluetooth ? "Bluetooth" : "USB" }

    init() {
        manager = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
    }

    deinit { close() }

    /// The IOKit location id of the device this instance opened — the handle a caller correlates
    /// with its `GCController`. `nil` until a successful `open`.
    private(set) var locationID: UInt32?

    /// A device's location id, or `nil` if IOKit does not report one.
    static func locationID(of dev: IOHIDDevice) -> UInt32? {
        IOHIDDeviceGetProperty(dev, kIOHIDLocationIDKey as CFString) as? UInt32
    }

    /// Every connected DualSense/Edge, by location id — what a caller pairs against its controllers.
    static func attachedLocationIDs() -> [UInt32] {
        let mgr = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
        let matches = productIDs.map { pid in
            [kIOHIDVendorIDKey: vendorSony, kIOHIDProductIDKey: pid] as CFDictionary
        }
        IOHIDManagerSetDeviceMatchingMultiple(mgr, matches as CFArray)
        guard IOHIDManagerOpen(mgr, IOOptionBits(kIOHIDOptionsTypeNone)) == kIOReturnSuccess else {
            return []
        }
        defer { IOHIDManagerClose(mgr, IOOptionBits(kIOHIDOptionsTypeNone)) }
        let devices = IOHIDManagerCopyDevices(mgr) as? Set<IOHIDDevice> ?? []
        return devices.compactMap(locationID(of:)).sorted()
    }

    /// Which attached device to drive, as an index into `ids` — the whole selection rule, pure so
    /// it can be tested without an `IOHIDDevice` (which cannot be constructed).
    ///
    /// `IOHIDManagerCopyDevices` returns an unordered `Set`, so the previous `Set.first` was not
    /// merely arbitrary — it can differ between two calls in one process. With two DualSenses that
    /// made each renderer's pad→device binding a coin flip: both could land on the same device
    /// (one pad's rumble coming out of the other, and the two per-instance write dedupes fighting
    /// over it) or split by luck. An explicit location id makes the binding deterministic; the
    /// lowest-id fallback at least makes it stable. `nil` ids sort last so a device IOKit cannot
    /// place never displaces one it can.
    static func preferredIndex(among ids: [UInt32?], preferring wanted: UInt32?) -> Int? {
        if let wanted, let hit = ids.firstIndex(where: { $0 == wanted }) { return hit }
        return ids.indices.min { (ids[$0] ?? .max) < (ids[$1] ?? .max) }
    }

    /// Pick the device to drive from everything attached (see [`preferredIndex`]).
    static func pick(_ devices: Set<IOHIDDevice>, preferring wanted: UInt32?) -> IOHIDDevice? {
        let ordered = Array(devices)
        guard let i = preferredIndex(among: ordered.map(locationID(of:)), preferring: wanted) else {
            return nil
        }
        return ordered[i]
    }

    /// Find and open a connected DualSense, preferring the one at `preferredLocationID`. Returns
    /// false if none is present or it can't be opened (caller then falls back to CoreHaptics).
    func open(preferringLocationID preferred: UInt32? = nil) -> Bool {
        let matches = Self.productIDs.map { pid in
            [kIOHIDVendorIDKey: Self.vendorSony, kIOHIDProductIDKey: pid] as CFDictionary
        }
        IOHIDManagerSetDeviceMatchingMultiple(manager, matches as CFArray)
        guard IOHIDManagerOpen(manager, IOOptionBits(kIOHIDOptionsTypeNone)) == kIOReturnSuccess else {
            log.info("rumble: DualSense HID manager open failed — falling back to CoreHaptics")
            return false
        }
        guard let devices = IOHIDManagerCopyDevices(manager) as? Set<IOHIDDevice>,
              let dev = Self.pick(devices, preferring: preferred)
        else {
            log.info("rumble: no DualSense HID device found — falling back to CoreHaptics")
            IOHIDManagerClose(manager, IOOptionBits(kIOHIDOptionsTypeNone))
            return false
        }
        device = dev
        locationID = Self.locationID(of: dev)
        if let preferred, locationID != preferred {
            // Not fatal — one pad still gets rumble — but with two pads attached it means this
            // renderer is driving the wrong one, and it is invisible without the log line.
            log.error(
                "rumble: wanted DualSense at location \(preferred, privacy: .public) but opened \(self.locationID.map(String.init) ?? "unknown", privacy: .public)"
            )
        }
        let transport = IOHIDDeviceGetProperty(dev, kIOHIDTransportKey as CFString) as? String
        bluetooth = transport?.lowercased().contains("bluetooth") ?? false
        log.info("rumble: DualSense raw-HID rumble active (transport=\(self.transport, privacy: .public))")
        return true
    }

    /// Drive the motors. `low` = left/heavy (low-frequency), `high` = right/light (high-frequency),
    /// each 0...255. (0, 0) stops.
    ///
    /// Returns whether the write reached the device. The caller needs this: it used to be logged
    /// and swallowed, so a failed write still counted as a successful render. That matters most
    /// for a **stop**, which has nothing behind it — the renderer stamps its write clock even on
    /// failure, the keepalive re-write only fires for non-zero levels, and the ticker is cancelled
    /// once the target is `(0, 0)`. On USB there is no firmware timeout either, so a swallowed
    /// stop left the motors running with nothing scheduled to try again.
    @discardableResult
    func rumble(low: UInt8, high: UInt8) -> Bool {
        send(bluetooth
            ? Self.bluetoothReport(low: low, high: high)
            : Self.usbReport(low: low, high: high))
    }

    /// Set the mic-mute LED: `mode` 0 off, 1 on, 2 pulse. GameController has no API for it.
    @discardableResult
    func micLED(mode: UInt8) -> Bool {
        let sent = send(bluetooth ? Self.bluetoothMicReport(mode: mode) : Self.usbMicReport(mode: mode))
        if sent { micLEDLit = mode != 0 }
        return sent
    }

    private func send(_ report: [UInt8]) -> Bool {
        guard let dev = device else { return false }
        let rc = report.withUnsafeBufferPointer { buf in
            IOHIDDeviceSetReport(
                dev, kIOHIDReportTypeOutput, CFIndex(report[0]), buf.baseAddress!, buf.count)
        }
        if rc != kIOReturnSuccess {
            log.error("DualSense: IOHIDDeviceSetReport failed (0x\(String(format: "%08x", rc), privacy: .public))")
            return false
        }
        return true
    }

    func close() {
        guard !closed else { return }
        closed = true
        if device != nil {
            rumble(low: 0, high: 0) // silence the motors before releasing
            if micLEDLit { micLED(mode: 0) }
        }
        device = nil
        IOHIDManagerClose(manager, IOOptionBits(kIOHIDOptionsTypeNone))
    }

    // MARK: - Report builders

    // DualSense effects payload (DS5EffectsState_t / hid-playstation `common`) — offsets relative
    // to the payload start:
    //   0  flag0 (enable bits)   2  motor_right (high-freq)   3  motor_left (low-freq)
    //   1  flag1                 38 flag2 (enhanced enable)
    // We mirror the Linux driver: flag0 = COMPATIBLE_VIBRATION | HAPTICS_SELECT, flag2 =
    // COMPATIBLE_VIBRATION2 (the enhanced-firmware path), motors sent directly. valid_flag1 stays
    // 0 so this rumble-only report leaves the lightbar / triggers / player LEDs (driven by
    // GameController) untouched.
    private static func fillEffects(_ data: inout [UInt8], at base: Int, low: UInt8, high: UInt8) {
        data[base + 0] = 0x03 // COMPATIBLE_VIBRATION (0x01) | HAPTICS_SELECT (0x02)
        data[base + 2] = high // motor_right
        data[base + 3] = low // motor_left
        data[base + 38] = 0x04 // COMPATIBLE_VIBRATION2 (enhanced rumble, firmware ≥ 0x0224)
    }

    // `usbReport` / `bluetoothReport` / `crc32` are internal (not private) so the unit tests can
    // pin the exact wire layout against the SDL / hid-playstation spec without a physical pad.
    static func usbReport(low: UInt8, high: UInt8) -> [UInt8] {
        usbFrame { fillEffects(&$0, at: $1, low: low, high: high) }
    }

    static func bluetoothReport(low: UInt8, high: UInt8) -> [UInt8] {
        bluetoothFrame { fillEffects(&$0, at: $1, low: low, high: high) }
    }

    /// flag1 bit 0 (mic-mute LED enable) and `mute_button_led` at payload offset 8.
    private static func fillMicLED(_ data: inout [UInt8], at base: Int, mode: UInt8) {
        data[base + 1] = 0x01
        data[base + 8] = mode
    }

    static func usbMicReport(mode: UInt8) -> [UInt8] {
        usbFrame { fillMicLED(&$0, at: $1, mode: mode) }
    }

    static func bluetoothMicReport(mode: UInt8) -> [UInt8] {
        bluetoothFrame { fillMicLED(&$0, at: $1, mode: mode) }
    }

    /// USB report 0x02: the payload starts at byte 1.
    private static func usbFrame(_ fill: (inout [UInt8], Int) -> Void) -> [UInt8] {
        var d = [UInt8](repeating: 0, count: 48)
        d[0] = 0x02 // report id
        fill(&d, 1)
        return d
    }

    /// Bluetooth report 0x31: the payload starts at byte 3, CRC32 in the last four.
    private static func bluetoothFrame(_ fill: (inout [UInt8], Int) -> Void) -> [UInt8] {
        var d = [UInt8](repeating: 0, count: 78)
        d[0] = 0x31 // report id
        d[1] = 0x00 // seq/tag (static, as SDL)
        d[2] = 0x10 // magic
        fill(&d, 3)
        // Trailing CRC32 over a 0xA2 seed byte + the report minus its 4 CRC bytes, little-endian.
        let crc = Self.crc32(seed: 0xA2, d[0..<(d.count - 4)])
        d[74] = UInt8(crc & 0xFF)
        d[75] = UInt8((crc >> 8) & 0xFF)
        d[76] = UInt8((crc >> 16) & 0xFF)
        d[77] = UInt8((crc >> 24) & 0xFF)
        return d
    }

    /// Standard reflected CRC32 (zlib poly 0xEDB88320, init 0xFFFFFFFF, final XOR) over `seed`
    /// followed by `bytes` — the DualSense Bluetooth output-report checksum (seed 0xA2). Matches
    /// SDL's `SDL_crc32`/the kernel's `crc32_le` framing.
    static func crc32<S: Sequence>(seed: UInt8, _ bytes: S) -> UInt32
    where S.Element == UInt8 {
        var crc: UInt32 = 0xFFFF_FFFF
        func step(_ b: UInt8) {
            crc ^= UInt32(b)
            for _ in 0..<8 {
                crc = (crc & 1) != 0 ? (crc >> 1) ^ 0xEDB8_8320 : crc >> 1
            }
        }
        step(seed)
        for b in bytes { step(b) }
        return ~crc
    }
}
#endif
