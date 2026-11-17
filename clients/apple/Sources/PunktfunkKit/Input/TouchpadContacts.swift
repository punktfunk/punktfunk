// Touchpad contacts: the pure half of the sources that put their fingers on the host's touchpad
// through `PunktfunkConnection.touchpadSurface`. A surface in tenths of a millimetre, a touch as a
// fraction of it, a platform timestamp in µs, what the touch mouse still sends while a
// touchscreen's fingers travel as contacts (frame mode), and what a captured Mac trackpad sends
// beside its contacts. Both decide again when core's touchpad changes
// (`.punktfunkTouchpadChanged`) and fall back to the ordinary pointer path when core refuses a
// call.
//
// The capture halves: StreamViewIOS.swift (touchscreen frames, the iPad trackpad's pinch) and
// StreamView.swift (the Mac trackpad). TouchpadContactsTests pins this file.

import CoreGraphics
import Foundation
import PunktfunkCore
#if os(macOS)
import AppKit
#endif

enum TouchpadContacts {
    /// UIKit gives no physical screen size, so a touchscreen is its idiom's density in points:
    /// 153 per inch on a phone (460 ppi at 3x), 132 on a pad (264 ppi at 2x).
    static func screenPointsPerInch(pad: Bool) -> CGFloat { pad ? 132 : 153 }

    /// The unit of `NSTouch.deviceSize`.
    static let trackpadPointsPerInch: CGFloat = 72

    /// A touch device shaped like a trackpad, from its `NSTouch.deviceSize`: landscape and at
    /// least 90 mm wide. Every Apple trackpad is; a Magic Mouse's portrait surface is not, and a
    /// zero size is no device. Only these touches become contacts.
    static func isTrackpad(_ deviceSize: CGSize) -> Bool {
        deviceSize.width > deviceSize.height
            && dmm(deviceSize.width, pointsPerInch: trackpadPointsPerInch) >= 900
    }

    /// `points` at `pointsPerInch` in tenths of a millimetre (254 per inch), clamped to the wire.
    static func dmm(_ points: CGFloat, pointsPerInch: CGFloat) -> UInt16 {
        let v = (points / pointsPerInch * 254).rounded()
        guard v.isFinite, v > 0 else { return 0 }
        return UInt16(min(v, CGFloat(UInt16.max)))
    }

    /// A platform timestamp (seconds since boot: `UITouch`, `NSEvent`, `systemUptime`) in µs, the
    /// one epoch every surface call of a session shares.
    static func micros(_ seconds: TimeInterval) -> UInt64 {
        seconds > 0 ? UInt64(seconds * 1_000_000) : 0
    }

    /// A point in a view of `size`, origin top-left, as a contact.
    static func touch(id: UInt64, at p: CGPoint, in size: CGSize) -> PunktfunkSurfaceTouch {
        PunktfunkSurfaceTouch(id: id, x: Float(p.x / size.width), y: Float(p.y / size.height))
    }

    /// An `NSTouch.normalizedPosition` (origin bottom-left) as a contact (origin top-left).
    static func touch(id: UInt64, normalized p: CGPoint) -> PunktfunkSurfaceTouch {
        PunktfunkSurfaceTouch(id: id, x: Float(p.x), y: Float(1 - p.y))
    }

    /// Whether the touch mouse's `event` still goes out while its fingers travel as contacts:
    /// never motion or scroll, which the host makes from the contacts, and buttons only when the
    /// client makes the taps.
    static func passes(_ event: PunktfunkInputEvent, hostTapsClick: Bool) -> Bool {
        switch UInt32(event.kind) {
        case PUNKTFUNK_INPUT_KIND_MOUSE_MOVE.rawValue, PUNKTFUNK_INPUT_KIND_SCROLL.rawValue:
            return false
        case PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_DOWN.rawValue,
             PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_UP.rawValue:
            return !hostTapsClick
        default:
            return true
        }
    }
}

/// Whether a touchscreen's fingers travel as the host touchpad's contacts (frame mode). The
/// host's touchpad is read at `refresh`: session start and each change of the scroll inversion,
/// which core answers with no touchpad. A refused call drops it until the next `refresh`, so a
/// finger is never muted while core takes nothing.
struct FrameMode {
    /// The host's touchpad as last read: it presents one, and its OS clicks on taps.
    private(set) var touchpad = (presents: false, taps: false)
    private(set) var on = false

    mutating func refresh(presents: Bool, taps: Bool) {
        touchpad = (presents, taps)
    }

    /// Follows the inputs. True: this edge sends one frame with no touches, the announce or the
    /// lift, which only a host still presenting its pad takes.
    mutating func sync(trusted: Bool, inWindow: Bool, mode: TouchInputMode) -> Bool {
        let want = touchpad.presents && trusted && inWindow && mode == .trackpad
        guard want != on else { return false }
        on = want
        return touchpad.presents
    }

    /// Core refused a touchpad call: the touch mouse sends everything until the next `refresh`.
    mutating func refused() {
        touchpad = (false, false)
        on = false
    }

    /// Whether the touch mouse's `event` goes out (`TouchpadContacts.passes` while on).
    func passes(_ event: PunktfunkInputEvent) -> Bool {
        !on || TouchpadContacts.passes(event, hostTapsClick: touchpad.taps)
    }
}

#if os(macOS)
/// What a captured Mac trackpad sends beside its contacts. While core forwards them, none of its
/// motion and none of its precise scroll; a scroll gesture the host took keeps its momentum too.
struct TrackpadContactGate {
    /// Core forwarded the last update and a finger is down: the trackpad's motion stays home.
    private(set) var forwarding = false
    /// The scroll gesture under way is the host's, through its momentum tail.
    private var hostScroll = false
    /// Core refused the last update: motion and scroll take their ordinary paths until capture
    /// decides again.
    private(set) var refused = false

    /// The trackpad's fingers become contacts only in the capture model toward a host whose
    /// touchpad is live (core reads none while scroll is inverted).
    static func engages(hostPresentsTouchpad: Bool, desktopMouse: Bool) -> Bool {
        hostPresentsTouchpad && !desktopMouse
    }

    /// Core's answer to an update with `fingers` down: forwarded or not, nil when refused.
    mutating func noteSurface(_ answer: Bool?, fingers: Int) {
        refused = answer == nil
        forwarding = answer == true && fingers > 0
        if refused { hostScroll = false }
    }

    /// Whether to hold this scroll event back. A counted wheel is a mouse's and always goes.
    mutating func dropsScroll(
        precise: Bool, phase: NSEvent.Phase, momentumPhase: NSEvent.Phase
    ) -> Bool {
        guard precise else { return false }
        if !momentumPhase.isEmpty {
            let drop = hostScroll
            if momentumPhase.contains(.ended) || momentumPhase.contains(.cancelled) {
                hostScroll = false
            }
            return drop
        }
        if phase.contains(.began) {
            hostScroll = forwarding
        } else if forwarding, !phase.isEmpty {
            hostScroll = true
        }
        return forwarding || (hostScroll && !phase.isEmpty)
    }
}
#endif
