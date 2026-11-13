// The tail both codec repackers share: one CMBlockBuffer, either allocated for the caller to
// write its own per-unit layout into, or the AU's own pool slot (rewritten in place), wrapped as
// a CMSampleBuffer.
//
// AnnexB (length-prefixed NALs) and AV1 (header + leb128-sized OBUs) differ only in the write
// loop; the block, the destination-length check, the timing and the sample create live here.

import CoreMedia
import Foundation

enum SamplePack {
    /// One sample of exactly `total` bytes, filled by `write`.
    ///
    /// `write` receives the block's base pointer and must fill all `total` bytes — the caller
    /// computed that size in its own first pass over the same access unit, so a mismatch is a bug
    /// in that pass rather than a runtime condition.
    static func sample(
        total: Int, ptsNs: UInt64, format: CMVideoFormatDescription,
        write: (UnsafeMutableRawPointer) -> Void
    ) -> CMSampleBuffer? {
        guard total > 0 else { return nil }
        var blockBuffer: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: nil,
            blockLength: total, blockAllocator: kCFAllocatorDefault,
            customBlockSource: nil, offsetToData: 0, dataLength: total,
            flags: kCMBlockBufferAssureMemoryNowFlag, blockBufferOut: &blockBuffer) == noErr,
            let block = blockBuffer
        else { return nil }

        var dstLen = 0
        var dstPtr: UnsafeMutablePointer<CChar>?
        guard CMBlockBufferGetDataPointer(
            block, atOffset: 0, lengthAtOffsetOut: nil, totalLengthOut: &dstLen,
            dataPointerOut: &dstPtr) == noErr,
            dstLen == total, let dstPtr
        else { return nil }
        write(UnsafeMutableRawPointer(dstPtr))
        return wrap(block, total: total, ptsNs: ptsNs, format: format)
    }

    /// One sample over `length` bytes at `offset` in `pool`, the slot `token` names: the block
    /// is that memory itself, held until CoreMedia lets the block go.
    static func sample(
        in pool: AccessUnitSlotPool, token: UInt64, offset: Int, length: Int,
        ptsNs: UInt64, format: CMVideoFormatDescription
    ) -> CMSampleBuffer? {
        guard length > 0 else { return nil }
        pool.hold(token)
        let lease = Unmanaged.passRetained(SlotLease(pool: pool, token: token))
        var source = CMBlockBufferCustomBlockSource(
            version: kCMBlockBufferCustomBlockSourceVersion,
            AllocateBlock: nil,
            FreeBlock: { refcon, _, _ in
                guard let refcon else { return }
                Unmanaged<SlotLease>.fromOpaque(refcon).takeRetainedValue().end()
            },
            refCon: lease.toOpaque())
        var blockBuffer: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: kCFAllocatorDefault, memoryBlock: pool.pointer(offset),
            blockLength: length, blockAllocator: nil,
            customBlockSource: &source, offsetToData: 0, dataLength: length,
            flags: 0, blockBufferOut: &blockBuffer) == noErr,
            let block = blockBuffer
        else {
            lease.takeRetainedValue().end()
            return nil
        }
        return wrap(block, total: length, ptsNs: ptsNs, format: format)
    }

    /// What keeps a slot a sample's until its block is freed.
    private final class SlotLease {
        let pool: AccessUnitSlotPool
        let token: UInt64
        init(pool: AccessUnitSlotPool, token: UInt64) {
            self.pool = pool
            self.token = token
        }
        func end() { pool.unhold(token) }
    }

    /// `block` as a one-sample buffer stamped `ptsNs`, which the host sets: past `Int64.max` it
    /// clamps rather than traps.
    private static func wrap(
        _ block: CMBlockBuffer, total: Int, ptsNs: UInt64, format: CMVideoFormatDescription
    ) -> CMSampleBuffer? {
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: Int64(clamping: ptsNs), timescale: 1_000_000_000),
            decodeTimeStamp: .invalid)
        var sampleSize = total
        var sample: CMSampleBuffer?
        guard CMSampleBufferCreate(
            allocator: kCFAllocatorDefault, dataBuffer: block, dataReady: true,
            makeDataReadyCallback: nil, refcon: nil, formatDescription: format,
            sampleCount: 1, sampleTimingEntryCount: 1, sampleTimingArray: &timing,
            sampleSizeEntryCount: 1, sampleSizeArray: &sampleSize,
            sampleBufferOut: &sample) == noErr,
            let sample
        else { return nil }

        // For the stage-1 AVSampleBufferDisplayLayer path: render on arrival rather than against
        // a clock. Inert on the stage-2 decode path, which paces frames itself.
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(
                dict,
                Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        }
        return sample
    }
}
