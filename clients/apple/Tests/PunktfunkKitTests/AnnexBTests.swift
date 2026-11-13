// Unit tests for the Annex-B ⇄ AVCC plumbing (pure byte-level; no codec involved —
// VideoToolboxRoundTripTests covers the real-bitstream path).

import CoreMedia
import XCTest
@testable import PunktfunkKit

final class AnnexBTests: XCTestCase {
    /// NAL with the given HEVC type in bits 1..6 of the first header byte.
    private func nal(type: UInt8, payload: [UInt8]) -> Data {
        Data([type << 1, 0x01] + payload)
    }

    private let start4: [UInt8] = [0, 0, 0, 1]
    private let start3: [UInt8] = [0, 0, 1]

    func testSplitMixedStartCodes() {
        let a = nal(type: 32, payload: [0xAA])
        let b = nal(type: 33, payload: [0xBB, 0xBC])
        let c = nal(type: 19, payload: [0xCC, 0xCD, 0xCE])
        var au = Data(start4)
        au.append(a)
        au.append(contentsOf: start3)
        au.append(b)
        au.append(contentsOf: start4)
        au.append(c)

        let nals = AnnexB.nalUnits(in: au)
        XCTAssertEqual(nals, [a, b, c])
        XCTAssertEqual(nals.map(AnnexB.hevcNalType), [32, 33, 19])
    }

    func testSplitSingleNalNoTrailingCode() {
        let v = nal(type: 34, payload: [1, 2, 3])
        let au = Data(start3) + v
        XCTAssertEqual(AnnexB.nalUnits(in: au), [v])
    }

    func testSplitEmptyAndGarbage() {
        XCTAssertEqual(AnnexB.nalUnits(in: Data()), [])
        // No start code at all → no NALs.
        XCTAssertEqual(AnnexB.nalUnits(in: Data([9, 8, 7, 6])), [])
    }

    func testSplitDropsTrailingZeroPadding() {
        // trailing_zero_8bits between NALs (and >2 zeros forming a long separator) must
        // not leak into the preceding NAL.
        let a = nal(type: 33, payload: [0xAA])
        let b = nal(type: 19, payload: [0xBB])
        var au = Data(start4)
        au.append(a)
        au.append(contentsOf: [0, 0, 0, 0, 0, 1]) // padding + start code
        au.append(b)
        XCTAssertEqual(AnnexB.nalUnits(in: au), [a, b])
    }

    func testAvccDropsParameterSetsAndPrefixesLengths() {
        let vps = nal(type: 32, payload: [0xAA])
        let sps = nal(type: 33, payload: [0xBB])
        let pps = nal(type: 34, payload: [0xCC])
        let idr = nal(type: 19, payload: [0xDD, 0xDE, 0xDF, 0xE0])
        var au = Data()
        for n in [vps, sps, pps, idr] {
            au.append(contentsOf: start4)
            au.append(n)
        }

        let avcc = AnnexB.avcc(from: au, codec: .hevc)
        // Only the IDR survives: 4-byte BE length, then the NAL bytes.
        var expected = Data([0, 0, 0, UInt8(idr.count)])
        expected.append(idr)
        XCTAssertEqual(avcc, expected)
    }

    func testFormatDescriptionNilWithoutParameterSets() {
        let idr = nal(type: 19, payload: [0xDD])
        let au = Data(start4) + idr
        XCTAssertNil(AnnexB.formatDescription(fromIDR: au, codec: .hevc))
    }

    /// The host decides how many parameter sets an AU carries: a flood of them is refused, and
    /// the stack does not grow with the count.
    func testFormatDescriptionSurvivesAFloodOfParameterSets() {
        var au = Data(start4) + nal(type: 32, payload: [0xAA])
        au.append(Data(start4) + nal(type: 33, payload: [0xBB]))
        let pps = Data(start3) + nal(type: 34, payload: [0xCC])
        for _ in 0..<100_000 { au.append(pps) }
        XCTAssertNil(AnnexB.formatDescription(fromIDR: au, codec: .hevc))
    }

    /// `enter` stops the walk at a NAL's start code: everything before it has been handed to
    /// `body`, and nothing of it or after it is.
    func testEnterStopsTheWalkBeforeANalsPayload() {
        let vps = nal(type: 32, payload: [0xAA])
        let sps = nal(type: 33, payload: [0xBB])
        let idr = nal(type: 19, payload: [0xDD, 0xDE])
        let late = nal(type: 34, payload: [0xCC])
        var au = Data()
        for n in [vps, sps, idr, late] {
            au.append(contentsOf: start4)
            au.append(n)
        }
        var seen: [UInt8] = []
        AnnexB.forEachNAL(in: au, enter: { ($0 >> 1) & 0x3F != 19 }) { base, range in
            seen.append((base[range.lowerBound] >> 1) & 0x3F)
            return true
        }
        XCTAssertEqual(seen, [32, 33])
    }

    /// The pack copies from the ranges the one scan found: parameter sets dropped, every
    /// payload NAL kept, in order.
    func testSampleBufferPacksEveryPayloadNal() throws {
        let format = try XCTUnwrap(Self.anyFormat())
        let sps = nal(type: 33, payload: [0xBB])
        let first = nal(type: 1, payload: [0x10, 0x11, 0x12])
        let second = nal(type: 1, payload: [0x20, 0x21])
        var au = Data()
        for n in [sps, first, second] {
            au.append(contentsOf: start4)
            au.append(n)
        }
        let sample = try XCTUnwrap(AnnexB.sampleBuffer(
            au: AccessUnit(data: au, ptsNs: 0, frameIndex: 0, flags: 0, receivedNs: 0),
            format: format, codec: .hevc))
        let block = try XCTUnwrap(CMSampleBufferGetDataBuffer(sample))
        var packed = Data(count: CMBlockBufferGetDataLength(block))
        let status = packed.withUnsafeMutableBytes {
            CMBlockBufferCopyDataBytes(
                block, atOffset: 0, dataLength: $0.count, destination: $0.baseAddress!)
        }
        XCTAssertEqual(status, noErr)
        var expected = Data([0, 0, 0, UInt8(first.count)]) + first
        expected.append(Data([0, 0, 0, UInt8(second.count)]) + second)
        XCTAssertEqual(packed, expected)
    }

    /// The host sets the pts: one past `Int64.max` clamps instead of trapping.
    func testSampleBufferClampsAPtsPastInt64() throws {
        let format = try XCTUnwrap(Self.anyFormat())
        let au = Data(start4) + nal(type: 1, payload: [0x10])
        let sample = try XCTUnwrap(AnnexB.sampleBuffer(
            au: AccessUnit(data: au, ptsNs: .max, frameIndex: 0, flags: 0, receivedNs: 0),
            format: format, codec: .hevc))
        XCTAssertEqual(CMSampleBufferGetPresentationTimeStamp(sample).value, .max)
    }

    /// `au` as the core would leave it: written into a slot the pool lent.
    private func placed(_ au: Data, in pool: AccessUnitSlotPool) throws
        -> (unit: AccessUnit, base: UnsafeMutableRawPointer, token: UInt64)
    {
        let slot = try XCTUnwrap(pool.take(au.count))
        au.withUnsafeBytes { slot.base.copyMemory(from: $0.baseAddress!, byteCount: $0.count) }
        let at = try XCTUnwrap(pool.locate(UnsafeRawPointer(slot.base)))
        let unit = AccessUnit(
            data: Data(bytesNoCopy: slot.base, count: au.count, deallocator: .none),
            ptsNs: 0, frameIndex: 0, flags: 0, receivedNs: 0,
            inPlace: AccessUnit.InPlace(token: at.token, offset: at.offset, count: au.count))
        return (unit, slot.base, slot.token)
    }

    /// The sample's block: its bytes and where it lies.
    private func blockBytes(_ sample: CMSampleBuffer) throws -> (bytes: Data, base: UnsafeMutableRawPointer) {
        let block = try XCTUnwrap(CMSampleBufferGetDataBuffer(sample))
        var length = 0
        var ptr: UnsafeMutablePointer<CChar>?
        XCTAssertEqual(CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &length,
            dataPointerOut: &ptr), noErr)
        let base = try XCTUnwrap(ptr)
        return (Data(bytes: base, count: length), UnsafeMutableRawPointer(base))
    }

    /// An AU the core assembled in a pool slot becomes the sample without a copy: the block is
    /// the slot, each start code now its NAL's length. The slot stays the sample's until the
    /// sample is gone, whatever the core released.
    func testSampleBufferOverTheSlotRewritesStartCodes() throws {
        let format = try XCTUnwrap(Self.anyFormat())
        let first = nal(type: 1, payload: [0x10, 0x11, 0x12])
        let second = nal(type: 1, payload: [0x20, 0x21])
        var au = Data()
        for n in [first, second] {
            au.append(contentsOf: start4)
            au.append(n)
        }
        let pool = AccessUnitSlotPool(capacity: 4096)
        let slot = try placed(au, in: pool)
        var expected = Data([0, 0, 0, UInt8(first.count)]) + first
        expected.append(Data([0, 0, 0, UInt8(second.count)]) + second)
        try autoreleasepool {
            let sample = try XCTUnwrap(
                AnnexB.sampleBuffer(au: slot.unit, format: format, codec: .hevc, in: pool))
            let block = try blockBytes(sample)
            XCTAssertEqual(block.base, slot.base, "the block is the slot")
            XCTAssertEqual(block.bytes, expected)
            pool.release(slot.token)
            let other = try XCTUnwrap(pool.take(16))
            XCTAssertNotEqual(other.token, slot.token, "held by the sample")
            pool.release(other.token)
        }
        // The sample is gone: the slot is free again (the pool hands out the last freed first).
        XCTAssertEqual(pool.take(16)?.token, slot.token)
    }

    /// A parameter set or a 3-byte start code leaves the AU to the repack: a fresh block.
    func testSampleBufferRepacksWhatCannotBeRewritten() throws {
        let format = try XCTUnwrap(Self.anyFormat())
        let pool = AccessUnitSlotPool(capacity: 4096)
        let withSps = Data(start4) + nal(type: 33, payload: [0xBB]) + Data(start4) + nal(type: 1, payload: [0x10])
        let threeByte = Data(start4) + nal(type: 1, payload: [0x10]) + Data(start3) + nal(type: 1, payload: [0x20])
        for au in [withSps, threeByte] {
            let slot = try placed(au, in: pool)
            let sample = try XCTUnwrap(
                AnnexB.sampleBuffer(au: slot.unit, format: format, codec: .hevc, in: pool))
            XCTAssertNotEqual(try blockBytes(sample).base, slot.base)
            pool.release(slot.token)
        }
    }

    /// Any video format description: the pack only carries it along.
    private static func anyFormat() -> CMVideoFormatDescription? {
        var format: CMVideoFormatDescription?
        CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, codecType: kCMVideoCodecType_HEVC, width: 16,
            height: 16, extensions: nil, formatDescriptionOut: &format)
        return format
    }
}
