// What a button of the keyboard row sends on a tap and on a long press, and what a latched
// modifier does with typed text. Pure: `KeyRowView` owns the fingers and the wire. `KeyRowTests`
// replays `clients/shared/key-row-vectors.json`, as the Android client does, so a rule changes
// in that file first.

import Foundation
import PunktfunkShared

/// One thing the row sends, in order.
enum KeyRowOut: Equatable {
    case key(UInt32, down: Bool)
    case text(String)
    /// A quick action for the session's shortcut path, by the phases a held shortcut uses.
    case fire(Shortcuts.Action, Shortcuts.Phase)
}

struct KeyRowEngine {
    /// Ctrl, Alt, Shift and Win: the keys that latch.
    static let modifiers: Set<UInt32> = [0x11, 0x12, 0x10, 0x5B]

    /// The modifiers down on the host, in the order they were tapped.
    private(set) var latched: [UInt32] = []
    /// The buttons a long press holds down.
    private var held: [RowSlot] = []

    /// Whether `button` is a modifier the row holds right now, which lights its cap.
    func isLatched(_ button: RowSlot) -> Bool {
        guard case .key(let name) = button, let vk = keyVk(name) else { return false }
        return latched.contains(vk)
    }

    /// A modifier latches, or lets go on its second tap. Any other key goes down and up, and a
    /// key combination fires as a tap: both are the next key, so the latched modifiers lift
    /// after them. Every other action fires and leaves the latch alone.
    mutating func tap(_ button: RowSlot) -> [KeyRowOut] {
        switch button {
        case .key(let name):
            guard let vk = keyVk(name) else { return [] }
            guard Self.modifiers.contains(vk) else {
                return [.key(vk, down: true), .key(vk, down: false)] + release()
            }
            if let at = latched.firstIndex(of: vk) {
                latched.remove(at: at)
                return [.key(vk, down: false)]
            }
            latched.append(vk)
            return [.key(vk, down: true)]
        case .action(let slot):
            let action = Shortcuts.Action.slot(slot)
            let fire = KeyRowOut.fire(action, action.kind == .keyLike ? .tap : .once)
            return [fire] + (Self.isCombination(slot) ? release() : [])
        }
    }

    /// A long press began: a key or a key-like action goes down until `lift`, so the host
    /// repeats it. A modifier and any other action take it as a tap.
    mutating func hold(_ button: RowSlot) -> [KeyRowOut] {
        switch button {
        case .key(let name):
            guard let vk = keyVk(name), !Self.modifiers.contains(vk) else { return tap(button) }
            held.append(button)
            return [.key(vk, down: true)]
        case .action(let slot):
            let action = Shortcuts.Action.slot(slot)
            guard action.kind == .keyLike else { return tap(button) }
            held.append(button)
            return [.fire(action, .down)]
        }
    }

    /// The finger is gone, lifted or cancelled: what its long press holds comes up.
    mutating func lift(_ button: RowSlot) -> [KeyRowOut] {
        guard let at = held.firstIndex(of: button) else { return [] }
        held.remove(at: at)
        switch button {
        case .key(let name):
            guard let vk = keyVk(name) else { return [] }
            return [.key(vk, down: false)] + release()
        case .action(let slot):
            return [.fire(.slot(slot), .up)] + (Self.isCombination(slot) ? release() : [])
        }
    }

    /// `text` typed on the system keyboard toward a host that takes text. `nil` while no
    /// modifier is latched: the caller types it as it always does. Under a latch one character
    /// with a key (`typedVk`) goes down and up beneath the modifiers; anything else goes as text
    /// once they are up, since text carries no modifiers.
    mutating func type(_ text: String) -> [KeyRowOut]? {
        guard !latched.isEmpty, !text.isEmpty else { return nil }
        if text.count == 1, let vk = Self.typedVk(text[text.startIndex]) {
            return [.key(vk, down: true), .key(vk, down: false)] + release()
        }
        return release() + [.text(text)]
    }

    /// Every latched modifier comes up: the next key lifted.
    mutating func release() -> [KeyRowOut] {
        defer { latched = [] }
        return latched.map { .key($0, down: false) }
    }

    /// The row is going, with the keyboard or the stream: every held button lifts, then every
    /// latched modifier.
    mutating func end() -> [KeyRowOut] {
        var out: [KeyRowOut] = []
        for button in held { out += lift(button) }
        return out + release()
    }

    /// The key that types `ch` unshifted on a US layout, a capital as its letter, Tab and
    /// Return as theirs; `nil` for a character with no such key.
    static func typedVk(_ ch: Character) -> UInt32? {
        guard let lower = ch.lowercased().first,
              let key = SoftKeyMap.vk(for: lower), !key.shift else { return nil }
        return key.vk
    }

    private static func isCombination(_ slot: SlotId) -> Bool {
        if case .shortcut = slot { return true }
        return false
    }
}
