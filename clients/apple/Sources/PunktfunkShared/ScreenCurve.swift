/// How the Vision Pro bends the screen: flat, or a curved monitor's radius in millimetres.
/// `DefaultsKey.screen`, console key `visionos.screen`.
public enum ScreenCurve: String, CaseIterable, Sendable {
    case flat, r4000 = "4000r", r3000 = "3000r", r2300 = "2300r", r1800 = "1800r",
         r1500 = "1500r", r1000 = "1000r", r800 = "800r"

    /// Older stored names: `window` and `environment` read as flat, `curved` as 1800R. Unknown
    /// names read as flat, so a newer client's value degrades safely.
    public init(name: String?) {
        self = name == "curved" ? .r1800 : name.flatMap(ScreenCurve.init(rawValue:)) ?? .flat
    }

    /// Metres; nil is flat.
    public var radius: Float? {
        self == .flat ? nil : Float(rawValue.dropLast()).map { $0 / 1000 }
    }

    public var label: String {
        if self == .flat { return String(localized: L10n.playerScreenFlat) }
        return rawValue.dropLast().uppercased() + "R"
    }
}
