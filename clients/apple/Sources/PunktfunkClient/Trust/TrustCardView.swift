// Trust-on-first-use prompt: shown over the live-but-blurred stream when connecting to an
// unpinned host. The user compares the fingerprint with the one the host logged at startup,
// or drops this and runs the PIN pairing ceremony instead.
//
// Controller-drivable on iOS/macOS (A trust, B cancel, X pair instead). It had no controller
// wiring at all, which made it a dead end for a pad-only user at the worst possible moment: the
// card appears mid-connect with capture disabled (ContentView blurs the stream and stops
// forwarding), so the pad in their hands genuinely did nothing and the only way past was to reach
// for the screen. tvOS needs none of this — the focus engine drives the buttons natively.

import Foundation
import PunktfunkKit
import SwiftUI

struct TrustCardView: View {
    let fingerprint: Data
    let onCancel: () -> Void
    let onTrust: () -> Void
    let onPairInstead: () -> Void

    #if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
    /// Observed so the legend appears the moment a pad wakes up mid-prompt — and so it stays
    /// absent for the mouse/touch users this card is otherwise for.
    @ObservedObject private var gamepads = GamepadManager.shared
    #endif

    var body: some View {
        // A phone on its side can be shorter than the card: it scrolls then, rather than
        // truncating the fingerprint.
        ViewThatFits(in: .vertical) {
            content
            ScrollView { content }
        }
        .padding(28)
        .frame(maxWidth: 440)
        // Floating trust card over the blurred stream — Liquid Glass on 26+, .regularMaterial
        // fallback below. The inner fingerprint box stays .quaternary (content, not glass).
        .glassBackground(RoundedRectangle(cornerRadius: 18))
        #if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
        .background {
            TrustControllerInput(onTrust: onTrust, onCancel: onCancel, onPairInstead: onPairInstead)
        }
        #endif
        // Off a phone's edges in portrait.
        .padding(16)
    }

    private var content: some View {
        VStack(spacing: 14) {
            Image(systemName: "lock.shield")
                .font(.system(size: 36, weight: .light))
                .foregroundStyle(.tint)
            Text(L10n.trustTitle)
                .font(.geist(20, .semibold, relativeTo: .title3))
            Text(L10n.trustExplain)
                .font(.geist(16, relativeTo: .callout))
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            Text(Self.format(fingerprint: fingerprint))
                .font(.system(.callout, design: .monospaced))
                // Always two lines of four groups: a narrow phone shrinks them instead.
                .lineLimit(2)
                .minimumScaleFactor(0.7)
                #if !os(tvOS)
                .textSelection(.enabled)
                #endif
                .padding(10)
                .background(.quaternary, in: RoundedRectangle(cornerRadius: 8))
            HStack(spacing: 12) {
                Button(L10n.commonCancel, role: .cancel, action: onCancel)
                    #if !os(tvOS)
                    .keyboardShortcut(.cancelAction)
                    #endif
                Button(L10n.trustAccept, action: onTrust)
                    // Opaque prominent, NOT glass: this card is itself a glass panel
                    // (.glassBackground below), and glass-on-glass loses contrast — a tinted
                    // bordered button reads cleanly over glass (HIG). The sheet primaries stay
                    // glass because the system manages the sheet's own glass layering.
                    .buttonStyle(.borderedProminent)
                    #if !os(tvOS)
                    .keyboardShortcut(.defaultAction)
                    #endif
            }
            #if os(iOS) || os(visionOS)
            .controlSize(.large)
            #endif
            // The verified alternative to eyeballing hex: drop this session (the host
            // serves one connection at a time) and run the SPAKE2 PIN ceremony instead.
            Button(L10n.pairUsePin, action: onPairInstead)
                #if os(macOS)
                .buttonStyle(.link)
                #else
                .buttonStyle(.borderless)
                #endif
                .font(.geist(16, relativeTo: .callout))
            #if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
            // Only with a pad attached: controller glyphs in front of a trackpad user would be
            // naming buttons they don't have.
            if gamepads.active != nil {
                GamepadHintBar(hints: [
                    .init(
                        glyph: buttonGlyph(\.buttonA, fallback: "a.circle"),
                        text: String(localized: L10n.trustAccept),
                        action: onTrust),
                    .init(
                        glyph: buttonGlyph(\.buttonX, fallback: "x.circle"),
                        text: String(localized: L10n.pairUsePin),
                        action: onPairInstead),
                    .init(
                        glyph: buttonGlyph(\.buttonB, fallback: "b.circle"),
                        text: String(localized: L10n.commonCancel),
                        action: onCancel),
                ])
                .padding(.top, 2)
            }
            #endif
        }
    }

    /// 64 hex chars → four groups per line, two lines — easy to eyeball against the log.
    private static func format(fingerprint: Data) -> String {
        let hex = fingerprint.hexLower
        let groups = stride(from: 0, to: hex.count, by: 8).map { i -> String in
            let start = hex.index(hex.startIndex, offsetBy: i)
            let end = hex.index(start, offsetBy: min(8, hex.count - i))
            return String(hex[start..<end])
        }
        return groups.chunks(of: 4).map { $0.joined(separator: " ") }.joined(separator: "\n")
    }
}

#if os(iOS) || os(visionOS) || os(macOS) || os(tvOS)
/// Controller binding for the trust prompt: A trusts, B cancels, X runs the PIN ceremony instead.
/// The same zero-size-backing-view shape as `ConnectOverlay`'s `ConnectControllerInput` — mounted
/// for exactly as long as the card is up, and `GamepadMenuInput`'s snapshot-on-start swallows
/// whatever button was still held when it appeared (the A press that started the connect is
/// usually still down).
///
/// Nothing else is polling the pad here: capture is off for the duration of the prompt, and the
/// home screens are unmounted behind the session view.
///
/// tvOS was excluded alongside `ConnectOverlay` and for no better reason (#453). It left the trust
/// card — which stands between a pad-only Apple TV user and every unknown host — with three
/// buttons and nothing reading the pad. As there, this covers an EXTENDED gamepad; a Siri Remote
/// reaches these buttons through the focus engine or not at all.
private struct TrustControllerInput: View {
    let onTrust: () -> Void
    let onCancel: () -> Void
    let onPairInstead: () -> Void
    @State private var input = GamepadMenuInput(manager: .shared)

    var body: some View {
        Color.clear
            .frame(width: 0, height: 0)
            .onAppear {
                input.onConfirm = onTrust
                input.onBack = onCancel
                input.onTertiary = onPairInstead
                input.start()
            }
            .onDisappear { input.stop() }
    }
}
#endif

private extension Array {
    func chunks(of size: Int) -> [[Element]] {
        stride(from: 0, to: count, by: size).map { Array(self[$0..<Swift.min($0 + size, count)]) }
    }
}
