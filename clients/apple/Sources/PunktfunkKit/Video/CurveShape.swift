// The volume screen's geometry, flat or curved. Platform-free, so a Mac's `swift test` checks it;
// the visionOS mesh and the overlay poses extend it in the app.

import Foundation
import simd

/// The picture flat, or on a cylinder segment, as wide as the box allows at its aspect: the chord
/// on the box's front face and the arc bowed back inside it. A box too shallow for the radius's
/// sag gets the strongest arc it holds instead.
public struct CurveShape: Equatable {
    /// Room left at the box's faces, metres.
    static let margin: Float = 0.01
    /// How far the ring floats over the picture, metres. The chord sits this much further back,
    /// or the volume clips the ring over a flat picture.
    public static let ringLift: Float = 0.02

    /// Metres; nil is flat.
    public let radius: Float?
    /// Half the arc's angle, radians; 0 when flat.
    public let halfAngle: Float
    public let halfChord: Float
    public let height: Float
    /// The chord's z inside the box: its front face, less the ring's lift.
    public let front: Float

    /// `box` in metres; `radius` is the stop's, nil for flat.
    public init(box: SIMD3<Float>, aspect: Float, radius: Float?) {
        let sag = max(box.z - 2 * Self.margin - Self.ringLift, 0.01)
        var chord = max(box.x - 2 * Self.margin, 0.1)
        var shape = Self.arc(chord: chord, sag: sag, radius: radius)
        // The box is the player's: the picture keeps its own shape inside it. Too tall at full
        // width, the chord narrows until the picture fits the box's height.
        let fit = (box.y - 2 * Self.margin) / (shape.length / aspect)
        if fit < 1 {
            chord *= fit
            shape = Self.arc(chord: chord, sag: sag, radius: radius)
        }
        self.radius = shape.radius
        halfAngle = shape.halfAngle
        halfChord = chord / 2
        height = shape.length / aspect
        front = box.z / 2 - Self.margin - Self.ringLift
    }

    /// The surface's z at `x` in the screen's own space: 0 at the edges, minus the sag at the
    /// centre.
    public func surfaceZ(atX x: Float) -> Float {
        guard let radius else { return 0 }
        return radius * cos(halfAngle) - sqrt(max(radius * radius - x * x, 0))
    }

    /// Picture coordinates, 0…1 from the top left, of a point on the screen in its own space.
    public func uv(at point: SIMD3<Float>) -> SIMD2<Float> {
        var u = (point.x / halfChord + 1) / 2
        if let radius {
            let angle = atan2(point.x, radius * cos(halfAngle) - point.z)
            u = (angle / halfAngle + 1) / 2
        }
        let v = 0.5 - point.y / height
        return [min(max(u, 0), 1), min(max(v, 0), 1)]
    }

    /// The box depth, metres, that holds `radius` across a box `width` metres wide: the arc's sag,
    /// the faces' room and the ring's lift. Flat needs only the room and the lift.
    public static func depth(width: Float, radius: Float?) -> Float {
        let room = 2 * margin + ringLift
        guard let radius else { return room }
        let half = min(max(width / 2 - margin, 0), radius)
        return radius - sqrt(radius * radius - half * half) + room
    }

    private static func arc(
        chord: Float, sag: Float, radius: Float?
    ) -> (radius: Float?, halfAngle: Float, length: Float) {
        guard let radius else { return (nil, 0, chord) }
        // The radius whose sagitta over this chord just fits the box, if `radius` does not.
        let fitted = max(radius, (chord * chord / 4 + sag * sag) / (2 * sag))
        let halfAngle = asin(min(chord / (2 * fitted), 1))
        return (fitted, halfAngle, 2 * halfAngle * fitted)
    }
}
