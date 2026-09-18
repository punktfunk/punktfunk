//! Per-frame H.273 (`ColorDesc`) and the Y′CbCr→RGB shader rows (`csc_rows`).
//!
//! `ColorDesc` is this frame's bitstream VUI / sequence header, not the
//! session handshake. The host can switch PQ in-band; drawing that frame as
//! BT.709 limited washes it out. H.273 2 is unspecified; [`csc_rows`] then
//! uses BT.709.
//!
//! [`csc_rows`] is the shared coefficient table. Tests in this file pin
//! limited-range white/black (8-bit and 10-bit P010) and the 601-vs-709 red
//! excursion.

/// Per-frame H.273. Follow this, not the session handshake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ColorDesc {
    /// H.273; 2 = unspecified.
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub full_range: bool,
}

impl ColorDesc {
    /// H.273 transfer 16 (PQ / ST.2084).
    pub fn is_pq(&self) -> bool {
        self.transfer == 16
    }
}

/// Shader rows: `rgb[i] = dot(r[i].xyz, yuv) + r[i].w`.
///
/// `depth` is the limited-range ladder: 8-bit 16/235/240 over 255, 10-bit
/// 64/940/960 over 1023. Those are not the same normalized values (~½ code).
/// `msb_packed` is P010/X6 (10 bits in the MSBs of 16): a UNORM16 sample is
/// `code·64/65535`; multiply by `65535/65472` to recover `code/1023`.
/// Neutral chroma is code 128 (512 at 10 bits) in both ranges, not 0.5.
pub fn csc_rows(desc: ColorDesc, depth: u8, msb_packed: bool) -> [[f32; 4]; 3] {
    // H.273 5/6 = BT.601, 9/10 = BT.2020; unspecified and the rest are BT.709.
    let (kr, kb) = match desc.matrix {
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    let max = f64::from((1u32 << depth) - 1); // 255 / 1023
    let step = f64::from(1u32 << (depth - 8)); // code points per 8-bit step: 1 / 4
    let pack = if msb_packed { 65535.0 / 65472.0 } else { 1.0 };
    let (sy, oy, sc) = if desc.full_range {
        (pack, 0.0f64, pack)
    } else {
        (
            pack * max / (219.0 * step),
            -(16.0 * step) / max,
            pack * max / (224.0 * step),
        )
    };
    // Fold M*(yuv+off) into w. Sampled `yuv` is already packed, so off / pack.
    let oc = -(128.0 * step) / max;
    let off = [oy / pack, oc / pack, oc / pack];
    let m = [
        [sy, 0.0, 2.0 * (1.0 - kr) * sc],
        [
            sy,
            -2.0 * (1.0 - kb) * kb / kg * sc,
            -2.0 * (1.0 - kr) * kr / kg * sc,
        ],
        [sy, 2.0 * (1.0 - kb) * sc, 0.0],
    ];
    core::array::from_fn(|r| {
        let w: f64 = (0..3).map(|c| m[r][c] * off[c]).sum();
        [m[r][0] as f32, m[r][1] as f32, m[r][2] as f32, w as f32]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(matrix: u8, full_range: bool) -> ColorDesc {
        ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix,
            full_range,
        }
    }

    fn apply(rows: &[[f32; 4]; 3], yuv: [f32; 3]) -> [f32; 3] {
        core::array::from_fn(|r| {
            rows[r][0] * yuv[0] + rows[r][1] * yuv[1] + rows[r][2] * yuv[2] + rows[r][3]
        })
    }

    #[test]
    fn bt2020_10bit_limited_white_black() {
        let rows = csc_rows(desc(9, false), 10, true);
        let s = |code: u32| ((code << 6) as f32) / 65535.0;
        let white = apply(&rows, [s(940), s(512), s(512)]);
        let black = apply(&rows, [s(64), s(512), s(512)]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 1e-4, "white {white:?}");
            assert!(b.abs() < 1e-4, "black {black:?}");
        }
    }

    #[test]
    fn bt709_limited_white_black() {
        let rows = csc_rows(desc(1, false), 8, false);
        let white = apply(&rows, [235.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        let black = apply(&rows, [16.0 / 255.0, 128.0 / 255.0, 128.0 / 255.0]);
        for (w, b) in white.iter().zip(black) {
            assert!((w - 1.0).abs() < 1e-4, "white {white:?}");
            assert!(b.abs() < 1e-4, "black {black:?}");
        }
    }

    #[test]
    fn full_range_and_red_excursion() {
        let rows = csc_rows(desc(5, true), 8, false);
        let c0 = 128.0 / 255.0;
        let white = apply(&rows, [1.0, c0, c0]);
        assert!(white.iter().all(|v| (v - 1.0).abs() < 1e-5), "{white:?}");
        let red = apply(&rows, [0.0, c0, 1.0]);
        assert!(
            (red[0] - 2.0 * (1.0 - 0.299) * (1.0 - c0)).abs() < 1e-4,
            "{red:?}"
        );
        let rows709 = csc_rows(desc(1, true), 8, false);
        let red709 = apply(&rows709, [0.0, c0, 1.0]);
        assert!(
            (red709[0] - 2.0 * (1.0 - 0.2126) * (1.0 - c0)).abs() < 1e-4,
            "{red709:?}"
        );
        assert!((red[0] - red709[0]).abs() > 0.05);
    }

    /// RGB → the hosts' BT.709 limited 8-bit CSC (`rgb2yuv.comp`, rounded to codes) → rows.
    /// Greys come back neutral and black exactly black; primaries within quantization.
    #[test]
    fn host_bt709_limited_round_trip() {
        let rows = csc_rows(desc(1, false), 8, false);
        let code = |v: f64| (v * 255.0).round().clamp(0.0, 255.0) / 255.0;
        for rgb in [
            [0.0f64, 0.0, 0.0],
            [0.18, 0.18, 0.18],
            [0.5, 0.5, 0.5],
            [1.0, 1.0, 1.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
            [1.0, 0.0, 1.0],
            [1.0, 1.0, 0.0],
        ] {
            let [r, g, b] = rgb;
            let y = code(16.0 / 255.0 + 0.1826 * r + 0.6142 * g + 0.0620 * b);
            let cb = code(128.0 / 255.0 - 0.1006 * r - 0.3386 * g + 0.4392 * b);
            let cr = code(128.0 / 255.0 + 0.4392 * r - 0.3989 * g - 0.0403 * b);
            let out = apply(&rows, [y as f32, cb as f32, cr as f32]);
            let grey = r == g && g == b;
            for c in 0..3 {
                let err = (f64::from(out[c].clamp(0.0, 1.0)) - rgb[c]) * 255.0;
                // Greys land on one code; saturated colours carry the 8-bit rounding.
                let tol = if grey { 0.6 } else { 2.0 };
                assert!(err.abs() <= tol, "{rgb:?} -> {out:?} ({err:+.2} codes)");
            }
            if grey {
                let spread = (out[0] - out[1]).abs().max((out[2] - out[1]).abs()) * 255.0;
                assert!(
                    spread < 0.05,
                    "{rgb:?} tinted by {spread:.2} codes: {out:?}"
                );
            }
        }
    }

    /// Same coefficients as `video_gl::yuv_to_rgb`, column-major packing.
    #[test]
    fn rows_match_the_gl_matrix_form() {
        for (matrix, full) in [(1u8, false), (1, true), (5, false), (9, false), (9, true)] {
            let d = desc(matrix, full);
            let rows = csc_rows(d, 8, false);
            // Independent of `csc_rows`: GL's column-major apply, for the packing check.
            let (kr, kb) = match matrix {
                5 | 6 => (0.299f32, 0.114f32),
                9 | 10 => (0.2627, 0.0593),
                _ => (0.2126, 0.0722),
            };
            let kg = 1.0 - kr - kb;
            let (sy, oy, sc) = if full {
                (1.0f32, 0.0f32, 1.0f32)
            } else {
                (255.0 / 219.0, -16.0 / 255.0, 255.0 / 224.0)
            };
            let mat = [
                sy,
                sy,
                sy,
                0.0,
                -2.0 * (1.0 - kb) * kb / kg * sc,
                2.0 * (1.0 - kb) * sc,
                2.0 * (1.0 - kr) * sc,
                -2.0 * (1.0 - kr) * kr / kg * sc,
                0.0,
            ];
            let off = [oy, -128.0 / 255.0, -128.0 / 255.0];
            for yuv in [
                [0.1f32, 0.3, 0.7],
                [0.9, 0.5, 0.5],
                [0.5, 0.2, 0.8],
                [16.0 / 255.0, 0.5, 0.5],
            ] {
                let v = [yuv[0] + off[0], yuv[1] + off[1], yuv[2] + off[2]];
                let gl: [f32; 3] =
                    core::array::from_fn(|r| (0..3).map(|c| mat[c * 3 + r] * v[c]).sum());
                let ours = apply(&rows, yuv);
                for (a, b) in gl.iter().zip(ours) {
                    assert!(
                        (a - b).abs() < 1e-5,
                        "{matrix}/{full}: gl {gl:?} rows {ours:?}"
                    );
                }
            }
        }
    }
}
