// Stream-mode presets grouped by aspect ratio — the twin of `punktfunk_core::resolutions` (and
// Android's `Resolutions` in Settings.kt). A picker shows one family at a time behind an aspect
// switch, plus its own native rows. Picking a family moves to its size nearest the current height
// (`nearest`), so the switch and the list always agree without picker-side state. Pure; tested in
// `ResolutionsTests`.

import Foundation

public enum Resolutions {
    /// One family of sizes with the same shape.
    public struct Aspect {
        public init(id: String? = nil, label: String, shape: Double, sizes: [(w: Int, h: Int)]) {
            self.id = id ?? label
            self.label = label
            self.shape = shape
            self.sizes = sizes
        }

        /// Identity: the ratio for a standard family, `screenID` or `safeAreaID` for a device
        /// entry. The switch compares this, never the label.
        public let id: String
        /// The switch label: "16:9".
        public let label: String
        /// Width over height of the shape.
        public let shape: Double
        /// Common panels of this shape, ascending. All sides even (the host rejects odd modes).
        public let sizes: [(w: Int, h: Int)]
    }

    /// Families in the order the switch shows them: most common first.
    public static let aspects: [Aspect] = [
        Aspect(
            label: "16:9", shape: 16 / 9,
            sizes: [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160), (5120, 2880)]),
        Aspect(
            label: "16:10", shape: 16 / 10,
            sizes: [(1280, 800), (1920, 1200), (2560, 1600), (2880, 1800), (3840, 2400)]),
        Aspect(
            label: "21:9", shape: 21 / 9,
            sizes: [(2560, 1080), (3440, 1440), (3840, 1600), (5120, 2160)]),
        Aspect(label: "32:9", shape: 32 / 9, sizes: [(3840, 1080), (5120, 1440), (7680, 2160)]),
        Aspect(
            label: "3:2", shape: 3 / 2,
            sizes: [(2160, 1440), (2256, 1504), (2880, 1920), (3000, 2000)]),
        Aspect(label: "4:3", shape: 4 / 3, sizes: [(1024, 768), (1600, 1200), (2048, 1536)]),
    ]

    /// Shape tolerance for `aspectOf`. "21:9" panels are really 2.37–2.40, so 4 % keeps them in
    /// one family and still parts 16:10 (1.60) from 3:2 (1.50).
    static let tolerance = 0.04

    /// The family `w`×`h` belongs to by shape, not by membership: a custom 1500×1000 is 3:2.
    /// `nil` for a zero side (native) or a shape no family has.
    public static func aspectOf(_ w: Int, _ h: Int) -> Int? {
        guard w > 0, h > 0 else { return nil }
        let shape = Double(w) / Double(h)
        return aspects.firstIndex { abs(shape / $0.shape - 1) < tolerance }
    }

    /// Smallest stream mode the host accepts, per side.
    public static let minWidth = 320
    public static let minHeight = 200

    /// A typed `w`×`h` as a mode the host takes: each side at least `minWidth`×`minHeight`, at most
    /// the codec's per-side ceiling, then floored even. Twin of `punktfunk_core::resolutions::custom`.
    public static func custom(_ w: Int, _ h: Int, codec: String) -> (w: Int, h: Int) {
        let cap = RenderScale.maxDimension(codec: codec)
        return (min(max(w, minWidth), cap) / 2 * 2, min(max(h, minHeight), cap) / 2 * 2)
    }

    /// The size in family `aspect` nearest in height to `h`; a native `0` looks for 1080. Ties go
    /// to the smaller size.
    public static func nearest(_ aspect: Int, height h: Int) -> (w: Int, h: Int) {
        nearestIn(aspects[aspect], height: h)
    }

    /// The size in `family` nearest in height to `h`; a native `0` looks for 1080.
    public static func nearestIn(_ family: Aspect, height h: Int) -> (w: Int, h: Int) {
        let h = h == 0 ? 1080 : h
        return family.sizes.min { abs($0.h - h) < abs($1.h - h) }!
    }

    public static let screenID = "screen"
    public static let safeAreaID = "safe-area"

    /// Heights a device entry offers below the screen's own.
    static let deviceHeights = [720, 1080, 1440, 2160]

    /// A device entry's sizes sit within this of its shape, tight enough to part a phone's screen
    /// from its safe area.
    static let deviceTolerance = 0.01

    /// The aspect switch on this device: "Screen", then "Safe area", then `aspects`. Each device
    /// entry appears only when no standard family has its shape, and the safe area only when it
    /// differs from the screen. Twin of `punktfunk_core::resolutions::families`.
    public static func families(screen: (w: Int, h: Int)?, safe: (w: Int, h: Int)?) -> [Aspect] {
        var own: [Aspect] = []
        let entries = [
            (screenID, String(localized: L10n.playerResolutionAspectScreen), screen),
            (safeAreaID, String(localized: L10n.playerResolutionAspectSafeArea), safe),
        ]
        for (id, label, dims) in entries {
            guard let dims, dims.w > 0, dims.h > 0 else { continue }
            let (w, h) = (dims.w, dims.h)
            if id == safeAreaID, let screen, screen.w == w, screen.h == h { continue }
            if aspectOf(w, h) != nil { continue }
            let sizes = deviceHeights.filter { $0 < h }.map { (w: w * $0 / h / 2 * 2, h: $0) }
                + [(w: w / 2 * 2, h: h / 2 * 2)]
            own.append(Aspect(id: id, label: label, shape: Double(w) / Double(h), sizes: sizes))
        }
        return own + aspects
    }

    /// The entry of `families` `w`×`h` belongs to by shape: a device entry first, then a standard
    /// one. `nil` for a zero side or a shape none has.
    public static func familyOf(_ families: [Aspect], _ w: Int, _ h: Int) -> Int? {
        guard w > 0, h > 0 else { return nil }
        let shape = Double(w) / Double(h)
        func within(_ a: Aspect, _ tol: Double) -> Bool { abs(shape / a.shape - 1) < tol }
        func device(_ a: Aspect) -> Bool { a.id == screenID || a.id == safeAreaID }
        return families.firstIndex { device($0) && within($0, deviceTolerance) }
            ?? families.firstIndex { !device($0) && within($0, tolerance) }
    }
}
