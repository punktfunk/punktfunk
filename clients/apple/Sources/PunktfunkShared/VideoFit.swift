// Where a decoded frame lands in a view — the twin of `punktfunk_core::video_fit`. Every case in
// `clients/shared/video-fit-vectors.json` runs against both (`VideoFitTests`); change the rule
// there first.
//
// `dst` is the whole-pixel rect inside the view the picture fills; `src` the part of the frame
// that stays visible. A scale within `snapPixels` / `snapRelative` of a whole number snaps to it,
// so the frame shows 1:1 or pixel-replicated instead of resampled by a hair.

import CoreGraphics
import Foundation

/// How a frame whose aspect differs from the view fills it. `DefaultsKey.videoFit`, preset key
/// `video_fit`.
public enum VideoFit: String, CaseIterable, Sendable {
    case fit, crop, stretch

    /// Unknown names read as `.fit`, so a newer client's value degrades safely.
    public init(name: String?) {
        self = name.flatMap(VideoFit.init(rawValue:)) ?? .fit
    }

    /// The `Hello::video_fit` byte (`PUNKTFUNK_VIDEO_FIT_*`); `0` is Fit.
    public var wire: UInt8 {
        switch self {
        case .fit: return 0
        case .crop: return 1
        case .stretch: return 2
        }
    }

    /// The picker label every client shares.
    public var label: String {
        switch self {
        case .fit: return String(localized: L10n.playerVideoFitFit)
        case .crop: return String(localized: L10n.playerVideoFitCrop)
        case .stretch: return String(localized: L10n.playerVideoFitStretch)
        }
    }

    public static let snapPixels = 8.0
    public static let snapRelative = 0.005

    /// Place a `frame` (pixels) in a `view` (device pixels of the drawing surface).
    public func place(view: (width: Int, height: Int), frame: (width: Int, height: Int)) -> VideoPlacement {
        guard view.width > 0, view.height > 0, frame.width > 0, frame.height > 0 else {
            return .empty
        }
        let sx0 = Double(view.width) / Double(frame.width)
        let sy0 = Double(view.height) / Double(frame.height)
        let long = Double(max(frame.width, frame.height))
        let sx: Double, sy: Double
        switch self {
        case .fit: sx = Self.snap(min(sx0, sy0), long); sy = sx
        case .crop: sx = Self.snap(max(sx0, sy0), long); sy = sx
        case .stretch:
            sx = Self.snap(sx0, Double(frame.width))
            sy = Self.snap(sy0, Double(frame.height))
        }
        let w = Self.size(frame.width, sx), h = Self.size(frame.height, sy)
        let x0 = Self.floorHalf(view.width - w), y0 = Self.floorHalf(view.height - h)
        let scaleX = Double(w) / Double(frame.width), scaleY = Double(h) / Double(frame.height)
        let dstX = max(x0, 0), dstY = max(y0, 0)
        let dstW = max(min(x0 + w, view.width) - dstX, 0)
        let dstH = max(min(y0 + h, view.height) - dstY, 0)
        return VideoPlacement(
            dstX: dstX, dstY: dstY, dstWidth: dstW, dstHeight: dstH,
            srcX: Double(dstX - x0) / scaleX, srcY: Double(dstY - y0) / scaleY,
            srcWidth: Double(dstW) / scaleX, srcHeight: Double(dstH) / scaleY,
            scaleX: scaleX, scaleY: scaleY)
    }

    private static func snap(_ s: Double, _ len: Double) -> Double {
        let k = (s + 0.5).rounded(.down)
        return k >= 1 && abs(k - s) <= max(snapPixels / len, snapRelative * k) ? k : s
    }

    /// Rounded half away from zero, like the Rust twin; at least one pixel.
    private static func size(_ frame: Int, _ scale: Double) -> Int {
        max(Int((Double(frame) * scale + 0.5).rounded(.down)), 1)
    }

    private static func floorHalf(_ v: Int) -> Int {
        Int((Double(v) / 2).rounded(.down))
    }
}

/// The resampling filter one axis gets, picked from its scale.
public enum VideoKernel: String, Sendable {
    case copy, nearest
    case catmullRom = "catmull-rom"
    case lanczos

    public init(scale: Double) {
        if scale == 1 {
            self = .copy
        } else if scale > 1, scale == scale.rounded(.down) {
            self = .nearest
        } else if scale > 1 {
            self = .catmullRom
        } else {
            self = .lanczos
        }
    }
}

public struct VideoPlacement: Equatable, Sendable {
    public var dstX, dstY, dstWidth, dstHeight: Int
    public var srcX, srcY, srcWidth, srcHeight: Double
    /// View pixels per frame pixel. Exact whole numbers when snapped.
    public var scaleX, scaleY: Double

    public static let empty = VideoPlacement(
        dstX: 0, dstY: 0, dstWidth: 0, dstHeight: 0,
        srcX: 0, srcY: 0, srcWidth: 0, srcHeight: 0, scaleX: 1, scaleY: 1)

    public var isEmpty: Bool { dstWidth == 0 || dstHeight == 0 }
    public var kernelX: VideoKernel { VideoKernel(scale: scaleX) }
    public var kernelY: VideoKernel { VideoKernel(scale: scaleY) }

    /// View pixel → frame pixel, clamped onto the visible region.
    public func frame(fromView p: CGPoint) -> CGPoint {
        CGPoint(
            x: min(max(srcX + (Double(p.x) - Double(dstX)) / scaleX, srcX), srcX + srcWidth),
            y: min(max(srcY + (Double(p.y) - Double(dstY)) / scaleY, srcY), srcY + srcHeight))
    }

    /// Frame pixel → view pixel, clamped onto `dst`.
    public func view(fromFrame p: CGPoint) -> CGPoint {
        let x = min(max(Double(p.x), srcX), srcX + srcWidth)
        let y = min(max(Double(p.y), srcY), srcY + srcHeight)
        return CGPoint(x: Double(dstX) + (x - srcX) * scaleX, y: Double(dstY) + (y - srcY) * scaleY)
    }

    /// Whether a view pixel lies on the picture (not on a bar).
    public func contains(view p: CGPoint) -> Bool {
        Double(p.x) >= Double(dstX) && Double(p.x) <= Double(dstX + dstWidth)
            && Double(p.y) >= Double(dstY) && Double(p.y) <= Double(dstY + dstHeight)
    }
}
