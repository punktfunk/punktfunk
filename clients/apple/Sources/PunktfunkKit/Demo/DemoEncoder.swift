// H.264 for the demo host: BGRA pixel buffers in, Annex-B access units out, SPS/PPS in-band
// on every IDR — the shape a real host puts on the wire.

import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

final class DemoEncoder {
    let width: Int
    let height: Int
    private let session: VTCompressionSession
    private let fps: Int32
    private let onAccessUnit: (Data, Bool) -> Void
    private var frameIndex: Int64 = 0

    init?(
        width: Int, height: Int, fps: Int, bitrateKbps: Int,
        onAccessUnit: @escaping (Data, Bool) -> Void
    ) {
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        var out: VTCompressionSession?
        let rc = VTCompressionSessionCreate(
            allocator: nil, width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_H264, encoderSpecification: nil,
            imageBufferAttributes: attrs as CFDictionary, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &out)
        guard rc == noErr, let session = out else { return nil }
        self.session = session
        self.width = width
        self.height = height
        self.fps = Int32(max(1, fps))
        self.onAccessUnit = onAccessUnit
        let set = { (key: CFString, value: CFTypeRef) in
            _ = VTSessionSetProperty(session, key: key, value: value)
        }
        set(kVTCompressionPropertyKey_RealTime, kCFBooleanTrue)
        // Decode order = display order, as the client's decoder expects from a host.
        set(kVTCompressionPropertyKey_AllowFrameReordering, kCFBooleanFalse)
        set(kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_High_AutoLevel)
        set(kVTCompressionPropertyKey_AverageBitRate, NSNumber(value: bitrateKbps * 1000))
        set(kVTCompressionPropertyKey_ExpectedFrameRate, NSNumber(value: fps))
        set(kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, NSNumber(value: 4))
        // BT.709, as every real host signals; untagged the client would guess.
        set(kVTCompressionPropertyKey_ColorPrimaries, kCMFormatDescriptionColorPrimaries_ITU_R_709_2)
        set(kVTCompressionPropertyKey_TransferFunction, kCMFormatDescriptionTransferFunction_ITU_R_709_2)
        set(kVTCompressionPropertyKey_YCbCrMatrix, kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2)
        VTCompressionSessionPrepareToEncodeFrames(session)
    }

    deinit {
        // Flushes pending output before returning, so no callback outlives the encoder.
        VTCompressionSessionCompleteFrames(session, untilPresentationTimeStamp: .invalid)
        VTCompressionSessionInvalidate(session)
    }

    /// A buffer from the encoder's own pool, sized and formatted for it.
    func makePixelBuffer() -> CVPixelBuffer? {
        guard let pool = VTCompressionSessionGetPixelBufferPool(session) else { return nil }
        var buffer: CVPixelBuffer?
        CVPixelBufferPoolCreatePixelBuffer(nil, pool, &buffer)
        return buffer
    }

    func encode(_ buffer: CVPixelBuffer, keyframe: Bool) {
        let pts = CMTime(value: frameIndex, timescale: fps)
        frameIndex += 1
        let props: CFDictionary? =
            keyframe ? [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue] as CFDictionary : nil
        let deliver = onAccessUnit
        VTCompressionSessionEncodeFrame(
            session, imageBuffer: buffer, presentationTimeStamp: pts,
            duration: CMTime(value: 1, timescale: fps), frameProperties: props, infoFlagsOut: nil
        ) { status, _, sample in
            guard status == noErr, let sample, let au = Self.annexB(sample) else { return }
            deliver(au.data, au.keyframe)
        }
    }

    /// AVCC sample → Annex-B AU; an IDR gets its SPS/PPS in front.
    static func annexB(_ sample: CMSampleBuffer) -> (data: Data, keyframe: Bool)? {
        guard let block = CMSampleBufferGetDataBuffer(sample),
              let format = CMSampleBufferGetFormatDescription(sample)
        else { return nil }
        let attachments =
            CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false)
            as? [[CFString: Any]]
        let keyframe = !(attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
        let startCode: [UInt8] = [0, 0, 0, 1]
        var au = Data()
        if keyframe {
            var count = 0
            guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: 0, parameterSetPointerOut: nil,
                parameterSetSizeOut: nil, parameterSetCountOut: &count,
                nalUnitHeaderLengthOut: nil) == noErr
            else { return nil }
            for i in 0..<count {
                var pointer: UnsafePointer<UInt8>?
                var size = 0
                guard CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format, parameterSetIndex: i, parameterSetPointerOut: &pointer,
                    parameterSetSizeOut: &size, parameterSetCountOut: nil,
                    nalUnitHeaderLengthOut: nil) == noErr, let pointer
                else { return nil }
                au.append(contentsOf: startCode)
                au.append(pointer, count: size)
            }
        }
        var avcc = Data(count: CMBlockBufferGetDataLength(block))
        let copied = avcc.withUnsafeMutableBytes { raw in
            CMBlockBufferCopyDataBytes(
                block, atOffset: 0, dataLength: raw.count, destination: raw.baseAddress!)
        }
        guard copied == noErr else { return nil }
        // 4-byte big-endian NAL lengths → start codes.
        var i = avcc.startIndex
        while i + 4 <= avcc.endIndex {
            let length = avcc[i..<i + 4].reduce(0) { ($0 << 8) | Int($1) }
            let body = i + 4
            guard body + length <= avcc.endIndex else { break }
            au.append(contentsOf: startCode)
            au.append(avcc[body..<body + length])
            i = body + length
        }
        return (au, keyframe)
    }
}
