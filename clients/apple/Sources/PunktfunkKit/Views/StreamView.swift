// SwiftUI presentation: AVSampleBufferDisplayLayer fed straight from the punktfunk/1 connection.
//
// Stage-1 presenter (see README): the layer accepts *compressed* HEVC sample buffers and
// does hardware decode + display itself — fastest path to pixels, IOSurface-backed
// zero-copy on Apple silicon. Stage 2 (explicit VTDecompressionSession + CAMetalLayer)
// replaces this when we start tuning frame pacing / measuring glass-to-glass.
//
// The view also owns the input-capture state machine (Moonlight-style): capture is a
// deliberate, reversible state — engaged when the stream starts and when the user clicks
// into the video, released by ⌃⌥⇧Q (the cross-client Ctrl+Alt+Shift+Q), ⌘⎋, or focus
// loss, and NEVER engaged by mere app activation (the click that activates the window may
// be a title-bar drag or a resize — warping the cursor there is exactly the intrusiveness
// this design removes). While released, nothing is forwarded to the host and the local
// cursor is free.
//
// macOS-first (NSViewRepresentable); the iOS variant is the same layer under
// UIViewRepresentable.

#if os(macOS)
import AppKit
import AVFoundation
import PunktfunkShared
import SwiftUI
import os

/// Same diagnostic switch as InputCapture: PUNKTFUNK_INPUT_DEBUG=1 logs when the macOS
/// NSEvent mouse monitor (relative motion + buttons) is installed/removed, so the user can
/// confirm the new motion path is actually live for a session.
private let streamInputLog = ClientLog(category: "input")
private let streamInputDebug =
    ProcessInfo.processInfo.environment["PUNKTFUNK_INPUT_DEBUG"] == "1"

/// Hides the LOCAL cursor while captured. The host renders its own cursor, and the local
/// one both diverges from it (the host applies acceleration/clamping to our deltas) and
/// can wander out of the window — a click there would focus another app. So while captured
/// we do what Moonlight does: warp the cursor into the view, freeze it
/// (`CGAssociateMouseAndMouseCursorPosition(false)` — under which NSEvent mouseMoved/
/// dragged deltas become the relative motion StreamLayerView forwards), and hide it.
/// hide/unhide and associate are balanced via `captured`.
///
/// In the DESKTOP mouse model (absolute pointer, remote-desktop-sweep M1) this is a no-op:
/// the pointer stays free (entering and leaving the stream at will) and StreamLayerView
/// forwards ABSOLUTE positions instead; the local cursor is hidden only while over the view
/// (cursor rects). `disassociate` selects between the two; `release()` only undoes what
/// `capture` actually did.
private final class CursorCapture {
    private var captured = false
    /// Whether the engaged capture actually disassociated+hid (false in cursor-visible mode),
    /// so `release()` only restores when it must.
    private var disassociated = false

    /// Returns whether capture actually engaged. It can fail mid app-activation — the click
    /// that reactivates the app delivers `mouseDown` before the app is frontmost, and
    /// `CGAssociateMouseAndMouseCursorPosition` is refused then — so the caller must stay
    /// released and let the NEXT click retry, never latching a half-captured state. With
    /// `disassociate: false` (cursor-visible mode) it always engages — there is no grab to
    /// be refused, the cursor stays free and visible.
    func capture(in view: NSView, disassociate: Bool) -> Bool {
        guard !captured, let window = view.window, view.bounds.width > 0 else { return false }
        if disassociate {
            // Park the cursor mid-view so a click can't land in (and activate) another app.
            let rectOnScreen = window.convertToScreen(view.convert(view.bounds, to: nil))
            let primaryHeight = NSScreen.screens.first?.frame.height ?? 0
            CGWarpMouseCursorPosition(
                CGPoint(x: rectOnScreen.midX, y: primaryHeight - rectOnScreen.midY))
            guard CGAssociateMouseAndMouseCursorPosition(0) == .success else { return false }
            NSCursor.hide()
        }
        captured = true
        disassociated = disassociate
        return true
    }

    func release() {
        guard captured else { return }
        if disassociated {
            CGAssociateMouseAndMouseCursorPosition(1)
            NSCursor.unhide()
        }
        captured = false
        disassociated = false
    }
}

public struct StreamView: NSViewRepresentable {
    private let connection: PunktfunkConnection
    private let captureEnabled: Bool
    private let onCaptureChange: ((Bool) -> Void)?
    private let onDisconnectRequest: (() -> Void)?
    private let onFrame: (@Sendable (AccessUnit) -> Void)?
    private let onSessionEnd: (@Sendable () -> Void)?
    private let onResizeTarget: ((UInt32, UInt32) -> Void)?
    private let onDecodedSize: (@Sendable (Int, Int) -> Void)?
    private let endToEndMeter: LatencyMeter?

    /// `onFrame`/`onSessionEnd` fire on the pump thread — hop to the main actor for UI.
    /// `captureEnabled: false` disables input capture entirely while UI (e.g. a trust
    /// prompt) is layered over the stream; flipping it to true auto-engages capture
    /// once. `onCaptureChange` (main thread) reports engage/release — drive the HUD's
    /// "click to capture" / "⌃⌥⇧Q releases" hint with it. `onDisconnectRequest` (main
    /// thread) fires on the reserved ⌃⌥⇧D combo while captured — the owner ends the
    /// session (released, the same combo reaches the Stream menu instead).
    /// `endToEndMeter` records capture→on-glass for the A/V sync loop.
    public init(
        connection: PunktfunkConnection,
        captureEnabled: Bool = true,
        onCaptureChange: ((Bool) -> Void)? = nil,
        onDisconnectRequest: (() -> Void)? = nil,
        // Call-site parity with the touch platforms: macOS has no twist (the ring opens from
        // the keyboard there, a later work package), so the parameter is accepted and unused.
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
        self.onDisconnectRequest = onDisconnectRequest
        self.onFrame = onFrame
        self.onSessionEnd = onSessionEnd
        self.onResizeTarget = onResizeTarget
        self.onDecodedSize = onDecodedSize
        self.endToEndMeter = endToEndMeter
    }

    public func makeNSView(context: Context) -> StreamLayerView {
        let view = StreamLayerView()
        view.onCaptureChange = onCaptureChange
        view.onDisconnectRequest = onDisconnectRequest
        view.captureEnabled = captureEnabled
        view.endToEndMeter = endToEndMeter
        view.onResizeTarget = onResizeTarget
        view.onDecodedSize = onDecodedSize
        view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        return view
    }

    public func updateNSView(_ view: StreamLayerView, context: Context) {
        view.onCaptureChange = onCaptureChange
        view.onDisconnectRequest = onDisconnectRequest
        view.captureEnabled = captureEnabled
        view.endToEndMeter = endToEndMeter
        view.onResizeTarget = onResizeTarget
        view.onDecodedSize = onDecodedSize
        // SwiftUI reuses the NSView across state changes — repoint the pump only when the
        // connection identity actually changed.
        if view.connection !== connection {
            view.start(connection: connection, onFrame: onFrame, onSessionEnd: onSessionEnd)
        }
    }

    public static func dismantleNSView(_ view: StreamLayerView, coordinator: ()) {
        view.stop()
    }
}

public final class StreamLayerView: NSView {
    private let displayLayer = AVSampleBufferDisplayLayer()
    /// Capture→on-glass for the A/V sync loop while the stage-2 presenter runs. Read at start().
    var endToEndMeter: LatencyMeter?
    /// The shared presenter stack: stage-2 (CAMetalLayer sublayer + display link) with the
    /// stage-1 StreamPump → displayLayer path as the Metal-unavailable / DEBUG fallback.
    private let presenter = SessionPresenter()
    public private(set) var connection: PunktfunkConnection?
    /// Match-window resize follower (C3) — non-nil while a session is active AND the `matchWindow`
    /// setting is on (DEFAULT on, for pixel-exact windowed streaming); fed the view's physical-pixel
    /// size on every relayout so the host mode tracks the window (1:1, no presenter resample).
    private var matchFollower: MatchWindowFollower?
    /// Last decoded frame size fed into the presenter's aspect-fit. A new-mode IDR after a resize
    /// re-fits the metal sublayer to the REAL content aspect here — `layout()` only re-runs on a
    /// bounds change and a resize-END has none, so without this the layer keeps its pre-resize aspect
    /// and the shader stretches the new frame into it (black bars + squish). Main-thread only.
    private var lastDecodedContentSize: CGSize?
    /// This screen's below-the-notch mode, as of the last layout — the one input to `videoBounds`
    /// too expensive to read per mouse event (see `layoutPresenter`). Main-thread only.
    private var safeModePixels: (width: Int, height: Int)?
    private let cursorCapture = CursorCapture()
    private var inputCapture: InputCapture?
    private var appObservers: [NSObjectProtocol] = []
    private var windowObservers: [NSObjectProtocol] = []
    /// Local NSEvent monitor carrying relative mouse MOTION + BUTTONS to the host while
    /// captured (GCMouse's own delivery proved unreliable on macOS — see InputCapture).
    /// Installed on engage, removed on release; nil while not captured.
    private var mouseEventMonitor: Any?
    /// The window's `acceptsMouseMovedEvents` value before client-side-cursor capture raised
    /// it (nil = not raised by us); restored on release so we leave the window as we found it.
    private var savedAcceptsMouseMoved: Bool?

    /// Whether input capture is currently engaged (cursor hidden+frozen, mouse/keyboard
    /// forwarded). Main-thread only.
    public private(set) var captured = false
    /// The ring released a captured mouse (see the `.punktfunkRingOpen` observer) and owes it back
    /// when the ring closes. False whenever capture was already released before it opened.
    private var ringHeldCapture = false

    /// Desktop (absolute) mouse model — remote-desktop-sweep M1: when true the pointer is
    /// never disassociated (it enters and leaves the stream freely) and the mouse monitor
    /// forwards ABSOLUTE positions through the letterbox; the local cursor is hidden only
    /// while over this view (cursor rects — the host's composited cursor, tracking our
    /// sends, is the one you see) and reappears the moment it leaves. When false the
    /// captured/disassociated relative path runs unchanged. Initialized at session start
    /// from the `mouseMode` setting gated by the host's resolved compositor (gamescope's
    /// EIS is relative-only — absolute sends would be dropped, so it pins to capture);
    /// flipped live by ⌃⌥⇧M. A live flip re-engages capture in the new model so
    /// disassociation + the abs/rel choice swap atomically. Main-thread only.
    private var desktopMouse = false
    /// Cursor channel (M2): the host forwards shape/state and WE draw the pointer. Active
    /// when the Welcome carried `HOST_CAP_CURSOR` (only sessions that advertised the client
    /// cap get it). Shapes cache by serial; state is latest-wins. Main-thread only.
    private var cursorChannelActive = false
    /// A forwarded host cursor shape, cached RAW (not as a finished `NSCursor`) so the pointer can be
    /// (re)built at the CURRENT video-fit scale — see `scaledCursor`. The host forwards the bitmap in
    /// host FRAMEBUFFER pixels, whose size tracks the host's display scaling (32 px at 100%, 96 px at
    /// 300% DPI); scaling by the video fit keeps the pointer sized to the streamed desktop at any host
    /// scaling instead of ballooning on a high-DPI host.
    private struct HostCursorShape {
        let cg: CGImage
        let width: Int
        let height: Int
        let hotX: Int
        let hotY: Int
    }
    private var hostCursors: [UInt32: HostCursorShape] = [:]
    /// The last shape actually worn. State (`0xD0`, a per-frame datagram) announces a new serial the
    /// moment the host QUEUES its bitmap on the reliable control stream, so the client routinely
    /// knows a serial before it holds the pixels — and the shape ring drops the NEWEST under burst
    /// (`CURSOR_SHAPE_QUEUE`), which the host never re-sends because it only sends on a serial
    /// CHANGE. Both leave `hostCursors[serial]` empty; wearing the previous pointer through that
    /// gap degrades it to a briefly-stale shape instead of blinking the pointer out of existence.
    private var lastWornShape: HostCursorShape?
    private var cursorState: PunktfunkConnection.CursorStateEvent?
    /// Last `CursorRenderMode.clientDraws` told to the host (the §8 mid-stream render flip);
    /// nil = nothing sent yet. Edge-detected by [`reconcileCursorRender`] from the live mouse
    /// model, so the chord, engage/release, and session start all reconcile through one path.
    private var sentClientDraws: Bool?
    /// M3 hint tracking: edge-triggered so a manual ⌃⌥⇧M isn't fought — the override latch
    /// holds until the HOST's intent next changes.
    private var lastHint: Bool?
    private var hintOverride = false
    /// One-shot auto-engage request (stream start, trust confirmed) — attempted as soon
    /// as the view is in a window with real bounds, then dropped, so it can never fire
    /// surprisingly later (e.g. on a resize).
    private var pendingAutoCapture = false

    /// Reports engage/release on the main thread.
    public var onCaptureChange: ((Bool) -> Void)?

    /// Fired (main thread) when the captured-state ⌃⌥⇧D combo asks to end the session — the
    /// view can't do that itself (the connection's owner disconnects).
    public var onDisconnectRequest: (() -> Void)?

    /// Resize overlay signals (design/midstream-resolution-resize.md client UX): `onResizeTarget`
    /// (main thread, via the follower) fires the instant the window starts steering toward a new
    /// size; `onDecodedSize` (PUMP thread) fires when a new-mode IDR's dims land. The owner drives
    /// the blur+spinner from these — set before `start()`.
    public var onResizeTarget: ((UInt32, UInt32) -> Void)?
    public var onDecodedSize: (@Sendable (Int, Int) -> Void)?

    /// Main-thread only. False = input capture disabled outright (UI layered over the
    /// stream); flipping to true auto-engages once.
    public var captureEnabled = true {
        didSet {
            guard captureEnabled != oldValue else { return }
            if captureEnabled {
                requestAutoCapture()
            } else {
                releaseCapture()
            }
        }
    }

    public override init(frame: NSRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        layer = displayLayer // layer-hosting: assign before wantsLayer
        wantsLayer = true
        // Focus loss releases capture. Becoming active does NOT re-engage: the click
        // that activates the window may be on the title bar (a drag) or a resize edge —
        // the user clicks into the video (or hits ⌘⎋) when they want capture back.
        appObservers.append(NotificationCenter.default.addObserver(
            forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.releaseCapture()
        })
        // The Stream menu's "Release Mouse" item (⌃⌥⇧Q's discoverable menu-bar surface). Only
        // the key window's stream may act — same ownership rule as the ⌘⎋ toggle. (While
        // captured the combo never reaches the menu — InputCapture's monitor handles it — so
        // in practice this fires only as a not-captured no-op; wired for honesty.)
        appObservers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkReleaseCapture, object: nil, queue: .main
        ) { [weak self] _ in
            guard let self, self.window?.isKeyWindow == true else { return }
            self.releaseCapture()
        })
        // The quick-action ring is SwiftUI ABOVE this layer, so a grabbed pointer can never click
        // one of its buttons — release for as long as it is up, and take capture back when it
        // closes. Without the second half a glance at the ring would cost the game its pointer
        // lock and a click to get it back, which is the cost the ring exists to avoid.
        appObservers.append(NotificationCenter.default.addObserver(
            forName: .punktfunkRingOpen, object: nil, queue: .main
        ) { [weak self] note in
            guard let self, self.window?.isKeyWindow == true else { return }
            if (note.object as? NSNumber)?.boolValue == true {
                guard self.captured else { return }
                self.ringHeldCapture = true
                self.releaseCapture()
            } else if self.ringHeldCapture {
                self.ringHeldCapture = false
                // Guarded inside: a ring closed BY "End stream" has no session to capture for,
                // and engageCapture's own `connection != nil` gate declines it.
                self.engageCapture(fromClick: false)
            }
        }
        )
    }

    public required init?(coder: NSCoder) { fatalError("not used") }

    public override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        windowObservers.forEach(NotificationCenter.default.removeObserver(_:))
        windowObservers.removeAll()
        guard let window else {
            releaseCapture()
            return
        }
        // ⌘-key-equivalents stay live while captured, so Settings (⌘,), a new window
        // (⌘N), or Minimize (⌘M) can take key status without the APP resigning active —
        // capture must release then too, or the new window inherits a hidden, frozen
        // cursor and its local typing is double-delivered to the host.
        for name in [NSWindow.didResignKeyNotification, NSWindow.didMiniaturizeNotification] {
            windowObservers.append(NotificationCenter.default.addObserver(
                forName: name, object: window, queue: .main
            ) { [weak self] _ in
                self?.releaseCapture()
            })
        }
        // Becoming key RETRIES a still-pending session-start auto-capture — the case where a
        // session began (reconnect) while this window wasn't key yet, so engageCapture(fromClick:
        // false) was refused by its key-window guard and, with no retry, capture stayed off and
        // input dead. This is a no-op once capture engaged (pendingAutoCapture is cleared) and
        // after a manual ⌘⎋/focus-loss release (the flag is already false), so it does NOT
        // resurrect the deliberately-rejected "auto-grab on every activation" behavior.
        windowObservers.append(NotificationCenter.default.addObserver(
            forName: NSWindow.didBecomeKeyNotification, object: window, queue: .main
        ) { [weak self] _ in
            self?.attemptPendingCapture()
        })
        attemptPendingCapture()
    }

    public override func layout() {
        super.layout()
        attemptPendingCapture() // bounds become real here on first presentation
        layoutPresenter() // keep the stage-2 sublayer aspect-fit to the view
    }

    public override func setFrameSize(_ newSize: NSSize) {
        super.setFrameSize(newSize)
        // `layout()` isn't guaranteed on a manual-frame (no-Auto-Layout) live resize, so the
        // stage-2 metal sublayer's frame could stay at the old size while the view grows —
        // the compositor then upscales a too-small layer and the video turns blocky. Re-fit
        // here too so it always tracks the window's size (no stale upscale).
        layoutPresenter()
    }

    // MARK: - Capture state machine

    /// Clicking into the video engages capture; that click is local (engagement), so
    /// InputCapture suppresses its press/release toward the host. Clicks while captured
    /// are the host's (GC forwards them) — nothing to do here.
    public override func mouseDown(with event: NSEvent) {
        if streamInputDebug {
            streamInputLog.debug(
                "mouseDown: captureEnabled=\(self.captureEnabled, privacy: .public) captured=\(self.captured, privacy: .public)")
        }
        if captureEnabled, !captured {
            engageCapture(fromClick: true)
            return
        }
        super.mouseDown(with: event)
    }

    /// A click from another app counts (one click into the video captures, not two).
    public override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    /// The engage click is complete — drop its suppression latch (see InputCapture;
    /// guards against GC delivering both halves of the click before our mouseDown).
    public override func mouseUp(with event: NSEvent) {
        inputCapture?.endClickSuppression()
        super.mouseUp(with: event)
    }

    /// Scroll is forwarded from here, not from GCMouse: trackpad/Magic Mouse gestures
    /// never reach GameController's scroll dpad. While captured the cursor is parked
    /// mid-view, so this view receives every scroll event. Precise (gesture) deltas are
    /// pixels — ~0.1 wheel notch per pixel (SDL's factor) → ×12 for WHEEL_DELTA(120);
    /// classic wheels report lines, one notch = ±1 → ×120. Signs pass through as-is,
    /// preserving the user's local (natural-)scrolling preference. The precise flag rides
    /// along so the host scrolls a gesture by the distance the fingers moved, instead of
    /// reading each 10 px as a wheel click and expanding it into a full scroll step.
    public override func scrollWheel(with event: NSEvent) {
        guard captured, let inputCapture else {
            super.scrollWheel(with: event)
            return
        }
        let precise = event.hasPreciseScrollingDeltas
        let scale: Float = precise ? 12 : 120
        inputCapture.sendScroll(
            dx: Float(event.scrollingDeltaX) * scale,
            dy: Float(event.scrollingDeltaY) * scale,
            precise: precise)
    }

    // While captured, the view is first responder and SENDS key events to the host straight
    // from NSEvent — GCKeyboard delivery proved unreliable on macOS (the same GameController
    // quirk that killed GCMouse motion, fixed in e414ec0), so the macOS GCKeyboard send path
    // is disabled and NSEvent is the single source. We map NSEvent.keyCode (a Carbon virtual
    // keycode) → Windows VK and forward via InputCapture.sendKey, then CONSUME (return without
    // super) to stop the responder chain's "unhandled keyDown" beep. Keys with no VK mapping
    // are still consumed while captured so they don't beep either. The ⌘⎋ toggle's Esc is
    // swallowed upstream by InputCapture's keyDown monitor (suppressedVK), so it never gets here
    // as a send — and so are ⌘ combos generally while captured, which that monitor forwards to the
    // host itself (`forwardsCommandChord`) rather than letting a menu key equivalent claim them.
    // Modifier keys never fire keyDown/keyUp — they come through flagsChanged below.
    public override var acceptsFirstResponder: Bool { true }
    // A click after the app was inactive (Cmd-Tab away and back) must reach mouseDown so the
    // user can re-capture — the deliberate design is that becoming active does NOT auto-grab;
    // you click into the video. Default NSViews aren't key-view candidates, which can drop
    // that first click; opting in keeps the view a valid click/responder target.
    public override var canBecomeKeyView: Bool { true }
    public override func keyDown(with event: NSEvent) {
        if captured {
            if let ic = inputCapture, let vk = InputCapture.keyCodeToVK[event.keyCode] {
                ic.sendKey(vk, down: true) // autorepeat (event.isARepeat) passes through — fine for VK
            }
            return // consume even unmapped keys while captured (no beep)
        }
        super.keyDown(with: event)
    }
    public override func keyUp(with event: NSEvent) {
        if captured {
            if let ic = inputCapture, let vk = InputCapture.keyCodeToVK[event.keyCode] {
                ic.sendKey(vk, down: false)
            }
            return
        }
        super.keyUp(with: event)
    }
    /// Modifier keys (shift/control/option/command) arrive ONLY as flagsChanged on macOS,
    /// never keyDown/keyUp — the changed key is `event.keyCode`; InputCapture resolves the
    /// down-vs-up direction from the flags (diffing the device-dependent flag bits alone
    /// proved unreliable — some keyboards omit them, which silently dropped Control).
    public override func flagsChanged(with event: NSEvent) {
        if captured, let inputCapture {
            inputCapture.handleFlagsChanged(
                keyCode: event.keyCode, rawFlags: UInt(event.modifierFlags.rawValue))
            return
        }
        super.flagsChanged(with: event)
    }

    private func requestAutoCapture() {
        pendingAutoCapture = true
        attemptPendingCapture()
    }

    private func attemptPendingCapture() {
        guard pendingAutoCapture, window != nil, bounds.width > 0 else { return }
        engageCapture(fromClick: false)
        // Clear the one-shot only once it ACTUALLY engaged. If the engage was refused — the
        // app/window isn't key yet (common right after a reconnect), or the cursor grab raced
        // app activation — leave it armed so didBecomeKey (or the next layout pass) retries.
        // This stays scoped to session start: a later manual release (⌘⎋, focus loss) doesn't
        // re-arm it, so it never resurrects auto-grab-on-activation.
        if captured { pendingAutoCapture = false }
    }

    private func engageCapture(fromClick: Bool) {
        // A click is explicit intent AND may arrive mid-activation (acceptsFirstMouse:
        // NSApp.isActive / isKeyWindow are still false for the click coming in from
        // another app) — only the auto-engage paths require already-held key status.
        // `connection != nil` is the session-active gate (presenter internals are opaque here).
        guard captureEnabled, !captured, let connection, window != nil,
              fromClick || (NSApp.isActive && window?.isKeyWindow == true)
        else { return }
        // Per-client access §7 — never capture what can't land: a Controller-only or
        // View-only session gets NO mouse/keyboard grab (its clicks stay local UI clicks),
        // instead of a frozen cursor over input the host silently drops. Live grants, so a
        // mid-session re-grant makes the next click work; the revoke direction is released
        // by the session model's access tick.
        guard connection.canSendPointer || connection.canSendKeyboard else { return }
        // If the cursor grab is refused (e.g. the reactivating click arrives before the app is
        // frontmost), stay released so the NEXT click retries — never latch captured=true over
        // a free cursor, which would make mouseDown's `!captured` guard reject every later click.
        // In the desktop mouse model there is no grab (the pointer stays free) — capture
        // always engages and the monitor forwards absolute positions instead. A session
        // whose grants exclude POINTER also keeps its cursor free (keyboard-only capture):
        // freezing a pointer whose motion cannot land would just trap the user's mouse.
        guard cursorCapture.capture(
            in: self, disassociate: !desktopMouse && connection.canSendPointer)
        else { return }
        inputCapture?.setForwarding(true, suppressClick: fromClick)
        // Install AFTER the warp + setForwarding: the engage warp generates no forwarded
        // delta (the monitor isn't up yet), and the engage click's suppression latch is
        // already armed, so the monitor only ever sees genuine post-engage input.
        installMouseMonitor()
        captured = true
        window?.makeFirstResponder(self)
        window?.invalidateCursorRects(for: self) // desktop model: hide-over-view engages
        notifyCaptureChange(true)
        reconcileCursorRender()
    }

    private func releaseCapture() {
        guard captured else { return }
        removeMouseMonitor()
        cursorCapture.release()
        inputCapture?.setForwarding(false)
        captured = false
        window?.invalidateCursorRects(for: self)
        notifyCaptureChange(false)
        reconcileCursorRender() // released ⇒ the host composites the pointer again
    }

    /// A fully transparent cursor for the desktop mouse model's hide-over-view rect —
    /// an empty 1×1 image draws nothing.
    private static let invisibleCursor = NSCursor(
        image: NSImage(size: NSSize(width: 1, height: 1)), hotSpot: .zero)

    /// Desktop mouse model: the local cursor is hidden while over the stream (the host's
    /// composited cursor, tracking our absolute sends, is the one you see) and reappears
    /// the moment it leaves the view — AppKit applies/removes the rect's cursor for us,
    /// so there is no hide/unhide balancing to get wrong. Capture model instead hides
    /// globally via `CursorCapture` (the pointer can't leave the view there).
    override public func resetCursorRects() {
        if captured && desktopMouse {
            // Cursor channel active: wear the HOST's pointer shape (it is no longer in the
            // video); a HIDDEN host pointer (or nothing seen yet at all) = invisible. Without the
            // channel, M1 behavior: invisible local cursor, the composited host cursor is the
            // visible one.
            //
            // A visible pointer whose announced serial has no bitmap yet falls back to the last
            // worn shape (see `lastWornShape`) rather than to `invisibleCursor`. That case is
            // routine, not degenerate — state outruns its bitmap on every single shape change —
            // and treating it as "hide the pointer" made the pointer VANISH over anything whose
            // shape arrived late or got dropped, with no recovery until the next change. Only
            // `st.visible == false` may hide the pointer; a missing bitmap may not.
            if cursorChannelActive, let st = cursorState, st.visible,
               let shape = hostCursors[st.serial] ?? lastWornShape {
                lastWornShape = shape
                addCursorRect(bounds, cursor: scaledCursor(shape))
            } else {
                addCursorRect(bounds, cursor: Self.invisibleCursor)
            }
        } else {
            super.resetCursorRects()
        }
    }

    /// Tell the host who renders the pointer (the §8 mid-stream render flip). The host may
    /// composite one into the video ONLY while we are holding a grabbed, hidden pointer — the
    /// capture model, engaged. That is the one state with no local cursor on screen.
    ///
    /// Every other state leaves a normal OS cursor visible over the video: the desktop model
    /// draws it wearing the host's shape, and a RELEASED view shows the plain arrow. A
    /// host-composited pointer then appears *underneath* it as a second cursor — and, because a
    /// released view forwards no motion, one that never moves. On glass that reads as a frozen
    /// duplicate stuck wherever the host pointer was last left (verified: `client_draws=false
    /// blended=true live=(-1, 622)` — parked on the streamed output's left edge while the user
    /// moved their own cursor around freely).
    ///
    /// So "released" counts as WE draw it: the host stops compositing, the client keeps
    /// receiving shape/state over the channel (the forwarder only ticks on this side of the
    /// flip), and re-engaging is seamless. One edge-detected reconciler, called from every
    /// transition (chord, engage/release, session start).
    private func reconcileCursorRender() {
        guard cursorChannelActive, let connection else { return }
        let clientDraws = !captured || desktopMouse
        guard sentClientDraws != clientDraws else { return }
        sentClientDraws = clientDraws
        connection.setCursorRender(clientDraws: clientDraws)
    }

    /// Flip the mouse model with the atomic release/re-engage swap; `reappearAt` (host video
    /// px — the M3 hand-back position) warps the local pointer so leaving relative lands the
    /// cursor exactly where the host last had it.
    private func setDesktopMouse(_ on: Bool, reappearAt: (x: Int32, y: Int32)?) {
        guard desktopMouse != on else { return }
        let wasCaptured = captured
        if wasCaptured { releaseCapture() }
        desktopMouse = on
        if wasCaptured { engageCapture(fromClick: false) }
        window?.invalidateCursorRects(for: self)
        if on, let p = reappearAt, let sp = cgScreenPoint(forHostX: p.x, p.y) {
            CGWarpMouseCursorPosition(sp)
        }
        reconcileCursorRender()
    }

    /// The single cursor pull thread (both planes share the connection's cursor lock):
    /// latest-wins state at a short timeout + a non-blocking shape poll per iteration.
    /// Exits when the connection closes; events hop to main where all cursor state lives.
    private func startCursorPump(_ connection: PunktfunkConnection) {
        let thread = Thread { [weak self] in
            while true {
                do {
                    var newest: PunktfunkConnection.CursorStateEvent?
                    if let st = try connection.nextCursorState(timeoutMs: 100) {
                        newest = st
                        while let more = try connection.nextCursorState(timeoutMs: 0) {
                            newest = more // drain — latest wins
                        }
                    }
                    while let shape = try connection.nextCursorShape(timeoutMs: 0) {
                        DispatchQueue.main.async { self?.applyCursorShape(shape) }
                    }
                    if let st = newest {
                        DispatchQueue.main.async { self?.applyCursorState(st) }
                    }
                } catch {
                    return // connection closed — the session is over
                }
                if self == nil { return }
            }
        }
        thread.name = "pf-cursor-pump"
        thread.start()
    }

    private func applyCursorShape(_ ev: PunktfunkConnection.CursorShapeEvent) {
        guard let shape = Self.makeShape(ev) else {
            // Truthful only because `resetCursorRects` falls back to `lastWornShape`: before that,
            // a rejection here left the announced serial with no bitmap and HID the pointer.
            streamInputLog.warning("cursor shape rejected (\(ev.width)x\(ev.height)) — keeping the previous cursor")
            return
        }
        if hostCursors.count >= 64 { hostCursors.removeAll() } // degenerate host: reset
        hostCursors[ev.serial] = shape
        if cursorState?.serial == ev.serial {
            window?.invalidateCursorRects(for: self)
        }
    }

    private func applyCursorState(_ ev: PunktfunkConnection.CursorStateEvent) {
        let prev = cursorState
        cursorState = ev
        if prev?.visible != ev.visible || prev?.serial != ev.serial {
            window?.invalidateCursorRects(for: self)
        }
        // M3 host-driven auto-flip is DISABLED: `relative_hint` is derived from host cursor
        // VISIBILITY, and Windows hides the pointer for ordinary desktop activity (clicking,
        // typing) — not just when a game grabs it. Acting on those transients flipped
        // desktop→capture→desktop, which warped the cursor to view-centre and flushed held
        // buttons (a spurious button-up ~200 ms into every press → broke window drags). Until
        // the host exposes a real pointer-LOCK signal (ClipCursor/raw-input, not visibility),
        // the mouse model is user-driven only (⌃⌥⇧M). The hint still rides the wire, unused.
        _ = (lastHint, hintOverride)
    }

    /// Decode a forwarded straight-alpha RGBA shape into a CGImage + hotspot. The on-screen SIZE is
    /// NOT baked in here — it is applied per-use in `scaledCursor` from the live video-fit scale, so
    /// the same shape re-fits across window resizes / retina moves without a re-forward.
    private static func makeShape(_ ev: PunktfunkConnection.CursorShapeEvent) -> HostCursorShape? {
        let (w, h) = (ev.width, ev.height)
        guard w > 0, h > 0, ev.rgba.count >= w * h * 4,
              let provider = CGDataProvider(data: ev.rgba as CFData),
              let cg = CGImage(
                  width: w, height: h, bitsPerComponent: 8, bitsPerPixel: 32,
                  bytesPerRow: w * 4,
                  space: CGColorSpace(name: CGColorSpace.sRGB) ?? CGColorSpaceCreateDeviceRGB(),
                  bitmapInfo: CGBitmapInfo(rawValue: CGImageAlphaInfo.last.rawValue),
                  provider: provider, decode: nil, shouldInterpolate: false,
                  intent: .defaultIntent)
        else { return nil }
        return HostCursorShape(
            cg: cg, width: w, height: h,
            hotX: min(ev.hotX, w - 1), hotY: min(ev.hotY, h - 1))
    }

    /// Points-per-host-pixel: the exact factor the video frame is placed into the view at (the same
    /// placement `hostPoint`/`cgScreenPoint` use). The host forwards the pointer bitmap in host
    /// framebuffer pixels — the mode we drive is in the client's BACKING pixels, so on retina this is
    /// ~1/backingScale and the pointer lands at its TRUE size relative to the streamed desktop
    /// (crisp, 1:1 with the video) rather than the 2×-inflated pixel-as-points it used to be. Because
    /// the bitmap grows with the host's display scaling (96 px at 300% DPI), scaling by this is what
    /// keeps a high-DPI host from forwarding a giant pointer. Falls back to 1 before the first
    /// mode/layout.
    /// The size the picture is actually being drawn at, in host pixels: the DECODED frame's, not
    /// the negotiated mode's. The two disagree whenever a host correctively acks a different mode
    /// (Windows falls back to an advertised one), and the presenter aspect-fits to the decoded
    /// size — so mapping input through the mode would letterbox against a different rectangle and
    /// offset every click for the whole session. Falls back to the mode before the first frame.
    private func hostContentSize() -> (width: UInt32, height: UInt32) {
        if let decoded = lastDecodedContentSize, decoded.width > 0, decoded.height > 0 {
            return (UInt32(decoded.width), UInt32(decoded.height))
        }
        let mode = connection?.currentMode() ?? (width: 0, height: 0, refreshHz: 0)
        return (mode.width, mode.height)
    }

    /// The rect the picture is fit into — `bounds`, except for a full-screen session whose mode is
    /// exactly this screen's below-the-notch mode, which is trimmed to sit under the camera
    /// housing (see `SafeDisplay.videoBox`). EVERY measurement against the picture reads this:
    /// the presenter's fit, the pointer mapping both ways, and the cursor scale.
    private var videoBounds: CGRect {
        guard let window, window.styleMask.contains(.fullScreen), let screen = window.screen
        else { return bounds }
        let content = hostContentSize()
        guard content.width > 0, content.height > 0 else { return bounds }
        return SafeDisplay.videoBox(
            bounds: bounds, topInsetPoints: Double(screen.safeAreaInsets.top),
            content: (Int(content.width), Int(content.height)),
            safeMode: safeModePixels)
    }

    /// Where the picture sits in `videoBounds`: the presenter's placement in backing pixels, the
    /// points→pixels scale, and the frame size. Every pointer mapping reads this, so a click lands on
    /// the pixel drawn there.
    private func videoPlacement()
        -> (placement: VideoPlacement, box: CGRect, scale: CGFloat, width: UInt32, height: UInt32)? {
        guard let connection else { return nil }
        let content = hostContentSize()
        let box = videoBounds
        let scale = window?.backingScaleFactor ?? 1
        guard content.width > 0, content.height > 0, box.width > 0, box.height > 0 else { return nil }
        let p = VideoFit(name: connection.settings.videoFit).place(
            view: (Int((box.width * scale).rounded()), Int((box.height * scale).rounded())),
            frame: (Int(content.width), Int(content.height)))
        return p.isEmpty ? nil : (p, box, scale, content.width, content.height)
    }

    /// Points per host pixel at the placement's scale. Stretch keeps the cursor's own shape.
    private func cursorFitScale() -> CGFloat {
        guard let v = videoPlacement() else { return 1 }
        return CGFloat(min(v.placement.scaleX, v.placement.scaleY)) / v.scale
    }

    /// Build the `NSCursor` for a cached shape at the CURRENT video-fit scale (see `cursorFitScale`).
    /// Both the image size and the hotspot scale together so the click point stays true.
    private func scaledCursor(_ shape: HostCursorShape) -> NSCursor {
        let scale = cursorFitScale()
        let sw = max(1, (CGFloat(shape.width) * scale).rounded())
        let sh = max(1, (CGFloat(shape.height) * scale).rounded())
        let image = NSImage(cgImage: shape.cg, size: NSSize(width: sw, height: sh))
        let hot = NSPoint(
            x: min(CGFloat(shape.hotX) * scale, sw - 1),
            y: min(CGFloat(shape.hotY) * scale, sh - 1))
        return NSCursor(image: image, hotSpot: hot)
    }

    /// Host video px → CG GLOBAL screen coordinates (top-left origin, the
    /// `CGWarpMouseCursorPosition` convention `CursorCapture` established) through the placement —
    /// the inverse direction of `hostPoint(from:)`. A host point cropped away lands on the edge.
    private func cgScreenPoint(forHostX hx: Int32, _ hy: Int32) -> CGPoint? {
        guard let window, let v = videoPlacement() else { return nil }
        let px = v.placement.view(fromFrame: CGPoint(x: Int(hx), y: Int(hy)))
        // The placement counts rows from the box's top; AppKit's box origin is its bottom.
        let boxTop = bounds.height - v.box.maxY
        let pTop = CGPoint(x: v.box.minX + px.x / v.scale, y: boxTop + px.y / v.scale)
        let inView = CGPoint(x: pTop.x, y: bounds.height - pTop.y)
        let inWindow = convert(inView, to: nil)
        let onScreen = window.convertPoint(toScreen: inWindow)
        let primaryHeight = NSScreen.screens.first?.frame.height ?? 0
        return CGPoint(x: onScreen.x, y: primaryHeight - onScreen.y)
    }

    /// A single local monitor for motion + buttons, installed only while captured. A local
    /// monitor is more robust than view overrides for relative motion: it sidesteps the
    /// `window.acceptsMouseMovedEvents`/tracking-area/responder-chain requirements, and
    /// since the cursor is frozen mid-view while captured every such event belongs here.
    /// ALL four motion types are covered so motion keeps flowing during a button-held drag,
    /// not just `.mouseMoved`. NSEvent deltas under disassociation are OS-pointer-
    /// acceleration-applied (not raw HID) — what Moonlight's macOS client ships; if the
    /// host re-accelerates there's mild double-acceleration, acceptable and fixable later
    /// via IOHID. Events are returned (not swallowed): the cursor is frozen, so they're
    /// inert locally.
    ///
    /// In the desktop mouse model the cursor is NOT frozen, so bare `.mouseMoved` events are
    /// only generated while `window.acceptsMouseMovedEvents` is true — we enable it here and
    /// restore it on removal so absolute hover-motion keeps flowing without a click held.
    private func installMouseMonitor() {
        guard mouseEventMonitor == nil else { return }
        if desktopMouse {
            savedAcceptsMouseMoved = window?.acceptsMouseMovedEvents
            window?.acceptsMouseMovedEvents = true
        }
        mouseEventMonitor = NSEvent.addLocalMonitorForEvents(matching: [
            .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged,
            .leftMouseDown, .leftMouseUp, .rightMouseDown, .rightMouseUp,
            .otherMouseDown, .otherMouseUp,
        ]) { [weak self] event in
            guard let self, self.captured, let ic = self.inputCapture else { return event }
            switch event.type {
            case .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged:
                if self.desktopMouse {
                    // Desktop mouse model: forward the ABSOLUTE position (mapped through the
                    // aspect-fit letterbox into host pixels), the same path the iPad pointer
                    // fallback uses. Events in the letterbox bars are dropped (nil host point).
                    if let p = self.hostPoint(from: event) {
                        ic.sendMouseAbs(x: p.x, y: p.y, surfaceWidth: p.w, surfaceHeight: p.h)
                    }
                } else {
                    ic.sendMotion(dx: Float(event.deltaX), dy: Float(event.deltaY)) // no y-negation
                }
            case .leftMouseDown: ic.sendMouseButton(1, pressed: true)
            case .leftMouseUp: ic.sendMouseButton(1, pressed: false)
            case .rightMouseDown: ic.sendMouseButton(3, pressed: true)
            case .rightMouseUp: ic.sendMouseButton(3, pressed: false)
            case .otherMouseDown: ic.sendMouseButton(self.wireButton(for: event), pressed: true)
            case .otherMouseUp: ic.sendMouseButton(self.wireButton(for: event), pressed: false)
            default: break
            }
            return event
        }
        if streamInputDebug { streamInputLog.debug("mouse NSEvent monitor installed (capture engaged)") }
    }

    private func removeMouseMonitor() {
        if let monitor = mouseEventMonitor {
            NSEvent.removeMonitor(monitor)
            mouseEventMonitor = nil
            if streamInputDebug { streamInputLog.debug("mouse NSEvent monitor removed (capture released)") }
        }
        // Restore the window's prior mouse-moved-events setting if we raised it (cursor mode).
        if let saved = savedAcceptsMouseMoved {
            window?.acceptsMouseMovedEvents = saved
            savedAcceptsMouseMoved = nil
        }
    }

    /// One host-pixel point on the negotiated output, with the surface dimensions the host
    /// rescales against (surface == host mode, so the host applies no extra scaling).
    private struct HostPoint { let x: Int32; let y: Int32; let w: UInt32; let h: UInt32 }

    /// Map an NSEvent's cursor location into host-mode pixels for the client-side-cursor
    /// (absolute) path. NSEvent.locationInWindow is window space, origin BOTTOM-left (+y up);
    /// we convert to this view's space, FLIP y to the host's top-left (+y down) convention,
    /// then aspect-fit-letterbox into the host mode exactly like the iOS touch/pointer path.
    /// Only an event in this view's own window counts: with the OS pointer outside every window
    /// of the app a mouse-moved event has a nil window and locationInWindow is in SCREEN
    /// coordinates, which read as window space would put the host cursor back inside the video
    /// and drag it along a pointer that has left the window. Returns nil for such events, for
    /// events in the letterbox bars (outside the video rect) so the host's cursor isn't dragged
    /// onto a black edge, and until a mode is negotiated.
    private func hostPoint(from event: NSEvent) -> HostPoint? {
        guard let window, event.window === window, let v = videoPlacement() else { return nil }
        // Window → view coords (non-flipped: origin bottom-left), then flip y into the box's
        // top-left pixel space, the placement's. The flip stays on the VIEW's height — `inView` is
        // in view coordinates, box or no box.
        let inView = convert(event.locationInWindow, from: nil)
        let boxTop = bounds.height - v.box.maxY
        let px = CGPoint(
            x: (inView.x - v.box.minX) * v.scale, y: (bounds.height - inView.y - boxTop) * v.scale)
        guard v.placement.contains(view: px) else { return nil } // the bars
        let f = v.placement.frame(fromView: px)
        let hx = Int32(f.x.rounded().clamped(to: 0...CGFloat(v.width - 1)))
        let hy = Int32(f.y.rounded().clamped(to: 0...CGFloat(v.height - 1)))
        return HostPoint(x: hx, y: hy, w: v.width, h: v.height)
    }

    /// NSEvent `buttonNumber` → GameStream wire id for the "other" buttons: 2 = middle,
    /// 3 = first side (X1), 4 = second side (X2). Unknown extras fall back to middle.
    private func wireButton(for event: NSEvent) -> UInt32 {
        switch event.buttonNumber {
        case 2: return 2 // middle
        case 3: return 4 // X1
        case 4: return 5 // X2
        default: return 2
        }
    }

    /// Engage/release can run inside a SwiftUI update pass (captureEnabled flips in
    /// updateNSView; release in dismantleNSView) — publishing model state synchronously
    /// there is undefined behavior, so the callback is deferred a runloop turn.
    private func notifyCaptureChange(_ captured: Bool) {
        guard let onCaptureChange else { return }
        DispatchQueue.main.async { onCaptureChange(captured) }
    }

    // MARK: - Session start/stop

    /// Wire up input capture and start the presenter (see SessionPresenter for the
    /// stage-2/stage-1 choice). `onFrame` fires per AU at receipt; `onSessionEnd` on close.
    public func start(
        connection: PunktfunkConnection,
        onFrame: (@Sendable (AccessUnit) -> Void)? = nil,
        onSessionEnd: (@Sendable () -> Void)? = nil
    ) {
        stop()
        self.connection = connection

        // The view owns the session's input capture: handlers attach now, but nothing is
        // forwarded until capture engages (captureEnabled + auto-engage or a click).
        let capture = InputCapture(connection: connection)
        capture.ownsEvent = { [weak self] event in event.window === self?.window }
        capture.onToggleCapture = { [weak self] in
            // The ⌘⎋ monitor is app-wide — only the key window's stream owns the toggle
            // (two stream windows would otherwise flip each other's capture).
            guard let self, self.window?.isKeyWindow == true else { return }
            if self.captured {
                self.releaseCapture()
            } else {
                self.engageCapture(fromClick: false)
            }
        }
        capture.onPreempted = { [weak self] in
            // A newer session took the GC handler slots — staying "captured" here would
            // be a cursor trap with dead input.
            self?.releaseCapture()
        }
        // ⌃⌥⇧M flips the mouse model (capture ⇄ desktop) live — the SDL clients' identical
        // chord. Only the key window's stream owns it (same guard as the ⌘⎋ capture toggle).
        // Re-engage capture in the new model so disassociation and the absolute/relative
        // forwarding choice swap atomically — releaseCapture restores the old model's grab
        // (if any), engageCapture installs the new one. On a gamescope host the chord is a
        // no-op: its EIS grants only a relative pointer, so the desktop model's absolute
        // sends would be silently dropped (pointer stuck = "all input dead").
        capture.onToggleMouseMode = { [weak self] in
            guard let self, self.window?.isKeyWindow == true,
                  let conn = self.connection else { return }
            guard conn.resolvedCompositor != .gamescope else {
                streamInputLog.info("mouse-mode chord ignored: gamescope host is relative-only")
                return
            }
            // A manual flip outranks the standing host hint until the hint next CHANGES.
            self.hintOverride = true
            self.setDesktopMouse(!self.desktopMouse, reappearAt: nil)
            streamInputLog.info("chord: mouse mode \(self.desktopMouse ? "desktop" : "capture", privacy: .public)")
        }
        // The cross-client combos (⌃⌥⇧Q/D/S — Ctrl+Alt+Shift on the other clients), delivered by
        // the monitor only while captured; the same key-window ownership rule as ⌘⎋ throughout.
        capture.onReleaseCapture = { [weak self] in
            guard let self, self.window?.isKeyWindow == true else { return }
            self.releaseCapture()
        }
        capture.onDisconnect = { [weak self] in
            guard let self, self.window?.isKeyWindow == true else { return }
            self.onDisconnectRequest?()
        }
        capture.onToggleFullscreen = { [weak self] in
            // App-level window action: post to the key window's FullscreenController (same routing as
            // the Stream menu's ⌃⌘F item, so captured and released states hit one code path).
            guard self?.window?.isKeyWindow == true else { return }
            NotificationCenter.default.post(name: .punktfunkToggleFullscreen, object: nil)
        }
        capture.onToggleMicMute = { [weak self] in
            // Session-level state the view doesn't own — post to this session's window, so the
            // captured and released paths end at one toggle.
            guard let self, self.window?.isKeyWindow == true else { return }
            NotificationCenter.default.post(name: .punktfunkToggleMicMute, object: self.connection)
        }
        capture.onQuickActions = { [weak self] in
            // The quick-action ring is the session VIEW's, not this layer's — post it (the same
            // routing as the fullscreen and mic chords), so the captured chord and the Stream
            // menu's identical equivalent end at one toggle.
            guard let self, self.window?.isKeyWindow == true else { return }
            NotificationCenter.default.post(name: .punktfunkToggleQuickActions, object: self.connection)
        }
        capture.onCycleStats = { [weak self] in
            guard let self, self.window?.isKeyWindow == true else { return }
            StatsVerbosity.requestCycle(for: self.connection)
        }
        capture.start()
        inputCapture = capture

        // Desktop (absolute) mouse model — resolved at session start from the mouseMode
        // setting, gated by the host's compositor: gamescope's input socket (EIS) grants
        // only a relative pointer, so absolute sends would be silently dropped there
        // (pointer stuck = "all input dead") — pinned to capture. ⌃⌥⇧M flips it live.
        let mode = MouseInputMode(rawValue: connection.settings.mouseMode) ?? .capture
        let absOK = connection.resolvedCompositor != .gamescope
        desktopMouse = mode == .desktop && absOK
        if mode == .desktop && !absOK {
            streamInputLog.info("desktop mouse mode unavailable on a gamescope host (relative-only) — using capture")
        }
        // Cursor channel (M2): the host stopped compositing the pointer — drain its shape/
        // state planes and draw the pointer as the real NSCursor (plus the M3 auto-flip).
        if connection.hostSupportsCursor {
            cursorChannelActive = true
            streamInputLog.info("cursor channel negotiated — host cursor renders locally")
            startCursorPump(connection)
            reconcileCursorRender() // initial render mode (a capture-model start composites)
        }

        // Presenter choice + lifecycle live in SessionPresenter (shared with iOS/tvOS): stage-2
        // (explicit VTDecompressionSession decode + a CAMetalLayer/display-link present) by
        // default, the stage-1 pump as the Metal-missing / DEBUG fallback. The link comes from
        // NSView.displayLink so it tracks the display this view is on.
        // Intercept the pump's coded-dims callback: re-fit the metal sublayer to the real content
        // aspect (main thread) BEFORE forwarding to the owner's overlay END-signal. Fires only on a
        // size CHANGE (first frame + each resolved resize), so this is rare, not per-frame.
        let overlayDecodedSize = onDecodedSize
        presenter.start(
            connection: connection,
            baseLayer: displayLayer,
            endToEndMeter: endToEndMeter,
            makeDisplayLink: { [unowned self] in self.displayLink(target: $0, selector: $1) },
            onFrame: onFrame,
            onSessionEnd: onSessionEnd,
            onDecodedSize: { [weak self] w, h in // resize overlay END signal (new-mode IDR dims)
                DispatchQueue.main.async { self?.noteDecodedContentSize(width: w, height: h) }
                overlayDecodedSize?(w, h)
            })
        // Match-window (C3): when ON, follow the window's pixel size so a windowed session streams
        // 1:1 (pixel-exact) instead of the presenter resampling a fixed-mode frame into a
        // non-matching window. The first real `layout()` feeds the initial size, so the stream
        // converges to the window even though the connect used the explicit/display mode; entering
        // fullscreen reports the full-display px, restoring a native-res 1:1 present there too.
        // OPT-IN — `?? false` matches the Settings toggle (which also defaults off); an unset
        // default keeps the explicit mode.
        let follower = MatchWindowFollower(
            connection: connection,
            enabled: connection.settings.matchWindow,
            renderScale: connection.settings.renderScale,
            maxDimension: RenderScale.maxDimension(codec: connection.settings.codec))
        follower.onResizeTarget = onResizeTarget // resize overlay START signal (instant, on the follower)
        matchFollower = follower
        layoutPresenter()
        requestAutoCapture() // entering a session is the deliberate "capture me" moment
    }

    /// Aspect-fit the stage-2 metal sublayer to the video box (`videoBounds`, which is the view
    /// except under a camera housing); refresh contentsScale on a retina↔non-retina move (see
    /// SessionPresenter.layout). Also feeds the Match-window follower the WINDOW's physical-pixel
    /// size (bounds → backing) — it follows the window, not the box — so a resize / retina move
    /// follows.
    private func layoutPresenter() {
        // Refresh BEFORE the fit below reads it. Enumerating display modes costs ~150 µs, and
        // `videoBounds` is read on every mouse event — that belongs on layout, not on input.
        safeModePixels = window?.screen?.notchSafePixelSize
        presenter.layout(in: videoBounds, contentsScale: window?.backingScaleFactor ?? 1)
        displayLayer.videoGravity = SessionPresenter.gravity(VideoFit(name: connection?.settings.videoFit))
        // Present routing tracks the window's composited state (fullscreen transitions always
        // re-layout, so this stays current): a windowed session presents through a Core Animation
        // transaction — the DCP swapID kernel-panic mitigation (see SessionPresenter.setComposited).
        // A view not yet in a window counts as composited (the safe default).
        presenter.setComposited(!(window?.styleMask.contains(.fullScreen) ?? false))
        // Feed the follower only once in a window (backing scale is real then) and with real
        // bounds — a pre-window layout would report point-sized dimensions.
        if window != nil, bounds.width > 0, bounds.height > 0 {
            let px = convertToBacking(bounds).size
            matchFollower?.noteSize(
                widthPx: Int(px.width.rounded()), heightPx: Int(px.height.rounded()))
        }
        // The video-fit scale just changed (resize / retina move); rebuild the worn host pointer at
        // the new scale so it tracks the video instead of freezing at its build-time size.
        if captured, desktopMouse, cursorChannelActive {
            window?.invalidateCursorRects(for: self)
        }
    }

    public override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        layoutPresenter() // backing scale changed (e.g. moved to a non-retina display)
    }

    /// A new decoded size landed (a new-mode IDR after a resize, or the session's first frame): push
    /// it to the presenter's aspect-fit and re-layout NOW. A resize-END triggers no `layout()`, so
    /// this is what makes the metal sublayer track the new content aspect instead of stretching the
    /// new frame into the pre-resize box. Deduped so a same-size repeat is a no-op. Main thread.
    private func noteDecodedContentSize(width: Int, height: Int) {
        let size = CGSize(width: width, height: height)
        guard size.width > 0, size.height > 0, size != lastDecodedContentSize else { return }
        lastDecodedContentSize = size
        presenter.setContentSize(size)
        layoutPresenter()
    }

    /// Stop pumping (≤ one poll timeout). Does not close the connection — that stays with
    /// whoever owns it (PunktfunkConnection.close() is safe alongside a draining pump).
    public func stop() {
        releaseCapture()
        removeMouseMonitor() // belt-and-suspenders: releaseCapture no-ops if not captured
        inputCapture?.stop()
        inputCapture = nil
        presenter.stop()
        matchFollower = nil
        lastDecodedContentSize = nil // the next session re-derives it from its first frame
        connection = nil
        // Cursor-channel state is per-session: without this reset a next session against a
        // host WITHOUT the cap would wear this session's stale shapes (`cursorChannelActive`
        // stayed latched true across sessions).
        cursorChannelActive = false
        cursorState = nil
        hostCursors.removeAll()
        sentClientDraws = nil
        window?.invalidateCursorRects(for: self)
    }

    deinit {
        removeMouseMonitor()
        appObservers.forEach(NotificationCenter.default.removeObserver(_:))
        windowObservers.forEach(NotificationCenter.default.removeObserver(_:))
        presenter.stop() // invalidate the display link + stop the pipeline if stop() was missed
    }
}
#endif
