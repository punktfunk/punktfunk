// What a settings category shows of its advanced rows: the rows in their own section, one button
// naming how many hold a changed value, or nothing. Pure; tested in `AdvancedDisclosureTests`.

import Foundation

public enum AdvancedDisclosure: Equatable, Sendable {
    /// The advanced rows, in their own section.
    case rows
    /// A button naming how many advanced rows hold a changed value. It turns Show advanced on.
    case countButton(Int)
    case hidden

    /// The rows while Show advanced is on or the edited preset overrides one of them. Otherwise
    /// Default settings counts its changed rows; a preset offers no count.
    public static func state(
        showAdvanced: Bool, overridden: Bool, changed: Int, inPreset: Bool
    ) -> AdvancedDisclosure {
        if showAdvanced || overridden { return .rows }
        if changed > 0, !inPreset { return .countButton(changed) }
        return .hidden
    }
}
