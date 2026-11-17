// Integration: the Swift wrapper against a real punktfunk/1 host over QUIC + UDP on loopback —
// the Swift twin of punktfunk-host's native/tests.rs::c_abi_connection_roundtrip, this time
// through the statically linked xcframework. Driven by clients/apple/test-loopback.sh, which
// builds and starts `punktfunk-host punktfunk1-host --source synthetic` and sets
// PUNKTFUNK_LOOPBACK_PORT.

import XCTest
@testable import PunktfunkKit

final class LoopbackIntegrationTests: XCTestCase {
    func testSyntheticStreamRoundTrip() throws {
        guard let portStr = ProcessInfo.processInfo.environment["PUNKTFUNK_LOOPBACK_PORT"],
              let port = UInt16(portStr)
        else {
            throw XCTSkip("needs a running punktfunk1-host — use clients/apple/test-loopback.sh")
        }

        let conn = try PunktfunkConnection(
            host: "127.0.0.1", port: port, width: 1280, height: 720, refreshHz: 60,
            bitrateKbps: 50_000)
        XCTAssertEqual(conn.width, 1280)
        XCTAssertEqual(conn.height, 720)
        XCTAssertEqual(conn.refreshHz, 60)
        // The Welcome echoes the negotiated encoder bitrate (50 Mbps is within the
        // host's accepted range, so it comes back unclamped).
        XCTAssertEqual(conn.resolvedBitrateKbps, 50_000)

        // Pull 25 synthetic frames and byte-verify the documented pattern:
        // u32 LE frame index, then data[i] = (idx as u8) &+ (i as u8). Alongside, drain the
        // per-AU host-timing plane (0xCF) the way the app's stats tick does — the connector
        // ORs VIDEO_CAP_HOST_TIMING in unconditionally and the synthetic host stamps one
        // report per AU, so the pts correlation must hold end to end through the xcframework.
        var got = 0
        var lastIndex: UInt32 = 0
        var receivedPts = Set<UInt64>()
        var timings: [PunktfunkConnection.HostTiming] = []
        let deadline = Date().addingTimeInterval(30)
        while got < 25 {
            XCTAssertLessThan(Date(), deadline, "timed out after \(got) frames")
            while let t = try conn.nextHostTiming(timeoutMs: 0) { timings.append(t) }
            guard let au = try conn.nextAU(timeoutMs: 2000) else { continue }
            let idx = au.data.prefix(4).reversed().reduce(UInt32(0)) { ($0 << 8) | UInt32($1) }
            for (i, byte) in au.data.enumerated().dropFirst(4) {
                let expected = UInt8(truncatingIfNeeded: idx) &+ UInt8(truncatingIfNeeded: i)
                if byte != expected {
                    XCTFail("frame \(idx) corrupt at offset \(i)")
                    break
                }
            }
            XCTAssertGreaterThan(au.ptsNs, 0)
            receivedPts.insert(au.ptsNs)
            lastIndex = idx
            got += 1
        }
        XCTAssertGreaterThanOrEqual(lastIndex, 24)
        // Belt-and-braces: the last frame's timing lands just after its AU — give it a bounded
        // grace drain (the stream keeps running, so this must not loop on fresh timings).
        var grace = 0
        while grace < 64, !timings.contains(where: { receivedPts.contains($0.ptsNs) }),
              let t = try conn.nextHostTiming(timeoutMs: 100) {
            timings.append(t)
            grace += 1
        }
        XCTAssertTrue(
            timings.contains { receivedPts.contains($0.ptsNs) },
            "no 0xCF host timing matched a received AU's pts (got \(timings.count) timings)")

        // Input goes the other way (enqueue-only; the host logs the count on close) —
        // including the touch kinds, gamepad events, the rich-input plane (DualSense
        // touchpad/motion), and the mic uplink plane (the synthetic host counts the
        // datagrams; injection/decoding are Linux-side concerns).
        conn.send(.mouseMove(dx: 1, dy: 2))
        conn.send(.key(0x41, down: true))
        conn.send(.key(0x41, down: false))
        conn.send(.touchDown(id: 0, x: 100, y: 200, surfaceWidth: 1280, surfaceHeight: 720))
        conn.send(.touchMove(id: 0, x: 110, y: 210, surfaceWidth: 1280, surfaceHeight: 720))
        conn.send(.touchUp(id: 0))
        conn.send(.gamepadButton(GamepadWire.a, down: true, pad: 0))
        conn.send(.gamepadButton(GamepadWire.a, down: false, pad: 0))
        conn.send(.gamepadAxis(GamepadWire.axisLSX, value: 12345, pad: 0))
        conn.send(.gamepadAxis(GamepadWire.axisRT, value: 200, pad: 0))
        conn.sendTouchpad(finger: 0, active: true, x: 32768, y: 16384)
        conn.sendTouchpad(finger: 0, active: false, x: 0, y: 0)
        conn.sendMotion(gyro: (100, -100, 0), accel: (0, 0, 10000))
        conn.sendMic(Data([0xFC, 0xFF, 0xFE]), seq: 0, ptsNs: 1)  // tiny opus-ish frame
        conn.sendMic(Data(), seq: 1, ptsNs: 2)  // DTX silence frame

        // The synthetic host (PUNKTFUNK_TEST_FEEDBACK=1, set by test-loopback.sh) scripts
        // one feedback burst on the host→client planes — drain both and verify, end to
        // end through the xcframework: rumble (0xCA) + the three hidout kinds (0xCD).
        if ProcessInfo.processInfo.environment["PUNKTFUNK_TEST_FEEDBACK"] == "1" {
            var rumble: (pad: UInt16, low: UInt16, high: UInt16, ttlMs: UInt32)?
            var hidout: [PunktfunkConnection.HidOutputEvent] = []
            let feedbackDeadline = Date().addingTimeInterval(10)
            while (rumble == nil || hidout.count < 3), Date() < feedbackDeadline {
                if rumble == nil, let r = try conn.nextRumble2(timeoutMs: 100) { rumble = r }
                if let ev = try conn.nextHidOutput(timeoutMs: 100) { hidout.append(ev) }
            }
            XCTAssertEqual(rumble?.pad, 0)
            XCTAssertEqual(rumble?.low, 0x4000)
            XCTAssertEqual(rumble?.high, 0x8000)
            // The synthetic host emits a self-terminating rumble (400 ms TTL) — assert the TTL
            // survived the full wire → C ABI → Swift path, not just the level.
            XCTAssertEqual(rumble?.ttlMs, 400)
            XCTAssertTrue(
                hidout.contains(.led(pad: 0, r: 10, g: 20, b: 30)),
                "missing the scripted lightbar event: \(hidout)")
            XCTAssertTrue(
                hidout.contains(.playerLEDs(pad: 0, bits: 0b00100)),
                "missing the scripted player-LED event: \(hidout)")
            XCTAssertTrue(
                hidout.contains(.triggerEffect(
                    pad: 0, which: 1, effect: [0x21, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10])),
                "missing the scripted trigger event: \(hidout)")
        }

        // Speed test against the synthetic host: a short 20 Mbps burst over the real
        // data plane. Probe filler is diverted from the frame queue (the 25-frame
        // verification above stays byte-exact), the host's end-of-burst report flips
        // `done`, and the measurement carries real numbers.
        conn.startSpeedTest(targetKbps: 20_000, durationMs: 500)
        var probe: PunktfunkConnection.ProbeResult?
        let probeDeadline = Date().addingTimeInterval(10)
        while Date() < probeDeadline {
            if let r = conn.probeResult(), r.done {
                probe = r
                break
            }
            Thread.sleep(forTimeInterval: 0.1)
        }
        let result = try XCTUnwrap(probe, "the probe never completed")
        XCTAssertGreaterThan(result.recvBytes, 0)
        XCTAssertGreaterThan(result.hostBytes, 0)
        XCTAssertGreaterThan(result.throughputKbps, 0)
        XCTAssertGreaterThan(result.elapsedMs, 0)
        XCTAssertGreaterThanOrEqual(result.lossPct, 0)

        conn.close()
        XCTAssertThrowsError(try conn.nextAU(timeoutMs: 10)) { error in
            guard case PunktfunkClientError.closed = error else {
                return XCTFail("expected .closed, got \(error)")
            }
        }
        XCTAssertNil(conn.probeResult())
    }

    func testConnectFailureThrows() {
        // Nothing listens on this port; connect must fail within its timeout, not hang.
        XCTAssertThrowsError(
            try PunktfunkConnection(
                host: "127.0.0.1", port: 9, width: 640, height: 480, refreshHz: 30,
                timeoutMs: 2000))
    }

    /// The PIN pairing ceremony + the --require-pairing gate through the Swift wrapper:
    /// no session while unpaired, the single wrong-PIN online guess, the real ceremony, and a
    /// paired + pinned session. Driven by test-loopback.sh, which arms TWO --require-pairing
    /// hosts and parses their random PINs out of the logs: a pairing attempt — right or wrong —
    /// consumes the host's one-shot arming window (SPAKE2's "one online guess"), so the wrong-PIN
    /// assertion burns the GUESS host's window and the real ceremony runs against the PAIRING
    /// host's untouched one.
    func testPairingCeremonyAndRequirePairingGate() throws {
        let env = ProcessInfo.processInfo.environment
        guard let portStr = env["PUNKTFUNK_PAIRING_PORT"], let port = UInt16(portStr),
              let pin = env["PUNKTFUNK_PAIRING_PIN"],
              let guessPortStr = env["PUNKTFUNK_GUESS_PORT"], let guessPort = UInt16(guessPortStr),
              let guessPin = env["PUNKTFUNK_GUESS_PIN"]
        else {
            throw XCTSkip("needs armed punktfunk1-hosts — use clients/apple/test-loopback.sh")
        }

        let identity = try generateIdentity()

        // 1. Unpaired clients don't get sessions from a require-pairing host. The host PARKS the
        //    identified knock for delegated console approval rather than rejecting it outright —
        //    nobody approves here, so the connect times out client-side. Either way: no session
        //    while unpaired.
        XCTAssertThrowsError(
            try PunktfunkConnection(
                host: "127.0.0.1", port: port, width: 1280, height: 720, refreshHz: 60,
                identity: identity, timeoutMs: 5000),
            "unpaired client must not get a session")

        // 2. A wrong PIN is exactly one failed online guess — distinguishable from transport
        //    errors so the UI can say "try again". The attempt consumes the GUESS host's arming
        //    window (that is the point of the one-guess design), which is why it gets its own host.
        XCTAssertThrowsError(
            try pair(
                host: "127.0.0.1", port: guessPort, identity: identity,
                pin: guessPin == "0000" ? "9999" : "0000", name: "wrong-pin", timeoutMs: 5000)
        ) { error in
            guard case PunktfunkClientError.wrongPIN = error else {
                return XCTFail("expected .wrongPIN, got \(error)")
            }
        }

        // 3. The real ceremony — the PAIRING host's first attempt, so neither its one-shot window
        //    nor the per-host pairing cooldown has been touched.
        let fingerprint = try pair(
            host: "127.0.0.1", port: port, identity: identity,
            pin: pin, name: "loopback-test", timeoutMs: 5000)
        XCTAssertEqual(fingerprint.count, 32)

        // 4. Paired + pinned: the same identity now gets a session, and the ceremony's
        //    fingerprint matches the certificate the host actually serves.
        let conn = try PunktfunkConnection(
            host: "127.0.0.1", port: port, width: 1280, height: 720, refreshHz: 60,
            pinSHA256: fingerprint, identity: identity, timeoutMs: 5000)
        XCTAssertEqual(conn.hostFingerprint, fingerprint)
        var got = 0
        let deadline = Date().addingTimeInterval(15)
        while got < 5, Date() < deadline {
            if try conn.nextAU(timeoutMs: 2000) != nil { got += 1 }
        }
        conn.close()
        XCTAssertGreaterThanOrEqual(got, 5, "paired session must stream")
    }
}
