// iOS/iPadOS presenter: the macOS AVSampleBufferDisplayLayer + StreamPump in a UIViewController,
// so the scene can pointer-lock. UITouch.type routes fingers and pointers apart.
//
// Direct fingers and Pencil always forward as wire touches, mapped through the aspect-fit letterbox
// into host-mode pixels. A mouse or trackpad is a pointer and never forwards as a touch.
//
// Locked (full-screen, frontmost, pointer capture allowed; see PointerLockChain): GCMouse drives
// motion and buttons for every connected mouse and the system hides the cursor. Unlocked (Stage
// Manager, not frontmost, iPhone): UIKit hover and indirect touches forward an absolute cursor
// plus buttons. `gcMouseForwarding` (== locked) keeps the two apart, so a pointer reporting on both
// never double-sends. Scroll always comes from UIKit's pan recognizers, locked or not.
//
// Hardware keyboards share InputCapture with macOS: engaged at stream start, ⌘⎋ toggles and
// ⌃⌥⇧Q releases, both read from the HID stream.
//
// The public type is named StreamView like its macOS twin, so the SwiftUI layer is shared.

#if os(iOS) || os(tvOS)
import AVFoundation
import GameController
import PunktfunkCore
import PunktfunkShared
import SwiftUI
import UIKit
import os
#if os(tvOS)
import AVKit // AVDisplayManager — the per-session display-mode (HDR10/refresh) request
#endif

/// Same diagnostic switch as InputCapture (PUNKTFUNK_INPUT_DEBUG=1): on iOS we log the
/// resolved pointer-lock state each time capture engages, so the user can see whether the
/// scene actually locked (GCMouse only delivers deltas while it did) or whether we're on
/// the touch fallback.
private let iosInputLog = ClientLog(category: "input")
private let iosInputDebug = ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

public struct StreamView: UIViewControllerRepresentable {
    private let connection: PunktfunkConnection
    private let captureEnabled: Bool
    private let onCaptureChange: ((Bool) -> Void)?
    private let onDial: ((DialEvent) -> Void)?
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?
    private let onResizeTarget: ((UInt32, UInt32) -> Void)?
    private let onDecodedSize: (@Sendable (Int, Int) -> Void)?
    private let endToEndMeter: LatencyMeter?

    /// `onDisconnectRequest` exists for call-site parity with the macOS StreamView (the
    /// captured-state ⌃⌥⇧D combo is detected by the macOS NSEvent monitor only); on iOS a
    /// hardware keyboard reaches Disconnect through the Stream menu's key equivalent instead,
    /// so the parameter is accepted and unused here.
    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onDisconnectRequest: (() -> Void)? = nil,
        onDial: ((DialEvent) -> Void)? = nil,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil,
        onResizeTarget: ((UInt32, UInt32) -> Void)? = nil,
        onDecodedSize: (@Sendable (Int, Int) -> Void)? = nil,
        endToEndMeter: LatencyMeter? = nil
    ) {
        self.connection = connection
        self.captureEnabled = captureEnabled
        self.onCaptureChange = onCaptureChange
        self.onDial = onDial
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
        self.onResizeTarget = onResizeTarget
        self.onDecodedSize = onDecodedSize
        self.endToEndMeter = endToEndMeter
    }

    public func makeUIViewController(context: Context) -> StreamViewController {
        let controller = StreamViewController()
        controller.onCaptureChange = onCaptureChange
        controller.onDial = onDial
        controller.captureEnabled = captureEnabled
        controller.endToEndMeter = endToEndMeter
        controller.onResizeTarget = onResizeTarget
        controller.onDecodedSize = onDecodedSize
        controller.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return controller
    }

    public func updateUIViewController(_ controller: StreamViewController, context: Context) {
        controller.onCaptureChange = onCaptureChange
        controller.onDial = onDial
        controller.captureEnabled = captureEnabled
        controller.endToEndMeter = endToEndMeter
        controller.onResizeTarget = onResizeTarget
        controller.onDecodedSize = onDecodedSize
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
/// stream is up. Without it a pad's B/Menu press doubles as a UIKit menu press — which ended
/// the session (or suspended the whole app) from ordinary gameplay; a SwiftUI
/// `.onExitCommand {}` swallow proved unreliable with nothing focusable on screen. Every
/// in-session exit is GC-level by design: the pad's escape chord (GamepadCapture) and the
/// remote's hold-Back (SiriRemotePointer).
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
    /// stage-1 StreamPump → displayLayer path as the Metal-unavailable / DEBUG fallback.
    private let presenter = SessionPresenter()
    /// Pending pen-boost release — 2 s of hysteresis so hover flicker at the glass edge
    /// doesn't thrash the deadline link's rate range (see `setInteractionBoost`).
    private var penBoostRelease: DispatchWorkItem?
    #if os(tvOS)
    /// The window's display manager the session's mode request was set on — held weakly so
    /// stop() can clear the request even after the view has left the window.
    private weak var sessionDisplayManager: AVDisplayManager?
    /// The decoded frames are HDR — what the display-mode request follows.
    private var frameHDR = false
    #endif
    private var inputCapture: InputCapture?
    #if os(iOS)
    fileprivate var captured = false
    private var pointerInteraction: UIPointerInteraction?
    /// Capture state at the last resign, restored on the next foreground — otherwise the
    /// mouse/keyboard stay released after navigating out and nothing re-grabs them.
    private var wasCapturedOnResign = false
    /// Match-window resize follower (C3) — non-nil while a session is active AND the `matchWindow`
    /// setting is on (DEFAULT on, for pixel-exact scene streaming); fed the view's physical-pixel
    /// size from `viewDidLayoutSubviews` so an iPad Stage Manager / Split View scene resize
    /// renegotiates the host mode (1:1, no presenter resample). iOS only (iPhone naturally no-ops
    /// its fixed full-screen scene; tvOS drives display modes via AVDisplayManager instead).
    private var matchFollower: MatchWindowFollower?
    /// The picture's surface on an attached monitor (see `ExternalDisplay`), and whether the
    /// stream presents there now. Input and the HUD stay on the phone either way.
    private lazy var externalVideo: ExternalVideoView = {
        let view = ExternalVideoView()
        view.onLayout = { [weak self] in self?.layoutMetalLayer() }
        return view
    }()
    private var onExternal = false
    // `prefersPointerLocked` mirrors `wantsPointerLock`; SpringBoard grants or drops on its own
    // terms. `requestPointerLock` re-asks only on an event that can change its answer: a drop
    // while wanted, a click while unlocked, the scene going active, the window filling the screen
    // again. Each is one false→true edge — re-asserting a value it already holds is ignored.

    /// True while a re-ask holds `prefersPointerLocked` at false for `pointerLockEdgeHold`.
    private var pointerLockSuppressed = false
    /// Long enough for UIKit to push the false pass to the scene before the true one follows.
    private static let pointerLockEdgeHold: TimeInterval = 0.05
    /// When the last drop-triggered re-ask was scheduled. A grant SpringBoard revokes at once
    /// would otherwise re-ask on every revoke; one a second keeps that bounded.
    private var pointerLockDropRelockAt: CFTimeInterval = -.infinity
    /// Whether the window filled its screen at the last layout pass (assumed so until a pass says
    /// otherwise) — the pass that makes it fill again, a re-maximise, is the one that re-asks.
    private var windowFilledScreen = true
    #endif

    /// Reads whether the scene's pointer is actually locked right now; nil = state
    /// unavailable (no scene yet, or pre-availability). Only while this is true does GCMouse
    /// deliver relative deltas — otherwise the touch path carries input.
    private func pointerLockEngaged() -> Bool? {
        #if os(iOS)
        return view.window?.windowScene?.pointerLockState?.isLocked
        #else
        return nil
        #endif
    }

    var onCaptureChange: ((Bool) -> Void)?
    /// The two-finger twist turning the quick-action ring (forwarded from the stream view).
    var onDial: ((DialEvent) -> Void)?
    /// Resize-overlay START: forwarded to the Match-window follower so a scene resize drives the
    /// blur+spinner the instant the window differs from the live mode (iOS only — tvOS has no
    /// follower). See `MatchWindowFollower.onResizeTarget`.
    var onResizeTarget: ((UInt32, UInt32) -> Void)?
    /// Resize-overlay END: the presenter reports the coded dims of each new-mode IDR here, so the
    /// overlay clears when a frame at the requested size actually decodes.
    var onDecodedSize: (@Sendable (Int, Int) -> Void)?
    /// Last decoded size fed into the presenter's aspect-fit. A new-mode IDR (an iPad scene resize,
    /// or a tvOS AVDisplayManager mode switch) re-fits the metal sublayer to the REAL content aspect
    /// here — `viewDidLayoutSubviews` only re-runs on a bounds change, which a resize-END lacks, so
    /// without this the layer keeps its pre-resize aspect and stretches the new frame into it. Main.
    private var lastDecodedContentSize: CGSize?

    var captureEnabled = true {
        didSet {
            guard captureEnabled != oldValue else { return }
            #if os(iOS)
            setCaptured(captureEnabled)
            #else
            inputCapture?.setForwarding(captureEnabled)
            #endif
        }
    }

    private var streamView: StreamLayerUIView {
        // swiftlint:disable:next force_cast
        view as! StreamLayerUIView
    }

    public override func loadView() {
        view = StreamLayerUIView()
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
        #if os(iOS)
        // Hide the iPadOS cursor while it hovers the video: the host renders its own
        // cursor from our deltas, so the local one only diverges from it. This hides the
        // pointer; true pointer LOCK (below) is what makes GCMouse deliver relative deltas
        // — and the system only grants it on a full-screen, frontmost iPad scene.
        let interaction = UIPointerInteraction(delegate: self)
        view.addInteraction(interaction)
        pointerInteraction = interaction
        #endif
    }

    #if os(iOS)
    /// Whether the user wants the mouse/trackpad pointer CAPTURED (pointer lock → relative
    /// movement, the gaming default) rather than forwarded as an absolute position (desktop
    /// use). Read from the session's resolved settings so it tracks the Settings toggle (it is
    /// tier G — this device's input hardware — so no preset can move it); defaults to on when
    /// unset. iPad-only — gated again in `prefersPointerLocked`.
    private var pointerCaptureEnabled: Bool {
        connection?.settings.pointerCapture ?? true
    }

    /// Whether the pointer should be CAPTURED right now: iPad, capture engaged, and the user
    /// hasn't opted into the absolute (desktop) pointer. The system additionally requires
    /// full-screen + frontmost and may drop the lock (Slide Over/Stage Manager/backgrounding) —
    /// syncPointerLock() handles the actual grant/drop and falls back to absolute when unlocked.
    private var wantsPointerLock: Bool {
        // The trailing grant test is per-client access §7 — no pointer lock without the
        // POINTER bit (a Controller-only guest's trackpad stays a normal local pointer);
        // read live, so a mid-session re-grant lets the next resolve pass lock.
        captured && pointerCaptureEnabled && UIDevice.current.userInterfaceIdiom == .pad
            && connection?.canSendPointer == true
    }

    public override var prefersPointerLocked: Bool { wantsPointerLock && !pointerLockSuppressed }
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

    /// Put THIS controller on the responder chain for hardware key presses.
    ///
    /// Nothing of ours is otherwise a first responder during a normal stream: keys arrive on the
    /// GameController (`GCKeyboard`) path, which is a parallel HID feed that does not consume the
    /// UIKit event, and `StreamLayerUIView` only becomes first responder to summon the SOFT
    /// keyboard (it is `UIKeyInput`, so making it one for any other reason would raise the on-screen
    /// keyboard mid-game). With no responder of ours in the chain, every hardware key press reaches
    /// UIKit unclaimed — and an unclaimed press is what lets the system apply its own default for
    /// that key. `pressesBegan` below is where we claim Escape; this is what gets it delivered.
    ///
    /// A controller is not `UIKeyInput`, so being first responder raises no keyboard. Deferred to
    /// the soft keyboard whenever the view has taken over, so the three-finger-swipe keyboard is
    /// unaffected.
    ///
    /// Only while captured — the whole claim is scoped to "the stream owns the keyboard", and
    /// holding the chain outside that would sit in front of SwiftUI's focus for no reason. Safe to
    /// call from anywhere: `start()` engages capture BEFORE SwiftUI puts us in a window (where
    /// `becomeFirstResponder` cannot succeed), so `viewDidAppear` calls it again to catch up.
    private func anchorKeyResponder() {
        guard captured, !streamView.isFirstResponder, !isFirstResponder else { return }
        becomeFirstResponder()
    }

    public override var canBecomeFirstResponder: Bool { true }

    /// Claim Escape while the stream owns the keyboard, so the SYSTEM never gets to act on it.
    ///
    /// This is the fix for "Escape hands the mouse back to iPadOS": the platform releases the
    /// scene's pointer lock on an Escape that nothing claimed — the same "let me out" the web
    /// Pointer Lock API mandates. Every recovery attempt before this one fought that release AFTER
    /// the fact (a re-lock burst, then a click), and the platform's post-Escape cooldown means the
    /// burst is refused by construction. Claiming the press means there is nothing to recover from.
    ///
    /// Escape is forwarded to the host on the GCKeyboard path, which is untouched by this — that
    /// path never sees the UIKit responder chain, so the host still receives the keystroke and
    /// in-game menus still open. Only the system's own interpretation is suppressed.
    ///
    /// Strictly scoped: only while `captured` (the stream owns input), and only Escape. Anything
    /// else — including every key while the pointer is released — goes to `super` untouched, so
    /// Escape still dismisses sheets, exits full screen and does everything else it should whenever
    /// we are not holding the keyboard. The deliberate ways out are unaffected: ⌘⎋ and ⌃⌥⇧Q are
    /// recognized on the GCKeyboard path and clear `captured` themselves.
    public override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unclaimed = presses.filter { !claimsPress($0) }
        if !unclaimed.isEmpty || presses.isEmpty {
            super.pressesBegan(unclaimed, with: event)
        }
    }

    public override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unclaimed = presses.filter { !claimsPress($0) }
        if !unclaimed.isEmpty || presses.isEmpty {
            super.pressesEnded(unclaimed, with: event)
        }
    }

    public override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        // Never swallowed: a cancelled press is the system taking the key away from us, and
        // dropping it here would strand UIKit's own bookkeeping for a press we did claim.
        super.pressesCancelled(presses, with: event)
    }

    /// Is this press one the stream owns outright (Escape while captured)?
    private func claimsPress(_ press: UIPress) -> Bool {
        captured && press.key?.keyCode == .keyboardEscape
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
        #if os(iOS)
        // Fresh session: drop any resign/foreground capture-restore state left over from a
        // prior session (stop() doesn't clear it). Otherwise a stale `true` could later
        // re-engage capture on a foreground that the new session never asked for.
        wasCapturedOnResign = false
        streamView.settings = connection.settings
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
        // Pencil stays a finger, exactly as before). Same trust gate as touch.
        streamView.penEnabled = connection.hostSupportsPen
        streamView.touchPassthroughEnabled = connection.hostSupportsTouch
        // The two-finger twist → the quick-action ring, same trust gate as touch.
        streamView.onDial = { [weak self] event in
            guard let self, self.captureEnabled else { return }
            self.onDial?(event)
        }
        streamView.onPenBatch = { [weak self, weak connection] batch in
            guard self?.captureEnabled == true else { return }
            connection?.sendPen(batch)
        }
        // Pencil near the glass ⇒ pin the panel (and with it UIKit's event cadence) at the
        // link range's ceiling, so a sub-panel-rate stream stops halving pencil sampling.
        // Engage is immediate; release waits 2 s so edge-of-canvas hover flicker can't
        // thrash the link. MAIN thread (UIKit events + main-queue work item).
        streamView.onPenProximity = { [weak self] near in
            guard let self else { return }
            self.penBoostRelease?.cancel()
            self.penBoostRelease = nil
            if near {
                self.presenter.setInteractionBoost(true)
            } else {
                let release = DispatchWorkItem { [weak self] in
                    guard let self else { return }
                    self.penBoostRelease = nil
                    self.presenter.setInteractionBoost(false)
                }
                self.penBoostRelease = release
                DispatchQueue.main.asyncAfter(deadline: .now() + 2, execute: release)
            }
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
            // `engageCapture(fromClick:)`. It is the local gesture, never forwarded: InputCapture's
            // latch swallows its release. Only button 1 has that latch; another button would send
            // a lone release and eat the next click. Captured → the absolute path forwards it.
            if !self.captured {
                if down, button == 1, self.captureEnabled { self.setCaptured(true, fromClick: true) }
                return
            }
            guard self.inputCapture?.gcMouseForwarding == false else { return }
            self.inputCapture?.sendMouseButton(button, pressed: down)
            // Captured but unlocked: the click is the user gesture SpringBoard wants before it
            // hands a declined or dropped lock back. On the UP, so the click has fully forwarded
            // on one transport — a grant mid-click would strand the release on the GCMouse path.
            if !down { self.requestPointerLock() }
        }
        // Every scroll, wheel and trackpad, locked or not: UIKit applies Natural Scrolling, which
        // GCMouse's raw wheel does not, so the direction never changes with the lock.
        streamView.onScroll = { [weak self] dx, dy, source, phase in
            self?.inputCapture?.sendScroll(dx: dx, dy: dy, source: source, phase: phase)
        }

        let capture = InputCapture(connection: connection)
        capture.onToggleCapture = { [weak self] in
            guard let self else { return }
            self.setCaptured(!self.captured)
        }
        // ⌃⌥⇧Q (cross-client parity with macOS/Windows/Linux) releases the captured pointer +
        // keyboard so the Magic Keyboard trackpad returns to driving the local iPad UI. Detected
        // from the HID stream in InputCapture (no NSEvent monitor on iOS); unlike the ⌘⎋ toggle it
        // only ever RELEASES — re-pressing it while already released is a no-op (setCaptured guards).
        capture.onReleaseCapture = { [weak self] in
            self?.setCaptured(false)
        }
        // ⌃⌥⇧A mutes/unmutes the mic uplink. Session state this controller doesn't own, so it
        // posts to the app exactly as the macOS chord does — the Stream menu's identical
        // equivalent (which a captured scene swallows) ends at the same toggle.
        capture.onToggleMicMute = {
            NotificationCenter.default.post(name: .punktfunkToggleMicMute, object: nil)
        }
        capture.onPreempted = { [weak self] in
            self?.setCaptured(false)
        }
        capture.start()
        inputCapture = capture
        // Match-window (C3): when ON, follow the scene's pixel size so a resizable iPad scene
        // streams 1:1 (pixel-exact) instead of the presenter resampling a fixed-mode frame into it.
        // `viewDidLayoutSubviews` feeds it — covers Stage Manager / Split View resizes and rotation.
        // iPhone is a fixed full-screen scene, so this naturally no-ops (reports the device mode).
        // OPT-IN — `?? false` matches the Settings toggle (which also defaults off); an unset
        // default keeps the explicit mode.
        let follower = MatchWindowFollower(
            connection: connection,
            enabled: connection.settings.matchWindow,
            renderScale: connection.settings.renderScale,
            maxDimension: RenderScale.maxDimension(codec: connection.settings.codec))
        follower.onResizeTarget = onResizeTarget
        matchFollower = follower
        // A monitor attached before the session starts shows the picture from the first frame.
        onExternal = ExternalDisplay.shared.screen != nil
        if onExternal { ExternalDisplay.shared.show(externalVideo) }
        #endif

        // Presenter choice + lifecycle live in SessionPresenter (shared with macOS): stage-2
        // (explicit VTDecompressionSession decode + a CAMetalLayer/display-link present) by
        // default, the stage-1 pump as the Metal-missing / DEBUG fallback.
        // Intercept the pump's coded-dims callback: re-fit the metal sublayer to the real content
        // aspect (main thread) BEFORE forwarding to the owner's overlay END-signal. Fires only on a
        // size CHANGE (first frame + each resolved resize), so this is rare, not per-frame.
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

        #if os(iOS)
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
                self.requestPointerLock()
            }
        })
        // The system grants or drops the lock on its own terms (Slide Over, Stage Manager, the
        // window leaving screen size). Re-resolve the routing on every change; a drop while the
        // lock is still wanted asks once more, a turn later, so a drop that precedes the app's own
        // resign finds `captured` already cleared and does nothing.
        observers.append(NotificationCenter.default.addObserver(
            forName: UIPointerLockState.didChangeNotification, object: nil, queue: .main
        ) { [weak self] note in
            guard let self else { return }
            self.syncPointerLock()
            guard (note.userInfo?[UIPointerLockState.sceneUserInfoKey] as? UIScene)
                === self.view.window?.windowScene,
                self.wantsPointerLock, self.pointerLockEngaged() == false
            else { return }
            let now = CACurrentMediaTime()
            guard now - self.pointerLockDropRelockAt >= 1 else { return }
            self.pointerLockDropRelockAt = now
            DispatchQueue.main.async { [weak self] in self?.requestPointerLock() }
        })
        // The Stream menu's "Release Mouse" (⌃⌥⇧Q) posts this — the discoverable menu surface for
        // the RELEASED state. While CAPTURED the combo is recognized from the HID stream in
        // InputCapture (onReleaseCapture) before the menu sees it, so in practice this fires as a
        // not-captured no-op (setCaptured guards it); wired for honesty + a non-GC fallback. Only the
        // foreground-active scene's stream acts — the iPad analogue of macOS's key-window guard, so a
        // second Stage Manager scene isn't released out from under the user.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkReleaseCapture, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self,
                  self.view.window?.windowScene?.activationState == .foregroundActive else { return }
            self.setCaptured(false)
        })
        // The ring's Keyboard slot shows the soft keyboard, and hides it when it is up. iPhone's
        // keyboard has no dismiss key, and passthrough has no three-finger swipe.
        observers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkToggleSoftKeyboard, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self,
                  self.view.window?.windowScene?.activationState == .foregroundActive else { return }
            self.streamView.setSoftKeyboardVisible(!self.streamView.isFirstResponder)
        })
        // A monitor plugged in or pulled mid-session takes the picture or hands it back.
        observers.append(NotificationCenter.default.addObserver(
            forName: ExternalDisplay.didChange, object: nil, queue: .main
        ) { [weak self] _ in
            self?.routeVideo()
        })

        if captureEnabled {
            setCaptured(true) // entering a session is the deliberate "capture me" moment
        }
        #endif

        #if os(tvOS)
        // No click-to-capture and no pointer lock here: a session IS the capture, so an attached
        // Bluetooth mouse or keyboard forwards for as long as one runs — once it is trusted.
        let capture = InputCapture(connection: connection)
        capture.start()
        capture.gcMouseForwarding = true
        capture.setForwarding(captureEnabled)
        inputCapture = capture
        // The TV's mode switch (requested in applyDisplayCriteriaIfNeeded) completes
        // asynchronously, and a dynamic-range-only switch doesn't re-layout by itself —
        // re-layout on the switch/mode notifications so the presenter sees the new EDR
        // headroom immediately (layout pushes UIScreen.currentEDRHeadroom down).
        observers.append(NotificationCenter.default.addObserver(
            forName: .AVDisplayManagerModeSwitchEnd, object: nil, queue: .main
        ) { [weak self] _ in self?.layoutMetalLayer() })
        observers.append(NotificationCenter.default.addObserver(
            forName: UIScreen.modeDidChangeNotification, object: nil, queue: .main
        ) { [weak self] _ in self?.layoutMetalLayer() })
        #endif
    }

    func stop() {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        observers.removeAll()
        #if os(iOS)
        setCaptured(false)
        inputCapture?.stop()
        inputCapture = nil
        // Release anything the touch-driven mouse still holds (a mid-drag session end) while
        // onTouchEvent can still deliver the button-up.
        streamView.resetTouchInput()
        streamView.onTouchEvent = nil
        streamView.onDial = nil
        TouchInputMode.sessionOverride = nil // the ring's mid-stream switch dies with the session
        streamView.onPenBatch = nil // after reset — the pen's leave-range sample rides it
        streamView.onPenProximity = nil // after reset — its leave-range transition fired above
        penBoostRelease?.cancel()
        penBoostRelease = nil
        streamView.penEnabled = false
        streamView.onPointerMoveAbs = nil
        streamView.onPointerButton = nil
        streamView.onScroll = nil
        streamView.currentHostMode = nil
        matchFollower = nil
        if onExternal {
            ExternalDisplay.shared.hide(externalVideo) // the monitor mirrors the phone again
            onExternal = false
        }
        #endif
        #if os(tvOS)
        inputCapture?.stop()
        inputCapture = nil
        // Return the TV to the user's preferred mode — the home screen must not stay in the
        // session's HDR10/refresh mode.
        sessionDisplayManager?.preferredDisplayCriteria = nil
        sessionDisplayManager = nil
        frameHDR = false
        #endif
        presenter.stop()
        lastDecodedContentSize = nil // the next session re-derives it from its first frame
        connection = nil
    }

    public override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        layoutMetalLayer()
        #if os(iOS)
        // Match-window (C3): feed the follower the view's physical-pixel size (points × scale).
        // Not while a monitor shows the picture: its mode comes from `requestSurfaceMode`.
        let b = streamView.bounds
        if b.width > 0, b.height > 0, !onExternal {
            let scale = renderScale
            matchFollower?.noteSize(
                widthPx: Int((b.width * scale).rounded()),
                heightPx: Int((b.height * scale).rounded()))
        }
        // The window back at screen size (a windowed scene re-maximised) can hold the lock again.
        let fills = windowFillsScreen
        if fills, !windowFilledScreen { requestPointerLock() }
        windowFilledScreen = fills
        #endif
        #if os(tvOS)
        applyDisplayCriteriaIfNeeded()
        #endif
    }

    #if os(tvOS)
    /// Ask the TV for a display mode matching the session — HDR10 at the stream's refresh rate —
    /// via AVDisplayManager, the tvOS mechanism custom renderers use for HDR output (AVFoundation
    /// playback layers do this implicitly). Honored only when the user allows matching (tvOS
    /// Settings → Video and Audio → Match Content); the presenter reads the RESULT off UIScreen's
    /// EDR headroom (pushed in SessionPresenter.layout) and keeps the in-shader tone-map whenever
    /// the switch never lands, so an SDR-composited display can't show blown-out PQ either way.
    /// Applied once per session, as soon as the window and the negotiated mode both exist; the
    /// stop() teardown clears it.
    ///
    /// ⚠️ Keyed on the decoded frames being HDR (`frameHDR`), not the Welcome or the setting alone.
    /// The criteria hardcode BT.2020 PQ, and in its HDR modes the Apple TV sends limited-range
    /// HDMI, so SDR frames on a full-range set show code 16 as grey. Frames that turn SDR hand the
    /// TV back its own mode.
    private func applyDisplayCriteriaIfNeeded() {
        guard let manager = view.window?.avDisplayManager, let connection else { return }
        guard frameHDR, connection.settings.hdrEnabled else {
            if sessionDisplayManager != nil {
                manager.preferredDisplayCriteria = nil
                sessionDisplayManager = nil
            }
            return
        }
        guard manager.preferredDisplayCriteria == nil else { return }
        let mode = connection.currentMode()
        guard mode.width > 0, mode.height > 0, mode.refreshHz > 0 else { return }
        // A synthetic HDR10-HEVC format description carrying the negotiated mode — what the
        // stream decodes to. AVDisplayCriteria(refreshRate:formatDescription:) matches the
        // display to it (tvOS 17+, our deployment floor).
        let ext: [CFString: Any] = [
            kCMFormatDescriptionExtension_ColorPrimaries:
                kCMFormatDescriptionColorPrimaries_ITU_R_2020,
            kCMFormatDescriptionExtension_TransferFunction:
                kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ,
            kCMFormatDescriptionExtension_YCbCrMatrix:
                kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
        ]
        var desc: CMFormatDescription?
        CMVideoFormatDescriptionCreate(
            allocator: kCFAllocatorDefault, codecType: kCMVideoCodecType_HEVC,
            width: Int32(mode.width), height: Int32(mode.height),
            extensions: ext as CFDictionary, formatDescriptionOut: &desc)
        guard let desc else { return }
        manager.preferredDisplayCriteria = AVDisplayCriteria(
            refreshRate: Float(mode.refreshHz), formatDescription: desc)
        sessionDisplayManager = manager
    }
    #endif

    /// The display scale to render the metal drawable at. `traitCollection.displayScale` is the
    /// canonical render scale and is reliable once the controller is in the hierarchy;
    /// `view.contentScaleFactor` can read 1.0 before the view attaches to a window/screen, which
    /// would size the drawable at point resolution → a pixelated, upscaled mess. Falls back to the
    /// main screen scale if the trait is still unspecified.
    private var renderScale: CGFloat {
        let s = traitCollection.displayScale
        return s > 0 ? s : UIScreen.main.scale
    }

    /// Aspect-fit the stage-2 metal sublayer to the surface showing the picture — this view, or
    /// an attached monitor — at that surface's render scale (see SessionPresenter.layout).
    private func layoutMetalLayer() {
        videoLayer.videoGravity = SessionPresenter.gravity(VideoFit(name: connection?.settings.videoFit))
        // UIKit exposes only the ceiling; the range and step stay unknown (min = max).
        let maxHz = Double((streamView.window?.screen ?? UIScreen.main).maximumFramesPerSecond)
        presenter.setPanel(PanelInfo(minHz: maxHz, maxHz: maxHz))
        #if os(iOS)
        if onExternal {
            let scale = externalVideo.traitCollection.displayScale
            presenter.layout(in: externalVideo.bounds, contentsScale: scale > 0 ? scale : 1)
            return
        }
        #endif
        presenter.layout(in: streamView.bounds, contentsScale: renderScale)
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
    /// new frame into the pre-resize box. Deduped so a same-size repeat is a no-op. Main thread.
    private func noteDecodedContentSize(width: Int, height: Int) {
        let size = CGSize(width: width, height: height)
        guard size.width > 0, size.height > 0, size != lastDecodedContentSize else { return }
        lastDecodedContentSize = size
        presenter.setContentSize(size)
        layoutMetalLayer()
    }

    #if os(iOS)
    /// Follow a monitor plugged in or pulled mid-session: move the picture onto it or back to the
    /// phone, then ask the host for the mode that fits. Main thread.
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
        requestSurfaceMode()
    }

    /// Ask the host for the mode that fits where the picture is: the monitor's pixels at its top
    /// refresh, or the session's own mode back on the phone. Skipped when it already streams that.
    private func requestSurfaceMode() {
        guard let connection else { return }
        let settings = connection.settings
        let target = (onExternal ? ExternalDisplay.streamMode(settings) : nil)
            ?? settings.streamMode(native: NativeDisplay.mode)
        let live = connection.currentMode()
        guard live.width != target.width || live.height != target.height
            || live.refreshHz != target.hz
        else { return }
        connection.requestMode(width: target.width, height: target.height, refreshHz: target.hz)
    }

    /// `fromClick` marks a click-driven engage (the released-state pointer click that re-captures):
    /// that click's press/release are suppressed toward the host — it's the local engage gesture,
    /// not a host click — exactly as macOS's `engageCapture(fromClick:)` does. Keyboard-driven
    /// engages (⌘⎋) pass false so a normal click still reaches the host.
    private func setCaptured(_ on: Bool, fromClick: Bool = false) {
        if on {
            // `connection != nil` is the session-active gate (presenter internals are opaque here).
            guard captureEnabled, !captured, connection != nil else { return }
            inputCapture?.setForwarding(true, suppressClick: fromClick)
            captured = true
            // Claim the responder chain for as long as we own the keyboard — `pressesBegan` has to
            // be delivered to us before it can keep Escape away from the system.
            anchorKeyResponder()
        } else {
            guard captured else { return }
            inputCapture?.setForwarding(false)
            captured = false
            // Hand the chain back: released means Escape is the system's again, and staying first
            // responder for a stream that no longer owns input would sit in front of SwiftUI focus.
            if isFirstResponder { resignFirstResponder() }
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
        guard let window = view.window, let scene = window.windowScene else { return false }
        return window.bounds.size == scene.screen.bounds.size
    }

    /// SpringBoard grants the lock only to a frontmost scene that fills its screen.
    private var sceneCanHoldPointerLock: Bool {
        view.window?.windowScene?.activationState == .foregroundActive && windowFillsScreen
    }

    /// Ask again for a lock SpringBoard declined or dropped, while the scene can hold one: one
    /// false→true edge, with the chain re-anchored in case a reparent broke the walk to us. A call
    /// during the edge folds into it. No-op when the lock is held or no longer wanted. Main queue.
    private func requestPointerLock() {
        guard wantsPointerLock, pointerLockEngaged() != true, sceneCanHoldPointerLock,
              !pointerLockSuppressed
        else { return }
        pointerLockSuppressed = true
        updatePointerLockChain()
        setNeedsUpdateOfPrefersPointerLocked()
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.pointerLockEdgeHold) { [weak self] in
            guard let self else { return }
            self.pointerLockSuppressed = false
            self.setNeedsUpdateOfPrefersPointerLocked()
        }
    }
    #endif

    deinit {
        observers.forEach(NotificationCenter.default.removeObserver(_:))
        presenter.stop() // invalidate the display link + stop the pipeline if stop() was missed
    }
}

#if os(iOS)
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
    var displayLayer: AVSampleBufferDisplayLayer {
        // swiftlint:disable:next force_cast
        layer as! AVSampleBufferDisplayLayer
    }

    #if os(iOS)
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
    /// `penEnabled`; without it the Pencil stays on the finger path exactly as before.
    var onPenBatch: (([PunktfunkPenSample]) -> Void)?
    /// The host advertised `HOST_CAP_PEN`, so Pencil input splits out of the finger path onto
    /// the pen plane — independent of the touch-input mode (drawing must not depend on it).
    var penEnabled = false
    /// The host injects wire touch contacts (`HOST_CAP2_TOUCH`). Off, the `touch` model's
    /// fingers drive the trackpad engine instead of vanishing on the host.
    var touchPassthroughEnabled = true
    /// Pencil proximity transitions (hover or contact) — the presenter's panel-rate boost.
    var onPenProximity: ((Bool) -> Void)?
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

    /// Wire touch ids per active direct UITouch; ids are reused after the touch ends.
    private var touchIDs: [ObjectIdentifier: UInt32] = [:]
    /// Live fingers that landed on a side bezel, for the passthrough dial opener
    /// ([`EdgeDial`]). Only in the `touch` model — every other model has the twist.
    private var edgeTracks: [ObjectIdentifier: EdgeTrack] = [:]
    /// GameStream button held per active indirect-pointer touch (one click/drag session);
    /// released when that touch ends.
    private var pointerButtons: [ObjectIdentifier: UInt32] = [:]
    /// Touch-driven mouse for the trackpad/pointer `TouchInputMode`s (see TouchMouse.swift).
    private lazy var touchMouse: TouchMouse = {
        let mouse = TouchMouse()
        mouse.send = { [weak self] event in self?.onTouchEvent?(event) }
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
    /// The finger route latched at gesture start — a Settings change mid-gesture applies to
    /// the NEXT touch, so one gesture never splits across input models.
    private var fingerRoute: TouchInputMode?
    /// The Apple Pencil pipeline (contacts + hover + squeeze/tap → pen samples).
    private lazy var pencil: PencilStream = {
        let stream = PencilStream()
        stream.send = { [weak self] batch in self?.onPenBatch?(batch) }
        stream.onProximity = { [weak self] near in self?.onPenProximity?(near) }
        stream.videoNorm = { [weak self] point in
            guard let h = self?.hostPoint(from: point) else { return nil }
            return (Float(h.x) / Float(max(h.w - 1, 1)), Float(h.y) / Float(max(h.h - 1, 1)))
        }
        return stream
    }()

    /// Release anything the touch-driven mouse holds and forget gesture state — session stop.
    func resetTouchInput() {
        touchMouse.reset()
        mutedMouse.reset()
        pencil.reset() // leaves range → the host lifts anything still inked
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
        if visible {
            becomeFirstResponder()
        } else if isFirstResponder {
            resignFirstResponder()
        }
    }
    #endif

    override init(frame: CGRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        #if os(iOS)
        isMultipleTouchEnabled = true
        // Button-less mouse/trackpad movement (no lock) arrives as hover, not touches —
        // forward it as absolute cursor moves so the host cursor tracks without a click held.
        addGestureRecognizer(
            UIHoverGestureRecognizer(target: self, action: #selector(handleHover)))
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
        // Pencil squeeze / double-tap → the pen plane's barrel buttons (no-op while
        // `penEnabled` is false — PencilStream ignores interactions out of range).
        let pencilInteraction = UIPencilInteraction()
        pencilInteraction.delegate = pencil
        addInteraction(pencilInteraction)
        #endif
        backgroundColor = .black
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not used") }

    #if os(iOS)
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

    /// Route direct fingers by the touch-input model, latched for the whole gesture:
    /// passthrough → real wire touches; trackpad/pointer/off → a TouchMouse gesture engine.
    private func forwardFingers(_ touches: Set<UITouch>, kind: TouchKind) {
        var mode = fingerRoute ?? TouchInputMode.current(settings)
        if mode == .touch, !touchPassthroughEnabled { mode = .trackpad }
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
            case .cancel: mouse.cancelled(touches)
            }
        }
        if touchIDs.isEmpty, touchMouse.isIdle, mutedMouse.isIdle { fingerRoute = nil }
    }

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

    /// Button-less mouse/trackpad movement (no lock) → absolute cursor move — unless it is a
    /// hovering PENCIL (`zOffset > 0`) on a pen-capable host, which becomes in-range pen
    /// samples (hover preview with distance/tilt/azimuth) instead of a cursor move.
    @objc private func handleHover(_ recognizer: UIHoverGestureRecognizer) {
        if penEnabled, pencil.maybeHover(recognizer, in: self) { return }
        switch recognizer.state {
        case .began, .changed:
            if let h = hostPoint(from: recognizer.location(in: self)) { onPointerMoveAbs?(h) }
        default:
            break
        }
    }

    /// Trackpad / wheel scroll → host scroll deltas. The translation is consumed each callback so
    /// the next is a fresh delta, and scales at ≈ one WHEEL notch per 10 pt of pan.
    ///
    /// Both axes pass through with their sign intact, which is what makes the stream follow the
    /// system's Natural Scrolling switch: UIKit has already applied that preference by the time it
    /// hands us a translation (it is what makes every UIScrollView on the device turn the right
    /// way), so the sign we get IS the user's choice, and the host's WHEEL convention agrees with
    /// it — +y is a wheel-forward notch, the one that moves content down. Negating y here, as this
    /// did, pinned the stream to traditional scrolling and inverted the setting for everyone on the
    /// default. macOS passes `NSEvent.scrollingDeltaY` through for exactly the same reason.
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
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
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
        let s = traitCollection.displayScale > 0 ? traitCollection.displayScale : UIScreen.main.scale
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
    /// 2 middle, 3 right, 4 X1/back, 5 X2/forward), so only those two swap. Without the 3…5 arms
    /// every button past the first two fell into the `else` and clicked LEFT on the host.
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

#if os(iOS)
// The soft keyboard's output → wire key events. UIKeyInput is deliberately minimal (no
// UITextInput): the stream needs keystrokes, not an editing buffer — insertions map through
// `SoftKeyMap` to US-positional VKs (with a VK_LSHIFT wrap for shifted characters) and
// characters outside the map (emoji, non-Latin scripts) are dropped, matching the wire's VK
// contract. Events ride the same `onTouchEvent` path as the touch-driven mouse, so they're
// gated on captureEnabled with everything else and can't leak past a trust prompt.
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
        for ch in text {
            guard let key = SoftKeyMap.vk(for: ch) else { continue }
            if key.shift { onTouchEvent?(.key(0xA0, down: true)) } // VK_LSHIFT
            onTouchEvent?(.key(key.vk, down: true))
            onTouchEvent?(.key(key.vk, down: false))
            if key.shift { onTouchEvent?(.key(0xA0, down: false)) }
        }
    }

    func deleteBackward() {
        guard GCKeyboard.coalesced == nil else { return } // see insertText
        onTouchEvent?(.key(0x08, down: true)) // VK_BACK
        onTouchEvent?(.key(0x08, down: false))
    }
}
#endif
#endif
