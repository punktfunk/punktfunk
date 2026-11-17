// One synchronous VideoToolbox decode of a single Annex-B keyframe: the 4:4:4 capability probe
// and the colour golden tests both need a picture out of one AU, with no pump around it.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

enum VTOneShot {
    /// Decode `annexB` (a keyframe with in-band parameter sets) to a `pixelFormat` buffer, or nil
    /// on any failure. `requireHardware` refuses a software decoder.
    ///
    /// Synchronous: without `._EnableAsynchronousDecompression` the output callback finishes
    /// before DecodeFrame returns, on a VideoToolbox thread. An async decode plus a semaphore wait
    /// blocks a userInteractive caller on VideoToolbox's QoS-less callback thread (a priority
    /// inversion).
    static func decode(
        annexB: Data, codec: VideoCodec, pixelFormat: OSType, requireHardware: Bool
    ) -> CVPixelBuffer? {
        guard let format = AnnexB.formatDescription(fromIDR: annexB, codec: codec) else {
            return nil
        }
        let spec: [CFString: Any]? = requireHardware
            ? [kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder: true] : nil
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: pixelFormat,
            kCVPixelBufferMetalCompatibilityKey: true,
        ]
        var session: VTDecompressionSession?
        let created = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault, formatDescription: format,
            decoderSpecification: spec as CFDictionary?,
            imageBufferAttributes: attrs as CFDictionary,
            outputCallback: nil, decompressionSessionOut: &session)
        guard created == noErr, let session else { return nil }
        defer { VTDecompressionSessionInvalidate(session) }

        let au = AccessUnit(data: annexB, ptsNs: 0, frameIndex: 0, flags: 0, receivedNs: 0)
        guard let sample = AnnexB.sampleBuffer(au: au, format: format, codec: codec) else {
            return nil
        }
        var produced: CVPixelBuffer?
        let status = VTDecompressionSessionDecodeFrame(
            session, sampleBuffer: sample, flags: [], infoFlagsOut: nil
        ) { status, _, imageBuffer, _, _ in
            if status == noErr { produced = imageBuffer }
        }
        return status == noErr ? produced : nil
    }
}
