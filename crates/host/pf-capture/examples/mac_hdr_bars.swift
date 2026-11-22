// Eight vertical bars on the screen named `Punktfunk`, drawn in extended linear sRGB through an
// EDR Metal layer: black, 18% grey, white (1.0), then 2, 4 and 8 times white, sRGB red and
// Display P3 red. `scripts/macos-hdr-check.sh` reads them back out of a session's stream.
//
// A strip along the bottom flickers, so the capture keeps getting new frames. The window
// follows the screen when its size changes, and the process ends by itself.
//
// Build: swiftc -O -o mac_hdr_bars mac_hdr_bars.swift
// Usage: mac_hdr_bars <seconds>
import AppKit
import Metal
import QuartzCore

setvbuf(stdout, nil, _IOLBF, 0)
let seconds = Double(CommandLine.arguments.dropFirst().first ?? "20") ?? 20
let bars: [(Float, Float, Float)] = [
    (0, 0, 0), (0.18, 0.18, 0.18), (1, 1, 1), (2, 2, 2), (4, 4, 4), (8, 8, 8),
    (1, 0, 0), (1.2249, -0.0420, -0.0197),
]

final class Pattern: NSObject {
    let window: NSWindow
    let layer = CAMetalLayer()
    let device = MTLCreateSystemDefaultDevice()!
    let queue: MTLCommandQueue
    var frames: [MTLTexture] = []
    var tick = 0

    init(screen: NSScreen) {
        queue = device.makeCommandQueue()!
        let scale = screen.backingScaleFactor
        let (w, h) = (Int(screen.frame.width * scale), Int(screen.frame.height * scale))
        window = NSWindow(contentRect: screen.frame, styleMask: .borderless, backing: .buffered, defer: false)
        super.init()
        layer.device = device
        layer.pixelFormat = .rgba16Float
        layer.wantsExtendedDynamicRangeContent = true
        layer.colorspace = CGColorSpace(name: CGColorSpace.extendedLinearSRGB)
        layer.framebufferOnly = false
        layer.drawableSize = CGSize(width: w, height: h)
        let view = NSView(frame: NSRect(origin: .zero, size: screen.frame.size))
        view.wantsLayer = true
        view.layer = layer
        window.contentView = view
        window.level = .screenSaver
        window.setFrame(screen.frame, display: true)
        window.orderFrontRegardless()
        frames = [Float(0.2), Float(0.3)].map { strip in
            let d = MTLTextureDescriptor.texture2DDescriptor(pixelFormat: .rgba16Float, width: w, height: h, mipmapped: false)
            let t = device.makeTexture(descriptor: d)!
            var px = [UInt16](repeating: 0, count: w * h * 4)
            for y in 0..<h {
                for x in 0..<w {
                    let c = y >= h * 95 / 100 ? (strip, strip, strip) : bars[min(x * bars.count / w, bars.count - 1)]
                    let i = (y * w + x) * 4
                    px[i] = Float16(c.0).bitPattern; px[i + 1] = Float16(c.1).bitPattern
                    px[i + 2] = Float16(c.2).bitPattern; px[i + 3] = Float16(1).bitPattern
                }
            }
            t.replace(region: MTLRegionMake2D(0, 0, w, h), mipmapLevel: 0, withBytes: px, bytesPerRow: w * 8)
            return t
        }
    }

    @objc func draw() {
        guard let drawable = layer.nextDrawable(), let buffer = queue.makeCommandBuffer(),
              let blit = buffer.makeBlitCommandEncoder() else { return }
        tick += 1
        blit.copy(from: frames[tick % 2], to: drawable.texture)
        blit.endEncoding()
        buffer.present(drawable)
        buffer.commit()
    }
}

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
var pattern: Pattern?
var drawer: Timer?
var waited = 0.0

/// Put the pattern on the Punktfunk screen as it is now.
func show(_ screen: NSScreen) {
    print(String(format: "screen %@ %.0fx%.0f: HDR headroom %.2f", screen.localizedName, screen.frame.width,
                 screen.frame.height, screen.maximumPotentialExtendedDynamicRangeColorComponentValue))
    drawer?.invalidate()
    pattern?.window.orderOut(nil)
    let p = Pattern(screen: screen)
    pattern = p
    drawer = Timer.scheduledTimer(timeInterval: 1.0 / 30, target: p, selector: #selector(Pattern.draw), userInfo: nil, repeats: true)
}
func ours() -> NSScreen? { NSScreen.screens.first { $0.localizedName == "Punktfunk" } }

Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { timer in
    waited += 0.1
    guard let screen = ours() else {
        if waited > 20 { print("no screen named Punktfunk appeared"); exit(1) }
        return
    }
    timer.invalidate()
    show(screen)
    var pending: DispatchWorkItem?
    NotificationCenter.default.addObserver(forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main) { _ in
        pending?.cancel()
        let again = DispatchWorkItem { if let s = ours() { show(s) } }
        pending = again
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: again)
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + seconds) { exit(0) }
}
app.run()
