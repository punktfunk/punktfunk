// The mic uplink's capture half.
//
// A sink node on the input runs on the IO thread once per IO cycle: the device buffer, by
// default 512 frames (10.7 ms at 48 kHz) on a Mac (macOS 27). It folds that buffer to mono and
// copies it into a ring, and a worker thread drains the ring into the resample and Opus chain.
// An engine tap delivers at least 100 ms per buffer (AVAudioNode.h), and the uplink would pay
// that as voice delay.
//
// The IO thread allocates nothing and logs nothing. It takes one lock, held for a copy.

import AVFoundation
import os

/// Which input channels become the mono mic. A 1-based `pick` copies that channel: a pro
/// interface has the mic on one of N inputs, where a converter's downmix would take 0 and 1.
/// A pick of 0, or one the device does not have, sums every channel but the 0-based `loopback`
/// inputs, which carry what the Mac plays. A loopback list covering every channel is ignored.
struct MicChannelPlan: Equatable, Sendable {
    let pick: Int
    let loopback: [Int]

    /// The 0-based channel to copy, or nil to mix.
    func pinned(channels: Int) -> Int? {
        pick >= 1 && pick <= channels ? pick - 1 : nil
    }

    /// Whether a mix of `channels` leaves the loopback inputs out.
    func skipsLoopback(channels: Int) -> Bool {
        !(0..<channels).allSatisfy { loopback.contains($0) }
    }

    /// The fold as the log names it: "channel 2/4", "mix 3/4ch→mono" or "mono".
    func label(channels: Int) -> String {
        if pinned(channels: channels) != nil { return "channel \(pick)/\(channels)" }
        guard channels > 1 else { return "mono" }
        let mixed = skipsLoopback(channels: channels)
            ? (0..<channels).filter { !loopback.contains($0) }.count : channels
        return "mix \(mixed)/\(channels)ch→mono"
    }

    /// Fold `channels` of input into `out`, `frames` long. `input` has the `floatChannelData`
    /// layout: one buffer strided by the channel count when `interleaved`, else one per channel.
    /// A mix is clamped to [-1, 1], so a lone hot channel passes at full level. Allocation-free:
    /// it runs on the IO thread.
    func fold(
        input: UnsafePointer<UnsafeMutablePointer<Float>>, frames: Int, channels: Int,
        interleaved: Bool, out: UnsafeMutablePointer<Float>
    ) {
        if let ch = pinned(channels: channels) {
            if interleaved {
                let d = input[0]
                for i in 0..<frames { out[i] = d[i * channels + ch] }
            } else {
                let d = input[ch]
                for i in 0..<frames { out[i] = d[i] }
            }
            return
        }
        let skip = skipsLoopback(channels: channels)
        out.update(repeating: 0, count: frames)
        for c in 0..<channels where !skip || !loopback.contains(c) {
            if interleaved {
                let d = input[0]
                for i in 0..<frames { out[i] += d[i * channels + c] }
            } else {
                let d = input[c]
                for i in 0..<frames { out[i] += d[i] }
            }
        }
        if channels > 1 { for i in 0..<frames { out[i] = max(-1, min(1, out[i])) } }
    }
}

/// Cuts the worker's resampled mono into the encoder's packet size and numbers the packets the
/// encoder returns. A remainder short of a packet waits for the next batch. Worker-confined.
struct MicPacketizer {
    let packetFrames: Int
    private var fifo: [Float] = []
    private var seq: UInt32 = 0

    init(packetFrames: Int) {
        self.packetFrames = packetFrames
        fifo.reserveCapacity(48_000)
    }

    /// Queue `samples`, give each whole packet to `encode`, and each packet it returns to `send`
    /// with its sequence number. A packet `encode` refuses (nil) is dropped.
    mutating func push(
        _ samples: UnsafeBufferPointer<Float>,
        encode: (UnsafeBufferPointer<Float>) -> [Data]?,
        send: (Data, UInt32) -> Void
    ) {
        fifo.append(contentsOf: samples)
        var head = 0
        while fifo.count - head >= packetFrames {
            let packets = fifo.withUnsafeBufferPointer {
                encode(UnsafeBufferPointer(rebasing: $0[head..<head + packetFrames]))
            }
            head += packetFrames
            for packet in packets ?? [] {
                send(packet, seq)
                seq &+= 1
            }
        }
        if head > 0 { fifo.removeFirst(head) } // keeps capacity: no realloc
    }
}

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
        _ list: UnsafeMutableAudioBufferListPointer, offset: Int, frames: Int,
        plan: MicChannelPlan
    ) -> Bool {
        guard let head = list.first, let data = head.mData else { return false }
        let width = MemoryLayout<Float>.size
        let packed = Int(head.mNumberChannels)
        if list.count == 1, packed > 1 {
            guard Int(head.mDataByteSize) >= (offset + frames) * packed * width else { return false }
            planes[0] = data.assumingMemoryBound(to: Float.self) + offset * packed
            plan.fold(
                input: planes, frames: frames, channels: packed, interleaved: true, out: mono)
            return true
        }
        let channels = min(list.count, Self.channels)
        for c in 0..<channels {
            guard let plane = list[c].mData,
                  Int(list[c].mDataByteSize) >= (offset + frames) * width
            else { return false }
            planes[c] = plane.assumingMemoryBound(to: Float.self) + offset
        }
        plan.fold(
            input: planes, frames: frames, channels: channels, interleaved: false, out: mono)
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

    /// The IO thread folds each quantum by `plan`. One second of ring at 192 kHz: a worker
    /// stalled for longer than that has lost the audio anyway.
    init(plan: MicChannelPlan) {
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
                guard scratch.fold(list, offset: done, frames: step, plan: plan)
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
