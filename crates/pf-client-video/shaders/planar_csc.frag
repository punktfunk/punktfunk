// Planar 3-plane YCbCr → RGBA — the PyroWave variant of nv12_csc.frag (separate Cb and
// Cr R8 planes instead of an interleaved CbCr plane; design/pyrowave-codec-plan.md §4.5).
// Same push-constant contract (csc_rows precomputes the matrix + range expansion), same
// output modes — though PyroWave itself is 8-bit SDR BT.709 limited, keeping parity means
// one less divergence if the codec ever signals more. 4:4:4 needs no shader change: the
// chroma planes arrive full-res and the siting correction self-disables.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450
#extension GL_GOOGLE_include_directive : require

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_y;
layout(set = 0, binding = 1) uniform sampler2D u_cb;
layout(set = 0, binding = 2) uniform sampler2D u_cr;

layout(push_constant) uniform Csc {
    vec4 r0;
    vec4 r1;
    vec4 r2;
    vec4 params; // x: mode, y: tonemap peak, z/w: reserved
} pc;

#include "tonemap.glsl"

void main() {
    // Left-cosited 4:2:0 chroma sampled at luma UV assumes CENTER siting — offset +0.25
    // chroma texels to re-align (same correction as nv12_csc.frag; self-disables when the
    // chroma plane is full-res).
    vec2 cuv = v_uv;
    int cw = textureSize(u_cb, 0).x;
    if (cw < textureSize(u_y, 0).x) {
        cuv.x += 0.25 / float(cw);
    }
    vec3 yuv = vec3(texture(u_y, v_uv).r, texture(u_cb, cuv).r, texture(u_cr, cuv).r);
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
