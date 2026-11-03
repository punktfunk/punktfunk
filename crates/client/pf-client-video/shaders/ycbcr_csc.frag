// YCbCr → RGBA through a sampler Y'CbCr conversion: nv12_csc.frag for a decode picture that
// has no plane views. The immutable sampler carries an RGB_IDENTITY conversion, so the driver
// only rebuilds chroma and hands back raw Y, Cb and Cr. The siting lives in that conversion
// (`pf_vkdecode::ycbcr_conversion_info`), which replaces nv12_csc.frag's quarter-texel offset.
// Same push-constant contract, same output modes.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450
#extension GL_GOOGLE_include_directive : require

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_ycbcr;

layout(push_constant) uniform Csc {
    vec4 r0;
    vec4 r1;
    vec4 r2;
    vec4 params; // x: mode, y: tonemap peak, zw: crop/pool UV scale
} pc;

#include "tonemap.glsl"

void main() {
    // G is luma, B is Cb, R is Cr: a two-plane format's components, unreordered.
    vec3 yuv = texture(u_ycbcr, v_uv * pc.params.zw).gbr;
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
