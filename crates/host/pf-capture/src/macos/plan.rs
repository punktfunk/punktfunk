//! The ScreenCaptureKit capture's choices that call no OS: the buffer a session asks for, where
//! the display lands in the frame, and where the pointer lands. Pinned by the tests below, which
//! run on every target.

use pf_frame::{OutputFormat, PixelFormat};

/// A Core Video pixel format, as its four characters.
const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*code)
}

/// What a session asks ScreenCaptureKit for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The `CVPixelBuffer` format.
    pub fourcc: u32,
    pub format: PixelFormat,
    /// HDR: BT.2100 PQ, composed for a canonical HDR10 display. Off: sRGB.
    pub pq: bool,
    /// Ask for the BT.709 matrix. ScreenCaptureKit takes one for 8-bit 4:2:0 only.
    pub matrix_709: bool,
    /// The stream writes BT.601 and tags nothing: 10-bit SDR, measured on macOS 27.0. Each
    /// buffer is tagged with what it holds.
    pub tag_601: bool,
}

/// The buffer for `out`: 4:4:4 is `BGRA`, 10-bit and HDR the biplanar 10-bit 4:2:0, the rest
/// 8-bit 4:2:0, each 4:2:0 at the session's range.
pub(crate) fn request(out: &OutputFormat) -> anyhow::Result<Request> {
    let ten_bit = out.hdr || out.ten_bit_sdr;
    if out.chroma_444 && ten_bit {
        anyhow::bail!("10-bit 4:4:4 capture is not built on macOS");
    }
    let (code, format) = match (out.chroma_444, ten_bit, out.full_range) {
        (true, _, _) => (b"BGRA", PixelFormat::Bgra),
        (false, true, false) => (b"x420", PixelFormat::P010),
        (false, true, true) => (b"xf20", PixelFormat::P010),
        (false, false, false) => (b"420v", PixelFormat::Nv12),
        (false, false, true) => (b"420f", PixelFormat::Nv12),
    };
    Ok(Request {
        fourcc: fourcc(code),
        format,
        pq: out.hdr,
        matrix_709: format == PixelFormat::Nv12,
        tag_601: format == PixelFormat::P010 && !out.hdr,
    })
}

/// The layout a delivered buffer holds, read from its own format type.
pub(crate) fn delivered(code: u32) -> Option<PixelFormat> {
    match &code.to_be_bytes() {
        b"420v" | b"420f" => Some(PixelFormat::Nv12),
        b"x420" | b"xf20" => Some(PixelFormat::P010),
        b"BGRA" => Some(PixelFormat::Bgra),
        _ => None,
    }
}

/// A rectangle, `x, y, w, h`.
pub(crate) type Rect = [f64; 4];

/// Where a display of `display` points lands in a `frame`-pixel buffer: scaled whole and
/// centred, with bars on the other axis. ScreenCaptureKit draws it there under
/// `preservesAspectRatio`.
pub(crate) fn fit(display: (f64, f64), frame: (u32, u32)) -> Rect {
    let (fw, fh) = (f64::from(frame.0), f64::from(frame.1));
    let scale = (fw / display.0).min(fh / display.1);
    let (w, h) = (display.0 * scale, display.1 * scale);
    [(fw - w) / 2.0, (fh - h) / 2.0, w, h]
}

/// A global pointer position in points, in frame pixels: the display at `bounds` (global
/// points) is drawn into `content`. `None` while the pointer is on another display.
pub(crate) fn to_frame(at: (f64, f64), bounds: Rect, content: Rect) -> Option<(f64, f64)> {
    let (dx, dy) = (at.0 - bounds[0], at.1 - bounds[1]);
    if !(0.0..bounds[2]).contains(&dx) || !(0.0..bounds[3]).contains(&dy) {
        return None;
    }
    let scale = content[2] / bounds[2];
    Some((content[0] + dx * scale, content[1] + dy * scale))
}

/// Premultiplied RGBA to the straight alpha a [`pf_frame::CursorOverlay`] carries.
pub(crate) fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = u16::from(px[3]);
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = (u16::from(*c) * 255 / a).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(hdr: bool, ten_bit_sdr: bool, chroma_444: bool, full_range: bool) -> OutputFormat {
        OutputFormat {
            hdr,
            ten_bit_sdr,
            chroma_444,
            full_range,
            ..OutputFormat::resolve(false, true)
        }
    }

    #[test]
    fn each_session_format_asks_for_its_buffer() {
        use PixelFormat::*;
        #[rustfmt::skip]
        let rows = [
            ("sdr studio",  out(false, false, false, false), b"420v", Nv12, false, true,  false),
            ("sdr full",    out(false, false, false, true),  b"420f", Nv12, false, true,  false),
            ("10-bit sdr",  out(false, true,  false, false), b"x420", P010, false, false, true),
            ("hdr studio",  out(true,  false, false, false), b"x420", P010, true,  false, false),
            ("hdr full",    out(true,  false, false, true),  b"xf20", P010, true,  false, false),
            ("4:4:4",       out(false, false, true,  false), b"BGRA", Bgra, false, false, false),
            ("4:4:4 full",  out(false, false, true,  true),  b"BGRA", Bgra, false, false, false),
        ];
        for (name, o, code, format, pq, matrix_709, tag_601) in rows {
            let want = Request {
                fourcc: fourcc(code),
                format,
                pq,
                matrix_709,
                tag_601,
            };
            assert_eq!(request(&o).unwrap(), want, "{name}");
            assert_eq!(delivered(want.fourcc), Some(format), "{name} reads back");
        }
        assert!(
            request(&out(true, false, true, false)).is_err(),
            "hdr 4:4:4"
        );
        assert!(
            request(&out(false, true, true, true)).is_err(),
            "10-bit 4:4:4"
        );
        assert_eq!(delivered(fourcc(b"l10r")), None);
    }

    #[test]
    fn the_display_fits_whole_and_centred() {
        assert_eq!(
            fit((5120.0, 1440.0), (1920, 1080)),
            [0.0, 270.0, 1920.0, 540.0]
        );
        assert_eq!(
            fit((1512.0, 982.0), (3024, 1964)),
            [0.0, 0.0, 3024.0, 1964.0]
        );
        assert_eq!(
            fit((1440.0, 900.0), (1920, 600)),
            [480.0, 0.0, 960.0, 600.0]
        );
    }

    #[test]
    fn the_pointer_lands_in_the_content_or_nowhere() {
        let bounds = [-1440.0, 0.0, 1440.0, 900.0];
        let content = fit((1440.0, 900.0), (1920, 600));
        assert_eq!(
            to_frame((-1440.0, 0.0), bounds, content),
            Some((480.0, 0.0))
        );
        assert_eq!(
            to_frame((-720.0, 450.0), bounds, content),
            Some((960.0, 300.0))
        );
        assert_eq!(
            to_frame((0.0, 450.0), bounds, content),
            None,
            "on the next display"
        );
        assert_eq!(to_frame((-720.0, -1.0), bounds, content), None);
    }

    #[test]
    fn unpremultiply_restores_straight_alpha() {
        let mut px = [128, 64, 0, 128, 10, 20, 30, 255, 9, 9, 9, 0];
        unpremultiply(&mut px);
        assert_eq!(px, [255, 127, 0, 128, 10, 20, 30, 255, 9, 9, 9, 0]);
    }
}
