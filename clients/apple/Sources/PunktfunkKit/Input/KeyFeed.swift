// Which of iOS's two hardware-key feeds speaks for a key edge.
//
// GameController is the feed. Full Keyboard Access silences it while UIKit presses still
// arrive, so presses carry the keys whenever GameController is silent. Around a change both can
// report one key: GameController's press of a key the press feed holds is dropped, and its
// release ends that key. Two whole keys in a row that go down and up as presses, with no word
// from GameController, say it fell silent. One is not enough: either feed leads the other by
// milliseconds on any key.

import Foundation
import GameController
import PunktfunkShared

struct KeyFeed {
    /// What the caller does with a press edge.
    enum Press: Equatable {
        /// GameController carries the keys.
        case ignore
        /// The press feed carries this edge.
        case take
        /// GameController fell silent, and this key's press went by unsent: replay the key.
        case missed
    }

    /// GameController has delivered a key since the last `reset`, and has not fallen silent.
    private(set) var gameControllerSpoke = false
    /// Presses carried a whole key, down and up, with GameController silent throughout: the
    /// keyboard is being reposted, which is what Full Keyboard Access does.
    private(set) var pressesCarriedKey = false
    /// HID usages the press feed put down and has not released.
    private var pressHeld: Set<Int> = []
    /// GameController events so far, and that count when each ignored press went down.
    private var gameControllerEvents = 0
    private var ignoredDown: [Int: Int] = [:]
    /// Whole ignored keys in a row with no GameController event since the first went down.
    private var silentKeys = 0

    /// A GameController edge. False for the echo of a key the press feed holds.
    mutating func takeGameController(usage: Int, down: Bool) -> Bool {
        gameControllerSpoke = true
        pressesCarriedKey = false
        gameControllerEvents += 1
        silentKeys = 0
        guard pressHeld.contains(usage) else { return true }
        if down { return false }
        pressHeld.remove(usage)
        return true
    }

    /// A UIKit press edge. `modelled`: GameController has an element for the key, so its
    /// silence about it means something.
    mutating func takePress(usage: Int, down: Bool, modelled: Bool = true) -> Press {
        guard gameControllerSpoke else {
            if down {
                pressHeld.insert(usage)
            } else if pressHeld.remove(usage) != nil {
                pressesCarriedKey = true
            }
            return .take
        }
        if down {
            if modelled { ignoredDown[usage] = gameControllerEvents }
            return .ignore
        }
        guard let since = ignoredDown.removeValue(forKey: usage) else { return .ignore }
        guard since == gameControllerEvents else {
            silentKeys = 0
            return .ignore
        }
        silentKeys += 1
        guard silentKeys >= 2 else { return .ignore }
        gameControllerSpoke = false
        pressesCarriedKey = true
        ignoredDown.removeAll()
        silentKeys = 0
        return .missed
    }

    /// Full Keyboard Access can change while the app is inactive: both feeds start over.
    mutating func reset() { self = KeyFeed() }
}

/// Full Keyboard Access drives the keyboard, as last proven. No API reports the setting. The
/// proofs are a key that presses carried with GameController silent (`KeyFeed`) and an
/// accessibility request it sent (`StreamLayerUIView`, `FullKeyboardAccessProbe`); a key from
/// GameController disproves it. Stored, so the next stream starts in its Input mode.
@MainActor
enum FullKeyboardAccess {
    /// Posted when `proven` changes, for a view that holds the keyboard outside a stream.
    static let didChange = Notification.Name("io.unom.punktfunk.fullKeyboardAccess")

    static var proven = UserDefaults.standard.bool(forKey: DefaultsKey.fullKeyboardAccess) {
        didSet {
            guard proven != oldValue else { return }
            UserDefaults.standard.set(proven, forKey: DefaultsKey.fullKeyboardAccess)
            watchGameController()
            NotificationCenter.default.post(name: didChange, object: nil)
        }
    }

    /// Outside a stream nothing reads GameController, so a proof that has gone stale would
    /// stand. While proven, the keyboard's free handler slot holds a watch that disproves it
    /// on any key. A stream's capture overwrites the slot, and `stop` calls this again.
    static func watchGameController() {
        guard proven, let input = GCKeyboard.coalesced?.keyboardInput,
              input.keyChangedHandler == nil
        else { return }
        input.keyChangedHandler = { input, _, _, _ in
            input.keyChangedHandler = nil
            MainActor.assumeIsolated { proven = false }
        }
    }
}
