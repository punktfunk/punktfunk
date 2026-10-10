// PyroWave Metal decoder tests — two layers:
//
// 1. Bitstream/window-walk parser tests (pure CPU): hand-crafted packet streams assert the
//    exact wire semantics of pyrowave_decoder.cpp's push_packet walk + the Phase-4
//    chunk-aligned framing (4-byte window prefix, FRAG chains, zeroed missing shards).
//
// 2. Golden-frame PSNR tests (Metal GPU): host-encoded fixtures (crates/host/punktfunk-host
//    encode/linux/pyrowave.rs `pyrowave_dump_golden`, run on a Vulkan box) decoded by the
//    Metal port and PSNR-matched against upstream's own decoder output. Float wavelet math is
//    not bit-exact across implementations (upstream ships precision variants), so the gate is
//    PSNR, not equality. This is the §4.7 validation oracle for the hand-ported kernels —
//    the gather/mirror addressing in idwt is the spot most likely to drift.

#if canImport(Metal)
import Metal
import XCTest

@testable import PunktfunkKit

final class PyroWaveParserTests: XCTestCase {
    // 256x144 → aligned 256x160; block space identical to the committed fixtures.
    private let width = 256
    private let height = 144

    /// A BitstreamSequenceHeader (START_OF_FRAME) for `width`x`height`, 4:2:0 BT.709 limited.
    private func sof(totalBlocks: Int, sequence: UInt32 = 1) -> [UInt8] {
        let word0 =
            UInt32(width - 1) | (UInt32(height - 1) << 14) | (sequence << 28) | (1 << 31)
        // code=0 (SOF), chroma=0 (420), primaries/trc/matrix=0 (BT.709), range=1 (LIMITED),
        // siting=0.
        let word1 = UInt32(totalBlocks) | (1 << 30)
        return le32(word0) + le32(word1)
    }

    /// A minimal coefficient packet: ballot=0 (all 8x8 blocks empty — legal and decodable),
    /// payload_words=2 (header only).
    private func packet(blockIndex: Int, sequence: UInt32 = 1) -> [UInt8] {
        let word0 = UInt32(0) | (2 << 16) | (sequence << 28)
        let word1 = UInt32(0) | (UInt32(blockIndex) << 8)
        return le32(word0) + le32(word1)
    }

    private func le32(_ v: UInt32) -> [UInt8] {
        [UInt8(v & 0xff), UInt8((v >> 8) & 0xff), UInt8((v >> 16) & 0xff), UInt8(v >> 24)]
    }

    /// Wrap bodies into `windowSize`-sized windows with the 4-byte used/kind prefix.
    private func window(_ body: [UInt8], kind: UInt16, size: Int) -> [UInt8] {
        precondition(body.count + 4 <= size)
        var out = [UInt8(body.count & 0xff), UInt8(body.count >> 8)]
        out += [UInt8(kind & 0xff), UInt8(kind >> 8)]
        out += body
        out += [UInt8](repeating: 0, count: size - out.count)
        return out
    }

    func testLayoutMatchesUpstreamBlockSpace() {
        // init_block_meta's walk for 256x144 (aligned 256x160): level extents halve from
        // 128x80; per (comp,level,band) count32 = ceil(ceil(w/8)/4) * ceil(ceil(h/8)/4).
        let layout = WaveletLayout(width: width, height: height, chroma444: false)
        XCTAssertEqual(layout.alignedWidth, 256)
        XCTAssertEqual(layout.alignedHeight, 160)
        XCTAssertEqual(layout.levelWidth(0), 128)
        XCTAssertEqual(layout.levelHeight(0), 80)
        XCTAssertEqual(layout.levelWidth(4), 8)
        XCTAssertEqual(layout.levelHeight(4), 5)
        // Hand-summed: L4 (8x5 → 1 block) × 3 comps × 4 bands = 12; L3 (16x10 → 1) × 9 = 9;
        // L2 (32x20 → 1) × 9 = 9; L1 (64x40 → 2x2=4... ) — trust the invariant instead:
        // every band's count is ceil(w8/4)*ceil(h8/4) and the total is their sum.
        var expected = 0
        for level in stride(from: 4, through: 0, by: -1) {
            let w8 = (layout.levelWidth(level) + 7) / 8
            let h8 = (layout.levelHeight(level) + 7) / 8
            let per = ((w8 + 3) / 4) * ((h8 + 3) / 4)
            for component in 0..<3 {
                if level == 0 && component != 0 { continue }
                expected += per * (level == 4 ? 4 : 3)
            }
        }
        XCTAssertEqual(layout.blockCount32, expected)
        // The finest luma level's stride is its 32-block row width.
        XCTAssertEqual(layout.blockMeta[0][0][1].stride, (128 + 31) / 32)
        // Level-0 chroma is not coded in 4:2:0.
        XCTAssertEqual(layout.blockMeta[1][0][1].offset, -1)
    }

    func testDenseParseFillsOffsetsAndCountsBlocks() throws {
        let layout = WaveletLayout(width: width, height: height, chroma444: false)
        var au = sof(totalBlocks: 4)
        au += packet(blockIndex: 0)
        au += packet(blockIndex: 3)
        au += packet(blockIndex: 3) // duplicate — first wins, not double-counted
        au += packet(blockIndex: layout.blockCount32 - 1)
        let frame = try XCTUnwrap(
            WaveletBitstream.parse(au: Data(au), chunkAligned: false, windowSize: 0))
        XCTAssertEqual(frame.layout.width, width)
        XCTAssertEqual(frame.totalBlocks, 4)
        XCTAssertEqual(frame.decodedBlocks, 3)
        XCTAssertEqual(frame.offsets[0], 0)
        XCTAssertEqual(frame.offsets[3], 2) // u32 words: each header-only packet is 2 words
        XCTAssertEqual(frame.offsets[1], UInt32.max)
        XCTAssertEqual(frame.payload.count, 6)
        XCTAssertFalse(frame.bt2020)
        XCTAssertFalse(frame.fullRange) // range bit 1 = LIMITED
    }

    func testHalfOrFewerBlocksIsDropped() {
        var au = sof(totalBlocks: 4)
        au += packet(blockIndex: 0)
        au += packet(blockIndex: 1)
        // 2 of 4 decoded = exactly half — upstream requires MORE than half.
        XCTAssertNil(WaveletBitstream.parse(au: Data(au), chunkAligned: false, windowSize: 0))
    }

    func testMissingSOFIsDropped() {
        let au = packet(blockIndex: 0) + packet(blockIndex: 1)
        XCTAssertNil(WaveletBitstream.parse(au: Data(au), chunkAligned: false, windowSize: 0))
    }

    func testTruncatedPacketIsRejected() {
        var au = sof(totalBlocks: 1)
        // Claims 4 payload words but only the 8-byte header follows.
        let word0 = UInt32(0) | (4 << 16) | (1 << 28)
        au += le32(word0) + le32(0)
        XCTAssertNil(WaveletBitstream.parse(au: Data(au), chunkAligned: false, windowSize: 0))
    }

    func testWindowWalkPackedFragAndMissingShard() throws {
        let size = 64
        // Window 1: SOF + one packet, PACKED. Window 2: a FRAG chain carrying one packet split
        // across two windows. Window 3: all zeros (a lost shard of a partial frame). Window 4:
        // a PACKED packet — the chain break must not eat it.
        let fragPacket = packet(blockIndex: 2)
        var au = window(sof(totalBlocks: 3) + packet(blockIndex: 0), kind: 0, size: size)
        au += window(Array(fragPacket[0..<5]), kind: 1, size: size)
        au += window(Array(fragPacket[5...]), kind: 3, size: size)
        au += [UInt8](repeating: 0, count: size) // missing shard
        au += window(packet(blockIndex: 1), kind: 0, size: size)
        let frame = try XCTUnwrap(
            WaveletBitstream.parse(au: Data(au), chunkAligned: true, windowSize: size))
        XCTAssertEqual(frame.decodedBlocks, 3)
        XCTAssertEqual(frame.offsets[0], 0)
        XCTAssertEqual(frame.offsets[2], 2)
        XCTAssertEqual(frame.offsets[1], 4)
    }

    func testBrokenFragChainIsDiscarded() throws {
        let size = 64
        let fragPacket = packet(blockIndex: 2)
        var au = window(sof(totalBlocks: 1) + packet(blockIndex: 0), kind: 0, size: size)
        au += window(Array(fragPacket[0..<5]), kind: 1, size: size)
        au += [UInt8](repeating: 0, count: size) // the chain's middle shard was lost
        au += window(Array(fragPacket[5...]), kind: 3, size: size) // dangling LAST — dropped
        let frame = try XCTUnwrap(
            WaveletBitstream.parse(au: Data(au), chunkAligned: true, windowSize: size))
        XCTAssertEqual(frame.decodedBlocks, 1)
        XCTAssertEqual(frame.offsets[2], UInt32.max)
    }
}

/// Golden-frame decode against the committed host-encoder fixtures. Skipped when the machine
/// has no Metal device (headless CI) — everywhere else this is the hand-ported kernels' guard.
final class PyroWaveGoldenTests: XCTestCase {
    private static let fixtureDir = "PyroWaveFixtures"

    private func fixture(_ name: String) throws -> Data {
        let url = try XCTUnwrap(
            Bundle.module.url(
                forResource: name, withExtension: "bin", subdirectory: Self.fixtureDir),
            "missing fixture \(name).bin — regenerate with pyrowave_dump_golden")
        return try Data(contentsOf: url)
    }

    /// Completion box — the decode callback lands on a Metal thread.
    private final class ResultBox: @unchecked Sendable {
        let lock = NSLock()
        var planes: WaveletPlanes?
    }

    /// Decode `au` synchronously and read all three planes back to CPU bytes.
    private func decode(
        au: Data, chunkAligned: Bool, windowSize: Int
    ) throws -> (y: [UInt8], cb: [UInt8], cr: [UInt8]) {
        let device = try XCTUnwrap(MTLCreateSystemDefaultDevice())
        let queue = try XCTUnwrap(device.makeCommandQueue())
        let decoder = try XCTUnwrap(MetalWaveletDecoder(device: device, queue: queue))
        let done = expectation(description: "decode completes")
        let box = ResultBox()
        let submitted = decoder.decode(
            au: au, chunkAligned: chunkAligned, windowSize: windowSize
        ) { planes in
            box.lock.lock()
            box.planes = planes
            box.lock.unlock()
            done.fulfill()
        }
        XCTAssertTrue(submitted, "the fixture AU must parse")
        wait(for: [done], timeout: 10)
        box.lock.lock()
        let result = box.planes
        box.lock.unlock()
        let planes = try XCTUnwrap(result, "the GPU pass must complete without error")
        return (
            try readback(planes.y, device: device, queue: queue),
            try readback(planes.cb, device: device, queue: queue),
            try readback(planes.cr, device: device, queue: queue)
        )
    }

    private func readback(
        _ texture: MTLTexture, device: MTLDevice, queue: MTLCommandQueue
    ) throws -> [UInt8] {
        let bytesPerRow = texture.width
        let length = bytesPerRow * texture.height
        let buffer = try XCTUnwrap(device.makeBuffer(length: length, options: .storageModeShared))
        let cmd = try XCTUnwrap(queue.makeCommandBuffer())
        let blit = try XCTUnwrap(cmd.makeBlitCommandEncoder())
        blit.copy(
            from: texture, sourceSlice: 0, sourceLevel: 0,
            sourceOrigin: MTLOrigin(x: 0, y: 0, z: 0),
            sourceSize: MTLSize(width: texture.width, height: texture.height, depth: 1),
            to: buffer, destinationOffset: 0, destinationBytesPerRow: bytesPerRow,
            destinationBytesPerImage: length)
        blit.endEncoding()
        cmd.commit()
        cmd.waitUntilCompleted()
        return [UInt8](UnsafeRawBufferPointer(start: buffer.contents(), count: length))
    }

    private func psnr(_ a: [UInt8], _ b: [UInt8]) -> Double {
        precondition(a.count == b.count)
        var sse = 0.0
        for i in 0..<a.count {
            let d = Double(a[i]) - Double(b[i])
            sse += d * d
        }
        if sse == 0 { return .infinity }
        let mse = sse / Double(a.count)
        return 10 * log10(255.0 * 255.0 / mse)
    }

    private func assertMatchesReference(
        _ decoded: (y: [UInt8], cb: [UInt8], cr: [UInt8]), prefix: String,
        file: StaticString = #filePath, line: UInt = #line
    ) throws {
        for (name, plane, ref) in [
            ("y", decoded.y, try fixture("\(prefix)-y")),
            ("cb", decoded.cb, try fixture("\(prefix)-cb")),
            ("cr", decoded.cr, try fixture("\(prefix)-cr")),
        ] {
            XCTAssertEqual(plane.count, ref.count, file: file, line: line)
            let db = psnr(plane, [UInt8](ref))
            print("pyrowave golden \(prefix) \(name): \(db) dB")
            // The Metal port and upstream's decoder run the same math at the same precision
            // tier; residual differences are float rounding + the gather/mirror edge handling.
            // Well-matched ports measure ≫50 dB; 45 catches a real divergence long before it
            // is visible.
            XCTAssertGreaterThan(db, 45.0, "plane PSNR \(db) dB", file: file, line: line)
        }
    }

    func testDenseGoldenFrame() throws {
        try XCTSkipIf(!MetalWaveletDecoder.supported, "no capable Metal device")
        let au = try fixture("au-dense")
        let decoded = try decode(au: au, chunkAligned: false, windowSize: 0)
        try assertMatchesReference(decoded, prefix: "ref-dense")
    }

    func testChunkAlignedGoldenFrame() throws {
        try XCTSkipIf(!MetalWaveletDecoder.supported, "no capable Metal device")
        let au = try fixture("au-chunked")
        let decoded = try decode(au: au, chunkAligned: true, windowSize: 1408)
        try assertMatchesReference(decoded, prefix: "ref-chunked")
    }

    /// 4:4:4: the chroma components run the full pyramid like luma (no level-0 skip, no
    /// early half-res emit) — the layout + dispatch structure Phase 4 added
    /// (design/pyrowave-444-hdr.md). The fixture comes from the 4:4:4 host encoder; the
    /// reference is upstream's own 4:4:4 decode (full-res chroma planes).
    func testDense444GoldenFrame() throws {
        try XCTSkipIf(!MetalWaveletDecoder.supported, "no capable Metal device")
        let au = try fixture("au-dense444")
        let decoded = try decode(au: au, chunkAligned: false, windowSize: 0)
        try assertMatchesReference(decoded, prefix: "ref-dense444")
    }

    /// Phase-4 partial delivery: zero a mid-AU window (a lost shard) — the frame must still
    /// decode (blocks > half) and stay recognizably the same picture (holes reconstruct as
    /// localized blur, not garbage).
    func testPartialFrameStillDecodes() throws {
        try XCTSkipIf(!MetalWaveletDecoder.supported, "no capable Metal device")
        var au = try fixture("au-chunked")
        let windows = au.count / 1408
        try XCTSkipIf(windows < 3, "fixture too small to punch a hole in")
        let hole = (windows / 2) * 1408
        au.replaceSubrange(hole..<(hole + 1408), with: [UInt8](repeating: 0, count: 1408))
        let decoded = try decode(au: au, chunkAligned: true, windowSize: 1408)
        let ref = try fixture("ref-chunked-y")
        let db = psnr(decoded.y, [UInt8](ref))
        XCTAssertGreaterThan(db, 25.0, "lossy frame should still resemble the source (\(db) dB)")
    }
}
#endif
