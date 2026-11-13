// The manual bitrate slider's scale: a position from 0 to 1 against kbps. Pure; tested in
// `BitrateScaleTests`.

import Foundation

public enum BitrateScale {
    /// Log scale: the useful range spans three orders of magnitude, a few Mbit/s to 3 Gbit/s, and
    /// a linear one would cram everything below 100 Mbit/s into the first pixels.
    public static let minKbps = 2_000.0
    public static let maxKbps = 3_000_000.0

    /// The slider position of a rate, held inside the scale.
    public static func position(kbps: Int) -> Double {
        let v = min(max(Double(kbps), minKbps), maxKbps)
        return log(v / minKbps) / log(maxKbps / minKbps)
    }

    /// The rate at a slider position, snapped to two significant figures so the readout shows
    /// round numbers instead of 47 322.
    public static func kbps(position: Double) -> Int {
        let raw = minKbps * pow(maxKbps / minKbps, position)
        let mag = pow(10, floor(log10(raw)) - 1)
        return Int((raw / mag).rounded() * mag)
    }
}
