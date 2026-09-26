// SettingsView's shared sections — each setting's Section is defined exactly once here and
// composed by the per-platform bodies in SettingsView.swift.
//
// 2026-07 settings revamp: every field carries its explanation DIRECTLY under it in the same
// cell (the `described` helper in SettingsView+Support) — the old per-section footer paragraphs
// collected several fields' explanations into one blob nobody could match back to its row.
// Where a picker's meaning depends on the selection (touch mode, modifier layout, prioritize),
// the description is DYNAMIC — it explains the current choice. The only footers left are the
// one-line "Applies from the next session." form notes.
//
// The SAME builders edit settings presets (design/client-settings-profiles.md §5.1 —
// SettingsView+Scope): a control's binding comes from `scoped(...)` rather than `@AppStorage`, so
// it writes whichever layer the scope switcher selected, and `described(_:field:)` marks the row
// when the edited preset overrides it. Rows that are NOT presetable — tier G (this device's
// hardware and endpoints) and tier H (properties of a host) — are gated on `!inPresetScope` and
// simply don't render there; sections that would end up empty don't either.
//
// Category map (SettingsCategory): General = session/app behavior, Display = everything about
// the picture (resolution lives HERE), Input = touch/keyboard/mouse, Audio, Controllers, About.

#if os(iOS)
import CoreHaptics
#endif
import PunktfunkKit
import SwiftUI

extension SettingsView {
    /// The SC2 passthrough toggle's caption: one clause of what it does + one of what it costs
    /// (the caption rule). macOS says it differently because USB is the preferred transport
    /// there and its cost is the Input Monitoring grant, not Bluetooth — the controller
    /// interface carries the lizard keyboard collection, so macOS gates the open behind that
    /// permission. Only iOS names menus: a Mac or Apple TV reaches them through GameController,
    /// which never sees the pad on iOS.
    static var sc2CaptureCaption: String {
        #if os(macOS)
        return "Stream a Steam Controller 2 or Puck as-is; needs Input Monitoring "
            + "(or Bluetooth) access."
        #elseif os(tvOS)
        return "Stream a Steam Controller 2 as-is; needs Bluetooth access."
        #else
        return "Browse and stream with a Steam Controller 2 as-is; needs Bluetooth access."
        #endif
    }

    // MARK: - Display: Resolution

    // NOTE: the Section content is deliberately split into the small named builders below — as one
    // inline expression the iOS branch (wheel + 3-way refresh + bitrate rows) blew Swift's
    // type-checker budget ("unable to type-check this expression in reasonable time"), which
    // failed exactly one slice: the iOS archive (macOS/tvOS never compile that branch).
    @ViewBuilder var resolutionSection: some View {
        Section("Resolution") {
            #if os(iOS) || os(macOS)
            // Match-window (design/midstream-resolution-resize.md D1): follow the session
            // window/scene, renegotiating the host mode on a resize. Off → the explicit mode below.
            // NO marker here even though this toggle writes one: match-window, width and
            // height are ONE override (they are reset together), and hanging its marker off the
            // first of the two controls that drive it read as if the toggle alone were
            // overridden. It goes under the size control below, for the group.
            described(effective.matchWindow
                ? "The host follows this window's size — pixel-exact through every resize."
                : "Streams the fixed mode below, scaled to the window.") {
                Toggle("Match window", isOn: scoped(SettingsFields.matchWindow))
            }
            #endif
            #if os(iOS)
            iosResolutionWheel
            overrideMarker(OverlayField.resolution)
            iosRefreshRows
            Button("Use this display's mode") { fillFromMainScreen() }
            #elseif os(macOS)
            HStack {
                TextField(
                    "Resolution", value: scoped(SettingsFields.width),
                    format: .number.grouping(.never))
                Text("×")
                TextField("", value: scoped(SettingsFields.height), format: .number.grouping(.never))
                    .labelsHidden()
            }
            overrideMarker(OverlayField.resolution)
            described("The host drives a real output at exactly this size — no scaling.",
                field: "refresh_hz") {
                TextField(
                    "Refresh rate (Hz)", value: scoped(SettingsFields.refreshHz),
                    format: .number.grouping(.never))
            }
            LabeledContent("") {
                displayModeControl
            }
            #elseif os(tvOS)
            tvStreamModeRow
            #endif
        }
    }

    #if os(iOS)
    // MARK: - Display: Resolution (iOS wheel)

    /// Touch-first: an aspect switch over a rotating wheel of that family's common sizes (this
    /// device's own mode first) — the same family as the Clock/Timer pickers. The host renders a
    /// virtual output at exactly the chosen mode, so these are real pixel sizes. The last wheel
    /// row, "Custom…", reveals width/height/refresh fields for an arbitrary mode (see
    /// `iosRefreshRows`).
    @ViewBuilder private var iosResolutionWheel: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text("Aspect ratio")
                .font(.geist(15, relativeTo: .subheadline))
                .foregroundStyle(.secondary)
            Picker("Aspect ratio", selection: aspectSelection) {
                ForEach(Array(SettingsOptions.families().enumerated()), id: \.offset) { i, family in
                    Text(family.label).tag(i)
                }
            }
            .labelsHidden()
            .pickerStyle(.segmented)
            Text("Resolution")
                .font(.geist(15, relativeTo: .subheadline))
                .foregroundStyle(.secondary)
                .padding(.top, 8)
            Picker("Resolution", selection: resolutionSelection) {
                ForEach(resolutionChoices, id: \.tag) { choice in
                    Text(choice.label).tag(choice.tag)
                }
            }
            .labelsHidden()
            .pickerStyle(.wheel)
            .frame(maxHeight: 140)
            Text("The host drives a real output at exactly this mode — no scaling.")
                .font(.geist(13, relativeTo: .footnote))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .modifier(CaptionWidth()) // the same reading cap + control column as `described`
        }
    }

    /// Custom W×H(+Hz) fields, a segmented refresh picker, or a static single-rate row.
    @ViewBuilder private var iosRefreshRows: some View {
        if isCustomResolution {
            // Arbitrary entry: type the exact width × height (and refresh) the host should drive.
            HStack {
                TextField("Width", value: scoped(SettingsFields.width),
                          format: .number.grouping(.never))
                    .keyboardType(.numberPad)
                Text("×")
                TextField("Height", value: scoped(SettingsFields.height),
                          format: .number.grouping(.never))
                    .labelsHidden()
                    .keyboardType(.numberPad)
            }
            // A row built from an HStack of TextFields otherwise insets its bottom separator to
            // the inner content, clipping the hairline under "Width"; pin it to the cell edge.
            .alignmentGuide(.listRowSeparatorLeading) { _ in 0 }
            LabeledContent("Refresh rate") {
                TextField("Hz", value: scoped(SettingsFields.refreshHz),
                          format: .number.grouping(.never))
                    .keyboardType(.numberPad)
                    .multilineTextAlignment(.trailing)
            }
        } else if refreshChoices.count > 1 {
            VStack(alignment: .leading, spacing: 6) {
                Text("Refresh rate")
                    .font(.geist(15, relativeTo: .subheadline))
                    .foregroundStyle(.secondary)
                Picker("Refresh rate", selection: scoped(SettingsFields.refreshHz)) {
                    ForEach(refreshChoices, id: \.self) { rate in
                        Text("\(rate) Hz").tag(rate)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                overrideMarker("refresh_hz")
            }
        } else {
            // A device with a single supported rate (e.g. 60 Hz) has nothing to pick.
            LabeledContent("Refresh rate") {
                Text("\(effective.refreshHz) Hz").foregroundStyle(.secondary)
            }
        }
    }

    /// Sentinel wheel tag for the "Custom…" row. Real tags are "WxH" (digits + "x"), so this can't
    /// collide with a resolution.
    private static let customResolutionTag = "custom"

    /// The family the wheel lists — see `SettingsOptions.family`.
    private var family: Int {
        SettingsOptions.family(width: effective.width, height: effective.height)
    }

    /// The segmented switch: picking a family writes its size nearest the current height, so the
    /// wheel below always holds a row of that family.
    private var aspectSelection: Binding<Int> {
        Binding(
            get: { family },
            set: { i in
                customMode = false
                let mode = Resolutions.nearestIn(
                    SettingsOptions.families()[i], height: effective.height)
                setResolution(width: mode.w, height: mode.h)
            })
    }

    /// Wheel rows: the resolution modes (device native first — see `SettingsOptions`), then a
    /// "Custom…" row that reveals the numeric fields.
    private var resolutionChoices: [(label: String, tag: String)] {
        SettingsOptions.resolutionModes(family: family)
            .map {
                (label: $0.name.isEmpty ? "\($0.w) × \($0.h)" : "\($0.name)  ·  \($0.w) × \($0.h)",
                 tag: "\($0.w)x\($0.h)")
            }
            + [(label: "Custom…", tag: Self.customResolutionTag)]
    }

    private var presetResolutionTags: Set<String> {
        Set(SettingsOptions.resolutionModes(family: family).map { "\($0.w)x\($0.h)" })
    }

    /// True when the editable custom fields should show: the wheel is parked on "Custom…" (sticky),
    /// or the effective size simply isn't one of the presets (e.g. a value synced from a Mac, or a
    /// preset's own override) — so a non-preset mode stays editable without a persisted flag.
    private var isCustomResolution: Bool {
        customMode || !presetResolutionTags.contains("\(effective.width)x\(effective.height)")
    }

    /// The wheel works in "WxH" tags so one selection drives both width and height; the custom
    /// sentinel toggles `customMode` instead of writing a size.
    private var resolutionSelection: Binding<String> {
        Binding(
            get: {
                isCustomResolution
                    ? Self.customResolutionTag
                    : "\(effective.width)x\(effective.height)"
            },
            set: { tag in
                if tag == Self.customResolutionTag {
                    customMode = true
                    return
                }
                customMode = false
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 2 else { return }
                setResolution(width: parts[0], height: parts[1])
            })
    }

    /// Refresh rates this device can display, plus any stored custom value (see `SettingsOptions`).
    private var refreshChoices: [Int] {
        SettingsOptions.refreshRates(including: effective.refreshHz)
    }
    #endif

    #if os(tvOS)
    // MARK: - Display: Stream mode (tvOS)

    /// A TV picks size and rate together, as its own settings do: this TV's mode, the common
    /// ones, and whatever is stored today.
    private var tvStreamModeRow: some View {
        described("The host drives a real output at exactly this mode — no scaling.",
                  field: OverlayField.resolution) {
            TVSelectionRow(title: "Stream mode", options: tvModeOptions, selection: tvModeTag)
        }
    }

    private static let tvModes: [(label: String, tag: String)] = [
        ("720p @ 60", "1280x720x60"),
        ("1080p @ 60", "1920x1080x60"),
        ("4K @ 60", "3840x2160x60"),
    ]

    private var tvModeOptions: [(label: String, tag: String)] {
        let s = effective
        let current = "\(s.width)x\(s.height)x\(s.refreshHz)"
        let bounds = UIScreen.main.nativeBounds
        let native = "\(Int(max(bounds.width, bounds.height)))x"
            + "\(Int(min(bounds.width, bounds.height)))x\(UIScreen.main.maximumFramesPerSecond)"
        var options = Self.tvModes
        if !options.contains(where: { $0.tag == native }) {
            options.insert(("This TV (native)", native), at: 0)
        }
        if !options.contains(where: { $0.tag == current }) {
            options.insert(("Custom (\(s.width)×\(s.height) @ \(s.refreshHz))", current), at: 0)
        }
        return options
    }

    /// Size and rate in one write, through the scoped setters, so a preset records both.
    private var tvModeTag: Binding<String> {
        Binding(
            get: { "\(effective.width)x\(effective.height)x\(effective.refreshHz)" },
            set: { tag in
                let parts = tag.split(separator: "x").compactMap { Int($0) }
                guard parts.count == 3 else { return }
                setResolution(width: parts[0], height: parts[1])
                scoped(SettingsFields.refreshHz).wrappedValue = parts[2]
            })
    }
    #endif

    // MARK: - Display: Quality

    @ViewBuilder var qualitySection: some View {
        Section("Quality") {
            renderScaleRow
            described("When the stream's shape differs from this screen. Fit shows the whole "
                + "picture with black bars, Crop to fill cuts the edges off, Stretch to fill "
                + "distorts it.", field: "video_fit") {
                settingPicker(
                    "Picture fit",
                    options: VideoFit.allCases.map { (label: $0.label, tag: $0.rawValue) },
                    selection: scoped(SettingsFields.videoFit))
            }
            #if os(tvOS)
            tvBitrateRow
            #else
            bitrateRows
            #endif
            described("A preference — the host falls back if it can't encode it.",
                      field: "codec") {
                settingPicker(
                    "Video codec",
                    options: SettingsOptions.codecs(current: scoped(SettingsFields.codec).wrappedValue),
                    selection: scoped(SettingsFields.codec))
            }
            described("HDR10 when the host sends it and this display supports it. Not with H.264.",
                field: "hdr_enabled") {
                Toggle("10-bit HDR", isOn: scoped(SettingsFields.hdrEnabled))
            }
            described("Sharper text and UI, at more bandwidth. For desktop work; HEVC only.",
                field: "enable_444") {
                Toggle("Full chroma (4:4:4)", isOn: scoped(SettingsFields.enable444))
            }
            described("Main10 for an SDR desktop, which costs a little bandwidth and takes the "
                + "banding out of gradients. 10-bit HDR already asks for the depth.",
                field: "ten_bit_sdr") {
                Toggle("10-bit SDR", isOn: scoped(SettingsFields.tenBitSdr))
            }
        }
    }

    /// Render-scale picker + the resulting host resolution. > 1 supersamples (sharper, at more
    /// bandwidth AND client decode); < 1 renders under native (lighter). The presenter resamples the
    /// decoded frame to this display, so the multiplier is where the sharpness/cost trade-off lives.
    @ViewBuilder var renderScaleRow: some View {
        described(renderScaleDescription, field: "render_scale") {
            settingPicker(
                "Render scale",
                options: RenderScale.presets.map { (label: RenderScale.label($0), tag: $0) },
                selection: scoped(SettingsFields.renderScale))
        }
    }

    /// Render scale explained, with the CONCRETE host resolution when it applies — the cost made
    /// legible. Only the explicit mode can show it (match-window derives the base from the live
    /// window, not these fields).
    private var renderScaleDescription: String {
        var text = "Above native is sharper, below is lighter on the host and link."
        let settings = effective
        if settings.renderScale != 1.0, !settings.matchWindow {
            let mode = RenderScale.apply(
                baseWidth: settings.width, baseHeight: settings.height,
                scale: settings.renderScale,
                maxDimension: RenderScale.maxDimension(codec: settings.codec))
            text += " Host renders \(Int(mode.width))×\(Int(mode.height))."
        }
        return text
    }

    #if !os(tvOS)
    /// The automatic-bitrate toggle + manual slider (and the >1 Gbps warning) rows.
    @ViewBuilder private var bitrateRows: some View {
        // PyroWave is always Automatic (ABR overhaul RFC §5.2): the session sends 0 and the
        // host pins a per-mode rate, so a live rate control here would change nothing. Same
        // support gate as the codec picker offering the option; the stored rate is untouched,
        // so switching the codec back restores it.
        if effective.codec == "pyrowave", MetalWaveletDecoder.supported {
            described("PyroWave sets its own rate from the stream mode — a fixed bitrate "
                + "doesn't apply.",
                field: "bitrate_kbps") {
                Toggle("Automatic bitrate", isOn: .constant(true))
                    .disabled(true)
            }
        } else {
            described("Uses the host's default, 20 Mbps. Off to set it yourself.",
                field: "bitrate_kbps") {
                Toggle("Automatic bitrate", isOn: automaticBitrate)
            }
        }
        if effective.codec != "pyrowave" || !MetalWaveletDecoder.supported,
           effective.bitrateKbps != 0 {
            HStack(spacing: 12) {
                Slider(value: bitrateSlider, in: 0...1) {
                    Text("Bitrate")
                }
                Text(SpeedTestView.mbpsLabel(kbps: effective.bitrateKbps))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
                    .frame(minWidth: 76, alignment: .trailing)
            }
            if effective.bitrateKbps > 1_000_000 {
                Label(Self.gigabitWarning, systemImage: "exclamationmark.triangle.fill")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.orange)
            }
        }
    }
    #else
    /// The TV's bitrate: a list of steps plus a typed rate, where the touch and desktop forms have
    /// a switch and a slider. PyroWave sets its own rate, so it shows none.
    @ViewBuilder private var tvBitrateRow: some View {
        if effective.codec == "pyrowave", MetalWaveletDecoder.supported {
            described("PyroWave sets its own rate from the stream mode — a fixed bitrate "
                + "doesn't apply.", field: "bitrate_kbps") {
                LabeledContent("Bitrate", value: "Automatic")
            }
        } else {
            described(effective.bitrateKbps > 1_000_000
                ? Self.gigabitWarning : "Automatic uses the host's default, 20 Mbps.",
                field: "bitrate_kbps") {
                TVSelectionRow(
                    title: "Bitrate",
                    options: SettingsOptions.bitrateOptions(current: effective.bitrateKbps),
                    selection: scoped(SettingsFields.bitrateKbps))
            }
            described("Any fixed rate, in Mbps.", field: "bitrate_kbps") {
                TVFieldRow(
                    label: "Custom bitrate",
                    value: SettingsOptions.isCustomBitrate(effective.bitrateKbps)
                        ? SpeedTestView.mbpsLabel(kbps: effective.bitrateKbps) : "",
                    placeholder: "Type a rate"
                ) { typingBitrate = true }
                .fullScreenCover(isPresented: $typingBitrate) {
                    TVTextEntry(title: "Bitrate (Mbps)", text: "", keyboardType: .numberPad) {
                        if let kbps = SettingsOptions.customBitrateKbps($0) {
                            scoped(SettingsFields.bitrateKbps).wrappedValue = kbps
                        }
                        typingBitrate = false
                    }
                }
            }
        }
    }
    #endif

    // MARK: - Display: Presentation

    // The presentation intent (design/apple-presentation-rebuild.md — replaced the visible
    // stage picker): latency (newest-wins, zero queue) vs smoothness (a small deliberate jitter
    // buffer). The stage ladder survives only as the hidden PUNKTFUNK_PRESENTER debug env lever.
    @ViewBuilder var presentationSection: some View {
        Section("Presentation") {
            described(effective.presentPriority == "smooth"
                ? "A small buffer evens out network hiccups, at its worth of added latency."
                : "Frames show as soon as they arrive; a hiccup repeats or skips one.",
                field: "present_priority") {
                settingPicker(
                    "Prioritize",
                    options: SettingsOptions.presentPriorities.map { option in
                        (label: option.tag == SettingsOptions.presentPriorityDefault
                            ? "\(option.label) (default)" : option.label, tag: option.tag)
                    },
                    selection: scoped(SettingsFields.presentPriority))
            }
            if effective.presentPriority == "smooth" {
                described("Each frame costs one refresh of latency and absorbs one of jitter.",
                    field: "smooth_buffer") {
                    settingPicker(
                        "Buffer",
                        options: SettingsOptions.smoothBuffers(refreshHz: effective.refreshHz),
                        selection: scoped(SettingsFields.smoothBuffer))
                }
            }
            // Non-tvOS: the Apple TV drives a fixed HDMI mode, so there's no adaptive refresh.
            #if !os(tvOS)
            described("A ProMotion or adaptive-sync display follows the stream's rate — "
                + "smoother motion.", field: "allow_vrr") {
                Toggle("Allow VRR", isOn: scoped(SettingsFields.allowVRR))
            }
            #endif
            // macOS-only: iOS/tvOS layers always present on the display's vsync, so the choice
            // only exists on the Mac (the layer's own sync stays off — see MetalVideoPresenter).
            #if os(macOS)
            described("Even pacing, at up to one refresh of added latency.", field: "vsync") {
                Toggle("V-Sync", isOn: scoped(SettingsFields.vsync))
            }
            // The DCP swapID-panic mitigation's user handle (see DefaultsKey.windowedSafePresent
            // for the saga). Default ON: turning it off re-arms a WHOLE-MACHINE kernel panic on
            // affected setups, so the caption says so in plain words.
            described(effective.windowedSafePresent
                ? "Windowed streams present in step with the compositor — avoids a macOS "
                    + "display-driver crash, at a small latency cost."
                : "Windowed streams use the fastest path. On some high-refresh Macs this can "
                    + "kernel-panic the machine.", field: "windowed_safe_present") {
                Toggle("Safe windowed presentation", isOn: scoped(SettingsFields.windowedSafePresent))
            }
            #endif
        }
    }

    // MARK: - Display: Host output

    @ViewBuilder var hostOutputSection: some View {
        Section {
            described("The backend the host drives its virtual output with — honored only if "
                + "available.",
                field: "compositor") {
                settingPicker(
                    "Compositor", options: SettingsOptions.compositors,
                    selection: scoped(SettingsFields.compositor))
            }
        } header: {
            Text("Host output")
        } footer: {
            // The one form-level note (deliberately not repeated on every row above).
            Text("Display changes apply from the next session.")
                .settingsFooter()
        }
    }

    // MARK: - General: Session

    /// Empty in preset scope everywhere but macOS: auto-wake is a property of the host and this
    /// network, and background keep-alive is a property of this device — neither is something
    /// "Game" and "Work" would ever differ on (§3, tiers H and G).
    private var showsSessionSection: Bool {
        #if os(macOS)
        return true
        #else
        return !inPresetScope
        #endif
    }

    @ViewBuilder var sessionSection: some View {
        if showsSessionSection {
            Section("Session") {
                #if os(macOS)
                described("Go fullscreen when a session starts; return to a window on the host "
                    + "list.", field: "fullscreen_on_stream") {
                    Toggle(
                        "Fullscreen while streaming",
                        isOn: scoped(SettingsFields.fullscreenWhileStreaming))
                }
                #endif
                if !inPresetScope {
                    described("Sends Wake-on-LAN to a sleeping saved host and waits for it.") {
                        Toggle("Auto-wake on connect", isOn: $autoWakeEnabled)
                    }
                }
                #if os(iOS) || os(tvOS)
                if !inPresetScope {
                    described("Audio and the connection stay live when you switch away; video "
                        + "pauses.") {
                        Toggle("Keep streaming in background", isOn: $backgroundKeepAlive)
                    }
                    if backgroundKeepAlive {
                        described(Self.backgroundTimeoutCaption) {
                            settingPicker(
                                "Disconnect after",
                                options: [("1 minute", 1), ("5 minutes", 5), ("10 minutes", 10),
                                          ("30 minutes", 30)],
                                selection: $backgroundTimeoutMinutes)
                        }
                    }
                }
                #endif
            }
        }
    }

    /// A phone's reason is its battery; a TV's is only not holding a host it has left.
    private static var backgroundTimeoutCaption: String {
        #if os(tvOS)
        "Ends a session left in the background after this long."
        #else
        "Ends a backgrounded session so it can't run down the battery."
        #endif
    }

    private static var gamepadUIModeCaption: String {
        #if os(tvOS)
        "Always keeps it up — otherwise this screen returns when the last controller disconnects."
        #else
        "Always keeps it up — otherwise touch returns when the last controller disconnects."
        #endif
    }

    // MARK: - General: Statistics overlay

    @ViewBuilder var overlaySection: some View {
        Section("Statistics") {
            described(Self.statisticsDescription, field: "stats_verbosity") {
                settingPicker(
                    "Statistics overlay", options: SettingsOptions.statsVerbosities,
                    selection: scoped(SettingsFields.statsVerbosity))
            }
            // Which corner the overlay sits in is a property of this device's screen, not of a
            // preset (tier G).
            if !inPresetScope {
                settingPicker(
                    "Position", options: SettingsOptions.hudPlacements, selection: $hudPlacement)
                    .disabled(effective.statsVerbosity == StatsVerbosity.off.rawValue)
                // The vocabulary is this device's choice, never a preset's (tier G).
                described(Self.advancedStatisticsDescription) {
                    Toggle("Advanced statistics", isOn: $advancedStats)
                }
                #if !os(tvOS)
                Link("What each number means", destination: Self.statsDocsURL)
                    .foregroundStyle(Color.brand) // a Link takes the system accent, not the tint
                #endif
            }
        }
    }

    // MARK: - General: Library

    @ViewBuilder var librarySection: some View {
        // An app-level feature switch for this device (tier G) — the whole section collapses in
        // preset scope rather than rendering an empty group.
        if !inPresetScope {
            Section("Library") {
                described("How the controller-optimized library arranges titles: Shelf is one "
                    + "row of covers, Grid shows more at once.") {
                    settingPicker(
                        "Library view",
                        options: LibraryArrangement.all.map { (label: $0.label, tag: $0.stored) },
                        selection: $libraryViewRaw)
                }
                described(startInFooter) {
                    settingPicker(
                        "Start in",
                        options: StartIn.allCases.map { (label: $0.label, tag: $0.stored) },
                        selection: $startInRaw)
                }
            }
        }
    }

    /// Names the host the setting resolves to, so the row explains itself — and says when it
    /// resolves to nothing, which is what every value does with no default host.
    private var startInFooter: String {
        let hosts = StartScreen.savedHosts()
        guard let host = StartScreen.defaultHost(id: defaultHostID, hosts: hosts).host else {
            return "Opens on the host list: there is no default host. Pair one, or pick one "
                + "from a host's menu when several are paired."
        }
        return "Library opens \(host.displayName)'s games; Stream also connects to its desktop. "
            + "Back leaves either one on the host list."
    }

    // MARK: - Input

    #if os(iOS)
    /// Touch-input model (iPhone + iPad) plus the iPad-only pointer-capture toggle: lock the
    /// mouse/trackpad for relative movement (games) vs forward an absolute cursor position.
    @ViewBuilder var pointerSection: some View {
        Section("Touch & pointer") {
            described(touchModeDescription, field: "touch_mode") {
                Picker("Touch input", selection: scoped(SettingsFields.touchMode)) {
                    Text("Trackpad").tag(TouchInputMode.trackpad.rawValue)
                    Text("Direct pointer").tag(TouchInputMode.pointer.rawValue)
                    Text("Touch passthrough").tag(TouchInputMode.touch.rawValue)
                    Text("Off").tag(TouchInputMode.off.rawValue)
                }
            }
            quickActionsRow
            // Whether a hardware mouse attached to THIS iPad gets locked is a fact about this
            // device's input hardware (tier G), not about how a host is streamed.
            if !inPresetScope, UIDevice.current.userInterfaceIdiom == .pad {
                described("Locks a hardware mouse for mouse-look. Needs the stream "
                    + "fullscreen.") {
                    Toggle("Capture pointer for games", isOn: $pointerCapture)
                }
            }
        }
    }

    /// The SELECTED touch mode explained — dynamic, so the caption always describes what the
    /// picker currently does instead of narrating every mode at once.
    private var touchModeDescription: String {
        switch TouchInputMode(rawValue: effective.touchMode) ?? .trackpad {
        case .trackpad:
            // The one caption where length is earned: this is a gesture reference, not an
            // explanation. Only the two gestures a trackpad user would not guess are listed.
            return "Drives the host cursor like a trackpad — two-finger tap right-clicks, "
                + "two-finger drag scrolls."
        case .pointer:
            return "The host cursor jumps to wherever you touch."
        case .touch:
            return "Real multi-touch reaches the host."
        case .off:
            return "Touches on the picture don't reach the host. On-screen controls still work."
        }
    }
    #endif

    #if os(iOS) || os(macOS)
    /// The in-stream quick-action ring's editor, opened as a sheet. Every platform with a ring
    /// the user can change: iOS from Touch & pointer, macOS from Keyboard & mouse (the Mac opens
    /// the ring with ⌃⌥⇧O or the Stream menu, so that is where a reader looks for it).
    /// How to reach the dial on THIS platform. The editor must name its opener: six buttons
    /// nobody can summon are six buttons nobody sees.
    private var dialOpener: String {
        #if os(macOS)
        "⌃⌥⇧O, the Stream menu or Select + A on a controller opens it mid-stream. "
        #else
        "A two-finger twist, or Select + A on a controller, opens it mid-stream. "
        #endif
    }

    @ViewBuilder var quickActionsRow: some View {
        described(dialOpener
                  + "Which actions the in-stream dial offers and the shortcuts it can send; "
                  + "a preset that changes it owns the whole dial.", field: "overlay_actions") {
            // A SHEET, not a push: the detail column is not a NavigationStack, and a
            // NavigationLink pushed from it popped the collapsed iPhone stack to the category
            // list on the way back and left the selection dead (AboutView's rows say the same).
            Button {
                showQuickActions = true
            } label: {
                HStack {
                    Text("Quick actions")
                    Spacer(minLength: 8)
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(.tertiary)
                        .accessibilityHidden(true)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .sheet(isPresented: $showQuickActions) {
                NavigationStack {
                    QuickActionsEditor(blob: scoped(SettingsFields.overlayActions),
                                       overridden: isOverridden("overlay_actions")) {
                        if inPresetScope {
                            resetOverride("overlay_actions")
                        } else {
                            scoped(SettingsFields.overlayActions).wrappedValue = ""
                        }
                    }
                    .toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { showQuickActions = false }
                        }
                    }
                }
                #if os(macOS)
                // A settings-window sheet has no size of its own; the editor is a ring at full
                // size over a stage, so give it one. Inside the 500 × 668 preferences window, not
                // over it — a sheet larger than its parent hangs off the edges.
                .frame(width: 460, height: 540)
                #endif
            }
        }
    }
    #endif

    #if !os(tvOS)
    /// Keyboard & mouse forwarding — applies wherever a hardware keyboard/mouse drives the stream
    /// (always on macOS; an attached keyboard/mouse on iPad). Absent on tvOS (no such input path).
    @ViewBuilder var inputSection: some View {
        Section("Keyboard & mouse") {
            #if os(macOS)
            described(mouseModeDescription, field: "mouse_mode") {
                Picker("Mouse input", selection: scoped(SettingsFields.mouseMode)) {
                    Text("Capture (games)").tag(MouseInputMode.capture.rawValue)
                    Text("Desktop (absolute)").tag(MouseInputMode.desktop.rawValue)
                }
            }
            described(
                "Sends ⌘ shortcuts — ⌘Space, ⌘Tab and Mission Control included — to the host while "
                    + "captured. ⌘⎋ always stays local — it releases capture.",
                field: "inhibit_shortcuts"
            ) {
                Toggle("Capture system shortcuts", isOn: scoped(SettingsFields.inhibitShortcuts))
            }
            quickActionsRow
            #endif
            described(
                (ModifierLayout(rawValue: effective.modifierLayout) ?? .mac).detail,
                field: "modifier_layout"
            ) {
                Picker("Modifier keys", selection: scoped(SettingsFields.modifierLayout)) {
                    ForEach(ModifierLayout.allCases, id: \.self) { layout in
                        Text(layout.label).tag(layout.rawValue)
                    }
                }
            }
            described("Reverses the wheel and trackpad scroll direction sent to the host.",
                      field: "invert_scroll") {
                Toggle("Invert scroll direction", isOn: scoped(SettingsFields.invertScroll))
            }
        }
    }

    #if os(macOS)
    /// The SELECTED mouse model explained — dynamic, like the touch-mode caption.
    private var mouseModeDescription: String {
        switch MouseInputMode(rawValue: effective.mouseMode) ?? .capture {
        case .capture:
            return "Locks the pointer and sends relative motion — best for games. ⌃⌥⇧M "
                + "switches live."
        case .desktop:
            return "The pointer moves freely and sends absolute positions — best for desktop "
                + "work."
        }
    }
    #endif
    #endif

    // MARK: - Audio

    @ViewBuilder var audioSection: some View {
        Section {
            described("The speaker layout requested from the host.", field: "audio_channels") {
                settingPicker(
                    "Audio channels", options: SettingsOptions.audioChannels,
                    selection: scoped(SettingsFields.audioChannels))
            }
            // Offered at every channel count. This row used to be hidden unless the session was
            // stereo; the frame ladder is channel-aware, so surround negotiates a shorter frame
            // rather than being impossible, and the caption is where the cases that genuinely do
            // not fit are stated (see `audioFormatCaption`). A hidden row silently discards a
            // choice and explains nothing.
            described(audioFormatCaption, field: "audio_format") {
                settingPicker(
                    "Audio quality", options: SettingsOptions.audioFormats,
                    selection: scoped(SettingsFields.audioFormat))
            }
            described("The host's speakers or headphones keep playing while you stream — "
                      + "needs a host on 0.32+",
                      field: "keep_host_audio") {
                Toggle("Keep host audio playing", isOn: scoped(SettingsFields.keepHostAudio))
            }
            #if os(macOS)
            // Which speaker THIS Mac plays through is this device's audio routing (tier G).
            if !inPresetScope {
                described("Where host audio plays on this Mac.") {
                    Picker("Speaker", selection: $speakerUID) {
                        Text("System default").tag("")
                        ForEach(outputDevices) { device in
                            Text(device.name).tag(device.uid)
                        }
                        if !speakerUID.isEmpty,
                           !outputDevices.contains(where: { $0.uid == speakerUID }) {
                            Text("Unavailable device").tag(speakerUID)
                        }
                    }
                }
            }
            #endif
            // An Apple TV has no microphone an app can open.
            #if !os(tvOS)
            described("This device's microphone feeds the host's virtual mic.",
                      field: "mic_enabled") {
                Toggle("Send microphone to the host", isOn: scoped(SettingsFields.micEnabled))
            }
            described(echoCancelCaption, field: "echo_cancel") {
                Toggle("Echo cancellation", isOn: scoped(SettingsFields.echoCancel))
                    .disabled(!effective.micEnabled)
            }
            #endif
            #if os(macOS)
            if !inPresetScope {
                Picker("Microphone", selection: $micUID) {
                    Text("System default").tag("")
                    ForEach(inputDevices) { device in
                        Text(device.name).tag(device.uid)
                    }
                    if !micUID.isEmpty,
                       !inputDevices.contains(where: { $0.uid == micUID }) {
                        Text("Unavailable device").tag(micUID)
                    }
                }
                .disabled(!effective.micEnabled)
                // Multi-channel interfaces only: the mic sits on ONE discrete input, so let the
                // user pick it. Auto sums every channel (a lone hot mic still passes at full
                // level).
                if micChannelCount > 1 {
                    described("Pick the input your mic is on; Auto sums every channel.") {
                        Picker("Microphone channel", selection: $micChannel) {
                            Text("Auto (all channels)").tag(0)
                            ForEach(1...micChannelCount, id: \.self) { ch in
                                Text("Channel \(ch)").tag(ch)
                            }
                        }
                        .disabled(!effective.micEnabled)
                    }
                }
            }
            #endif
        } header: {
            Text("Audio")
        } footer: {
            // The lossless gates live HERE, once, rather than as a rider on each of the five
            // lossless rows. This sentence is what keeps `audioFormatCaption` honest — see its
            // doc comment: the picker must never read as a promise of the resolved format.
            Text("Applies from the next session. Lossless falls back to Standard if the host or "
                + "this device's output declines it.")
                .settingsFooter()
        }
    }

    /// The SELECTED audio format: what it is, and what it costs on the wire. That is the whole
    /// per-row caption. The ways a request can come to nothing — the host's own switch (off by
    /// default), its capture gate, this device's output being unable to open the rate — are stated
    /// ONCE in the section footer instead of riding all five lossless rows.
    ///
    /// ⚠ The rule this caption exists to keep is the design's: **the UI states the RESOLVED
    /// format, never the requested one.** Nothing here may read as a guarantee — the stats
    /// overlay's audio line is built from the connection's `Welcome`, and that is the only place a
    /// format is asserted as fact. The footer's "falls back to Standard" line is what carries that
    /// now that the per-row gate riders are gone; do not drop it without replacing it.
    private var audioFormatCaption: String {
        let choice = AudioFormatChoice(setting: effective.audioFormat)
        guard choice != .opus else {
            return "Compressed 256 kbps Opus — effectively transparent."
        }
        // Stereo cost at 24-bit, from `pcm::bitrate_kbps`. 5.1 is three times it and 7.1 four, so
        // the surround rider below states the multiplier rather than repeating the table.
        let mbps: String
        switch choice {
        case .opus: mbps = "" // unreachable — the guard above returns
        case .lossless441: mbps = "2.1"
        case .lossless48: mbps = "2.3"
        case .lossless882: mbps = "4.2"
        case .lossless96: mbps = "4.6"
        case .lossless1764: mbps = "8.5"
        }
        let head = "Bit-exact PCM — about \(mbps) Mbps on top of the video."
        guard effective.audioChannels > 2 else { return head }
        // Surround is offered at every rate, so what it costs has to be said out loud. The plane
        // sends one frame per datagram and never fragments, so more channels buy a SHORTER frame
        // rather than a bigger packet — a packet rate, not an impossibility, right up until no rung
        // on the ladder fits at all. Where that line falls, at 24-bit and the default datagram size
        // (`pcm::frame_us_for` against ~1 387 B of payload): 44.1 kHz 5.1/7.1 and 48 kHz 7.1 land
        // on the 1 ms rung, 48 kHz 5.1 on 1.5 ms, and 88.2 kHz upward fit NOTHING. Two different
        // sentences, because "it will cost you" and "it will not happen" are two different things
        // to tell someone.
        guard choice == .lossless441 || choice == .lossless48 else {
            return head + " Surround needs 48 kHz or lower."
        }
        return head + (effective.audioChannels == 6
            ? " Three times that on 5.1."
            : " Four times that on 7.1.")
    }

    /// Honest about the macOS escape hatch: the voice processor only follows the system
    /// default devices, so hand-picked endpoints silently keep the raw path (see
    /// SessionAudio's topology note) — better said here than discovered mid-call.
    private var echoCancelCaption: String {
        let base = "Filters the stream's own audio out of the mic pickup."
        #if os(macOS)
        return base + " Only on the system default devices."
        #else
        return base
        #endif
    }

    // MARK: - Controllers

    @ViewBuilder var controllersSection: some View {
        Section {
            // The master switch, above everything it governs. Presetable, so it renders in
            // both scopes: a "Work" preset can decline to forward what "Game" forwards.
            described("Sends this device's controllers to the host. Off if they already reach it "
                + "another way.",
                field: "gamepad_forwarding") {
                Toggle("Forward controllers", isOn: scoped(SettingsFields.gamepadForwarding))
            }
            // Which physical pad this device forwards, and what its own haptics do, are facts
            // about THIS device (tier G) — only the virtual pad the host creates is presetable.
            if !inPresetScope {
                if gamepads.controllers.isEmpty {
                    Text("No controllers detected")
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(gamepads.controllers) { controller in
                        controllerRow(controller)
                    }
                }
                described("Which pad is player 1. Automatic picks the newest connection.") {
                    settingPicker(
                        "Use controller", options: controllerOptions,
                        selection: $gamepads.preferredID)
                        .disabled(!effective.gamepadForwarding)
                }
                // Steam Controller 2 as-is passthrough — device tier like the pad rows above
                // (EffectiveSettings.sc2Capture is not presetable). The capture engages at the
                // next stream; the in-stream badge announces it. Caption: see sc2CaptureCaption.
                described(Self.sc2CaptureCaption) {
                    Toggle("Steam Controller 2 passthrough", isOn: $sc2Capture)
                        .disabled(!effective.gamepadForwarding)
                }
            }
            described("The virtual pad the host creates — Automatic matches your controller.",
                field: "gamepad") {
                settingPicker(
                    "Controller type", options: SettingsOptions.padTypes,
                    selection: scoped(SettingsFields.gamepadType))
                    .disabled(!effective.gamepadForwarding)
            }
            described("Where guide and share presses go while streaming.",
                field: "system_buttons") {
                settingPicker(
                    "Guide button", options: SettingsOptions.systemButtons,
                    selection: scoped(SettingsFields.systemButtons))
                    .disabled(!effective.gamepadForwarding)
            }
            described("Hold Select for the host's guide button; keep holding for its "
                + "quick-access menu.",
                field: "guide_gesture") {
                settingPicker(
                    "Hold Select for guide", options: SettingsOptions.guideGestures,
                    selection: scoped(SettingsFields.guideGesture))
                    .disabled(!effective.gamepadForwarding)
            }
            #if os(iOS)
            // iPhone only in practice: hidden where the device itself can't play haptics (iPad).
            if !inPresetScope, CHHapticEngine.capabilitiesForHardware().supportsHaptics {
                described("Plays player 1's rumble on the phone itself — for pads without "
                    + "motors.") {
                    Toggle("Rumble on this iPhone", isOn: $rumbleOnDevice)
                }
            }
            // The rumble mirror's sibling, data flowing the other way: hidden where the
            // device has no motion hardware, engages only while the player-1 controller
            // reports no rotation rate of its own.
            if !inPresetScope, DeviceGyro.isAvailable {
                described("Sends this device's motion as player 1's when the controller has no "
                    + "gyro.") {
                    Toggle("Gyro from this device", isOn: $gyroFromDevice)
                }
            }
            #endif
            if !inPresetScope {
                described("A controller-friendly layout for the host list and library.") {
                    Toggle("Gamepad-optimized browsing", isOn: $gamepadUIEnabled)
                }
                // Only meaningful while the switch above is on, so it is HIDDEN rather than
                // disabled when it isn't: a picker whose every option decides nothing is worse
                // than no picker, and this Section is short enough that nothing jumps far.
                if gamepadUIEnabled {
                    described(Self.gamepadUIModeCaption) {
                        settingPicker(
                            "Show it", options: SettingsOptions.gamepadUIModes,
                            selection: $gamepadUIMode)
                    }
                    #if os(tvOS)
                    // A TV's only route to `ui_palette`: the gamepad settings that carry it
                    // elsewhere need a controller to open.
                    described("The background of the controller-optimized screens.") {
                        settingPicker(
                            "Background",
                            options: ConsoleBridge.palettes.map { (label: $0.name, tag: $0.id) },
                            selection: $uiPalette)
                    }
                    #endif
                }
            }
            #if DEBUG && !os(tvOS)
            if !inPresetScope {
                Button("Test Controller…") { showControllerTest = true }
                    .disabled(gamepads.active == nil)
                    .sheet(isPresented: $showControllerTest) { ControllerTestView() }
            }
            #endif
        } header: {
            Text("Controllers")
        } footer: {
            Text("Applies from the next session.")
                .settingsFooter()
        }
    }
}
