// The mic uplink's capture half.
//
// A sink node on the input runs on the IO thread once per IO cycle: the device buffer, by
// default 512 frames (10.7 ms at 48 kHz) on a Mac (macOS 27). It folds that buffer to mono and
// copies it into a ring, and a worker thread drains the ring into the resample and Opus chain.
// An engine tap delivers at least 100 ms per buffer (AVAudioNode.h), and the uplink would pay
// that as voice delay.
//
// The IO thread allocates nothing and logs nothing. It takes one lock, held for a copy.

#if !os(tvOS)
import AVFoundation
import os

/// Mono capture on its way from the IO thread to the worker. Fixed size: a worker that falls
/// behind loses the oldest audio, and the IO thread never waits on it.
final class MicRing: @unchecked Sendable {
    private let lock = OSAllocatedUnfairLock()
    private let store: UnsafeMutablePointer<Float>
    private let capacity: Int
    private var head = 0
    private var count = 0

    init(capacity: Int) {
        self.capacity = max(capacity, 1)
        store = .allocate(capacity: self.capacity)
    }

    deinit { store.deallocate() }

    func write(_ samples: UnsafePointer<Float>, count incoming: Int) {
        guard incoming > 0 else { return }
        // More than the ring holds: only the newest `capacity` samples survive.
        let skipped = max(0, incoming - capacity)
        let src = samples + skipped
        let len = incoming - skipped
        lock.lock()
        defer { lock.unlock() }
        let dropped = max(0, count + len - capacity)
        head = (head + dropped) % capacity
        count -= dropped
        let tail = (head + count) % capacity
        let first = min(len, capacity - tail)
        (store + tail).update(from: src, count: first)
        store.update(from: src + first, count: len - first)
        count += len
    }

    /// Up to `limit` samples, oldest first. Returns how many were copied.
    func read(into out: UnsafeMutablePointer<Float>, limit: Int) -> Int {
        lock.lock()
        defer { lock.unlock() }
        let taken = min(limit, count)
        guard taken > 0 else { return 0 }
        let first = min(taken, capacity - head)
        out.update(from: store + head, count: first)
        (out + first).update(from: store, count: taken - first)
        head = (head + taken) % capacity
        count -= taken
        return taken
    }
}

/// The IO thread's scratch: one quantum folded to mono, and the channel pointers the fold
/// reads through. Owned by the sink node's block, so it lives as long as the last callback.
private final class FoldScratch: @unchecked Sendable {
    static let frames = 8192
    static let channels = 64
    let mono = UnsafeMutablePointer<Float>.allocate(capacity: frames)
    let planes = UnsafeMutablePointer<UnsafeMutablePointer<Float>>.allocate(capacity: channels)

    deinit {
        mono.deallocate()
        planes.deallocate()
    }

    /// Fold `frames` frames of `list`, from frame `offset`, into `mono`. False when the list
    /// holds less than that, which a well-formed callback never does.
    func fold(
        _ list: UnsafeMutableAudioBufferListPointer, offset: Int, frames: Int, pinned: Int?,
        skip: [Int]
    ) -> Bool {
        guard let head = list.first, let data = head.mData else { return false }
        let width = MemoryLayout<Float>.size
        let packed = Int(head.mNumberChannels)
        if list.count == 1, packed > 1 {
            guard Int(head.mDataByteSize) >= (offset + frames) * packed * width else { return false }
            planes[0] = data.assumingMemoryBound(to: Float.self) + offset * packed
            SessionAudio.foldToMono(
                input: planes, frames: frames, channels: packed, interleaved: true,
                pinned: pinned, skip: skip, out: mono)
            return true
        }
        let channels = min(list.count, Self.channels)
        for c in 0..<channels {
            guard let plane = list[c].mData,
                  Int(list[c].mDataByteSize) >= (offset + frames) * width
            else { return false }
            planes[c] = plane.assumingMemoryBound(to: Float.self) + offset
        }
        SessionAudio.foldToMono(
            input: planes, frames: frames, channels: channels, interleaved: false,
            pinned: pinned, skip: skip, out: mono)
        return true
    }
}

/// The sink node and the worker behind it. Attach `node` downstream of the input, then `start`
/// once the input's format is known.
final class MicUplink: @unchecked Sendable {
    let node: AVAudioSinkNode
    private let ring: MicRing
    private let wake: DispatchSemaphore
    private let stopped = StopFlag()

    /// `pinned` is the 0-based input channel to take; nil sums them all but the 0-based `skip`
    /// (loopback inputs). One second of ring at 192 kHz: a worker stalled for longer than that
    /// has lost the audio anyway.
    init(pinned: Int?, skip: [Int] = []) {
        let ring = MicRing(capacity: 192_000)
        let wake = DispatchSemaphore(value: 0)
        let scratch = FoldScratch()
        self.ring = ring
        self.wake = wake
        node = AVAudioSinkNode { _, frameCount, inputData in
            let list = UnsafeMutableAudioBufferListPointer(UnsafeMutablePointer(mutating: inputData))
            let total = Int(frameCount)
            var done = 0
            while done < total {
                let step = min(total - done, FoldScratch.frames)
                guard scratch.fold(list, offset: done, frames: step, pinned: pinned, skip: skip)
                else { break }
                ring.write(scratch.mono, count: step)
                done += step
            }
            wake.signal()
            return noErr
        }
    }

    /// The most samples one `consume` call carries.
    static let batchLimit = 8192

    /// Start the worker. It calls `consume` on its own thread with each batch of mono samples,
    /// at the input's rate, for as long as the IO thread delivers. The samples are valid for
    /// the call only.
    func start(consume: @escaping (UnsafePointer<Float>, Int) -> Void) {
        let ring = ring, wake = wake, stopped = stopped
        let worker = Thread {
            let batch = UnsafeMutablePointer<Float>.allocate(capacity: Self.batchLimit)
            defer { batch.deallocate() }
            while true {
                wake.wait()
                if stopped.isStopped { return }
                while true {
                    let taken = ring.read(into: batch, limit: Self.batchLimit)
                    if taken == 0 { break }
                    consume(batch, taken)
                }
            }
        }
        worker.name = "io.unom.punktfunk.mic-uplink"
        worker.qualityOfService = .userInteractive
        worker.start()
    }

    /// End the worker. Stop the engine first: a callback after this queues audio nobody reads.
    func stop() {
        stopped.stop()
        wake.signal()
    }
}
#endif
