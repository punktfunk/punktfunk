import CoreVideo
import XCTest
import simd

@testable import PunktfunkKit

/// Mirrors pf-client-core's `csc_rows` tests (crates/pf-client-core/src/video.rs) — the Swift port
/// must stay in LOCKSTEP with the Rust implementation, so these are the same fixtures with the
/// same tolerances. A divergence here means the two sides would render the same stream
/// differently.
final class CscRowsTests: XCTestCase {
    private func apply(_ u: CscUniform, _ yuv: SIMD3<Float>) -> SIMD3<Float> {
        SIMD3(
            simd_dot(SIMD3(u.r0.x, u.r0.y, u.r0.z), yuv) + u.r0.w,
            simd_dot(SIMD3(u.r1.x, u.r1.y, u.r1.z), yuv) + u.r1.w,
            simd_dot(SIMD3(u.r2.x, u.r2.y, u.r2.z), yuv) + u.r2.w)
    }

    /// 10-bit limited MSB-packed (P010/x444): reference white Y=940, black Y=64, neutral
    /// chroma 512 — sampled as UNORM16 of `code << 6`.
    func testBt2020TenBitLimitedWhiteBlack() {
        let rows = CscRows.rows(.init(matrix: 9, fullRange: false), depth: 10, msbPacked: true)
        func s(_ code: UInt32) -> Float { Float(code << 6) / 65535.0 }
        let white = apply(rows, SIMD3(s(940), s(512), s(512)))
        let black = apply(rows, SIMD3(s(64), s(512), s(512)))
        for i in 0..<3 {
            XCTAssertEqual(white[i], 1.0, accuracy: 1e-4, "white \(white)")
            XCTAssertEqual(black[i], 0.0, accuracy: 1e-4, "black \(black)")
        }
    }

    /// Reference white (Y=235, U=V=128 limited) → RGB 1.0; reference black (Y=16) → 0.0.
    func testBt709LimitedWhiteBlack() {
        let rows = CscRows.rows(.init(matrix: 1, fullRange: false), depth: 8, msbPacked: false)
        let white = apply(rows, SIMD3(235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0))
        let black = apply(rows, SIMD3(16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0))
        for i in 0..<3 {
            XCTAssertEqual(white[i], 1.0, accuracy: 1e-4, "white \(white)")
            XCTAssertEqual(black[i], 0.0, accuracy: 1e-4, "black \(black)")
        }
    }

    /// Full-range identity points + the 601-vs-709 red excursion (guards the matrix-code
    /// dispatch — the two matrices MUST differ measurably, that difference is the whole bug
    /// class this port fixes).
    func testFullRangeAndRedExcursion() {
        let rows601 = CscRows.rows(.init(matrix: 5, fullRange: true), depth: 8, msbPacked: false)
        let c0: Float = 128.0 / 255.0
        let white = apply(rows601, SIMD3(1.0, c0, c0))
        for i in 0..<3 {
            XCTAssertEqual(white[i], 1.0, accuracy: 1e-5, "\(white)")
        }
        let red601 = apply(rows601, SIMD3(0.0, c0, 1.0))
        XCTAssertEqual(red601[0], 2.0 * (1.0 - 0.299) * (1.0 - c0), accuracy: 1e-4, "\(red601)")
        let rows709 = CscRows.rows(.init(matrix: 1, fullRange: true), depth: 8, msbPacked: false)
        let red709 = apply(rows709, SIMD3(0.0, c0, 1.0))
        XCTAssertEqual(red709[0], 2.0 * (1.0 - 0.2126) * (1.0 - c0), accuracy: 1e-4, "\(red709)")
        XCTAssertGreaterThan(abs(red601[0] - red709[0]), 0.05)
    }

    /// RGB → the hosts' BT.709 limited 8-bit CSC (rounded to codes) → rows. Mirrors the Rust
    /// `host_bt709_limited_round_trip`: greys neutral, black black, primaries within rounding.
    func testHostBt709LimitedRoundTrip() {
        let rows = CscRows.rows(.init(matrix: 1, fullRange: false), depth: 8, msbPacked: false)
        func code(_ v: Double) -> Float { Float(min(max((v * 255).rounded(), 0), 255) / 255) }
        let patches: [[Double]] = [
            [0, 0, 0], [0.18, 0.18, 0.18], [0.5, 0.5, 0.5], [1, 1, 1],
            [1, 0, 0], [0, 1, 0], [0, 0, 1], [0, 1, 1], [1, 0, 1], [1, 1, 0],
        ]
        for p in patches {
            let (r, g, b) = (p[0], p[1], p[2])
            let yuv = SIMD3(
                code(16.0 / 255 + 0.1826 * r + 0.6142 * g + 0.0620 * b),
                code(128.0 / 255 - 0.1006 * r - 0.3386 * g + 0.4392 * b),
                code(128.0 / 255 + 0.4392 * r - 0.3989 * g - 0.0403 * b))
            let out = apply(rows, yuv)
            let grey = r == g && g == b
            for c in 0..<3 {
                let err = (Double(min(max(out[c], 0), 1)) - p[c]) * 255
                XCTAssertLessThanOrEqual(abs(err), grey ? 0.6 : 2.0, "\(p) -> \(out)")
            }
            if grey {
                XCTAssertLessThan(max(abs(out[0] - out[1]), abs(out[2] - out[1])) * 255, 0.05, "\(p) -> \(out)")
            }
        }
    }

    /// Unspecified (2) and unknown matrix codes fall back to BT.709 — the same default as the
    /// Rust side and every punktfunk host's implicit SDR baseline.
    func testUnspecifiedFallsBackTo709() {
        let unspec = CscRows.rows(.init(matrix: 2, fullRange: false), depth: 8, msbPacked: false)
        let bt709 = CscRows.rows(.init(matrix: 1, fullRange: false), depth: 8, msbPacked: false)
        XCTAssertEqual(unspec, bt709)
    }

    /// `signal(of:)` reads the matrix off the buffer's attachment (what VideoToolbox propagates
    /// from the VUI) and the range off the pixel format — a 601-tagged buffer must come back as
    /// matrix 5, an untagged one as unspecified (2), and a full-range sibling as fullRange.
    func testSignalReadsAttachmentAndRange() throws {
        func makeBuffer(_ format: OSType) throws -> CVPixelBuffer {
            var pb: CVPixelBuffer?
            let status = CVPixelBufferCreate(kCFAllocatorDefault, 64, 64, format, nil, &pb)
            guard status == kCVReturnSuccess, let pb else {
                throw XCTSkip("could not allocate a \(format) pixel buffer")
            }
            return pb
        }

        let tagged = try makeBuffer(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        CVBufferSetAttachment(
            tagged, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_601_4,
            .shouldPropagate)
        XCTAssertEqual(CscRows.signal(of: tagged), CscRows.Signal(matrix: 5, fullRange: false))

        let untagged = try makeBuffer(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange)
        XCTAssertEqual(CscRows.signal(of: untagged), CscRows.Signal(matrix: 2, fullRange: false))

        let full = try makeBuffer(kCVPixelFormatType_420YpCbCr8BiPlanarFullRange)
        CVBufferSetAttachment(
            full, kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_2020,
            .shouldPropagate)
        XCTAssertEqual(CscRows.signal(of: full), CscRows.Signal(matrix: 9, fullRange: true))
    }
}
