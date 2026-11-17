// GlassStyle.swift — the app's single, availability-gated entry point to Apple's "Liquid
// Glass" (iOS / macOS / tvOS 26). Every Liquid Glass symbol (glassEffect, Glass, the
// .glassProminent button style …) is HARD-gated to OS 26: referencing one with our
// deployment targets (macOS 14 / iOS 17 / tvOS 17) is a COMPILE error, not a silent no-op,
// unless it sits behind `if #available`. So all glass in the app routes through the two
// helpers below, each of which falls back to `.regularMaterial` / `.borderedProminent` on
// older OSes, and the gating lives in exactly one file.

import SwiftUI

// MARK: - Glass background

/// Liquid Glass behind a floating / overlay surface, with the pre-26 `.regularMaterial`
/// look as the fallback. Use ONLY on the floating control / overlay layer (the streaming
/// HUD, the trust card, the touch exit chip) — never on content tiles or dense forms (HIG).
///
/// `glassEffect()`'s own default shape is a Capsule, so panels MUST pass an explicit shape
/// (a RoundedRectangle / Circle) or they render as a pill. `interactive` makes the glass
/// react to press — only meaningful when the glass itself is the tap target.
private struct GlassBackground<S: Shape>: ViewModifier {
    let shape: S
    var interactive = false

    #if os(visionOS)
    @Environment(\.inAttachment) private var inAttachment
    #endif

    func body(content: Content) -> some View {
        #if os(visionOS)
        // visionOS materials are its glass; a RealityView attachment draws none, so a dark
        // card keeps the text off the picture there.
        if inAttachment {
            content.background(.black.opacity(0.8), in: shape)
        } else {
            content.background(.regularMaterial, in: shape)
        }
        #else
        if #available(iOS 26, macOS 26, tvOS 26, *) {
            content.glassEffect(interactive ? .regular.interactive() : .regular, in: shape)
        } else {
            content.background(.regularMaterial, in: shape)
        }
        #endif
    }
}

#if os(visionOS)
extension EnvironmentValues {
    /// The view floats in a RealityView attachment, where materials do not draw.
    @Entry var inAttachment = false
}
#endif

extension View {
    /// Liquid Glass (26+) or the existing `.regularMaterial` (pre-26) behind a floating
    /// surface. Pass the surface's shape explicitly — glass defaults to a Capsule otherwise.
    func glassBackground<S: Shape>(_ shape: S, interactive: Bool = false) -> some View {
        modifier(GlassBackground(shape: shape, interactive: interactive))
    }
}

// MARK: - Glass primary button

/// The single prominent action on a floating / overlay or sheet surface: the Liquid-Glass
/// prominent button style on 26+, falling back to `.borderedProminent` (the app's current
/// primary style) below. Apply directly to a `Button`; role / keyboardShortcut / disabled
/// chain after it as usual. tvOS stays `.borderedProminent` always — glass chrome fights the
/// focus engine. visionOS has no `.glassProminent` (unavailable in the XROS 27.0 SDK).
private struct GlassProminentButton: ViewModifier {
    func body(content: Content) -> some View {
        #if os(tvOS) || os(visionOS)
        content.buttonStyle(.borderedProminent)
        #else
        if #available(iOS 26, macOS 26, *) {
            content.buttonStyle(.glassProminent)
        } else {
            content.buttonStyle(.borderedProminent)
        }
        #endif
    }
}

extension View {
    /// Liquid-Glass prominent style (26+, non-tvOS) or `.borderedProminent`. Drop-in for the
    /// `.buttonStyle(.borderedProminent)` on a surface's primary action.
    func glassProminentButtonStyle() -> some View {
        modifier(GlassProminentButton())
    }
}
