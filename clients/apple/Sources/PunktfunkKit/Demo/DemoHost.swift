// Demo mode's host: the core's `punktfunk_demo_host_*` loopback host plus the picture it
// streams. The core speaks the protocol on 127.0.0.1; this class renders `DemoScene`, encodes
// it and feeds the access units back, on its own queue at the session's frame rate. A client
// dials `port` pinned to `fingerprint` like any saved host.

import Foundation
import os
import PunktfunkCore

public final class DemoHost: @unchecked Sendable {
    public let port: UInt16
    /// SHA-256 of the host certificate: the pin to dial it with.
    public let fingerprint: Data

    private let handle: OpaquePointer
    /// Library id → title, for the launched title the picture names.
    private let titles: [String: String]
    private let queue = DispatchQueue(label: "io.unom.punktfunk.demo-host", qos: .userInteractive)
    // Render state, touched only on `queue`.
    private var timer: DispatchSourceTimer?
    private var timerHz = 0
    private var generation: UInt32 = 0
    private var encoder: DemoEncoder?
    private var forceKeyframe = false
    private var stopped = false
    private let scene = DemoScene()
    /// The generation whose AUs may reach the core. The encoder's output arrives on a
    /// VideoToolbox thread, and a superseded encoder's flush must not land in the new session.
    private let liveGeneration = OSAllocatedUnfairLock<UInt32>(initialState: 0)

    /// Start listening on a free loopback port. Nil if the core could not bind one.
    public init?(titles: [String: String]) {
        guard let handle = punktfunk_demo_host_start(UInt8(PUNKTFUNK_CODEC_H264)) else { return nil }
        self.handle = handle
        self.titles = titles
        port = punktfunk_demo_host_port(handle)
        var sha = [UInt8](repeating: 0, count: 32)
        _ = punktfunk_demo_host_fingerprint(handle, &sha)
        fingerprint = Data(sha)
        queue.async { self.schedule(hz: 10) }
    }

    deinit { stop() }

    /// End any session and free the core host. Idempotent.
    public func stop() {
        let wasRunning: Bool = queue.sync {
            guard !stopped else { return false }
            stopped = true
            timer?.cancel()
            timer = nil
            // The encoder's deinit flushes its callbacks before the handle goes away.
            encoder = nil
            return true
        }
        if wasRunning { punktfunk_demo_host_stop(handle) }
    }

    private func schedule(hz: Int) {
        guard !stopped else { return }
        timer?.cancel()
        let t = DispatchSource.makeTimerSource(flags: .strict, queue: queue)
        t.schedule(deadline: .now(), repeating: .nanoseconds(1_000_000_000 / hz), leeway: .milliseconds(1))
        t.setEventHandler { [weak self] in self?.tick() }
        t.resume()
        timer = t
        timerHz = hz
    }

    private func tick() {
        let now = ProcessInfo.processInfo.systemUptime
        var info = PunktfunkDemoSession()
        guard punktfunk_demo_host_session(handle, &info) else {
            // No client: drop the encoder and poll slowly.
            encoder = nil
            generation = 0
            if timerHz != 10 { schedule(hz: 10) }
            return
        }
        if info.generation != generation { begin(info) }
        let hz = Int(max(1, info.refresh_hz))
        if timerHz != hz { schedule(hz: hz) }

        var ev = PunktfunkInputEvent()
        while punktfunk_demo_host_next_input(handle, &ev) { scene.apply(ev, now: now) }

        guard let encoder, let buffer = encoder.makePixelBuffer() else { return }
        scene.render(into: buffer, now: now)
        let keyframe = punktfunk_demo_host_take_keyframe_request(handle) || forceKeyframe
        forceKeyframe = false
        encoder.encode(buffer, keyframe: keyframe)
    }

    /// A new session or mode: fresh encoder at the negotiated size, opening on an IDR.
    private func begin(_ info: PunktfunkDemoSession) {
        generation = info.generation
        liveGeneration.withLock { $0 = info.generation }
        encoder = nil
        let gen = info.generation
        let (handle, live) = (handle, liveGeneration)
        encoder = DemoEncoder(
            width: Int(info.width), height: Int(info.height), fps: Int(info.refresh_hz),
            bitrateKbps: Int(info.bitrate_kbps)
        ) { au, keyframe in
            guard live.withLock({ $0 }) == gen else { return }
            au.withUnsafeBytes { raw in
                _ = punktfunk_demo_host_submit_video(
                    handle, raw.bindMemory(to: UInt8.self).baseAddress, UInt(raw.count), keyframe)
            }
        }
        scene.resize(width: Int(info.width), height: Int(info.height))
        scene.title = launchTitle()
        forceKeyframe = true
    }

    private func launchTitle() -> String {
        var buf = [CChar](repeating: 0, count: 256)
        let n = punktfunk_demo_host_launch(handle, &buf, UInt(buf.count))
        guard n > 0, n < UInt(buf.count) else { return "Desktop" }
        let id = String(cString: buf)
        return titles[id] ?? id
    }
}
