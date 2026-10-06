// Client settings presets — named bundles of setting overrides applied on top of the global
// defaults (design/client-settings-profiles.md §4). The Swift half of the model whose Rust
// original is `crates/pf-client-core/src/presets.rs`; the two are mirrored field for field so a
// future export/import has one shape to speak.
//
// A preset overrides only the fields the user touched; everything else keeps following the
// global defaults *live*, so fixing a global once fixes it everywhere. That is why an overlay is
// sparse `Optional`s rather than a snapshot copy, and why `.some(x)` is written on touch and nil
// on an explicit "reset to default" — never by diffing against the current global (a `.some`
// equal to today's global is a legitimate *pin*: the preset keeps `x` when the global later
// moves).
//
// The catalog lives in the APP GROUP suite, with the saved hosts rather than with the settings:
// per-host data is already shared there so the widget can read it, and bindings/pins live on the
// host record. It is a dependency-free part of PunktfunkShared for the same reason `StoredHost`
// is — an extension process can read it without linking PunktfunkKit.

import Foundation

/// The catalog file's schema version. Bumped only for a breaking shape change — additive fields
/// ride the unknown-key preservation below.
public let punktfunkPresetsVersion = 1

// MARK: - Unknown-key carry-through

/// A decoded-but-unmodelled JSON value. Exists only so an overlay (or a preset) written by a
/// NEWER build survives an open→save round-trip on an older one: the don't-clobber rule from the
/// design's §4.1, and the reason the Rust overlay carries a `#[serde(flatten)] extra` map.
public enum JSONValue: Codable, Equatable, Sendable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([JSONValue])
    case object([String: JSONValue])

    public init(from decoder: Decoder) throws {
        let c = try decoder.singleValueContainer()
        if c.decodeNil() {
            self = .null
        } else if let v = try? c.decode(Bool.self) {
            self = .bool(v)
        } else if let v = try? c.decode(Double.self) {
            self = .number(v)
        } else if let v = try? c.decode(String.self) {
            self = .string(v)
        } else if let v = try? c.decode([JSONValue].self) {
            self = .array(v)
        } else {
            self = .object(try c.decode([String: JSONValue].self))
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.singleValueContainer()
        switch self {
        case .null: try c.encodeNil()
        case .bool(let v): try c.encode(v)
        case .number(let v):
            // Whole numbers encode as integers so a carried-through `"width": 3840` doesn't come
            // back as `3840.0` and read as a different value to the next parser.
            if v == v.rounded(), abs(v) < 9_007_199_254_740_992 {
                try c.encode(Int(v))
            } else {
                try c.encode(v)
            }
        case .string(let v): try c.encode(v)
        case .array(let v): try c.encode(v)
        case .object(let v): try c.encode(v)
        }
    }
}

/// A `CodingKey` for keys only known at runtime — what the unknown-key sweep needs.
private struct AnyKey: CodingKey {
    let stringValue: String
    var intValue: Int? { nil }
    init(_ s: String) { stringValue = s }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { nil }
}

// MARK: - The overlay

/// Every presetable ("tier P") setting, as an `Optional`: nil = inherit the global value, live.
///
/// Tier-H fields (host properties like the per-host clipboard toggle) and tier-G ones (this
/// device's hardware and audio endpoints) are deliberately absent — see the design's §3 curation.
/// The last seven are the Apple-only additions §3 lists: presentation and modifier choices that
/// are host-OS- and display-specific by nature.
///
/// The coding keys are the Rust overlay's serialized names. Two of them carry a different
/// REPRESENTATION here — `compositor` and `gamepad` are the wire enum's ordinals on Apple and
/// strings in the Rust core — because those are the shapes the two clients already persist; a
/// cross-platform importer has to map them either way.
public struct SettingsOverlay: Codable, Equatable, Sendable {
    public var width: Int?
    public var height: Int?
    public var refreshHz: Int?
    public var matchWindow: Bool?
    public var bitrateKbps: Int?
    public var renderScale: Double?
    /// A `VideoFit` raw value.
    public var videoFit: String?
    public var codec: String?
    public var hdrEnabled: Bool?
    public var compositor: Int?
    public var audioChannels: Int?
    /// An `AudioFormatChoice` raw value — the audio format this preset asks the host for.
    /// Presetable because it is about how a HOST is streamed (a wired desktop can afford
    /// lossless; a phone on cellular cannot), not about this device's hardware.
    ///
    /// The Rust overlay (`pf-client-core::presets`) carries the same `audio_format` key with the
    /// same spellings, but its table stops at `opus`/`lossless48`/`lossless96`. A preset written
    /// here as `lossless441` or `lossless882` round-trips there intact and plays as Opus, since an
    /// unrecognized value falls back to the default rather than failing the connect.
    public var audioFormat: String?
    public var micEnabled: Bool?
    public var echoCancel: Bool?
    public var keepHostAudio: Bool?
    public var touchMode: String?
    public var mouseMode: String?
    public var invertScroll: Bool?
    /// The whole ring blob (design/touch-client-overlay.md D10): a preset inherits the default
    /// ring entirely or owns its own ring and shortcuts.
    public var overlayActions: String?
    public var inhibitShortcuts: Bool?
    public var gamepadType: Int?
    public var gamepadForwarding: Bool?
    public var systemButtons: String?
    public var guideGesture: String?
    /// A `StatsVerbosity` raw value ("off"/"compact"/"normal"/"detailed") — the enum lives in
    /// PunktfunkKit, which this module must not depend on.
    public var statsVerbosity: String?
    public var fullscreenWhileStreaming: Bool?
    // Apple-only additions (design §3).
    public var enable444: Bool?
    public var tenBitSdr: Bool?
    public var presentPriority: String?
    public var smoothBuffer: Int?
    public var vsync: Bool?
    public var allowVRR: Bool?
    public var modifierLayout: String?
    /// Overlay keys a newer build wrote and this one doesn't model — carried through a
    /// load→save round-trip untouched.
    public var extra: [String: JSONValue] = [:]

    public init() {}

    /// True when the preset overrides nothing — "inherits everything", the state a freshly
    /// created preset starts in. Unknown-key carry-through counts: a preset that only holds a
    /// newer build's field is not empty.
    public var isEmpty: Bool { self == SettingsOverlay() }

    // MARK: Codable

    private enum Key: String, CaseIterable {
        case width, height
        case refreshHz = "refresh_hz"
        case matchWindow = "match_window"
        case bitrateKbps = "bitrate_kbps"
        case renderScale = "render_scale"
        case videoFit = "video_fit"
        case codec
        case hdrEnabled = "hdr_enabled"
        case compositor
        case audioChannels = "audio_channels"
        case audioFormat = "audio_format"
        case micEnabled = "mic_enabled"
        case echoCancel = "echo_cancel"
        case keepHostAudio = "keep_host_audio"
        case touchMode = "touch_mode"
        case mouseMode = "mouse_mode"
        case invertScroll = "invert_scroll"
        case overlayActions = "overlay_actions"
        case inhibitShortcuts = "inhibit_shortcuts"
        case gamepadType = "gamepad"
        case gamepadForwarding = "gamepad_forwarding"
        case systemButtons = "system_buttons"
        case guideGesture = "guide_gesture"
        case statsVerbosity = "stats_verbosity"
        case fullscreenWhileStreaming = "fullscreen_on_stream"
        case enable444 = "enable_444"
        case tenBitSdr = "ten_bit_sdr"
        case presentPriority = "present_priority"
        case smoothBuffer = "smooth_buffer"
        case vsync
        case allowVRR = "allow_vrr"
        case modifierLayout = "modifier_layout"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: AnyKey.self)
        func int(_ k: Key) -> Int? { try? c.decodeIfPresent(Int.self, forKey: AnyKey(k.rawValue)) }
        func dbl(_ k: Key) -> Double? { try? c.decodeIfPresent(Double.self, forKey: AnyKey(k.rawValue)) }
        func bool(_ k: Key) -> Bool? { try? c.decodeIfPresent(Bool.self, forKey: AnyKey(k.rawValue)) }
        func str(_ k: Key) -> String? { try? c.decodeIfPresent(String.self, forKey: AnyKey(k.rawValue)) }
        width = int(.width)
        height = int(.height)
        refreshHz = int(.refreshHz)
        matchWindow = bool(.matchWindow)
        bitrateKbps = int(.bitrateKbps)
        renderScale = dbl(.renderScale)
        videoFit = str(.videoFit)
        codec = str(.codec)
        hdrEnabled = bool(.hdrEnabled)
        compositor = int(.compositor)
        audioChannels = int(.audioChannels)
        audioFormat = str(.audioFormat)
        micEnabled = bool(.micEnabled)
        echoCancel = bool(.echoCancel)
        keepHostAudio = bool(.keepHostAudio)
        touchMode = str(.touchMode)
        mouseMode = str(.mouseMode)
        invertScroll = bool(.invertScroll)
        overlayActions = str(.overlayActions)
        inhibitShortcuts = bool(.inhibitShortcuts)
        gamepadType = int(.gamepadType)
        gamepadForwarding = bool(.gamepadForwarding)
        systemButtons = str(.systemButtons)
        guideGesture = str(.guideGesture)
        statsVerbosity = str(.statsVerbosity)
        fullscreenWhileStreaming = bool(.fullscreenWhileStreaming)
        enable444 = bool(.enable444)
        tenBitSdr = bool(.tenBitSdr)
        presentPriority = str(.presentPriority)
        smoothBuffer = int(.smoothBuffer)
        vsync = bool(.vsync)
        allowVRR = bool(.allowVRR)
        modifierLayout = str(.modifierLayout)
        let known = Set(Key.allCases.map(\.rawValue))
        for key in c.allKeys where !known.contains(key.stringValue) {
            extra[key.stringValue] = try c.decode(JSONValue.self, forKey: key)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyKey.self)
        // Absent overrides serialize away entirely (no `null`s) — the file stays readable, and
        // "not overridden" has exactly one representation on disk.
        try c.encodeIfPresent(width, forKey: AnyKey(Key.width.rawValue))
        try c.encodeIfPresent(height, forKey: AnyKey(Key.height.rawValue))
        try c.encodeIfPresent(refreshHz, forKey: AnyKey(Key.refreshHz.rawValue))
        try c.encodeIfPresent(matchWindow, forKey: AnyKey(Key.matchWindow.rawValue))
        try c.encodeIfPresent(bitrateKbps, forKey: AnyKey(Key.bitrateKbps.rawValue))
        try c.encodeIfPresent(renderScale, forKey: AnyKey(Key.renderScale.rawValue))
        try c.encodeIfPresent(videoFit, forKey: AnyKey(Key.videoFit.rawValue))
        try c.encodeIfPresent(codec, forKey: AnyKey(Key.codec.rawValue))
        try c.encodeIfPresent(hdrEnabled, forKey: AnyKey(Key.hdrEnabled.rawValue))
        try c.encodeIfPresent(compositor, forKey: AnyKey(Key.compositor.rawValue))
        try c.encodeIfPresent(audioChannels, forKey: AnyKey(Key.audioChannels.rawValue))
        try c.encodeIfPresent(audioFormat, forKey: AnyKey(Key.audioFormat.rawValue))
        try c.encodeIfPresent(micEnabled, forKey: AnyKey(Key.micEnabled.rawValue))
        try c.encodeIfPresent(echoCancel, forKey: AnyKey(Key.echoCancel.rawValue))
        try c.encodeIfPresent(keepHostAudio, forKey: AnyKey(Key.keepHostAudio.rawValue))
        try c.encodeIfPresent(touchMode, forKey: AnyKey(Key.touchMode.rawValue))
        try c.encodeIfPresent(mouseMode, forKey: AnyKey(Key.mouseMode.rawValue))
        try c.encodeIfPresent(invertScroll, forKey: AnyKey(Key.invertScroll.rawValue))
        try c.encodeIfPresent(overlayActions, forKey: AnyKey(Key.overlayActions.rawValue))
        try c.encodeIfPresent(inhibitShortcuts, forKey: AnyKey(Key.inhibitShortcuts.rawValue))
        try c.encodeIfPresent(gamepadType, forKey: AnyKey(Key.gamepadType.rawValue))
        try c.encodeIfPresent(
            gamepadForwarding, forKey: AnyKey(Key.gamepadForwarding.rawValue))
        try c.encodeIfPresent(systemButtons, forKey: AnyKey(Key.systemButtons.rawValue))
        try c.encodeIfPresent(guideGesture, forKey: AnyKey(Key.guideGesture.rawValue))
        try c.encodeIfPresent(statsVerbosity, forKey: AnyKey(Key.statsVerbosity.rawValue))
        try c.encodeIfPresent(
            fullscreenWhileStreaming, forKey: AnyKey(Key.fullscreenWhileStreaming.rawValue))
        try c.encodeIfPresent(enable444, forKey: AnyKey(Key.enable444.rawValue))
        try c.encodeIfPresent(tenBitSdr, forKey: AnyKey(Key.tenBitSdr.rawValue))
        try c.encodeIfPresent(presentPriority, forKey: AnyKey(Key.presentPriority.rawValue))
        try c.encodeIfPresent(smoothBuffer, forKey: AnyKey(Key.smoothBuffer.rawValue))
        try c.encodeIfPresent(vsync, forKey: AnyKey(Key.vsync.rawValue))
        try c.encodeIfPresent(allowVRR, forKey: AnyKey(Key.allowVRR.rawValue))
        try c.encodeIfPresent(modifierLayout, forKey: AnyKey(Key.modifierLayout.rawValue))
        let known = Set(Key.allCases.map(\.rawValue))
        for (key, value) in extra where !known.contains(key) {
            try c.encode(value, forKey: AnyKey(key))
        }
    }
}

// MARK: - Field descriptors

/// The overlay's fields, named the way a UI can carry them as plain strings: the serialized key,
/// with `resolution` as the one alias covering the width/height/match-window tri-state a single
/// control drives on every client. Kept as one list so "what can be reset" lives in one place —
/// two copies of it is exactly how a reset button and a commit path drift.
public enum OverlayField {
    public static let resolution = "resolution"

    /// Drop one override by name, putting the row back to inheriting. Returns false for a name
    /// this build doesn't know.
    @discardableResult
    public static func clear(_ field: String, in overlay: inout SettingsOverlay) -> Bool {
        switch field {
        case resolution:
            overlay.width = nil
            overlay.height = nil
            overlay.matchWindow = nil
        case "width": overlay.width = nil
        case "height": overlay.height = nil
        case "refresh_hz": overlay.refreshHz = nil
        case "match_window": overlay.matchWindow = nil
        case "bitrate_kbps": overlay.bitrateKbps = nil
        case "render_scale": overlay.renderScale = nil
        case "video_fit": overlay.videoFit = nil
        case "codec": overlay.codec = nil
        case "hdr_enabled": overlay.hdrEnabled = nil
        case "compositor": overlay.compositor = nil
        case "audio_channels": overlay.audioChannels = nil
        case "audio_format": overlay.audioFormat = nil
        case "mic_enabled": overlay.micEnabled = nil
        case "echo_cancel": overlay.echoCancel = nil
        case "keep_host_audio": overlay.keepHostAudio = nil
        case "touch_mode": overlay.touchMode = nil
        case "mouse_mode": overlay.mouseMode = nil
        case "invert_scroll": overlay.invertScroll = nil
        case "overlay_actions": overlay.overlayActions = nil
        case "inhibit_shortcuts": overlay.inhibitShortcuts = nil
        case "gamepad": overlay.gamepadType = nil
        case "gamepad_forwarding": overlay.gamepadForwarding = nil
        case "system_buttons": overlay.systemButtons = nil
        case "guide_gesture": overlay.guideGesture = nil
        case "stats_verbosity": overlay.statsVerbosity = nil
        case "fullscreen_on_stream": overlay.fullscreenWhileStreaming = nil
        case "enable_444": overlay.enable444 = nil
        case "ten_bit_sdr": overlay.tenBitSdr = nil
        case "present_priority": overlay.presentPriority = nil
        case "smooth_buffer": overlay.smoothBuffer = nil
        case "vsync": overlay.vsync = nil
        case "allow_vrr": overlay.allowVRR = nil
        case "modifier_layout": overlay.modifierLayout = nil
        default: return false
        }
        return true
    }

    /// Does this overlay override `field`? Drives the per-row override marker; `resolution` is
    /// overridden when any leg of its tri-state is.
    public static func isOverridden(_ field: String, in o: SettingsOverlay) -> Bool {
        switch field {
        case resolution: return o.width != nil || o.height != nil || o.matchWindow != nil
        case "width": return o.width != nil
        case "height": return o.height != nil
        case "refresh_hz": return o.refreshHz != nil
        case "match_window": return o.matchWindow != nil
        case "bitrate_kbps": return o.bitrateKbps != nil
        case "render_scale": return o.renderScale != nil
        case "video_fit": return o.videoFit != nil
        case "codec": return o.codec != nil
        case "hdr_enabled": return o.hdrEnabled != nil
        case "compositor": return o.compositor != nil
        case "audio_channels": return o.audioChannels != nil
        case "audio_format": return o.audioFormat != nil
        case "mic_enabled": return o.micEnabled != nil
        case "echo_cancel": return o.echoCancel != nil
        case "keep_host_audio": return o.keepHostAudio != nil
        case "touch_mode": return o.touchMode != nil
        case "mouse_mode": return o.mouseMode != nil
        case "invert_scroll": return o.invertScroll != nil
        case "overlay_actions": return o.overlayActions != nil
        case "inhibit_shortcuts": return o.inhibitShortcuts != nil
        case "gamepad": return o.gamepadType != nil
        case "gamepad_forwarding": return o.gamepadForwarding != nil
        case "system_buttons": return o.systemButtons != nil
        case "guide_gesture": return o.guideGesture != nil
        case "stats_verbosity": return o.statsVerbosity != nil
        case "fullscreen_on_stream": return o.fullscreenWhileStreaming != nil
        case "enable_444": return o.enable444 != nil
        case "ten_bit_sdr": return o.tenBitSdr != nil
        case "present_priority": return o.presentPriority != nil
        case "smooth_buffer": return o.smoothBuffer != nil
        case "vsync": return o.vsync != nil
        case "allow_vrr": return o.allowVRR != nil
        case "modifier_layout": return o.modifierLayout != nil
        default: return false
        }
    }

    /// Does this build model `field` at all? `clear` and `isOverridden` both answer "no override"
    /// for a name they do not know, which is indistinguishable from "not overridden" — so a typo
    /// in a settings row loses its marker and its Reset with nothing to report it. Callers that
    /// take a field name from source rather than from data assert on this.
    public static func isModelled(_ field: String) -> Bool {
        var probe = SettingsOverlay()
        return clear(field, in: &probe)
    }
}

// MARK: - The preset

/// One named bundle of overrides. `id` is stable across renames — bindings, pins and deep links
/// all point at it, never at the name.
public struct StreamPreset: Codable, Equatable, Identifiable, Sendable {
    public var id: String
    /// User-facing and editable; unique case-insensitively (menus are ambiguous otherwise —
    /// enforced by the editing UIs via `PresetCatalog.nameTaken`).
    public var name: String
    /// `#RRGGBB` chip color. The UI may ignore it; the schema reserves it (pinned cards use it
    /// to tint their subtitle).
    public var accent: String?
    public var overrides: SettingsOverlay
    /// Preset keys a newer build wrote — preserved across a load→save round-trip.
    public var extra: [String: JSONValue] = [:]

    /// A new, empty preset: inherits everything (the right creation default under
    /// inherit-by-exception — "Duplicate" covers starting from another preset).
    public init(name: String, id: String = newPresetID(), accent: String? = nil,
                overrides: SettingsOverlay = SettingsOverlay()) {
        self.id = id
        self.name = name
        self.accent = accent
        self.overrides = overrides
    }

    private enum Key: String, CaseIterable {
        case id, name, accent, overrides
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: AnyKey.self)
        id = (try? c.decode(String.self, forKey: AnyKey(Key.id.rawValue))) ?? newPresetID()
        name = (try? c.decode(String.self, forKey: AnyKey(Key.name.rawValue))) ?? ""
        accent = try? c.decodeIfPresent(String.self, forKey: AnyKey(Key.accent.rawValue))
        overrides = (try? c.decodeIfPresent(
            SettingsOverlay.self, forKey: AnyKey(Key.overrides.rawValue))) ?? SettingsOverlay()
        let known = Set(Key.allCases.map(\.rawValue))
        for key in c.allKeys where !known.contains(key.stringValue) {
            extra[key.stringValue] = try c.decode(JSONValue.self, forKey: key)
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: AnyKey.self)
        try c.encode(id, forKey: AnyKey(Key.id.rawValue))
        try c.encode(name, forKey: AnyKey(Key.name.rawValue))
        try c.encodeIfPresent(accent, forKey: AnyKey(Key.accent.rawValue))
        try c.encode(overrides, forKey: AnyKey(Key.overrides.rawValue))
        let known = Set(Key.allCases.map(\.rawValue))
        for (key, value) in extra where !known.contains(key) {
            try c.encode(value, forKey: AnyKey(key))
        }
    }
}

/// 12 lowercase hex characters — the shape the Rust `new_preset_id` mints, so the two catalogs
/// speak one id format.
public func newPresetID() -> String {
    (0..<6).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
}

// MARK: - The catalog

/// What a `preset=` / `Connect with ▸` reference resolved to. Ambiguity is reported rather than
/// guessed: a link naming two presets must refuse, not pick one.
public enum PresetResolution: Equatable, Sendable {
    case found
    case notFound
    /// More than one preset carries this name (case-insensitively).
    case ambiguous
}

/// The preset catalog — client-wide, not per host: "Work" applied to three hosts is one
/// preset, and the per-host part is only the binding on the host record.
public struct PresetCatalog: Codable, Equatable, Sendable {
    public var version: Int
    public var presets: [StreamPreset]

    public init(version: Int = punktfunkPresetsVersion, presets: [StreamPreset] = []) {
        self.version = version
        self.presets = presets
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = (try? c.decodeIfPresent(Int.self, forKey: .version)) ?? 0
        // `profiles`: the key a catalog saved before the rename carries.
        let old = try decoder.container(keyedBy: LegacyKeys.self)
        presets = (try? c.decodeIfPresent([StreamPreset].self, forKey: .presets))
            ?? (try? old.decodeIfPresent([StreamPreset].self, forKey: .profiles)) ?? []
    }

    private enum LegacyKeys: String, CodingKey { case profiles }

    /// The stored catalog, or an empty one — a missing or unreadable blob is "no presets", never
    /// an error: nothing about streaming may hinge on this key existing. A catalog saved before the
    /// rename is read from its old entry until the first save writes the new one.
    public static func load(from defaults: UserDefaults = AppGroup.defaults) -> PresetCatalog {
        guard let data = defaults.data(forKey: DefaultsKey.presets)
                ?? defaults.data(forKey: DefaultsKey.legacyPresets),
              let catalog = try? JSONDecoder().decode(PresetCatalog.self, from: data)
        else { return PresetCatalog(presets: []) }
        return catalog
    }

    public func save(to defaults: UserDefaults = AppGroup.defaults) {
        var out = self
        out.version = punktfunkPresetsVersion
        guard let data = try? JSONEncoder().encode(out) else { return }
        defaults.set(data, forKey: DefaultsKey.presets)
    }

    public func preset(id: String) -> StreamPreset? {
        presets.first { $0.id == id }
    }

    /// The binding a connect resolves to: the launched title's own, else the host's default.
    /// A dangling id is skipped, never an error: a deleted title binding falls through to the
    /// host's default, and a deleted default is "no preset".
    public func binding(for host: StoredHost, launch: String? = nil) -> StreamPreset? {
        launch.flatMap { host.gamePresets?[$0] }.flatMap { preset(id: $0) }
            ?? host.presetID.flatMap { preset(id: $0) }
    }

    /// This host's pinned presets, in card order, with duplicates and dangling ids dropped —
    /// a pin is presentation only, so a deleted preset just loses its card.
    public func pinned(for host: StoredHost) -> [StreamPreset] {
        var seen = Set<String>()
        return (host.pinnedPresetIDs ?? [])
            .filter { seen.insert($0).inserted }
            .compactMap { preset(id: $0) }
    }

    /// Resolve a reference the way every surface must: exact id first, then a unique
    /// case-insensitive name. Ambiguous names resolve to `.ambiguous`, never to the first match.
    public func resolve(_ reference: String) -> (StreamPreset?, PresetResolution) {
        if let p = preset(id: reference) { return (p, .found) }
        let hits = presets.filter { $0.name.lowercased() == reference.lowercased() }
        switch hits.count {
        case 1: return (hits[0], .found)
        case 0: return (nil, .notFound)
        default: return (nil, .ambiguous)
        }
    }

    /// Is this name already used (case-insensitively) by a *different* preset? The create/rename
    /// guard — `except` is the preset being renamed, so renaming "Work" to "work" is allowed.
    public func nameTaken(_ name: String, except: String? = nil) -> Bool {
        presets.contains {
            $0.name.lowercased() == name.lowercased() && $0.id != except
        }
    }
}
