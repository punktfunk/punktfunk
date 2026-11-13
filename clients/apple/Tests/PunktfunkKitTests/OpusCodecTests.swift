// The mic uplink's Opus encoder through CoreAudio (kAudioFormatOpus): a real encode → decode
// round trip, with CoreAudio's own decoder as the oracle. If AVAudioConverter can't handle raw
// Opus packets, fail HERE, not in the app.

import AVFoundation
import XCTest

@testable import PunktfunkKit

final class OpusCodecTests: XCTestCase {
    /// Encode a 440 Hz mono tone (the uplink's shape), decode it back through a stereo
    /// decoder (Opus upmixes mono packets), and
    /// require the result to be recognizably the same signal (Opus is lossy — check
    /// correlation, not bytes).
    func testEncodeDecodeRoundTripPreservesTone() throws {
        let encoder = try OpusEncoder()
        let decoder = try OpusDecoder(framesPerPacket: UInt32(encoder.framesPerPacket))
        let pcmFormat = encoder.pcmFormat

        let frames = encoder.framesPerPacket
        var packets: [Data] = []
        var phase: Float = 0
        let step = 2 * Float.pi * 440 / 48_000

        // 1 s of tone, whatever packet duration the encoder chose (10 ms → 100 chunks).
        let chunks = Int(48_000 / frames)
        for _ in 0..<chunks {
            let buf = AVAudioPCMBuffer(pcmFormat: pcmFormat, frameCapacity: frames)!
            buf.frameLength = frames
            let p = buf.floatChannelData![0] // mono: one plane
            for f in 0..<Int(frames) {
                p[f] = sin(phase) * 0.5
                phase += step
            }
            packets.append(contentsOf: try encoder.encode(buf))
        }
        XCTAssertGreaterThanOrEqual(
            packets.count, chunks - 5, "encoder must emit ~one packet per buffer")
        XCTAssertTrue(packets.allSatisfy { !$0.isEmpty })

        var decoded: [Float] = []
        let out = AVAudioPCMBuffer(pcmFormat: decoder.pcmFormat, frameCapacity: 5760)!
        for packet in packets {
            let n = try decoder.decode(packet, into: out)
            let p = out.floatChannelData![0]
            for f in 0..<Int(n) {
                decoded.append(p[f * 2]) // left channel
            }
        }
        XCTAssertGreaterThan(decoded.count, 40_000, "~1 s of 48 kHz audio back out")

        // The decoded signal must contain a strong 440 Hz component: correlate against
        // quadrature reference tones (phase-agnostic), skipping the codec warm-up.
        let skip = 4800
        var inPhase: Float = 0
        var quadrature: Float = 0
        var energy: Float = 0
        for (i, s) in decoded[skip...].enumerated() {
            let t = Float(i) * step
            inPhase += s * sin(t)
            quadrature += s * cos(t)
            energy += s * s
        }
        let n = Float(decoded.count - skip)
        let correlation = (inPhase * inPhase + quadrature * quadrature).squareRoot() / n
        let rms = (energy / n).squareRoot()
        XCTAssertGreaterThan(rms, 0.2, "decoded audio is not silence")
        // A clean sine at amplitude a yields correlation a/2 (≈0.25 here); noise ≈ 0.
        XCTAssertGreaterThan(correlation, 0.15, "440 Hz tone must survive the round trip")
    }
}

/// CoreAudio's Opus decoder, 48 kHz stereo float32 interleaved out: the oracle for the encoder's
/// packets here and for a host's raw Opus plane in `RemoteFirstLightTests`. Opus upmixes a mono
/// packet into both channels.
final class OpusDecoder {
    private let converter: AVAudioConverter
    private let inBuf: AVAudioCompressedBuffer
    let pcmFormat: AVAudioFormat

    init(framesPerPacket: UInt32) throws {
        guard let pcm = AVAudioFormat(
                commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2,
                interleaved: true),
              let opus = opusFormat(framesPerPacket: framesPerPacket, channels: 2),
              let converter = AVAudioConverter(from: opus, to: pcm)
        else { throw OpusCodecError.unavailable }
        self.converter = converter
        self.pcmFormat = pcm
        inBuf = AVAudioCompressedBuffer(format: opus, packetCapacity: 1, maximumPacketSize: 1500)
    }

    /// Decode one packet into `out` (in `pcmFormat`); returns the frames written.
    func decode(_ packet: Data, into out: AVAudioPCMBuffer) throws -> AVAudioFrameCount {
        guard !packet.isEmpty else { return 0 }
        guard packet.count <= Int(inBuf.maximumPacketSize) else {
            throw OpusCodecError.convertFailed("packet larger than maximumPacketSize")
        }
        packet.withUnsafeBytes { raw in
            inBuf.data.copyMemory(from: raw.baseAddress!, byteCount: raw.count)
        }
        inBuf.byteLength = UInt32(packet.count)
        inBuf.packetCount = 1
        inBuf.packetDescriptions![0] = AudioStreamPacketDescription(
            mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: UInt32(packet.count))
        out.frameLength = 0
        var fed = false
        var convError: NSError?
        let status = converter.convert(to: out, error: &convError) { [inBuf] _, outStatus in
            if fed {
                outStatus.pointee = .noDataNow
                return nil
            }
            fed = true
            outStatus.pointee = .haveData
            return inBuf
        }
        if status == .error {
            throw OpusCodecError.convertFailed(convError?.localizedDescription ?? "decode")
        }
        return out.frameLength
    }
}
