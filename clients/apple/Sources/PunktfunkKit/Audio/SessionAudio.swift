// Session audio, both directions:
//
//   host → speaker: a drain thread pulls audio packets off their own plane in the core, which
//   decodes them there (nextAudioPcm) and hands back interleaved f32, and writes that into a
//   jitter ring; an AVAudioSourceNode pulls from the ring (silence on underrun with re-priming,
//   so a network gap costs one dip, not permanent crackle).
//
//   mic → host: a tap on the input node folds the capture to one mono bus (the chosen channel
//   of a multi-channel interface, or a sum of all channels), resamples to 48 kHz mono, slices
//   10 ms chunks, Opus-encodes, and sendMic()s each packet — the host feeds them into a
//   virtual PipeWire source.
//
// The downlink's FORMAT is negotiated, not assumed. `connection.resolvedAudioRateHz` is 48 kHz
// for every Opus session and every host older than the lossless plane, and any rate on the
// lossless ladder (44 100 / 48 000 / 88 200 / 96 000 / 176 400) on `0xD3` — and it is what the
// ring, the A/V sync loop and the render graph's AVAudioFormat are all built from
// (design/hi-res-audio.md §9). Its CHANNEL count is negotiated the same way and is no longer
// stereo on the lossless plane either: the frame ladder is sized per channel count, so a 5.1/7.1
// lossless session simply arrives on a shorter frame. The UPLINK is deliberately untouched: Opus
// is 48 kHz by construction and the mic carries voice, so §3 excludes it.
//
// Engine topology. With the mic enabled and echo cancellation on (both defaults), BOTH
// directions run on ONE AVAudioEngine with the system voice processor engaged
// (`setVoiceProcessingEnabled`) — AEC needs render and capture on the same unit so it can
// subtract what the speaker is playing from what the mic hears; without it, a loudspeaker
// client feeds the host's own game audio straight back to it (the primary reported echo
// source). The voice processor can only follow the system DEFAULT devices, so explicit
// endpoint choices fall back to the old two-engine topology — see `wantsCombined` for the
// exact decision, and `startCapture` for why two engines handle arbitrary device pairs.
//
// Devices are chosen by UID ("" = system default: the engine is then never pinned to a
// concrete device and follows default-device changes).
//
// Surviving the hardware. An AVAudioEngine does NOT follow the audio hardware: when the output
// device changes underneath a running engine, the engine stops itself and stays stopped. The
// session therefore watches for that and rebuilds its engines — see "Device changes" below.

import AVFoundation
import os

private let log = ClientLog(category: "audio")

/// Render-block-owned scratch storage: freed exactly when the closure (and thus the
/// last possible render call) is released — never racing CoreAudio.
private final class ScratchBuffer {
    // 8192 frames × up to 8 channels (7.1) — the render block caps `frames` at 8192.
    let ptr = UnsafeMutablePointer<Float>.allocate(capacity: 8192 * 8)
    deinit { ptr.deallocate() }
}

public final class SessionAudio {
    private let connection: PunktfunkConnection
    private let flag = StopFlag()
    private let drainDone = DispatchSemaphore(value: 0)
    /// Owns the engine handles + drainStarted, paired with `flag`: stop() sets the flag
    /// BEFORE taking the engines, every publisher re-checks the flag under this lock
    /// after publishing-side work — so a startCapture racing stop() (the mic-permission
    /// callback arrives whenever the user clicks the prompt) can never leave a hot
    /// microphone with no owner.
    private let stateLock = NSLock()
    private var playbackEngine: AVAudioEngine?
    private var captureEngine: AVAudioEngine?
    /// The one engine running BOTH directions when the voice processor is engaged;
    /// `playbackEngine`/`captureEngine` stay nil while this is set.
    private var combinedEngine: AVAudioEngine?
    private var drainStarted = false
    /// The mute LATCH: the effective mute the owner last asked for (see `setMicMuted`). Held
    /// because the uplink engine can appear LATER than the request — the mic permission prompt
    /// is answered at the user's leisure, and the engine is built on the grant — so a mute set
    /// in the meantime must be waiting for it. Applied by whichever start path wins the race.
    /// Guarded by `stateLock`, like the engines it applies to.
    private var micMuted = false
    /// The playback jitter ring — created by whichever engine starts playback first and KEPT
    /// across an engine rebuild (the permission-grant upgrade in `startEngines` swaps engines,
    /// not the ring, so the drain thread never has to be re-pointed). Guarded by `stateLock`:
    /// the start paths run on `engineQueue`, while `stats` reads from the main thread.
    private var ring: AudioRing?
    /// Every engine build, start, stop and rebuild runs here, serially — and NOT on the main
    /// thread. macOS captures and sends input from the main thread, so the seconds a
    /// voice-processing start can take (~1.9 s measured in the 2026-08-14 field loop) would
    /// freeze the stream's input for exactly that long — the recovery must never make the main
    /// thread wait on the audio server. The main queue keeps only the trigger bookkeeping
    /// (debounce, backoff, retry ladder), which is cheap by construction.
    private let engineQueue = DispatchQueue(
        label: "io.unom.punktfunk.audio.engines", qos: .userInitiated)
    /// The video plane's end-to-end meter (capture→on-glass), if the owner wired one — the
    /// reference the A/V sync loop steers the ring against. `nil` leaves the loop inert and the
    /// ring exactly as it was before sync existed, which is also what the stage-1 fallback
    /// presenter gets: it decodes and presents inside the layer with no per-frame stamp, so it can
    /// offer no reference, and a loop with no reference must not invent one. Written ONCE in
    /// `start()` before anything is dispatched (the queue hop orders it for `startDrain`); the
    /// meter itself is internally locked and read from the drain thread.
    private var videoLatency: LatencyMeter?
    #if !os(macOS)
    /// AVAudioSession `setCategory`/`setActive` are synchronous and block on the audio server, so
    /// they must not run on the main thread (UI stall — AVFoundation warns about it). PROCESS-WIDE
    /// (static) so every SessionAudio shares one serial queue: the AVAudioSession is a process
    /// singleton, and across a reconnect the old session's deactivate must be ordered before the
    /// new session's activate (a per-instance queue would let them race and leave the new session's
    /// audio deactivated). stop() enqueues its deactivate promptly so it lands before the next
    /// session's activate.
    private static let sessionQueue = DispatchQueue(label: "io.unom.punktfunk.audio.session")
    #endif
    #if !os(macOS)
    /// Token for the route-change observer: it revives an engine the route change stopped, and on
    /// iOS re-applies the earpiece steer (see `installRouteObserver`). Guarded by `stateLock`.
    private var routeObserver: NSObjectProtocol?
    /// Token for the media-services-reset observer — the audio server restarting takes the
    /// session's configuration and every engine with it. Guarded by `stateLock`.
    private var mediaResetObserver: NSObjectProtocol?
    /// Token for the interruption observer — a phone call or a non-mixable app stops the engines,
    /// and ending the interruption restarts nothing by itself (see
    /// `installInterruptionObserver`). Guarded by `stateLock`.
    private var interruptionObserver: NSObjectProtocol?
    #endif

    // MARK: - Device changes (see `installDeviceChangeRecovery`)

    /// What `start()` was asked for, so a rebuild can put back the SAME topology the session was
    /// started with. Guarded by `stateLock` (written on the caller's thread, read when a rebuild
    /// fires on the main queue).
    private var startConfig: StartConfig?
    private struct StartConfig {
        let speakerUID: String
        let micUID: String
        let micChannel: Int
        let micEnabled: Bool
        let echoCancel: Bool
    }
    /// Watches the hardware for us (see `AudioDeviceWatcher`). Guarded by `stateLock`.
    private var deviceWatcher: AudioDeviceWatcher?
    /// Whether the engines have been built at least once. Distinguishes "not started yet" (every
    /// platform starts asynchronously now) from "started and dead", which is what the recovery
    /// may act on. Guarded by `stateLock` (set on `engineQueue`, read on the main queue).
    private var enginesAttempted = false
    /// A rebuild is already on the main queue — one device switch produces a burst of triggers
    /// and they must collapse into one restart. Main-thread confined.
    private var rebuildQueued = false
    /// Debounce/floor for the next rebuild, with an escalating floor when rebuilds chain (each
    /// retriggered by its predecessor — see `RebuildBackoff`). Main-thread confined.
    private var rebuildBackoff = RebuildBackoff()
    #if os(macOS)
    /// Latches a voice-processing start failure per input device, so a rebuild never re-attempts
    /// a topology that deterministically fails — the retry is what turned one failure into a
    /// rebuild loop (see `CombinedTopologyGate` and the note on `installDeviceChangeRecovery`).
    /// `engineQueue`-confined, like the start paths that consult and feed it.
    private var combinedGate = CombinedTopologyGate()
    #endif
    /// Retries when a rebuild's `start()` loses the race with a device that is still going away
    /// (0.3 s, 0.6 s, 1.2 s). A failed rebuild leaves no engine to post the next notification,
    /// so this ladder — and, on macOS, the HAL listener — is all that stands between a mistimed
    /// switch and a silent session.
    private static let rebuildAttempts = 3

    public init(connection: PunktfunkConnection) {
        self.connection = connection
    }

    /// Backstop for an owner dropping us without stop() — unblocks the drain thread
    /// (which captures the connection strongly, NOT self) within one poll timeout.
    /// Engine teardown still belongs to stop().
    deinit {
        flag.stop()
        // The observers only hold self weakly, so we can be deinited with them still registered;
        // drop them here too rather than leaking them when an owner skips stop().
        deviceWatcher?.stop()
        #if !os(macOS)
        if let routeObserver { NotificationCenter.default.removeObserver(routeObserver) }
        if let mediaResetObserver {
            NotificationCenter.default.removeObserver(mediaResetObserver)
        }
        if let interruptionObserver {
            NotificationCenter.default.removeObserver(interruptionObserver)
        }
        #endif
    }

    /// Start playback (and, if enabled+authorized, the mic uplink). Empty UIDs = system default
    /// device; on iOS the UIDs are ignored entirely (routes are AVAudioSession-managed).
    /// ASYNCHRONOUS on every platform: the engines start on `engineQueue` (iOS/tvOS activate the
    /// AVAudioSession off the main thread first), gated by `!flag.isStopped` — so playback is
    /// live shortly after, not on return. An engine start can block on the audio server for
    /// seconds, and the caller's (main) thread is where macOS input capture lives — it must
    /// never wait. The mic may start later still if the permission prompt is pending.
    /// `echoCancel` picks the engine topology — see the header note and `wantsCombined`.
    ///
    /// `videoLatency` is the session's END-TO-END latency meter (capture→on-glass). Pass it to arm
    /// A/V sync: it is the only thing that tells the audio plane where the picture actually is, and
    /// without it the ring keeps today's free-running behaviour. Omit it for a playback-only or
    /// stage-1 session, where no such figure is measured.
    public func start(
        speakerUID: String, micUID: String, micChannel: Int, micEnabled: Bool, echoCancel: Bool,
        videoLatency: LatencyMeter? = nil
    ) {
        self.videoLatency = videoLatency // before any dispatch below — startDrain reads it
        // Before any engine exists: the recovery watches the hardware, not the engines, and the
        // config it rebuilds from has to be recorded whether or not this start succeeds.
        stateLock.lock()
        startConfig = StartConfig(
            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
            micEnabled: micEnabled, echoCancel: echoCancel)
        stateLock.unlock()
        installDeviceChangeRecovery(micEnabled: micEnabled)
        #if os(macOS)
        // No AVAudioSession on macOS — but the engines start on `engineQueue`, never the
        // caller's (main) thread: a voice-processing start can block on the audio server for
        // seconds, and the main thread is where input capture lives.
        engineQueue.async { [weak self] in
            guard let self, !self.flag.isStopped else { return }
            self.startEngines(
                speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
                micEnabled: micEnabled, echoCancel: echoCancel)
        }
        #else
        // Configure + activate the session OFF the main thread (it blocks on the audio server),
        // then start the engines on `engineQueue` once it's active — engine routing/format
        // depend on the active session. A stop() racing in between is caught by the flag guard.
        Self.sessionQueue.async { [weak self] in
            guard let self else { return }
            self.activateAudioSession(micEnabled: micEnabled)
            self.engineQueue.async { [weak self] in
                guard let self, !self.flag.isStopped else { return }
                self.startEngines(
                    speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
                    micEnabled: micEnabled, echoCancel: echoCancel)
            }
        }
        #endif
    }

    /// The rate the samples on the wire are actually at — `Welcome`'s RESOLVED figure, not what
    /// this client asked for. 48 000 on every Opus session and every host older than the lossless
    /// plane; any rate on the ladder (44 100 / 48 000 / 88 200 / 96 000 / 176 400) on `0xD3`.
    /// Everything that turns samples into time — the ring's ms ⇄ sample conversion, the A/V sync
    /// loop's, and the `AVAudioFormat` the render graph is built at — is denominated in this,
    /// because it is what `nextAudioPcm` hands back.
    private var wireRateHz: Int { Int(connection.resolvedAudioRateHz) }

    /// How much audio one datagram carries, in MICROSECONDS — `Welcome`'s resolved
    /// `audio_frame_us`. 5 000 on every Opus session; on the lossless plane the host sizes it so
    /// the payload fits one datagram, which is 4 000 at 48 kHz/24-bit stereo, 2 000 at
    /// 96 kHz/24-bit stereo, and shorter again for surround (a 5.1 frame carries three times the
    /// samples, so it drops to roughly 1 000–1 500). Microseconds because the ladder has
    /// sub-millisecond rungs (`AudioRing.setFrameUs`).
    private var wireFrameUs: Int { Int(connection.resolvedAudioFrameUs) }

    /// The same figure rounded UP to whole milliseconds, for the one consumer that can only express
    /// itself in them: the drain thread's poll timeout. Rounding up rather than down keeps it "at
    /// most one frame" — a 2 500 µs frame polls at 3 ms, never 2, so the loop cannot spin a wake-up
    /// per frame for nothing. Never below 1.
    private var wireFrameMS: Int { max(1, (wireFrameUs + 999) / 1000) }

    /// IO quantum asked of `AVAudioSession` on iOS/tvOS, every session, mic on or off. Two wire
    /// packets: the playback ring's floor is one quantum plus one packet, so this sets that floor
    /// at 15 ms. Best-effort — `activateAudioSession` logs the granted value.
    static let preferredIOBufferSeconds: Double = 0.010

    #if !os(macOS)
    /// Route + policy live in the session, not per-engine: playback at the negotiated rate and
    /// channel count, mic capture when enabled, Bluetooth allowed on iOS. Failure is non-fatal
    /// (defaults). Runs on `sessionQueue`.
    private func activateAudioSession(micEnabled: Bool) {
        let session = AVAudioSession.sharedInstance()
        let wanted = Double(wireRateHz)
        do {
            #if os(iOS)
            if micEnabled {
                // NO .defaultToSpeaker here, deliberately. It reads like "prefer the speaker over
                // the earpiece", and the comment that used to sit here claimed headphones and
                // Bluetooth still won. That is true of WIRED headphones and false of Bluetooth —
                // a cable is the one way to test this and see the right answer. It is an
                // OVERRIDE, and it outranks an A2DP route: with it set, every Bluetooth headset
                // lost the stream to the phone's own speaker. That is the 0.25 field report ("no
                // audio over Bluetooth ... plays through speakers if Mic input is enabled") — mic
                // and echo cancellation both default to ON, so this branch is the DEFAULT path
                // and every Bluetooth listener hit it; turning the mic off was the accidental
                // workaround, because that lands on `.playback` below, which routes to A2DP
                // happily.
                //
                // The earpiece problem it was reaching for is real, so it is solved after
                // activation instead, against the route we were ACTUALLY given —
                // see `steerBuiltInOutputToSpeaker`.
                //
                // `.allowBluetoothA2DP` alone, also deliberately: adding `.allowBluetooth` would
                // make a headset's MIC usable, but it buys that by dragging the whole route onto
                // HFP/SCO and collapsing game audio to narrowband. High-quality A2DP output plus
                // the built-in mic is the better trade for a game-streaming client.
                // `.mixWithOthers`, both branches: without it this session is EXCLUSIVE — merely
                // activating it paused the user's Music at connect, and Music's RESUME took the
                // session right back, which read as "stream audio stops when I resume Music"
                // (field report; the interruption observer below is the other half of that fix).
                // A game stream mixing over someone's playlist is the behavior a console has,
                // and what this client's peers do. The trade is real but right: a mixable
                // session is nobody's Now Playing app, so the lock screen shows the music, not
                // the stream — which is exactly how it should read.
                try session.setCategory(
                    .playAndRecord, mode: .default,
                    options: [.allowBluetoothA2DP, .mixWithOthers])
            } else {
                try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
            }
            #else // tvOS — no app-accessible mic
            // No `.mixWithOthers` here: a mixable playback session is offered a stereo mix, so
            // HDMI would carry PCM 2.0 whatever the wire negotiated — and there is nothing to
            // mix with on the box anyway.
            try session.setCategory(.playback, mode: .default)
            #endif
            // Asked for on every branch, mic on or off. The hardware IO buffer is one shared,
            // device-wide setting: a session that asks for nothing runs at whatever iOS or another
            // mixing app chose (85 ms seen), and the ring's floor is this quantum plus one packet.
            try? session.setPreferredIOBufferDuration(Self.preferredIOBufferSeconds)
            // The session's rate, on every branch. BEFORE `setActive`: the hardware is configured
            // on activation, and a preference expressed after it only takes effect at the next
            // route change. Best-effort — a Bluetooth route has no 96 kHz mode to give, which is
            // why `noteOutputFormat` reads back what the graph actually got.
            try? session.setPreferredSampleRate(wanted)
            // Channel count, same rule and same ordering as the rate: a session that asks for
            // nothing is offered the route's default — 2 on HDMI — and the mixer then folds a
            // negotiated 5.1 to stereo without a word. Best-effort like the rest: a stereo
            // route answers 2.
            let wireChannels = Int(connection.resolvedAudioChannels)
            if wireChannels > 2 {
                try? session.setPreferredOutputNumberOfChannels(wireChannels)
            }
            try session.setActive(true)
            // Apple validates the channel ask against the ACTIVE route's maximum, so the ask
            // above can come back as 2. Ask again, clamped: an 8-channel wire on a 5.1 AVR gets 6.
            let reachable = min(wireChannels, session.maximumOutputNumberOfChannels)
            if reachable > 2, session.outputNumberOfChannels < reachable {
                try? session.setPreferredOutputNumberOfChannels(reachable)
            }
            // What we were actually GRANTED, not what we asked for. All three are best-effort, and
            // the ring's behaviour depends on the quantum it really gets — without this, a report of
            // audio jitter arrives with no way to tell a 10 ms session from a 5 ms or a 23 ms one,
            // which is exactly the gap that made the last round of this take a simulation to close.
            let grantedMS = session.ioBufferDuration * 1000
            let askedMS = Self.preferredIOBufferSeconds * 1000
            log.info("""
                AVAudioSession active: io_buffer_ms=\
                \(grantedMS, format: .fixed(precision: 2)) asked_ms=\(Int(askedMS)) \
                sample_rate=\(Int(session.sampleRate)) wire_rate=\(Int(wanted)) \
                out_ch=\(session.outputNumberOfChannels)/\(session.maximumOutputNumberOfChannels) \
                wire_ch=\(wireChannels) \
                route=\(session.currentRoute.outputs.first?.portType.rawValue ?? "none") \
                input=\(session.currentRoute.inputs.first?.portType.rawValue ?? "none")
                """)
            // The ring cannot hold less than quantum + one packet, so a grant far above the
            // request IS the audio delay — said at connect, with what is known to cause it.
            if grantedMS > askedMS * 2 {
                log.warning("""
                    AVAudioSession granted a \(grantedMS, format: .fixed(precision: 0)) ms IO \
                    buffer against a \(Int(askedMS)) ms request — expect audio ≥ \
                    \(grantedMS + Double(wireFrameMS), format: .fixed(precision: 0)) ms behind the \
                    picture. Another app's active audio session (this one mixes with others), a \
                    USB/Bluetooth accessory on the route, or Low Power Mode holds the hardware \
                    buffer at this size.
                    """)
            }
            #if os(iOS)
            // Only the `.playAndRecord` session can land on the earpiece, and only it accepts an
            // output override — so the mic-off (`.playback`) path deliberately does neither.
            // (The route OBSERVER that re-applies this per route is installed by
            // `installDeviceChangeRecovery`, for every session — a `.playback` session steers
            // nothing but still has engines a route change can stop.)
            if micEnabled { steerBuiltInOutputToSpeaker(session) }
            #endif
        } catch {
            log.warning("AVAudioSession setup failed: \(error.localizedDescription)")
        }
    }
    #endif

    #if os(iOS)
    /// `.playAndRecord` parks the BUILT-IN output on the earpiece — right for a phone call,
    /// useless for a game. Move it to the speaker, but ONLY when the route we were actually given
    /// is the receiver: anything external (Bluetooth, wired, CarPlay, AirPlay) is left strictly
    /// alone. That "look first" is the whole difference between this and the `.defaultToSpeaker`
    /// option it replaced, which forced the speaker unconditionally and so beat Bluetooth.
    ///
    /// Idempotent and cheap, so the route observer can simply call it again.
    private func steerBuiltInOutputToSpeaker(_ session: AVAudioSession) {
        // An override already in force shows up as `.builtInSpeaker`, not `.builtInReceiver`, so
        // re-running this never fights its own previous result.
        guard session.currentRoute.outputs.contains(where: { $0.portType == .builtInReceiver })
        else { return }
        do {
            try session.overrideOutputAudioPort(.speaker)
        } catch {
            log.warning("speaker override refused: \(error.localizedDescription)")
        }
    }

    #endif

    #if !os(macOS)
    /// Routes change under a live session: a headset connects mid-stream, or disconnects and hands
    /// the stream back to the built-in output. Two things follow from that.
    ///
    /// iOS drops an output override whenever the route changes — which is what lets a newly-
    /// connected headset win — so the earpiece steer is a property of the CURRENT route and has to
    /// be re-applied per route. Without it, dropping Bluetooth mid-stream lands the game on the
    /// earpiece.
    ///
    /// And on every platform a route change can take the engines down with it (see
    /// `installDeviceChangeRecovery`), which is why this is installed for `.playback` sessions and
    /// on tvOS too, where there is no earpiece to steer away from.
    private func installRouteObserver() {
        let observer = NotificationCenter.default.addObserver(
            forName: AVAudioSession.routeChangeNotification,
            object: AVAudioSession.sharedInstance(), queue: nil
        ) { [weak self] _ in
            // Arrives on whatever thread AVFoundation posts it from, and the session API blocks
            // on the audio server — so do the work on the shared session queue, like every
            // other call into it.
            SessionAudio.sessionQueue.async {
                guard let self, !self.flag.isStopped else { return }
                #if os(iOS)
                self.steerBuiltInOutputToSpeaker(AVAudioSession.sharedInstance())
                #endif
                self.reviveStoppedEngines("the audio route changed")
            }
        }
        stateLock.lock()
        let stale = routeObserver
        routeObserver = observer
        stateLock.unlock()
        if let stale { NotificationCenter.default.removeObserver(stale) }
    }
    #endif

    /// Build + start the engines — combined (voice-processed) or split, per `wantsCombined` —
    /// with the mic uplink only when enabled + authorized. Runs on `engineQueue` (a start can
    /// block on the audio server for seconds — never the main thread); on iOS/tvOS the session
    /// is already active by the time this runs.
    private func startEngines(
        speakerUID: String, micUID: String, micChannel: Int, micEnabled: Bool, echoCancel: Bool
    ) {
        stateLock.lock()
        enginesAttempted = true // even if every path below fails — see `reviveStoppedEngines`
        stateLock.unlock()
        #if os(tvOS)
        // No app-accessible microphone input on tvOS — playback only.
        startPlayback(speakerUID: speakerUID)
        #else
        guard micEnabled else {
            startPlayback(speakerUID: speakerUID)
            return
        }
        #if os(macOS)
        // A rebuild must not re-attempt a voice-processing start that already failed on this
        // input device: the failure repeats, and the failed attempt's HAL churn stops the healthy
        // fallback engines — the 2026-08-14 rebuild loop (see `CombinedTopologyGate`).
        var combined = wantsCombined(
            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
            echoCancel: echoCancel)
        if combined, !combinedGate.shouldTry(input: AudioDevices.defaultInputDevice()) {
            log.info("""
                voice processing already failed on this input device — split engines, no echo \
                cancellation
                """)
            combined = false
        }
        #else
        let combined = wantsCombined(
            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel,
            echoCancel: echoCancel)
        #endif
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized:
            if combined {
                startCombined(speakerUID: speakerUID, micUID: micUID, micChannel: micChannel)
            } else {
                startPlayback(speakerUID: speakerUID)
                startCapture(micUID: micUID, micChannel: micChannel)
            }
        case .notDetermined:
            // Playback must not wait out the permission prompt (the user answers at their
            // leisure) — start it now, and on a grant either bolt the capture engine on
            // (split) or swap the playback engine for the combined one (the ring and its
            // drain thread carry over — see `makePlaybackChain`).
            startPlayback(speakerUID: speakerUID)
            AVCaptureDevice.requestAccess(for: .audio) { [weak self] granted in
                guard let self else { return }
                self.engineQueue.async { [weak self] in
                    guard let self, granted, !self.flag.isStopped else { return }
                    if combined {
                        self.stateLock.lock()
                        let playback = self.playbackEngine
                        self.playbackEngine = nil
                        self.stateLock.unlock()
                        playback?.stop()
                        self.startCombined(
                            speakerUID: speakerUID, micUID: micUID, micChannel: micChannel)
                    } else {
                        self.startCapture(micUID: micUID, micChannel: micChannel)
                    }
                }
            }
        default:
            startPlayback(speakerUID: speakerUID)
            log.warning("microphone access denied — mic uplink disabled (System Settings → Privacy)")
        }
        #endif
    }

    #if !os(tvOS)
    /// One engine or two: the voice processor requires render + capture on one unit, and that
    /// unit can only follow the system DEFAULT devices — so echo cancellation gets the combined
    /// engine only while nothing is explicitly pinned. On macOS a chosen speaker/mic UID or a
    /// picked input channel (the voice processor's capture side is its own mono mix — a
    /// per-channel pick can't survive it) keeps today's two-engine path, AEC-less but honoring
    /// the exact endpoints the user named. On iOS routes are session-managed and the UIDs are
    /// ignored, so the toggle alone decides.
    private func wantsCombined(
        speakerUID: String, micUID: String, micChannel: Int, echoCancel: Bool
    ) -> Bool {
        guard echoCancel else { return false }
        #if os(macOS)
        return speakerUID.isEmpty && micUID.isEmpty && micChannel == 0
        #else
        return true
        #endif
    }
    #endif

    /// Stop both directions. Safe from any thread; waits the drain thread out (≤ its
    /// poll timeout) so the caller can close the connection right after.
    public func stop() {
        flag.stop() // before taking the engines — see stateLock's comment
        stateLock.lock()
        let wasDraining = drainStarted
        drainStarted = false
        let watcher = deviceWatcher
        deviceWatcher = nil
        #if !os(macOS)
        let route = routeObserver
        routeObserver = nil
        let mediaReset = mediaResetObserver
        mediaResetObserver = nil
        let interruption = interruptionObserver
        interruptionObserver = nil
        #endif
        stateLock.unlock()
        // Every watcher goes before the engines do: a device change landing during teardown must
        // not schedule a rebuild of a session we are in the middle of releasing. (`flag` already
        // guards that, but not arming the trigger is better than catching it.) On iOS this is
        // also ahead of the deactivate below, so a route change cannot re-steer a dying session.
        watcher?.stop()
        #if !os(macOS)
        if let route { NotificationCenter.default.removeObserver(route) }
        if let mediaReset { NotificationCenter.default.removeObserver(mediaReset) }
        if let interruption { NotificationCenter.default.removeObserver(interruption) }
        #endif
        tearDownEngines()
        #if !os(macOS)
        // Release the session. (A mixable session interrupts nobody, so the resume cue below is
        // now a courtesy for the edge where an OLD non-mixable install interrupted something —
        // harmless either way, and deactivating promptly is still what orders a reconnect.) Like
        // activation, setActive is synchronous/blocking — run it on the shared serial session queue
        // (off the main thread). Enqueued HERE — engines already stopped, and BEFORE the drain wait
        // below — so across a reconnect it lands ahead of the next session's activate on the shared
        // queue (otherwise a deferred deactivate could deactivate the new session). Fire-and-forget.
        Self.sessionQueue.async {
            do {
                try AVAudioSession.sharedInstance().setActive(
                    false, options: .notifyOthersOnDeactivation)
            } catch {
                log.warning("AVAudioSession deactivation failed: \(error.localizedDescription)")
            }
        }
        #endif
        if wasDraining {
            _ = drainDone.wait(timeout: .now() + .milliseconds(400))
        }
    }

    /// Stop and release every engine we own, leaving the ring, the drain thread, the observers and
    /// the audio session alone — the teardown half shared by `stop()` and a rebuild. Safe from any
    /// thread; the engines are taken under the lock before any of them is touched.
    private func tearDownEngines() {
        stateLock.lock()
        let capture = captureEngine
        captureEngine = nil
        let playback = playbackEngine
        playbackEngine = nil
        let combined = combinedEngine
        combinedEngine = nil
        stateLock.unlock()
        if let capture {
            capture.inputNode.removeTap(onBus: 0)
            capture.stop()
        }
        playback?.stop()
        if let combined {
            combined.inputNode.removeTap(onBus: 0)
            combined.stop()
        }
    }

    // MARK: - Device changes

    /// An AVAudioEngine does not follow the audio hardware. When the output device changes under a
    /// running engine — AirPods taken out of an ear, a headset unplugged, the default switched in
    /// System Settings — the engine's IO unit sees the new hardware, THE ENGINE STOPS ITSELF, and
    /// it posts `AVAudioEngineConfigurationChange`. It stays stopped until somebody starts it
    /// again. Nothing here ever did, so from that moment the session rendered silence: no audio on
    /// the speakers the stream had just moved to, and none in the AirPods when they went back in
    /// (that is a second stop, not a recovery), until the whole stream was restarted. Measured on
    /// this exact topology: render callbacks go from ~94/s to zero the instant the default output
    /// device changes, and both restarting the same engine and building a fresh one resume them.
    ///
    /// Three triggers feed one rebuild, because no single one of them covers the ground:
    ///
    ///  - the engine notification, everywhere — the direct signal, but only an engine that still
    ///    EXISTS can post it, so it cannot report a rebuild that failed to start;
    ///  - the HAL default-output-device listener, macOS — independent of any engine and of the
    ///    engine's topology. It is what makes the recovery work for the voice-processing engine
    ///    (mic + echo cancellation, the DEFAULT macOS configuration) without having to assume that
    ///    a VPIO engine posts the notification the plain one demonstrably does;
    ///  - the route-change and media-services-reset notifications, iOS/tvOS, where the session and
    ///    not the device is what moves.
    ///
    /// And three defenses keep the recovery from ANSWERING ITSELF — a rebuild is not a silent
    /// act (a voice-processing start builds and tears down HAL aggregates, and every fresh engine
    /// renegotiates its IO), so its own fallout can retrigger it. The 2026-08-14 field loop was
    /// exactly that: VPIO failed on a 6-channel mic, every rebuild re-tried it, and the failure's
    /// churn stopped the fallback engines — audio and (via the main thread) INPUT cutting out
    /// every ~2.5 s for the whole session. The defenses: a configuration change from an engine
    /// that is RUNNING is a rebuild's echo and is ignored (`hardwareMoved`); a VPIO failure is
    /// latched per input device and never re-attempted on it (`CombinedTopologyGate`); and
    /// rebuilds that chain anyway back off exponentially instead of metronoming
    /// (`RebuildBackoff`).
    ///
    /// `micEnabled` only decides whether the mic-bearing session observers are worth installing.
    /// Main thread.
    private func installDeviceChangeRecovery(micEnabled: Bool) {
        stateLock.lock()
        let already = deviceWatcher != nil
        stateLock.unlock()
        guard !already else { return } // a second start() on one SessionAudio: keep the first set

        let watcher = AudioDeviceWatcher(
            isOurs: { [weak self] posted in self?.ownsEngine(posted) ?? false },
            onChange: { [weak self] reason, posted in self?.hardwareMoved(reason, posted: posted) })
        stateLock.lock()
        deviceWatcher = watcher
        stateLock.unlock()
        watcher.start()

        #if !os(macOS)
        installRouteObserver()
        installMediaResetObserver(micEnabled: micEnabled)
        installInterruptionObserver(micEnabled: micEnabled)
        #endif
    }

    /// Is `posted` one of the engines this session currently owns? A retired engine posts one last
    /// configuration change as it is torn down, and another AVAudioEngine in the process is none of
    /// our business — identity only, the object is never resurrected.
    private func ownsEngine(_ posted: AnyObject?) -> Bool {
        stateLock.lock()
        defer { stateLock.unlock() }
        return posted === playbackEngine || posted === captureEngine || posted === combinedEngine
    }

    /// The hardware moved (main queue, from `AudioDeviceWatcher`). Both reasons ask the same
    /// question — is playback still where it should be — but they answer it differently: an engine
    /// that told us it stopped is definitive, while the default device moving might not concern us
    /// at all.
    private func hardwareMoved(_ reason: AudioDeviceWatcher.Reason, posted: AnyObject?) {
        guard !flag.isStopped else { return }
        switch reason {
        case .engineConfiguration:
            // The engine stops itself BEFORE posting this — so an engine that is RUNNING when the
            // notification lands on the main queue is one a rebuild already replaced or restarted:
            // the notification is the rebuild's own echo, and answering it is how the recovery
            // loops. A change that stops the engine again after this posts again, and the HAL
            // backstop checks placement independently, so ignoring a live engine's echo can never
            // strand a stopped one.
            if let engine = posted as? AVAudioEngine, engine.isRunning { return }
            scheduleEngineRebuild(reason: reason.rawValue)
        case .defaultOutputDevice:
            #if os(macOS)
            defaultOutputChanged()
            #else
            break // the watcher only raises this one on macOS
            #endif
        }
    }

    /// Restart the engines if — and only if — playback is down. The conservative trigger: it is
    /// what a route change (iOS/tvOS) and the macOS backstop get to do, since a HEALTHY engine
    /// that followed the change on its own must not be interrupted for it. Safe from any thread.
    ///
    /// The liveness check runs on `engineQueue`, BEHIND any start or rebuild in flight. A
    /// voice-processing start takes about a second and posts route changes of its own, so a check
    /// made on the main queue found the engines mid-swap, read that as "stopped", and scheduled
    /// the rebuild that would trigger the next one — the iPad mic-on loop, one rebuild per
    /// backoff step for the whole session.
    ///
    /// Gated on a start having been ATTEMPTED rather than on an engine existing, which is the
    /// difference between recovering a session whose very first `startPlayback` failed — no
    /// output device at the moment it connected — and leaving it silent for good.
    private func reviveStoppedEngines(_ reason: String) {
        engineQueue.async { [weak self] in
            guard let self, !self.flag.isStopped else { return }
            self.stateLock.lock()
            let attempted = self.enginesAttempted
            self.stateLock.unlock()
            guard attempted, !self.playbackIsLive else { return }
            DispatchQueue.main.async {
                self.scheduleEngineRebuild(reason: "playback is stopped and \(reason)")
            }
        }
    }

    /// Is the render side actually running? Both engines can carry it (`combinedEngine` when the
    /// voice processor is engaged, `playbackEngine` otherwise). Taken out from under `stateLock`
    /// before asking AVAudioEngine anything — the lock guards our handles, not the framework.
    private var playbackIsLive: Bool {
        stateLock.lock()
        let playback = playbackEngine
        let combined = combinedEngine
        stateLock.unlock()
        return (playback?.isRunning ?? false) || (combined?.isRunning ?? false)
    }

    /// Coalesce: one device switch produces a burst — the old device leaving, the default moving,
    /// the new device settling, and each engine we own posting its own change — and one rebuild
    /// serves all of it. The floor between rebuilds keeps a device that renegotiates in a loop
    /// from spinning the session. Main thread.
    private func scheduleEngineRebuild(reason: String) {
        guard !rebuildQueued else { return }
        rebuildQueued = true
        let delay = rebuildBackoff.delay(now: ProcessInfo.processInfo.systemUptime)
        if rebuildBackoff.chain >= 2 {
            // Each rebuild is retriggering the next — a feedback shape the echo guard and the
            // topology gate did not identify. Keep answering (a real recovery must not be
            // abandoned), but say what is happening: this line repeating IS the diagnosis.
            log.warning("""
                audio engine rebuilds are chaining (\(self.rebuildBackoff.chain) in a row — \
                \(reason)); backing off \(Int(delay * 1000)) ms
                """)
        } else {
            log.info("\(reason) — restarting the audio engines in \(Int(delay * 1000)) ms")
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
            self?.rebuildFire(attempt: 0)
        }
    }

    /// The scheduled rebuild came due (main queue): close out the bookkeeping and hand the
    /// actual engine work to `engineQueue` — the teardown + start can block on the audio server
    /// for seconds, and the main thread is where macOS captures and sends the stream's input.
    /// A trigger arriving while the work is in flight schedules a fresh rebuild rather than
    /// being swallowed; `engineQueue` is serial, so the two never interleave.
    private func rebuildFire(attempt: Int) {
        rebuildQueued = false
        guard !flag.isStopped else { return }
        stateLock.lock()
        let config = startConfig
        stateLock.unlock()
        guard let config else { return }
        rebuildBackoff.noteRebuild(at: ProcessInfo.processInfo.systemUptime)
        engineQueue.async { [weak self] in
            self?.performRebuild(config: config, attempt: attempt)
        }
    }

    /// Put back the topology this session was started with, on whatever hardware is there now.
    /// Runs on `engineQueue`.
    ///
    /// A full rebuild rather than a `start()` on the stopped engine, because the mic side has to
    /// follow too: `installMicTap` reads the input's live format, and the voice processor
    /// renegotiates its own. The RING is deliberately not touched — it is the one thing carried
    /// across (`makePlaybackChain` reuses it, `startDrain` is idempotent), so the drain thread
    /// keeps decoding right through the switch and its overflow policy has already dropped
    /// everything that went stale while the engine was down.
    private func performRebuild(config: StartConfig, attempt: Int) {
        guard !flag.isStopped else { return }
        tearDownEngines()
        startEngines(
            speakerUID: config.speakerUID, micUID: config.micUID, micChannel: config.micChannel,
            micEnabled: config.micEnabled, echoCancel: config.echoCancel)

        // Did playback actually come back? A device caught mid-transition can refuse to start, and
        // a rebuild that fails leaves no engine to post the next notification — so this is the one
        // path that must not just give up. (`startEngines` has logged the reason already.)
        if playbackIsLive {
            log.info("audio engines restarted on the current device")
            return
        }
        DispatchQueue.main.async { [weak self] in
            self?.rebuildFailed(attempt: attempt)
        }
    }

    /// A rebuild's playback did not come back (main queue) — walk the retry ladder. Retries
    /// when a rebuild's `start()` loses the race with a device that is still going away
    /// (0.3 s, 0.6 s, 1.2 s): a failed rebuild leaves no engine to post the next notification,
    /// so this ladder — and, on macOS, the HAL listener — is all that stands between a mistimed
    /// switch and a silent session.
    private func rebuildFailed(attempt: Int) {
        guard !flag.isStopped else { return }
        guard attempt < Self.rebuildAttempts else {
            #if os(macOS)
            log.error("""
                audio did not come back after the device change — the default-output watcher will \
                try again when a device appears
                """)
            #else
            log.error("audio did not come back after the route change")
            #endif
            return
        }
        guard !rebuildQueued else { return } // a fresh trigger already queued a full rebuild
        rebuildQueued = true // holds off a trigger that would only race this ladder
        let delay = RebuildBackoff.debounce * Double(1 << (attempt + 1))
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak self] in
            self?.rebuildFire(attempt: attempt + 1)
        }
    }

    #if os(macOS)
    /// The system's output device moved. Rebuild only when it actually concerns this session: the
    /// engine is gone or stopped, or it is playing to a device that is no longer the one we should
    /// be on. Somebody changing the default while we are pinned to a named speaker is none of our
    /// business, and rebuilding for it would cost an audible gap for nothing. Main queue (the
    /// listener block is registered against it).
    private func defaultOutputChanged() {
        guard !flag.isStopped, let config = startConfig else { return }
        stateLock.lock()
        let engine = combinedEngine ?? playbackEngine
        stateLock.unlock()
        guard let engine, engine.isRunning, let unit = engine.outputNode.audioUnit,
              let playingOn = Self.currentDevice(of: unit)
        else {
            // Nothing is playing. If an engine was expected at all, this is the backstop firing.
            reviveStoppedEngines("the default output device moved")
            return
        }
        // Empty UID = follow the system default; a pinned UID only moves if that device itself
        // came or went, which `deviceID(forUID:)` reports by resolving to a different ID or none.
        let shouldBeOn = config.speakerUID.isEmpty
            ? AudioDevices.defaultOutputDevice()
            : AudioDevices.deviceID(forUID: config.speakerUID)
        guard let shouldBeOn, shouldBeOn != playingOn else { return }
        scheduleEngineRebuild(reason: "the output device changed under the session")
    }
    #endif

    #if !os(macOS)
    /// The audio server can die and restart. It takes the session's configuration and every engine
    /// with it, and the documented recovery is to build all of it again — the same rebuild a route
    /// change uses, with the session activation back in front of it.
    private func installMediaResetObserver(micEnabled: Bool) {
        let observer = NotificationCenter.default.addObserver(
            forName: AVAudioSession.mediaServicesWereResetNotification, object: nil, queue: nil
        ) { [weak self] _ in
            SessionAudio.sessionQueue.async {
                guard let self, !self.flag.isStopped else { return }
                self.activateAudioSession(micEnabled: micEnabled)
                DispatchQueue.main.async {
                    self.scheduleEngineRebuild(reason: "the audio services were reset")
                }
            }
        }
        stateLock.lock()
        let stale = mediaResetObserver
        mediaResetObserver = observer
        stateLock.unlock()
        if let stale { NotificationCenter.default.removeObserver(stale) }
    }

    /// Interruptions still happen to a mixable session — a phone call, Siri, an app that claims
    /// a NON-mixable session of its own. iOS stops the engines, and when the interruption ends it
    /// restarts NOTHING by itself; before this observer the stream just stayed silent (under the
    /// old exclusive category, Music itself was such an interrupter, which is how "resume Music,
    /// lose the stream" was ever possible). Reactivate and revive on `.ended` — unconditionally,
    /// not only when iOS hints `.shouldResume`: a live stream is the one case where the user's
    /// intent to keep hearing it is not in doubt, and `reviveStoppedEngines` already declines
    /// when playback never went down.
    private func installInterruptionObserver(micEnabled: Bool) {
        let observer = NotificationCenter.default.addObserver(
            forName: AVAudioSession.interruptionNotification,
            object: AVAudioSession.sharedInstance(), queue: nil
        ) { [weak self] note in
            guard let raw = note.userInfo?[AVAudioSessionInterruptionTypeKey] as? UInt,
                  AVAudioSession.InterruptionType(rawValue: raw) == .ended else { return }
            SessionAudio.sessionQueue.async {
                guard let self, !self.flag.isStopped else { return }
                // The full activation, not a bare `setActive`: an interruption can drop the
                // category configuration too, and on iOS the earpiece steer is per-route.
                self.activateAudioSession(micEnabled: micEnabled)
                self.reviveStoppedEngines("an audio interruption ended")
            }
        }
        stateLock.lock()
        let stale = interruptionObserver
        interruptionObserver = observer
        stateLock.unlock()
        if let stale { NotificationCenter.default.removeObserver(stale) }
    }
    #endif

    /// Silence the mic uplink (no room audio leaves the device) or restore it. THE one muting
    /// mechanism: the owner composes its reasons — the user's in-stream mute and the background
    /// keep-alive's privacy mute — into one effective state and passes that here, so neither can
    /// clear the other (see `SessionModel.applyMicMute`).
    ///
    /// Two-engine sessions pause/resume the capture engine; a combined session instead mutes the
    /// voice processor's input (playback shares that engine and must keep running, so the engine
    /// itself never pauses — the mute zeroes the mic at the IO unit, and the tap encodes silence).
    /// Local and instant either way: nothing is negotiated with the host, and the packets that do
    /// leave carry silence. A no-op when there's no uplink (playback-only / tvOS / mic disabled),
    /// except that the state is LATCHED for an uplink that starts later. The audio SESSION stays
    /// active for background playback, so iOS may keep showing the recording indicator until a
    /// full reconfigure — either path stops room audio leaving the device, which is the
    /// privacy-relevant part. Main thread.
    public func setMicMuted(_ muted: Bool) {
        stateLock.lock()
        micMuted = muted
        let capture = captureEngine
        let combined = combinedEngine
        stateLock.unlock()
        apply(micMuted: muted, capture: capture, combined: combined)
    }

    /// Push the latched mute onto whichever engine carries the uplink. Split out from
    /// `setMicMuted` because the start paths call it too, with the engine they just started —
    /// that's how a mute requested before the permission grant lands on the engine the grant
    /// creates. Never resumes a stopped session's engine.
    private func apply(micMuted muted: Bool, capture: AVAudioEngine?, combined: AVAudioEngine?) {
        if let combined {
            combined.inputNode.isVoiceProcessingInputMuted = muted
            return
        }
        guard let capture else { return }
        if muted {
            capture.pause()
        } else if !flag.isStopped {
            try? capture.start()
        }
    }

    // MARK: - Stats

    /// The playback plane's two latency numbers, for the stats overlay.
    ///
    /// Both, never just the depth: a deep ring on a jittery link is CORRECT behaviour — the
    /// adaptive floor put it there because the link kept starving — and only the offset separates
    /// that from a ring that is simply holding audio late. Before this pair existed the plane
    /// published nothing any surface could render (depth and target lived in a periodic log line),
    /// and a field investigation into "the audio delay seems way too high" ran all the way to its
    /// conclusion without either number.
    public struct Stats: Sendable {
        /// Decoded audio queued ahead of the speaker (ms).
        public let bufferMS: Int
        /// The A/V sync loop's smoothed offset (ms): positive = audio playing BEHIND the picture.
        /// `0` before the loop has evidence, with sync unwired, or genuinely aligned.
        public let avOffsetMS: Int
    }

    /// A snapshot of `Stats`, or nil before playback starts. Safe from any thread (the handle is
    /// taken under `stateLock`; the ring's own numbers are taken under its lock, so they
    /// describe one instant).
    public var stats: Stats? {
        stateLock.lock()
        let ring = self.ring
        stateLock.unlock()
        guard let s = ring?.stats else { return nil }
        return Stats(bufferMS: s.bufferedMS, avOffsetMS: s.avOffsetMS)
    }

    #if os(macOS)
    /// Whether playback is rendering, and the device it is rendering to. The device-change
    /// recovery has exactly one observable signature from outside — "running again, on the device
    /// the system just moved to" — and nothing else here could tell the two halves apart: a
    /// stopped engine can still name the old device, and a retargeted one can still be stopped.
    /// Used by `AudioDeviceSwitchTests`.
    var playbackState: (running: Bool, device: AudioDeviceID?) {
        stateLock.lock()
        let engine = combinedEngine ?? playbackEngine
        stateLock.unlock()
        guard let engine else { return (false, nil) }
        return (engine.isRunning, engine.outputNode.audioUnit.flatMap(Self.currentDevice(of:)))
    }
    #endif

    // MARK: - Playback (host → speaker)

    /// The playback jitter ring + the source node draining it — shared by the plain playback
    /// engine and the combined voice-processing engine, and REUSED across an engine rebuild
    /// (same session, same ring: the drain thread keeps writing right through the swap). nil
    /// when the host's channel layout can't be expressed (already logged). Runs on `engineQueue`.
    private func makePlaybackChain()
        -> (ring: AudioRing, source: AVAudioSourceNode, format: AVAudioFormat)?
    {
        // Build the playback layout from the host-RESOLVED channel count (never the request):
        // 2 = stereo / 6 = 5.1 / 8 = 7.1, canonical wire order FL FR FC LFE RL RR SL SR. Same rule
        // for the rate — `resolvedAudioRateHz`, never the 96 kHz this client may have asked for.
        let channels = Int(connection.resolvedAudioChannels)
        let rateHz = wireRateHz
        // One SECOND of interleaved capacity at the session's format. The de-jitter depth itself is
        // the ring's own business now (`AudioRing.targetMS`, mirroring `JitterTuning::COREAUDIO`)
        // rather than a prefill passed in here.
        stateLock.lock()
        let ring = self.ring ?? AudioRing(seconds: 1, channels: channels, rateHz: rateHz)
        self.ring = ring
        stateLock.unlock()
        // The session's REAL frame, which the ring cannot know at construction and must not assume:
        // the shed drops exactly one frame and the target floor is a device quantum plus one, so a
        // ring left on the 5 ms default sheds two and a half frames at a time on a 96 kHz session
        // and fades across a whole one. Idempotent, so the rebuild path that reuses this very ring
        // simply sets it again.
        ring.setFrameUs(wireFrameUs)
        // The device behind this engine may be a different one, or the same one with a different
        // buffer grant. The largest callback the PREVIOUS engine saw is not a floor for this one.
        ring.forgetRenderQuantum()

        // Engine-native deinterleaved float; the render block deinterleaves the ring's wire order,
        // surround with an explicit channel layout (`wireChannelLayout` is failable — hence
        // if/else, not map). The rate describes the SAMPLES, not the device: declare the device's
        // rate and a 96 kHz stream plays at half speed. The mixer converts to whatever the output
        // runs at — resample and downmix alike; `noteOutputFormat` says when that happened.
        let rate = Double(rateHz)
        var format: AVAudioFormat?
        if channels == 2 {
            format = AVAudioFormat(standardFormatWithSampleRate: rate, channels: 2)
        } else if let layout = wireChannelLayout(channels: channels) {
            format = AVAudioFormat(standardFormatWithSampleRate: rate, channelLayout: layout)
        }
        guard let format else {
            log.error(
                "no \(channels)-channel \(rateHz) Hz audio format — audio disabled")
            return nil
        }
        let scratch = ScratchBuffer() // block-owned; freed with the closure
        let source = AVAudioSourceNode(format: format) { _, _, frameCount, abl -> OSStatus in
            let frames = Int(frameCount)
            guard frames <= 8192 else { return kAudioUnitErr_TooManyFramesToProcess }
            ring.read(into: scratch.ptr, count: frames * channels)
            let buffers = UnsafeMutableAudioBufferListPointer(abl)
            // Deinterleave the wire-order interleaved ring into the engine's per-channel buses.
            if buffers.count >= channels {
                for ch in 0..<channels {
                    if let dst = buffers[ch].mData?.assumingMemoryBound(to: Float.self) {
                        for f in 0..<frames { dst[f] = scratch.ptr[f * channels + ch] }
                    }
                }
            }
            return noErr
        }
        return (ring, source, format)
    }

    /// Connect the main mixer to the output at the output's own width. The connection the engine
    /// makes on first use of `mainMixerNode` is stereo whatever the route carries, so a 5.1 or 7.1
    /// source folds to 2 channels inside the engine and HDMI gets PCM 2.0 in 8 slots. A stereo
    /// route keeps the default connection.
    private func connectMixerAtOutputWidth(_ engine: AVAudioEngine) {
        let hw = engine.outputNode.outputFormat(forBus: 0)
        guard hw.channelCount > 2, hw.sampleRate > 0,
              let layout = hw.channelLayout ?? wireChannelLayout(channels: Int(hw.channelCount)),
              layout.channelCount == hw.channelCount
        else { return }
        let format = AVAudioFormat(standardFormatWithSampleRate: hw.sampleRate, channelLayout: layout)
        engine.connect(engine.mainMixerNode, to: engine.outputNode, format: format)
    }

    /// Say — out loud, in the log — what rate and channel count this engine is REALLY rendering
    /// at, versus what the session negotiated. Call it after `prepare()`, when the output node
    /// has settled on the device's format; on iOS/tvOS that follows the AVAudioSession, on macOS
    /// the HAL device.
    ///
    /// §9's "never claim a rate you did not get", plus its channel twin: a session that never
    /// asks AVAudioSession for more than 2 renders a negotiated 5.1 as PCM 2.0 over HDMI — the
    /// handshake says 6ch, the AVR says stereo, and nothing anywhere disagrees.
    /// `setPreferredSampleRate` is advisory, the mixer resamples and downmixes silently between
    /// any two formats, and the stream keeps playing either way. Nothing here re-formats the
    /// graph — the mixer's conversion is the correct fallback; somebody just has to say it
    /// happened.
    private func noteOutputFormat(_ engine: AVAudioEngine, wireRateHz: Int) {
        // What the device adds behind the ring; A/V sync counts it as audio already queued.
        #if os(macOS)
        let latency = engine.outputNode.presentationLatency
        #else
        let latency = AVAudioSession.sharedInstance().outputLatency
        #endif
        stateLock.lock()
        let ring = self.ring
        stateLock.unlock()
        ring?.noteOutputLatency(ns: Int64(max(0, latency) * 1_000_000_000))
        let outFormat = engine.outputNode.outputFormat(forBus: 0)
        let deviceRate = Int(outFormat.sampleRate)
        // 0 = the node has no device yet (a start that is about to fail) — nothing to compare.
        guard deviceRate > 0 else { return }
        let outChannels = Int(outFormat.channelCount)
        let wireChannels = Int(connection.resolvedAudioChannels)
        let mixerChannels = Int(engine.mainMixerNode.outputFormat(forBus: 0).channelCount)
        if mixerChannels < wireChannels, outChannels >= wireChannels {
            log.warning("""
                the engine mixes to \(mixerChannels)ch before a \(outChannels)ch output — the \
                \(wireChannels)ch session folds inside the engine
                """)
        }
        if deviceRate == wireRateHz, outChannels == wireChannels {
            log.info("""
                audio output opened at \(wireRateHz) Hz \(outChannels)ch — the negotiated format
                """)
            return
        }
        if deviceRate != wireRateHz {
            log.warning("""
                audio output is \(deviceRate) Hz but the session negotiated \(wireRateHz) Hz — \
                the engine is resampling. Playback is correct; this session is NOT \
                \(wireRateHz) Hz at the speaker, whatever the host resolved
                """)
        }
        if outChannels < wireChannels {
            log.warning("""
                audio output is \(outChannels)ch but the session negotiated \(wireChannels)ch — \
                the stream is folded to the route's width before it leaves the box. On an HDMI \
                route that is PCM 2.0 to the AVR, whatever the host sent
                """)
        } else if outChannels > wireChannels {
            log.info("""
                audio output is \(outChannels)ch on a \(wireChannels)ch session — the tail \
                channels carry silence
                """)
        }
    }

    private func startPlayback(speakerUID: String) {
        guard let (ring, source, format) = makePlaybackChain() else { return }
        let engine = AVAudioEngine()
        #if os(macOS)
        if !speakerUID.isEmpty {
            if let dev = AudioDevices.deviceID(forUID: speakerUID),
               let unit = engine.outputNode.audioUnit {
                if !Self.setDevice(dev, on: unit) {
                    log.error("speaker \(speakerUID) not selectable — using default")
                }
            } else {
                log.warning("speaker \(speakerUID) not present — using default")
            }
        }
        #endif
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
        connectMixerAtOutputWidth(engine)
        engine.prepare()
        do {
            try engine.start()
        } catch {
            log.error("playback engine failed to start: \(error.localizedDescription)")
            return
        }
        noteOutputFormat(engine, wireRateHz: wireRateHz)
        stateLock.lock()
        if flag.isStopped {
            stateLock.unlock()
            engine.stop() // stop() already ran — don't strand a started engine
            return
        }
        playbackEngine = engine
        stateLock.unlock()
        startDrain(into: ring)
    }

    /// Idempotent — the permission-grant engine swap reaches here a second time with the
    /// drain thread already feeding the (carried-over) ring.
    private func startDrain(into ring: AudioRing) {
        stateLock.lock()
        if drainStarted {
            stateLock.unlock()
            return
        }
        drainStarted = true
        stateLock.unlock()
        // A/V sync. This thread is the only place that holds all three ingredients at once: the
        // packet's host capture `ptsNs`, the ring depth, and the video plane's end-to-end figure.
        // `ptsNs` was decoded into `AudioPCM` and then dropped on the floor right here for the
        // plane's entire existence, which is why audio ran at whatever depth its jitter ring
        // happened to settle at and nothing ever placed it against the picture.
        //
        // The escape hatch mirrors the Rust clients': a field regression in a loop that steers
        // PLAYBACK should be bisectable without a rebuild. macOS honours it from the environment;
        // elsewhere it simply never trips, which is the same as today's behaviour.
        let syncEnabled = !["1", "true"].contains(
            ProcessInfo.processInfo.environment["PUNKTFUNK_NO_AV_SYNC"] ?? "")
        // nil disarms the loop entirely — no reference, no correction (see `videoLatency`).
        let videoLatency = syncEnabled ? self.videoLatency : nil
        if !syncEnabled { log.info("A/V sync disabled by PUNKTFUNK_NO_AV_SYNC") }
        let channels = Int(connection.resolvedAudioChannels)
        let rateHz = wireRateHz
        // Read on the caller's thread, not inside the closure: `self` is deliberately not captured
        // by the drain thread (it holds the connection strongly and self not at all — see
        // `deinit`), so anything derived from the connection has to be resolved out here.
        let frameUs = wireFrameUs
        let frameMS = wireFrameMS
        AudioDrain.start(
            connection: connection, flag: flag, done: drainDone, ring: ring,
            videoLatency: videoLatency, channels: channels, rateHz: rateHz,
            frameUs: frameUs, frameMS: frameMS)
    }

    // MARK: - Mic (mic → host)

    #if !os(tvOS)
    /// The combined topology failed to come up. On macOS, latch the input device it failed on so
    /// a rebuild goes straight to the split topology instead of re-running the failure — the
    /// failed attempt is what churns the HAL and retriggers the recovery (see
    /// `CombinedTopologyGate`). On iOS routes are session-managed and a VPIO failure is the
    /// transient route-transition kind, so nothing is latched there.
    private func noteCombinedFailure() {
        #if os(macOS)
        combinedGate.noteFailure(input: AudioDevices.defaultInputDevice())
        #endif
    }

    /// One engine, both directions: engage the system voice processor on the shared IO unit
    /// (AEC + noise suppression + AGC), hang the playback source off its render side and the
    /// mic tap off its capture side. Every failure falls back to a WORKING configuration —
    /// the split path (no AEC) when the voice processor won't engage, plain playback when the
    /// mic chain can't be built — a session never loses audio to the echo-cancel feature.
    private func startCombined(speakerUID: String, micUID: String, micChannel: Int) {
        let engine = AVAudioEngine()
        let input = engine.inputNode
        do {
            // Before anything reads the input's format: the voice processor changes it (often
            // to its own mono mix, sometimes at a lower rate) — installMicTap reads the format
            // AFTER this, so the converter chain adapts to whatever the processor emits.
            try input.setVoiceProcessingEnabled(true)
        } catch {
            log.warning("""
                voice processing unavailable (\(error.localizedDescription)) — separate \
                engines, no echo cancellation
                """)
            noteCombinedFailure()
            startPlayback(speakerUID: speakerUID)
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        // Symmetric enable for the render side; with both directions on one engine the
        // input-node enable already covers it, so a refusal here is not a failure.
        try? engine.outputNode.setVoiceProcessingEnabled(true)
        // This is a game stream, not a call: never duck the host's audio under the outgoing
        // voice. .min is the closest to "off" the API offers, and advanced (selective)
        // ducking stays off with it.
        input.voiceProcessingOtherAudioDuckingConfiguration = .init(
            enableAdvancedDucking: false, duckingLevel: .min)

        guard let (ring, source, format) = makePlaybackChain() else {
            // Playback impossible (logged) — keep the uplink alive, as the split path would.
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        engine.attach(source)
        engine.connect(source, to: engine.mainMixerNode, format: format)
        connectMixerAtOutputWidth(engine)

        // The capture side must be PULLED, and only the render graph pulls anything. An input
        // node carrying nothing but a tap is not part of that graph, so on the combined engine
        // nobody drove it: the IO unit came up (the recording indicator lit for a beat, then went
        // out as the input went idle) and NOT ONE BUFFER ever reached the tap — no error, no
        // failed start, just a session that quietly sent no microphone at all. Routing the input
        // through a silent sink puts it in the graph, which is what Apple's own voice-processing
        // sample does. The split path never needed it: a capture-only engine has the input node
        // AS its graph, so it is pulled by definition — which is why this only broke when the
        // combined topology became the default.
        //
        // `outputVolume = 0` on the sink: the mic has to reach the graph, never the speaker. At
        // any audible volume this is a microphone wired straight to the earpiece.
        let micSink = AVAudioMixerNode()
        engine.attach(micSink)
        micSink.outputVolume = 0
        engine.connect(engine.inputNode, to: micSink, format: nil)
        engine.connect(micSink, to: engine.mainMixerNode, format: nil)

        // BEFORE the tap reads a format. Enabling voice processing swaps the engine's IO unit
        // for the VPIO one and renegotiates its formats, and until the engine is prepared the
        // input node can still report the pre-swap state — 0 Hz / 0 channels included, which
        // `installMicTap` (correctly) refuses as "no usable input device". Preparing first means
        // the chain is built against what the voice processor will actually emit.
        engine.prepare()
        guard installMicTap(on: engine.inputNode, micUID: micUID, micChannel: micChannel) else {
            // Mic chain unavailable on the voice-processed engine (logged). The mic outranks
            // echo cancellation: fall back to the split path — own engine, no voice processor —
            // rather than dropping the uplink for the session.
            engine.stop()
            noteCombinedFailure()
            startPlayback(speakerUID: speakerUID)
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        do {
            try engine.start()
        } catch {
            log.error("combined engine failed to start: \(error.localizedDescription)")
            engine.inputNode.removeTap(onBus: 0)
            engine.stop()
            noteCombinedFailure()
            // Same rule: a working mic without echo cancellation beats no mic at all.
            startPlayback(speakerUID: speakerUID)
            startCapture(micUID: micUID, micChannel: micChannel)
            return
        }
        stateLock.lock()
        if flag.isStopped {
            stateLock.unlock()
            input.removeTap(onBus: 0)
            engine.stop() // stop() already ran — don't strand a started engine (or a hot mic)
            return
        }
        combinedEngine = engine
        let muted = micMuted // latched before this engine existed (a mute during the prompt)
        stateLock.unlock()
        apply(micMuted: muted, capture: nil, combined: engine)
        // Worth its own read on this path rather than only the plain one: the voice processor picks
        // its OWN formats when it engages (that is why the mic tap reads them after `prepare()`),
        // and a VPIO unit is the least likely thing in the graph to have honoured a 96 kHz request.
        noteOutputFormat(engine, wireRateHz: wireRateHz)
        startDrain(into: ring)
        log.info("audio engines joined — voice processing (echo cancellation) active")
    }

    /// The split path: capture on its OWN engine, playback on another — the pre-echo-cancel
    /// topology, kept verbatim. Two engines, not one — a single AVAudioEngine ties
    /// input+output to one aggregate clock, separate engines keep arbitrary mic/speaker
    /// combinations trivial. That freedom is exactly why the voice processor can't ride this
    /// path (AEC needs both directions on one unit) and why explicitly pinned endpoints land
    /// here — see `wantsCombined`.
    private func startCapture(micUID: String, micChannel: Int) {
        let engine = AVAudioEngine()
        let input = engine.inputNode
        #if os(macOS)
        if !micUID.isEmpty {
            if let dev = AudioDevices.deviceID(forUID: micUID), let unit = input.audioUnit {
                if !Self.setDevice(dev, on: unit) {
                    log.error("microphone \(micUID) not selectable — using default")
                }
            } else {
                log.warning("microphone \(micUID) not present — using default")
            }
        }
        #endif
        // Prepared before the tap reads a format, for the same reason the combined path does it:
        // a node that hasn't been through `prepare()` can still report the pre-negotiation
        // format (0 Hz / 0 channels on a device that is perfectly fine), which reads downstream
        // as "no microphone".
        engine.prepare()
        guard installMicTap(on: engine.inputNode, micUID: micUID, micChannel: micChannel) else {
            log.error("mic uplink unavailable — this session sends no microphone audio")
            engine.stop()
            return
        }
        do {
            try engine.start()
        } catch {
            log.error("capture engine failed to start: \(error.localizedDescription)")
            input.removeTap(onBus: 0)
            return
        }
        stateLock.lock()
        if flag.isStopped {
            // stop() ran while we were starting (the permission prompt resolves at the
            // user's leisure) — tear the engine down ourselves, nobody else owns it now.
            stateLock.unlock()
            input.removeTap(onBus: 0)
            engine.stop()
            return
        }
        captureEngine = engine
        let muted = micMuted // latched before this engine existed (a mute during the prompt)
        stateLock.unlock()
        apply(micMuted: muted, capture: engine, combined: nil)
        log.info("mic uplink started (\(micUID.isEmpty ? "default input" : micUID))")
    }

    /// Resolve the input's live format + fold plan, build the mono→Opus chain, and install the
    /// capture tap on `input` — everything mic except engine ownership, shared verbatim by the
    /// combined and split topologies. Reads `input.outputFormat(forBus:)` at call time, so the
    /// chain follows whatever the node emits: the raw device format, or the voice processor's
    /// own mix when that's enabled. False (logged) when no input is usable or the encoder
    /// can't be built; the tap is installed on true.
    private func installMicTap(
        on input: AVAudioInputNode, micUID: String, micChannel: Int
    ) -> Bool {
        let inFormat = input.outputFormat(forBus: 0)
        // The tap carries the node's OUTPUT format, but AVFAudio validates that against the input
        // HARDWARE format and raises an uncatchable Objective-C exception when the two don't line
        // up. With no default input device the hardware side reads 0 Hz / 0 ch while the output
        // side still reports the default-device aggregate — 2 ch of AirPlay, seemingly recordable.
        let hwFormat = input.inputFormat(forBus: 0)
        guard inFormat.sampleRate > 0, inFormat.channelCount > 0,
              hwFormat.sampleRate > 0, hwFormat.channelCount > 0 else {
            log.error("no usable input device — mic uplink disabled")
            return false
        }

        // Multi-channel-interface handling. A pro interface exposes N discrete inputs with the mic
        // on ONE of them, but AVAudioConverter's N→stereo downmix takes channels 0/1 — dead
        // silence when the mic sits higher up (the classic "host receives zeros"). So we fold the
        // input to a single mono bus OURSELVES and resample that. micChannel: 0 = Auto (sum every
        // channel — a lone hot mic passes at full level), n≥1 pins 1-based input channel n.
        let inChannels = Int(inFormat.channelCount)
        let pinnedChannel: Int? = {
            guard micChannel >= 1 else { return nil }
            let idx = micChannel - 1
            guard idx < inChannels else {
                log.warning(
                    "mic channel \(micChannel) out of range (device has \(inChannels)) — mixing all")
                return nil
            }
            return idx
        }()
        let channelPlan = pinnedChannel.map { "channel \($0 + 1)/\(inChannels)" }
            ?? (inChannels > 1 ? "mix \(inChannels)ch→mono" : "mono")

        // Name the device we're ACTUALLY recording from + its format + how we fold it, once per
        // session. This single line localizes the whole class of "host receives silence" failures
        // that otherwise need a host-side tone injection to pin down: a UID that silently fell back
        // to the default, the wrong device being live, or the wrong channel picked.
        #if os(macOS)
        if let unit = input.audioUnit, let live = Self.currentDevice(of: unit),
           let dev = AudioDevices.describe(live) {
            if !micUID.isEmpty, dev.uid != micUID {
                log.warning("""
                    mic selection not honored — requested \(micUID) but capturing from \
                    \(dev.name) [\(dev.uid)]; the device's UID likely changed (replug) — \
                    reselect it in Settings
                    """)
            }
            log.info("""
                mic capture: \(dev.name) [\(dev.uid)] — \(Int(inFormat.sampleRate)) Hz, \
                \(inChannels) ch, \(channelPlan)
                """)
        } else {
            log.info("""
                mic capture: <device unavailable> — \(Int(inFormat.sampleRate)) Hz, \
                \(inChannels) ch, \(channelPlan)
                """)
        }
        #else
        log.info(
            "mic capture: \(Int(inFormat.sampleRate)) Hz, \(inChannels) ch, \(channelPlan)")
        #endif

        // Encode a single mono bus (folded from the tap's own buffer format): the resampler goes
        // mono@inputSR → the encoder's 48 kHz mono, so it handles the rate change and the
        // wrong-channel downmix never happens. Mono end to end — the host's decoder upmixes,
        // so the old duplicate-into-stereo step only cost bits and cycles.
        //
        // `chain` carries the rate-dependent pieces INCLUDING the per-callback scratch buffers,
        // preallocated HERE for the rate the input currently reports — the steady-state tap path
        // allocates nothing. The tap rebuilds it if the device's real rate or quantum differ,
        // which is the price of installing the tap with the bus's own format (see below).
        let scratchFrames: AVAudioFrameCount = 8192
        guard let encoder = try? OpusEncoder(),
              var chain = Self.micChain(
                  rate: inFormat.sampleRate, frames: scratchFrames, to: encoder.pcmFormat),
              let chunk = AVAudioPCMBuffer(
                  pcmFormat: encoder.pcmFormat, frameCapacity: encoder.framesPerPacket)
        else {
            log.error("Opus encoder unavailable — mic uplink disabled")
            return false
        }

        // Tap-thread-confined state: fold into `chain.mono`, resample into `chain.staging`,
        // accumulate in `fifo`, slice `framesPerPacket` (10 ms) chunks for the encoder.
        var fifo: [Float] = []
        fifo.reserveCapacity(48_000)
        var seq: UInt32 = 0
        let connection = connection
        let flag = flag

        // Silence tripwire (tap-confined): a "recording" app can be handed pure digital zeros —
        // a zeroed input-volume slider, a stale TCC grant, a muted device, OR the wrong channel
        // picked — and everything downstream looks alive while the host gets silence. Track the
        // peak of the EXTRACTED mono bus over the first ~10 s (not the raw device — a mic present
        // on a channel we didn't grab must still read as silence) and emit exactly ONE verdict.
        // This is the log line whose absence made the last occurrence take a host-side tone.
        let silenceWindow = Int(inFormat.sampleRate * 10)
        let deviceLabel = micUID.isEmpty ? "default input" : micUID
        var framesInspected = 0
        var inputPeak: Float = 0
        var levelReported = false

        // 480 frames = 10 ms, matching the packet duration — advisory, CoreAudio delivers the
        // device quantum whatever we ask. `format: nil` (not the format read above) means
        // "whatever the bus emits": a format read a moment earlier can go stale under a device
        // switch or a rate change, and a stale one raises where nil follows the bus.
        input.installTap(onBus: 0, bufferSize: 480, format: nil) { buffer, _ in
            if flag.isStopped { return }
            let frames = Int(buffer.frameLength)
            guard frames > 0, let src = buffer.floatChannelData else { return }
            // Rebuild the rate-dependent chain when the device changes rate under a live tap
            // (resampling by the old ratio would pitch-shift the mic), and when a quantum larger
            // than the scratch arrives (`bufferSize` is advisory both ways) — regrown once to the
            // new high-water mark, so the steady state stays allocation-free.
            if buffer.format.sampleRate != chain.monoFormat.sampleRate
                || buffer.frameLength > chain.mono.frameCapacity {
                guard let rebuilt = Self.micChain(
                          rate: buffer.format.sampleRate,
                          frames: max(buffer.frameLength, scratchFrames),
                          to: encoder.pcmFormat)
                else { return }
                chain = rebuilt
            }
            let mono = chain.mono, staging = chain.staging, resampler = chain.resampler
            guard let dst = mono.floatChannelData?[0] else { return }
            mono.frameLength = buffer.frameLength

            // Fold the multi-channel input down to the one mono bus we encode.
            Self.foldToMono(
                input: src, frames: frames, channels: Int(buffer.format.channelCount),
                interleaved: buffer.format.isInterleaved, pinned: pinnedChannel, out: dst)

            if !levelReported {
                var localPeak: Float = 0
                for i in 0..<frames where abs(dst[i]) > localPeak { localPeak = abs(dst[i]) }
                if localPeak > inputPeak { inputPeak = localPeak }
                framesInspected += frames
                if framesInspected >= silenceWindow {
                    levelReported = true
                    if inputPeak == 0 {
                        log.warning("""
                            mic uplink has been pure digital SILENCE for 10 s (\(deviceLabel), \
                            \(channelPlan)) — check the input level (System Settings → Sound → \
                            Input), Privacy & Security → Microphone, and the Microphone channel in \
                            Settings; the host is receiving zeros
                            """)
                    } else {
                        let dbfs = 20 * log10(inputPeak)
                        log.info("""
                            mic uplink OK — peak \(String(format: "%.1f", dbfs)) dBFS over first \
                            10 s (\(deviceLabel), \(channelPlan))
                            """)
                    }
                }
            }

            var fed = false
            var convError: NSError?
            let status = resampler.convert(to: staging, error: &convError) { _, outStatus in
                if fed {
                    outStatus.pointee = .noDataNow
                    return nil
                }
                fed = true
                outStatus.pointee = .haveData
                return mono
            }
            guard status != .error, let p = staging.floatChannelData?[0] else { return }
            fifo.append(contentsOf: UnsafeBufferPointer(
                start: p, count: Int(staging.frameLength)))

            // Consume whole chunks through a head index, then drop the eaten prefix in ONE
            // move of the sub-chunk remainder. The old per-chunk removeFirst memmoved the
            // entire backlog for every packet — O(n) on the render-adjacent tap thread.
            let samplesPerChunk = Int(encoder.framesPerPacket)
            var head = 0
            while fifo.count - head >= samplesPerChunk {
                chunk.frameLength = encoder.framesPerPacket
                fifo.withUnsafeBufferPointer { src in
                    chunk.floatChannelData![0].update(
                        from: src.baseAddress! + head, count: samplesPerChunk)
                }
                head += samplesPerChunk
                guard let packets = try? encoder.encode(chunk) else { continue }
                for packet in packets {
                    connection.sendMic(
                        packet, seq: seq, ptsNs: DispatchTime.now().uptimeNanoseconds)
                    seq &+= 1
                }
            }
            if head > 0 { fifo.removeFirst(head) } // keeps capacity — no realloc
        }
        return true
    }

    /// The rate-dependent half of the mic chain: a mono bus at `rate`, the resampler from it onto
    /// the encoder's 48 kHz mono, and the two scratch buffers sized for `frames`. Grouped so the
    /// tap can swap all four together — they are only ever valid as a set.
    struct MicChain {
        let monoFormat: AVAudioFormat
        let resampler: AVAudioConverter
        let mono: AVAudioPCMBuffer
        let staging: AVAudioPCMBuffer
    }

    /// Build a `MicChain` for `rate`, or nil if the rate is unusable or an allocation fails.
    /// Built once up front for the format the input reports, and again from the tap whenever the
    /// device's real rate differs — a macOS input can change rate under a live tap, and a chain
    /// pinned to the old rate resamples by the wrong ratio (a pitch-shifted mic).
    /// `internal` for unit testing: it needs no engine, device or permission.
    static func micChain(
        rate: Double, frames: AVAudioFrameCount, to pcmFormat: AVAudioFormat
    ) -> MicChain? {
        // `staging` holds the resampled mono at the DESTINATION rate, so it must fit the upward
        // ratio from `rate` (a 44.1 kHz quantum grows by ~1.088 into 48 kHz); +64 covers the
        // converter's own slack. Sized from `pcmFormat`, never a literal: a destination rate the
        // buffer was not sized for truncates silently and drifts the uplink's pitch.
        guard rate > 0, frames > 0,
              let monoFormat = AVAudioFormat(
                  commonFormat: .pcmFormatFloat32, sampleRate: rate, channels: 1,
                  interleaved: false),
              let resampler = AVAudioConverter(from: monoFormat, to: pcmFormat),
              let mono = AVAudioPCMBuffer(pcmFormat: monoFormat, frameCapacity: frames),
              let staging = AVAudioPCMBuffer(
                  pcmFormat: pcmFormat,
                  frameCapacity: AVAudioFrameCount(
                      (Double(frames) * pcmFormat.sampleRate / rate).rounded(.up)) + 64)
        else { return nil }
        return MicChain(
            monoFormat: monoFormat, resampler: resampler, mono: mono, staging: staging)
    }

    /// Fold `channels` of input (`floatChannelData` layout: `interleaved` → one buffer strided by
    /// channel count; else one buffer per channel) down to a single mono bus in `out` (`frames`
    /// long). `pinned` (0-based, must be `< channels`) copies exactly that channel — the fix for a
    /// mic on one input of a multi-channel interface; `nil` sums every channel, clamped to
    /// [-1, 1], so a lone hot channel still passes at full level instead of the silent 0/1 the
    /// default N→stereo downmix would grab. Pure + `internal` for unit testing the index math.
    static func foldToMono(
        input: UnsafePointer<UnsafeMutablePointer<Float>>, frames: Int, channels: Int,
        interleaved: Bool, pinned: Int?, out: UnsafeMutablePointer<Float>
    ) {
        if let ch = pinned, ch < channels {
            if interleaved {
                let d = input[0]
                for i in 0..<frames { out[i] = d[i * channels + ch] }
            } else {
                let d = input[ch]
                for i in 0..<frames { out[i] = d[i] }
            }
        } else if interleaved {
            let d = input[0]
            for i in 0..<frames {
                var s: Float = 0
                for c in 0..<channels { s += d[i * channels + c] }
                out[i] = max(-1, min(1, s))
            }
        } else {
            let d0 = input[0]
            for i in 0..<frames { out[i] = d0[i] }
            for c in 1..<channels {
                let d = input[c]
                for i in 0..<frames { out[i] += d[i] }
            }
            if channels > 1 { for i in 0..<frames { out[i] = max(-1, min(1, out[i])) } }
        }
    }
    #endif

    #if os(macOS)
    private static func setDevice(_ id: AudioDeviceID, on unit: AudioUnit) -> Bool {
        var dev = id
        return AudioUnitSetProperty(
            unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
            &dev, UInt32(MemoryLayout<AudioDeviceID>.size)) == noErr
    }

    /// Read back the AUHAL's live device — the definitive "what are we actually capturing
    /// from", which catches a selection that succeeded on paper but silently fell back to
    /// the system default (a stale/changed UID, a device that vanished between resolve and
    /// start). 0 / an error means we couldn't tell.
    private static func currentDevice(of unit: AudioUnit) -> AudioDeviceID? {
        var dev = AudioDeviceID(0)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        let status = AudioUnitGetProperty(
            unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, &dev, &size)
        guard status == noErr, dev != 0 else { return nil }
        return dev
    }
    #endif
}
