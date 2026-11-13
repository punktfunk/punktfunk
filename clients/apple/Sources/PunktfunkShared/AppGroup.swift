// The App-Group foundation shared by the app and its extensions (Widgets / Live Activity).
//
// PunktfunkShared is deliberately dependency-free: it never links PunktfunkKit, which would drag
// in the Rust staticlib and the presentation layer. A widget process gets ~30 MB, so everything an
// extension needs — the stored-host model and its JSON codec, the settings-key names, the deep-link
// grammar, the Live Activity attributes — lives here and here only. (A few files import SwiftUI for
// a Color or an AppIntent; none import PunktfunkKit.)

import Foundation

/// The one App-Group identifier, matched by `Config/*.entitlements` and
/// `PunktfunkWidgetsExtension.entitlements` (`com.apple.security.application-groups`). Registered on the developer portal for both the app
/// id (`io.unom.punktfunk`) and the widget extension id (`io.unom.punktfunk.widgets`).
public enum AppGroup {
    public static let suiteName = "group.io.unom.punktfunk"

    /// The shared defaults suite. A process without the group entitlement (unsigned `swift run`,
    /// a misprovisioned build) still gets a suite, private to it, so the widget does not see the
    /// app's store there. `UserDefaults(suiteName:)` is nil only for the app's own bundle id or
    /// the global domain, so `.standard` here is a type-level fallback.
    public static var defaults: UserDefaults {
        UserDefaults(suiteName: suiteName) ?? .standard
    }
}

/// Widget kind strings. The extension declares them and the app reloads by them, so a rename that
/// touches only one side leaves a widget the app never reloads: the library timeline is
/// `.never`, the hosts one refreshes hourly.
public enum WidgetKind {
    public static let hosts = "PunktfunkHosts"
    public static let library = "PunktfunkLibrary"
}
