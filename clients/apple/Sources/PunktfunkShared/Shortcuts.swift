// In-stream shortcuts: a press or hold of controller buttons or keys, bound to a quick action.
// The Swift twin of pf-client-core's `shortcuts` module (design/client-shortcuts.md):
// the model, the `shortcuts` blob, each client's defaults, the editor's validation and the rows
// the Shortcuts page draws. `ShortcutsEngine.swift` holds the engine. `ShortcutVectorsTests`
// replays `clients/shared/shortcut-vectors.json`, so a rule changes there first.

import Foundation

/// Every shortcut of one device and preset, in display order.
public struct Shortcuts: Equatable, Sendable {
    public var bindings: [Binding]

    public static let schemaVersion = 1

    public init(bindings: [Binding] = []) {
        self.bindings = bindings
    }

    /// A client family with its own bindings.
    public enum Client: Sendable {
        case desktop, android
        /// An iPhone, iPad, Vision Pro or Apple TV.
        case apple
        /// A Mac: the iPad's chords, plus the mouse mode.
        case mac
        case webOS, web, tizen, ps5
    }

    /// Where a trigger is pressed; one `Engine` runs per pad and one for the keyboard.
    public enum Device: Sendable {
        case pad, keyboard
    }

    /// What the player presses. Pad buttons are wire bits in stored order; keys are `keyVk`
    /// names, modifiers in `modifiers` order.
    public enum Trigger: Equatable, Sendable {
        case pad(buttons: [UInt32], hold: Bool)
        case key(modifiers: [String], key: String)

        public var device: Device {
            if case .pad = self { return .pad }
            return .keyboard
        }

        /// A keyboard trigger is always a press.
        public var hold: Bool {
            if case .pad(_, true) = self { return true }
            return false
        }

        /// What the engine matches, sorted: wire bits, or the VKs of the modifiers and the key.
        public var ids: [UInt32] {
            let raw: [UInt32]
            switch self {
            case .pad(let buttons, _): raw = buttons
            case .key(let mods, let key): raw = (mods + [key]).compactMap(keyVk)
            }
            return Array(Set(raw)).sorted()
        }
    }

    /// How an action meets a hold: once, or down and up on the host like a key.
    public enum ActionKind: Sendable {
        case once
        /// Guide, Quick access, a key combination: a hold keeps it down on the host.
        case keyLike
    }

    /// A quick action, or opening the dial itself. `unknown` is an id from a newer client: the
    /// row stays, dimmed, and the engine never sees it.
    public enum Action: Equatable, Sendable {
        case slot(SlotId)
        case ring
        case unknown(String)

        public init(id: String) {
            if id == "ring" {
                self = .ring
            } else if let slot = SlotId.parse(id) {
                self = .slot(slot)
            } else {
                self = .unknown(id)
            }
        }

        public var id: String {
            switch self {
            case .slot(let slot): return slot.id
            case .ring: return "ring"
            case .unknown(let id): return id
            }
        }

        public var kind: ActionKind {
            switch self {
            case .slot(.guide), .slot(.qam), .slot(.shortcut): return .keyLike
            default: return .once
            }
        }
    }

    public struct Binding: Equatable, Sendable {
        public var trigger: Trigger
        public var action: Action

        public init(trigger: Trigger, action: Action) {
            self.trigger = trigger
            self.action = action
        }
    }

    /// Why the editor refuses a new binding. Indices point into `bindings`.
    public enum Conflict: Equatable, Sendable {
        /// The same trigger already exists: offer to replace it.
        case identical(Int)
        /// One of the two is a press whose buttons the other's contain: it fires first.
        case pressSubset(Int)
        /// One button is a hold; a press needs two.
        case singlePress
        /// A key other than F1–F24 needs Ctrl, Alt or Shift.
        case noModifier
        case noKey
    }

    /// Pad buttons by their stored name: `select` for Back, the wire's own names for the rest.
    /// The bits are `punktfunk_core::input::gamepad`'s; `ShortcutVectorsTests` pins them to
    /// `GamepadWire`.
    public static let buttons: [(name: String, bit: UInt32)] = [
        ("a", 0x1000), ("b", 0x2000), ("x", 0x4000), ("y", 0x8000),
        ("lb", 0x0100), ("rb", 0x0200), ("ls", 0x0040), ("rs", 0x0080),
        ("start", 0x0010), ("select", 0x0020), ("guide", 0x0400),
        ("up", 0x0001), ("down", 0x0002), ("left", 0x0004), ("right", 0x0008),
        ("touchpad", 0x10_0000), ("misc1", 0x20_0000), ("misc2", 0x40_0000),
        ("paddle1", 0x1_0000), ("paddle2", 0x2_0000), ("paddle3", 0x4_0000), ("paddle4", 0x8_0000),
    ]

    /// The wire bit of a stored button name, any case; `back` is `select`.
    public static func buttonBit(_ name: String) -> UInt32? {
        var n = name.trimmingCharacters(in: .whitespaces).lowercased()
        if n == "back" { n = "select" }
        return buttons.first { $0.name == n }?.bit
    }

    public static func buttonName(_ bit: UInt32) -> String? {
        buttons.first { $0.bit == bit }?.name
    }

    /// The keyboard modifiers a shortcut may hold, in display order. ⌘ and Win are the host's.
    public static let modifiers = ["ctrl", "alt", "shift"]

    static func modifier(_ name: String) -> String? {
        switch name.trimmingCharacters(in: .whitespaces).lowercased() {
        case "ctrl", "control": return "ctrl"
        case "alt", "option": return "alt"
        case "shift": return "shift"
        default: return nil
        }
    }

    /// F1–F24: a shortcut on its own, as F11 is.
    static func functionKey(_ key: String) -> Bool {
        guard key.hasPrefix("f"), let n = UInt8(key.dropFirst()) else { return false }
        return (1...24).contains(n)
    }

    // MARK: - The blob

    /// Never fails: an unparseable blob is empty, and a binding with a name this build does not
    /// know is dropped. An unknown action keeps its row as `.unknown`.
    public static func parse(_ json: String) -> Shortcuts {
        read(json) ?? Shortcuts()
    }

    /// `nil` when the blob is not a shortcuts blob at all; the caller takes the defaults.
    static func read(_ json: String) -> Shortcuts? {
        guard let data = json.data(using: .utf8),
              let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let list = obj["bindings"] as? [Any]
        else { return nil }
        return Shortcuts(bindings: list.compactMap { ($0 as? [String: Any]).flatMap(stored) })
    }

    /// One stored binding; `nil` when a field is missing or a name is unknown.
    private static func stored(_ b: [String: Any]) -> Binding? {
        guard let on = b["on"] as? String, let keys = b["keys"] as? [String],
              let id = b["do"] as? String
        else { return nil }
        let hold: Bool
        switch b["hold"] {
        case nil, is NSNull: hold = false
        case let v as Bool: hold = v
        default: return nil
        }
        let trigger: Trigger
        switch on {
        case "pad":
            var bits: [UInt32] = []
            for name in keys {
                guard let bit = buttonBit(name) else { return nil }
                if !bits.contains(bit) { bits.append(bit) }
            }
            guard !bits.isEmpty else { return nil }
            trigger = .pad(buttons: bits, hold: hold)
        case "key":
            guard let last = keys.last else { return nil }
            let key = last.trimmingCharacters(in: .whitespaces).lowercased()
            guard keyVk(key) != nil else { return nil }
            var mods: [String] = []
            for m in keys.dropLast() {
                guard let canon = modifier(m) else { return nil }
                mods.append(canon)
            }
            trigger = .key(modifiers: modifiers.filter(mods.contains), key: key)
        default:
            return nil
        }
        return Binding(trigger: trigger, action: Action(id: id))
    }

    public func toJSON() -> String {
        let list: [[String: Any]] = bindings.map { b in
            switch b.trigger {
            case .pad(let bits, let hold):
                return ["on": "pad", "keys": bits.compactMap(Self.buttonName), "hold": hold,
                        "do": b.action.id]
            case .key(let mods, let key):
                return ["on": "key", "keys": mods + [key], "do": b.action.id]
            }
        }
        let obj: [String: Any] = ["v": Self.schemaVersion, "bindings": list]
        guard let data = try? JSONSerialization.data(withJSONObject: obj, options: [.sortedKeys]),
              let text = String(data: data, encoding: .utf8)
        else { return "" }
        return text
    }

    // MARK: - Validation

    /// Whether `candidate` may join these bindings. An edit is checked against the list without
    /// the binding it changes.
    public func validate(_ candidate: Binding) -> Conflict? {
        switch candidate.trigger {
        case .key(let mods, let key):
            if key.isEmpty || Self.modifier(key) != nil { return .noKey }
            if mods.isEmpty && !Self.functionKey(key) { return .noModifier }
        case .pad(let bits, let hold):
            if bits.count < 2 && !(hold && bits.count == 1) { return .singlePress }
        }
        let ids = candidate.trigger.ids
        let device = candidate.trigger.device
        let same = bindings.indices.filter { bindings[$0].trigger.device == device }
        if let i = same.first(where: {
            bindings[$0].trigger.ids == ids && bindings[$0].trigger.hold == candidate.trigger.hold
        }) {
            return .identical(i)
        }
        // Modifiers are down before the key, so only pad presses can cut a chord short.
        guard device == .pad else { return nil }
        func strict(_ small: [UInt32], _ big: [UInt32]) -> Bool {
            small.count < big.count && small.allSatisfy(big.contains)
        }
        if let i = same.first(where: {
            let other = bindings[$0].trigger
            return (!candidate.trigger.hold && strict(ids, other.ids))
                || (!other.hold && strict(other.ids, ids))
        }) {
            return .pressSubset(i)
        }
        return nil
    }

    // MARK: - Defaults

    /// This client's defaults. `legacyGuideGesture` is the old *Hold Select for guide* value:
    /// `on` and `off` decide the Hold Select row, anything else is the client's automatic.
    public static func defaults(
        client: Client, gameMode: Bool = false, legacyGuideGesture: String = "auto"
    ) -> Shortcuts {
        let guide: Bool
        switch legacyGuideGesture {
        case "on": guide = true
        case "off": guide = false
        default: guide = client == .desktop ? gameMode : client == .apple
        }
        // The clients with their own pad path; the others' dial is their only pad chord.
        let native = [.desktop, .android, .apple, .mac].contains(client)
        let open = client == .ps5 ? "touchpad" : "select"
        let exit = ["lb", "rb", "start", client == .ps5 ? "touchpad" : "select"]
        var b: [Binding] = []
        b.append(pad([open, "a"], hold: false, .ring))
        if native { b.append(pad(["select", "x"], hold: false, .slot(.stats))) }
        if client == .android { b.append(pad(["select", "y"], hold: false, .slot(.mic))) }
        if native && guide { b.append(pad(["select"], hold: true, .slot(.guide))) }
        if [.desktop, .web, .tizen].contains(client) {
            b.append(pad(exit, hold: false, .slot(.releaseInput)))
        }
        b.append(pad(exit, hold: true, .slot(.endStream)))
        func chord(_ k: String, _ action: Action) -> Binding {
            key(["ctrl", "alt", "shift"], k, action)
        }
        switch client {
        case .desktop, .web, .tizen:
            b += [
                chord("q", .slot(.releaseInput)), chord("d", .slot(.endStream)),
                chord("s", .slot(.stats)), chord("o", .ring), chord("v", .slot(.mic)),
                chord("m", .slot(.mouseMode)), key([], "f11", .slot(.fullscreen)),
                key(["alt"], "enter", .slot(.fullscreen)),
            ]
        case .android:
            b += [chord("q", .slot(.releaseInput)), chord("o", .ring)]
        case .apple, .mac:
            b += [
                chord("q", .slot(.releaseInput)), chord("d", .slot(.endStream)),
                chord("s", .slot(.stats)), chord("o", .ring),
            ]
            if client == .mac { b.append(chord("m", .slot(.mouseMode))) }
            b += [chord("c", .slot(.clipboard)), chord("a", .slot(.mic))]
        case .webOS, .ps5:
            break
        }
        return Shortcuts(bindings: b)
    }

    /// The bindings an engine on this device runs: what it can do, and the dial only while
    /// something opens it, so those buttons otherwise reach the game.
    public func runnable(dial: Bool) -> [Binding] {
        bindings.filter { b in
            switch b.action {
            case .ring: return dial
            case .slot(let slot): return slot.runsHere
            case .unknown: return false
            }
        }
    }

    /// The stored blob, or this client's defaults when it is empty or unreadable.
    public static func resolve(
        _ blob: String, client: Client, gameMode: Bool = false, legacyGuideGesture: String = "auto"
    ) -> Shortcuts {
        read(blob) ?? defaults(client: client, gameMode: gameMode, legacyGuideGesture: legacyGuideGesture)
    }

    private static func pad(_ names: [String], hold: Bool, _ action: Action) -> Binding {
        Binding(trigger: .pad(buttons: names.compactMap(buttonBit), hold: hold), action: action)
    }

    private static func key(_ mods: [String], _ key: String, _ action: Action) -> Binding {
        Binding(trigger: .key(modifiers: mods, key: key), action: action)
    }
}

// MARK: - Rows

extension Shortcuts {
    /// Which device a group's rows are pressed on.
    public enum Kind: Sendable {
        case remote, keyboard, touch, pad
    }

    /// One row of the Shortcuts page: what to press, then what it does.
    public struct Row: Identifiable, Sendable {
        public let id: String
        /// Key and button names are English data; a gesture's name and "Hold …" are messages.
        public let keys: String
        public let text: String
        /// The binding this row edits; `nil` for a fixed row. Two presses of one action on one
        /// device share a row, which edits the first.
        public let binding: Int?
    }

    public struct Group: Identifiable, Sendable {
        public let kind: Kind
        public let title: String
        /// A quieter line beside the title: when the rows apply, or what a button is called.
        public let note: String?
        public let rows: [Row]
        public var id: String { "\(kind)" }
    }

    /// The groups a client shows, remote and keyboard first. `touchscreen` is false on a TV,
    /// which binds no keys and lists the Siri Remote instead. `overlay` names key combinations.
    public func rows(client: Client, touchscreen: Bool, overlay: OverlayConfig? = nil) -> [Group] {
        var out: [Group] = []
        let tv = client == .apple && !touchscreen
        if tv {
            let hold = { L10n.controlsHold(keys: $0) }
            out.append(Group(kind: .remote, title: String(localized: L10n.controlsGroupRemote), note: nil, rows: [
                fixed("back", "Back", String(localized: L10n.controlsOpenDial)),
                fixed("back_hold", hold("Back"), String(localized: L10n.controlsDisconnect)),
                fixed("surface", String(localized: L10n.appleControlsTouchSurface),
                      String(localized: L10n.appleControlsMovePointer)),
                fixed("press", String(localized: L10n.appleControlsPress),
                      String(localized: L10n.appleControlsClick)),
                fixed("play", "Play/Pause", String(localized: L10n.appleControlsRightClick)),
                fixed("play_hold", hold("Play/Pause"), String(localized: L10n.controlsCycleStats)),
            ]))
        } else {
            var keys = items(.keyboard, client: client, overlay: overlay)
            if client == .mac {
                keys += [
                    fixed("click", String(localized: L10n.controlsGestureClick),
                          String(localized: L10n.controlsCaptureMouseKeyboard)),
                    fixed("cmd_esc", "⌘⎋", String(localized: L10n.controlsReleaseMouseKeyboard)),
                    fixed("ctrl_cmd_f", "⌃⌘F", L10n.controlsToggleFullscreenAfter(keys: "⌘⎋")),
                ]
            } else if client == .apple {
                keys.append(fixed("cmd_esc", "⌘⎋", String(localized: L10n.controlsReleasePointerDevice)))
            }
            out.append(Group(kind: .keyboard, title: String(localized: L10n.controlsGroupKeyboard),
                             note: nil, rows: keys))
        }
        if client == .apple, touchscreen {
            func g(_ id: String, _ k: LocalizedStringResource, _ t: LocalizedStringResource) -> Row {
                fixed(id, String(localized: k), String(localized: t))
            }
            out.append(Group(kind: .touch, title: String(localized: L10n.controlsGroupTouch), note: nil, rows: [
                g("twist", L10n.controlsGestureTwoFingerTwist, L10n.controlsOpenDial),
                g("tap", L10n.controlsGestureThreeFingerTap, L10n.controlsCycleStats),
                g("swipe_up", L10n.controlsGestureThreeFingerSwipeUp, L10n.controlsShowKeyboard),
                g("swipe_down", L10n.controlsGestureThreeFingerSwipeDown, L10n.controlsHideKeyboard),
                g("side_edge", L10n.appleControlsGestureSideEdge, L10n.appleControlsOpenDialPassthrough),
            ]))
        }
        out.append(Group(
            kind: .pad, title: String(localized: L10n.controlsGroupPad),
            note: client == .ps5 ? nil : L10n.controlsGroupPadNote(select: "Select", back: "Back", view: "View"),
            rows: items(.pad, client: client, overlay: overlay)))
        return out
    }

    /// The first trigger on `device` bound to `action`, as one line: "Hold L1 + R1 + Start +
    /// Select", "⌃⌥⇧D". For the exit hint, the start banner and the settings captions.
    public func hint(_ action: Action, device: Device, client: Client = .apple) -> String? {
        trigger(for: action, device: device)?.label(client: client)
    }

    /// The first trigger on `device` bound to `action`.
    public func trigger(for action: Action, device: Device) -> Trigger? {
        bindings.first { $0.action == action && $0.trigger.device == device }?.trigger
    }

    /// One row per binding on `device`; two presses of one action share an "either" row.
    private func items(_ device: Device, client: Client, overlay: OverlayConfig?) -> [Row] {
        var shown = Set<Int>()
        var rows: [Row] = []
        for (i, b) in bindings.enumerated() where !shown.contains(i) && b.trigger.device == device {
            var keys = b.trigger.label(client: client)
            if !b.trigger.hold, let j = bindings.indices.first(where: {
                $0 > i && bindings[$0].action == b.action && bindings[$0].trigger.device == device
                    && !bindings[$0].trigger.hold
            }) {
                shown.insert(j)
                keys = L10n.controlsEither(first: keys, second: bindings[j].trigger.chord(client: client))
            }
            rows.append(Row(id: "b\(i)", keys: keys, text: b.action.label(overlay), binding: i))
        }
        return rows
    }

    private func fixed(_ id: String, _ keys: String, _ text: String) -> Row {
        Row(id: id, keys: keys, text: text, binding: nil)
    }

    /// A button as the rows name it: Xbox positions with Select and Start and PlayStation
    /// shoulders, or the DualSense's own names.
    public static func buttonLabel(_ bit: UInt32, dualsense: Bool = false) -> String {
        let ds: [UInt32: String] = [
            0x1000: "Cross", 0x2000: "Circle", 0x4000: "Square", 0x8000: "Triangle",
            0x0010: "Options", 0x0020: "Create", 0x0400: "PS",
        ]
        if dualsense, let name = ds[bit] { return name }
        let names: [UInt32: String] = [
            0x1000: "A", 0x2000: "B", 0x4000: "X", 0x8000: "Y", 0x0100: "L1", 0x0200: "R1",
            0x0040: "L3", 0x0080: "R3", 0x0010: "Start", 0x0020: "Select", 0x0400: "Guide",
            0x0001: "Up", 0x0002: "Down", 0x0004: "Left", 0x0008: "Right",
            0x10_0000: "Touchpad", 0x20_0000: "Share", 0x40_0000: "C",
            0x1_0000: "R4", 0x2_0000: "L4", 0x4_0000: "R5", 0x8_0000: "L5",
        ]
        return names[bit] ?? "?"
    }
}

extension Shortcuts.Trigger {
    /// The keys alone: "Select + A", "⌃⌥⇧Q" on Apple, "Ctrl+Alt+Shift+Q" elsewhere.
    public func chord(client: Shortcuts.Client = .apple) -> String {
        switch self {
        case .pad(let bits, _):
            return bits.map { Shortcuts.buttonLabel($0, dualsense: client == .ps5) }
                .joined(separator: " + ")
        case .key(let mods, let key):
            guard client == .apple || client == .mac else {
                return (mods + [key]).map(keyLegend).joined(separator: "+")
            }
            let glyphs = mods.map { ["ctrl": "⌃", "alt": "⌥", "shift": "⇧"][$0] ?? keyLegend($0) }
            return glyphs.joined() + keyLegend(key)
        }
    }

    /// The whole line: the chord, or "Hold …" for a hold.
    public func label(client: Shortcuts.Client = .apple) -> String {
        hold ? L10n.controlsHold(keys: chord(client: client)) : chord(client: client)
    }
}

extension Shortcuts.Action {
    /// The dial's word for the action, or its id where this build has none. `overlay` names
    /// the key combinations.
    public func label(_ overlay: OverlayConfig? = nil) -> String {
        switch self {
        case .ring: return String(localized: L10n.ringOpenRing)
        case .unknown(let id): return id
        case .slot(let slot): return slot.label(overlay)
        }
    }

    /// Bound to something this build or this preset has no target for: the row stays, dimmed.
    public func dangling(_ overlay: OverlayConfig) -> Bool {
        switch self {
        case .unknown: return true
        case .slot(.shortcut(let id)): return overlay.shortcut(id) == nil
        default: return false
        }
    }
}

extension SlotId {
    /// Whether this kind of device runs the slot at all. The dial dims the rest, and a shortcut
    /// to one stays out of the engine, so its buttons reach the game.
    public var runsHere: Bool {
        switch self {
        case .sendText, .streamMute, .swapScreens: return false
        #if os(macOS)
        case .touchMode, .pad, .keyboard: return false
        #else
        case .mouseMode, .fullscreen, .minimize: return false
        #endif
        #if os(tvOS)
        case .touchMode, .pad, .keyboard, .mic, .releaseInput, .clipboard: return false
        #endif
        default: return true
        }
    }

    /// The catalogue's word for a slot; a key combination by its name or its keys.
    public func label(_ overlay: OverlayConfig? = nil) -> String {
        let r: LocalizedStringResource
        switch self {
        case .endStream: r = L10n.ringEndStream
        case .endGame: r = L10n.ringEndGame
        case .disconnectLinger: r = L10n.ringDisconnectLinger
        case .touchMode: r = L10n.ringTouchMode
        case .keyboard: r = L10n.ringKeyboard
        case .stats: r = L10n.ringStats
        case .mic: r = L10n.ringMic
        case .pad: r = L10n.ringPad
        case .sendText: r = L10n.ringSendText
        case .guide: r = L10n.ringGuide
        case .qam: r = L10n.ringQam
        case .padMouse: r = L10n.ringPadMouse
        case .padType: r = L10n.ringPadType
        case .streamMute: r = L10n.ringStreamMute
        case .swapScreens: r = L10n.ringSwapScreens
        case .releaseInput: r = L10n.ringReleaseInput
        case .mouseMode: r = L10n.ringMouseMode
        case .fullscreen: r = L10n.ringFullscreen
        case .minimize: r = L10n.ringMinimize
        case .clipboard: r = L10n.ringClipboard
        case .host(let id):
            guard let word = Self.hostWord(id) else { return self.id }
            r = word
        case .shortcut(let id):
            guard let sc = overlay?.shortcut(id) else { return self.id }
            return sc.label.isEmpty ? chordChip(sc.keys) : sc.label
        }
        return String(localized: r)
    }

    /// This app's word for a host action id it knows; nil for one the host alone names.
    public static func hostWord(_ id: String) -> LocalizedStringResource? {
        switch id {
        case "power.sleep": L10n.hostPowerSleep
        case "power.reboot": L10n.hostPowerReboot
        case "power.shutdown": L10n.hostPowerShutdown
        case "host.restart": L10n.hostPowerRestartService
        case "display.next": L10n.hostactionDisplayNext
        default: nil
        }
    }
}
