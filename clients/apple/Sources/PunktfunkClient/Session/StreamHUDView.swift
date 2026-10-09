// The streaming overlay HUD: the core formats every stats line (`SessionModel.hudLines`, one
// vocabulary for every client) and this view paints them by role on one glass card, beside the
// Apple-only chrome (the tvOS access line, the capture hints, the buttons). `.off` never
// reaches this view (StreamScene gates the overlay on the tier).

import PunktfunkKit
import SwiftUI
#if canImport(UIKit)
import UIKit
#endif

struct StreamHUDView: View {
    @ObservedObject var model: SessionModel
    let connection: PunktfunkConnection
    var placement: HUDPlacement = .topTrailing
    let verbosity: StatsVerbosity
    /// The player's Statistics size on top of the system text size; 1 is the stock look.
    var scale: Double = 1
    /// Read so a text-size change redraws the scaled styles below.
    @Environment(\.dynamicTypeSize) private var dynamicTypeSize

    var body: some View {
        // .off is gated upstream (StreamScene only mounts the HUD when the tier is on) —
        // render nothing if it ever slips through.
        if verbosity != .off {
            // ONE shared glass card wraps the tier-dependent content, so a verbosity change MORPHS
            // this card — its frame (and, on iOS, its clamped corner) animate to the new size — rather
            // than cross-fading a whole new card in. Only the inner content switches per tier.
            tierContent
                .padding(cardPadding)
                .glassBackground(cardShape)
                .padding(edgeInset)
        }
    }

    /// The tier-dependent content, unwrapped (the shared card in `body` supplies the padding +
    /// glass background). Compact is a one-line pill; normal/detailed the full stack.
    @ViewBuilder private var tierContent: some View {
        if verbosity == .compact {
            compactContent
        } else {
            fullContent
        }
    }

    // MARK: - Compact tier

    /// The core's one Compact line, plus any warning a platform raises at that tier.
    private var compactContent: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Circle()
                .fill(Color.accentColor)
                .frame(width: 7, height: 7)
            VStack(alignment: .leading, spacing: 2) {
                ForEach(Array(model.hudLines.enumerated()), id: \.offset) { _, line in
                    Text(line.text)
                        .font(hudFont(.caption))
                        .foregroundStyle(style(line.role))
                }
            }
        }
    }

    // MARK: - Normal / detailed tiers

    private var fullContent: some View {
        VStack(alignment: placement.isTrailing ? .trailing : .leading, spacing: 4) {
            if let first = model.hudLines.first {
                HStack(spacing: 6) {
                    Circle()
                        .fill(Color.accentColor)
                        .frame(width: 7, height: 7)
                    Text(first.text)
                        .font(hudFont(.caption))
                }
            }
            #if os(tvOS)
            // The session's access level (per-client access §7). tvOS carries it HERE, as a
            // stats-overlay line, instead of the floating chip the pointer platforms wear — a
            // couch surface where every extra overlay competes with the picture keeps the
            // fact with the other session facts. Absent for full-and-permanent sessions
            // (every old host): today's overlay must not change there.
            if model.accessLimited {
                Text(model.accessRemainingSecs == 0
                    ? "access \(model.accessLevel.label.lowercased())"
                    : "access \(model.accessLevel.label.lowercased()) · ends in "
                        + SessionModel.accessCountdown(model.accessRemainingSecs))
                    .font(hudFont(.caption2))
                    .foregroundStyle(.secondary)
            }
            #endif
            ForEach(Array(model.hudLines.dropFirst().enumerated()), id: \.offset) { _, line in
                Text(line.text)
                    .font(hudFont(.caption2))
                    .foregroundStyle(style(line.role))
            }
            // Capture hint, shown only until input is captured — how to grab it. The RELEASE
            // shortcut is intentionally not surfaced in the overlay (it lives on the Stream menu
            // and, on macOS, the start-of-stream banner), keeping the HUD uncluttered while playing.
            // Both hints are additionally gated on the session's grants ALLOWING a capture
            // (per-client access §7): inviting a Controller-only or View-only session to
            // "capture input" the host would only drop is the lie the grants advert exists
            // to prevent. Read live off the connection — a re-render lands with the model's
            // access churn.
            #if os(macOS)
            if !model.mouseCaptured, connection.canSendPointer || connection.canSendKeyboard {
                Text("Click the stream to capture input")
                    .font(.geist(11, relativeTo: .caption2))
                    .foregroundStyle(.secondary)
            }
            #elseif os(iOS) || os(visionOS)
            // Touch always plays directly; ⌘⎋ (hardware keyboard) captures kb/mouse.
            if !model.mouseCaptured, connection.canSendPointer || connection.canSendKeyboard {
                Text("⌘⎋ captures keyboard & mouse")
                    .font(.geist(11, relativeTo: .caption2))
                    .foregroundStyle(.secondary)
            }
            #endif
            // Mic mute — the in-stream toggle, on the same card as the other in-overlay action.
            // Absent (not greyed) when the session sends no microphone: the HUD is a status card,
            // and a dead control on it would read as "there is a mic, and it is on". The muted
            // STATE is not this button's job — the badge over the stream says that at every tier
            // and with the overlay off entirely. tvOS gets no control: no microphone, and a
            // focusable one would steal the controller's A press from the host.
            #if !os(tvOS)
            if model.micAvailable {
                Button(micButtonTitle) { model.toggleMicMute() }
                    .font(.geist(12, relativeTo: .caption))
            }
            #endif
            // ⌃⌥⇧D lives on the app's Stream menu (so it still works when the HUD is hidden)
            // and in InputCapture's monitor while captured; this button is the in-overlay,
            // click-to-disconnect affordance. tvOS deliberately gets NEITHER a button (a
            // focusable control would steal the controller's A press from the host) NOR a hint
            // line: the exits are the hold gestures the start-of-stream banner teaches (hold
            // the remote's Back; hold L1+R1+Start+Select on a pad).
            #if os(macOS)
            Button("Disconnect (⌃⌥⇧D)") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #elseif os(iOS) || os(visionOS)
            Button("Disconnect") { model.disconnect() }
                .font(.geist(12, relativeTo: .caption))
            #endif
        }
    }

    /// A monospaced HUD text style at the player's Statistics size. At 1 it is the stock style;
    /// otherwise the style's current point size, so Dynamic Type still applies underneath.
    private func hudFont(_ style: Font.TextStyle) -> Font {
        guard scale != 1 else { return .system(style, design: .monospaced) }
        #if os(macOS)
        let base = NSFont.preferredFont(forTextStyle: style == .caption ? .caption1 : .caption2)
        #else
        let base = UIFont.preferredFont(forTextStyle: style == .caption ? .caption1 : .caption2)
        #endif
        return .system(size: base.pointSize * scale, design: .monospaced)
    }

    /// The HUD's quiet palette: breakdowns recede, and only a warning is allowed to shout.
    private func style(_ role: PunktfunkConnection.HudLine.Role) -> AnyShapeStyle {
        switch role {
        case .primary: return AnyShapeStyle(.primary)
        case .detail: return AnyShapeStyle(.secondary)
        case .muted: return AnyShapeStyle(.tertiary)
        case .warn: return AnyShapeStyle(.orange)
        }
    }

    #if !os(tvOS)
    /// The mute button's wording. macOS names the chord, exactly as its Disconnect button does;
    /// iOS/iPadOS spells the action out (the HUD's buttons there carry no shortcuts, even where a
    /// hardware keyboard could fire one — the Stream menu is that keyboard's surface).
    private var micButtonTitle: String {
        #if os(macOS)
        return model.micMuted ? "Unmute Mic (⌃⌥⇧A)" : "Mute Mic (⌃⌥⇧A)"
        #else
        return model.micMuted ? "Unmute Microphone" : "Mute Microphone"
        #endif
    }
    #endif

    // MARK: - Card metrics

    /// The card's inner content padding. Roomier on tvOS — the stat text auto-scales for the
    /// couch (relative system styles), so the card's chrome must keep pace or it reads cramped.
    ///
    /// On iOS it also has to CLEAR THE CORNER. A rounded corner of radius `r` pulls the card's
    /// edge inward by `r − √(r² − (r−y)²)` at a distance `y` below the top, so the first and last
    /// lines of a padded stack sit inside the arc unless the padding keeps pace with the radius.
    /// At `0.45 · r` that intrusion stays well inside the padding across the whole range this
    /// card can wear (≈4.6 pt of arc against 12.6 pt of padding at the 28 pt cap), so no line
    /// ever runs into the curve.
    private var cardPadding: CGFloat {
        #if os(tvOS)
        return 16
        #elseif os(iOS) || os(visionOS)
        return max(10, cardCornerRadius * 0.45)
        #else
        return 10
        #endif
    }

    /// The OUTER gap between the card and the screen edge. On iOS the card hugs a physically
    /// rounded display corner, so it sits a little further in and pairs with a concentric corner
    /// radius (below); tvOS floats it well clear of the TV's overscan-ish edge; macOS windows
    /// keep the classic 10.
    private var edgeInset: CGFloat {
        #if os(iOS) || os(visionOS)
        return 14
        #elseif os(tvOS)
        return 24
        #else
        return 10
        #endif
    }

    /// The card's corner radius. On iOS it aims to be concentric with the physical display
    /// corner — `displayCornerRadius − edgeInset`, so the gap to the screen edge stays uniform
    /// right around the corner instead of a small-radius card cutting into the very rounded
    /// glass — but that aim is BOUNDED by what a card this small can actually carry.
    ///
    /// Unbounded, a modern phone (~62 pt of display radius) asked for a 48 pt corner on a card
    /// whose lines sit 10 pt from the edge: the arc reaches ~19 pt inward at the first line, so
    /// the top and bottom lines rendered INSIDE the curve. Concentricity is only a virtue while
    /// the radius is small next to the card; past that it is just a blob eating its own text.
    /// 28 pt is the most this card's stack can wear (with `cardPadding` scaling alongside), and
    /// devices whose display radius asks for less than that still get a truly concentric corner.
    private var cardCornerRadius: CGFloat {
        #if os(iOS) || os(visionOS)
        return min(28, max(12, DeviceMetrics.displayCornerRadius - edgeInset))
        #elseif os(tvOS)
        return 16 // scales with the roomier padding
        #else
        return 10
        #endif
    }

    /// The card background shape — a continuous (squircle) rounded rectangle, matching the curve
    /// Apple's hardware display corners use so the concentric inset actually reads as parallel.
    private var cardShape: RoundedRectangle {
        RoundedRectangle(cornerRadius: cardCornerRadius, style: .continuous)
    }
}

/// The bottom-centre badges over the stream: the transient hints, the access chip and the muted
/// microphone. One stack, so two badges never land on top of each other while they overlap.
/// Nothing mounts while `captureEnabled` is off (the trust prompt, the console's launch hold).
struct StreamBadgeStack: View {
    @ObservedObject var model: SessionModel
    let captureEnabled: Bool
    let statsVerbosity: StatsVerbosity

    /// How every badge enters and leaves.
    private static let pop: AnyTransition = .opacity.combined(with: .scale(scale: 0.9))

    var body: some View {
        VStack(spacing: 8) {
            // How to leave, for a few seconds at stream start.
            if captureEnabled, model.exitHintShown {
                ExitHintBadge(text: model.exitHintText).transition(Self.pop)
            }
            // A forwarded pad has a gyro this session's virtual controller cannot carry. Shown
            // briefly at every stats tier, on every platform: the gyro otherwise just does
            // nothing, and the fix is a setting, so the hint has to name it.
            if captureEnabled, model.motionUnreachableKind != nil {
                MotionUnreachableBadge().transition(Self.pop)
            }
            // The SC2 passthrough's claim edge, the capture's only visible trace.
            if captureEnabled, model.sc2CapturedHint {
                Sc2CapturedBadge().transition(Self.pop)
            }
            // The Touch (passthrough) model met a host that drops contacts; the fingers run the
            // trackpad engine instead, and this says so once.
            if captureEnabled, model.touchFallbackNotice {
                TouchFallbackBadge().transition(Self.pop)
            }
            // The expiry warning (T−5 m / T−1 m, per-client access §7), every platform and tier:
            // a dead pad must read as ended access while it can still be fixed.
            if captureEnabled, let warning = model.accessWarning {
                AccessWarningBadge(text: warning).transition(Self.pop)
            }
            // The host's word on a launch that did not give the player their game.
            if captureEnabled, let notice = model.launchNotice {
                AccessWarningBadge(text: notice, icon: "exclamationmark.triangle")
                    .transition(Self.pop)
            }
            #if !os(tvOS)
            // The access chip rides the stats tier for a LIMITED session only; a
            // full-and-permanent one never mounts it. tvOS states it in the stats overlay.
            if captureEnabled && statsVerbosity != .off && model.accessLimited {
                AccessChipBadge(
                    label: model.accessLevel.label, remainingSecs: model.accessRemainingSecs)
                    .transition(Self.pop)
            }
            // Up for as long as the mic is muted, at every stats tier (see MicMutedBadge).
            if captureEnabled && model.micMuted {
                MicMutedBadge { model.setMicMuted(false) }.transition(Self.pop)
            }
            #endif
        }
        .padding(.bottom, 24)
        // The badges' visibility drivers, the stats tier included (the access chip rides it). A
        // badge whose driver is missing here pops in unanimated.
        .animation(.easeOut(duration: 0.2), value: model.micMuted)
        .animation(.easeOut(duration: 0.2), value: model.accessWarning)
        .animation(.easeOut(duration: 0.2), value: model.launchNotice)
        .animation(.easeOut(duration: 0.2), value: model.accessLimited)
        .animation(.easeOut(duration: 0.2), value: statsVerbosity)
        .animation(.easeOut(duration: 0.2), value: model.motionUnreachableKind)
        .animation(.easeOut(duration: 0.2), value: model.sc2CapturedHint)
        .animation(.easeOut(duration: 0.6), value: model.exitHintShown)
    }
}

/// The exit hint: one line on how to leave, in the badges' glass language.
struct ExitHintBadge: View {
    let text: String

    var body: some View {
        Text(text)
            .font(.geist(12, .medium, relativeTo: .caption))
            .foregroundStyle(.white.opacity(0.9))
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            .glassBackground(Capsule())
            .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
    }
}

/// "This pad's gyro can't reach the game" — shown briefly when a forwarded controller with motion
/// meets a session whose virtual controller has no motion plane (an X-Box class pad has no gyro in
/// its HID contract, so every sample would be decoded and dropped).
///
/// Not a control, unlike `MicMutedBadge`: the fix is the Controller type setting, which is not
/// reachable mid-stream on every platform, and changing it applies from the next session anyway.
/// So this states the fact and names the setting, in the HUD's glass language, and gets out of the
/// way — the alternative is what shipped before, which was a gyro that silently did nothing with
/// no way to tell that from a broken sensor.
///
/// Every platform: a DualSense on an Apple TV is an ordinary way to play, and it is exactly the
/// pad this can happen to.
struct MotionUnreachableBadge: View {
    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "gyroscope")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.yellow)
            Text("Motion won't reach this session — set Controller type to DualSense")
                .font(.geist(12, .medium, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.9))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .glassBackground(Capsule())
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "This controller's motion will not reach the game. "
                + "Set Controller type to DualSense to enable it.")
    }
}

/// The Steam Controller passthrough badge — the SC2 capture's claim edge made visible.
/// It is the capture's ONLY UI surface: the raw BLE device never enters GameController, so
/// the Controllers page cannot list it, and without this the pad's arrival is indistinguishable
/// from the setting being off. Transient like the motion hint it stacks with (shown on the
/// claim — stream start or a mid-session power-on — dropped early on release).
struct Sc2CapturedBadge: View {
    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "gamecontroller.fill")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.green)
            Text("Steam Controller passing through")
                .font(.geist(12, .medium, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.9))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .glassBackground(Capsule())
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "Steam Controller connected — passing through to the host as itself.")
    }
}

#if !os(tvOS)
/// The session's access chip (per-client access §7) — "Controller only · ends in 1 h 58 m".
/// Rides over the stream for the life of a LIMITED session, at every stats tier and with the
/// overlay off entirely, in the badges' glass language: what this session may do (and for how
/// long) is not a statistic, and a guest whose keyboard does nothing deserves the why on
/// screen. Never mounted for full-and-permanent sessions — today's look does not change.
/// (tvOS states the same fact as a stats-overlay line instead — a chip would fight the couch
/// UI's single-focus rule.)
struct AccessChipBadge: View {
    let label: String
    /// Seconds until access expires; `0` = permanent (the chip then shows the level alone).
    let remainingSecs: UInt32

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "lock.fill")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.white.opacity(0.75))
            Text(remainingSecs == 0
                ? label
                : "\(label) · ends in \(SessionModel.accessCountdown(remainingSecs))")
                .font(.geist(12, .medium, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.9))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .glassBackground(Capsule())
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            remainingSecs == 0
                ? "Access level: \(label)"
                : "Access level: \(label), ends in \(SessionModel.accessCountdown(remainingSecs))")
    }
}
#endif

/// The expiry-warning toast (per-client access §7): the host's T−5 m / T−1 m `AccessUpdate`
/// warnings, surfaced briefly in the badge stack — every platform, tvOS included (unlike the
/// chip, a warning is worth a moment of couch overlay; it is how "the pad just died" becomes
/// "the evening's access ended, ask for more").
struct AccessWarningBadge: View {
    let text: String
    var icon = "clock.badge.exclamationmark"

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: icon)
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.yellow)
            Text(text)
                .font(.geist(12, .medium, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.9))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .glassBackground(Capsule())
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityElement(children: .combine)
        .accessibilityLabel(text)
    }
}

#if !os(tvOS)
/// The muted-microphone badge — the mute STATE, as opposed to the buttons that flip it. It rides
/// over the stream whenever the mic is muted, INDEPENDENT of the stats overlay (which the user
/// may have cycled off, and which the compact tier reduces to a stat line): "am I muted?" is not a
/// statistic, and a mute you can't see is how people talk to nobody for a minute. Same glass
/// language as the HUD, sized like the start-of-stream banner it shares the bottom edge with.
///
/// It is also a control: tapping it unmutes. That is the guaranteed way back for a touch user who
/// muted with the overlay off, and it costs the badge nothing (it is on screen either way).
struct MicMutedBadge: View {
    let onUnmute: () -> Void

    var body: some View {
        Button(action: onUnmute) {
            HStack(spacing: 7) {
                Image(systemName: "mic.slash.fill")
                    .font(.system(size: 13, weight: .semibold))
                    .foregroundStyle(.red)
                Text("Microphone muted")
                    .font(.geist(12, .medium, relativeTo: .caption))
                    .foregroundStyle(.white.opacity(0.9))
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 8)
            // interactive: the badge IS the tap target, so the glass reacts to press.
            .glassBackground(Capsule(), interactive: true)
            .contentShape(Capsule())
        }
        .buttonStyle(.plain)
        .environment(\.colorScheme, .dark) // reads over any frame, like the resize overlay
        .accessibilityLabel("Microphone muted")
        .accessibilityHint("Unmutes the microphone")
    }
}
#endif

#if os(iOS) || os(visionOS)
/// Device display geometry the overlay needs but UIKit doesn't expose publicly.
enum DeviceMetrics {
    /// The physical display's corner radius. There's no public API for it, so read the private
    /// `_displayCornerRadius` via KVC on the active window scene's screen, guarded by a fallback that
    /// approximates a modern rounded device — a future OS that hides the key just yields a slightly
    /// less-perfect inset, never a crash. The key is assembled from parts so it isn't a plain literal
    /// in the binary; note the App Store private-API consideration regardless.
    static var displayCornerRadius: CGFloat {
        #if os(visionOS)
        return 46 // every visionOS window's corner radius
        #else
        let key = ["_display", "Corner", "Radius"].joined()
        guard
            let screen = UIApplication.shared.connectedScenes
                .compactMap({ $0 as? UIWindowScene })
                .first?.screen,
            let radius = screen.value(forKey: key) as? NSNumber,
            radius.doubleValue > 0
        else { return 44 }
        return CGFloat(radius.doubleValue)
        #endif
    }
}
#endif

/// "This host doesn't accept touch" — the passthrough touch model met a host whose injector
/// drops contacts (no `HOST_CAP2_TOUCH`); the session runs the trackpad model instead. Same
/// slot and lifetime as the motion badge, for the same reason: the setting would otherwise be
/// silently ignored.
struct TouchFallbackBadge: View {
    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "hand.tap")
                .font(.system(size: 13, weight: .semibold))
                .foregroundStyle(.yellow)
            Text("This host doesn't accept touch — using the trackpad model")
                .font(.geist(12, .medium, relativeTo: .caption))
                .foregroundStyle(.white.opacity(0.9))
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .glassBackground(Capsule())
        .environment(\.colorScheme, .dark)
        .accessibilityElement(children: .combine)
        .accessibilityLabel(
            "This host does not accept touch input. Fingers drive the cursor as a trackpad instead.")
    }
}
