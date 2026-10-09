// Button glyphs for a legend, for a controller that ISN'T currently attached.
//
// While a pad is connected the truth is GameController's own `sfSymbolsName` on the live element —
// nothing here competes with that. The problem this file solves is the other half of the time: the
// instant `GamepadManager.active` goes nil (the pad slept, its battery died, it was unplugged, or
// `gamepadUIMode == "always"` put the console UI up with no pad at all) there is no element left to
// ask, and every legend fell back to the generic letter glyphs — which read as an Xbox pad. A
// DualSense user watched their ✕/◯ legends turn into A/B the moment the controller dozed off.
//
// So: `GamepadManager` remembers the KIND of the last controller that was actually attached
// (`DefaultsKey.lastGamepadKind`, never cleared on disconnect) and the legends resolve through this
// table instead. Deliberately NOT a user-facing setting — a "glyph style" picker is one more row in
// a settings screen to answer a question the app can answer itself, and the remembered pad is right
// essentially always: people own the controller they last plugged in.
//
// Positional, not nominal. `GCExtendedGamepad`'s buttonA/B/X/Y are POSITIONS (A = bottom, B =
// right, X = left, Y = top), so each family maps its own labels onto those positions — which is why
// the Nintendo column looks transposed: a Switch pad's bottom button is B and its right one is A.

import Foundation
import GameController

/// A face/shoulder button by POSITION, which is what `GCExtendedGamepad` exposes and what a legend
/// actually means ("press the bottom button"). The label drawn for it is the family's business.
public enum GamepadButtonRole: Sendable {
    /// Bottom face button — Xbox A, PlayStation ✕, Nintendo B.
    case a
    /// Right face button — Xbox B, PlayStation ◯, Nintendo A.
    case b
    /// Left face button — Xbox X, PlayStation □, Nintendo Y.
    case x
    /// Top face button — Xbox Y, PlayStation △, Nintendo X.
    case y
    case leftShoulder
    case rightShoulder

    /// The role a `GCExtendedGamepad` key path names, so a caller that already spells its buttons
    /// as key paths (every legend does — it reads `sfSymbolsName` off the live
    /// element through one) can reach this table without restating itself. nil for any other
    /// button: the legends only ever name these six, and a role invented for, say, the menu button
    /// would have no honest glyph on half the families.
    ///
    /// Compared with `==` rather than matched with `switch`: key paths are reference-typed and
    /// their pattern-matching goes through the generic `Equatable` `~=`, which is easy to send to
    /// an unintended overload. This spelling has exactly one meaning.
    public init?(keyPath: KeyPath<GCExtendedGamepad, GCControllerButtonInput>) {
        if keyPath == \GCExtendedGamepad.buttonA { self = .a }
        else if keyPath == \GCExtendedGamepad.buttonB { self = .b }
        else if keyPath == \GCExtendedGamepad.buttonX { self = .x }
        else if keyPath == \GCExtendedGamepad.buttonY { self = .y }
        else if keyPath == \GCExtendedGamepad.leftShoulder { self = .leftShoulder }
        else if keyPath == \GCExtendedGamepad.rightShoulder { self = .rightShoulder }
        else { return nil }
    }
}

public enum GamepadGlyphs {
    /// The SF Symbol a `role` wears on a `kind` of pad. Every name here is asserted to resolve on
    /// the running OS by `GamepadGlyphTests` — a symbol name that doesn't exist renders as NOTHING
    /// (SwiftUI draws an empty image rather than failing), so a typo would silently blank a legend
    /// on real hardware and never show up in a build.
    public static func symbol(_ role: GamepadButtonRole, for kind: PunktfunkConnection.GamepadType)
        -> String {
        switch role {
        case .leftShoulder: return "l1.rectangle.roundedbottom"
        case .rightShoulder: return "r1.rectangle.roundedbottom"
        case .a, .b, .x, .y: return faceSymbol(role, for: kind)
        }
    }

    /// The Siri Remote's button for `role`, or nil where the remote has none. Positions map to what
    /// tvOS routes each press to: the clickpad selects (A), Back backs out (B), Play/Pause is the
    /// pad's X. Y and the shoulders have no button on the remote.
    public static func remoteSymbol(_ role: GamepadButtonRole) -> String? {
        switch role {
        case .a: return "smallcircle.filled.circle"
        case .b: return "chevron.backward.circle"
        case .x: return "playpause.circle"
        case .y, .leftShoulder, .rightShoulder: return nil
        }
    }

    private static func faceSymbol(
        _ role: GamepadButtonRole, for kind: PunktfunkConnection.GamepadType
    ) -> String {
        switch kind {
        // PlayStation shapes. ✕ is the BOTTOM button, so it belongs to role `.a` — the mapping
        // people mean when they say "the PlayStation glyphs".
        case .dualSense, .dualSenseEdge, .dualShock4:
            switch role {
            case .a: return "xmark.circle"
            case .b: return "circle.circle"
            case .x: return "square.circle"
            case .y: return "triangle.circle"
            default: return "circle.circle"
            }
        // Nintendo's labels sit transposed on the same positions (bottom = B, right = A,
        // left = Y, top = X) — printing Xbox letters on a Switch pad would name the wrong
        // physical button, which is worse than a generic glyph. The families match the
        // console's `GlyphStyle::from_pref`.
        case .switchPro, .eightBitDoPro2, .eightBitDoPro3, .joyConPair, .switch2Pro,
             .switch2GameCube:
            switch role {
            case .a: return "b.circle"
            case .b: return "a.circle"
            case .x: return "y.circle"
            case .y: return "x.circle"
            default: return "a.circle"
            }
        // Xbox, the Steam pads (Deck included — its ABXY is the Xbox layout), and `.auto`, which
        // is what a client with no remembered pad has. Xbox letters double as the neutral default
        // because they ARE the positional names in `GCExtendedGamepad`.
        case .auto, .xbox360, .xboxOne, .xboxElite, .steamController, .steamDeck,
             .steamController2, .steamController2Puck, .eightBitDoUltimate2, .horipadSteam:
            switch role {
            case .a: return "a.circle"
            case .b: return "b.circle"
            case .x: return "x.circle"
            case .y: return "y.circle"
            default: return "a.circle"
            }
        }
    }
}
