//! CPU capture frames the Linux PyroWave and Vulkan Video tests feed their encoders.

use pf_frame::{CapturedFrame, FramePayload, PixelFormat};

/// A BGRX frame of one `fill` colour.
pub fn cpu_frame(w: u32, h: u32, pts_ns: u64, fill: [u8; 4]) -> CapturedFrame {
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&fill);
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns,
        format: PixelFormat::Bgrx,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}

/// Packed 24-bpp CPU frame. `rgb` is (r, g, b) regardless of `fmt`'s byte order.
pub fn cpu_frame_24(w: u32, h: u32, pts_ns: u64, rgb: [u8; 3], fmt: PixelFormat) -> CapturedFrame {
    let px = match fmt {
        PixelFormat::Rgb => [rgb[0], rgb[1], rgb[2]],
        PixelFormat::Bgr => [rgb[2], rgb[1], rgb[0]],
        _ => unreachable!("24-bpp helper"),
    };
    let mut buf = vec![0u8; (w * h * 3) as usize];
    for p in buf.chunks_exact_mut(3) {
        p.copy_from_slice(&px);
    }
    CapturedFrame {
        provenance: Default::default(),
        width: w,
        height: h,
        pts_ns,
        format: fmt,
        payload: FramePayload::Cpu(buf),
        cursor: None,
    }
}
