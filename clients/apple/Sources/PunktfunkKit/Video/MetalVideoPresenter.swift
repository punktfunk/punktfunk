// Stage-2 presenter, present half: draw a decoded NV12 / P010 / 4:4:4 CVPixelBuffer into a CAMetalLayer
// drawable with a Y′CbCr→RGB shader. The hosting view's CADisplayLink still paces the pipeline once per
// vsync, but the actual `render` runs on Stage2Pipeline's dedicated RENDER THREAD (the link tick just
// signals it), so `nextDrawable()`'s blocking never lands on the main thread. See docs
// apple-stage2-presenter.md.
//
// Threading: created during view setup (main); `render`/`configure` run on the render thread — the
// layer's drawable/format/colour mutations all happen there (CAMetalLayer is designed for dedicated
// render threads; only the layer's GEOMETRY — frame/contentsScale — is touched from main, in
// SessionPresenter.layout, which also pushes the resulting pixel size here via `setDrawableTarget`
// so the render thread never reads layer geometry cross-thread). `setHdrMeta` (pump thread) and
// `setDrawableTarget` (main) only write lock-guarded staging state the render thread drains.

#if canImport(Metal) && canImport(QuartzCore)
import CoreGraphics
import CoreVideo
#if os(macOS)
import IOSurface
#endif
import Metal
import MetalPerformanceShaders
import QuartzCore
import os

private let presenterLog = ClientLog(category: "presenter")

#if os(macOS)
/// HOW a windowed (composited) macOS session pushes finished frames to glass — the DCP
/// "mismatched swapID's" kernel-panic saga's mechanism picker. Fullscreen always presents
/// `async` (direct-scanout promotion, lowest latency, no panic reports there); the windowed
/// mechanism is resolved per session by SessionPresenter (user setting +
/// PUNKTFUNK_WINDOWED_PRESENT env override) and routed here via `setWindowedPresent`.
///
/// - `async`: the CAMetalLayer image queue (`commandBuffer.present`) — the fastest composited
///   path and the PANIC TRIGGER on high-refresh displays (the out-of-band swaps race
///   WindowServer's compositor; it survived glass pacing and every codec).
/// - `transaction`: `CAMetalLayer.presentsWithTransaction` — the swap commits WITH the layer
///   tree, in lockstep with the compositor (Apple's documented remedy; validated no-panic on
///   the 240 Hz repro machine). The present is committed from the RENDER thread inside an
///   explicit CATransaction + flush — see `encodePresent` for why that beats the original
///   main-thread hop.
/// - `surface`: no image queue at all — render into a pooled IOSurface and swap it into a plain
///   CALayer's `contents` (the f407f418 PyroWave mitigation, resurrected format-aware:
///   rgba16Float + PQ tagging keeps HDR). WindowServer treats it as ordinary layer damage on
///   its own composite cadence. PROTOTYPE: whether the compositor honors PQ/EDR for plain-layer
///   IOSurface contents still needs an on-glass eyeball — the metal layer stays underneath with
///   `wantsExtendedDynamicRangeContent` as the EDR anchor.
enum WindowedPresentMode: String, Sendable {
    case async
    case transaction
    case surface
}
#endif

/// HDR reference white (BT.2408 "HDR Reference White"): the absolute luminance, in nits, that the
/// PQ signal's diffuse white sits at. Passed to `CAEDRMetadata.hdr10(opticalOutputScale:)`, it anchors
/// 203-nit diffuse white at EDR 1.0 (the display's SDR-white level) and lets the system tone-map the
/// brighter highlights into the panel's headroom. This is the missing anchor that made the old HDR path
/// render "way too bright" (no `edrMetadata` → no reference-white anchoring); a LARGER value renders
/// dimmer.
///
/// ⚠️ One half of a pair: hosts map SDR into the PQ container at the same 203 nits (gamescope's
/// `SDR_REFERENCE_WHITE_NITS`). A host-side SDR brightness setting deliberately moves SDR off it.
private let hdrReferenceWhiteNits: Float = 203.0

/// The SDR layer's colour tag. `colorspace = nil` means NO colour matching: the BT.709-encoded
/// stream is handed to the compositor untagged and drawn in the display's native space. That is
/// mild oversaturation on a P3 Mac or iPad, and on a tvOS display composited for HDR it also lifts
/// the black floor — the 2026-08-13 field report of greys where blacks should be, which arrived
/// with the client's own HDR switch already OFF, so no other stage had tagged those pixels either.
/// Tagging lets CoreAnimation colour-match into whatever the output actually is, which is the
/// strictly correct rendering, so it is now the default.
///
/// `PUNKTFUNK_SDR_COLORSPACE=none` restores the old untagged look — the A/B lever if a panel
/// regresses, or for anyone who preferred the more vivid rendition.
private let sdrColorspace: CGColorSpace? = {
    guard ProcessInfo.processInfo.environment["PUNKTFUNK_SDR_COLORSPACE"] != "none" else {
        return nil
    }
    return CGColorSpace(name: CGColorSpace.sRGB)
}()

/// The SDR drawable of a 10-bit session: the same gamma-encoded BT.709 samples as the 8-bit
/// path in a 10-bit unorm drawable, so the decode's depth reaches the panel instead of being
/// requantised at present. Tagged by `sdrColorspace` like the 8-bit one. Not an extended-range
/// format: those re-map the [0,1] range and want a different shader output.
///
/// `PUNKTFUNK_SDR10_DRAWABLE=8` keeps the 8-bit drawable — the A/B lever if a panel composites
/// the wide format wrong.
private let sdr10Drawable: MTLPixelFormat =
    ProcessInfo.processInfo.environment["PUNKTFUNK_SDR10_DRAWABLE"] == "8"
    ? .bgra8Unorm : .bgr10a2Unorm

/// Runtime-compiled (no metallib build step needed in SwiftPM): a fullscreen triangle and Y′CbCr→RGB
/// fragment shaders whose conversion arrives as three constant rows computed per frame on the CPU
/// (`CscRows` — the Swift port of pf-client-core's `csc_rows`, from the decoded buffer's actual
/// signaling). One set of coefficients honors BT.601/709/2020 × full/limited × 8/10-bit instead of
/// the old hardcoded BT.709/BT.2020 matrices — a BT.601-signaled stream (a Linux host's RGB-input
/// NVENC) used to render with BT.709 coefficients, a constant hue error. uv.y is flipped (1 - p.y)
/// so the top-left-origin texture presents upright (NDC y is up). The HDR shader outputs PQ-encoded
/// R′G′B′ as-is — the CAMetalLayer's `itur_2100_PQ` colour space + `edrMetadata` tell the system
/// compositor the samples are PQ and how to tone-map them (no EOTF here, matching the host's
/// BT.2020 PQ emission).
private let shaderSource = """
#include <metal_stdlib>
using namespace metal;

// `uv` addresses the visible part of the frame (chroma, and luma drawn as decoded); `luv` is luma's
// own, the unit square once `lumaForTarget` has already cropped and scaled it.
struct VOut { float4 pos [[position]]; float2 uv; float2 luv; };

// Origin + size of each, in texture UV (see `UvRects` in Swift).
struct UvRects { float4 frame; float4 luma; };

// The CPU-computed CSC rows (CscRows.swift, layout-matched): rgb[i] = dot(ri.xyz, yuv) + ri.w.
// Range expansion, the matrix, and the 10-bit MSB-packing factor are all folded in.
struct CscUniform { float4 r0; float4 r1; float4 r2; };

vertex VOut pf_vtx(uint vid [[vertex_id]], constant UvRects& rects [[buffer(0)]]) {
    float2 p = float2(float((vid << 1) & 2), float(vid & 2));
    VOut o;
    o.pos = float4(p * 2.0 - 1.0, 0.0, 1.0);
    float2 unit = float2(p.x, 1.0 - p.y);
    o.uv = rects.frame.xy + unit * rects.frame.zw;
    o.luv = rects.luma.xy + unit * rects.luma.zw;
    return o;
}

// Catmull-Rom luma for a drawable at or above the frame size: crisp upscale, exact texel at 1:1.
// A smaller drawable binds luma already Lanczos-scaled to its size (`lumaForTarget`), sampled 1:1.
// Nine bilinear taps (TheRealMJP's 16-tap optimisation); `s` MUST be linear. Chroma stays bilinear.
float catmullRomLuma(texture2d<float> tex, sampler s, float2 uv) {
    float2 texSize = float2(tex.get_width(), tex.get_height());
    float2 samplePos = uv * texSize;
    float2 tc1 = floor(samplePos - 0.5) + 0.5;
    float2 f = samplePos - tc1;
    float2 w0 = f * (-0.5 + f * (1.0 - 0.5 * f));
    float2 w1 = 1.0 + f * f * (-2.5 + 1.5 * f);
    float2 w2 = f * (0.5 + f * (2.0 - 1.5 * f));
    float2 w3 = f * f * (-0.5 + 0.5 * f);
    float2 w12 = w1 + w2;
    float2 off12 = w2 / w12;
    float2 tc0 = (tc1 - 1.0) / texSize;
    float2 tc3 = (tc1 + 2.0) / texSize;
    float2 tc12 = (tc1 + off12) / texSize;
    float r = 0.0;
    r += tex.sample(s, float2(tc0.x,  tc0.y)).r  * (w0.x  * w0.y);
    r += tex.sample(s, float2(tc12.x, tc0.y)).r  * (w12.x * w0.y);
    r += tex.sample(s, float2(tc3.x,  tc0.y)).r  * (w3.x  * w0.y);
    r += tex.sample(s, float2(tc0.x,  tc12.y)).r * (w0.x  * w12.y);
    r += tex.sample(s, float2(tc12.x, tc12.y)).r * (w12.x * w12.y);
    r += tex.sample(s, float2(tc3.x,  tc12.y)).r * (w3.x  * w12.y);
    r += tex.sample(s, float2(tc0.x,  tc3.y)).r  * (w0.x  * w3.y);
    r += tex.sample(s, float2(tc12.x, tc3.y)).r  * (w12.x * w3.y);
    r += tex.sample(s, float2(tc3.x,  tc3.y)).r  * (w3.x  * w3.y);
    return r;
}

// 4:2:0 chroma is left-cosited horizontally, centered vertically (H.273 chroma_loc 0); sampling at
// the luma UV assumes center siting, so shift +0.25 chroma texels. The shift self-disables when
// chroma is not narrower than the bound luma: 4:4:4, and 4:2:0 luma pre-scaled under 0.5×, where
// the missed shift stays under a quarter output pixel.
float2 chromaUV(texture2d<float> lumaTex, texture2d<float> chromaTex, float2 uv) {
    if (chromaTex.get_width() < lumaTex.get_width()) {
        uv.x += 0.25 / float(chromaTex.get_width());
    }
    return uv;
}

// The shared sample + row-multiply: Y′CbCr (bicubic luma, siting-corrected bilinear chroma) →
// RGB via the per-frame rows. A full-size 4:4:4 chroma plane needs no change vs 4:2:0 (the siting
// offset self-disables). What the result MEANS depends on the stream: an SDR frame's rows yield
// gamma-encoded RGB, an HDR frame's rows yield PQ-encoded R′G′B′ — the fragment variants below
// differ only in what they do next.
float3 sampleRgb(texture2d<float> lumaTex, texture2d<float> chromaTex, float2 luv, float2 uv,
                 constant CscUniform& csc) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
#ifdef PF_BILINEAR_LUMA
    // DEBUG (PUNKTFUNK_BILINEAR_LUMA=1): plain bilinear luma — Catmull-Rom OFF. A/B lever to see if
    // the bicubic overshoot contributes to edge fringing. NOTE: at a true 1:1 present both paths
    // reduce to the identity texel, so if this toggle VISIBLY changes the picture, the present is
    // NOT 1:1 (there's a resample); if it looks identical, the fringing is upstream (codec/source/OS).
    float lumaY = lumaTex.sample(s, luv).r;
#else
    float lumaY = catmullRomLuma(lumaTex, s, luv);
#endif
    float3 yuv = float3(lumaY,
                        chromaTex.sample(s, chromaUV(lumaTex, chromaTex, uv)).rg);
    return saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                           dot(csc.r1.xyz, yuv) + csc.r1.w,
                           dot(csc.r2.xyz, yuv) + csc.r2.w));
}

// SDR: 8-bit NV12 / 4:4:4 → full-range RGB, transfer left baked (shown as-is, the proven SDR
// layer config).
fragment float4 pf_frag(VOut in [[stage_in]],
                        texture2d<float> lumaTex [[texture(0)]],
                        texture2d<float> chromaTex [[texture(1)]],
                        constant CscUniform& csc [[buffer(0)]]) {
    return float4(sampleRgb(lumaTex, chromaTex, in.luv, in.uv, csc), 1.0);
}

// PyroWave planar SDR: three separate R8 planes (Y full-res, Cb/Cr half-res 4:2:0) from the
// Metal wavelet decoder — the Metal twin of pf-presenter's planar_csc.frag. Same bicubic luma
// and left-cosited chroma correction as the biplanar path (chromaUV self-disables at 4:4:4).
fragment float4 pf_frag_planar(VOut in [[stage_in]],
                               texture2d<float> lumaTex [[texture(0)]],
                               texture2d<float> cbTex [[texture(1)]],
                               texture2d<float> crTex [[texture(2)]],
                               constant CscUniform& csc [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
#ifdef PF_BILINEAR_LUMA
    float lumaY = lumaTex.sample(s, in.luv).r;
#else
    float lumaY = catmullRomLuma(lumaTex, s, in.luv);
#endif
    float2 cuv = chromaUV(lumaTex, cbTex, in.uv);
    float3 yuv = float3(lumaY, cbTex.sample(s, cuv).r, crTex.sample(s, cuv).r);
    float3 rgb = saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                                 dot(csc.r1.xyz, yuv) + csc.r1.w,
                                 dot(csc.r2.xyz, yuv) + csc.r2.w));
    return float4(rgb, 1.0);
}

// HDR: 10-bit P010 / 4:4:4 (BT.2020, PQ-encoded Y′CbCr) → full-range PQ R′G′B′, output as-is —
// the CAMetalLayer's itur_2100_PQ colour space + edrMetadata tell the compositor the samples are
// PQ, so it does the PQ→display tone-map. No EOTF here. The rows fold in the exact 10-bit
// MSB-packing factor (the old hardcoded shader carried a documented ~0.1% /1024-vs-/1023 error).
fragment float4 pf_frag_hdr(VOut in [[stage_in]],
                            texture2d<float> lumaTex [[texture(0)]],
                            texture2d<float> chromaTex [[texture(1)]],
                            constant CscUniform& csc [[buffer(0)]]) {
    return float4(sampleRgb(lumaTex, chromaTex, in.luv, in.uv, csc), 1.0);
}

// tvOS without HDR headroom has no EDR API and composites a PQ tag untone-mapped, so PQ becomes
// SDR in-shader for the sRGB-tagged layer. Same curve as pf-client-core's tonemap.glsl and the
// host's gamescope capture: BT.2390 EETF in PQ from a 1000-nit source onto 203-nit white, on
// max(R,G,B) in linear BT.709, then the sRGB OETF.
static inline float pqEotf(float e) {
    float p = pow(max(e, 0.0), 1.0 / 78.84375);
    return pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125);
}

static inline float pqOetf(float y) {
    float p = pow(clamp(y, 0.0, 1.0), 0.1593017578125);
    return pow((0.8359375 + 18.8515625 * p) / (1.0 + 18.6875 * p), 78.84375);
}

static inline float3 pqToSdr(float3 pq) {
    float3 lin = float3(pqEotf(pq.r), pqEotf(pq.g), pqEotf(pq.b));
    lin = max(float3(
        dot(lin, float3( 1.6605, -0.5876, -0.0728)),
        dot(lin, float3(-0.1246,  1.1329, -0.0083)),
        dot(lin, float3(-0.0182, -0.1006,  1.1187))), 0.0);
    float l = max(lin.r, max(lin.g, lin.b));
    const float white = 203.0 / 10000.0;
    if (l > 0.0) {
        float src = pqOetf(1000.0 / 10000.0);
        float maxLum = pqOetf(white) / src;
        float ks = 1.5 * maxLum - 0.5;
        float e = min(pqOetf(l) / src, 1.0);
        if (e > ks) {
            float t = (e - ks) / (1.0 - ks);
            float t2 = t * t;
            float t3 = t2 * t;
            e = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks)
                + (-2.0 * t3 + 3.0 * t2) * maxLum;
        }
        lin *= pqEotf(e * src) / l;
    }
    float3 c = saturate(lin / white);
    return select(1.055 * pow(c, 1.0 / 2.4) - 0.055, 12.92 * c, c <= 0.0031308);
}

fragment float4 pf_frag_hdr_tv(VOut in [[stage_in]],
                               texture2d<float> lumaTex [[texture(0)]],
                               texture2d<float> chromaTex [[texture(1)]],
                               constant CscUniform& csc [[buffer(0)]]) {
    // Y′CbCr → full-range PQ R′G′B′ via the per-frame rows (as pf_frag_hdr), then the tail.
    return float4(pqToSdr(sampleRgb(lumaTex, chromaTex, in.luv, in.uv, csc)), 1.0);
}

// PyroWave planar HDR tone-map: three separate R16 planes (P010-style studio codes; the rows
// fold in depth-10 MSB packing) → PQ R′G′B′ → the shared SDR tail. Used when a PQ pyrowave
// stream must land on an 8-bit surface: tvOS without HDR headroom. The passthrough planar
// HDR pipeline reuses pf_frag_planar itself on an rgba16Float drawable (identical math — the
// layer's itur_2100_PQ colour space + EDR metadata do the interpretation).
fragment float4 pf_frag_planar_tm(VOut in [[stage_in]],
                                  texture2d<float> lumaTex [[texture(0)]],
                                  texture2d<float> cbTex [[texture(1)]],
                                  texture2d<float> crTex [[texture(2)]],
                                  constant CscUniform& csc [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
#ifdef PF_BILINEAR_LUMA
    float lumaY = lumaTex.sample(s, in.luv).r;
#else
    float lumaY = catmullRomLuma(lumaTex, s, in.luv);
#endif
    float2 cuv = chromaUV(lumaTex, cbTex, in.uv);
    float3 yuv = float3(lumaY, cbTex.sample(s, cuv).r, crTex.sample(s, cuv).r);
    float3 pq = saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                                dot(csc.r1.xyz, yuv) + csc.r1.w,
                                dot(csc.r2.xyz, yuv) + csc.r2.w));
    return float4(pqToSdr(pq), 1.0);
}
"""

public final class MetalVideoPresenter {
    /// The layer the hosting view installs (as a sublayer) and sizes to its bounds.
    public let layer: CAMetalLayer

    #if os(macOS)
    /// WINDOWED-mode present coordination — the macOS DCP KERNEL PANIC mitigation.
    ///
    /// The panic ("mismatched swapID's" @UnifiedPipeline.cpp, WindowServer dies, machine reboots):
    /// the CAMetalLayer's ASYNCHRONOUS image queue (`commandBuffer.present(drawable)` — an
    /// out-of-band flip, mandatory with `displaySyncEnabled=false`) diverges from WindowServer's
    /// compositor on a high-refresh COMPOSITED (windowed) session — the compositor's notion of the
    /// current swap and the layer's queued swap disagree, and the DCP asserts. It survived glass
    /// pacing: a fully serialized one-in-flight present stream still panicked a 240 Hz Mac Studio
    /// (2026-07-18, PyroWave), and a windowed HEVC session panicked the same machine 2026-07-21 —
    /// so it is the async image queue itself, at any pacing or codec, not a present rate.
    ///
    /// The fix keeps the full render path (rgba16Float / PQ / EDR — real HDR is preserved) and
    /// only changes HOW the drawable is presented: `CAMetalLayer.presentsWithTransaction`. With it
    /// set, we don't hand the drawable to the command buffer; we commit, wait until scheduled, then
    /// call `drawable.present()` INSIDE a CATransaction — the present is enrolled in Core
    /// Animation's transaction and committed together with the layer tree, so the swap stays in
    /// lockstep with the compositor instead of racing it (Apple's documented remedy for Metal
    /// presentation drifting out of sync with CA). Fullscreen keeps the async path (direct-scanout
    /// promotion, lowest latency, no compositor and no panic reports there).
    ///
    /// 2026-07-21 latency rework: the mitigation MECHANISM is now a three-way pick
    /// (`WindowedPresentMode`) and the transactional present commits from the RENDER thread —
    /// see `encodePresent`. Staged under `stagingLock` (main pushes it via
    /// `setComposited`→`setWindowedPresent`); the render thread drains it and toggles the layer
    /// property + present style. `Active` is the render-thread copy so the layer property flips
    /// exactly once per mode change.
    private var windowedPresentStaged: WindowedPresentMode = .async
    private var windowedPresentActive: WindowedPresentMode = .async

    /// PUNKTFUNK_TXN_PRESENT=main — the ORIGINAL transactional present (commit →
    /// waitUntilScheduled → hop to the MAIN thread and present inside its CATransaction), kept
    /// as a field A/B lever. The default is the render-thread commit: the present harness
    /// (2026-07-21, this saga) measured the main hop landing a runloop turn late on a busy main
    /// thread, and an ACTIVE implicit transaction there NESTS the explicit one — presents batch
    /// at runloop-iteration rate (the field's presents=55 @ fps=240, display_p50 18.6 ms).
    /// Off-main commits measured immune to main-thread churn (~10 ms glass p50 at 240 Hz
    /// full-size vs 14+ ms under a churned main hop).
    private let txnPresentOnMain =
        ProcessInfo.processInfo.environment["PUNKTFUNK_TXN_PRESENT"] == "main"

    /// The WINDOWED-mode `surface` present target: a plain CALayer sized like `layer` (installed
    /// as a sibling ABOVE it by SessionPresenter), fed IOSurfaces via `contents` inside explicit
    /// CATransactions. Transparent (nil contents) whenever surface mode is off, so the metal
    /// layer below shows through. See `WindowedPresentMode.surface`.
    let surfaceLayer: CALayer = {
        let l = CALayer()
        l.contentsGravity = .resize // frame is already aspect-fit + pixel-snapped by layout
        l.isOpaque = true
        l.actions = ["contents": NSNull(), "bounds": NSNull(), "position": NSNull()]
        return l
    }()

    /// One IOSurface-backed render target of the windowed surface-present pool. All pool state
    /// is RENDER-THREAD confined; only the immutable surface refs cross threads (contents swap).
    private struct SurfaceSlot {
        let surface: IOSurfaceRef
        let texture: MTLTexture
        /// Monotonic use stamp — the reuse picker takes the least-recently-rendered free slot.
        var seq: UInt64 = 0
    }

    private var surfacePool: [SurfaceSlot] = []
    private var surfacePoolSize: CGSize = .zero
    /// What the pool's surfaces and textures were allocated for.
    private enum SurfaceDepth { case sdr8, sdr10, hdr }
    private var surfacePoolDepth = SurfaceDepth.sdr8
    private var surfaceSeq: UInt64 = 0
    /// Index of the slot most recently handed to the layer — never rewritten next, even if its
    /// use count already dropped (the compositor may still be scanning out the previous frame).
    private var lastHandedOff: Int?

    /// Once-per-second decomposition of the ACTIVE windowed present path (the field-diagnosis
    /// half of the DCP-latency work): scheduled/completed wait + commit/flush cost per present,
    /// and how many presents/swaps were issued. The pf-present line shows the GLASS side
    /// (latchMs / dropped); this shows the ISSUE side. Logged via `presenterLog` only while a
    /// windowed mechanism is active (zero cost fullscreen). Lock-guarded: transaction mode
    /// records from the render thread, surface mode from Metal completion threads.
    private final class WindowedPresentDiag: @unchecked Sendable {
        private let lock = NSLock()
        private var presents = 0
        private var schedMs: [Double] = []
        private var commitMs: [Double] = []
        private var last = CACurrentMediaTime()

        func record(schedMs sched: Double, commitMs commit: Double, mode: WindowedPresentMode) {
            lock.lock()
            presents += 1
            schedMs.append(sched)
            commitMs.append(commit)
            let now = CACurrentMediaTime()
            guard now - last >= 1 else {
                lock.unlock()
                return
            }
            last = now
            let sSched = schedMs.sorted()
            let sCommit = commitMs.sorted()
            let line = String(
                format: "pf-windowed mode=%@ presents=%d schedMs p50=%.2f max=%.2f "
                    + "commitMs p50=%.2f max=%.2f",
                mode.rawValue, presents, sSched[sSched.count / 2], sSched.last ?? 0,
                sCommit[sCommit.count / 2], sCommit.last ?? 0)
            presents = 0
            schedMs.removeAll(keepingCapacity: true)
            commitMs.removeAll(keepingCapacity: true)
            lock.unlock()
            presenterLog.info("\(line, privacy: .public)")
        }
    }

    private let windowedDiag = WindowedPresentDiag()
    #endif

    private let device: MTLDevice
    private let queue: MTLCommandQueue
    /// SDR (BT.709 8-bit → bgra8) and HDR (BT.2020 PQ 10-bit → rgba16Float) pipelines. Selected per
    /// frame in `render`; the layer is reconfigured to match when the session flips (HDR toggle).
    private let pipelineSDR: MTLRenderPipelineState
    /// `pf_frag` into `sdr10Drawable`: the SDR shader, wider attachment.
    private let pipelineSDR10: MTLRenderPipelineState
    private let pipelineHDR: MTLRenderPipelineState
    /// tvOS only: the in-shader PQ→SDR tone-map fallback (pf_frag_hdr_tv → bgra8), used whenever
    /// the display is composited without HDR headroom — see `setDisplayHeadroom`. nil elsewhere.
    private let pipelineHDRToneMap: MTLRenderPipelineState?
    /// PyroWave's 3-plane SDR path (pf_frag_planar → bgra8) — see `renderPlanar`.
    private let pipelinePlanar: MTLRenderPipelineState
    /// PyroWave planar HDR passthrough (pf_frag_planar → rgba16Float; the layer's PQ colour
    /// space + EDR interpret the samples) and the planar PQ→SDR tone-map (pf_frag_planar_tm →
    /// bgra8; tvOS without HDR headroom).
    private let pipelinePlanarHDR: MTLRenderPipelineState
    private let pipelinePlanarToneMap: MTLRenderPipelineState
    private var textureCache: CVMetalTextureCache?
    /// Apple's Lanczos scaler for luma whenever the render target is smaller than the frame; nil
    /// without MPS or under `PUNKTFUNK_DOWNSCALE=bicubic` (the shader's Catmull-Rom scales then).
    /// Render-thread confined, like `scaledLuma`, the reused target it writes.
    private lazy var lanczos: MPSImageLanczosScale? = {
        guard ProcessInfo.processInfo.environment["PUNKTFUNK_DOWNSCALE"] != "bicubic",
              MPSSupportsMTLDevice(device)
        else { return nil }
        let kernel = MPSImageLanczosScale(device: device)
        kernel.edgeMode = .clamp
        return kernel
    }()
    private var scaledLuma: MTLTexture?

    /// The PyroWave Metal decoder records on the presenter's device + queue: one device means
    /// decode, CSC and present share textures with zero interop, and one queue means Metal's
    /// hazard tracking orders a ring-slot rewrite after the render still sampling it.
    var metalDevice: MTLDevice { device }
    var metalQueue: MTLCommandQueue { queue }

    /// Current layer configuration — switched in `configure(hdr:)` when a frame's HDR-ness differs.
    /// Render-thread confined once the pipeline runs (Stage2Pipeline.start's one pre-thread
    /// `configure` call is ordered before the thread starts, so it doesn't race).
    private var hdrActive = false
    /// The SDR layer holds `sdr10Drawable` (a 10-bit session). Switched beside `hdrActive` in
    /// `configure`; not read while `hdrActive`.
    private var tenBitSDRActive = false
    /// Has `configureColor` run even once? `hdrActive` starts `false`, so a session that is SDR from
    /// the first frame matches the initial state and used to fall straight through `configure`'s
    /// guard — the layer then kept `make()`'s bare config, which never assigns a colour space, and
    /// the SDR stream presented untagged for the whole session. That also made
    /// `PUNKTFUNK_SDR_COLORSPACE` dead code on exactly the sessions it was meant to fix, so an
    /// operator A/B-ing it in the field saw nothing change. Same-state calls after the first are
    /// still no-ops, which is what the guard is for.
    private var didConfigureColor = false
    /// tvOS only: whether HDR frames currently present as PQ PASSTHROUGH (display has HDR headroom
    /// — its own tone-map applies) vs the in-shader tone-map fallback. Render-thread confined;
    /// derived from the staged display headroom at the top of every `render`.
    private var hdrPassthroughActive = false
    /// Last HDR mastering grade received via `setHdrMeta` (the host's 0xCE). Cached so a mid-session
    /// SDR→HDR flip's `configureColor` re-applies the real grade instead of clobbering it back to the
    /// bare reference-white anchor (an out-of-order race otherwise: `setHdrMeta` and the flip both write
    /// `edrMetadata`). Render-thread confined (drained from `pendingHdrMeta` at the top of `render`).
    private var lastHdrMeta: PunktfunkConnection.HdrMeta?

    /// Cross-thread staging, all under `stagingLock`: the pump thread parks a fresh 0xCE grade in
    /// `pendingHdrMeta` and the main thread parks the layout-derived drawable pixel size in
    /// `drawableTarget`; the render thread drains both at the top of `render`, so every layer
    /// format/colour mutation stays on the one thread that also calls `nextDrawable()`.
    private let stagingLock = NSLock()
    private var pendingHdrMeta: PunktfunkConnection.HdrMeta?
    private var drawableTarget: CGSize = .zero
    /// The visible part of the frame in texture UV, staged by `setSourceRect`. The unit square
    /// except under Crop to fill (or a few-pixel snap).
    private var sourceRect = CGRect(x: 0, y: 0, width: 1, height: 1)
    /// tvOS: the display's current EDR headroom (UIScreen.currentEDRHeadroom), pushed from the
    /// main thread (SessionPresenter.layout + the mode-switch observers). > 1 ⇒ the display is
    /// composited with HDR headroom, so HDR frames present as PQ passthrough; otherwise the
    /// in-shader tone-map keeps the picture from blowing out. 1 (the default) is the safe start.
    private var stagedDisplayHeadroom: CGFloat = 1.0

    #if DEBUG
    /// Last logged "decoded→drawable" signature, so the diagnostic logs only on a size/HDR change.
    private var lastSizeSig = ""
    #endif

    /// nil if Metal is unavailable (no GPU / a headless CI) or a shader fails to compile — the caller
    /// falls back to stage-1.
    public static func make() -> MetalVideoPresenter? {
        guard let device = MTLCreateSystemDefaultDevice(),
              let queue = device.makeCommandQueue()
        else { return nil }
        let pipelineSDR: MTLRenderPipelineState
        let pipelineSDR10: MTLRenderPipelineState
        let pipelineHDR: MTLRenderPipelineState
        let pipelineHDRToneMap: MTLRenderPipelineState?
        let pipelinePlanar: MTLRenderPipelineState
        let pipelinePlanarHDR: MTLRenderPipelineState
        let pipelinePlanarToneMap: MTLRenderPipelineState
        do {
            // DEBUG A/B lever: PUNKTFUNK_BILINEAR_LUMA=1 compiles the shader with Catmull-Rom OFF
            // (plain bilinear luma) by prepending a #define ahead of the source. Default (unset) is
            // the normal bicubic path. Read at presenter creation — set it in the environment and
            // relaunch to flip; the log line confirms which path built.
            let bilinearLuma = ProcessInfo.processInfo.environment["PUNKTFUNK_BILINEAR_LUMA"] == "1"
            let source = (bilinearLuma ? "#define PF_BILINEAR_LUMA 1\n" : "") + shaderSource
            if bilinearLuma {
                presenterLog.info("stage2: PUNKTFUNK_BILINEAR_LUMA=1 — Catmull-Rom luma DISABLED (bilinear)")
            }
            let library = try device.makeLibrary(source: source, options: nil)
            let vtx = library.makeFunction(name: "pf_vtx")
            let sdr = MTLRenderPipelineDescriptor()
            sdr.vertexFunction = vtx
            sdr.fragmentFunction = library.makeFunction(name: "pf_frag")
            sdr.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelineSDR = try device.makeRenderPipelineState(descriptor: sdr)
            let sdr10 = MTLRenderPipelineDescriptor()
            sdr10.vertexFunction = vtx
            sdr10.fragmentFunction = library.makeFunction(name: "pf_frag")
            sdr10.colorAttachments[0].pixelFormat = sdr10Drawable
            pipelineSDR10 = try device.makeRenderPipelineState(descriptor: sdr10)
            let hdr = MTLRenderPipelineDescriptor()
            hdr.vertexFunction = vtx
            hdr.fragmentFunction = library.makeFunction(name: "pf_frag_hdr")
            hdr.colorAttachments[0].pixelFormat = .rgba16Float // PQ passthrough
            pipelineHDR = try device.makeRenderPipelineState(descriptor: hdr)
            #if os(tvOS)
            // tvOS carries BOTH HDR pipelines: PQ passthrough when the display is composited
            // with HDR headroom, the in-shader tone-map (→ the 8-bit SDR config) when it isn't.
            // See setDisplayHeadroom / configureColor.
            let tm = MTLRenderPipelineDescriptor()
            tm.vertexFunction = vtx
            tm.fragmentFunction = library.makeFunction(name: "pf_frag_hdr_tv")
            tm.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelineHDRToneMap = try device.makeRenderPipelineState(descriptor: tm)
            #else
            pipelineHDRToneMap = nil
            #endif
            let planar = MTLRenderPipelineDescriptor()
            planar.vertexFunction = vtx
            planar.fragmentFunction = library.makeFunction(name: "pf_frag_planar")
            planar.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelinePlanar = try device.makeRenderPipelineState(descriptor: planar)
            let planarHdr = MTLRenderPipelineDescriptor()
            planarHdr.vertexFunction = vtx
            planarHdr.fragmentFunction = library.makeFunction(name: "pf_frag_planar")
            planarHdr.colorAttachments[0].pixelFormat = .rgba16Float // PQ passthrough
            pipelinePlanarHDR = try device.makeRenderPipelineState(descriptor: planarHdr)
            let planarTm = MTLRenderPipelineDescriptor()
            planarTm.vertexFunction = vtx
            planarTm.fragmentFunction = library.makeFunction(name: "pf_frag_planar_tm")
            planarTm.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelinePlanarToneMap = try device.makeRenderPipelineState(descriptor: planarTm)
        } catch {
            return nil
        }
        var cache: CVMetalTextureCache?
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        guard let textureCache = cache else { return nil }

        let layer = CAMetalLayer()
        layer.device = device
        layer.pixelFormat = .bgra8Unorm
        layer.framebufferOnly = true
        layer.isOpaque = true
        #if os(macOS)
        // displaySyncEnabled MUST stay false on macOS. It has flip-flopped, so the full history:
        // sync ON was tried twice and starves the drawable pool both times — on macOS 26 a synced
        // present only reaches glass when the WindowServer composites the window, and its FramePacing
        // path does not treat our out-of-band image-queue presents as damage, so with a static scene
        // the ONLY recurring damage is the 1 Hz stats HUD update: presents queue, all drawables stay
        // held, `nextDrawable()` sleeps (sampled: ~70% of the render thread inside
        // CAMetalLayerPrivateNextDrawableLocked → usleep), and the stream turns into a ~1 fps
        // slideshow with normal-LOOKING stats (each rare frame is fresh, newest-wins ring). The
        // 94fb7d1b fullscreen judder was the same starvation biting the then-main-thread render.
        // With sync OFF the flip is immediate; the vsync alignment that sync was supposed to give
        // (the HUD-off direct-scanout pacing fix) comes from scheduling the present at the display
        // link's target time instead (`present(at:)` — see `render`).
        layer.displaySyncEnabled = false
        #endif
        // The drawable is rendered at the LAYER's pixel size (set per-frame in `render`), so the
        // shader — not the compositor — performs the decoded→on-screen scale (bicubic luma; the
        // compositor's contentsGravity path is plain bilinear). The gravity stays aspect-fit as a
        // transient fallback: during a live resize the compositor may composite a drawable from
        // the previous layout before the next render catches up.
        layer.contentsGravity = .resizeAspect
        // Triple-buffer: more in-flight drawables before `nextDrawable()` (called on the display-link /
        // MAIN thread) has to block waiting for one to free.
        // ⚠ This is the STAGE-2/3 depth. Stage-4 (deadline pacing, the iOS default) never
        // calls `nextDrawable()` — the link vends every drawable — so the third slot only gives
        // the compositor room to queue a second present ahead of scanout, i.e. the two-refresh
        // present floor. `Stage2Pipeline.startDeadlinePresenter` clamps it to 2 for that pacing;
        // keep the two in step if this number ever changes.
        layer.maximumDrawableCount = 3

        return MetalVideoPresenter(
            device: device, queue: queue, pipelineSDR: pipelineSDR, pipelineSDR10: pipelineSDR10,
            pipelineHDR: pipelineHDR,
            pipelineHDRToneMap: pipelineHDRToneMap, pipelinePlanar: pipelinePlanar,
            pipelinePlanarHDR: pipelinePlanarHDR, pipelinePlanarToneMap: pipelinePlanarToneMap,
            textureCache: textureCache, layer: layer)
    }

    private init(
        device: MTLDevice, queue: MTLCommandQueue, pipelineSDR: MTLRenderPipelineState,
        pipelineSDR10: MTLRenderPipelineState,
        pipelineHDR: MTLRenderPipelineState, pipelineHDRToneMap: MTLRenderPipelineState?,
        pipelinePlanar: MTLRenderPipelineState,
        pipelinePlanarHDR: MTLRenderPipelineState,
        pipelinePlanarToneMap: MTLRenderPipelineState,
        textureCache: CVMetalTextureCache, layer: CAMetalLayer
    ) {
        self.device = device
        self.queue = queue
        self.pipelineSDR = pipelineSDR
        self.pipelineSDR10 = pipelineSDR10
        self.pipelineHDR = pipelineHDR
        self.pipelineHDRToneMap = pipelineHDRToneMap
        self.pipelinePlanar = pipelinePlanar
        self.pipelinePlanarHDR = pipelinePlanarHDR
        self.pipelinePlanarToneMap = pipelinePlanarToneMap
        self.textureCache = textureCache
        self.layer = layer
    }

    /// Configure the layer + active pipeline for an SDR or HDR session. Called once at session start
    /// (main, before the render thread exists) and again per-frame from `render` on the RENDER THREAD
    /// (idempotent — the guard makes a same-state call a no-op), so a mid-session HDR toggle (the host
    /// re-inits its encoder; the decoded `frame.isHDR` flips) reconfigures here automatically. HDR uses
    /// an rgba16Float drawable + BT.2020 PQ colour space + EDR with a 203-nit reference-white anchor;
    /// SDR uses the sRGB-tagged 8-bit drawable, or `sdr10Drawable` when `tenBitSDR` (a 10-bit
    /// session — HDR outranks it, an HDR frame never lands on the SDR-10 drawable).
    public func configure(hdr: Bool, tenBitSDR: Bool = false) {
        let wide = tenBitSDR && !hdr
        #if os(tvOS)
        // Reconfigure on an HDR flip AND on a passthrough↔tone-map flip: the display's headroom
        // changes when the AVDisplayManager mode switch (requested at session start) completes —
        // typically a second or two into the session.
        stagingLock.lock()
        let passthrough = stagedDisplayHeadroom > 1.0
        stagingLock.unlock()
        guard !didConfigureColor || hdr != hdrActive || wide != tenBitSDRActive
            || (hdr && passthrough != hdrPassthroughActive)
        else { return }
        hdrActive = hdr
        tenBitSDRActive = wide
        hdrPassthroughActive = passthrough
        #else
        guard !didConfigureColor || hdr != hdrActive || wide != tenBitSDRActive else { return }
        hdrActive = hdr
        tenBitSDRActive = wide
        #endif
        didConfigureColor = true
        configureColor(hdr: hdr, tenBitSDR: wide)
    }

    /// tvOS: park the display's current EDR headroom (a MAIN-thread `UIScreen` read — pushed by
    /// SessionPresenter.layout and the stream view's mode-switch observers). > 1 flips HDR frames
    /// to PQ passthrough (the display's own tone-map applies); ≤ 1 keeps the in-shader tone-map.
    /// Applied by the render thread on the next frame, like every other staged value here.
    public func setDisplayHeadroom(_ headroom: CGFloat) {
        stagingLock.lock()
        stagedDisplayHeadroom = headroom
        stagingLock.unlock()
    }

    /// Set the layer's pixel format + colour config for SDR or HDR. MAIN THREAD ONLY. EDR is requested
    /// on macOS + iOS (the old `#if os(macOS)` guard left iOS EDR half-engaged). tvOS has NO EDR API
    /// (`wantsExtendedDynamicRangeContent`/`edrMetadata`/`CAEDRMetadata` are all unavailable there) —
    /// and a bare PQ colour-space tag composites UNtone-mapped (the "overblown HDR" Apple TV report),
    /// so tvOS instead tone-maps PQ→SDR in the shader (pf_frag_hdr_tv) and keeps the SDR layer config.
    private func configureColor(hdr: Bool, tenBitSDR: Bool) {
        if hdr {
            #if os(tvOS)
            if hdrPassthroughActive {
                // Display composited WITH HDR headroom (the session's AVDisplayManager request
                // landed): emit PQ passthrough — in a real HDR10 output that's the correct
                // emission, and the TV applies its own tone-map.
                layer.pixelFormat = .rgba16Float
                layer.colorspace = CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            } else {
                // SDR-composited display: PQ would render untone-mapped (blown out) — the
                // pf_frag_hdr_tv shader tone-maps to SDR instead. Its output is BT.709, so it
                // carries the same SDR tag as a genuinely SDR session.
                layer.pixelFormat = .bgra8Unorm
                layer.colorspace = sdrColorspace
            }
            #else
            layer.pixelFormat = .rgba16Float
            layer.colorspace = CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            layer.wantsExtendedDynamicRangeContent = true
            // Anchor reference white. Re-apply the real grade if one already arrived (0xCE before the
            // flip); otherwise the bare 203-nit anchor. Without this anchor the PQ signal is too bright.
            layer.edrMetadata = makeEDR(lastHdrMeta)
            #endif
        } else {
            // SDR: gamma-encoded BT.709 [0,1], tagged so CoreAnimation colour-matches it into
            // the output rather than drawing it in the panel's native space (see sdrColorspace;
            // PUNKTFUNK_SDR_COLORSPACE=none restores untagged). A 10-bit session takes the
            // 10-bit drawable so the decode's depth reaches the panel.
            layer.pixelFormat = tenBitSDR ? sdr10Drawable : .bgra8Unorm
            layer.colorspace = sdrColorspace
            #if !os(tvOS)
            layer.wantsExtendedDynamicRangeContent = false
            layer.edrMetadata = nil
            #endif
        }
    }

    #if !os(tvOS)
    private func makeEDR(_ meta: PunktfunkConnection.HdrMeta?) -> CAEDRMetadata {
        CAEDRMetadata.hdr10(
            displayInfo: meta?.masteringDisplayColorVolume(),
            contentInfo: meta?.contentLightLevelInfo(),
            opticalOutputScale: hdrReferenceWhiteNits)
    }
    #endif

    /// Update the HDR mastering metadata (drained from the host's 0xCE datagram) to refine the system
    /// tone-map from the real grade. Called from the PUMP thread — the grade is only PARKED here (lock-
    /// guarded); the render thread applies it at the top of the next `render`, keeping every layer
    /// colour mutation on the one thread that also vends drawables.
    public func setHdrMeta(_ meta: PunktfunkConnection.HdrMeta) {
        stagingLock.lock()
        pendingHdrMeta = meta
        stagingLock.unlock()
    }

    /// Park the drawable pixel size the shader should render at: the metal layer's laid-out frame ×
    /// contentsScale, both owned by the MAIN thread (SessionPresenter.layout pushes it on every layout/
    /// backing change). The render thread reads this instead of the layer's geometry so it never
    /// touches main-owned CALayer state. Zero until the first layout → `render` falls back to the
    /// decoded frame size.
    public func setDrawableTarget(_ size: CGSize) {
        stagingLock.lock()
        drawableTarget = size
        stagingLock.unlock()
    }

    /// Park the visible part of the frame (texture UV, top-left origin) the drawable shows —
    /// SessionPresenter.layout's placement. MAIN thread; drained per present.
    public func setSourceRect(_ rect: CGRect) {
        stagingLock.lock()
        sourceRect = rect
        stagingLock.unlock()
    }

    #if os(macOS)
    /// Park the windowed present mechanism (MAIN thread — the hosting view pushes its window
    /// state on every layout; SessionPresenter resolves the mechanism per session). `.async` =
    /// FULLSCREEN (or the user opted out of the mitigation): the image queue. `.transaction` /
    /// `.surface` = COMPOSITED (windowed) mitigation mechanisms — see `WindowedPresentMode`.
    /// Applied by the render thread on the next frame, like every other staged value here.
    func setWindowedPresent(_ mode: WindowedPresentMode) {
        stagingLock.lock()
        windowedPresentStaged = mode
        stagingLock.unlock()
        // Leaving `surface` means somebody is about to clear the layer's contents. A swap already
        // committed would otherwise land afterwards and put a stale frame back on an opaque layer
        // that sits ABOVE the metal one, covering the live stream for the rest of the session.
        if mode != .surface { surfaceEpoch.bump() }
    }

    /// Generation of the surface-present target, so a completion handler can tell whether the
    /// layer it is about to write to is still the one it rendered for. Its own object because the
    /// handler must not retain the presenter.
    final class SurfaceEpoch: @unchecked Sendable {
        private let lock = NSLock()
        private var value: UInt64 = 0
        func bump() { lock.lock(); value &+= 1; lock.unlock() }
        func current() -> UInt64 { lock.lock(); defer { lock.unlock() }; return value }
        func isCurrent(_ v: UInt64) -> Bool { current() == v }
    }
    let surfaceEpoch = SurfaceEpoch()
    #endif

    /// Deadline pacing only, RENDER THREAD: reconcile the layer with a decoded frame BEFORE a
    /// drawable exists. The link vends from the layer's CURRENT config, and the layer starts
    /// with `drawableSize` 0 (it never tracks bounds once set explicitly, and the sublayer's
    /// frame isn't even laid out when the link spins up) — so leaving all reconciliation to the
    /// render path (which needs a frame AND a vended drawable) deadlocks at session start:
    /// every vend fails allocation at 0×0, the stash stays empty, no pair ever completes, and
    /// the size is never set. The 2026-07-19 iPad black screen ("[CAMetalLayer nextDrawable]
    /// returning nil because allocation failed" every refresh). Called on EVERY frame arrival:
    /// drains the same staging the render path drains (both are idempotent about it) and
    /// applies size + HDR config, so the next vend always matches the frame about to present —
    /// this also makes a mid-session HDR flip cost at most one skipped vend instead of waiting
    /// for a paired present to retag the layer.
    /// Drain the staged HDR grade and apply it. RENDER THREAD (or `reconcileLayer`'s caller):
    /// idempotent, so every present path can call it and the first one to run wins. Every path
    /// must — a stream whose path skipped it tone-maps against the bare reference-white anchor
    /// with no mastering volume for the whole session.
    private func applyStagedHdrMeta() {
        stagingLock.lock()
        let newHdrMeta = pendingHdrMeta
        pendingHdrMeta = nil
        stagingLock.unlock()
        guard let newHdrMeta else { return }
        lastHdrMeta = newHdrMeta
        // tvOS has no edrMetadata — the cached grade still matters for a later flip's
        // configureColor. macOS/iOS refine the live tone-map now.
        #if !os(tvOS)
        if hdrActive { layer.edrMetadata = makeEDR(newHdrMeta) }
        #endif
    }

    /// A 10-bit decode (P010 / `x444`, either range) — the buffer's own word, not the session's.
    static func tenBitBuffer(_ buffer: CVPixelBuffer) -> Bool {
        let pf = CVPixelBufferGetPixelFormatType(buffer)
        return pf == kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
            || pf == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
            || pf == kCVPixelFormatType_444YpCbCr10BiPlanarVideoRange
            || pf == kCVPixelFormatType_444YpCbCr10BiPlanarFullRange
    }

    func reconcileLayer(decodedSize: CGSize, isHDR: Bool, tenBitSDR: Bool = false) {
        stagingLock.lock()
        let targetFromLayout = drawableTarget
        stagingLock.unlock()
        configure(hdr: isHDR, tenBitSDR: tenBitSDR)
        applyStagedHdrMeta()
        let targetSize = (targetFromLayout.width > 0 && targetFromLayout.height > 0)
            ? targetFromLayout : decodedSize
        if layer.drawableSize != targetSize { layer.drawableSize = targetSize }
    }

    /// Draw one decoded frame to the next drawable and present it. RENDER THREAD (Stage2Pipeline's;
    /// `nextDrawable()` may block up to a frame — that wait belongs here, never on main). `isHDR`
    /// selects the 10-bit BT.2020 PQ path vs the 8-bit BT.709 path and is reconciled with the
    /// layer config via `configure`. Returns true on success; false when there's no drawable yet, a
    /// texture couldn't be made, or Metal errored — the caller then doesn't stamp a present (and can
    /// requeue the frame). `onPresented` fires once the drawable actually reached glass, with the
    /// `CLOCK_REALTIME` instant from the drawable's `presentedTime` — or nil when the system reports
    /// none (a dropped drawable). It runs on a Metal callback thread; keep the handler thread-safe.
    ///
    /// `presentAtMediaTime` (a `CACurrentMediaTime`-basis host time — the display link's
    /// `targetTimestamp`) schedules the flip ON the vsync instead of "as soon as the GPU finishes":
    /// with the layer's own sync disabled (mandatory on macOS — see init) an immediate present hits
    /// glass mid-refresh whenever the layer is direct-scanout promoted (fullscreen, no HUD), which
    /// is the "frametimes are off with the stats HUD closed" report. nil presents immediately
    /// (`PUNKTFUNK_PRESENT_MODE=immediate` — the pre-fix behavior, kept as a diagnostic A/B).
    ///
    /// `into drawable` (deadline pacing) supplies the CAMetalDisplayLink-vended drawable to
    /// render into instead of calling `nextDrawable()` — see `encodePresent` for the format
    /// guard that skips a vend the layer's config outran.
    @discardableResult
    public func render(
        _ pixelBuffer: CVPixelBuffer, isHDR: Bool = false,
        presentAtMediaTime: CFTimeInterval? = nil,
        into drawable: CAMetalDrawable? = nil,
        onPresented: ((Int64?) -> Void)? = nil
    ) -> Bool {
        // Drain the cross-thread staging (see `stagingLock`): the layout-derived drawable size and
        // any freshly-arrived HDR grade, both applied from this thread.
        stagingLock.lock()
        let targetFromLayout = drawableTarget
        stagingLock.unlock()

        // P010/x444 store 10-bit luma/chroma in 16-bit samples → R16/RG16; NV12/444v is 8-bit → R8/RG8.
        // Derived from the actual decoded buffer so a 4:4:4 (full chroma plane) frame just works.
        let tenBit = Self.tenBitBuffer(pixelBuffer)
        // Reconcile the layer with the decoded frame's HDR-ness and depth (a mid-session SDR↔HDR
        // flip, and the 10-bit SDR drawable).
        configure(hdr: isHDR, tenBitSDR: tenBit)
        applyStagedHdrMeta()
        // The frame's Y′CbCr→RGB rows, from its ACTUAL signaling (buffer attachments + pixel
        // format) — a BT.601-signaled stream gets 601 coefficients, full-range gets full-range
        // expansion; recomputed per frame because the host can flip colour in-band (SDR↔HDR).
        var csc = CscRows.rows(
            CscRows.signal(of: pixelBuffer), depth: tenBit ? 10 : 8, msbPacked: tenBit)
        guard let textureCache,
              let luma = makeTexture(
                pixelBuffer, plane: 0, format: tenBit ? .r16Unorm : .r8Unorm, cache: textureCache),
              let chroma = makeTexture(
                pixelBuffer, plane: 1, format: tenBit ? .rg16Unorm : .rg8Unorm, cache: textureCache),
              let lumaTexture = CVMetalTextureGetTexture(luma)
        else { return false }
        // The cache holds a reference to every IOSurface it has wrapped until asked not to, which
        // pins the decoder pool's buffers — including the previous mode's full-size ones across a
        // mid-stream resize. The two textures above are retained locally, so flushing now only
        // drops what nothing is using.
        CVMetalTextureCacheFlush(textureCache, 0)

        #if os(tvOS)
        // HDR splits by the display's headroom (kept in step with the layer by `configure` above):
        // PQ passthrough into an HDR-composited display, the tone-map shader otherwise.
        let hdrPipeline = hdrPassthroughActive ? pipelineHDR : (pipelineHDRToneMap ?? pipelineHDR)
        let pipeline = hdrActive ? hdrPipeline : (tenBitSDRActive ? pipelineSDR10 : pipelineSDR)
        #else
        let pipeline = hdrActive ? pipelineHDR : (tenBitSDRActive ? pipelineSDR10 : pipelineSDR)
        #endif
        let decodedSize = CGSize(
            width: CVPixelBufferGetWidth(pixelBuffer), height: CVPixelBufferGetHeight(pixelBuffer))
        return encodePresent(
            decodedSize: decodedSize, targetFromLayout: targetFromLayout, pipeline: pipeline,
            presentAtMediaTime: presentAtMediaTime, providedDrawable: drawable,
            onPresented: onPresented, luma: lumaTexture,
            // Hold the CVMetalTextures + source pixel buffer (its IOSurface) alive until the GPU
            // finishes sampling — releasing them at scope exit could free the backing mid-read.
            keepAlive: [luma, chroma, pixelBuffer]
        ) { encoder in
            encoder.setFragmentTexture(CVMetalTextureGetTexture(chroma), index: 1)
            encoder.setFragmentBytes(&csc, length: MemoryLayout<CscUniform>.stride, index: 0)
        }
    }

    /// Draw one PyroWave planar frame (three R8 planes off the Metal wavelet decoder) and
    /// present it. RENDER THREAD, same contract as `render` — PyroWave is 8-bit SDR, so the
    /// layer always takes the plain SDR config, and the CSC rows arrive precomputed from the
    /// stream's own sequence-header signaling (no CVPixelBuffer to inspect).
    @discardableResult
    func renderPlanar(
        _ planes: WaveletPlanes,
        presentAtMediaTime: CFTimeInterval? = nil,
        into drawable: CAMetalDrawable? = nil,
        onPresented: ((Int64?) -> Void)? = nil
    ) -> Bool {
        stagingLock.lock()
        let targetFromLayout = drawableTarget
        stagingLock.unlock()
        // A PQ (HDR) pyrowave stream drives the same layer/EDR machinery as the biplanar path —
        // including macOS windowed sessions, which keep real HDR (the DCP mitigation is the
        // transactional present in `encodePresent`, not a colour downgrade).
        configure(hdr: planes.pq)
        applyStagedHdrMeta()
        var csc = planes.csc
        // PQ passthrough needs the HDR drawable; a PQ frame while the drawable is (still)
        // 8-bit — tvOS without display headroom, or a not-yet-flipped layer — tone-maps
        // in-shader instead (the pipeline must match the drawable's pixel format).
        #if os(tvOS)
        let planarPassthrough = hdrActive && hdrPassthroughActive
        #else
        let planarPassthrough = hdrActive
        #endif
        let planarPipeline: MTLRenderPipelineState =
            planes.pq
                ? (planarPassthrough ? pipelinePlanarHDR : pipelinePlanarToneMap)
                : pipelinePlanar
        return encodePresent(
            decodedSize: CGSize(width: planes.width, height: planes.height),
            targetFromLayout: targetFromLayout, pipeline: planarPipeline,
            presentAtMediaTime: presentAtMediaTime, providedDrawable: drawable,
            onPresented: onPresented, luma: planes.y,
            // The ring textures stay valid by ring depth; retaining them here also pins the
            // slot's set until the sample completes (mirrors the biplanar keep-alive).
            keepAlive: [planes.y, planes.cb, planes.cr]
        ) { encoder in
            encoder.setFragmentTexture(planes.cb, index: 1)
            encoder.setFragmentTexture(planes.cr, index: 2)
            encoder.setFragmentBytes(&csc, length: MemoryLayout<CscUniform>.stride, index: 0)
        }
    }

    /// The shared present tail of `render`/`renderPlanar`: size the drawable, encode one
    /// fullscreen triangle with `pipeline` (`luma` through `lumaForTarget` at index 0, `bind` the
    /// rest), schedule the present and the on-glass callback.
    ///
    /// `providedDrawable` (deadline pacing) is the CAMetalDisplayLink-vended drawable to render
    /// into instead of `nextDrawable()`. It was vended against the layer's config at vend time,
    /// so after a mid-session reconfigure (HDR flip: `configure` above already retagged the
    /// layer) its pixel format can lag the pipeline's attachment format — encoding would be a
    /// Metal validation failure. The guard returns false instead: the drawable drops back to
    /// the pool, the caller re-rings the frame, and the link's next vend carries the new format.
    private func encodePresent(
        decodedSize: CGSize, targetFromLayout: CGSize, pipeline: MTLRenderPipelineState,
        presentAtMediaTime: CFTimeInterval?, providedDrawable: CAMetalDrawable? = nil,
        onPresented: ((Int64?) -> Void)?,
        luma: MTLTexture, keepAlive: [Any], bind: (MTLRenderCommandEncoder) -> Void
    ) -> Bool {
        // The drawable takes the layer's pixel size (staged by SessionPresenter.layout), so this draw
        // is the only decoded→on-screen scale; before the first layout it takes the frame size.
        // drawableSize never tracks bounds: set it before nextDrawable, and only on a change.
        let targetSize = (targetFromLayout.width > 0 && targetFromLayout.height > 0)
            ? targetFromLayout : decodedSize
        // Under a provided (link-vended) drawable this sizes the NEXT vend — the one in hand
        // keeps its size, and a live-resize transient composites via contentsGravity as ever.
        if layer.drawableSize != targetSize { layer.drawableSize = targetSize }
        #if DEBUG
        logSizeIfChanged(decoded: decodedSize, drawable: targetSize)
        #endif
        #if os(macOS)
        // Windowed (composited) → the DCP swapID-panic mitigation mechanism (see
        // `WindowedPresentMode`). Toggle the layer property BEFORE vending a drawable so the
        // vend matches how it will be presented; drained here on the render thread, flipped
        // exactly once per mode change.
        stagingLock.lock()
        let windowedMode = windowedPresentStaged
        stagingLock.unlock()
        if windowedMode != windowedPresentActive {
            windowedPresentActive = windowedMode
            layer.presentsWithTransaction = windowedMode == .transaction
            if windowedMode != .surface, !surfacePool.isEmpty {
                // Leaving surface mode (fullscreen entry / mechanism A/B): drop the pool — at 5K
                // it holds >100 MB, and re-entering rebuilds it in one frame. SessionPresenter
                // clears the surface layer's contents on main.
                surfacePool.removeAll()
                surfacePoolSize = .zero
                lastHandedOff = nil
            }
            presenterLog.info(
                "stage2: windowed present mode \(windowedMode.rawValue, privacy: .public) (DCP swapID-panic mitigation)")
        }
        if windowedMode == .surface {
            // No image queue at all: render into a pooled IOSurface and swap it into the
            // sibling layer's contents. The drawable/queue tail below never runs.
            return encodeToSurface(
                targetSize: targetSize, pipeline: pipeline, onPresented: onPresented,
                luma: luma, keepAlive: keepAlive, bind: bind)
        }
        #endif
        if let providedDrawable,
           providedDrawable.texture.pixelFormat != layer.pixelFormat {
            return false // config outran the vend (HDR flip) — next vend has the new format
        }
        guard let drawable = providedDrawable ?? layer.nextDrawable(),
              let commandBuffer = queue.makeCommandBuffer()
        else { return false }
        let (lumaTexture, uvRects) = lumaForTarget(
            luma, target: drawable.texture, commandBuffer: commandBuffer)
        var rects = uvRects

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else {
            return false
        }
        encoder.setRenderPipelineState(pipeline)
        encoder.setFragmentTexture(lumaTexture, index: 0)
        encoder.setVertexBytes(&rects, length: MemoryLayout<UvRects>.stride, index: 0)
        bind(encoder)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        if let onPresented {
            #if targetEnvironment(simulator)
            // The simulator SDK exposes neither addPresentedHandler nor presentedTime — report
            // nil so the caller stamps with its display-link estimate (the pre-presentedTime
            // behavior; simulator numbers are indicative only anyway).
            onPresented(nil)
            #else
            // Registered BEFORE present. presentedTime is CACurrentMediaTime-based; 0 means the
            // system never put this drawable on glass (dropped) — report nil, the caller falls
            // back to its display-link estimate.
            drawable.addPresentedHandler { d in
                onPresented(
                    d.presentedTime > 0
                        ? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: d.presentedTime)
                        : nil)
            }
            #endif
        }
        // Keep the bound sources alive until the GPU finishes sampling (see the callers).
        commandBuffer.addCompletedHandler { _ in _ = keepAlive }
        #if os(macOS)
        if windowedPresentActive == .transaction {
            // Windowed DCP mitigation: present the drawable THROUGH a Core Animation transaction
            // (`presentsWithTransaction`, set above) instead of the async image queue, so the swap
            // commits with the layer tree and stays in lockstep with the compositor (no out-of-band
            // flip to race WindowServer's swaps). Wait until the GPU work is scheduled (contents
            // will be ready — p50 ~0.1 ms), then present inside an EXPLICIT CATransaction ON THIS
            // RENDER THREAD and `flush()`. `presentAtMediaTime` does not apply — the transaction
            // paces.
            //
            // Threading history, because BOTH failure modes shipped or nearly shipped:
            // • A bare `present()` from this thread (no transaction) never flushes — nothing
            //   commits a runloop-less thread's implicit transaction, so drawables are never
            //   released; after maximumDrawableCount vends `nextDrawable()` blocks forever and
            //   the stream FREEZES (the fullscreen→windowed switch did exactly this).
            // • The explicit begin/commit alone is NOT enough either: this thread has an ACTIVE
            //   implicit transaction (the layer mutations above — drawableSize/colour — created
            //   it), so the explicit transaction NESTS inside it and its commit defers to the
            //   implicit one that never comes. The harness reproduced the exact freeze: every
            //   present reported presentedTime=0, nothing reached glass. `CATransaction.flush()`
            //   pushes the implicit transaction (present included) to the render server NOW.
            // • The original fix hopped to MAIN and presented there — correct, but slow in the
            //   field (presents=55 @ fps=240, display_p50 18.6 ms on the 240 Hz Studio): each
            //   present lands a runloop turn late, and main's own implicit transaction batches
            //   enrolled presents at runloop-iteration rate. Kept as PUNKTFUNK_TXN_PRESENT=main.
            //   The off-main commit measured immune to main-thread churn in the harness
            //   (2026-07-21: glass p50 ~10 ms at 240 Hz full-size, cadence a clean 4.17 ms).
            commandBuffer.commit()
            let schedStart = CACurrentMediaTime()
            commandBuffer.waitUntilScheduled()
            let schedMs = (CACurrentMediaTime() - schedStart) * 1000
            let commitStart = CACurrentMediaTime()
            if txnPresentOnMain {
                let presentedDrawable = drawable
                DispatchQueue.main.async {
                    CATransaction.begin()
                    CATransaction.setDisableActions(true)
                    presentedDrawable.present()
                    CATransaction.commit()
                }
            } else {
                CATransaction.begin()
                CATransaction.setDisableActions(true)
                drawable.present()
                CATransaction.commit()
                CATransaction.flush()
            }
            windowedDiag.record(
                schedMs: schedMs, commitMs: (CACurrentMediaTime() - commitStart) * 1000,
                mode: .transaction)
            return true
        }
        #endif
        // Scheduled on the vsync when the pipeline gave us the link's target (see the doc comment);
        // immediate otherwise. A target already in the past presents immediately — same thing.
        if let presentAtMediaTime {
            commandBuffer.present(drawable, atTime: presentAtMediaTime)
        } else {
            commandBuffer.present(drawable)
        }
        commandBuffer.commit()
        return true
    }

    #if os(macOS)
    /// The WINDOWED `surface` present tail (see `WindowedPresentMode.surface`): render with the
    /// same per-frame pipeline into a pooled IOSurface and hand it to `surfaceLayer.contents`
    /// from the command buffer's COMPLETION handler, inside an explicit CATransaction + flush
    /// (the same off-main commit discipline as the transactional present — an ordinary
    /// damaged-layer update on WindowServer's own composite cadence, no image queue anywhere).
    /// RENDER THREAD. `onPresented` is stamped right after the contents swap commits — the
    /// closest observable analogue of "reached glass" here (the composite follows within a
    /// refresh, so the display-stage meters read slightly OPTIMISTIC in this mode).
    ///
    /// The pool tracks the layer's depth: bgra8 or `sdr10Drawable` for SDR, rgba16Float tagged
    /// BT.2100 PQ for HDR — `configure` already ran, so the caller's `pipeline` attachment
    /// format always matches.
    /// HDR OPEN RISK (why this whole mode is a prototype): whether the compositor honors the
    /// PQ tag + EDR for plain-CALayer IOSurface contents needs an on-glass eyeball; the metal
    /// layer underneath keeps `wantsExtendedDynamicRangeContent` as the EDR anchor (the harness
    /// measured the display's EDR headroom engaging with this arrangement).
    private func encodeToSurface(
        targetSize: CGSize, pipeline: MTLRenderPipelineState,
        onPresented: ((Int64?) -> Void)?,
        luma: MTLTexture, keepAlive: [Any], bind: (MTLRenderCommandEncoder) -> Void
    ) -> Bool {
        ensureSurfacePool(
            size: targetSize, depth: hdrActive ? .hdr : (tenBitSDRActive ? .sdr10 : .sdr8))
        guard let slotIndex = takeSurfaceSlot(),
              let commandBuffer = queue.makeCommandBuffer()
        else { return false }
        let slot = surfacePool[slotIndex]
        let (lumaTexture, uvRects) = lumaForTarget(
            luma, target: slot.texture, commandBuffer: commandBuffer)
        var rects = uvRects

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = slot.texture
        pass.colorAttachments[0].loadAction = .clear
        pass.colorAttachments[0].clearColor = MTLClearColor(red: 0, green: 0, blue: 0, alpha: 1)
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else {
            return false
        }
        encoder.setRenderPipelineState(pipeline)
        encoder.setFragmentTexture(lumaTexture, index: 0)
        encoder.setVertexBytes(&rects, length: MemoryLayout<UvRects>.stride, index: 0)
        bind(encoder)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        let surface = slot.surface
        let surfaceLayer = surfaceLayer // captured directly — the handler must not retain self
        let diag = windowedDiag
        let epoch = surfaceEpoch
        let epochAtCommit = epoch.current()
        let commitStamp = CACurrentMediaTime()
        commandBuffer.addCompletedHandler { _ in
            _ = keepAlive // sources pinned until the GPU finished sampling
            // The present target moved on while this was in flight (fullscreen entry clears the
            // layer) — writing now would restore a frame nobody is going to replace.
            guard epoch.isCurrent(epochAtCommit) else { return }
            let completedAt = CACurrentMediaTime()
            // Swap on THIS Metal completion thread: explicit transaction + flush, so the commit
            // reaches the render server now, independent of main (completion handlers for one
            // queue fire in execution order, so swaps can't reorder).
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            surfaceLayer.contents = surface
            CATransaction.commit()
            CATransaction.flush()
            diag.record(
                schedMs: (completedAt - commitStamp) * 1000,
                commitMs: (CACurrentMediaTime() - completedAt) * 1000, mode: .surface)
            onPresented?(Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: CACurrentMediaTime()))
        }
        commandBuffer.commit()
        lastHandedOff = slotIndex
        return true
    }

    /// IOSurface and Metal formats that share one word. Metal `bgr10a2Unorm` is IOSurface `l10r`
    /// (`ARGB2101010LEPacked`): alpha in the top two bits, then R, G, B down to bit 0.
    private static func surfaceFormats(_ depth: SurfaceDepth) -> (OSType, MTLPixelFormat) {
        switch depth {
        case .hdr: return (kCVPixelFormatType_64RGBAHalf, .rgba16Float)
        case .sdr8: return (kCVPixelFormatType_32BGRA, .bgra8Unorm)
        case .sdr10:
            return sdr10Drawable == .bgra8Unorm
                ? (kCVPixelFormatType_32BGRA, .bgra8Unorm)
                : (kCVPixelFormatType_ARGB2101010LEPacked, sdr10Drawable)
        }
    }

    /// (Re)build the pool at `size`/`depth` — 4 IOSurface render targets (one on glass, one
    /// committed in CA, one rendering, one spare). RENDER THREAD. A failed allocation leaves the
    /// pool empty; the caller returns false and the ring's putBack + display-link retry take
    /// over.
    private func ensureSurfacePool(size: CGSize, depth: SurfaceDepth) {
        guard size != surfacePoolSize || depth != surfacePoolDepth else { return }
        surfacePool.removeAll()
        lastHandedOff = nil
        let w = Int(size.width)
        let h = Int(size.height)
        guard w > 0, h > 0 else { return }
        let hdr = depth == .hdr
        // rgba16Float (8 B/px) carries the PQ-encoded HDR samples; the SDR formats are 4 B/px.
        // 256-byte row alignment satisfies both IOSurface and Metal linear-texture rules.
        let (surfaceFormat, textureFormat) = Self.surfaceFormats(depth)
        let bytesPerElement = hdr ? 8 : 4
        let bytesPerRow = ((w * bytesPerElement) + 255) & ~255
        let props: [String: Any] = [
            kIOSurfaceWidth as String: w,
            kIOSurfaceHeight as String: h,
            kIOSurfaceBytesPerElement as String: bytesPerElement,
            kIOSurfaceBytesPerRow as String: bytesPerRow,
            kIOSurfacePixelFormat as String: surfaceFormat,
        ]
        let desc = MTLTextureDescriptor.texture2DDescriptor(
            pixelFormat: textureFormat, width: w, height: h, mipmapped: false)
        desc.usage = [.renderTarget]
        desc.storageMode = .shared
        for _ in 0..<4 {
            guard let surface = IOSurfaceCreate(props as CFDictionary),
                  let texture = device.makeTexture(descriptor: desc, iosurface: surface, plane: 0)
            else {
                // Leave the key UNSET so the next frame retries. Recording it up front latched a
                // single failed allocation (memory pressure at 5K) as "this size is built", and
                // every later frame short-circuited on the guard above — the picture froze for the
                // session while audio and input stayed live.
                surfacePool.removeAll()
                return
            }
            // Tag the surface like the metal layer (BT.2100 PQ, or `sdrColorspace`), so the
            // compositor colour-matches the contents instead of drawing them in the panel's space.
            let space = hdr ? CGColorSpace(name: CGColorSpace.itur_2100_PQ) : sdrColorspace
            if let name = space?.name {
                IOSurfaceSetValue(surface, "IOSurfaceColorSpace" as CFString, name)
            }
            surfacePool.append(SurfaceSlot(surface: surface, texture: texture))
        }
        // Only now is this size actually built.
        surfacePoolSize = size
        surfacePoolDepth = depth
        // The EDR request rides the SURFACE layer too (its contents are what composite); the
        // metal layer underneath keeps its own from configureColor as the anchor. Layer flags
        // are committed by the next swap's transaction flush.
        surfaceLayer.wantsExtendedDynamicRangeContent = hdr
    }

    /// Pick the slot to render into: never the one just handed to the layer (the compositor may
    /// still scan it), prefer surfaces the window server isn't holding (`IOSurfaceIsInUse`), and
    /// among those the least recently rendered. Falls back to the LRU busy slot rather than
    /// stalling — a visible glitch at worst, never a queue-up. RENDER THREAD.
    private func takeSurfaceSlot() -> Int? {
        guard !surfacePool.isEmpty else { return nil }
        var free: Int?
        var busy: Int?
        for i in surfacePool.indices where i != lastHandedOff {
            if !IOSurfaceIsInUse(surfacePool[i].surface) {
                if free == nil || surfacePool[i].seq < surfacePool[free!].seq { free = i }
            } else {
                if busy == nil || surfacePool[i].seq < surfacePool[busy!].seq { busy = i }
            }
        }
        guard let pick = free ?? busy else { return nil }
        surfaceSeq += 1
        surfacePool[pick].seq = surfaceSeq
        return pick
    }
    #endif

    /// Luma to bind for a draw into `target`, with the UV rects that sample it. The plane itself
    /// when the visible region is not larger than the target; otherwise that region Lanczos-scaled
    /// to the target's size, sampled over the unit square. r16Float holds a 10-bit code to within
    /// one step and is writable and filterable on every Apple GPU. RENDER THREAD.
    private func lumaForTarget(
        _ luma: MTLTexture, target: MTLTexture, commandBuffer: MTLCommandBuffer
    ) -> (MTLTexture, UvRects) {
        stagingLock.lock()
        let src = sourceRect
        stagingLock.unlock()
        let frame = SIMD4<Float>(
            Float(src.minX), Float(src.minY), Float(src.width), Float(src.height))
        let whole = UvRects(frame: frame, luma: frame)
        let cropW = src.width * CGFloat(luma.width), cropH = src.height * CGFloat(luma.height)
        guard CGFloat(target.width) < cropW - 0.5 || CGFloat(target.height) < cropH - 0.5,
              let lanczos = lanczos
        else { return (luma, whole) }
        if scaledLuma?.width != target.width || scaledLuma?.height != target.height {
            let desc = MTLTextureDescriptor.texture2DDescriptor(
                pixelFormat: .r16Float, width: target.width, height: target.height,
                mipmapped: false)
            desc.usage = [.shaderRead, .shaderWrite]
            desc.storageMode = .private
            scaledLuma = device.makeTexture(descriptor: desc)
        }
        guard let scaledLuma else { return (luma, whole) }
        // Scale + translate in destination pixels (MPSImageResampling.h): the visible region
        // fills the target exactly.
        let sx = Double(target.width) / Double(cropW), sy = Double(target.height) / Double(cropH)
        var transform = MPSScaleTransform(
            scaleX: sx, scaleY: sy,
            translateX: -Double(src.minX * CGFloat(luma.width)) * sx,
            translateY: -Double(src.minY * CGFloat(luma.height)) * sy)
        withUnsafePointer(to: &transform) { lanczos.scaleTransform = $0 }
        lanczos.encode(
            commandBuffer: commandBuffer, sourceTexture: luma, destinationTexture: scaledLuma)
        return (scaledLuma, UvRects(frame: frame, luma: SIMD4<Float>(0, 0, 1, 1)))
    }

    /// Returns the CVMetalTexture (not just its MTLTexture) so the caller can keep it alive past the
    /// draw — the MTLTexture is only valid while its CVMetalTexture is retained.
    private func makeTexture(
        _ pixelBuffer: CVPixelBuffer, plane: Int, format: MTLPixelFormat, cache: CVMetalTextureCache
    ) -> CVMetalTexture? {
        let w = CVPixelBufferGetWidthOfPlane(pixelBuffer, plane)
        let h = CVPixelBufferGetHeightOfPlane(pixelBuffer, plane)
        var cvTexture: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault, cache, pixelBuffer, nil, format, w, h, plane, &cvTexture)
        guard status == kCVReturnSuccess, let cvTexture,
              CVMetalTextureGetTexture(cvTexture) != nil
        else { return nil }
        return cvTexture
    }

    #if DEBUG
    private func logSizeIfChanged(decoded: CGSize, drawable: CGSize) {
        let sig = "\(Int(decoded.width))x\(Int(decoded.height))→\(Int(drawable.width))x\(Int(drawable.height))|hdr\(hdrActive ? 1 : 0)|sdr10\(tenBitSDRActive ? 1 : 0)"
        if sig != lastSizeSig {
            lastSizeSig = sig
            // Explicit verdict: is the shader presenting 1:1 (decoded == drawable) or resampling? The
            // scale ratio makes a residual match-window mismatch obvious. If this says 1:1 but the
            // picture is still soft, the resample is downstream of us (macOS compositor — a scaled
            // display mode, or a fractional-pixel window position), not the shader.
            let sx = decoded.width > 0 ? drawable.width / decoded.width : 0
            let sy = decoded.height > 0 ? drawable.height / decoded.height : 0
            let verdict = decoded == drawable
                ? "1:1 (no resample)"
                : String(
                    format: "RESAMPLE scale=%.4fx%.4f %@", sx, sy,
                    (sx < 1 || sy < 1) && lanczos != nil ? "lanczos" : "catmull-rom")
            let msg =
                "stage2: decoded \(Int(decoded.width))x\(Int(decoded.height)) → drawable \(Int(drawable.width))x\(Int(drawable.height)) [\(verdict)] hdr=\(hdrActive) sdr10=\(tenBitSDRActive)"
            presenterLog.info("\(msg, privacy: .public)")
        }
    }
    #endif
}

/// `UvRects` in the shader, field for field: two `float4`, 32 bytes.
private struct UvRects {
    var frame: SIMD4<Float>
    var luma: SIMD4<Float>
}

#endif
