// The settings resolution controls' rules: the touch and desktop wheel works in "WxH" tags with a
// "Custom…" row that reveals typed fields, and the TV's stream-mode row in "WxHxHz" tags that set
// size and rate together. Pure; tested in `ResolutionPickTests`.

import Foundation

public enum ResolutionPick {
    /// The wheel's "Custom…" row. Real tags are digits and "x", so this can't collide with one.
    public static let customTag = "custom"

    /// A wheel row's tag for a size. `0x0` is Native.
    public static func tag(width: Int, height: Int) -> String { "\(width)x\(height)" }

    /// The typed fields show: the wheel is parked on Custom… (`customMode`, sticky so a typed size
    /// that briefly equals a row doesn't snap off it), or the size is none of the wheel's `tags`.
    public static func isCustom(customMode: Bool, width: Int, height: Int, tags: Set<String>) -> Bool {
        customMode || !tags.contains(tag(width: width, height: height))
    }

    /// A wheel pick: the custom flag after it, and the size to write. Custom… writes 1080p over
    /// Native, so the fields open on numbers, and keeps any other size.
    public static func apply(
        tag: String, currentWidth: Int
    ) -> (customMode: Bool, size: (w: Int, h: Int)?) {
        if tag == customTag {
            return (true, currentWidth == 0 ? (1920, 1080) : nil)
        }
        let parts = tag.split(separator: "x").compactMap { Int($0) }
        guard parts.count == 2 else { return (false, nil) }
        return (false, (parts[0], parts[1]))
    }

    /// The TV's stream-mode tag. `0x0x0` is Native.
    public static func modeTag(width: Int, height: Int, hz: Int) -> String {
        "\(width)x\(height)x\(hz)"
    }

    /// A stream-mode tag's size and rate, nil for anything else.
    public static func mode(tag: String) -> (w: Int, h: Int, hz: Int)? {
        let parts = tag.split(separator: "x").compactMap { Int($0) }
        guard parts.count == 3 else { return nil }
        return (parts[0], parts[1], parts[2])
    }

    /// The overrides the stream-mode row's marker shows and its Reset clears: the size and the
    /// rate it writes together. A TV has no other refresh row to clear the rate from.
    public static let modeFields = [OverlayField.resolution, "refresh_hz"]
}
