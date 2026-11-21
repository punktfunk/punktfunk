// Which of iOS's two hardware-key feeds speaks for a key edge.
//
// GameController is the feed. Full Keyboard Access silences it while UIKit presses still
// arrive, so presses carry the keys until GameController speaks. Around that moment both can
// report one key: GameController's press of a key the press feed holds is dropped, and its
// release ends that key.

struct KeyFeed {
    /// GameController has delivered a key since the last `reset`.
    private(set) var gameControllerSpoke = false
    /// HID usages the press feed put down and has not released.
    private var pressHeld: Set<Int> = []

    /// A GameController edge. False for the echo of a key the press feed holds.
    mutating func takeGameController(usage: Int, down: Bool) -> Bool {
        gameControllerSpoke = true
        guard pressHeld.contains(usage) else { return true }
        if down { return false }
        pressHeld.remove(usage)
        return true
    }

    /// A UIKit press edge. False once GameController has spoken: it carries the keys.
    mutating func takePress(usage: Int, down: Bool) -> Bool {
        guard !gameControllerSpoke else { return false }
        if down { pressHeld.insert(usage) } else { pressHeld.remove(usage) }
        return true
    }

    /// Full Keyboard Access can change while the app is inactive: both feeds start over.
    mutating func reset() { self = KeyFeed() }
}
