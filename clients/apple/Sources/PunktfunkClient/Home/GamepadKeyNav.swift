// Hardware-keyboard navigation for the library grid (iOS, iPadOS, visionOS and macOS): arrows
// move, Return/Space activate, Esc backs out.
//
// An iPad on a Magic Keyboard and a couch Mac are the same situation the console layout was built
// for — a screen driven from a distance with a fixed set of directional inputs — and the whole
// navigation model (a cursor, a confirm, a back) already exists here for the controller. A
// keyboard is just a third input onto it, alongside the pad poll and touch.
//
// tvOS is excluded: the focus engine already routes hardware-keyboard arrows into focus moves
// there, and these screens hand it navigation authority on purpose.
//
// The view must be FOCUSED to receive key presses, so this takes focus on appear.

import PunktfunkKit
import SwiftUI
#if os(iOS) || os(visionOS) || os(macOS)

extension View {
    /// Route arrows / Return / Esc into the same handlers the controller poll drives.
    ///
    /// `active` off hands the keys back: the view is not focusable and every press is ignored.
    /// `LibraryView` passes `onLaunch != nil`, so a browse-only shelf takes no key presses.
    func gamepadKeyNavigation(
        active: Bool = true,
        onMove: @escaping (GamepadMenuInput.Direction) -> Void,
        onConfirm: @escaping () -> Void,
        onBack: (() -> Void)? = nil
    ) -> some View {
        modifier(GamepadKeyNav(active: active, onMove: onMove, onConfirm: onConfirm, onBack: onBack))
    }
}

private struct GamepadKeyNav: ViewModifier {
    let active: Bool
    let onMove: (GamepadMenuInput.Direction) -> Void
    let onConfirm: () -> Void
    let onBack: (() -> Void)?

    @FocusState private var focused: Bool

    func body(content: Content) -> some View {
        content
            .focusable(active)
            // No focus ring: these screens draw their own cursor (the centred card, the focused
            // row), and a system ring around the whole scroll view on top of it reads as a bug.
            .focusEffectDisabled()
            .focused($focused)
            // Claim focus on appear, and re-claim it whenever this screen becomes the active one
            // again — a pushed screen popping off leaves the one underneath unfocused.
            .onAppear { focused = active }
            .onChange(of: active) { _, nowActive in
                if nowActive { focused = true }
            }
            .onKeyPress(.upArrow) { handle { onMove(.up) } }
            .onKeyPress(.downArrow) { handle { onMove(.down) } }
            .onKeyPress(.leftArrow) { handle { onMove(.left) } }
            .onKeyPress(.rightArrow) { handle { onMove(.right) } }
            .onKeyPress(.return) { handle(onConfirm) }
            .onKeyPress(.space) { handle(onConfirm) }
            .onKeyPress(.escape) {
                guard let onBack else { return .ignored }
                return handle(onBack)
            }
    }

    /// Run a handler only while this screen owns input, and report back whether the press was
    /// consumed. `.ignored` matters: an unhandled Esc still has to reach the `.cancelAction`
    /// shortcut that closes a macOS sheet.
    private func handle(_ action: () -> Void) -> KeyPress.Result {
        guard active else { return .ignored }
        action()
        return .handled
    }
}
#endif
