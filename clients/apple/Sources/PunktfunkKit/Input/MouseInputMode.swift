/// How a physical mouse drives the host — the cross-client mouse model (the SDL clients'
/// `MouseMode` / `Settings::mouse_mode`, design/remote-desktop-sweep.md M1). Stored stringly
/// under `DefaultsKey.mouseMode`.
public enum MouseInputMode: String, CaseIterable, Sendable {
    /// Pointer capture (disassociated, hidden cursor, relative deltas) — the game model: the
    /// only cursor you see is the host's. `SettingDefault.mouseMode` picks the default.
    case capture
    /// Absolute pointer, uncaptured: the cursor enters and leaves the stream freely and
    /// motion is forwarded as absolute positions through the letterbox. The remote desktop
    /// model. Requires a host injector with absolute support (not gamescope).
    case desktop
}
