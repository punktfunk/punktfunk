// Controller-side haptic feedback for the console's menus. The couch case is the whole point: the user is holding a game controller, not the
// iPhone/iPad, so a device-only `.sensoryFeedback` tick never reaches their hands — this plays a
// short CoreHaptics transient on the ACTIVE controller instead, so a dpad move / launch / end-stop
// is felt on the pad. (The views pair this with `.sensoryFeedback` so a touch/handheld user still
// gets the device Taptic tick; the two are independent channels, and both firing is intended.)
//
// This is menu-only — it never runs during a stream (the session's own GamepadFeedback owns the
// controller then), so there's no contention over the pad's haptic engine. Like GamepadMenuInput,
// it reads `GamepadManager.shared.active` fresh and rebuilds its engine when the controller
// changes, so a hot-swapped pad just starts buzzing on the next tick. Everything is best-effort:
// a pad with no haptics (many Xbox pads on iOS, a Siri Remote) silently no-ops.

import CoreHaptics
import Foundation
import GameController

@MainActor
public final class MenuHaptics {
    private let manager: GamepadManager
    /// The engine for the controller it was built against — dropped and rebuilt when `active`
    /// changes (identity compare) or after a stop/reset handler fires.
    private var engine: CHHapticEngine?
    private weak var boundController: GCController?

    public init(manager: GamepadManager) {
        self.manager = manager
    }

    /// A light, crisp detent — one per menu step. Deliberately tiny so a held direction repeating
    /// at `GamepadMenuInput.repeatInterval` reads as a smooth ratchet rather than a jackhammer.
    public func move() {
        play(intensity: 0.45, sharpness: 0.75, duration: 0.02)
    }

    /// A fuller, rounder pulse on confirm/launch — the "you did the thing" thunk.
    public func confirm() {
        play(intensity: 1.0, sharpness: 0.55, duration: 0.055)
    }

    /// A soft, dull bump when a move is refused at the end of a non-wrapping list — low sharpness so
    /// it feels like hitting a wall, distinct from the crisp `move()` detent.
    public func boundary() {
        play(intensity: 0.7, sharpness: 0.18, duration: 0.06)
    }

    /// Release the engine and forget the controller — call on the menu screen's disappear so the
    /// pad's haptic engine isn't held open while streaming or on the touch UI.
    public func stop() {
        engine?.stop(completionHandler: nil)
        engine = nil
        boundController = nil
    }

    /// Fire a single transient. Rebuilds the engine against the current active controller if it
    /// changed; swallows every failure (a pad without a haptics engine, a transient XPC hiccup) —
    /// menu haptics are a nicety, never a correctness path.
    private func play(intensity: Float, sharpness: Float, duration: TimeInterval) {
        guard let controller = manager.active?.controller else {
            // No pad (or a non-forwardable one): nothing to buzz. Drop any stale engine.
            if boundController != nil { stop() }
            return
        }
        guard let engine = engine(for: controller) else { return }
        let event = CHHapticEvent(
            eventType: .hapticTransient,
            parameters: [
                CHHapticEventParameter(parameterID: .hapticIntensity, value: intensity),
                CHHapticEventParameter(parameterID: .hapticSharpness, value: sharpness),
            ],
            relativeTime: 0,
            duration: duration)
        do {
            let player = try engine.makePlayer(with: CHHapticPattern(events: [event], parameters: []))
            try player.start(atTime: CHHapticTimeImmediate)
        } catch {
            // The engine went stale between builds (stopped/reset). Drop it; the next tick rebuilds.
            self.engine = nil
            boundController = nil
        }
    }

    /// The started engine for `controller`, (re)built on first use or after a controller swap.
    private func engine(for controller: GCController) -> CHHapticEngine? {
        if let engine, boundController === controller { return engine }
        engine?.stop(completionHandler: nil)
        engine = nil
        boundController = nil
        guard let built = controller.haptics?.createEngine(withLocality: .default) else { return nil }
        // Menu ticks carry no audio — keep the engine out of the app's audio session (the same
        // discipline the session RumbleRenderer uses).
        built.playsHapticsOnly = true
        // The haptic server can pull the engine out from under us (backgrounding, an audio
        // interruption, a controller drop); drop our reference so the next tick lazily rebuilds
        // rather than throwing forever.
        built.stoppedHandler = { [weak self] _ in
            Task { @MainActor in self?.dropEngine(if: controller) }
        }
        built.resetHandler = { [weak self] in
            Task { @MainActor in self?.dropEngine(if: controller) }
        }
        do {
            try built.start()
        } catch {
            return nil
        }
        engine = built
        boundController = controller
        return built
    }

    /// Drop the cached engine only if it's still the one for `controller` — a handler firing after a
    /// swap must not clobber the freshly built engine for the new pad.
    private func dropEngine(if controller: GCController) {
        guard boundController === controller else { return }
        engine = nil
        boundController = nil
    }
}
