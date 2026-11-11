// The quick-action sheet's Resolution and Refresh rows: the mode a press steps to, over Native
// and the presets. Pure, so `swift test` checks it (`RingNavModeTests`).

extension RingNav {
    /// A stream mode: pixels and refresh rate.
    public struct Mode: Equatable, Sendable {
        public var width: UInt32
        public var height: UInt32
        public var hz: UInt32

        public init(width: UInt32, height: UInt32, hz: UInt32) {
            self.width = width
            self.height = height
            self.hz = hz
        }
    }

    /// The Resolution row's stops after Native, by name.
    public static let resolutions: [(name: String, width: UInt32, height: UInt32)] = [
        ("1440p", 2560, 1440), ("1080p", 1920, 1080), ("720p", 1280, 720),
    ]
    /// The Refresh row's stops after Native.
    public static let refreshRates: [UInt32] = [120, 60]

    /// The Resolution row's next stop `dir` (±1) steps to from `mode`, round Native and the
    /// presets; the refresh rate stays. A 0×0 Native, a session read after its handle went, is
    /// no stop.
    public static func nextMode(mode: Mode, native: Mode, dir: Int) -> Mode {
        let first = native.width > 0 && native.height > 0 ? [(native.width, native.height)] : []
        let stops = first + resolutions.map { ($0.width, $0.height) }
        let i = stops.firstIndex { $0 == (mode.width, mode.height) } ?? 0
        let next = stops[wrap(i + dir, stops.count)]
        return Mode(width: next.0, height: next.1, hz: mode.hz)
    }

    /// The Refresh row's next stop `dir` (±1) steps to from `mode`, round Native's rate and the
    /// presets; the size stays. A zero Native rate is no stop.
    public static func nextRefresh(mode: Mode, native: Mode, dir: Int) -> Mode {
        var stops = native.hz > 0 ? [native.hz] : []
        for hz in refreshRates where !stops.contains(hz) { stops.append(hz) }
        let i = stops.firstIndex(of: mode.hz) ?? 0
        return Mode(width: mode.width, height: mode.height, hz: stops[wrap(i + dir, stops.count)])
    }

    private static func wrap(_ i: Int, _ n: Int) -> Int { (i % n + n) % n }
}
