// The stream view on iOS, iPadOS, visionOS and tvOS: the shared SessionPresenter in a
// UIViewController, so the scene can pointer-lock. UITouch.type routes fingers and pointers apart.
//
// Direct fingers follow the touch-input model: `touch` forwards wire touches to a host that
// injects them, mapped through the video placement into host-mode pixels, and the other models
// drive TouchMouse. A Pencil takes the pen plane toward a pen-capable host and is a finger
// otherwise. A mouse or trackpad is a pointer and never forwards as a touch. Toward a touchpad
// host, Trackpad mode's fingers and the trackpad's pinch go out as touchpad contacts.
//
// Locked (full-screen, frontmost, pointer capture allowed; see PointerLockChain): GCMouse drives
// motion and buttons for every connected mouse and the system hides the cursor. Unlocked (Stage
// Manager, not frontmost, iPhone): UIKit hover and indirect touches forward an absolute cursor
// plus buttons. `gcMouseForwarding` (== locked) keeps the two apart, so a pointer reporting on both
// never double-sends. Scroll always comes from UIKit's pan recognizers, locked or not.
//
// Hardware keyboards share InputCapture with macOS: engaged at stream start, and ⌘⎋ or the
// ⌃⌥⇧Q shortcut toggle it, both read from the HID stream.
//
// The public type is named StreamView like its macOS twin, so the SwiftUI layer is shared.

#if os(iOS) || os(visionOS) || os(tvOS)
import AVFoundation
import Accessibility
import GameController
import PunktfunkCore
import PunktfunkShared
import SwiftUI
import UIKit
import os
#if os(tvOS)
import AVKit // AVDisplayManager — the per-session display-mode (HDR10/refresh) request
#endif

#if os(visionOS)
/// One pinch on the curved screen, in host-mode pixels.
struct CurvePinch {
    /// A wire contact id no finger in the window takes while the picture is on the curve.
    static let touchID: UInt32 = 15
    /// Travel under this many host pixels, under `tapSeconds`, is a click, not a drag.
    static let tapSlop: CGFloat = 16
    static let tapSeconds: CFTimeInterval = 0.35
    let route: TouchInputMode
    let start: CGPoint
    var last: CGPoint
    let at: CFTimeInterval
    /// Sub-pixel travel not yet sent.
    var carry = CGPoint.zero

    func isTap(at end: CGPoint) -> Bool {
        hypot(end.x - start.x, end.y - start.y) < Self.tapSlop
            && CACurrentMediaTime() - at < Self.tapSeconds
    }
}
#endif

/// Same diagnostic switch as InputCapture (PUNKTFUNK_INPUT_DEBUG=1): logs the resolved
/// pointer-lock state on every capture or lock change, so a log shows whether the scene locked
/// (GCMouse delivers deltas only then) or the absolute pointer path carries the mouse.
private let iosInputLog = ClientLog(category: "input")
private let sceneLog = ClientLog(category: "present")
private let iosInputDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

public struct StreamView: UIViewControllerRepresentable {
    private let connection: PunktfunkConnection
    private let captureEnabled: Bool
    private let onCaptureChange: ((Bool) -> Void)?
    private let onShortcut: ((Shortcuts.Action, Shortcuts.Phase) -> Void)?
    private let onDial: ((DialEvent) -> Void)?
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?
    private let onResizeTarget: ResizeTarget?
    private let onDecodedSize: (@Sendable (Int, Int) -> Void)?
    private let endToEndMeter: LatencyMeter?
    #if os(visionOS)
    private var curveTarget: CurveTarget?
    #endif
    #if os(iOS)
    private var keyRowClient: KeyRowClient?
    #endif

    /// `onShortcut` (main queue) is a keyboard shortcut that fired on an iPad keyboard, as on
    /// the macOS StreamView.
    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onShortcut: ((Shortcuts.Action, Shortcuts.Phase) -> Void)? = nil,
        onDial: ((DialEvent) -> Void)? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil,
        onResizeTarget: ResizeTarget? = nil,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        endToEndMeter: LatencyMeter? = nil
    ) {
        self.connection = connection
        self.captureEnabled = captureEnabled
        self.onCaptureChange = onCaptureChange
        self.onShortcut = onShortcut
        self.onDial = onDial
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
        self.onResizeTarget = onResizeTarget
        self.onDecodedSize = onDecodedSize
        self.endToEndMeter = endToEndMeter
    }

    #if os(visionOS)
    /// Draw the picture on `target`'s curved screen instead of this view while it is set.
    public func curve(_ target: CurveTarget?) -> StreamView {
        var view = self
        view.curveTarget = target
        return view
    }
    #endif

    #if os(iOS)
    /// The session behind the keyboard row's quick actions: how each looks and what runs it.
    public func keyRow(_ client: KeyRowClient?) -> StreamView {
        var view = self
        view.keyRowClient = client
        return view
    }
    #endif

    public func makeUIViewController(context: Context) -> StreamViewController {
        let controller = StreamViewController()
        #if os(visionOS)
        controller.setCurve(curveTarget)
        #endif
        controller.onCaptureChange = onCaptureChange
        controller.onShortcut = onShortcut
        controller.onDial = onDial
        controller.captureEnabled = captureEnabled
        controller.endToEndMeter = endToEndMeter
        controller.onResizeTarget = onResizeTarget
        controller.onDecodedSize = onDecodedSize
        #if os(iOS)
        controller.keyRowClient = keyRowClient
        #endif
        controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return controller
    }

    public func updateUIViewController(_ controller: StreamViewController, context: Context) {
        controller.onCaptureChange = onCaptureChange
        controller.onShortcut = onShortcut
        controller.onDial = onDial
        controller.captureEnabled = captureEnabled
        controller.endToEndMeter = endToEndMeter
        controller.onResizeTarget = onResizeTarget
        controller.onDecodedSize = onDecodedSize
        #if os(visionOS)
        controller.setCurve(curveTarget)
        #endif
        #if os(iOS)
        controller.keyRowClient = keyRowClient
        #endif
        if controller.connection !== connection {
            controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        }
    }

    public static func dismantleUIViewController(
        _ controller: StreamViewController, coordinator: ()
    ) {
        controller.stop()
    }
}

#if os(tvOS)
/// tvOS: a GCEventViewController with `controllerUserInteractionEnabled = false` routes game-
/// controller (and Siri Remote) input EXCLUSIVELY to the GameController framework while the
/// stream is up. Without it a pad's B/Menu press is also a UIKit menu press, which ends the
/// session or suspends the app from ordinary gameplay. Every in-session exit is GC-level by
/// design: the pad's escape chord (GamepadCapture) and the remote's hold-Back
/// (SiriRemotePointer).
public typealias StreamViewControllerBase = GCEventViewController
#else
public typealias StreamViewControllerBase = UIViewController
#endif

public final class StreamViewController: StreamViewControllerBase {
    public private(set) var connection: PunktfunkConnection?
    private var observers: [NSObjectProtocol] = []
    /// Capture→on-glass for the A/V sync loop while the stage-2 presenter runs. Read at start().
    var endToEndMeter: LatencyMeter?
    /// The shared presenter stack: stage-2 (CAMetalLayer sublayer + display link) with the
    /// stage-1 StreamPump → displayLayer path as the Metal-unavailable fallback (or
    /// `PUNKTFUNK_PRESENTER=stage1`).
    private let presenter = SessionPresenter()
    #if os(tvOS)
    /// The window's display manager the session's mode request was set on — held weakly so
    /// stop() can clear the request even after the view has left the window.
    private weak var sessionDisplayManager: AVDisplayManager?
    /// Watches the mode switch our criteria asked for; its end is the HDR output signal.
    private var displayModeObservation: NSKeyValueObservation?
    /// The decoded frames are HDR — what the display-mode request follows.
    private var frameHDR = false
    /// Bumped when a display request is withdrawn, so its switch-end hop can tell it is stale.
    private var hdrRequest = 0
    #endif
    private var inputCapture: InputCapture?
    #if os(iOS) || os(visionOS)
    fileprivate var captured = false
    private var pointerInteraction: UIPointerInteraction?
    /// Capture state at the last resign, restored on the next foreground — otherwise the
    /// mouse/keyboard stay released after navigating out and nothing re-grabs them.
    private var wasCapturedOnResign = false
    /// Match window's follower, non-nil while a session is active. It acts only while the
    /// `matchWindow` setting is on and the picture is not on a monitor (`routeVideo`).
    /// `viewDidLayoutSubviews` feeds it the view's pixel size, so a scene resize, a
    /// rotation, a fold or the upright half of a half-open one renegotiates the host mode 1:1.
    /// tvOS drives display modes through AVDisplayManager instead.
    private var matchFollower: MatchWindowFollower?
    /// The view the last surface ask sent beside its mode; nil before one, when the connect's
    /// stands.
    private var surfaceView: (width: UInt32, height: UInt32)??
    /// The picture's surface on an attached monitor (see `ExternalDisplay`), and whether the
    /// stream presents there now. Input and the HUD stay on the phone either way.
    #if os(iOS)
    private lazy var externalVideo: ExternalVideoView = {
        let view = ExternalVideoView()
        view.onLayout = { [weak self] in self?.layoutMetalLayer() }
        return view
    }()
    /// The session's monitor accessory (`UISceneAccessoryRegistration`, iOS 27+). From iOS 27 the
    /// monitor's scene connects only while one is registered.
    private var monitorAccessory: AnyObject?
    /// This screen's native mode at the last layout pass, and the request a change of it queued.
    private var lastNativeMode: (width: Int, height: Int, hz: Int)?
    private var screenModeWork: DispatchWorkItem?
    /// The phone's mode, queued while a pulled monitor may still come back.
    private var monitorLossWork: DispatchWorkItem?
    #endif
    private var onExternal = false
    /// The re-ask edge and its clocks; `prefersPointerLocked` mirrors `wantsPointerLock` outside
    /// an edge. SpringBoard grants or drops on its own terms.
    private var pointerLock = PointerLock()
    /// The quick-action ring is up, so the mouse is UIKit's: a locked pointer cannot click a
    /// button above the video. Only the lock yields. The keyboard stays captured, or ⌃⌥⇧O
    /// could not close the ring.
    private var ringHoldsPointer = false
    #endif
    #if os(visionOS)
    /// The scene `fitWindowToPicture` restricted; `stop()` hands it back free-resizing.
    private weak var fittedScene: UIWindowScene?
    #endif

    #if os(iOS) || os(visionOS)
    /// Whether the scene's pointer is locked right now; nil with no scene yet. Only while this is
    /// true does GCMouse deliver relative deltas; otherwise the absolute pointer path carries the
    /// mouse.
    private func pointerLockEngaged() -> Bool? {
        view.window?.windowScene?.pointerLockState?.isLocked
    }
    #endif

    var onCaptureChange: ((Bool) -> Void)?
    /// A keyboard shortcut that fired (see `StreamView.init`).
    var onShortcut: ((Shortcuts.Action, Shortcuts.Phase) -> Void)?
    #if os(iOS)
    /// The session behind the keyboard row's quick actions. SwiftUI hands a fresh one whenever
    /// the scene's state changed, which is when a button's face may have.
    var keyRowClient: KeyRowClient? {
        didSet { if isViewLoaded { streamView.keyRowClient = keyRowClient } }
    }
    #endif
    /// The two-finger twist turning the quick-action ring (forwarded from the stream view).
    var onDial: ((DialEvent) -> Void)?
    /// Resize-overlay START: forwarded to the Match-window follower so a scene resize drives the
    /// blur+spinner the instant the window differs from the live mode (iOS and visionOS; tvOS has
    /// no follower). See `MatchWindowFollower.onResizeTarget`.
    var onResizeTarget: ResizeTarget?
    /// Resize-overlay END: the presenter reports the coded dims of each new-mode IDR here, so the
    /// overlay clears when a frame at the requested mode or its view actually decodes.
    var onDecodedSize: (@Sendable (Int, Int) -> Void)?
    /// Last decoded size fed into the presenter's aspect-fit. A new-mode IDR (an iPad scene resize,
    /// or a tvOS AVDisplayManager mode switch) re-fits the metal sublayer to the REAL content aspect
    /// here — `viewDidLayoutSubviews` only re-runs on a bounds change, which a resize-END lacks, so
    /// without this the layer keeps its pre-resize aspect and stretches the new frame into it. Main.
    private var lastDecodedContentSize: CGSize?

    var captureEnabled = true {
        didSet {
            guard captureEnabled != oldValue else { return }
            #if os(iOS) || os(visionOS)
            setCaptured(captureEnabled)
            #else
            inputCapture?.setForwarding(captureEnabled)
            #endif
            #if os(iOS)
            if isViewLoaded {
                streamView.touchTrusted = captureEnabled
                streamView.syncTouchpadFrames()
            }
            #endif
        }
    }

    private var streamView: StreamLayerUIView {
        // swiftlint:disable:next force_cast
        view as! StreamLayerUIView
    }

    public override func loadView() {
        view = StreamLayerUIView()
        presenter.surfaceState = { [weak self] in self?.surfaceLine() ?? "-" }
        #if os(tvOS)
        // Kill the pad/remote → UIKit press path at the source for the whole session (see the
        // GCEventViewController typealias above). GC delivery is untouched: GamepadCapture
        // forwards the pad, SiriRemotePointer drives the pointer and owns the remote exit.
        controllerUserInteractionEnabled = false
        #endif
        // Re-size the stage-2 drawable if the display scale changes without a bounds change (e.g.
        // moving to an external display at a different scale) — the iOS analogue of macOS's
        // viewDidChangeBackingProperties relayout. The handler takes the VC as its argument, so it
        // doesn't capture self (no retain cycle with the registration).
        registerForTraitChanges([UITraitDisplayScale.self]) { (vc: StreamViewController, _) in
            vc.layoutMetalLayer()
        }
        #if os(iOS) || os(visionOS)
        // Hides the local cursor while the scene is pointer-locked and the host draws its own
        // from GCMouse deltas (the delegate below). Unlocked, the cursor stays visible. The
        // system grants the lock only to a full-screen, frontmost iPad scene.
        let interaction = UIPointerInteraction(delegate: self)
        view.addInteraction(interaction)
        pointerInteraction = interaction
        #endif
        #if os(iOS) || os(visionOS)
        // The pad is the game's: on iOS 18+ and visionOS every press goes to GameController
        // alone. Otherwise visionOS turns a press into a pinch, and iPadOS moves focus onto the
        // stream view, which raises the soft keyboard or takes it down.
        if #available(iOS 18.0, *) {
            let pad = GCEventInteraction()
            pad.handledEventTypes = .gamepad
            view.addInteraction(pad)
        }
        #endif
    }

    #if os(iOS) || os(visionOS)
    /// Whether the user wants the mouse/trackpad pointer CAPTURED (pointer lock → relative
    /// movement, the gaming default) rather than forwarded as an absolute position (desktop
    /// use). Read from the session's resolved settings so it tracks the Settings toggle (it is
    /// tier G — this device's input hardware — so no preset can move it); defaults to on when
    /// unset. iPad-only — gated again in `prefersPointerLocked`.
    private var pointerCaptureEnabled: Bool {
        connection?.settings.pointerCapture ?? true
    }

    /// Whether the pointer should be CAPTURED right now: iPad, capture engaged, the quick-action
    /// ring down, and the user hasn't opted into the absolute (desktop) pointer. The system
    /// additionally requires full-screen + frontmost and may drop the lock (Slide Over/Stage
    /// Manager/backgrounding) — syncPointerLock() handles the actual grant/drop and falls back
    /// to absolute when unlocked.
    private var wantsPointerLock: Bool {
        // The trailing grant test is per-client access §7 — no pointer lock without the
        // POINTER bit (a Controller-only guest's trackpad stays a normal local pointer);
        // read live, so a mid-session re-grant lets the next resolve pass lock.
        captured && !ringHoldsPointer && pointerCaptureEnabled
            && UIDevice.current.userInterfaceIdiom == .pad
            && connection?.canSendPointer == true
    }

    public override var prefersPointerLocked: Bool {
        pointerLock.prefersLocked(wanted: wantsPointerLock)
    }
    public override var prefersHomeIndicatorAutoHidden: Bool { true }

    // NOTE: we deliberately do NOT override `childViewControllerForPointerLock`. The default
    // returns nil, which tells the system to use THIS controller's own `prefersPointerLocked` —
    // exactly what we want, since `PointerLockChain` forces our SwiftUI ancestors to forward the
    // downward walk to us and we are the terminal anchor. Returning `self` here would make the
    // system ask the same controller forever (it keeps delegating to the returned child) →
    // unbounded recursion → stack overflow once the chain actually reaches us.

    /// (Re)build or tear down the forced pointer-lock forwarding chain from this controller to the
    /// window root so the system actually resolves our `prefersPointerLocked`. Safe to call
    /// repeatedly — it no-ops until the view is in a window with a parent chain, and re-runs from
    /// the appearance/parent callbacks once SwiftUI has placed us.
    private func updatePointerLockChain() {
        // Engaging needs a live parent chain to the window root; disengaging is always safe and
        // must run even after the view has left the window (session teardown) so the stamped
        // SwiftUI ancestors are cleared.
        if wantsPointerLock, view.window != nil {
            PointerLockChain.engage(self)
        } else {
            PointerLockChain.disengage(self)
        }
    }

    public override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        // SwiftUI places us in the hierarchy AFTER start()'s setCaptured(true), and may reparent us
        // later — re-anchor the chain here so a lock requested before we had a parent still lands.
        updatePointerLockChain()
        anchorKeyResponder()
    }

    public override func didMove(toParent parent: UIViewController?) {
        super.didMove(toParent: parent)
        updatePointerLockChain() // chain shape changed — re-anchor (or no-op if not yet in a window)
    }

    /// Put the stream on the responder chain, and key focus on its view, for hardware key
    /// presses.
    ///
    /// Keys arrive on GameController (`GCKeyboard`), a parallel feed that does not consume the
    /// UIKit event, so each press reaches UIKit too: `pressesBegan` claims the ones the system
    /// must not act on. Under Full Keyboard Access presses are the only feed. UIKit sends them
    /// to the focused item, which is an overlay control unless the view holds focus.
    ///
    /// A controller is not `UIKeyInput`, so being first responder raises no keyboard. The view
    /// is, and keeps the chain while its soft keyboard is up. With Full Keyboard Access proven
    /// the view takes the chain itself and focus follows it (`StreamLayerUIView.takeKeyChain`).
    ///
    /// Only while captured. `start()` engages capture before SwiftUI puts the view in a window,
    /// where neither call succeeds, so `viewDidAppear` calls this again.
    private func anchorKeyResponder() {
        guard captured else { return }
        streamView.holdsKeyFocus = true
        if streamView.takeKeyChain() { return }
        if !streamView.isFirstResponder, !isFirstResponder { becomeFirstResponder() }
        setNeedsFocusUpdate()
        updateFocusIfNeeded()
    }

    public override var canBecomeFirstResponder: Bool { true }

    /// Hardware key presses: each key goes to `InputCapture.pressKey`, and the stream claims
    /// the ones that are its own.
    ///
    /// While captured it claims the keys the system acts on (`systemKeys`), so they reach the
    /// host alone. It also claims every key the press feed carries, which is all of them while
    /// GameController is silent under Full Keyboard Access.
    ///
    /// Everything else goes to `super` untouched, as does every key while capture is released:
    /// Escape still dismisses sheets and exits full screen then.
    public override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unclaimed = presses.filter { !takesPress($0, down: true) }
        if !unclaimed.isEmpty || presses.isEmpty {
            super.pressesBegan(unclaimed, with: event)
        }
    }

    public override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unclaimed = presses.filter { !takesPress($0, down: false) }
        if !unclaimed.isEmpty || presses.isEmpty {
            super.pressesEnded(unclaimed, with: event)
        }
    }

    public override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        // A release for the press feed. Never swallowed: UIKit keeps books on a claimed press.
        for press in presses { _ = takesPress(press, down: false) }
        super.pressesCancelled(presses, with: event)
    }

    /// Keys the system acts on when nothing claims them: Escape releases the scene's pointer
    /// lock and refuses a re-lock for a while, Tab and the arrows move focus.
    private static let systemKeys: Set<UIKeyboardHIDUsage> = [
        .keyboardEscape, .keyboardTab,
        .keyboardRightArrow, .keyboardLeftArrow, .keyboardDownArrow, .keyboardUpArrow,
    ]

    /// Feeds the press's key to `InputCapture`. True when the stream claims it: while captured,
    /// the `systemKeys` and every key that feed carries.
    private func takesPress(_ press: UIPress, down: Bool) -> Bool {
        guard let key = press.key?.keyCode else { return false }
        let fed = inputCapture?.pressKey(usage: key.rawValue, down: down) ?? false
        return captured && (fed || Self.systemKeys.contains(key))
    }
    #endif

    #if os(tvOS)
    // The GCEventViewController's interaction flag applies to the deepest such controller
    // CONTAINING THE FIRST RESPONDER — inside SwiftUI's hosting-controller sandwich that is not
    // guaranteed to be us unless we anchor the responder chain here explicitly.
    public override var canBecomeFirstResponder: Bool { true }

    public override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        becomeFirstResponder()
    }
    #endif

    func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)?,
        onSessionEnd: (@Sendable () -> Void)?
    ) {
        stop()
        self.connection = connection
        loadViewIfNeeded()
        #if os(iOS) || os(visionOS)
        // Fresh session: drop any resign/foreground capture-restore state left over from a
        // prior session (stop() doesn't clear it). Otherwise a stale `true` could later
        // re-engage capture on a foreground that the new session never asked for.
        wasCapturedOnResign = false
        wireTouchSurface(connection)

        let capture = InputCapture(connection: connection)
        capture.onToggleCapture = { [weak self] in
            guard let self else { return }
            self.setCaptured(!self.captured)
        }
        // The keyboard shortcuts, detected from the HID stream in InputCapture (no NSEvent
        // monitor on iOS), go to the session, which runs them as the dial does.
        capture.onShortcut = { [weak self] action, phase in self?.onShortcut?(action, phase) }
        capture.onPreempted = { [weak self] in
            self?.setCaptured(false)
        }
        #if os(iOS)
        // A hardware key is the next key to a modifier the keyboard row holds.
        capture.onKeyLifted = { [weak self] in self?.streamView.hostKeyLifted() }
        #endif
        capture.onKeyboardReposted = { on in FullKeyboardAccess.proven = on }
        streamView.onFullKeyboardAccess = { [weak self] in
            self?.inputCapture?.gameControllerFellSilent()
            FullKeyboardAccess.proven = true
        }
        streamView.onActivate = { [weak self] in self?.setCaptured(true) }
        // Proven on, the view takes the responder chain. Off, the controller takes it back.
        // The proof also changes outside this stream.
        observers.append(NotificationCenter.default.addObserver(
            forName: FullKeyboardAccess.didChange, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated {
                if !FullKeyboardAccess.proven { self?.streamView.dropKeyChain() }
                self?.anchorKeyResponder()
            }
        })
        capture.start()
        inputCapture = capture
        // Match window: follow the picture's own pixel size, so a resized scene, a fold or the
        // upright half of a half-open one streams 1:1. `viewDidLayoutSubviews` feeds it. Off by
        // default, like the Settings toggle.
        let follower = MatchWindowFollower(
            connection: connection,
            enabled: connection.settings.matchWindow,
            renderScale: connection.settings.renderScale,
            maxDimension: RenderScale.maxDimension(codec: connection.settings.codec))
        follower.onResizeTarget = onResizeTarget
        #if os(iOS)
        // A native rate follows the screen the picture is on.
        follower.refreshHz = { [weak self] in
            guard let self, self.connection?.settings.refreshHz == 0 else { return nil }
            return UInt32(clamping: max(self.nativeMode.hz, 30))
        }
        // What the connect asked for: a session that starts on a half-open fold differs at once.
        lastNativeMode = NativeDisplay.mode
        #endif
        matchFollower = follower
        #endif
        #if os(iOS)
        // A monitor attached before the session starts shows the picture from the first frame.
        onExternal = ExternalDisplay.shared.screen != nil
        if onExternal { ExternalDisplay.shared.show(externalVideo) }
        // iOS 27 connects the monitor's scene only for this; `routeVideo` takes it when it lands.
        if #available(iOS 27, *) {
            let config = UISceneConfiguration(
                name: nil, sessionRole: .windowExternalDisplayNonInteractive)
            config.delegateClass = ExternalDisplaySceneDelegate.self
            let accessory = UISceneAccessory.externalNonInteractive(sceneConfiguration: config)
            monitorAccessory = registerSceneAccessory(accessory)
        }
        #endif

        // SessionPresenter picks stage 2 or the stage-1 fallback and owns their lifecycle. The
        // coded-dims hop re-fits the metal sublayer to the real content aspect on main before the
        // owner's overlay END signal; it fires only on a size change, never per frame.
        let overlayDecodedSize = onDecodedSize
        presenter.start(
            connection: connection,
            baseLayer: videoLayer,
            endToEndMeter: endToEndMeter,
            makeDisplayLink: { CADisplayLink(target: $0, selector: $1) },
            onFrame: onFrame,
            onSessionEnd: onSessionEnd,
            onDecodedSize: { [weak self] w, h in
                DispatchQueue.main.async { self?.noteDecodedContentSize(width: w, height: h) }
                overlayDecodedSize?(w, h)
            },
            onFrameHDR: { [weak self] hdr in
                DispatchQueue.main.async { self?.noteFrameHDR(hdr) }
            })
        layoutMetalLayer()

        #if os(iOS) || os(visionOS)
        observeScene(connection)

        if captureEnabled {
            setCaptured(true) // entering a session is the deliberate "capture me" moment
        }
        #endif

        #if os(tvOS)
        startTV(connection)
        #endif
    }

    #if os(iOS) || os(visionOS)
    /// The stream view's touch, Pencil, pointer and scroll sinks for `connection`'s session.
    private func wireTouchSurface(_ connection: PunktfunkConnection) {
        streamView.settings = connection.settings
        #if os(iOS)
        streamView.keyRowClient = keyRowClient
        #endif
        // The letterbox must follow an accepted requestMode() mid-stream, so this stays a live
        // read — but behind a short TTL: the pencil path maps every coalesced sample through
        // here (≤8 per event at panel rate, main thread), and a per-sample FFI read re-takes
        // the ABI lock the batch send itself needs next. 250 ms staleness is invisible next to
        // the video reconfigure a mode switch performs anyway. Main-thread-only closure.
        var cachedMode = CGSize.zero
        var cachedAt = CACurrentMediaTime() - 1
        // The DECODED frame's size, not the negotiated mode's: the two disagree whenever a host
        // correctively acks a different mode (Windows falls back to an advertised one), and the
        // presenter aspect-fits to the decoded size — so mapping touches through the mode would
        // letterbox against a different rectangle and offset every tap for the session. The mode
        // is the fallback until the first frame lands.
        streamView.currentHostMode = { [weak self, weak connection] in
            let now = CACurrentMediaTime()
            if now - cachedAt > 0.25 {
                if let decoded = self?.lastDecodedContentSize,
                   decoded.width > 0, decoded.height > 0 {
                    cachedMode = decoded
                } else if let connection {
                    let mode = connection.currentMode()
                    cachedMode = CGSize(width: Double(mode.width), height: Double(mode.height))
                } else {
                    return .zero
                }
                cachedAt = now
            }
            return cachedMode
        }
        streamView.onTouchEvent = { [weak self, weak connection] event in
            // Touch IS the intent during a trusted session, but must not leak to the host
            // while a trust prompt is up (captureEnabled == false) — gate it on that. The
            // ⌘⎋ mouse/keyboard toggle (captured) deliberately does NOT gate touch.
            guard self?.captureEnabled == true else { return }
            connection?.send(event)
        }
        // Apple Pencil → the stylus plane, only against a pen-capable host (elsewhere the
        // Pencil stays a finger). Same trust gate as touch.
        streamView.penEnabled = connection.hostSupportsPen
        streamView.touchPassthroughEnabled = connection.hostSupportsTouch
        streamView.textInputEnabled = connection.hostSupportsText
        // The two-finger twist → the quick-action ring, same trust gate as touch.
        streamView.onDial = { [weak self] event in
            guard let self, self.captureEnabled else { return }
            self.onDial?(event)
        }
        streamView.onPenBatch = { [weak self, weak connection] batch in
            guard self?.captureEnabled == true else { return }
            connection?.sendPen(batch)
        }
        // Indirect pointer WITHOUT a lock → absolute cursor + buttons. Under lock a mouse or trackpad
        // also emits UIKit indirect events pinned at the lock point while GCMouse owns motion and
        // buttons, so this path is gated off (`gcMouseForwarding`). Otherwise a click sends twice.
        streamView.onPointerMoveAbs = { [weak self] p in
            guard let self, self.inputCapture?.gcMouseForwarding == false else { return }
            self.inputCapture?.sendMouseAbs(
                x: p.x, y: p.y, surfaceWidth: p.w, surfaceHeight: p.h)
        }
        streamView.onPointerButton = { [weak self] button, down in
            guard let self else { return }
            // Released → a primary press into the video re-engages capture, like macOS's
            // `engageCapture(fromClick:)`. It is the local gesture, never forwarded.
            // Captured → the absolute path forwards it.
            if !self.captured {
                if PointerLock.clickEngagesCapture(button: button, down: down), self.captureEnabled {
                    self.setCaptured(true, fromClick: true)
                }
                return
            }
            guard self.inputCapture?.gcMouseForwarding == false else { return }
            self.inputCapture?.sendMouseButton(button, pressed: down)
            // Captured but unlocked: the click is the user gesture SpringBoard wants before it
            // hands a declined or dropped lock back. On the UP, so the click has fully forwarded
            // on one transport — a grant mid-click would strand the release on the GCMouse path.
            if !down { self.stepPointerLock(.reask) }
        }
        // Every scroll, wheel and trackpad, locked or not: UIKit applies Natural Scrolling, which
        // GCMouse's raw wheel does not, so the direction never changes with the lock.
        streamView.onScroll = { [weak self] dx, dy, source, phase in
            self?.inputCapture?.sendScroll(dx: dx, dy: dy, source: source, phase: phase)
        }
        #if os(iOS)
        // Toward a touchpad host, Trackpad mode's fingers are its contacts (`refreshTouchpad`).
        streamView.touchpadHost = { [weak connection] in
            (connection?.hostPresentsTouchpad ?? false, connection?.hostTapsClick ?? false)
        }
        streamView.touchTrusted = captureEnabled
        streamView.onTouchpadSurface = { [weak connection] touches, keep, retractTaps, size, timeUs in
            connection?.touchpadSurface(
                touches, maxContacts: 2, keep: keep, retractTaps: retractTaps,
                widthDmm: size.width, heightDmm: size.height, timeUs: timeUs)
        }
        // The trackpad's pinch is pointer input like its scroll: it begins only while captured
        // (a pinch held back here is not a refusal), and its end always goes.
        streamView.onTouchpadPinch = { [weak self, weak connection] phase, scale, rotation in
            guard self?.captured == true || phase == UInt8(PUNKTFUNK_PINCH_END) else { return true }
            return connection?.sendTouchpadPinch(phase: phase, scale: scale, rotation: rotation)
                ?? false
        }
        streamView.refreshTouchpad()
        #endif
    }

    /// The app, scene, pointer-lock, ring and keyboard notifications a session follows.
    private func observeScene(_ connection: PunktfunkConnection) {
        // GC only delivers while active; everything held is flushed by InputCapture's
        // own resign observer — here we just mirror the capture state for the HUD and
        // the pointer lock.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.willResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self else { return }
            self.wasCapturedOnResign = self.captured
            self.setCaptured(false)
        })
        // Returning to the foreground restores the capture the user had before leaving —
        // without this the mouse/keyboard stay released and nothing re-grabs them (touch
        // always plays regardless). The macOS twin re-engages on a click into the video.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            // inputCapture != nil: don't try to restore before this session's capture is wired
            // up — setForwarding would silently no-op on the nil handlers and leave input dead.
            guard let self, self.captureEnabled, self.connection != nil, self.inputCapture != nil
            else { return }
            if self.wasCapturedOnResign {
                self.setCaptured(true)
            } else {
                // Captured before the scene was active (a launch straight into a stream): the
                // first ask found no frontmost scene, so ask again now there is one.
                self.stepPointerLock(.reask)
            }
        })
        // The system grants or drops the lock on its own terms (Slide Over, Stage Manager, the
        // window leaving screen size). Re-resolve the routing on every change; a drop while the
        // lock is still wanted asks once more (`PointerLock`).
        observers.append(NotificationCenter.default.addObserver(
            forName: UIPointerLockState.didChangeNotification, object: nil, queue: .main
        ) { [weak self] note in
            guard let self else { return }
            self.syncPointerLock()
            let scene = note.userInfo?[UIPointerLockState.sceneUserInfoKey] as? UIScene
            self.stepPointerLock(
                .lockChanged(ours: scene === self.view.window?.windowScene, at: CACurrentMediaTime()))
        })
        // The Stream menu's Release Mouse posts this, and so does the session once the grants lose
        // both pointer and keyboard (`AccessWarning.releasesCapture`). Only the foreground-active
        // scene's stream acts, the iPad analogue of macOS's key-window guard.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkReleaseCapture, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self,
                  self.view.window?.windowScene?.activationState == .foregroundActive else { return }
            self.setCaptured(false)
        })
        // The quick actions only this view runs, from the dial or a shortcut, for its session.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkStreamSlot, object: connection, queue: .main
        ) { [weak self] note in
            guard let self, note.userInfo?["slot"] as? String == SlotId.releaseInput.id else { return }
            self.setCaptured(!self.captured)
        })
        // The quick-action ring is SwiftUI above this layer. Unlock the pointer for as long as it
        // is up, and take the lock back when it closes: the false→true edge is the ask. A close
        // is honoured from any scene state, or a ring that outlived its scene's focus would
        // leave the pointer unlocked for the rest of the session.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkRingOpen, object: nil, queue: .main
        ) { [weak self] note in
            guard let self else { return }
            #if os(iOS)
            self.streamView.syncTouchpadFrames() // the ring may have switched the touch mode
            #endif
            let open = (note.object as? NSNumber)?.boolValue == true
            if open, self.view.window?.windowScene?.activationState != .foregroundActive { return }
            guard self.ringHoldsPointer != open else { return }
            self.ringHoldsPointer = open
            self.setNeedsUpdateOfPrefersPointerLocked()
            self.updatePointerLockChain()
            self.syncPointerLock()
        })
        // The ring's Keyboard slot shows the soft keyboard, and hides it when it is up. iPhone's
        // keyboard has no dismiss key, and passthrough has no three-finger swipe. The slot names
        // its session: every visionOS window is foreground-active at once. A trackpad half's
        // swipe says which way it went. On the curve the volume's anchor takes the keyboard.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkToggleSoftKeyboard, object: connection, queue: .main
        ) { [weak self] note in
            guard let self,
                  self.view.window?.windowScene?.activationState == .foregroundActive else { return }
            let show = note.userInfo?[SoftKeyboardNote.show] as? Bool
            #if os(visionOS)
            if self.showsElsewhere, let anchor = self.presenter.curve?.anchor {
                anchor.keys = self.streamView
                if show ?? !anchor.isFirstResponder {
                    anchor.becomeFirstResponder()
                } else {
                    anchor.resignFirstResponder()
                }
                return
            }
            #endif
            self.streamView.setSoftKeyboardVisible(show ?? !self.streamView.softKeyboardUp)
        })
        #if os(iOS)
        // The scroll inversion changed, and with it whether core takes touchpad contacts.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkTouchpadChanged, object: connection, queue: .main
        ) { [weak self] _ in
            self?.streamView.refreshTouchpad()
        })
        // A monitor plugged in or pulled mid-session takes the picture or hands it back.
        observers.append(NotificationCenter.default.addObserver(
            forName: ExternalDisplay.didChange, object: nil, queue: .main
        ) { [weak self] _ in
            self?.routeVideo()
        })
        #endif
        #if os(visionOS)
        // Several windows can stream at once and none is "in front": input follows the window
        // or curved screen the player last pinched into. Another app window's scene taking it
        // (a host list being typed into) releases this one; this scene's ornaments do not.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIWindow.didBecomeKeyNotification, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, let window = note.object as? UIWindow else { return }
            if window === self.view.window || window === self.presenter.curve?.anchor?.window {
                self.setCaptured(true)
            } else if let scene = window.windowScene, scene !== self.view.window?.windowScene,
                      [.windowApplication, .windowApplicationVolumetric]
                          .contains(scene.session.role) {
                self.setCaptured(false)
            }
        })
        #endif
    }
    #endif

    #if os(tvOS)
    /// tvOS's part of `start`: input capture, and the observers around the TV's display mode.
    private func startTV(_ connection: PunktfunkConnection) {
        // No click-to-capture and no pointer lock here: a session IS the capture, so an attached
        // Bluetooth mouse or keyboard forwards for as long as one runs — once it is trusted.
        let capture = InputCapture(connection: connection)
        capture.start()
        capture.gcMouseForwarding = true
        capture.setForwarding(captureEnabled)
        inputCapture = capture
        // Re-layout when the TV's mode switch ends or the screen mode changes: a dynamic-range-
        // only switch does not re-layout by itself, and layout pushes the screen's EDR headroom
        // to the presenter. The HDR output itself follows the switch's end (`setHDROutput`).
        observers.append(NotificationCenter.default.addObserver(
            forName: .AVDisplayManagerModeSwitchEnd, object: nil, queue: .main
        ) { [weak self] _ in self?.layoutMetalLayer() })
        observers.append(NotificationCenter.default.addObserver(
            forName: UIScreen.modeDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.layoutMetalLayer() })
        // Every transition tvOS makes around the window, with the surface state at that
        // moment: a bundle then shows what the system did when the picture stopped.
        let witnessed: [Notification.Name] = [
            UIApplication.willResignActiveNotification, UIApplication.didBecomeActiveNotification,
            UIApplication.didEnterBackgroundNotification,
            UIApplication.willEnterForegroundNotification,
            UIWindow.didBecomeHiddenNotification, UIWindow.didBecomeVisibleNotification,
            UIWindow.didResignKeyNotification, UIWindow.didBecomeKeyNotification,
            UIScreen.modeDidChangeNotification, .AVDisplayManagerModeSwitchStart,
            .AVDisplayManagerModeSwitchEnd, .AVDisplayManagerModeSwitchSettingsChanged,
            ProcessInfo.thermalStateDidChangeNotification,
        ]
        for name in witnessed {
            observers.append(NotificationCenter.default.addObserver(
                forName: name, object: nil, queue: .main
            ) { [weak self] note in
                guard let self else { return }
                let short = note.name.rawValue
                    .replacingOccurrences(of: "Notification", with: "")
                    .replacingOccurrences(of: "UIApplication", with: "app.")
                    .replacingOccurrences(of: "UIWindow", with: "window.")
                    .replacingOccurrences(of: "UIScreen", with: "screen.")
                    .replacingOccurrences(of: "AVDisplayManager", with: "display.")
                sceneLog.info("scene \(short, privacy: .public) — \(self.surfaceLine(), privacy: .public)")
            })
        }
    }
    #endif

    func stop() {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        #if os(iOS) || os(visionOS)
        setCaptured(false)
        inputCapture?.stop()
        inputCapture = nil
        // Release anything the touch-driven mouse still holds (a mid-drag session end) while
        // onTouchEvent can still deliver the button-up.
        streamView.resetTouchInput()
        #if os(iOS)
        streamView.onTouchpadSurface = nil // after reset — the frame lifting every finger rides it
        streamView.onTouchpadPinch = nil
        streamView.touchpadHost = nil
        #endif
        streamView.onTouchEvent = nil
        streamView.onDial = nil
        TouchInputMode.sessionOverride = nil // the ring's mid-stream switch dies with the session
        streamView.onPenBatch = nil // after reset — the pen's leave-range sample rides it
        streamView.penEnabled = false
        streamView.onPointerMoveAbs = nil
        streamView.onPointerButton = nil
        streamView.onScroll = nil
        streamView.currentHostMode = nil
        matchFollower = nil
        #endif
        #if os(visionOS)
        fittedScene?.requestGeometryUpdate(.Vision(resizingRestrictions: .freeform))
        fittedScene = nil
        #endif
        #if os(iOS)
        monitorLossWork?.cancel()
        monitorLossWork = nil
        if onExternal {
            ExternalDisplay.shared.hide(externalVideo) // the monitor mirrors the phone again
            onExternal = false
        }
        if #available(iOS 27, *), let accessory = monitorAccessory as? UISceneAccessoryRegistration {
            unregisterSceneAccessory(accessory)
        }
        monitorAccessory = nil
        #endif
        #if os(tvOS)
        inputCapture?.stop()
        inputCapture = nil
        // Return the TV to the user's preferred mode — the home screen must not stay in the
        // session's HDR10/refresh mode.
        withdrawDisplayRequest()
        frameHDR = false
        #endif
        presenter.stop()
        lastDecodedContentSize = nil // the next session re-derives it from its first frame
        connection = nil
    }

    public override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        layoutMetalLayer()
        #if os(iOS) || os(visionOS)
        // The curve's screen is not the window's size: Match window sits it out.
        if !onExternal, !showsElsewhere { noteMatchWindowSize() }
        #if os(iOS)
        noteScreen()
        #endif
        // The window back at screen size (a windowed scene re-maximised) can hold the lock again.
        stepPointerLock(.layout(fills: windowFillsScreen))
        #endif
        #if os(tvOS)
        applyDisplayCriteriaIfNeeded()
        #endif
    }

    #if os(tvOS)
    /// Withdraw the session's display request and stop treating the output as HDR. A switch-end
    /// hop still queued for the withdrawn request sees a newer `hdrRequest` and does nothing.
    private func withdrawDisplayRequest() {
        sessionDisplayManager?.preferredDisplayCriteria = nil
        sessionDisplayManager = nil
        displayModeObservation = nil
        hdrRequest += 1
        presenter.setHDROutput(false)
    }

    /// Ask the TV for a display mode matching the session — HDR10 at the stream's refresh rate —
    /// via AVDisplayManager, the tvOS mechanism custom renderers use for HDR output (AVFoundation
    /// playback layers do this implicitly). Honored only when the user allows matching (tvOS
    /// Settings → Video and Audio → Match Content). The presenter passes PQ through once the
    /// switch ends (`setHDROutput`) or the screen reports headroom, and tone-maps in the shader
    /// otherwise.
    /// Requested while the decoded frames are HDR and withdrawn when they turn SDR, so a session
    /// can apply it more than once; the stop() teardown clears it.
    ///
    /// ⚠️ Keyed on the decoded frames being HDR (`frameHDR`), not the Welcome or the setting alone.
    /// The criteria hardcode BT.2020 PQ, so SDR frames would drive the TV into an HDR10 mode they
    /// do not match. Frames that turn SDR hand the TV back its own mode.
    private func applyDisplayCriteriaIfNeeded() {
        guard let manager = view.window?.avDisplayManager, let connection else { return }
        guard frameHDR, connection.settings.hdrEnabled else {
            if sessionDisplayManager != nil { withdrawDisplayRequest() }
            return
        }
        guard manager.preferredDisplayCriteria == nil else { return }
        let mode = connection.currentMode()
        guard mode.width > 0, mode.height > 0, mode.refreshHz > 0 else { return }
        // A synthetic HEVC format description of what the stream decodes to: BT.2020 PQ at 10
        // bits WITH an HDR10 grade. tvOS reads the range off the static metadata; tagged PQ
        // alone counts as SDR. A 1000-nit BT.2020 grade stands in until the host's own arrives
        // — the mode is the same.
        let grade = PunktfunkConnection.HdrMeta(
            primariesX: [8500, 6550, 35400], primariesY: [39850, 2300, 14600],
            whitePointX: 15635, whitePointY: 16450,
            maxMasteringLuminance: 10_000_000, minMasteringLuminance: 1, maxCLL: 1000, maxFALL: 400)
        let ext: [CFString: Any] = [
            kCMFormatDescriptionExtension_ColorPrimaries:
                kCMFormatDescriptionColorPrimaries_ITU_R_2020,
            kCMFormatDescriptionExtension_TransferFunction:
                kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ,
            kCMFormatDescriptionExtension_YCbCrMatrix:
                kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
            kCMFormatDescriptionExtension_BitsPerComponent: 10,
            kCMFormatDescriptionExtension_MasteringDisplayColorVolume:
                grade.masteringDisplayColorVolume() as CFData,
            kCMFormatDescriptionExtension_ContentLightLevelInfo:
                grade.contentLightLevelInfo() as CFData,
        ]
        var desc: CMFormatDescription?
        CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, codecType: kCMVideoCodecType_HEVC,
            width: Int32(mode.width), height: Int32(mode.height),
            extensions: ext as CFDictionary, formatDescriptionOut: &desc)
        guard let desc else { return }
        // The stream's exact rate, so a TV asked for 59.94 Hz is not switched to 60.
        let exact = connection.currentRefreshMilliHz()
        manager.preferredDisplayCriteria = AVDisplayCriteria(
            refreshRate: exact > 0 ? Float(exact) / 1000 : Float(mode.refreshHz),
            formatDescription: desc)
        sessionDisplayManager = manager
        // The switch runs for a second or two; its end is taken as the output being in our mode.
        // The hop lands only while this request stands: frames that turned SDR meanwhile, or a
        // stop, withdrew it and already switched HDR output off.
        let request = hdrRequest
        displayModeObservation = manager.observe(\.isDisplayModeSwitchInProgress, options: [.old, .new]) {
            [weak self] _, change in
            guard change.oldValue == true, change.newValue == false else { return }
            DispatchQueue.main.async { [weak self] in
                guard let self, self.hdrRequest == request else { return }
                self.presenter.setHDROutput(true)
            }
        }
        // PUNKTFUNK_PRESENT_DEBUG: whether the box takes the request, now and as the switch lands.
        if presentDebug {
            for delay in [0.0, 1.0, 2.0, 3.0, 5.0] {
                DispatchQueue.main.asyncAfter(deadline: .now() + delay) { [weak manager, weak self] in
                    let screen = self?.view.window?.screen ?? UIScreen.main
                    print(String(
                        format: "pf-tvdisplay t+%.2f matching=%d switching=%d criteria=%d headroom=%.2f "
                            + "potential=%.2f gamut=%d mode=%dx%d maxFps=%d",
                        delay, manager?.isDisplayCriteriaMatchingEnabled == true ? 1 : 0,
                        manager?.isDisplayModeSwitchInProgress == true ? 1 : 0,
                        manager?.preferredDisplayCriteria != nil ? 1 : 0,
                        screen.currentEDRHeadroom, screen.potentialEDRHeadroom,
                        screen.traitCollection.displayGamut.rawValue,
                        Int(screen.currentMode?.size.width ?? 0), Int(screen.currentMode?.size.height ?? 0),
                        screen.maximumFramesPerSecond))
                }
            }
        }
    }
    #endif

    /// The scale to render the metal drawable at: the panel's pixels per point, not the logical
    /// `displayScale`. Under Display Zoom (an iPad's "More Space", a phone's "Larger Text") the
    /// two differ, and a drawable at the logical scale is resampled twice — up by the shader,
    /// down by the display. The stream is negotiated from `nativeBounds`, so the panel's scale
    /// makes decoded, drawable and panel 1:1. Falls back to the trait, then the main screen.
    private var renderScale: CGFloat {
        let s = traitCollection.displayScale
        #if os(visionOS)
        return s > 0 ? s : 2 // visionOS windows render at 2x
        #else
        let native = (view.window?.windowScene?.screen ?? NativeDisplay.screen).nativeScale
        if native > 0 { return native }
        return s > 0 ? s : UIScreen.main.scale
        #endif
    }

    /// Aspect-fit the stage-2 metal sublayer to the surface showing the picture — this view, or
    /// an attached monitor — at that surface's render scale (see SessionPresenter.layout).
    private func layoutMetalLayer() {
        videoLayer.videoGravity = SessionPresenter.gravity(VideoFit(name: connection?.settings.videoFit))
        #if !os(visionOS) // visionOS exposes no display, so its panel stays unknown
        // UIKit exposes only the ceiling; the range and step stay unknown (min = max).
        let maxHz = Double((streamView.window?.screen ?? UIScreen.main).maximumFramesPerSecond)
        presenter.setPanel(PanelInfo(minHz: maxHz, maxHz: maxHz))
        #endif
        #if os(iOS)
        if onExternal {
            let scale = externalVideo.traitCollection.displayScale
            presenter.layout(in: externalVideo.bounds, contentsScale: scale > 0 ? scale : 1)
            return
        }
        #endif
        presenter.layout(in: streamView.bounds, contentsScale: renderScale)
    }

    /// The window, scene and display around the picture, for the presenter's wedge line: a
    /// hidden window, an inactive scene, a modal over the view or a display mode switch in
    /// flight each name the level that stopped taking frames. Main thread.
    private func surfaceLine() -> String {
        let window = view.window
        let app = UIApplication.shared
        var line = "app=\(app.applicationState.rawValue) idleTimerOff=\(app.isIdleTimerDisabled) "
            + "window=\(window.map { "key=\($0.isKeyWindow) hidden=\($0.isHidden) level=\($0.windowLevel.rawValue)" } ?? "none") "
            + "scene=\(window?.windowScene?.activationState.rawValue ?? -9) "
            + "view=\(view.bounds.size) hidden=\(view.isHidden) alpha=\(view.alpha) attached=\(view.superview != nil) "
            + "modal=\(presentedViewController != nil || window?.rootViewController?.presentedViewController != nil) "
            + "plane=\(streamView.displayLayer.status.rawValue) "
            + "thermal=\(ProcessInfo.processInfo.thermalState.rawValue)"
        #if !os(visionOS)
        if let screen = window?.screen {
            line += " screen=\(screen.bounds.size)@\(screen.maximumFramesPerSecond)"
        }
        #endif
        #if os(tvOS)
        if let manager = window?.avDisplayManager {
            line += " modeSwitch=\(manager.isDisplayModeSwitchInProgress) "
                + "matching=\(manager.isDisplayCriteriaMatchingEnabled) "
                + "criteria=\(manager.preferredDisplayCriteria != nil)"
        }
        line += " headroom=\(UIScreen.main.currentEDRHeadroom)"
        #endif
        return line
    }

    /// The display layer the picture presents into: the monitor's while one shows it.
    private var videoLayer: AVSampleBufferDisplayLayer {
        #if os(iOS)
        if onExternal { return externalVideo.displayLayer }
        #endif
        return streamView.displayLayer
    }

    /// The decoded frames turned HDR or SDR. tvOS follows them with the display mode. Main thread.
    private func noteFrameHDR(_ hdr: Bool) {
        #if os(tvOS)
        frameHDR = hdr
        applyDisplayCriteriaIfNeeded()
        #endif
    }

    /// A new decoded size landed (a scene/mode resize's new IDR, or the first frame): push it to the
    /// presenter's aspect-fit and re-layout NOW. A resize-END triggers no `viewDidLayoutSubviews`, so
    /// this is what makes the metal sublayer track the new content aspect instead of stretching the
    /// new frame into the pre-resize box. On visionOS the window then takes the picture's shape.
    /// Deduped so a same-size repeat is a no-op. Main thread.
    private func noteDecodedContentSize(width: Int, height: Int) {
        let size = CGSize(width: width, height: height)
        guard size.width > 0, size.height > 0, size != lastDecodedContentSize else { return }
        lastDecodedContentSize = size
        presenter.setContentSize(size)
        layoutMetalLayer()
        #if os(visionOS)
        fitWindowToPicture(size)
        #endif
    }

    /// The picture shows in the curve's volume, not in this view's window.
    private var showsElsewhere: Bool {
        #if os(visionOS)
        return presenter.curve?.curved == true
        #else
        return false
        #endif
    }

    #if os(visionOS)
    /// Draw the picture on a curved screen, or back here with nil. Main thread.
    func setCurve(_ target: CurveTarget?) {
        guard target !== presenter.curve else { return }
        target?.pointer = self
        presenter.setCurve(target)
        if connection != nil { layoutMetalLayer() }
    }

    /// The window takes the picture's shape: a fixed mode need not match the window, and a bar is
    /// picture no one sees. Match window drives the mode from the window instead, so it resizes
    /// free. A new shape keeps the window's width.
    private func fitWindowToPicture(_ picture: CGSize) {
        guard let window = view.window, let scene = window.windowScene else { return }
        let prefs = UIWindowScene.GeometryPreferences.Vision()
        if connection?.settings.matchWindow == true {
            prefs.resizingRestrictions = .freeform
        } else {
            let width = window.bounds.width
            let height = (width * picture.height / picture.width).rounded()
            if abs(window.bounds.height - height) > 1 { prefs.size = CGSize(width: width, height: height) }
            prefs.resizingRestrictions = .uniform
        }
        scene.requestGeometryUpdate(prefs)
        fittedScene = scene
    }
    #endif

    #if os(iOS) || os(visionOS)
    #if os(iOS)
    /// Follow a monitor plugged in or pulled mid-session: move the picture onto it or back to the
    /// phone at once. A plugged monitor's mode is asked for right away. A pulled one may only be
    /// changing its mode, so its mode stays for `NativeDisplay.lossHold` before `takePhoneMode`.
    /// Main thread.
    private func routeVideo() {
        guard connection != nil else { return }
        let external = ExternalDisplay.shared.screen != nil
        guard external != onExternal else { return }
        onExternal = external
        if external {
            ExternalDisplay.shared.show(externalVideo)
        } else {
            ExternalDisplay.shared.hide(externalVideo)
        }
        presenter.move(to: videoLayer)
        layoutMetalLayer()
        // Match window sits out a monitor and the hold after one: a size request queued before
        // either must not fire.
        matchFollower?.setEnabled(false)
        monitorLossWork?.cancel()
        monitorLossWork = nil
        guard !external else { return requestSurfaceMode() }
        let work = DispatchWorkItem { [weak self] in self?.takePhoneMode() }
        monitorLossWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + NativeDisplay.lossHold, execute: work)
    }

    /// The monitor stayed away: Match window owns the mode again, else the phone's Native does.
    private func takePhoneMode() {
        guard let connection, !onExternal else { return }
        let follows = connection.settings.matchWindow
        matchFollower?.setEnabled(follows)
        if follows { noteMatchWindowSize() } else { requestSurfaceMode() }
    }

    /// The native mode of what the picture fills: the screen this view is on, or the view's own
    /// pixels while it holds only the upper part of its window (the upright half of a half-open
    /// fold).
    private var nativeMode: (width: Int, height: Int, hz: Int) {
        let mode = NativeDisplay.mode(on: view.window?.windowScene?.screen ?? NativeDisplay.screen)
        guard let window = view.window, streamView.bounds.height < window.bounds.height - 1
        else { return mode }
        let scale = renderScale
        return (
            Int((streamView.bounds.width * scale).rounded()),
            Int((streamView.bounds.height * scale).rounded()), mode.hz)
    }

    /// The picture's native mode changed: it moved to another screen (a fold or unfold), or onto
    /// or off the upright half of a half-open fold. A session streaming Native takes the new
    /// mode. 0.4 s lets the hinge settle, so a flap costs the host no rebuild. Match window
    /// follows the view's size on its own.
    private func noteScreen() {
        let native = nativeMode
        defer { lastNativeMode = native }
        guard let last = lastNativeMode, last != native else { return }
        screenModeWork?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self, !self.onExternal, self.connection?.settings.matchWindow == false
            else { return }
            self.requestSurfaceMode()
        }
        screenModeWork = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: work)
    }
    #endif

    /// Match-window (C3): feed the follower the view's physical-pixel size (points × scale).
    private func noteMatchWindowSize() {
        let b = streamView.bounds
        guard b.width > 0, b.height > 0 else { return }
        let scale = renderScale
        matchFollower?.noteSize(
            widthPx: Int((b.width * scale).rounded()),
            heightPx: Int((b.height * scale).rounded()))
    }

    /// Ask the host for the session's mode where the picture is, with its view above 1x: Native is
    /// the monitor's pixels at its top refresh, or the phone's back on the phone. Skipped when it
    /// already streams that mode at that view.
    private func requestSurfaceMode() {
        guard let connection else { return }
        let settings = connection.settings
        #if os(iOS)
        let native = (onExternal ? ExternalDisplay.nativeMode : nil) ?? nativeMode
        #else
        let native = NativeDisplay.mode
        #endif
        let target = settings.streamMode(native: native)
        let view = settings.streamView(native: native, mode: target)
        let live = connection.currentMode()
        let liveView = surfaceView ?? connection.connectView
        guard live.width != target.width || live.height != target.height
            || live.refreshHz != target.hz
            || MatchWindow.Ask((0, 0), view: view) != MatchWindow.Ask((0, 0), view: liveView)
        else { return }
        connection.requestMode(
            width: target.width, height: target.height, refreshHz: target.hz, view: view)
        surfaceView = .some(view)
    }

    /// `fromClick` marks a click-driven engage (the released-state pointer click that re-captures):
    /// that click's press/release are suppressed toward the host — it's the local engage gesture,
    /// not a host click — exactly as macOS's `engageCapture(fromClick:)` does. Keyboard-driven
    /// engages (⌘⎋) pass false so a normal click still reaches the host.
    private func setCaptured(_ on: Bool, fromClick: Bool = false) {
        if on {
            // `connection != nil` is the session-active gate (presenter internals are opaque here).
            guard captureEnabled, !captured, connection != nil else { return }
            inputCapture?.reclaim() // another window's stream may hold the keyboard and mouse
            inputCapture?.setForwarding(true, suppressClick: fromClick)
            captured = true
            // Claim the responder chain for as long as we own the keyboard — `pressesBegan` has to
            // be delivered to us before it can keep Escape away from the system.
            anchorKeyResponder()
        } else {
            guard captured else { return }
            inputCapture?.setForwarding(false)
            captured = false
            #if os(iOS)
            streamView.endTouchpadPinch()
            #endif
            // Hand the chain back: released means Escape is the system's again, and staying first
            // responder for a stream that no longer owns input would sit in front of SwiftUI focus.
            if isFirstResponder { resignFirstResponder() }
            streamView.holdsKeyFocus = false
        }
        setNeedsUpdateOfPrefersPointerLocked()
        updatePointerLockChain() // (re)anchor the SwiftUI ancestors so the lock actually resolves
        syncPointerLock() // resolve cursor + GCMouse/absolute routing for the current state
        let onCaptureChange = onCaptureChange
        let captured = captured
        DispatchQueue.main.async { [weak self] in
            onCaptureChange?(captured)
            // The lock request is async — the resolved state can land a runloop later, and the
            // initial grant may precede our didChange observer, so re-resolve the routing here.
            self?.syncPointerLock()
        }
    }

    /// Resolve the mouse routing for the scene's CURRENT pointer-lock state: GCMouse (relative
    /// deltas + buttons) while locked, the absolute UIKit pointer path while not, and the
    /// hidden-vs-visible local cursor to match. Idempotent — safe to call on every lock-state
    /// change and capture toggle. Main queue.
    private func syncPointerLock() {
        let locked = pointerLockEngaged() == true
        let useGCMouse = captured && locked
        // Lock dropped (or capture ended) while the GCMouse path held a button down: once
        // gcMouseForwarding flips false its release handler is gated off, so flush any held
        // mouse button here before the switch — otherwise it sticks down on the host.
        if inputCapture?.gcMouseForwarding == true, !useGCMouse {
            inputCapture?.releaseMouseButtons()
        }
        inputCapture?.gcMouseForwarding = useGCMouse
        pointerInteraction?.invalidate() // re-resolve the hidden/visible cursor for the state
        if iosInputDebug {
            iosInputLog.debug(
                """
                pointer lock isLocked=\(locked, privacy: .public) \
                captured=\(self.captured, privacy: .public) \
                wanted=\(self.wantsPointerLock, privacy: .public) \
                sceneQualifies=\(self.sceneCanHoldPointerLock, privacy: .public) \
                chainReachesScene=\(PointerLockChain.reachesScene(from: self), privacy: .public)
                """)
        }
    }

    /// The window at its screen's size. A windowed scene — including the one the title strip's
    /// double-click leaves behind — is refused the lock outright.
    private var windowFillsScreen: Bool {
        #if os(visionOS)
        return false // a visionOS window never fills a screen
        #else
        guard let window = view.window, let scene = window.windowScene else { return false }
        return window.bounds.size == scene.screen.bounds.size
        #endif
    }

    /// SpringBoard grants the lock only to a frontmost scene that fills its screen.
    private var sceneCanHoldPointerLock: Bool {
        view.window?.windowScene?.activationState == .foregroundActive && windowFillsScreen
    }

    /// Step the lock's state with what the scene says now, and do what it returns. Main queue.
    private func stepPointerLock(_ event: PointerLock.Event) {
        let facts = PointerLock.Facts(
            wanted: wantsPointerLock, granted: pointerLockEngaged(),
            sceneCanHold: sceneCanHoldPointerLock)
        switch pointerLock.step(event, facts) {
        case .none:
            break
        case .beginEdge:
            // Re-anchored in case a reparent broke the walk to us.
            updatePointerLockChain()
            setNeedsUpdateOfPrefersPointerLocked()
            DispatchQueue.main.asyncAfter(deadline: .now() + PointerLock.edgeHold) { [weak self] in
                self?.stepPointerLock(.edgeEnded)
            }
        case .endEdge:
            setNeedsUpdateOfPrefersPointerLocked()
        case .reaskNextTurn:
            DispatchQueue.main.async { [weak self] in self?.stepPointerLock(.reask) }
        }
    }
    #endif

    isolated deinit {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        presenter.stop() // invalidate the display link + stop the pipeline if stop() was missed
    }
}

#if os(iOS) || os(visionOS)
extension StreamViewController: UIPointerInteractionDelegate {
    public func pointerInteraction(
        _ interaction: UIPointerInteraction, styleFor region: UIPointerRegion
    ) -> UIPointerStyle? {
        // Hide the local cursor only while the scene is actually pointer-LOCKED — the host draws
        // its own from GCMouse deltas. Unlocked, it stays visible so the user can aim; the pointer
        // forwards as an absolute position and both cursors track together.
        captured && pointerLockEngaged() == true ? .hidden() : nil
    }
}
#endif

/// The layer-backed video surface + touch source. Touches are mapped through the
/// aspect-fit letterbox into host-mode pixels (surface == host mode, so the host-side
/// rescale is the identity); touches outside the video area are clamped onto its edge.
final class StreamLayerUIView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    #if os(iOS) || os(visionOS)
    /// The stream owns the keyboard (`anchorKeyResponder`). UIKit sends key presses to the
    /// focused item, so the view is one then.
    var holdsKeyFocus = false {
        didSet { if !holdsKeyFocus { dropKeyChain() } }
    }
    /// Activation is the click: the controller captures.
    var onActivate: (() -> Void)?
    /// Full Keyboard Access sent a request for the view: the controller records the proof.
    var onFullKeyboardAccess: (() -> Void)?
    /// First responder for Full Keyboard Access's Input mode, not for the soft keyboard.
    private var inInputMode = false

    /// Under Full Keyboard Access the view stays focusable when released: moving onto it and
    /// activating is the way back in.
    override var canBecomeFocused: Bool { holdsKeyFocus || FullKeyboardAccess.proven }

    /// Tab or an arrow does not move focus off a view that owns the keyboard. An update with
    /// no heading still does, so an alert takes focus. Full Keyboard Access moves it freely:
    /// Ctrl-Tab is its way out of Input mode.
    override func shouldUpdateFocus(in context: UIFocusUpdateContext) -> Bool {
        FullKeyboardAccess.proven || !holdsKeyFocus || context.nextFocusedItem === self
            || context.focusHeading.isEmpty
    }

    override func didUpdateFocus(
        in context: UIFocusUpdateContext, with coordinator: UIFocusAnimationCoordinator
    ) {
        super.didUpdateFocus(in: context, with: coordinator)
        if context.nextFocusedItem === self {
            takeKeyChain()
        } else if context.previouslyFocusedItem === self {
            dropKeyChain()
        }
    }

    /// Under Full Keyboard Access the view is first responder while it owns the keyboard, as a
    /// text field is while edited: a text input in that place starts its Input mode, where
    /// every key reaches the app, and focus follows a change of first responder. False where
    /// that does not apply: unproven, or no hardware keyboard, where the soft one would rise.
    @discardableResult
    func takeKeyChain() -> Bool {
        guard FullKeyboardAccess.proven, holdsKeyFocus, GCKeyboard.coalesced != nil
        else { return false }
        // Asked before the view is in a window, UIKit takes it up once it is: the screen's
        // first focus pick then lands here, not on an overlay button.
        if !isFirstResponder { becomeFirstResponder() }
        inInputMode = true
        return true
    }

    /// Focus left the view, or the stream let the keyboard go: Input mode ends with the chain.
    func dropKeyChain() {
        guard inInputMode else { return }
        inInputMode = false
        if isFirstResponder { resignFirstResponder() }
    }

    /// Full Keyboard Access reads these before it acts on the view, and no API says it is on.
    override var accessibilityTraits: UIAccessibilityTraits {
        get {
            noteFullKeyboardAccess()
            return super.accessibilityTraits
        }
        set { super.accessibilityTraits = newValue }
    }

    override var isAccessibilityElement: Bool {
        get {
            noteFullKeyboardAccess()
            return super.isAccessibilityElement
        }
        set { super.isAccessibilityElement = newValue }
    }

    /// Activation is the click: Space under Full Keyboard Access captures a released stream.
    override func accessibilityActivate() -> Bool {
        noteFullKeyboardAccess()
        guard let onActivate else { return super.accessibilityActivate() }
        onActivate()
        return true
    }

    /// The request being answered comes from Full Keyboard Access (iOS 18): tell the
    /// controller, outside the accessibility callback.
    private func noteFullKeyboardAccess() {
        guard #available(iOS 18.0, *), !FullKeyboardAccess.proven,
              AccessibilityRequest.current?.technology == .fullKeyboardAccess
        else { return }
        DispatchQueue.main.async { [weak self] in self?.onFullKeyboardAccess?() }
    }
    #endif
    var displayLayer: AVSampleBufferDisplayLayer {
        // swiftlint:disable:next force_cast
        layer as! AVSampleBufferDisplayLayer
    }

    #if os(iOS) || os(visionOS)
    /// A position already mapped into host-mode pixels, with the surface dims the host
    /// rescales against (== host mode, so its rescale is the identity).
    struct HostPoint { let x: Int32; let y: Int32; let w: UInt32; let h: UInt32 }

    /// Reads the LIVE negotiated mode in pixels (the touch/pointer coordinate space).
    var currentHostMode: (() -> CGSize)?
    /// The live session's settings, set when it starts. Scroll inversion is not seeded here:
    /// the connection's outbound seam applies it (`setInvertScroll` at connect).
    var settings = EffectiveSettings()
    /// Direct fingers / Pencil → wire events: real touches in passthrough mode, or the
    /// touch-driven mouse events (`TouchMouse`) in the trackpad/pointer modes.
    var onTouchEvent: ((PunktfunkInputEvent) -> Void)?
    /// Apple Pencil → state-full pen sample batches (the stylus plane). Active only while
    /// `penEnabled`; without it the Pencil stays on the finger path.
    var onPenBatch: (([PunktfunkPenSample]) -> Void)?
    /// The host advertised `HOST_CAP_PEN`, so Pencil input splits out of the finger path onto
    /// the pen plane — independent of the touch-input mode (drawing must not depend on it).
    var penEnabled = false
    /// The host injects wire touch contacts (`HOST_CAP2_TOUCH`). Off, the `touch` model's
    /// fingers drive the trackpad engine instead of vanishing on the host.
    var touchPassthroughEnabled = true
    /// The host types committed text (`HOST_CAP_TEXT_INPUT`), so the soft keyboard sends
    /// characters instead of US key positions.
    var textInputEnabled = false
    /// Indirect pointer (mouse/trackpad with no lock) → absolute cursor moves.
    var onPointerMoveAbs: ((HostPoint) -> Void)?
    /// Indirect-pointer buttons (GameStream ids: 1=left 3=right); `down` = press.
    var onPointerButton: ((_ button: UInt32, _ down: Bool) -> Void)?
    /// Trackpad two-finger / wheel scroll → host scroll deltas in `source`'s unit (points =
    /// DIP for a measured surface; the discrete recognizer's OS-translated distance can't
    /// recover notches, so it reports `Continuous`, not `Wheel`).
    var onScroll: (
        (_ dx: Float, _ dy: Float, _ source: PunktfunkScrollSource,
         _ phase: PunktfunkScrollPhase) -> Void
    )?
    /// The two-finger twist turning the quick-action ring, or the passthrough edge pull.
    var onDial: ((DialEvent) -> Void)?
    #if os(iOS)
    /// Reads the host's touchpad now: it presents one, and its OS clicks on taps.
    var touchpadHost: (() -> (presents: Bool, taps: Bool))?
    /// No trust prompt is up: touch may reach the host.
    var touchTrusted = false
    /// Every finger down as touchpad contacts: whether the twist keeps them, whether core takes
    /// taps back, the view in tenths of a millimetre, the capture time in µs. Answers forwarded
    /// or not, nil when core refused the call.
    var onTouchpadSurface: (
        (_ touches: [PunktfunkSurfaceTouch], _ keep: Bool, _ retractTaps: Bool,
         _ size: (width: UInt16, height: UInt16), _ timeUs: UInt64) -> Bool?
    )?
    /// The trackpad's pinch: a `PUNKTFUNK_PINCH_*` phase, scale and rotation since it began.
    /// False when core refused it.
    var onTouchpadPinch: ((_ phase: UInt8, _ scale: Float, _ rotation: Float) -> Bool)?
    /// Frame mode and the host's touchpad it follows (`refreshTouchpad`, `syncTouchpadFrames`).
    private var frames = FrameMode()
    /// The trackpad pinch under way, nil when none, and the two recognizers that make it.
    private var trackpadPinch: (scale: CGFloat, rotation: CGFloat)?
    private var transformRecognizers: [UIGestureRecognizer] = []
    #endif

    /// Wire touch ids per active direct UITouch; ids are reused after the touch ends.
    private var touchIDs: [ObjectIdentifier: UInt32] = [:]
    /// Live fingers that landed on a side bezel, for the passthrough dial opener
    /// ([`EdgeDial`]). Only in the `touch` model — every other model has the twist.
    private var edgeTracks: [ObjectIdentifier: EdgeTrack] = [:]
    /// GameStream button held per active indirect-pointer touch (one click/drag session);
    /// released when that touch ends.
    private var pointerButtons: [ObjectIdentifier: UInt32] = [:]
    /// Touch-driven mouse for the trackpad/pointer `TouchInputMode`s (see TouchMouse.swift). In
    /// frame mode the host makes motion and scroll from the contacts, so only what
    /// `FrameMode.passes` allows goes out.
    private lazy var touchMouse: TouchMouse = {
        let mouse = TouchMouse()
        mouse.send = { [weak self] event in
            guard let self else { return }
            #if os(iOS)
            guard self.frames.passes(event) else { return }
            #endif
            self.onTouchEvent?(event)
        }
        mouse.hostPoint = { [weak self] point in self?.hostPoint(from: point) }
        mouse.onKeyboardGesture = { [weak self] show in self?.setSoftKeyboardVisible(show) }
        mouse.onDial = { [weak self] event in self?.onDial?(event) }
        return mouse
    }()
    /// The `off` model: the same gestures (twist, keyboard swipe, stats tap) with no `send`,
    /// so a miss beside the on-screen pad never moves the host cursor.
    private lazy var mutedMouse: TouchMouse = {
        let mouse = TouchMouse()
        mouse.onKeyboardGesture = { [weak self] show in self?.setSoftKeyboardVisible(show) }
        mouse.onDial = { [weak self] event in self?.onDial?(event) }
        return mouse
    }()
    #if os(visionOS)
    /// The curved screen's pinch in progress, latched to the route it started in.
    var curve: CurvePinch?
    #endif
    /// The finger route latched at gesture start — a mid-gesture switch (the ring's
    /// `TouchInputMode.sessionOverride`) applies to the NEXT touch, so one gesture never splits
    /// across input models.
    private var fingerRoute: TouchInputMode?
    /// The Apple Pencil pipeline (contacts + hover + squeeze/tap → pen samples).
    private lazy var pencil: PencilStream = {
        let stream = PencilStream()
        stream.send = { [weak self] batch in self?.onPenBatch?(batch) }
        stream.videoNorm = { [weak self] point in
            guard let h = self?.hostPoint(from: point) else { return nil }
            return (Float(h.x) / Float(max(h.w - 1, 1)), Float(h.y) / Float(max(h.h - 1, 1)))
        }
        return stream
    }()

    /// Release anything the touch-driven mouse holds and forget gesture state — session stop.
    /// The touch mouse resets first, while frame mode still filters what it releases.
    func resetTouchInput() {
        touchMouse.reset()
        mutedMouse.reset()
        pencil.reset() // leaves range → the host lifts anything still inked
        #if os(iOS)
        endTouchpadPinch()
        touchTrusted = false
        syncTouchpadFrames() // the lift, while the session's surface sink is still set
        frames.refresh(presents: false, taps: false)
        #endif
        fingerRoute = nil
        setSoftKeyboardVisible(false) // a stream that's gone takes its keyboard with it
    }

    /// The soft keyboard is keyed off first-responder status: the three-finger swipe
    /// (TouchMouse) summons/dismisses it here, and the UIKeyInput conformance below turns
    /// what it types into wire key events. Also the reason `canBecomeFirstResponder` is true
    /// on iOS (tvOS anchors the responder chain on the CONTROLLER instead — see
    /// StreamViewController.viewDidAppear).
    override var canBecomeFirstResponder: Bool { true }

    func setSoftKeyboardVisible(_ visible: Bool) {
        #if os(iOS)
        summonKeyRow(visible)
        // First responder already, beside a hardware keyboard: only the row comes and goes, and
        // the view keeps the key chain it holds for Full Keyboard Access.
        if isFirstResponder, visible || inInputMode {
            reloadInputViews()
            return
        }
        #endif
        if visible {
            becomeFirstResponder()
            #if os(iOS)
            // Refused, the keyboard is not up: the next ask must show it, not hide it.
            if !isFirstResponder { summonKeyRow(false) }
            #endif
        } else if isFirstResponder {
            resignFirstResponder()
        }
    }

    #if os(iOS)
    /// The session behind the row's quick actions (`StreamView.keyRow`).
    var keyRowClient: KeyRowClient? {
        didSet { keyRow?.client = keyRowClient }
    }
    /// The keyboard row, while the player has the keyboard up and the row has buttons.
    private var keyRow: KeyRowView?
    /// The player asked for the keyboard. First responder does not say so: under Full Keyboard
    /// Access the view holds the key chain beside a hardware keyboard with no keyboard asked for.
    private var keyboardSummoned = false

    /// The system docks it on the keyboard it shows, or alone at the screen's bottom while a
    /// hardware keyboard keeps the soft one away.
    override var inputAccessoryView: UIView? { keyRow }

    /// The keyboard is coming: build the row from the session's quick actions. The keyboard is
    /// going: release every key the row holds on the host, then drop the row.
    private func summonKeyRow(_ visible: Bool) {
        keyboardSummoned = visible
        keyRow?.releaseAll()
        keyRow = nil
        let buttons = OverlayConfig.parse(settings.overlayActions).rowButtons
        guard visible, !buttons.isEmpty else { return }
        let row = KeyRowView(buttons: buttons)
        row.client = keyRowClient
        row.sendKey = { [weak self] vk, down in self?.onTouchEvent?(.key(vk, down: down)) }
        row.sendText = { [weak self] text in
            text.unicodeScalars.forEach { self?.onTouchEvent?(.text($0)) }
        }
        row.hide = { [weak self] in self?.setSoftKeyboardVisible(false) }
        keyRow = row
    }

    /// The keyboard can go without `setSoftKeyboardVisible`, by its own hide key: the row goes
    /// with it either way.
    @discardableResult
    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { summonKeyRow(false) }
        return resigned
    }
    #endif

    /// A key the row did not send lifted on the host, from a hardware keyboard
    /// (`InputCapture.onKeyLifted`) or typed on the system one. It was the next key to any
    /// modifier the row holds.
    func hostKeyLifted() {
        #if os(iOS)
        keyRow?.keyLifted()
        #endif
    }

    /// Whether the keyboard the Keyboard action toggles is up.
    var softKeyboardUp: Bool {
        #if os(iOS)
        keyboardSummoned
        #else
        isFirstResponder
        #endif
    }
    #endif

    override init(frame: CGRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        #if os(iOS) || os(visionOS)
        focusEffect = nil // the picture is no control: no halo around it
        isMultipleTouchEnabled = true
        // Button-less mouse/trackpad movement (no lock) arrives as hover, not touches —
        // forward it as absolute cursor moves so the host cursor tracks without a click held.
        // The Pencil hovers on a recognizer of its own, so neither has to guess the device.
        let pointerHover = UIHoverGestureRecognizer(target: self, action: #selector(handleHover))
        let pointerTypes: [UITouch.TouchType] = [.direct, .indirect, .indirectPointer]
        pointerHover.allowedTouchTypes = pointerTypes.map { NSNumber(value: $0.rawValue) }
        addGestureRecognizer(pointerHover)
        let pencilHover = UIHoverGestureRecognizer(
            target: self, action: #selector(handlePencilHover))
        pencilHover.allowedTouchTypes = [NSNumber(value: UITouch.TouchType.pencil.rawValue)]
        #if os(iOS)
        pencilHover.delegate = self // runs beside a pointer that is already hovering
        #endif
        addGestureRecognizer(pencilHover)
        // Trackpad two-finger / wheel scroll → scroll-ONLY pans: allowedTouchTypes = []
        // rejects finger drags (those stay host touches). One recognizer per scroll type so
        // UIKit itself says which device this is: a continuous trackpad delta is a measured
        // DISTANCE the host must travel, a discrete wheel delta is a click count. Asking for
        // `.all` on one recognizer merges them and forces the host to guess.
        for (mask, sel) in [
            (UIScrollTypeMask.continuous, #selector(handlePreciseScroll)),
            (UIScrollTypeMask.discrete, #selector(handleWheelScroll)),
        ] {
            let scrollPan = UIPanGestureRecognizer(target: self, action: sel)
            scrollPan.allowedScrollTypesMask = mask
            scrollPan.allowedTouchTypes = []
            addGestureRecognizer(scrollPan)
        }
        #endif
        #if os(iOS)
        // Pencil squeeze / double-tap → the pen plane's barrel buttons (no-op while
        // `penEnabled` is false — PencilStream ignores interactions out of range).
        let pencilInteraction = UIPencilInteraction()
        pencilInteraction.delegate = pencil
        addInteraction(pencilInteraction)
        // The trackpad's pinch and rotation. The delegate admits transform events alone, so a
        // finger pinch never reaches them, and they never cancel the view's touches.
        let transforms: [UIGestureRecognizer] = [
            UIPinchGestureRecognizer(target: self, action: #selector(handleTrackpadTransform)),
            UIRotationGestureRecognizer(target: self, action: #selector(handleTrackpadTransform)),
        ]
        for recognizer in transforms {
            recognizer.delegate = self
            recognizer.cancelsTouchesInView = false
            addGestureRecognizer(recognizer)
        }
        transformRecognizers = transforms
        #endif
        backgroundColor = .black
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    #if os(iOS) || os(visionOS)
    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .down)
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .move)
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .up)
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        route(touches, event: event, kind: .cancel)
    }

    private enum TouchKind { case down, move, up, cancel }

    /// Split a touch batch by kind: an INDIRECT POINTER (mouse/trackpad with no lock) drives
    /// the host cursor as an absolute mouse; a Pencil goes to the pen plane when the host
    /// supports it; everything else (direct finger — and the Pencil toward a pen-less host)
    /// is a host touch. Mixed batches are possible, so partition rather than branch on the
    /// first touch.
    private func route(_ touches: Set<UITouch>, event: UIEvent?, kind: TouchKind) {
        var fingers: Set<UITouch> = []
        var pencilTouches: Set<UITouch> = []
        for touch in touches {
            if touch.type == .indirectPointer {
                handleIndirectPointer(touch, event: event, kind: kind)
            } else if penEnabled, touch.type == .pencil {
                pencilTouches.insert(touch)
            } else {
                fingers.insert(touch)
            }
        }
        if !pencilTouches.isEmpty {
            let phase: PencilStream.Phase =
                switch kind {
                case .down: .down
                case .move: .move
                case .up: .up
                case .cancel: .cancel
                }
            pencil.touches(pencilTouches, event: event, phase: phase, in: self)
        }
        if !fingers.isEmpty { forwardFingers(fingers, kind: kind) }
    }

    /// The fingers' touch-input model: the gesture's latch, else the setting, with `touch`
    /// falling back to the trackpad engine toward a host that injects no touches.
    private var fingerMode: TouchInputMode {
        let mode = fingerRoute ?? TouchInputMode.current(settings)
        return mode == .touch && !touchPassthroughEnabled ? .trackpad : mode
    }

    /// Route direct fingers by the touch-input model, latched for the whole gesture:
    /// passthrough → real wire touches; trackpad/pointer/off → a TouchMouse gesture engine. In
    /// frame mode every event also sends the fingers down as contacts, after the engine.
    private func forwardFingers(_ touches: Set<UITouch>, kind: TouchKind) {
        #if os(iOS)
        if fingerRoute == nil { syncTouchpadFrames() } // a mode switch applies from a gesture's start
        #endif
        let mode = fingerMode
        fingerRoute = mode
        switch mode {
        case .touch:
            // A cancellation lifts the wire touch like a normal up — the host just sees the
            // contact end.
            forwardTouches(touches, kind: kind == .cancel ? .up : kind)
            // …then read the same fingers for the edge pull, which is this model's only way to
            // the dial. Forwarding first is deliberate: a pull that never completes must not
            // have cost the host a contact.
            trackEdgePull(touches, kind: kind)
        case .trackpad, .pointer, .off:
            let mouse = mode == .off ? mutedMouse : touchMouse
            switch kind {
            case .down: mouse.began(touches, in: self, trackpad: mode != .pointer)
            case .move: mouse.moved(touches, in: self)
            case .up: mouse.ended(touches, in: self)
            case .cancel:
                #if os(iOS)
                // A cancelled touch is no tap on the host either: core takes the gesture back.
                if frames.on { sendTouchpadFrame(at: touches, keep: true) }
                #endif
                mouse.cancelled(touches)
            }
            #if os(iOS)
            if frames.on { sendTouchpadFrame(at: touches, keep: touchMouse.holdsFingers) }
            #endif
        }
        if touchIDs.isEmpty, touchMouse.isIdle, mutedMouse.isIdle { fingerRoute = nil }
    }

    #if os(iOS)
    /// Reads the host's touchpad again (session start, each change of the scroll inversion) and
    /// follows it. Gone, a pinch under way is forgotten: core lifted what was down.
    func refreshTouchpad() {
        let host = touchpadHost?() ?? (presents: false, taps: false)
        frames.refresh(presents: host.presents, taps: host.taps)
        if !host.presents { trackpadPinch = nil }
        syncTouchpadFrames()
    }

    /// Frame mode: Trackpad mode toward a touchpad host, touch trusted, the view in a window. An
    /// edge sends one frame with no touches, the announce or the lift, while the host has a pad.
    func syncTouchpadFrames() {
        if frames.sync(trusted: touchTrusted, inWindow: window != nil, mode: fingerMode) {
            surface([], keep: false, at: ProcessInfo.processInfo.systemUptime)
        }
    }

    /// Every finger the touch mouse holds, as fractions of the view, at the batch's capture time.
    private func sendTouchpadFrame(at touches: Set<UITouch>, keep: Bool) {
        let size = bounds.size
        guard size.width > 0, size.height > 0 else { return }
        let contacts = touchMouse.fingers.map { key, point in
            TouchpadContacts.touch(id: UInt64(UInt(bitPattern: key)), at: point, in: size)
        }
        let time = touches.map(\.timestamp).max() ?? ProcessInfo.processInfo.systemUptime
        surface(contacts, keep: keep, at: time)
    }

    /// One surface update. A refused call hands the fingers and the trackpad back to their
    /// ordinary paths until `refreshTouchpad`, so no finger stays muted while core takes nothing.
    private func surface(_ touches: [PunktfunkSurfaceTouch], keep: Bool, at time: TimeInterval) {
        let answer = onTouchpadSurface?(
            touches, keep, !frames.touchpad.taps, touchpadSize, TouchpadContacts.micros(time))
        if answer == nil { dropTouchpad() }
    }

    private func dropTouchpad() {
        frames.refused()
        trackpadPinch = nil
    }

    /// The view in tenths of a millimetre, at its idiom's screen density.
    private var touchpadSize: (width: UInt16, height: UInt16) {
        let ppi = TouchpadContacts.screenPointsPerInch(
            pad: UIDevice.current.userInterfaceIdiom == .pad)
        return (
            TouchpadContacts.dmm(bounds.width, pointsPerInch: ppi),
            TouchpadContacts.dmm(bounds.height, pointsPerInch: ppi))
    }

    override func didMoveToWindow() {
        super.didMoveToWindow()
        syncTouchpadFrames() // a view leaves nothing down on the host's pad
    }

    /// The trackpad's pinch and rotation as one pinch on the host's touchpad, each cumulative
    /// since it began. It ends when both recognizers have. A refusal drops the touchpad.
    @objc private func handleTrackpadTransform(_ g: UIGestureRecognizer) {
        guard frames.touchpad.presents else { return }
        let live = { (r: UIGestureRecognizer) in r.state == .began || r.state == .changed }
        var now = trackpadPinch ?? (scale: 1, rotation: 0)
        if live(g), let pinch = g as? UIPinchGestureRecognizer { now.scale = pinch.scale }
        if live(g), let turn = g as? UIRotationGestureRecognizer { now.rotation = turn.rotation }
        let phase: UInt8
        if transformRecognizers.contains(where: live) {
            phase = UInt8(trackpadPinch == nil ? PUNKTFUNK_PINCH_BEGIN : PUNKTFUNK_PINCH_UPDATE)
            trackpadPinch = now
        } else {
            guard trackpadPinch != nil else { return }
            phase = UInt8(PUNKTFUNK_PINCH_END)
            trackpadPinch = nil
        }
        if onTouchpadPinch?(phase, Float(now.scale), Float(now.rotation)) == false { dropTouchpad() }
    }

    /// Lifts the trackpad pinch under way: capture released, session stop.
    func endTouchpadPinch() {
        guard let pinch = trackpadPinch else { return }
        trackpadPinch = nil
        _ = onTouchpadPinch?(UInt8(PUNKTFUNK_PINCH_END), Float(pinch.scale), Float(pinch.rotation))
    }
    #endif

    /// An indirect-pointer touch is a button-held click/drag session: forward its position as
    /// an absolute cursor move and its button as a mouse button (down on begin, up on end).
    private func handleIndirectPointer(_ touch: UITouch, event: UIEvent?, kind: TouchKind) {
        let key = ObjectIdentifier(touch)
        let host = hostPoint(from: touch.location(in: self))
        switch kind {
        case .down:
            let button = Self.gsButton(for: event?.buttonMask ?? .primary)
            pointerButtons[key] = button
            if let host { onPointerMoveAbs?(host) } // place the cursor, then press
            onPointerButton?(button, true)
        case .move:
            if let host { onPointerMoveAbs?(host) }
        case .up, .cancel:
            if let host { onPointerMoveAbs?(host) }
            if let button = pointerButtons.removeValue(forKey: key) {
                onPointerButton?(button, false)
            }
        }
    }

    private func forwardTouches(_ touches: Set<UITouch>, kind: TouchKind) {
        guard onTouchEvent != nil else { return }
        for touch in touches {
            let key = ObjectIdentifier(touch)
            let id: UInt32
            switch kind {
            case .down:
                id = nextFreeID()
                touchIDs[key] = id
            case .move, .up, .cancel:
                guard let known = touchIDs[key] else { continue }
                id = known
            }
            if kind == .up {
                touchIDs.removeValue(forKey: key)
                onTouchEvent?(.touchUp(id: id))
                continue
            }
            guard let h = hostPoint(from: touch.location(in: self)) else { continue }
            onTouchEvent?(
                kind == .down
                    ? .touchDown(id: id, x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h)
                    : .touchMove(id: id, x: h.x, y: h.y, surfaceWidth: h.w, surfaceHeight: h.h))
        }
    }

    /// The passthrough dial opener: follow fingers that landed on a side bezel, and when two of
    /// them have been pulled in together, lift those contacts on the host and open the dial.
    ///
    /// The lift is what keeps the host honest — it already saw the touches go down, so ending
    /// them is the difference between a stray tap and two fingers stuck at the edge of the game.
    private func trackEdgePull(_ touches: Set<UITouch>, kind: TouchKind) {
        guard onDial != nil else { return }
        switch kind {
        case .down:
            let width = bounds.width
            for touch in touches {
                let p = touch.location(in: self)
                guard let edge = EdgeDial.edge(of: p, width: width) else { continue }
                edgeTracks[ObjectIdentifier(touch)] = EdgeTrack(edge: edge, start: p, now: p)
            }
        case .move:
            for touch in touches {
                let key = ObjectIdentifier(touch)
                guard var track = edgeTracks[key] else { continue }
                track.now = touch.location(in: self)
                edgeTracks[key] = track
            }
            guard let (a, b) = firstCompletedPull() else { return }
            // End the pull's own contacts, by the wire ids they were given. Dropping them from
            // `touchIDs` is also what makes the rest of this gesture invisible: `forwardTouches`
            // skips a touch it has no id for, so the later moves and the real lift send nothing.
            for key in edgeTracks.keys {
                if let id = touchIDs.removeValue(forKey: key) {
                    onTouchEvent?(.touchUp(id: id))
                }
            }
            edgeTracks.removeAll()
            onDial?(.open(at: EdgeDial.centre(a, b)))
        case .up, .cancel:
            for touch in touches { edgeTracks.removeValue(forKey: ObjectIdentifier(touch)) }
        }
    }

    /// The first pair of tracked fingers that satisfies [`EdgeDial.completes`].
    private func firstCompletedPull() -> (EdgeTrack, EdgeTrack)? {
        let tracks = Array(edgeTracks.values)
        for i in tracks.indices {
            for j in (i + 1)..<tracks.count where EdgeDial.completes(tracks[i], tracks[j]) {
                return (tracks[i], tracks[j])
            }
        }
        return nil
    }

    /// A hovering Pencil → in-range pen samples (distance, tilt, azimuth) on a pen-capable
    /// host. Toward any other host it moves the cursor, like a pointer.
    @objc private func handlePencilHover(_ recognizer: UIHoverGestureRecognizer) {
        if penEnabled {
            pencil.hover(recognizer, in: self)
        } else {
            handleHover(recognizer)
        }
    }

    /// Button-less mouse/trackpad movement (no lock) → absolute cursor move.
    @objc private func handleHover(_ recognizer: UIHoverGestureRecognizer) {
        switch recognizer.state {
        case .began, .changed:
            if let h = hostPoint(from: recognizer.location(in: self)) { onPointerMoveAbs?(h) }
        default:
            break
        }
    }

    /// Trackpad / wheel scroll → host scroll deltas. The translation is consumed each callback so
    /// the next is a fresh delta, sent in points and unscaled.
    ///
    /// Both axes pass through with their sign intact, which is what makes the stream follow the
    /// system's Natural Scrolling switch: UIKit has already applied that preference by the time it
    /// hands us a translation (it is what makes every UIScrollView on the device turn the right
    /// way), so the sign we get IS the user's choice, and the host's WHEEL convention agrees with
    /// it — +y is a wheel-forward notch, the one that moves content down. macOS passes
    /// `NSEvent.scrollingDeltaY` through for exactly the same reason.
    /// A continuous (trackpad) recognizer is a measured distance in points — DIP on the
    /// wire — with the recognizer's state as the gesture boundary.
    @objc private func handlePreciseScroll(_ g: UIPanGestureRecognizer) {
        forwardScroll(g, source: PUNKTFUNK_SCROLL_SOURCE_FINGER)
    }

    /// A discrete (wheel) recognizer reports OS-translated POINTS, not raw detents — no
    /// points-per-notch constant exists to recover them, so the fallback is honest
    /// Continuous distance without a gesture phase.
    @objc private func handleWheelScroll(_ g: UIPanGestureRecognizer) {
        forwardScroll(g, source: PUNKTFUNK_SCROLL_SOURCE_CONTINUOUS)
    }

    private func forwardScroll(_ g: UIPanGestureRecognizer, source: PunktfunkScrollSource) {
        let phase: PunktfunkScrollPhase
        switch g.state {
        case .began: phase = PUNKTFUNK_SCROLL_PHASE_BEGIN
        case .changed: phase = PUNKTFUNK_SCROLL_PHASE_UPDATE
        case .ended: phase = PUNKTFUNK_SCROLL_PHASE_END
        case .cancelled, .failed: phase = PUNKTFUNK_SCROLL_PHASE_CANCEL
        default: return // .possible — nothing yet
        }
        var t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        #if os(iOS)
        // A trackpad pinch under way owns the fingers: its scroll moves nothing, a stop still closes.
        if trackpadPinch != nil {
            guard phase == PUNKTFUNK_SCROLL_PHASE_END || phase == PUNKTFUNK_SCROLL_PHASE_CANCEL
            else { return }
            t = .zero
        }
        #endif
        // A stop's last translation rides out as movement first (ScrollCapture splits it);
        // the discrete fallback claims no boundary at all.
        let wirePhase = source == PUNKTFUNK_SCROLL_SOURCE_CONTINUOUS
            ? PUNKTFUNK_SCROLL_PHASE_NONE : phase
        onScroll?(Float(t.x), Float(t.y), source, wirePhase)
    }

    /// Map a view-space point through the presenter's placement into host-mode pixels, at the
    /// display scale the drawable is sized with; points on a bar or a cropped-away edge clamp onto
    /// the picture. nil until a mode is negotiated.
    private func hostPoint(from p: CGPoint) -> HostPoint? {
        guard let hostMode = currentHostMode?(), hostMode.width > 0, hostMode.height > 0
        else { return nil }
        #if os(visionOS)
        let s = traitCollection.displayScale > 0 ? traitCollection.displayScale : 2
        #else
        let s = traitCollection.displayScale > 0 ? traitCollection.displayScale : UIScreen.main.scale
        #endif
        let placement = VideoFit(name: settings.videoFit).place(
            view: (Int((bounds.width * s).rounded()), Int((bounds.height * s).rounded())),
            frame: (Int(hostMode.width), Int(hostMode.height)))
        guard !placement.isEmpty else { return nil }
        let f = placement.frame(fromView: CGPoint(x: p.x * s, y: p.y * s))
        let x = Int32(f.x.rounded().clamped(to: 0...(hostMode.width - 1)))
        let y = Int32(f.y.rounded().clamped(to: 0...(hostMode.height - 1)))
        return HostPoint(x: x, y: y, w: UInt32(hostMode.width), h: UInt32(hostMode.height))
    }

    /// UIKit's button mask → the wire's GameStream button number.
    ///
    /// The mask is 1-based over the HID button order — 1 primary, 2 secondary, 3 middle, 4/5 the
    /// side buttons — while the wire numbers middle and right the other way round (1 left,
    /// 2 middle, 3 right, 4 X1/back, 5 X2/forward), so only those two swap.
    ///
    /// `.primary`/`.secondary` are spelled out because they are the only two named cases; the rest
    /// come from `.button(_:)`, which takes the same 1-based number.
    private static func gsButton(for mask: UIEvent.ButtonMask) -> UInt32 {
        if mask.contains(.secondary) { return 3 }
        if mask.contains(.button(3)) { return 2 }
        if mask.contains(.button(4)) { return 4 }
        if mask.contains(.button(5)) { return 5 }
        return 1
    }

    private func nextFreeID() -> UInt32 {
        var id: UInt32 = 0
        while touchIDs.values.contains(id) { id += 1 }
        return id
    }
    #endif
}

#if os(visionOS)
extension StreamViewController: CurvePointerSink {
    public func curvePointer(u: Double, v: Double, phase: CurvePointerPhase) {
        if phase == .began, captureEnabled { setCaptured(true) }
        streamView.curvePointer(u: u, v: v, phase: phase)
    }

    /// Match window follows the curved screen as it does this view: its pixels at render scale.
    public func curveSize(width: Double, height: Double) {
        guard showsElsewhere, width > 0, height > 0 else { return }
        let scale = Double(renderScale)
        matchFollower?.noteSize(
            widthPx: Int((width * scale).rounded()), heightPx: Int((height * scale).rounded()))
    }

    /// Back in the window, Match window follows the window again.
    public func curveShown(_ shown: Bool) {
        if !shown, !onExternal { noteMatchWindowSize() }
    }
}

extension StreamLayerUIView {
    /// A pinch on the curved screen, in the session's touch model like a finger here: touch
    /// is one wire contact, pointer moves the cursor and holds the left button, and trackpad
    /// drags the cursor by the pinch's travel or, short and still, clicks where it is.
    func curvePointer(u: Double, v: Double, phase: CurvePointerPhase) {
        guard let mode = currentHostMode?(), mode.width > 0, mode.height > 0 else { return }
        let point = CGPoint(
            x: (u * mode.width).clamped(to: 0...(mode.width - 1)),
            y: (v * mode.height).clamped(to: 0...(mode.height - 1)))
        let host = HostPoint(
            x: Int32(point.x.rounded()), y: Int32(point.y.rounded()),
            w: UInt32(mode.width), h: UInt32(mode.height))
        if phase == .began {
            var route = TouchInputMode.current(settings)
            if route == .touch, !touchPassthroughEnabled { route = .trackpad }
            curve = CurvePinch(route: route, start: point, last: point, at: CACurrentMediaTime())
        }
        guard var pinch = curve else { return }
        switch pinch.route {
        case .touch:
            switch phase {
            case .began:
                onTouchEvent?(.touchDown(
                    id: CurvePinch.touchID, x: host.x, y: host.y,
                    surfaceWidth: host.w, surfaceHeight: host.h))
            case .moved:
                onTouchEvent?(.touchMove(
                    id: CurvePinch.touchID, x: host.x, y: host.y,
                    surfaceWidth: host.w, surfaceHeight: host.h))
            case .ended:
                onTouchEvent?(.touchUp(id: CurvePinch.touchID))
            }
        case .pointer:
            onTouchEvent?(.mouseMoveAbs(
                x: host.x, y: host.y, surfaceWidth: host.w, surfaceHeight: host.h))
            if phase != .moved { onTouchEvent?(.mouseButton(1, down: phase == .began)) }
        case .trackpad:
            pinch.carry.x += point.x - pinch.last.x
            pinch.carry.y += point.y - pinch.last.y
            pinch.last = point
            let dx = pinch.carry.x.rounded(.towardZero), dy = pinch.carry.y.rounded(.towardZero)
            if dx != 0 || dy != 0 {
                onTouchEvent?(.mouseMove(dx: Int32(dx), dy: Int32(dy)))
                pinch.carry.x -= dx
                pinch.carry.y -= dy
            }
            if phase == .ended, pinch.isTap(at: point) {
                onTouchEvent?(.mouseButton(1, down: true))
                onTouchEvent?(.mouseButton(1, down: false))
            }
        case .off:
            break
        }
        curve = phase == .ended ? nil : pinch
    }
}
#endif

#if os(iOS) || os(visionOS)
// The soft keyboard's output → wire events. UIKeyInput is deliberately minimal (no UITextInput):
// the stream needs keystrokes, not an editing buffer. Toward a text-capable host each character
// goes as committed text, typed in the host's layout; Return and Tab stay keys. Otherwise
// `SoftKeyMap` gives US-positional VKs, a capital wrapped in VK_LSHIFT, and drops the rest.
// Events ride `onTouchEvent`, so the trust prompt gates them with the touch mouse.
extension StreamLayerUIView: UIKeyInput {
    // Keep the IME literal — no autocorrect/smart substitutions; a remote desktop is not prose,
    // and the host does its own text handling.
    var autocorrectionType: UITextAutocorrectionType { get { .no } set {} }
    var autocapitalizationType: UITextAutocapitalizationType { get { .none } set {} }
    var spellCheckingType: UITextSpellCheckingType { get { .no } set {} }
    var smartQuotesType: UITextSmartQuotesType { get { .no } set {} }
    var smartDashesType: UITextSmartDashesType { get { .no } set {} }
    var smartInsertDeleteType: UITextSmartInsertDeleteType { get { .no } set {} }
    var keyboardType: UIKeyboardType { get { .asciiCapable } set {} }

    var hasText: Bool { false }

    func insertText(_ text: String) {
        // A hardware keyboard's presses reach the host through GCKeyboard AND arrive here as
        // UIKeyInput insertions while we're first responder — forwarding both would double
        // every character, so the HID path owns keys whenever a hardware keyboard is attached.
        guard GCKeyboard.coalesced == nil else { return }
        #if os(iOS)
        // Text carries no modifiers: under one the keyboard row holds, the row sends this.
        if textInputEnabled, keyRow?.typed(text) == true { return }
        #endif
        var sentKey = false
        for ch in text {
            let control = ch.asciiValue.map { $0 < 0x20 } ?? false // Return, Tab
            if textInputEnabled, !control {
                ch.unicodeScalars.forEach { onTouchEvent?(.text($0)) }
                continue
            }
            guard let key = SoftKeyMap.vk(for: ch) else { continue }
            if key.shift { onTouchEvent?(.key(0xA0, down: true)) } // VK_LSHIFT
            onTouchEvent?(.key(key.vk, down: true))
            onTouchEvent?(.key(key.vk, down: false))
            if key.shift { onTouchEvent?(.key(0xA0, down: false)) }
            sentKey = true
        }
        if sentKey { hostKeyLifted() }
    }

    func deleteBackward() {
        guard GCKeyboard.coalesced == nil else { return } // see insertText
        onTouchEvent?(.key(0x08, down: true)) // VK_BACK
        onTouchEvent?(.key(0x08, down: false))
        hostKeyLifted()
    }
}
#endif

#if os(iOS)
extension StreamLayerUIView: UIGestureRecognizerDelegate {
    /// The Pencil's hover recognizer takes every event. The pinch and rotation recognizers see
    /// the trackpad's transform events alone (iPadOS 13.4+), and only toward a touchpad host.
    func gestureRecognizer(_ g: UIGestureRecognizer, shouldReceive event: UIEvent) -> Bool {
        g is UIHoverGestureRecognizer || (frames.touchpad.presents && event.type == .transform)
    }

    /// Pinch, rotation and the Pencil's hover run beside every other recognizer on the view.
    func gestureRecognizer(
        _ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer
    ) -> Bool { true }
}
#endif
#endif
