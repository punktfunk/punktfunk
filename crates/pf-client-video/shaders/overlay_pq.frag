// overlay.frag for an HDR10 swapchain. The console UI is premultiplied sRGB, so each texel is
// linearised, put at 203-nit SDR white in BT.2020 and PQ-encoded before the fixed-function blend.
// Partial alpha then blends PQ codes, not light.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450
#extension GL_GOOGLE_include_directive : require

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_tex;

#include "tonemap.glsl"

void main() {
    vec4 t = texture(u_tex, v_uv);
    if (t.a <= 0.0) {
        frag = vec4(0.0);
        return;
    }
    vec3 c = clamp(t.rgb / t.a, 0.0, 1.0);
    bvec3 lo = lessThanEqual(c, vec3(0.04045));
    vec3 lin = mix(pow((c + 0.055) / 1.055, vec3(2.4)), c / 12.92, vec3(lo));
    // BT.709 → BT.2020 primaries (BT.2087), linear light.
    lin = mat3(
        0.6274, 0.0691, 0.0164,
        0.3293, 0.9195, 0.0880,
        0.0433, 0.0114, 0.8956
    ) * lin * (203.0 / 10000.0);
    vec3 pq = vec3(pq_oetf(lin.r), pq_oetf(lin.g), pq_oetf(lin.b));
    frag = vec4(pq * t.a, t.a);
}
