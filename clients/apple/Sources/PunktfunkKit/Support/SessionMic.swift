// Whether a session has a microphone to mute, and the mute its uplink applies. `SessionModel`
// reads both; the platform is an argument, so the Mac runs the tvOS row (`SessionMicTests`).

import AVFoundation

public enum SessionMic {
    #if os(tvOS)
    /// tvOS gives an app no microphone; `SessionAudio` never opens an uplink there.
    public static let platformHasMic = false
    #else
    public static let platformHasMic = true
    #endif

    /// The session sends a microphone, so a mute control is no lie: the platform has one, the
    /// session's resolved settings turn it on, its grants carry MIC, and the OS has not refused
    /// it. `.notDetermined` counts: a grant during the session starts the uplink. The same
    /// conditions `SessionAudio` starts an uplink on; `authorization` is read only when the
    /// rest allow it.
    public static func available(
        hasMic: Bool = platformHasMic, enabled: Bool, granted: Bool,
        authorization: @autoclosure () -> AVAuthorizationStatus
    ) -> Bool {
        guard hasMic, enabled, granted else { return false }
        switch authorization() {
        case .authorized, .notDetermined: return true
        default: return false
        }
    }

    /// The mute the uplink applies: the player's own, or the background keep-alive's privacy
    /// mute. Neither clears the other, so a return from the background keeps a player's mute,
    /// and an unmute while in the background opens nothing.
    public static func muted(user: Bool, backgrounded: Bool) -> Bool {
        user || backgrounded
    }
}
