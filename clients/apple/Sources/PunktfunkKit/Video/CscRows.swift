// The Y′CbCr→RGB conversion as three shader rows, ported from pf-client-core's `csc_rows`
// (crates/pf-client-core/src/video.rs) — the ONE coefficient implementation every punktfunk
// presenter derives its CSC from. Keep the two in LOCKSTEP: both carry the same unit tests
// (CscRowsTests.swift ↔ the Rust `csc_rows` tests), and a coefficient change lands in both or
// neither.
//
// The rows follow each decoded buffer's signalled matrix (VideoToolbox propagates the HEVC VUI /
// AV1 colour config onto the CVPixelBuffer's attachments), so a BT.601-tagged stream renders
// with BT.601 coefficients instead of a hardcoded BT.709/BT.2020.

import CoreVideo
import simd

/// The fragment shaders' CSC constant block: `rgb[i] = dot(r[i].xyz, yuv) + r[i].w`.
/// Layout matches the Metal-side `struct CscUniform { float4 r0; float4 r1; float4 r2; }`
/// (three 16-byte-aligned float4s, stride 48) — passed via `setFragmentBytes`.
public struct CscUniform: Equatable, Sendable {
    public var r0: SIMD4<Float>
    public var r1: SIMD4<Float>
    public var r2: SIMD4<Float>
}

public enum CscRows {
    /// A decoded frame's Y′CbCr signaling: the H.273 matrix code (1 = BT.709, 5/6 = BT.601,
    /// 9/10 = BT.2020; 2 = unspecified → the BT.709 SDR default, mirroring `ColorDesc`) and
    /// whether the samples are full range.
    public struct Signal: Equatable, Sendable {
        public var matrix: UInt8
        public var fullRange: Bool

        public init(matrix: UInt8, fullRange: Bool) {
            self.matrix = matrix
            self.fullRange = fullRange
        }
    }

    /// Read a decoded buffer's signaling: the matrix from the `CVImageBuffer` attachment
    /// (VideoToolbox propagates the bitstream's colour description there), the range from the
    /// pixel format itself (the video- vs full-range biplanar siblings), so a full-range stream
    /// expands correctly no matter which sibling VideoToolbox delivered.
    public static func signal(of buffer: CVPixelBuffer) -> Signal {
        var matrix: UInt8 = 2 // unspecified → BT.709 default in rows()
        if let att = CVBufferCopyAttachment(buffer, kCVImageBufferYCbCrMatrixKey, nil),
           CFGetTypeID(att) == CFStringGetTypeID() {
            let s = unsafeDowncast(att, to: CFString.self)
            if CFEqual(s, kCVImageBufferYCbCrMatrix_ITU_R_709_2) {
                matrix = 1
            } else if CFEqual(s, kCVImageBufferYCbCrMatrix_ITU_R_601_4) {
                matrix = 5
            } else if CFEqual(s, kCVImageBufferYCbCrMatrix_SMPTE_240M_1995) {
                matrix = 7
            } else if CFEqual(s, kCVImageBufferYCbCrMatrix_ITU_R_2020) {
                matrix = 9
            } else {
                // CICP codes CoreMedia has no named constant for arrive as the literal string
                // "YCbCrMatrix#<code>" — the suffix IS the H.273 code. BT.470BG (5) takes this
                // form (proven by the 601 golden fixture), and BT.470BG is exactly what a Linux
                // host's RGB-input NVENC signals, so missing it re-creates the hue bug the
                // per-frame signaling exists to fix.
                let str = s as String
                if str.hasPrefix("YCbCrMatrix#"), let code = UInt8(str.dropFirst(12)) {
                    matrix = code
                }
            }
        }
        let pf = CVPixelBufferGetPixelFormatType(buffer)
        let fullRange = pf == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            || pf == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
            || pf == kCVPixelFormatType_444YpCbCr8BiPlanarFullRange
            || pf == kCVPixelFormatType_444YpCbCr10BiPlanarFullRange
        return Signal(matrix: matrix, fullRange: fullRange)
    }

    /// Compute the three rows — bit-depth exact. `depth` picks the limited-range code points
    /// (8-bit: 16/235/240 over 255; 10-bit: 64/940/960 over 1023 — NOT the same normalized
    /// values, the difference is ~half a code). `msbPacked` folds in the P010/x444 packing
    /// factor: 10 significant bits live in the MSBs of 16, so an `.r16Unorm` sample reads
    /// `code·64/65535` — multiplying by `65535/65472` recovers exact `code/1023` (this replaces
    /// the shaders' old documented ~0.1% approximation).
    public static func rows(_ signal: Signal, depth: Int, msbPacked: Bool) -> CscUniform {
        // BT.601 (5/6), BT.2020 (9/10); everything else — incl. unspecified — is the host's
        // BT.709 SDR default (mirrors the Rust side's dispatch).
        let (kr, kb): (Double, Double)
        switch signal.matrix {
        case 5, 6: (kr, kb) = (0.299, 0.114)
        case 9, 10: (kr, kb) = (0.2627, 0.0593)
        default: (kr, kb) = (0.2126, 0.0722)
        }
        let kg = 1.0 - kr - kb
        let max = Double((1 << depth) - 1) // 255 / 1023
        let step = Double(1 << (depth - 8)) // code points per 8-bit step: 1 / 4
        let pack = msbPacked ? 65535.0 / 65472.0 : 1.0
        let (sy, oy, sc): (Double, Double, Double)
        if signal.fullRange {
            (sy, oy, sc) = (pack, 0.0, pack)
        } else {
            (sy, oy, sc) = (
                pack * max / (219.0 * step),
                -(16.0 * step) / max,
                pack * max / (224.0 * step)
            )
        }
        // rgb = M * (yuv + off). `yuv` is the sampled (packed) value, so offsets divide by the
        // packing factor. Neutral chroma is code 128 (512 at 10 bits) in both ranges, not 0.5.
        let oc = -(128.0 * step) / max
        let off = [oy / pack, oc / pack, oc / pack]
        let m: [[Double]] = [
            [sy, 0.0, 2.0 * (1.0 - kr) * sc],
            [
                sy,
                -2.0 * (1.0 - kb) * kb / kg * sc,
                -2.0 * (1.0 - kr) * kr / kg * sc,
            ],
            [sy, 2.0 * (1.0 - kb) * sc, 0.0],
        ]
        func row(_ r: Int) -> SIMD4<Float> {
            let w = (0..<3).reduce(0.0) { $0 + m[r][$1] * off[$1] }
            return SIMD4(Float(m[r][0]), Float(m[r][1]), Float(m[r][2]), Float(w))
        }
        return CscUniform(r0: row(0), r1: row(1), r2: row(2))
    }
}
