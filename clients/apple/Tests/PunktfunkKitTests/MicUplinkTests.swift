// The mic uplink's capture half: the ring between the IO thread and the worker, and, where
// the machine has an input and the permission to use it, the cadence a sink node delivers at.

#if !os(tvOS)
import AVFoundation
import XCTest

@testable import PunktfunkKit

final class MicUplinkTests: XCTestCase {
    private func read(_ ring: MicRing, limit: Int) -> [Float] {
        var out = [Float](repeating: -1, count: limit)
        let taken = out.withUnsafeMutableBufferPointer {
            ring.read(into: $0.baseAddress!, limit: limit)
        }
        return Array(out.prefix(taken))
    }

    func testRingReturnsSamplesInOrderAcrossTheWrap() {
        let ring = MicRing(capacity: 8)
        ring.write([1, 2, 3, 4, 5, 6], count: 6)
        XCTAssertEqual(read(ring, limit: 4), [1, 2, 3, 4])
        // Head at 4, two queued: these five wrap past the end of the store.
        ring.write([7, 8, 9, 10, 11], count: 5)
        XCTAssertEqual(read(ring, limit: 16), [5, 6, 7, 8, 9, 10, 11])
        XCTAssertEqual(read(ring, limit: 16), [])
    }

    func testFullRingKeepsTheNewest() {
        let ring = MicRing(capacity: 4)
        ring.write([1, 2, 3], count: 3)
        ring.write([4, 5, 6], count: 3)
        XCTAssertEqual(read(ring, limit: 8), [3, 4, 5, 6])
        // One write larger than the ring keeps its own tail.
        ring.write([1, 2, 3, 4, 5, 6, 7], count: 7)
        XCTAssertEqual(read(ring, limit: 8), [4, 5, 6, 7])
    }

    /// Against the default input, when there is one. A tap delivers 100 ms at a time, 4800
    /// frames at 48 kHz; the sink node has to deliver the IO quantum.
    func testCaptureArrivesInQuantumSizedBatches() throws {
        guard AVCaptureDevice.authorizationStatus(for: .audio) == .authorized else {
            throw XCTSkip("needs microphone permission")
        }
        let engine = AVAudioEngine()
        let input = engine.inputNode
        engine.prepare()
        let hardware = input.inputFormat(forBus: 0)
        guard hardware.sampleRate > 0, hardware.channelCount > 0 else {
            throw XCTSkip("needs an input device")
        }
        let uplink = MicUplink(plan: MicChannelPlan(pick: 0, loopback: []))
        engine.attach(uplink.node)
        engine.connect(input, to: uplink.node, format: nil)
        engine.prepare()
        let rate = input.outputFormat(forBus: 0).sampleRate

        let tally = Tally()
        uplink.start { _, frames in tally.note(frames) }
        try engine.start()
        Thread.sleep(forTimeInterval: 1)
        engine.stop()
        uplink.stop()

        let (batches, frames, largest) = tally.totals
        // A second of audio, give or take the start: most of it must have arrived.
        XCTAssertGreaterThan(Double(frames), rate * 0.7)
        XCTAssertLessThan(Double(frames), rate * 1.2)
        // 25 ms is past any IO quantum in use, and a quarter of a tap's batch.
        XCTAssertLessThan(Double(largest), rate * 0.025, "\(batches) batches, largest \(largest)")
    }

    #if os(macOS)
    /// The whole uplink through a real session, on the split path: the voice processor is a
    /// second topology, and a test process that starts it can wedge the audio server.
    /// Driven by clients/apple/test-loopback.sh.
    @MainActor
    func testSessionCaptureStartsOnTheSplitPath() throws {
        guard let portStr = ProcessInfo.processInfo.environment["PUNKTFUNK_LOOPBACK_PORT"],
              let port = UInt16(portStr)
        else {
            throw XCTSkip("needs a running punktfunk1-host — use clients/apple/test-loopback.sh")
        }
        guard AVCaptureDevice.authorizationStatus(for: .audio) == .authorized,
              AudioDevices.defaultInputDevice() != nil
        else {
            throw XCTSkip("needs an input device and microphone permission")
        }
        let conn = try PunktfunkConnection(
            host: "127.0.0.1", port: port, width: 1280, height: 720, refreshHz: 60,
            bitrateKbps: 50_000)
        let audio = SessionAudio(connection: conn)
        audio.start(
            speakerUID: "", micUID: "", micChannel: 0, micEnabled: true, echoCancel: false)
        defer {
            audio.stop()
            conn.close()
        }

        let marker = "mic capture: first batch is "
        var batch: Int?
        let deadline = Date().addingTimeInterval(5)
        while batch == nil, Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
            let log = ClientLogRing.render(header: "")
            if let at = log.range(of: marker, options: .backwards) {
                batch = Int(log[at.upperBound...].prefix { $0.isNumber })
            }
        }
        let frames = try XCTUnwrap(batch, "the uplink delivered no audio")
        XCTAssertLessThan(frames, 4800, "the capture is batching like a tap")
    }
    #endif

    // Whole packets go out in arrival order, numbered without gaps, whatever the batch sizes;
    // the remainder waits for the next batch.
    func testThePacketizerCutsWholePacketsAcrossBatches() {
        var packetizer = MicPacketizer(packetFrames: 4)
        var sent: [(first: UInt8, seq: UInt32)] = []
        let samples = (0..<11).map(Float.init)
        for range in [0..<3, 3..<9, 9..<11] {
            Array(samples[range]).withUnsafeBufferPointer { batch in
                packetizer.push(batch, encode: { packet in
                    XCTAssertEqual(packet.count, 4)
                    return [Data([UInt8(packet[0])]), Data([UInt8(packet[3])])]
                }, send: { packet, seq in sent.append((packet[0], seq)) })
            }
        }
        XCTAssertEqual(sent.map(\.first), [0, 3, 4, 7])
        XCTAssertEqual(sent.map(\.seq), [0, 1, 2, 3])
    }
}

private final class Tally: @unchecked Sendable {
    private let lock = NSLock()
    private var batches = 0
    private var frames = 0
    private var largest = 0

    func note(_ count: Int) {
        lock.withLock {
            batches += 1
            frames += count
            largest = max(largest, count)
        }
    }

    var totals: (batches: Int, frames: Int, largest: Int) {
        lock.withLock { (batches, frames, largest) }
    }
}
#endif
