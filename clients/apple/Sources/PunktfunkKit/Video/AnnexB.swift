// Annex-B (HEVC / H.264) → CoreMedia plumbing.
//
// The punktfunk host emits Annex-B access units with in-band parameter sets on every IDR
// (deliberately — the client needs no out-of-band extradata). VideoToolbox wants the AVCC
// flavor instead: a CMVideoFormatDescription built from the parameter sets, and sample
// buffers whose NALs are 4-byte-length-prefixed. This file converts between the two, for
// the codec the host resolved in the Welcome (`connection.videoCodec`) — HEVC and H.264
// differ only in NAL-header layout and which parameter sets exist (HEVC adds a VPS). AV1
// is not an Annex-B/NAL codec and isn't handled here — its OBU flavor of the same plumbing
// lives in AV1.swift, and the pumps reach both through `VideoCodec`'s dispatching
// `formatDescription(fromKeyframe:)` / `sampleBuffer(au:format:)`, so nothing below is ever
// called with `.av1`.
//
// HOT PATH: both pumps run `formatDescription(fromIDR:codec:)` + `sampleBuffer(au:format:codec:)`
// once per AU, so the conversion is built on `forEachNAL` — a zero-copy scan over the AU's bytes
// (ranges, not materialized Datas) — and `sampleBuffer` packs the AVCC form straight into
// the CMBlockBuffer's own allocation. Per AU that leaves exactly one copy here (source →
// block buffer) instead of the naive scan-copy-slice-repack chain.

import CoreMedia
import Foundation

/// The video codec of the host's elementary stream — negotiated in the Welcome and read via
/// `punktfunk_connection_codec`.
public enum VideoCodec: Equatable {
    case h264
    case hevc
    case av1
    /// PyroWave wavelet (opt-in wired-LAN low-latency codec): not a NAL/OBU codec and not
    /// VideoToolbox-decoded at all — the Metal wavelet decoder consumes the raw AUs
    /// (Stage2Pipeline's PyroWave pump). Only ever resolved when this client both advertised
    /// and preferred it.
    case pyrowave

    /// Resolve from the wire `Welcome.codec` byte (`PUNKTFUNK_CODEC_*`; unknown → HEVC).
    public init(wire: UInt8) {
        switch wire {
        case 0x01: self = .h264 // PUNKTFUNK_CODEC_H264
        case 0x04: self = .av1 // PUNKTFUNK_CODEC_AV1
        case 0x08: self = .pyrowave // PUNKTFUNK_CODEC_PYROWAVE
        default: self = .hevc // PUNKTFUNK_CODEC_HEVC — the default / older-host codec
        }
    }

    /// NAL unit type from a NAL's first byte. HEVC: bits 1..6; H.264: bits 0..4.
    fileprivate func nalType(_ first: UInt8) -> UInt8 {
        self == .hevc ? (first >> 1) & 0x3F : first & 0x1F
    }

    /// True for a parameter-set NAL (dropped from AVCC; kept for the format description).
    /// HEVC: VPS 32 / SPS 33 / PPS 34. H.264: SPS 7 / PPS 8 (no VPS).
    fileprivate func isParameterSet(_ first: UInt8) -> Bool {
        let t = nalType(first)
        return self == .hevc ? (32...34).contains(t) : t == 7 || t == 8
    }

    /// True for a VCL (slice) NAL — in a conforming AU no parameter set follows the first one,
    /// so the format-description scan can stop there.
    fileprivate func isVCL(_ first: UInt8) -> Bool {
        let t = nalType(first)
        return self == .hevc ? t <= 31 : (1...5).contains(t)
    }
}

public enum AnnexB {
    /// Walk the NAL units of `data` without copying: `body` receives the buffer base and each
    /// NAL's byte range (start codes 00 00 01 / 00 00 00 01 excluded), and returns false to
    /// stop the walk early (e.g. at the first VCL NAL). All zeros immediately preceding a
    /// start code are dropped: they're either the 4-byte-code prefix or `trailing_zero_8bits`
    /// padding, never NAL payload (emulation prevention keeps 00 00 0x out of conforming NAL
    /// bytes) — same policy as ffmpeg. The base pointer is only valid inside `body`.
    ///
    /// `body` runs once a NAL's END is found, so stopping there has already scanned its
    /// payload. `enter` sees a NAL's first byte as its start code is found, and returns false
    /// to stop the walk before that.
    static func forEachNAL(
        in data: Data, enter: (_ first: UInt8) -> Bool = { _ in true },
        _ body: (_ base: UnsafePointer<UInt8>, _ range: Range<Int>) -> Bool
    ) {
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            let count = raw.count
            var i = 0
            var start = -1
            while i + 2 < count {
                // A code ends in 01: a third byte above 1 rules out a code starting at any
                // of these three positions, so the walk skips them. Most payload bytes do.
                let third = base[i + 2]
                if third > 1 {
                    i += 3
                } else if third == 1, base[i] == 0, base[i + 1] == 0 {
                    var codeStart = i
                    while codeStart > 0, base[codeStart - 1] == 0 {
                        codeStart -= 1
                    }
                    if start >= 0, start < codeStart, !body(base, start..<codeStart) { return }
                    start = i + 3
                    if start < count, !enter(base[start]) { return }
                    i += 3
                } else {
                    i += 1
                }
            }
            if start >= 0, start < count {
                _ = body(base, start..<count)
            }
        }
    }

    /// Split an Annex-B stream into NAL units (start codes stripped — see `forEachNAL` for
    /// the boundary policy). Materializes a Data per NAL; the streaming paths use
    /// `forEachNAL` directly instead.
    static func nalUnits(in data: Data) -> [Data] {
        var nals: [Data] = []
        forEachNAL(in: data) { base, range in
            nals.append(Data(bytes: base + range.lowerBound, count: range.count))
            return true
        }
        return nals
    }

    /// HEVC NAL unit type (bits 1..6 of the first byte).
    static func hevcNalType(_ nal: Data) -> UInt8 {
        guard let first = nal.first else { return 0xFF }
        return (first >> 1) & 0x3F
    }

    /// Build a format description from an IDR AU's in-band parameter sets (HEVC: VPS/SPS/PPS;
    /// H.264: SPS/PPS). Returns nil when the AU carries no parameter sets (non-IDR). Runs per
    /// AU on the pump thread: parameter sets precede the first VCL NAL in a conforming AU, so
    /// the scan stops at that NAL's start code. A delta frame costs a few byte compares.
    public static func formatDescription(
        fromIDR au: Data, codec: VideoCodec
    ) -> CMVideoFormatDescription? {
        // ACCUMULATED, not last-wins: an AU may legally carry more than one of a kind (two PPS
        // with different ids is what some encoders emit), and keeping only the last leaves every
        // slice that references a dropped one undecodable — a permanent decode-error loop rather
        // than a visible failure.
        var vps: [Range<Int>] = [], sps: [Range<Int>] = [], pps: [Range<Int>] = []
        forEachNAL(in: au, enter: { !codec.isVCL($0) }) { base, range in
            let first = base[range.lowerBound]
            switch codec.nalType(first) {
            case 32 where codec == .hevc:
                vps.append(range)
            case 33 where codec == .hevc, 7 where codec == .h264:
                sps.append(range)
            case 34 where codec == .hevc, 8 where codec == .h264:
                pps.append(range)
            default:
                if codec.isVCL(first) { return false } // no parameter sets can follow
                // AUD/SEI/… may precede the slices; keep scanning.
            }
            return true
        }
        guard !sps.isEmpty, !pps.isEmpty else { return nil }
        // In the order VideoToolbox wants them: HEVC VPS,SPS,PPS (VPS required); H.264 SPS,PPS.
        let sets: [Range<Int>]
        switch codec {
        case .hevc:
            guard !vps.isEmpty else { return nil }
            sets = vps + sps + pps
        case .h264:
            sets = sps + pps
        case .av1, .pyrowave:
            return nil // no parameter-set NALs — dispatched in AV1.swift, never reaches here
        }

        var format: CMVideoFormatDescription?
        // The sets are ranges of `au`, pinned once for the create call. The host decides how many
        // there are, so nothing here may scale the stack with that count.
        let status: OSStatus = au.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return -1 }
            let pointers = sets.map { UnsafePointer(base + $0.lowerBound) }
            let sizes = sets.map(\.count)
            switch codec {
            case .hevc:
                return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: pointers.count,
                    parameterSetPointers: pointers,
                    parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4,
                    extensions: nil,
                    formatDescriptionOut: &format)
            case .h264:
                return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: kCFAllocatorDefault,
                    parameterSetCount: pointers.count,
                    parameterSetPointers: pointers,
                    parameterSetSizes: sizes,
                    nalUnitHeaderLength: 4,
                    formatDescriptionOut: &format)
            case .av1, .pyrowave:
                return -1 // unreachable — the arm above already returned
            }
        }
        return status == noErr ? format : nil
    }

    /// Re-pack an Annex-B AU as AVCC (4-byte big-endian length before each NAL), dropping
    /// the parameter-set NALs (they live in the format description).
    static func avcc(from au: Data, codec: VideoCodec) -> Data {
        var out = Data(capacity: au.count + 16)
        forEachNAL(in: au) { base, range in
            if codec.isParameterSet(base[range.lowerBound]) { return true }
            var len = UInt32(range.count).bigEndian
            withUnsafeBytes(of: &len) { out.append(contentsOf: $0) }
            out.append(UnsafeBufferPointer(start: base + range.lowerBound, count: range.count))
            return true
        }
        return out
    }

    /// Wrap one AU as a decode-ready CMSampleBuffer. An AU in `pool` is rewritten where it lies
    /// when it can be (`sampleInPlace`); otherwise the AVCC form is packed directly into the
    /// CMBlockBuffer's allocation, with no intermediate Data: one scan finds the payload NALs
    /// and sizes the block, the pack copies from the ranges it found.
    public static func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription, codec: VideoCodec,
        in pool: AccessUnitSlotPool? = nil
    ) -> CMSampleBuffer? {
        if let pool, let placed = au.inPlace,
           let sample = sampleInPlace(au: au, placed: placed, pool: pool, format: format, codec: codec) {
            return sample
        }
        var payload: [Range<Int>] = []
        payload.reserveCapacity(8)
        var total = 0
        forEachNAL(in: au.data) { base, range in
            if !codec.isParameterSet(base[range.lowerBound]) {
                total += 4 + range.count
                payload.append(range)
            }
            return true
        }
        // Nothing decodable (a parameter-set-only AU — our host never sends one): drop it
        // rather than hand the decoder an empty sample.
        guard total > 0 else { return nil }

        return SamplePack.sample(total: total, ptsNs: au.ptsNs, format: format) { dst in
            // Length prefix + payload per NAL, straight into the block.
            au.data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
                guard let base = raw.baseAddress else { return }
                var off = 0
                for range in payload {
                    var len = UInt32(range.count).bigEndian
                    withUnsafeBytes(of: &len) {
                        dst.advanced(by: off).copyMemory(from: $0.baseAddress!, byteCount: 4)
                    }
                    dst.advanced(by: off + 4)
                        .copyMemory(from: base + range.lowerBound, byteCount: range.count)
                    off += 4 + range.count
                }
            }
        }
    }

    /// The AVCC form written over the AU in its pool slot: each start code becomes its NAL's
    /// length, and the sample's block is the slot. Needs every start code 4 bytes long (a NAL
    /// behind a 3-byte code has no room for its length) and no parameter-set NAL (those stay
    /// out of the sample, which only the repack can do). nil = repack.
    private static func sampleInPlace(
        au: AccessUnit, placed: AccessUnit.InPlace, pool: AccessUnitSlotPool,
        format: CMVideoFormatDescription, codec: VideoCodec
    ) -> CMSampleBuffer? {
        guard let slotStart = pool.slotStart(placed.token), placed.offset >= slotStart,
              placed.count <= slotStart + pool.capacity - placed.offset
        else { return nil }
        var nals: [Range<Int>] = []
        nals.reserveCapacity(8)
        var previousEnd = 0
        var fits = true
        forEachNAL(in: au.data) { base, range in
            // The code is what lies between the previous NAL's end and this one's start: a
            // longer run means a 3-byte code plus trailing zeros, or zero bytes the repack drops.
            guard range.lowerBound - previousEnd == 4, !codec.isParameterSet(base[range.lowerBound])
            else {
                fits = false
                return false
            }
            nals.append(range)
            previousEnd = range.upperBound
            return true
        }
        guard fits, !nals.isEmpty, previousEnd == placed.count else { return nil }
        let base = pool.pointer(placed.offset)
        for range in nals {
            var len = UInt32(range.count).bigEndian
            withUnsafeBytes(of: &len) {
                base.advanced(by: range.lowerBound - 4).copyMemory(from: $0.baseAddress!, byteCount: 4)
            }
        }
        return SamplePack.sample(
            in: pool, token: placed.token, offset: placed.offset, length: placed.count,
            ptsNs: au.ptsNs, format: format)
    }
}
