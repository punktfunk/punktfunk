// What a pad or remote press does to the quick-action ring (design/touch-client-overlay.md
// §2.6): the highlight, the sheet's cursor and the armed slot it leaves, and what the overlay
// performs. Pure, so `swift test` checks it (`RingNavStepTests`); `RingOverlay` applies it.

extension RingNav {
    /// Where the pad stands: the ring's highlight (a slot 0…5, 6 the centre, nil until a pad
    /// moves it), whether the sheet is up and its cursor row, and the slot awaiting its second
    /// press.
    public struct State: Equatable, Sendable {
        public var highlight: Int?
        public var sheet: Bool
        public var cursor: Int
        public var armed: String?

        public init(highlight: Int? = nil, sheet: Bool = false, cursor: Int = 0, armed: String? = nil) {
            self.highlight = highlight
            self.sheet = sheet
            self.cursor = cursor
            self.armed = armed
        }
    }

    /// What a ring slot or a sheet row offers a press.
    public struct Row: Equatable, Sendable {
        /// The slot's id, the one `State.armed` names.
        public var id: String
        public var enabled: Bool
        /// Destructive: the first press arms it, the second fires.
        public var twoPress: Bool
        /// Left and Right step its value.
        public var adjustable: Bool

        public init(id: String = "", enabled: Bool = true, twoPress: Bool = false, adjustable: Bool = false) {
            self.id = id
            self.enabled = enabled
            self.twoPress = twoPress
            self.adjustable = adjustable
        }
    }

    public enum Haptic: Sendable {
        case press, refuse, warn
    }

    /// What the overlay does after a step, in order.
    public enum Effect: Equatable, Sendable {
        case haptic(Haptic)
        /// Say why ring slot `k` is dimmed.
        case explain(Int)
        /// Ask for ring slot `k`'s second press.
        case ask(Int)
        /// Run ring slot `k`.
        case fire(Int)
        /// Step the cursor row's value by ±1.
        case adjust(Int)
        /// Tap the cursor row.
        case tap
        case close
    }

    /// One press. `rows` are the ring's six slots (nil for an empty one) while the sheet is
    /// down, else the sheet's rows. On the ring the stick aims (its sector is the highlight,
    /// neutral the centre) and the D-pad steps: Right clockwise, Left anticlockwise, Up to 12
    /// o'clock, Down to 6. Y returns to the centre, A fires the highlight (the centre opens the
    /// sheet), B closes. In the sheet Up and Down walk the rows, Left and Right adjust one, A
    /// taps it and B leaves the sheet.
    public static func step(state: State, event: RingNav, rows: [Row?]) -> (state: State, effects: [Effect]) {
        var s = state
        if s.sheet {
            switch event {
            // A sheet is a list, not a dial: the six sectors fold onto its four directions.
            case .sector(let k):
                guard let k else { return (s, []) }
                return step(state: s, event: k == 0 ? .up : k == 3 ? .down : k < 3 ? .right : .left, rows: rows)
            case .up:
                s.cursor = max(s.cursor - 1, 0)
            case .down:
                s.cursor = min(s.cursor + 1, max(rows.count - 1, 0))
            case .left, .right:
                guard row(rows, s.cursor)?.adjustable == true else { return (s, [.haptic(.refuse)]) }
                return (s, [.adjust(event == .left ? -1 : 1), .haptic(.press)])
            case .confirm:
                guard let r = row(rows, s.cursor) else { return (s, []) }
                return (s, [.haptic(r.enabled ? .press : .refuse), .tap])
            case .back:
                s.sheet = false
            case .centre:
                return (s, [])
            }
            return (s, [.haptic(.press)])
        }
        let h = s.highlight ?? 6
        switch event {
        // The weapon-wheel idiom: the thumb's sector is the slot, neutral is the centre.
        case .sector(let k):
            let next = k ?? 6
            guard s.highlight != next else { return (s, []) }
            s.highlight = next
        case .right: s.highlight = h >= 6 ? 0 : (h + 1) % 6
        case .left: s.highlight = h >= 6 ? 5 : (h + 5) % 6
        case .up: s.highlight = 0
        case .down: s.highlight = 3
        case .centre: s.highlight = 6
        case .confirm:
            guard h < 6 else {
                s.cursor = 0
                s.sheet = true
                return (s, [.haptic(.press)])
            }
            guard row(rows, h) != nil else { return (s, [.haptic(.refuse)]) }
            return fire(state: s, slot: h, rows: rows)
        case .back:
            return (State(cursor: s.cursor), [.close])
        }
        return (s, [.haptic(.press)])
    }

    /// A press on ring slot `k`, from the pad or a finger: a dimmed slot refuses and says why, a
    /// destructive one arms on its first press and asks again, anything else runs.
    public static func fire(state: State, slot k: Int, rows: [Row?]) -> (state: State, effects: [Effect]) {
        var s = state
        guard let r = row(rows, k) else { return (s, []) }
        guard r.enabled else {
            s.armed = nil
            return (s, [.haptic(.refuse), .explain(k)])
        }
        if r.twoPress, s.armed != r.id {
            s.armed = r.id
            return (s, [.haptic(.warn), .ask(k)])
        }
        s.armed = nil
        return (s, [.haptic(.press), .fire(k)])
    }

    private static func row(_ rows: [Row?], _ i: Int) -> Row? {
        rows.indices.contains(i) ? rows[i] : nil
    }
}
