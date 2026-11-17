// Access-unit memory for the VideoToolbox pumps: 4 KiB-aligned slots the core assembles AUs in,
// which a sample buffer then wraps as its own block (SamplePack), so an AU is copied neither at
// the pull nor into the sample. A slot is free once the core released it and no sample holds it.

import Foundation

public final class AccessUnitSlotPool: AccessUnitMemory {
    /// AUs in flight at once: a few reassembling, one being wrapped, the samples still decoding.
    static let slots = 8
    /// Room for a delta frame at any bitrate the LAN carries; a bigger one grows the pool.
    static let defaultCapacity = 2 << 20

    /// Bytes an AU may take.
    let capacity: Int
    let stride: Int
    private let base: UnsafeMutableRawPointer
    private let lock = NSLock()
    private var lentToCore = [Bool](repeating: false, count: AccessUnitSlotPool.slots)
    private var heldBySample = [Bool](repeating: false, count: AccessUnitSlotPool.slots)
    private var free = Array((0..<AccessUnitSlotPool.slots).reversed())

    init(capacity: Int) {
        self.capacity = (capacity + 3) & ~3
        stride = (self.capacity + 4095) & ~4095
        base = UnsafeMutableRawPointer.allocate(byteCount: stride * Self.slots, alignment: 4096)
    }

    deinit { base.deallocate() }

    /// The pool for the next AU: `current`, or a bigger one lent from here on when a delta frame
    /// did not fit it. A keyframe that did not fit is left alone: it takes the repack anyway.
    static func grown(
        for au: AccessUnit, keyframe: Bool, from current: AccessUnitSlotPool,
        connection: PunktfunkConnection
    ) -> AccessUnitSlotPool {
        guard au.inPlace == nil, !keyframe, au.byteCount > current.capacity else { return current }
        let bigger = AccessUnitSlotPool(capacity: au.byteCount * 5 / 4)
        connection.setAccessUnitMemory(bigger)
        return bigger
    }

    /// Byte offset of `token`'s slot, or nil for a token that is not this pool's.
    func slotStart(_ token: UInt64) -> Int? {
        token < UInt64(Self.slots) ? Int(token) * stride : nil
    }

    /// The memory `offset` bytes into the pool.
    func pointer(_ offset: Int) -> UnsafeMutableRawPointer { base + offset }

    public func take(_ count: Int) -> (base: UnsafeMutableRawPointer, count: Int, token: UInt64)? {
        guard count <= capacity else { return nil }
        lock.lock()
        defer { lock.unlock() }
        guard let index = free.popLast() else { return nil }
        lentToCore[index] = true
        return (base + index * stride, capacity, UInt64(index))
    }

    public func release(_ token: UInt64) {
        guard token < UInt64(Self.slots) else { return }
        let index = Int(token)
        lock.lock()
        lentToCore[index] = false
        if !heldBySample[index] { free.append(index) }
        lock.unlock()
    }

    public func locate(_ pointer: UnsafeRawPointer) -> (token: UInt64, offset: Int)? {
        let base = UnsafeRawPointer(self.base)
        guard pointer >= base, pointer < base + stride * Self.slots else { return nil }
        let offset = base.distance(to: pointer)
        return (UInt64(offset / stride), offset)
    }

    /// A sample takes `token`'s slot from now until `unhold`.
    func hold(_ token: UInt64) {
        guard token < UInt64(Self.slots) else { return }
        lock.lock()
        heldBySample[Int(token)] = true
        lock.unlock()
    }

    /// The sample over `token`'s slot is gone.
    func unhold(_ token: UInt64) {
        guard token < UInt64(Self.slots) else { return }
        let index = Int(token)
        lock.lock()
        heldBySample[index] = false
        if !lentToCore[index] { free.append(index) }
        lock.unlock()
    }
}
