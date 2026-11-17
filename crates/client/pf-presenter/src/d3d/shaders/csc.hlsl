// YCbCr -> RGB for the D3D11 presenter: the HLSL twin of pf-client-video's fullscreen.vert,
// planar_csc.frag, nv12_csc.frag and tonemap.glsl. Same constant contract as the Vulkan push
// constants: csc_rows' three rows, then params (x: mode, 1 = PQ -> SDR tonemap; y: tonemap
// peak; zw: uv scale, display / coded). Compiled at runtime by D3DCompile (vs_5_0 / ps_5_0).

cbuffer Csc : register(b0) {
    float4 r0;
    float4 r1;
    float4 r2;
    float4 params;
};

Texture2D<float> u_y : register(t0);
Texture2D<float> u_cb : register(t1);
Texture2D<float> u_cr : register(t2);
// A pool slice's two planes, each a one-slice array view. Registers apart from the CPU
// planes, so no register carries two texture types.
Texture2DArray<float> u_slice_y : register(t3);
Texture2DArray<float2> u_slice_cbcr : register(t4);
SamplerState u_linear : register(s0);

struct VsOut {
    float4 pos : SV_Position;
    float2 uv : TEXCOORD0;
};

// The bufferless fullscreen triangle. D3D clip space has y = +1 at the TOP, so y flips
// here and uv (0,0) still lands on the top-left, where the video's row 0 lives.
VsOut vs_fullscreen(uint id : SV_VertexID) {
    VsOut o;
    float2 uv = float2((id << 1) & 2, id & 2);
    o.uv = uv;
    o.pos = float4(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    return o;
}

// SMPTE ST.2084 (PQ) EOTF: code value -> linear, 1.0 = 10000 nits.
float3 pq_eotf(float3 e) {
    const float m1 = 0.1593017578125;  // 2610/16384
    const float m2 = 78.84375;         // 2523/4096 * 128
    const float c1 = 0.8359375;        // 3424/4096
    const float c2 = 18.8515625;       // 2413/4096 * 32
    const float c3 = 18.6875;          // 2392/4096 * 32
    float3 p = pow(max(e, 0.0), 1.0 / m2);
    return pow(max(p - c1, 0.0) / (c2 - c3 * p), 1.0 / m1);
}

// Inverse of pq_eotf for one channel: linear (1.0 = 10000 nits) -> code value.
float pq_oetf(float y) {
    const float m1 = 0.1593017578125;
    const float m2 = 78.84375;
    const float c1 = 0.8359375;
    const float c2 = 18.8515625;
    const float c3 = 18.6875;
    float p = pow(saturate(y), m1);
    return pow((c1 + c2 * p) / (1.0 + c3 * p), m2);
}

// BT.2020 -> BT.709 primaries (linear light). Rows of the GLSL column-major mat3.
float3 bt2020_to_709(float3 c) {
    return float3(
        dot(float3(1.6605, -0.5876, -0.0728), c),
        dot(float3(-0.1246, 1.1329, -0.0083), c),
        dot(float3(-0.0182, -0.1006, 1.1187), c));
}

// Linear -> sRGB OETF.
float3 srgb_oetf(float3 c) {
    c = saturate(c);
    float3 hi = 1.055 * pow(c, 1.0 / 2.4) - 0.055;
    return lerp(hi, c * 12.92, float3(c <= 0.0031308));
}

// `pq`: full-range PQ R'G'B' (BT.2020). `peak`: source peak in 203-nit units.
float3 pq_to_sdr(float3 pq, float peak) {
    float3 lin = max(bt2020_to_709(pq_eotf(saturate(pq))), 0.0);
    float l = max(lin.r, max(lin.g, lin.b));
    if (l > 0.0) {
        const float white = 203.0 / 10000.0;
        float src = pq_oetf(max(peak, 1.0001) * white);
        float max_lum = pq_oetf(white) / src;
        float ks = 1.5 * max_lum - 0.5;
        float e = min(pq_oetf(l) / src, 1.0);
        if (e > ks) {
            float t = (e - ks) / (1.0 - ks);
            float t2 = t * t;
            float t3 = t2 * t;
            e = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks)
                + (-2.0 * t3 + 3.0 * t2) * max_lum;
        }
        lin *= pq_eotf(float3(e * src, e * src, e * src)).r / l;
    }
    return srgb_oetf(lin / (203.0 / 10000.0));
}

float3 apply_rows(float3 yuv) {
    return float3(
        dot(r0.xyz, yuv) + r0.w,
        dot(r1.xyz, yuv) + r1.w,
        dot(r2.xyz, yuv) + r2.w);
}

float4 finish(float3 rgb) {
    if (params.x > 0.5) {
        rgb = pq_to_sdr(rgb, params.y);
    } else {
        rgb = saturate(rgb);
    }
    return float4(rgb, 1.0);
}

// Three R8 planes (the CPU I420 lane). Left-cosited 4:2:0 chroma sampled at luma uv
// assumes centre siting: +0.25 chroma texel re-aligns it; full-res chroma skips it.
float4 ps_planar(VsOut i) : SV_Target {
    uint yw, yh, cw, ch;
    u_y.GetDimensions(yw, yh);
    u_cb.GetDimensions(cw, ch);
    float2 cuv = i.uv;
    if (cw < yw) {
        cuv.x += 0.25 / float(cw);
    }
    float3 yuv = float3(
        u_y.Sample(u_linear, i.uv),
        u_cb.Sample(u_linear, cuv),
        u_cr.Sample(u_linear, cuv));
    return finish(apply_rows(yuv));
}

// Two planes of a pool slice (NV12 or P010): luma, then interleaved CbCr. `params.zw` crops
// uv to the display rect of the coded slice; chroma siting as `ps_planar`.
float4 ps_nv12(VsOut i) : SV_Target {
    float2 uv = i.uv * params.zw;
    uint yw, yh, cw, ch, n;
    u_slice_y.GetDimensions(yw, yh, n);
    u_slice_cbcr.GetDimensions(cw, ch, n);
    float2 cuv = uv;
    if (cw < yw) {
        cuv.x += 0.25 / float(cw);
    }
    float y = u_slice_y.Sample(u_linear, float3(uv, 0.0));
    float2 cbcr = u_slice_cbcr.Sample(u_linear, float3(cuv, 0.0));
    return finish(apply_rows(float3(y, cbcr)));
}
