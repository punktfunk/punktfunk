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
