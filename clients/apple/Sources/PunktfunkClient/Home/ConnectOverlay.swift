// The unified "getting you connected" overlay — one look for BOTH phases of reaching a host, so the
// user gets feedback the instant they pick one and it flows seamlessly into a wake if the host turns
// out to be asleep. The Apple mirror of the Android client's `ConnectOverlay` and the shared console
// UI's connect/wake takeover; it replaces the old centered-card `WakeOverlay`.
//
//   - Connecting (`connectingHostName` non-nil): the dial is in flight. Shown immediately on activate
//     so a host that takes a beat to answer no longer looks like nothing happened.
//   - Waking (`waker.waking` non-nil): the dial failed on a sleeping host, so we're firing
//     Wake-on-LAN and waiting for it to advertise again, escalating to a retry/cancel prompt on
//     timeout.
//
// A Liquid Glass modal over a dim scrim, among the app's other floating surfaces (the trust card,
// the HUD). The console draws its own dial and wake wait, so this is the touch UI's.
//
// The two phases hand off within a single view update (HostWaker clears `waking` and starts the
// connect in the same MainActor step), so the overlay never blinks between them. It swallows input to
// the screen behind it, and the pad drives it (B cancels, A retries once timed out).

import PunktfunkKit
import SwiftUI

struct ConnectOverlay: View {
    /// Non-nil while a plain dial is in flight (the delegated-approval wait has its own prompt, so it
    /// passes nil here). Drives the "Connecting…" phase.
    let connectingHostName: String?
    @ObservedObject var waker: HostWaker
    /// Cancel a dial in flight — tears down the (uncancelable) connect and returns the UI; the late
    /// result is discarded by SessionModel's connect guard.
    var onCancelConnect: () -> Void

    private enum Phase {
        case connecting(name: String)
        case waking(HostWaker.Waking)
    }

    /// Waking takes precedence — it only ever exists after a dial has already failed, so a stray
    /// overlap can't strand the "Connecting…" phase over a wake in progress.
    private var phase: Phase? {
        if let w = waker.waking { return .waking(w) }
        if let name = connectingHostName { return .connecting(name: name) }
        return nil
    }

    var body: some View {
        if let phase {
            ZStack {
                Rectangle().fill(.black.opacity(0.5)).ignoresSafeArea()
                    .contentShape(Rectangle()).onTapGesture {}
                content(phase)
                    .padding(28)
                    .frame(maxWidth: 380)
                    .glassBackground(RoundedRectangle(cornerRadius: 26, style: .continuous))
                    .overlay(
                        RoundedRectangle(cornerRadius: 26, style: .continuous)
                            .strokeBorder(.white.opacity(0.12), lineWidth: 1))
                    .padding(40)
            }
            // Dark over its black scrim, whatever the system look.
            .environment(\.colorScheme, .dark)
            .transition(.opacity)
            #if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
            .background { ConnectControllerInput(waker: waker, onCancelConnect: onCancelConnect) }
            #endif
            #if os(tvOS)
            // Menu (a pad's B arrives as the same press). Reaches here because ContentView
            // disables the home under the takeover, so focus has nowhere to sit but this overlay.
            .onExitCommand { if waker.waking != nil { waker.cancel() } else { onCancelConnect() } }
            #endif
        }
    }

    /// White: the modal is forced dark over a black scrim.
    private let overlayFG: Color = .white

    @ViewBuilder private func content(_ phase: Phase) -> some View {
        let titleSize: CGFloat = 19
        let bodySize: CGFloat = 13
        VStack(spacing: 14) {
            switch phase {
            case .connecting(let name):
                ProgressView().controlSize(.large).tint(overlayFG)
                Text(L10n.dialConnectingTo(host: name))
                    .font(.geist(titleSize, .bold, relativeTo: .title3)).foregroundStyle(overlayFG)
                    .multilineTextAlignment(.center)
                Text(L10n.dialConnectingDetail)
                    .font(.geist(bodySize, relativeTo: .caption))
                    .foregroundStyle(overlayFG.opacity(0.6))
                Button(L10n.commonCancel) { onCancelConnect() }.buttonStyle(.bordered).padding(.top, 6)
            case .waking(let w) where w.timedOut:
                Image(systemName: "moon.zzz.fill")
                    .font(.system(size: 34))
                    .foregroundStyle(overlayFG.opacity(0.9))
                Text(L10n.dialWakeParked(host: w.hostName))
                    .font(.geist(titleSize, .bold, relativeTo: .title3)).foregroundStyle(overlayFG)
                    .multilineTextAlignment(.center)
                Text(L10n.dialWakeParkedHint)
                    .font(.geist(bodySize, relativeTo: .caption))
                    .foregroundStyle(overlayFG.opacity(0.6))
                    .multilineTextAlignment(.center)
                HStack(spacing: 12) {
                    Button(L10n.commonCancel) { waker.cancel() }.buttonStyle(.bordered)
                    Button(L10n.commonRetry) { waker.retry() }.glassProminentButtonStyle()
                }
                .padding(.top, 6)
            case .waking(let w):
                ProgressView().controlSize(.large).tint(overlayFG)
                Text(L10n.dialWaking(host: w.hostName))
                    .font(.geist(titleSize, .bold, relativeTo: .title3)).foregroundStyle(overlayFG)
                    .multilineTextAlignment(.center)
                Text(L10n.dialWakingProgress(seconds: String(w.seconds)))
                    .font(.geistFixed(bodySize)).foregroundStyle(overlayFG.opacity(0.6))
                    .monospacedDigit()
                // A wake-only wait (no dial after) offers "Stop Waiting"; a wake-&-connect is "Cancel".
                Button(w.connectsAfter ? L10n.commonCancel : L10n.dialStopWaiting) { waker.cancel() }
                    .buttonStyle(.bordered).padding(.top, 6)
            }
        }
    }
}

#if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
/// Controller binding for the overlay: B cancels whatever's in flight (a dial or the wake wait); A
/// retries once a wake has timed out. The closures read the live state on each press, so they stay
/// correct across the Connecting ↔ Waking handoff without the view re-mounting. A zero-size backing
/// view owning a `GamepadMenuInput` for the overlay's lifetime (the home is gated inactive while the
/// overlay is up, so nothing else is consuming the pad).
///
/// tvOS was left out of this until #453 reported the wake prompt as a dead end there. It is not a
/// platform that drives these buttons some other way: the console home it sits over runs this same
/// binding on tvOS, so with the home inactive and no binding here, nothing read the pad at all.
///
/// `GamepadMenuInput` needs an EXTENDED gamepad. A Siri Remote is not one and does not reach these
/// buttons through here — that path is the tvOS focus engine's, which lands in this overlay
/// because ContentView disables everything under it.
private struct ConnectControllerInput: View {
    @ObservedObject var waker: HostWaker
    var onCancelConnect: () -> Void
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .onAppear {
                input.onBack = { if waker.waking != nil { waker.cancel() } else { onCancelConnect() } }
                input.onConfirm = { if waker.waking?.timedOut == true { waker.retry() } }
                input.start()
            }
            .onDisappear { input.stop() }
    }
}
#endif
