// The audio drain loop: pull decoded PCM off the connection, place it against the picture, and
// write it into the ring.
//
// Its own type because the loop is closed. The thread captures the connection, the stop flag and
// the ring and NEVER `SessionAudio` — so it keeps draining a session being torn down and stops on
// the flag rather than on a deallocation. Taking those as parameters is what keeps that true:
// there is no `self` here to reach for.

import AVFoundation
import Foundation
import PunktfunkCore

private let log = ClientLog(category: "audio")

enum AudioDrain {
    /// Start the drain thread. Every argument is read on the CALLER's thread, because the values
    /// derive from the connection and the loop must not hold the owner to ask.
    static func start(
        connection: PunktfunkConnection,
        flag: StopFlag,
        done: DispatchSemaphore,
        ring: AudioRing,
        videoLatency: LatencyMeter?,
        channels: Int,
        rateHz: Int,
        frameUs: Int,
        frameMS: Int
    ) {
    let thread = Thread { [connection, flag, done] in
        defer { done.signal() }
        var drained = 0
        var av = AvSync(channels: channels, rateHz: rateHz, frameUs: frameUs)
        // The drought half of concealment: core heals a gap only once a later packet reveals
        // it, so a wire that simply goes quiet drains the ring into a de-prime whose re-prime is
        // a longer artifact than the missing audio. Given the SESSION's frame, like the ring —
        // it spends a wall-clock budget one frame at a time, and assuming 5 ms would misreport
        // a 2 ms lossless session by two and a half times.
        var drought = DroughtConceal(maxMS: JitterPolicy.plcMaxMS, frameUs: frameUs)
        var lastPacketNs = DispatchTime.now().uptimeNanoseconds
        // Something has decoded, so there is both state to conceal from and continuity to
        // hold. Until then a session whose host never sends audio keeps the long timeout below
        // rather than waking two hundred times a second to do nothing.
        var decoded = false
        // Decode happens IN-CORE (libopus multistream) — AudioToolbox's Opus path is
        // stereo-only — and is handed back as interleaved f32 PCM in wire channel order.
        // Per-iteration autorelease pool: no runloop on this thread (see Stage2Pipeline).
        var alive = true
        while alive, !flag.isStopped {
            alive = autoreleasepool { () -> Bool in
            let pcm: PunktfunkConnection.AudioPCM?
            do {
                // Wait at most one frame WHILE there is a stream to protect: the drought
                // decision has to be made on the wire's schedule, not whenever the next packet
                // happens to turn up. The SESSION's frame, so a lossless plane sending every
                // 2 ms is not judged on a 5 ms clock.
                pcm = try connection.nextAudioPcm(
                    timeoutMs: decoded ? UInt32(frameMS) : 100)
            } catch PunktfunkClientError.status(let rc)
                where rc == PUNKTFUNK_STATUS_BAD_PACKET.rawValue
            {
                // One undecodable packet costs that packet, as on the desktop: not the audio.
                return true
            } catch {
                return false // session closed
            }
            guard let pcm, pcm.frameCount > 0 else {
                // Nothing on the wire: conceal from the decoder's own state, bounded by the
                // de-prime fuse so a dead stream is not papered over. ONE frame per tick, so
                // concealment keeps pace with playout instead of racing a stale depth reading.
                guard decoded else { return true }
                let quietMS = Int(
                    (DispatchTime.now().uptimeNanoseconds &- lastPacketNs) / 1_000_000)
                guard drought.conceal(sinceLastPacketMS: quietMS, depthMS: ring.bufferedMS)
                else {
                    return true
                }
                let plc: PunktfunkConnection.AudioPCM?
                do {
                    plc = try connection.audioPlc()
                } catch {
                    return false // session closed
                }
                if let plc {
                    plc.samples.withUnsafeBufferPointer { p in
                        if let base = p.baseAddress {
                            ring.write(base, count: plc.frameCount * plc.channels)
                        }
                    }
                }
                ring.notePlcMS(drought.totalMS)
                return true
            }
            decoded = true
            lastPacketNs = DispatchTime.now().uptimeNanoseconds
            drought.packet()
            // Place this frame against the picture it belongs with BEFORE queueing it: the
            // depth read here is everything that must still play first, which is exactly what
            // delays it. Skipped wholesale when no meter was wired, so an un-armed session
            // does not even read the ring.
            if let videoLatency {
                let depth = ring.bufferedSamples
                let nowNs = realtimeNowNs()
                // Half a second of tolerance on the reference, and steer only on an
                // observation the sync ACCEPTED: the desired depth builds on the current one,
                // so re-requesting it against a frozen offset walks the ring to its cap.
                let accepted = av.observe(AvSync.Observation(
                    ptsNs: pcm.ptsNs, nowLocalNs: nowNs,
                    clockOffsetNs: connection.clockOffsetNs, bufferedAhead: depth,
                    outputLatencyNs: ring.outputLatencyNs,
                    videoE2eNs: videoLatency.latestSample(asOfNs: nowNs, maxAgeMs: 500)))
                if accepted != nil {
                    ring.setSyncTarget(av.desiredDepth(currentDepth: depth))
                }
                ring.noteAvOffset(av.offsetMS)
            }
            pcm.samples.withUnsafeBufferPointer { p in
                if let base = p.baseAddress {
                    ring.write(base, count: pcm.frameCount * pcm.channels)
                }
            }
            // Periodic vitals, so an audio report arrives with numbers. `plc_ms` rides along
            // because healthy `underruns` bought with a climbing `plc_ms` is a link in trouble;
            // `rate_hz`/`frame_us` lead so a log says which plane and frame it was sized on.
            drained += 1
            if drained % 2_000 == 0 {
                let s = ring.stats
                log.info(
                    "audio: rate_hz=\(rateHz) frame_us=\(frameUs) buffer_ms=\(s.bufferedMS) target_ms=\(s.targetMS) underruns=\(s.underruns) drift_sheds=\(s.sheds) drift_inserts=\(s.inserts) av_offset_ms=\(s.avOffsetMS) plc_ms=\(s.plcMS)"
                )
            }
            return true
            }
        }
    }
        thread.name = "punktfunk-audio"
        thread.qualityOfService = .userInteractive
        thread.start()
    }
}
