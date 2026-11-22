// The Punktfunk mark, drawn in code: two overlapping discs, the front one upper right, from
// assets/punktfunk-logo.svg and scripts/gen-tray-icons.py. The menu bar glyph is a template
// image in an 18 pt box; the panel header shows the mark in the brand colours.
import AppKit
import SwiftUI

enum Glyph {
    static let size: CGFloat = 18
    /// The discs' radius and their centres' offset from the box centre, from the logo's
    /// geometry (R 194.41, centres ±97.4 in a 583.6 span) scaled into 15 pt.
    static let radius: CGFloat = 5.0
    static let offset: CGFloat = 2.5

    static func image(_ state: GlyphState) -> NSImage {
        let image = NSImage(size: NSSize(width: size, height: size), flipped: false) { _ in
            draw(state)
            return true
        }
        image.isTemplate = true
        return image
    }

    private static func disc(_ center: CGPoint, _ r: CGFloat) -> NSBezierPath {
        NSBezierPath(ovalIn: NSRect(x: center.x - r, y: center.y - r, width: 2 * r, height: 2 * r))
    }

    /// Clears `path` with a margin, so the next shape stands apart from what is under it.
    private static func knockOut(_ path: NSBezierPath, margin: CGFloat) {
        NSGraphicsContext.current?.compositingOperation = .clear
        path.lineWidth = margin * 2
        path.fill()
        path.stroke()
        NSGraphicsContext.current?.compositingOperation = .sourceOver
    }

    private static func draw(_ state: GlyphState) {
        let mid = size / 2
        let back = CGPoint(x: mid - offset, y: mid - offset)
        let front = CGPoint(x: mid + offset, y: mid + offset)
        let line: CGFloat = 1.4
        NSColor.black.set()
        if state.filled {
            disc(back, radius).fill()
            knockOut(disc(front, radius), margin: 1)
            disc(front, radius).fill()
        } else {
            let ring = disc(back, radius - line / 2)
            ring.lineWidth = line
            ring.stroke()
            knockOut(disc(front, radius - line / 2), margin: line)
            let top = disc(front, radius - line / 2)
            top.lineWidth = line
            top.stroke()
        }
        if state.badge {
            let center = CGPoint(x: size - 3.5, y: 3.5)
            knockOut(disc(center, 2.5), margin: 1.2)
            disc(center, 2.5).fill()
        }
        if state.struck {
            let slash = NSBezierPath()
            slash.move(to: CGPoint(x: 2, y: size - 2))
            slash.line(to: CGPoint(x: size - 2, y: 2))
            slash.lineCapStyle = .round
            knockOut(slash, margin: line * 1.2)
            slash.lineWidth = line
            slash.stroke()
        }
    }
}

/// The mark in colour: the light disc behind, the deep disc in front, their overlap lit.
struct BrandMark: View {
    var size: CGFloat = 28

    var body: some View {
        Canvas { ctx, box in
            let s = box.width / Glyph.size
            let r = Glyph.radius * s, o = Glyph.offset * s, mid = box.width / 2
            // SwiftUI's y grows downward: the front disc sits up and to the right.
            let back = Path(ellipseIn: CGRect(x: mid - o - r, y: mid + o - r, width: 2 * r, height: 2 * r))
            let front = Path(ellipseIn: CGRect(x: mid + o - r, y: mid - o - r, width: 2 * r, height: 2 * r))
            ctx.fill(back, with: .color(Color(red: 0xA7 / 255, green: 0x9F / 255, blue: 0xF8 / 255)))
            ctx.fill(front, with: .color(Color(red: 0x6C / 255, green: 0x5B / 255, blue: 0xF3 / 255)))
            ctx.clip(to: back)
            ctx.fill(front, with: .color(Color(red: 0xD2 / 255, green: 0xC9 / 255, blue: 0xFB / 255)))
        }
        .frame(width: size, height: size)
        .accessibilityHidden(true)
    }
}

extension Color {
    /// The brand accent: #6656F2 in light, #8678F5 in dark, as the Apple client's.
    static let brand = Color(NSColor(name: nil) { appearance in
        appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            ? NSColor(red: 0x86 / 255, green: 0x78 / 255, blue: 0xF5 / 255, alpha: 1)
            : NSColor(red: 0x66 / 255, green: 0x56 / 255, blue: 0xF2 / 255, alpha: 1)
    })
}
