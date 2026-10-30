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
import Metal
import MetalPerformanceShaders
import QuartzCore
import os

private let presenterLog = ClientLog(category: "presenter")

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
let sdr10Drawable: MTLPixelFormat =
    ProcessInfo.processInfo.environment["PUNKTFUNK_SDR10_DRAWABLE"] == "8"
    ? .bgra8Unorm : .bgr10a2Unorm

/// How an HDR frame reaches the layer.
///
/// - `linear` (iOS, visionOS): the shader decodes PQ, rolls highlights off onto the display's EDR
///   headroom and writes linear Display P3 (1.0 = 203 nits) into a half-float extended-linear
///   layer with no EDR metadata. The system has nothing to tone-map, and the iPad scans it out
///   Direct. visionOS writes the same into its screen's half-float texture.
/// - `pq` (macOS, tvOS): PQ code values under `itur_2100_PQ`, with the HDR10 grade as EDR
///   metadata; the system tone-maps. Composites on the iPad Pro.
///
/// `PUNKTFUNK_HDR_MODE=pq|linear` overrides the platform's choice, for A/B runs, except on
/// visionOS: its screen texture takes linear values only.
enum HdrMode {
    case pq, linear
    static let current: HdrMode = {
        #if os(visionOS)
        return .linear
        #else
        switch ProcessInfo.processInfo.environment["PUNKTFUNK_HDR_MODE"] {
        case "pq": return .pq
        case "linear": return .linear
        default:
            #if os(iOS)
            return .linear
            #else
            return .pq
            #endif
        }
        #endif
    }()
}

/// The shaders' `HdrTarget`: content peak and display peak, in nits.
struct HdrTargetUniform {
    var srcNits: Float
    var dstNits: Float
}

/// The HDR drawable (see `hdrDrawableFormat`).
let hdrDrawable = hdrDrawableFormat(
    linear: HdrMode.current == .linear,
    env: ProcessInfo.processInfo.environment["PUNKTFUNK_HDR_DRAWABLE"],
    halfFloatPQ: preA15AppleTV)

/// Linear EDR values exceed 1, so `linear` takes half-float, as `CurveTarget`'s HDR queue does.
/// PQ fits 10 bits, the pairing Apple documents for HDR10, except where `halfFloatPQ`: Apple TVs
/// older than the A15 show the 10-bit PQ layer at half rate. `PUNKTFUNK_HDR_DRAWABLE=16|10` picks
/// the PQ drawable for an A/B.
func hdrDrawableFormat(linear: Bool, env: String?, halfFloatPQ: Bool) -> MTLPixelFormat {
    if linear { return .rgba16Float }
    switch env {
    case "16": return .rgba16Float
    case "10": return .bgr10a2Unorm
    default: return halfFloatPQ ? .rgba16Float : .bgr10a2Unorm
    }
}

/// An Apple TV with an A10X or A12: its GPU stops short of the Apple 8 family (A15).
private let preA15AppleTV: Bool = {
    #if os(tvOS)
    return MTLCreateSystemDefaultDevice().map { !$0.supportsFamily(.apple8) } ?? false
    #else
    return false
    #endif
}()

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
// own, the unit square once `lumaForTarget` has already cropped and scaled it. `upscale` is 1 while
// the bound luma is smaller than the target (or scaled by the shader), 0 when it is 1:1.
struct VOut { float4 pos [[position]]; float2 uv; float2 luv; float upscale [[flat]]; };

// Origin + size of each, in texture UV, and the mode (see `UvRects` in Swift).
struct UvRects { float4 frame; float4 luma; float4 mode; };

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
    o.upscale = rects.mode.x;
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

// Luma at `luv`: Catmull-Rom while upscaling. At 1:1 one bilinear tap lands on the texel centre
// and returns it exactly, so the nine taps would buy nothing.
// PUNKTFUNK_BILINEAR_LUMA=1 forces the single tap everywhere: if that visibly changes a 1:1
// picture, the present is not 1:1.
float sampleLuma(texture2d<float> tex, sampler s, float2 luv, float upscale) {
#ifdef PF_BILINEAR_LUMA
    return tex.sample(s, luv).r;
#else
    return upscale > 0.5 ? catmullRomLuma(tex, s, luv) : tex.sample(s, luv).r;
#endif
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
                 float upscale, constant CscUniform& csc) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float lumaY = sampleLuma(lumaTex, s, luv, upscale);
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
    return float4(sampleRgb(lumaTex, chromaTex, in.luv, in.uv, in.upscale, csc), 1.0);
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
    float lumaY = sampleLuma(lumaTex, s, in.luv, in.upscale);
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
    return float4(sampleRgb(lumaTex, chromaTex, in.luv, in.uv, in.upscale, csc), 1.0);
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

// BT.2390 EETF on luminance `l` (linear, 1.0 = 10 000 nits): content up to `srcNits` rolls off
// onto `dstNits`, applied in PQ. Returns the factor that scales linear RGB.
static inline float eetfScale(float l, float srcNits, float dstNits) {
    if (l <= 0.0 || dstNits >= srcNits) return 1.0;
    float src = pqOetf(srcNits / 10000.0);
    float maxLum = pqOetf(dstNits / 10000.0) / src;
    float ks = 1.5 * maxLum - 0.5;
    float e = min(pqOetf(l) / src, 1.0);
    if (e <= ks) return 1.0;
    float t = (e - ks) / (1.0 - ks);
    float t2 = t * t;
    float t3 = t2 * t;
    e = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks)
        + (-2.0 * t3 + 3.0 * t2) * maxLum;
    return pqEotf(e * src) / l;
}

static inline float3 pqToSdr(float3 pq) {
    float3 lin = float3(pqEotf(pq.r), pqEotf(pq.g), pqEotf(pq.b));
    lin = max(float3(
        dot(lin, float3( 1.6605, -0.5876, -0.0728)),
        dot(lin, float3(-0.1246,  1.1329, -0.0083)),
        dot(lin, float3(-0.0182, -0.1006,  1.1187))), 0.0);
    const float white = 203.0 / 10000.0;
    lin *= eetfScale(max(lin.r, max(lin.g, lin.b)), 1000.0, 203.0);
    float3 c = saturate(lin / white);
    return select(1.055 * pow(c, 1.0 / 2.4) - 0.055, 12.92 * c, c <= 0.0031308);
}

// HdrMode.linear: the content's peak and the display's (EDR headroom × 203 nits).
struct HdrTarget { float srcNits; float dstNits; };

// PQ BT.2020 → linear Display P3 with 1.0 = 203 nits, highlights rolled onto the display's peak.
static inline float3 pqToLinearP3(float3 pq, constant HdrTarget& t) {
    float3 lin = float3(pqEotf(pq.r), pqEotf(pq.g), pqEotf(pq.b));
    lin = float3(dot(lin, float3( 1.3435783, -0.2821797, -0.0613986)),
                 dot(lin, float3(-0.0652975,  1.0757879, -0.0104905)),
                 dot(lin, float3( 0.0028218, -0.0195985,  1.0167767)));
    float l = max(max(lin.r, max(lin.g, lin.b)), 0.0);
    return lin * eetfScale(l, t.srcNits, t.dstNits) * (10000.0 / 203.0);
}

fragment float4 pf_frag_hdr_linear(VOut in [[stage_in]],
                                   texture2d<float> lumaTex [[texture(0)]],
                                   texture2d<float> chromaTex [[texture(1)]],
                                   constant CscUniform& csc [[buffer(0)]],
                                   constant HdrTarget& target [[buffer(1)]]) {
    float3 pq = sampleRgb(lumaTex, chromaTex, in.luv, in.uv, in.upscale, csc);
    return float4(pqToLinearP3(pq, target), 1.0);
}

fragment float4 pf_frag_hdr_tv(VOut in [[stage_in]],
                               texture2d<float> lumaTex [[texture(0)]],
                               texture2d<float> chromaTex [[texture(1)]],
                               constant CscUniform& csc [[buffer(0)]]) {
    // Y′CbCr → full-range PQ R′G′B′ via the per-frame rows (as pf_frag_hdr), then the tail.
    return float4(pqToSdr(sampleRgb(lumaTex, chromaTex, in.luv, in.uv, in.upscale, csc)), 1.0);
}

// PyroWave planar HDR tone-map: three separate R16 planes (P010-style studio codes; the rows
// fold in depth-10 MSB packing) → PQ R′G′B′ → the shared SDR tail. Used when a PQ pyrowave
// stream must land on an 8-bit surface: tvOS without HDR headroom. The passthrough planar
// HDR pipeline reuses pf_frag_planar itself on the HDR drawable (identical math — the
// layer's itur_2100_PQ colour space + EDR metadata do the interpretation).
fragment float4 pf_frag_planar_tm(VOut in [[stage_in]],
                                  texture2d<float> lumaTex [[texture(0)]],
                                  texture2d<float> cbTex [[texture(1)]],
                                  texture2d<float> crTex [[texture(2)]],
                                  constant CscUniform& csc [[buffer(0)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float lumaY = sampleLuma(lumaTex, s, in.luv, in.upscale);
    float2 cuv = chromaUV(lumaTex, cbTex, in.uv);
    float3 yuv = float3(lumaY, cbTex.sample(s, cuv).r, crTex.sample(s, cuv).r);
    float3 pq = saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                                dot(csc.r1.xyz, yuv) + csc.r1.w,
                                dot(csc.r2.xyz, yuv) + csc.r2.w));
    return float4(pqToSdr(pq), 1.0);
}

// PyroWave planar HDR under HdrMode.linear: the planar sample, then pqToLinearP3.
fragment float4 pf_frag_planar_hdr_linear(VOut in [[stage_in]],
                                          texture2d<float> lumaTex [[texture(0)]],
                                          texture2d<float> cbTex [[texture(1)]],
                                          texture2d<float> crTex [[texture(2)]],
                                          constant CscUniform& csc [[buffer(0)]],
                                          constant HdrTarget& target [[buffer(1)]]) {
    constexpr sampler s(filter::linear, address::clamp_to_edge);
    float lumaY = sampleLuma(lumaTex, s, in.luv, in.upscale);
    float2 cuv = chromaUV(lumaTex, cbTex, in.uv);
    float3 yuv = float3(lumaY, cbTex.sample(s, cuv).r, crTex.sample(s, cuv).r);
    float3 pq = saturate(float3(dot(csc.r0.xyz, yuv) + csc.r0.w,
                                dot(csc.r1.xyz, yuv) + csc.r1.w,
                                dot(csc.r2.xyz, yuv) + csc.r2.w));
    return float4(pqToLinearP3(pq, target), 1.0);
}

// Late latch: the pass is encoded at the vend and the pick names the frame through `Latch`,
// an argument buffer written after the encode — the planes as resource IDs, kept resident on
// the queue. A slot below zero discards: that drawable is never presented.
struct Latch {
    UvRects rects; CscUniform csc; HdrTarget target; int slot; int pad0; int pad1; int pad2;
    texture2d<float> luma; texture2d<float> chroma;
};

fragment float4 pf_frag_latched(VOut in [[stage_in]], constant Latch& l [[buffer(0)]]) {
    if (l.slot < 0) discard_fragment();
    return float4(sampleRgb(l.luma, l.chroma, in.luv, in.uv, in.upscale, l.csc), 1.0);
}

fragment float4 pf_frag_hdr_linear_latched(VOut in [[stage_in]], constant Latch& l [[buffer(0)]]) {
    if (l.slot < 0) discard_fragment();
    float3 pq = sampleRgb(l.luma, l.chroma, in.luv, in.uv, in.upscale, l.csc);
    return float4(pqToLinearP3(pq, l.target), 1.0);
}
"""

public final class MetalVideoPresenter {
    /// The layer the hosting view installs (as a sublayer) and sizes to its bounds.
    public let layer: CAMetalLayer
    /// Each frame's GPU start and end, `CACurrentMediaTime` seconds, on a Metal completion
    /// thread. Read as each frame encodes, so deadline pacing sets it per present on the render
    /// thread.
    var onGPUTime: (@Sendable (_ start: CFTimeInterval, _ end: CFTimeInterval) -> Void)?


    private let device: MTLDevice
    private let queue: MTLCommandQueue
    /// SDR (BT.709 8-bit → bgra8) and HDR (BT.2020 PQ 10-bit → `hdrDrawable`) pipelines. Selected per
    /// frame in `render`; the layer is reconfigured to match when the session flips (HDR toggle).
    private let pipelineSDR: MTLRenderPipelineState
    /// `pf_frag` into `sdr10Drawable`: the SDR shader, wider attachment.
    private let pipelineSDR10: MTLRenderPipelineState
    private let pipelineHDR: MTLRenderPipelineState
    /// The in-shader PQ→SDR tone-map (pf_frag_hdr_tv → bgra8): tvOS without display headroom
    /// (see `setDisplayHeadroom`). nil elsewhere.
    private let pipelineHDRToneMap: MTLRenderPipelineState?
    /// PyroWave's 3-plane SDR path (pf_frag_planar → bgra8) — see `renderPlanar`.
    private let pipelinePlanar: MTLRenderPipelineState
    /// PyroWave planar HDR passthrough (pf_frag_planar → `hdrDrawable`; the layer's PQ colour
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
    #if os(visionOS)
    /// The volume's screen: frames draw into its queue in place of the layer. Set before the
    /// render thread starts.
    var curve: CurveTarget?
    #endif
    /// The screen takes the frames: the layer keeps its SDR config, and a 10-bit SDR session
    /// draws 8-bit into the screen's sRGB queue.
    private var onCurve: Bool {
        #if os(visionOS)
        return curve != nil
        #else
        return false
        #endif
    }

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
    /// tvOS: the box switched into the HDR output mode our display criteria asked for.
    private var stagedHDROutput = false
    /// The most headroom the display could give (`potentialEDRHeadroom`): read out only.
    private var stagedPotentialHeadroom: CGFloat = 0

    /// Deadline pacing's layer handoff. The link thread vends every drawable there and reads the
    /// layer's format, colour space and EDR metadata as it allocates one, so a write from the
    /// render thread can free the metadata mid-vend. After `deferLayerWrites`, `writeLayer` queues
    /// and the link thread applies the queue between vends (`applyLayerWrites`).
    private let layerWriteLock = NSLock()
    private var layerWritesDeferred = false
    private var layerWrites: [(CAMetalLayer) -> Void] = []
    /// The pixel format and drawable size last written or queued. Render-thread confined: the
    /// config the next vend carries, which a queued write has not reached the layer with yet.
    private var layerFormat: MTLPixelFormat = .bgra8Unorm
    private var layerSize: CGSize?

    #if DEBUG
    /// Last logged "decoded→drawable" signature, so the diagnostic logs only on a size/HDR change.
    private var lastSizeSig = ""
    #endif

    private struct Pipelines {
        let device: MTLDevice
        let sdr, sdr10, hdr: MTLRenderPipelineState
        let hdrToneMap: MTLRenderPipelineState?
        let planar, planarHDR, planarToneMap: MTLRenderPipelineState
        /// The late-latch twins of `sdr`, `sdr10` and `hdr`; the tone-map variants have none.
        let latchedSDR, latchedSDR10, latchedHDR: MTLRenderPipelineState
    }

    /// Compiled once per process: the source and its env lever never change, and every session
    /// start, wedge rebuild and monitor move would otherwise recompile on the main thread.
    private static let pipelines: Pipelines? = {
        guard let device = MTLCreateSystemDefaultDevice() else { return nil }
        let pipelineSDR: MTLRenderPipelineState
        let pipelineSDR10: MTLRenderPipelineState
        let pipelineHDR: MTLRenderPipelineState
        let pipelineHDRToneMap: MTLRenderPipelineState?
        let pipelinePlanar: MTLRenderPipelineState
        let pipelinePlanarHDR: MTLRenderPipelineState
        let pipelinePlanarToneMap: MTLRenderPipelineState
        let latchedSDR: MTLRenderPipelineState
        let latchedSDR10: MTLRenderPipelineState
        let latchedHDR: MTLRenderPipelineState
        do {
            // DEBUG A/B lever: PUNKTFUNK_BILINEAR_LUMA=1 compiles the shader with Catmull-Rom OFF
            // (plain bilinear luma) by prepending a #define ahead of the source. Default (unset) is
            // the normal bicubic path. Read once per process — set it in the environment and
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
            let linear = HdrMode.current == .linear
            hdr.fragmentFunction = library.makeFunction(
                name: linear ? "pf_frag_hdr_linear" : "pf_frag_hdr")
            hdr.colorAttachments[0].pixelFormat = hdrDrawable
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
            planarHdr.fragmentFunction = library.makeFunction(
                name: linear ? "pf_frag_planar_hdr_linear" : "pf_frag_planar")
            planarHdr.colorAttachments[0].pixelFormat = hdrDrawable
            pipelinePlanarHDR = try device.makeRenderPipelineState(descriptor: planarHdr)
            let planarTm = MTLRenderPipelineDescriptor()
            planarTm.vertexFunction = vtx
            planarTm.fragmentFunction = library.makeFunction(name: "pf_frag_planar_tm")
            planarTm.colorAttachments[0].pixelFormat = .bgra8Unorm
            pipelinePlanarToneMap = try device.makeRenderPipelineState(descriptor: planarTm)
            // The latched twins: the pool-indexed fragment on the same three attachments.
            func latched(_ name: String, _ format: MTLPixelFormat) throws -> MTLRenderPipelineState {
                let d = MTLRenderPipelineDescriptor()
                d.vertexFunction = vtx
                d.fragmentFunction = library.makeFunction(name: name)
                d.colorAttachments[0].pixelFormat = format
                return try device.makeRenderPipelineState(descriptor: d)
            }
            latchedSDR = try latched("pf_frag_latched", .bgra8Unorm)
            latchedSDR10 = try latched("pf_frag_latched", sdr10Drawable)
            latchedHDR = try latched(
                linear ? "pf_frag_hdr_linear_latched" : "pf_frag_latched", hdrDrawable)
        } catch {
            return nil
        }
        return Pipelines(
            device: device, sdr: pipelineSDR, sdr10: pipelineSDR10, hdr: pipelineHDR,
            hdrToneMap: pipelineHDRToneMap, planar: pipelinePlanar,
            planarHDR: pipelinePlanarHDR, planarToneMap: pipelinePlanarToneMap,
            latchedSDR: latchedSDR, latchedSDR10: latchedSDR10, latchedHDR: latchedHDR)
    }()

    /// nil if Metal is unavailable (no GPU / a headless CI) or a shader fails to compile — the caller
    /// falls back to stage-1.
    public static func make() -> MetalVideoPresenter? {
        guard let p = pipelines, let queue = p.device.makeCommandQueue() else { return nil }
        let device = p.device
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
            device: device, queue: queue, pipelineSDR: p.sdr, pipelineSDR10: p.sdr10,
            pipelineHDR: p.hdr,
            pipelineHDRToneMap: p.hdrToneMap, pipelinePlanar: p.planar,
            pipelinePlanarHDR: p.planarHDR, pipelinePlanarToneMap: p.planarToneMap,
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
    /// `hdrDrawable` + BT.2020 PQ colour space + EDR with a 203-nit reference-white anchor;
    /// SDR uses the sRGB-tagged 8-bit drawable, or `sdr10Drawable` when `tenBitSDR` (a 10-bit
    /// session — HDR outranks it, an HDR frame never lands on the SDR-10 drawable).
    public func configure(hdr: Bool, tenBitSDR: Bool = false) {
        let wide = tenBitSDR && !hdr && !onCurve
        #if os(tvOS)
        // Reconfigure on an HDR flip AND on a passthrough↔tone-map flip. Passthrough needs the
        // box in an HDR output mode: the switch our criteria asked for landed (`setHDROutput`),
        // or the screen reports headroom — which tvOS never does, even in HDR.
        stagingLock.lock()
        let headroom = stagedDisplayHeadroom
        let hdrOutput = stagedHDROutput
        stagingLock.unlock()
        let passthrough = headroom > 1.0 || hdrOutput
        guard !didConfigureColor || hdr != hdrActive || wide != tenBitSDRActive
            || (hdr && passthrough != hdrPassthroughActive)
        else { return }
        hdrActive = hdr
        tenBitSDRActive = wide
        hdrPassthroughActive = passthrough
        if hdr {
            presenterLog.info(
                "tvOS HDR \(passthrough ? "passthrough" : "tone-map") headroom=\(headroom, format: .fixed(precision: 2)) hdrOutput=\(hdrOutput) drawable=\(hdrDrawable == .rgba16Float ? "rgba16Float" : "bgr10a2Unorm")")
        }
        #else
        guard !didConfigureColor || hdr != hdrActive || wide != tenBitSDRActive else { return }
        hdrActive = hdr
        tenBitSDRActive = wide
        #endif
        didConfigureColor = true
        configureColor(hdr: hdr && !onCurve, tenBitSDR: wide)
    }

    /// Park the display's current EDR headroom (a MAIN-thread `UIScreen` read, pushed by
    /// SessionPresenter). tvOS: > 1 flips HDR frames to PQ passthrough, ≤ 1 keeps the in-shader
    /// tone-map. iOS: the linear HDR shaders roll highlights onto it. Applied on the next frame.
    /// `potential` is only read out.
    public func setDisplayHeadroom(_ headroom: CGFloat, potential: CGFloat) {
        stagingLock.lock()
        stagedDisplayHeadroom = headroom
        stagedPotentialHeadroom = potential
        stagingLock.unlock()
    }

    /// tvOS: whether the HDMI output is in the HDR mode our display criteria asked for — the
    /// AVDisplayManager switch landed, pushed from the main thread. Applied on the next frame.
    public func setHDROutput(_ on: Bool) {
        stagingLock.lock()
        stagedHDROutput = on
        stagingLock.unlock()
    }

    /// The headroom last parked, current and potential, and the tvOS HDR output flag, for the
    /// deadline window line.
    var stagedHeadroom: (current: CGFloat, potential: CGFloat, hdrOutput: Bool) {
        stagingLock.lock()
        defer { stagingLock.unlock() }
        return (stagedDisplayHeadroom, stagedPotentialHeadroom, stagedHDROutput)
    }

    /// Set the layer's pixel format + colour config for SDR or HDR through `writeLayer`, on
    /// `configure`'s thread. EDR is requested on macOS + iOS. tvOS has NO EDR API
    /// (`wantsExtendedDynamicRangeContent`/`edrMetadata`/`CAEDRMetadata` are all unavailable there) —
    /// and a bare PQ colour-space tag composites UNtone-mapped (the "overblown HDR" Apple TV report),
    /// so tvOS instead tone-maps PQ→SDR in the shader (pf_frag_hdr_tv) and keeps the SDR layer config.
    private func configureColor(hdr: Bool, tenBitSDR: Bool) {
        if hdr {
            #if os(tvOS)
            // With HDR headroom (the session's AVDisplayManager request landed) PQ passes through
            // and the TV tone-maps. Without it PQ would composite blown out, so pf_frag_hdr_tv
            // tone-maps to BT.709 and the layer carries the SDR tag.
            let passthrough = hdrPassthroughActive
            let format: MTLPixelFormat = passthrough ? hdrDrawable : .bgra8Unorm
            let space = passthrough ? CGColorSpace(name: CGColorSpace.itur_2100_PQ) : sdrColorspace
            layerFormat = format
            writeLayer {
                $0.pixelFormat = format
                $0.colorspace = space
            }
            #else
            // PQ: anchor reference white with the real grade if one already arrived (0xCE before
            // the flip), else the bare 203-nit anchor; without it the PQ signal is too bright.
            // Linear carries no metadata: the shader already tone-mapped (see HdrMode).
            let edr = HdrMode.current == .pq ? makeEDR(lastHdrMeta) : nil
            let space = HdrMode.current == .linear
                ? CGColorSpace(name: CGColorSpace.extendedLinearDisplayP3)
                : CGColorSpace(name: CGColorSpace.itur_2100_PQ)
            layerFormat = hdrDrawable
            writeLayer {
                $0.pixelFormat = hdrDrawable
                $0.colorspace = space
                $0.wantsExtendedDynamicRangeContent = true
                $0.edrMetadata = edr
            }
            #endif
        } else {
            // SDR: gamma-encoded BT.709 [0,1], tagged so CoreAnimation colour-matches it into
            // the output rather than drawing it in the panel's native space (see sdrColorspace;
            // PUNKTFUNK_SDR_COLORSPACE=none restores untagged). A 10-bit session takes the
            // 10-bit drawable so the decode's depth reaches the panel.
            let format = tenBitSDR ? sdr10Drawable : .bgra8Unorm
            layerFormat = format
            writeLayer {
                $0.pixelFormat = format
                $0.colorspace = sdrColorspace
                #if !os(tvOS)
                $0.wantsExtendedDynamicRangeContent = false
                $0.edrMetadata = nil
                #endif
            }
        }
    }

    /// Apply `write` to the layer now, or queue it for the link thread once `deferLayerWrites`
    /// ran. Queued writes apply in order.
    private func writeLayer(_ write: @escaping (CAMetalLayer) -> Void) {
        layerWriteLock.lock()
        defer { layerWriteLock.unlock() }
        if layerWritesDeferred { layerWrites.append(write) } else { write(layer) }
    }

    /// Deadline pacing, RENDER THREAD, before the link starts: every later layer write queues
    /// for `applyLayerWrites`. The layer already carries the first frame's config.
    func deferLayerWrites() {
        layerWriteLock.lock()
        layerWritesDeferred = true
        layerWriteLock.unlock()
    }

    /// Deadline pacing, LINK THREAD, once per update: apply the queued writes. The update's
    /// drawable is already vended, so they reach the next vend and never race one.
    func applyLayerWrites() {
        layerWriteLock.lock()
        let writes = layerWrites
        layerWrites.removeAll()
        layerWriteLock.unlock()
        for write in writes { write(layer) }
    }

    /// Size the next drawable. Compared against the last requested size, not the layer's: a
    /// queued write has not reached the layer yet.
    private func sizeLayer(_ size: CGSize) {
        guard layerSize != size else { return }
        layerSize = size
        writeLayer { $0.drawableSize = size }
    }

    /// The `HdrTarget` the linear HDR shaders roll highlights with: the grade's peak (MaxCLL,
    /// else mastering max, else 1000 nits) onto the display's (headroom × reference white). No
    /// staged headroom means no roll-off. RENDER THREAD (`lastHdrMeta` is its own).
    private func hdrTarget() -> HdrTargetUniform {
        stagingLock.lock()
        let headroom = stagedDisplayHeadroom
        stagingLock.unlock()
        var src: Float = 1000
        if let meta = lastHdrMeta {
            if meta.maxCLL > 0 {
                src = Float(meta.maxCLL)
            } else if meta.maxMasteringLuminance > 0 {
                src = Float(meta.maxMasteringLuminance) / 10_000
            }
        }
        let dst = headroom > 1 ? Float(headroom) * hdrReferenceWhiteNits : .greatestFiniteMagnitude
        return HdrTargetUniform(srcNits: max(src, 400), dstNits: dst)
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

    /// Drain the staged HDR grade and apply it through `writeLayer`. RENDER THREAD (or
    /// `reconcileLayer`'s caller): idempotent, so every present path can call it and the first
    /// one to run wins. Every path must — a stream whose path skipped it tone-maps against the
    /// bare reference-white anchor with no mastering volume for the whole session. The host
    /// repeats the grade on every keyframe (every PyroWave frame); an unchanged one leaves the
    /// layer alone.
    private func applyStagedHdrMeta() {
        stagingLock.lock()
        let newHdrMeta = pendingHdrMeta
        pendingHdrMeta = nil
        stagingLock.unlock()
        guard let newHdrMeta, newHdrMeta != lastHdrMeta else { return }
        lastHdrMeta = newHdrMeta
        // tvOS has no edrMetadata — the cached grade still matters for a later flip's
        // configureColor. macOS/iOS refine the live tone-map now.
        #if !os(tvOS)
        if hdrActive, HdrMode.current == .pq {
            let edr = makeEDR(newHdrMeta)
            writeLayer { $0.edrMetadata = edr }
        }
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

    // MARK: Late latch

    /// `PUNKTFUNK_LATE_LATCH=0` encodes at the pick as before, for the A/B.
    static let lateLatchEnabled =
        ProcessInfo.processInfo.environment["PUNKTFUNK_LATE_LATCH"] != "0"
    /// Parked passes queue apart from `queue`: one waiting on its event would hold the
    /// wavelet decoder's work behind it.
    private lazy var latchQueue: MTLCommandQueue? = device.makeCommandQueue()
    private lazy var latchEvent: MTLSharedEvent? = device.makeSharedEvent()
    private var latchValue: UInt64 = 0
    /// Three uniforms in rotation: a pass has long run before its buffer comes round.
    private lazy var latchUniforms: [MTLBuffer] = (0..<3).compactMap { _ in
        device.makeBuffer(length: MemoryLayout<LatchUniform>.stride, options: .storageModeShared)
    }
    /// The planes a latch names reach the GPU by resource ID, so they must be resident on the
    /// latch queue: this set holds every surface `latchTextures` wrapped. nil before iOS 18, or
    /// without Metal 3 — no latching there. Typed `AnyObject` because a stored property
    /// cannot be conditionally available; the readers cast under the same check.
    private lazy var latchResidency: AnyObject? = {
        guard #available(iOS 18, tvOS 18, macOS 15, visionOS 2, *),
              device.supportsFamily(.metal3), let queue = latchQueue
        else { return nil }
        let desc = MTLResidencySetDescriptor()
        desc.initialCapacity = 64
        guard let set = try? device.makeResidencySet(descriptor: desc) else { return nil }
        queue.addResidencySet(set)
        return set
    }()
    /// Decoded surfaces as textures, made on the surface (not through the cache, so nothing
    /// pins a pool buffer), keyed by surface ID and checked against the surface itself: the
    /// pool allocates freely, so an ID can come back on a new surface. Oldest out past 48.
    private var latchCache: [IOSurfaceID: (surface: IOSurfaceRef, luma: MTLTexture, chroma: MTLTexture)] = [:]
    private var latchOrder: [IOSurfaceID] = []
    private var latchTenBit = false
    /// The last luma's size, for the park's fit check.
    private var latchLumaSize: CGSize?

    /// The planes of `pixelBuffer` as resident textures, wrapped on first sight. nil when the
    /// device cannot latch or the surface cannot be wrapped. RENDER THREAD.
    private func latchTextures(
        for pixelBuffer: CVPixelBuffer, tenBit: Bool
    ) -> (luma: MTLTexture, chroma: MTLTexture)? {
        guard #available(iOS 18, tvOS 18, macOS 15, visionOS 2, *),
              let residency = latchResidency as? MTLResidencySet,
              let surface = CVPixelBufferGetIOSurface(pixelBuffer)?.takeUnretainedValue()
        else { return nil }
        let id = IOSurfaceGetID(surface)
        if latchTenBit != tenBit {
            for entry in latchCache.values {
                residency.removeAllocation(entry.luma)
                residency.removeAllocation(entry.chroma)
            }
            latchCache.removeAll()
            latchOrder.removeAll()
            latchTenBit = tenBit
        }
        if let hit = latchCache[id], hit.surface === surface { return (hit.luma, hit.chroma) }
        func plane(_ i: Int, _ format: MTLPixelFormat) -> MTLTexture? {
            let desc = MTLTextureDescriptor.texture2DDescriptor(
                pixelFormat: format, width: CVPixelBufferGetWidthOfPlane(pixelBuffer, i),
                height: CVPixelBufferGetHeightOfPlane(pixelBuffer, i), mipmapped: false)
            desc.usage = .shaderRead
            return device.makeTexture(descriptor: desc, iosurface: surface, plane: i)
        }
        guard let luma = plane(0, tenBit ? .r16Unorm : .r8Unorm),
              let chroma = plane(1, tenBit ? .rg16Unorm : .rg8Unorm)
        else { return nil }
        if let stale = latchCache.removeValue(forKey: id) {
            residency.removeAllocation(stale.luma)
            residency.removeAllocation(stale.chroma)
            latchOrder.removeAll { $0 == id }
        }
        while latchOrder.count >= 48, let oldest = latchOrder.first {
            latchOrder.removeFirst()
            if let entry = latchCache.removeValue(forKey: oldest) {
                residency.removeAllocation(entry.luma)
                residency.removeAllocation(entry.chroma)
            }
        }
        latchCache[id] = (surface, luma, chroma)
        latchOrder.append(id)
        residency.addAllocation(luma)
        residency.addAllocation(chroma)
        residency.commit()
        latchLumaSize = CGSize(width: luma.width, height: luma.height)
        if presentDebug { print("pf-latch surface \(id) wrapped (\(latchCache.count) live)") }
        return (luma, chroma)
    }

    /// Encode the deadline path's pass for `drawable` now and park it on the GPU behind the
    /// event; `latch` names its frame at the pick. nil when the pass cannot park — no frame
    /// wrapped yet, the drawable's format lags the layer, a smaller target wants the Lanczos
    /// pass, or the pipeline has no latched twin — and the pick encodes as before. The pass
    /// starts as a skip, so a signal that passes it over runs it as a discard. RENDER THREAD.
    func park(_ drawable: CAMetalDrawable) -> ParkedPass? {
        guard Self.lateLatchEnabled, !onCurve, latchResidency != nil,
              drawable.texture.pixelFormat == layerFormat,
              let queue = latchQueue, let event = latchEvent, let pipelines = Self.pipelines,
              let lumaSize = latchLumaSize,
              lanczos == nil || visibleLuma(size: lumaSize, target: drawable.texture).fits
        else { return nil }
        let pipeline: MTLRenderPipelineState?
        #if os(tvOS)
        pipeline = hdrActive
            ? (hdrPassthroughActive ? pipelines.latchedHDR : nil)
            : (tenBitSDRActive ? pipelines.latchedSDR10 : pipelines.latchedSDR)
        #else
        pipeline = hdrActive
            ? pipelines.latchedHDR : (tenBitSDRActive ? pipelines.latchedSDR10 : pipelines.latchedSDR)
        #endif
        guard let pipeline, let commandBuffer = queue.makeCommandBuffer() else { return nil }
        let value = latchValue + 1
        let uniform = latchUniforms[Int(value % 3)]
        uniform.contents().storeBytes(of: Int32(-1), toByteOffset: LatchUniform.slotOffset, as: Int32.self)
        commandBuffer.encodeWaitForEvent(event, value: value)
        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = drawable.texture
        pass.colorAttachments[0].loadAction = .dontCare
        // A skipped pass still stores its untouched tile; a store the pick could cancel would
        // save that write.
        pass.colorAttachments[0].storeAction = .store
        guard let encoder = commandBuffer.makeRenderCommandEncoder(descriptor: pass) else { return nil }
        encoder.setRenderPipelineState(pipeline)
        encoder.setVertexBuffer(uniform, offset: 0, index: 0)
        encoder.setFragmentBuffer(uniform, offset: 0, index: 0)
        encoder.drawPrimitives(type: .triangle, vertexStart: 0, vertexCount: 3)
        encoder.endEncoding()
        latchValue = value
        let parked = ParkedPass(
            drawable: drawable, value: value, uniform: uniform, hdr: hdrActive,
            tenBit: latchTenBit)
        // The present goes out as the GPU schedules the pass, not from the pick: presented
        // earlier, the drawable has no pending write yet and CoreAnimation shows what it held.
        commandBuffer.addScheduledHandler { _ in
            parked.lock.lock()
            let presents = parked.presents
            parked.lock.unlock()
            if presents { parked.drawable.present() }
        }
        commandBuffer.addCompletedHandler { cb in
            parked.lock.lock()
            let onGPU = parked.onGPU, signalAt = parked.signalAt
            parked.keep.removeAll()
            parked.lock.unlock()
            if let onGPU, cb.gpuEndTime > signalAt {
                onGPU(max(cb.gpuStartTime, signalAt), cb.gpuEndTime)
            }
        }
        commandBuffer.commit()
        return parked
    }

    /// At the pick: name the frame a parked pass samples, release it and let it present.
    /// false when the pass cannot show this frame — its kind differs from the park's, or the
    /// surface cannot be wrapped — then the pass is skipped and the caller re-rings the frame.
    /// The GPU time reported runs from the signal. RENDER THREAD.
    func latch(
        _ parked: ParkedPass, _ pixelBuffer: CVPixelBuffer, isHDR: Bool,
        onPresented: ((Int64?) -> Void)?
    ) -> Bool {
        let tenBit = Self.tenBitBuffer(pixelBuffer)
        configure(hdr: isHDR, tenBitSDR: tenBit)
        applyStagedHdrMeta()
        guard parked.hdr == isHDR, parked.tenBit == tenBit, hdrActive == isHDR,
              let planes = latchTextures(for: pixelBuffer, tenBit: tenBit)
        else {
            skip(parked)
            return false
        }
        var uniform = LatchUniform(
            rects: visibleLuma(size: CGSize(width: planes.luma.width, height: planes.luma.height),
                               target: parked.drawable.texture).rects,
            csc: CscRows.rows(CscRows.signal(of: pixelBuffer), depth: tenBit ? 10 : 8, msbPacked: tenBit),
            target: hdrTarget(), slot: 0,
            luma: planes.luma.gpuResourceID, chroma: planes.chroma.gpuResourceID)
        parked.uniform.contents().copyMemory(from: &uniform, byteCount: MemoryLayout<LatchUniform>.stride)
        #if targetEnvironment(simulator)
        onPresented?(nil)
        #else
        if let onPresented {
            parked.drawable.addPresentedHandler { d in
                onPresented(
                    d.presentedTime > 0
                        ? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: d.presentedTime) : nil)
            }
        }
        #endif
        parked.lock.lock()
        parked.keep = [pixelBuffer, planes.luma, planes.chroma]
        parked.onGPU = onGPUTime
        parked.presents = true
        parked.signalAt = CACurrentMediaTime()
        parked.lock.unlock()
        latchEvent?.signaledValue = parked.value
        return true
    }

    /// Release a parked pass without a frame: it runs as a discard and its drawable goes back
    /// to the pool unpresented. RENDER THREAD.
    func skip(_ parked: ParkedPass) {
        guard let latchEvent, latchEvent.signaledValue < parked.value else { return }
        latchEvent.signaledValue = parked.value
    }

    /// Release every parked pass: the render thread's exit.
    func flushParked() {
        guard let latchEvent, latchEvent.signaledValue < latchValue else { return }
        latchEvent.signaledValue = latchValue
    }

    /// Deadline pacing, RENDER THREAD, on every frame arrival: bring the layer's size and colour
    /// config in line with the frame before a drawable exists. The link vends from the layer's
    /// current config, and the layer starts at `drawableSize` 0×0, where every vend fails
    /// allocation — so the first frame's reconcile is what lets the first vend succeed. Drains
    /// the same staging as the render path; both are idempotent about it.
    func reconcileLayer(decodedSize: CGSize, isHDR: Bool, tenBitSDR: Bool = false) {
        stagingLock.lock()
        let targetFromLayout = drawableTarget
        stagingLock.unlock()
        configure(hdr: isHDR, tenBitSDR: tenBitSDR)
        applyStagedHdrMeta()
        sizeLayer(
            (targetFromLayout.width > 0 && targetFromLayout.height > 0)
                ? targetFromLayout : decodedSize)
    }

    /// Draw one decoded frame to the next drawable and present it. RENDER THREAD;
    /// `nextDrawable()` may block, so it never runs on main. `isHDR` selects and reconciles the
    /// HDR/SDR layer path. Returns false when no drawable or texture is available or Metal fails.
    ///
    /// `onPresented` fires on a Metal callback with the drawable's CLOCK_REALTIME on-glass stamp,
    /// or nil when the system drops it. `presentAtMediaTime` schedules an absolute display-link
    /// target; nil presents when the GPU finishes. `into` supplies a CAMetalDisplayLink drawable
    /// for deadline pacing instead of calling `nextDrawable()`.
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
        var target = hdrTarget()
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
        // Warm the latch: the first park needs a plane size, and a wrapped surface saves the
        // pick the wrap.
        if Self.lateLatchEnabled { _ = latchTextures(for: pixelBuffer, tenBit: tenBit) }

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
            encoder.setFragmentBytes(&target, length: MemoryLayout<HdrTargetUniform>.stride, index: 1)
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
        // A PQ (HDR) pyrowave stream drives the same layer/EDR machinery as the biplanar path.
        configure(hdr: planes.pq)
        applyStagedHdrMeta()
        var csc = planes.csc
        var target = hdrTarget()
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
            encoder.setFragmentBytes(&target, length: MemoryLayout<HdrTargetUniform>.stride, index: 1)
        }
    }

    /// The shared present tail of `render`/`renderPlanar`: size the drawable, encode one
    /// fullscreen triangle with `pipeline` (`luma` through `lumaForTarget` at index 0, `bind` the
    /// rest), schedule the present and the on-glass callback.
    ///
    /// `providedDrawable` (deadline pacing) is the CAMetalDisplayLink-vended drawable to render
    /// into instead of `nextDrawable()`. It was vended against the layer's config at vend time,
    /// so after a mid-session reconfigure (HDR flip) its pixel format can lag `layerFormat`, the
    /// pipeline's attachment format — encoding would be a Metal validation failure. The guard
    /// returns false instead: the drawable drops back to the pool, the caller re-rings the frame,
    /// and a vend after the link applied the queued write carries the new format.
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
        sizeLayer(targetSize)
        #if DEBUG
        logSizeIfChanged(decoded: decodedSize, drawable: targetSize)
        #endif
        if let providedDrawable, providedDrawable.texture.pixelFormat != layerFormat {
            return false // config outran the vend (HDR flip) — a later vend has the new format
        }
        guard let target = presentTarget(provided: providedDrawable, decodedSize: decodedSize),
              let commandBuffer = queue.makeCommandBuffer()
        else { return false }
        let (lumaTexture, uvRects) = lumaForTarget(
            luma, target: target.texture, commandBuffer: commandBuffer)
        var rects = uvRects

        let pass = MTLRenderPassDescriptor()
        pass.colorAttachments[0].texture = target.texture
        pass.colorAttachments[0].loadAction = .dontCare // the fullscreen triangle writes every pixel
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
            // back to its display-link estimate. The curve reports none: done is its stamp.
            if let drawable = target.drawable {
                drawable.addPresentedHandler { d in
                    onPresented(
                        d.presentedTime > 0
                            ? Stage2Pipeline.realtimeNs(forDisplayLinkTimestamp: d.presentedTime)
                            : nil)
                }
            } else {
                commandBuffer.addCompletedHandler { _ in onPresented(nil) }
            }
            #endif
        }
        // Keep the bound sources alive until the GPU finishes sampling (see the callers).
        let onGPUTime = onGPUTime
        commandBuffer.addCompletedHandler { cb in
            _ = keepAlive
            if let onGPUTime, cb.gpuEndTime > cb.gpuStartTime {
                onGPUTime(cb.gpuStartTime, cb.gpuEndTime)
            }
        }
        target.present(commandBuffer, presentAtMediaTime)
        commandBuffer.commit()
        return true
    }

    /// Where one frame draws: a layer drawable, or the curved screen's.
    private struct PresentTarget {
        let texture: MTLTexture
        /// The layer's drawable, for its on-glass stamp; nil on the curve.
        let drawable: CAMetalDrawable?
        let present: (MTLCommandBuffer, CFTimeInterval?) -> Void
    }

    /// The drawable this frame draws into. The curve takes the frame's own size and range; it
    /// shows the whole picture whatever the view's layout. RENDER THREAD.
    private func presentTarget(provided: CAMetalDrawable?, decodedSize: CGSize) -> PresentTarget? {
        #if os(visionOS)
        if let curve {
            guard let frame = curve.next(size: decodedSize, hdr: hdrActive) else { return nil }
            return PresentTarget(texture: frame.view, drawable: nil) { commandBuffer, _ in
                commandBuffer.present(frame.drawable)
            }
        }
        #endif
        guard let drawable = provided ?? layer.nextDrawable() else { return nil }
        // An absolute link target is the fixed-grid path; without one, present when the GPU
        // finishes.
        return PresentTarget(texture: drawable.texture, drawable: drawable) { commandBuffer, at in
            if let at {
                commandBuffer.present(drawable, atTime: at)
            } else {
                commandBuffer.present(drawable)
            }
        }
    }

    /// Luma to bind for a draw into `target`, with the UV rects that sample it. The plane itself
    /// when the visible region is not larger than the target; otherwise that region Lanczos-scaled
    /// to the target's size, sampled over the unit square. r16Float holds a 10-bit code to within
    /// one step and is writable and filterable on every Apple GPU. The shader filters luma only
    /// while it resamples: a 1:1 plane on the pixel grid, or the Lanczos output, takes one tap.
    /// RENDER THREAD.
    private func lumaForTarget(
        _ luma: MTLTexture, target: MTLTexture, commandBuffer: MTLCommandBuffer
    ) -> (MTLTexture, UvRects) {
        let visible = visibleLuma(
            size: CGSize(width: luma.width, height: luma.height), target: target)
        guard !visible.fits, let lanczos = lanczos else { return (luma, visible.rects) }
        let src = visible.src
        let cropW = visible.crop.width, cropH = visible.crop.height
        if scaledLuma?.width != target.width || scaledLuma?.height != target.height {
            let desc = MTLTextureDescriptor.texture2DDescriptor(
                pixelFormat: .r16Float, width: target.width, height: target.height,
                mipmapped: false)
            desc.usage = [.shaderRead, .shaderWrite]
            desc.storageMode = .private
            scaledLuma = device.makeTexture(descriptor: desc)
        }
        guard let scaledLuma else { return (luma, visible.rects) }
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
        return (
            scaledLuma,
            UvRects(
                frame: visible.rects.frame, luma: SIMD4<Float>(0, 0, 1, 1),
                mode: SIMD4<Float>(0, 0, 0, 0)))
    }

    /// The visible part of a luma plane of `size` (the staged source rect) as the rects that
    /// sample it whole, its size in texels, and whether a `target`-sized drawable holds it
    /// without the Lanczos pass (not smaller than the crop). RENDER THREAD.
    private func visibleLuma(
        size: CGSize, target: MTLTexture
    ) -> (rects: UvRects, src: CGRect, crop: CGSize, fits: Bool) {
        stagingLock.lock()
        let src = sourceRect
        stagingLock.unlock()
        let frame = SIMD4<Float>(
            Float(src.minX), Float(src.minY), Float(src.width), Float(src.height))
        let cropW = src.width * size.width, cropH = src.height * size.height
        let onGrid = Self.lumaOnGrid(
            target: CGSize(width: target.width, height: target.height),
            crop: CGRect(
                x: src.minX * size.width, y: src.minY * size.height,
                width: cropW, height: cropH))
        let fits = CGFloat(target.width) >= cropW - 0.5 && CGFloat(target.height) >= cropH - 0.5
        return (
            UvRects(frame: frame, luma: frame, mode: SIMD4<Float>(onGrid ? 0 : 1, 0, 0, 0)),
            src, CGSize(width: cropW, height: cropH), fits)
    }

    /// A visible region (in luma texels) that lands 1:1 on a `target`-sized drawable: same size,
    /// whole-texel origin, so every pixel centre is a texel centre. Internal for tests.
    static func lumaOnGrid(target: CGSize, crop: CGRect) -> Bool {
        abs(target.width - crop.width) < 0.5 && abs(target.height - crop.height) < 0.5
            && abs(crop.minX - crop.minX.rounded()) < 0.01
            && abs(crop.minY - crop.minY.rounded()) < 0.01
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

/// `UvRects` in the shader, field for field: three `float4`, 48 bytes. `mode.x` is the shader's
/// `upscale`.
private struct UvRects {
    var frame: SIMD4<Float>
    var luma: SIMD4<Float>
    var mode: SIMD4<Float>
}

/// `Latch` in the shader, field for field: the vertex rects first, so one buffer serves both
/// stages, then the rows, the HDR target, the slot and the planes' resource IDs. The pads are
/// three plain ints so `luma` lands at 120 on both sides. A slot below zero skips.
private struct LatchUniform {
    var rects: UvRects
    var csc: CscUniform
    var target: HdrTargetUniform
    var slot: Int32
    var pad0: Int32 = 0, pad1: Int32 = 0, pad2: Int32 = 0
    var luma: MTLResourceID
    var chroma: MTLResourceID
    static let slotOffset = MemoryLayout<LatchUniform>.offset(of: \.slot)!
}

/// The deadline path's pass, encoded at the vend and parked on the GPU behind the presenter's
/// event until `latch` names its frame or `skip` discards it. Render-thread confined; the
/// completed handler holds it, and so the drawable and the frame, until the GPU is done.
final class ParkedPass {
    let drawable: CAMetalDrawable
    fileprivate let value: UInt64
    fileprivate let uniform: MTLBuffer
    fileprivate let hdr: Bool, tenBit: Bool
    /// Written at the latch, read by the completed handler: the lock orders the two threads.
    fileprivate let lock = NSLock()
    fileprivate var keep: [Any] = []
    fileprivate var signalAt: CFTimeInterval = 0
    fileprivate var onGPU: (@Sendable (CFTimeInterval, CFTimeInterval) -> Void)?
    /// Set by `latch`: the scheduled handler presents. A skip leaves it false.
    fileprivate var presents = false

    fileprivate init(
        drawable: CAMetalDrawable, value: UInt64, uniform: MTLBuffer, hdr: Bool, tenBit: Bool
    ) {
        self.drawable = drawable
        self.value = value
        self.uniform = uniform
        self.hdr = hdr
        self.tenBit = tenBit
    }
}

#endif
