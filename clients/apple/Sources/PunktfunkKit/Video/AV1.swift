// AV1 (low-overhead OBU bitstream) → CoreMedia plumbing — the AV1 sibling of AnnexB.swift.
//
// The punktfunk host emits AV1 access units as low-overhead temporal units (the raw encoder
// output every other client feeds ffmpeg): a temporal-delimiter OBU, then — on every keyframe,
// per the same in-band-config policy as the NAL codecs — a sequence-header OBU, then the frame
// OBUs. VideoToolbox instead wants the ISOBMFF 'av01' flavor: a CMVideoFormatDescription
// carrying an `av1C` configuration record (built from the sequence header), and sample buffers
// holding the temporal unit with the temporal delimiter stripped and every OBU size-fielded.
// This file converts between the two.
//
// HOT PATH: like AnnexB, both pumps run `formatDescription(fromKeyframe:)` +
// `sampleBuffer(au:format:)` once per AU, so everything is built on `forEachOBU` — a zero-copy
// scan over the AU's bytes (ranges, not materialized Datas). A delta AU (no sequence header)
// costs a few OBU-header reads; the sample repack leaves exactly one copy (source → block
// buffer), mirroring AnnexB.sampleBuffer.
//
// The sequence-header parse (the core's, over the vendored AV1 parser) runs only when a keyframe
// carries one. It fills the `av1C` record fields (profile/level/tier/depth/chroma) and the
// colorimetry extensions, so VideoDecoder.isHDRFormat and the presenter's color handling work
// identically across codecs. An AV1 stream carries 8- or 10-bit 4:2:0 — the host gates 4:4:4
// to HEVC, never depth.

import CoreMedia
import Foundation
import PunktfunkCore
import VideoToolbox

public enum AV1 {
    /// True when this device can hardware-decode AV1 (M3-class Macs, A17 Pro-class iPhones,
    /// current iPads; false on every Apple TV to date). VideoToolbox has no software AV1
    /// decoder, so this is the advertisement gate: a client must never invite a stream it
    /// can't decode in real time.
    public static let hardwareDecodeSupported: Bool =
        VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)

    // MARK: - OBU walking

    /// OBU types (AV1 spec 6.2.2) — only the ones this file dispatches on.
    enum OBUType {
        static let sequenceHeader: UInt8 = 1
        static let temporalDelimiter: UInt8 = 2
        static let padding: UInt8 = 15
    }

    /// Walk the OBUs of a low-overhead temporal unit without copying: `body` receives the buffer
    /// base, each OBU's header range (header byte + optional extension byte + size field, i.e.
    /// everything before the payload), payload range, and type — and returns false to stop early.
    /// The walk ends at the first malformed OBU (forbidden bit set, truncated header, or a size
    /// field overrunning the buffer): a torn AU decodes as garbage anyway and the pumps' keyframe
    /// recovery re-anchors, so bailing beats guessing at boundaries. An OBU with
    /// `obu_has_size_field == 0` extends to the end of the buffer (legal only for the last one).
    /// The base pointer is only valid inside `body`.
    static func forEachOBU(
        in data: Data,
        _ body: (
            _ base: UnsafePointer<UInt8>, _ header: Range<Int>, _ payload: Range<Int>,
            _ type: UInt8
        ) -> Bool
    ) {
        data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            let count = raw.count
            var i = 0
            while i < count {
                let start = i
                let h = base[i]
                guard h & 0x80 == 0 else { return } // obu_forbidden_bit — not an OBU stream
                let type = (h >> 3) & 0x0F
                let hasExtension = h & 0x04 != 0
                let hasSize = h & 0x02 != 0
                i += 1
                if hasExtension {
                    guard i < count else { return }
                    i += 1
                }
                let payloadLen: Int
                if hasSize {
                    guard let (size, sizeLen) = leb128(base: base, at: i, count: count)
                    else { return }
                    i += sizeLen
                    payloadLen = size
                } else {
                    payloadLen = count - i // no size field: extends to the end (must be last)
                }
                guard i + payloadLen <= count else { return }
                if !body(base, start..<i, i..<(i + payloadLen), type) { return }
                i += payloadLen
            }
        }
    }

    /// Decode a leb128 value at `at` (AV1 spec 4.10.5). Returns (value, encoded length) or nil
    /// on truncation / a value past 32 bits (sizes beyond that are nonsense for an OBU).
    private static func leb128(
        base: UnsafePointer<UInt8>, at: Int, count: Int
    ) -> (Int, Int)? {
        var value: UInt64 = 0
        for i in 0..<8 {
            guard at + i < count else { return nil }
            let byte = base[at + i]
            value |= UInt64(byte & 0x7F) << (7 * i)
            if byte & 0x80 == 0 {
                guard value <= UInt64(UInt32.max) else { return nil }
                return (Int(value), i + 1)
            }
        }
        return nil
    }

    /// leb128-encoded byte length of `value`.
    private static func leb128Length(_ value: Int) -> Int {
        var v = UInt32(value)
        var n = 1
        while v >= 0x80 {
            v >>= 7
            n += 1
        }
        return n
    }

    /// Encode `value` as leb128 into `dst`; returns the byte count written.
    private static func putLeb128(_ value: Int, into dst: UnsafeMutableRawPointer) -> Int {
        var v = UInt32(value)
        var n = 0
        repeat {
            var byte = UInt8(v & 0x7F)
            v >>= 7
            if v != 0 { byte |= 0x80 }
            dst.storeBytes(of: byte, toByteOffset: n, as: UInt8.self)
            n += 1
        } while v != 0
        return n
    }

    // MARK: - Sequence header

    /// The `av1C` and colorimetry fields of the first sequence header in `obus` (a temporal unit
    /// or a run of sized OBUs), parsed by the core. Nil when it carries none or it does not parse.
    static func sequenceInfo(_ obus: Data) -> PunktfunkAv1SequenceInfo? {
        var info = PunktfunkAv1SequenceInfo()
        let status = obus.withUnsafeBytes { raw in
            punktfunk_av1_sequence_info(
                raw.bindMemory(to: UInt8.self).baseAddress, UInt(raw.count), &info)
        }
        return status == PUNKTFUNK_STATUS_OK.rawValue ? info : nil
    }

    // MARK: - Format description

    /// Build a format description from a keyframe AU's in-band sequence header — the AV1
    /// equivalent of `AnnexB.formatDescription(fromIDR:)`. Returns nil when the AU carries no
    /// sequence-header OBU (a delta frame): the pumps latch the previous description exactly as
    /// they do for the NAL codecs. The description carries the `av1C` record (with the sequence
    /// header as its configOBUs) plus colorimetry extensions mapped from color_config, so
    /// `VideoDecoder.isHDRFormat` and the presenter treat AV1 like any other stream.
    public static func formatDescription(fromKeyframe au: Data) -> CMVideoFormatDescription? {
        // The sequence-header OBU, re-emitted with a size field (encoders size-field everything
        // in practice; the rewrap also covers a last-OBU-without-size corner).
        var seqHeaderOBU: Data?
        forEachOBU(in: au) { base, header, payload, type in
            guard type == OBUType.sequenceHeader else { return true }
            var obu = Data(capacity: 2 + leb128Length(payload.count) + payload.count)
            obu.append(base[header.lowerBound] | 0x02) // has_size_field set
            if base[header.lowerBound] & 0x04 != 0 { // extension byte rides along
                obu.append(base[header.lowerBound + 1])
            }
            var lenBuf = [UInt8](repeating: 0, count: 8)
            let lenLen = lenBuf.withUnsafeMutableBytes {
                putLeb128(payload.count, into: $0.baseAddress!)
            }
            obu.append(contentsOf: lenBuf[0..<lenLen])
            obu.append(UnsafeBufferPointer(start: base + payload.lowerBound, count: payload.count))
            seqHeaderOBU = obu
            return false
        }
        guard let seqHeaderOBU, let sh = sequenceInfo(seqHeaderOBU) else { return nil }

        // AV1CodecConfigurationRecord (AV1-ISOBMFF §2.3): 4 fixed bytes + configOBUs.
        var av1C = Data(capacity: 4 + seqHeaderOBU.count)
        av1C.append(0x81) // marker=1, version=1
        av1C.append((sh.profile << 5) | sh.level_idx0)
        av1C.append(
            (sh.tier0 << 7)
                | ((sh.high_bitdepth ? 1 : 0) << 6)
                | ((sh.twelve_bit ? 1 : 0) << 5)
                | ((sh.mono_chrome ? 1 : 0) << 4)
                | ((sh.subsampling_x ? 1 : 0) << 3)
                | ((sh.subsampling_y ? 1 : 0) << 2)
                | sh.chroma_sample_position)
        av1C.append(0) // no initial_presentation_delay
        av1C.append(seqHeaderOBU)

        // Colorimetry from color_config's H.273 codes; unspecified (2) falls back to BT.709 —
        // the host's SDR default, same policy the presenter applies elsewhere.
        let primaries: CFString = {
            switch sh.color_primaries {
            case 9: return kCMFormatDescriptionColorPrimaries_ITU_R_2020
            case 6: return kCMFormatDescriptionColorPrimaries_SMPTE_C
            case 5: return kCMFormatDescriptionColorPrimaries_EBU_3213
            default: return kCMFormatDescriptionColorPrimaries_ITU_R_709_2
            }
        }()
        let transfer: CFString = {
            switch sh.transfer_characteristics {
            case 16: return kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ
            case 18: return kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG
            case 13: return kCMFormatDescriptionTransferFunction_sRGB
            case 8: return kCMFormatDescriptionTransferFunction_Linear
            default: return kCMFormatDescriptionTransferFunction_ITU_R_709_2
            }
        }()
        let matrix: CFString = {
            switch sh.matrix_coefficients {
            case 9, 10: return kCMFormatDescriptionYCbCrMatrix_ITU_R_2020
            case 5, 6: return kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4
            default: return kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2
            }
        }()
        let extensions: [CFString: Any] = [
            kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms: ["av1C": av1C],
            kCMFormatDescriptionExtension_ColorPrimaries: primaries,
            kCMFormatDescriptionExtension_TransferFunction: transfer,
            kCMFormatDescriptionExtension_YCbCrMatrix: matrix,
            kCMFormatDescriptionExtension_FullRangeVideo: sh.full_range,
        ]
        var format: CMVideoFormatDescription?
        let status = CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault,
            codecType: kCMVideoCodecType_AV1,
            width: Int32(sh.max_width), height: Int32(sh.max_height),
            extensions: extensions as CFDictionary,
            formatDescriptionOut: &format)
        return status == noErr ? format : nil
    }

    // MARK: - Sample buffers

    /// Wrap one temporal unit as a decode-ready CMSampleBuffer in the ISOBMFF 'av01' sample
    /// format: the temporal-delimiter (and padding) OBUs are dropped, every remaining OBU is
    /// re-emitted with a size field, and — mirroring AnnexB.sampleBuffer — the result is packed
    /// straight into the CMBlockBuffer's allocation (sized by a first cheap scan). The sequence
    /// header stays in-band (spec-legal: it's bit-identical to the one in `av1C`, which is
    /// rebuilt from the same keyframe), preserving the host's self-contained-keyframe policy.
    public static func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription, in pool: AccessUnitSlotPool? = nil
    ) -> CMSampleBuffer? {
        if let pool, let placed = au.inPlace,
           let sample = sampleInPlace(au: au, placed: placed, pool: pool, format: format) {
            return sample
        }
        // Pass 1: byte scan only — total repacked size of the kept OBUs.
        var total = 0
        forEachOBU(in: au.data) { base, header, payload, type in
            if type == OBUType.temporalDelimiter || type == OBUType.padding { return true }
            let headerLen = base[header.lowerBound] & 0x04 != 0 ? 2 : 1
            total += headerLen + leb128Length(payload.count) + payload.count
            return true
        }
        // Nothing decodable (a delimiter-only AU — our host never sends one): drop it rather
        // than hand the decoder an empty sample.
        guard total > 0 else { return nil }

        return SamplePack.sample(total: total, ptsNs: au.ptsNs, format: format) { dst in
            // Header (+extension) byte, size field, payload per OBU.
            var off = 0
            forEachOBU(in: au.data) { base, header, payload, type in
                if type == OBUType.temporalDelimiter || type == OBUType.padding { return true }
                dst.storeBytes(
                    of: base[header.lowerBound] | 0x02, toByteOffset: off, as: UInt8.self)
                off += 1
                if base[header.lowerBound] & 0x04 != 0 {
                    dst.storeBytes(
                        of: base[header.lowerBound + 1], toByteOffset: off, as: UInt8.self)
                    off += 1
                }
                off += putLeb128(payload.count, into: dst.advanced(by: off))
                dst.advanced(by: off)
                    .copyMemory(from: base + payload.lowerBound, byteCount: payload.count)
                off += payload.count
                return true
            }
        }
    }

    /// The temporal unit as the sample, where it lies in its pool slot: only the leading
    /// temporal delimiter is left out, by offset. Needs every OBU to carry its size field, no
    /// padding OBU (dropping one would move bytes) and the walk to end at the AU's last byte, as
    /// the repack keeps no torn tail. nil = repack.
    private static func sampleInPlace(
        au: AccessUnit, placed: AccessUnit.InPlace, pool: AccessUnitSlotPool,
        format: CMVideoFormatDescription
    ) -> CMSampleBuffer? {
        guard let slotStart = pool.slotStart(placed.token), placed.offset >= slotStart,
              placed.count <= slotStart + pool.capacity - placed.offset
        else { return nil }
        var skip = 0
        var fits = true
        var kept = 0
        var end = 0
        forEachOBU(in: au.data) { base, header, payload, type in
            end = payload.upperBound
            guard base[header.lowerBound] & 0x02 != 0, type != OBUType.padding else {
                fits = false
                return false
            }
            if type == OBUType.temporalDelimiter {
                guard header.lowerBound == 0, payload.isEmpty else {
                    fits = false
                    return false
                }
                skip = header.upperBound
            } else {
                kept += 1
            }
            return true
        }
        guard fits, kept > 0, end == placed.count else { return nil }
        return SamplePack.sample(
            in: pool, token: placed.token, offset: placed.offset + skip,
            length: placed.count - skip, ptsNs: au.ptsNs, format: format)
    }
}

extension VideoCodec {
    /// Codec-dispatching format-description refresh: the AV1 path keys on an in-band sequence
    /// header, the NAL codecs on in-band parameter sets — one call site in each pump. PyroWave
    /// has no CoreMedia representation at all (its pump feeds the Metal wavelet decoder raw).
    public func formatDescription(fromKeyframe au: Data) -> CMVideoFormatDescription? {
        switch self {
        case .av1: return AV1.formatDescription(fromKeyframe: au)
        case .pyrowave: return nil
        default: return AnnexB.formatDescription(fromIDR: au, codec: self)
        }
    }

    /// Codec-dispatching sample wrap (see `formatDescription(fromKeyframe:)`). `pool` is where
    /// the core assembled the AU, for a sample over the AU itself.
    public func sampleBuffer(
        au: AccessUnit, format: CMVideoFormatDescription, in pool: AccessUnitSlotPool? = nil
    ) -> CMSampleBuffer? {
        switch self {
        case .av1: return AV1.sampleBuffer(au: au, format: format, in: pool)
        case .pyrowave: return nil
        default: return AnnexB.sampleBuffer(au: au, format: format, codec: self, in: pool)
        }
    }
}
