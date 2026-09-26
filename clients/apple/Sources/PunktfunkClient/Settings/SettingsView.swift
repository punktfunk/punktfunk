// App settings: one category map on every platform (SettingsCategory) — macOS tabs, an iOS
// split view, a tvOS sidebar with the focused row's caption in a band. Each row is defined once,
// in SettingsView+Sections.swift; helpers such as `described` are in SettingsView+Support.swift.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

@MainActor
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    // Which LAYER this surface is editing (SettingsView+Scope): the global defaults, or one
    // preset's overrides. A TV picks it in its Editing pane.
    @ObservedObject var presets = PresetStore.shared
    @State var scope: SettingsScope = .defaults
    /// The preset editor (create / duplicate / edit), when it is open, and the preset a delete
    /// is being confirmed for.
    @State var presetDraft: PresetDraft?
    @State var presetPendingDelete: StreamPreset?
    /// The menu's icons are rasterised per appearance (see `MenuIcon`), so the surface has to know
    /// which one it is drawing in.
    @Environment(\.colorScheme) var colorScheme
    #if os(macOS)
    @State private var macTab: MacTab = .general
    #endif
    @AppStorage(DefaultsKey.streamWidth) var width = 1920
    @AppStorage(DefaultsKey.streamHeight) var height = 1080
    @AppStorage(DefaultsKey.streamHz) var hz = 60
    // Opt-in (default OFF): the explicit mode below is used and never auto-resized. When ON, a
    // windowed session instead streams at the window's native pixels (1:1, no scaling) so it stays
    // pixel-exact rather than the presenter resampling a fixed-mode frame into the window.
    @AppStorage(DefaultsKey.matchWindow) var matchWindow = false
    // Render-resolution multiplier: the host renders/encodes at chosen-resolution × this, and the
    // presenter downscales (> 1 = supersampling for sharpness) or upscales (< 1 = a lighter host /
    // link). 1.0 = Native (the prior behaviour).
    @AppStorage(DefaultsKey.renderScale) var renderScale = 1.0
    @AppStorage(DefaultsKey.videoFit) var videoFit = VideoFit.fit.rawValue
    @AppStorage(DefaultsKey.compositor) var compositor = 0
    @AppStorage(DefaultsKey.gamepadType) var gamepadType = 0
    @AppStorage(DefaultsKey.gamepadForwarding) var gamepadForwarding = true
    @AppStorage(DefaultsKey.systemButtons) var systemButtons = "auto"
    @AppStorage(DefaultsKey.guideGesture) var guideGesture = "auto"
    @AppStorage(DefaultsKey.bitrateKbps) var bitrateKbps = 0
    @AppStorage(DefaultsKey.presentPriority) var presentPriority =
        SettingsOptions.presentPriorityDefault
    @AppStorage(DefaultsKey.smoothBuffer) var smoothBuffer = 0
    #if os(macOS)
    @AppStorage(DefaultsKey.vsync) var vsync = false
    @AppStorage(DefaultsKey.windowedSafePresent) var windowedSafePresent = true
    #endif
    #if !os(tvOS)
    @AppStorage(DefaultsKey.allowVRR) var allowVRR = true
    #endif
    @AppStorage(DefaultsKey.hdrEnabled) var hdrEnabled = true
    @AppStorage(DefaultsKey.enable444) var enable444 = false
    @AppStorage(DefaultsKey.tenBitSdr) var tenBitSdr = false
    /// The gamepad library's arrangement — a device preference, stored as the cross-client
    /// `library_view` value.
    @AppStorage(DefaultsKey.libraryView) var libraryViewRaw = LibraryArrangement.shelf.stored
    @AppStorage(DefaultsKey.startIn) var startInRaw = StartIn.hosts.stored
    @AppStorage(DefaultsKey.defaultHost) var defaultHostID = ""
    @AppStorage(DefaultsKey.fullscreenWhileStreaming) var fullscreenWhileStreaming = true
    @AppStorage(DefaultsKey.micEnabled) var micEnabled = true
    @AppStorage(DefaultsKey.echoCancel) var echoCancel = true
    @AppStorage(DefaultsKey.keepHostAudio) var keepHostAudio = false
    @AppStorage(DefaultsKey.audioChannels) var audioChannels = 2
    @AppStorage(DefaultsKey.audioFormat) var audioFormat = AudioFormatChoice.opus.rawValue
    @AppStorage(DefaultsKey.codec) var codec = "auto"
    // The overlay tier's raw string (the pickers tag by rawValue); the absent-key default runs
    // the legacy-hudEnabled migration (same pattern as ContentView/StreamCommands).
    @AppStorage(DefaultsKey.statsVerbosity) var statsVerbosityRaw = StatsVerbosity.current.rawValue
    @AppStorage(DefaultsKey.hudPlacement) var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.advancedStats) var advancedStats = false
    @ObservedObject var gamepads = GamepadManager.shared
    @AppStorage(DefaultsKey.gamepadUIEnabled) var gamepadUIEnabled = true
    /// When the switch above takes over — read (and shown) only while it is on.
    @AppStorage(DefaultsKey.gamepadUIMode) var gamepadUIMode =
        GamepadUIEnvironment.modeWhenConnected
    /// The gamepad UI's background palette. Edited here on tvOS only (`controllersSection`) —
    /// every other platform reaches it through the gamepad settings screen, which an Apple TV
    /// without a controller cannot open.
    @AppStorage(DefaultsKey.uiPalette) var uiPalette = "violet"
    @AppStorage(DefaultsKey.autoWake) var autoWakeEnabled = true
    @AppStorage(DefaultsKey.backgroundKeepAlive) var backgroundKeepAlive = false
    @AppStorage(DefaultsKey.backgroundTimeoutMinutes) var backgroundTimeoutMinutes = 10
    #if !os(tvOS)
    // Keyboard & mouse forwarding (macOS + a hardware keyboard/mouse on iPad). Invert-scroll flips
    // both wheel axes; modifier-layout relocates the ⌥/⌘ → Alt/Super roles by physical position.
    @AppStorage(DefaultsKey.invertScroll) var invertScroll = false
    @AppStorage(DefaultsKey.modifierLayout) var modifierLayout = ModifierLayout.mac.rawValue
    #endif
    // The quick-action ring's blob — every platform, tvOS included (the pad opens the ring there).
    @AppStorage(DefaultsKey.overlayActions) var overlayActions = ""
    /// The quick-actions editor, as a sheet (the detail column is not a NavigationStack).
    @State var showQuickActions = false
    #if DEBUG && !os(tvOS)
    @State var showControllerTest = false
    #endif
    #if os(iOS)
    @AppStorage(DefaultsKey.pointerCapture) var pointerCapture = true
    @AppStorage(DefaultsKey.touchMode) var touchMode = TouchInputMode.trackpad.rawValue
    @AppStorage(DefaultsKey.rumbleOnDevice) var rumbleOnDevice = false
    @AppStorage(DefaultsKey.gyroFromDevice) var gyroFromDevice = false
    // The sidebar selection drives the detail pane on iPad and the pushed sub-page on iPhone.
    // Width class decides the initial value: nil on iPhone (show the category list first),
    // General on iPad (a two-column layout should never open with an empty detail).
    @Environment(\.horizontalSizeClass) private var horizontalSizeClass
    @State private var settingsSelection: SettingsCategory?
    // Tracked so the detail can show its own Done whenever the sidebar (and its Done) is off screen
    // — not just on iPhone, but on any iPad layout that collapses the sidebar to an overlay. Starts
    // .doubleColumn so iPad reliably opens with the sidebar (and its Done) visible.
    @State private var columnVisibility: NavigationSplitViewVisibility = .doubleColumn
    // Sticky once the wheel lands on "Custom…", so editing a width/height that briefly equals a
    // preset doesn't snap the wheel back off Custom. A stored non-preset value reads as custom even
    // when this is false (see `isCustomResolution`), so it survives relaunches without persisting.
    @State var customMode = false
    #endif
    #if os(tvOS)
    /// What the TV's pane shows: the Editing row's preset manager, or a category.
    enum TVPane: Hashable {
        case editing
        case category(SettingsCategory)
    }

    /// The system keyboard is up for the Custom bitrate row.
    @State var typingBitrate = false
    /// Focus on a sidebar row picks what the pane shows, as on a tab bar.
    @State private var tvPane: TVPane = .category(.general)
    @FocusState private var tvFocusedPane: TVPane?
    /// The focused row's caption: each row binds it in `described`, and the band shows it.
    @FocusState var tvCaption: SettingsCaption?
    #endif
    /// Steam Controller 2 passthrough (device tier). Every platform shows the row, so the
    /// storage sits outside the per-platform blocks.
    @AppStorage(DefaultsKey.sc2Capture) var sc2Capture = false
    #if os(macOS)
    @AppStorage(DefaultsKey.mouseMode) var mouseMode = MouseInputMode.capture.rawValue
    /// Cross-client `inhibit_shortcuts` — here, the ⌘-chord passthrough (⌘Q & co. reach the host
    /// instead of the app menu while captured). macOS-only: it is the one platform whose window
    /// system hands a plain app no keyboard grab, so the client has to claim the chords itself.
    @AppStorage(DefaultsKey.inhibitShortcuts) var inhibitShortcuts = true
    @AppStorage(DefaultsKey.speakerUID) var speakerUID = ""
    @AppStorage(DefaultsKey.micUID) var micUID = ""
    @AppStorage(DefaultsKey.micChannel) var micChannel = 0
    @State var outputDevices: [AudioDevice] = []
    @State var inputDevices: [AudioDevice] = []
    // Input channels of the selected mic — drives the "Microphone channel" picker, which only
    // appears for a multi-channel interface (>1). 0 until the Audio tab loads it.
    @State var micChannelCount = 0
    #endif

    #if os(iOS)
    /// `initialCategory` is nil in the app (the list opens un-selected on iPhone; iPad lands on
    /// General via `onAppear`). The screenshot harness passes an explicit category so the captured
    /// shot opens on a real settings page (a populated detail) rather than the bare category list.
    init(initialCategory: SettingsCategory? = nil) {
        _settingsSelection = State(initialValue: initialCategory)
    }
    #elseif os(tvOS)
    #if DEBUG
    /// Shot harness: the sidebar sits out focus, so focus starts on the pane's first row.
    var shotFocusesPane = false
    /// Shot harness: the dial's editor opens a slot's list and goes back.
    var shotPushPop = false
    #endif

    /// The app opens on General in Default settings; the screenshot harness opens a specific
    /// pane and layer.
    init(
        initialCategory: SettingsCategory = .general, initialScope: SettingsScope = .defaults,
        startsOnEditing: Bool = false
    ) {
        _tvPane = State(initialValue: startsOnEditing ? .editing : .category(initialCategory))
        _scope = State(initialValue: initialScope)
    }
    #endif

    var body: some View {
        #if os(tvOS)
        // No inline text entry on a TV: values are picked from pushed lists, as in the system
        // Settings app.
        tvBody
        #elseif os(macOS)
        macBody
        #else
        iosBody
        #endif
    }

    // MARK: - macOS: tabbed preferences

    #if os(macOS)
    /// The preferences tabs, tagged so the scope control can sit out the one page that isn't a
    /// settings layer at all.
    private enum MacTab: Hashable {
        case general, display, input, audio, controllers, about
    }

    private var macBody: some View {
        // The scope control heads the window — above the tabs, because it is about which layer
        // every tab is editing, not about any one of them. About is the exception: it edits
        // nothing, and directly above the acknowledgements the switcher read as belonging to
        // them.
        VStack(alignment: .leading, spacing: 0) {
            if macTab != .about {
                scopeSwitcher
                    .padding(.horizontal, 20)
                    .padding(.top, 14)
                    .padding(.bottom, 10)
            }
            macTabs
        }
        .frame(width: 500, height: 580)
    }

    private var macTabs: some View {
        // Tab map mirrors SettingsCategory: General = session/app behavior, Display = the whole
        // picture (resolution lives here), Input = keyboard & mouse.
        TabView(selection: $macTab) {
            Form {
                sessionSection
                overlaySection
                librarySection
            }
            .formStyle(.grouped)
            .tabItem { Label("General", systemImage: "gearshape") }
            .tag(MacTab.general)

            Form {
                resolutionSection
                qualitySection
                presentationSection
                hostOutputSection
            }
            .formStyle(.grouped)
            .tabItem { Label("Display", systemImage: "display") }
            .tag(MacTab.display)

            Form {
                inputSection
            }
            .formStyle(.grouped)
            .tabItem { Label("Input", systemImage: "keyboard") }
            .tag(MacTab.input)

            Form {
                audioSection
            }
            .formStyle(.grouped)
            .onAppear {
                outputDevices = AudioDevices.outputs()
                inputDevices = AudioDevices.inputs()
                micChannelCount = AudioDevices.inputChannelCount(forUID: micUID)
            }
            .onChange(of: micUID) { _, newUID in
                // A different mic → different channel count; drop a now-out-of-range pin to Auto.
                micChannelCount = AudioDevices.inputChannelCount(forUID: newUID)
                if micChannel > micChannelCount { micChannel = 0 }
            }
            .tabItem { Label("Audio", systemImage: "speaker.wave.2") }
            .tag(MacTab.audio)

            Form {
                controllersSection
            }
            .formStyle(.grouped)
            .onAppear {
                gamepads.refresh()
                gamepads.startDiscovery()
            }
            .onDisappear { gamepads.stopDiscovery() }
            .tabItem { Label("Controllers", systemImage: "gamecontroller") }
            .tag(MacTab.controllers)

            AboutView()
                .tabItem { Label("About", systemImage: "info.circle") }
                .tag(MacTab.about)
        }
    }
    #endif

    // MARK: - iOS / iPadOS: adaptive split view

    #if os(iOS)
    private var iosBody: some View {
        NavigationSplitView(columnVisibility: $columnVisibility) {
            List(selection: $settingsSelection) {
                // The scope control heads the category list: on iPhone this is the screen you
                // start on, and on iPad it stays visible beside whichever category is open — so
                // the layer being edited is never off screen while you edit it. The caption is
                // the section's FOOTER; in the row it wrapped to four lines.
                Section {
                    scopeRow
                } footer: {
                    // A List footer inherits the app's body font unless it's told otherwise, and
                    // 17pt Geist next to the 12pt footers everywhere else read as a different
                    // kind of text. Same style as every other footer in this surface.
                    Text(scopeCaption)
                        .font(.geist(12, relativeTo: .caption))
                        .foregroundStyle(.secondary)
                }
                ForEach(SettingsCategory.allCases) { category in
                    // On iPhone the split view collapses to a push list, but a selection List
                    // draws no disclosure indicator of its own — add one in compact width for the
                    // expected drill-in affordance. On iPad the selected row highlights instead, so
                    // the chevron is omitted there.
                    HStack {
                        Label(category.title, systemImage: category.symbol)
                        if horizontalSizeClass == .compact {
                            Spacer()
                            Image(systemName: "chevron.forward")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(.tertiary)
                                // Purely a drill-in affordance — the row's button trait already
                                // conveys "opens"; keep it out of the VoiceOver announcement.
                                .accessibilityHidden(true)
                        }
                    }
                    .tag(category)
                }
            }
            .navigationTitle("Settings")
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        } detail: {
            // NavigationSplitView hosts the detail in its own navigation context (its title bar),
            // so no inner NavigationStack — that would double the bar on iPad. On iPhone the split
            // view collapses to one stack and pushes this when a row is tapped. `?? .general` only
            // backs the brief pre-selection window; the list never auto-pushes on a nil selection.
            settingsDetail(settingsSelection ?? .general)
                // Keep a Done on the detail whenever the sidebar (and its Done) isn't on screen: the
                // iPhone push, or any iPad layout that collapsed the sidebar to an overlay. When the
                // sidebar is showing, its Done is the only one — so this stays hidden to avoid two.
                .toolbar {
                    if horizontalSizeClass == .compact || columnVisibility == .detailOnly {
                        ToolbarItem(placement: .confirmationAction) {
                            Button("Done") { dismiss() }
                        }
                    }
                }
        }
        .onAppear {
            if horizontalSizeClass == .regular, settingsSelection == nil {
                settingsSelection = .general
            }
            gamepads.refresh()
            gamepads.startDiscovery()
        }
        // A regular→regular launch sets the default above; this catches a compact→regular change
        // (e.g. an iPad leaving narrow split-screen multitasking) so the detail pane fills in.
        .onChange(of: horizontalSizeClass) { _, newValue in
            if newValue == .regular, settingsSelection == nil {
                settingsSelection = .general
            }
        }
        .onDisappear { gamepads.stopDiscovery() }
    }

    @ViewBuilder
    private func settingsDetail(_ category: SettingsCategory) -> some View {
        switch category {
        case .general:
            Form {
                sessionSection
                overlaySection
                librarySection
            }
            .formStyle(.grouped)
            .navigationTitle("General")
            .navigationBarTitleDisplayMode(.inline)
        case .display:
            Form {
                resolutionSection
                qualitySection
                presentationSection
                hostOutputSection
            }
            .formStyle(.grouped)
            .navigationTitle("Display")
            .navigationBarTitleDisplayMode(.inline)
        case .input:
            Form {
                pointerSection
                inputSection
            }
            .formStyle(.grouped)
            .navigationTitle("Input")
            .navigationBarTitleDisplayMode(.inline)
        case .audio:
            Form { audioSection }
                .formStyle(.grouped)
                .navigationTitle("Audio")
                .navigationBarTitleDisplayMode(.inline)
        case .controllers:
            Form { controllersSection }
                .formStyle(.grouped)
                .navigationTitle("Controllers")
                .navigationBarTitleDisplayMode(.inline)
        case .about:
            // The identity card; the license wall is one push further in. Inline title to match
            // the five sibling detail pages (it would otherwise inherit the large title from the
            // "Settings" sidebar root).
            AboutView()
                .navigationTitle("About")
                .navigationBarTitleDisplayMode(.inline)
        }
    }
    #endif

    // MARK: - tvOS: a sidebar of categories

    #if os(tvOS)
    /// The Editing row and the categories beside the chosen pane, the focused row's caption in a
    /// band under it (design/apple-tvos-ui-overhaul.md §2.2). Two focus sections side by side:
    /// focus on a sidebar row picks the pane, as on a tab bar, and a swipe right enters it.
    private var tvBody: some View {
        presetPrompts(
            HStack(alignment: .top, spacing: 48) {
                tvSidebar
                    .frame(width: 460)
                    .focusSection()
                    #if DEBUG
                    .disabled(shotFocusesPane)
                    #endif
                VStack(spacing: 0) {
                    tvPaneContent
                        .tvPaneRoom()
                    SettingsCaptionBand(caption: tvCaption)
                }
                .frame(maxWidth: .infinity)
                .focusSection()
            }
            .padding(.horizontal, 60))
            // Focus enters on the chosen pane, so coming back finds the category left open.
            .defaultFocus($tvFocusedPane, tvPane)
            .onChange(of: tvFocusedPane) { _, pane in
                if let pane { tvPane = pane }
            }
            .onAppear {
                gamepads.refresh()
                gamepads.startDiscovery()
            }
            .onDisappear { gamepads.stopDiscovery() }
    }

    private var tvSidebar: some View {
        VStack(alignment: .leading, spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                // The layer every category below edits, never off screen while you edit it.
                Button {
                    tvPane = .editing
                } label: {
                    HStack(spacing: 14) {
                        scopeDot
                        VStack(alignment: .leading, spacing: 2) {
                            Text("Editing")
                                .font(.geist(20, relativeTo: .caption))
                                .foregroundStyle(.secondary)
                            Text(scopeName)
                                .lineLimit(1)
                        }
                        Spacer(minLength: 16)
                        if tvPane == .editing {
                            Image(systemName: "chevron.forward")
                                .foregroundStyle(.secondary)
                        }
                    }
                }
                .buttonStyle(TVSidebarRowStyle(chosen: tvPane == .editing))
                .focused($tvFocusedPane, equals: .editing)
                .padding(.bottom, 12)
                ForEach(SettingsCategory.allCases) { category in
                    Button {
                        tvPane = .category(category)
                    } label: {
                        HStack {
                            Label(category.title, systemImage: category.symbol)
                            Spacer(minLength: 16)
                            if tvPane == .category(category) {
                                Image(systemName: "chevron.forward")
                                    .foregroundStyle(.secondary)
                            }
                        }
                    }
                    .buttonStyle(TVSidebarRowStyle(chosen: tvPane == .category(category)))
                    .focused($tvFocusedPane, equals: .category(category))
                }
            }
            .tvSidebarCard()
            Spacer(minLength: 0)
        }
    }

    @ViewBuilder private var tvPaneContent: some View {
        switch tvPane {
        case .editing:
            tvPresetManager
        case .category(.general):
            Form {
                sessionSection
                overlaySection
                librarySection
            }
        case .category(.display):
            Form {
                resolutionSection
                qualitySection
                presentationSection
                hostOutputSection
            }
        case .category(.audio):
            Form { audioSection }
        case .category(.controllers):
            Form { controllersSection }
        case .category(.quickActions):
            tvQuickActions
        case .category(.about):
            AboutView()
        }
    }

    private var tvQuickActions: TVQuickActionsEditor {
        var editor = TVQuickActionsEditor(
            blob: scoped(SettingsFields.overlayActions),
            overridden: isOverridden("overlay_actions")
        ) {
            if inPresetScope {
                resetOverride("overlay_actions")
            } else {
                scoped(SettingsFields.overlayActions).wrappedValue = ""
            }
        }
        #if DEBUG
        editor.shotPushPop = shotPushPop
        #endif
        return editor
    }

    /// The Editing pane: which layer the categories edit, and the edited preset's own acts as
    /// rows — the iPhone's scope menu, without a menu inside a menu.
    private var tvPresetManager: some View {
        Form {
            Section {
                tvLayerRow(.defaults, name: "Default settings", color: nil, detail: nil)
                ForEach(presets.presets) { preset in
                    tvLayerRow(
                        .preset(preset.id), name: preset.name, color: preset.accentColor,
                        detail: tvUsage(of: preset))
                }
                Button {
                    presetDraft = .create()
                } label: {
                    Label("New Preset…", systemImage: "plus")
                }
            } header: {
                Text("Editing")
            } footer: {
                Text(scopeCaption)
            }
            if let active = activePreset {
                Section(active.name) {
                    Button {
                        presetDraft = .edit(active)
                    } label: {
                        Label("Name and Color…", systemImage: "pencil")
                    }
                    Button {
                        presetDraft = .duplicate(
                            active, name: Self.copyName(of: active.name, in: presets))
                    } label: {
                        Label("Duplicate…", systemImage: "plus.square.on.square")
                    }
                    Button(role: .destructive) {
                        presetPendingDelete = active
                    } label: {
                        Label("Delete…", systemImage: "trash")
                    }
                }
            }
        }
    }

    private func tvLayerRow(
        _ layer: SettingsScope, name: String, color: Color?, detail: String?
    ) -> some View {
        Button {
            scope = layer
        } label: {
            HStack(spacing: 16) {
                if let color {
                    Circle().fill(color).frame(width: 18, height: 18)
                } else {
                    Image(systemName: "gearshape")
                }
                Text(name)
                Spacer(minLength: 16)
                if let detail {
                    Text(detail).foregroundStyle(.secondary)
                }
                if scope == layer {
                    Image(systemName: "checkmark")
                }
            }
        }
    }

    /// What a delete would change, said before anyone asks: bound hosts and pinned cards.
    private func tvUsage(of preset: StreamPreset) -> String {
        let (bound, pinned) = presets.usage(of: preset.id)
        var parts: [String] = []
        if bound > 0 { parts.append(bound == 1 ? "1 host" : "\(bound) hosts") }
        if pinned > 0 { parts.append(pinned == 1 ? "1 card" : "\(pinned) cards") }
        return parts.isEmpty ? "Not used" : parts.joined(separator: " · ")
    }
    #endif
}
