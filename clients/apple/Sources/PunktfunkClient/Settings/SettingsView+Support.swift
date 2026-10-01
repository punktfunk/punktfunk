// SettingsView's footers and stateful helpers, used by both the section builders
// (SettingsView+Sections.swift) and the per-platform bodies (SettingsView.swift). The option
// LISTS live in SettingsOptions — they're shared with the gamepad settings screen too.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

/// How wide a `described` row's caption may run. TEXT only — the override marker's Reset is a
/// control and belongs on the row's trailing edge, not in the caption's column.
///
/// Two limits, because they solve different problems. The 360pt CAP is about reading: past roughly
/// 46 characters a line stops scanning well, and on a wide Mac window or an iPad detail pane an
/// uncapped caption runs the full cell. The trailing INSET is about collision: an iOS grouped row
/// puts its switch or value in from the trailing edge, and a caption laid out to the full cell
/// width runs its last line straight under that control — which is what the cap alone never
/// fixed, because an iPhone cell is narrower than the cap in the first place.
struct CaptionWidth: ViewModifier {
    #if os(iOS)
    /// Reserve the control column. A `UISwitch` is 51pt, and the rest is breathing room — the
    /// caption should stop visibly short of the control, not graze it.
    private static let trailingInset: CGFloat = 76
    #elseif os(visionOS)
    /// A visionOS picker is a capsule as wide as its value (~230 pt for the modifier keys) and
    /// taller than its label line, so a caption must clear the whole capsule.
    private static let trailingInset: CGFloat = 240
    #else
    // macOS lays the control out inline and its cells are wider than the cap; nothing to clear.
    private static let trailingInset: CGFloat = 0
    #endif
    #if os(visionOS)
    /// The cap holds the inset too, so visionOS grows it by its wider column: 284pt of text, as
    /// on iOS.
    private static let cap: CGFloat = 524
    #else
    private static let cap: CGFloat = 360
    #endif

    func body(content: Content) -> some View {
        content
            // Order matters: the padding shrinks what the text is offered, THEN the cap applies —
            // so a narrow phone cell reserves the control column and a wide pane still caps.
            .padding(.trailing, Self.trailingInset)
            .frame(maxWidth: Self.cap, alignment: .leading)
    }
}

extension SettingsView {
    // MARK: - Described rows (the 2026-07 revamp's field idiom)

    /// A control with its explanation attached to the SAME cell: the field, then a tight caption
    /// directly under it. This replaced the per-section footer paragraphs — a description the eye
    /// can't match to its field is one nobody reads. Keep captions to one or two sentences; when
    /// a picker's meaning depends on the selection, pass a DYNAMIC string describing the current
    /// choice.
    /// `field` is the overlay's name for this row (see `SettingsField`). Passing it puts the
    /// override marker + Reset in the caption line while a preset is being edited — with the row
    /// it belongs to, which is the only place the state is legible. On a TV the caption goes to
    /// the pane's band instead (`SettingsCaptionBand`).
    func described<Content: View>(
        _ caption: String, field: String? = nil, @ViewBuilder content: () -> Content
    ) -> some View {
        // A name the overlay does not model silently loses both the marker and its Reset, with no
        // compile error to say so — the one-way door the marker exists to prevent. Caught here in
        // debug rather than by noticing a missing badge.
        assert(field.map { OverlayField.isModelled($0) } ?? true,
               "described(field:) got \(field ?? "") — not a field SettingsOverlay models")
        #if os(tvOS)
        // A TV row carries no caption of its own: its focus names the one the pane's band shows,
        // and an overridden row wears a dot and resets from its context menu.
        let overridden = field.map(isOverridden) ?? false
        return content()
            .modifier(TVOverrideMark(overridden: overridden) {
                if let field { resetOverride(field) }
            })
            .focused($tvCaption, equals: SettingsCaption(text: caption, overridden: overridden))
        #else
        return VStack(alignment: .leading, spacing: 5) {
            content()
            Text(caption)
                .font(.geist(13, relativeTo: .footnote))
                .foregroundStyle(.secondary)
                // Wrap, never truncate, in Form cells.
                .fixedSize(horizontal: false, vertical: true)
                .modifier(CaptionWidth())
            if let field {
                // Full cell width, deliberately: the marker's Reset is a CONTROL, and it belongs
                // on the same trailing edge as the row's own control above it. Sharing the
                // caption's reserved column left it stranded mid-row.
                overrideMarker(field)
            }
        }
        .padding(.vertical, 2)
        #endif
    }

    /// One picker row for every platform: the system `Picker`, or on tvOS the pushed selection
    /// list (`Picker`'s own push draws its rows in the focused style while it animates).
    @ViewBuilder
    func settingPicker<Tag: Hashable>(
        _ title: String, options: [(label: String, tag: Tag)], selection: Binding<Tag>
    ) -> some View {
        #if os(tvOS)
        TVSelectionRow(title: title, options: options, selection: selection)
        #else
        Picker(title, selection: selection) {
            ForEach(options, id: \.tag) { option in
                Text(option.label).tag(option.tag)
            }
        }
        #endif
    }

    // MARK: - Bitrate

    /// Slider domain, log-scale: the useful range spans three orders of magnitude
    /// (a few Mbps … 3 Gbps) — linear would cram everything below 100 Mbps into the
    /// first pixels.
    private static let minSliderKbps = 2_000.0
    private static let maxSliderKbps = 3_000_000.0

    static let gigabitWarning =
        "Above 1 Gbps — more than the link sustains causes loss and stutter. Speed-test first."

    /// `bitrateKbps == 0` is Automatic; switching to manual lands on the host default. Scoped, so
    /// flipping it in a preset records the override there rather than moving the global.
    var automaticBitrate: Binding<Bool> {
        let bitrate = scoped(SettingsFields.bitrateKbps)
        return Binding(
            get: { bitrate.wrappedValue == 0 },
            set: { bitrate.wrappedValue = $0 ? 0 : 20_000 })
    }

    /// Slider position 0...1 ↔ kbps on the log scale, snapped to two significant figures
    /// so the readout shows round numbers instead of 47_322.
    var bitrateSlider: Binding<Double> {
        let bitrate = scoped(SettingsFields.bitrateKbps)
        return Binding(
            get: {
                let v = min(max(Double(bitrate.wrappedValue), Self.minSliderKbps),
                            Self.maxSliderKbps)
                return log(v / Self.minSliderKbps)
                    / log(Self.maxSliderKbps / Self.minSliderKbps)
            },
            set: { pos in
                let raw = Self.minSliderKbps
                    * pow(Self.maxSliderKbps / Self.minSliderKbps, pos)
                let mag = pow(10, floor(log10(raw)) - 1)
                bitrate.wrappedValue = Int((raw / mag).rounded() * mag)
            })
    }

    // MARK: - Statistics

    static var statisticsDescription: String {
        // No "Live session stats in a corner overlay" preamble: it restates the row's own label.
        let base = "Compact is a one-line pill; Detailed adds the latency breakdown."
        #if os(macOS)
        return base + " ⌃⌥⇧S cycles it."
        #elseif os(iOS) || os(visionOS)
        return base + " ⌃⌥⇧S or a three-finger tap cycles it."
        #else
        return base
        #endif
    }

    static let advancedStatisticsDescription =
        "Off shows the figures Moonlight's overlay also shows. On shows capture to glass as "
            + "p50/p95 and every stage between."

    static let statsDocsURL = URL(string: "https://docs.punktfunk.unom.io/docs/stats")!

    // MARK: - Controllers

    /// "Use controller" choices for this view's manager (see `SettingsOptions.controllerOptions`).
    var controllerOptions: [(label: String, tag: String)] {
        SettingsOptions.controllerOptions(gamepads)
    }

    func controllerRow(_ controller: GamepadManager.DiscoveredController) -> some View {
        HStack(spacing: 10) {
            Image(systemName: controller.hasTouchpadAndMotion ? "playstation.logo" : "gamecontroller.fill")
                .foregroundStyle(.secondary)
            VStack(alignment: .leading, spacing: 2) {
                Text(controller.name)
                HStack(spacing: 8) {
                    if !controller.isExtended {
                        Text(controller.productCategory)
                    }
                    if controller.hasAdaptiveTriggers {
                        Image(systemName: "r2.button.roundedtop.horizontal")
                    }
                    if controller.hasLight {
                        Image(systemName: "lightbulb.fill")
                    }
                    if controller.hasMotion {
                        Image(systemName: "gyroscope")
                    }
                    if controller.hasHaptics {
                        Image(systemName: "waveform")
                    }
                    if let level = controller.batteryLevel {
                        Text("\(Int(level * 100))%")
                        if controller.isCharging {
                            Image(systemName: "bolt.fill")
                        }
                    }
                }
                .font(.geist(11, relativeTo: .caption2))
                .foregroundStyle(.secondary)
            }
            Spacer()
            // Every forwarded controller is surfaced (not just the primary `active`) with its
            // wire pad index as a player number — a pin forwards only one, Automatic forwards all.
            if let pad = gamepads.padIndex(for: controller) {
                Text("Player \(pad + 1)")
                    .font(.geist(11, .semibold, relativeTo: .caption2))
                    .padding(.horizontal, 8)
                    .padding(.vertical, 3)
                    .background(Capsule().fill(.green.opacity(0.2)))
                    .foregroundStyle(.green)
            }
        }
    }

    /// Fill the mode fields from the screen this app is on. On a Mac that is the PANEL, never the
    /// framebuffer a scaled mode renders into — see `SettingsOptions.macDisplayModes`.
    func fillFromMainScreen() {
        #if os(macOS)
        guard let panel = SettingsOptions.macDisplayModes().first else { return }
        applyDisplayMode(panel)
        customMode = false
        #elseif os(visionOS)
        let native = NativeDisplay.mode
        setResolution(width: native.width, height: native.height)
        scoped(SettingsFields.refreshHz).wrappedValue = native.hz
        customMode = false
        #else
        // nativeBounds is portrait-oriented pixels — streams are landscape.
        let bounds = UIScreen.main.nativeBounds
        setResolution(
            width: Int(max(bounds.width, bounds.height)),
            height: Int(min(bounds.width, bounds.height)))
        scoped(SettingsFields.refreshHz).wrappedValue = UIScreen.main.maximumFramesPerSecond
        #if os(iOS)
        // The native mode is the "This device" wheel row, so leave Custom mode if it was on.
        customMode = false
        #endif
        #endif
    }

    #if os(macOS)
    /// Write one of `SettingsOptions.macDisplayModes()` into the mode fields, at the screen's top
    /// rate.
    func applyDisplayMode(_ mode: (name: String, w: Int, h: Int)) {
        customMode = false
        setResolution(width: mode.w, height: mode.h)
        scoped(SettingsFields.refreshHz).wrappedValue = NSScreen.main?.maximumFramesPerSecond ?? 60
    }

    /// "Use this display's mode" — a menu on a Mac with a camera housing, where the panel and the
    /// area a full-screen stream can show whole are different sizes and only the viewer knows which
    /// they want. Every other Mac has one answer and keeps the plain button.
    @ViewBuilder var displayModeControl: some View {
        let modes = SettingsOptions.macDisplayModes()
        if modes.count > 1 {
            Menu("Use this display's mode") {
                ForEach(modes, id: \.name) { mode in
                    Button("\(mode.name) · \(mode.w) × \(mode.h)") { applyDisplayMode(mode) }
                }
            }
            .fixedSize()
        } else {
            Button("Use this display's mode") { fillFromMainScreen() }
        }
    }
    #endif
}

extension View {
    /// A settings footer: caption-sized in hand; near body size and brighter on a TV, where 12 pt
    /// can't be read from the couch.
    func settingsFooter() -> some View {
        #if os(tvOS)
        font(.geist(26, relativeTo: .body)).foregroundStyle(Color.primary.opacity(0.7))
        #else
        font(.geist(12, relativeTo: .caption)).foregroundStyle(.secondary)
        #endif
    }
}

#if os(tvOS)
/// A settings row's caption and whether the edited preset overrides it: the value a row's focus
/// binds in `described`, for the band under the rows.
struct SettingsCaption: Hashable {
    let text: String
    let overridden: Bool
}

/// The caption of whichever row has focus, in one place under the rows: per-row text does not
/// scale to 10-foot type, and a caption per cluster can't be matched to its row.
struct SettingsCaptionBand: View {
    let caption: SettingsCaption?

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if caption?.overridden == true {
                Label("Overrides Default settings — hold to reset", systemImage: "circle.fill")
                    .foregroundStyle(Color.brand)
            }
            Text(caption?.text ?? "")
                .foregroundStyle(Color.primary.opacity(0.8))
                .lineLimit(3, reservesSpace: true)
        }
        // Near body size and near white, to read from the couch.
        .font(.geist(28, relativeTo: .body))
        .frame(maxWidth: .infinity, alignment: .leading)
        .padding(.vertical, 20)
        .animation(.easeOut(duration: 0.15), value: caption)
    }
}

/// An overridden row's dot, in the row's leading padding, and the long-press reset.
struct TVOverrideMark: ViewModifier {
    let overridden: Bool
    let reset: () -> Void

    @ViewBuilder func body(content: Content) -> some View {
        if overridden {
            content
                .overlay(alignment: .leading) {
                    // In the platter's padding, left of the text (the row's frame starts at
                    // the text); past the platter the list clips it.
                    Circle()
                        .fill(Color.brand)
                        .frame(width: 10, height: 10)
                        .offset(x: -15)
                        .accessibilityHidden(true)
                }
                .contextMenu {
                    Button("Reset to Default settings", systemImage: "arrow.uturn.backward",
                           action: reset)
                }
        } else {
            content
        }
    }
}

/// A TV sidebar row: bare until focused, when it lifts on a white platter as a system row does.
/// The chosen row keeps a faint platter, so the open pane stays marked while focus is in it.
struct TVSidebarRowStyle: ButtonStyle {
    let chosen: Bool

    func makeBody(configuration: Configuration) -> some View {
        Row(label: configuration.label, pressed: configuration.isPressed, chosen: chosen)
    }

    private struct Row: View {
        let label: ButtonStyleConfiguration.Label
        let pressed: Bool
        let chosen: Bool
        @Environment(\.isFocused) private var focused

        var body: some View {
            label
                .padding(.horizontal, 24)
                .padding(.vertical, 10)
                .frame(maxWidth: .infinity, minHeight: 66, alignment: .leading)
                .foregroundStyle(focused ? Color.black : Color.primary)
                .background(
                    focused ? Color.white : Color.primary.opacity(chosen ? 0.1 : 0),
                    in: RoundedRectangle(cornerRadius: 16, style: .continuous))
                .scaleEffect(focused ? 1.04 : 1)
                .shadow(color: .black.opacity(focused ? 0.35 : 0), radius: 16, y: 8)
                .opacity(pressed ? 0.85 : 1)
                .animation(.easeOut(duration: 0.15), value: focused)
        }
    }
}

extension View {
    /// The card a TV sidebar's rows sit on, so they read as navigation beside the fields.
    func tvSidebarCard() -> some View {
        padding(12)
            .background(
                Color.primary.opacity(0.07),
                in: RoundedRectangle(cornerRadius: 28, style: .continuous))
    }

    /// Room past a pane's sides for a focused row's lift and shadow: the pane's frame, where its
    /// list clips, grows 40 pt a side, and the safe area puts the rows back. Layout, not an
    /// environment value, so a list pushed from the pane keeps its own clip.
    func tvPaneRoom() -> some View {
        safeAreaPadding(.horizontal, 40).padding(.horizontal, -40)
    }
}
#endif
