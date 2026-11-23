//! Where white sits in an HDR capture. ScreenCaptureKit's PQ puts a unit of white at 100 nits;
//! the stream carries it at 203 (BT.2408), where every client anchors SDR. No capture setting
//! and no display property moves it, so one Metal compute pass does: packed 10-bit RGB in, each
//! channel through [`plan::lift_white`], BT.2020 Y'CbCr 4:2:0 out in the 10-bit biplanar buffer
//! the encoder takes as it is.
//!
//! The pass runs on the capture queue and waits for the GPU: the encoder reads the surface next,
//! and nothing else orders the two.

use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_video::{
    kCVImageBufferChromaLocationTopFieldKey, kCVImageBufferChromaLocation_Center,
    kCVImageBufferColorPrimariesKey, kCVImageBufferColorPrimaries_ITU_R_2020,
    kCVImageBufferTransferFunctionKey, kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
    kCVImageBufferYCbCrMatrixKey, kCVImageBufferYCbCrMatrix_ITU_R_2020, kCVPixelBufferHeightKey,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
    CVAttachmentMode, CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetIOSurface, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
    CVPixelBufferPool,
};
use objc2_foundation::NSString;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue,
    MTLComputeCommandEncoder, MTLComputePipelineState, MTLCreateSystemDefaultDevice, MTLDevice,
    MTLLibrary, MTLOrigin, MTLPixelFormat, MTLRegion, MTLSize, MTLTexture, MTLTextureDescriptor,
    MTLTextureUsage,
};

use super::plan;

/// One thread per chroma sample: the 2x2 block of pixels it covers. Chroma is their mean, so
/// it sits at the block's centre.
const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

struct Quant { float luma_scale; float luma_offset; float chroma_scale; float chroma_offset; };

static float3 lifted(texture2d<float, access::read> src,
                     texture2d<float, access::read> lut, uint2 at) {
    uint3 code = uint3(src.read(at).rgb * 1023.0 + 0.5);
    return float3(lut.read(uint2(code.r, 0)).r, lut.read(uint2(code.g, 0)).r,
                  lut.read(uint2(code.b, 0)).r);
}

kernel void lift(texture2d<float, access::read> src [[texture(0)]],
                 texture2d<float, access::read> lut [[texture(1)]],
                 texture2d<float, access::write> luma [[texture(2)]],
                 texture2d<float, access::write> chroma [[texture(3)]],
                 constant Quant &q [[buffer(0)]],
                 uint2 gid [[thread_position_in_grid]]) {
    if (gid.x >= chroma.get_width() || gid.y >= chroma.get_height()) { return; }
    const float3 k = float3(0.2627, 0.6780, 0.0593);
    const uint2 last = uint2(src.get_width() - 1, src.get_height() - 1);
    float3 sum = float3(0.0);
    for (uint j = 0; j < 2; j++) {
        for (uint i = 0; i < 2; i++) {
            uint2 at = min(gid * 2 + uint2(i, j), last);
            float3 rgb = lifted(src, lut, at);
            sum += rgb;
            float code = round(q.luma_offset + q.luma_scale * dot(rgb, k));
            luma.write(float4(code * 64.0 / 65535.0), at);
        }
    }
    float3 mean = sum * 0.25;
    float y = dot(mean, k);
    float2 c = float2((mean.b - y) / 1.8814, (mean.r - y) / 1.4746);
    float2 code = clamp(round(q.chroma_offset + q.chroma_scale * c), 0.0, 1023.0);
    chroma.write(float4(code * 64.0 / 65535.0, 0.0, 0.0), gid);
}
"#;

/// Threads a side in one group; the kernel returns for those past the chroma plane.
const GROUP: usize = 16;

/// The device's half of the pass: the compiled kernel and the white table. Compiling takes a
/// few hundred milliseconds, so the process does it once and every capture shares the result.
struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLComputePipelineState>>,
    lut: Retained<ProtocolObject<dyn MTLTexture>>,
}

// SAFETY: a Metal device, command queue, pipeline state and texture may each be used from any
// thread; this holds no command buffer or encoder.
unsafe impl Send for Gpu {}
// SAFETY: as above; nothing here changes after `new`.
unsafe impl Sync for Gpu {}

/// The pass for one capture: its output format and its buffers.
pub(crate) struct Lift {
    gpu: &'static Gpu,
    /// The output buffer's format, and its 10-bit code scales.
    code: u32,
    quant: [f32; 4],
    /// Output buffers of the last frame size.
    pool: Mutex<Option<(usize, usize, CFRetained<CVPixelBufferPool>)>>,
    /// Frames converted, and the microseconds they took together.
    spent: [AtomicU64; 2],
}

// SAFETY: a `CVPixelBufferPool` may be used from any thread, and this one is replaced only
// under its mutex; the rest is `Sync` already.
unsafe impl Send for Lift {}
// SAFETY: as above.
unsafe impl Sync for Lift {}

impl Gpu {
    /// The process's one [`Gpu`], made at the first HDR capture.
    fn shared() -> Result<&'static Gpu> {
        static GPU: OnceLock<std::result::Result<Gpu, String>> = OnceLock::new();
        let made = GPU.get_or_init(|| Gpu::new().map_err(|e| format!("{e:#}")));
        made.as_ref().map_err(|e| anyhow!("{e}"))
    }

    fn new() -> Result<Gpu> {
        let device = MTLCreateSystemDefaultDevice().context("no Metal device")?;
        let queue = device.newCommandQueue().context("make a Metal queue")?;
        let library = device
            .newLibraryWithSource_options_error(&NSString::from_str(SHADER), None)
            .map_err(|e| anyhow!("compile the lift kernel: {}", e.localizedDescription()))?;
        let function = library
            .newFunctionWithName(&NSString::from_str("lift"))
            .context("find the lift kernel")?;
        let pipeline = device
            .newComputePipelineStateWithFunction_error(&function)
            .map_err(|e| anyhow!("build the lift pipeline: {}", e.localizedDescription()))?;
        let table = plan::lift_white();
        let lut = texture(
            &device,
            None,
            MTLPixelFormat::R16Unorm,
            (table.len(), 1),
            false,
        )?;
        let region = MTLRegion {
            origin: MTLOrigin { x: 0, y: 0, z: 0 },
            size: MTLSize {
                width: table.len(),
                height: 1,
                depth: 1,
            },
        };
        // SAFETY: `table` holds one row of `region`'s width in 16-bit texels, the texture's
        // format, and outlives the call, which copies it.
        unsafe {
            lut.replaceRegion_mipmapLevel_withBytes_bytesPerRow(
                region,
                0,
                NonNull::from(&table).cast(),
                std::mem::size_of_val(&table),
            );
        }
        Ok(Gpu {
            device,
            queue,
            pipeline,
            lut,
        })
    }
}

impl Lift {
    /// The pass that writes buffers of format `code`: `x420` or `xf20`.
    pub(crate) fn new(code: u32) -> Result<Lift> {
        Ok(Lift {
            gpu: Gpu::shared()?,
            code,
            quant: plan::quantizer(code).context("the lift writes 10-bit 4:2:0 only")?,
            pool: Mutex::new(None),
            spent: Default::default(),
        })
    }

    /// The mean time one [`Self::convert`] took, in microseconds.
    pub(crate) fn mean_us(&self) -> u64 {
        let [frames, us] = &self.spent;
        us.load(Ordering::Relaxed) / frames.load(Ordering::Relaxed).max(1)
    }

    /// `src` (packed 10-bit RGB, PQ) as a new 10-bit 4:2:0 buffer, white lifted, tagged
    /// BT.2020 PQ. Returns once the GPU has written it.
    pub(crate) fn convert(&self, src: &CVPixelBuffer) -> Result<CFRetained<CVPixelBuffer>> {
        let start = Instant::now();
        let out = self.run(src)?;
        self.spent[0].fetch_add(1, Ordering::Relaxed);
        self.spent[1].fetch_add(start.elapsed().as_micros() as u64, Ordering::Relaxed);
        Ok(out)
    }

    fn run(&self, src: &CVPixelBuffer) -> Result<CFRetained<CVPixelBuffer>> {
        let (w, h) = (CVPixelBufferGetWidth(src), CVPixelBufferGetHeight(src));
        let out = self.buffer(w, h)?;
        let chroma = (
            CVPixelBufferGetWidthOfPlane(&out, 1),
            CVPixelBufferGetHeightOfPlane(&out, 1),
        );
        // Three textures are made per frame. The GPU wait dwarfs that; cache them by surface
        // if a profile ever names them.
        let from =
            CVPixelBufferGetIOSurface(Some(src)).context("the captured buffer has no surface")?;
        let to =
            CVPixelBufferGetIOSurface(Some(&out)).context("the output buffer has no surface")?;
        let source = texture(
            &self.gpu.device,
            Some((&from, 0)),
            MTLPixelFormat::BGR10A2Unorm,
            (w, h),
            false,
        )?;
        let luma = texture(
            &self.gpu.device,
            Some((&to, 0)),
            MTLPixelFormat::R16Unorm,
            (w, h),
            true,
        )?;
        let cbcr = texture(
            &self.gpu.device,
            Some((&to, 1)),
            MTLPixelFormat::RG16Unorm,
            chroma,
            true,
        )?;
        let commands = (self.gpu.queue.commandBuffer()).context("make a command buffer")?;
        let pass = commands
            .computeCommandEncoder()
            .context("make a compute pass")?;
        pass.setComputePipelineState(&self.gpu.pipeline);
        // SAFETY: four live textures at the indices the kernel declares, each retained past
        // `waitUntilCompleted`; `quant` is the kernel's 16-byte `Quant`, copied by the call.
        unsafe {
            for (index, t) in [&source, &self.gpu.lut, &luma, &cbcr]
                .into_iter()
                .enumerate()
            {
                pass.setTexture_atIndex(Some(t), index);
            }
            pass.setBytes_length_atIndex(
                NonNull::from(&self.quant).cast(),
                std::mem::size_of_val(&self.quant),
                0,
            );
        }
        let groups = MTLSize {
            width: chroma.0.div_ceil(GROUP),
            height: chroma.1.div_ceil(GROUP),
            depth: 1,
        };
        let side = MTLSize {
            width: GROUP,
            height: GROUP,
            depth: 1,
        };
        pass.dispatchThreadgroups_threadsPerThreadgroup(groups, side);
        pass.endEncoding();
        commands.commit();
        commands.waitUntilCompleted();
        if commands.status() != MTLCommandBufferStatus::Completed {
            let why = commands
                .error()
                .map(|e| e.localizedDescription().to_string());
            bail!(
                "the lift pass did not complete: {}",
                why.unwrap_or_default()
            );
        }
        // SAFETY: a live buffer no reader has seen yet; static keys and values.
        unsafe {
            for (key, value) in [
                (
                    kCVImageBufferColorPrimariesKey,
                    kCVImageBufferColorPrimaries_ITU_R_2020,
                ),
                (
                    kCVImageBufferTransferFunctionKey,
                    kCVImageBufferTransferFunction_SMPTE_ST_2084_PQ,
                ),
                (
                    kCVImageBufferYCbCrMatrixKey,
                    kCVImageBufferYCbCrMatrix_ITU_R_2020,
                ),
                (
                    kCVImageBufferChromaLocationTopFieldKey,
                    kCVImageBufferChromaLocation_Center,
                ),
            ] {
                out.set_attachment(key, value, CVAttachmentMode::ShouldPropagate);
            }
        }
        Ok(out)
    }

    /// A buffer of `w` by `h` from the pool, which is made again when the size changes.
    fn buffer(&self, w: usize, h: usize) -> Result<CFRetained<CVPixelBuffer>> {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if !matches!(&*pool, Some((pw, ph, _)) if (*pw, *ph) == (w, h)) {
            *pool = Some((w, h, new_pool(w, h, self.code)?));
        }
        let (_, _, pool) = pool.as_ref().context("no buffer pool")?;
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: a live pool and a valid out pointer.
        let status =
            unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut out)) };
        let buffer = NonNull::new(out).filter(|_| status == 0);
        // SAFETY: a successful create hands over one retain.
        buffer
            .map(|b| unsafe { CFRetained::from_raw(b) })
            .ok_or_else(|| anyhow!("take a {w}x{h} buffer from the pool: CVReturn {status}"))
    }
}

/// A pool of IOSurface-backed `code` buffers, which the GPU writes and the encoder reads
/// with no copy.
fn new_pool(w: usize, h: usize, code: u32) -> Result<CFRetained<CVPixelBufferPool>> {
    // An empty dictionary asks for default IOSurface properties.
    let surface = CFDictionary::<CFString, CFType>::empty();
    let numbers = [i64::from(code), w as i64, h as i64].map(CFNumber::new_i64);
    let surface: &CFType = surface.as_opaque().as_ref();
    // SAFETY: immutable keys CoreVideo exports.
    let keys = unsafe {
        [
            kCVPixelBufferPixelFormatTypeKey,
            kCVPixelBufferWidthKey,
            kCVPixelBufferHeightKey,
            kCVPixelBufferIOSurfacePropertiesKey,
        ]
    };
    let [format, width, height] = &numbers;
    let values: [&CFType; 4] = [format.as_ref(), width.as_ref(), height.as_ref(), surface];
    let attrs = CFDictionary::<CFString, CFType>::from_slices(&keys, &values);
    let mut out: *mut CVPixelBufferPool = std::ptr::null_mut();
    // SAFETY: an attributes dictionary of the documented types and a valid out pointer.
    let status = unsafe {
        CVPixelBufferPool::create(None, None, Some(attrs.as_opaque()), NonNull::from(&mut out))
    };
    let pool = NonNull::new(out).filter(|_| status == 0);
    // SAFETY: a successful create hands over one retain.
    pool.map(|p| unsafe { CFRetained::from_raw(p) })
        .ok_or_else(|| anyhow!("make a {w}x{h} buffer pool: CVReturn {status}"))
}

/// A 2D texture of `size`: on `plane` of a surface, or the device's own. `write` lets the
/// kernel write it.
fn texture(
    device: &ProtocolObject<dyn MTLDevice>,
    plane: Option<(&IOSurfaceRef, usize)>,
    format: MTLPixelFormat,
    size: (usize, usize),
    write: bool,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>> {
    // SAFETY: a plain descriptor for a 2D texture with no mipmaps.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            format, size.0, size.1, false,
        )
    };
    descriptor.setUsage(if write {
        MTLTextureUsage::ShaderWrite
    } else {
        MTLTextureUsage::ShaderRead
    });
    let made = match plane {
        Some((surface, index)) => {
            device.newTextureWithDescriptor_iosurface_plane(&descriptor, surface, index)
        }
        None => device.newTextureWithDescriptor(&descriptor),
    };
    made.with_context(|| format!("make a {}x{} {format:?} texture", size.0, size.1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
    };

    const X420: u32 = u32::from_be_bytes(*b"x420");
    const L10R: u32 = u32::from_be_bytes(*b"l10r");

    /// The pass, or `None` on a machine with no GPU to run it.
    fn lift() -> Option<Lift> {
        let lift = Lift::new(X420);
        if let Err(e) = &lift {
            eprintln!("skipped: {e:#}");
        }
        lift.ok()
    }

    /// A packed 10-bit RGB buffer of `w` by `h`, each pixel `rgb(x)`.
    fn source(w: usize, h: usize, rgb: impl Fn(usize) -> [u32; 3]) -> CFRetained<CVPixelBuffer> {
        let surface = CFDictionary::<CFString, CFType>::empty();
        // SAFETY: an immutable key CoreVideo exports.
        let key = unsafe { kCVPixelBufferIOSurfacePropertiesKey };
        let attrs =
            CFDictionary::<CFString, CFDictionary>::from_slices(&[key], &[surface.as_opaque()]);
        let mut out: *mut CVPixelBuffer = std::ptr::null_mut();
        // SAFETY: a plain create; the buffer is locked across the writes, each inside its row.
        unsafe {
            let made = CVPixelBufferCreate(
                None,
                w,
                h,
                L10R,
                Some(attrs.as_opaque()),
                NonNull::from(&mut out),
            );
            assert_eq!(made, 0);
            let buffer = CFRetained::from_raw(NonNull::new(out).unwrap());
            CVPixelBufferLockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            let base = CVPixelBufferGetBaseAddress(&buffer).cast::<u8>();
            let pitch = CVPixelBufferGetBytesPerRow(&buffer);
            for y in 0..h {
                for x in 0..w {
                    let [r, g, b] = rgb(x);
                    let px = (3 << 30) | (r << 20) | (g << 10) | b;
                    base.add(y * pitch + x * 4)
                        .cast::<u32>()
                        .write_unaligned(px);
                }
            }
            CVPixelBufferUnlockBaseAddress(&buffer, CVPixelBufferLockFlags(0));
            buffer
        }
    }

    /// The 10-bit `(Y', Cb, Cr)` at pixel `(x, y)` of a 10-bit 4:2:0 buffer.
    fn read(buffer: &CVPixelBuffer, x: usize, y: usize) -> [u16; 3] {
        // SAFETY: the buffer is locked across the reads, each inside its plane.
        unsafe {
            CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
            let luma = CVPixelBufferGetBaseAddressOfPlane(buffer, 0).cast::<u8>();
            let chroma = CVPixelBufferGetBaseAddressOfPlane(buffer, 1).cast::<u8>();
            let lp = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0);
            let cp = CVPixelBufferGetBytesPerRowOfPlane(buffer, 1);
            let at =
                |base: *mut u8, offset: usize| base.add(offset).cast::<u16>().read_unaligned() >> 6;
            let out = [
                at(luma, y * lp + x * 2),
                at(chroma, (y / 2) * cp + (x / 2) * 4),
                at(chroma, (y / 2) * cp + (x / 2) * 4 + 2),
            ];
            CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
            out
        }
    }

    /// What the kernel must write for a pixel of 10-bit PQ `rgb`, by the same arithmetic.
    fn expected(rgb: [u32; 3]) -> [f64; 3] {
        let lut = plan::lift_white();
        let [r, g, b] = rgb.map(|c| f64::from(lut[c as usize]) / 65535.0);
        let y = 0.2627 * r + 0.6780 * g + 0.0593 * b;
        [
            64.0 + 876.0 * y,
            512.0 + 896.0 * (b - y) / 1.8814,
            512.0 + 896.0 * (r - y) / 1.4746,
        ]
    }

    /// The kernel on a real GPU: white moves to 203 nits, grey stays neutral, red stays red.
    #[test]
    fn the_pass_lifts_white_and_keeps_colour() {
        let Some(lift) = lift() else { return };
        let white = (plan::pq_signal(100.0) * 1023.0).round() as u32;
        // Sixteen-pixel columns, so each 2x2 chroma block holds one colour.
        let colours = [[0, 0, 0], [white; 3], [white, 0, 0], [300, 520, 700]];
        let out = lift.convert(&source(64, 16, |x| colours[x / 16])).unwrap();
        for (i, rgb) in colours.into_iter().enumerate() {
            let got = read(&out, i * 16 + 8, 8);
            for (plane, (got, want)) in got.into_iter().zip(expected(rgb)).enumerate() {
                let off = (f64::from(got) - want).abs();
                assert!(off <= 1.0, "colour {i} plane {plane}: {got} vs {want}");
            }
        }
        // White: the 10-bit studio code of 203 nits, and no colour.
        let code = (64.0 + 876.0 * plan::pq_signal(203.0)).round() as i32;
        let [y, cb, cr] = read(&out, 24, 8).map(i32::from);
        assert!((y - code).abs() <= 1, "white is {y}, want {code}");
        assert_eq!((cb, cr), (512, 512), "white has no colour");
    }

    /// An odd size: the last column and row have half a chroma block, and still get written.
    #[test]
    fn an_odd_size_is_written_to_its_edge() {
        let Some(lift) = lift() else { return };
        let white = (plan::pq_signal(100.0) * 1023.0).round() as u32;
        let out = lift.convert(&source(33, 17, |_| [white; 3])).unwrap();
        let want = expected([white; 3]).map(|v| v.round() as u16);
        assert_eq!(read(&out, 32, 16), want);
        assert_eq!(read(&out, 0, 0), want);
    }

    /// What one frame costs on this machine's GPU. Run by hand:
    /// `cargo test -p pf-capture --release -- --ignored --nocapture the_pass_takes`
    #[test]
    #[ignore = "a timing, not a check"]
    fn the_pass_takes() {
        let Some(lift) = lift() else { return };
        for (w, h) in [(1920, 1080), (3840, 2160), (5120, 1440)] {
            let src = source(w, h, |x| [(x % 1024) as u32; 3]);
            for _ in 0..20 {
                lift.convert(&src).unwrap();
            }
            // A stream holds a few frames at a time, so the pool hands buffers back out.
            let mut held = std::collections::VecDeque::new();
            let start = Instant::now();
            for _ in 0..200 {
                held.push_back(lift.convert(&src).unwrap());
                if held.len() > 4 {
                    held.pop_front();
                }
            }
            let us = start.elapsed().as_micros() / 200;
            eprintln!("{w}x{h}: {us} us a frame");
        }
    }
}
