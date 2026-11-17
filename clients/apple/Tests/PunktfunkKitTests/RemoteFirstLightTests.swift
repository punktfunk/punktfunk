// First light, headless: the full client pipeline against a REAL remote host — QUIC
// handshake over the LAN, NVENC HEVC AUs through FEC + AES-GCM, AnnexB conversion, and a
// real VTDecompressionSession turning them into pixels. Everything the GUI does except
// putting the layer on glass.
//
// Run (host side, on the Linux box):
//   PUNKTFUNK_COMPOSITOR=gamescope PUNKTFUNK_GAMESCOPE_APP=vkcube PUNKTFUNK_ZEROCOPY=1 \
//   punktfunk-host punktfunk1-host --source virtual --seconds 120
// Then here:
//   PUNKTFUNK_REMOTE_HOST=192.168.1.70 swift test --filter RemoteFirstLightTests

import AVFoundation
import CoreMedia
import VideoToolbox
import XCTest
@testable import PunktfunkKit

final class RemoteFirstLightTests: XCTestCase {
    /// The pairing ceremony over the real LAN, exactly as the app runs it: fresh identity,
    /// SPAKE2 with the host's arming PIN, then a pinned + identified session. Needs the host
    /// armed (`punktfunk1-host` arms at startup) and its printed PIN in PUNKTFUNK_REMOTE_PIN.
    /// Heads-up: every run durably adds one throwaway "remote-test" identity to the host's
    /// ~/.config/punktfunk/punktfunk1-paired.json — prune those entries at will.
    func testRemotePairingThenPinnedStream() throws {
        let env = ProcessInfo.processInfo.environment
        guard let host = env["PUNKTFUNK_REMOTE_HOST"], let pin = env["PUNKTFUNK_REMOTE_PIN"]
        else {
            throw XCTSkip("set PUNKTFUNK_REMOTE_HOST + PUNKTFUNK_REMOTE_PIN "
                + "(host armed with --allow-pairing)")
        }
        let port = env["PUNKTFUNK_REMOTE_PORT"].flatMap(UInt16.init) ?? 9777

        let identity = try generateIdentity()
        let fingerprint = try pair(
            host: host, port: port, identity: identity, pin: pin, name: "remote-test")
        XCTAssertEqual(fingerprint.count, 32)

        let conn = try PunktfunkConnection(
            host: host, port: port, width: 1280, height: 720, refreshHz: 60,
            pinSHA256: fingerprint, identity: identity)
        defer { conn.close() }
        XCTAssertEqual(conn.hostFingerprint, fingerprint)
        var got = 0
        let deadline = Date().addingTimeInterval(20)
        while got < 10, Date() < deadline {
            if try conn.nextAU(timeoutMs: 2000) != nil { got += 1 }
        }
        XCTAssertGreaterThanOrEqual(got, 10, "paired + pinned session must stream")
    }

    /// Audio both ways against the real host: drain the Opus plane and decode it to PCM
    /// (host → speaker path minus the speaker), and uplink an encoded tone (mic path
    /// minus the mic) — the host's mic pump logs "virtual mic ready" once its source is open.
    func testRemoteAudioBothDirections() throws {
        let env = ProcessInfo.processInfo.environment
        guard let host = env["PUNKTFUNK_REMOTE_HOST"] else {
            throw XCTSkip("set PUNKTFUNK_REMOTE_HOST (and start punktfunk1-host --source virtual there)")
        }
        let port = env["PUNKTFUNK_REMOTE_PORT"].flatMap(UInt16.init) ?? 9777

        let conn = try PunktfunkConnection(
            host: host, port: port, width: 1280, height: 720, refreshHz: 60)
        defer { conn.close() }

        // Mic uplink: 2 s of 440 Hz mono tone (the host's mic pump opens its virtual
        // source at host start — check its log).
        let encoder = try OpusEncoder()
        let chunk = AVAudioPCMBuffer(
            pcmFormat: encoder.pcmFormat, frameCapacity: encoder.framesPerPacket)!
        var phase: Float = 0
        let step = 2 * Float.pi * 440 / 48_000
        var seq: UInt32 = 0
        let chunks = 2 * 48_000 / Int(encoder.framesPerPacket)
        let packetNs = UInt64(encoder.framesPerPacket) * 1_000_000_000 / 48_000
        for _ in 0..<chunks {
            chunk.frameLength = encoder.framesPerPacket
            let p = chunk.floatChannelData![0] // mono: one plane
            for f in 0..<Int(encoder.framesPerPacket) {
                p[f] = sin(phase) * 0.25
                phase += step
            }
            for packet in try encoder.encode(chunk) {
                conn.sendMic(packet, seq: seq, ptsNs: UInt64(seq) * packetNs)
                seq &+= 1
            }
        }
        XCTAssertGreaterThanOrEqual(
            seq, UInt32(chunks - 5), "mic encoder must emit ~one packet per chunk")

        // Downlink: pull host audio packets and decode them (the host streams its sink
        // monitor — silence still produces packets).
        let decoder = try OpusDecoder(framesPerPacket: 240)
        let pcm = AVAudioPCMBuffer(pcmFormat: decoder.pcmFormat, frameCapacity: 5760)!
        var packets = 0
        var decodedFrames = 0
        let deadline = Date().addingTimeInterval(10)
        while packets < 100, Date() < deadline {
            guard let pkt = try conn.nextAudio(timeoutMs: 1000) else { continue }
            packets += 1
            decodedFrames += Int(try decoder.decode(pkt.data, into: pcm))
        }
        XCTAssertGreaterThanOrEqual(packets, 100, "host audio plane must deliver")
        // 100 packets × 5 ms × 48 kHz = 24000 frames.
        XCTAssertGreaterThan(decodedFrames, 20_000, "host packets must decode to PCM")
    }

    func testRemoteStreamDecodesToPixels() throws {
        let env = ProcessInfo.processInfo.environment
        guard let host = env["PUNKTFUNK_REMOTE_HOST"] else {
            throw XCTSkip("set PUNKTFUNK_REMOTE_HOST (and start punktfunk1-host --source virtual there)")
        }
        let port = env["PUNKTFUNK_REMOTE_PORT"].flatMap(UInt16.init) ?? 9777
        // PUNKTFUNK_REMOTE_COMPOSITOR=kwin|gamescope|… asks the host for a specific
        // backend (verify in its log: "punktfunk/1 virtual display compositor=…").
        let compositor = env["PUNKTFUNK_REMOTE_COMPOSITOR"]
            .flatMap(PunktfunkConnection.Compositor.init(name:)) ?? .auto
        let width: UInt32 = 1280
        let height: UInt32 = 720

        let conn = try PunktfunkConnection(
            host: host, port: port, width: width, height: height, refreshHz: 60,
            compositor: compositor)
        defer { conn.close() }
        XCTAssertEqual(conn.width, width)
        XCTAssertEqual(conn.height, height)

        var format: CMVideoFormatDescription?
        var decoder: VTDecompressionSession?
        defer { decoder.map { VTDecompressionSessionInvalidate($0) } }
        var received = 0
        var decoded = 0
        var firstPtsNs: UInt64 = 0
        var lastPtsNs: UInt64 = 0
        let deadline = Date().addingTimeInterval(30)

        while decoded < 60, Date() < deadline {
            guard let au = try conn.nextAU(timeoutMs: 2000) else { continue }
            received += 1
            if firstPtsNs == 0 { firstPtsNs = au.ptsNs }
            lastPtsNs = au.ptsNs

            if let f = AnnexB.formatDescription(fromIDR: au.data, codec: .hevc) {
                format = f
                if decoder == nil {
                    let dims = CMVideoFormatDescriptionGetDimensions(f)
                    XCTAssertEqual(UInt32(dims.width), width)
                    XCTAssertEqual(UInt32(dims.height), height)
                    var session: VTDecompressionSession?
                    XCTAssertEqual(
                        VTDecompressionSessionCreate(
                            allocator: nil, formatDescription: f, decoderSpecification: nil,
                            imageBufferAttributes: nil, outputCallback: nil,
                            decompressionSessionOut: &session),
                        noErr)
                    decoder = session
                }
            }
            guard let f = format, let dec = decoder,
                  let sample = AnnexB.sampleBuffer(au: au, format: f, codec: .hevc)
            else { continue }

            var gotPixels = false
            VTDecompressionSessionDecodeFrame(
                dec, sampleBuffer: sample, flags: [], infoFlagsOut: nil
            ) { status, _, imageBuffer, _, _ in
                gotPixels = status == noErr && imageBuffer != nil
            }
            if gotPixels { decoded += 1 }
        }

        XCTAssertGreaterThanOrEqual(decoded, 60, "decoded \(decoded)/\(received) received AUs")
        // The host stamps pts with its capture wall clock — 60 frames should span ~1 s.
        let spanMs = Double(lastPtsNs &- firstPtsNs) / 1_000_000
        print("first light: \(decoded) frames decoded, \(received) received, pts span \(Int(spanMs)) ms")
    }
}
