// The option lists every settings surface renders from — one source of truth shared by the
// touch/desktop SettingsView (Pickers) and the tvOS pushed selection rows. Pure data + small pure
// helpers; anything that
// reads live view state (e.g. the bitrate slider mapping) stays on SettingsView.

#if os(macOS)
import AppKit
#endif
import PunktfunkKit
import SwiftUI

enum SettingsOptions {
    /// Compositor choices — the `tag` is the wire value (`PunktfunkConnection.Compositor` raw).
    static let compositors: [(label: String, tag: Int)] = [
        (String(localized: L10n.playerCompositorAuto), 0),
        ("KWin (KDE Plasma)", 1),
        ("Mutter (GNOME)", 3),
        ("Hyprland", 5),
        ("niri", 7),
        ("Umbriel", 8),
        ("wlroots (Sway / River)", 2),
        ("gamescope", 4),
    ]

    static let audioChannels: [(label: String, tag: Int)] = [
        (String(localized: L10n.playerAudioChannels2), 2),
        (String(localized: L10n.playerAudioChannels6), 6),
        (String(localized: L10n.playerAudioChannels8), 8),
    ]

    /// Audio format (`DefaultsKey.audioFormat`) — the `tag` is an `AudioFormatChoice` raw value.
    /// Standard is the default; every lossless row is a per-session opt-in that spends real
    /// bandwidth outside the ABR loop, and the host has its own switch which is also off by
    /// default.
    ///
    /// Ordered by rate rather than by family, because that is the order a listener reads a rate
    /// ladder in — the two families interleave (44.1, 48, 88.2, 96, 176.4) and grouping them would
    /// put 88.2 above 48.
    ///
    /// **Offered at every channel count**, unlike before. This picker used to be hidden unless the
    /// session was stereo, on the grounds that a surround frame does not fit one datagram — but the
    /// frame ladder is sized per channel count (`pcm::frame_us_for`), so 5.1 and 7.1 simply
    /// negotiate SHORTER frames: 48 kHz/16-bit 5.1 lands around 2 ms and 48/24 shorter still, which
    /// is a higher packet rate rather than an impossibility. What genuinely fits no rung at the
    /// default MTU is surround above 48 kHz, and the honest treatment of that is the caption plus
    /// the host's own decline — not a hidden row, which silently discards a choice the user made
    /// and explains nothing.
    static let audioFormats: [(label: String, tag: String)] = [
        (String(localized: L10n.playerAudioFormatOpus), AudioFormatChoice.opus.rawValue),
        (String(localized: L10n.playerAudioFormatLossless441),
         AudioFormatChoice.lossless441.rawValue),
        (String(localized: L10n.playerAudioFormatLossless48),
         AudioFormatChoice.lossless48.rawValue),
        (String(localized: L10n.playerAudioFormatLossless882),
         AudioFormatChoice.lossless882.rawValue),
        (String(localized: L10n.playerAudioFormatLossless96),
         AudioFormatChoice.lossless96.rawValue),
        (String(localized: L10n.playerAudioFormatLossless1764),
         AudioFormatChoice.lossless1764.rawValue),
    ]

    /// Virtual-pad types — the `tag` is the wire value (`PunktfunkConnection.GamepadType` raw).
    /// This picks the pad the HOST presents to the game, so it does not depend on what this
    /// device can capture. Keep in step with pf-console-ui's `PAD_TYPES` and the Linux shell's
    /// `GAMEPADS`: a preset written on one client opens on another, and a tag missing here has
    /// no index in the picker.
    static let padTypes: [(label: String, tag: Int)] = [
        (String(localized: L10n.playerGamepadAuto), 0),
        ("Xbox 360", 1),
        ("Xbox One", 3),
        ("DualSense", 2),
        ("DualShock 4", 4),
        ("Steam Deck", 6),
    ]

    /// System-button routing (the cross-client `system_buttons` key): where the guide
    /// (Xbox/PS) and share presses land while streaming. Auto = forward on Apple.
    static let systemButtons: [(label: String, tag: String)] = [
        (String(localized: L10n.playerSystemButtonsAuto), "auto"),
        (String(localized: L10n.playerSystemButtonsForward), "forward"),
        (String(localized: L10n.playerSystemButtonsLocal), "local"),
    ]

    /// The hold-Select guide gesture (the cross-client `guide_gesture` key). Auto = on
    /// everywhere but macOS.
    static let guideGestures: [(label: String, tag: String)] = [
        (String(localized: L10n.playerGuideGestureAuto), "auto"),
        (String(localized: L10n.playerGuideGestureOn), "on"),
        (String(localized: L10n.playerGuideGestureOff), "off"),
    ]

    static let hudPlacements: [(label: String, tag: String)] =
        HUDPlacement.allCases.map { ($0.label, $0.rawValue) }

    /// The overlay's size on top of the system's text size — core's `STATS_SCALE_PCTS`.
    static let statsScales: [(label: String, tag: Int)] =
        [75, 100, 125, 150, 175, 200].map { (L10n.commonPercent(value: String($0)), $0) }

    /// The macOS Fullscreen picker. "always" is `fullscreenAlways`; the other two are the
    /// presetable `fullscreenWhileStreaming`.
    static let fullscreenModes: [(label: String, tag: String)] = [
        (String(localized: L10n.playerFullscreenOff), "off"),
        (String(localized: L10n.playerFullscreenStream), "stream"),
        (String(localized: L10n.playerFullscreenAlways), "always"),
    ]

    /// When the gamepad UI takes over (`DefaultsKey.gamepadUIMode`) — only meaningful while
    /// `gamepadUIEnabled` is on, so every surface that offers it hides the row when the switch
    /// is off rather than showing a picker that decides nothing.
    static let gamepadUIModes: [(label: String, tag: String)] = [
        (String(localized: L10n.playerGamepadUiModeConnected), GamepadUIEnvironment.modeWhenConnected),
        (String(localized: L10n.playerGamepadUiModeAlways), GamepadUIEnvironment.modeAlways),
    ]

    /// Presentation intent (`DefaultsKey.presentPriority` — the 2026-07 rebuild that replaced
    /// the visible stage picker with intent; see SessionPresenter's PresentPriority and
    /// design/apple-presentation-rebuild.md). The stage ladder survives only as the hidden
    /// PUNKTFUNK_PRESENTER debug env lever.
    static let presentPriorities: [(label: String, tag: String)] = [
        (String(localized: L10n.playerPresentPriorityLatency), "latency"),
        (String(localized: L10n.playerPresentPrioritySmooth), "smooth"),
    ]

    /// Smoothness's jitter-buffer sizes (`DefaultsKey.smoothBuffer`; 0 = Automatic, currently 2
    /// frames). The ms hints derive from the chosen refresh setting — each buffered frame costs
    /// about one refresh interval of display latency and absorbs about one interval of arrival
    /// jitter.
    static func smoothBuffers(refreshHz: Int) -> [(label: String, tag: Int)] {
        let periodMs = 1000.0 / Double(max(24, refreshHz))
        func cost(_ frames: Int) -> String {
            L10n.playerSmoothBufferFramesCost(
                count: frames, ms: String(format: "%.0f", Double(frames) * periodMs))
        }
        return [
            (String(localized: L10n.playerSmoothBufferAutomatic), 0),
            (cost(1), 1),
            (cost(2), 2),
            (cost(3), 3),
        ]
    }

    /// Stats-overlay tiers (`DefaultsKey.statsVerbosity`) — the `tag` is the raw value.
    static let statsVerbosities: [(label: String, tag: String)] =
        StatsVerbosity.allCases.map { ($0.label, $0.rawValue) }

    /// Video-codec preference (`DefaultsKey.codec`) — a soft preference the host falls back from.
    /// AV1 appears only on devices with an AV1 hardware decoder (the same
    /// `AV1.hardwareDecodeSupported` gate SessionModel advertises by) — elsewhere it would be a
    /// dead setting the host could never honor. Ordered by the host's resolve precedence
    /// (HEVC > AV1 > H.264).
    ///
    /// [current] is the stored value, kept listed even when this device can't decode it — a
    /// preset made on another device carries one here — and then labelled with the reason.
    /// Dropping it silently leaves a blank picker over a session that streams HEVC.
    static func codecs(current: String) -> [(label: String, tag: String)] {
        var options: [(label: String, tag: String)] = [
            (String(localized: L10n.playerCodecAuto), "auto"),
            ("HEVC", "hevc"),
            ("H.264", "h264"),
        ]
        if AV1.hardwareDecodeSupported {
            options.insert(("AV1", "av1"), at: 2)
        } else if current == "av1" {
            options.insert((String(localized: L10n.playerCodecAv1Unsupported), "av1"), at: 2)
        }
        // PyroWave is the opt-in wired-LAN low-latency codec (100–400 Mbps all-intra wavelet,
        // 8-bit SDR): selecting it advertises + prefers it for the session. Offered only when
        // the Metal decode probe passes (same gate SessionModel advertises by) — elsewhere the
        // host could never emit it.
        if MetalWaveletDecoder.supported {
            options.append((String(localized: L10n.playerCodecPyrowave), "pyrowave"))
        } else if current == "pyrowave" {
            options.append((String(localized: L10n.playerCodecPyrowaveUnsupported), "pyrowave"))
        }
        return options
    }

    // MARK: - Bitrate

    /// Discrete bitrate steps for the surfaces with no Slider (tvOS pushed pickers, the gamepad
    /// settings' left/right cycling), up to the same 3 Gbit/s ceiling the slider has. 5 Mbit/s
    /// apart to 50, where most links settle.
    static let bitratePresets: [(label: String, tag: Int)] =
        [(label: String(localized: L10n.playerBitrateKbpsAutomatic), tag: 0)]
        + bitrateSteps.map { (label: PyroWaveQuality.rateLabel(kbps: UInt32($0)), tag: $0) }

    private static let bitrateSteps = [
        5_000, 10_000, 15_000, 20_000, 25_000, 30_000, 35_000, 40_000, 45_000, 50_000,
        60_000, 80_000, 100_000, 150_000, 300_000, 500_000,
        1_000_000, 1_500_000, 2_000_000, 3_000_000,
    ]

    /// A stored rate none of the presets hold: typed, slid, or written by the speed test.
    static func isCustomBitrate(_ kbps: Int) -> Bool {
        !bitratePresets.contains { $0.tag == kbps }
    }

    /// A typed rate in whole Mbit/s as kbps, capped at the slider's 3 Gbit/s top. nil for empty or
    /// `0`, which leave the stored rate alone — Automatic is a list entry, not a number.
    static func customBitrateKbps(_ text: String) -> Int? {
        guard let mbps = Int(text.prefix(4)), mbps > 0 else { return nil }
        return min(mbps, 3_000) * 1_000
    }

    /// The presets plus the currently stored value when it isn't one of them (set via the touch
    /// slider or a synced device) — so the current choice stays visible/selectable.
    static func bitrateOptions(current: Int) -> [(label: String, tag: Int)] {
        var options = bitratePresets
        if !options.contains(where: { $0.tag == current }) {
            options.insert(
                (L10n.playerBitrateKbpsCustomValue(value: SpeedTestView.mbpsLabel(kbps: current)),
                 current),
                at: 1)
        }
        return options
    }

    // MARK: - Controllers

    /// "Use controller" choices: Automatic, every forwardable controller, and — so a stale pin
    /// stays visible instead of leaving the selection tag-less — any pinned id that is NOT among
    /// the selectable (extended) entries, present-but-unusable included.
    @MainActor
    static func controllerOptions(_ gamepads: GamepadManager) -> [(label: String, tag: String)] {
        let selectable = gamepads.controllers.filter(\.isExtended)
        var options: [(label: String, tag: String)] = [
            (String(localized: L10n.playerForwardPadAuto), "")
        ]
        options += selectable.map { ($0.name, $0.id) }
        if !gamepads.preferredID.isEmpty,
           !selectable.contains(where: { $0.id == gamepads.preferredID }) {
            options.append((String(localized: L10n.playerForwardPadSaved), gamepads.preferredID))
        }
        return options
    }

    // MARK: - Stream mode (iOS/macOS pickers + the gamepad settings rows on all three; the
    // touch/remote tvOS SettingsView builds its own preset list)

    /// The aspect switch's entries: this device's screen and safe area first when no standard
    /// shape has them (a phone), then the standard families — see `Resolutions.families`.
    @MainActor
    static func families() -> [Resolutions.Aspect] {
        let own = nativeModes()
        return Resolutions.families(
            screen: own.first.map { (w: $0.w, h: $0.h) },
            safe: own.dropFirst().first.map { (w: $0.w, h: $0.h) })
    }

    /// The entry a picker lists for a stored size: its shape, with native (`0`) under this
    /// device's own screen where it has one; otherwise, and for a shape none has, the first entry.
    @MainActor
    static func family(width: Int, height: Int) -> Int {
        let families = families()
        if width == 0 {
            return families.firstIndex { $0.id == Resolutions.screenID } ?? 0
        }
        return Resolutions.familyOf(families, width, height) ?? 0
    }

    /// What a row of the resolution list is. The label is built from the kind, so no code reads
    /// a row's identity out of its text.
    enum ModeKind: Hashable {
        case native, size
        case thisDevice, thisDeviceSafeArea
        case thisDisplay, thisDisplayOverNotch, thisDisplayBelowNotch
    }

    /// A row's label: the kind's name with its size, or the bare size for a standard one.
    static func modeLabel(_ kind: ModeKind, w: Int, h: Int) -> String {
        let (width, height) = (String(w), String(h))
        switch kind {
        case .native: return String(localized: L10n.playerResolutionNative)
        case .size: return "\(w) × \(h)"
        case .thisDevice:
            return L10n.appleResolutionThisDeviceSize(width: width, height: height)
        case .thisDeviceSafeArea:
            return L10n.appleResolutionThisDeviceSafeAreaSize(width: width, height: height)
        case .thisDisplay:
            return L10n.appleResolutionThisDisplaySize(width: width, height: height)
        case .thisDisplayOverNotch:
            return L10n.appleResolutionThisDisplayOverNotchSize(width: width, height: height)
        case .thisDisplayBelowNotch:
            return L10n.appleResolutionThisDisplayBelowNotchSize(width: width, height: height)
        }
    }

    /// Native (`0 × 0`, the display resolved at connect), this device's other native mode (a
    /// phone's safe area, a notched Mac's whole panel — never the one Native already is), then
    /// one entry's sizes from `families()`, deduped by dimensions (native wins a tie).
    @MainActor
    static func resolutionModes(family: Int) -> [(kind: ModeKind, w: Int, h: Int)] {
        let entries = families()
        let sizes = entries[min(family, entries.count - 1)].sizes.map {
            (kind: ModeKind.size, w: $0.w, h: $0.h)
        }
        let native = NativeDisplay.mode
        let others = nativeModes().filter { $0.w != native.width || $0.h != native.height }
        var seen = Set<String>()
        return ([(kind: ModeKind.native, w: 0, h: 0)] + others + sizes)
            .filter { seen.insert("\($0.w)x\($0.h)").inserted }
    }

    /// A refresh rate's label: `0` is Native.
    static func refreshLabel(_ hz: Int) -> String {
        hz == 0
            ? String(localized: L10n.playerRefreshHzNative) : L10n.commonHz(value: String(hz))
    }

    /// A typed "2560 × 1440" (any separator) through the shared rule; nil without two numbers.
    static func typedSize(_ text: String, codec: String) -> (w: Int, h: Int)? {
        let numbers = text.split(whereSeparator: { !$0.isNumber }).compactMap { Int($0) }
        guard numbers.count >= 2, numbers[0] > 0, numbers[1] > 0 else { return nil }
        return Resolutions.custom(numbers[0], numbers[1], codec: codec)
    }

    /// This device's own modes: the screen, then its safe-area variant where it differs.
    ///
    /// On iOS the safe-area variant is the same mode narrowed so the picture clears the sensor
    /// housing and the rounded corners — see [`SafeDisplay`] for why a narrower mode is the whole
    /// fix. A notched Mac gets the same pair from [`macDisplayModes`], shortened instead of
    /// narrowed. A device with no housing has no second mode: an odd panel would otherwise
    /// differ from its even-floored self.
    @MainActor
    static func nativeModes() -> [(kind: ModeKind, w: Int, h: Int)] {
        var native: [(kind: ModeKind, w: Int, h: Int)] = []
        #if !os(macOS)
        let mode = NativeDisplay.mode
        native = [(.thisDevice, mode.width, mode.height)]
        #if os(iOS)
        let inset = mainWindowSideInset()
        if inset > 0 {
            let safe = SafeDisplay.mode(
                nativeWidth: mode.width, nativeHeight: mode.height,
                sideInsetPoints: inset, scale: NativeDisplay.screen.nativeScale)
            native.append((.thisDeviceSafeArea, safe.width, safe.height))
        }
        #endif
        #else
        native = macDisplayModes()
        #endif
        var seen = Set<String>()
        return native.filter { seen.insert("\($0.w)x\($0.h)").inserted }
    }

    #if os(macOS)
    /// This display's real modes: the PANEL first, then — on a notched Mac — the variant that
    /// clears the camera housing, which is Native's mode there (`NativeDisplay.mode`): the one
    /// native fullscreen shows whole and scans out Direct. The panel is named for the housing it
    /// covers only on a Mac that has one.
    ///
    /// The two are deduped here, so a second entry means "this display has a housing" and a caller
    /// can offer the choice on exactly the Macs that have one.
    @MainActor
    static func macDisplayModes() -> [(kind: ModeKind, w: Int, h: Int)] {
        guard let screen = NSScreen.main else { return [] }
        let panel = screen.panelPixelSize
        let safe = screen.notchSafePixelSize
        guard panel.height > safe.height else {
            return [(kind: .thisDisplay, w: panel.width, h: panel.height)]
        }
        return [
            (kind: .thisDisplayOverNotch, w: panel.width, h: panel.height),
            (kind: .thisDisplayBelowNotch, w: safe.width, h: safe.height),
        ]
    }
    #endif

    #if os(iOS)
    /// The key window's per-side safe-area inset in points, resolved for the LANDSCAPE stream even
    /// when this settings screen is currently portrait (see `SafeDisplay.sideInsetPoints`).
    ///
    /// Zero when no window is up yet: `nativeModes` then offers no safe mode, which is the right
    /// answer for a device we can't measure.
    @MainActor
    private static func mainWindowSideInset() -> Double {
        let insets = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap(\.windows)
            .first { $0.isKeyWindow }?
            .safeAreaInsets
        guard let insets else { return 0 }
        return SafeDisplay.sideInsetPoints(
            left: Double(insets.left), right: Double(insets.right), top: Double(insets.top),
            isPhone: UIDevice.current.userInterfaceIdiom == .phone)
    }
    #endif

    /// Native (`0`), then the rates the device can actually display (no point asking the host to
    /// render frames the screen can't show), plus any stored custom value so it stays selectable.
    /// A Mac lists the desktop clients' rates; a phone or tablet the few its panels run at; a
    /// Vision Pro the rates its panels run at, which visionOS does not report.
    @MainActor
    static func refreshRates(including current: Int) -> [Int] {
        #if os(visionOS)
        // 90 is what Native asks for; M5 panels reach 120.
        let maxHz = 120
        let ladder = [60, 90, 120]
        #elseif !os(macOS)
        let maxHz = NativeDisplay.mode.hz
        let ladder = [60, 120, 240]
        #else
        let maxHz = NSScreen.main?.maximumFramesPerSecond ?? 60
        let ladder = [30, 60, 90, 120, 144, 165, 240]
        #endif
        var rates = ladder.filter { $0 <= maxHz }
        if rates.isEmpty { rates = [maxHz] }
        if current != 0, !rates.contains(current) { rates.append(current) }
        return [0] + rates.sorted()
    }
}
