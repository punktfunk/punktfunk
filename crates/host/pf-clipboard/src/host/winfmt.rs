//! Win32 clipboard format ↔ portable wire MIME conversions
//! (`design/clipboard-and-file-transfer.md` §3.5). No `windows` crate, so the
//! conversions compile and unit-test on every host. [`super::windows`] is the
//! only production caller (`GetClipboardData` / `SetClipboardData`).
//!
//! * `CF_UNICODETEXT` (UTF-16LE + NUL) ↔ `text/plain;charset=utf-8`
//! * `"HTML Format"` (CF_HTML, UTF-8 + ASCII header) ↔ `text/html`
//! * `"Rich Text Format"` (raw RTF) ↔ `text/rtf`
//! * `"PNG"` (raw PNG) ↔ `image/png` — identity, handled by the backend.

/// `CF_UNICODETEXT` HGLOBAL → UTF-8. `raw` is the `GlobalSize` buffer: little-endian
/// UTF-16. The first `0x0000` ends the string; `GlobalSize` can exceed that terminator.
pub fn text_from_utf16(raw: &[u8]) -> Vec<u8> {
    // Odd trailing byte (invalid CF_UNICODETEXT) is dropped by `chunks_exact`.
    let units: Vec<u16> = raw
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    // CF_UNICODETEXT is a C string. Padding or garbage past the first NUL is not payload.
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end]).into_bytes()
}

/// UTF-8 → `CF_UNICODETEXT` (UTF-16LE + required `0x0000` terminator).
pub fn text_to_utf16(wire: &[u8]) -> Vec<u8> {
    let s = String::from_utf8_lossy(wire);
    let mut out = Vec::with_capacity(wire.len() * 2 + 2);
    for u in s.encode_utf16() {
        out.extend_from_slice(&u.to_le_bytes());
    }
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

// CF_HTML: ASCII `Key:Value\r\n` offsets, then HTML. Offsets are bytes from
// buffer start and live *inside* the header, so digit width changes the
// length. Fixed 10-digit zero-pad (Chromium/Firefox/LibreOffice) makes the
// header a compile-time constant.

const CF_HTML_HEADER: &str = "Version:0.9\r\n\
    StartHTML:0000000000\r\n\
    EndHTML:0000000000\r\n\
    StartFragment:0000000000\r\n\
    EndFragment:0000000000\r\n";
const CF_HTML_PREFIX: &str = "<html><body>\r\n<!--StartFragment-->";
const CF_HTML_SUFFIX: &str = "<!--EndFragment-->\r\n</body></html>";

/// UTF-8 fragment → `CF_HTML` + trailing NUL. `EndHTML` is the content end,
/// before that NUL (CF_HTML §4).
pub fn html_to_cf(wire: &[u8]) -> Vec<u8> {
    let fragment = String::from_utf8_lossy(wire);
    let start_html = CF_HTML_HEADER.len();
    let start_fragment = start_html + CF_HTML_PREFIX.len();
    let end_fragment = start_fragment + fragment.len(); // UTF-8 bytes, not chars
    let end_html = end_fragment + CF_HTML_SUFFIX.len();

    let mut buf = Vec::with_capacity(end_html + 1);
    buf.extend_from_slice(CF_HTML_HEADER.as_bytes());
    buf.extend_from_slice(CF_HTML_PREFIX.as_bytes());
    buf.extend_from_slice(fragment.as_bytes());
    buf.extend_from_slice(CF_HTML_SUFFIX.as_bytes());

    // Patch only the header so a fragment containing "StartHTML:" cannot match.
    patch_offset(&mut buf[..start_html], b"StartHTML:", start_html);
    patch_offset(&mut buf[..start_html], b"EndHTML:", end_html);
    patch_offset(&mut buf[..start_html], b"StartFragment:", start_fragment);
    patch_offset(&mut buf[..start_html], b"EndFragment:", end_fragment);

    buf.push(0);
    buf
}

/// `CF_HTML` → UTF-8 fragment via `StartFragment`/`EndFragment`; falls back
/// to `StartHTML`/`EndHTML`, then the whole buffer.
pub fn html_from_cf(raw: &[u8]) -> Vec<u8> {
    let range = header_range(raw, b"StartFragment:", b"EndFragment:")
        .or_else(|| header_range(raw, b"StartHTML:", b"EndHTML:"));
    match range {
        Some((start, end)) => {
            let slice = &raw[start..end];
            strip_trailing_nul(slice).to_vec()
        }
        None => strip_trailing_nul(raw).to_vec(),
    }
}

fn header_range(raw: &[u8], start_label: &[u8], end_label: &[u8]) -> Option<(usize, usize)> {
    let start = read_header_offset(raw, start_label)?;
    let end = read_header_offset(raw, end_label)?;
    if start <= end && end <= raw.len() {
        Some((start, end))
    } else {
        None
    }
}

fn patch_offset(header: &mut [u8], label: &[u8], value: usize) {
    if let Some(pos) = find(header, label) {
        let at = pos + label.len();
        if at + 10 <= header.len() {
            let digits = format!("{value:010}");
            header[at..at + 10].copy_from_slice(digits.as_bytes());
        }
    }
}

/// Decimal after `label:` (`StartFragment:`). The colon keeps this off the
/// `<!--StartFragment-->` marker and any fragment text.
fn read_header_offset(raw: &[u8], label: &[u8]) -> Option<usize> {
    let mut at = find(raw, label)? + label.len();
    let mut n: usize = 0;
    let mut any = false;
    while let Some(&b) = raw.get(at) {
        if b.is_ascii_digit() {
            n = n.checked_mul(10)?.checked_add((b - b'0') as usize)?;
            any = true;
            at += 1;
        } else {
            break;
        }
    }
    any.then_some(n)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// `"Rich Text Format"` HGLOBAL → RTF. Strip one trailing NUL some producers
/// append past the final `}`.
pub fn rtf_from_cf(raw: &[u8]) -> Vec<u8> {
    strip_trailing_nul(raw).to_vec()
}

fn strip_trailing_nul(b: &[u8]) -> &[u8] {
    match b.last() {
        Some(0) => &b[..b.len() - 1],
        _ => b,
    }
}

// CF_DIB is a BMP minus the 14-byte BITMAPFILEHEADER (BITMAPINFOHEADER or
// V4/V5, optional palette/masks, then rows). Windows synthesizes
// `CF_BITMAP`/`CF_DIBV5` from it; the registered `"PNG"` format is rare.

/// PNG/JPEG/GIF (anything `image` sniffs) → `CF_DIB` (BITMAPINFOHEADER,
/// 32bpp BGRA, BI_RGB, bottom-up). GIF uses frame 0. `None` = leave unrendered.
pub fn image_to_dib(bytes: &[u8]) -> Option<Vec<u8>> {
    // Decode caps, not result caps: `image` defaults to 512 MiB and would
    // allocate a huge declared size before the check below. 32767 is the
    // CF_DIB per-side max; 256 MiB bounds the decode well below that.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32767);
    limits.max_image_height = Some(32767);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut reader = reader;
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);
    if w == 0 || h == 0 || w > 32767 || h > 32767 {
        return None;
    }
    let mut out = Vec::with_capacity(40 + w * h * 4);
    // BITMAPINFOHEADER, 32bpp BI_RGB (no masks/palette). Positive height = bottom-up.
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(w as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(h as i32).to_le_bytes()); // biHeight (bottom-up)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0u8; 16]); // XPels/YPels/ClrUsed/ClrImportant
                                       // Bottom-up BGRA. 32bpp rows are already 4-byte aligned.
    for row in rgba.rows().rev() {
        for px in row {
            let [r, g, b, a] = px.0;
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    Some(out)
}

/// `CF_DIB` → PNG: prepend the BITMAPFILEHEADER a BMP decoder expects
/// (pixel offset from header + palette/masks). `None` on malformed input.
pub fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let hdr_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if hdr_size < 40 || hdr_size > dib.len() {
        return None;
    }
    let bit_count = u16::from_le_bytes(dib[14..16].try_into().ok()?) as usize;
    let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    let clr_used = u32::from_le_bytes(dib[32..36].try_into().ok()?) as usize;
    // Palette (≤8bpp) or BI_BITFIELDS masks sit between header and pixels.
    let palette = if bit_count <= 8 {
        (if clr_used != 0 {
            clr_used
        } else {
            1 << bit_count
        }) * 4
    } else if compression == 3 {
        // BI_BITFIELDS: 3 DWORD masks after a 40-byte BITMAPINFOHEADER.
        // V4/V5 already store the masks inside `hdr_size`.
        if hdr_size == 40 {
            12
        } else {
            0
        }
    } else {
        0
    };
    let pixel_offset = 14 + hdr_size + palette;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes()); // reserved
    bmp.extend_from_slice(&(pixel_offset as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).ok()?;
    let mut png = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .ok()?;
    Some(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trips_and_handles_terminator() {
        let wire = "héllo 🌍".as_bytes();
        let cf = text_to_utf16(wire);
        assert_eq!(&cf[cf.len() - 2..], &[0, 0]);
        assert_eq!(text_from_utf16(&cf), wire);

        let no_term: Vec<u8> = "hi".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(text_from_utf16(&no_term), b"hi");

        assert_eq!(text_to_utf16(b""), vec![0, 0]);
        assert_eq!(text_from_utf16(&[0, 0]), b"");
    }

    #[test]
    fn text_from_utf16_stops_at_the_first_terminator() {
        // GlobalSize is the allocation, not the string. Extra zero units and
        // uninitialized tail bytes sit past the required NUL on real HGLOBALs.
        let mut padded: Vec<u8> = "hi".encode_utf16().flat_map(u16::to_le_bytes).collect();
        padded.extend_from_slice(&[0, 0]); // terminator
        padded.extend_from_slice(&[0, 0, 0, 0, 0xFF, 0xEE]);
        assert_eq!(text_from_utf16(&padded), b"hi");
    }

    #[test]
    fn cf_html_matches_the_spec_offsets() {
        let cf = html_to_cf(b"Hello");
        let s = String::from_utf8(cf.clone()).unwrap();
        assert!(s.contains("StartHTML:0000000105"), "{s}");
        assert!(s.contains("EndHTML:0000000178"), "{s}");
        assert!(s.contains("StartFragment:0000000139"), "{s}");
        assert!(s.contains("EndFragment:0000000144"), "{s}");
        let start = read_header_offset(&cf, b"StartFragment:").unwrap();
        let end = read_header_offset(&cf, b"EndFragment:").unwrap();
        assert_eq!(&cf[start..end], b"Hello");
        assert_eq!(*cf.last().unwrap(), 0);
        assert_eq!(read_header_offset(&cf, b"EndHTML:").unwrap(), cf.len() - 1);
    }

    #[test]
    fn cf_html_round_trips_including_multibyte() {
        for frag in [
            "Hello",
            "<b>bold</b> & <i>ital</i>",
            "café ☕ <span>x</span>",
            "",
        ] {
            let cf = html_to_cf(frag.as_bytes());
            assert_eq!(html_from_cf(&cf), frag.as_bytes(), "fragment {frag:?}");
        }
    }

    #[test]
    fn cf_html_extract_tolerates_foreign_producers() {
        // Offsets drive extraction; do not assume a 105-byte header.
        let fragment = "picked";
        let prefix = "<html><body><!--StartFragment-->";
        let header_body = format!(
            "Version:1.0\r\nStartHTML:{sh:010}\r\nEndHTML:{eh:010}\r\n\
             StartFragment:{sf:010}\r\nEndFragment:{ef:010}\r\nSourceURL:https://x/\r\n",
            sh = 0,
            eh = 0,
            sf = 0,
            ef = 0,
        );
        let start_html = header_body.len();
        let start_fragment = start_html + prefix.len();
        let end_fragment = start_fragment + fragment.len();
        let end_html = end_fragment + "<!--EndFragment--></body></html>".len();
        let full = format!(
            "Version:1.0\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\n\
             StartFragment:{start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\nSourceURL:https://x/\r\n\
             {prefix}{fragment}<!--EndFragment--></body></html>"
        );
        assert_eq!(html_from_cf(full.as_bytes()), fragment.as_bytes());
    }

    #[test]
    fn cf_html_extract_falls_back_without_markers() {
        let mut b = b"<p>no markers</p>".to_vec();
        assert_eq!(html_from_cf(&b), b"<p>no markers</p>");
        b.push(0);
        assert_eq!(html_from_cf(&b), b"<p>no markers</p>");
    }

    #[test]
    fn rtf_strips_one_trailing_nul() {
        assert_eq!(rtf_from_cf(br"{\rtf1 hi}"), br"{\rtf1 hi}");
        assert_eq!(rtf_from_cf(b"{\\rtf1 hi}\0"), br"{\rtf1 hi}");
        assert_eq!(rtf_from_cf(b"x\0\0"), b"x\0");
    }

    #[test]
    fn png_dib_round_trip() {
        let mut png = Vec::new();
        let img = image::RgbaImage::from_fn(3, 2, |x, y| {
            image::Rgba([x as u8 * 40, y as u8 * 80, 200, 255])
        });
        image::DynamicImage::ImageRgba8(img.clone())
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let dib = image_to_dib(&png).expect("png -> dib");
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 3);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), 2);
        let png2 = dib_to_png(&dib).expect("dib -> png");
        let back = image::load_from_memory(&png2).unwrap().to_rgba8();
        assert_eq!(back.dimensions(), (3, 2));
        assert_eq!(back.get_pixel(2, 1), img.get_pixel(2, 1));
    }

    #[test]
    fn dib_to_png_rejects_garbage() {
        assert!(dib_to_png(&[0u8; 10]).is_none());
        assert!(dib_to_png(&[0xFFu8; 200]).is_none());
    }
}
