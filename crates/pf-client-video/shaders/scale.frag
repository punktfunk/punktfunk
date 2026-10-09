// One axis of the video scale (punktfunk_core::video_fit's filter ladder). Two passes:
// x from the frame into an intermediate, then y into the destination rect. The other
// axis passes through texel for texel.
//
// Kernel 0 is nearest, 1 Catmull-Rom (upscale), 2 Lanczos-3 widened by the downscale
// ratio. Texel i covers [i, i+1); weights are normalised, so edge clamping keeps the
// brightness of the border rows.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450

layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_src;

layout(push_constant) uniform Params {
    float origin;       // source coordinate at the destination edge on the filtered axis
    float step;         // source texels per destination pixel
    float dst_offset;   // destination edge on the filtered axis, in framebuffer pixels
    float other_offset; // destination edge on the pass-through axis
    int axis;           // 0 = filter x, 1 = filter y
    int kernel;
    int size;           // source texels along the filtered axis
    int other_size;     // source texels along the pass-through axis
} pc;

// Footprint cap: a 10x downscale still fits the tap loop.
const float MAX_STRETCH = 10.0;
const int MAX_TAPS = 64;

vec4 fetch(int i, int other) {
    return pc.axis == 0 ? texelFetch(u_src, ivec2(i, other), 0)
                        : texelFetch(u_src, ivec2(other, i), 0);
}

float catmull_rom(float x) {
    x = abs(x);
    if (x < 1.0) return (1.5 * x - 2.5) * x * x + 1.0;
    if (x < 2.0) return ((-0.5 * x + 2.5) * x - 4.0) * x + 2.0;
    return 0.0;
}

float lanczos3(float x) {
    x = abs(x);
    if (x < 1e-5) return 1.0;
    if (x >= 3.0) return 0.0;
    float px = 3.14159265358979 * x;
    return 3.0 * sin(px) * sin(px / 3.0) / (px * px);
}

void main() {
    vec2 fc = gl_FragCoord.xy;
    float d = (pc.axis == 0 ? fc.x : fc.y) - pc.dst_offset;
    int other = clamp(int(floor((pc.axis == 0 ? fc.y : fc.x) - pc.other_offset)), 0, pc.other_size - 1);
    float s = pc.origin + d * pc.step;
    if (pc.kernel == 0) {
        frag = fetch(clamp(int(floor(s)), 0, pc.size - 1), other);
        return;
    }
    float stretch = pc.kernel == 2 ? clamp(pc.step, 1.0, MAX_STRETCH) : 1.0;
    float support = (pc.kernel == 2 ? 3.0 : 2.0) * stretch;
    int first = int(floor(s - support - 0.5)) + 1;
    int last = int(ceil(s + support - 0.5)) - 1;
    last = min(last, first + MAX_TAPS - 1);
    vec4 acc = vec4(0.0);
    float total = 0.0;
    for (int i = first; i <= last; i++) {
        float x = (float(i) + 0.5 - s) / stretch;
        float w = pc.kernel == 2 ? lanczos3(x) : catmull_rom(x);
        acc += w * fetch(clamp(i, 0, pc.size - 1), other);
        total += w;
    }
    frag = vec4((acc / total).rgb, 1.0);
}
