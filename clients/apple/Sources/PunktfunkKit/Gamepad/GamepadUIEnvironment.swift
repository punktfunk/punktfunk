// Whether the app should front the console instead of the touch/desktop layouts. A pure function, not a singleton: the reactivity comes from callers already observing
// `GamepadManager.shared` and the `DefaultsKey.gamepadUIEnabled` @AppStorage themselves (the same
// local-read pattern SettingsView already uses for GamepadManager), so this stays the single place
// the inputs combine without adding a second ObservableObject or an environment key nobody else needs.

import Foundation
import PunktfunkShared

public enum GamepadUIEnvironment {
    /// `DefaultsKey.gamepadUIMode`: take over only while a controller is attached. The default,
    /// and what the switch meant when it was a lone Bool.
    public static let modeWhenConnected = "connected"
    /// `DefaultsKey.gamepadUIMode`: take over whenever the switch is on, pad or no pad — asked
    /// for by people driving a TV-connected iPad or a couch Mac, where the console layout is the
    /// one they want and the pad is not always awake.
    public static let modeAlways = "always"

    /// `enabledSetting` is the user's Settings switch (`DefaultsKey.gamepadUIEnabled`) — off means
    /// the touch/desktop UI, full stop. `mode` is `DefaultsKey.gamepadUIMode`, and only matters
    /// once the switch is on: `modeAlways` takes over unconditionally, anything else (including a
    /// value a newer client wrote) waits for a controller.
    ///
    /// `gamepadConnected` is `GamepadManager.shared.uiPadConnected` — true only once a usable
    /// controller is actually attached: an extended GameController pad (a non-extended-profile
    /// device leaves `active` nil, which keeps the touch UI), or the iOS Steam Controller 2
    /// GameController never surfaces. Exactly the sources `GamepadMenuInput` navigates with, so
    /// a screen this takes over is always a screen the pad can drive. A `Bool` rather than the
    /// `DiscoveredController` itself: this function has nothing else to inspect, and it keeps the
    /// helper testable without a real `GCController` (which XCTest can't construct).
    /// `mode` carries no default on purpose: a call site that forgot it would silently strand
    /// everyone who picked Always back on "only with a controller", which is exactly the bug
    /// this parameter exists to make impossible.
    public static func isActive(
        gamepadConnected: Bool,
        enabledSetting: Bool,
        mode: String
    ) -> Bool {
        guard enabledSetting else { return false }
        return mode == modeAlways || gamepadConnected
    }
}
