// CoreAudio HAL device enumeration for the Settings pickers. Devices are persisted by
// UID (stable across reboots/replugs — AudioDeviceIDs are not); the empty UID means
// "system default", which additionally tracks default-device changes because we then
// never pin the engine to a concrete device.

#if os(macOS)
import CoreAudio
import Foundation

public struct AudioDevice: Hashable, Identifiable, Sendable {
    public let uid: String
    public let name: String
    public var id: String { uid }
}

public enum AudioDevices {
    /// Output-capable devices (speakers, headphones, multi-output…).
    public static func outputs() -> [AudioDevice] {
        all().filter { hasStreams($0, scope: kAudioObjectPropertyScopeOutput) }
            .compactMap(describe)
    }

    /// Input-capable devices (microphones, interfaces…).
    public static func inputs() -> [AudioDevice] {
        all().filter { hasStreams($0, scope: kAudioObjectPropertyScopeInput) }
            .compactMap(describe)
    }

    /// Resolve a persisted UID to the current AudioDeviceID — nil when unplugged.
    static func deviceID(forUID uid: String) -> AudioDeviceID? {
        all().first { id in
            stringProperty(id, kAudioDevicePropertyDeviceUID) == uid
        }
    }

    /// Input channel count of the mic the picker would use — the device with this UID, or the
    /// system default input when `uid` is empty. 0 when it can't be resolved. Drives the
    /// "Microphone channel" picker (only shown for multi-channel interfaces).
    public static func inputChannelCount(forUID uid: String) -> Int {
        let id = uid.isEmpty ? defaultInputDevice() : deviceID(forUID: uid)
        guard let id else { return 0 }
        return channelCount(id, scope: kAudioObjectPropertyScopeInput)
    }

    /// 0-based input channels the device names as loopback (Focusrite 4th Gen: "Loopback 1",
    /// "Loopback 2"). They carry what the Mac plays, so a mic mix that took them would send the
    /// stream back to the host.
    static func loopbackInputChannels(of id: AudioDeviceID) -> [Int] {
        (0..<channelCount(id, scope: kAudioObjectPropertyScopeInput)).filter { c in
            let name = stringProperty(
                id, kAudioObjectPropertyElementName,
                scope: kAudioObjectPropertyScopeInput, element: UInt32(c + 1))
            return name?.lowercased().filter(\.isLetter).contains("loopback") == true
        }
    }

    /// The device the system is currently capturing from — the key `SessionAudio`'s
    /// voice-processing gate latches a start failure against (the failure is a property of the
    /// input device, so a new device earns a fresh attempt).
    static func defaultInputDevice() -> AudioDeviceID? {
        systemDevice(kAudioHardwarePropertyDefaultInputDevice)
    }

    /// The device the system is currently playing to — what an engine with no pinned speaker UID
    /// follows, and so what `SessionAudio` compares its live output device against when the
    /// default moves (AirPods in or out, a headset unplugged).
    static func defaultOutputDevice() -> AudioDeviceID? {
        systemDevice(kAudioHardwarePropertyDefaultOutputDevice)
    }

    private static func systemDevice(
        _ selector: AudioObjectPropertySelector
    ) -> AudioDeviceID? {
        var address = AudioObjectPropertyAddress(
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var dev = AudioDeviceID(0)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        guard AudioObjectGetPropertyData(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &dev) == noErr,
            dev != 0
        else { return nil }
        return dev
    }

    /// Sum of channels across the device's streams in `scope` (its total input/output channels).
    private static func channelCount(
        _ id: AudioDeviceID, scope: AudioObjectPropertyScope
    ) -> Int {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreamConfiguration,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size) == noErr, size > 0
        else { return 0 }
        let raw = UnsafeMutableRawPointer.allocate(
            byteCount: Int(size), alignment: MemoryLayout<AudioBufferList>.alignment)
        defer { raw.deallocate() }
        guard AudioObjectGetPropertyData(id, &address, 0, nil, &size, raw) == noErr else { return 0 }
        let abl = UnsafeMutableAudioBufferListPointer(
            raw.assumingMemoryBound(to: AudioBufferList.self))
        return abl.reduce(0) { $0 + Int($1.mNumberChannels) }
    }

    private static func all() -> [AudioDeviceID] {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyDevices,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        guard AudioObjectGetPropertyDataSize(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size) == noErr,
            size > 0
        else { return [] }
        var ids = [AudioDeviceID](
            repeating: 0, count: Int(size) / MemoryLayout<AudioDeviceID>.size)
        guard AudioObjectGetPropertyData(
            AudioObjectID(kAudioObjectSystemObject), &address, 0, nil, &size, &ids) == noErr
        else { return [] }
        return ids
    }

    private static func hasStreams(
        _ id: AudioDeviceID, scope: AudioObjectPropertyScope
    ) -> Bool {
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioDevicePropertyStreams,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMain)
        var size: UInt32 = 0
        return AudioObjectGetPropertyDataSize(id, &address, 0, nil, &size) == noErr && size > 0
    }

    /// UID + human name for a live AudioDeviceID (nil if either property is unreadable).
    static func describe(_ id: AudioDeviceID) -> AudioDevice? {
        guard let uid = stringProperty(id, kAudioDevicePropertyDeviceUID),
              let name = stringProperty(id, kAudioObjectPropertyName)
        else { return nil }
        return AudioDevice(uid: uid, name: name)
    }

    private static func stringProperty(
        _ id: AudioDeviceID, _ selector: AudioObjectPropertySelector,
        scope: AudioObjectPropertyScope = kAudioObjectPropertyScopeGlobal,
        element: AudioObjectPropertyElement = kAudioObjectPropertyElementMain
    ) -> String? {
        var address = AudioObjectPropertyAddress(
            mSelector: selector,
            mScope: scope,
            mElement: element)
        var ref: CFString?
        var size = UInt32(MemoryLayout<CFString?>.size)
        let status = withUnsafeMutablePointer(to: &ref) { p in
            AudioObjectGetPropertyData(id, &address, 0, nil, &size, p)
        }
        guard status == noErr, let ref else { return nil }
        return ref as String
    }
}
#endif
