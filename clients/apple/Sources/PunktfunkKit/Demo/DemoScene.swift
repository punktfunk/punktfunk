// The demo host's picture — what a streamed desktop shows, drawn with CoreGraphics on the demo
// host's render queue. Every input kind a client sends changes it, so a session shows its round
// trip: the remote, mouse, keys, touch and pads travel to the host and come back as video.

import CoreGraphics
import CoreText
import CoreVideo
import Foundation
import PunktfunkCore

final class DemoScene {
    /// Layout is in points of a 1080-tall canvas; its width follows the stream's aspect.
    private static let canvasHeight: CGFloat = 1080

    var title = "Desktop"
    private var canvas = CGSize(width: 1920, height: 1080)
    /// Stream pixels per canvas point, for relative pointer deltas.
    private var pixelScale: CGFloat = 1
    private var cursor = CGPoint(x: 960, y: 700)
    private var ripples: [(at: CGPoint, born: Double)] = []
    private var buttons: UInt32 = 0
    /// LX, LY, RX, RY in −1…1, +y up.
    private var sticks: [CGFloat] = [0, 0, 0, 0]
    /// LT, RT in 0…1.
    private var triggers: [CGFloat] = [0, 0]
    /// An empty `PUNKTFUNK_DEMO_FLASH=` leaves the flash off.
    private static let flashLabel = ProcessInfo.processInfo.environment["PUNKTFUNK_DEMO_FLASH"]
        .flatMap { $0.isEmpty ? nil : $0 }
    private(set) var lastInput = "Nothing yet"
    private var lastInputAt = -10.0
    private var lastRender: Double?
    private var frame = 0
    private let space = CGColorSpace(name: CGColorSpace.sRGB)!
    private let clock: DateFormatter = {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "HH:mm:ss"
        return f
    }()
    private var fonts: [String: CTFont] = [:]
    private lazy var backdrop = gradient([0x160E33, 0x05101F])
    private lazy var violetGlow = gradient([0x7C5CFF], alphas: [0.5, 0])
    private lazy var tealGlow = gradient([0x22D3EE], alphas: [0.3, 0])

    private static let padButtons: [(bit: UInt32, name: String)] = [
        (0x1000, "A"), (0x2000, "B"), (0x4000, "X"), (0x8000, "Y"),
        (0x1, "D-pad up"), (0x2, "D-pad down"), (0x4, "D-pad left"), (0x8, "D-pad right"),
        (0x10, "Menu"), (0x20, "View"), (0x40, "Left stick"), (0x80, "Right stick"),
        (0x100, "LB"), (0x200, "RB"), (0x400, "Home"), (0x100000, "Touchpad"),
    ]

    func resize(width: Int, height: Int) {
        pixelScale = CGFloat(height) / Self.canvasHeight
        canvas = CGSize(width: CGFloat(width) / pixelScale, height: Self.canvasHeight)
        cursor = CGPoint(x: canvas.width / 2, y: 700)
    }

    // MARK: - Input

    func apply(_ ev: PunktfunkInputEvent, now: Double) {
        switch UInt32(ev.kind) {
        case PUNKTFUNK_INPUT_KIND_MOUSE_MOVE.rawValue:
            // Accelerated like a desktop pointer: a Siri Remote swipe arrives as 2–6 px steps
            // at 100 Hz, which 1:1 crawls. About 3× at 6 px, capped at 5×.
            let gain = 1 + min(hypot(CGFloat(ev.x), CGFloat(ev.y)), 12) * 0.35
            move(dx: CGFloat(ev.x) * gain / pixelScale, dy: CGFloat(ev.y) * gain / pixelScale)
        case PUNKTFUNK_INPUT_KIND_MOUSE_MOVE_ABS.rawValue, PUNKTFUNK_INPUT_KIND_TOUCH_MOVE.rawValue:
            place(ev)
        case PUNKTFUNK_INPUT_KIND_TOUCH_DOWN.rawValue:
            place(ev)
            press("Touch", now)
        case PUNKTFUNK_INPUT_KIND_MOUSE_BUTTON_DOWN.rawValue:
            let name = [1: "Left click", 2: "Middle click", 3: "Right click"][ev.code] ?? "Click"
            press(name, now)
        case PUNKTFUNK_INPUT_KIND_MOUSE_SCROLL.rawValue, PUNKTFUNK_INPUT_KIND_SCROLL.rawValue:
            // `code` is the axis, 1 horizontal; positive is up or right.
            guard ev.x != 0 else { break }
            if ev.code == 1 {
                note(ev.x > 0 ? "Scroll right" : "Scroll left", now)
            } else {
                note(ev.x > 0 ? "Scroll up" : "Scroll down", now)
            }
        case PUNKTFUNK_INPUT_KIND_KEY_DOWN.rawValue:
            key(ev.code, now)
        case PUNKTFUNK_INPUT_KIND_TEXT_INPUT.rawValue:
            if let scalar = UnicodeScalar(ev.code) { note("Typed \u{201C}\(Character(scalar))\u{201D}", now) }
        case PUNKTFUNK_INPUT_KIND_GAMEPAD_STATE.rawValue:
            // Snapshot: buttons in `code`, sticks packed in x/y, triggers in `flags`.
            setButtons(ev.code, now)
            sticks = [
                stick(Int16(truncatingIfNeeded: ev.x >> 16)), stick(Int16(truncatingIfNeeded: ev.x)),
                stick(Int16(truncatingIfNeeded: ev.y >> 16)), stick(Int16(truncatingIfNeeded: ev.y)),
            ]
            triggers = [CGFloat((ev.flags >> 16) & 0xFF) / 255, CGFloat((ev.flags >> 8) & 0xFF) / 255]
        case PUNKTFUNK_INPUT_KIND_GAMEPAD_BUTTON.rawValue:
            setButtons(ev.x != 0 ? buttons | ev.code : buttons & ~ev.code, now)
        case PUNKTFUNK_INPUT_KIND_GAMEPAD_AXIS.rawValue:
            switch ev.code {
            case 0...3: sticks[Int(ev.code)] = stick(Int16(clamping: ev.x))
            case 4, 5: triggers[Int(ev.code) - 4] = CGFloat(min(max(ev.x, 0), 255)) / 255
            default: break
            }
        case PUNKTFUNK_INPUT_KIND_GAMEPAD_ARRIVAL.rawValue:
            note("Controller connected", now)
        default:
            break
        }
    }

    private func stick(_ v: Int16) -> CGFloat { CGFloat(v) / 32767 }

    private func move(dx: CGFloat, dy: CGFloat) {
        cursor.x = min(max(cursor.x + dx, 0), canvas.width)
        cursor.y = min(max(cursor.y + dy, 0), canvas.height)
    }

    /// Absolute events carry the client surface as `(width << 16) | height`.
    private func place(_ ev: PunktfunkInputEvent) {
        let w = CGFloat(ev.flags >> 16), h = CGFloat(ev.flags & 0xFFFF)
        guard w > 0, h > 0 else { return }
        cursor = CGPoint(x: CGFloat(ev.x) / w * canvas.width, y: CGFloat(ev.y) / h * canvas.height)
    }

    private func note(_ text: String, _ now: Double) {
        lastInput = text
        lastInputAt = now
    }

    private func press(_ text: String, _ now: Double) {
        note(text, now)
        ripples.append((cursor, now))
    }

    private func setButtons(_ next: UInt32, _ now: Double) {
        let pressed = next & ~buttons
        buttons = next
        guard pressed != 0 else { return }
        let names = Self.padButtons.filter { pressed & $0.bit != 0 }.map(\.name)
        if pressed & 0x1000 != 0 {
            press("Controller \(names.joined(separator: " + "))", now)
        } else {
            note("Controller \(names.joined(separator: " + "))", now)
        }
        // The d-pad nudges the pointer, so a pad alone can reach anything on screen.
        if pressed & 0x1 != 0 { move(dx: 0, dy: -60) }
        if pressed & 0x2 != 0 { move(dx: 0, dy: 60) }
        if pressed & 0x4 != 0 { move(dx: -60, dy: 0) }
        if pressed & 0x8 != 0 { move(dx: 60, dy: 0) }
    }

    private func key(_ vk: UInt32, _ now: Double) {
        switch vk {
        case 0x25: move(dx: -60, dy: 0)
        case 0x26: move(dx: 0, dy: -60)
        case 0x27: move(dx: 60, dy: 0)
        case 0x28: move(dx: 0, dy: 60)
        case 0x0D, 0x20:
            press("Key \(Self.keyName(vk))", now)
            return
        default: break
        }
        note("Key \(Self.keyName(vk))", now)
    }

    /// Windows virtual-key codes, as clients send them.
    private static func keyName(_ vk: UInt32) -> String {
        switch vk {
        case 0x30...0x39, 0x41...0x5A: return String(UnicodeScalar(UInt8(vk)))
        case 0x08: return "Backspace"
        case 0x09: return "Tab"
        case 0x0D: return "Return"
        case 0x1B: return "Escape"
        case 0x20: return "Space"
        case 0x25: return "\u{2190}"
        case 0x26: return "\u{2191}"
        case 0x27: return "\u{2192}"
        case 0x28: return "\u{2193}"
        case 0x10, 0xA0, 0xA1: return "Shift"
        case 0x11, 0xA2, 0xA3: return "Control"
        case 0x12, 0xA4, 0xA5: return "Option"
        case 0x5B, 0x5C: return "Command"
        default: return String(format: "0x%02X", vk)
        }
    }

    // MARK: - Drawing

    func render(into buffer: CVPixelBuffer, now: Double) {
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        let width = CVPixelBufferGetWidth(buffer), height = CVPixelBufferGetHeight(buffer)
        guard let base = CVPixelBufferGetBaseAddress(buffer),
              let ctx = CGContext(
                  data: base, width: width, height: height, bitsPerComponent: 8,
                  bytesPerRow: CVPixelBufferGetBytesPerRow(buffer), space: space,
                  bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue
                      | CGBitmapInfo.byteOrder32Little.rawValue)
        else { return }
        // Top-left origin in canvas points; the text matrix un-flips glyphs.
        ctx.translateBy(x: 0, y: CGFloat(height))
        ctx.scaleBy(x: pixelScale, y: -pixelScale)
        ctx.textMatrix = CGAffineTransform(scaleX: 1, y: -1)

        // The left stick steers the pointer, 900 points a second at full tilt.
        let dt = min(now - (lastRender ?? now), 0.1)
        lastRender = now
        move(dx: sticks[0] * 900 * dt, dy: -sticks[1] * 900 * dt)

        // Camera latency test: black, full white for 250 ms after each press, the run's label in
        // a corner so a slow-motion video shows which presenter it filmed.
        if let label = Self.flashLabel {
            let lit = now - lastInputAt < 0.25
            ctx.setFillColor(CGColor(gray: lit ? 1 : 0, alpha: 1))
            ctx.fill(CGRect(origin: .zero, size: canvas))
            text(ctx, label, at: CGPoint(x: 48, y: 96), size: 56, bold: true,
                 color: CGColor(gray: 0.5, alpha: 1))
            frame += 1
            return
        }

        drawBackdrop(ctx, now)
        drawHeader(ctx)
        drawTitle(ctx)
        drawInputPanel(ctx, now)
        drawPad(ctx)
        drawRipples(ctx, now)
        drawCursor(ctx)
        frame += 1
    }

    private func drawBackdrop(_ ctx: CGContext, _ t: Double) {
        ctx.drawLinearGradient(
            backdrop, start: .zero, end: CGPoint(x: canvas.width, y: canvas.height), options: [])
        // Two drifting glows: every frame differs, so a frozen stream is obvious.
        let a = CGPoint(
            x: canvas.width * (0.5 + 0.3 * cos(t * 0.35)), y: canvas.height * (0.42 + 0.2 * sin(t * 0.5)))
        let b = CGPoint(
            x: canvas.width * (0.5 - 0.32 * sin(t * 0.27)), y: canvas.height * (0.6 + 0.18 * cos(t * 0.41)))
        ctx.drawRadialGradient(
            violetGlow, startCenter: a, startRadius: 0, endCenter: a, endRadius: 560, options: [])
        ctx.drawRadialGradient(
            tealGlow, startCenter: b, startRadius: 0, endCenter: b, endRadius: 440, options: [])
    }

    private func drawHeader(_ ctx: CGContext) {
        text(ctx, "punktfunk demo host", at: CGPoint(x: 64, y: 84), size: 30, bold: true, color: white(0.92))
        text(
            ctx, "\(clock.string(from: Date()))   frame \(frame)",
            at: CGPoint(x: canvas.width - 64, y: 84), size: 30, color: white(0.7), anchor: .right)
    }

    private func drawTitle(_ ctx: CGContext) {
        let mid = canvas.width / 2
        text(ctx, title, at: CGPoint(x: mid, y: 400), size: 96, bold: true, color: white(1), anchor: .center)
        text(
            ctx, "Streamed from the demo host running inside this app.",
            at: CGPoint(x: mid, y: 476), size: 32, color: white(0.78), anchor: .center)
        text(
            ctx, "Move, click, type or use a controller \u{2014} the host draws it back here.",
            at: CGPoint(x: mid, y: 524), size: 32, color: white(0.78), anchor: .center)
    }

    private func drawInputPanel(_ ctx: CGContext, _ now: Double) {
        let rect = CGRect(x: 64, y: canvas.height - 264, width: 600, height: 200)
        let flash = max(0, 1 - (now - lastInputAt) / 0.5)
        panel(ctx, rect, highlight: flash)
        text(ctx, "LAST INPUT", at: CGPoint(x: rect.minX + 36, y: rect.minY + 62), size: 22, bold: true, color: white(0.55))
        text(ctx, lastInput, at: CGPoint(x: rect.minX + 36, y: rect.minY + 136), size: 44, bold: true, color: white(1))
    }

    private func drawPad(_ ctx: CGContext) {
        let rect = CGRect(x: canvas.width - 664, y: canvas.height - 264, width: 600, height: 200)
        panel(ctx, rect, highlight: 0)
        let y = rect.minY + 118
        // Triggers and bumpers along the top.
        bar(ctx, CGRect(x: rect.minX + 36, y: rect.minY + 26, width: 120, height: 16), fill: triggers[0])
        bar(ctx, CGRect(x: rect.maxX - 156, y: rect.minY + 26, width: 120, height: 16), fill: triggers[1])
        bar(ctx, CGRect(x: rect.minX + 176, y: rect.minY + 26, width: 80, height: 16), fill: lit(0x100) ? 1 : 0)
        bar(ctx, CGRect(x: rect.maxX - 256, y: rect.minY + 26, width: 80, height: 16), fill: lit(0x200) ? 1 : 0)
        stickRing(ctx, CGPoint(x: rect.minX + 96, y: y), sticks[0], sticks[1])
        stickRing(ctx, CGPoint(x: rect.maxX - 96, y: y), sticks[2], sticks[3])
        // D-pad.
        let d = CGPoint(x: rect.minX + 226, y: y)
        for (bit, dx, dy) in [(UInt32(0x1), 0.0, -30.0), (0x2, 0, 30), (0x4, -30, 0), (0x8, 30, 0)] {
            let r = CGRect(x: d.x + dx - 13, y: d.y + dy - 13, width: 26, height: 26)
            ctx.setFillColor(lit(bit) ? white(0.95) : white(0.18))
            ctx.addPath(CGPath(roundedRect: r, cornerWidth: 5, cornerHeight: 5, transform: nil))
            ctx.fillPath()
        }
        // Face buttons.
        let f = CGPoint(x: rect.maxX - 226, y: y)
        let face: [(UInt32, Double, Double, UInt32)] = [
            (0x1000, 0, 32, 0x3DDC84), (0x2000, 32, 0, 0xFF5C5C),
            (0x4000, -32, 0, 0x4F8CFF), (0x8000, 0, -32, 0xFFC83D),
        ]
        for (bit, dx, dy, hex) in face {
            let r = CGRect(x: f.x + dx - 17, y: f.y + dy - 17, width: 34, height: 34)
            ctx.setFillColor(rgb(hex, lit(bit) ? 1 : 0.22))
            ctx.fillEllipse(in: r)
        }
    }

    private func drawRipples(_ ctx: CGContext, _ now: Double) {
        ripples.removeAll { now - $0.born > 0.6 }
        ctx.setLineWidth(4)
        for ripple in ripples {
            let age = CGFloat((now - ripple.born) / 0.6)
            let r = 14 + age * 90
            ctx.setStrokeColor(rgb(0x9D86FF, 1 - age))
            ctx.strokeEllipse(in: CGRect(x: ripple.at.x - r, y: ripple.at.y - r, width: r * 2, height: r * 2))
        }
    }

    private func drawCursor(_ ctx: CGContext) {
        let p = cursor
        let arrow = CGMutablePath()
        arrow.addLines(between: [(0, 0), (0, 36), (9, 27), (16, 42), (23, 39), (16, 25), (28, 25)].map {
            CGPoint(x: p.x + $0.0, y: p.y + $0.1)
        })
        arrow.closeSubpath()
        ctx.addPath(arrow)
        ctx.setFillColor(white(1))
        ctx.setStrokeColor(CGColor(gray: 0, alpha: 0.85))
        ctx.setLineWidth(2.5)
        ctx.drawPath(using: .fillStroke)
    }

    // MARK: - Primitives

    private enum Anchor { case left, center, right }

    private func text(
        _ ctx: CGContext, _ string: String, at point: CGPoint, size: CGFloat, bold: Bool = false,
        color: CGColor, anchor: Anchor = .left
    ) {
        let key = "\(size)-\(bold)"
        let font = fonts[key] ?? {
            let f = CTFontCreateUIFontForLanguage(bold ? .emphasizedSystem : .system, size, nil)
                ?? CTFontCreateWithName("Helvetica" as CFString, size, nil)
            fonts[key] = f
            return f
        }()
        let attributes: [NSAttributedString.Key: Any] = [
            NSAttributedString.Key(kCTFontAttributeName as String): font,
            NSAttributedString.Key(kCTForegroundColorAttributeName as String): color,
        ]
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: string, attributes: attributes))
        let width = CGFloat(CTLineGetTypographicBounds(line, nil, nil, nil))
        let x: CGFloat = switch anchor {
        case .left: point.x
        case .center: point.x - width / 2
        case .right: point.x - width
        }
        ctx.textPosition = CGPoint(x: x, y: point.y)
        CTLineDraw(line, ctx)
    }

    private func panel(_ ctx: CGContext, _ rect: CGRect, highlight: Double) {
        let path = CGPath(roundedRect: rect, cornerWidth: 28, cornerHeight: 28, transform: nil)
        ctx.addPath(path)
        ctx.setFillColor(white(0.08))
        ctx.fillPath()
        ctx.addPath(path)
        ctx.setStrokeColor(highlight > 0 ? rgb(0x9D86FF, 0.35 + 0.65 * highlight) : white(0.14))
        ctx.setLineWidth(2)
        ctx.strokePath()
    }

    private func bar(_ ctx: CGContext, _ rect: CGRect, fill: CGFloat) {
        ctx.setFillColor(white(0.14))
        ctx.addPath(CGPath(roundedRect: rect, cornerWidth: 8, cornerHeight: 8, transform: nil))
        ctx.fillPath()
        guard fill > 0 else { return }
        let filled = CGRect(x: rect.minX, y: rect.minY, width: max(rect.height, rect.width * fill), height: rect.height)
        ctx.setFillColor(rgb(0x9D86FF, 1))
        ctx.addPath(CGPath(roundedRect: filled, cornerWidth: 8, cornerHeight: 8, transform: nil))
        ctx.fillPath()
    }

    private func stickRing(_ ctx: CGContext, _ center: CGPoint, _ x: CGFloat, _ y: CGFloat) {
        ctx.setStrokeColor(white(0.3))
        ctx.setLineWidth(3)
        ctx.strokeEllipse(in: CGRect(x: center.x - 50, y: center.y - 50, width: 100, height: 100))
        let dot = CGPoint(x: center.x + x * 34, y: center.y - y * 34)
        ctx.setFillColor(white(0.95))
        ctx.fillEllipse(in: CGRect(x: dot.x - 16, y: dot.y - 16, width: 32, height: 32))
    }

    private func lit(_ bit: UInt32) -> Bool { buttons & bit != 0 }

    private func white(_ alpha: CGFloat) -> CGColor { CGColor(gray: 1, alpha: alpha) }

    private func rgb(_ hex: UInt32, _ alpha: CGFloat) -> CGColor {
        CGColor(
            srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
    }

    /// One colour through `alphas`, or a run of opaque colours.
    private func gradient(_ hexes: [UInt32], alphas: [CGFloat]? = nil) -> CGGradient {
        let colors = alphas.map { a in a.map { rgb(hexes[0], $0) } } ?? hexes.map { rgb($0, 1) }
        return CGGradient(colorsSpace: space, colors: colors as CFArray, locations: nil)!
    }
}
