// Opus ⇄ PCM through CoreAudio's built-in codec (kAudioFormatOpus, macOS 10.13+ / iOS
// 11+) — no bundled libopus. The host's audio plane is raw Opus packets (48 kHz stereo,
// one frame per packet); the mic uplink is 48 kHz MONO packets (one microphone bus —
// the host's decoder upmixes, so duplicating it into a second channel only cost bits).
// AVAudioConverter handles both as single-packet AVAudioCompressedBuffers with explicit
// packet descriptions.
//
// ⚠ **The 48 kHz in this file is CORRECT and must not be "fixed" for hi-res.** Opus is 48 kHz by
// construction — it has no other internal rate — which is the whole reason the lossless plane
// exists (design/hi-res-audio.md §2). Nothing on that plane comes through here: `0xD3` frames are
// unpacked in core and arrive as f32 via `nextAudioPcm`, and the only surviving user of this file
// in the app is `OpusEncoder` on the mic UPLINK, which §3 keeps at 48 kHz deliberately (voice,
// 10 ms frames, unchanged). `OpusDecoder` is exercised by the codec tests; the playback path stopped
// using it when decoding moved into core.
//
// Both classes are single-threaded by contract (one per direction, owned by their
// drain/capture pipelines).

import AVFoundation

enum OpusCodecError: Error {
    /// CoreAudio rejected the Opus stream format or had no converter for it.
    case unavailable
    case convertFailed(String)
}

/// 48 kHz stereo float32 interleaved — the decoder's PCM side (the host plane's shape).
func opusPCMFormat() -> AVAudioFormat? {
    AVAudioFormat(
        commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 2, interleaved: true)
}

/// The compressed side: raw Opus, `framesPerPacket` nominal samples per packet at 48 kHz
/// (240 = the host's 5 ms audio plane; 480 = the 10 ms packets the encoder emits) and
/// `channels` (2 = the host plane, 1 = the mic uplink).
private func opusFormat(framesPerPacket: UInt32, channels: UInt32) -> AVAudioFormat? {
    var desc = AudioStreamBasicDescription(
        mSampleRate: 48_000,
        mFormatID: kAudioFormatOpus,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: framesPerPacket,
        mBytesPerFrame: 0,
        mChannelsPerFrame: channels,
        mBitsPerChannel: 0,
        mReserved: 0)
    return AVAudioFormat(streamDescription: &desc)
}

final class OpusDecoder {
    private let converter: AVAudioConverter
    private let inBuf: AVAudioCompressedBuffer
    private let opus: AVAudioFormat
    let pcmFormat: AVAudioFormat

    /// `framesPerPacket`: the sender's packet duration in samples (host audio = 240).
    init(framesPerPacket: UInt32) throws {
        guard let pcm = opusPCMFormat(),
              let opus = opusFormat(framesPerPacket: framesPerPacket, channels: 2),
              let converter = AVAudioConverter(from: opus, to: pcm)
        else { throw OpusCodecError.unavailable }
        self.converter = converter
        self.opus = opus
        self.pcmFormat = pcm
        inBuf = AVAudioCompressedBuffer(
            format: opus, packetCapacity: 1, maximumPacketSize: 1500)
    }

    /// Decode one Opus packet into `out` (whose format must be `pcmFormat`); returns the
    /// number of frames written. Empty packets (DTX) decode to 0 frames.
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

final class OpusEncoder {
    /// The encoder's packet duration in samples: 480 = 10 ms, which CoreAudio emits as one 10 ms
    /// CELT packet (TOC config 30) per 480-frame chunk. 960 is the fallback for a codec that
    /// refuses 480; the host's mic service decodes any Opus frame size up to 120 ms.
    let framesPerPacket: AVAudioFrameCount

    /// 48 kHz MONO float32 interleaved — the uplink carries one microphone bus.
    let pcmFormat: AVAudioFormat

    private let converter: AVAudioConverter
    private let outBuf: AVAudioCompressedBuffer

    init() throws {
        guard let pcm = AVAudioFormat(
            commonFormat: .pcmFormatFloat32, sampleRate: 48_000, channels: 1,
            interleaved: true)
        else { throw OpusCodecError.unavailable }
        var made: (converter: AVAudioConverter, fpp: AVAudioFrameCount)?
        for fpp: AVAudioFrameCount in [480, 960] {
            if let opus = opusFormat(framesPerPacket: UInt32(fpp), channels: 1),
               let converter = AVAudioConverter(from: pcm, to: opus) {
                made = (converter, fpp)
                break
            }
        }
        guard let made else { throw OpusCodecError.unavailable }
        // 48 kbps: transparent for mono voice.
        made.converter.bitRate = 48_000
        converter = made.converter
        framesPerPacket = made.fpp
        pcmFormat = pcm
        outBuf = AVAudioCompressedBuffer(
            format: made.converter.outputFormat, packetCapacity: 4, maximumPacketSize: 1500)
    }

    /// Encode exactly `framesPerPacket` frames of `pcmFormat` audio; returns the encoded
    /// packets (normally one).
    func encode(_ pcm: AVAudioPCMBuffer) throws -> [Data] {
        outBuf.byteLength = 0
        outBuf.packetCount = 0
        var fed = false
        var convError: NSError?
        let status = converter.convert(to: outBuf, error: &convError) { _, outStatus in
            if fed {
                outStatus.pointee = .noDataNow
                return nil
            }
            fed = true
            outStatus.pointee = .haveData
            return pcm
        }
        if status == .error {
            throw OpusCodecError.convertFailed(convError?.localizedDescription ?? "encode")
        }
        guard let descs = outBuf.packetDescriptions else { return [] }
        return (0..<Int(outBuf.packetCount)).map { i in
            let d = descs[i]
            return Data(
                bytes: outBuf.data.advanced(by: Int(d.mStartOffset)),
                count: Int(d.mDataByteSize))
        }
    }
}
