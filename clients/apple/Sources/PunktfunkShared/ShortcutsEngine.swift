// The shortcut engine: one device's presses and releases in, what the host and the client get
// out. Pure: the caller owns the clock and the wire. Twin of pf-client-core's `engine.rs`.

import Foundation

extension Shortcuts {
    /// A held trigger fires after this long.
    public static let hold: Duration = .milliseconds(1000)
    /// A held-back tap's release follows its press by this, so the two don't fold into one pad
    /// state on the wire.
    public static let tapPress: Duration = .milliseconds(50)
    /// A key-like hold released after this long is a tap of its key, not of the button.
    public static let keyTap: Duration = .milliseconds(350)

    /// A pad's wire bit, or a key's VK with left and right modifiers folded onto
    /// `keyVk("ctrl" | "alt" | "shift")`.
    public enum Event: Equatable, Sendable {
        case press(UInt32)
        case release(UInt32)
    }

    public enum Phase: Equatable, Sendable {
        case once
        /// A key-like action held: down on the host until `.up`.
        case down
        case up
        /// A key-like action pressed and released at once.
        case tap
    }

    public enum Out: Equatable, Sendable {
        /// Send this transition on, as input to the host (it obeys the input mask).
        case forward(UInt32, Bool)
        case fire(Action, Phase)
        /// Release these on the host, mask or not: the rest of a chord that fired.
        case lift([UInt32])
    }

    /// One pad's or the keyboard's shortcuts, by the rules of design/client-shortcuts.md §4.
    /// Feed every transition to `step`, call `tick` on every poll (late only delays), and
    /// `flush` when the device goes away. Pass only available actions.
    public struct Engine: Sendable {
        public typealias Instant = ContinuousClock.Instant

        private struct Rule: Sendable {
            let ids: [UInt32]
            let hold: Bool
            let action: Action
        }

        private struct Armed: Sendable {
            let since: Instant
            let hold: Int
            /// The press with the same buttons: it fires when the chord lifts before `hold`.
            let twin: Int?
        }

        private let device: Device
        private let rules: [Rule]
        /// Physically down, in press order.
        private var down: [UInt32] = []
        /// Down on the host because of a `.forward`.
        private var forwarded: [UInt32] = []
        /// Pressed, not sent: a held chord's presses, only while its hold is armed.
        private var heldBack: [UInt32] = []
        private var armed: Armed?
        /// Buttons of a chord that fired: their releases are dropped.
        private var claimed: [UInt32] = []
        /// A key-like hold that went down, and the buttons whose first lift ends it.
        private var keyDown: (action: Action, ids: [UInt32])?
        /// A tap's release, due at the instant.
        private var owed: [(due: Instant, bit: UInt32)] = []

        /// Keeps `device`'s bindings the engine can act on: no unknown actions, no keyboard
        /// holds, and no one-button pad press (that tap is the game's).
        public init(_ bindings: [Binding], device: Device) {
            self.device = device
            rules = bindings
                .filter { b in
                    if case .unknown = b.action { return false }
                    return b.trigger.device == device
                }
                .map { Rule(ids: $0.trigger.ids, hold: $0.trigger.hold, action: $0.action) }
                .filter { r in
                    device == .pad ? r.hold || r.ids.count >= 2 : !r.hold && !r.ids.isEmpty
                }
        }

        /// The buttons some rule reads, for a caller that claims them from the OS.
        public var buttons: Set<UInt32> { Set(rules.flatMap(\.ids)) }

        /// When `tick` next has work: an armed hold or an owed release. `nil` while idle, so an
        /// event-driven caller needs no poll.
        public var nextDue: Instant? {
            (owed.map(\.due) + (armed.map { [$0.since + Shortcuts.hold] } ?? [])).min()
        }

        /// One transition at `now`. A tick that was due runs first.
        public mutating func step(_ event: Event, at now: Instant) -> [Out] {
            var out = tick(now)
            switch event {
            case .press(let b): press(b, now, &out)
            case .release(let b): release(b, now, &out)
            }
            settle(now, &out)
            return out
        }

        /// Owed tap releases and an armed hold that came due.
        public mutating func tick(_ now: Instant) -> [Out] {
            var out: [Out] = []
            owed.removeAll { o in
                guard now >= o.due else { return false }
                out.append(.forward(o.bit, false))
                return true
            }
            if let a = armed, now - a.since >= Shortcuts.hold {
                fire(a.hold, rules[a.hold].action.kind == .once ? .once : .down, &out)
            }
            return out
        }

        /// The device is gone: a key-like hold comes up, and the host lets go of everything this
        /// engine sent down or still owes. Forgets every press.
        public mutating func flush() -> [Out] {
            var out: [Out] = []
            if let k = keyDown {
                keyDown = nil
                out.append(.fire(k.action, .up))
            }
            let lift = owed.map(\.bit) + forwarded
            if !lift.isEmpty { out.append(.lift(lift)) }
            owed = []
            forwarded = []
            down = []
            heldBack = []
            claimed = []
            armed = nil
            return out
        }

        private mutating func press(_ b: UInt32, _ now: Instant, _ out: inout [Out]) {
            // Key repeat, or a press the engine already counts.
            if down.contains(b) { return }
            if let i = owed.firstIndex(where: { $0.bit == b }) {
                owed.remove(at: i)
                out.append(.forward(b, false))
            }
            down.append(b)
            armed = nil
            // While a key-like hold is down, a press is the host's: Guide+A is Steam's chord.
            if !isModifier(b), keyDown == nil {
                switch (exact(hold: false), exact(hold: true)) {
                case let (p?, nil):
                    return fire(p, rules[p].action.kind == .once ? .once : .tap, &out)
                case let (twin, hold?):
                    heldBack.append(b)
                    armed = Armed(since: now, hold: hold, twin: twin)
                    return
                case (nil, nil):
                    break
                }
            }
            // A partner that completes nothing makes a held-back press real, sent first.
            sendHeldBack(&out)
            forwarded.append(b)
            out.append(.forward(b, true))
        }

        private mutating func release(_ b: UInt32, _ now: Instant, _ out: inout [Out]) {
            guard let i = down.firstIndex(of: b) else {
                // Pressed before this engine: the host may hold it.
                out.append(.forward(b, false))
                return
            }
            down.remove(at: i)
            if let k = keyDown, k.ids.contains(b) {
                keyDown = nil
                out.append(.fire(k.action, .up))
            }
            if let c = claimed.firstIndex(of: b) {
                claimed.remove(at: c)
                return
            }
            // Armed means down was the hold's chord, so `b` is one of its buttons.
            if let a = armed {
                armed = nil
                if let twin = a.twin {
                    return fire(twin, rules[twin].action.kind == .once ? .once : .tap, &out)
                }
                if rules[a.hold].action.kind == .keyLike, now - a.since >= Shortcuts.keyTap {
                    return fire(a.hold, .tap, &out)
                }
            }
            if let h = heldBack.firstIndex(of: b) {
                heldBack.remove(at: h)
                out.append(.forward(b, true))
                owed.append((now + Shortcuts.tapPress, b))
                return
            }
            forwarded.removeAll { $0 == b }
            out.append(.forward(b, false))
        }

        /// The rule fires: the host lets go of the chord, the held-back press is never sent, and
        /// every button still down is the chord's until it lifts.
        private mutating func fire(_ rule: Int, _ phase: Phase, _ out: inout [Out]) {
            if !forwarded.isEmpty { out.append(.lift(forwarded)) }
            forwarded = []
            let action = rules[rule].action
            out.append(.fire(action, phase))
            if phase == .down { keyDown = (action, rules[rule].ids) }
            heldBack = []
            armed = nil
            for b in down where !claimed.contains(b) { claimed.append(b) }
        }

        /// A press stays held back only while its hold is armed. When a release disarms it, a
        /// hold still exactly down (a nested one) arms from `now`; otherwise the press goes out.
        private mutating func settle(_ now: Instant, _ out: inout [Out]) {
            guard armed == nil, !heldBack.isEmpty else { return }
            if let hold = exact(hold: true) {
                armed = Armed(since: now, hold: hold, twin: exact(hold: false))
            } else {
                sendHeldBack(&out)
            }
        }

        private mutating func sendHeldBack(_ out: inout [Out]) {
            for h in heldBack {
                forwarded.append(h)
                out.append(.forward(h, true))
            }
            heldBack = []
        }

        /// The rule whose buttons are exactly what is down.
        private func exact(hold: Bool) -> Int? {
            rules.firstIndex { $0.hold == hold && $0.ids.count == down.count && subset(down, $0.ids) }
        }

        /// A modifier never completes a keyboard shortcut and is never held back.
        private func isModifier(_ b: UInt32) -> Bool {
            device == .keyboard && Shortcuts.modifiers.contains { keyVk($0) == b }
        }

        private func subset(_ small: [UInt32], _ big: [UInt32]) -> Bool {
            small.allSatisfy(big.contains)
        }
    }
}
