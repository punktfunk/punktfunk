// The in-stream quick-action ring's configuration — the `overlay_actions` setting, one JSON blob
// (schema v2, design/touch-client-overlay.md §3.2). The Swift twin of pf-client-core's
// `overlay_actions.rs`; `OverlayActionsTests` replays the shared contract,
// `clients/shared/overlay-actions-vectors.json`.
//
// Parsing never fails: fewer than six slots pad with empty, more are truncated, an unknown id or
// a dangling `shortcut:` reference is an empty slot, an absent field takes its default, and an
// unparseable blob is the platform default — presets sync between client versions.

import Foundation

/// What a ring slot does. `host` carries a host-advertised action id; `shortcut` refers into
/// `OverlayConfig.shortcuts` by id.
public enum SlotId: Equatable, Sendable {
    case endStream, disconnectLinger, touchMode, keyboard, stats, mic, pad, sendText
    /// End the game this device launched, then the stream.
    case endGame
    /// The host's guide button (Xbox / PS / Steam) and its quick-access `…`, as synthetic pad taps.
    case guide, qam
    /// Controller mouse: the pad drives the host pointer instead of its virtual pad.
    case padMouse
    /// Step the controller type the host emulates, live, for this stream.
    case padType
    /// Another client's picks this one does not offer: kept so a rewrite keeps them, drawn disabled.
    case streamMute, swapScreens
    case host(String)
    case shortcut(String)

    /// The wire id, the inverse of `parse`.
    public var id: String {
        switch self {
        case .endStream: return "end_stream"
        case .endGame: return "end_game"
        case .disconnectLinger: return "disconnect_linger"
        case .touchMode: return "touch_mode"
        case .keyboard: return "keyboard"
        case .stats: return "stats"
        case .mic: return "mic"
        case .pad: return "pad"
        case .sendText: return "send_text"
        case .guide: return "guide"
        case .qam: return "qam"
        case .padMouse: return "pad_mouse"
        case .padType: return "pad_type"
        case .streamMute: return "stream_mute"
        case .swapScreens: return "swap_screens"
        case .host(let id): return "host:\(id)"
        case .shortcut(let id): return "shortcut:\(id)"
        }
    }

    /// An id from the blob; `nil` for one this build does not know (an empty slot).
    public static func parse(_ s: String) -> SlotId? {
        switch s {
        case "end_stream": return .endStream
        case "end_game": return .endGame
        case "disconnect_linger": return .disconnectLinger
        case "touch_mode": return .touchMode
        case "keyboard": return .keyboard
        case "stats": return .stats
        case "mic": return .mic
        case "pad": return .pad
        case "send_text": return .sendText
        case "guide": return .guide
        case "qam": return .qam
        case "pad_mouse": return .padMouse
        case "pad_type": return .padType
        case "stream_mute": return .streamMute
        case "swap_screens": return .swapScreens
        default:
            if s.hasPrefix("host:"), s.count > 5 { return .host(String(s.dropFirst(5))) }
            if s.hasPrefix("shortcut:"), s.count > 9 { return .shortcut(String(s.dropFirst(9))) }
            return nil
        }
    }
}

/// A custom key chord. `keys` are names from the shared keymap tables, never raw key codes.
public struct OverlayShortcut: Equatable, Sendable {
    public var id: String
    public var label: String
    public var keys: [String]

    public init(id: String, label: String = "", keys: [String] = []) {
        self.id = id
        self.label = label
        self.keys = keys
    }
}

/// The Windows virtual-key code a shortcut key name stands for (the wire speaks VKs); `nil` for
/// a name this build does not know. Twin of the Rust `key_vk`: `OverlayActionsTests` replays
/// `crates/punktfunk-core/testdata/key-vk-vectors.json`, so a new name lands there first.
public func keyVk(_ name: String) -> UInt32? {
    let n = name.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    switch n {
    case "ctrl", "control": return 0x11
    case "shift": return 0x10
    case "alt", "option": return 0x12
    case "win", "cmd", "super", "meta": return 0x5B
    case "escape", "esc": return 0x1B
    case "tab": return 0x09
    case "enter", "return": return 0x0D
    case "space": return 0x20
    case "backspace": return 0x08
    case "delete", "del": return 0x2E
    case "insert": return 0x2D
    case "home": return 0x24
    case "end": return 0x23
    case "pageup": return 0x21
    case "pagedown": return 0x22
    case "up": return 0x26
    case "down": return 0x28
    case "left": return 0x25
    case "right": return 0x27
    case "printscreen": return 0x2C
    case "pause": return 0x13
    case "capslock": return 0x14
    default:
        let u = Array(n.unicodeScalars)
        if u.count == 1, ("a"..."z").contains(n) { return 0x41 + (u[0].value - 97) }
        if u.count == 1, ("0"..."9").contains(n) { return 0x30 + (u[0].value - 48) }
        if n.hasPrefix("f"), let f = Int(n.dropFirst()), (1...24).contains(f) { return UInt32(0x70 + f - 1) }
        return nil
    }
}

/// A chord as a legend reads it: `Ctrl+Shift+Esc`.
public func chordChip(_ keys: [String]) -> String {
    keys.map(keyLegend).joined(separator: "+")
}

/// One key's legend: the word a keyboard prints on it (`Ctrl`, `Esc`, `PgUp`), arrows as
/// arrows. Symbols like ❖ or ⇧ read as nothing to most people, so none are used here.
public func keyLegend(_ k: String) -> String {
    let n = k.trimmingCharacters(in: .whitespaces).lowercased()
    switch n {
    case "ctrl", "control": return "Ctrl"
    case "shift": return "Shift"
    case "alt", "option": return "Alt"
    case "win", "cmd", "super", "meta": return "Win"
    case "escape", "esc": return "Esc"
    case "enter", "return": return "Enter"
    case "backspace": return "Backspace"
    case "delete", "del": return "Del"
    case "insert": return "Ins"
    case "pageup": return "PgUp"
    case "pagedown": return "PgDn"
    case "printscreen": return "PrtSc"
    case "capslock": return "Caps"
    case "up": return "↑"
    case "down": return "↓"
    case "left": return "←"
    case "right": return "→"
    default: return n.prefix(1).uppercased() + n.dropFirst()
    }
}

/// One control's layout override: `x` and `y` place its centre as fractions of the layer's
/// width and height, `scale` sizes it about that centre, `hidden` takes it out of the stream
/// (the editor still shows it, ghosted). An absent field keeps the preset's value.
public struct PadTweak: Equatable, Sendable {
    public var x: Float?
    public var y: Float?
    public var scale: Float?
    public var hidden: Bool

    public init(x: Float? = nil, y: Float? = nil, scale: Float? = nil, hidden: Bool = false) {
        self.x = x
        self.y = y
        self.scale = scale
        self.hidden = hidden
    }
}

/// The virtual controller: `layout` is `full`, `sticks` or `dpad`; `opacity` and `scale` are
/// the two sliders. `controls` carries the per-control overrides keyed by control id (`ls`,
/// `rs`, `dpad`, `face`, `lb`, `rb`, `lt`, `rt`, `l3`, `r3`, `select`, `guide`, `start`,
/// `ring`), `controlsNarrow`
/// the same for a narrow (upright) layer — the two classes the preset already lays out
/// differently.
public struct PadConfig: Equatable, Sendable {
    public var layout = "full"
    public var opacity: Float = 0.45
    public var scale: Float = 1
    public var controls: [String: PadTweak] = [:]
    public var controlsNarrow: [String: PadTweak] = [:]

    public init(layout: String = "full", opacity: Float = 0.45, scale: Float = 1,
                controls: [String: PadTweak] = [:], controlsNarrow: [String: PadTweak] = [:]) {
        self.layout = layout
        self.opacity = opacity
        self.scale = scale
        self.controls = controls
        self.controlsNarrow = controlsNarrow
    }
}

/// Which platform default ring applies: iOS and iPadOS are `touch`, macOS is `desktop`, and
/// tvOS is `tv`, which leaves out the touch, keyboard and microphone slots a TV cannot run.
public enum RingPlatform: Sendable {
    case touch, desktop, tv
}

public struct OverlayConfig: Equatable, Sendable {
    public static let ringSlots = 6
    public static let schemaVersion = 2

    /// Exactly `ringSlots` entries, clockwise from 12 o'clock; `nil` is an empty slot.
    public var ring: [SlotId?]
    public var shortcuts: [OverlayShortcut]
    public var pad: PadConfig

    public init(ring: [SlotId?], shortcuts: [OverlayShortcut] = [], pad: PadConfig = PadConfig()) {
        self.ring = ring
        self.shortcuts = shortcuts
        self.pad = pad
    }

    public func shortcut(_ id: String) -> OverlayShortcut? {
        shortcuts.first { $0.id == id }
    }

    /// Add or replace a shortcut. A new one takes the first empty slot, as the editors promise.
    public mutating func saveShortcut(_ sc: OverlayShortcut) {
        if let i = shortcuts.firstIndex(where: { $0.id == sc.id }) {
            shortcuts[i] = sc
        } else {
            shortcuts.append(sc)
            if let k = ring.firstIndex(where: { $0 == nil }) { ring[k] = .shortcut(sc.id) }
        }
    }

    /// Remove a shortcut, emptying every slot that sent it: `parse` would, on the next read.
    public mutating func removeShortcut(_ id: String) {
        shortcuts.removeAll { $0.id == id }
        ring = ring.map { slot in
            if case .shortcut(let sid) = slot, sid == id { return nil }
            return slot
        }
    }

    /// The id a new shortcut takes: one past the highest `s<n>` in use.
    public var nextShortcutID: String {
        "s\((shortcuts.compactMap { Int($0.id.dropFirst()) }.max() ?? 0) + 1)"
    }

    public static func platformDefault(_ platform: RingPlatform = .touch) -> OverlayConfig {
        switch platform {
        case .touch:
            return OverlayConfig(ring: [.endStream, .keyboard, .touchMode, .stats, .mic, .pad])
        case .desktop:
            return OverlayConfig(ring: [.endStream, .disconnectLinger, .touchMode, .stats, .mic, .sendText])
        case .tv:
            return OverlayConfig(ring: [.endStream, .disconnectLinger, .stats, .guide, .qam, nil])
        }
    }

    /// Parse the setting; an empty or unparseable blob is the platform default.
    public static func parse(_ json: String?, platform: RingPlatform = .touch) -> OverlayConfig {
        guard let json, !json.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty,
              let data = json.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
        else { return platformDefault(platform) }
        let shortcuts: [OverlayShortcut] = ((obj["shortcuts"] as? [[String: Any]]) ?? []).compactMap { s in
            guard let id = s["id"] as? String, !id.isEmpty else { return nil }
            return OverlayShortcut(
                id: id, label: s["label"] as? String ?? "", keys: s["keys"] as? [String] ?? [])
        }
        let ringIn = obj["ring"] as? [Any] ?? []
        let ring: [SlotId?] = (0..<ringSlots).map { i in
            guard i < ringIn.count, let s = ringIn[i] as? String, let slot = SlotId.parse(s) else {
                return nil
            }
            if case .shortcut(let id) = slot, !shortcuts.contains(where: { $0.id == id }) { return nil }
            return slot
        }
        let padIn = obj["pad"] as? [String: Any] ?? [:]
        var pad = PadConfig()
        if let l = padIn["layout"] as? String, !l.isEmpty { pad.layout = l }
        if let o = padIn["opacity"] as? Double { pad.opacity = Float(o) }
        if let s = padIn["scale"] as? Double { pad.scale = Float(s) }
        func tweaks(_ key: String) -> [String: PadTweak] {
            guard let m = padIn[key] as? [String: Any] else { return [:] }
            var out: [String: PadTweak] = [:]
            for (id, v) in m {
                guard let t = v as? [String: Any] else { continue }
                out[id] = PadTweak(x: (t["x"] as? Double).map(Float.init),
                                   y: (t["y"] as? Double).map(Float.init),
                                   scale: (t["scale"] as? Double).map(Float.init),
                                   hidden: t["hidden"] as? Bool ?? false)
            }
            return out
        }
        pad.controls = tweaks("controls")
        pad.controlsNarrow = tweaks("controls_narrow")
        return OverlayConfig(ring: ring, shortcuts: shortcuts, pad: pad)
    }

    /// The blob to store — always the current schema version.
    public func toJSON() -> String {
        // An untouched pad keeps its blob clean: no empty maps, no absent fields as nulls.
        var padOut: [String: Any] = ["layout": pad.layout, "opacity": Double(pad.opacity), "scale": Double(pad.scale)]
        func tweaks(_ m: [String: PadTweak]) -> [String: Any] {
            m.mapValues { t in
                var o: [String: Any] = [:]
                if let x = t.x { o["x"] = Double(x) }
                if let y = t.y { o["y"] = Double(y) }
                if let s = t.scale { o["scale"] = Double(s) }
                if t.hidden { o["hidden"] = true }
                return o
            }
        }
        if !pad.controls.isEmpty { padOut["controls"] = tweaks(pad.controls) }
        if !pad.controlsNarrow.isEmpty { padOut["controls_narrow"] = tweaks(pad.controlsNarrow) }
        let obj: [String: Any] = [
            "v": Self.schemaVersion,
            "ring": ring.map { $0.map { $0.id as Any } ?? NSNull() },
            "shortcuts": shortcuts.map { ["id": $0.id, "label": $0.label, "keys": $0.keys] },
            "pad": padOut,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys]),
              let text = String(data: data, encoding: .utf8)
        else { return "" } // plain data always serialises
        return text
    }
}
