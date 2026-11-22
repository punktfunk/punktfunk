// The three privacy grants the host needs, asked for and read from this process, which is the
// host: one identity, so every answer is the host's own. Screen recording's preflight answer is
// fixed for the life of the process; the other two read live.
import AppKit
import CoreAudio
import CoreGraphics

enum Grant: Equatable, Sendable {
    case allowed, denied, unknown
}

struct Grants: Equatable, Sendable {
    var screen: Bool
    var input: Bool
    var audio: Grant

    /// Screen recording and input control: without either, a stream shows nothing or can't be
    /// driven. System audio is not checked at launch; its answer is often unreadable.
    var missingRequired: Bool { !screen || !input }

    static let all = Grants(screen: true, input: true, audio: .allowed)
}

enum Permissions {
    static func read() -> Grants {
        Grants(screen: CGPreflightScreenCaptureAccess(), input: CGPreflightPostEventAccess(), audio: audio())
    }

    /// Shows macOS's prompt the first time; later it only answers.
    static func requestScreen() { _ = CGRequestScreenCaptureAccess() }
    static func requestInput() { _ = CGRequestPostEventAccess() }

    enum Pane: String {
        case screen = "Privacy_ScreenCapture"
        case input = "Privacy_Accessibility"
        case audio = "Privacy_AudioCapture"
    }

    static func open(_ pane: Pane) {
        let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(pane.rawValue)")!
        NSWorkspace.shared.open(url)
    }

    /// The system audio grant through TCCAccessPreflight, which is private (TCC.framework) and
    /// optional: `.unknown` when it does not resolve or has no answer yet.
    static func audio() -> Grant {
        typealias Preflight = @convention(c) (CFString, CFDictionary?) -> Int
        guard let tcc = dlopen("/System/Library/PrivateFrameworks/TCC.framework/Versions/A/TCC", RTLD_LAZY),
              let symbol = dlsym(tcc, "TCCAccessPreflight")
        else { return .unknown }
        switch unsafeBitCast(symbol, to: Preflight.self)("kTCCServiceAudioCapture" as CFString, nil) {
        case 0: return .allowed
        case 1: return .denied
        default: return .unknown
        }
    }

    /// macOS asks for system audio only when a process tap starts, so this starts one for a
    /// second: a private, unmuted tap feeding a private aggregate device, then tears both down.
    /// The output and its volume stay as they were.
    static func askForAudio() async {
        await withCheckedContinuation { done in
            // Off the concurrency pool: the HAL's calls block while macOS's prompt is up.
            DispatchQueue.global(qos: .userInitiated).async {
                tapOnce()
                done.resume()
            }
        }
    }

    private static func tapOnce() {
        let description = CATapDescription(stereoGlobalTapButExcludeProcesses: [])
        description.isPrivate = true
        description.muteBehavior = .unmuted
        var tap = AudioObjectID(kAudioObjectUnknown)
        guard AudioHardwareCreateProcessTap(description, &tap) == noErr else { return }
        defer { AudioHardwareDestroyProcessTap(tap) }
        let uid = "io.unom.punktfunk.host.permission-\(getpid())"
        let aggregate: [String: Any] = [
            kAudioAggregateDeviceUIDKey: uid,
            kAudioAggregateDeviceNameKey: uid,
            kAudioAggregateDeviceIsPrivateKey: true,
            kAudioAggregateDeviceTapAutoStartKey: true,
            kAudioAggregateDeviceTapListKey: [[kAudioSubTapUIDKey: description.uuid.uuidString]],
        ]
        var device = AudioObjectID(kAudioObjectUnknown)
        guard AudioHardwareCreateAggregateDevice(aggregate as CFDictionary, &device) == noErr else { return }
        defer { AudioHardwareDestroyAggregateDevice(device) }
        var proc: AudioDeviceIOProcID?
        guard AudioDeviceCreateIOProcIDWithBlock(&proc, device, nil, { _, _, _, _, _ in }) == noErr,
              let proc
        else { return }
        defer { AudioDeviceDestroyIOProcID(device, proc) }
        guard AudioDeviceStart(device, proc) == noErr else { return }
        Thread.sleep(forTimeInterval: 1)
        AudioDeviceStop(device, proc)
    }
}
