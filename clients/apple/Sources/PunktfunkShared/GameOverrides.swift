// Game overrides: the stream settings a host sets for one game, the same for every device that
// plays it. The host resolves them onto each library entry; the connect that launches the game
// lays them over this device's settings, then `fit` clamps the mode to this device. Mirrors
// punktfunk-core's `game_overrides`; `clients/shared/game-override-vectors.json` pins all three.

import Foundation

/// One game's resolved overrides, as `GET /library` sends them. A nil field leaves the device's
/// own value; `name` is the shared override they came from.
public struct GameOverrides: Codable, Hashable, Sendable {
    public var name: String?
    /// Height in lines; the width keeps the device's aspect.
    public var lines: UInt32?
    public var hz: UInt32?
    public var hdr: Bool?
    /// A host `GamepadPref` name, as `GamepadType(name:)` reads it.
    public var gamepad: String?
    /// `lines` also reaches a smaller panel, decoded and scaled down.
    public var beyondScreen: Bool

    private enum CodingKeys: String, CodingKey {
        case name, lines, hz, hdr, gamepad
        case beyondScreen = "beyond_screen"
    }

    public init(
        name: String? = nil, lines: UInt32? = nil, hz: UInt32? = nil, hdr: Bool? = nil,
        gamepad: String? = nil, beyondScreen: Bool = false
    ) {
        self.name = name
        self.lines = lines
        self.hz = hz
        self.hdr = hdr
        self.gamepad = gamepad
        self.beyondScreen = beyondScreen
    }

    /// A mistyped field reads as unset: one bad value must not empty the library.
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        name = try? c.decodeIfPresent(String.self, forKey: .name)
        lines = try? c.decodeIfPresent(UInt32.self, forKey: .lines)
        hz = try? c.decodeIfPresent(UInt32.self, forKey: .hz)
        hdr = try? c.decodeIfPresent(Bool.self, forKey: .hdr)
        gamepad = try? c.decodeIfPresent(String.self, forKey: .gamepad)
        beyondScreen = (try? c.decodeIfPresent(Bool.self, forKey: .beyondScreen)) ?? false
    }

    /// The wire shape: unset fields and a false `beyond_screen` left out, as the host writes it.
    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encodeIfPresent(name, forKey: .name)
        try c.encodeIfPresent(lines, forKey: .lines)
        try c.encodeIfPresent(hz, forKey: .hz)
        try c.encodeIfPresent(hdr, forKey: .hdr)
        try c.encodeIfPresent(gamepad, forKey: .gamepad)
        if beyondScreen { try c.encode(true, forKey: .beyondScreen) }
    }

    /// No field set. A name alone sets nothing.
    public var isEmpty: Bool {
        lines == nil && hz == nil && hdr == nil && gamepad == nil && !beyondScreen
    }

    /// Nearest even number; the host refuses odd sizes.
    static func even(_ x: Double) -> UInt32 {
        UInt32((x / 2).rounded()) * 2
    }

    /// The mode to ask for once these apply to `mode`, the one this device would ask on its own.
    /// `lines` replaces the height and the width keeps `mode`'s aspect, capped at `native`'s
    /// lines unless `beyondScreen`; `hz` is capped at `native`'s. A zero in `native` caps
    /// nothing. Then both axes scale down to `codecMax` together.
    public func fit(
        _ mode: (width: UInt32, height: UInt32, hz: UInt32),
        native: (width: Int, height: Int, hz: Int), codecMax: Int
    ) -> (width: UInt32, height: UInt32, hz: UInt32) {
        var out = mode
        if let lines {
            let panel = UInt32(clamping: native.height)
            let l = beyondScreen || panel == 0 ? lines : min(lines, panel)
            if l != mode.height, mode.height > 0 {
                out.width = Self.even(Double(mode.width) * Double(l) / Double(mode.height))
                out.height = Self.even(Double(l))
            }
        }
        if let hz {
            let panel = UInt32(clamping: native.hz)
            out.hz = panel == 0 ? hz : min(hz, panel)
        }
        let cap = UInt32(clamping: codecMax)
        let longest = max(out.width, out.height)
        if longest > cap {
            let s = Double(cap) / Double(longest)
            out.width = min(Self.even(Double(out.width) * s), cap)
            out.height = min(Self.even(Double(out.height) * s), cap)
        }
        return out
    }
}
