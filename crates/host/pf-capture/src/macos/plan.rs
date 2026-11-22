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
    /// The `CVPixelBuffer` format the stream is asked for.
    pub fourcc: u32,
    /// The layout the capturer delivers: the stream's own, or [`Self::lift`]'s.
    pub format: PixelFormat,
    /// HDR: BT.2100 PQ, composed for a canonical HDR10 display. Off: sRGB.
    pub pq: bool,
    /// HDR: the 10-bit 4:2:0 buffer the stream's packed RGB is converted into, its white
    /// moved by [`lift_white`].
    pub lift: Option<u32>,
    /// Ask for the BT.709 matrix. ScreenCaptureKit takes one for 8-bit 4:2:0 only.
    pub matrix_709: bool,
    /// The stream writes BT.601 and tags nothing: 10-bit SDR, measured on macOS 27.0. Each
    /// buffer is tagged with what it holds.
    pub tag_601: bool,
}

/// The buffer for `out`: 4:4:4 is `BGRA`, 10-bit and HDR the biplanar 10-bit 4:2:0, the rest
/// 8-bit 4:2:0, each 4:2:0 at the session's range. HDR asks the stream for packed 10-bit RGB
/// and converts it, because its white has to move first.
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
        fourcc: fourcc(if out.hdr { b"l10r" } else { code }),
        format,
        pq: out.hdr,
        lift: out.hdr.then(|| fourcc(code)),
        matrix_709: format == PixelFormat::Nv12,
        tag_601: format == PixelFormat::P010 && !out.hdr,
    })
}

/// Nits ScreenCaptureKit's PQ gives a unit of white: straight to 16 units, canonical or local
/// display, reference display or not (measured, macOS 27.2).
const CAPTURE_WHITE_NITS: f64 = 100.0;

/// Nits the stream gives it: BT.2408 reference white, where every client anchors SDR.
const STREAM_WHITE_NITS: f64 = 203.0;

/// PQ (SMPTE ST 2084): a signal in `0..=1` to nits.
fn pq_nits(signal: f64) -> f64 {
    let p = signal.max(0.0).powf(1.0 / PQ_M2);
    10_000.0 * ((p - PQ_C1).max(0.0) / (PQ_C2 - PQ_C3 * p)).powf(1.0 / PQ_M1)
}

/// PQ: nits to a signal in `0..=1`.
pub(super) fn pq_signal(nits: f64) -> f64 {
    let y = (nits / 10_000.0).clamp(0.0, 1.0).powf(PQ_M1);
    ((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y)).powf(PQ_M2)
}

const PQ_M1: f64 = 2610.0 / 16384.0;
const PQ_M2: f64 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f64 = 3424.0 / 4096.0;
const PQ_C2: f64 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f64 = 2392.0 / 4096.0 * 32.0;

/// The capture's 10-bit PQ code to the stream's 16-bit one: the same light, white moved from
/// [`CAPTURE_WHITE_NITS`] to [`STREAM_WHITE_NITS`]. It is a gain in linear light, so it applies
/// to each of R, G and B. Light past PQ's 10 000 nits stays at the top code.
pub(crate) fn lift_white() -> [u16; 1024] {
    let gain = STREAM_WHITE_NITS / CAPTURE_WHITE_NITS;
    std::array::from_fn(|code| {
        let nits = pq_nits(code as f64 / 1023.0) * gain;
        (pq_signal(nits) * 65535.0).round() as u16
    })
}

/// 10-bit codes from a normalized Y' and Cb/Cr, for the buffer `code` names:
/// `[luma scale, luma offset, chroma scale, chroma offset]`.
pub(crate) fn quantizer(code: u32) -> Option<[f32; 4]> {
    match &code.to_be_bytes() {
        b"x420" => Some([876.0, 64.0, 896.0, 512.0]),
        b"xf20" => Some([1023.0, 0.0, 1023.0, 512.0]),
        _ => None,
    }
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
            // An HDR stream is asked for packed RGB, which becomes the row's buffer.
            let want = Request {
                fourcc: fourcc(if pq { b"l10r" } else { code }),
                format,
                pq,
                lift: pq.then(|| fourcc(code)),
                matrix_709,
                tag_601,
            };
            assert_eq!(request(&o).unwrap(), want, "{name}");
            let buffer = want.lift.unwrap_or(want.fourcc);
            assert_eq!(delivered(buffer), Some(format), "{name} reads back");
            assert_eq!(quantizer(buffer).is_some(), format == P010, "{name}");
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

    /// White lands at 203 nits and every level keeps its ratio to it: 2x white is 406.
    #[test]
    fn the_lift_moves_white_to_the_stream_level() {
        let lut = lift_white();
        let out = |nits: f64| {
            let code = (pq_signal(nits) * 1023.0).round() as usize;
            (
                pq_nits(code as f64 / 1023.0),
                pq_nits(f64::from(lut[code]) / 65535.0),
            )
        };
        for nits in [1.0, 18.0, 100.0, 200.0, 400.0, 1000.0, 4000.0] {
            let (captured, lifted) = out(nits);
            assert!(
                (lifted / captured - 2.03).abs() < 0.005,
                "{nits}: {captured} -> {lifted}"
            );
        }
        assert!(
            (out(100.0).1 - 203.0).abs() < 1.0,
            "white: {}",
            out(100.0).1
        );
        assert_eq!((lut[0], lut[1023]), (0, 65535));
        assert!(lut.windows(2).all(|w| w[0] <= w[1]), "rises");
        // Past 10 000 nits there is no code: the top of the range stays the top.
        assert_eq!(lut[(pq_signal(6000.0) * 1023.0) as usize], 65535);
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
