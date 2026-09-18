// CoreBluetooth transport for a Steam Controller 2 paired directly with this device — the thin
// hardware shim under `Sc2Capture`, and the ONLY file here that imports CoreBluetooth (every
// table/framing rule it applies lives device-free in `Sc2Device`, where tests reach it). The
// acquisition, subscription and write paths are the ones proven against real hardware on the
// bench (2026-06-08/09): OS-paired acquisition via the connected set, the per-report output
// characteristics, and feature writes on 100F6C34.
//
// OS-paired is FINE: the controller is normally connected via the system Settings, holding the
// standard HID (0x1812) binding — we open our own handle to the CUSTOM Valve service, which
// coexists with it (exactly what Steam Link does). "Lizard mode" is just the controller's
// default reporting, cleared by the disable-lizard write — not a barrier to the vendor service.
//
// Simulator has no BLE radio — physical device only.
//
// ⚠ DIFFER from Android's `Sc2BleLink.kt`: no `requestMtu(100)` / connection-priority-HIGH
// equivalents — CoreBluetooth negotiates MTU and connection interval itself; the
// `maximumWriteValueLength` census line is a sanity log only. The link itself sends
// DISABLE_LIZARD on ready and re-sends it every ~3 s (Android's cadence — SDL's): the host's
// virtual pad only relays what Steam sends AFTER it claims the pad, so until then nothing else
// would feed the firmware watchdog and the controller would fall back to lizard mode. `stop()`
// writes lizard back ON before it disconnects, so the pad drives the OS again at once. The client
// NEVER self-enables the gyro — Steam's own forwarded write drives `Sc2ImuGate`.

import CoreBluetooth
import Foundation

private let log = ClientLog(category: "gamepad")

final class Sc2BleLink: NSObject {
    /// Per-frame / per-write diagnostics (raw hex, per-characteristic props). Lifecycle
    /// milestones (acquire/connect/census/ready/disconnect) always log; flip this only to debug
    /// the BLE seam.
    private static let verbose = false

    /// How long `stop()` holds the connection open for the lizard restore to land.
    private static let restoreWait: DispatchTimeInterval = .milliseconds(150)

    private let serviceCB = CBUUID(string: Sc2Device.serviceUUID)
    private let inputCB = CBUUID(string: Sc2Device.inputCharUUID)
    private let reportCB = CBUUID(string: Sc2Device.reportCharUUID)
    /// Report 0x47's characteristic. Never subscribed (see `Sc2Device.timestampCharUUID`); held
    /// here only so the sweep can exclude a known-purpose characteristic by UUID.
    private let timestampCB = CBUUID(string: Sc2Device.timestampCharUUID)
    /// Device Information — every BLE device exposes it, which is what makes it the handle for
    /// `retrieveConnectedPeripherals` (the OS-paired controller is NOT advertising).
    private let deviceInfoCB = CBUUID(string: "180A")

    /// The serial queue every delegate callback and every state mutation runs on — owned by
    /// `Sc2Capture`, USER_INTERACTIVE because SDL's hid.m warns BLE packets are silently dropped
    /// if the consumer stalls.
    private let queue: DispatchQueue
    /// One incoming report, already id-first framed (`Sc2Device.frameIncoming`) — on `queue`.
    private let onReport: ([UInt8]) -> Void
    /// The controller disconnected (powered off / out of range) — on `queue`. The link keeps
    /// re-acquiring by itself; this only tells the capture to release its slot.
    private let onClosed: () -> Void

    // All state below is touched ONLY on `queue`.
    private var central: CBCentralManager?
    private var controller: CBPeripheral?
    private var inputChar: CBCharacteristic?
    private var reportChar: CBCharacteristic?
    /// uuid (lowercase) → characteristic, for the per-report output routing.
    private var allChars: [String: CBCharacteristic] = [:]
    /// Writable characteristics in discovery order — the sweep fallback for a firmware whose
    /// output chars are not at id+0x35.
    private var candidateChars: [CBCharacteristic] = []
    private var scanning = false
    private var polling = false
    private var ready = false
    private var lizardTimer: DispatchSourceTimer?
    /// `stop()` wrote the lizard restore and is waiting for it to land — see `finishStop`.
    private var pendingStop = false
    private var lizardSends = 0
    private var sweepCounter = 0
    private var inCounter = 0
    /// Output report ids already reported as having no characteristic on this firmware — the
    /// unknown-firmware signal is worth exactly one line per id, not one per resend at 25-40 ms.
    private var unmappedIds: Set<UInt8> = []

    init(
        queue: DispatchQueue,
        onReport: @escaping ([UInt8]) -> Void,
        onClosed: @escaping () -> Void
    ) {
        self.queue = queue
        self.onReport = onReport
        self.onClosed = onClosed
        super.init()
    }

    /// Start (or restart) acquisition. Idempotent; safe from any thread. Scanning begins once
    /// the central reports poweredOn.
    func start() {
        queue.async { [self] in
            // A restart never waits out the lizard restore below — finish that teardown first,
            // or this returns early on the central it was about to drop.
            if pendingStop { finishStop() }
            guard central == nil else { return }
            scanning = false
            polling = false
            ready = false
            controller = nil
            central = CBCentralManager(delegate: self, queue: queue)
        }
    }

    /// Hand lizard mode back, then disconnect and tear the central down. Idempotent; safe from
    /// any thread. Does not fire `onClosed` — the caller is the one tearing down.
    ///
    /// Order is the point. The keep-alive dies first, so no disable can land after the restore;
    /// the restore goes out before the cancel, which drops whatever is still queued.
    func stop() {
        queue.async { [self] in
            stopLizardTimer()
            // A second stop rides the first one's window rather than cutting it short: the
            // teardown below is already scheduled, and the restore still needs its ack.
            if pendingStop { return }
            guard let controller, let reportChar,
                  let payload = Sc2Device.featurePayload(frame: Sc2Device.enableLizard)
            else {
                finishStop()
                return
            }
            pendingStop = true
            let type: CBCharacteristicWriteType =
                reportChar.properties.contains(.write) ? .withResponse : .withoutResponse
            controller.writeValue(Data(payload), for: reportChar, type: type)
            // 150 ms ≈ several BLE connection intervals. A pad that went out of range mid-write
            // never acks, so the wait has to end on its own. `pendingStop` is what a `start()`
            // in the meantime clears — this must not tear down the central it built.
            // Strong: the owner drops its reference right after stop(), and a freed central
            // cancels the connection before the restore lands.
            queue.asyncAfter(deadline: .now() + Self.restoreWait) { [self] in
                guard pendingStop else { return }
                finishStop()
            }
        }
    }

    /// The teardown itself, once the restore has had its window. Idempotent.
    private func finishStop() {
        pendingStop = false
        // CoreBluetooth takes these three only while the radio is on. A teardown lands before
        // that whenever the session ends with the permission alert still up, and the frameworks
        // then log "API MISUSE ... powered on state". Dropping the state clears everything below
        // anyway, and a central that never powered on holds no connection to cancel.
        if central?.state == .poweredOn {
            if let inputChar, let controller {
                controller.setNotifyValue(false, for: inputChar)
            }
            if let controller {
                central?.cancelPeripheralConnection(controller)
            }
            central?.stopScan()
        }
        controller = nil
        inputChar = nil
        reportChar = nil
        allChars.removeAll()
        candidateChars.removeAll()
        scanning = false
        polling = false
        ready = false
        central = nil
    }

    /// Replay one raw host report on the physical controller. `kind` is the C ABI's
    /// `PUNKTFUNK_HID_RAW_OUTPUT` (0) / `PUNKTFUNK_HID_RAW_FEATURE` (1); `frame` is id-first,
    /// exactly as Steam wrote it. Safe from any thread — `frame` is an owned copy, and the
    /// resolution + GATT write happen on the BLE queue.
    func writeRaw(kind: UInt8, frame: [UInt8]) {
        queue.async { [self] in
            guard let controller else { return }
            let target: CBCharacteristic?
            let payload: [UInt8]
            if kind == 0 {
                // OUTPUT: per-report characteristic, id stripped, payload trimmed to the
                // declared length (Sc2Device.outputWrite).
                guard let write = Sc2Device.outputWrite(frame: frame) else { return }
                payload = write.payload
                target = allChars[write.charUUID] ?? sweepCandidate(id: frame[0])
                // A miss means this firmware does not put report `id` at 100F6C<id+0x35>. There
                // is no safe way to recover it live — ten output characteristics share one
                // property mask and nothing distinguishes rumble from a trackpad pulse, so any
                // guess is as likely to drive the wrong actuator as the right one. Drop the
                // write and say so ONCE per id: with the connect-time GATT census above, a log
                // bundle from one affected pad is enough to add its ids to
                // `Sc2Device.outputCharUUID` — the map is the only reliable fix.
                if target == nil, unmappedIds.insert(frame[0]).inserted {
                    let id = String(format: "%02x", frame[0])
                    log.warning("SC2: firmware has no characteristic for output id 0x\(id) (expected \(write.charUUID)) — dropping it; that actuator stays silent on this pad")
                }
            } else {
                // FEATURE: strip the 0x01 channel report-id, write to 100F6C34 whole
                // (zero-padding included — the firmware accepts the padded form).
                guard let stripped = Sc2Device.featurePayload(frame: frame) else { return }
                payload = stripped
                target = reportChar
            }
            guard let target else { return }
            if Self.verbose {
                let hex = payload.prefix(13).map { String(format: "%02x", $0) }.joined(separator: " ")
                log.debug("SC2 write kind=\(kind) id=0x\(String(format: "%02x", frame[0])) \(payload.count)B: \(hex)")
            }
            // OUTPUT prefers unacked writes (the 25-40 ms rumble resend rate must not queue
            // behind acks); FEATURE prefers acked. Either way honor what the char offers.
            let wantAck = kind != 0
            let type: CBCharacteristicWriteType
            if wantAck {
                type = target.properties.contains(.write) ? .withResponse : .withoutResponse
            } else {
                type = target.properties.contains(.writeWithoutResponse) ? .withoutResponse : .withResponse
            }
            controller.writeValue(Data(payload), for: target, type: type)
        }
    }

    /// Firmware without a per-report characteristic at id+0x35: rotate through the writable
    /// candidates (~1 s each at the 25 Hz resend rate) to find where its outputs live. This is
    /// how the real map was found on the bench, and it stays for the next unknown firmware.
    ///
    /// ⚠ BENCH TOOL — gated off in shipping builds, because it does not converge. Nothing here
    /// observes whether a write produced haptics, so there is no success signal to latch: the
    /// rotation runs for the life of the session, and on an unknown firmware MOST output frames
    /// land on the wrong characteristic forever rather than eventually settling on the right one
    /// (with N candidates the correct one is live 1 second in every N). That is not a fallback
    /// that degrades gracefully — losing haptics on such a pad is the better failure than feeding
    /// output payloads to parsers we have not identified. Flip `verbose` to map a new firmware,
    /// then add its ids to `Sc2Device.outputCharUUID`.
    private func sweepCandidate(id: UInt8) -> CBCharacteristic? {
        guard Self.verbose, !candidateChars.isEmpty else { return nil }
        let index = (sweepCounter / 25) % candidateChars.count
        if sweepCounter % 25 == 0 {
            log.info("SC2: no per-report char for id=0x\(String(format: "%02x", id)) — sweeping candidate \(index)")
        }
        sweepCounter += 1
        return candidateChars[index]
    }

    // MARK: - Acquisition (mirroring Valve's own SDL hid.m)

    /// PRIMARY: the controller is normally OS-paired and NOT advertising — find it in the
    /// OS-connected set (Device Information 0x180A, which every BLE device exposes) by name.
    /// FALLBACK: scan for a first-time/advertising controller. Plus the CRITICAL 2 s re-poll:
    /// an already-paired controller powered on MID-SESSION connects to the OS without
    /// advertising, so scan alone never sees it — only the re-polled connected set does.
    private func acquire() {
        guard let central, controller == nil else { return }
        let connected = central.retrieveConnectedPeripherals(withServices: [deviceInfoCB])
        if let match = connected.first(where: { Self.nameMatches($0.name) }) {
            log.info("SC2: found OS-connected controller '\(match.name ?? "?")'")
            controller = match // retain before connecting
            if scanning {
                central.stopScan()
                scanning = false
            }
            central.connect(match, options: nil)
            return
        }
        if !scanning {
            log.info("SC2: no OS-connected Steam controller; scanning + polling")
            central.scanForPeripherals(withServices: [serviceCB], options: nil)
            scanning = true
        }
        if !polling {
            polling = true
            queue.asyncAfter(deadline: .now() + 2.0) { [weak self] in
                guard let self else { return }
                self.polling = false
                if self.controller == nil, self.central?.state == .poweredOn {
                    self.acquire() // re-poll until the controller appears
                }
            }
        }
    }

    /// The bench-proven "Steam" prefix plus Android's broader hint set — BLE exposes no PID
    /// here, so the name is all there is.
    static func nameMatches(_ name: String?) -> Bool {
        guard let name else { return false }
        if name.hasPrefix("Steam") { return true }
        let lowered = name.lowercased()
        return ["steam ctrl", "steam controller", "steamcontroller", "valve"]
            .contains { lowered.contains($0) }
    }

    private func startLizardTimer() {
        guard lizardTimer == nil else { return }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(
            deadline: .now(), repeating: Sc2Device.lizardRefreshSeconds, leeway: .milliseconds(200))
        timer.setEventHandler { [weak self] in self?.sendLizardOff() }
        timer.resume()
        lizardTimer = timer
    }

    private func stopLizardTimer() {
        lizardTimer?.cancel()
        lizardTimer = nil
    }

    /// Acked DISABLE_LIZARD to the report characteristic — the firmware watchdog re-enables
    /// lizard mode after a few seconds of silence, so the timer above re-sends on SDL's cadence.
    /// Framed through `Sc2Device.featurePayload`, the SAME path the host-forwarded feature
    /// writes take (`writeRaw` kind == 1): the characteristic VALUE carries no 0x01 channel
    /// report-id — the firmware parses byte 0 as the settings-command id (the hardware-proven
    /// bench-proven contract) — so writing `disableLizard` whole would arrive as command
    /// 0x01 instead of 0x87 and silently do nothing. Throttled logging (the write repeats
    /// every 3 s for the whole session).
    private func sendLizardOff() {
        guard let controller, let reportChar,
              let payload = Sc2Device.featurePayload(frame: Sc2Device.disableLizard)
        else { return }
        let type: CBCharacteristicWriteType =
            reportChar.properties.contains(.write) ? .withResponse : .withoutResponse
        controller.writeValue(Data(payload), for: reportChar, type: type)
        lizardSends += 1
        if lizardSends == 1 || lizardSends % 20 == 0 {
            log.info("SC2: lizard-off keepalive #\(lizardSends)")
        }
    }
}

// MARK: - CBCentralManagerDelegate

extension Sc2BleLink: CBCentralManagerDelegate {
    func centralManagerDidUpdateState(_ central: CBCentralManager) {
        if central.state == .poweredOn {
            acquire()
        } else {
            log.info("SC2: BLE central state=\(central.state.rawValue) (need poweredOn)")
        }
    }

    func centralManager(
        _ central: CBCentralManager, didDiscover peripheral: CBPeripheral,
        advertisementData: [String: Any], rssi RSSI: NSNumber
    ) {
        guard controller == nil else { return }
        log.info("SC2: discovered '\(peripheral.name ?? "?")' (RSSI \(RSSI))")
        controller = peripheral // retain before connecting
        central.stopScan()
        scanning = false
        central.connect(peripheral, options: nil)
    }

    func centralManager(_ central: CBCentralManager, didConnect peripheral: CBPeripheral) {
        log.info("SC2: connected; discovering services")
        peripheral.delegate = self
        peripheral.discoverServices([serviceCB])
    }

    func centralManager(
        _ central: CBCentralManager, didFailToConnect peripheral: CBPeripheral, error: Error?
    ) {
        log.warning("SC2: connect failed (\(error.map { String(describing: $0) } ?? "?")); retrying")
        controller = nil
        if central.state == .poweredOn { acquire() }
    }

    func centralManager(
        _ central: CBCentralManager, didDisconnectPeripheral peripheral: CBPeripheral,
        error: Error?
    ) {
        log.info("SC2: disconnected (\(error.map { String(describing: $0) } ?? "clean")) — releasing + re-acquiring")
        stopLizardTimer()
        ready = false
        controller = nil
        inputChar = nil
        reportChar = nil
        allChars.removeAll()
        candidateChars.removeAll()
        onClosed() // the capture releases its wire slot + re-arms the IMU gate
        if central.state == .poweredOn {
            acquire() // pads power-cycle many times per session — keep polling
        }
    }
}

// MARK: - CBPeripheralDelegate

extension Sc2BleLink: CBPeripheralDelegate {
    func peripheral(_ peripheral: CBPeripheral, didDiscoverServices error: Error?) {
        for service in peripheral.services ?? [] {
            peripheral.discoverCharacteristics(nil, for: service)
        }
    }

    func peripheral(
        _ peripheral: CBPeripheral, didDiscoverCharacteristicsFor service: CBService,
        error: Error?
    ) {
        var writable = 0
        var notifying = 0
        for ch in service.characteristics ?? [] {
            let uuid = ch.uuid.uuidString.lowercased()
            allChars[uuid] = ch
            if !ch.properties.isDisjoint(with: [.write, .writeWithoutResponse]) {
                writable += 1
                // ⚠ A characteristic whose purpose is KNOWN is never a sweep candidate. 100F6C34
                // is writable (props 0x0a) and parses what it receives as a settings command
                // (`0x87` = write register/value, the lizard-off and gyro-enable path), so an
                // output payload swept onto it can persist an arbitrary firmware setting on the
                // user's own controller. Counted in the census, excluded from the rotation.
                if ch.uuid != reportCB && ch.uuid != inputCB && ch.uuid != timestampCB {
                    candidateChars.append(ch)
                }
            }
            if ch.properties.contains(.notify) { notifying += 1 }
            if Self.verbose {
                log.debug("SC2 char \(uuid) props=0x\(String(ch.properties.rawValue, radix: 16))")
            }
            if ch.uuid == inputCB {
                inputChar = ch
                peripheral.setNotifyValue(true, for: ch)
            } else if ch.uuid == reportCB {
                reportChar = ch
                if ch.properties.contains(.notify) {
                    peripheral.setNotifyValue(true, for: ch) // feature replies: logged + dropped
                }
            }
            // The 0x47 timestamp char is deliberately NOT subscribed — nothing rides it that the
            // punktfunk wire needs (the gyro streams inside the same 0x45 report once enabled).
        }
        // The on-device census (expected: 17 chars — 1× 0x0a report, 6× 0x12 read+notify,
        // 10× 0x0e read+write+writeNoResp) + the MTU sanity line CoreBluetooth negotiated.
        let mtu = peripheral.maximumWriteValueLength(for: .withoutResponse)
        log.info(
            "SC2 GATT census: \(allChars.count) chars (\(writable) writable, \(notifying) notify), maxWriteNoRsp=\(mtu)B — input=\(inputChar != nil) report=\(reportChar != nil)")
        if reportChar != nil {
            // READY enough to keep the firmware out of lizard mode; input flows once the
            // subscribe completes.
            startLizardTimer()
        }
    }

    func peripheral(
        _ peripheral: CBPeripheral, didUpdateNotificationStateFor characteristic: CBCharacteristic,
        error: Error?
    ) {
        if let error {
            log.warning("SC2: subscribe \(characteristic.uuid.uuidString) failed: \(String(describing: error))")
        }
    }

    func peripheral(
        _ peripheral: CBPeripheral, didUpdateValueFor characteristic: CBCharacteristic,
        error: Error?
    ) {
        guard error == nil, let data = characteristic.value, !data.isEmpty else { return }
        if characteristic.uuid == inputCB {
            let framed = Sc2Device.frameIncoming([UInt8](data))
            inCounter += 1
            if !ready {
                ready = true
                log.info("SC2: first input report (\(data.count) B) — device live")
            }
            if Self.verbose, inCounter <= 8 || inCounter % 200 == 0 {
                let hex = data.prefix(32).map { String(format: "%02x", $0) }.joined(separator: " ")
                log.debug("SC2 in #\(inCounter) len=\(data.count) raw: \(hex)")
            }
            onReport(framed)
        } else if characteristic.uuid == reportCB {
            // A feature reply. The HOST's virtual pad answers Steam's feature reads, and no
            // client→host reply plane exists — log and drop. (A stack whose synthetic pad lives
            // client-side would instead round-trip these to its own GET_FEATURE handler.)
            log.info("SC2: feature reply (\(data.count) B) — dropped (host answers Steam)")
        }
    }
}
