// The settings ONE session runs on — the global defaults with the session's preset overlaid,
// resolved once at connect and read from there on (design/client-settings-profiles.md §4.2/§4.4):
//
//     effective = overlay(preset).apply(globals)
//     preset   = one-off pick (Connect with ▸)  ??  the launched title's binding
//                ??  host.presetID  ??  none
//
// Session readers read their own session's copy: `PunktfunkConnection.settings` in the kit,
// `SessionModel.settings` in the app. Never the globals mid-session, and never another window's.
//
// Only SESSION-CONSUMED values live here. Pure app-level preferences — the library toggle, the
// gamepad-UI switch, HUD placement, auto-wake, background keep-alive — stay plain `@AppStorage`
// where they are read: they are about the app, not about a stream.

import Foundation

public struct EffectiveSettings: Equatable, Sendable {
    // Tier P — presetable (design §3). A size or rate of 0 is Native, resolved by `streamMode`.
    public var width = SettingDefault.streamWidth
    public var height = SettingDefault.streamHeight
    public var refreshHz = SettingDefault.streamHz
    public var matchWindow = SettingDefault.matchWindow
    public var bitrateKbps = SettingDefault.bitrateKbps
    /// PyroWave quality, bits per pixel (`DefaultsKey.pyrowaveBpp`).
    public var pyrowaveBpp = SettingDefault.pyrowaveBpp
    public var renderScale = SettingDefault.renderScale
    /// A `VideoFit` raw value; unknown reads as fit.
    public var videoFit = SettingDefault.videoFit
    public var codec = SettingDefault.codec
    public var hdrEnabled = SettingDefault.hdrEnabled
    public var compositor = SettingDefault.compositor
    public var audioChannels = SettingDefault.audioChannels
    /// An `AudioFormatChoice` raw value. `"opus"` — the default — is byte-for-byte the session
    /// every build before the lossless plane ran.
    public var audioFormat = SettingDefault.audioFormat
    public var micEnabled = SettingDefault.micEnabled
    public var echoCancel = SettingDefault.echoCancel
    public var keepHostAudio = SettingDefault.keepHostAudio
    public var touchMode = SettingDefault.touchMode
    public var mouseMode = SettingDefault.mouseMode
    public var invertScroll = SettingDefault.invertScroll
    /// Cross-client `overlay_actions`: the ring blob, empty = the platform default.
    public var overlayActions = SettingDefault.overlayActions
    /// Cross-client `inhibit_shortcuts` (default on): system chords reach the host while input is
    /// captured. See `DefaultsKey.inhibitShortcuts` — on macOS this is the ⌘-chord passthrough.
    public var inhibitShortcuts = SettingDefault.inhibitShortcuts
    public var gamepadType = SettingDefault.gamepadType
    public var gamepadForwarding = SettingDefault.gamepadForwarding
    /// Steam Controller 2 as-is passthrough (`DefaultsKey.sc2Capture`, default off). Read at
    /// connect beside `gamepadForwarding`. Deliberately NOT presetable (no overlay field): the
    /// toggle is about hardware this device captures, not about how a host is streamed.
    public var sc2Capture = SettingDefault.sc2Capture
    /// Cross-client `system_buttons`: "auto" | "forward" | "local".
    public var systemButtons = SettingDefault.systemButtons
    /// Cross-client `guide_gesture`: "auto" | "on" | "off".
    public var guideGesture = SettingDefault.guideGesture
    /// A `StatsVerbosity` raw value; the enum lives in PunktfunkKit, which this module can't see.
    public var statsVerbosity = SettingDefault.statsVerbosity
    public var fullscreenWhileStreaming = SettingDefault.fullscreenWhileStreaming
    public var enable444 = SettingDefault.enable444
    /// Cross-client `ten_bit_sdr`. Subsumed by `hdrEnabled`, which advertises the depth already.
    public var tenBitSdr = SettingDefault.tenBitSdr
    public var presentPriority = SettingDefault.presentPriority
    public var smoothBuffer = SettingDefault.smoothBuffer
    public var vsync = SettingDefault.vsync
    public var allowVRR = SettingDefault.allowVRR
    public var modifierLayout = SettingDefault.modifierLayout
    // Tier G — this device's endpoints and hardware. Session-consumed, so they ride along, but
    // never presetable: a preset is about how a host is streamed, not about which speaker this
    // Mac uses.
    public var speakerUID = SettingDefault.speakerUID
    public var micUID = SettingDefault.micUID
    public var micChannel = SettingDefault.micChannel
    public var pointerCapture = SettingDefault.pointerCapture
    /// The preset this resolution came from, when one applied — the HUD names it so "which
    /// preset am I on?" is answerable mid-session, and the one-off/binding distinction never has
    /// to be guessed from the settings themselves.
    public var presetID: String?
    public var presetName: String?
    /// The preset's `#RRGGBB` chip colour. The HUD tints the name with it, matching the card
    /// that launched the session.
    public var presetAccent: String?

    public init() {}

    /// The global defaults, as every `@AppStorage` in the settings surface sees them. `.standard`
    /// by default — settings are per-device, unlike the App-Group-shared host store.
    public init(defaults: UserDefaults) {
        func int(_ key: String, _ fallback: Int) -> Int {
            defaults.object(forKey: key) as? Int ?? fallback
        }
        func dbl(_ key: String, _ fallback: Double) -> Double {
            defaults.object(forKey: key) as? Double ?? fallback
        }
        func bool(_ key: String, _ fallback: Bool) -> Bool {
            defaults.object(forKey: key) as? Bool ?? fallback
        }
        func str(_ key: String, _ fallback: String) -> String {
            defaults.string(forKey: key) ?? fallback
        }
        width = int(DefaultsKey.streamWidth, width)
        height = int(DefaultsKey.streamHeight, height)
        refreshHz = int(DefaultsKey.streamHz, refreshHz)
        matchWindow = bool(DefaultsKey.matchWindow, matchWindow)
        bitrateKbps = int(DefaultsKey.bitrateKbps, bitrateKbps)
        pyrowaveBpp = dbl(DefaultsKey.pyrowaveBpp, pyrowaveBpp)
        renderScale = dbl(DefaultsKey.renderScale, renderScale)
        videoFit = str(DefaultsKey.videoFit, videoFit)
        codec = str(DefaultsKey.codec, codec)
        hdrEnabled = bool(DefaultsKey.hdrEnabled, hdrEnabled)
        compositor = int(DefaultsKey.compositor, compositor)
        audioChannels = int(DefaultsKey.audioChannels, audioChannels)
        audioFormat = str(DefaultsKey.audioFormat, audioFormat)
        micEnabled = bool(DefaultsKey.micEnabled, micEnabled)
        echoCancel = bool(DefaultsKey.echoCancel, echoCancel)
        keepHostAudio = bool(DefaultsKey.keepHostAudio, keepHostAudio)
        touchMode = str(DefaultsKey.touchMode, touchMode)
        mouseMode = str(DefaultsKey.mouseMode, mouseMode)
        invertScroll = bool(DefaultsKey.invertScroll, invertScroll)
        overlayActions = str(DefaultsKey.overlayActions, overlayActions)
        inhibitShortcuts = bool(DefaultsKey.inhibitShortcuts, inhibitShortcuts)
        gamepadType = int(DefaultsKey.gamepadType, gamepadType)
        gamepadForwarding = bool(DefaultsKey.gamepadForwarding, gamepadForwarding)
        sc2Capture = bool(DefaultsKey.sc2Capture, sc2Capture)
        systemButtons = str(DefaultsKey.systemButtons, systemButtons)
        guideGesture = str(DefaultsKey.guideGesture, guideGesture)
        statsVerbosity = Self.storedStatsVerbosity(defaults)
        fullscreenWhileStreaming = bool(
            DefaultsKey.fullscreenWhileStreaming, fullscreenWhileStreaming)
        enable444 = bool(DefaultsKey.enable444, enable444)
        tenBitSdr = bool(DefaultsKey.tenBitSdr, tenBitSdr)
        presentPriority = str(DefaultsKey.presentPriority, presentPriority)
        smoothBuffer = int(DefaultsKey.smoothBuffer, smoothBuffer)
        vsync = bool(DefaultsKey.vsync, vsync)
        allowVRR = bool(DefaultsKey.allowVRR, allowVRR)
        modifierLayout = str(DefaultsKey.modifierLayout, modifierLayout)
        speakerUID = str(DefaultsKey.speakerUID, speakerUID)
        micUID = str(DefaultsKey.micUID, micUID)
        micChannel = int(DefaultsKey.micChannel, micChannel)
        pointerCapture = bool(DefaultsKey.pointerCapture, pointerCapture)
    }

    /// The stats tier as stored, migrating the pre-tier `hudEnabled` bool: written once, here,
    /// because the tier and the resolved settings disagreeing about a legacy install is a bug
    /// nobody would see until an old device upgraded.
    public static func storedStatsVerbosity(_ defaults: UserDefaults) -> String {
        if let raw = defaults.string(forKey: DefaultsKey.statsVerbosity) { return raw }
        if let legacy = defaults.object(forKey: DefaultsKey.hudEnabled) as? Bool, !legacy {
            return "off"
        }
        return SettingDefault.statsVerbosity
    }

    /// The `system_buttons` policy resolved for this platform: forward the raw guide (and
    /// share/QAM misc) presses? Auto = forward on every Apple platform — where the OS shows
    /// its own overlay for the press that is the OS's business, and suppressing our send
    /// would only break users who handed the button to the app (iOS 27's Home-button
    /// setting; macOS with the gestures claimed).
    public var systemButtonsForward: Bool {
        switch systemButtons {
        case "local": return false
        default: return true
        }
    }

    /// The hold-Select guide gesture resolved for this platform ([`guideGesture`]). Auto =
    /// on everywhere but macOS: iOS reserves the physical Home press (the Game Overlay,
    /// uncapturable pre-27) and tvOS never delivers it, so holding Select is the controller
    /// route to the host's guide — and, held on, to a Gaming-Mode host's QAM. On macOS the
    /// raw press reaches the host, so auto stays off and Select keeps its exact timing.
    public var guideGestureEnabled: Bool {
        switch guideGesture {
        case "on": return true
        case "off": return false
        default:
            #if os(macOS)
            return false
            #else
            return true
            #endif
        }
    }

    /// The one resolution seam: this overlay on top of these settings. Pure — no store reads, no
    /// clock — so it is testable field by field. A `.some` that happens to equal the base is a
    /// legitimate PIN: it keeps its value when the global later moves.
    public func applying(_ overlay: SettingsOverlay) -> EffectiveSettings {
        var s = self
        if let v = overlay.width { s.width = v }
        if let v = overlay.height { s.height = v }
        if let v = overlay.refreshHz { s.refreshHz = v }
        if let v = overlay.matchWindow { s.matchWindow = v }
        if let v = overlay.bitrateKbps { s.bitrateKbps = v }
        if let v = overlay.pyrowaveBpp { s.pyrowaveBpp = v }
        if let v = overlay.renderScale { s.renderScale = v }
        if let v = overlay.videoFit { s.videoFit = v }
        if let v = overlay.codec { s.codec = v }
        if let v = overlay.hdrEnabled { s.hdrEnabled = v }
        if let v = overlay.compositor { s.compositor = v }
        if let v = overlay.audioChannels { s.audioChannels = v }
        if let v = overlay.audioFormat { s.audioFormat = v }
        if let v = overlay.micEnabled { s.micEnabled = v }
        if let v = overlay.echoCancel { s.echoCancel = v }
        if let v = overlay.keepHostAudio { s.keepHostAudio = v }
        if let v = overlay.touchMode { s.touchMode = v }
        if let v = overlay.mouseMode { s.mouseMode = v }
        if let v = overlay.invertScroll { s.invertScroll = v }
        if let v = overlay.overlayActions { s.overlayActions = v }
        if let v = overlay.inhibitShortcuts { s.inhibitShortcuts = v }
        if let v = overlay.gamepadType { s.gamepadType = v }
        if let v = overlay.gamepadForwarding { s.gamepadForwarding = v }
        if let v = overlay.systemButtons { s.systemButtons = v }
        if let v = overlay.guideGesture { s.guideGesture = v }
        if let v = overlay.statsVerbosity { s.statsVerbosity = v }
        if let v = overlay.fullscreenWhileStreaming { s.fullscreenWhileStreaming = v }
        if let v = overlay.enable444 { s.enable444 = v }
        if let v = overlay.tenBitSdr { s.tenBitSdr = v }
        if let v = overlay.presentPriority { s.presentPriority = v }
        if let v = overlay.smoothBuffer { s.smoothBuffer = v }
        if let v = overlay.vsync { s.vsync = v }
        if let v = overlay.allowVRR { s.allowVRR = v }
        if let v = overlay.modifierLayout { s.modifierLayout = v }
        return s
    }

    /// The whole resolution, in one call, in the precedence every client shares:
    /// **one-off pick ?? host binding ?? none**. A dangling id — a preset deleted out from under
    /// a binding, a link naming one that no longer exists — resolves as none: never an error,
    /// never a blocked connect (§4.4).
    public static func resolve(
        host: StoredHost?, selection: PresetSelection = .inherit, launch: String? = nil,
        catalog: PresetCatalog? = nil, defaults: UserDefaults = .standard
    ) -> EffectiveSettings {
        let base = EffectiveSettings(defaults: defaults)
        let preset: StreamPreset? = {
            switch selection {
            case .defaults:
                return nil
            case .preset(let id):
                return (catalog ?? PresetCatalog.load()).preset(id: id)
            case .inherit:
                guard let host else { return nil }
                return (catalog ?? PresetCatalog.load()).binding(for: host, launch: launch)
            }
        }()
        guard let preset else { return base }
        var out = base.applying(preset.overrides)
        out.presetID = preset.id
        out.presetName = preset.name
        out.presetAccent = preset.accent
        return out
    }
}

// MARK: - Audio format

/// The audio format a session ASKS the host for (`DefaultsKey.audioFormat`) — one choice rather
/// than a free rate/depth pair, so the states that cannot be asked for are unrepresentable rather
/// than merely validated.
///
/// **The ladder is both rate families**, matching `punktfunk_core::audio::pcm::rate_is_supported`:
/// 44 100 / 48 000 / 88 200 / 96 000 / 176 400 Hz. The 44.1 kHz family used to be absent, and for
/// exactly one reason: every buffer figure in the jitter policy — at both ends and in all four
/// clients — was `ms × perMS` with `perMS` an INTEGER number of samples per millisecond, so
/// 44 100 → 44.1 truncated to 44 and put every target, every de-prime fuse and every reported
/// `buffer_ms` 2.3 % low. That arithmetic now multiplies before it divides (see the conversion
/// helpers at the top of `AudioRing.swift`, and `JitterPolicy::new_at_rate`), which is the whole of
/// what §4.1 deferred the family behind (design/hi-res-audio.md §4.1).
///
/// ⚠ **A rate being representable is not a promise that it will be granted**, and this surface must
/// never read as one. The host runs a five-condition gate and any failure resolves the session back
/// to Opus 48 kHz; on top of that the frame has to FIT one QUIC datagram, which the top of the
/// ladder only barely does — 176 400/24-bit stereo is 8.5 Mbps and fits only the shortest rung
/// (1 ms, ~1 069 B), and surround above 48 kHz fits no rung at all. What a session actually got is
/// `PunktfunkConnection`'s `resolvedAudioRateHz`/`resolvedAudioBits`/`resolvedAudioChannels`/
/// `isLosslessAudio`, which is what the HUD shows.
///
/// Lossless at the DEFAULT 48 kHz/16-bit is deliberately not offered: it spends ~1.5 Mbps to sound
/// like the transparent 256 kbps Opus it replaces, and it is the one lossless request whose wire
/// parameters are indistinguishable from a legacy one (it needs `CLIENT_CAP_AUDIO_HIRES` set by
/// hand — see the C ABI's note on that constant). 24-bit is where the plane earns its bandwidth.
public enum AudioFormatChoice: String, CaseIterable, Sendable {
    /// Opus 48 kHz — the default, and byte-for-byte the session every earlier build ran.
    case opus
    /// Bit-exact PCM at 44.1 kHz / 24-bit. ~2.1 Mbps. The CD family's base rate: what an ordinary
    /// Windows endpoint or a 44.1 kHz interface reports as its OWN engine rate, and the request
    /// that spares such a host a resample, exactly as `lossless48` does on a 48 kHz one.
    case lossless441
    /// Bit-exact PCM at 48 kHz / 24-bit. ~2.3 Mbps. The honest win even without a hi-res
    /// interface: no lossy stage at all, and no double resample on a 48 kHz host.
    case lossless48
    /// Bit-exact PCM at 88.2 kHz / 24-bit. ~4.2 Mbps — 96 kHz's counterpart in the 44.1 family, and
    /// the one to prefer over it on 44.1-derived material, since doubling is exact.
    case lossless882
    /// Bit-exact PCM at 96 kHz / 24-bit. ~4.6 Mbps, and only real if the host's capture endpoint
    /// genuinely runs at 96 kHz — the host declines rather than upsampling, and this client says so
    /// rather than claiming a rate its own output device refused.
    case lossless96
    /// Bit-exact PCM at 176.4 kHz / 24-bit — 8.5 Mbps, and the one row far more likely to be
    /// declined than granted. Three things have to go right: the host's bandwidth gate gives audio
    /// at most a quarter of the video budget, so the session needs ~34 Mbps of video before it is
    /// even considered; a stereo frame fits a QUIC datagram only on the ladder's shortest rung
    /// (1 ms — a thousand datagrams a second — at ~1 069 B, so any connection with a smaller
    /// datagram declines it) and a surround one fits no rung at all; and this device's output has
    /// to open the rate. Offered because it is reachable, not because it is likely — the HUD's
    /// `audio lossless …` line is what says which happened.
    case lossless1764

    /// The stored raw value, falling back to `.opus` for anything a newer build wrote.
    ///
    /// ⚠ **The raw values are shared VERBATIM with `pf_client_core::session::AUDIO_FORMATS` and the
    /// Android client's `AUDIO_FORMAT_*`, and must never be renamed.** One preset catalog
    /// round-trips through all four clients, and a spelling that differs by a single character
    /// fails in the worst possible way: the key is carried through untouched, so the preset keeps
    /// "working" on the other client and silently inherits its global default instead. The naming
    /// rule is the kHz figure with the decimal point dropped — `lossless48`, `lossless96`, and for
    /// the 44.1 family `lossless441` / `lossless882` / `lossless1764`. Left implicit (case name ==
    /// raw value) precisely so the two cannot drift apart here; `SharedFoundationTests` pins the
    /// resulting strings against the other clients' tables.
    public init(setting: String) {
        self = AudioFormatChoice(rawValue: setting) ?? .opus
    }

    /// The `Hello` fields this choice asks for. Anything other than `48 000`/`16` asks core for the
    /// `0xD3` plane and lets it derive `CLIENT_CAP_AUDIO_HIRES` from the format, so the bit and the
    /// format can never disagree.
    ///
    /// ⚠ `.opus` reads `(48_000, 16)` here because that is what an Opus session runs at — **not**
    /// because that pair is a way to ask for it. Core's hi-res entry point treats an explicit
    /// 48 000/16 as a real request for the lossless plane's cheapest rung (the unspecified pair is
    /// `0`/`0`, which is what the legacy entry point sends). `PunktfunkConnection.init` is where
    /// that distinction is enforced — it compares against this pair and dials the legacy entry
    /// point instead. Read that comment before changing either side.
    public var wire: (rateHz: UInt32, bits: UInt8) {
        switch self {
        case .opus: return (48_000, 16)
        case .lossless441: return (44_100, 24)
        case .lossless48: return (48_000, 24)
        case .lossless882: return (88_200, 24)
        case .lossless96: return (96_000, 24)
        case .lossless1764: return (176_400, 24)
        }
    }

    /// True for the lossless plane — the gate for anything that spends the extra bandwidth.
    public var isLossless: Bool { self != .opus }
}

public extension EffectiveSettings {
    /// This session's requested format, resolved from the stored string.
    var audioFormatChoice: AudioFormatChoice { AudioFormatChoice(setting: audioFormat) }
}

/// What a single connect was told to use, before any store is consulted.
///
/// The third case is why this is an enum rather than an `Optional<StreamPreset>`: "Connect with ▸
/// Default settings" on a BOUND host has to force the globals, and "no pick at all" has to fall
/// through to the binding. Collapsing the two would make the menu item that says "Default
/// settings" silently connect with the host's preset. It is the same distinction the session
/// binary's `--preset ""` reserves on the desktop clients.
public enum PresetSelection: Hashable, Sendable {
    /// No pick — the host's default binding applies (a plain click/tap).
    case inherit
    /// Force the global defaults for this one connect, whatever the host is bound to.
    case defaults
    /// This preset, for this one connect. NEVER rebinds the host (§5.2).
    case preset(String)

    public init(presetID: String?) {
        self = presetID.map(PresetSelection.preset) ?? .inherit
    }
}

extension EffectiveSettings {
    /// The mode a session asks the host for: the configured size at the render scale, capped at
    /// the codec's per-axis limit, at the configured refresh. A zero width, height or refresh is
    /// the console's Native and takes `native`'s; a native refresh counts as at least 30.
    public func streamMode(
        native: (width: Int, height: Int, hz: Int)
    ) -> (width: UInt32, height: UInt32, hz: UInt32) {
        let mode = RenderScale.apply(
            baseWidth: width == 0 ? native.width : width,
            baseHeight: height == 0 ? native.height : height, scale: renderScale,
            maxDimension: RenderScale.maxDimension(codec: codec))
        let hz = refreshHz == 0 ? max(native.hz, 30) : refreshHz
        return (mode.width, mode.height, UInt32(clamping: hz))
    }

    /// `pyrowaveBpp` in the hundredths the dial carries, held inside 0.5…2.
    public var pyrowaveBppX100: UInt16 {
        UInt16((min(max(pyrowaveBpp, 0.5), 2.0) * 100).rounded())
    }
}
