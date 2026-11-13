// PyroWave native Metal decoder: the Apple twin of the Vulkan decoder in
// crates/client/pf-client-video/src/video_pyrowave.rs, on the presenter's own MTLDevice so decode,
// CSC and present share one device. The packet walk ports pyrowave_decoder.cpp's
// push_packet/decode_packet; MetalWaveletShaders.swift hand-ports the vendored GLSL. A vendored
// PyroWave bump means re-diffing both decode shaders and the two 8-byte header structs.
//
// Wire shape (fixed by the host encoder, pf-encode's enc/linux/pyrowave.rs):
// • One AU is one frame of packets, each one 32x32 coefficient block for one (component, level,
//   band), sized by its 8-byte BitstreamHeader. A per-frame START_OF_FRAME header carries dims,
//   the block count and the VUI bits (4:2:0, BT.709/BT.2020, limited/full).
// • With `USER_FLAG_CHUNK_ALIGNED` the AU is whole `shard_payload` windows, each prefixed with
//   used-len u16 LE + kind u16 LE: kind 0 = whole packets, 1/2/3 = a FRAG chain. A lost shard
//   arrives as an all-zero window and is skipped; its blocks decode as zeros (local blur).
// • A frame with no SOF, or with half its blocks or fewer, is dropped, as upstream does.
//
// No copy of the AU: the core assembles it in a `WaveletAccessUnitPool` slot (one shared
// MTLBuffer), the parse records where each packet lies, and the dequant kernel reads it there;
// only a FRAG chain or a misaligned body moves, within the AU. GPU, per frame: one
// concurrent encoder for the ~42 dequant dispatches, then one per iDWT level (encoder
// boundaries stand in for Vulkan's barriers), into a ring of plane sets (the pipeline sizes it
// past its frame store, 4 at least) that stays alive while the presenter samples it.

#if canImport(Metal)
import Foundation
import Metal
import os

private let waveletLog = ClientLog(category: "pyrowave")

/// The per-(component, level, band) 32x32-block table — the exact Swift port of
/// `WaveletBuffers::init_block_meta` (pyrowave_common.cpp): the walk order (level 4→0,
/// component 0→2 skipping level-0 chroma in 4:2:0, band (level==4 ? 0 : 1)→3) DEFINES the
/// global `block_index` space the wire packets address, so it must match the encoder exactly.
struct WaveletLayout {
    static let decompositionLevels = 5
    static let alignment = 32
    static let minimumImageSize = 128

    let width: Int
    let height: Int
    /// Full-res chroma (4:4:4): chroma components get the full band set including level 0,
    /// exactly like luma — upstream `init_block_meta` with `Chroma444`.
    let chroma444: Bool
    let alignedWidth: Int
    let alignedHeight: Int
    /// blockMeta[component][level][band] = (blockOffset32x32, blockStride32x32); -1 offset =
    /// band not coded (level-0 chroma in 4:2:0).
    let blockMeta: [[[(offset: Int, stride: Int)]]]
    let blockCount32: Int

    /// Band-image extent at `level` — mip `level` of the (aligned/2)-sized coefficient image.
    /// Exact halving: the aligned dims are 32-aligned, so /2 is 16-aligned and survives 4 shifts.
    func levelWidth(_ level: Int) -> Int { (alignedWidth / 2) >> level }
    func levelHeight(_ level: Int) -> Int { (alignedHeight / 2) >> level }

    init(width: Int, height: Int, chroma444: Bool) {
        self.width = width
        self.height = height
        self.chroma444 = chroma444
        let align = { (v: Int) in
            max((v + Self.alignment - 1) & ~(Self.alignment - 1), Self.minimumImageSize)
        }
        alignedWidth = align(width)
        alignedHeight = align(height)

        var meta = [[[(offset: Int, stride: Int)]]](
            repeating: [[(offset: Int, stride: Int)]](
                repeating: [(offset: Int, stride: Int)](repeating: (-1, 0), count: 4),
                count: Self.decompositionLevels),
            count: 3)
        var count32 = 0
        let aw = alignedWidth
        let ah = alignedHeight
        for level in stride(from: Self.decompositionLevels - 1, through: 0, by: -1) {
            for component in 0..<3 {
                if level == 0 && component != 0 && !chroma444 { continue } // 4:2:0: no top-level chroma
                for band in (level == Self.decompositionLevels - 1 ? 0 : 1)..<4 {
                    let levelW = (aw / 2) >> level
                    let levelH = (ah / 2) >> level
                    let blocksX8 = (levelW + 7) / 8
                    let blocksY8 = (levelH + 7) / 8
                    let blocksX32 = (levelW + 31) / 32
                    meta[component][level][band] = (count32, blocksX32)
                    // accumulate_block_mapping's 32x32 count.
                    count32 += ((blocksX8 + 3) / 4) * ((blocksY8 + 3) / 4)
                }
            }
        }
        blockMeta = meta
        blockCount32 = count32
    }
}

/// One parsed frame, CPU side: where each block's packet lies in the pool buffer the dequant
/// kernel reads, plus the sequence header's facts.
struct ParsedWaveletFrame {
    var layout: WaveletLayout
    /// Per 32x32 block: u32 word offset of its packet (header included) from the pool
    /// buffer's start, or UInt32.max = block missing.
    var offsets: [UInt32]
    var totalBlocks: Int
    var decodedBlocks: Int
    /// VUI bits from the sequence header (BitstreamSequenceHeader).
    var bt2020: Bool
    /// PQ transfer ⇒ HDR session: 16-bit studio-code planes + EDR present. The host stamps it
    /// only on a 10-bit session; 10-bit without it is SDR (`WaveletPlanes.tenBitSDR`).
    var pq: Bool
    var fullRange: Bool

    /// The frame's Y′CbCr→RGB signal for the presenter's planar CSC. PyroWave today is always
    /// BT.709 limited (the host's fixed contract), but the sequence header signals it, so honor
    /// what it says.
    var cscSignal: CscRows.Signal {
        CscRows.Signal(matrix: bt2020 ? 9 : 1, fullRange: fullRange)
    }
}

enum WaveletBitstream {
    /// Window kinds of the chunk-aligned framing (host WIN_* constants).
    private static let winPacked: UInt16 = 0
    private static let winFragFirst: UInt16 = 1
    private static let winFragCont: UInt16 = 2
    private static let winFragLast: UInt16 = 3

    /// Parse one AU where it lies: `count` bytes at `base`, `wordBase` u32 words into the
    /// buffer the dequant kernel reads. `windowSize` > 0 with `chunkAligned` walks the
    /// shard-window framing, moving packets within the AU; otherwise the AU is one packet
    /// stream. nil = drop the frame (malformed, no SOF, or not enough blocks survived loss —
    /// upstream's `decoded_blocks > total/2` partial rule).
    static func parse(
        base: UnsafeMutablePointer<UInt8>, count: Int, wordBase: Int,
        chunkAligned: Bool, windowSize: Int
    ) -> ParsedWaveletFrame? {
        var state = ParseState()
        let ok: Bool
        if chunkAligned, windowSize >= 8 {
            // Whole windows only; a trailing partial window would be a framing bug.
            guard count % windowSize == 0 else { return nil }
            ok = walkWindows(
                base: base, count: count, wordBase: wordBase, windowSize: windowSize,
                state: &state)
        } else {
            ok = state.pushPackets(UnsafeBufferPointer(start: base, count: count), wordBase: wordBase)
        }
        guard ok, let frame = state.finish() else { return nil }
        // Upstream's partial rule: with no SOF the frame is undecodable; at half the
        // blocks or fewer it is presumed garbage.
        guard frame.totalBlocks > 0, frame.decodedBlocks > frame.totalBlocks / 2 else {
            return nil
        }
        return frame
    }

    /// The shard-window walk: used u16 + kind u16, then whole packets (PACKED) or one piece of
    /// a FRAG chain. A body off 4-byte alignment slides back into its own window prefix and a
    /// chain closes up over the prefixes it spans, so every packet is read inside the AU. A
    /// zeroed (lost) window drops itself and breaks any chain across it (mirrors
    /// video_pyrowave.rs push_in_place).
    private static func walkWindows(
        base: UnsafeMutablePointer<UInt8>, count: Int, wordBase: Int, windowSize: Int,
        state: inout ParseState
    ) -> Bool {
        // The open FRAG chain: where it starts and where its next body goes.
        var chain: (at: Int, end: Int)?
        func push(_ at: Int, _ end: Int) -> Bool {
            state.pushPackets(
                UnsafeBufferPointer(start: base + at, count: end - at), wordBase: wordBase + at / 4)
        }
        var pos = 0
        while pos < count {
            let win = base + pos
            let bodyPos = pos + 4
            pos += windowSize
            let len = Int(win[0]) | (Int(win[1]) << 8)
            let kind = UInt16(win[2]) | (UInt16(win[3]) << 8)
            guard len > 0, 4 + len <= windowSize else {
                chain = nil
                continue
            }
            func slide(to: Int) -> Int {
                if to != bodyPos {
                    UnsafeMutableRawPointer(base + to).copyMemory(from: base + bodyPos, byteCount: len)
                }
                return to + len
            }
            let low = bodyPos & ~3
            switch kind {
            case winPacked:
                chain = nil
                guard push(low, slide(to: low)) else { return false }
            case winFragFirst:
                chain = (low, slide(to: low))
            case winFragCont:
                if let open = chain { chain = (open.at, slide(to: open.end)) }
            case winFragLast:
                if let open = chain {
                    chain = nil
                    guard push(open.at, slide(to: open.end)) else { return false }
                }
            default:
                chain = nil
            }
        }
        return true
    }

    /// Streaming packet-walk state (pyrowave_decoder.cpp push_packet + decode_packet). The
    /// SOF sequence header arrives first in every host AU, which fixes the dims → layout →
    /// offset-table size before any coefficient packet lands; a coefficient packet before the
    /// SOF (its window was lost) is skipped — its block just stays missing.
    private struct ParseState {
        var layout: WaveletLayout?
        var offsets: [UInt32] = []
        var totalBlocks = 0
        var decodedBlocks = 0
        var bt2020 = false
        var pq = false
        var fullRange = false
        var sawSOF = false

        /// Walk the packets in `buf`, which starts `wordBase` words into the pool buffer and
        /// 4-byte aligned, recording where each block's packet lies.
        mutating func pushPackets(_ buf: UnsafeBufferPointer<UInt8>, wordBase: Int) -> Bool {
            guard let base = buf.baseAddress else { return true }
            var pos = 0
            let count = buf.count
            while count - pos >= 8 {
                let word0 = loadWord(base, pos)
                let word1 = loadWord(base, pos + 4)
                let extended = (word0 >> 31) & 1
                if extended != 0 {
                    // BitstreamSequenceHeader: w-1[0:14] h-1[14:28] seq[28:31] ext[31];
                    // total[0:24] code[24:26] chroma[26] prim[27] trc[28] mtx[29] range[30]
                    // siting[31].
                    let code = (word1 >> 24) & 0x3
                    guard code == 0 else { return false } // only START_OF_FRAME is defined
                    let chroma444 = (word1 >> 26) & 1 != 0
                    let w = Int(word0 & 0x3fff) + 1
                    let h = Int((word0 >> 14) & 0x3fff) + 1
                    // The fields reach 16384 a side, and a ring of planes that size is
                    // gigabytes. The host's own per-axis wall is the limit here too.
                    let side = RenderScale.maxDimension(codec: "pyrowave")
                    guard w >= 2, h >= 2, w <= side, h <= side,
                          chroma444 || (w % 2 == 0 && h % 2 == 0)
                    else { return false }
                    if sawSOF {
                        // One frame, one geometry — a second SOF must agree.
                        guard layout?.width == w, layout?.height == h,
                              layout?.chroma444 == chroma444
                        else { return false }
                    } else {
                        sawSOF = true
                        let l = WaveletLayout(width: w, height: h, chroma444: chroma444)
                        layout = l
                        offsets = [UInt32](repeating: .max, count: l.blockCount32)
                        totalBlocks = Int(word1 & 0xff_ffff)
                        bt2020 = (word1 >> 29) & 1 != 0
                        // transfer_function bit: PQ ⇒ an HDR session (16-bit studio-code
                        // planes by the negotiated coupling — design/pyrowave-444-hdr.md).
                        pq = (word1 >> 28) & 1 != 0
                        fullRange = (word1 >> 30) & 1 == 0 // YCBCR_RANGE_FULL = 0
                    }
                    pos += 8
                    continue
                }
                // BitstreamHeader: ballot[0:16] payload_words[16:28] seq[28:31] ext[31];
                // quant_code[0:8] block_index[8:32]. payload_words counts u32s INCLUDING the
                // 8-byte header.
                let payloadWords = Int((word0 >> 16) & 0xfff)
                guard payloadWords >= 2, pos + payloadWords * 4 <= count else { return false }
                let blockIndex = Int(word1 >> 8)
                if let layout, blockIndex < layout.blockCount32 {
                    // First write wins (duplicate packets are ignored, like upstream).
                    if offsets[blockIndex] == .max {
                        offsets[blockIndex] = UInt32(wordBase + pos / 4)
                        decodedBlocks += 1
                    }
                } else if layout != nil {
                    return false // out-of-bounds block index — corrupt stream
                }
                // No layout yet (SOF lost): skip the packet, the block stays missing.
                pos += payloadWords * 4
            }
            // In the windowed framing, `used` delimits exactly; dense AUs must also consume
            // fully (upstream errors on trailing bytes).
            return pos == count
        }

        private func loadWord(_ base: UnsafePointer<UInt8>, _ offset: Int) -> UInt32 {
            UInt32(base[offset])
                | (UInt32(base[offset + 1]) << 8)
                | (UInt32(base[offset + 2]) << 16)
                | (UInt32(base[offset + 3]) << 24)
        }

        func finish() -> ParsedWaveletFrame? {
            guard let layout else { return nil }
            return ParsedWaveletFrame(
                layout: layout, offsets: offsets,
                totalBlocks: totalBlocks, decodedBlocks: decodedBlocks,
                bt2020: bt2020, pq: pq, fullRange: fullRange)
        }
    }
}

/// Access-unit memory the core assembles PyroWave frames in: one shared MTLBuffer the dequant
/// kernel reads where the reassembler wrote, cut into slots of an AU and the kernel's
/// read-past-the-end slack. A slot is free once the core released it and no decode reads it.
public final class WaveletAccessUnitPool: AccessUnitMemory {
    /// Past an AU: room for what the dequant kernel reads beyond a packet's own bytes. A
    /// well-formed packet overshoots by one 64-bit sign load. A malformed one can send the
    /// kernel about 2.5 KB past its header: 16 blocks of at most 144 bytes, then the sign
    /// stream. One page keeps every such read inside the buffer. 4096-byte slot strides keep
    /// every AU 4-byte aligned.
    static let slackBytes = 4096
    /// AUs in flight at once: a few reassembling, one queued, the decodes still reading.
    static let slots = 8

    let buffer: MTLBuffer
    /// Bytes an AU may take.
    let capacity: Int
    let stride: Int
    private let lock = NSLock()
    private var lentToCore = [Bool](repeating: false, count: WaveletAccessUnitPool.slots)
    private var readByDecode = [Bool](repeating: false, count: WaveletAccessUnitPool.slots)
    private var free = Array((0..<WaveletAccessUnitPool.slots).reversed())

    /// Room for an AU at `width`×`height`: 6 bits a pixel at 4:2:0, 9 at 4:4:4 (the host's
    /// 4.0 bpp ceiling plus window padding), as the Vulkan ring sizes it; 1 MiB at least.
    static func capacity(width: Int, height: Int, chroma444: Bool) -> Int {
        let px = max(width, 1) * max(height, 1)
        let bytes = chroma444 ? px * 9 / 8 : px * 3 / 4
        return max((bytes + 4095) & ~4095, 1 << 20)
    }

    init?(device: MTLDevice, capacity: Int) {
        self.capacity = (capacity + 3) & ~3
        stride = (self.capacity + Self.slackBytes + 4095) & ~4095
        guard let buffer = device.makeBuffer(length: stride * Self.slots, options: .storageModeShared)
        else { return nil }
        buffer.label = "pyrowave AU pool"
        self.buffer = buffer
    }

    /// Byte offset of `token`'s slot, or nil for a token that is not this pool's.
    func slotStart(_ token: UInt64) -> Int? {
        token < UInt64(Self.slots) ? Int(token) * stride : nil
    }

    public func take(_ count: Int) -> (base: UnsafeMutableRawPointer, count: Int, token: UInt64)? {
        guard count <= capacity else { return nil }
        lock.lock()
        defer { lock.unlock() }
        guard let index = free.popLast() else { return nil }
        lentToCore[index] = true
        return (buffer.contents() + index * stride, capacity, UInt64(index))
    }

    public func release(_ token: UInt64) {
        guard token < UInt64(Self.slots) else { return }
        let index = Int(token)
        lock.lock()
        lentToCore[index] = false
        if !readByDecode[index] { free.append(index) }
        lock.unlock()
    }

    public func locate(_ pointer: UnsafeRawPointer) -> (token: UInt64, offset: Int)? {
        let base = UnsafeRawPointer(buffer.contents())
        guard pointer >= base, pointer < base + stride * Self.slots else { return nil }
        let offset = base.distance(to: pointer)
        return (UInt64(offset / stride), offset)
    }

    /// A slot for an AU that lies elsewhere (the heap, a fixture), copied in and held for a
    /// decode; nil when it does not fit or no slot is free. `decode` frees it.
    func stage(_ au: Data) -> AccessUnit.InPlace? {
        guard au.count <= capacity else { return nil }
        lock.lock()
        guard let index = free.popLast() else {
            lock.unlock()
            return nil
        }
        readByDecode[index] = true
        lock.unlock()
        au.withUnsafeBytes { src in
            if let from = src.baseAddress, !src.isEmpty {
                (buffer.contents() + index * stride).copyMemory(from: from, byteCount: src.count)
            }
        }
        return AccessUnit.InPlace(token: UInt64(index), offset: index * stride, count: au.count)
    }

    /// A decode reads `token`'s slot from now until `done`.
    func reading(_ token: UInt64) {
        guard token < UInt64(Self.slots) else { return }
        lock.lock()
        readByDecode[Int(token)] = true
        lock.unlock()
    }

    /// The decode reading `token`'s slot is over.
    func done(_ token: UInt64) {
        guard token < UInt64(Self.slots) else { return }
        let index = Int(token)
        lock.lock()
        readByDecode[index] = false
        if !lentToCore[index] { free.append(index) }
        lock.unlock()
    }
}

/// One decoded frame's output planes, handed to the presenter's planar render path. The
/// textures belong to a slot of the decoder's ring. While a copy of this value lives, `lease`
/// keeps the decoder out of that slot; a render already committed is ordered ahead of the
/// next decode by the queue they share. Public because it rides inside `ReadyImage`.
public struct WaveletPlanes: @unchecked Sendable {
    public let y: MTLTexture
    public let cb: MTLTexture
    public let cr: MTLTexture
    public let csc: CscUniform
    /// PQ (HDR) stream: the presenter picks the HDR/tone-map planar pipeline + EDR config.
    public let pq: Bool
    /// The ring slot's lease, nil for planes no decoder owns. Only its reference count
    /// matters: the decoder reuses a slot once nothing else holds this.
    var lease: AnyObject?
    /// 10-bit without PQ: the planes are 16-bit, and the presenter takes the 10-bit drawable.
    public var tenBitSDR: Bool { !pq && y.pixelFormat == .r16Unorm }
    public var width: Int { y.width }
    public var height: Int { y.height }
}

public final class MetalWaveletDecoder {
    /// Slots in the output ring. A slot's planes stay live while its frame waits in the present
    /// store or sits with the render thread, so the pipeline sizes this past that store.
    private let ringDepth: Int

    /// Device-capability gate for advertisement (SessionModel) and the settings picker: the
    /// dequant kernel needs simdgroup prefix sums with its 16 header lanes inside one
    /// simdgroup, so compile the real kernels once and check the pipeline facts. Apple6 (A13)
    /// and every Mac2 device pass the family check; the compile probe is authoritative.
    public static let supported: Bool = {
        guard let device = MTLCreateSystemDefaultDevice() else { return false }
        guard device.supportsFamily(.apple6) || device.supportsFamily(.mac2) else { return false }
        do {
            let lib = try device.makeLibrary(source: waveletShaderSource, options: nil)
            guard let dequant = lib.makeFunction(name: "wavelet_dequant") else { return false }
            let p = try device.makeComputePipelineState(function: dequant)
            var shift = false
            let fc = MTLFunctionConstantValues()
            fc.setConstantValue(&shift, type: .bool, index: 0)
            _ = try lib.makeFunction(name: "idwt", constantValues: fc)
            return p.threadExecutionWidth >= 16 && p.maxTotalThreadsPerThreadgroup >= 128
        } catch {
            waveletLog.info("pyrowave probe: kernels rejected (\(error, privacy: .public))")
            return false
        }
    }()

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private let dequantPipeline: MTLComputePipelineState
    private let idwtPipeline: MTLComputePipelineState
    private let idwtShiftPipeline: MTLComputePipelineState
    private let mirrorSampler: MTLSamplerState
    /// Where the AUs lie. The pump swaps in a bigger one when a mode outgrows it; a decode
    /// in flight keeps the old one through its completion. PUMP THREAD.
    var pool: WaveletAccessUnitPool

    // Size-dependent state, rebuilt when the SOF dims change (this is also the mid-stream
    // Reconfigure/resize path — the wavelet decoder is fixed-size per geometry).
    private var layout: WaveletLayout?
    /// coefficients[component][level]: 4-slice R16Float (levels 0–1) / R32Float (levels 2–4)
    /// texture2d_array — the band images (precision-1 split, see MetalWaveletShaders).
    private var coefficients: [[MTLTexture]] = []
    /// llViews[component][level]: slice-0 (LL band) 2D write view of `coefficients` — the iDWT
    /// output target chaining level L+1 into level L.
    private var llViews: [[MTLTexture]] = []

    private final class SlotLease {}

    private struct Slot {
        var y: MTLTexture
        var cb: MTLTexture
        var cr: MTLTexture
        var offsets: MTLBuffer
        /// Held by every `WaveletPlanes` of this slot: a decode in flight, a frame in the
        /// present store, one the render thread is drawing. Free when only the ring holds it.
        var lease = SlotLease()
    }

    private var slots: [Slot] = []
    private var nextSlot = 0
    /// One permit per ring slot — see the wait in `decode`.
    private let ringSlots: DispatchSemaphore
    /// The ring's plane format facts (from the last SOF): 10-bit ⇒ 16-bit UNORM planes.
    private var hdr16 = false
    /// The session's negotiated depth. The header has no depth bit, and a Linux host sends
    /// 10-bit SDR, so PQ alone does not decide the plane depth.
    private let tenBit: Bool

    /// The current geometry (from the last SOF that built the resources) — the pump reports
    /// decoded-size changes to the resize overlay from this. PUMP THREAD.
    var decodedSize: (width: Int, height: Int)? {
        layout.map { ($0.width, $0.height) }
    }

    /// The pump thread owns `decode`; everything mutable is confined to it. `tenBit` is the
    /// session's negotiated depth (`connection.bitDepth >= 10`); `pool` is where the core
    /// assembles the AUs, or a 4 MiB one of the decoder's own for AUs handed in as `Data`.
    init?(
        device: MTLDevice, queue: MTLCommandQueue, tenBit: Bool = false, ringDepth: Int = 4,
        pool: WaveletAccessUnitPool? = nil
    ) {
        self.device = device
        self.queue = queue
        self.tenBit = tenBit
        self.ringDepth = max(2, ringDepth)
        self.ringSlots = DispatchSemaphore(value: self.ringDepth)
        guard let pool = pool ?? WaveletAccessUnitPool(device: device, capacity: 4 << 20) else {
            return nil
        }
        self.pool = pool
        do {
            let lib = try device.makeLibrary(source: waveletShaderSource, options: nil)
            guard let dequantFn = lib.makeFunction(name: "wavelet_dequant") else { return nil }
            dequantPipeline = try device.makeComputePipelineState(function: dequantFn)
            var shift = false
            let fcOff = MTLFunctionConstantValues()
            fcOff.setConstantValue(&shift, type: .bool, index: 0)
            idwtPipeline = try device.makeComputePipelineState(
                function: try lib.makeFunction(name: "idwt", constantValues: fcOff))
            shift = true
            let fcOn = MTLFunctionConstantValues()
            fcOn.setConstantValue(&shift, type: .bool, index: 0)
            idwtShiftPipeline = try device.makeComputePipelineState(
                function: try lib.makeFunction(name: "idwt", constantValues: fcOn))
        } catch {
            waveletLog.error("pyrowave: pipeline build failed (\(error, privacy: .public))")
            return nil
        }
        guard dequantPipeline.threadExecutionWidth >= 16,
              dequantPipeline.maxTotalThreadsPerThreadgroup >= 128
        else { return nil }
        // Upstream's mirror_repeat_sampler: mirrored repeat, NEAREST everything, normalized
        // coords — the idwt gather footprint + coordinate nudge depend on exactly this.
        let samp = MTLSamplerDescriptor()
        samp.sAddressMode = .mirrorRepeat
        samp.tAddressMode = .mirrorRepeat
        samp.minFilter = .nearest
        samp.magFilter = .nearest
        samp.mipFilter = .notMipmapped
        samp.normalizedCoordinates = true
        guard let sampler = device.makeSamplerState(descriptor: samp) else { return nil }
        mirrorSampler = sampler
    }

    /// `decode(_:)` of an AU in its own memory: copied into a pool slot first. Tests, and the
    /// pump's fallback for an AU the core assembled elsewhere.
    func decode(
        au: Data, chunkAligned: Bool, windowSize: Int,
        completion: @escaping @Sendable (WaveletPlanes?) -> Void
    ) -> Bool {
        guard let placed = pool.stage(au) else { return false }
        return decode(placed, chunkAligned: chunkAligned, windowSize: windowSize, completion: completion)
    }

    /// Decode one AU where it lies in `pool`. Synchronous CPU parse + async GPU decode:
    /// returns false when the frame was dropped (malformed / SOF lost / not enough blocks);
    /// on true, `completion` fires on a Metal callback thread once the planes are decoded
    /// (nil = the GPU pass errored). The slot goes back to the pool either way. PUMP THREAD only.
    func decode(
        _ placed: AccessUnit.InPlace, chunkAligned: Bool, windowSize: Int,
        completion: @escaping @Sendable (WaveletPlanes?) -> Void
    ) -> Bool {
        let pool = self.pool
        guard let slotStart = pool.slotStart(placed.token),
              placed.offset % 4 == 0, placed.offset >= slotStart,
              placed.count <= slotStart + pool.capacity - placed.offset
        else { return false }
        pool.reading(placed.token)
        var committed = false
        defer { if !committed { pool.done(placed.token) } }

        guard
            let frame = WaveletBitstream.parse(
                base: (pool.buffer.contents() + placed.offset).assumingMemoryBound(to: UInt8.self),
                count: placed.count, wordBase: placed.offset / 4,
                chunkAligned: chunkAligned, windowSize: windowSize)
        else { return false }

        let wide = tenBit || frame.pq
        if layout?.width != frame.layout.width || layout?.height != frame.layout.height
            || layout?.chroma444 != frame.layout.chroma444 || hdr16 != wide {
            guard rebuild(layout: frame.layout, hdr16: wide) else { return false }
        }
        guard let layout, !slots.isEmpty else { return false }

        // Bound in-flight decodes to the ring's depth. The offset buffers are `.storageModeShared`
        // and re-filled by the CPU below, and Metal's hazard tracking orders GPU work against GPU
        // work only — it does not stop this write landing while an earlier dispatch is still
        // reading the same slot. Timed, so a wedged GPU drops a frame instead of parking the pump
        // thread forever.
        guard ringSlots.wait(timeout: .now() + .milliseconds(250)) == .success else { return false }
        var submitted = false
        defer { if !submitted { ringSlots.signal() } }

        // The next slot no frame still holds. A slot rewritten while its frame waits in the
        // store or with the render thread shows that frame with a later picture. None free
        // drops this frame: the GPU or the presenter is several frames behind.
        var free: Int?
        for step in 0..<slots.count {
            let index = (nextSlot + step) % slots.count
            if isKnownUniquelyReferenced(&slots[index].lease) {
                free = index
                break
            }
        }
        guard let free else { return false }
        let slot = slots[free]
        frame.offsets.withUnsafeBytes { src in
            slot.offsets.contents().copyMemory(
                from: src.baseAddress!, byteCount: min(src.count, slot.offsets.length))
        }

        guard let cmd = queue.makeCommandBuffer() else { return false }

        // Stage 1: dequant — every (component, level, band) block grid in one concurrent
        // encoder (each dispatch writes its own band layer; no intra-stage hazards, exactly
        // like the barrier-free Vulkan dispatch loop). The payload is the pool buffer itself.
        guard let dequant = cmd.makeComputeCommandEncoder(dispatchType: .concurrent) else {
            return false
        }
        dequant.label = "pyrowave dequant"
        dequant.setComputePipelineState(dequantPipeline)
        dequant.setBuffer(slot.offsets, offset: 0, index: 0)
        dequant.setBuffer(pool.buffer, offset: 0, index: 1)
        for level in 0..<WaveletLayout.decompositionLevels {
            for component in 0..<3 {
                if level == 0 && component != 0 && !layout.chroma444 { continue } // 4:2:0
                for band in (level == WaveletLayout.decompositionLevels - 1 ? 0 : 1)..<4 {
                    let meta = layout.blockMeta[component][level][band]
                    let w = layout.levelWidth(level)
                    let h = layout.levelHeight(level)
                    var regs = DequantRegisters(
                        resolution: SIMD2(Int32(w), Int32(h)),
                        outputLayer: Int32(band),
                        blockOffset32x32: Int32(meta.offset),
                        blockStride32x32: Int32(meta.stride))
                    dequant.setTexture(coefficients[component][level], index: 0)
                    dequant.setBytes(
                        &regs, length: MemoryLayout<DequantRegisters>.stride, index: 2)
                    dequant.dispatchThreadgroups(
                        MTLSize(width: (w + 31) / 32, height: (h + 31) / 32, depth: 1),
                        threadsPerThreadgroup: MTLSize(width: 128, height: 1, depth: 1))
                }
            }
        }
        dequant.endEncoding()

        // Stage 2: iDWT, coarsest level in — one encoder per level; the encoder boundary is
        // the write→sampled-read barrier chaining each level's LL into the next.
        for inputLevel in stride(from: WaveletLayout.decompositionLevels - 1, through: 0, by: -1) {
            guard let idwt = cmd.makeComputeCommandEncoder(dispatchType: .concurrent) else {
                return false
            }
            idwt.label = "pyrowave idwt L\(inputLevel)"
            idwt.setSamplerState(mirrorSampler, index: 0)
            // Resolution rides TRANSPOSED (the kernel transposes on load and store).
            let rx = layout.levelHeight(inputLevel)
            let ry = layout.levelWidth(inputLevel)
            var regs = IdwtRegisters(
                resolution: SIMD2(Int32(rx), Int32(ry)),
                invResolution: SIMD2(1.0 / Float(rx), 1.0 / Float(ry)))
            idwt.setBytes(&regs, length: MemoryLayout<IdwtRegisters>.stride, index: 0)
            let grid = MTLSize(width: (rx + 15) / 16, height: (ry + 15) / 16, depth: 1)
            let group = MTLSize(width: 64, height: 1, depth: 1)
            if inputLevel == 0 {
                // Final full-res pass: luma only in 4:2:0 (chroma finished at level 1); all
                // three components in 4:4:4 (chroma runs the full pyramid like luma).
                idwt.setComputePipelineState(idwtShiftPipeline)
                let components = layout.chroma444 ? 3 : 1
                for component in 0..<components {
                    idwt.setTexture(coefficients[component][0], index: 0)
                    let out = component == 0 ? slot.y : (component == 1 ? slot.cb : slot.cr)
                    idwt.setTexture(out, index: 1)
                    idwt.dispatchThreadgroups(grid, threadsPerThreadgroup: group)
                }
            } else {
                for component in 0..<3 {
                    idwt.setTexture(coefficients[component][inputLevel], index: 0)
                    if component != 0 && inputLevel == 1 && !layout.chroma444 {
                        // 4:2:0 chroma emits its final half-res plane one level early.
                        idwt.setComputePipelineState(idwtShiftPipeline)
                        idwt.setTexture(component == 1 ? slot.cb : slot.cr, index: 1)
                    } else {
                        idwt.setComputePipelineState(idwtPipeline)
                        idwt.setTexture(llViews[component][inputLevel - 1], index: 1)
                    }
                    idwt.dispatchThreadgroups(grid, threadsPerThreadgroup: group)
                }
            }
            idwt.endEncoding()
        }

        let planes = WaveletPlanes(
            y: slot.y, cb: slot.cb, cr: slot.cr,
            csc: CscRows.rows(frame.cscSignal, depth: wide ? 10 : 8, msbPacked: wide),
            pq: frame.pq, lease: slot.lease)
        // Captured directly: the handler must not retain the decoder.
        let slotReleased = ringSlots
        let token = placed.token
        cmd.addCompletedHandler { buffer in
            slotReleased.signal()
            pool.done(token)
            completion(buffer.error == nil ? planes : nil)
        }
        cmd.commit()
        submitted = true
        committed = true
        nextSlot = (free + 1) % slots.count
        return true
    }

    /// (Re)allocate every size-dependent resource for `layout`'s geometry. Also the mid-stream
    /// resize path: a Reconfigure shows up here as new SOF dims.
    private func rebuild(layout newLayout: WaveletLayout, hdr16 newHdr16: Bool) -> Bool {
        waveletLog.info(
            "pyrowave: building decoder \(newLayout.width)x\(newLayout.height) (aligned \(newLayout.alignedWidth)x\(newLayout.alignedHeight), \(newLayout.blockCount32) blocks, \(newLayout.chroma444 ? "4:4:4" : "4:2:0", privacy: .public)\(newHdr16 ? " HDR16" : "", privacy: .public))")
        var coeff: [[MTLTexture]] = []
        var lls: [[MTLTexture]] = []
        for component in 0..<3 {
            var perLevel: [MTLTexture] = []
            var perLevelLL: [MTLTexture] = []
            for level in 0..<WaveletLayout.decompositionLevels {
                let desc = MTLTextureDescriptor()
                desc.textureType = .type2DArray
                desc.arrayLength = 4
                // Upstream precision 1: fp16 storage for the two finest levels, fp32 for the
                // coarse levels whose values feed every later reconstruction step.
                desc.pixelFormat = level < 2 ? .r16Float : .r32Float
                desc.width = newLayout.levelWidth(level)
                desc.height = newLayout.levelHeight(level)
                desc.usage = [.shaderRead, .shaderWrite]
                desc.storageMode = .private
                guard let tex = device.makeTexture(descriptor: desc) else { return false }
                tex.label = "pyrowave coeff c\(component) L\(level)"
                guard
                    let ll = tex.makeTextureView(
                        pixelFormat: desc.pixelFormat, textureType: .type2D,
                        levels: 0..<1, slices: 0..<1)
                else { return false }
                ll.label = "pyrowave LL c\(component) L\(level)"
                perLevel.append(tex)
                perLevelLL.append(ll)
            }
            coeff.append(perLevel)
            lls.append(perLevelLL)
        }

        var newSlots: [Slot] = []
        for i in 0..<ringDepth {
            let planeFormat: MTLPixelFormat = newHdr16 ? .r16Unorm : .r8Unorm
            let plane = { (w: Int, h: Int, name: String) -> MTLTexture? in
                let desc = MTLTextureDescriptor.texture2DDescriptor(
                    pixelFormat: planeFormat, width: w, height: h, mipmapped: false)
                desc.usage = [.shaderRead, .shaderWrite]
                desc.storageMode = .private
                let t = self.device.makeTexture(descriptor: desc)
                t?.label = name
                return t
            }
            let cw = newLayout.chroma444 ? newLayout.width : newLayout.width / 2
            let ch = newLayout.chroma444 ? newLayout.height : newLayout.height / 2
            guard
                let y = plane(newLayout.width, newLayout.height, "pyrowave Y[\(i)]"),
                let cb = plane(cw, ch, "pyrowave Cb[\(i)]"),
                let cr = plane(cw, ch, "pyrowave Cr[\(i)]"),
                let offsets = device.makeBuffer(
                    length: max(newLayout.blockCount32 * 4, 4), options: .storageModeShared)
            else { return false }
            newSlots.append(Slot(y: y, cb: cb, cr: cr, offsets: offsets))
        }

        coefficients = coeff
        llViews = lls
        slots = newSlots
        nextSlot = 0
        layout = newLayout
        hdr16 = newHdr16
        return true
    }

    // MSL-side layouts (MetalWaveletShaders.swift) — keep in lockstep.
    private struct DequantRegisters {
        var resolution: SIMD2<Int32>
        var outputLayer: Int32
        var blockOffset32x32: Int32
        var blockStride32x32: Int32
    }

    private struct IdwtRegisters {
        var resolution: SIMD2<Int32>
        var invResolution: SIMD2<Float>
    }
}
#endif
