import CoreVideo
import XCTest

@testable import PunktfunkKit

/// `clients/shared/csc-vectors.json`, which pf-client-video's `csc_rows` writes, against the Swift
/// port: a divergence here means the two sides would render the same stream differently.
final class CscRowsTests: XCTestCase {
    func testRowsMatchTheSharedVectors() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent() // PunktfunkKitTests
            .deletingLastPathComponent() // Tests
            .deletingLastPathComponent() // apple
            .deletingLastPathComponent() // clients
            .appendingPathComponent("shared/csc-vectors.json")
        let root = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let cases = try XCTUnwrap(root["cases"] as? [[String: Any]])
        XCTAssertGreaterThanOrEqual(cases.count, 40, "the vector file is the contract; keep it rich")
        for c in cases {
            let matrix = try UInt8(XCTUnwrap(c["matrix"] as? Int))
            let fullRange = try XCTUnwrap(c["full_range"] as? Bool)
            let depth = try XCTUnwrap(c["depth"] as? Int)
            let msbPacked = try XCTUnwrap(c["msb_packed"] as? Bool)
            let want = try XCTUnwrap(c["rows"] as? [[Double]])
            let got = CscRows.rows(
                .init(matrix: matrix, fullRange: fullRange), depth: depth, msbPacked: msbPacked)
            let name = "matrix \(matrix) full \(fullRange) depth \(depth) packed \(msbPacked)"
            for (r, row) in [got.r0, got.r1, got.r2].enumerated() {
                for k in 0..<4 {
                    XCTAssertEqual(Double(row[k]), want[r][k], accuracy: 1e-6, "\(name) r\(r)[\(k)]")
                }
            }
        }
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
