// Geist — the punktfunk brand typeface (the same family the website ships). Bundled as static
// OTF weights in this kit's resources and registered with Core Text at first use, so it works
// identically in the Xcode app and the `swift run` dev shell (Bundle.module resolves to the
// package resource bundle in both). Only Geist Sans ships, in four weights.
//
// Licensed under the SIL Open Font License 1.1 (Resources/Fonts/Geist-OFL.txt).

import CoreText
import SwiftUI
#if canImport(UIKit)
import UIKit
#elseif canImport(AppKit)
import AppKit
#endif

public enum BrandFont {
    public enum Weight {
        case regular, medium, semibold, bold
    }

    /// PostScript names of the bundled faces (verified from each OTF's name table). Geist Sans only
    /// — Geist Mono is intentionally not shipped; the app's typeface is Geist Sans throughout.
    private static let sansFaces = ["Geist-Regular", "Geist-Medium", "Geist-SemiBold", "Geist-Bold"]

    /// Registered exactly once per process — a static `let` initializer is run lazily and is
    /// guaranteed thread-safe + run-at-most-once by the runtime.
    private static let registered: Void = {
        for face in sansFaces {
            guard let url = Bundle.module.url(
                forResource: face, withExtension: "otf", subdirectory: "Fonts") else {
                #if DEBUG
                print("BrandFont: bundled face \(face).otf not found — text will fall back to system")
                #endif
                continue
            }
            var error: Unmanaged<CFError>?
            if !CTFontManagerRegisterFontsForURL(url as CFURL, .process, &error) {
                #if DEBUG
                let message = error?.takeRetainedValue().localizedDescription ?? "unknown error"
                print("BrandFont: \(face) not registered: \(message)")
                #endif
            }
        }
    }()

    /// Force registration before the first `Font.custom` lookup. Cheap to call repeatedly.
    public static func registerIfNeeded() { _ = registered }

    fileprivate static func sansFace(_ weight: Weight) -> String {
        switch weight {
        case .regular: return "Geist-Regular"
        case .medium: return "Geist-Medium"
        case .semibold: return "Geist-SemiBold"
        case .bold: return "Geist-Bold"
        }
    }
}

// Color.brand lives in PunktfunkShared/BrandColor.swift (re-exported here): the widget
// extension links Shared alone and must render the same purple.

public extension Font {
    /// Geist Sans at an explicit point size, scaling with Dynamic Type relative to `textStyle`.
    static func geist(
        _ size: CGFloat, _ weight: BrandFont.Weight = .regular,
        relativeTo textStyle: TextStyle = .body
    ) -> Font {
        BrandFont.registerIfNeeded()
        return .custom(BrandFont.sansFace(weight), size: size, relativeTo: textStyle)
    }

    /// Geist Sans at a FIXED point size that does not scale with Dynamic Type — for glyphs pinned
    /// inside a fixed-size container (e.g. the monogram tile), where a scaled letter would overflow.
    static func geistFixed(_ size: CGFloat, _ weight: BrandFont.Weight = .regular) -> Font {
        BrandFont.registerIfNeeded()
        return .custom(BrandFont.sansFace(weight), fixedSize: size)
    }
}
