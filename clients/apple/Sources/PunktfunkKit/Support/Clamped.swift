import CoreGraphics

extension Comparable {
    /// Clamp into `range`. One definition for the whole module: the absolute-cursor mapping, the
    /// pen's pressure and tilt, the gamepad wire's axis ranges and the scroll quantizer share it.
    func clamped(to range: ClosedRange<Self>) -> Self {
        Swift.min(Swift.max(self, range.lowerBound), range.upperBound)
    }
}
