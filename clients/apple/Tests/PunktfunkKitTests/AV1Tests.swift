// Real-bitstream tests for the AV1 OBU → CoreMedia plumbing (AV1.swift): the OBU walk, the
// sequence-header parse, the `av1C` format description, the sample repack, the VideoCodec
// dispatch — and, on AV1-hardware devices, a real VTDecompressionSession decode of the blob
// (the AV1 counterpart of VideoToolboxRoundTripTests; there is no VT AV1 *encoder*, so the
// bitstream is generated offline).
//
// Blobs: the first two temporal units of an SVT-AV1 clip — a keyframe TU (temporal delimiter +
// sequence header + frame) and a delta TU (temporal delimiter + frame), exactly the wire shape
// the punktfunk host emits. Generated with:
//   ffmpeg -f lavfi -i testsrc2=size=320x180:rate=30 -frames:v 2 \
//          -c:v libsvtav1 -preset 12 -crf 63 -g 30 -f obu out.obu
// then split on the temporal-delimiter OBUs. 320×180 clears the hardware decoder's
// minimum-dimension floor (see Probe444Blobs). Ground truth (ffprobe + a reference parse):
// Main profile (0), level_idx 0, tier 0, 8-bit, 4:2:0, no color description (unspecified),
// studio range, max frame 320×180, chroma sample position 0.

import CoreMedia
import PunktfunkCore
import VideoToolbox
import XCTest
@testable import PunktfunkKit

final class AV1Tests: XCTestCase {
    // MARK: - OBU walk

    func testOBUWalkKeyframe() {
        var seen: [(type: UInt8, payloadCount: Int)] = []
        var lastEnd = 0
        AV1.forEachOBU(in: Data(Self.keyframeTU)) { _, header, payload, type in
            XCTAssertEqual(header.lowerBound, lastEnd, "OBUs must be contiguous")
            XCTAssertEqual(header.upperBound, payload.lowerBound)
            lastEnd = payload.upperBound
            seen.append((type, payload.count))
            return true
        }
        XCTAssertEqual(lastEnd, Self.keyframeTU.count, "walk must cover the whole TU")
        XCTAssertEqual(seen.map(\.type), [
            AV1.OBUType.temporalDelimiter, AV1.OBUType.sequenceHeader, 6, // 6 = OBU_FRAME
        ])
        XCTAssertEqual(seen[0].payloadCount, 0)
        XCTAssertEqual(seen[1].payloadCount, 11)
    }

    func testOBUWalkStopsEarly() {
        var calls = 0
        AV1.forEachOBU(in: Data(Self.keyframeTU)) { _, _, _, _ in
            calls += 1
            return false
        }
        XCTAssertEqual(calls, 1)
    }

    func testOBUWalkRejectsGarbage() {
        // 0x80 = forbidden bit set: not an OBU stream, the walk must not call the body.
        AV1.forEachOBU(in: Data([0x80, 0x00, 0x01])) { _, _, _, _ in
            XCTFail("garbage must not yield OBUs")
            return true
        }
        // A size field overrunning the buffer stops the walk at the previous OBU.
        var truncated = Data([0x12, 0x00]) // valid TD
        truncated.append(contentsOf: [0x0A, 0x7F, 0x01]) // seq header claiming 127 bytes, 1 present
        var types: [UInt8] = []
        AV1.forEachOBU(in: truncated) { _, _, _, type in
            types.append(type)
            return true
        }
        XCTAssertEqual(types, [AV1.OBUType.temporalDelimiter])
    }

    // MARK: - Sequence header

    func testSequenceHeaderParse() throws {
        // The sequence-header OBU sits at bytes 2..<15, behind the 2-byte temporal delimiter.
        let sh = try XCTUnwrap(AV1.sequenceInfo(Data(Self.keyframeTU[2..<15])))
        XCTAssertEqual(sh.profile, 0) // Main
        XCTAssertEqual(sh.level_idx0, 0)
        XCTAssertEqual(sh.tier0, 0)
        XCTAssertFalse(sh.high_bitdepth)
        XCTAssertFalse(sh.twelve_bit)
        XCTAssertFalse(sh.mono_chrome)
        XCTAssertTrue(sh.subsampling_x) // profile 0 ⇒ 4:2:0
        XCTAssertTrue(sh.subsampling_y)
        XCTAssertEqual(sh.chroma_sample_position, 0)
        XCTAssertEqual(sh.color_primaries, 2) // no color description ⇒ unspecified
        XCTAssertEqual(sh.transfer_characteristics, 2)
        XCTAssertEqual(sh.matrix_coefficients, 2)
        XCTAssertFalse(sh.full_range)
        XCTAssertEqual(sh.max_width, 320)
        XCTAssertEqual(sh.max_height, 180)
        XCTAssertNotNil(AV1.sequenceInfo(Data(Self.keyframeTU)), "a whole temporal unit")
    }

    func testSequenceHeaderRejectsTruncation() {
        let obu = Data(Self.keyframeTU[2..<15])
        for cut in 0..<obu.count {
            XCTAssertNil(
                AV1.sequenceInfo(obu.prefix(cut)),
                "a header truncated to \(cut) bytes must not parse")
        }
        XCTAssertNil(AV1.sequenceInfo(Data(Self.deltaTU)), "a delta TU carries no header")
    }

    // MARK: - Format description

    func testFormatDescriptionFromKeyframe() throws {
        let format = try XCTUnwrap(AV1.formatDescription(fromKeyframe: Data(Self.keyframeTU)))
        XCTAssertEqual(CMFormatDescriptionGetMediaSubType(format), kCMVideoCodecType_AV1)
        let dims = CMVideoFormatDescriptionGetDimensions(format)
        XCTAssertEqual(dims.width, 320)
        XCTAssertEqual(dims.height, 180)

        // The av1C record: marker/version, profile+level, the packed flags byte (4:2:0, 8-bit,
        // csp 0 → 0x0C), no presentation delay — then the sequence-header OBU verbatim.
        let atoms = try XCTUnwrap(
            CMFormatDescriptionGetExtension(
                format,
                extensionKey: kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms)
                as? [String: Any])
        let av1C = try XCTUnwrap(atoms["av1C"] as? Data)
        XCTAssertEqual([UInt8](av1C.prefix(4)), [0x81, 0x00, 0x0C, 0x00])
        XCTAssertEqual([UInt8](av1C.dropFirst(4)), [UInt8](Self.keyframeTU[2..<15]),
                       "configOBUs must be the size-fielded sequence-header OBU")

        // Unspecified color codes fall back to BT.709 studio range — and the transfer-function
        // extension is what keeps VideoDecoder.isHDRFormat working for AV1.
        let transfer = CMFormatDescriptionGetExtension(
            format, extensionKey: kCMFormatDescriptionExtension_TransferFunction) as? String
        XCTAssertEqual(transfer, kCMFormatDescriptionTransferFunction_ITU_R_709_2 as String)
        XCTAssertFalse(VideoDecoder.isHDRFormat(format))
    }

    func testDeltaTUYieldsNoFormat() {
        XCTAssertNil(AV1.formatDescription(fromKeyframe: Data(Self.deltaTU)),
                     "a delta TU has no sequence header — the pumps must latch the previous one")
    }

    // MARK: - Sample repack

    func testSampleRepack() throws {
        let format = try XCTUnwrap(AV1.formatDescription(fromKeyframe: Data(Self.keyframeTU)))
        let au = AccessUnit(
            data: Data(Self.keyframeTU), ptsNs: 1_000_000, frameIndex: 0, flags: 0, receivedNs: 0)
        let sample = try XCTUnwrap(AV1.sampleBuffer(au: au, format: format))

        // The blob is already fully size-fielded, so the repack is byte-identical minus the
        // 2-byte temporal delimiter.
        let block = try XCTUnwrap(CMSampleBufferGetDataBuffer(sample))
        var length = 0
        var ptr: UnsafeMutablePointer<CChar>?
        XCTAssertEqual(CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
            dataPointerOut: &ptr), noErr)
        let bytes = UnsafeRawBufferPointer(start: ptr, count: length)
        XCTAssertEqual([UInt8](bytes), [UInt8](Self.keyframeTU[2...]))

        // No temporal delimiter survives, and the pts round-trips at nanosecond scale.
        AV1.forEachOBU(in: Data(bytes)) { _, _, _, type in
            XCTAssertNotEqual(type, AV1.OBUType.temporalDelimiter)
            return true
        }
        let pts = CMSampleBufferGetPresentationTimeStamp(sample)
        XCTAssertEqual(pts.value, 1_000_000)
        XCTAssertEqual(pts.timescale, 1_000_000_000)
    }

    /// A temporal unit the core assembled in a pool slot is the sample itself, from just past
    /// the temporal delimiter: no block of its own, the same bytes the repack produces.
    func testSampleOverTheSlotSkipsTheDelimiter() throws {
        let format = try XCTUnwrap(AV1.formatDescription(fromKeyframe: Data(Self.keyframeTU)))
        let tu = Self.keyframeTU
        let pool = AccessUnitSlotPool(capacity: 4096)
        let slot = try XCTUnwrap(pool.take(tu.count))
        tu.withUnsafeBytes { slot.base.copyMemory(from: $0.baseAddress!, byteCount: $0.count) }
        let at = try XCTUnwrap(pool.locate(UnsafeRawPointer(slot.base)))
        let au = AccessUnit(
            data: Data(bytesNoCopy: slot.base, count: tu.count, deallocator: .none),
            ptsNs: 7, frameIndex: 0, flags: 0, receivedNs: 0,
            inPlace: AccessUnit.InPlace(token: at.token, offset: at.offset, count: tu.count))
        let sample = try XCTUnwrap(AV1.sampleBuffer(au: au, format: format, in: pool))
        let block = try XCTUnwrap(CMSampleBufferGetDataBuffer(sample))
        var length = 0
        var ptr: UnsafeMutablePointer<CChar>?
        XCTAssertEqual(CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
            dataPointerOut: &ptr), noErr)
        XCTAssertEqual(UnsafeMutableRawPointer(try XCTUnwrap(ptr)), slot.base + 2)
        XCTAssertEqual([UInt8](UnsafeRawBufferPointer(start: ptr, count: length)), [UInt8](tu[2...]))
        XCTAssertEqual(CMSampleBufferGetPresentationTimeStamp(sample).value, 7)
    }

    /// A torn OBU after the frame ends the walk early: the slot would carry the torn bytes into
    /// the sample, so the AU goes to the repack, which keeps only the whole OBUs.
    func testATornTailLeavesTheSlotToTheRepack() throws {
        let format = try XCTUnwrap(AV1.formatDescription(fromKeyframe: Data(Self.keyframeTU)))
        let tu = Self.keyframeTU + [0x0A, 0x7F, 0x01] // a sequence header claiming 127 bytes
        let pool = AccessUnitSlotPool(capacity: 4096)
        let slot = try XCTUnwrap(pool.take(tu.count))
        tu.withUnsafeBytes { slot.base.copyMemory(from: $0.baseAddress!, byteCount: $0.count) }
        let at = try XCTUnwrap(pool.locate(UnsafeRawPointer(slot.base)))
        let au = AccessUnit(
            data: Data(bytesNoCopy: slot.base, count: tu.count, deallocator: .none),
            ptsNs: 0, frameIndex: 0, flags: 0, receivedNs: 0,
            inPlace: AccessUnit.InPlace(token: at.token, offset: at.offset, count: tu.count))
        let sample = try XCTUnwrap(AV1.sampleBuffer(au: au, format: format, in: pool))
        let block = try XCTUnwrap(CMSampleBufferGetDataBuffer(sample))
        var length = 0
        var ptr: UnsafeMutablePointer<CChar>?
        XCTAssertEqual(CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
            dataPointerOut: &ptr), noErr)
        XCTAssertNotEqual(UnsafeMutableRawPointer(try XCTUnwrap(ptr)), slot.base + 2)
        XCTAssertEqual(
            [UInt8](UnsafeRawBufferPointer(start: ptr, count: length)),
            [UInt8](Self.keyframeTU[2...]))
        pool.release(slot.token)
    }

    func testSampleRepackDelimiterOnlyIsDropped() throws {
        let format = try XCTUnwrap(AV1.formatDescription(fromKeyframe: Data(Self.keyframeTU)))
        let au = AccessUnit(
            data: Data([0x12, 0x00]), ptsNs: 0, frameIndex: 0, flags: 0, receivedNs: 0)
        XCTAssertNil(AV1.sampleBuffer(au: au, format: format))
    }

    // MARK: - VideoCodec dispatch

    func testWireCodecResolution() {
        XCTAssertEqual(VideoCodec(wire: 0x01), .h264)
        XCTAssertEqual(VideoCodec(wire: 0x02), .hevc)
        XCTAssertEqual(VideoCodec(wire: 0x04), .av1)
        XCTAssertEqual(VideoCodec(wire: 0xFF), .hevc) // unknown → the default codec
    }

    func testCodecDispatch() {
        let au = Data(Self.keyframeTU)
        XCTAssertNotNil(VideoCodec.av1.formatDescription(fromKeyframe: au))
        // The same bytes through the NAL paths must not parse — proves the dispatch matters.
        XCTAssertNil(VideoCodec.hevc.formatDescription(fromKeyframe: au))
        XCTAssertNil(VideoCodec.h264.formatDescription(fromKeyframe: au))
    }

    // MARK: - Hardware decode (end to end)

    /// The AV1 counterpart of VideoToolboxRoundTripTests' decode half: the keyframe blob through
    /// the REAL VideoDecoder (format description → repack → hardware VTDecompressionSession).
    /// Pixels out = the whole AV1 decode path is sound. Skipped on devices without AV1 hardware
    /// (exactly the devices the client never advertises AV1 from).
    func testHardwareDecodeEndToEnd() throws {
        try XCTSkipUnless(
            AV1.hardwareDecodeSupported, "no AV1 hardware decoder — AV1 is never advertised here")

        let format = try XCTUnwrap(AV1.formatDescription(fromKeyframe: Data(Self.keyframeTU)))
        let au = AccessUnit(
            data: Data(Self.keyframeTU), ptsNs: 42_000_000, frameIndex: 0, flags: 0, receivedNs: 1)
        let ready = try decodeOnce(au, format: format) { $0.setCodec(.av1) }
        XCTAssertEqual(ready.ptsNs, 42_000_000)
        XCTAssertFalse(ready.isHDR)
        let buffer = try XCTUnwrap(ready.pixelBuffer, "a VT decode delivers a .video frame")
        XCTAssertEqual(CVPixelBufferGetWidth(buffer), 320)
        XCTAssertEqual(CVPixelBufferGetHeight(buffer), 180)
        XCTAssertEqual(
            CVPixelBufferGetPixelFormatType(buffer),
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, "SDR AV1 must decode to NV12")
    }

    // MARK: - Blobs

    /// Keyframe temporal unit (740 bytes).
    static let keyframeTU: [UInt8] = [
        0x12, 0x00, 0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xfe, 0xcc, 0x4a, 0xf9, 0x00, 0x40, 0x32,
        0xd2, 0x05, 0x10, 0x00, 0x9b, 0xa0, 0x8f, 0xbe, 0x7d, 0xf0, 0xdf, 0xbc, 0xf8, 0x00, 0xdd, 0x6d,
        0x69, 0x7f, 0xb3, 0x26, 0x63, 0x5e, 0x79, 0xf4, 0xf5, 0xc5, 0x84, 0xda, 0xcf, 0xec, 0xd8, 0xa6,
        0xc1, 0x56, 0x99, 0x43, 0xf0, 0xff, 0x31, 0xd5, 0x41, 0xd8, 0xbb, 0x07, 0x10, 0x5e, 0x42, 0xc5,
        0x9b, 0x63, 0xc0, 0x14, 0xe7, 0x28, 0x73, 0xf3, 0x50, 0x12, 0x02, 0x8e, 0x2b, 0x91, 0xd7, 0x3d,
        0xc5, 0x33, 0xc9, 0x9b, 0xd9, 0xea, 0xfb, 0xc3, 0x5a, 0xdd, 0xb3, 0x0b, 0x5d, 0xc6, 0xde, 0x1a,
        0xca, 0x90, 0x61, 0x0a, 0x77, 0x83, 0xb5, 0x8f, 0x9a, 0x88, 0xf0, 0x3d, 0xa2, 0x78, 0x81, 0x6c,
        0xb6, 0x8e, 0x85, 0x90, 0x44, 0xa1, 0xda, 0xa8, 0xf6, 0xcb, 0xa2, 0xf2, 0xb8, 0xb9, 0xac, 0x6b,
        0xba, 0xfd, 0x8f, 0x7e, 0x04, 0x32, 0x0d, 0x90, 0xed, 0x5a, 0xe2, 0x1d, 0x8a, 0x03, 0x29, 0x47,
        0xde, 0xf6, 0x9a, 0xd4, 0x45, 0x91, 0x17, 0x5c, 0x7b, 0xdf, 0xa3, 0xe2, 0x7a, 0xd9, 0x93, 0xfd,
        0x55, 0xeb, 0xd9, 0x3f, 0xa6, 0xec, 0xdd, 0x2a, 0x00, 0xbc, 0x8d, 0x1d, 0x98, 0xa2, 0x39, 0x88,
        0x3f, 0x32, 0x3e, 0x18, 0x85, 0x12, 0x46, 0x57, 0x1d, 0x25, 0x8e, 0xcc, 0x41, 0x37, 0xc3, 0x9f,
        0xbd, 0x7b, 0xf9, 0xb9, 0xa4, 0x9d, 0x86, 0xf4, 0xcc, 0x2b, 0x5a, 0xf3, 0xbc, 0x77, 0x80, 0xcb,
        0xc4, 0xf3, 0x02, 0x91, 0xf6, 0xb8, 0x59, 0x93, 0x33, 0xbe, 0xe2, 0x23, 0xac, 0xb9, 0xe2, 0x69,
        0x67, 0xad, 0x63, 0x45, 0x35, 0x94, 0x9e, 0x2e, 0xfa, 0x2b, 0xb9, 0xc3, 0xf9, 0x39, 0x5e, 0xa8,
        0x33, 0x4d, 0xf3, 0x5c, 0xbd, 0xe6, 0x5b, 0x50, 0x19, 0xe6, 0xd3, 0xf2, 0x01, 0xcf, 0x35, 0x09,
        0xd6, 0x2a, 0x67, 0x17, 0xd2, 0xbd, 0x91, 0xc3, 0x91, 0x17, 0x4a, 0xbc, 0x29, 0xf0, 0xb8, 0xd4,
        0xfc, 0x04, 0xac, 0x63, 0xfb, 0x2f, 0xc5, 0xe9, 0xb2, 0x06, 0xac, 0x3c, 0x79, 0x33, 0x5c, 0x73,
        0x80, 0x95, 0x0f, 0xad, 0xff, 0xee, 0xed, 0x78, 0xaf, 0xc6, 0x1b, 0xb4, 0xc2, 0x96, 0x5f, 0x7f,
        0x20, 0x5f, 0xb6, 0xdb, 0x70, 0xab, 0x60, 0x0b, 0xea, 0xd1, 0xaf, 0x57, 0x71, 0xeb, 0x3b, 0xef,
        0xb1, 0x3c, 0x01, 0x72, 0x5b, 0x59, 0x6d, 0x36, 0xe3, 0x16, 0xda, 0x0a, 0x6b, 0xc7, 0x0b, 0xa0,
        0xa6, 0x6f, 0x77, 0x4f, 0x0b, 0xe6, 0x62, 0x34, 0x0e, 0xdd, 0xfa, 0xbe, 0x4f, 0x67, 0x21, 0x40,
        0xc2, 0xcd, 0xf3, 0x63, 0x9e, 0xb2, 0x28, 0xaf, 0x5b, 0x0e, 0x28, 0xba, 0x91, 0xec, 0xec, 0xf2,
        0xf4, 0xdb, 0x9c, 0x51, 0x24, 0x67, 0x77, 0xe0, 0x67, 0x70, 0x51, 0xfb, 0x00, 0x61, 0x50, 0x9f,
        0xae, 0x5e, 0x96, 0x2d, 0x1e, 0xa2, 0xab, 0x49, 0x90, 0x90, 0x1c, 0x04, 0x93, 0x8b, 0xc2, 0xee,
        0x53, 0x61, 0x62, 0x19, 0x62, 0x5c, 0xff, 0x15, 0x84, 0x7a, 0x5c, 0x70, 0xaa, 0x6d, 0x39, 0xb2,
        0xe9, 0x19, 0xd1, 0x9f, 0xf3, 0xb3, 0xb7, 0x05, 0xd2, 0xef, 0x5f, 0xe9, 0x2a, 0x25, 0x55, 0x0a,
        0xf3, 0xd2, 0x95, 0xba, 0x22, 0x5f, 0x49, 0x9b, 0x5d, 0xae, 0xeb, 0x51, 0x63, 0x51, 0xa0, 0x85,
        0xd6, 0xb6, 0x23, 0x6f, 0x92, 0xbf, 0x99, 0xf3, 0xf9, 0xbf, 0x07, 0xd4, 0x05, 0x1c, 0x6b, 0xe1,
        0x42, 0x49, 0xfe, 0x99, 0x4c, 0x6f, 0x34, 0xea, 0x29, 0x14, 0xd5, 0x92, 0x17, 0xfa, 0xc4, 0x35,
        0xef, 0x97, 0x31, 0x06, 0xdc, 0xc7, 0x57, 0xe7, 0x79, 0x3d, 0x64, 0xf3, 0x12, 0x0d, 0x60, 0x5d,
        0x9d, 0xa5, 0x11, 0xb9, 0xf9, 0x75, 0x3a, 0x59, 0x42, 0x2a, 0xd0, 0xed, 0xb4, 0xa8, 0xee, 0x87,
        0x9e, 0x3d, 0x52, 0x47, 0x6c, 0x8f, 0x43, 0x98, 0xd0, 0x72, 0x8b, 0x16, 0xef, 0x2a, 0xaa, 0x9c,
        0x42, 0x91, 0xc8, 0x92, 0x85, 0x79, 0xc0, 0xf8, 0xc0, 0x12, 0x44, 0x38, 0x49, 0xdb, 0x5f, 0xfc,
        0x7b, 0x6e, 0xac, 0xea, 0x38, 0x3c, 0x3f, 0x49, 0xc2, 0xe7, 0x82, 0xb3, 0x30, 0xf2, 0x7e, 0x31,
        0xc2, 0xf8, 0xfc, 0x9a, 0x9e, 0x6d, 0x4c, 0x5f, 0xd4, 0xc0, 0x22, 0xd3, 0x40, 0xc9, 0x66, 0x59,
        0x38, 0x14, 0x64, 0x24, 0xc0, 0x0d, 0x56, 0x88, 0x6d, 0x9f, 0x80, 0x71, 0xc8, 0x26, 0x3e, 0x2f,
        0xd1, 0xd2, 0x6d, 0x8a, 0xf2, 0x2c, 0x01, 0xbf, 0x89, 0x15, 0xc7, 0x66, 0x3d, 0x19, 0x9f, 0xb8,
        0x4c, 0xb9, 0x6f, 0xd7, 0xe8, 0x59, 0xf3, 0xe7, 0xdd, 0x14, 0x3e, 0x99, 0x37, 0x90, 0xb5, 0x2d,
        0x49, 0xcc, 0x40, 0xd6, 0xe1, 0x29, 0x4e, 0x31, 0x7c, 0xef, 0xdb, 0x74, 0xf0, 0x9f, 0xa6, 0xdd,
        0xbc, 0xd9, 0x65, 0x0f, 0xf7, 0x22, 0xa8, 0xd0, 0xfb, 0x78, 0x49, 0x40, 0xbf, 0x96, 0xa1, 0x5a,
        0x7b, 0xc6, 0x60, 0x63, 0x22, 0xad, 0x5d, 0x97, 0xa2, 0x77, 0x3f, 0x58, 0x3b, 0x94, 0xa4, 0xd9,
        0xe3, 0xe5, 0xae, 0x0c, 0x30, 0x91, 0xdd, 0x5a, 0xbb, 0xfd, 0x10, 0x6a, 0x22, 0x8f, 0xbd, 0x8c,
        0x41, 0xf5, 0xa2, 0x29, 0xd7, 0xad, 0x4e, 0x58, 0xfb, 0x1e, 0x9a, 0x0e, 0x98, 0x54, 0xc1, 0xd7,
        0xfb, 0xdf, 0xcc, 0x1d, 0x5d, 0xe3, 0x25, 0x6b, 0x57, 0x69, 0x67, 0x80, 0x0c, 0xb5, 0xcb, 0x01,
        0x5a, 0x56, 0x56, 0x01, 0x47, 0xad, 0x6b, 0x26, 0x28, 0x30, 0x36, 0x79, 0x91, 0x62, 0x52, 0x93,
        0xa4, 0xe8, 0x52, 0x30,
    ]

    /// Delta temporal unit (27 bytes).
    static let deltaTU: [UInt8] = [
        0x12, 0x00, 0x32, 0x17, 0x30, 0x02, 0x04, 0x09, 0x24, 0x92, 0x22, 0x7f, 0x80, 0x00, 0x01, 0x9f,
        0x00, 0x00, 0x00, 0x8b, 0x07, 0x27, 0x7a, 0x64, 0x4c, 0xec, 0xf4,
    ]
}
