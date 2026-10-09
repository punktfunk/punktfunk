// YCbCr (2-plane 4:2:0) → RGBA with the stream's CICP signaling — the Vulkan port of
// the GL presenter's fragment shader, grown depth- and HDR-aware.
//
// The YUV→RGB matrix + range expansion arrive as three push-constant rows precomputed
// on the CPU (csc.rs `csc_rows` — bit-depth exact, including the P010/X6 MSB-packing
// factor): rgb[i] = dot(r_i.xyz, yuv) + r_i.w. One shader for BT.601/709/2020,
// full/limited, 8- and 10-bit. The chroma plane is half-res; the linear sampler
// interpolates, same as the GL path.
//
// params.x selects the output mode:
//   0 — passthrough: the transfer stays baked (SDR BT.709 shown as-is; PQ BT.2020
//       written to an HDR10 swapchain that expects exactly PQ-encoded values).
//   1 — PQ → SDR tonemap (an HDR stream on a desktop without an HDR10 surface):
//       `pq_to_sdr` in tonemap.glsl.
// params.y = tonemap source peak in 203-nit units (~= peak_nits / 203).
// params.zw = the crop→surface UV scale (frame size / decode-pool size). A Vulkan-Video
//       pool image is the CODED surface, taller than the picture whenever the height is
//       not a multiple of the driver's alignment (1080 → 1088); sampling the full 0..1
//       would drag those padding rows into view — and since encoders fill them by
//       replicating the last picture line, that reads as the bottom row smeared over the
//       final few rows. 1.0/1.0 for every path whose image is already crop-sized (dmabuf
//       imports the planes at the crop over the real stride; D3D11VA clamps in its
//       VideoProcessor blit).
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450
#extension GL_GOOGLE_include_directive : require

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_y;
layout(set = 0, binding = 1) uniform sampler2D u_c;

layout(push_constant) uniform Csc {
    vec4 r0;
    vec4 r1;
    vec4 r2;
    vec4 params; // x: mode, y: tonemap peak, zw: crop/pool UV scale
} pc;

#include "tonemap.glsl"

void main() {
    // Crop to the visible picture: the triangle spans the whole render target, so its 0..1
    // maps onto the pool surface only after this scale (see params.zw above).
    vec2 uv = v_uv * pc.params.zw;
    // 4:2:0 chroma is left-cosited (H.273 type 0 — the default inference when unsignaled, and
    // what the hosts produce), but sampling the half-res plane at the luma UV assumes CENTER
    // siting — a ~0.5-luma-px rightward chroma shift on hard colored edges. Offset +0.25 chroma
    // texels to re-align (the same correction the Apple/Windows clients apply). Self-disables
    // when the plane widths match (a full-size 4:4:4 chroma plane needs no correction).
    // textureSize is the POOL's chroma width, which is the space `uv` is already in — so the
    // offset stays a true quarter-texel whatever the crop.
    vec2 cuv = uv;
    int cw = textureSize(u_c, 0).x;
    if (cw < textureSize(u_y, 0).x) {
        cuv.x += 0.25 / float(cw);
    }
    vec3 yuv = vec3(texture(u_y, uv).r, texture(u_c, cuv).rg);
    vec3 rgb = vec3(
        dot(pc.r0.xyz, yuv) + pc.r0.w,
        dot(pc.r1.xyz, yuv) + pc.r1.w,
        dot(pc.r2.xyz, yuv) + pc.r2.w
    );

    if (pc.params.x > 0.5) {
        rgb = pq_to_sdr(rgb, pc.params.y);
    } else {
        rgb = clamp(rgb, 0.0, 1.0);
    }
    frag = vec4(rgb, 1.0);
}
