// PyroWave quality as the player reads it: the rate it needs at the mode a connect would ask,
// and the warning when this device's link is short of it. Core prices both, so every client
// words them the same way.

import Foundation
import PunktfunkCore

public enum PyroWaveQuality {
    /// Core's `BPP_FLOOR` to `BPP_MAX`, by tenths.
    public static let range = 0.5...2.0
    public static let rungs = stride(from: 5, through: 20, by: 1).map { Double($0) / 10 }

    /// `bpp` on the nearest rung.
    public static func snapped(_ bpp: Double) -> Double {
        (min(max(bpp, range.lowerBound), range.upperBound) * 10).rounded() / 10
    }

    /// This device's link toward its default route: a `PUNKTFUNK_IFACE_KIND_*` and Mbit/s,
    /// `0` where the OS did not say. A route lookup: nothing is sent.
    public static func localLink() -> (kind: UInt8, mbps: UInt32) {
        var kind: UInt8 = 0
        var mbps: UInt32 = 0
        _ = punktfunk_local_link_facts(nil, &kind, &mbps)
        return (kind, mbps)
    }

    /// The kbps `s`'s quality needs at `mode`. 4:4:4 and 10 bits follow the switches that ask.
    public static func kbps(
        _ s: EffectiveSettings, mode: (width: UInt32, height: UInt32, hz: UInt32)
    ) -> UInt32 {
        punktfunk_pyrowave_kbps(
            mode.width, mode.height, mode.hz, s.enable444,
            s.hdrEnabled || s.tenBitSdr ? 10 : 8, s.pyrowaveBppX100)
    }

    /// The row's caption, `3840×2160 at 120 Hz: 1.6 Gbit/s`, and the warning when `link` is
    /// short of that rate.
    public static func lines(
        _ s: EffectiveSettings, mode: (width: UInt32, height: UInt32, hz: UInt32),
        link: (kind: UInt8, mbps: UInt32)
    ) -> (caption: String, warning: String?) {
        let need = kbps(s, mode: mode)
        var buf = [CChar](repeating: 0, count: 160)
        _ = punktfunk_pyrowave_link_warning(need, link.kind, link.mbps, &buf, UInt(buf.count))
        let warning = String(cString: buf)
        return ("\(mode.width)×\(mode.height) at \(mode.hz) Hz: \(rateLabel(kbps: need))",
                warning.isEmpty ? nil : warning)
    }

    /// `kbps` as core's `rate_label` writes it: `940 Mbit/s`, `1.3 Gbit/s`.
    public static func rateLabel(kbps: UInt32) -> String {
        let mbps = (UInt64(kbps) + 500) / 1000
        if mbps < 1000 { return "\(mbps) Mbit/s" }
        var gbps = String(format: "%.1f", Double(kbps) / 1e6)
        if gbps.hasSuffix(".0") { gbps.removeLast(2) }
        return "\(gbps) Gbit/s"
    }
}
