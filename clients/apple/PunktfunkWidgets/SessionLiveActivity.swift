// The Live Activity UI (Lock Screen banner + Dynamic Island) for a running session. The app owns
// the Activity's lifecycle (SessionActivityController); this is only its presentation, rendered in
// the widget-extension process from the shared `PunktfunkSessionAttributes`.
//
// The End button runs `EndStreamIntent` (a LiveActivityIntent) IN THE APP's process, which posts
// .punktfunkEndActiveSession → the app disconnects. Elapsed time ticks client-side via
// Text(timerInterval:) — no per-second push.

import ActivityKit
import AppIntents
import SwiftUI
import WidgetKit

import PunktfunkShared

struct PunktfunkSessionLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: PunktfunkSessionAttributes.self) { context in
            LockScreenView(context: context)
                .activitySystemActionForegroundColor(.white)
        } dynamicIsland: { context in
            // The EXPANDED island leads with identity (brand glyph + host), keeps the elapsed
            // clock at the trailing edge, and gives the bottom region the session status and
            // the End action. COMPACT shows one number: the elapsed clock while streaming, the
            // disconnect countdown while backgrounded, a state glyph otherwise.
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    HStack(spacing: 6) {
                        Image(systemName: "play.tv.fill")
                            .font(.subheadline)
                            .foregroundStyle(Color.brand)
                        Text(context.attributes.hostName)
                            .font(.subheadline.weight(.semibold))
                            .lineLimit(1)
                    }
                    .padding(.leading, 4)
                    .padding(.top, 2)
                }
                DynamicIslandExpandedRegion(.trailing) {
                    ElapsedClock(startedAt: context.state.startedAt)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .frame(maxWidth: 64, alignment: .trailing)
                        .padding(.trailing, 4)
                        .padding(.top, 2)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    // The expanded island's height is there to be used: the status row (the
                    // mode string stays on the Lock Screen), then the platform-conventional
                    // LARGE full-width action button.
                    VStack(spacing: 10) {
                        HStack {
                            StatusLine(state: context.state)
                            Spacer(minLength: 8)
                        }
                        Spacer(minLength: 0) // any slack height goes here — button hugs the bottom
                        EndButton(fullWidth: true)
                    }
                    .frame(maxHeight: .infinity)
                    .padding(.horizontal, 4)
                    .padding(.top, 6)
                }
            } compactLeading: {
                Image(systemName: "play.tv.fill")
                    .foregroundStyle(Color.brand)
            } compactTrailing: {
                CompactReadout(state: context.state)
            } minimal: {
                Image(systemName: "play.tv.fill")
                    .foregroundStyle(Color.brand)
            }
            .keylineTint(Color.brand)
        }
    }
}

// MARK: - Lock Screen banner

private struct LockScreenView: View {
    let context: ActivityViewContext<PunktfunkSessionAttributes>

    var body: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: "play.tv.fill")
                .font(.title2)
                .foregroundStyle(Color.brand)
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text(context.attributes.hostName).font(.headline).lineLimit(1)
                    Spacer()
                    ElapsedClock(startedAt: context.state.startedAt)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                }
                StatusLine(state: context.state)
                Text(context.state.modeLine)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .lineLimit(1)
            }
            if context.state.stage == .background {
                EndButton()
            }
        }
        .padding()
    }
}

// MARK: - Shared pieces

/// The ticking elapsed-session clock — client-side via `timerInterval`, no per-second push.
private struct ElapsedClock: View {
    let startedAt: Date
    var body: some View {
        Text(timerInterval: startedAt...Date.distantFuture, countsDown: false)
            .monospacedDigit()
            .multilineTextAlignment(.trailing)
    }
}

/// The keep-alive countdown. The lower bound is clamped because `Date()...deadline` traps once
/// the deadline is past, and an Activity outlives the app's `.ending` update whenever the app is
/// suspended first — the widget process would fatal-error instead of rendering.
private struct CountdownClock: View {
    let deadline: Date
    var body: some View {
        Text(timerInterval: min(Date(), deadline)...deadline, countsDown: true)
            .monospacedDigit()
    }
}

/// The session's state as a colored dot + label; while backgrounded with a deadline, the label
/// IS the countdown. One shared truth for the island bottom and the Lock Screen banner.
private struct StatusLine: View {
    let state: PunktfunkSessionAttributes.ContentState

    var body: some View {
        HStack(spacing: 5) {
            Circle()
                .fill(color)
                .frame(width: 6, height: 6)
            label
                .font(.caption2.weight(.medium))
                .foregroundStyle(.secondary)
                .lineLimit(1)
        }
    }

    private var color: Color {
        switch state.stage {
        case .streaming: return .green
        case .background: return .orange
        case .ending: return .secondary
        }
    }

    @ViewBuilder private var label: some View {
        switch state.stage {
        case .streaming:
            Text(L10n.appleLiveStreaming)
        case .background:
            if let deadline = state.backgroundDeadline {
                HStack(spacing: 3) {
                    Text(L10n.appleLiveBackgroundEndsIn)
                    CountdownClock(deadline: deadline)
                }
            } else {
                Text(L10n.appleLiveBackground)
            }
        case .ending:
            Text(L10n.appleLiveEnded)
        }
    }
}

/// The compact trailing readout — the island's one glanceable number. Streaming: the elapsed
/// clock. Backgrounded: the auto-disconnect countdown. Ending shows a state glyph.
private struct CompactReadout: View {
    let state: PunktfunkSessionAttributes.ContentState

    var body: some View {
        switch state.stage {
        case .streaming:
            ElapsedClock(startedAt: state.startedAt)
                .font(.caption2)
                .frame(maxWidth: 48)
        case .background:
            if let deadline = state.backgroundDeadline {
                CountdownClock(deadline: deadline)
                    .font(.caption2)
                    .multilineTextAlignment(.trailing)
                    .frame(maxWidth: 48)
                    .foregroundStyle(.orange)
            } else {
                Image(systemName: "moon.fill").foregroundStyle(.orange)
            }
        case .ending:
            Image(systemName: "stop.fill").foregroundStyle(.secondary)
        }
    }
}

/// End-stream button — runs EndStreamIntent in the app process (LiveActivityIntent).
/// `fullWidth` is the expanded island's large bottom action (the platform convention there);
/// the compact form remains the Lock Screen banner's trailing button while backgrounded.
private struct EndButton: View {
    var fullWidth = false

    var body: some View {
        if fullWidth {
            Button(intent: EndStreamIntent()) {
                Label(L10n.appleLiveEndSession, systemImage: "stop.fill")
                    .font(.subheadline.weight(.semibold))
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 2)
            }
            .tint(.red)
            .buttonStyle(.bordered)
        } else {
            Button(intent: EndStreamIntent()) {
                Label(L10n.appleLiveEnd, systemImage: "stop.fill")
                    .font(.caption).bold()
            }
            .tint(.red)
            .buttonStyle(.bordered)
        }
    }
}

// MARK: - Previews (Xcode canvas)
//
// The PunktfunkWidgetsExtension scheme's canvas renders every surface without starting an
// Activity; the island states need a Dynamic Island phone as the canvas device. Each content
// state is one frame of the timeline strip. Sample state stays fileprivate.

extension PunktfunkSessionAttributes {
    fileprivate static var preview: PunktfunkSessionAttributes {
        PunktfunkSessionAttributes(hostID: UUID(), hostName: "Studio")
    }
}

extension PunktfunkSessionAttributes.ContentState {
    fileprivate static var streaming: Self {
        .init(
            stage: .streaming, startedAt: .now.addingTimeInterval(-754),
            modeLine: "2752×2064 @120 · HEVC · HDR")
    }
    fileprivate static var backgrounded: Self {
        .init(
            stage: .background, startedAt: .now.addingTimeInterval(-1975),
            modeLine: "2752×2064 @120 · HEVC · HDR",
            backgroundDeadline: .now.addingTimeInterval(9 * 60))
    }
    fileprivate static var ended: Self {
        .init(
            stage: .ending, startedAt: .now.addingTimeInterval(-3541),
            modeLine: "2752×2064 @120 · HEVC · HDR")
    }
}

#Preview("Lock Screen", as: .content, using: PunktfunkSessionAttributes.preview) {
    PunktfunkSessionLiveActivity()
} contentStates: {
    PunktfunkSessionAttributes.ContentState.streaming
    PunktfunkSessionAttributes.ContentState.backgrounded
    PunktfunkSessionAttributes.ContentState.ended
}

#Preview(
    "Island expanded", as: .dynamicIsland(.expanded),
    using: PunktfunkSessionAttributes.preview
) {
    PunktfunkSessionLiveActivity()
} contentStates: {
    PunktfunkSessionAttributes.ContentState.streaming
    PunktfunkSessionAttributes.ContentState.backgrounded
}

#Preview(
    "Island compact", as: .dynamicIsland(.compact),
    using: PunktfunkSessionAttributes.preview
) {
    PunktfunkSessionLiveActivity()
} contentStates: {
    PunktfunkSessionAttributes.ContentState.streaming
}

#Preview(
    "Island minimal", as: .dynamicIsland(.minimal),
    using: PunktfunkSessionAttributes.preview
) {
    PunktfunkSessionLiveActivity()
} contentStates: {
    PunktfunkSessionAttributes.ContentState.streaming
}
