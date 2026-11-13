// Stage-1's pump against the demo host: a restart's pump must never run beside the old one.

import AVFoundation
import XCTest
@testable import PunktfunkKit

final class StreamPumpTests: XCTestCase {
    private func connect(_ host: DemoHost) throws -> PunktfunkConnection {
        try PunktfunkConnection(
            host: "127.0.0.1", port: host.port, width: 1280, height: 720, refreshHz: 60,
            pinSHA256: host.fingerprint, videoCodecs: PunktfunkConnection.codecH264,
            launchID: "custom:aurora")
    }

    /// `stop` returns only once the pump thread is gone, even mid-iteration.
    func testStopWaitsForThePumpThread() throws {
        let host = try XCTUnwrap(DemoHost(titles: ["custom:aurora": "Aurora Drift"]))
        defer { host.stop() }
        let connection = try connect(host)
        defer { connection.close() }
        let first = LatestBox<Bool>()
        first.put(true)
        let inFrame = DispatchSemaphore(value: 0)
        let frameDone = DispatchSemaphore(value: 0)
        let pump = StreamPump()
        pump.start(
            connection: connection, layer: AVSampleBufferDisplayLayer(),
            onFrame: { _ in
                guard first.take() == true else { return }
                inFrame.signal()
                Thread.sleep(forTimeInterval: 0.2)
                frameDone.signal()
            },
            onSessionEnd: nil)
        XCTAssertEqual(inFrame.wait(timeout: .now() + 5), .success, "the demo host streams")
        pump.stop()
        XCTAssertEqual(
            frameDone.wait(timeout: .now()), .success,
            "stop returned while the pump thread was still in its iteration")
    }
}
