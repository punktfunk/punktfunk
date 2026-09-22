// The console's glass: a `CAMetalLayer` the shell draws into, paced by the display link.
//
// Apple's compositor wants the drawable acquired on the link, so the frame call happens in the
// link callback on the main thread — the thread that built the bridge. Skia submits to our queue;
// we present after it returns, on the same queue, so the present cannot outrun the drawing.

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

    public init(bridge: ConsoleBridge, device: MTLDevice, queue: MTLCommandQueue, delegate: ConsoleViewDelegate?) {
        self.bridge = bridge
        self.queue = queue
        self.delegate = delegate
        super.init(frame: .zero)
        #if canImport(UIKit)
        isMultipleTouchEnabled = false
        #else
        wantsLayer = true
        #endif
        let metal = metalLayer
        metal.device = device
        metal.pixelFormat = .bgra8Unorm
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

    /// The drawable follows the view, in pixels.
    private func resize() {
        let layer = metalLayer
        #if canImport(UIKit)
        let scale = window?.screen.scale ?? 2
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
        let scale = window?.screen.scale ?? 2
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
        let scale = window?.screen.scale ?? 2
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
    public override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        guard tracked == nil, let touch = touches.first else { return }
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
        let scale = window?.screen.scale ?? 2
        let p = touch.location(in: self)
        bridge.pointer(kind, x: Float(p.x * scale), y: Float(p.y * scale))
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
