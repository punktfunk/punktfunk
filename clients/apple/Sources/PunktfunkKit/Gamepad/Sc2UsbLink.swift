// IOKit HID transport for a Steam Controller 2 over USB: wired (`28DE:1302`) or through the
// Puck dongle (`1304`/`1305`). Same surface as `Sc2BleLink` (`start` / `stop` / `writeRaw`), so
// `Sc2Capture` holds either. macOS only: iOS and tvOS apps get no IOKit HID device access.
//
// The match is the DEVICE usage pair FF00:01 (`Sc2Device.usagePageVendor`/`usageController`).
// Each controller interface is one `IOHIDDevice` with all its collections (the lizard mouse and
// keyboard too, so opening can need Input Monitoring); the pair selects the four Puck slots and
// never the management interface (FF00:02), which must stay closed.
//
// Every matched collection opens as its own source, keyed by IOKit registry entry id: a Puck does
// not say which slot a pad bonded to, and host writes must reach the pad that claimed them.
//
// The firmware re-enables lizard mode after a few seconds of silence, so `disableLizard` and
// `normalizeJoysticks` repeat every ~3 s. The client never enables the gyro itself; Steam's
// forwarded write does.

#if os(macOS)

import Foundation
import IOKit
import IOKit.hid

private let log = ClientLog(category: "gamepad")

final class Sc2UsbLink {
    /// Per-report diagnostics. Lifecycle milestones always log; flip this only to debug the seam.
    private static let verbose = false

    /// The queue every IOKit callback and every mutation below runs on — owned by `Sc2Capture`,
    /// USER_INTERACTIVE for the same reason the BLE link's is (a stalled consumer drops reports).
    private let queue: DispatchQueue
    /// One incoming report from one source (the collection's registry entry id), id-first — on
    /// `queue`. USB reports carry their id out of band, so this link prepends it; the wire
    /// contract is id-first on every transport.
    private let onReport: (UInt64, [UInt8]) -> Void
    /// One opened collection went away (pad unplugged; fires once per collection when the
    /// dongle is pulled) — on `queue`.
    private let onSourceClosed: (UInt64) -> Void

    // All state below is touched ONLY on `queue`.
    private var manager: IOHIDManager?
    /// Every opened controller collection, by IOKit registry entry id. The Puck contributes up
    /// to four; a wired pad exactly one.
    private var open: [UInt64: IOHIDDevice] = [:]
    /// Matched devices whose open failed with not-permitted — the Input Monitoring grant was
    /// missing. Held (as the MANAGER's objects, never opened) so a later `start()` can retry
    /// them once the user flips the grant in System Settings: the manager fires its matching
    /// callback once per device, so without this the pads stay dead until an app relaunch.
    private var denied: [UInt64: IOHIDDevice] = [:]
    /// Per-device input buffer, kept alive for as long as the device is open:
    /// `IOHIDDeviceRegisterInputReportCallback` writes into this memory for the device's whole
    /// lifetime, so a Swift array's storage would be a dangling pointer the moment it moved.
    private var buffers: [UInt64: UnsafeMutablePointer<UInt8>] = [:]
    private var keepAlive: DispatchSourceTimer?
    private var started = false
    /// Report ids seen so far — one line each, for remote diagnosis of what the pad emits.
    private var seenIds: Set<UInt8> = []
    /// Whether the unknown-source write drop logged yet (once per link life — the drop is
    /// expected only for a source that just unplugged, and a persistent one is invisible
    /// otherwise).
    private var loggedNoTarget = false
    /// Sources whose serial GET already ran this bond cycle — one blocking GET per live pad,
    /// re-armed by a wireless-disconnect edge so a DIFFERENT pad bonding to the same slot is
    /// re-read instead of inheriting the previous pad's identity. On `queue`.
    private var serialAttempted: Set<UInt64> = []

    /// The sources that belong to a Puck dongle, and each source's engraved serial; written on
    /// `queue`, read from any thread, hence the dedicated lock (the capture reads both on the
    /// main actor at claim time).
    private let dongleLock = NSLock()
    private var dongleSources: Set<UInt64> = []
    private var serials: [UInt64: String] = [:]

    /// The pad's engraved serial (`FXA…`), read once on the slot's first report — nil when the
    /// pad never answered. Safe from any thread. Logged with the claim today; the durable
    /// identity a future wire extension carries to the host, whose virtual pads currently mint
    /// serials by a positional pad slot (an ordering that reshuffles across sessions).
    func serial(source: UInt64) -> String? {
        dongleLock.lock()
        defer { dongleLock.unlock() }
        return serials[source]
    }

    /// Re-adopt every device the Input Monitoring gate refused, once the grant is present.
    /// On `queue`.
    private func retryDenied() {
        guard !denied.isEmpty,
              IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) == kIOHIDAccessTypeGranted
        else { return }
        let retry = denied
        denied.removeAll()
        log.info("SC2 USB: Input Monitoring granted — retrying \(retry.count, privacy: .public) denied open(s)")
        for (_, device) in retry { adopt(device) }
    }

    /// Whether `source` is a Puck-dongle collection rather than a directly-attached pad. Read
    /// by `Sc2Capture` to pick the declared wire kind and to decide whether wireless-status
    /// reports are authoritative — a WIRED pad emits them too, truthfully reporting "no radio
    /// link". Safe from any thread.
    func isDongle(source: UInt64) -> Bool {
        dongleLock.lock()
        defer { dongleLock.unlock() }
        return dongleSources.contains(source)
    }

    /// `IOHIDDeviceRegisterInputReportCallback`'s report buffer size. The Triton's longest input
    /// report is the 54-byte 0x42 state; 64 is the HID report bound the ABI already clamps to
    /// (`PUNKTFUNK_HID_REPORT_MAX`).
    private static let reportBufSize = 64

    init(
        queue: DispatchQueue,
        onReport: @escaping (UInt64, [UInt8]) -> Void,
        onSourceClosed: @escaping (UInt64) -> Void
    ) {
        self.queue = queue
        self.onReport = onReport
        self.onSourceClosed = onSourceClosed
    }

    /// Whether any SC2 controller collection is attached right now — the cheap pre-flight
    /// `Sc2Capture` uses to pick USB over BLE. Opens nothing, so it cannot prompt or disturb a
    /// device another app holds. Safe from any thread.
    static func attached() -> Bool {
        let mgr = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
        IOHIDManagerSetDeviceMatchingMultiple(mgr, matchingCriteria() as CFArray)
        let devices = IOHIDManagerCopyDevices(mgr) as? Set<IOHIDDevice> ?? []
        return !devices.isEmpty
    }

    /// One match dictionary per USB product id, each pinned to the controller collection's usage
    /// pair. ⚠ The DEVICE-usage-pair keys, not the primary-usage ones: on-glass (Puck, macOS 27,
    /// 2026-08-31) each controller interface is ONE `IOHIDDevice` carrying ALL its collections —
    /// lizard mouse, pointer, lizard keyboard, and the vendor controller (`01:02 01:01 01:06
    /// ff00:01`) — with PRIMARY usage `0001:0002`, so a primary-usage match finds nothing. The
    /// pair keys match any declared pair: `ff00:01` selects exactly the four controller slots
    /// and still excludes the management interface, whose only pair is `ff00:02`.
    /// An SC2 is attached and Input Monitoring lets this app open it. Safe from any thread.
    static func canOpenAttached() -> Bool {
        attached() && IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) == kIOHIDAccessTypeGranted
    }

    private static func matchingCriteria() -> [CFDictionary] {
        Sc2Device.usbPIDs.map { pid in
            [
                kIOHIDVendorIDKey: Sc2Device.vidValve,
                kIOHIDProductIDKey: pid,
                kIOHIDDeviceUsagePageKey: Sc2Device.usagePageVendor,
                kIOHIDDeviceUsageKey: Sc2Device.usageController,
            ] as CFDictionary
        }
    }

    /// Begin acquisition. Idempotent; safe from any thread. Devices attached later are picked up
    /// by the manager's matching callback, so a pad plugged in mid-session simply joins — and a
    /// re-`start()` on a running link retries any opens the Input Monitoring gate refused, which
    /// is how returning from System Settings recovers the pads without a relaunch (the app's
    /// did-become-active path re-runs `startTransport`).
    func start() {
        queue.async { [self] in
            guard manager == nil else {
                retryDenied()
                return
            }
            started = true
            // The controller interface carries the lizard KEYBOARD collection too (on-glass
            // census, see the header), so opening it sits behind the Input Monitoring TCC gate.
            // Ask before the first open — without this, `IOHIDDeviceOpen` fails not-permitted
            // forever and the user is never shown the question. ⚠ An UNBUNDLED binary (swift
            // run) gets no prompt from this — measured silent-false 2026-08-31; the grant must
            // be added by hand in System Settings for dev-shell runs. The bundled app prompts.
            if IOHIDCheckAccess(kIOHIDRequestTypeListenEvent) != kIOHIDAccessTypeGranted {
                let granted = IOHIDRequestAccess(kIOHIDRequestTypeListenEvent)
                log.info(
                    "SC2 USB: Input Monitoring requested (granted=\(granted, privacy: .public))")
            }
            let mgr = IOHIDManagerCreate(kCFAllocatorDefault, IOOptionBits(kIOHIDOptionsTypeNone))
            IOHIDManagerSetDeviceMatchingMultiple(mgr, Self.matchingCriteria() as CFArray)
            let ctx = Unmanaged.passUnretained(self).toOpaque()
            IOHIDManagerRegisterDeviceMatchingCallback(
                mgr,
                { ctx, _, _, device in
                    guard let ctx else { return }
                    Unmanaged<Sc2UsbLink>.fromOpaque(ctx).takeUnretainedValue().adopt(device)
                }, ctx)
            IOHIDManagerRegisterDeviceRemovalCallback(
                mgr,
                { ctx, _, _, device in
                    guard let ctx else { return }
                    Unmanaged<Sc2UsbLink>.fromOpaque(ctx).takeUnretainedValue().drop(device)
                }, ctx)
            IOHIDManagerSetDispatchQueue(mgr, queue)
            IOHIDManagerActivate(mgr)
            manager = mgr
        }
    }

    /// Close every collection and tear the manager down. Idempotent; safe from any thread. Does
    /// not fire `onSourceClosed` — the caller is the one tearing down.
    ///
    /// Lizard mode goes back on first: without it the pad stays in Steam-Input HID until the
    /// firmware watchdog fires, driving neither keyboard nor mouse for those seconds.
    func stop() {
        queue.async { [self] in
            started = false
            // Keep-alive first, so no disable can land after the restore; the restore before the
            // cancels, so it reaches a collection that is still open.
            stopKeepAlive()
            sendFeature([Sc2Device.enableLizard])
            for (id, dev) in open { cancel(id: id, device: dev) }
            open.removeAll()
            denied.removeAll()
            dongleLock.lock()
            dongleSources.removeAll()
            serials.removeAll()
            dongleLock.unlock()
            seenIds.removeAll()
            loggedNoTarget = false
            serialAttempted.removeAll()
            if let mgr = manager {
                // Dispatch-queue mode: Cancel, never Close/UnscheduleFromRunLoop — mixing the
                // run-loop teardown API with a queue-scheduled manager is undefined and crashes.
                // The matching/removal callbacks carry an UNRETAINED self, and the cancel is
                // asynchronous, so both are held until IOKit says it is done with them: a device
                // arriving inside that window would otherwise reach a freed link.
                let held = Unmanaged.passRetained(self)
                IOHIDManagerSetCancelHandler(mgr) { held.release() }
                IOHIDManagerCancel(mgr)
            }
            manager = nil
        }
    }

    /// Retire one opened collection: stop delivery, then free its report buffer — in that order,
    /// and only once IOKit says delivery has actually stopped.
    ///
    /// The ordering is the whole point. `IOHIDDeviceRegisterInputReportCallback` keeps writing
    /// into `buffers[id]` until the device is cancelled, and `IOHIDDeviceCancel` is ASYNCHRONOUS.
    /// Deallocating on the calling side would therefore race a callback already in flight and
    /// scribble on freed memory. The cancel handler is the one place IOKit guarantees no further
    /// callback can arrive, so the close and the free both live there.
    private func cancel(id: UInt64, device: IOHIDDevice) {
        let buf = buffers.removeValue(forKey: id)
        IOHIDDeviceSetCancelHandler(device) {
            IOHIDDeviceClose(device, IOOptionBits(kIOHIDOptionsTypeNone))
            buf?.deallocate()
        }
        IOHIDDeviceCancel(device)
    }

    /// Replay one raw host report on the physical pad behind `source`. `kind` is the C ABI's
    /// `PUNKTFUNK_HID_RAW_OUTPUT` (0) / `PUNKTFUNK_HID_RAW_FEATURE` (1); `frame` is id-first,
    /// exactly as Steam wrote it. Safe from any thread.
    ///
    /// Unlike the BLE link there is no per-report characteristic to resolve and NO trimming:
    /// `IOHIDDeviceSetReport` takes the frame as the device's own HID stack expects it, which is
    /// precisely what the host already sent. (`Sc2Device.strippedOutputLen` exists to undo the
    /// GATT transport's id-stripping; USB has no such transform to undo.)
    func writeRaw(source: UInt64, kind: UInt8, frame: [UInt8]) {
        queue.async { [self] in
            guard let id = frame.first else { return }
            guard let dev = open[source] else {
                // Expected only in the unplug window — the capture routed by a wire slot the
                // source claimed, so a miss means the collection just went away. Say so once:
                // if this fires steadily, host writes are being thrown away.
                if !loggedNoTarget {
                    loggedNoTarget = true
                    log.info("SC2 USB: host write for a gone source — dropped")
                }
                return
            }
            let type: IOHIDReportType = kind == 1 ? kIOHIDReportTypeFeature : kIOHIDReportTypeOutput
            let rc = frame.withUnsafeBufferPointer { buf in
                IOHIDDeviceSetReport(dev, type, CFIndex(id), buf.baseAddress!, buf.count)
            }
            if rc != kIOReturnSuccess {
                // Not fatal and deliberately not retried: rumble is re-sent by Steam every
                // 25–40 ms and settings every ~3 s, so the next frame self-heals. Worth a line —
                // a persistently failing write is invisible otherwise.
                log.error(
                    "SC2 USB: SetReport id 0x\(String(id, radix: 16), privacy: .public) failed (0x\(String(format: "%08x", rc), privacy: .public))"
                )
            }
        }
    }

    // MARK: - Device lifecycle (queue)

    /// Open one newly matched controller collection and start its report callback.
    ///
    /// ⚠ Never opens or registers on the MANAGER's device object: `IOHIDManagerActivate`
    /// activates every device the manager owns, this callback runs during that activation, and
    /// per-device callback registration on an activated device traps (on-glass crash
    /// 2026-08-31: EXC_BREAKPOINT in `IOHIDDeviceRegisterInputReportCallback` under
    /// `IOHIDManagerActivate`). A fresh `IOHIDDevice` minted from the same IOKit service is
    /// ours alone, so the register → set-queue → activate order the API requires holds.
    private func adopt(_ matched: IOHIDDevice) {
        guard started else { return }
        guard let id = Self.registryID(matched) else {
            log.error("SC2 USB: matched collection has no registry id — dropping it")
            return
        }
        guard open[id] == nil else { return }
        let service = IOHIDDeviceGetService(matched)
        guard service != MACH_PORT_NULL,
              let device = IOHIDDeviceCreate(kCFAllocatorDefault, service)
        else {
            log.error("SC2 USB: no device from the matched service")
            return
        }
        let rc = IOHIDDeviceOpen(device, IOOptionBits(kIOHIDOptionsTypeNone))
        guard rc == kIOReturnSuccess else {
            // Not-permitted = the Input Monitoring grant is missing (the controller interface
            // carries the lizard keyboard collection). Park the matched device for
            // `retryDenied` — the manager will not re-fire matching for it, so without the
            // stash the pad stays dead until an app relaunch.
            let hint = rc == kIOReturnNotPermitted ? " — not permitted (Input Monitoring)" : ""
            if rc == kIOReturnNotPermitted { denied[id] = matched }
            log.error(
                "SC2 USB: open failed (0x\(String(format: "%08x", rc), privacy: .public))\(hint, privacy: .public)"
            )
            return
        }
        let pid = (IOHIDDeviceGetProperty(device, kIOHIDProductIDKey as CFString) as? Int) ?? 0
        let buf = UnsafeMutablePointer<UInt8>.allocate(capacity: Self.reportBufSize)
        buf.initialize(repeating: 0, count: Self.reportBufSize)
        buffers[id] = buf
        open[id] = device
        if Sc2Device.isDongle(pid: pid) {
            dongleLock.lock()
            dongleSources.insert(id)
            dongleLock.unlock()
        }
        IOHIDDeviceRegisterInputReportCallback(
            device, buf, Self.reportBufSize,
            { ctx, _, sender, _, reportID, report, len in
                guard let ctx, let sender else { return }
                let link = Unmanaged<Sc2UsbLink>.fromOpaque(ctx).takeUnretainedValue()
                let dev = Unmanaged<IOHIDDevice>.fromOpaque(sender).takeUnretainedValue()
                link.handle(device: dev, reportID: reportID, report: report, len: len)
            }, Unmanaged.passUnretained(self).toOpaque())
        IOHIDDeviceSetDispatchQueue(device, queue)
        IOHIDDeviceActivate(device)
        log.info(
            "SC2 USB: opened \(Sc2Device.isDongle(pid: pid) ? "Puck" : "wired", privacy: .public) collection 0x\(String(format: "%04x", pid), privacy: .public) (\(self.open.count, privacy: .public) open)"
        )
        startKeepAlive()
    }

    /// A collection went away: retire it and tell the capture, so ONE pad of several unplugging
    /// releases exactly its own wire slot — a Puck losing one slot is not a dongle unplug.
    private func drop(_ matched: IOHIDDevice) {
        guard let id = Self.registryID(matched) else { return }
        denied.removeValue(forKey: id)
        serialAttempted.remove(id)
        // Cancel OUR minted device (see `adopt`) — the manager's object was never activated,
        // and cancelling a non-activated device is its own trap.
        guard let mine = open.removeValue(forKey: id) else { return }
        cancel(id: id, device: mine)
        dongleLock.lock()
        dongleSources.remove(id)
        serials.removeValue(forKey: id)
        dongleLock.unlock()
        onSourceClosed(id)
        guard open.isEmpty else { return }
        stopKeepAlive()
        seenIds.removeAll()
        loggedNoTarget = false
        log.info("SC2 USB: last collection removed — link idle")
    }

    /// One input report from IOKit. USB delivers the id out of band (`reportID`) and the payload
    /// without it, so the id is prepended here: the punktfunk wire — and `Sc2Device.parseState`,
    /// and core's gate — are id-first on every transport.
    private func handle(
        device: IOHIDDevice, reportID: UInt32, report: UnsafeMutablePointer<UInt8>, len: CFIndex
    ) {
        // `drop` removes the entry before IOKit stops delivering (the device cancel is
        // asynchronous), so a report can still arrive for a collection we have retired. Acting on
        // it resurrects the source downstream and claims a wire slot nothing will ever release.
        guard len > 0, let source = Self.registryID(device), open[source] != nil else { return }
        let id = UInt8(truncatingIfNeeded: reportID)
        // The payload byte position follows the same id-in-band rule the framing below applies.
        let wirelessPayload: UInt8? = (id == Sc2Device.idWireless || id == Sc2Device.idWirelessX)
            ? (report[0] == id ? (len >= 2 ? report[1] : nil) : report[0])
            : nil
        if wirelessPayload == Sc2Device.wirelessDisconnect, isDongle(source: source) {
            // A disconnect edge re-arms the serial read: whatever bonds to this slot next may
            // be a DIFFERENT pad, and serving pad A's engraved identity for pad B is the one
            // failure the serial must never have. Dongle-gated for the same reason
            // `Sc2Capture.handleWireless` is — a WIRED pad emits this too, truthfully saying
            // "no radio link", and acting on it would re-arm a blocking GET on every one.
            serialAttempted.remove(source)
            dongleLock.lock()
            serials.removeValue(forKey: source)
            dongleLock.unlock()
        } else if !serialAttempted.contains(source), let dev = open[source] {
            // Read the engraved serial lazily, on the slot's FIRST report of a bond cycle: at
            // adopt a Puck slot is usually empty (pads bond later), and a blocking feature GET
            // against a silent slot could stall this shared queue for every pad. A slot that
            // just spoke has a live pad behind it, which answers control transfers promptly.
            // Marked attempted either way, so a wired pad costs one `isDongle` per bond cycle.
            serialAttempted.insert(source)
            // Puck slots only: they all share the dongle's USB serial, which is what makes the
            // engraved one worth a control transfer. A wired pad's USB serial is already its own.
            if isDongle(source: source) { readSerial(id: source, device: dev) }
        }
        if seenIds.insert(id).inserted {
            log.info(
                "SC2 USB: report id=0x\(String(id, radix: 16), privacy: .public) seen (len=\(len, privacy: .public))"
            )
        }
        // The callback buffer arrives id-FIRST on-glass (0x45 at len 46 — the id-INCLUDED wire
        // size; 2026-08-31), so it is forwarded verbatim: re-prepending shifted every field one
        // byte and the typed mirror sprayed phantom input. The prepend survives only as the
        // guard for a platform that ever delivers the buffer stripped.
        var framed: [UInt8]
        if report[0] == id {
            let n = min(Int(len), Self.reportBufSize)
            framed = [UInt8](repeating: 0, count: n)
            for i in 0..<n { framed[i] = report[i] }
        } else {
            framed = [UInt8](repeating: 0, count: min(Int(len), Self.reportBufSize - 1) + 1)
            framed[0] = id
            for i in 1..<framed.count { framed[i] = report[i - 1] }
        }
        if Self.verbose {
            log.debug("SC2 USB in: \(framed.map { String(format: "%02x", $0) }.joined(), privacy: .public)")
        }
        onReport(source, framed)
    }

    /// One feature-2 GET per bond cycle — the engraved per-pad serial (`Sc2Device.
    /// featureSerial`). Blocking and synchronous at the kernel boundary, so it obeys the
    /// once-per-pad rule the Android transport pins for GET_REPORT: never per input report. A
    /// pad that answers with nothing parseable simply has no serial recorded; the capture logs
    /// `serial ?` and everything else works. On `queue`.
    private func readSerial(id: UInt64, device: IOHIDDevice) {
        var buf = [UInt8](repeating: 0, count: 65)
        buf[0] = Sc2Device.featureSerial
        var len: CFIndex = buf.count
        let rc = buf.withUnsafeMutableBufferPointer { p in
            IOHIDDeviceGetReport(
                device, kIOHIDReportTypeFeature,
                CFIndex(Sc2Device.featureSerial), p.baseAddress!, &len)
        }
        guard rc == kIOReturnSuccess, len > 0,
              let serial = Sc2Device.parseSerial(Array(buf[..<min(Int(len), buf.count)]))
        else {
            log.info(
                "SC2 USB: no engraved serial (GET 0x02 rc=0x\(String(format: "%08x", rc), privacy: .public))"
            )
            return
        }
        dongleLock.lock()
        serials[id] = serial
        dongleLock.unlock()
        log.info("SC2 USB: slot serial \(serial)")
    }

    /// `source`'s identity for the host: its USB serial, its replies to core's feature queries,
    /// and on a Puck its slot (interface 2–5), so the host seats pads of one Puck together. A Puck
    /// stalls each GET until its pad answers over the radio (~40 ms), so each is retried for up
    /// to a second. On `queue`, which it blocks for the reads.
    func identity(source: UInt64) -> PunktfunkConnection.PadIdentity? {
        guard let dev = open[source] else { return nil }
        let serial = (IOHIDDeviceGetProperty(dev, kIOHIDSerialNumberKey as CFString) as? String) ?? ""
        let puck = isDongle(source: source)
        let requests = PunktfunkConnection.sc2IdentityRequests(puck: puck)
        let replies = requests.compactMap { request in
            exchange(dev, request).map { PunktfunkConnection.PadIdentity.Reply(request: request, reply: $0) }
        }
        let slot = puck ? UInt8(clamping: min(max((Self.interfaceNumber(dev) ?? 2) - 2, 0), 3)) : 0
        log.info("SC2 USB: identity \(serial, privacy: .public) slot \(slot), \(replies.count, privacy: .public)/\(requests.count, privacy: .public) replies")
        return .init(serial: serial, replies: replies, slot: slot)
    }

    /// The USB interface `device` is: `bInterfaceNumber` on its nearest IOKit ancestor.
    private static func interfaceNumber(_ device: IOHIDDevice) -> Int? {
        let service = IOHIDDeviceGetService(device)
        guard service != MACH_PORT_NULL else { return nil }
        let value = IORegistryEntrySearchCFProperty(
            service, kIOServicePlane, "bInterfaceNumber" as CFString, kCFAllocatorDefault,
            IOOptionBits(kIORegistryIterateRecursively | kIORegistryIterateParents))
        return (value as? NSNumber)?.intValue
    }

    /// SET one feature query, then GET until the reply echoes its command. Id-first both ways.
    private func exchange(_ dev: IOHIDDevice, _ request: [UInt8]) -> [UInt8]? {
        guard request.count >= 2 else { return nil }
        let id = request[0]
        let frame = request + [UInt8](repeating: 0, count: max(0, 64 - request.count))
        let set = frame.withUnsafeBufferPointer { buf in
            IOHIDDeviceSetReport(dev, kIOHIDReportTypeFeature, CFIndex(id), buf.baseAddress!, buf.count)
        }
        guard set == kIOReturnSuccess else { return nil }
        for _ in 0..<50 {
            usleep(20_000)
            var buf = [UInt8](repeating: 0, count: 64)
            buf[0] = id
            var len: CFIndex = buf.count
            let rc = buf.withUnsafeMutableBufferPointer { p in
                IOHIDDeviceGetReport(dev, kIOHIDReportTypeFeature, CFIndex(id), p.baseAddress!, &len)
            }
            if rc == kIOReturnSuccess, len > 1, buf[1] == request[1] {
                return Array(buf[..<min(Int(len), buf.count)])
            }
        }
        return nil
    }

    // MARK: - Lizard keep-alive

    /// Re-send the initialization features on SDL's cadence, and once immediately: without it the
    /// firmware watchdog restores lizard mode a few seconds in, and the pad reverts to driving
    /// the desktop cursor instead of streaming controller reports.
    private func startKeepAlive() {
        guard keepAlive == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: Sc2Device.lizardRefreshSeconds)
        timer.setEventHandler { [weak self] in self?.sendInitFeatures() }
        keepAlive = timer
        timer.resume()
    }

    private func stopKeepAlive() {
        keepAlive?.cancel()
        keepAlive = nil
    }

    /// Both initialization features, to EVERY open collection — before the first report there is
    /// no known target, and on a Puck the bonded slot is exactly what we are trying to discover.
    private func sendInitFeatures() {
        sendFeature([Sc2Device.disableLizard, Sc2Device.normalizeJoysticks])
    }

    /// Write id-first feature frames to every open collection, in order. `IOHIDDeviceSetReport`
    /// is synchronous, so a frame sent here has landed before the next line runs.
    private func sendFeature(_ frames: [[UInt8]]) {
        for (_, dev) in open {
            for frame in frames {
                _ = frame.withUnsafeBufferPointer { buf in
                    IOHIDDeviceSetReport(
                        dev, kIOHIDReportTypeFeature, CFIndex(frame[0]), buf.baseAddress!,
                        buf.count)
                }
            }
        }
    }

    // MARK: - Helpers

    /// A stable per-collection key. `IOHIDDevice` is a CF type with no usable identity in a
    /// Swift dictionary, and the Puck's four collections share VID/PID, so the IOKit registry
    /// entry id is what tells them apart.
    ///
    /// It must come from the SERVICE, and nil when there is none. `adopt`/`drop` key off the
    /// MANAGER's matched object while `handle` sees the one we minted from the same service
    /// — only the service's id is equal across the two, so any object-derived fallback would
    /// silently split one collection into two keys: no serial, and every host write dropped.
    private static func registryID(_ device: IOHIDDevice) -> UInt64? {
        let service = IOHIDDeviceGetService(device)
        guard service != MACH_PORT_NULL else { return nil }
        var id: UInt64 = 0
        guard IORegistryEntryGetRegistryEntryID(service, &id) == KERN_SUCCESS else { return nil }
        return id
    }
}

#endif
