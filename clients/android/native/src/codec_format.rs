//! The codec-facing rules `decode` computes before it touches the NDK: the picture size and crop
//! a decoder reports. Plain values in and out; `MediaFormat` and `MediaCodec` are read and
//! written in `decode/`.

/// The picture size from a decoder's output format: the crop rect when the format carries one
/// (1080 rows decode into a 1088-row buffer), else `width` × `height`. The crop is one inclusive
/// rect, `(left, top, right, bottom)`, under `crop`: only Java's `MediaFormat` splits it into
/// `crop-left`…`crop-bottom`, so those keys never exist in the NDK's.
pub(crate) fn picture_dims(
    width: i32,
    height: i32,
    crop: Option<(i32, i32, i32, i32)>,
) -> (i32, i32) {
    match crop {
        Some((l, t, r, b)) if r >= l && b >= t => (r - l + 1, b - t + 1),
        _ => (width, height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_is_the_crop_when_the_format_has_one() {
        // (name, width, height, crop, size)
        let rows = [
            ("no crop is the buffer", 1920, 1080, None, (1920, 1080)),
            (
                "1080 rows in a 1088-row buffer",
                1920,
                1088,
                Some((0, 0, 1919, 1079)),
                (1920, 1080),
            ),
            (
                "an offset crop counts both edges",
                1936,
                1096,
                Some((8, 8, 1927, 1087)),
                (1920, 1080),
            ),
            (
                "one pixel is an inclusive rect",
                64,
                64,
                Some((5, 5, 5, 5)),
                (1, 1),
            ),
            (
                "a rect that ends before it starts",
                1920,
                1088,
                Some((10, 0, 9, 1079)),
                (1920, 1088),
            ),
            (
                "a rect that ends above its top",
                1920,
                1088,
                Some((0, 10, 1919, 9)),
                (1920, 1088),
            ),
        ];
        for (name, w, h, crop, want) in rows {
            assert_eq!(picture_dims(w, h, crop), want, "{name}");
        }
    }
}
