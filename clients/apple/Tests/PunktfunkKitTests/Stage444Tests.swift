// 4:4:4 decode-path coverage: the hardware-capability probe is stable/cached, and a real 4:4:4 HEVC
// keyframe decodes through VideoDecoder to a biplanar 4:4:4 pixel buffer. Reuses the same synthetic
// 4:4:4 blobs the runtime probe ships with.

import CoreVideo
import VideoToolbox
import XCTest
@testable import PunktfunkKit

final class Stage444Tests: XCTestCase {
    /// The capability probe is device-static and cached — reading it twice must return the same value
    /// (and must never crash, including where 4:4:4 is unsupported → false).
    func testProbeIsStableAndCached() {
        XCTAssertEqual(Stage444Probe.hwDecode444_8bit, Stage444Probe.hwDecode444_8bit)
        XCTAssertEqual(Stage444Probe.hwDecode444_10bit, Stage444Probe.hwDecode444_10bit)
    }

    /// A 10-bit SDR stream decodes to a 10-bit buffer and stays SDR. The probe blob is plain
    /// libx265 with no PQ/HLG, which is exactly the shape of a host's 10-bit SDR session
    /// (Main10 under BT.709): depth comes from the negotiated session, HDR from the transfer
    /// function.
    func testVideoDecoderDecodes10BitSdrAsTenBitAndNotHDR() throws {
        try XCTSkipUnless(
            Stage444Probe.hwDecode444_10bit, "no hardware 10-bit 4:4:4 decode on this device")
        let data = Data(Probe444Blobs.au444_10bit)
        let format = try XCTUnwrap(
            AnnexB.formatDescription(fromIDR: data, codec: .hevc),
            "the 10-bit blob must yield a format description")
        XCTAssertFalse(
            VideoDecoder.isHDRFormat(format), "the probe blob carries no PQ/HLG transfer")
        let au = AccessUnit(data: data, ptsNs: 9_000_000, frameIndex: 0, flags: 0, receivedNs: 0)

        let ready = try decodeOnce(au, format: format) { decoder in
            decoder.setChroma444(true)
            decoder.setBitDepth(10)
        }
        guard case .video(let buffer, let isHDR) = ready.image else {
            return XCTFail("a VideoToolbox decode must deliver a .video frame")
        }
        let pf = CVPixelBufferGetPixelFormatType(buffer)
        XCTAssertTrue(
            pf == kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange
                || pf == kCVPixelFormatType_444YpCbCr10BiPlanarFullRange,
            "a 10-bit session must decode to a 10-bit buffer, got \(fourCCString(pf))")
        XCTAssertFalse(isHDR, "10-bit alone is not HDR — this is Main10 under BT.709")
    }

    /// A real 8-bit 4:4:4 HEVC keyframe (the embedded probe blob) decodes through `VideoDecoder` with
    /// `setChroma444(true)` to a 256×256 biplanar 4:4:4 (`444v`/`444f`) buffer classified SDR.
    /// (4:4:4 sessions require a hardware decoder — skip where there isn't one, which is exactly where
    /// the client wouldn't advertise 4:4:4 anyway.)
    func testVideoDecoderDecodes444() throws {
        try XCTSkipUnless(
            Stage444Probe.hwDecode444_8bit, "no hardware 4:4:4 decode on this device")
        let data = Data(Probe444Blobs.au444_8bit)
        let format = try XCTUnwrap(
            AnnexB.formatDescription(fromIDR: data, codec: .hevc), "the 4:4:4 blob must yield a format description")
        let au = AccessUnit(data: data, ptsNs: 7_000_000, frameIndex: 0, flags: 0, receivedNs: 0)

        let ready = try decodeOnce(au, format: format) { $0.setChroma444(true) }
        guard case .video(let buffer, let isHDR) = ready.image else {
            return XCTFail("a VideoToolbox decode must deliver a .video frame")
        }
        XCTAssertEqual(CVPixelBufferGetWidth(buffer), 256)
        XCTAssertEqual(CVPixelBufferGetHeight(buffer), 256)
        let pf = CVPixelBufferGetPixelFormatType(buffer)
        XCTAssertTrue(
            pf == kCVPixelFormatType_444YpCbCr8BiPlanarVideoRange
                || pf == kCVPixelFormatType_444YpCbCr8BiPlanarFullRange,
            "expected a biplanar 4:4:4 8-bit buffer, got \(fourCCString(pf))")
        XCTAssertFalse(isHDR, "an 8-bit BT.709 4:4:4 stream is SDR")
        // The chroma plane (plane 1) must be FULL resolution for 4:4:4 (vs half for 4:2:0) — this is
        // what lets the shader sample chroma at the luma UV.
        XCTAssertEqual(CVPixelBufferGetWidthOfPlane(buffer, 1), 256)
        XCTAssertEqual(CVPixelBufferGetHeightOfPlane(buffer, 1), 256)
    }

    private func fourCCString(_ t: OSType) -> String {
        let b = [UInt8(t >> 24 & 0xff), UInt8(t >> 16 & 0xff), UInt8(t >> 8 & 0xff), UInt8(t & 0xff)]
        return String(bytes: b, encoding: .ascii) ?? "\(t)"
    }
}
