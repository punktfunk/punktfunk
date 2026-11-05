//! A LINEAR dmabuf as a CUDA device pointer, with no copy, for NVENC to encode in place.
//!
//! The buffer maps through the memory object NVIDIA's driver holds behind it ([`rm`]), which
//! keeps the allocation's own page kind. Where that export is refused, it imports as one
//! `EGLImage` over every plane instead: an `AR24` view `row_bytes / 4` wide and `rows` tall at
//! the buffer's stride, which `cuGraphicsEGLRegisterImage` maps as one pitched allocation.
//! Either way NVENC registers one allocation as NV12, P010 or packed RGB at that pitch; a
//! planar import maps each plane on its own, and NVENC refuses chroma outside the luma's.
//!
//! In-process: CUDA cannot share an EGL mapping with another process (`cuIpcGetMemHandle`
//! refuses it). The driver sees each pool buffer once; a frame only touches NVENC.

use super::cuda::{self, CUdeviceptr, CUeglFrame, CUgraphicsResource, CU_EGL_FRAME_TYPE_PITCH};
use super::drm;
use super::egl::{self as pegl, Egl};
use super::gbm::GbmDevice;
use super::rm;
use anyhow::{bail, ensure, Context as _, Result};
use khronos_egl as egl;
use std::cell::Cell;
use std::os::fd::{AsRawFd as _, BorrowedFd};
use std::rc::Rc;

/// Where a buffer's bytes sit: `rows` rows of `row_bytes` at `stride`, from `offset`. A planar
/// layout counts every plane's rows, contiguous under one stride.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Span {
    pub offset: u32,
    pub stride: u32,
    pub row_bytes: u32,
    pub rows: u32,
}

impl Span {
    /// Width of the `AR24` view that covers `row_bytes`; it must fit the stride.
    fn view_width(&self) -> Result<u32> {
        let width = self.row_bytes.div_ceil(4);
        ensure!(
            self.rows > 0 && width > 0 && u64::from(width) * 4 <= u64::from(self.stride),
            "a {}-byte row does not fit a {}-byte stride",
            self.row_bytes,
            self.stride
        );
        Ok(width)
    }
}

/// `egl` before `gbm`: fields drop in order, and the display lives on the GBM device.
struct Display {
    egl: Egl,
    display: egl::Display,
    gbm: GbmDevice,
}

impl Drop for Display {
    /// Every image on the display is gone: each [`Mapping`] holds its `Rc`. Terminated before
    /// the GBM device goes, so a later display on a reused `gbm_device*` starts fresh.
    fn drop(&mut self) {
        let _ = self.egl.terminate(self.display);
    }
}

/// The render node and GBM `EGLDisplay` the mappings import through. Thread-affine with them.
/// `rm`: whether the driver exports a dmabuf's memory object; one refusal settles it.
pub struct DirectImporter {
    display: Rc<Display>,
    rm: Cell<Option<bool>>,
}

/// One buffer mapped into CUDA. `ptr` stays valid until this drops.
pub struct Mapping {
    pub ptr: CUdeviceptr,
    backing: Backing,
    display: Rc<Display>,
}

/// What holds the mapping: the buffer's own memory object, or an EGL image registered with CUDA.
enum Backing {
    Rm {
        _memory: cuda::ExternalDmabuf,
    },
    Egl {
        resource: CUgraphicsResource,
        image: egl::Image,
    },
}

impl DirectImporter {
    pub fn new() -> Result<DirectImporter> {
        let (egl, display, gbm, _) = pegl::gbm_display()?;
        Ok(DirectImporter {
            display: Rc::new(Display { egl, display, gbm }),
            rm: Cell::new(None),
        })
    }

    /// Map `span` of the LINEAR dmabuf `fd`. The shared CUDA context is made current here.
    pub fn map(&self, fd: BorrowedFd<'_>, span: Span) -> Result<Mapping> {
        let width = span.view_width()?;
        let d = &*self.display;
        if self.rm.get() != Some(false) && super::nvenc_rm_import_enabled() {
            cuda::make_current()?;
            match rm::import(d.gbm.node(), fd) {
                Ok((ext, object)) => {
                    let end =
                        u64::from(span.offset) + u64::from(span.rows) * u64::from(span.stride);
                    ensure!(
                        end <= ext.size,
                        "a span ending at byte {end} does not fit the {}-byte buffer",
                        ext.size
                    );
                    if self.rm.replace(Some(true)).is_none() {
                        tracing::info!(
                            ?object,
                            "in-place NVENC reads the dmabuf through its own memory object"
                        );
                    }
                    return Ok(Mapping {
                        ptr: ext.ptr + CUdeviceptr::from(span.offset),
                        backing: Backing::Rm { _memory: ext },
                        display: self.display.clone(),
                    });
                }
                Err(e) => {
                    self.rm.set(Some(false));
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        "dmabuf memory object export refused — in-place NVENC reads through EGL"
                    );
                }
            }
        }
        let attrs = [
            egl::WIDTH as egl::Attrib,
            width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            span.rows as egl::Attrib,
            pegl::EGL_LINUX_DRM_FOURCC_EXT,
            drm::AR24 as egl::Attrib,
            pegl::EGL_DMA_BUF_PLANE0_FD_EXT,
            fd.as_raw_fd() as egl::Attrib,
            pegl::EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            span.offset as egl::Attrib,
            pegl::EGL_DMA_BUF_PLANE0_PITCH_EXT,
            span.stride as egl::Attrib,
            pegl::EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
            0,
            pegl::EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
            0,
            egl::ATTRIB_NONE,
        ];
        // SAFETY: a dma-buf import takes a NULL client buffer; `from_ptr` only stores it.
        let client = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        // SAFETY: `NO_CONTEXT` is the null sentinel a dma-buf import requires.
        let no_ctx = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        let image = d
            .egl
            .create_image(
                d.display,
                no_ctx,
                pegl::EGL_LINUX_DMA_BUF_EXT,
                client,
                &attrs,
            )
            .context("eglCreateImage (LINEAR dma-buf as AR24)")?;
        if let Err(e) = cuda::make_current() {
            let _ = d.egl.destroy_image(d.display, image);
            return Err(e);
        }
        let mut resource: CUgraphicsResource = std::ptr::null_mut();
        const READ_ONLY: u32 = 0x01;
        // SAFETY: `image` is the live EGLImage above, the shared context is current, and
        // `resource` is a live out-param.
        let r =
            unsafe { cuda::cuGraphicsEGLRegisterImage(&mut resource, image.as_ptr(), READ_ONLY) };
        if r != 0 {
            let _ = d.egl.destroy_image(d.display, image);
            bail!("cuGraphicsEGLRegisterImage: CUDA error {r}");
        }
        // From here `Drop` releases the registration and the image.
        let mut mapping = Mapping {
            ptr: 0,
            backing: Backing::Egl { resource, image },
            display: self.display.clone(),
        };
        // SAFETY: all-zero is a valid `CUeglFrame` (null pointers, zero counts).
        let mut frame: CUeglFrame = unsafe { std::mem::zeroed() };
        // SAFETY: `resource` is the registration above; `frame` is a live out-param.
        let r = unsafe { cuda::cuGraphicsResourceGetMappedEglFrame(&mut frame, resource, 0, 0) };
        ensure!(
            r == 0,
            "cuGraphicsResourceGetMappedEglFrame: CUDA error {r}"
        );
        ensure!(
            frame.frame_type == CU_EGL_FRAME_TYPE_PITCH
                && frame.plane_count == 1
                && frame.pitch == span.stride
                && !frame.planes[0].is_null(),
            "EGL frame is not one pitched plane at the buffer's stride (type {}, {} planes, \
             pitch {})",
            frame.frame_type,
            frame.plane_count,
            frame.pitch
        );
        mapping.ptr = frame.planes[0] as CUdeviceptr;
        Ok(mapping)
    }
}

impl Drop for Mapping {
    /// An `Rm` backing frees itself. An EGL one unregisters before the image goes: CUDA still
    /// references it.
    fn drop(&mut self) {
        let Backing::Egl { resource, image } = self.backing else {
            return;
        };
        if cuda::make_current().is_ok() {
            // SAFETY: the registration from `map`, released once.
            unsafe { cuda::cuGraphicsUnregisterResource(resource) };
        }
        let d = &*self.display;
        let _ = d.egl.destroy_image(d.display, image);
    }
}

#[cfg(test)]
mod tests {
    use super::{DirectImporter, Span};

    #[test]
    #[ignore = "requires an NVIDIA GPU + driver — run on the RTX box (.21)"]
    fn the_display_opens_again_after_one_drops() {
        for _ in 0..3 {
            drop(DirectImporter::new().expect("open the GBM import display"));
        }
    }

    #[test]
    fn the_view_covers_the_row_and_fits_the_stride() {
        let span = |row_bytes, stride| Span {
            offset: 0,
            stride,
            row_bytes,
            rows: 3255,
        };
        // NV12 3456 wide: 864 AR24 pixels. P010: twice the bytes.
        assert_eq!(span(3456, 3584).view_width().unwrap(), 864);
        assert_eq!(span(6912, 7168).view_width().unwrap(), 1728);
        // A row that is not a multiple of four rounds up, and must still fit.
        assert_eq!(span(1366, 1536).view_width().unwrap(), 342);
        assert!(span(1366, 1366).view_width().is_err());
        assert!(span(0, 256).view_width().is_err());
    }
}
