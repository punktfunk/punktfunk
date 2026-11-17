// The output-device switch, end to end against a real session.
//
// An AVAudioEngine does not follow the audio hardware: when the output device changes under a
// running engine it stops itself and stays stopped. A stream whose output moves mid-session —
// AirPods taken out of an ear, a headset unplugged, the default changed in System Settings —
// needs `SessionAudio` to rebuild its engines on the device the system moved to, and again
// when the device returns, which is a second stop rather than a recovery.
//
// This drives the real `SessionAudio` against the loopback host and moves the system's default
// output device out from under it, twice: out and back.
// Playback-only (mic off): it is the render side that stops, and a mic would drag the microphone
// permission and the voice processor into a test that is about neither.
//
// Driven by clients/apple/test-loopback.sh, like its LoopbackIntegrationTests siblings.

#if os(macOS)
import AVFoundation
import CoreAudio
import XCTest

@testable import PunktfunkKit

final class AudioDeviceSwitchTests: XCTestCase {
    /// Set the system default output device. Test-local on purpose: nothing in the app ever
    /// changes the user's device, it only follows it.
    private func setDefaultOutput(_ id: AudioDeviceID) -> OSStatus {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDefaultOutputDevice,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var dev = id
        return AudioObjectSetPropertyData(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil,
            UInt32(MemoryLayout<AudioDeviceID>.size), &dev)
    }

    /// Pump the MAIN runloop until playback is running on `device`, or the deadline passes. The
    /// recovery lands on the main queue (a debounced hop, then possibly a retry ladder), so a
    /// sleeping test would block the very thing it is waiting for.
    private func waitForPlayback(
        _ audio: SessionAudio, on device: AudioDeviceID, timeout: TimeInterval
    ) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            RunLoop.current.run(until: Date().addingTimeInterval(0.05))
            let state = audio.playbackState
            if state.running, state.device == device { return true }
        }
        return false
    }

    @MainActor
    func testPlaybackFollowsAnOutputDeviceChange() throws {
        guard let portStr = ProcessInfo.processInfo.environment["PUNKTFUNK_LOOPBACK_PORT"],
              let port = UInt16(portStr)
        else {
            throw XCTSkip("needs a running punktfunk1-host — use clients/apple/test-loopback.sh")
        }
        guard let original = AudioDevices.defaultOutputDevice() else {
            throw XCTSkip("no default output device")
        }
        try XCTSkipIf(
            AudioDevices.isRunningSomewhere(original),
            "something is playing on the default output — moving it would hop that audio")
        let others = AudioDevices.outputs()
            .compactMap { AudioDevices.deviceID(forUID: $0.uid) }
            .filter { $0 != original }
        guard let target = others.first else {
            throw XCTSkip("needs a second output device to switch to")
        }

        let conn = try PunktfunkConnection(
            host: "127.0.0.1", port: port, width: 1280, height: 720, refreshHz: 60,
            bitrateKbps: 50_000)
        let audio = SessionAudio(connection: conn)
        // "" speaker UID = follow the system default, the only configuration a default-device
        // change moves.
        audio.start(
            speakerUID: "", micUID: "", micChannel: 0, micEnabled: false, echoCancel: false)
        defer {
            audio.stop()
            _ = setDefaultOutput(original)
        }

        XCTAssertTrue(
            waitForPlayback(audio, on: original, timeout: 5),
            "playback never started on the current default output device")

        // Out: the device the stream was playing to goes away underneath it.
        XCTAssertEqual(setDefaultOutput(target), noErr)
        XCTAssertTrue(
            waitForPlayback(audio, on: target, timeout: 10),
            "playback did not come back after the output device changed — this is the field "
                + "report: no sound on the device the system moved to, until the stream is "
                + "restarted")

        // And back: the device returning is a second stop, which the session recovers from too.
        XCTAssertEqual(setDefaultOutput(original), noErr)
        XCTAssertTrue(
            waitForPlayback(audio, on: original, timeout: 10),
            "playback did not come back after the output device changed back")
    }
}
#endif
