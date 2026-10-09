//! The Raspberry Pi HEVC decoder's column format (`NV12_COL128`) into tight
//! planar 4:2:0.
//!
//! The picture is cut into columns 128 pixels wide. Each column is stored
//! whole: its luma rows, then its interleaved chroma rows, 128 bytes per row.
//! `column_height` is the rows one column occupies (the format's
//! `bytesperline`), so column `c` starts at byte `c * 128 * column_height`,
//! and its chroma starts `coded_height` rows into it.
//!
//! No Vulkan driver imports this layout, so the rung copies it out. It is a
//! rearrangement, not a conversion: every sample is moved once.

/// Pixels per column.
pub const COLUMN: usize = 128;

/// Why a buffer cannot be unpacked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandError {
    /// The visible picture is larger than the coded one, or empty.
    Geometry,
    /// The buffer ends before the last sample the geometry names.
    Short { needed: usize, got: usize },
}

impl std::fmt::Display for SandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandError::Geometry => write!(f, "the visible picture does not fit the coded one"),
            SandError::Short { needed, got } => {
                write!(
                    f,
                    "the picture buffer holds {got} bytes, its geometry needs {needed}"
                )
            }
        }
    }
}

impl std::error::Error for SandError {}

/// Tightly packed Y, Cb, Cr planes of the visible picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planar {
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
}

/// Unpack the visible `width` x `height` of a column-format buffer.
/// `coded_height` and `column_height` are the format's `height` and
/// `bytesperline`.
pub fn unpack(
    src: &[u8],
    width: usize,
    height: usize,
    coded_height: usize,
    column_height: usize,
) -> Result<Planar, SandError> {
    let chroma_rows = height.div_ceil(2);
    if width == 0 || height == 0 || height > coded_height {
        return Err(SandError::Geometry);
    }
    if coded_height + chroma_rows > column_height {
        return Err(SandError::Geometry);
    }
    let column_bytes = COLUMN * column_height;
    let columns = width.div_ceil(COLUMN);
    let needed = (columns - 1) * column_bytes + (coded_height + chroma_rows) * COLUMN;
    if src.len() < needed {
        return Err(SandError::Short {
            needed,
            got: src.len(),
        });
    }

    let chroma_width = width.div_ceil(2);
    let mut y = vec![0u8; width * height];
    let mut cb = vec![0u8; chroma_width * chroma_rows];
    let mut cr = vec![0u8; chroma_width * chroma_rows];
    for column in 0..columns {
        let base = column * column_bytes;
        let x0 = column * COLUMN;
        let luma = COLUMN.min(width - x0);
        for row in 0..height {
            let from = base + row * COLUMN;
            y[row * width + x0..][..luma].copy_from_slice(&src[from..from + luma]);
        }
        // 64 Cb/Cr pairs per 128-byte row.
        let pairs = (COLUMN / 2).min(chroma_width - x0 / 2);
        let chroma = base + coded_height * COLUMN;
        for row in 0..chroma_rows {
            let line = &src[chroma + row * COLUMN..][..pairs * 2];
            let at = row * chroma_width + x0 / 2;
            for (i, pair) in line.chunks_exact(2).enumerate() {
                cb[at + i] = pair[0];
                cr[at + i] = pair[1];
            }
        }
    }
    Ok(Planar { y, cb, cr })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A coded buffer where every sample encodes its own coordinates.
    fn pack(coded_width: usize, coded_height: usize, column_height: usize) -> Vec<u8> {
        let columns = coded_width / COLUMN;
        let mut buf = vec![0xEE; columns * COLUMN * column_height];
        for column in 0..columns {
            let base = column * COLUMN * column_height;
            for row in 0..coded_height {
                for i in 0..COLUMN {
                    buf[base + row * COLUMN + i] = luma(column * COLUMN + i, row);
                }
            }
            for row in 0..coded_height / 2 {
                for pair in 0..COLUMN / 2 {
                    let at = base + (coded_height + row) * COLUMN + pair * 2;
                    let x = column * COLUMN / 2 + pair;
                    buf[at] = cb(x, row);
                    buf[at + 1] = cr(x, row);
                }
            }
        }
        buf
    }

    fn luma(x: usize, y: usize) -> u8 {
        (x * 7 + y * 13) as u8
    }
    fn cb(x: usize, y: usize) -> u8 {
        (x * 3 + y * 5 + 1) as u8
    }
    fn cr(x: usize, y: usize) -> u8 {
        (x * 11 + y * 17 + 2) as u8
    }

    #[test]
    fn every_sample_lands_at_its_own_coordinates() {
        // 1080p as the driver lays it out: 15 columns, rows padded to 1088,
        // a column as tall as luma plus chroma.
        let (w, h, coded_w, coded_h) = (1920, 1080, 1920, 1088);
        let column_height = coded_h * 3 / 2;
        let src = pack(coded_w, coded_h, column_height);
        let out = unpack(&src, w, h, coded_h, column_height).unwrap();
        for (x, y) in [(0, 0), (127, 0), (128, 0), (1919, 1079), (640, 333)] {
            assert_eq!(out.y[y * w + x], luma(x, y), "luma ({x}, {y})");
        }
        for (x, y) in [(0, 0), (63, 0), (64, 0), (959, 539), (320, 100)] {
            assert_eq!(out.cb[y * (w / 2) + x], cb(x, y), "cb ({x}, {y})");
            assert_eq!(out.cr[y * (w / 2) + x], cr(x, y), "cr ({x}, {y})");
        }
        assert_eq!(out.y.len(), w * h);
        assert_eq!(out.cb.len(), (w / 2) * (h / 2));
    }

    #[test]
    fn a_width_that_ends_inside_a_column_is_cropped() {
        let (w, h, coded_h) = (200, 64, 64);
        let column_height = 96;
        let src = pack(256, coded_h, column_height);
        let out = unpack(&src, w, h, coded_h, column_height).unwrap();
        assert_eq!(out.y[199], luma(199, 0));
        assert_eq!(out.y[63 * w + 199], luma(199, 63));
        assert_eq!(out.cb[31 * 100 + 99], cb(99, 31));
        assert_eq!(out.y.len(), w * h);
    }

    #[test]
    fn a_buffer_too_short_for_its_geometry_is_refused() {
        let src = vec![0u8; 1000];
        assert!(matches!(
            unpack(&src, 1920, 1080, 1088, 1632),
            Err(SandError::Short { .. })
        ));
        assert_eq!(unpack(&src, 0, 0, 0, 0), Err(SandError::Geometry));
        assert_eq!(unpack(&src, 64, 64, 32, 96), Err(SandError::Geometry));
    }
}
