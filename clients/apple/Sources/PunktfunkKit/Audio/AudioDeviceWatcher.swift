// "The audio output moved under us" — the one signal `SessionAudio` needs to survive a device
// change, and the one piece of it that can be tested without a stream.
//
// Split out of SessionAudio deliberately. An end-to-end test of the recovery needs a live session,
// which needs a host, and punktfunk-host does not build on macOS — so the wiring that matters most
// (is the observer actually installed? does the identity check let the notification through?) would
// otherwise ship unverified, and a silent failure in it costs the session ALL of its audio. On its
// own this can be pointed at the real hardware from a unit test: see AudioDeviceWatcherTests.
//
// What it does NOT own: anything with session semantics. The iOS route-change steer and the
// media-services-reset re-activation stay in SessionAudio, next to the AVAudioSession they act on.

import AVFoundation
import os
#if os(macOS)
import CoreAudio
#endif

private let log = ClientLog(category: "audio")

final class AudioDeviceWatcher {
    /// Why the owner is being told. Only for the log line — every reason leads to the same
    /// question, "is playback still on the device it should be on".
    enum Reason: String {
        /// An engine stopped itself because its IO hardware changed underneath it.
        case engineConfiguration = "the audio hardware configuration changed"
        /// The system's default output device moved (macOS).
        case defaultOutputDevice = "the default output device changed"
        /// An audio device came or went (macOS): a pinned device can return without becoming the
        /// default.
        case deviceList = "an audio device came or went"
    }

    /// Does this configuration change belong to an engine the session still owns? A retired engine
    /// posts one last change as it is torn down, and other AVAudioEngines in the process are not
    /// ours to restart.
    private let isOurs: (AnyObject?) -> Bool
    /// Delivered on the main queue. The second argument is the engine that posted the change
    /// (`.engineConfiguration` only; nil for the HAL listener) — the owner needs the OBJECT, not
    /// just the reason, because an engine that is RUNNING when the notification lands is one the
    /// owner already restarted: acting on that echo is how a rebuild loop starts.
    private let onChange: (Reason, AnyObject?) -> Void

    private let lock = NSLock()
    private var configObserver: NSObjectProtocol?
    #if os(macOS)
    private var defaultOutputListener: AudioObjectPropertyListenerBlock?
    #endif

    init(isOurs: @escaping (AnyObject?) -> Bool, onChange: @escaping (Reason, AnyObject?) -> Void) {
        self.isOurs = isOurs
        self.onChange = onChange
    }

    deinit { stop() }

    /// Idempotent.
    func start() {
        lock.lock()
        let already = configObserver != nil
        lock.unlock()
        guard !already else { return }

        let token = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: nil, queue: nil
        ) { [weak self] note in
            // Posted from whatever thread the IO unit noticed on. The engine is the notification's
            // object; it is only ever compared by identity, never resurrected.
            let posted = note.object as AnyObject?
            DispatchQueue.main.async {
                guard let self, self.isOurs(posted) else { return }
                self.onChange(.engineConfiguration, posted)
            }
        }
        lock.lock()
        configObserver = token
        lock.unlock()

        #if os(macOS)
        // The engine notification is delivered BY an engine: useless after a rebuild that could
        // not start (no engine left to notify anyone), and unverified for the voice-processing
        // engine. The HAL is told either way.
        let block: AudioObjectPropertyListenerBlock = { [weak self] count, addresses in
            // On the main queue — registered against it below. No engine posted this, so nil.
            let changed = UnsafeBufferPointer(start: addresses, count: Int(count))
            self?.onChange(Self.reason(changed.map(\.mSelector)), nil)
        }
        var address = Self.defaultOutputAddress()
        let status = AudioObjectAddPropertyListenerBlock(
            AudioObjectID(kAudioObjectSystemObject), &address, DispatchQueue.main, block)
        guard status == noErr else {
            log.warning("""
                no listener on the default output device (\(status)) — an output device change \
                mid-stream may need a reconnect
                """)
            return
        }
        var devices = Self.deviceListAddress()
        let listStatus = AudioObjectAddPropertyListenerBlock(
            AudioObjectID(kAudioObjectSystemObject), &devices, DispatchQueue.main, block)
        if listStatus != noErr {
            log.warning("no listener on the device list (\(listStatus)) — a replugged pinned device needs a reconnect")
        }
        lock.lock()
        defaultOutputListener = block
        lock.unlock()
        #endif
    }

    /// Idempotent, and safe from any thread. After it returns, no further `onChange` is delivered
    /// except one already in flight on the main queue — which the owner's own stopped-flag catches.
    func stop() {
        lock.lock()
        let token = configObserver
        configObserver = nil
        #if os(macOS)
        let listener = defaultOutputListener
        defaultOutputListener = nil
        #endif
        lock.unlock()
        if let token { NotificationCenter.default.removeObserver(token) }
        #if os(macOS)
        guard let listener else { return }
        var address = Self.defaultOutputAddress()
        AudioObjectRemovePropertyListenerBlock(
            AudioObjectID(kAudioObjectSystemObject), &address, DispatchQueue.main, listener)
        var devices = Self.deviceListAddress()
        AudioObjectRemovePropertyListenerBlock(
            AudioObjectID(kAudioObjectSystemObject), &devices, DispatchQueue.main, listener)
        #endif
    }

    #if os(macOS)
    /// One listener call can carry several changed properties: a device list change that also
    /// moved the default output is a default output change.
    static func reason(_ selectors: [AudioObjectPropertySelector]) -> Reason {
        selectors.contains(kAudioHardwarePropertyDevices)
            && !selectors.contains(kAudioHardwarePropertyDefaultOutputDevice)
            ? .deviceList : .defaultOutputDevice
    }

    /// Freshly built per call rather than held in a mutable static: the HAL takes the address
    /// `inout` and copies it, so there is nothing to share and a shared one would only be a
    /// mutable global.
    private static func defaultOutputAddress() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
    }

    private static func deviceListAddress() -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
    }
    #endif
}
