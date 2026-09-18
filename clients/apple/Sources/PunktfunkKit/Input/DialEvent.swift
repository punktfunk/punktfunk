import CoreGraphics

/// The two-finger twist's progress, for the quick-action ring (design/touch-client-overlay.md
/// §2.1): `turn` on every move once the twist has armed, then `commit` at the commit angle, or
/// `cancel` when the fingers lift short of it (or wind it back). Emitted by `TouchMouse`,
/// surfaced through `StreamView.onDial`.
public enum DialEvent: Equatable, Sendable {
    /// `progress` 0…1 drives the ring's unwind; `clockwise` is the hand's direction; `at`
    /// (stream-view points) the centroid the ring is centred on.
    case turn(progress: CGFloat, clockwise: Bool, at: CGPoint)
    case commit
    case cancel
    /// Straight to open at `at`, with no turn behind it — the edge pull, which is the only
    /// opener a passthrough session has (`EdgeDial`). It cannot preview the way a twist does:
    /// the fingers belong to the host until the pull completes.
    case open(at: CGPoint)
}
