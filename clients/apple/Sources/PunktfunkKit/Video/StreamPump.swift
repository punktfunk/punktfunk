// Stage-1's pump: one thread pulling AUs from the connection into an
// AVSampleBufferDisplayLayer, with the format description refreshed on every IDR (the host
// opens with an IDR carrying in-band parameter sets; recovery keyframes re-send them — there
// is no out-of-band extradata). `SessionPresenter.start` runs it when stage-1 is chosen.

import AVFoundation
import Foundation
import os

private let pumpLog = ClientLog(category: "video")

/// One pump per instance; create a fresh StreamPump per start (the stop is permanent —
/// a restart hands the old pump its own token, so it can never be revived by a newer start()).
final class StreamPump {
    private let token = StopFlag()
    /// Signalled as the pump thread exits; the first `stop` waits on it.
    private let exited = DispatchSemaphore(value: 0)
    private var joinable = false

    /// Pump thread: pull AUs, wrap, enqueue. Non-IDR AUs before the first format
    /// description are dropped. `onFrame`/`onSessionEnd` fire on the pump thread. Main thread.
    ///
    /// `endToEndMeter` measures capture→ENQUEUE, not capture→glass like the Metal paths: the
    /// layer decodes and presents after the hand-off and reports no presented time, so a
    /// comparison across paths reads it as end-to-end minus decode and display.
    func start(
        connection: PunktfunkConnection,
        layer: AVSampleBufferDisplayLayer,
        endToEndMeter: LatencyMeter? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil
    ) {
        let token = token
        // A pump start is a fresh decoder: the core's post-loss freeze starts over with it.
        connection.recoveryReset()
        // The layer is non-Sendable but its enqueue/flush are documented thread-safe, and after
        // this point only the pump thread drives it — assert that so the @Sendable Thread closure
        // may capture it.
        nonisolated(unsafe) let layer = layer
        layer.flush() // drop any frames a previous connection left queued

        let exited = exited
        let thread = Thread {
            defer { exited.signal() }
            var intake = AUIntake(connection: connection)
            // The core assembles each AU in a pool slot, and the sample is that slot.
            var pool = AccessUnitSlotPool(capacity: AccessUnitSlotPool.defaultCapacity)
            connection.setAccessUnitMemory(pool)
            defer { connection.releaseAccessUnitMemory(pool) }
            var wasFailed = false
            // Every iteration drains its own autorelease pool: this thread has no runloop, so
            // autoreleased CM/layer temporaries would otherwise accumulate until session end.
            // `false` = session over — exit the loop (the closure can't `break` across itself).
            var alive = true
            while alive, !token.isStopped {
                alive = autoreleasepool { () -> Bool in
                do {
                    guard let ready = try intake.next(onFrame: onFrame, onDecodedSize: onDecodedSize)
                    else { return true }
                    let au = ready.au
                    let failed = layer.status == .failed || layer.requiresFlushToResumeDecoding
                    if failed {
                        // Decode wedged (a lost opening IDR, or an iOS interruption that left the
                        // layer ignoring samples): flush and, unless THIS AU is the recovering IDR,
                        // report the loss (the core freezes and asks) and wait for parameter sets.
                        if !wasFailed { pumpLog.warning("video: display layer wedged — flushing + re-anchoring") }
                        layer.flush()
                        if !ready.idr {
                            connection.recoveryLost()
                            intake.requireIDR()
                        }
                    }
                    wasFailed = failed
                    // A delta between a loss and its re-anchor references the lost picture; one
                    // such AU wedges the layer's decoder until an IDR. Withheld, the anchor lands.
                    if ready.step.withhold { return true }
                    pool = AccessUnitSlotPool.grown(
                        for: au, keyframe: ready.idr, from: pool, connection: connection)
                    guard let f = intake.pump.format,
                          let sample = connection.videoCodec.sampleBuffer(au: au, format: f, in: pool),
                          !token.isStopped // don't enqueue a stale frame after a restart
                    else { return true }
                    // Stage-1 has no decode callback, so the core's freeze is folded at ENQUEUE from
                    // the AU's wire flags. A held frame is flagged DoNotDisplay: the layer still
                    // decodes it, keeping the reference chain fed, and the last GOOD picture stays.
                    if !connection.recoveryOnDecoded(flags: au.flags) {
                        // Capture→enqueue (see start's doc). Only frames that will DISPLAY:
                        // a withheld frame never reaches glass, so its enqueue instant would
                        // dilute the population the Metal rungs are compared against. The
                        // offset is read PER ENQUEUE — it is live (mid-stream re-synced) and
                        // caching it rebuilds the stale-offset corruption (see clockOffsetNs).
                        endToEndMeter?.record(ptsNs: au.ptsNs, offsetNs: connection.clockOffsetNs)
                    } else {
                        StreamPump.setDoNotDisplay(sample)
                    }
                    layer.enqueue(sample)
                    return true
                } catch {
                    if !token.isStopped {
                        onSessionEnd?()
                    }
                    return false // session closed
                }
                }
            }
        }
        thread.name = "punktfunk-pump"
        thread.qualityOfService = .userInteractive
        joinable = true
        thread.start()
    }

    /// Flag a sample decode-but-don't-display (`kCMSampleAttachmentKey_DoNotDisplay`). Used to
    /// withhold decoder-concealed post-loss frames while the re-anchor gate holds: the layer keeps
    /// its reference chain fed without flipping the frozen picture. No-op if the attachments array
    /// can't be materialized (then the frame just displays — the freeze degrades to the old behavior).
    private static func setDoNotDisplay(_ sample: CMSampleBuffer) {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(
            sample, createIfNecessary: true), CFArrayGetCount(attachments) > 0
        else { return }
        let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
        CFDictionarySetValue(
            dict,
            Unmanaged.passUnretained(kCMSampleAttachmentKey_DoNotDisplay).toOpaque(),
            Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
    }

    /// Stop pumping and wait for the pump thread, at most 0.5 s (one 100 ms poll in practice),
    /// so a restart's pump never runs beside it. Does not close the connection. Main thread.
    func stop() {
        token.stop()
        guard joinable else { return }
        joinable = false
        _ = exited.wait(timeout: .now() + 0.5)
    }

    deinit { token.stop() }
}
