// DEBUG-only controller test panel, reached from Settings → Controllers → "Test Controller…".
// It shows the live input of the active controller and lets you fire the host→client feedback
// channels — rumble, DualSense adaptive triggers, lightbar, player LEDs — straight at the
// physical pad (no host needed), so the rendering paths a session uses can be confirmed
// on-device. Driven by PunktfunkKit's `ControllerTester`, which reuses the real renderers.
//
// Every card renders a plain value model (`ShotPad` / `InputSnapshot`) that the live path samples
// out of the real pad each timeline tick. A GCController cannot be constructed, and the App Store
// screenshot harness needs this panel with pads the capture machine doesn't have — ShotScenes
// injects them via `shotPads` (the same seam as `padsOverride` on Android's ControllersScreen).
//
// tvOS is excluded: the panel is laid out for a pointer. macOS + iOS/iPadOS cover the
// validation need.

#if DEBUG && !os(tvOS)
import GameController
import PunktfunkKit
import SwiftUI

@MainActor
struct ControllerTestView: View {
    /// What one panel section says about a pad, as plain values. The live path flattens the
    /// active `DiscoveredController` into one; the screenshot harness hands the panel pads that
    /// were never connected. `input`/`rumbleBackend` are the harness's section knobs (nil hides
    /// that card) — the live path always shows both, fed from the live pad and tester.
    struct ShotPad: Identifiable {
        let name: String
        /// The header's second line. Production shows the GC product category; a shot packs
        /// transport/battery/player facts into it (the panel has no dedicated battery row).
        let detail: String
        let isDualSense: Bool
        let hasAdaptiveTriggers: Bool
        let hasLight: Bool
        var input: InputSnapshot? = nil
        var rumbleBackend: String? = nil
        var id: String { name }
    }

    /// One frame of the input readout. The live path samples the real `GCExtendedGamepad` into
    /// one of these on every 30 Hz tick; the harness writes a mid-game frame by hand.
    struct InputSnapshot {
        struct Stick {
            var x: Float
            var y: Float
            var pressed = false
        }
        struct Touch {
            /// Finger position in GC's -1...1 axes; nil = lifted. (GC snaps a lifted finger to
            /// exactly (0, 0), so a real (0, 0) contact is indistinguishable anyway.)
            var primary: CGPoint?
            var secondary: CGPoint?
            var clicked = false
        }
        struct Motion {
            var gyro: SIMD3<Double>
            var accel: SIMD3<Double>
        }
        var leftStick: Stick
        var rightStick: Stick
        var leftTrigger: Float = 0
        var rightTrigger: Float = 0
        /// Grid order; label → pressed.
        var buttons: [(String, Bool)]
        var touchpad: Touch?
        var motion: Motion?
    }

    @Environment(\.dismiss) private var dismiss
    @ObservedObject private var gamepads = GamepadManager.shared
    @StateObject private var tester = ControllerTester()

    /// Screenshot-harness injection — nil (the app) renders the live active pad.
    private let shotPads: [ShotPad]?

    init(shotPads: [ShotPad]? = nil) {
        self.shotPads = shotPads
    }

    @State private var heavyOn = false
    @State private var lightOn = false
    @State private var intensity = 0.75
    @State private var triggerTarget = TriggerTarget.both
    @State private var playerLED = -1

    private enum TriggerTarget: String, CaseIterable, Identifiable {
        case left = "L2", right = "R2", both = "Both"
        var id: String { rawValue }
    }

    private struct TriggerDemo: Identifiable {
        let label: String
        let effect: DualSenseTriggerEffect
        var id: String { label }
    }

    private static let triggerDemos: [TriggerDemo] = [
        .init(label: "Off", effect: .off),
        .init(label: "Resistance", effect: .feedback(start: 0.3, strength: 0.7)),
        .init(label: "Weapon", effect: .weapon(start: 0.4, end: 0.7, strength: 0.9)),
        .init(label: "Vibration", effect: .vibration(start: 0.1, amplitude: 0.8, frequency: 0.5)),
        .init(label: "Bow", effect: .slope(start: 0.2, end: 0.9, startStrength: 0.2, endStrength: 0.9)),
    ]

    // (display name, hardware colour, swatch colour)
    private static let lightSwatches: [(String, GCColor, Color)] = [
        ("Red", GCColor(red: 1, green: 0, blue: 0), .red),
        ("Green", GCColor(red: 0, green: 1, blue: 0), .green),
        ("Blue", GCColor(red: 0, green: 0.2, blue: 1), .blue),
        ("White", GCColor(red: 1, green: 1, blue: 1), .white),
    ]

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Text("Test Controller").font(.geist(17, .semibold, relativeTo: .headline))
                Spacer()
                Button("Done") { dismiss() }.keyboardShortcut(.cancelAction)
            }
            .padding()
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    if let shotPads {
                        ForEach(shotPads) { pad in
                            shotPanel(pad)
                        }
                    } else if let active = gamepads.active {
                        livePanel(active)
                    } else {
                        ContentUnavailableView(
                            "No controller",
                            systemImage: "gamecontroller",
                            description: Text("Connect a controller and pick it under "
                                + "Settings → Controllers → Use controller."))
                            .frame(maxWidth: .infinity, minHeight: 220)
                    }
                }
                .padding()
            }
        }
        .frame(minWidth: 420, minHeight: 540)
        .onAppear { if shotPads == nil { tester.target(gamepads.active?.controller) } }
        .onDisappear { if shotPads == nil { tester.stop() } }
        .onChange(of: gamepads.active?.id) { _, _ in
            guard shotPads == nil else { return }
            heavyOn = false
            lightOn = false
            playerLED = -1
            // Quiet the pad we are LEAVING first: `target` only swaps the rumble renderer, so a
            // DualSense would keep its lightbar, its player LED and its trigger effect with no
            // control left on screen that reaches it.
            tester.stop()
            tester.target(gamepads.active?.controller)
        }
    }

    // MARK: Panels

    @ViewBuilder
    private func livePanel(_ active: GamepadManager.DiscoveredController) -> some View {
        let pad = Self.describe(active)
        header(pad)
        liveInputCard
        rumbleCard(backend: tester.rumbleBackend, health: tester.rumbleHealth)
        triggerCard(pad)
        extrasCard(pad)
    }

    /// An injected pad's cards, in the live panel's order. The adaptive-trigger card is skipped
    /// outright for a pad without them — the live path's "needs a DualSense" hint is a diagnosis,
    /// and a capture has nothing to diagnose.
    @ViewBuilder
    private func shotPanel(_ pad: ShotPad) -> some View {
        header(pad)
        if let input = pad.input {
            card("Input") { inputReadout(input) }
        }
        if let backend = pad.rumbleBackend {
            rumbleCard(backend: backend, health: nil)
        }
        if pad.hasAdaptiveTriggers {
            triggerCard(pad)
        }
        extrasCard(pad)
    }

    /// The live pad, flattened to what the panel renders about it.
    private static func describe(_ c: GamepadManager.DiscoveredController) -> ShotPad {
        ShotPad(
            name: c.name, detail: c.productCategory, isDualSense: c.isDualSense,
            hasAdaptiveTriggers: c.hasAdaptiveTriggers, hasLight: c.hasLight)
    }

    // MARK: Header

    private func header(_ pad: ShotPad) -> some View {
        HStack(spacing: 10) {
            Image(systemName: pad.isDualSense ? "playstation.logo" : "gamecontroller.fill")
                .font(.title2)
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(pad.name).font(.geist(17, .semibold, relativeTo: .headline))
                Text(pad.detail).font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
            }
            Spacer()
        }
    }

    // MARK: Input

    private var liveInputCard: some View {
        card("Input") {
            // Poll the live controller at 30 Hz — no handlers installed, so nothing else's
            // capture is disturbed.
            TimelineView(.periodic(from: .now, by: 1.0 / 30.0)) { _ in
                if let gp = gamepads.active?.controller.extendedGamepad {
                    inputReadout(Self.snapshot(gp, controller: gamepads.active?.controller))
                } else {
                    Text("Not an extended gamepad").foregroundStyle(.secondary)
                }
            }
        }
    }

    /// One readout frame off the live pad.
    private static func snapshot(
        _ g: GCExtendedGamepad, controller: GCController?
    ) -> InputSnapshot {
        var buttons: [(String, Bool)] = [
            ("A", g.buttonA.isPressed), ("B", g.buttonB.isPressed),
            ("X", g.buttonX.isPressed), ("Y", g.buttonY.isPressed),
            ("LB", g.leftShoulder.isPressed), ("RB", g.rightShoulder.isPressed),
            ("L3", g.leftThumbstickButton?.isPressed ?? false),
            ("R3", g.rightThumbstickButton?.isPressed ?? false),
            ("Menu", g.buttonMenu.isPressed),
            ("Opts", g.buttonOptions?.isPressed ?? false),
            ("↑", g.dpad.up.isPressed), ("↓", g.dpad.down.isPressed),
            ("←", g.dpad.left.isPressed), ("→", g.dpad.right.isPressed),
        ]
        let tp = touchpad(g)
        if let tp { buttons.append(("Pad", tp.button.isPressed)) }
        return InputSnapshot(
            leftStick: .init(
                x: g.leftThumbstick.xAxis.value, y: g.leftThumbstick.yAxis.value,
                pressed: g.leftThumbstickButton?.isPressed ?? false),
            rightStick: .init(
                x: g.rightThumbstick.xAxis.value, y: g.rightThumbstick.yAxis.value,
                pressed: g.rightThumbstickButton?.isPressed ?? false),
            leftTrigger: g.leftTrigger.value, rightTrigger: g.rightTrigger.value,
            buttons: buttons,
            touchpad: tp.map {
                .init(primary: finger($0.primary), secondary: finger($0.secondary),
                      clicked: $0.button.isPressed)
            },
            motion: controller?.motion.map { m -> InputSnapshot.Motion in
                let a = totalAccel(m)
                return .init(
                    gyro: .init(m.rotationRate.x, m.rotationRate.y, m.rotationRate.z),
                    accel: .init(a.0, a.1, a.2))
            })
    }

    private static func finger(_ pad: GCControllerDirectionPad) -> CGPoint? {
        let x = pad.xAxis.value, y = pad.yAxis.value
        // GC snaps a lifted finger to exactly (0, 0).
        return (x == 0 && y == 0) ? nil : CGPoint(x: CGFloat(x), y: CGFloat(y))
    }

    @ViewBuilder
    private func inputReadout(_ s: InputSnapshot) -> some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack(alignment: .top, spacing: 20) {
                stick("L", s.leftStick)
                stick("R", s.rightStick)
                VStack(spacing: 8) {
                    triggerBar("L2", value: s.leftTrigger)
                    triggerBar("R2", value: s.rightTrigger)
                }
            }
            buttonGrid(s.buttons)
            if let tp = s.touchpad {
                touchpadView(tp)
            }
            if let m = s.motion {
                motionReadout(m)
            }
        }
    }

    private func stick(_ label: String, _ s: InputSnapshot.Stick) -> some View {
        VStack(spacing: 4) {
            ZStack {
                Circle().stroke(Color.secondary.opacity(0.3))
                Circle()
                    .fill(s.pressed ? Color.accentColor : Color.secondary)
                    .frame(width: 12, height: 12)
                    .offset(x: CGFloat(s.x) * 22, y: CGFloat(-s.y) * 22) // GC y is +up
            }
            .frame(width: 56, height: 56)
            Text("\(label) \(sgn(s.x)),\(sgn(s.y))").font(.caption2.monospaced()).foregroundStyle(.secondary)
        }
    }

    private func triggerBar(_ label: String, value: Float) -> some View {
        HStack(spacing: 6) {
            Text(label).font(.caption2.monospaced()).frame(width: 22, alignment: .leading)
            GeometryReader { geo in
                ZStack(alignment: .leading) {
                    Capsule().fill(Color.secondary.opacity(0.15))
                    Capsule().fill(Color.accentColor).frame(width: geo.size.width * CGFloat(value))
                }
            }
            .frame(height: 10)
            Text(mag(value)).font(.caption2.monospaced()).frame(width: 34, alignment: .trailing)
                .foregroundStyle(.secondary)
        }
        .frame(width: 150)
    }

    private func buttonGrid(_ items: [(String, Bool)]) -> some View {
        LazyVGrid(
            columns: Array(repeating: GridItem(.flexible(), spacing: 6), count: 5), spacing: 6
        ) {
            ForEach(items.indices, id: \.self) { i in
                Text(items[i].0)
                    .font(.caption.monospaced())
                    .frame(maxWidth: .infinity, minHeight: 24)
                    .background(
                        RoundedRectangle(cornerRadius: 6)
                            .fill(items[i].1 ? Color.accentColor : Color.secondary.opacity(0.15)))
                    .foregroundStyle(items[i].1 ? Color.white : Color.secondary)
            }
        }
    }

    private func touchpadView(_ tp: InputSnapshot.Touch) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Touchpad\(tp.clicked ? " — click" : "")")
                .font(.geist(11, relativeTo: .caption2)).foregroundStyle(.secondary)
            ZStack {
                RoundedRectangle(cornerRadius: 8).stroke(Color.secondary.opacity(0.3))
                fingerDot(tp.primary, color: .accentColor)
                fingerDot(tp.secondary, color: .orange)
            }
            .frame(width: 150, height: 74)
        }
    }

    private func fingerDot(_ p: CGPoint?, color: Color) -> some View {
        Circle().fill(color).frame(width: 10, height: 10)
            .offset(x: (p?.x ?? 0) * 71, y: -(p?.y ?? 0) * 33)
            .opacity(p == nil ? 0 : 1)
    }

    private func motionReadout(_ m: InputSnapshot.Motion) -> some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Motion").font(.geist(11, relativeTo: .caption2)).foregroundStyle(.secondary)
            Text(String(format: "gyro  %+.2f %+.2f %+.2f", m.gyro.x, m.gyro.y, m.gyro.z))
                .font(.caption2.monospaced())
            Text(String(format: "accel %+.2f %+.2f %+.2f", m.accel.x, m.accel.y, m.accel.z))
                .font(.caption2.monospaced())
        }
    }

    // MARK: Rumble

    private func rumbleCard(backend: String, health: String?) -> some View {
        card("Rumble") {
            VStack(alignment: .leading, spacing: 12) {
                Picker("Strength", selection: $intensity) {
                    Text("25%").tag(0.25)
                    Text("50%").tag(0.5)
                    Text("75%").tag(0.75)
                    Text("100%").tag(1.0)
                }
                .pickerStyle(.segmented)
                Toggle("Heavy motor (left)", isOn: $heavyOn)
                Toggle("Light motor (right)", isOn: $lightOn)
                Label("Backend: \(backend)", systemImage: "waveform")
                    .font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
                if let problem = health {
                    Label(problem, systemImage: "exclamationmark.triangle.fill")
                        .font(.geist(12, relativeTo: .caption)).foregroundStyle(.orange)
                }
                Text("Toggle a motor to feel it. The host maps a game's low/high-frequency "
                    + "rumble onto these two. A DualSense is driven over raw HID (CoreHaptics "
                    + "can't reach its motors on macOS).")
                    .font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
            }
            .onChange(of: heavyOn) { _, _ in applyRumble() }
            .onChange(of: lightOn) { _, _ in applyRumble() }
            .onChange(of: intensity) { _, _ in applyRumble() }
        }
    }

    private func applyRumble() {
        tester.rumble(low: heavyOn ? Float(intensity) : 0, high: lightOn ? Float(intensity) : 0)
    }

    // MARK: Adaptive triggers

    private func triggerCard(_ pad: ShotPad) -> some View {
        card("Adaptive triggers") {
            if pad.hasAdaptiveTriggers {
                VStack(alignment: .leading, spacing: 12) {
                    Picker("Apply to", selection: $triggerTarget) {
                        ForEach(TriggerTarget.allCases) { Text($0.rawValue).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    LazyVGrid(
                        columns: [GridItem(.adaptive(minimum: 96), spacing: 8)], spacing: 8
                    ) {
                        ForEach(Self.triggerDemos) { demo in
                            Button(demo.label) { applyTrigger(demo.effect) }
                                .buttonStyle(.bordered)
                        }
                    }
                    Text("Pick an effect, then pull L2/R2 to feel the resistance.")
                        .font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
                }
            } else {
                Text("Adaptive triggers need a DualSense.")
                    .font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
            }
        }
    }

    private func applyTrigger(_ e: DualSenseTriggerEffect) {
        switch triggerTarget {
        case .left: tester.applyTrigger(e, right: false)
        case .right: tester.applyTrigger(e, right: true)
        case .both:
            tester.applyTrigger(e, right: false)
            tester.applyTrigger(e, right: true)
        }
    }

    // MARK: Lightbar + player LED

    @ViewBuilder
    private func extrasCard(_ pad: ShotPad) -> some View {
        if pad.hasLight {
            card("Lightbar & player LED") {
                VStack(alignment: .leading, spacing: 12) {
                    HStack(spacing: 12) {
                        ForEach(Self.lightSwatches.indices, id: \.self) { i in
                            Button { tester.setLight(Self.lightSwatches[i].1) } label: {
                                Circle().fill(Self.lightSwatches[i].2)
                                    .frame(width: 26, height: 26)
                                    .overlay(Circle().stroke(Color.secondary.opacity(0.4)))
                            }
                            .buttonStyle(.plain)
                        }
                        Button("Off") { tester.setLight(nil) }.buttonStyle(.bordered)
                    }
                    Picker("Player LED", selection: $playerLED) {
                        Text("Off").tag(-1)
                        Text("1").tag(0)
                        Text("2").tag(1)
                        Text("3").tag(2)
                        Text("4").tag(3)
                    }
                    .pickerStyle(.segmented)
                    .onChange(of: playerLED) { _, v in
                        tester.setPlayerIndex(GCControllerPlayerIndex(rawValue: v) ?? .indexUnset)
                    }
                }
            }
        }
    }

    // MARK: Helpers

    private func card<Content: View>(
        _ title: String, @ViewBuilder _ content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Text(title).font(.geist(15, .semibold, relativeTo: .subheadline))
            content()
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(14)
        .background(RoundedRectangle(cornerRadius: 12).fill(Color.secondary.opacity(0.08)))
    }

    private func sgn(_ v: Float) -> String { String(format: "%+.2f", v) }
    private func mag(_ v: Float) -> String { String(format: "%.2f", v) }

    /// The touchpad surface of a PlayStation pad — `GCDualSenseGamepad` and `GCDualShockGamepad`
    /// don't share a touchpad type, so downcast either. `nil` for any other controller.
    private static func touchpad(
        _ g: GCExtendedGamepad
    ) -> (primary: GCControllerDirectionPad, secondary: GCControllerDirectionPad,
          button: GCControllerButtonInput)? {
        if let ds = g as? GCDualSenseGamepad {
            return (ds.touchpadPrimary, ds.touchpadSecondary, ds.touchpadButton)
        }
        if let ds4 = g as? GCDualShockGamepad {
            return (ds4.touchpadPrimary, ds4.touchpadSecondary, ds4.touchpadButton)
        }
        return nil
    }

    /// Total acceleration in g: gravity + user when the pad splits them, else the raw vector.
    private static func totalAccel(_ m: GCMotion) -> (Double, Double, Double) {
        if m.hasGravityAndUserAcceleration {
            return (m.gravity.x + m.userAcceleration.x,
                    m.gravity.y + m.userAcceleration.y,
                    m.gravity.z + m.userAcceleration.z)
        }
        return (m.acceleration.x, m.acceleration.y, m.acceleration.z)
    }
}
#endif
