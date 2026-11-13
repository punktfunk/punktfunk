// The opt-in phone-gyro mirror (`DefaultsKey.gyroFromDevice`): when player 1's forwarded
// controller has no rotation sensor of its own, THIS device's IMU speaks for it on the wire's
// motion plane — for clip-on and third-party pads that ship without a gyro, where the phone
// body is rigidly attached to (or simply is) the thing in the player's hands. The sibling of
// `GamepadFeedback`'s rumble-on-device mirror, with the data flowing the other way.
//
// GamepadCapture owns the engage/stand-down decision (it knows the pad-0 slot and whether its
// controller reports a rotation rate); this class only turns CoreMotion on and off and converts
// samples. Two invariants it enforces itself:
//  - one motion writer per pad: samples go out only between `start` and `stop`, and capture
//    suppresses pad 0's controller-motion forwarding while this runs;
//  - no stale rotation: `stop` sends a single zero-gyro sample after the last real one, so the
//    host's virtual pad never keeps integrating an angular velocity this device stopped
//    producing (the gyro-sweep "stale angular velocity re-sent forever" failure mode).
//
// Samples are CMDeviceMotion (bias-corrected rotation, gravity split from user acceleration)
// at CoreMotion's ~100 Hz ceiling; the host's motion plane is event-driven, so fewer is fine.
// Units match `GamepadCapture.forwardMotion` (the `GamepadWire` scales, accel negated); the
// frame does not. CoreMotion's portrait frame, turned by the interface orientation, is (right,
// up, out of the screen): the wire's frame already, so `appleMotionToWire` is not applied.

import Foundation

/// Device-frame → controller-frame axis remap for one interface orientation. CoreMotion's
/// frame is fixed to the portrait device (+x right edge, +y top, +z out of the screen); the
/// controller frame keeps +z (the screen always faces the player) and rotates x/y so they
/// mean "player's right" and "player's up". Derived, like the wire scale constants — pinned
/// by `DeviceGyroVectorTests` (the table Android's `DeviceGyro.remap` runs too), correctable
/// in one place if on-glass says otherwise.
/// File-scope rather than nested so the sample thread can use it without actor isolation, and
/// outside the iOS gate so its table runs under `swift test`.
enum DeviceGyroRemap {
    case identity
    /// Upside-down portrait: both in-plane axes flip.
    case flipped
    /// Landscape, device top to the player's LEFT (interface `.landscapeRight`):
    /// player-right = device-bottom, player-up = device-right.
    case topLeft
    /// Landscape, device top to the player's RIGHT (interface `.landscapeLeft`).
    case topRight

    /// Rotate one device-frame vector (rotation rate or acceleration — both transform the
    /// same way under an in-plane rotation) into the controller frame.
    func apply(x: Float, y: Float, z: Float) -> (x: Float, y: Float, z: Float) {
        switch self {
        case .identity: return (x, y, z)
        case .flipped: return (-x, -y, z)
        case .topLeft: return (-y, x, z)
        case .topRight: return (y, -x, z)
        }
    }
}

#if os(iOS)
import CoreMotion
import UIKit

extension DeviceGyroRemap {
    init(_ orientation: UIInterfaceOrientation) {
        switch orientation {
        case .portraitUpsideDown: self = .flipped
        case .landscapeRight: self = .topLeft
        case .landscapeLeft: self = .topRight
        default: self = .identity
        }
    }
}

@MainActor
public final class DeviceGyro {
    /// Whether this device can source motion at all — gates the settings rows (a device
    /// without an IMU would make the toggle a silent no-op, the rumble mirror's rule).
    /// One shared probe: Apple recommends a single `CMMotionManager` per app, and the
    /// settings UI asking per-render must not allocate one each time.
    public static let isAvailable: Bool = CMMotionManager().isDeviceMotionAvailable

    /// Everything the sample thread touches, behind one lock: the orientation remap (written
    /// on main when the device rotates), the last converted accel, and whether a real sample
    /// went out (so `stop` knows it owes the wire a zero). Kept off the actor deliberately —
    /// `forward` runs on the delivery queue.
    private final class SampleState: @unchecked Sendable {
        let lock = NSLock()
        var remap: DeviceGyroRemap = .identity
        var sentSample = false
        /// Re-sent with the closing zero-gyro sample so "rotation stopped" doesn't also
        /// overwrite a plausible gravity vector with free-fall.
        var lastAccel: (Int16, Int16, Int16) = (0, 0, 0)
    }

    /// Ship one converted sample (wire pad 0). Must be thread-safe — invoked from the
    /// delivery queue (`PunktfunkConnection.sendMotion` locks internally).
    private let send: @Sendable (_ gyro: (Int16, Int16, Int16), _ accel: (Int16, Int16, Int16)) -> Void

    private let motion = CMMotionManager()
    /// Dedicated serial delivery queue — deliberately NOT main (the controller path's
    /// main-queue delivery is a known jitter source; the mirror starts clean).
    private let queue: OperationQueue = {
        let q = OperationQueue()
        q.name = "punktfunk.device-gyro"
        q.maxConcurrentOperationCount = 1
        return q
    }()

    private let state = SampleState()
    private var orientationObserver: NSObjectProtocol?

    /// Whether the mirror is between `start` and `stop` — read by GamepadCapture to keep the
    /// controller path off pad 0's motion while this runs.
    public private(set) var isRunning = false

    public init(
        send: @escaping @Sendable (_ gyro: (Int16, Int16, Int16), _ accel: (Int16, Int16, Int16)) -> Void
    ) {
        self.send = send
    }

    /// Begin sourcing pad-0 motion from this device. Idempotent.
    public func start() {
        guard !isRunning, motion.isDeviceMotionAvailable else { return }
        isRunning = true
        updateRemap()
        // Interface orientation only changes alongside a device-orientation notification, so
        // this is the one signal needed; re-reading the scene keeps a rotation lock stable.
        orientationObserver = NotificationCenter.default.addObserver(
            forName: UIDevice.orientationDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.updateRemap() }
        }
        // CoreMotion's practical ceiling; requesting faster just clamps.
        motion.deviceMotionUpdateInterval = 1.0 / 100.0
        motion.startDeviceMotionUpdates(to: queue) { [state, send] m, _ in
            guard let m else { return }
            Self.forward(m, state: state, send: send)
        }
    }

    /// Stop sourcing and, if anything was sent, park the host pad's rotation at zero. The
    /// zero rides the same serial queue as the samples, so it is guaranteed last — without
    /// blocking the caller.
    public func stop() {
        guard isRunning else { return }
        isRunning = false
        motion.stopDeviceMotionUpdates()
        if let o = orientationObserver {
            NotificationCenter.default.removeObserver(o)
            orientationObserver = nil
        }
        queue.addOperation { [state, send] in
            state.lock.lock()
            let owed = state.sentSample
            state.sentSample = false
            let accel = state.lastAccel
            state.lock.unlock()
            if owed { send((0, 0, 0), accel) }
        }
    }

    private func updateRemap() {
        let o = UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .first?.interfaceOrientation ?? .portrait
        state.lock.lock()
        state.remap = DeviceGyroRemap(o)
        state.lock.unlock()
    }

    /// Runs on the delivery queue: remap, scale, ship.
    nonisolated private static func forward(
        _ m: CMDeviceMotion, state: SampleState,
        send: (_ gyro: (Int16, Int16, Int16), _ accel: (Int16, Int16, Int16)) -> Void
    ) {
        state.lock.lock()
        let r = state.remap
        state.lock.unlock()
        let rot = r.apply(
            x: Float(m.rotationRate.x), y: Float(m.rotationRate.y), z: Float(m.rotationRate.z))
        // Total acceleration, NEGATED: Apple reports the gravity vector (down), the wire carries
        // proper acceleration (up at rest), as `GamepadCapture.forwardMotion` does.
        let acc = r.apply(
            x: -Float(m.gravity.x + m.userAcceleration.x),
            y: -Float(m.gravity.y + m.userAcceleration.y),
            z: -Float(m.gravity.z + m.userAcceleration.z))
        // NO frame conversion here, and that is not an oversight — `GamepadCapture.forwardMotion`
        // applies `GamepadWire.appleMotionToWire` and this deliberately does not.
        //
        // The trap is that two different frames are both called "the controller frame". GCMotion
        // reports a CONTROLLER in (Right, Forward, Up) — measured on a real DualSense — which is
        // not the wire's frame, hence the conversion over there. `r` above resolves THIS DEVICE
        // into the frame the header describes: x right, y up, z out of the screen. For the pose
        // this mirror exists to serve — a phone clipped upright, screen facing the player — "out of
        // the screen" points AT the player, so that frame is (Right, Up, Backward), which IS the
        // wire's frame. Straight through is already correct.
        //
        // Applying the controller path's conversion here was tried and was WRONG: a phone at rest
        // would have reported gravity as −1 g on the roll axis instead of +1 g up, i.e. lying on
        // its edge. Caught by measuring the Android twin, which does the same thing straight
        // through and reads +1 g on the up axis end to end. If a future capture path needs a
        // conversion, decide it from that source's OWN measured frame rather than by analogy.
        let gs = GamepadWire.gyroLSBPerRadS
        let as_ = GamepadWire.accelLSBPerG
        let gyro = (
            GamepadWire.motionRaw(rot.x, scale: gs),
            GamepadWire.motionRaw(rot.y, scale: gs),
            GamepadWire.motionRaw(rot.z, scale: gs)
        )
        let accel = (
            GamepadWire.motionRaw(acc.x, scale: as_),
            GamepadWire.motionRaw(acc.y, scale: as_),
            GamepadWire.motionRaw(acc.z, scale: as_)
        )
        state.lock.lock()
        state.lastAccel = accel
        state.sentSample = true
        state.lock.unlock()
        send(gyro, accel)
    }
}
#endif
