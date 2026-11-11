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

#if os(iOS) || os(visionOS)
import CoreHaptics
#endif
import GameController
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
        return String(localized: L10n.applePlayerSc2CaptureMacHint)
        #elseif os(tvOS)
        return String(localized: L10n.playerSc2CaptureHint)
        #else
        return String(localized: L10n.applePlayerSc2CaptureIosHint)
        #endif
    }

    // MARK: - Display: Resolution

    // NOTE: the Section content is deliberately split into the small named builders below — as one
    // inline expression the iOS branch (wheel + 3-way refresh + bitrate rows) blew Swift's
    // type-checker budget ("unable to type-check this expression in reasonable time"), which
    // failed exactly one slice: the iOS archive (macOS/tvOS never compile that branch).
    @ViewBuilder var resolutionSection: some View {
        Section(L10n.prefsGroupResolution) {
            #if os(iOS) || os(visionOS) || os(macOS)
            // Match window follows the window, renegotiating the host mode on a resize; off, the
            // mode below. Its marker sits under the size control: the toggle, width and height
            // are one override.
            described(effective.matchWindow
                ? L10n.appleMatchWindowOnHint
                : L10n.appleMatchWindowOffHint) {
                Toggle(L10n.appleMatchWindow, isOn: scoped(SettingsFields.matchWindow))
            }
            #endif
            #if os(iOS) || os(visionOS)
            iosResolutionWheel
            overrideMarker([OverlayField.resolution])
            iosRefreshRows
            Button(L10n.appleUseDisplayMode) { fillFromMainScreen() }
            #elseif os(macOS)
            macResolutionRows
            #elseif os(tvOS)
            tvStreamModeRow
            tvCustomSizeRow
            #endif
        }
    }

    #if os(iOS) || os(visionOS)
    // MARK: - Display: Resolution (iOS wheel)

    /// Touch-first: wrapping aspect chips over a rotating wheel of that family's common sizes
    /// (this device's own mode first) — the same family as the Clock/Timer pickers. The host
    /// renders a virtual output at exactly the chosen mode, so these are real pixel sizes. The
    /// last wheel row, "Custom…", reveals width/height/refresh fields for an arbitrary mode (see
    /// `iosRefreshRows`).
    @ViewBuilder private var iosResolutionWheel: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(L10n.playerResolutionAspect)
                .font(.geist(15, relativeTo: .subheadline))
                .foregroundStyle(.secondary)
            WrapLayout(spacing: 8) {
                ForEach(Array(SettingsOptions.families().enumerated()), id: \.offset) { i, family in
                    aspectChip(family.label, index: i)
                }
            }
            .padding(.vertical, 2)
            Text(L10n.playerResolution)
                .font(.geist(15, relativeTo: .subheadline))
                .foregroundStyle(.secondary)
                .padding(.top, 8)
            Picker(L10n.playerResolution, selection: resolutionSelection) {
                ForEach(resolutionChoices, id: \.tag) { choice in
                    Text(choice.label).tag(choice.tag)
                }
            }
            .labelsHidden()
            .pickerStyle(.wheel)
            .frame(maxHeight: 140)
            Text(resolutionCaption)
                .font(.geist(13, relativeTo: .footnote))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
                .modifier(CaptionWidth()) // the same reading cap + control column as `described`
        }
    }

    /// One aspect family as a capsule, in the Library shelf chips' style. Plain style, so a tap in
    /// the list row hits only this chip.
    private func aspectChip(_ label: String, index: Int) -> some View {
        let on = index == family
        return Button { aspectSelection.wrappedValue = index } label: {
            Text(label)
                .font(.geist(13, .semibold, relativeTo: .subheadline))
                .lineLimit(1)
                .foregroundStyle(on ? Color.white : Color.primary)
                .padding(.horizontal, 12)
                .padding(.vertical, 6)
                .background(Capsule().fill(on ? AnyShapeStyle(Color.brand) : AnyShapeStyle(.regularMaterial)))
                .overlay { if !on { Capsule().strokeBorder(.quaternary, lineWidth: 1) } }
        }
        .buttonStyle(.plain)
        #if os(visionOS)
        .cardHover(Capsule())
        #endif
        .accessibilityAddTraits(on ? .isSelected : [])
    }

    /// Custom W×H(+Hz) fields, a segmented refresh picker, or a static single-rate row.
    @ViewBuilder private var iosRefreshRows: some View {
        if isCustomResolution {
            // Arbitrary entry: type the exact width × height (and refresh) the host should drive.
            HStack {
                TextField(L10n.playerResolutionWidth, value: customSide(width: true),
                          format: .number.grouping(.never))
                    .keyboardType(.numberPad)
                Text("×")
                TextField(L10n.playerResolutionHeight, value: customSide(width: false),
                          format: .number.grouping(.never))
                    .labelsHidden()
                    .keyboardType(.numberPad)
            }
            // A row built from an HStack of TextFields otherwise insets its bottom separator to
            // the inner content, clipping the hairline under "Width"; pin it to the cell edge.
            .alignmentGuide(.listRowSeparatorLeading) { _ in 0 }
            LabeledContent(String(localized: L10n.playerRefreshHz)) {
                TextField("Hz", value: scoped(SettingsFields.refreshHz),
                          format: .number.grouping(.never))
                    .keyboardType(.numberPad)
                    .multilineTextAlignment(.trailing)
            }
        } else if refreshChoices.count > 1 {
            VStack(alignment: .leading, spacing: 6) {
                Text(L10n.playerRefreshHz)
                    .font(.geist(15, relativeTo: .subheadline))
                    .foregroundStyle(.secondary)
                Picker(L10n.playerRefreshHz, selection: scoped(SettingsFields.refreshHz)) {
                    ForEach(refreshChoices, id: \.self) { rate in
                        Text(SettingsOptions.refreshLabel(rate)).tag(rate)
                    }
                }
                .labelsHidden()
                .pickerStyle(.segmented)
                overrideMarker(["refresh_hz"])
            }
        } else {
            // A device with a single supported rate (e.g. 60 Hz) has nothing to pick.
            LabeledContent(String(localized: L10n.playerRefreshHz)) {
                Text(SettingsOptions.refreshLabel(effective.refreshHz))
                    .foregroundStyle(.secondary)
            }
        }
    }
    #endif

    #if os(macOS)
    /// The desktop layout, as on Linux and Windows: the aspect switch over Native, this display's
    /// notch-safe mode, the family's sizes and Custom…, which shows the typed width and height.
    @ViewBuilder private var macResolutionRows: some View {
        Picker(L10n.playerResolutionAspect, selection: aspectSelection) {
            ForEach(Array(SettingsOptions.families().enumerated()), id: \.offset) { i, family in
                Text(family.label).tag(i)
            }
        }
        described(L10n.playerResolutionHint, field: OverlayField.resolution) {
            Picker(L10n.playerResolution, selection: resolutionSelection) {
                ForEach(resolutionChoices, id: \.tag) { choice in
                    Text(choice.label).tag(choice.tag)
                }
            }
        }
        if isCustomResolution {
            HStack {
                TextField(L10n.playerResolutionWidth, value: customSide(width: true),
                          format: .number.grouping(.never))
                Text("×")
                TextField(L10n.playerResolutionHeight, value: customSide(width: false),
                          format: .number.grouping(.never))
                    .labelsHidden()
            }
        }
        described(L10n.playerRefreshHzHint, field: "refresh_hz") {
            Picker(L10n.playerRefreshHz, selection: scoped(SettingsFields.refreshHz)) {
                ForEach(refreshChoices, id: \.self) { rate in
                    Text(SettingsOptions.refreshLabel(rate)).tag(rate)
                }
            }
        }
        LabeledContent("") {
            displayModeControl
        }
    }
    #endif

    #if os(iOS) || os(visionOS) || os(macOS)
    /// A typed side through the shared rule, applied when the field commits (Return or focus
    /// loss), so a half-typed number is never clamped under the thumb.
    private func customSide(width: Bool) -> Binding<Int> {
        Binding(
            get: { width ? effective.width : effective.height },
            set: { typed in
                let s = effective
                let size = Resolutions.custom(
                    width ? typed : s.width, width ? s.height : typed, codec: s.codec)
                setResolution(width: size.w, height: size.h)
            })
    }

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

    /// The caption under the wheel. The one mode that covers a Mac's camera housing names its
    /// cost: that fullscreen is composited, never scanned out Direct.
    private var resolutionCaption: String {
        #if os(macOS)
        if let screen = NSScreen.main, screen.safeAreaInsets.top > 0,
           SafeDisplay.coversHousing(
               content: (effective.width, effective.height),
               panel: screen.panelPixelSize, safe: screen.notchSafePixelSize) {
            return String(localized: L10n.appleResolutionNotchHint)
        }
        #endif
        return String(localized: L10n.playerResolutionHint)
    }

    /// The list: Native (`0x0`, which follows the display), this device's other native mode, the
    /// family's sizes (see `SettingsOptions.resolutionModes`), then "Custom…", which reveals the
    /// numeric fields.
    private var resolutionChoices: [(label: String, tag: String)] {
        SettingsOptions.resolutionModes(family: family)
            .map {
                (label: SettingsOptions.modeLabel($0.kind, w: $0.w, h: $0.h),
                 tag: ResolutionPick.tag(width: $0.w, height: $0.h))
            }
            + [(label: String(localized: L10n.playerResolutionCustom), tag: ResolutionPick.customTag)]
    }

    private var presetResolutionTags: Set<String> {
        Set(SettingsOptions.resolutionModes(family: family).map {
            ResolutionPick.tag(width: $0.w, height: $0.h)
        })
    }

    /// The typed fields show (`ResolutionPick.isCustom`).
    private var isCustomResolution: Bool {
        ResolutionPick.isCustom(
            customMode: customMode, width: effective.width, height: effective.height,
            tags: presetResolutionTags)
    }

    /// One selection drives width and height (`ResolutionPick.apply`).
    private var resolutionSelection: Binding<String> {
        Binding(
            get: {
                isCustomResolution
                    ? ResolutionPick.customTag
                    : ResolutionPick.tag(width: effective.width, height: effective.height)
            },
            set: { tag in
                let pick = ResolutionPick.apply(tag: tag, currentWidth: effective.width)
                customMode = pick.customMode
                if let size = pick.size { setResolution(width: size.w, height: size.h) }
            })
    }

    /// Refresh rates this device can display, plus any stored custom value (see `SettingsOptions`).
    private var refreshChoices: [Int] {
        SettingsOptions.refreshRates(including: effective.refreshHz)
    }
    #endif

    #if os(tvOS)
    // MARK: - Display: Stream mode (tvOS)

    /// A TV picks size and rate together, as its own settings do: Native, the common modes, and
    /// whatever is stored today.
    private var tvStreamModeRow: some View {
        described(String(localized: L10n.playerResolutionHint), fields: ResolutionPick.modeFields) {
            TVSelectionRow(
                title: String(localized: L10n.appleStreamMode), options: tvModeOptions,
                selection: tvModeTag)
        }
    }

    /// Any size, typed on the system keyboard as "width × height", at the stored rate.
    private var tvCustomSizeRow: some View {
        described(L10n.appleCustomSizeHint, field: OverlayField.resolution) {
            TVFieldRow(
                label: String(localized: L10n.playerResolutionCustomRow),
                value: Self.tvModes.contains { $0.tag == tvModeTag.wrappedValue }
                    ? "" : "\(effective.width) × \(effective.height)",
                placeholder: String(localized: L10n.appleCustomSizePlaceholder)
            ) { typingSize = true }
            .fullScreenCover(isPresented: $typingSize) {
                TVTextEntry(
                    title: String(localized: L10n.appleCustomSizeEntry), text: "",
                    keyboardType: .numbersAndPunctuation
                ) {
                    if let size = SettingsOptions.typedSize($0, codec: effective.codec) {
                        setResolution(width: size.w, height: size.h)
                    }
                    typingSize = false
                }
            }
        }
    }

    /// `0x0x0` is Native: this TV's own mode, resolved at connect.
    private static let tvModes: [(label: String, tag: String)] = [
        (String(localized: L10n.appleStreamModeNative), "0x0x0"),
        ("720p @ 60", "1280x720x60"),
        ("1080p @ 60", "1920x1080x60"),
        ("4K @ 60", "3840x2160x60"),
    ]

    private var tvModeOptions: [(label: String, tag: String)] {
        let s = effective
        let current = ResolutionPick.modeTag(width: s.width, height: s.height, hz: s.refreshHz)
        var options = Self.tvModes
        if !options.contains(where: { $0.tag == current }) {
            options.insert((Self.customModeLabel(s), current), at: 0)
        }
        return options
    }

    /// A stored mode none of `tvModes` holds. Native size and Native rate each have their own
    /// sentence, so no word is spliced into another message.
    private static func customModeLabel(_ s: EffectiveSettings) -> String {
        switch (s.width == 0, s.refreshHz == 0) {
        case (true, true):
            return String(localized: L10n.appleStreamModeNative)
        case (true, false):
            return L10n.appleStreamModeCustomNativeSize(hz: String(s.refreshHz))
        case (false, true):
            return L10n.appleStreamModeCustomNativeRate(
                width: String(s.width), height: String(s.height))
        case (false, false):
            return L10n.appleStreamModeCustom(
                width: String(s.width), height: String(s.height), hz: String(s.refreshHz))
        }
    }

    /// Size and rate in one write, through the scoped setters, so a preset records both.
    private var tvModeTag: Binding<String> {
        Binding(
            get: {
                ResolutionPick.modeTag(
                    width: effective.width, height: effective.height, hz: effective.refreshHz)
            },
            set: { tag in
                guard let mode = ResolutionPick.mode(tag: tag) else { return }
                setResolution(width: mode.w, height: mode.h)
                scoped(SettingsFields.refreshHz).wrappedValue = mode.hz
            })
    }
    #endif

    // MARK: - Display: Picture

    @ViewBuilder var pictureSection: some View {
        Section {
            described(L10n.playerVideoFitHint, field: "video_fit") {
                settingPicker(
                    L10n.playerVideoFit,
                    options: VideoFit.allCases.map { (label: $0.label, tag: $0.rawValue) },
                    selection: scoped(SettingsFields.videoFit))
            }
            #if os(macOS)
            described(L10n.playerScreensHint, field: "screens") {
                settingPicker(
                    L10n.playerScreens,
                    options: [
                        (label: String(localized: L10n.playerScreensThis), tag: "this"),
                        (label: String(localized: L10n.playerScreensAll), tag: "all"),
                    ],
                    selection: scoped(SettingsFields.screens))
            }
            #endif
            #if os(visionOS)
            // How this headset bends the picture (tier G). An older stored name shows as its stop.
            if !inPresetScope {
                described(L10n.playerScreenHint) {
                    settingPicker(
                        L10n.playerScreen,
                        options: ScreenCurve.allCases.map { (label: $0.label, tag: $0.rawValue) },
                        selection: Binding(
                            get: { ScreenCurve(name: screen).rawValue }, set: { screen = $0 }))
                }
            }
            #endif
            #if os(tvOS)
            tvBitrateRow
            #else
            bitrateRows
            #endif
            described(L10n.playerHdrEnabledHint, field: "hdr_enabled") {
                Toggle(L10n.playerHdrEnabled, isOn: scoped(SettingsFields.hdrEnabled))
            }
            prioritizeRow
            described(L10n.playerCodecHint, field: "codec") {
                settingPicker(
                    L10n.playerCodec,
                    options: SettingsOptions.codecs(current: scoped(SettingsFields.codec).wrappedValue),
                    selection: scoped(SettingsFields.codec))
            }
            pyrowaveQualityRows
        } header: {
            Text(L10n.prefsGroupPicture)
        } footer: {
            // The one form-level note (deliberately not repeated on every row above).
            Text(L10n.prefsAppliesNextSession)
                .settingsFooter()
        }
    }

    // MARK: - Display: Advanced

    @ViewBuilder var displayAdvancedSection: some View {
        advancedSection(changed: displayChanged, fields: Self.displayAdvancedFields) {
            if effective.presentPriority == "smooth" {
                described(L10n.playerSmoothBufferHint, field: "smooth_buffer") {
                    settingPicker(
                        L10n.playerSmoothBuffer,
                        options: SettingsOptions.smoothBuffers(
                            refreshHz: Int(effective.streamMode(native: connectNative).hz)),
                        selection: scoped(SettingsFields.smoothBuffer))
                }
            }
            renderScaleRow
            pictureDepthRows
            presentationRows
            described(L10n.playerCompositorHint, field: "compositor") {
                settingPicker(
                    L10n.playerCompositor, options: SettingsOptions.compositors,
                    selection: scoped(SettingsFields.compositor))
            }
            // This device's screen, so never a preset's.
            if !inPresetScope {
                described(L10n.playerKeepAwakeHint) {
                    Toggle(L10n.playerKeepAwake, isOn: $keepAwake)
                }
            }
        }
    }

    private static let displayAdvancedFields = [
        "smooth_buffer", "render_scale", "enable_444", "ten_bit_sdr", "vsync", "allow_vrr",
        "compositor",
    ]

    /// Advanced display rows away from their defaults, for the collapsed section's count.
    private var displayChanged: Int {
        let e = effective, d = EffectiveSettings()
        var changed = [
            e.presentPriority == "smooth" && e.smoothBuffer != d.smoothBuffer,
            e.renderScale != d.renderScale, e.enable444 != d.enable444,
            e.tenBitSdr != d.tenBitSdr, e.compositor != d.compositor, !keepAwake,
        ]
        #if os(macOS)
        changed.append(e.vsync != d.vsync)
        #endif
        #if !os(tvOS)
        changed.append(e.allowVRR != d.allowVRR)
        #endif
        return changed.filter { $0 }.count
    }

    @ViewBuilder private var pictureDepthRows: some View {
        described(L10n.playerEnable444Hint, field: "enable_444") {
            Toggle(L10n.playerEnable444, isOn: scoped(SettingsFields.enable444))
        }
        described(L10n.playerTenBitSdrHint, field: "ten_bit_sdr") {
            Toggle(L10n.playerTenBitSdr, isOn: scoped(SettingsFields.tenBitSdr))
        }
    }

    /// Render-scale picker + the resulting host resolution. > 1 supersamples (sharper, at more host
    /// work; a host that cannot scale the picture down also costs bandwidth and client decode);
    /// < 1 renders under native (lighter), and the presenter upscales.
    /// Any percent from 50 to 400 can be typed: "Custom…" shows the field, a TV has its own row.
    @ViewBuilder var renderScaleRow: some View {
        #if os(tvOS)
        described(renderScaleDescription, field: "render_scale") {
            TVSelectionRow(
                title: String(localized: L10n.playerRenderScale), options: tvScaleOptions,
                selection: scoped(SettingsFields.renderScale))
        }
        described(L10n.appleCustomScaleHint, field: "render_scale") {
            TVFieldRow(
                label: String(localized: L10n.appleCustomScale),
                value: RenderScale.presetIndex(effective.renderScale) == nil
                    ? L10n.commonPercent(value: String(RenderScale.percent(effective.renderScale)))
                    : "",
                placeholder: String(localized: L10n.appleCustomScalePlaceholder)
            ) { typingScale = true }
            .fullScreenCover(isPresented: $typingScale) {
                TVTextEntry(
                    title: String(localized: L10n.playerRenderScaleEntry), text: "",
                    keyboardType: .numberPad
                ) {
                    if let percent = Int($0), percent > 0 {
                        scoped(SettingsFields.renderScale).wrappedValue =
                            RenderScale.fromPercent(percent)
                    }
                    typingScale = false
                }
            }
        }
        #else
        described(renderScaleDescription, field: "render_scale") {
            settingPicker(
                L10n.playerRenderScale,
                options: RenderScale.presets.map { (label: RenderScale.label($0), tag: $0) }
                    + [(label: String(localized: L10n.playerRenderScaleCustom),
                        tag: Self.customScaleTag)],
                selection: renderScaleSelection)
        }
        if isCustomScale {
            TextField(
                L10n.playerRenderScalePercent, value: customScalePercent,
                format: .number.grouping(.never))
        }
        #endif
    }

    #if os(tvOS)
    /// The stops, plus a typed scale that is none of them, so the row always reads what is stored.
    private var tvScaleOptions: [(label: String, tag: Double)] {
        var options = RenderScale.presets.map { (label: RenderScale.label($0), tag: $0) }
        let stored = effective.renderScale
        if RenderScale.presetIndex(stored) == nil {
            options.append((
                label: L10n.playerRenderScaleCustomValue(value: RenderScale.label(stored)),
                tag: stored))
        }
        return options
    }
    #else
    /// Sentinel tag for the picker's "Custom…" row: no multiplier is negative.
    private static let customScaleTag = -1.0

    /// True when the percent field should show: the picker is parked on "Custom…" (sticky), or
    /// the stored multiplier is none of the stops (typed here, or on another client).
    private var isCustomScale: Bool {
        customScaleMode || RenderScale.presetIndex(effective.renderScale) == nil
    }

    /// The picker works in multiplier tags; the sentinel toggles `customScaleMode` and leaves the
    /// stored value alone. A float-stored stop selects its exact tag.
    private var renderScaleSelection: Binding<Double> {
        let scale = scoped(SettingsFields.renderScale)
        return Binding(
            get: {
                guard !isCustomScale, let i = RenderScale.presetIndex(scale.wrappedValue)
                else { return Self.customScaleTag }
                return RenderScale.presets[i]
            },
            set: { tag in
                customScaleMode = tag == Self.customScaleTag
                if tag != Self.customScaleTag { scale.wrappedValue = tag }
            })
    }

    /// The typed percent, applied when the field commits (Return or focus loss), so a half-typed
    /// number is never clamped under the thumb.
    private var customScalePercent: Binding<Int> {
        let scale = scoped(SettingsFields.renderScale)
        return Binding(
            get: { RenderScale.percent(scale.wrappedValue) },
            set: { scale.wrappedValue = RenderScale.fromPercent($0) })
    }
    #endif

    /// Render scale explained, with the CONCRETE host resolution when it applies — the cost made
    /// legible. Only the explicit mode can show it (match-window derives the base from the live
    /// window, not these fields).
    private var renderScaleDescription: String {
        var text = String(localized: L10n.playerRenderScaleHint)
        let settings = effective
        if settings.renderScale != 1.0, !settings.matchWindow {
            let mode = settings.streamMode(native: connectNative)
            text += " " + L10n.appleRenderScaleHostSize(
                width: String(Int(mode.width)), height: String(Int(mode.height)))
        }
        return text
    }

    #if !os(tvOS)
    /// The automatic-bitrate toggle + manual slider (and the >1 Gbps warning) rows. Under
    /// PyroWave only the toggle: its quality row stands in for the slider, and the stored rate
    /// waits for the next codec.
    @ViewBuilder private var bitrateRows: some View {
        if pyroWaveRate {
            pyrowaveAutomaticRow
        } else {
            described(L10n.playerBitrateKbpsAutoSwitchHint, field: "bitrate_kbps") {
                Toggle(L10n.playerBitrateKbpsAutoSwitch, isOn: automaticBitrate)
            }
        }
        if !pyroWaveRate, effective.bitrateKbps != 0 {
            HStack(spacing: 12) {
                Slider(value: bitrateSlider, in: 0...1) {
                    Text(L10n.playerBitrateKbps)
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
    /// a switch and a slider. Under PyroWave only the switch, beside the quality row's rates.
    @ViewBuilder private var tvBitrateRow: some View {
        if pyroWaveRate {
            pyrowaveAutomaticRow
        } else {
            described(effective.bitrateKbps > 1_000_000
                ? Self.gigabitWarning : String(localized: L10n.playerBitrateKbpsHint),
                field: "bitrate_kbps") {
                TVSelectionRow(
                    title: String(localized: L10n.playerBitrateKbps),
                    options: SettingsOptions.bitrateOptions(current: effective.bitrateKbps),
                    selection: scoped(SettingsFields.bitrateKbps))
            }
            described(L10n.appleCustomBitrateHint, field: "bitrate_kbps") {
                TVFieldRow(
                    label: String(localized: L10n.appleCustomBitrate),
                    value: SettingsOptions.isCustomBitrate(effective.bitrateKbps)
                        ? SpeedTestView.mbpsLabel(kbps: effective.bitrateKbps) : "",
                    placeholder: String(localized: L10n.appleCustomBitratePlaceholder)
                ) { typingBitrate = true }
                .fullScreenCover(isPresented: $typingBitrate) {
                    TVTextEntry(
                        title: String(localized: L10n.playerBitrateKbpsEntry), text: "",
                        keyboardType: .numberPad
                    ) {
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

    /// Automatic under PyroWave: on, the session steps its quality down while the link falls
    /// short; off, any fixed rate pins it at the quality row's.
    private var pyrowaveAutomaticRow: some View {
        described(L10n.playerBitrateKbpsPyrowaveSwitchHint, field: "bitrate_kbps") {
            Toggle(L10n.playerBitrateKbpsAutoSwitch, isOn: automaticBitrate)
        }
    }

    /// The codec is PyroWave and this device decodes it: the session's rate is its quality's.
    var pyroWaveRate: Bool {
        effective.codec == "pyrowave" && MetalWaveletDecoder.supported
    }

    /// Native as a connect resolves it: an attached monitor's on iPhone and iPad, else this
    /// display's. Every caption that names a mode reads it.
    private var connectNative: (width: Int, height: Int, hz: Int) {
        #if os(iOS)
        if let native = ExternalDisplay.nativeMode { return native }
        #endif
        return NativeDisplay.mode
    }

    /// The picture a connect would have the host encode (`streamPicture`).
    private func connectPicture(_ s: EffectiveSettings) -> (width: UInt32, height: UInt32, hz: UInt32) {
        s.streamPicture(native: connectNative)
    }

    /// PyroWave quality in Bitrate's place: 0.5 to 2 by tenths, read as the rate it needs at the
    /// picture a connect would have the host encode, warned when this device's link is short of
    /// it. The TV lists those rates.
    @ViewBuilder private var pyrowaveQualityRows: some View {
        if pyroWaveRate {
            let s = effective
            let picture = connectPicture(s)
            let lines = PyroWaveQuality.lines(s, mode: picture, link: PyroWaveQuality.localLink())
            let field = scoped(SettingsFields.pyrowaveBpp)
            let quality = Binding(
                get: { PyroWaveQuality.snapped(field.wrappedValue) },
                set: { field.wrappedValue = PyroWaveQuality.snapped($0) })
            #if os(tvOS)
            described(lines.warning.map { "\(lines.caption)\n\($0)" } ?? lines.caption,
                field: "pyrowave_bpp") {
                settingPicker(
                    L10n.playerPyrowaveBpp,
                    options: PyroWaveQuality.rungs.map { bpp in
                        var at = s
                        at.pyrowaveBpp = bpp
                        let kbps = PyroWaveQuality.kbps(at, mode: picture)
                        return (label: PyroWaveQuality.rateLabel(kbps: kbps), tag: bpp)
                    },
                    selection: quality)
            }
            #else
            described(lines.caption, field: "pyrowave_bpp") {
                VStack(alignment: .leading, spacing: 6) {
                    Text(L10n.playerPyrowaveBpp)
                    Slider(value: quality, in: PyroWaveQuality.range, step: 0.1) {
                        Text(L10n.playerPyrowaveBpp)
                    }
                    .labelsHidden()
                }
            }
            if let warning = lines.warning {
                Label(warning, systemImage: "exclamationmark.triangle.fill")
                    .font(.geist(12, relativeTo: .caption))
                    .foregroundStyle(.orange)
            }
            #endif
        }
    }

    // MARK: - Display: Presentation

    // The presentation intent (design/apple-presentation-rebuild.md): latency (newest-wins,
    // zero queue) vs smoothness (a small deliberate jitter buffer). The stage ladder survives
    // only as the hidden PUNKTFUNK_PRESENTER debug env lever.
    private var prioritizeRow: some View {
        described(effective.presentPriority == "smooth"
            ? L10n.playerPresentPrioritySmoothHint
            : L10n.playerPresentPriorityLatencyHint,
            field: "present_priority") {
            settingPicker(
                L10n.playerPresentPriority,
                options: SettingsOptions.presentPriorities,
                selection: scoped(SettingsFields.presentPriority))
        }
    }

    @ViewBuilder private var presentationRows: some View {
        // macOS-only: iOS/tvOS layers always present on the display's vsync, so the choice
        // only exists on the Mac (the layer's own sync stays off — see MetalVideoPresenter).
        #if os(macOS)
        described(L10n.playerVsyncHint, field: "vsync") {
            Toggle("V-Sync", isOn: scoped(SettingsFields.vsync))
        }
        #endif
        // Non-tvOS: the Apple TV drives a fixed HDMI mode, so there's no adaptive refresh.
        #if !os(tvOS)
        described(L10n.playerAllowVrrHint, field: "allow_vrr") {
            Toggle(L10n.playerAllowVrr, isOn: scoped(SettingsFields.allowVRR))
        }
        #endif
    }

    // MARK: - Advanced

    /// A category's advanced rows, as `AdvancedDisclosure` decides. `fields` are the rows'
    /// overlay names.
    @ViewBuilder
    func advancedSection<Content: View>(
        changed: Int, fields: [String] = [], @ViewBuilder content: () -> Content
    ) -> some View {
        switch AdvancedDisclosure.state(
            showAdvanced: showAdvanced, overridden: fields.contains(where: isOverridden),
            changed: changed, inPreset: inPresetScope)
        {
        case .rows:
            Section(L10n.prefsAdvanced) { content() }
        case .countButton(let count):
            Section {
                Button(L10n.prefsAdvancedChanged(count: count)) {
                    showAdvanced = true
                }
            }
        case .hidden:
            EmptyView()
        }
    }

    /// Device-wide, and in both scopes: it changes what this surface lists, not a stream.
    var showAdvancedSection: some View {
        Section {
            described(L10n.playerShowAdvancedHint) {
                Toggle(L10n.playerShowAdvanced, isOn: $showAdvanced)
            }
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
            Section(L10n.prefsGroupSession) {
                #if os(macOS)
                if inPresetScope {
                    described(fullscreenAlways
                        ? L10n.appleFullscreenOnStreamAlwaysHint
                        : L10n.playerFullscreenOnStreamHint, field: "fullscreen_on_stream") {
                        Toggle(
                            L10n.playerFullscreenOnStream,
                            isOn: scoped(SettingsFields.fullscreenWhileStreaming))
                    }
                } else {
                    described(fullscreenCaption) {
                        settingPicker(
                            L10n.playerFullscreen, options: SettingsOptions.fullscreenModes,
                            selection: fullscreenMode)
                    }
                }
                #endif
                if !inPresetScope {
                    described(L10n.playerAutoWakeHint) {
                        Toggle(L10n.playerAutoWake, isOn: $autoWakeEnabled)
                    }
                }
                #if os(iOS) || os(visionOS) || os(tvOS)
                if !inPresetScope {
                    described(L10n.playerBackgroundKeepAliveHint) {
                        Toggle(L10n.playerBackgroundKeepAlive, isOn: $backgroundKeepAlive)
                    }
                    if backgroundKeepAlive {
                        described(Self.backgroundTimeoutCaption) {
                            settingPicker(
                                L10n.playerBackgroundTimeoutMinutes,
                                options: [1, 5, 10, 30].map {
                                    (label: L10n.playerBackgroundTimeoutMinutesCount(count: $0),
                                     tag: $0)
                                },
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
        String(localized: L10n.playerBackgroundTimeoutMinutesTvHint)
        #else
        String(localized: L10n.playerBackgroundTimeoutMinutesHint)
        #endif
    }

    #if os(macOS)
    /// The Fullscreen picker over its two stores. Global scope only: a preset holds just the
    /// streaming half, so "always" never reaches one.
    private var fullscreenMode: Binding<String> {
        Binding(
            get: { fullscreenAlways ? "always" : (fullscreenWhileStreaming ? "stream" : "off") },
            set: { mode in
                fullscreenAlways = mode == "always"
                if mode != "always" { fullscreenWhileStreaming = mode == "stream" }
            })
    }

    private var fullscreenCaption: String {
        switch fullscreenMode.wrappedValue {
        case "always": String(localized: L10n.playerFullscreenAlwaysHint)
        case "stream": String(localized: L10n.playerFullscreenStreamHint)
        default: String(localized: L10n.playerFullscreenOffHint)
        }
    }
    #endif

    private static var gamepadUIModeCaption: String {
        String(localized: L10n.playerGamepadUiModeHint)
    }

    // MARK: - General: Statistics overlay

    @ViewBuilder var overlaySection: some View {
        Section(L10n.prefsGroupStatistics) {
            described(Self.statisticsDescription, field: "stats_verbosity") {
                settingPicker(
                    L10n.playerStatsVerbosity, options: SettingsOptions.statsVerbosities,
                    selection: scoped(SettingsFields.statsVerbosity))
            }
            #if !os(tvOS)
            if !inPresetScope {
                Link(L10n.playerStatsDocsLink, destination: Self.statsDocsURL)
                    .foregroundStyle(Color.brand) // a Link takes the system accent, not the tint
            }
            #endif
        }
    }

    /// The overlay's vocabulary, corner and size, and the exit hint: this device's screen,
    /// never a preset's (tier G), so the section sits out preset scope.
    @ViewBuilder var generalAdvancedSection: some View {
        if !inPresetScope {
            advancedSection(changed: generalChanged) {
                described(Self.advancedStatisticsDescription) {
                    Toggle(L10n.playerAdvancedStats, isOn: $advancedStats)
                }
                described(L10n.playerHudPlacementHint) {
                    settingPicker(
                        L10n.playerHudPlacement, options: SettingsOptions.hudPlacements,
                        selection: $hudPlacement)
                }
                .disabled(effective.statsVerbosity == StatsVerbosity.off.rawValue)
                described(L10n.appleStatsScalePctHint) {
                    settingPicker(
                        L10n.playerStatsScalePct, options: SettingsOptions.statsScales,
                        selection: $statsScalePct)
                }
                described(L10n.playerExitHintHint) {
                    Toggle(L10n.playerExitHint, isOn: $exitHint)
                }
            }
        }
    }

    /// General's last section: every setting back to a fresh install's value, asked first. A
    /// preset resets row by row, so only Default settings has it.
    @ViewBuilder var resetSection: some View {
        if !inPresetScope {
            Section {
                Button(L10n.prefsResetAll, role: .destructive) { confirmingReset = true }
                    .alert(L10n.prefsResetAllTitle, isPresented: $confirmingReset) {
                        Button(L10n.commonCancel, role: .cancel) {}
                        Button(L10n.commonReset, role: .destructive) { resetAllSettings() }
                    } message: {
                        Text(L10n.prefsResetAllBody)
                    }
            }
        }
    }

    private func resetAllSettings() {
        SettingDefault.resetAll()
        // This tier's @AppStorage default was read at launch, from the value just removed.
        statsVerbosityRaw = SettingDefault.statsVerbosity
        // The pad manager keeps its own copy of the pinned controller, read at launch.
        gamepads.preferredID = ""
        #if os(iOS) || os(visionOS) || os(macOS)
        clearCustomFlags()
        #endif
    }

    private var generalChanged: Int {
        [advancedStats, hudPlacement != HUDPlacement.topTrailing.rawValue, statsScalePct != 100,
         !exitHint].filter { $0 }.count
    }

    // MARK: - General: Library

    @ViewBuilder var librarySection: some View {
        // An app-level feature switch for this device (tier G) — the whole section collapses in
        // preset scope rather than rendering an empty group.
        if !inPresetScope {
            Section(L10n.prefsGroupLibrary) {
                described(L10n.playerLibraryViewHint) {
                    settingPicker(
                        L10n.playerLibraryView,
                        options: LibraryArrangement.all.map { (label: $0.label, tag: $0.stored) },
                        selection: $libraryViewRaw)
                }
                described(startInFooter) {
                    settingPicker(
                        L10n.playerStartIn,
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
            return String(localized: L10n.playerStartInNoHostHint)
        }
        return L10n.playerStartInHostHint(host: host.displayName)
    }

    // MARK: - Input

    #if os(iOS) || os(visionOS)
    /// Touch-input model (iPhone + iPad) plus the iPad-only pointer-capture toggle: lock the
    /// mouse/trackpad for relative movement (games) vs forward an absolute cursor position.
    @ViewBuilder var pointerSection: some View {
        Section(L10n.prefsGroupTouchPointer) {
            described(touchModeDescription, field: "touch_mode") {
                Picker(L10n.playerTouchMode, selection: scoped(SettingsFields.touchMode)) {
                    Text(L10n.playerTouchModeTrackpad).tag(TouchInputMode.trackpad.rawValue)
                    Text(L10n.playerTouchModePointer).tag(TouchInputMode.pointer.rawValue)
                    Text(L10n.playerTouchModeTouch).tag(TouchInputMode.touch.rawValue)
                    Text(L10n.playerTouchModeOff).tag(TouchInputMode.off.rawValue)
                }
            }
            quickActionsRow
            shortcutsRow
            // Whether a hardware mouse attached to THIS iPad gets locked is a fact about this
            // device's input hardware (tier G), not about how a host is streamed.
            if !inPresetScope, UIDevice.current.userInterfaceIdiom == .pad {
                described(L10n.applePlayerPointerCaptureHint) {
                    Toggle(L10n.applePlayerPointerCapture, isOn: $pointerCapture)
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
            return String(localized: L10n.playerTouchModeTrackpadHint)
        case .pointer:
            return String(localized: L10n.playerTouchModePointerHint)
        case .touch:
            return String(localized: L10n.playerTouchModeTouchHint)
        case .off:
            return String(localized: L10n.playerTouchModeOffHint)
        }
    }
    #endif

    /// The shortcuts of the layer being edited, as the next stream resolves them.
    var editedShortcuts: Shortcuts {
        Shortcuts.resolve(effective.shortcuts, client: shortcutClient,
                          legacyGuideGesture: effective.guideGesture)
    }

    /// The Shortcuts page of the layer being edited.
    var shortcutsEditor: ShortcutsEditor {
        ShortcutsEditor(
            blob: scoped(SettingsFields.shortcuts), legacyGuideGesture: effective.guideGesture,
            overlayActions: effective.overlayActions, overridden: isOverridden("shortcuts")
        ) {
            if inPresetScope {
                resetOverride("shortcuts")
            } else {
                scoped(SettingsFields.shortcuts).wrappedValue = ""
            }
        }
    }

    #if os(iOS) || os(visionOS) || os(macOS)
    /// How to reach the dial on THIS platform, from the live shortcuts. The editor must name its
    /// opener: six buttons nobody can summon are six buttons nobody sees.
    private var dialOpener: String {
        let s = editedShortcuts
        guard let keys = s.hint(.ring, device: .keyboard), let pad = s.hint(.ring, device: .pad) else {
            return ""
        }
        #if os(macOS)
        return L10n.applePlayerOverlayActionsOpenerMac(keys: keys, padKeys: pad) + " "
        #else
        return L10n.applePlayerOverlayActionsOpener(keys: keys, padKeys: pad) + " "
        #endif
    }

    /// The in-stream quick-action ring's editor, opened as a sheet. Every platform with a ring
    /// the user can change: iOS from Touch & pointer, macOS from Keyboard & mouse.
    @ViewBuilder var quickActionsRow: some View {
        described(dialOpener + String(localized: L10n.playerOverlayActionsHint),
                  field: "overlay_actions") {
            // A SHEET, not a push: the detail column is not a NavigationStack, and a
            // NavigationLink pushed from it popped the collapsed iPhone stack to the category
            // list on the way back and left the selection dead (AboutView's rows say the same).
            Button {
                showQuickActions = true
            } label: {
                HStack {
                    Text(L10n.playerOverlayActions)
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
                            Button(L10n.commonDone) { showQuickActions = false }
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

    /// The Shortcuts page (design/client-shortcuts.md §5), a sheet like Quick actions.
    @ViewBuilder var shortcutsRow: some View {
        described(L10n.playerShortcutsHint, field: "shortcuts") {
            Button {
                showShortcuts = true
            } label: {
                HStack {
                    Text(L10n.playerShortcuts)
                    Spacer(minLength: 8)
                    Image(systemName: "chevron.right")
                        .font(.footnote.weight(.semibold))
                        .foregroundStyle(.tertiary)
                        .accessibilityHidden(true)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .sheet(isPresented: $showShortcuts) {
                NavigationStack {
                    shortcutsEditor.toolbar {
                        ToolbarItem(placement: .confirmationAction) {
                            Button(L10n.commonDone) { showShortcuts = false }
                        }
                    }
                }
                #if os(macOS)
                .frame(width: 460, height: 560)
                #endif
            }
        }
    }
    #endif

    #if !os(tvOS)
    /// Keyboard & mouse forwarding — applies wherever a hardware keyboard/mouse drives the stream
    /// (always on macOS; an attached keyboard/mouse on iPad). Absent on tvOS (no such input path).
    @ViewBuilder var inputSection: some View {
        Section(L10n.prefsGroupKeyboardMouse) {
            #if os(macOS)
            described(mouseModeDescription, field: "mouse_mode") {
                Picker(L10n.playerMouseMode, selection: scoped(SettingsFields.mouseMode)) {
                    Text(L10n.playerMouseModeCapture).tag(MouseInputMode.capture.rawValue)
                    Text(L10n.playerMouseModeDesktop).tag(MouseInputMode.desktop.rawValue)
                }
            }
            described(
                L10n.playerInhibitShortcutsHint(keys: "⌘Space, ⌘Tab, Mission Control, …")
                    + " " + L10n.applePlayerInhibitShortcutsReleaseHint(keys: "⌘⎋"),
                field: "inhibit_shortcuts"
            ) {
                Toggle(L10n.playerInhibitShortcuts, isOn: scoped(SettingsFields.inhibitShortcuts))
            }
            quickActionsRow
            shortcutsRow
            #endif
            described(
                (ModifierLayout(rawValue: effective.modifierLayout) ?? .mac).detail,
                field: "modifier_layout"
            ) {
                Picker(
                    L10n.applePlayerModifierLayout,
                    selection: scoped(SettingsFields.modifierLayout)
                ) {
                    ForEach(ModifierLayout.allCases, id: \.self) { layout in
                        Text(layout.label).tag(layout.rawValue)
                    }
                }
            }
            described(L10n.playerInvertScrollHint, field: "invert_scroll") {
                Toggle(L10n.playerInvertScroll, isOn: scoped(SettingsFields.invertScroll))
            }
        }
    }

    #if os(macOS)
    /// The SELECTED mouse model explained — dynamic, like the touch-mode caption.
    private var mouseModeDescription: String {
        switch MouseInputMode(rawValue: effective.mouseMode) ?? .capture {
        case .capture:
            let keys = editedShortcuts.hint(.slot(.mouseMode), device: .keyboard)
            return String(localized: L10n.playerMouseModeCaptureHint)
                + (keys.map { " " + L10n.playerMouseModeKeysHint(keys: $0) } ?? "")
        case .desktop:
            return String(localized: L10n.playerMouseModeDesktopHint)
        }
    }
    #endif
    #endif

    // MARK: - Audio

    @ViewBuilder var audioSection: some View {
        Section {
            described(L10n.playerAudioChannelsHint, field: "audio_channels") {
                settingPicker(
                    L10n.playerAudioChannels, options: SettingsOptions.audioChannels,
                    selection: scoped(SettingsFields.audioChannels))
            }
            #if os(macOS)
            // Which speaker THIS Mac plays through is this device's audio routing (tier G).
            if !inPresetScope {
                described(L10n.playerSpeakerDeviceHint) {
                    Picker(L10n.playerSpeakerDevice, selection: $speakerUID) {
                        Text(L10n.playerSpeakerDeviceSystem).tag("")
                        ForEach(outputDevices) { device in
                            Text(device.name).tag(device.uid)
                        }
                        if !speakerUID.isEmpty,
                           !outputDevices.contains(where: { $0.uid == speakerUID }) {
                            Text(L10n.appleAudioDeviceUnavailable).tag(speakerUID)
                        }
                    }
                }
            }
            #endif
            // An Apple TV has no microphone an app can open.
            #if !os(tvOS)
            described(L10n.playerMicEnabledHint, field: "mic_enabled") {
                Toggle(L10n.playerMicEnabled, isOn: scoped(SettingsFields.micEnabled))
            }
            #endif
            #if os(visionOS)
            // Where this headset plays the sound (tier G).
            if !inPresetScope {
                described(L10n.playerSpatialAudioHint) {
                    Toggle(L10n.playerSpatialAudio, isOn: $spatialAudio)
                }
            }
            #endif
            #if os(macOS)
            if !inPresetScope {
                Picker(L10n.playerMicDevice, selection: $micUID) {
                    Text(L10n.playerMicDeviceSystem).tag("")
                    ForEach(inputDevices) { device in
                        Text(device.name).tag(device.uid)
                    }
                    if !micUID.isEmpty,
                       !inputDevices.contains(where: { $0.uid == micUID }) {
                        Text(L10n.appleAudioDeviceUnavailable).tag(micUID)
                    }
                }
                .disabled(!effective.micEnabled)
                // Multi-channel interfaces only: the mic sits on ONE discrete input, so let the
                // user pick it. Auto sums every channel (a lone hot mic still passes at full
                // level).
                if micChannelCount > 1 {
                    described(L10n.applePlayerMicChannelHint) {
                        Picker(L10n.applePlayerMicChannel, selection: $micChannel) {
                            Text(L10n.applePlayerMicChannelAuto).tag(0)
                            ForEach(1...micChannelCount, id: \.self) { ch in
                                Text(L10n.applePlayerMicChannelN(n: String(ch))).tag(ch)
                            }
                        }
                        .disabled(!effective.micEnabled)
                    }
                }
            }
            #endif
        } header: {
            Text(L10n.prefsCategoryAudio)
        } footer: {
            Text(L10n.prefsAppliesNextSession)
                .settingsFooter()
        }
        advancedSection(
            changed: audioChanged, fields: ["audio_format", "keep_host_audio", "echo_cancel"]
        ) {
            // Offered at every channel count: surround negotiates a shorter frame, and the
            // caption states the cases that genuinely do not fit.
            described(audioFormatCaption, field: "audio_format") {
                settingPicker(
                    L10n.playerAudioFormat, options: SettingsOptions.audioFormats,
                    selection: scoped(SettingsFields.audioFormat))
            }
            described(L10n.playerKeepHostAudioHint, field: "keep_host_audio") {
                Toggle(L10n.playerKeepHostAudio, isOn: scoped(SettingsFields.keepHostAudio))
            }
            #if !os(tvOS)
            described(echoCancelCaption, field: "echo_cancel") {
                Toggle(L10n.playerEchoCancel, isOn: scoped(SettingsFields.echoCancel))
                    .disabled(!effective.micEnabled)
            }
            #endif
        }
    }

    private var audioChanged: Int {
        let e = effective, d = EffectiveSettings()
        var changed = [e.audioFormat != d.audioFormat, e.keepHostAudio != d.keepHostAudio]
        #if !os(tvOS)
        changed.append(e.echoCancel != d.echoCancel)
        #endif
        return changed.filter { $0 }.count
    }

    /// The SELECTED audio format: what it is, what it costs on the wire, and that it falls back
    /// to Standard — the host's own switch (off by default), its capture gate or this device's
    /// output can each decline it.
    ///
    /// ⚠ The design's rule: **the UI states the RESOLVED format, never the requested one.**
    /// Nothing here may read as a guarantee — the stats overlay's audio line, built from the
    /// connection's `Welcome`, is the only place a format is asserted as fact. The fallback
    /// clause is what carries that; do not drop it without replacing it.
    private var audioFormatCaption: String {
        let choice = AudioFormatChoice(setting: effective.audioFormat)
        guard choice != .opus else {
            return String(localized: L10n.applePlayerAudioFormatOpusHint)
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
        let head = L10n.applePlayerAudioFormatLosslessHint(rate: mbps)
        guard effective.audioChannels > 2 else { return head }
        // One frame per datagram: more channels buy a shorter frame. At 24-bit and the default
        // datagram (`pcm::frame_us_for`), 44.1 and 48 kHz surround fit a 1–1.5 ms rung, which
        // costs packets; 88.2 kHz upward fits none, so it gets the other sentence.
        guard choice == .lossless441 || choice == .lossless48 else {
            return head + " " + String(localized: L10n.applePlayerAudioFormatSurroundLimitHint)
        }
        return head + " " + String(localized: effective.audioChannels == 6
            ? L10n.applePlayerAudioFormatSurround6Hint
            : L10n.applePlayerAudioFormatSurround8Hint)
    }

    /// Honest about the macOS escape hatch: the voice processor only follows the system
    /// default devices, so hand-picked endpoints silently keep the raw path (see
    /// SessionAudio's topology note) — better said here than discovered mid-call.
    private var echoCancelCaption: String {
        let base = String(localized: L10n.playerEchoCancelHint)
        #if os(macOS)
        return base + " " + String(localized: L10n.applePlayerEchoCancelMacHint)
        #else
        return base
        #endif
    }

    // MARK: - Controllers

    @ViewBuilder var controllersSection: some View {
        Section {
            // Which physical pads this device sees, and what its own haptics do, are facts
            // about THIS device (tier G) — only the virtual pad the host creates is presetable.
            if !inPresetScope {
                if gamepads.controllers.isEmpty {
                    Text(L10n.padNoneDetected)
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(gamepads.controllers) { controller in
                        controllerRow(controller)
                    }
                }
            }
            described(L10n.playerGamepadHint, field: "gamepad") {
                settingPicker(
                    L10n.playerGamepad, options: SettingsOptions.padTypes,
                    selection: scoped(SettingsFields.gamepadType))
                    .disabled(!effective.gamepadForwarding)
            }
            if !inPresetScope {
                described(L10n.playerPadRumbleHint) {
                    Toggle(L10n.playerPadRumble, isOn: $padRumble)
                        .disabled(!effective.gamepadForwarding)
                }
            }
            #if !os(tvOS)
            if homeButtonKept, !inPresetScope, effective.gamepadForwarding,
               effective.systemButtonsForward, #available(macOS 27.0, iOS 27.0, visionOS 27.0, *) {
                described(L10n.applePadHomeButtonHint) {
                    Button(L10n.applePadHomeButtonSettings) {
                        try? GCControllerHomeButtonSettingsManager()
                            .openControllerHomeButtonSettings(for: .customizeInAppAction)
                    }
                }
            }
            #endif
            #if os(iOS)
            // iPhone only in practice: hidden where the device itself can't play haptics (iPad).
            if !inPresetScope, CHHapticEngine.capabilitiesForHardware().supportsHaptics {
                described(L10n.playerRumbleOnPhoneHint) {
                    Toggle(L10n.playerRumbleOnPhone, isOn: $rumbleOnDevice)
                }
            }
            // The rumble mirror's sibling, data flowing the other way: hidden where the
            // device has no motion hardware, engages only while the player-1 controller
            // reports no rotation rate of its own.
            if !inPresetScope, DeviceGyro.isAvailable {
                described(L10n.playerGyroOnPhoneHint) {
                    Toggle(L10n.playerGyroOnPhone, isOn: $gyroFromDevice)
                }
            }
            #endif
            if !inPresetScope {
                described(L10n.playerGamepadUiEnabledHint) {
                    Toggle(L10n.playerGamepadUiEnabled, isOn: $gamepadUIEnabled)
                }
                // Only meaningful while the switch above is on, so it is HIDDEN rather than
                // disabled when it isn't: a picker whose every option decides nothing is worse
                // than no picker, and this Section is short enough that nothing jumps far.
                if gamepadUIEnabled {
                    described(Self.gamepadUIModeCaption) {
                        settingPicker(
                            L10n.playerGamepadUiMode, options: SettingsOptions.gamepadUIModes,
                            selection: $gamepadUIMode)
                    }
                    #if os(tvOS)
                    // A TV's only route to `ui_palette`: the gamepad settings that carry it
                    // elsewhere need a controller to open.
                    described(L10n.playerUiPaletteHint) {
                        settingPicker(
                            L10n.playerUiPalette,
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
            Text(L10n.prefsCategoryControllers)
        } footer: {
            Text(L10n.prefsAppliesNextSession)
                .settingsFooter()
        }
        #if !os(tvOS)
        .task(id: gamepads.controllers.isEmpty) { await watchHomeButton() }
        #endif
        advancedSection(
            changed: controllersChanged,
            fields: ["gamepad_forwarding", "system_buttons", "guide_gesture"]
        ) {
            controllerAdvancedRows
        }
    }

    /// Forwarding and the system buttons. The master switch is presetable, so it renders in both
    /// scopes: a "Work" preset can decline to forward what "Game" forwards.
    @ViewBuilder private var controllerAdvancedRows: some View {
        described(L10n.playerGamepadForwardingHint, field: "gamepad_forwarding") {
            Toggle(L10n.playerGamepadForwarding, isOn: scoped(SettingsFields.gamepadForwarding))
        }
        if !inPresetScope {
            described(L10n.playerForwardPadHint) {
                settingPicker(
                    L10n.playerForwardPad, options: controllerOptions,
                    selection: $gamepads.preferredID)
                    .disabled(!effective.gamepadForwarding)
            }
        }
        described(L10n.playerSystemButtonsHint, field: "system_buttons") {
            settingPicker(
                L10n.playerSystemButtons, options: SettingsOptions.systemButtons,
                selection: scoped(SettingsFields.systemButtons))
                .disabled(!effective.gamepadForwarding)
        }
        // Steam Controller 2 as-is passthrough: device tier (EffectiveSettings.sc2Capture is not
        // presetable). It engages at the next stream; the in-stream badge announces it.
        if !inPresetScope {
            described(Self.sc2CaptureCaption) {
                Toggle(L10n.playerSc2Capture, isOn: $sc2Capture)
                    .disabled(!effective.gamepadForwarding)
            }
        }
    }

    private var controllersChanged: Int {
        let e = effective, d = EffectiveSettings()
        return [
            e.gamepadForwarding != d.gamepadForwarding, !gamepads.preferredID.isEmpty,
            e.systemButtons != d.systemButtons, sc2Capture,
        ].filter { $0 }.count
    }

    #if !os(tvOS)
    /// Tracks whether the OS keeps the controller's Home press (macOS/iOS 27). An app listed under
    /// Home Button Overrides reads `.defer`, and `GamepadCapture.attach`'s gesture claim then
    /// hands the press to the stream. The setting reads only while a controller is connected.
    func watchHomeButton() async {
        guard #available(macOS 27.0, iOS 27.0, visionOS 27.0, *), !gamepads.controllers.isEmpty else {
            homeButtonKept = false
            return
        }
        let settings = GCControllerHomeButtonSettingsManager()
        let read = { (try? settings.controllerHomeButtonInAppAction.action) == .systemDefault }
        homeButtonKept = read()
        for await _ in settings.settingsUpdates { homeButtonKept = read() }
    }
    #endif
}

#if os(iOS) || os(visionOS)
/// Its children left to right at their own size, starting a new line where the next one would
/// pass the proposed width.
private struct WrapLayout: Layout {
    var spacing: CGFloat

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let frames = frames(subviews, width: proposal.width ?? .infinity)
        return CGSize(width: frames.map(\.maxX).max() ?? 0, height: frames.map(\.maxY).max() ?? 0)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for (view, frame) in zip(subviews, frames(subviews, width: bounds.width)) {
            view.place(at: CGPoint(x: bounds.minX + frame.minX, y: bounds.minY + frame.minY),
                       proposal: ProposedViewSize(frame.size))
        }
    }

    private func frames(_ subviews: Subviews, width: CGFloat) -> [CGRect] {
        var frames: [CGRect] = []
        var origin = CGPoint.zero
        var lineHeight: CGFloat = 0
        for view in subviews {
            let size = view.sizeThatFits(.unspecified)
            if origin.x > 0, origin.x + size.width > width {
                origin = CGPoint(x: 0, y: origin.y + lineHeight + spacing)
                lineHeight = 0
            }
            frames.append(CGRect(origin: origin, size: size))
            origin.x += size.width + spacing
            lineHeight = max(lineHeight, size.height)
        }
        return frames
    }
}
#endif
