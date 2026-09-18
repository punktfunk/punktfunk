// The quick-action dial's opener for a passthrough session: two fingers started at one side
// bezel and pulled inward together (design/touch-client-overlay.md §2.1).
//
// Passthrough gives every finger to the host verbatim, which is why the twist is not offered
// there — a gesture read off the stream is a touch the game never gets. So this one takes
// nothing on the way: the contacts forward as normal, and only a COMPLETED pull lifts them and
// opens the dial. A pull that stops short was a real touch all along, and the host kept it.
//
// Side bezels only. The top and bottom of an iOS screen are the system's own (home indicator,
// Control Centre), and a gesture that races SpringBoard loses.

import Foundation

/// Which bezel a pull started from. The two fingers must agree.
public enum DialEdge: Equatable, Sendable {
    case left
    case right
}

/// One finger's contribution to a pull: where it landed and where it is now.
public struct EdgeTrack: Equatable, Sendable {
    public let edge: DialEdge
    public let start: CGPoint
    public var now: CGPoint

    public init(edge: DialEdge, start: CGPoint, now: CGPoint) {
        self.edge = edge
        self.start = start
        self.now = now
    }

    /// Inward travel, always positive whichever bezel it came from.
    var pulled: CGFloat {
        switch edge {
        case .left: return now.x - start.x
        case .right: return start.x - now.x
        }
    }
}

public enum EdgeDial {
    /// How close to a bezel a finger has to land to be part of a pull. A thumb reaching over
    /// the edge is wider than a stylus, and a game's own controls rarely live in the last
    /// quarter-inch.
    public static let strip: CGFloat = 24
    /// How far in both fingers have to come. Long enough that a two-finger tap at the bezel or
    /// a short drag inside a game cannot reach it.
    public static let travel: CGFloat = 64
    /// How far apart the two may be. One hand is one gesture; two players' thumbs at opposite
    /// ends of an iPad are not.
    public static let spread: CGFloat = 220

    /// Is `p` within [`strip`] of a side bezel of a `width`-wide view?
    public static func edge(of p: CGPoint, width: CGFloat) -> DialEdge? {
        if p.x <= strip { return .left }
        if p.x >= width - strip { return .right }
        return nil
    }

    /// Does this pair complete the pull?
    ///
    /// Both from the same bezel, both far enough in, and near enough to each other to be one
    /// hand. Vertical distance counts toward the spread: two fingers at opposite corners of the
    /// same edge are two touches that happen to travel together, not a pull.
    public static func completes(_ a: EdgeTrack, _ b: EdgeTrack) -> Bool {
        guard a.edge == b.edge else { return false }
        guard a.pulled >= travel, b.pulled >= travel else { return false }
        return hypot(a.now.x - b.now.x, a.now.y - b.now.y) <= spread
    }

    /// Where the dial should open for a completed pull: between the fingers, so it arrives
    /// under the hand that asked for it.
    public static func centre(_ a: EdgeTrack, _ b: EdgeTrack) -> CGPoint {
        CGPoint(x: (a.now.x + b.now.x) / 2, y: (a.now.y + b.now.y) / 2)
    }
}
