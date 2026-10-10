// PyroWave decode compute kernels — the Metal port of the vendored Vulkan shaders
// (crates/codec/pyrowave-sys/vendor/pyrowave/shaders/wavelet_dequant.comp + idwt.comp, upstream pin
// c0b997f8, MIT © 2025 Hans-Kristian Arntzen). Runtime-compiled Swift strings per client
// convention (no metallib build step — see GamepadChrome.swift's rationale); these are the
// client's first compute pipelines.
//
// Port notes (design/pyrowave-codec-plan.md §4.7):
// • Only the STORAGE_MODE 0 path exists: MSL device pointers replace the 8/16-bit-storage SSBO
//   aliases; the texel-buffer (mode 1) and linear-image (mode 2) fallbacks are non-Apple IHV
//   workarounds and are dropped, as is the fragment-iDWT path (Mali/Adreno only).
// • Subgroup ops map 1:1: subgroupInclusiveAdd → simd_prefix_inclusive_sum, and the fixed
//   32-wide Apple simdgroups take the GLSL's `SubgroupSize <= 32` scan branch; the shuffle-up
//   and LDS fallbacks for exotic wave sizes are dead code here. The dequant kernel needs the
//   16 header lanes inside ONE simdgroup — MetalWaveletDecoder's probe enforces
//   threadExecutionWidth >= 16.
// • Precision matches upstream's desktop default (PYROWAVE_PRECISION=1): float arithmetic,
//   half2 threadgroup storage; the coefficient textures are R16Float for DWT levels 0–1 and
//   R32Float for levels 2–4 (the low-res levels feed long reconstruction chains — upstream
//   keeps them fp32 for exactly that reason).
// • The gather + mirrored-repeat addressing in idwt is the precision-sensitive spot (upstream
//   fought a Mali compiler bug there); the golden-frame PSNR fixtures are the guard.

import Foundation

let waveletShaderSource = """
#include <metal_stdlib>
using namespace metal;

// ---------------------------------------------------------------------------------------------
// Shared helpers (dwt_swizzle.h / constants.h / dwt_quant_scale.h)
// ---------------------------------------------------------------------------------------------

static inline int2 unswizzle8x8(uint index)
{
    uint y = extract_bits(index, 0, 1);
    uint x = extract_bits(index, 1, 2);
    y |= extract_bits(index, 3, 2) << 1;
    x |= extract_bits(index, 5, 1) << 2;
    return int2(int(x), int(y));
}

// GLSL bitfieldExtract(x, 0, n) where n may be 0; MSL extract_bits(bits=0) is not guaranteed
// to return 0, so mask explicitly.
static inline uint mask_lo(uint x, int n)
{
    return (n <= 0) ? 0u : (x & (0xffffffffu >> (32 - n)));
}

// pyrowave_common.hpp decode_quant: custom FP formulation, MaxScaleExp = 4.
static inline float decode_quant(uint quant_code)
{
    int e = 4 - int(quant_code >> 3);
    int m = int(quant_code) & 0x7;
    return (1.0f / (8.0f * 1024.0f * 1024.0f)) * float((8 + m) * (1 << (20 + e)));
}

// dwt_quant_scale.h: per-8x8 quant scale, min 0.25, max ~2.2.
static inline float decode_quant_scale(uint code)
{
    return float(code) / 8.0f + 0.25f;
}

// constants.h
constant int QUANT_SCALE_OFFSET = 20;
constant int QUANT_SCALE_BITS = 4;

// ---------------------------------------------------------------------------------------------
// wavelet_dequant — one 128-thread threadgroup decodes one 32x32 coefficient block
// ---------------------------------------------------------------------------------------------

struct DequantRegisters {
    int2 resolution;
    int output_layer;
    int block_offset_32x32;
    int block_stride_32x32;
};

struct DecodedPair { float4 col0; float4 col1; }; // GLSL mat2x4: m[j][i] -> colJ[i]

// Bit-plane magnitude decode for one thread's 4x2 coefficient group (decode_payload in the
// GLSL). `code_word` is the 8x8 block's 16-bit control word (2 bits of extra planes per 4x2
// group), `q_bits` the base plane count, `offset` the block's plane-payload start byte,
// `block_index` this thread's group (0..7). Nonzero magnitudes get the +0.5 deadzone
// reconstruction bias.
static DecodedPair decode_payload(const device uchar *payload_u8,
                                  uint code_word, uint q_bits, uint offset, uint block_index)
{
    DecodedPair m;
    m.col0 = float4(0.0f);
    m.col1 = float4(0.0f);
    if (code_word == 0)
        return m;

    int bit_offset = 2 * int(block_index);

    uint lsbs = code_word & 0x5555u;
    uint msbs = code_word & 0xaaaau;
    uint msbs_shift = msbs >> 1;
    msbs |= msbs_shift;

    uint byte_offset =
        popcount(mask_lo(lsbs, bit_offset)) +
        popcount(mask_lo(msbs, bit_offset)) +
        q_bits * block_index + offset;

    uint payload = uint(payload_u8[byte_offset]);

    uint local_control_word = extract_bits(code_word, uint(bit_offset), 2);
    int decoded_abs[8] = {0, 0, 0, 0, 0, 0, 0, 0};
    int plane_iterations = int(q_bits + local_control_word);

    for (int q = plane_iterations - 1; q >= 0; q--)
    {
        for (int b = 0; b < 8; b++)
        {
            int decoded = int(extract_bits(payload, uint(b), 1));
            decoded_abs[b] = insert_bits(decoded_abs[b], decoded, uint(q), 1);
        }
        byte_offset++;
        payload = uint(payload_u8[byte_offset]);
    }

    for (int i = 0; i < 4; i++)
    {
        for (int j = 0; j < 2; j++)
        {
            float v = float(decoded_abs[i * 2 + j]);
            if (v != 0.0f)
                v += 0.5f;
            if (j == 0) m.col0[i] = v; else m.col1[i] = v;
        }
    }
    return m;
}

kernel void wavelet_dequant(
    texture2d_array<float, access::write> uDequantImg [[texture(0)]],
    const device uint *payload_offsets [[buffer(0)]],
    const device uint *payload_u32 [[buffer(1)]],
    constant DequantRegisters &registers [[buffer(2)]],
    uint3 wg_id [[threadgroup_position_in_grid]],
    uint local_index [[thread_index_in_threadgroup]],
    uint simd_lane [[thread_index_in_simdgroup]],
    uint simd_group [[simdgroup_index_in_threadgroup]],
    uint simd_size [[threads_per_simdgroup]])
{
    // STORAGE_MODE 0's three aliased SSBO views over one buffer, as typed pointers.
    const device ushort *payload_u16 = reinterpret_cast<const device ushort *>(payload_u32);
    const device uchar *payload_u8 = reinterpret_cast<const device uchar *>(payload_u32);

    threadgroup uint shared_sign_offset;
    threadgroup uint shared_plane_byte_offsets[16];
    threadgroup uint shared_sign_scan[128 / 4];

    int block_index_32x32 = int(uint(registers.block_offset_32x32) +
        wg_id.y * uint(registers.block_stride_32x32) +
        wg_id.x);

    uint block_local_index = extract_bits(local_index, 0, 3);
    uint block_x = extract_bits(local_index, 3, 2);
    uint block_y = extract_bits(local_index, 5, 2);
    uint linear_block = block_y * 4 + block_x;

    // Each thread individually decodes 8 values (a 4x2 group of its 8x8 block).
    int2 local_coord = unswizzle8x8(block_local_index << 3);

    int2 coord = int2(wg_id.xy) * 32;
    coord += 8 * int2(int(block_x), int(block_y));
    coord += local_coord;

    uint offset_u32 = payload_offsets[block_index_32x32];

    // Missing / lost block: zero coefficients (this is how a partial frame's holes decode).
    if (offset_u32 == ~0u)
    {
        for (int j = 0; j < 2; j++)
            for (int i = 0; i < 4; i++)
                uDequantImg.write(float4(0.0f), uint2(coord + int2(i, j)), uint(registers.output_layer));
        return;
    }

    uint ballot = payload_u32[offset_u32] & 0xffffu;
    uint q_code = payload_u32[offset_u32 + 1] & 0xffu;

    // Threads 0..15 (one per 8x8 block, all inside simdgroup 0) prefix-scan the per-block
    // plane-payload byte costs into shared_plane_byte_offsets, and lane 15 records where the
    // sign bitstream starts.
    if (local_index < 16)
    {
        uint control_word = 0;
        uint q_bits = 0;

        if (extract_bits(ballot, local_index, 1) != 0)
        {
            uint local_code_offset = popcount(mask_lo(ballot, int(local_index)));
            control_word = uint(payload_u16[offset_u32 * 2 + 4 + local_code_offset]);
            q_bits = uint(payload_u8[offset_u32 * 4 + 8 + popcount(ballot) * 2 + local_code_offset]) & 0xfu;
        }

        uint lsbs = control_word & 0x5555u;
        uint msbs = control_word & 0xaaaau;
        uint msbs_shift = msbs >> 1;
        msbs |= msbs_shift;
        uint byte_cost = popcount(lsbs) + popcount(msbs) + q_bits * 8;

        uint byte_scan = offset_u32 * 4 + 8 + 3 * popcount(ballot) + simd_prefix_inclusive_sum(byte_cost);
        if (local_index == 15)
            shared_sign_offset = 8 * byte_scan;
        shared_plane_byte_offsets[local_index] = byte_scan - byte_cost;
    }

    threadgroup_barrier(mem_flags::mem_threadgroup);

    DecodedPair v;
    int significant_count;

    if (extract_bits(ballot, linear_block, 1) != 0)
    {
        uint local_code_offset = popcount(mask_lo(ballot, int(linear_block)));

        uint control_word = uint(payload_u16[offset_u32 * 2 + 4 + local_code_offset]);
        uint control_word2 = uint(payload_u8[offset_u32 * 4 + 8 + popcount(ballot) * 2 + local_code_offset]);

        v = decode_payload(payload_u8, control_word, control_word2 & 0xfu,
            shared_plane_byte_offsets[linear_block], block_local_index);

        significant_count = 0;
        for (int j = 0; j < 2; j++)
            for (int i = 0; i < 4; i++)
                significant_count += int(((j == 0) ? v.col0[i] : v.col1[i]) != 0.0f);

        float q = decode_quant(q_code);
        float inv_scale = q * decode_quant_scale(extract_bits(control_word2, uint(QUANT_SCALE_OFFSET - 16), uint(QUANT_SCALE_BITS)));

        v.col0 *= inv_scale;
        v.col1 *= inv_scale;
    }
    else
    {
        v.col0 = float4(0.0f);
        v.col1 = float4(0.0f);
        significant_count = 0;
    }

    // Cross-threadgroup scan of significant-coefficient counts → each thread's first sign-bit
    // position. Apple simdgroups are >= 16 wide, so this is the GLSL's `SubgroupSize <= 32`
    // branch; the shuffle/LDS fallbacks are unnecessary.
    int significant_scan = int(simd_prefix_inclusive_sum(uint(significant_count)));
    if (simd_lane == simd_size - 1)
        shared_sign_scan[simd_group] = uint(significant_scan);

    threadgroup_barrier(mem_flags::mem_threadgroup);
    uint num_simdgroups = (128 + simd_size - 1) / simd_size;
    if (local_index < num_simdgroups)
        shared_sign_scan[local_index] = simd_prefix_inclusive_sum(shared_sign_scan[local_index]);
    threadgroup_barrier(mem_flags::mem_threadgroup);

    uint sign_offset = shared_sign_offset + uint(significant_scan - significant_count);
    if (simd_group != 0)
        sign_offset += shared_sign_scan[simd_group - 1];

    // Load 64 bits of sign stream and bit-align (may read one word past the payload — the
    // buffer carries a 16-byte zeroed guard tail for exactly this).
    uint sign_word = payload_u32[sign_offset / 32 + 0];
    uint sign_word_upper = payload_u32[sign_offset / 32 + 1];

    uint masked_sign_offset = sign_offset & 31u;
    if (masked_sign_offset != 0)
    {
        sign_word >>= masked_sign_offset;
        sign_word |= sign_word_upper << (32 - masked_sign_offset);
    }

    int sign_counter = 0;

    for (int i = 0; i < 4; i++)
    {
        for (int j = 0; j < 2; j++)
        {
            float val = (j == 0) ? v.col0[i] : v.col1[i];
            if (val != 0.0f)
            {
                val *= 1.0f - 2.0f * float(extract_bits(sign_word, uint(sign_counter), 1));
                sign_counter++;
                if (j == 0) v.col0[i] = val; else v.col1[i] = val;
            }
        }
    }

    for (int j = 0; j < 2; j++)
        for (int i = 0; i < 4; i++)
            uDequantImg.write(float4((j == 0) ? v.col0[i] : v.col1[i]),
                              uint2(coord + int2(i, j)), uint(registers.output_layer));
}

// ---------------------------------------------------------------------------------------------
// idwt — inverse CDF 9/7; one 64-thread threadgroup reconstructs one 32x32 output tile from the
// four half-res band layers (LL/HL/LH/HH), with a 4-sample mirror apron. The caller passes the
// band-image resolution TRANSPOSED (the kernel transposes on load and store, so one kernel does
// both the horizontal and vertical passes).
// ---------------------------------------------------------------------------------------------

constant bool DCShift [[function_constant(0)]];

struct IdwtRegisters {
    int2 resolution;
    float2 inv_resolution;
};

constant int APRON = 4;
constant int APRON_HALF = APRON / 2;
constant int BLOCK_SIZE = 32;
constant int BLOCK_SIZE_HALF = BLOCK_SIZE >> 1;

// CDF 9/7 lifting constants (dwt_common.h).
constant float ALPHA = -1.586134342059924f;
constant float BETA = -0.052980118572961f;
constant float GAMMA = 0.882911075530934f;
constant float DELTA = 0.443506852043971f;
constant float K = 1.230174104914001f;
constant float inv_K = 1.0f / 1.230174104914001f;

constant int SHARED_ROWS = (BLOCK_SIZE + 2 * APRON) / 2;      // 20
constant int SHARED_COLS = (BLOCK_SIZE + 2 * APRON) + 1;      // 41 (+1 avoids bank conflicts)

static inline float2 load_shared(threadgroup half2 (&blk)[SHARED_ROWS][SHARED_COLS], int y, int x)
{
    return float2(blk[y][x]);
}

static inline void store_shared(threadgroup half2 (&blk)[SHARED_ROWS][SHARED_COLS], int y, int x, float2 v)
{
    blk[y][x] = half2(v);
}

// Even/odd-phase coordinate nudge so mirrored-repeat gather reproduces JPEG2000 whole-sample
// mirroring at the image borders, then transpose (uv.yx) on load.
static inline float2 generate_mirror_uv(int2 coord, bool even_x, bool even_y,
                                        int2 resolution, float2 inv_resolution)
{
    coord.x -= int(even_x && coord.x < 0);
    coord.y -= int(even_y && coord.y < 0);
    coord += 1;
    coord.x += int(!even_x && coord.x >= resolution.x);
    coord.y += int(!even_y && coord.y >= resolution.y);
    float2 uv = float2(coord) * inv_resolution;
    return uv.yx;
}

static inline void write_shared_4x4(threadgroup half2 (&blk)[SHARED_ROWS][SHARED_COLS],
                                    int2 coord, float4 t0, float4 t1, float4 t2, float4 t3)
{
    store_shared(blk, coord.y + 0, 2 * coord.x + 0, float2(t0.x, t2.x));
    store_shared(blk, coord.y + 0, 2 * coord.x + 1, float2(t1.x, t3.x));
    store_shared(blk, coord.y + 0, 2 * coord.x + 2, float2(t0.y, t2.y));
    store_shared(blk, coord.y + 0, 2 * coord.x + 3, float2(t1.y, t3.y));
    store_shared(blk, coord.y + 1, 2 * coord.x + 0, float2(t0.z, t2.z));
    store_shared(blk, coord.y + 1, 2 * coord.x + 1, float2(t1.z, t3.z));
    store_shared(blk, coord.y + 1, 2 * coord.x + 2, float2(t0.w, t2.w));
    store_shared(blk, coord.y + 1, 2 * coord.x + 3, float2(t1.w, t3.w));
}

// textureGather(...).wxzy — Metal's gather returns the same counter-clockwise-from-(i0,j1)
// component order as Vulkan, so the reorder is identical.
static inline float4 gather_layer(texture2d_array<float, access::sample> tex, sampler smp,
                                  float2 uv, uint layer)
{
    float4 g = tex.gather(smp, uv, layer);
    return float4(g.w, g.x, g.z, g.y);
}

static void load_image_with_apron(texture2d_array<float, access::sample> tex, sampler smp,
                                  threadgroup half2 (&blk)[SHARED_ROWS][SHARED_COLS],
                                  uint local_index, uint2 wg_id,
                                  int2 resolution, float2 inv_resolution)
{
    int2 base_coord = int2(wg_id) * BLOCK_SIZE_HALF - APRON_HALF;
    int2 local_coord0 = 2 * unswizzle8x8(local_index);
    int2 coord0 = base_coord + local_coord0;

    // Band layers gathered in 0/2/1/3 order (LL/LH/HL/HH interleave for the 2x2 scatter).
    float4 texels0 = gather_layer(tex, smp, generate_mirror_uv(coord0, true, true, resolution, inv_resolution), 0);
    float4 texels1 = gather_layer(tex, smp, generate_mirror_uv(coord0, false, true, resolution, inv_resolution), 2);
    float4 texels2 = gather_layer(tex, smp, generate_mirror_uv(coord0, true, false, resolution, inv_resolution), 1);
    float4 texels3 = gather_layer(tex, smp, generate_mirror_uv(coord0, false, false, resolution, inv_resolution), 3);
    write_shared_4x4(blk, local_coord0, texels0, texels1, texels2, texels3);

    int2 local_coord_horiz = int2(BLOCK_SIZE_HALF + 2 * int(local_index % 2u), 2 * int(local_index / 2u));
    if (local_coord_horiz.y < BLOCK_SIZE_HALF + 2 * APRON_HALF)
    {
        int2 c = base_coord + local_coord_horiz;
        texels0 = gather_layer(tex, smp, generate_mirror_uv(c, true, true, resolution, inv_resolution), 0);
        texels1 = gather_layer(tex, smp, generate_mirror_uv(c, false, true, resolution, inv_resolution), 2);
        texels2 = gather_layer(tex, smp, generate_mirror_uv(c, true, false, resolution, inv_resolution), 1);
        texels3 = gather_layer(tex, smp, generate_mirror_uv(c, false, false, resolution, inv_resolution), 3);
        write_shared_4x4(blk, local_coord_horiz, texels0, texels1, texels2, texels3);
    }

    int2 local_coord_vert = local_coord_horiz.yx;
    if (local_coord_vert.x < BLOCK_SIZE_HALF)
    {
        int2 c = base_coord + local_coord_vert;
        texels0 = gather_layer(tex, smp, generate_mirror_uv(c, true, true, resolution, inv_resolution), 0);
        texels1 = gather_layer(tex, smp, generate_mirror_uv(c, false, true, resolution, inv_resolution), 2);
        texels2 = gather_layer(tex, smp, generate_mirror_uv(c, true, false, resolution, inv_resolution), 1);
        texels3 = gather_layer(tex, smp, generate_mirror_uv(c, false, false, resolution, inv_resolution), 3);
        write_shared_4x4(blk, local_coord_vert, texels0, texels1, texels2, texels3);
    }

    threadgroup_barrier(mem_flags::mem_threadgroup);
}

static void inverse_transform8x2(threadgroup half2 (&blk)[SHARED_ROWS][SHARED_COLS], uint local_index)
{
    const int SIZE = 8;
    const int PADDED_SIZE = SIZE + 2 * APRON;
    const int PADDED_SIZE_HALF = PADDED_SIZE / 2;
    float2 values[PADDED_SIZE];

    int2 local_coord = int2(8 * int(local_index % 4u), int(local_index / 4u));

    for (int i = 0; i < PADDED_SIZE; i += 2)
    {
        float2 v0 = load_shared(blk, local_coord.y, local_coord.x + i + 0);
        float2 v1 = load_shared(blk, local_coord.y, local_coord.x + i + 1);
        values[i + 0] = v0 * K;
        values[i + 1] = v1 * inv_K;
    }

    // CDF 9/7 inverse lifting steps.
    for (int i = 2; i < PADDED_SIZE - 1; i += 2)
        values[i] -= DELTA * (values[i - 1] + values[i + 1]);
    for (int i = 3; i < PADDED_SIZE - 2; i += 2)
        values[i] -= GAMMA * (values[i - 1] + values[i + 1]);
    for (int i = 4; i < PADDED_SIZE - 3; i += 2)
        values[i] -= BETA * (values[i - 1] + values[i + 1]);
    for (int i = 5; i < PADDED_SIZE - 4; i += 2)
        values[i] -= ALPHA * (values[i - 1] + values[i + 1]);

    // Avoid WAR hazard.
    threadgroup_barrier(mem_flags::mem_threadgroup);

    for (int i = APRON_HALF; i < PADDED_SIZE_HALF - APRON_HALF; i++)
    {
        float2 a = values[2 * i + 0];
        float2 b = values[2 * i + 1];

        // Transpose the 2x2 block, transpose write.
        float2 t0 = float2(a.x, b.x);
        float2 t1 = float2(a.y, b.y);

        int y_coord = (local_coord.x >> 1) + (i - APRON_HALF);
        store_shared(blk, y_coord, 2 * local_coord.y + 0, t0);
        store_shared(blk, y_coord, 2 * local_coord.y + 1, t1);
    }
}

static void inverse_transform4x2(threadgroup half2 (&blk)[SHARED_ROWS][SHARED_COLS],
                                 uint local_index, bool active_lane, int y_offset)
{
    const int SIZE = 4;
    const int PADDED_SIZE = SIZE + 2 * APRON;
    const int PADDED_SIZE_HALF = PADDED_SIZE / 2;
    float2 values[PADDED_SIZE];

    int2 local_coord = int2(4 * int(local_index % 8u), int(local_index / 8u) + y_offset);

    if (active_lane)
    {
        for (int i = 0; i < PADDED_SIZE; i += 2)
        {
            float2 v0 = load_shared(blk, local_coord.y, local_coord.x + i + 0);
            float2 v1 = load_shared(blk, local_coord.y, local_coord.x + i + 1);
            values[i + 0] = v0 * K;
            values[i + 1] = v1 * inv_K;
        }

        for (int i = 2; i < PADDED_SIZE - 1; i += 2)
            values[i] -= DELTA * (values[i - 1] + values[i + 1]);
        for (int i = 3; i < PADDED_SIZE - 2; i += 2)
            values[i] -= GAMMA * (values[i - 1] + values[i + 1]);
        for (int i = 4; i < PADDED_SIZE - 3; i += 2)
            values[i] -= BETA * (values[i - 1] + values[i + 1]);
        for (int i = 5; i < PADDED_SIZE - 4; i += 2)
            values[i] -= ALPHA * (values[i - 1] + values[i + 1]);
    }

    threadgroup_barrier(mem_flags::mem_threadgroup);

    if (active_lane)
    {
        for (int i = APRON_HALF; i < PADDED_SIZE_HALF - APRON_HALF; i++)
        {
            float2 a = values[2 * i + 0];
            float2 b = values[2 * i + 1];

            float2 t0 = float2(a.x, b.x);
            float2 t1 = float2(a.y, b.y);

            int y_coord = (local_coord.x >> 1) + (i - APRON_HALF);
            store_shared(blk, y_coord, 2 * local_coord.y + 0, t0);
            store_shared(blk, y_coord, 2 * local_coord.y + 1, t1);
        }
    }
}

kernel void idwt(
    texture2d_array<float, access::sample> uTexture [[texture(0)]],
    texture2d<float, access::write> uOutput [[texture(1)]],
    sampler uSampler [[sampler(0)]],
    constant IdwtRegisters &registers [[buffer(0)]],
    uint3 wg_id [[threadgroup_position_in_grid]],
    uint local_index [[thread_index_in_threadgroup]])
{
    threadgroup half2 shared_block[SHARED_ROWS][SHARED_COLS];

    load_image_with_apron(uTexture, uSampler, shared_block, local_index, wg_id.xy,
                          registers.resolution, registers.inv_resolution);

    // Horizontal transform.
    inverse_transform8x2(shared_block, local_index);

    // Also need to transform the apron.
    inverse_transform4x2(shared_block, local_index, local_index < 32, BLOCK_SIZE_HALF);

    threadgroup_barrier(mem_flags::mem_threadgroup);

    // Vertical transform.
    inverse_transform8x2(shared_block, local_index);

    threadgroup_barrier(mem_flags::mem_threadgroup);

    int2 local_coord = unswizzle8x8(local_index);

    for (int y = local_coord.y; y < BLOCK_SIZE_HALF; y += 8)
    {
        for (int x = local_coord.x; x < BLOCK_SIZE; x += 8)
        {
            float2 v = load_shared(shared_block, y, x);
            if (DCShift)
                v += 0.5f;
            // Transposed store (wg_id.yx) — undoes the transpose-on-load; out-of-range writes
            // at the aligned-size overhang are dropped by Metal (matching the Vulkan behavior).
            int2 out0 = int2(2 * y + 0, x) + BLOCK_SIZE * int2(int(wg_id.y), int(wg_id.x));
            int2 out1 = int2(2 * y + 1, x) + BLOCK_SIZE * int2(int(wg_id.y), int(wg_id.x));
            uOutput.write(float4(v.x), uint2(out0));
            uOutput.write(float4(v.y), uint2(out1));
        }
    }
}
"""
