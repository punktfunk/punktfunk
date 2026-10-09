//! The session at a client's real size: a striped picture through the CPU upload
//! and through a linear GBM dmabuf (what Mutter hands the host), written out for
//! a decoder to look at. Ignored: needs a VAAPI encode device. `PF_W`/`PF_H` size
//! it (5120×1440), `PF_CODEC=h264` picks H.264, `PF_ENC_OUT` takes the stream.
#![cfg(target_os = "linux")]
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

use pf_libva::encode::{open, CodecParams};
use pf_libva::DmabufSource;
use pf_vaapi::drm::ExportedPlane;
use pf_vaapi::enc_params::SessionParams;
use pf_vaapi::hevc::COLOUR_BT709;
use pf_vaapi::vpp::{DRM_FORMAT_XRGB8888, VA_FOURCC_BGRA};

fn size() -> (u32, u32) {
    let get = |k: &str, d: u32| {
        std::env::var(k)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(d)
    };
    (get("PF_W", 5120), get("PF_H", 1440))
}

fn params(w: u32, h: u32) -> SessionParams {
    SessionParams {
        width: w,
        height: h,
        fps_num: 60,
        fps_den: 1,
        bitrate_bps: 30_000_000,
        slots: 2,
        max_num_reorder_frames: 0,
        initial_qp: 26,
        vbv_frames: 1.0,
    }
}

fn codec() -> CodecParams {
    if std::env::var("PF_CODEC").is_ok_and(|c| c == "h264") {
        CodecParams::H264
    } else {
        CodecParams::Hevc {
            ten_bit: false,
            colour: COLOUR_BT709,
        }
    }
}

/// 32-px vertical stripes (white / dark) and an 8-row red band every 128 rows: a
/// wrong pitch anywhere turns the stripes into diagonals.
fn stripes(w: u32, h: u32, stride: u32, phase: u32) -> Vec<u8> {
    let mut buf = vec![0u8; stride as usize * h as usize];
    for y in 0..h {
        let row = &mut buf[(y * stride) as usize..][..(w * 4) as usize];
        for (x, px) in row.chunks_mut(4).enumerate() {
            let on = ((x as u32 + phase * 8) / 32) % 2 == 0;
            let (b, g, r) = if y % 128 < 8 {
                (0, 0, 255)
            } else if on {
                (235, 235, 235)
            } else {
                (40, 40, 40)
            };
            px.copy_from_slice(&[b, g, r, 255]);
        }
    }
    buf
}

fn write_out(stream: &[u8]) {
    if let Ok(path) = std::env::var("PF_ENC_OUT") {
        std::fs::write(&path, stream).expect("write the stream out");
        println!("wrote {path} ({} bytes)", stream.len());
    }
}

#[test]
#[ignore = "needs a VAAPI encode device"]
fn a_wide_upload_encodes_straight() {
    let (w, h) = size();
    let mut enc = open(params(w, h), codec()).expect("an encoder");
    let mut stream = Vec::new();
    for i in 0..3 {
        enc.submit_packed(
            &stripes(w, h, w * 4, i),
            VA_FOURCC_BGRA,
            w,
            h,
            (w * 4) as usize,
        )
        .expect("upload");
        enc.encode(i == 0).expect("encode");
        stream.extend_from_slice(
            &enc.collect(true)
                .expect("collect")
                .expect("a picture per encode")
                .bytes,
        );
    }
    write_out(&stream);
}

mod gbm {
    use std::ffi::c_void;
    pub enum Device {}
    pub enum Bo {}
    pub const XRGB8888: u32 = 0x3432_5258;
    pub const USE_RENDERING: u32 = 1 << 2;
    pub const TRANSFER_WRITE: u32 = 1 << 1;
    #[link(name = "gbm")]
    unsafe extern "C" {
        pub fn gbm_create_device(fd: i32) -> *mut Device;
        pub fn gbm_device_destroy(dev: *mut Device);
        pub fn gbm_bo_create_with_modifiers2(
            dev: *mut Device,
            w: u32,
            h: u32,
            format: u32,
            modifiers: *const u64,
            count: u32,
            flags: u32,
        ) -> *mut Bo;
        pub fn gbm_bo_destroy(bo: *mut Bo);
        pub fn gbm_bo_get_stride(bo: *mut Bo) -> u32;
        pub fn gbm_bo_get_fd(bo: *mut Bo) -> i32;
        pub fn gbm_bo_map(
            bo: *mut Bo,
            x: u32,
            y: u32,
            w: u32,
            h: u32,
            flags: u32,
            stride: *mut u32,
            map_data: *mut *mut c_void,
        ) -> *mut c_void;
        pub fn gbm_bo_unmap(bo: *mut Bo, map_data: *mut c_void);
    }
}

/// The everyday capture shape: a LINEAR XR24 GBM buffer, filled through its map,
/// imported by fd at GBM's own stride.
#[test]
#[ignore = "needs a VAAPI encode device"]
fn a_wide_linear_dmabuf_encodes_straight() {
    let (w, h) = size();
    let node = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/renderD128")
        .expect("render node");
    // SAFETY: plain FFI over a live render-node fd; every handle is destroyed below.
    let (fd, stride) = unsafe {
        let dev = gbm::gbm_create_device(node.as_raw_fd());
        assert!(!dev.is_null(), "gbm device");
        let linear = 0u64;
        let bo = gbm::gbm_bo_create_with_modifiers2(
            dev,
            w,
            h,
            gbm::XRGB8888,
            &linear,
            1,
            gbm::USE_RENDERING,
        );
        assert!(!bo.is_null(), "linear XR24 bo");
        let mut map_stride = 0u32;
        let mut map_data = std::ptr::null_mut();
        let ptr = gbm::gbm_bo_map(
            bo,
            0,
            0,
            w,
            h,
            gbm::TRANSFER_WRITE,
            &mut map_stride,
            &mut map_data,
        );
        assert!(!ptr.is_null(), "map the bo");
        let pixels = stripes(w, h, map_stride, 0);
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), ptr.cast::<u8>(), pixels.len());
        gbm::gbm_bo_unmap(bo, map_data);
        let stride = gbm::gbm_bo_get_stride(bo);
        let fd = OwnedFd::from_raw_fd(gbm::gbm_bo_get_fd(bo));
        gbm::gbm_bo_destroy(bo);
        gbm::gbm_device_destroy(dev);
        (fd, stride)
    };
    println!("gbm linear {w}x{h}: stride {stride} (w*4 = {})", w * 4);
    let mut enc = open(params(w, h), codec()).expect("an encoder");
    let planes = [ExportedPlane {
        fd: fd.as_raw_fd(),
        offset: 0,
        stride,
    }];
    let mut stream = Vec::new();
    for i in 0..3 {
        enc.submit_dmabuf(&DmabufSource {
            width: w,
            height: h,
            drm_fourcc: DRM_FORMAT_XRGB8888,
            modifier: 0,
            planes: &planes,
        })
        .expect("import");
        enc.encode(i == 0).expect("encode");
        stream.extend_from_slice(
            &enc.collect(true)
                .expect("collect")
                .expect("a picture per encode")
                .bytes,
        );
    }
    write_out(&stream);
}
