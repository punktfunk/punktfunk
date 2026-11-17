import CoreMedia
import CoreVideo
import VideoToolbox
import XCTest
import simd

@testable import PunktfunkKit

/// Golden end-to-end colour tests: decode the known-signaling bar fixtures through a real
/// `VTDecompressionSession`, read the buffer's propagated signaling via `CscRows.signal(of:)`,
/// convert sampled Y′CbCr through `CscRows.rows` — the exact math the Metal shaders run — and
/// require the ORIGINAL RGB bars back. This is the proof of the two assumptions the stage-2
/// colour fix rests on: (1) VideoToolbox propagates the bitstream's matrix onto the decoded
/// CVPixelBuffer's attachments, and (2) signal+rows renders it correctly for BT.601/709 ×
/// limited/full. A hardcoded-709 regression fails the 601 fixture by tens of code points.
final class ColorBarDecodeTests: XCTestCase {
    private static let bars: [(r: Float, g: Float, b: Float)] = [
        (255, 255, 255), (255, 255, 0), (0, 255, 255), (0, 255, 0),
        (255, 0, 255), (255, 0, 0), (0, 0, 255), (0, 0, 0),
    ]

    private func assertBars(
        _ name: String, au: [UInt8], pixelFormat: OSType,
        expected: CscRows.Signal
    ) throws {
        let buffer = try XCTUnwrap(
            VTOneShot.decode(
                annexB: Data(au), codec: .hevc, pixelFormat: pixelFormat, requireHardware: false),
            "\(name): VideoToolbox must decode the fixture")
        let signal = CscRows.signal(of: buffer)
        XCTAssertEqual(signal, expected, "\(name): VT must propagate the bitstream signaling")

        let rows = CscRows.rows(signal, depth: 8, msbPacked: false)
        CVPixelBufferLockBaseAddress(buffer, .readOnly)
        defer { CVPixelBufferUnlockBaseAddress(buffer, .readOnly) }
        let yBase = try XCTUnwrap(CVPixelBufferGetBaseAddressOfPlane(buffer, 0))
            .assumingMemoryBound(to: UInt8.self)
        let yStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        let cBase = try XCTUnwrap(CVPixelBufferGetBaseAddressOfPlane(buffer, 1))
            .assumingMemoryBound(to: UInt8.self)
        let cStride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1)

        for (i, bar) in Self.bars.enumerated() {
            let (cx, cy) = (i * 32 + 16, 32)
            let y = Float(yBase[cy * yStride + cx]) / 255.0
            let cb = Float(cBase[(cy / 2) * cStride + (cx / 2) * 2]) / 255.0
            let cr = Float(cBase[(cy / 2) * cStride + (cx / 2) * 2 + 1]) / 255.0
            let yuv = SIMD3<Float>(y, cb, cr)
            let rgb = SIMD3<Float>(
                simd_dot(SIMD3(rows.r0.x, rows.r0.y, rows.r0.z), yuv) + rows.r0.w,
                simd_dot(SIMD3(rows.r1.x, rows.r1.y, rows.r1.z), yuv) + rows.r1.w,
                simd_dot(SIMD3(rows.r2.x, rows.r2.y, rows.r2.z), yuv) + rows.r2.w)
            XCTAssertEqual(rgb.x * 255, bar.r, accuracy: 3, "\(name) bar \(i) R")
            XCTAssertEqual(rgb.y * 255, bar.g, accuracy: 3, "\(name) bar \(i) G")
            XCTAssertEqual(rgb.z * 255, bar.b, accuracy: 3, "\(name) bar \(i) B")
        }
    }

    /// BT.601 (BT.470BG) limited — what a Linux host's RGB-input NVENC signals. The fixture that
    /// catches a hardcoded-BT.709 shader.
    func testGolden601LimitedBars() throws {
        try assertBars(
            "601-limited", au: ColorBarFixtures.bars601Limited,
            pixelFormat: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            expected: .init(matrix: 5, fullRange: false))
    }

    /// BT.709 limited — the hosts' explicit SDR signaling.
    func testGolden709LimitedBars() throws {
        try assertBars(
            "709-limited", au: ColorBarFixtures.bars709Limited,
            pixelFormat: kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            expected: .init(matrix: 1, fullRange: false))
    }

    /// BT.709 full range — requesting the full-range sibling keeps VT from range-converting,
    /// so the full-range rows are exercised.
    func testGolden709FullBars() throws {
        try assertBars(
            "709-full", au: ColorBarFixtures.bars709Full,
            pixelFormat: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            expected: .init(matrix: 1, fullRange: true))
    }
}
