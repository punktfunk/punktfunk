// Render-resolution scaling — the pure geometry behind `DefaultsKey.renderScale`. The client asks
// the host to render at `chosen resolution × scale`. Above 1x it also names its `view`, the chosen
// resolution, and a host that reads it scales the picture down before encoding; any other host
// sends the mode and the presenter downscales. Below 1x the presenter upscales.
//
// Multiply, keep the aspect ratio, floor to even (the host's `validate_dimensions` rejects odd
// sizes), and clamp to the codec's per-axis ceiling. Pinned by `RenderScaleTests` against
// `clients/shared/render-scale-vectors.json`.

import Foundation

public enum RenderScale {
    /// The supported multiplier range. Below 1 renders under native (upscaled on present); above 1
    /// supersamples. The UI clamps its slider to this and the connect clamps the raw stored value.
    public static let range: ClosedRange<Double> = 0.5...4.0

    /// The multipliers the picker offers. 1.0 (Native) is the default; the rest are the round stops
    /// users reason about.
    public static let presets: [Double] = [0.5, 0.67, 0.75, 1.0, 1.25, 1.5, 2.0, 3.0, 4.0]

    /// The encoder/host per-axis ceiling for a codec preference string (`DefaultsKey.codec`). H.264
    /// tops out at 4096 px/axis; HEVC / AV1 / PyroWave (and "auto", which negotiates one of those in
    /// practice) at 8192. The host enforces the same walls in `codec.rs::validate_dimensions`.
    public static func maxDimension(codec: String) -> Int {
        codec == "h264" ? 4096 : 8192
    }

    /// A compact user-facing label for a multiplier: "Native", "1.5×", "2× (supersample)".
    /// Shared by every platform's picker so the wording stays identical.
    public static func label(_ scale: Double) -> String {
        if scale == 1.0 { return String(localized: L10n.playerRenderScaleNative) }
        if scale > 1 { return L10n.playerRenderScaleSupersample(scale: String(format: "%g", scale)) }
        return String(format: "%g×", scale)
    }

    /// Clamp a raw stored multiplier into `range`, treating a missing/zero value as 1.0 (Native).
    public static func sanitize(_ raw: Double) -> Double {
        guard raw > 0 else { return 1.0 }
        return min(max(raw, range.lowerBound), range.upperBound)
    }

    /// A typed percent as a multiplier, sanitized. A percent, because whole percents keep every
    /// client storing the same hundredths. Mirrors core's `from_percent`.
    public static func fromPercent(_ percent: Int) -> Double {
        sanitize(Double(percent) / 100)
    }

    /// The whole percent a stored multiplier reads as.
    public static func percent(_ scale: Double) -> Int {
        Int((sanitize(scale) * 100).rounded())
    }

    /// Which of `presets` a stored multiplier is; nil for a typed one. Compared in hundredths: a
    /// client that stores a float reads 0.67 back as 0.6700000167.
    public static func presetIndex(_ scale: Double) -> Int? {
        presets.firstIndex { abs($0 - scale) < 0.005 }
    }

    /// Apply `scale` to a base pixel size, preserving aspect, even-flooring each axis, and clamping
    /// uniformly so neither axis exceeds `maxDimension` (the larger axis lands on the cap, the ratio
    /// is kept). Also floors each axis at `minWidth`/`minHeight` (the host never accepts < 320×200).
    /// The result is a directly host-valid `Mode` width/height.
    public static func apply(
        baseWidth: Int,
        baseHeight: Int,
        scale rawScale: Double,
        maxDimension: Int,
        minWidth: Int = 320,
        minHeight: Int = 200
    ) -> (width: UInt32, height: UInt32) {
        let scale = sanitize(rawScale)
        var w = Double(max(baseWidth, 1)) * scale
        var h = Double(max(baseHeight, 1)) * scale
        // Uniform down-clamp if either axis blew past the ceiling — keep the aspect ratio intact.
        let cap = Double(maxDimension)
        let over = max(w / cap, h / cap)
        if over > 1 {
            w /= over
            h /= over
        }
        let evenFloor: (Double, Int) -> UInt32 = { value, minimum in
            let clamped = max(Int(value.rounded(.down)), minimum)
            return UInt32(clamped / 2 * 2)
        }
        return (evenFloor(w, minWidth), evenFloor(h, minHeight))
    }

    /// The size the picture is shown at, for the host to encode instead of the mode `apply` asks
    /// for: the base at 1x. nil at or below 1x, or when the codec cap leaves the mode no larger
    /// than the view. Mirrors core's `render_scale::view`.
    public static func view(
        baseWidth: Int, baseHeight: Int, scale: Double, maxDimension: Int
    ) -> (width: UInt32, height: UInt32)? {
        let view = apply(
            baseWidth: baseWidth, baseHeight: baseHeight, scale: 1, maxDimension: maxDimension)
        let mode = apply(
            baseWidth: baseWidth, baseHeight: baseHeight, scale: scale, maxDimension: maxDimension)
        return sanitize(scale) > 1 && mode != view ? view : nil
    }
}
