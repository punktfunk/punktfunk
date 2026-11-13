// The resize overlay (design/midstream-resolution-resize.md — client resize UX). A Match-window
// resize renegotiates the host's virtual display + encoder and re-inits the local VideoToolbox
// decoder on the first new-mode IDR — an unavoidable sub-second gap where the last frame lingers,
// briefly freezes, or the picture pops to the new geometry. Rather than let that read as a stutter,
// we make it DELIBERATE: the caller blurs the live stream and this centered spinner + caption
// acknowledges the transition. It clears the instant a frame at the requested size decodes (the
// `onDecodedSize` END signal) or on the follower's safety timeout — see `SessionModel.resizing`.
//
// Floating overlay, never a hit-test target: input keeps flowing to the stream underneath so a
// resize the user triggers by dragging the window never swallows their next click.

import PunktfunkKit
import SwiftUI

struct ResizeIndicatorView: View {
    /// Mirrors `SessionModel.resizing`; the fade in/out is driven off this.
    let active: Bool

    var body: some View {
        ZStack {
            if active {
                VStack(spacing: 12) {
                    ProgressView().controlSize(.large).tint(.white)
                    Text(L10n.noticeResizing)
                        .font(.geist(15, .medium, relativeTo: .callout))
                        .foregroundStyle(.white.opacity(0.85))
                }
                .padding(.horizontal, 30)
                .padding(.vertical, 24)
                .glassBackground(RoundedRectangle(cornerRadius: 20, style: .continuous))
                .overlay(
                    RoundedRectangle(cornerRadius: 20, style: .continuous)
                        .strokeBorder(.white.opacity(0.12), lineWidth: 1))
                .transition(.opacity.combined(with: .scale(scale: 0.92)))
            }
        }
        .environment(\.colorScheme, .dark) // the spinner + glass read over any frame
        .animation(.easeInOut(duration: 0.22), value: active)
        .allowsHitTesting(false) // the stream keeps receiving input the whole time
    }
}
