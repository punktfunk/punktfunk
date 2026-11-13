#if DEBUG
import Combine
import GameController

/// Local feedback driver for the Settings → Controllers "Test Controller" panel (DEBUG builds
/// only). It drives the SAME CoreHaptics rumble renderer and `DualSenseTriggerEffect` path a
/// live session uses — just aimed at the physically-connected controller instead of the
/// host→client feedback planes — so rumble, the adaptive triggers, the lightbar and the player
/// LEDs can be confirmed on-device without a host. Reusing the real renderers is the point:
/// a passing test exercises the exact code a session runs.
@MainActor
public final class ControllerTester: ObservableObject {
    // The panel's toggles hold a level until changed: the renderer never ends one on its own.
    private let renderer = RumbleRenderer()
    private weak var controller: GCController?

    /// The rumble backend now in use — "DualSense HID · USB/Bluetooth", "CoreHaptics", or "—" —
    /// for the test panel to display so it's obvious which path a given pad takes.
    @Published public private(set) var rumbleBackend = "—"

    /// Why rumble structurally cannot work right now (nil = healthy) — e.g. the device's
    /// haptics service refusing every connection, or a pad with no rumble engine. Shown by the
    /// test panel so silence diagnoses itself instead of reading as an app bug.
    @Published public private(set) var rumbleHealth: String?

    public init() {}

    /// Aim the feedback at a controller (nil releases it). Idempotent — safe to call on every
    /// active-controller change.
    public func target(_ c: GCController?) {
        guard c !== controller else { return }
        controller = c
        renderer.retarget(
            c,
            onBackend: { [weak self] note in
                Task { @MainActor in self?.rumbleBackend = note }
            },
            onHealth: { [weak self] problem in
                Task { @MainActor in self?.rumbleHealth = problem }
            })
    }

    /// Drive both motors at 0...1 amplitudes — low = left/heavy, high = right/light — mapped to
    /// the 0...0xFFFF wire range the session carries, through the real `RumbleRenderer`.
    public func rumble(low: Float, high: Float) {
        func u16(_ v: Float) -> UInt16 { UInt16((min(max(v, 0), 1) * 65535).rounded()) }
        renderer.apply(low: u16(low), high: u16(high))
    }

    /// Replay an adaptive-trigger effect on a DualSense via the real `DualSenseTriggerEffect`
    /// renderer. `right == false` → L2, `true` → R2. No-op on a non-DualSense pad.
    public func applyTrigger(_ effect: DualSenseTriggerEffect, right: Bool) {
        guard let ds = controller?.extendedGamepad as? GCDualSenseGamepad else { return }
        effect.apply(to: right ? ds.rightTrigger : ds.leftTrigger)
    }

    public func resetTriggers() {
        guard let ds = controller?.extendedGamepad as? GCDualSenseGamepad else { return }
        ds.leftTrigger.setModeOff()
        ds.rightTrigger.setModeOff()
    }

    /// Lightbar colour (DualSense / DualShock 4); nil turns it off. No-op without a light.
    public func setLight(_ color: GCColor?) {
        controller?.light?.color = color ?? GCColor(red: 0, green: 0, blue: 0)
    }

    /// Player-indicator LEDs (`.index1`...`.index4`, or `.indexUnset` to clear).
    public func setPlayerIndex(_ index: GCControllerPlayerIndex) {
        controller?.playerIndex = index
    }

    /// Silence every channel and release the controller — call on the panel's disappear.
    public func stop() {
        resetTriggers()
        setPlayerIndex(.indexUnset)
        setLight(nil)
        renderer.retarget(nil) // async teardown: stops the motors + drops the controller ref
        controller = nil
    }
}
#endif
