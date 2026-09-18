//! Does ingest produce the NV12 the encoder expects? RGB in, BT.709 limited range
//! out, read straight back from the surface — no encoder, no decoder, just the
//! numbers. Then the same through a dmabuf, exported and re-imported on this
//! display: a capture buffer with no compositor in the way. And a source twice
//! the size, scaled on the same pass: what a mirrored 4K head becomes.
//!
//! Ignored: needs a VAAPI device. `.25` and `.50` both have one.

use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;

use pf_libva::encode::open;
use pf_libva::encode::CodecParams;
use pf_libva::vpp::Vpp;
use pf_libva::Display;
use pf_libva::DmabufSource;
use pf_libva::Libva;
use pf_libva::VaSurfaceId;
use pf_vaapi::drm::flatten;
use pf_vaapi::drm::VaDrmPrimeSurfaceDescriptor;
use pf_vaapi::drm::VA_EXPORT_SURFACE_READ_ONLY;
use pf_vaapi::drm::VA_EXPORT_SURFACE_SEPARATE_LAYERS;
use pf_vaapi::drm::VA_FOURCC_NV12;
use pf_vaapi::drm::VA_FOURCC_P010;
use pf_vaapi::enc_params::SessionParams;
use pf_vaapi::vpp::DRM_FORMAT_ARGB8888;
use pf_vaapi::vpp::DRM_FORMAT_XRGB8888;
use pf_vaapi::vpp::VA_FOURCC_BGRA;
use pf_vaapi::vpp::VA_FOURCC_X2R10G10B10;
use pf_vaapi::vpp::VA_RT_FORMAT_RGB32;
use pf_vaapi::vpp::VA_RT_FORMAT_RGB32_10;
use pf_vaapi::vpp::VA_RT_FORMAT_YUV420;
use pf_vaapi::vpp::VA_RT_FORMAT_YUV420_10;
use pf_vaapi::vpp::VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;

const W: u32 = 320;
const H: u32 = 240;

fn display() -> Display {
    Display::open(Libva::load().expect("libva")).expect("a VAAPI display")
}

/// A solid BGRA picture.
fn bgra(b: u8, g: u8, r: u8) -> Vec<u8> {
    [b, g, r, 255].repeat((W * H) as usize)
}

/// Y, Cb, Cr at the picture centre.
fn centre_yuv(display: &Display, nv12: VaSurfaceId) -> (u8, u8, u8) {
    yuv_at(display, nv12, W as usize / 2, H as usize / 2)
}

/// Y, Cb, Cr at (`x`, `y`) of an NV12 surface.
fn yuv_at(display: &Display, nv12: VaSurfaceId, x: usize, y: usize) -> (u8, u8, u8) {
    display
        .map_image(nv12, |image, ptr| {
            // SAFETY: inside the mapped image, by the driver's own pitches and offsets.
            unsafe {
                let luma = *ptr.add(image.offsets[0] as usize + y * image.pitches[0] as usize + x);
                let uv = ptr.add(
                    image.offsets[1] as usize + (y / 2) * image.pitches[1] as usize + (x / 2) * 2,
                );
                Ok((luma, *uv, *uv.add(1)))
            }
        })
        .expect("read the NV12 back")
}

fn near(got: (u8, u8, u8), want: (u8, u8, u8), what: &str) {
    for ((g, w), name) in [got.0, got.1, got.2]
        .into_iter()
        .zip([want.0, want.1, want.2])
        .zip(["Y", "Cb", "Cr"])
    {
        assert!(
            (i32::from(g) - i32::from(w)).abs() <= 4,
            "{what}: {name} = {g}, want {w} (got {got:?}, want {want:?})"
        );
    }
}

/// BT.709, limited range: red is Y 63 / Cb 102 / Cr 240 and white is Y 235.
/// BT.601 would put red's Y at 82 and full range at 54 — a different, wrong picture
/// that decodes fine.
#[test]
#[ignore = "needs a VAAPI device"]
fn rgb_becomes_limited_range_bt709_nv12() {
    let display = display();
    let vpp = Vpp::new(&display, W, H).expect("VideoProc");
    let src = display
        .create_surface(VA_RT_FORMAT_RGB32, Some(VA_FOURCC_BGRA), W, H)
        .expect("a BGRA surface");
    let dst = display
        .create_surface(VA_RT_FORMAT_YUV420, Some(VA_FOURCC_NV12), W, H)
        .expect("an NV12 surface");
    let row = W as usize * 4;

    display
        .write_packed(src, &bgra(0, 0, 255), row)
        .expect("upload red");
    vpp.convert(
        &display,
        src,
        (W, H),
        true,
        pf_vaapi::hevc::COLOUR_BT709,
        dst,
    )
    .expect("convert red");
    near(centre_yuv(&display, dst), (63, 102, 240), "red");

    display
        .write_packed(src, &bgra(255, 255, 255), row)
        .expect("upload white");
    vpp.convert(
        &display,
        src,
        (W, H),
        true,
        pf_vaapi::hevc::COLOUR_BT709,
        dst,
    )
    .expect("convert white");
    near(centre_yuv(&display, dst), (235, 128, 128), "white");

    // The same red through a dmabuf, exported here and imported back as `XR24`
    // (the everyday capture format) and as `AR24`.
    display
        .write_packed(src, &bgra(0, 0, 255), row)
        .expect("upload red");
    let mut desc = VaDrmPrimeSurfaceDescriptor::zeroed();
    // SAFETY: live display and surface; `desc` is exactly the layout `DRM_PRIME_2`
    // writes and it outlives the call.
    display
        .va
        .check("vaExportSurfaceHandle", unsafe {
            (display.va.export_surface_handle)(
                display.display,
                src,
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
                (&raw mut desc).cast(),
            )
        })
        .expect("export the BGRA surface");
    let exported = flatten(&desc).expect("a flat plane list");
    // SAFETY: each fd came out of a successful export and is wrapped exactly once.
    let fds: Vec<OwnedFd> = exported
        .object_fds
        .iter()
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    println!(
        "exported: {} plane(s), modifier {:#x}, stride {}",
        exported.planes.len(),
        exported.modifier,
        exported.planes[0].stride
    );
    for drm_fourcc in [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888] {
        let source = DmabufSource {
            width: W,
            height: H,
            drm_fourcc,
            modifier: exported.modifier,
            planes: &exported.planes,
        };
        let imported = display.import_dmabuf(&source).expect("import the dmabuf");
        vpp.convert(
            &display,
            imported,
            (W, H),
            true,
            pf_vaapi::hevc::COLOUR_BT709,
            dst,
        )
        .expect("convert the import");
        near(centre_yuv(&display, dst), (63, 102, 240), "red via dmabuf");
        display.destroy_surface(imported);
    }
    drop(fds);
    display.destroy_surface(src);
    display.destroy_surface(dst);
    vpp.destroy(&display);
}

/// Y, Cb, Cr at the picture centre of a P010 surface: ten bits in the top of each
/// sixteen.
fn centre_yuv10(display: &Display, p010: VaSurfaceId) -> (u16, u16, u16) {
    display
        .map_image(p010, |image, ptr| {
            let (x, y) = (W as usize / 2, H as usize / 2);
            // SAFETY: inside the mapped image, by the driver's own pitches and offsets;
            // samples are 16-bit words, read unaligned to be safe.
            unsafe {
                let luma = ptr
                    .add(image.offsets[0] as usize + y * image.pitches[0] as usize + x * 2)
                    .cast::<u16>()
                    .read_unaligned();
                let uv = ptr.add(
                    image.offsets[1] as usize + (y / 2) * image.pitches[1] as usize + (x / 2) * 4,
                );
                let cb = uv.cast::<u16>().read_unaligned();
                let cr = uv.add(2).cast::<u16>().read_unaligned();
                Ok((luma >> 6, cb >> 6, cr >> 6))
            }
        })
        .expect("read the P010 back")
}

/// A source twice the session's size, red on the left and white on the right, comes
/// out at the session's size with the seam still in the middle: the VideoProc pass
/// scales when the two regions differ, and the colours survive it.
#[test]
#[ignore = "needs a VAAPI device"]
fn a_larger_source_is_scaled_into_the_picture() {
    let display = display();
    let vpp = Vpp::new(&display, W, H).expect("VideoProc");
    let (sw, sh) = (W * 2, H * 2);
    let src = display
        .create_surface(VA_RT_FORMAT_RGB32, Some(VA_FOURCC_BGRA), sw, sh)
        .expect("a BGRA surface");
    let dst = display
        .create_surface(VA_RT_FORMAT_YUV420, Some(VA_FOURCC_NV12), W, H)
        .expect("an NV12 surface");
    let mut picture = Vec::with_capacity((sw * sh * 4) as usize);
    for _ in 0..sh {
        picture.extend([0, 0, 255, 255].repeat(sw as usize / 2));
        picture.extend([255, 255, 255, 255].repeat(sw as usize / 2));
    }
    display
        .write_packed(src, &picture, sw as usize * 4)
        .expect("upload the split picture");
    vpp.convert(
        &display,
        src,
        (sw, sh),
        true,
        pf_vaapi::hevc::COLOUR_BT709,
        dst,
    )
    .expect("scale and convert");
    near(
        yuv_at(&display, dst, W as usize / 4, H as usize / 2),
        (63, 102, 240),
        "left, red",
    );
    near(
        yuv_at(&display, dst, W as usize * 3 / 4, H as usize / 2),
        (235, 128, 128),
        "right, white",
    );
    display.destroy_surface(src);
    display.destroy_surface(dst);
    vpp.destroy(&display);
}

/// A joiner's crop: the red and white middle of a source with blue edges, scaled to half,
/// comes out with no blue at either edge. The cut-off columns never reach the picture.
#[test]
#[ignore = "needs a VAAPI device"]
fn a_cropped_source_keeps_only_its_rectangle() {
    let display = display();
    let mut vpp = Vpp::new(&display, W, H).expect("VideoProc");
    let (sw, sh) = (W * 4, H * 2);
    let src = display
        .create_surface(VA_RT_FORMAT_RGB32, Some(VA_FOURCC_BGRA), sw, sh)
        .expect("a BGRA surface");
    let dst = display
        .create_surface(VA_RT_FORMAT_YUV420, Some(VA_FOURCC_NV12), W, H)
        .expect("an NV12 surface");
    let (blue, red, white) = ([255, 0, 0, 255], [0, 0, 255, 255], [255, 255, 255, 255]);
    let mut picture = Vec::with_capacity((sw * sh * 4) as usize);
    for _ in 0..sh {
        for band in [blue, red, white, blue] {
            picture.extend(band.repeat(W as usize));
        }
    }
    display
        .write_packed(src, &picture, sw as usize * 4)
        .expect("upload the banded picture");
    vpp.crop = Some([W, 0, W * 2, sh]);
    vpp.convert(
        &display,
        src,
        (sw, sh),
        true,
        pf_vaapi::hevc::COLOUR_BT709,
        dst,
    )
    .expect("crop, scale and convert");
    near(
        yuv_at(&display, dst, 4, H as usize / 2),
        (63, 102, 240),
        "left edge, red",
    );
    near(
        yuv_at(&display, dst, W as usize - 4, H as usize / 2),
        (235, 128, 128),
        "right edge, white",
    );
    display.destroy_surface(src);
    display.destroy_surface(dst);
    vpp.destroy(&display);
}

/// Ten bits: BT.2020 limited range puts red at Y 294 / Cb 387 / Cr 960. BT.709
/// coefficients in a stream tagged BT.2020 would show as Y 250 — a picture that
/// decodes fine and is the wrong red.
#[test]
#[ignore = "needs a VAAPI device"]
fn ten_bit_rgb_becomes_limited_range_bt2020_p010() {
    let display = display();
    let vpp = Vpp::new(&display, W, H).expect("VideoProc");
    let src = display
        .create_surface(VA_RT_FORMAT_RGB32_10, Some(VA_FOURCC_X2R10G10B10), W, H)
        .expect("an XR30 surface");
    let dst = display
        .create_surface(VA_RT_FORMAT_YUV420_10, Some(VA_FOURCC_P010), W, H)
        .expect("a P010 surface");
    let red: Vec<u8> = (1023u32 << 20).to_le_bytes().repeat((W * H) as usize);
    display
        .write_packed(src, &red, W as usize * 4)
        .expect("upload red");
    vpp.convert(
        &display,
        src,
        (W, H),
        true,
        pf_vaapi::hevc::COLOUR_BT2020_PQ,
        dst,
    )
    .expect("convert red");
    let (y, cb, cr) = centre_yuv10(&display, dst);
    println!("ten-bit red: Y {y} Cb {cb} Cr {cr}");
    for (got, want, name) in [(y, 294u16, "Y"), (cb, 387, "Cb"), (cr, 960, "Cr")] {
        assert!(
            (i32::from(got) - i32::from(want)).abs() <= 8,
            "red: {name} = {got}, want {want} (BT.2020 limited)"
        );
    }
    display.destroy_surface(src);
    display.destroy_surface(dst);
    vpp.destroy(&display);
}

/// The whole path through the session's own ingest: RGB in, H.264 out.
/// `PF_ENC_OUT` writes the stream; frame 0 is pure red, so a decoder's first pixel
/// is the second opinion on the colour.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn the_session_encodes_what_ingest_gives_it() {
    let params = SessionParams {
        width: W,
        height: H,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps: 4_000_000,
        slots: 1,
        max_num_reorder_frames: 0,
        initial_qp: 26,
        vbv_frames: 1.0,
    };
    let mut enc = open(params, CodecParams::H264).expect("an encoder");
    let mut stream = Vec::new();
    for i in 0..10u8 {
        // A colour that moves every frame, so the P frames carry something.
        enc.submit_packed(
            &bgra(i * 20, 0, 255 - i * 20),
            VA_FOURCC_BGRA,
            W,
            H,
            W as usize * 4,
        )
        .expect("ingest");
        let pic = enc.encode(i == 0).expect("encode");
        assert_eq!(pic.is_idr, i == 0);
        stream.extend_from_slice(&pic.bytes);
    }
    if let Ok(path) = std::env::var("PF_ENC_OUT") {
        std::fs::write(&path, &stream).expect("write the stream out");
        println!("wrote {path}");
    }
    assert!(stream.len() > 100, "{} bytes for ten frames", stream.len());
}

/// A session fed pictures twice its size — a mirrored 4K head into a client-sized
/// encoder — scales them on ingest and encodes as usual.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn a_larger_picture_encodes_at_the_session_size() {
    let params = SessionParams {
        width: W,
        height: H,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps: 4_000_000,
        slots: 1,
        max_num_reorder_frames: 0,
        initial_qp: 26,
        vbv_frames: 1.0,
    };
    let mut enc = open(params, CodecParams::H264).expect("an encoder");
    let (sw, sh) = (W * 2, H * 2);
    for i in 0..3u8 {
        let picture = [i * 40, 0, 255 - i * 40, 255].repeat((sw * sh) as usize);
        enc.submit_packed(&picture, VA_FOURCC_BGRA, sw, sh, sw as usize * 4)
            .expect("scaled ingest");
        let pic = enc.encode(i == 0).expect("encode");
        assert_eq!(pic.is_idr, i == 0);
        assert!(
            pic.bytes.len() > 10,
            "{} bytes for frame {i}",
            pic.bytes.len()
        );
    }
}

/// Chroma sits on the left luma column (H.273 type 0), the siting decoders assume. A red|white
/// edge between columns 160 and 161 leaves the chroma sample at column 160 fully red when it is
/// point-sited left, 75% red for a left [1 2 1], and 50% red for a centre-sited 2×2 box.
#[test]
#[ignore = "needs a VAAPI device"]
fn chroma_is_sited_on_the_left_column() {
    let display = display();
    let vpp = Vpp::new(&display, W, H).expect("VideoProc");
    let src = display
        .create_surface(VA_RT_FORMAT_RGB32, Some(VA_FOURCC_BGRA), W, H)
        .expect("a BGRA surface");
    let dst = display
        .create_surface(VA_RT_FORMAT_YUV420, Some(VA_FOURCC_NV12), W, H)
        .expect("an NV12 surface");
    let picture: Vec<u8> = (0..(W * H) as usize)
        .flat_map(|i| {
            if i % W as usize <= 160 {
                [0, 0, 255, 255]
            } else {
                [255, 255, 255, 255]
            }
        })
        .collect();
    display
        .write_packed(src, &picture, W as usize * 4)
        .expect("upload the edge");
    vpp.convert(
        &display,
        src,
        (W, H),
        true,
        pf_vaapi::hevc::COLOUR_BT709,
        dst,
    )
    .expect("convert");
    let (_, _, cr) = yuv_at(&display, dst, 160, H as usize / 2);
    let red = (f64::from(cr) - 128.0) / (240.0 - 128.0);
    println!("chroma at column 160: Cr {cr}, {:.0}% red", red * 100.0);
    assert!(
        red > 0.65,
        "centre-sited chroma ({:.0}% red at column 160)",
        red * 100.0
    );
    display.destroy_surface(src);
    display.destroy_surface(dst);
    vpp.destroy(&display);
}

/// Rows below the picture in a macroblock-aligned target: what does a convert leave there?
/// The encoder codes them and deblocks across the edge, so they must not be stale.
#[test]
#[ignore = "needs a VAAPI device"]
fn padding_rows_below_the_picture_are_written() {
    let (w, h, coded_h) = (320u32, 250u32, 256u32);
    let display = display();
    let vpp = Vpp::new(&display, w, h).expect("VideoProc");
    let src = display
        .create_surface(VA_RT_FORMAT_RGB32, Some(VA_FOURCC_BGRA), w, h)
        .expect("a BGRA surface");
    let dst = display
        .create_surface(VA_RT_FORMAT_YUV420, Some(VA_FOURCC_NV12), w, coded_h)
        .expect("an aligned NV12 surface");
    // A sentinel the convert has to overwrite.
    display
        .map_image(dst, |image, ptr| {
            for y in 0..usize::from(image.height) {
                // SAFETY: inside the mapped image, by the driver's own pitches and offsets.
                unsafe {
                    let luma = image.offsets[0] as usize + y * image.pitches[0] as usize;
                    std::ptr::write_bytes(ptr.add(luma), 7, w as usize);
                    if y % 2 == 0 {
                        let uv = image.offsets[1] as usize + (y / 2) * image.pitches[1] as usize;
                        std::ptr::write_bytes(ptr.add(uv), 7, w as usize);
                    }
                }
            }
            Ok(())
        })
        .expect("fill the sentinel");
    display
        .write_packed(
            src,
            &[255, 255, 255, 255].repeat((w * h) as usize),
            w as usize * 4,
        )
        .expect("upload white");
    vpp.convert(
        &display,
        src,
        (w, h),
        true,
        pf_vaapi::hevc::COLOUR_BT709,
        dst,
    )
    .expect("convert");
    for y in [h as usize - 1, h as usize, coded_h as usize - 1] {
        println!("row {y}: {:?}", yuv_at(&display, dst, w as usize / 2, y));
    }
    near(
        yuv_at(&display, dst, w as usize / 2, h as usize - 1),
        (235, 128, 128),
        "last row",
    );
    for y in h as usize..coded_h as usize {
        let got = yuv_at(&display, dst, w as usize / 2, y);
        assert_ne!(
            got,
            (7, 7, 7),
            "row {y} kept the sentinel: the convert never wrote it"
        );
    }
    display.destroy_surface(src);
    display.destroy_surface(dst);
    vpp.destroy(&display);
}
