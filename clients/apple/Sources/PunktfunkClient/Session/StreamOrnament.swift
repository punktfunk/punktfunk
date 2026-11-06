// visionOS: the controls hung above a stream's scene, the window or the curve. They show for a
// session's first seconds, like the system's own window bar; otherwise they collapse to a small
// bar that a look opens.

#if os(visionOS)
import PunktfunkKit
import SwiftUI

/// The stream's controls. At rest only a small bar marks the spot; a look there swaps it for the
/// pill, as the system's window bar opens. A drag on the slider keeps it up; a pinch on the
/// picture is the host's click. The system draws the swap.
struct StreamOrnament: View {
    let quickActions: () -> Void
    @State private var resting = false
    /// The slider is being dragged: the controls stay up wherever the gaze goes.
    @State private var editing = false
    /// Each slider release starts the rest timer over.
    @State private var wakes = 0

    var body: some View {
        let shown = editing || !resting
        ZStack {
            Capsule()
                .fill(.white.opacity(0.55))
                .frame(width: 48, height: 5)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .contentShape(.hoverEffect, .rect(cornerRadius: 24))
                .hoverEffect { effect, active, _ in
                    effect.animation(.smooth(duration: 0.25)) { $0.opacity(active || shown ? 0 : 1) }
                }
            HStack(spacing: 12) {
                Button(action: quickActions) {
                    Label(L10n.appleQuickActionsA11y, systemImage: "ellipsis.circle")
                }
                CurveSlider(editing: $editing)
                NewWindowButton()
            }
            .labelStyle(.iconOnly)
            .padding(12)
            .glassBackgroundEffect()
            .contentShape(.hoverEffect, .rect(cornerRadius: 24))
            .hoverEffect { effect, active, _ in
                effect.animation(.smooth(duration: 0.25)) { $0.opacity(active || shown ? 1 : 0) }
            }
        }
        // A look at the bar or at any control is a look at the pill: one group swaps them.
        .hoverEffectGroup()
        .onChange(of: editing) { _, held in
            if !held { wakes &+= 1 }
        }
        // The first seconds of a session are longer.
        .task(id: wakes) {
            resting = false
            try? await Task.sleep(for: .seconds(wakes == 0 ? 6 : 3))
            resting = true
        }
    }
}

/// Flat to 800R, stop by stop: the stored Screen value, one with the Settings row. A curved stop
/// moves the picture to the curve, Flat back to the window; `editing` is true while it is held.
private struct CurveSlider: View {
    @Binding var editing: Bool
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen

    var body: some View {
        let stops = ScreenCurve.allCases
        let curve = ScreenCurve(name: screen)
        HStack(spacing: 8) {
            Slider(
                value: Binding(
                    get: { Double(stops.firstIndex(of: curve) ?? 0) },
                    set: { screen = stops[Int($0.rounded())].rawValue }),
                in: 0...Double(stops.count - 1), step: 1,
                label: { Text(L10n.playerScreen) },
                onEditingChanged: { editing = $0 })
            .labelsHidden()
            .accessibilityValue(curve.label)
            .frame(width: 160)
            Text(curve.label)
                .monospacedDigit()
                .frame(minWidth: 52, alignment: .leading)
        }
    }
}

/// Opens another main window: its own host list and, once connected, its own stream.
struct NewWindowButton: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button { openWindow(id: PunktfunkClientApp.mainSceneID) } label: {
            Label(L10n.appleNewWindow, systemImage: "macwindow.badge.plus")
        }
    }
}
#endif
