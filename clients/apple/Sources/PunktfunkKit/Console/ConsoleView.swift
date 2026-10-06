// The console's glass: a `CAMetalLayer` the shell draws into, paced by the display link.
//
// Apple's compositor wants the drawable acquired on the link, so the frame call happens in the
// link callback on the main thread — the thread that built the bridge. Skia submits to our queue;
// we present after it returns, on the same queue, so the present cannot outrun the drawing.

import GameController
import Metal
import PunktfunkCore
import QuartzCore
import SwiftUI

#if canImport(UIKit)
import UIKit
public typealias ConsolePlatformView = UIView
#elseif canImport(AppKit)
import AppKit
public typealias ConsolePlatformView = NSView
#endif

/// What the view hands back to its owner. The view itself knows nothing about hosts.
@MainActor
public protocol ConsoleViewDelegate: AnyObject {
    /// A frame is about to be drawn: the moment to drain what the shell raised.
    func consoleDidDrawFrame()
    /// The console asked to quit — a Back the shell did not take (tvOS Menu at the root).
    func consoleDidRequestQuit()
}

@MainActor
public final class ConsoleMetalView: ConsolePlatformView {
    private let bridge: ConsoleBridge
    private let queue: MTLCommandQueue
    private weak var delegate: ConsoleViewDelegate?
    private var link: CADisplayLink?
    /// Touch state: the console takes one finger, the first one down.
    private var tracked: ObjectIdentifier?
    /// Where a trackpad scroll landed as a finger, while it drags.
    private var scrollFrom: CGPoint?
    /// Where the remote's swipe last stepped from.
    private var swipeFrom: CGPoint?
    /// A held remote direction and the timer that repeats it.
    private var held: (press: ObjectIdentifier, timer: Timer)?
    /// Presses taken on the way down. Their release stays ours, even when the Back one carried
    /// reached the root in between.
    private var claimed = Set<ObjectIdentifier>()

    public init(bridge: ConsoleBridge, device: MTLDevice, queue: MTLCommandQueue, delegate: ConsoleViewDelegate?) {
        self.bridge = bridge
        self.queue = queue
        self.delegate = delegate
        super.init(frame: .zero)
        #if os(iOS) || os(visionOS)
        // The console takes one finger; a second would only fight the first for the cursor.
        isMultipleTouchEnabled = false
        addPointerInput()
        #elseif canImport(AppKit)
        wantsLayer = true
        #endif
        let metal = metalLayer
        metal.device = device
        // 10-bit like the video's SDR drawable: the backdrop's gradients band in 8. Skia wraps
        // the texture as BGRA1010102, or BGRA8888 under the `PUNKTFUNK_SDR10_DRAWABLE=8` lever.
        metal.pixelFormat = sdr10Drawable
        // Skia reads back while blending, so the drawable cannot be write-only.
        metal.framebufferOnly = false
        metal.isOpaque = true
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("not from a nib") }

    #if canImport(UIKit)
    public override class var layerClass: AnyClass { CAMetalLayer.self }
    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
    #else
    public override func makeBackingLayer() -> CALayer { CAMetalLayer() }
    private var metalLayer: CAMetalLayer { layer as! CAMetalLayer }
    #endif

    // MARK: - the link

    public func start() {
        guard link == nil else { return }
        #if canImport(UIKit)
        let link = CADisplayLink(target: self, selector: #selector(tick))
        link.add(to: .main, forMode: .common)
        #else
        let link = displayLink(target: self, selector: #selector(tick))
        link.add(to: .main, forMode: .common)
        #endif
        self.link = link
    }

    public func stop() {
        link?.invalidate()
        link = nil
        held?.timer.invalidate()
        held = nil
    }

    @objc private func tick() {
        resize()
        let layer = metalLayer
        guard layer.drawableSize.width > 0, let drawable = layer.nextDrawable() else { return }
        let drawn = bridge.frame(
            texture: drawable.texture, width: drawable.texture.width,
            height: drawable.texture.height, insets: pixelInsets, scale: designScale)
        if drawn, let commands = queue.makeCommandBuffer() {
            commands.present(drawable)
            commands.commit()
        }
        delegate?.consoleDidDrawFrame()
    }

    #if canImport(UIKit)
    private var pixelScale: CGFloat {
        #if os(visionOS)
        return traitCollection.displayScale > 0 ? traitCollection.displayScale : 2
        #else
        return window?.screen.scale ?? 2
        #endif
    }
    #endif

    /// The drawable follows the view, in pixels.
    private func resize() {
        let layer = metalLayer
        #if canImport(UIKit)
        let scale = pixelScale
        #else
        let scale = window?.backingScaleFactor ?? 2
        #endif
        layer.contentsScale = scale
        let size = CGSize(width: bounds.width * scale, height: bounds.height * scale)
        if layer.drawableSize != size { layer.drawableSize = size }
    }

    /// Safe-area insets in drawable pixels: the chrome stays inside, the backdrop does not.
    private var pixelInsets: PunktfunkInsets {
        #if canImport(UIKit)
        let scale = pixelScale
        let insets = safeAreaInsets
        return PunktfunkInsets(
            left: Float(insets.left * scale), top: Float(insets.top * scale),
            right: Float(insets.right * scale), bottom: Float(insets.bottom * scale))
        #else
        return PunktfunkInsets(left: 0, top: 0, right: 0, bottom: 0)
        #endif
    }

    /// Design units per pixel, as Android's shell computes them: a TV takes the shell's own
    /// couch formula (`0`), everything else the larger of the couch fit and the device scale.
    private var designScale: Double {
        #if os(tvOS)
        return 0
        #else
        #if canImport(UIKit)
        let scale = pixelScale
        #else
        let scale = window?.backingScaleFactor ?? 2
        #endif
        let pixels = min(bounds.width, bounds.height) * scale
        guard pixels > 0 else { return 0 }
        return min(max(max(pixels / 800, scale * 0.75), 0.75), 3)
        #endif
    }

    // MARK: - pointer

    #if canImport(UIKit)
    // On iOS touches are a finger on the glass. A Siri Remote's clickpad sends indirect touches
    // that start at the screen's centre, so on tvOS they are only a swipe's travel.
    #if os(iOS) || os(visionOS)
    public override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard tracked == nil, scrollFrom == nil, let touch = touches.first else { return }
        tracked = ObjectIdentifier(touch)
        send(.touchDown, touch)
    }

    public override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = touches.first(where: { ObjectIdentifier($0) == tracked }) else { return }
        send(.move, touch)
    }

    public override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let touch = touches.first(where: { ObjectIdentifier($0) == tracked }) else { return }
        tracked = nil
        send(.up, touch)
    }

    public override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard touches.contains(where: { ObjectIdentifier($0) == tracked }) else { return }
        tracked = nil
        bridge.pointer(.cancel, x: 0, y: 0)
    }

    private func send(_ kind: ConsoleBridge.Pointer, _ touch: UITouch) {
        send(kind, at: touch.location(in: self))
    }

    private func send(_ kind: ConsoleBridge.Pointer, at p: CGPoint, wheel: Float = 0) {
        let scale = pixelScale
        bridge.pointer(kind, x: Float(p.x * scale), y: Float(p.y * scale), wheel: wheel)
    }

    // MARK: - mouse and trackpad

    /// A pointer that has not clicked sends no touches: its movement is hover and its scroll
    /// is a pan that takes no finger. One recognizer per scroll type, so UIKit names the device.
    private func addPointerInput() {
        addGestureRecognizer(UIHoverGestureRecognizer(target: self, action: #selector(hover)))
        for (mask, action) in [
            (UIScrollTypeMask.continuous, #selector(trackpadScroll)),
            (UIScrollTypeMask.discrete, #selector(wheelScroll)),
        ] {
            let pan = UIPanGestureRecognizer(target: self, action: action)
            pan.allowedScrollTypesMask = mask
            pan.allowedTouchTypes = []
            addGestureRecognizer(pan)
        }
    }

    @objc private func hover(_ g: UIHoverGestureRecognizer) {
        // While a finger or a scroll is down the shell reads a move as its drag.
        guard tracked == nil, scrollFrom == nil, g.state == .began || g.state == .changed
        else { return }
        send(.move, at: g.location(in: self))
    }

    /// Each wheel report is one scroll step. UIKit has applied Natural Scrolling, so + is up.
    @objc private func wheelScroll(_ g: UIPanGestureRecognizer) {
        let t = g.translation(in: self)
        g.setTranslation(.zero, in: self)
        let step = abs(t.y) >= abs(t.x) ? t.y : t.x
        if step != 0 { send(.wheel, at: g.location(in: self), wheel: Float(step)) }
    }

    /// A two-finger scroll drags the console as a finger on the glass does, so it tracks and
    /// flings the same way. The finger lands once the scroll has left the shell's 12-unit tap
    /// slop with room to spare: a finger that lifts inside it is a tap.
    @objc private func trackpadScroll(_ g: UIPanGestureRecognizer) {
        let t = g.translation(in: self)
        switch g.state {
        case .began, .changed:
            if scrollFrom == nil {
                guard tracked == nil, hypot(t.x, t.y) * pixelScale >= 16 * designScale
                else { return }
                let from = g.location(in: self)
                scrollFrom = from
                send(.touchDown, at: from)
            }
            if let from = scrollFrom { send(.move, at: CGPoint(x: from.x + t.x, y: from.y + t.y)) }
        case .ended:
            guard let from = scrollFrom else { return }
            scrollFrom = nil
            send(.up, at: CGPoint(x: from.x + t.x, y: from.y + t.y))
        case .cancelled, .failed:
            guard scrollFrom != nil else { return }
            scrollFrom = nil
            bridge.pointer(.cancel, x: 0, y: 0)
        default:
            break
        }
    }
    #elseif os(tvOS)
    public override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        swipeFrom = touches.first?.location(in: self)
    }

    /// One step each time the thumb travels a sixth of the screen, along the axis it travelled
    /// most. A click's own wobble stays well short of that.
    public override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard let from = swipeFrom, let p = touches.first?.location(in: self) else { return }
        let (dx, dy) = (p.x - from.x, p.y - from.y)
        guard max(abs(dx), abs(dy)) >= bounds.width / 6 else { return }
        bridge.menu(abs(dx) >= abs(dy) ? (dx > 0 ? .right : .left) : (dy > 0 ? .down : .up), from: .keys)
        swipeFrom = p
    }

    public override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { swipeFrom = nil }
    public override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { swipeFrom = nil }
    #endif

    // MARK: - presses

    // A Siri Remote's clicks and a hardware keyboard's keys arrive here, not through
    // GameController: the pad poller reads only the active extended pad. That pad arrives here
    // too, its stick as diagonal arrow pairs, and the poller already acted on it.

    public override var canBecomeFirstResponder: Bool { true }
    #if os(tvOS)
    public override var canBecomeFocused: Bool { true }
    #endif

    public override func didMoveToWindow() {
        super.didMoveToWindow()
        if window != nil { becomeFirstResponder() }
    }

    public override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unclaimed = presses.filter { press in
            guard claim(press, repeated: false) else { return true }
            claimed.insert(ObjectIdentifier(press))
            return false
        }
        if !unclaimed.isEmpty || presses.isEmpty { super.pressesBegan(unclaimed, with: event) }
    }

    public override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        // Claimed on the way down, so the matching release is ours; Select's release acts.
        if presses.contains(where: { $0.key == nil && $0.type == .select && !fromPad($0) }) {
            bridge.menu(.okUp, from: .keys)
        }
        release(presses)
        let unclaimed = presses.filter { claimed.remove(ObjectIdentifier($0)) == nil }
        if !unclaimed.isEmpty || presses.isEmpty { super.pressesEnded(unclaimed, with: event) }
    }

    public override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        release(presses)
        let unclaimed = presses.filter { claimed.remove(ObjectIdentifier($0)) == nil }
        if !unclaimed.isEmpty || presses.isEmpty { super.pressesCancelled(unclaimed, with: event) }
    }

    /// A press from the pad the poller reads. GameController makes a controller current before
    /// UIKit delivers its press, so this tells the pad's presses from the remote's.
    private func fromPad(_ press: UIPress) -> Bool {
        guard press.key == nil, let pad = GamepadManager.shared.active?.controller else { return false }
        return GCController.current === pad
    }

    /// Hand the press to the console. `false` = not ours, let the system have it. The pad's
    /// own presses are claimed but do nothing: the poller already acted on them, B included.
    /// Its Home button, not B, takes a pad player Home.
    private func claim(_ press: UIPress, repeated: Bool) -> Bool {
        if fromPad(press) {
            return [.select, .upArrow, .downArrow, .leftArrow, .rightArrow, .playPause, .menu]
                .contains(press.type)
        }
        if let key = key(for: press) {
            let shift = press.key?.modifierFlags.contains(.shift) ?? false
            bridge.key(key, shift: shift, repeated: repeated)
            return true
        }
        let event: ConsoleBridge.Menu
        switch press.type {
        case .select: event = .okDown
        case .upArrow: event = .up
        case .downArrow: event = .down
        case .leftArrow: event = .left
        case .rightArrow: event = .right
        case .playPause: event = .secondary
        // At the root the remote's Menu is the system's, which takes a TV player Home.
        case .menu where !bridge.atRoot: event = .back
        default: return false
        }
        bridge.menu(event, from: .keys)
        if [.up, .down, .left, .right].contains(event) { hold(press, event) }
        return true
    }

    /// Repeat a held direction at the pad poller's pace until its press ends.
    private func hold(_ press: UIPress, _ event: ConsoleBridge.Menu) {
        held?.timer.invalidate()
        let timer = Timer(
            fire: Date() + GamepadMenuInput.initialRepeatDelay,
            interval: GamepadMenuInput.repeatInterval, repeats: true
        ) { [weak self] _ in
            MainActor.assumeIsolated { _ = self?.bridge.menu(event, from: .keys) }
        }
        RunLoop.main.add(timer, forMode: .common)
        held = (ObjectIdentifier(press), timer)
    }

    private func release(_ presses: Set<UIPress>) {
        guard let held, presses.contains(where: { ObjectIdentifier($0) == held.press }) else { return }
        held.timer.invalidate()
        self.held = nil
    }

    /// A hardware keyboard's key, when the console has a use for it.
    private func key(for press: UIPress) -> ConsoleBridge.Key? {
        switch press.key?.keyCode {
        case .keyboardLeftArrow: return .left
        case .keyboardRightArrow: return .right
        case .keyboardUpArrow: return .up
        case .keyboardDownArrow: return .down
        case .keyboardReturnOrEnter: return .return
        case .keyboardSpacebar: return .space
        case .keyboardEscape: return .escape
        case .keyboardDeleteOrBackspace: return .backspace
        case .keyboardPageUp: return .pageUp
        case .keyboardPageDown: return .pageDown
        case .keyboardTab: return .tab
        case .keyboardY: return .y
        case .keyboardX: return .x
        default: return nil
        }
    }
    #else
    public override func mouseDown(with event: NSEvent) { send(.down, event) }
    public override func mouseDragged(with event: NSEvent) { send(.move, event) }
    public override func mouseUp(with event: NSEvent) { send(.up, event) }
    public override func mouseMoved(with event: NSEvent) { send(.move, event) }
    public override func rightMouseDown(with event: NSEvent) { send(.back, event) }

    public override func scrollWheel(with event: NSEvent) {
        let scale = window?.backingScaleFactor ?? 2
        let p = convert(event.locationInWindow, from: nil)
        bridge.pointer(
            .wheel, x: Float(p.x * scale), y: Float((bounds.height - p.y) * scale),
            wheel: Float(event.scrollingDeltaY / 10))
    }

    private func send(_ kind: ConsoleBridge.Pointer, _ event: NSEvent) {
        let scale = window?.backingScaleFactor ?? 2
        let p = convert(event.locationInWindow, from: nil)
        // AppKit's origin is bottom-left; the shell's is top-left, like the texture.
        bridge.pointer(kind, x: Float(p.x * scale), y: Float((bounds.height - p.y) * scale))
    }
    #endif
}

/// The console as a SwiftUI view. The bridge outlives any one layout pass, so the owner holds it.
public struct ConsoleView {
    private let bridge: ConsoleBridge
    private let device: MTLDevice
    private let queue: MTLCommandQueue
    private weak var delegate: ConsoleViewDelegate?

    public init(bridge: ConsoleBridge, device: MTLDevice, queue: MTLCommandQueue, delegate: ConsoleViewDelegate?) {
        self.bridge = bridge
        self.device = device
        self.queue = queue
        self.delegate = delegate
    }

    @MainActor private func make() -> ConsoleMetalView {
        let view = ConsoleMetalView(bridge: bridge, device: device, queue: queue, delegate: delegate)
        view.start()
        return view
    }
}

#if canImport(UIKit)
extension ConsoleView: UIViewRepresentable {
    @MainActor public func makeUIView(context: Context) -> ConsoleMetalView { make() }
    public func updateUIView(_ view: ConsoleMetalView, context: Context) {}
    public static func dismantleUIView(_ view: ConsoleMetalView, coordinator: ()) {
        MainActor.assumeIsolated { view.stop() }
    }
}
#else
extension ConsoleView: NSViewRepresentable {
    @MainActor public func makeNSView(context: Context) -> ConsoleMetalView { make() }
    public func updateNSView(_ view: ConsoleMetalView, context: Context) {}
    public static func dismantleNSView(_ view: ConsoleMetalView, coordinator: ()) {
        MainActor.assumeIsolated { view.stop() }
    }
}
#endif
