// A key combination as the shortcut editors hold it: the modifier toggles and at most one key
// from the keyboard they draw. The Swift twin of pf-client-core's `Chord` (`overlay_actions.rs`);
// `ChordTests` pins it. A modifier alone is a combination: Win opens the host's Start menu.

import Foundation

public struct Chord: Equatable, Sendable {
    /// The modifier toggles, in host send order.
    public static let modifiers = ["ctrl", "alt", "shift", "win"]

    /// The keys a chord can end on, grouped the way a keyboard groups them: every name `keyVk`
    /// knows that is neither a modifier nor an alias.
    public static let keyGroups: [(id: String, title: String, keys: [String])] = [
        ("function", String(localized: L10n.ringKeyGroupFunction), ["escape"] + (1...12).map { "f\($0)" }),
        ("letters", String(localized: L10n.ringKeyGroupLetters), "qwertyuiopasdfghjklzxcvbnm".map(String.init)),
        ("numbers", String(localized: L10n.ringKeyGroupNumbers), (1...9).map(String.init) + ["0"]),
        ("editing", String(localized: L10n.ringKeyGroupEditing),
         ["tab", "space", "enter", "backspace", "delete", "insert"]),
        ("navigation", String(localized: L10n.ringKeyGroupNavigation),
         ["home", "end", "pageup", "pagedown", "up", "down", "left", "right"]),
        ("other", String(localized: L10n.ringKeyGroupOther), ["printscreen", "pause", "capslock"]),
    ]

    public var mods: Set<String>
    public var key: String?

    public init(mods: Set<String> = [], key: String? = nil) {
        self.mods = mods
        self.key = key
    }

    /// Read stored keys: the modifiers among them, and the first key that is not one.
    public static func parse(keys: [String]) -> Chord {
        Chord(mods: Set(keys.filter(modifiers.contains)), key: keys.first { !modifiers.contains($0) })
    }

    /// Host send order: the held modifiers, then the key.
    public var keys: [String] {
        Self.modifiers.filter(mods.contains) + (key.map { [$0] } ?? [])
    }

    /// A modifier chip's tap.
    public mutating func toggle(_ modifier: String) {
        if mods.remove(modifier) == nil { mods.insert(modifier) }
    }
}
