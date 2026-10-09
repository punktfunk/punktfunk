include!(concat!(env!("OUT_DIR"), "/tables.rs"));

/// Add two GF(2^8) elements (XOR).
#[inline(always)]
pub fn add(a: u8, b: u8) -> u8 {
    a ^ b
}

/// Multiply two GF(2^8) elements using lookup table.
#[inline(always)]
pub fn mul(a: u8, b: u8) -> u8 {
    MUL_TABLE[a as usize][b as usize]
}

/// Divide a by b in GF(2^8). Panics if b is 0.
#[inline(always)]
pub fn div(a: u8, b: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    assert!(b != 0, "Division by zero in GF(2^8)");
    let log_a = LOG_TABLE[a as usize] as isize;
    let log_b = LOG_TABLE[b as usize] as isize;
    let mut log_result = log_a - log_b;
    if log_result < 0 {
        log_result += 255;
    }
    EXP_TABLE[log_result as usize]
}

/// Compute a^n in GF(2^8).
#[inline(always)]
pub fn exp(a: u8, n: usize) -> u8 {
    if n == 0 {
        return 1;
    }
    if a == 0 {
        return 0;
    }
    let log_a = LOG_TABLE[a as usize] as usize;
    let log_result = log_a * (n % 255) % 255;
    EXP_TABLE[log_result]
}

/// Multiply each element of `input` by `c` and write to `out`.
///
/// Uses SIMD acceleration when available:
/// - GFNI + AVX2 (best: single-instruction GF multiply on 32 bytes)
/// - AVX2 VPSHUFB (split-table nibble lookup on 32 bytes)
/// - GFNI + SSE (single-instruction GF multiply on 16 bytes)
/// - SSSE3 VPSHUFB (split-table nibble lookup on 16 bytes)
/// - Scalar fallback
#[inline]
pub fn mul_slice(c: u8, input: &[u8], out: &mut [u8]) {
    assert_eq!(input.len(), out.len());
    if input.is_empty() || c == 0 {
        out.iter_mut().for_each(|o| *o = 0);
        return;
    }
    if c == 1 {
        out.copy_from_slice(input);
        return;
    }

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("gfni") && is_x86_feature_detected!("avx2") {
            unsafe {
                mul_slice_gfni_avx2(c, input, out);
            }
            return;
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                mul_slice_avx2(c, input, out);
            }
            return;
        }
        if is_x86_feature_detected!("gfni") {
            unsafe {
                mul_slice_gfni_sse(c, input, out);
            }
            return;
        }
        if is_x86_feature_detected!("ssse3") {
            unsafe {
                mul_slice_ssse3(c, input, out);
            }
            return;
        }
    }

    mul_slice_scalar(c, input, out);
}

/// Multiply each element of `input` by `c` and XOR into `out`.
///
/// Uses SIMD acceleration when available (same priority as `mul_slice`).
#[inline]
pub fn mul_slice_xor(c: u8, input: &[u8], out: &mut [u8]) {
    assert_eq!(input.len(), out.len());
    if input.is_empty() || c == 0 {
        return;
    }
    if c == 1 {
        for (o, i) in out.iter_mut().zip(input.iter()) {
            *o ^= *i;
        }
        return;
    }

    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("gfni") && is_x86_feature_detected!("avx2") {
            unsafe {
                mul_slice_xor_gfni_avx2(c, input, out);
            }
            return;
        }
        if is_x86_feature_detected!("avx2") {
            unsafe {
                mul_slice_xor_avx2(c, input, out);
            }
            return;
        }
        if is_x86_feature_detected!("gfni") {
            unsafe {
                mul_slice_xor_gfni_sse(c, input, out);
            }
            return;
        }
        if is_x86_feature_detected!("ssse3") {
            unsafe {
                mul_slice_xor_ssse3(c, input, out);
            }
            return;
        }
    }

    mul_slice_xor_scalar(c, input, out);
}

/// Function pointer types for bulk GF(2^8) operations.
pub type MulSliceFn = fn(u8, &[u8], &mut [u8]);

/// Pair of (mul_slice, mul_slice_xor) function pointers for the best available SIMD path.
///
/// Unlike `mul_slice`/`mul_slice_xor`, these skip runtime feature detection on every call.
/// The caller checks once and stores the result.
///
/// Note: These raw dispatch functions do NOT handle the c==0 or c==1 special cases.
/// The caller must handle those before calling through the function pointer.
pub fn detect_mul_slice() -> (MulSliceFn, MulSliceFn) {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("gfni") && is_x86_feature_detected!("avx2") {
            return (
                wrap_mul_slice_gfni_avx2 as MulSliceFn,
                wrap_mul_slice_xor_gfni_avx2 as MulSliceFn,
            );
        }
        if is_x86_feature_detected!("avx2") {
            return (
                wrap_mul_slice_avx2 as MulSliceFn,
                wrap_mul_slice_xor_avx2 as MulSliceFn,
            );
        }
        if is_x86_feature_detected!("gfni") {
            return (
                wrap_mul_slice_gfni_sse as MulSliceFn,
                wrap_mul_slice_xor_gfni_sse as MulSliceFn,
            );
        }
        if is_x86_feature_detected!("ssse3") {
            return (
                wrap_mul_slice_ssse3 as MulSliceFn,
                wrap_mul_slice_xor_ssse3 as MulSliceFn,
            );
        }
    }
    (
        mul_slice_scalar as MulSliceFn,
        mul_slice_xor_scalar as MulSliceFn,
    )
}

// Safe wrappers for SIMD functions (used as function pointer targets)
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_gfni_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_gfni_avx2(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_xor_gfni_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_xor_gfni_avx2(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_avx2(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_xor_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_xor_avx2(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_gfni_sse(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_gfni_sse(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_xor_gfni_sse(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_xor_gfni_sse(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_ssse3(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_ssse3(c, input, out) }
}
#[cfg(target_arch = "x86_64")]
fn wrap_mul_slice_xor_ssse3(c: u8, input: &[u8], out: &mut [u8]) {
    // The unsafe callee bounds every load AND store on input.len(); unequal
    // lengths would write past `out`. Same invariant mul_slice/mul_slice_xor enforce.
    assert_eq!(input.len(), out.len());
    unsafe { mul_slice_xor_ssse3(c, input, out) }
}

// ── Scalar fallback ──────────────────────────────────────────────────────

fn mul_slice_scalar(c: u8, input: &[u8], out: &mut [u8]) {
    let mt = &MUL_TABLE[c as usize];
    for (o, &i) in out.iter_mut().zip(input.iter()) {
        *o = mt[i as usize];
    }
}

fn mul_slice_xor_scalar(c: u8, input: &[u8], out: &mut [u8]) {
    let mt = &MUL_TABLE[c as usize];
    for (o, &i) in out.iter_mut().zip(input.iter()) {
        *o ^= mt[i as usize];
    }
}

// ── x86_64 SIMD implementations ─────────────────────────────────────────

// ── GFNI + AVX2 (best path: 32 bytes per vgf2p8affineqb) ──────────────

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "gfni,avx2")]
unsafe fn mul_slice_gfni_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let matrix = GFNI_TABLE[c as usize] as i64;
    let mat_vec = _mm256_set1_epi64x(matrix);

    let len = input.len();
    let mut i = 0;

    while i + 32 <= len {
        let data = _mm256_loadu_si256(input.as_ptr().add(i) as *const _);
        let result = _mm256_gf2p8affine_epi64_epi8(data, mat_vec, 0);
        _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut _, result);
        i += 32;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) = mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "gfni,avx2")]
unsafe fn mul_slice_xor_gfni_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let matrix = GFNI_TABLE[c as usize] as i64;
    let mat_vec = _mm256_set1_epi64x(matrix);

    let len = input.len();
    let mut i = 0;

    while i + 32 <= len {
        let data = _mm256_loadu_si256(input.as_ptr().add(i) as *const _);
        let existing = _mm256_loadu_si256(out.as_ptr().add(i) as *const _);
        let mul_result = _mm256_gf2p8affine_epi64_epi8(data, mat_vec, 0);
        let result = _mm256_xor_si256(mul_result, existing);
        _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut _, result);
        i += 32;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) ^= mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

// ── GFNI + SSE (16 bytes per vgf2p8affineqb) ──────────────────────────

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "gfni")]
unsafe fn mul_slice_gfni_sse(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let matrix = GFNI_TABLE[c as usize] as i64;
    let mat_vec = _mm_set1_epi64x(matrix);

    let len = input.len();
    let mut i = 0;

    while i + 16 <= len {
        let data = _mm_loadu_si128(input.as_ptr().add(i) as *const _);
        let result = _mm_gf2p8affine_epi64_epi8(data, mat_vec, 0);
        _mm_storeu_si128(out.as_mut_ptr().add(i) as *mut _, result);
        i += 16;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) = mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "gfni")]
unsafe fn mul_slice_xor_gfni_sse(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let matrix = GFNI_TABLE[c as usize] as i64;
    let mat_vec = _mm_set1_epi64x(matrix);

    let len = input.len();
    let mut i = 0;

    while i + 16 <= len {
        let data = _mm_loadu_si128(input.as_ptr().add(i) as *const _);
        let existing = _mm_loadu_si128(out.as_ptr().add(i) as *const _);
        let mul_result = _mm_gf2p8affine_epi64_epi8(data, mat_vec, 0);
        let result = _mm_xor_si128(mul_result, existing);
        _mm_storeu_si128(out.as_mut_ptr().add(i) as *mut _, result);
        i += 16;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) ^= mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

// ── AVX2 VPSHUFB (32 bytes, split-table nibble lookup) ─────────────────

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mul_slice_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let low = &MUL_TABLE_LOW[c as usize];
    let high = &MUL_TABLE_HIGH[c as usize];

    // Broadcast the 16-byte low/high tables to 256-bit registers by duplicating
    let low_vec = _mm256_broadcastsi128_si256(_mm_loadu_si128(low.as_ptr() as *const _));
    let high_vec = _mm256_broadcastsi128_si256(_mm_loadu_si128(high.as_ptr() as *const _));
    let mask = _mm256_set1_epi8(0x0F);

    let len = input.len();
    let mut i = 0;

    // Process 32 bytes at a time
    while i + 32 <= len {
        let data = _mm256_loadu_si256(input.as_ptr().add(i) as *const _);
        let lo_nibble = _mm256_and_si256(data, mask);
        let hi_nibble = _mm256_and_si256(_mm256_srli_epi64(data, 4), mask);
        let lo_result = _mm256_shuffle_epi8(low_vec, lo_nibble);
        let hi_result = _mm256_shuffle_epi8(high_vec, hi_nibble);
        let result = _mm256_xor_si256(lo_result, hi_result);
        _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut _, result);
        i += 32;
    }

    // Handle remaining bytes with scalar
    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) = mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mul_slice_xor_avx2(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let low = &MUL_TABLE_LOW[c as usize];
    let high = &MUL_TABLE_HIGH[c as usize];

    let low_vec = _mm256_broadcastsi128_si256(_mm_loadu_si128(low.as_ptr() as *const _));
    let high_vec = _mm256_broadcastsi128_si256(_mm_loadu_si128(high.as_ptr() as *const _));
    let mask = _mm256_set1_epi8(0x0F);

    let len = input.len();
    let mut i = 0;

    while i + 32 <= len {
        let data = _mm256_loadu_si256(input.as_ptr().add(i) as *const _);
        let existing = _mm256_loadu_si256(out.as_ptr().add(i) as *const _);
        let lo_nibble = _mm256_and_si256(data, mask);
        let hi_nibble = _mm256_and_si256(_mm256_srli_epi64(data, 4), mask);
        let lo_result = _mm256_shuffle_epi8(low_vec, lo_nibble);
        let hi_result = _mm256_shuffle_epi8(high_vec, hi_nibble);
        let result = _mm256_xor_si256(_mm256_xor_si256(lo_result, hi_result), existing);
        _mm256_storeu_si256(out.as_mut_ptr().add(i) as *mut _, result);
        i += 32;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) ^= mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn mul_slice_ssse3(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let low = &MUL_TABLE_LOW[c as usize];
    let high = &MUL_TABLE_HIGH[c as usize];

    let low_vec = _mm_loadu_si128(low.as_ptr() as *const _);
    let high_vec = _mm_loadu_si128(high.as_ptr() as *const _);
    let mask = _mm_set1_epi8(0x0F);

    let len = input.len();
    let mut i = 0;

    while i + 16 <= len {
        let data = _mm_loadu_si128(input.as_ptr().add(i) as *const _);
        let lo_nibble = _mm_and_si128(data, mask);
        let hi_nibble = _mm_and_si128(_mm_srli_epi64(data, 4), mask);
        let lo_result = _mm_shuffle_epi8(low_vec, lo_nibble);
        let hi_result = _mm_shuffle_epi8(high_vec, hi_nibble);
        let result = _mm_xor_si128(lo_result, hi_result);
        _mm_storeu_si128(out.as_mut_ptr().add(i) as *mut _, result);
        i += 16;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) = mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn mul_slice_xor_ssse3(c: u8, input: &[u8], out: &mut [u8]) {
    use core::arch::x86_64::*;

    let low = &MUL_TABLE_LOW[c as usize];
    let high = &MUL_TABLE_HIGH[c as usize];

    let low_vec = _mm_loadu_si128(low.as_ptr() as *const _);
    let high_vec = _mm_loadu_si128(high.as_ptr() as *const _);
    let mask = _mm_set1_epi8(0x0F);

    let len = input.len();
    let mut i = 0;

    while i + 16 <= len {
        let data = _mm_loadu_si128(input.as_ptr().add(i) as *const _);
        let existing = _mm_loadu_si128(out.as_ptr().add(i) as *const _);
        let lo_nibble = _mm_and_si128(data, mask);
        let hi_nibble = _mm_and_si128(_mm_srli_epi64(data, 4), mask);
        let lo_result = _mm_shuffle_epi8(low_vec, lo_nibble);
        let hi_result = _mm_shuffle_epi8(high_vec, hi_nibble);
        let result = _mm_xor_si128(_mm_xor_si128(lo_result, hi_result), existing);
        _mm_storeu_si128(out.as_mut_ptr().add(i) as *mut _, result);
        i += 16;
    }

    let mt = &MUL_TABLE[c as usize];
    while i < len {
        *out.get_unchecked_mut(i) ^= mt[*input.get_unchecked(i) as usize];
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gfni_table() {
        // Verify GFNI_TABLE by emulating vgf2p8affineqb in software:
        //   result_bit[i] = popcount(x AND qword_byte[7-i]) mod 2
        for c in 0u16..256 {
            let matrix = GFNI_TABLE[c as usize];
            for b in 0u16..256 {
                let expected = MUL_TABLE[c as usize][b as usize];
                let x = b as u8;
                let mut result: u8 = 0;
                for i in 0..8u32 {
                    let row_byte = ((matrix >> ((7 - i) * 8)) & 0xFF) as u8;
                    let dot = (row_byte & x).count_ones() % 2;
                    result |= (dot as u8) << i;
                }
                assert_eq!(
                    result, expected,
                    "GFNI table mismatch: c={c}, b={b}, got={result}, expected={expected}"
                );
            }
        }
    }

    #[test]
    fn test_add() {
        assert_eq!(add(0, 0), 0);
        assert_eq!(add(1, 0), 1);
        assert_eq!(add(0, 1), 1);
        assert_eq!(add(1, 1), 0);
        assert_eq!(add(0xFF, 0xFF), 0);
        assert_eq!(add(0xAA, 0x55), 0xFF);
    }

    #[test]
    fn test_mul() {
        assert_eq!(mul(0, 0), 0);
        assert_eq!(mul(1, 0), 0);
        assert_eq!(mul(0, 1), 0);
        assert_eq!(mul(1, 1), 1);
        // a * 1 = a
        for a in 0u8..=255 {
            assert_eq!(mul(a, 1), a);
            assert_eq!(mul(1, a), a);
        }
        // a * 0 = 0
        for a in 0u8..=255 {
            assert_eq!(mul(a, 0), 0);
        }
    }

    #[test]
    fn test_div() {
        // a / 1 = a
        for a in 0u8..=255 {
            assert_eq!(div(a, 1), a);
        }
        // a / a = 1 (for a != 0)
        for a in 1u8..=255 {
            assert_eq!(div(a, a), 1);
        }
        // (a * b) / b = a
        for a in 1u8..=255 {
            for b in 1u8..=255 {
                assert_eq!(div(mul(a, b), b), a);
            }
        }
    }

    #[test]
    fn test_exp() {
        assert_eq!(exp(0, 0), 1);
        assert_eq!(exp(1, 0), 1);
        assert_eq!(exp(5, 0), 1);
        assert_eq!(exp(0, 1), 0);
        assert_eq!(exp(0, 100), 0);
        // a^1 = a
        for a in 0u8..=255 {
            assert_eq!(exp(a, 1), a);
        }
        // a^2 = a * a
        for a in 0u8..=255 {
            assert_eq!(exp(a, 2), mul(a, a));
        }
    }

    #[test]
    fn test_mul_slice_basic() {
        let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut out = [0u8; 8];
        mul_slice(3, &input, &mut out);
        for i in 0..input.len() {
            assert_eq!(out[i], mul(3, input[i]));
        }
    }

    #[test]
    fn test_mul_slice_xor_basic() {
        let input = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut out = [10u8; 8];
        let original = out;
        mul_slice_xor(3, &input, &mut out);
        for i in 0..input.len() {
            assert_eq!(out[i], original[i] ^ mul(3, input[i]));
        }
    }

    #[test]
    fn test_mul_slice_large() {
        // Test with a buffer large enough to exercise SIMD paths
        let input: Vec<u8> = (0..256).map(|i| i as u8).collect();
        let mut out = vec![0u8; 256];
        let mut expected = vec![0u8; 256];

        for c in [2u8, 7, 42, 128, 255] {
            mul_slice_scalar(c, &input, &mut expected);
            mul_slice(c, &input, &mut out);
            assert_eq!(out, expected, "mul_slice mismatch for c={c}");
        }
    }

    #[test]
    fn test_mul_slice_xor_large() {
        let input: Vec<u8> = (0..256).map(|i| i as u8).collect();

        for c in [2u8, 7, 42, 128, 255] {
            let mut out_expected = vec![0xABu8; 256];
            let mut out_simd = out_expected.clone();
            mul_slice_xor_scalar(c, &input, &mut out_expected);
            mul_slice_xor(c, &input, &mut out_simd);
            assert_eq!(out_simd, out_expected, "mul_slice_xor mismatch for c={c}");
        }
    }

    #[test]
    fn test_mul_slice_unaligned_sizes() {
        // Test sizes that don't align to SIMD width
        for size in [1, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 100] {
            let input: Vec<u8> = (0..size).map(|i| i as u8).collect();
            let mut out = vec![0u8; size];
            let mut expected = vec![0u8; size];

            mul_slice_scalar(42, &input, &mut expected);
            mul_slice(42, &input, &mut out);
            assert_eq!(out, expected, "mul_slice mismatch for size={size}");
        }
    }
}
