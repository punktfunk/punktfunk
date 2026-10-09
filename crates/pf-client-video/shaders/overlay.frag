// The overlay composite: one premultiplied-alpha RGBA texture (the console UI's
// offscreen image) sampled over the swapchain. Blending happens in fixed function
// (src ONE, dst ONE_MINUS_SRC_ALPHA — Skia surfaces are premultiplied).
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450

layout(location = 0) in vec2 v_uv;
layout(location = 0) out vec4 frag;

layout(set = 0, binding = 0) uniform sampler2D u_tex;

void main() {
    frag = texture(u_tex, v_uv);
}
