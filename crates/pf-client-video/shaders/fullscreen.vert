// The bufferless fullscreen triangle (same trick as the GL presenter's gl_VertexID
// geometry). Vulkan clip space: y=-1 is the TOP, so uv (0,0) lands on the top-left —
// which is exactly where the video's row 0 lives. No flip anywhere.
//
// Regenerate: shaders/build.sh (committed .spv, no build-time toolchain).
#version 450

layout(location = 0) out vec2 v_uv;

void main() {
    vec2 uv = vec2((gl_VertexIndex << 1) & 2, gl_VertexIndex & 2);
    v_uv = uv;
    gl_Position = vec4(uv * 2.0 - 1.0, 0.0, 1.0);
}
