// The Punktfunk mark, drawn in code from the brand's symbol (assets/punktfunk-logo.svg): a disc
// behind, a disc in front up and to the right, and a leaf inside the front disc, cut by a third
// circle centred on the front disc's edge. The menu bar glyph is a template image in an 18 pt
// box; the panel and the setup window show the mark in the brand colours.
import AppKit
import SwiftUI

/// The mark in a square box, y growing downward. The three circles share one radius and the
/// mark spans three of them.
struct MarkGeometry: Equatable {
    let radius: CGFloat
    let back: CGPoint
    let front: CGPoint
    /// The centre of the circle that cuts the leaf out of the front disc.
    let cutter: CGPoint

    init(side: CGFloat, inset: CGFloat = 0) {
        let r = (side - 2 * inset) / 3
        let d = r / 2.squareRoot()
        radius = r
        back = CGPoint(x: inset + r, y: inset + 2 * r)
        front = CGPoint(x: inset + 2 * r, y: inset + r)
        cutter = CGPoint(x: front.x - d, y: front.y + d)
    }

    func box(_ center: CGPoint, grow: CGFloat = 0) -> CGRect {
        let r = radius + grow
        return CGRect(x: center.x - r, y: center.y - r, width: 2 * r, height: 2 * r)
    }
}

enum Glyph {
    static let size: CGFloat = 18
    static let geometry = MarkGeometry(side: size, inset: 1.5)
    /// The clear band that sets the front disc apart from the one behind it.
    static let gap: CGFloat = 1
    static let line: CGFloat = 1.3
    /// How dark the leaf stays inside a filled front disc.
    static let leafAlpha: CGFloat = 0.35

    static func image(_ state: GlyphState) -> NSImage {
        let image = NSImage(size: NSSize(width: size, height: size), flipped: true) { _ in
            draw(state)
            return true
        }
        image.isTemplate = true
        return image
    }

    private static func disc(_ center: CGPoint, grow: CGFloat = 0) -> NSBezierPath {
        NSBezierPath(ovalIn: geometry.box(center, grow: grow))
    }

    private static func clear(_ path: NSBezierPath) {
        NSGraphicsContext.current?.compositingOperation = .clear
        path.fill()
        NSGraphicsContext.current?.compositingOperation = .sourceOver
    }

    /// Runs `body` with drawing limited to the front disc.
    private static func insideFront(_ body: () -> Void) {
        NSGraphicsContext.saveGraphicsState()
        disc(geometry.front).addClip()
        body()
        NSGraphicsContext.restoreGraphicsState()
    }

    /// The back disc is always solid. The front disc is a ring holding a solid leaf, and fills
    /// while a stream runs, the leaf then lighter than the disc around it.
    private static func draw(_ state: GlyphState) {
        let g = geometry
        NSColor.black.set()
        disc(g.back).fill()
        clear(disc(g.front, grow: gap))
        if state.filled {
            disc(g.front).fill()
            insideFront {
                clear(disc(g.cutter))
                NSColor.black.withAlphaComponent(leafAlpha).set()
                disc(g.cutter).fill()
            }
            NSColor.black.set()
        } else {
            let ring = disc(g.front, grow: -line / 2)
            ring.lineWidth = line
            ring.stroke()
            insideFront { disc(g.cutter).fill() }
        }
        if state.badge {
            let dot = NSBezierPath(ovalIn: NSRect(x: size - 6, y: size - 6, width: 5, height: 5))
            clear(NSBezierPath(ovalIn: NSRect(x: size - 7.2, y: size - 7.2, width: 7.4, height: 7.4)))
            dot.fill()
        }
        if state.struck {
            let slash = NSBezierPath()
            slash.move(to: CGPoint(x: 2, y: 2))
            slash.line(to: CGPoint(x: size - 2, y: size - 2))
            slash.lineCapStyle = .round
            NSGraphicsContext.current?.compositingOperation = .clear
            slash.lineWidth = line * 3.4
            slash.stroke()
            NSGraphicsContext.current?.compositingOperation = .sourceOver
            slash.lineWidth = line
            slash.stroke()
        }
    }
}

/// The mark in colour: the light disc behind, the deep disc in front, the leaf lit toward the
/// front disc's centre. Dark appearance takes the brand's colours for a dark ground.
struct BrandMark: View {
    var size: CGFloat = 28
    @Environment(\.colorScheme) private var scheme

    var body: some View {
        Canvas { ctx, box in
            let g = MarkGeometry(side: box.width)
            let dark = scheme == .dark
            ctx.fill(Path(ellipseIn: g.box(g.back)), with: .color(dark ? Color(hex: 0xF2F1FE) : Color(hex: 0xA79FF8)))
            let front = Path(ellipseIn: g.box(g.front))
            ctx.fill(front, with: .color(dark ? Color(hex: 0x8C7DF5) : Color(hex: 0x6C5BF3)))
            ctx.clip(to: front)
            let lit = Gradient(colors: [Color(hex: 0xCEC9FB).opacity(0), Color(hex: 0xFCFCFF)])
            ctx.fill(Path(ellipseIn: g.box(g.cutter)), with: .linearGradient(lit, startPoint: g.cutter, endPoint: g.front))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

extension Color {
    init(hex: UInt32) {
        self.init(red: Double(hex >> 16 & 0xFF) / 255, green: Double(hex >> 8 & 0xFF) / 255, blue: Double(hex & 0xFF) / 255)
    }

    /// The brand accent: #6656F2 in light, #8678F5 in dark, as the Apple client's.
    static let brand = Color(NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(red: 0x86 / 255, green: 0x78 / 255, blue: 0xF5 / 255, alpha: 1)
            : NSColor(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255, alpha: 1)
    })
}
