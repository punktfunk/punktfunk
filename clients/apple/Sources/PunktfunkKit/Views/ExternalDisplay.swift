// An attached monitor as its own full-screen stream surface (iOS).
//
// iOS gives a USB-C or AirPlay monitor a non-interactive scene: before iOS 27 through the app
// delegate, from iOS 27 only while a stream registers its scene accessory. While that scene holds
// no window, iOS mirrors the phone into it, letterboxed. A session puts its picture there instead
// (StreamViewController); Native then means the monitor's own size and rate. Touch, the HUD and
// the ring stay on the phone.

#if os(iOS)
import AVFoundation
import UIKit

/// The monitor scene's delegate. The app assigns it the `windowExternalDisplayNonInteractive` role.
public final class ExternalDisplaySceneDelegate: UIResponder, UIWindowSceneDelegate {
    public func scene(
        _ scene: UIScene, willConnectTo session: UISceneSession,
        options connectionOptions: UIScene.ConnectionOptions
    ) {
        guard let scene = scene as? UIWindowScene else { return }
        ExternalDisplay.shared.connect(scene)
    }

    public func sceneDidDisconnect(_ scene: UIScene) {
        guard let scene = scene as? UIWindowScene else { return }
        ExternalDisplay.shared.disconnect(scene)
    }
}

/// The attached monitor, and the video views shown on it.
@MainActor
public final class ExternalDisplay {
    public static let shared = ExternalDisplay()
    /// Posted when a monitor connects or disconnects.
    static let didChange = Notification.Name("punktfunk.externalDisplayDidChange")

    private weak var scene: UIWindowScene?
    private var window: UIWindow?

    /// The monitor's screen; nil with none attached.
    var screen: UIScreen? { scene?.screen }

    func connect(_ scene: UIWindowScene) {
        self.scene = scene
        NotificationCenter.default.post(name: Self.didChange, object: nil)
        LandscapeLock.refresh()
    }

    func disconnect(_ scene: UIWindowScene) {
        guard scene === self.scene else { return }
        window = nil
        self.scene = nil
        NotificationCenter.default.post(name: Self.didChange, object: nil)
        LandscapeLock.refresh()
    }

    /// Fill the monitor with `view`, on top of any other. The window is never key: input stays
    /// with the phone's scene.
    func show(_ view: UIView) {
        guard let scene else { return }
        let window = window ?? UIWindow(windowScene: scene)
        let root = window.rootViewController ?? UIViewController()
        root.view.backgroundColor = .black
        view.frame = root.view.bounds
        view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        root.view.addSubview(view)
        window.rootViewController = root
        window.isHidden = false
        self.window = window
    }

    /// Take `view` off the monitor. With no view left the window leaves the scene, and iOS
    /// mirrors the phone again.
    func hide(_ view: UIView) {
        guard let window, let root = window.rootViewController?.view, view.superview === root
        else { return }
        view.removeFromSuperview()
        if root.subviews.isEmpty {
            window.windowScene = nil
            self.window = nil
        }
    }

    /// The monitor's pixels and top refresh: Native while the picture is on it. nil with no
    /// monitor.
    public static var nativeMode: (width: Int, height: Int, hz: Int)? {
        guard let screen = shared.screen else { return nil }
        let px = screen.currentMode?.size ?? screen.nativeBounds.size
        guard px.width > 0, px.height > 0 else { return nil }
        return (Int(px.width), Int(px.height), screen.maximumFramesPerSecond)
    }
}

/// The monitor-side video surface: a bare display layer for the presenter to rebuild onto.
/// `onLayout` fires on each layout, which is when the presenter re-fits to the monitor.
final class ExternalVideoView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    var displayLayer: AVSampleBufferDisplayLayer {
        // swiftlint:disable:next force_cast
        layer as! AVSampleBufferDisplayLayer
    }
    var onLayout: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        backgroundColor = .black
        displayLayer.videoGravity = .resizeAspect
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    override func layoutSubviews() {
        super.layoutSubviews()
        onLayout?()
    }
}
#endif
