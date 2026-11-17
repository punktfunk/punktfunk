// Usage: swift scripts/render-tvos-icon.swift clients/apple/App/punktfunk_Logo.icon
//        "clients/apple/App/Assets.xcassets/App Icon & Top Shelf Image.brandassets"
// Icon Composer 2.0 has no tvOS platform, so this bakes its Liquid Glass render into the tvOS
// parallax layers. ICTOOL overrides the renderer.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let icon = CommandLine.arguments[1]
let outDir = CommandLine.arguments[2]
let ictool = ProcessInfo.processInfo.environment["ICTOOL"]
    ?? "/Applications/Icon Composer.app/Contents/Executables/ictool"
let srgb = CGColorSpace(name: CGColorSpace.sRGB)!
let side = 1024

func context(_ w: Int, _ h: Int) -> CGContext {
    let ctx = CGContext(
        data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: w * 4,
        space: srgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.interpolationQuality = .high
    return ctx
}

func png(_ w: Int, _ h: Int, draw: (CGContext) -> Void) -> Data {
    let ctx = context(w, h)
    draw(ctx)
    let data = NSMutableData()
    let dst = CGImageDestinationCreateWithData(data, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(dst, ctx.makeImage()!, nil)
    CGImageDestinationFinalize(dst)
    return data as Data
}

func write(_ data: Data, _ path: String) {
    let url = URL(fileURLWithPath: outDir).appendingPathComponent(path)
    try! FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    try! data.write(to: url)
    print(path)
}

/// Renders only `groups` (icon.json indices, top first) over a solid fill of `grey` (0 or 1),
/// as premultiplied sRGB RGBA.
func render(_ groups: [Int], grey: Int) -> [UInt8] {
    let fm = FileManager.default
    let tmp = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
    defer { try? fm.removeItem(at: tmp) }
    let doc = tmp.appendingPathComponent("icon.icon")
    try! fm.createDirectory(at: tmp, withIntermediateDirectories: true)
    try! fm.copyItem(at: URL(fileURLWithPath: icon), to: doc)
    let jsonURL = doc.appendingPathComponent("icon.json")
    var json = try! JSONSerialization.jsonObject(with: Data(contentsOf: jsonURL)) as! [String: Any]
    let all = json["groups"] as! [Any]
    json["groups"] = groups.map { all[$0] }
    json["fill"] = ["solid": "srgb:\(grey),\(grey),\(grey),1"]
    try! JSONSerialization.data(withJSONObject: json).write(to: jsonURL)

    let out = tmp.appendingPathComponent("out.png")
    let p = Process()
    p.executableURL = URL(fileURLWithPath: ictool)
    p.arguments = [
        doc.path, "--export-image", "--output-file", out.path, "--platform", "iOS",
        "--rendition", "Default", "--width", "\(side)", "--height", "\(side)", "--scale", "1",
    ]
    p.standardOutput = FileHandle.nullDevice
    try! p.run()
    p.waitUntilExit()
    // A missing PNG is the error signal: ictool 2.0 writes none for a document it cannot read.
    guard let src = CGImageSourceCreateWithURL(out as CFURL, nil),
          let img = CGImageSourceCreateImageAtIndex(src, 0, nil)
    else { fatalError("ictool render of groups \(groups)") }
    let ctx = context(side, side)
    ctx.draw(img, in: CGRect(x: 0, y: 0, width: side, height: side))
    return Array(UnsafeBufferPointer(
        start: ctx.data!.assumingMemoryBound(to: UInt8.self), count: side * side * 4))
}

/// The glass of `groups` over transparency. Rendered on black and on white, a pixel's alpha is
/// 1 − (white − black) and the black render is its premultiplied colour.
func matte(_ groups: [Int]) -> CGImage {
    let black = render(groups, grey: 0), white = render(groups, grey: 1)
    let ctx = context(side, side)
    let px = ctx.data!.assumingMemoryBound(to: UInt8.self)
    // The squircle's own rim lives in the outer eighth; the art and its shadows sit inside.
    let inset = side / 8
    for y in inset..<(side - inset) {
        for x in inset..<(side - inset) {
            let o = (y * side + x) * 4
            var a = 0
            for c in 0..<3 { a += 255 - (Int(white[o + c]) - Int(black[o + c])) }
            a = min(max(a / 3, 0), 255)
            let edge = x == inset || y == inset || x == side - inset - 1 || y == side - inset - 1
            if edge && a > 8 { fatalError("groups \(groups) reach the rim crop at alpha \(a)") }
            px[o + 3] = UInt8(a)
            for c in 0..<3 { px[o + c] = min(black[o + c], UInt8(a)) }
        }
    }
    return ctx.makeImage()!
}

// A vertical sRGB approximation of icon.json's display-p3 automatic-gradient violet.
func gradient(_ ctx: CGContext, _ h: Int) {
    let top = CGColor(srgbRed: 0.49, green: 0.42, blue: 0.97, alpha: 1)
    let bottom = CGColor(srgbRed: 0.35, green: 0.26, blue: 0.91, alpha: 1)
    let g = CGGradient(colorsSpace: srgb, colors: [top, bottom] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(g, start: CGPoint(x: 0, y: h), end: .zero, options: [])
}

func place(_ ctx: CGContext, _ art: CGImage, _ w: Int, _ h: Int, fraction: Double) {
    let s = Double(h) * fraction
    ctx.draw(art, in: CGRect(x: (Double(w) - s) / 2, y: (Double(h) - s) / 2, width: s, height: s))
}

// icon.json group order: lens, right circle, left circle. The lens is the circles' overlap, so it
// shares the right circle's layer and the two never drift apart in the focus parallax.
let layers = [("Front", matte([0, 1])), ("Circle1", matte([2]))]

// App icon stacks: art at 92% of canvas height (tvOS crops edges in the focus effect).
for (stack, sizes) in [
    ("App Icon.imagestack", [("@1x", 400, 240), ("@2x", 800, 480)]),
    ("App Icon - App Store.imagestack", [("@1x", 1280, 768)]),
] {
    for (suffix, w, h) in sizes {
        write(png(w, h) { gradient($0, h) },
              "\(stack)/Back.imagestacklayer/Content.imageset/back\(suffix).png")
        for (name, art) in layers {
            write(png(w, h) { place($0, art, w, h, fraction: 0.92) },
                  "\(stack)/\(name).imagestacklayer/Content.imageset/\(name.lowercased())\(suffix).png")
        }
    }
}

// About's `AboutAppIcon`, beside the brand assets, is the small stack flattened: the icon the
// Home screen shows at rest, so the two can't drift apart.
for (suffix, w, h) in [("@1x", 400, 240), ("@2x", 800, 480)] {
    write(png(w, h) { ctx in
        gradient(ctx, h)
        for (_, art) in layers.reversed() { place(ctx, art, w, h, fraction: 0.92) }
    }, "../AboutAppIcon.imageset/about-icon\(suffix).png")
}

// Top shelf images are flat, so they take the whole mark with its refraction between groups.
let mark = matte([0, 1, 2])
for (path, w, h) in [
    ("Top Shelf Image.imageset/shelf@1x.png", 1920, 720),
    ("Top Shelf Image.imageset/shelf@2x.png", 3840, 1440),
    ("Top Shelf Image Wide.imageset/shelf-wide@1x.png", 2320, 720),
    ("Top Shelf Image Wide.imageset/shelf-wide@2x.png", 4640, 1440),
] {
    write(png(w, h) { gradient($0, h); place($0, mark, w, h, fraction: 0.7) }, path)
}
