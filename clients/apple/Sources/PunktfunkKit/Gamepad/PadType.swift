// The quick action's controller types: the step order (pf-client-core's `PAD_TYPE_CYCLE`) and
// each type's mark. The marks are the console's pad outlines, generated into
// Resources/PadMarks.xcassets by scripts/gen-pad-marks.py; template rendering tints them like an
// SF Symbol.

import PunktfunkShared
import SwiftUI

extension PunktfunkConnection.GamepadType {
    /// What the Controller type slot steps through: Automatic, then the pads every host builds.
    public static let ringCycle: [Self] = [.auto, .xbox360, .xboxOne, .dualSense, .dualShock4, .steamDeck]

    /// The type after this one; a type outside the cycle (picked in Settings) steps to Automatic.
    public var nextInRing: Self {
        guard let i = Self.ringCycle.firstIndex(of: self) else { return .auto }
        return Self.ringCycle[(i + 1) % Self.ringCycle.count]
    }

    /// The Controller type slot's state word.
    public var ringLabel: String {
        switch self {
        case .auto: return String(localized: L10n.ringPadTypeAutomatic)
        case .xbox360: return "Xbox 360"
        case .xboxOne: return "Xbox One"
        case .dualSense: return "DualSense"
        case .dualShock4: return "DualShock 4"
        case .steamDeck: return "Steam Deck"
        default: return String(localized: L10n.ringPadTypeOther)
        }
    }

    /// Every type by name, the words pf-client-core's `pad_name` prints.
    public var padName: String {
        switch self {
        case .auto: return String(localized: L10n.ringPadTypeAutomatic)
        case .xbox360: return "Xbox 360"
        case .dualSense: return "DualSense"
        case .dualSenseEdge: return "DualSense Edge"
        case .dualShock4: return "DualShock 4"
        case .xboxOne: return "Xbox One"
        case .xboxElite: return "Xbox Elite Series 2"
        case .steamDeck: return "Steam Deck"
        case .steamController: return "Steam Controller"
        case .steamController2: return "Steam Controller 2"
        case .steamController2Puck: return "Steam Controller 2 Puck"
        case .switchPro: return "Switch Pro"
        case .eightBitDoUltimate2: return "8BitDo Ultimate 2"
        case .eightBitDoPro2: return "8BitDo Pro 2"
        case .eightBitDoPro3: return "8BitDo Pro 3"
        case .horipadSteam: return "HORIPAD for Steam"
        case .joyConPair: return String(localized: L10n.padKindJoyconPair)
        case .switch2Pro: return "Switch 2 Pro"
        case .switch2GameCube: return "GameCube (Switch 2)"
        }
    }

    /// This type's pad outline, or nil for Automatic and a type with no shipped mark.
    public var mark: Image? { markName.map { Image($0, bundle: .module) } }

    /// The imageset in PadMarks.xcassets.
    var markName: String? {
        switch self {
        case .xbox360: return "pad-xbox360"
        case .xboxOne: return "pad-xboxone"
        case .dualSense: return "pad-dualsense"
        case .dualShock4: return "pad-dualshock4"
        case .steamDeck: return "pad-steamdeck"
        default: return nil
        }
    }
}
