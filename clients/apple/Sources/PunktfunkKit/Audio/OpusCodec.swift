// The mic uplink's Opus encoder, through CoreAudio's built-in codec (kAudioFormatOpus): 48 kHz
// MONO packets, one microphone bus (the host's decoder upmixes). AVAudioConverter emits them into
// an AVAudioCompressedBuffer with explicit packet descriptions.
//
// ⚠ **The 48 kHz in this file is CORRECT and must not be "fixed" for hi-res.** Opus is 48 kHz by
// construction, which is why the lossless plane exists (design/hi-res-audio.md §2), and the uplink
// stays 48 kHz Opus whatever the playback plane negotiated (§3). Host audio never comes through
// here: core decodes both planes and hands back f32 through `nextAudioPcm`.
//
// Single-threaded by contract: the mic uplink's worker owns its encoder.

import AVFoundation

enum OpusCodecError: Error {
    /// CoreAudio rejected the Opus stream format or had no converter for it.
    case unavailable
    case convertFailed(String)
}

/// The compressed side: raw Opus, `framesPerPacket` nominal samples per packet at 48 kHz,
/// `channels` wide.
func opusFormat(framesPerPacket: UInt32, channels: UInt32) -> AVAudioFormat? {
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
