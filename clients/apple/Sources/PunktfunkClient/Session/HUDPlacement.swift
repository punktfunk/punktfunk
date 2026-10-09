// The HUD-corner model persisted by Settings and read wherever the overlay is placed
// (StreamScene, StreamHUDView).

import SwiftUI

/// Which corner the HUD overlay occupies (persisted as `DefaultsKey.hudPlacement`). The raw
/// values are stable on disk — rename the cases freely, never the strings.
enum HUDPlacement: String, CaseIterable, Identifiable {
    case topLeading, topTrailing, bottomLeading, bottomTrailing

    var id: String { rawValue }

    /// SwiftUI overlay alignment for `.overlay(alignment:)`.
    var alignment: Alignment {
        switch self {
        case .topLeading: return .topLeading
        case .topTrailing: return .topTrailing
        case .bottomLeading: return .bottomLeading
        case .bottomTrailing: return .bottomTrailing
        }
    }

    /// The HUD's own stack hugs the screen edge it sits against, so its text aligns outward.
    var isTrailing: Bool { self == .topTrailing || self == .bottomTrailing }

    /// The corner as a `UnitPoint`, so a scale transition grows/shrinks the HUD out of its own
    /// corner rather than its centre.
    var unitPoint: UnitPoint {
        switch self {
        case .topLeading: return .topLeading
        case .topTrailing: return .topTrailing
        case .bottomLeading: return .bottomLeading
        case .bottomTrailing: return .bottomTrailing
        }
    }

    /// User-facing corner label.
    var label: String {
        switch self {
        case .topLeading: return "Top Left"
        case .topTrailing: return "Top Right"
        case .bottomLeading: return "Bottom Left"
        case .bottomTrailing: return "Bottom Right"
        }
    }
}
