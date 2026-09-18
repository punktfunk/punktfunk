//! Headless NVIDIA EGL importer: GBM display on the render node, PipeWire dmabuf → `EGLImage`
//! (`EGL_LINUX_DMA_BUF_EXT`). The DRM modifier is mandatory — NVIDIA buffers are tiled; omitting
//! it is `EGL_BAD_MATCH` or a corrupt image.
//!
//! Desktop NVIDIA cannot register a dmabuf `EGLImage` with CUDA (`cuGraphicsEGLRegisterImage` is
//! Tegra-only; `cuGraphicsGLRegisterImage` rejects EGLImage-backed textures). Bind the image to a
//! GL texture (`glEGLImageTargetTexture2DOES`), blit into an immutable `GL_RGBA8` (or NV12 / YUV444
//! convert targets), register that texture, then device-copy into an owned [`DeviceBuffer`] so the
//! dmabuf can return to the compositor immediately.
//!
//! Pin: `picks_the_nvidia_node_not_the_first_one`. LINEAR dmabufs go through [`super::vulkan`].

#![allow(non_upper_case_globals)]

use super::cuda::{self, DeviceBuffer};
use anyhow::{ensure, Context as _, Result};
use khronos_egl as egl;
use std::os::fd::{AsRawFd as _, FromRawFd as _};
use std::os::raw::{c_int, c_void};

// Not in khronos-egl: EGL_EXT_image_dma_buf_import(_modifiers) and the GBM platform enum.
const EGL_LINUX_DMA_BUF_EXT: egl::Enum = 0x3270;
const EGL_PLATFORM_GBM_KHR: egl::Enum = 0x31D7;
const EGL_LINUX_DRM_FOURCC_EXT: egl::Attrib = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: egl::Attrib = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: egl::Attrib = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: egl::Attrib = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: egl::Attrib = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: egl::Attrib = 0x3444;

#[path = "egl/gl.rs"]
mod gl;
use gl::*;

/// NVIDIA PCI vendor. This importer and the Vulkan bridge both key the physical device on it.
const PCI_VENDOR_NVIDIA: u32 = 0x10de;

/// NVIDIA DRM render node: `PUNKTFUNK_ZEROCOPY_RENDER_NODE`, else the first `/dev/dri/renderD*`
/// whose sysfs PCI vendor is NVIDIA, else `/dev/dri/renderD128`.
///
/// Scan by vendor, not first node: on a hybrid host `renderD128` is the iGPU. Do not call
/// `pf_gpu::linux_render_node` — this crate is a leaf worker, and that helper follows the
/// operator VAAPI preference, which may name the iGPU. The question here is where CUDA lives.
fn nvidia_render_node() -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    if let Some(p) = std::env::var_os("PUNKTFUNK_ZEROCOPY_RENDER_NODE").filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    // No NVIDIA node (or no /sys): keep `/dev/dri/renderD128`. CUDA construction fails if it is wrong.
    nvidia_render_node_in(Path::new("/dev/dri"), Path::new("/sys/class/drm"))
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"))
}

/// Scan half of [`nvidia_render_node`]. Roots are parameters so tests can pin
/// `<sys_class_drm>/<node>/device/vendor`. Name order keeps the pick stable across boots.
fn nvidia_render_node_in(
    dri: &std::path::Path,
    sys_class_drm: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut nodes: Vec<std::ffi::OsString> = std::fs::read_dir(dri)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .filter(|n| n.as_encoded_bytes().starts_with(b"renderD"))
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();
    nodes
        .into_iter()
        .find(|node| {
            std::fs::read_to_string(sys_class_drm.join(node).join("device").join("vendor"))
                .ok()
                .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
                == Some(PCI_VENDOR_NVIDIA)
        })
        .map(|node| dri.join(node))
}

/// GL names created mid-constructor, deleted on unwind if the struct never takes ownership.
/// `defuse()` hands them to the built struct's `Drop`. Declare this guard *before* any
/// `RegisteredTexture` local so CUDA unregisters before `glDelete*` on unwind.
#[derive(Default)]
struct GlNameGuard {
    textures: Vec<u32>,
    fbos: Vec<u32>,
    vaos: Vec<u32>,
    programs: Vec<u32>,
}

impl GlNameGuard {
    fn defuse(mut self) {
        self.textures.clear();
        self.fbos.clear();
        self.vaos.clear();
        self.programs.clear();
    }
}

impl Drop for GlNameGuard {
    fn drop(&mut self) {
        // SAFETY: each name was created on the GL context still current on this thread
        // (constructors run and unwind on the capture thread). `glDelete*` n=1, pointer to one
        // live element. Names here were never `defuse`d, so each is deleted exactly once.
        unsafe {
            for t in &self.textures {
                glDeleteTextures(1, t);
            }
            for f in &self.fbos {
                glDeleteFramebuffers(1, f);
            }
            for v in &self.vaos {
                glDeleteVertexArrays(1, v);
            }
            for &p in &self.programs {
                glDeleteProgram(p);
            }
        }
    }
}

/// GBM device plus the render-node fd it borrows. Drop order is destroy-device then close-fd.
/// `EglImporter` has no `Drop`, so this field is last: GL/CUDA objects release against a live display.
struct GbmDevice {
    raw: *mut c_void,
    _fd: std::os::fd::OwnedFd,
}

impl Drop for GbmDevice {
    fn drop(&mut self) {
        // SAFETY: `raw` is the non-null `gbm_device*` from `gbm_create_device`, owned exclusively
        // here and destroyed once — before `_fd` (the borrowed render-node fd) closes.
        unsafe { gbm_device_destroy(self.raw) };
    }
}

/// Per-size blit: dmabuf EGLImage → CUDA-registrable `GL_RGBA8`.
struct GlBlit {
    program: u32,
    vao: u32,
    fbo: u32,
    dst_tex: u32,
    /// Retargeted to each frame's EGLImage.
    src_tex: u32,
    width: u32,
    height: u32,
    /// `dst_tex` registered once; mapped and copied each frame.
    registered: cuda::RegisteredTexture,
    pool: cuda::BufferPool,
}

impl GlBlit {
    unsafe fn new(width: u32, height: u32) -> Result<GlBlit> {
        // SAFETY: caller contract (`import_inner`): GL and the shared CUDA context are current
        // on this thread. GL calls pass live locals; every created name is owned by `guard`
        // until the struct exists.
        unsafe {
            // Guard first so it drops last on unwind, after CUDA unregisters.
            let mut guard = GlNameGuard::default();
            let program = compile_program()?;
            guard.programs.push(program);
            let mut vao = 0u32;
            glGenVertexArrays(1, &mut vao); // core profile: glDrawArrays needs a bound VAO
            guard.vaos.push(vao);
            let mut fbo = 0u32;
            glGenFramebuffers(1, &mut fbo);
            guard.fbos.push(fbo);

            let mut dst_tex = 0u32;
            glGenTextures(1, &mut dst_tex);
            guard.textures.push(dst_tex);
            glBindTexture(GL_TEXTURE_2D, dst_tex);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, width as c_int, height as c_int);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

            let mut src_tex = 0u32;
            glGenTextures(1, &mut src_tex);
            guard.textures.push(src_tex);
            glBindTexture(GL_TEXTURE_2D, src_tex);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glBindTexture(GL_TEXTURE_2D, 0);

            glBindFramebuffer(GL_FRAMEBUFFER, fbo);
            glFramebufferTexture2D(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                dst_tex,
                0,
            );
            let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            ensure!(
                status == GL_FRAMEBUFFER_COMPLETE,
                "blit FBO incomplete ({status:#x})"
            );
            let registered = cuda::RegisteredTexture::register_gl(dst_tex)?;
            let pool = cuda::BufferPool::new(width, height)?;
            guard.defuse();
            Ok(GlBlit {
                program,
                vao,
                fbo,
                dst_tex,
                src_tex,
                width,
                height,
                registered,
                pool,
            })
        }
    }

    /// # Safety: the GL context is current on this thread; `image` is a valid `EGLImage`.
    unsafe fn run(&self, egl_image_target: EglImageTargetFn, image: *mut c_void) -> Result<()> {
        // SAFETY: caller contract (`# Safety` above): GL context current, `image` a valid EGLImage.
        // Raw GL calls pass names owned by `self`, created on this same context.
        unsafe {
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            let _ = glGetError();
            egl_image_target(GL_TEXTURE_2D, image);
            let e = glGetError();
            glBindTexture(GL_TEXTURE_2D, 0);
            ensure!(e == 0, "glEGLImageTargetTexture2DOES failed ({e:#x})");

            glBindFramebuffer(GL_FRAMEBUFFER, self.fbo);
            glViewport(0, 0, self.width as c_int, self.height as c_int);
            glUseProgram(self.program);
            glActiveTexture(GL_TEXTURE0);
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            glBindVertexArray(self.vao);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glBindVertexArray(0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            glFlush(); // GL must finish before CUDA maps the texture
            Ok(())
        }
    }
}

impl Drop for GlBlit {
    fn drop(&mut self) {
        // Unregister CUDA before `glDelete*` on the texture it wraps — same hazard as `Nv12Blit::drop`.
        self.registered.release();
        // SAFETY: these names were created by this `GlBlit` on the GL context still current here
        // (`EglImporter` never releases it; capture thread; no `Drop` of its own, so this field
        // drops before `GbmDevice`). Each `glDelete*` is n=1 on a live field, after CUDA release.
        unsafe {
            glDeleteTextures(1, &self.dst_tex);
            glDeleteTextures(1, &self.src_tex);
            glDeleteFramebuffers(1, &self.fbo);
            glDeleteVertexArrays(1, &self.vao);
            glDeleteProgram(self.program);
        }
    }
}

/// Per-size NV12 convert (BT.709 limited): full-res Y into `GL_R8`, half-res UV into `GL_RG8`.
/// Native NV12 lets NVENC skip its RGB→YUV CSC. Replaces the BGRx swizzle [`GlBlit`] did.
struct Nv12Blit {
    y_program: u32,
    uv_program: u32,
    vao: u32,
    y_fbo: u32,
    uv_fbo: u32,
    /// Immutable `GL_R8` luma, W×H.
    y_tex: u32,
    /// Immutable `GL_RG8` chroma, W/2 × H/2.
    uv_tex: u32,
    /// Retargeted per frame. `GL_LINEAR` so the UV pass's two taps each average two texels.
    src_tex: u32,
    width: u32,
    height: u32,
    y_registered: cuda::RegisteredTexture,
    uv_registered: cuda::RegisteredTexture,
    pool: cuda::BufferPool,
    /// Test path only: `src_tex` already has immutable RGBA8 storage. Live path retargets via EGLImage.
    test_src_storage: bool,
}

impl Nv12Blit {
    unsafe fn new(width: u32, height: u32) -> Result<Nv12Blit> {
        // SAFETY: caller contract (`import_inner`): GL and the shared CUDA context are current
        // on this thread. GL calls pass live locals; every created name is owned by `guard`
        // until the struct exists.
        unsafe {
            ensure!(
                width % 2 == 0 && height % 2 == 0,
                "NV12 convert needs even dimensions (got {width}x{height})"
            );
            // Guard first so it drops last on unwind, after CUDA unregisters.
            let mut guard = GlNameGuard::default();
            let y_program = compile_program_with(FRAG_Y_SRC)?;
            guard.programs.push(y_program);
            let uv_program = compile_program_with(FRAG_UV_SRC)?;
            guard.programs.push(uv_program);
            let mut vao = 0u32;
            glGenVertexArrays(1, &mut vao);
            guard.vaos.push(vao);
            let mut fbos = [0u32; 2];
            glGenFramebuffers(2, fbos.as_mut_ptr());
            guard.fbos.extend_from_slice(&fbos);
            let (y_fbo, uv_fbo) = (fbos[0], fbos[1]);

            let mut y_tex = 0u32;
            glGenTextures(1, &mut y_tex);
            guard.textures.push(y_tex);
            glBindTexture(GL_TEXTURE_2D, y_tex);
            glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, width as c_int, height as c_int);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

            // GL_RG8 half-res: R=U, G=V.
            let mut uv_tex = 0u32;
            glGenTextures(1, &mut uv_tex);
            guard.textures.push(uv_tex);
            glBindTexture(GL_TEXTURE_2D, uv_tex);
            glTexStorage2D(
                GL_TEXTURE_2D,
                1,
                GL_RG8,
                (width / 2) as c_int,
                (height / 2) as c_int,
            );
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);

            let mut src_tex = 0u32;
            glGenTextures(1, &mut src_tex);
            guard.textures.push(src_tex);
            glBindTexture(GL_TEXTURE_2D, src_tex);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glBindTexture(GL_TEXTURE_2D, 0);

            for (fbo, tex) in [(y_fbo, y_tex), (uv_fbo, uv_tex)] {
                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
                let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
                glBindFramebuffer(GL_FRAMEBUFFER, 0);
                ensure!(
                    status == GL_FRAMEBUFFER_COMPLETE,
                    "NV12 blit FBO incomplete ({status:#x}) — GL_R8/GL_RG8 not renderable?"
                );
            }
            let y_registered = cuda::RegisteredTexture::register_gl(y_tex)?;
            let uv_registered = cuda::RegisteredTexture::register_gl(uv_tex)?;
            let pool = cuda::BufferPool::new_nv12(width, height)?;
            guard.defuse();
            Ok(Nv12Blit {
                y_program,
                uv_program,
                vao,
                y_fbo,
                uv_fbo,
                y_tex,
                uv_tex,
                src_tex,
                width,
                height,
                y_registered,
                uv_registered,
                pool,
                test_src_storage: false,
            })
        }
    }

    /// # Safety: the GL context is current on this thread; `image` is a valid `EGLImage`.
    unsafe fn run(&self, egl_image_target: EglImageTargetFn, image: *mut c_void) -> Result<()> {
        // SAFETY: caller contract (`# Safety` above): GL context current, `image` a valid EGLImage.
        // Raw GL calls pass names owned by `self`, created on this same context.
        unsafe {
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            let _ = glGetError();
            egl_image_target(GL_TEXTURE_2D, image);
            let e = glGetError();
            glBindTexture(GL_TEXTURE_2D, 0);
            ensure!(e == 0, "glEGLImageTargetTexture2DOES failed ({e:#x})");
            self.run_passes()
        }
    }

    /// Convert from whatever currently sits in `src_tex` (EGLImage bind or test upload).
    ///
    /// # Safety: the GL context is current on this thread.
    unsafe fn run_passes(&self) -> Result<()> {
        // SAFETY: caller contract (`# Safety` above): GL context current. Raw GL calls pass names
        // owned by `self`, created on this same context.
        unsafe {
            glActiveTexture(GL_TEXTURE0);
            glBindVertexArray(self.vao);
            glBindFramebuffer(GL_FRAMEBUFFER, self.y_fbo);
            glViewport(0, 0, self.width as c_int, self.height as c_int);
            glUseProgram(self.y_program);
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            glDrawArrays(GL_TRIANGLES, 0, 3);
            glBindFramebuffer(GL_FRAMEBUFFER, self.uv_fbo);
            glViewport(0, 0, (self.width / 2) as c_int, (self.height / 2) as c_int);
            glUseProgram(self.uv_program);
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            glDrawArrays(GL_TRIANGLES, 0, 3);

            glBindVertexArray(0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            glFlush(); // GL must finish before CUDA maps the textures
            Ok(())
        }
    }
}

impl Drop for Nv12Blit {
    fn drop(&mut self) {
        // Unregister CUDA before `glDelete*` — `Drop::drop` runs before field drops, so deleting
        // first would leave a registration on freed GL state.
        self.y_registered.release();
        self.uv_registered.release();
        // SAFETY: names created by this `Nv12Blit` on the GL context still current (`EglImporter`
        // never releases it; capture thread; this field drops before `GbmDevice`). `glDelete*` n=1
        // on a live `&u32`; `[y_fbo, uv_fbo].as_ptr()` is a 2-element temporary that lives for the
        // call (n=2). Each name is deleted once, after CUDA release above.
        unsafe {
            glDeleteTextures(1, &self.y_tex);
            glDeleteTextures(1, &self.uv_tex);
            glDeleteTextures(1, &self.src_tex);
            glDeleteFramebuffers(2, [self.y_fbo, self.uv_fbo].as_ptr());
            glDeleteVertexArrays(1, &self.vao);
            glDeleteProgram(self.y_program);
            glDeleteProgram(self.uv_program);
        }
    }
}

/// `PUNKTFUNK_444_FULLRANGE=1`: the YUV444 convert writes full range. The encoder signals the
/// same answer in the VUI, so both read it here.
pub fn yuv444_full_range() -> bool {
    std::env::var("PUNKTFUNK_444_FULLRANGE").is_ok_and(|v| v.trim() == "1")
}

/// Per-size planar YUV444 convert (BT.709; studio or full range via `PUNKTFUNK_444_FULLRANGE`).
/// Three full-res `GL_R8` passes share `src_tex`. The pool is one stacked allocation
/// (`BufferPool::new_yuv444`) so the worker↔host wire stays single-plane.
struct Yuv444Blit {
    programs: [u32; 3],
    vao: u32,
    fbos: [u32; 3],
    /// Full-res `GL_R8` targets: Y, U, V.
    texs: [u32; 3],
    /// Retargeted to each frame's EGLImage.
    src_tex: u32,
    width: u32,
    height: u32,
    registered: [cuda::RegisteredTexture; 3],
    pool: cuda::BufferPool,
}

impl Yuv444Blit {
    unsafe fn new(width: u32, height: u32) -> Result<Yuv444Blit> {
        // SAFETY: caller contract (`import_inner`): GL and the shared CUDA context are current
        // on this thread. GL calls pass live locals; every created name is owned by `guard`
        // until the struct exists.
        unsafe {
            ensure!(
                width % 2 == 0 && height % 2 == 0,
                "YUV444 convert needs even dimensions (got {width}x{height})"
            );
            let full_range = yuv444_full_range();
            let (y_src, u_src, v_src) = yuv444_frag_sources(full_range);
            // Guard first so it drops last on unwind, after CUDA unregisters.
            let mut guard = GlNameGuard::default();
            let mut programs = [0u32; 3];
            for (p, src) in programs.iter_mut().zip([&y_src, &u_src, &v_src]) {
                *p = compile_program_with(src)?;
                guard.programs.push(*p);
            }
            let mut vao = 0u32;
            glGenVertexArrays(1, &mut vao);
            guard.vaos.push(vao);
            let mut fbos = [0u32; 3];
            glGenFramebuffers(3, fbos.as_mut_ptr());
            guard.fbos.extend_from_slice(&fbos);
            let mut texs = [0u32; 3];
            glGenTextures(3, texs.as_mut_ptr());
            guard.textures.extend_from_slice(&texs);
            for &tex in &texs {
                glBindTexture(GL_TEXTURE_2D, tex);
                glTexStorage2D(GL_TEXTURE_2D, 1, GL_R8, width as c_int, height as c_int);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
            }
            // LINEAR is exact at 1:1 (texel centres), matching Nv12Blit.
            let mut src_tex = 0u32;
            glGenTextures(1, &mut src_tex);
            guard.textures.push(src_tex);
            glBindTexture(GL_TEXTURE_2D, src_tex);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glBindTexture(GL_TEXTURE_2D, 0);
            for (&fbo, &tex) in fbos.iter().zip(&texs) {
                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
                let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
                glBindFramebuffer(GL_FRAMEBUFFER, 0);
                ensure!(
                    status == GL_FRAMEBUFFER_COMPLETE,
                    "YUV444 blit FBO incomplete ({status:#x}) — GL_R8 not renderable?"
                );
            }
            let registered = [
                cuda::RegisteredTexture::register_gl(texs[0])?,
                cuda::RegisteredTexture::register_gl(texs[1])?,
                cuda::RegisteredTexture::register_gl(texs[2])?,
            ];
            let pool = cuda::BufferPool::new_yuv444(width, height)?;
            guard.defuse();
            if full_range {
                tracing::info!("YUV444 zero-copy convert: FULL range (PUNKTFUNK_444_FULLRANGE=1)");
            }
            Ok(Yuv444Blit {
                programs,
                vao,
                fbos,
                texs,
                src_tex,
                width,
                height,
                registered,
                pool,
            })
        }
    }

    /// # Safety: the GL context is current on this thread; `image` is a valid `EGLImage`.
    unsafe fn run(&self, egl_image_target: EglImageTargetFn, image: *mut c_void) -> Result<()> {
        // SAFETY: caller contract (`# Safety` above): GL context current, `image` a valid EGLImage.
        // Raw GL calls pass names owned by `self`, created on this same context.
        unsafe {
            glBindTexture(GL_TEXTURE_2D, self.src_tex);
            let _ = glGetError();
            egl_image_target(GL_TEXTURE_2D, image);
            let e = glGetError();
            glBindTexture(GL_TEXTURE_2D, 0);
            ensure!(e == 0, "glEGLImageTargetTexture2DOES failed ({e:#x})");
            glActiveTexture(GL_TEXTURE0);
            glBindVertexArray(self.vao);
            for (&fbo, &program) in self.fbos.iter().zip(&self.programs) {
                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                glViewport(0, 0, self.width as c_int, self.height as c_int);
                glUseProgram(program);
                glBindTexture(GL_TEXTURE_2D, self.src_tex);
                glDrawArrays(GL_TRIANGLES, 0, 3);
            }
            glBindVertexArray(0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            glFlush(); // GL must finish before CUDA maps the textures
            Ok(())
        }
    }
}

impl Drop for Yuv444Blit {
    fn drop(&mut self) {
        // Unregister CUDA before `glDelete*` — same teardown-order hazard as `Nv12Blit::drop`.
        for r in &mut self.registered {
            r.release();
        }
        // SAFETY: names created by this `Yuv444Blit` on the GL context still current (`EglImporter`
        // never releases it; capture thread; this field drops before `GbmDevice`). `glDelete*` n
        // matches live arrays/fields. Each name is deleted once, after CUDA release above.
        unsafe {
            glDeleteTextures(3, self.texs.as_ptr());
            glDeleteTextures(1, &self.src_tex);
            glDeleteFramebuffers(3, self.fbos.as_ptr());
            glDeleteVertexArrays(1, &self.vao);
            for &p in &self.programs {
                glDeleteProgram(p);
            }
        }
    }
}

/// GPU conversion `import_inner` runs on the de-tiled EGLImage; mirrors tiled [`super::proto::ImportKind`].
#[derive(Clone, Copy)]
enum Convert {
    /// BGRx swizzle ([`GlBlit`]).
    Rgb,
    /// RGB → NV12, BT.709 limited ([`Nv12Blit`]).
    Nv12,
    /// RGB → planar YUV444, BT.709 ([`Yuv444Blit`]).
    Yuv444,
}

/// One PipeWire dmabuf plane (BGRx is single-plane).
#[derive(Clone, Copy, Debug)]
pub struct DmabufPlane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

type Egl = egl::DynamicInstance<egl::EGL1_5>;

/// Headless GBM EGLDisplay plus a surfaceless desktop-GL context. Lives on the capture thread;
/// the GL context is made current there once and never released.
pub struct EglImporter {
    egl: Egl,
    display: egl::Display,
    no_ctx: egl::Context,
    _gl_ctx: egl::Context,
    egl_image_target: EglImageTargetFn,
    /// Recreated when the frame size changes.
    blit: Option<GlBlit>,
    /// Recreated on size change (`PUNKTFUNK_NV12`).
    nv12_blit: Option<Nv12Blit>,
    /// Recreated on size change (4:4:4 sessions).
    yuv444_blit: Option<Yuv444Blit>,
    /// LINEAR path: Vulkan bridge (dmabuf → exportable OPAQUE_FD → CUDA), lazy on first frame.
    vk: Option<super::vulkan::VkBridge>,
    linear_pool: Option<cuda::BufferPool>,
    /// NV12 twin of [`linear_pool`](Self::linear_pool). Separate because a session may fall back to RGB mid-stream.
    linear_nv12_pool: Option<cuda::BufferPool>,
    /// Last on purpose: `EglImporter` has no `Drop`, so fields drop in declaration order.
    /// Blits / CUDA / Vulkan must release against a live GBM display.
    _gbm: GbmDevice,
}

// SAFETY: `EglImporter` owns thread-affine handles (EGL display/contexts current on one thread,
// a GL proc, `gbm_device*`, fd, CUDA-registered textures). Constructed on the dedicated
// PipeWire thread; every method runs there. `Send` is only for transferring ownership into
// stream user-data (that API requires `Send`). Live handles are never used off-thread. Not `Sync`.
unsafe impl Send for EglImporter {}

impl EglImporter {
    /// Open a headless EGLDisplay on the NVIDIA GBM device. Creates the shared CUDA context so
    /// later `import` is hot-path only.
    pub fn new() -> Result<EglImporter> {
        // GBM on the NVIDIA render node so the EGLDisplay shares the DRM device CUDA-GL interop
        // uses. The EGL *device* platform does not — `cuGraphicsGLRegisterImage` rejects those textures.
        let node = nvidia_render_node();
        let path = std::ffi::CString::new(node.as_os_str().as_encoded_bytes())
            .with_context(|| format!("render node path {} has an interior NUL", node.display()))?;
        // SAFETY: `path` is a live local `CString` (constructor rejected interior NULs, so it is
        // NUL-terminated). `open` only reads the pointer for this call and does not retain it.
        let render_fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        ensure!(render_fd >= 0, "open {} for GBM", node.display());
        // SAFETY: `open` returned this fd (`>= 0`) and nothing else owns it. `OwnedFd` takes sole
        // ownership; every `?` closes it after `GbmDevice` destroys the device that borrows it.
        let render_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(render_fd) };
        // SAFETY: `render_fd` is the live DRM render-node fd. `gbm_create_device` borrows it and
        // returns `*mut gbm_device` (or null); `GbmDevice` keeps the fd open until after
        // `gbm_device_destroy`. No Rust-owned memory is passed.
        let raw_gbm = unsafe { gbm_create_device(render_fd.as_raw_fd()) };
        if raw_gbm.is_null() {
            anyhow::bail!("gbm_create_device failed on {}", node.display());
        }
        let gbm = GbmDevice {
            raw: raw_gbm,
            _fd: render_fd,
        };

        // SAFETY: `Egl::load_required` dlopens libEGL and binds EGL 1.5 entry points matching
        // the `khronos_egl` `EGL1_5` ABI. No Rust memory is passed; later use is through the
        // safe wrappers.
        let egl: Egl =
            unsafe { Egl::load_required() }.context("load libEGL (EGL 1.5 dynamic instance)")?;
        // SAFETY: `gbm.raw` is the non-null `gbm_device*` just created; `EGL_PLATFORM_GBM_KHR`
        // is the platform enum that pairs with a GBM device as native display. `&[ATTRIB_NONE]`
        // is a terminated empty attrib list borrowed for this call; EGL does not retain it.
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm.raw as egl::NativeDisplayType,
                &[egl::ATTRIB_NONE],
            )
        }
        .with_context(|| format!("eglGetPlatformDisplay(GBM) on {}", node.display()))?;
        egl.initialize(display).context("eglInitialize")?;

        let exts = egl
            .query_string(Some(display), egl::EXTENSIONS)
            .context("query EGL extensions")?
            .to_string_lossy()
            .into_owned();
        ensure!(
            exts.contains("EGL_EXT_image_dma_buf_import"),
            "EGL lacks EGL_EXT_image_dma_buf_import"
        );
        ensure!(
            exts.contains("EGL_EXT_image_dma_buf_import_modifiers"),
            "EGL lacks EGL_EXT_image_dma_buf_import_modifiers (needed for NVIDIA tiled dmabufs)"
        );

        // Surfaceless desktop-GL so we can bind the dmabuf EGLImage to a texture.
        // `cuGraphicsEGLRegisterImage` is Tegra-only; desktop CUDA interop goes through GL.
        egl.bind_api(egl::OPENGL_API)
            .context("eglBindAPI(OpenGL)")?;
        // Default SURFACE_TYPE is WINDOW_BIT; a headless display has none. Ask pbuffer first
        // (NVIDIA GBM has those). Fall back to no surface-type constraint: we never create an
        // EGLSurface (`eglMakeCurrent` surfaceless). Mesa GBM advertises only window configs, so
        // the pbuffer request is empty on a Mesa device.
        let want_pbuffer = [
            egl::SURFACE_TYPE,
            egl::PBUFFER_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_BIT,
            egl::NONE,
        ];
        let any_surface = [egl::RENDERABLE_TYPE, egl::OPENGL_BIT, egl::NONE];
        let config = match egl
            .choose_first_config(display, &want_pbuffer)
            .context("eglChooseConfig")?
        {
            Some(c) => c,
            None => {
                tracing::debug!(
                    node = %node.display(),
                    "no pbuffer-capable OpenGL EGL config — retrying without a surface-type \
                     constraint (we run surfaceless)"
                );
                egl.choose_first_config(display, &any_surface)
                    .context("eglChooseConfig (no surface-type constraint)")?
                    .with_context(|| {
                        format!(
                            "no EGL config for OpenGL on {} — this display serves no \
                             OpenGL-renderable config at all",
                            node.display()
                        )
                    })?
            }
        };
        let gl_ctx = egl
            .create_context(
                display,
                config,
                None,
                &[egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE],
            )
            .context("eglCreateContext(OpenGL)")?;
        egl.make_current(display, None, None, Some(gl_ctx))
            .context("eglMakeCurrent surfaceless (needs EGL_KHR_surfaceless_context)")?;
        // SAFETY: GL is current (required for a usable `eglGetProcAddress`). The non-null pointer
        // for `glEGLImageTargetTexture2DOES` has ABI `void(GLenum, GLeglImageOES)` =
        // `(u32, *mut c_void)` `extern "system"`, matching `EglImageTargetFn`. Present because
        // `EGL_EXT_image_dma_buf_import` was asserted on this display.
        let egl_image_target: EglImageTargetFn = unsafe {
            std::mem::transmute(
                egl.get_proc_address("glEGLImageTargetTexture2DOES")
                    .context("glEGLImageTargetTexture2DOES unavailable")?,
            )
        };

        cuda::context().context("create CUDA context")?;

        // SAFETY: `egl::NO_CONTEXT` is the null sentinel. `Context::from_ptr` only stores the
        // handle; `eglCreateImage(EGL_LINUX_DMA_BUF_EXT)` requires `EGL_NO_CONTEXT`.
        let no_ctx = unsafe { egl::Context::from_ptr(egl::NO_CONTEXT) };
        tracing::info!(
            node = %node.display(),
            "zero-copy EGL importer ready (GBM platform + GL texture interop, dma_buf_import + modifiers)"
        );
        Ok(EglImporter {
            egl,
            display,
            no_ctx,
            _gl_ctx: gl_ctx,
            egl_image_target,
            blit: None,
            nv12_blit: None,
            yuv444_blit: None,
            vk: None,
            linear_pool: None,
            linear_nv12_pool: None,
            _gbm: gbm,
        })
    }

    /// The Vulkan bridge, brought up on first use.
    fn vk_bridge(&mut self) -> Result<&mut super::vulkan::VkBridge> {
        if self.vk.is_none() {
            self.vk = Some(super::vulkan::VkBridge::new()?);
        }
        Ok(self.vk.as_mut().expect("set above"))
    }

    /// The fused convert lane (`vulkan/convert.rs`): the host's NVENC slot, imported once.
    pub fn register_slot(&mut self, id: u32, fd: std::os::fd::OwnedFd, size: u64) -> Result<()> {
        self.vk_bridge()?.register_slot(id, fd, size)
    }

    pub fn forget_slots(&mut self) {
        if let Some(vk) = self.vk.as_mut() {
            vk.forget_slots();
        }
    }

    pub fn set_cursor(&mut self, serial: u64, width: u32, height: u32, rgba: &[u8]) -> Result<()> {
        self.vk_bridge()?.set_cursor(serial, width, height, rgba)
    }

    /// One fused pass: dmabuf (any modifier) + cursor → the registered slot. Returns the
    /// timeline value the pass signals.
    pub fn convert(
        &mut self,
        src: &super::proto::ConvertSrc,
        slot: u32,
        out: &super::proto::ConvertOut,
        cursor: Option<super::proto::CursorRect>,
    ) -> Result<u64> {
        self.vk_bridge()?.convert(src, slot, out, cursor)
    }

    /// The convert timeline as an OPAQUE_FD for the host's CUDA import.
    pub fn convert_timeline_fd(&mut self) -> Result<std::os::fd::OwnedFd> {
        self.vk_bridge()?.convert_timeline_fd()
    }

    /// Import a LINEAR dmabuf via the Vulkan bridge. NVIDIA EGL cannot sample LINEAR; CUDA
    /// rejects raw dmabuf fds. See [`super::vulkan`].
    pub fn import_linear(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.linear_pool.as_ref().map(|p| (p.width(), p.height())) != Some((width, height)) {
            self.linear_pool = Some(cuda::BufferPool::new(width, height)?);
        }
        if self.vk.is_none() {
            self.vk = Some(super::vulkan::VkBridge::new()?);
        }
        self.vk.as_mut().unwrap().import_linear(
            plane.fd,
            plane.offset,
            plane.stride,
            height,
            self.linear_pool.as_ref().unwrap(),
        )
    }

    /// LINEAR analogue of [`import_nv12`](Self::import_nv12): the bridge's compute CSC writes a
    /// two-plane NV12 buffer so NVENC encodes native YUV.
    pub fn import_linear_nv12(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        // Even dimensions only: UV copy walks `height.div_ceil(2)` rows, the pool is `height/2`.
        // Odd height writes one `uv_pitch` past the allocation and poisons the shared CUDA context.
        anyhow::ensure!(
            width % 2 == 0 && height % 2 == 0,
            "LINEAR NV12 needs even dimensions (got {width}x{height})"
        );
        cuda::make_current()?;
        if self
            .linear_nv12_pool
            .as_ref()
            .map(|p| (p.width(), p.height()))
            != Some((width, height))
        {
            self.linear_nv12_pool = Some(cuda::BufferPool::new_nv12(width, height)?);
        }
        if self.vk.is_none() {
            self.vk = Some(super::vulkan::VkBridge::new()?);
        }
        self.vk.as_mut().unwrap().import_linear_nv12(
            plane.fd,
            plane.offset,
            plane.stride,
            width,
            height,
            self.linear_nv12_pool.as_ref().unwrap(),
        )
    }

    /// Drop the Vulkan bridge's cached per-fd import ([`super::vulkan::VkBridge::forget_fd`]).
    /// No-op if the bridge was never built (tiled-only captures).
    pub fn forget_linear_fd(&mut self, fd: i32) {
        if let Some(vk) = self.vk.as_mut() {
            vk.forget_fd(fd);
            vk.forget_src_image(fd);
        }
    }

    /// Drop the LINEAR import cache (Vulkan bridge and every per-fd source). PipeWire renegotiate
    /// invalidates the keyed pool; a recycled fd must not resolve to a stale import.
    pub fn clear_linear_cache(&mut self) {
        self.vk = None;
    }

    /// DRM modifiers NVIDIA EGL can import for `fourcc` (`eglQueryDmaBufModifiersEXT`), advertised
    /// to PipeWire so the compositor allocates a layout we can import. Empty on failure.
    pub fn supported_modifiers(&self, fourcc: u32) -> Vec<u64> {
        type QueryFn = unsafe extern "system" fn(
            dpy: *mut c_void,
            format: i32,
            max_modifiers: i32,
            modifiers: *mut u64,
            external_only: *mut u32,
            num_modifiers: *mut i32,
        ) -> u32;
        let Some(sym) = self.egl.get_proc_address("eglQueryDmaBufModifiersEXT") else {
            return Vec::new();
        };
        // SAFETY: `sym` is the non-null `eglQueryDmaBufModifiersEXT` proc. `QueryFn` matches that
        // ABI (`EGLDisplay, EGLint, EGLint, EGLuint64*, EGLBoolean*, EGLint* -> EGLBoolean`)
        // `extern "system"`; the transmute retypes a same-size thin fn pointer.
        let query: QueryFn = unsafe { std::mem::transmute(sym) };
        let dpy = self.display.as_ptr();
        // SAFETY: `dpy` is this importer's live `EGLDisplay`. First call: null out-arrays,
        // `max_modifiers == 0` → write only `&mut count`. Second: `mods`/`ext` are `Vec`s of
        // `count` elements, `max_modifiers == count`, so writes stay in bounds; `&mut n` is a
        // live local. `truncate` only shrinks, so `n > count` cannot read out of bounds.
        unsafe {
            let mut count: i32 = 0;
            if query(
                dpy,
                fourcc as i32,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut count,
            ) == 0
                || count <= 0
            {
                return Vec::new();
            }
            let mut mods = vec![0u64; count as usize];
            let mut ext = vec![0u32; count as usize];
            let mut n: i32 = 0;
            if query(
                dpy,
                fourcc as i32,
                count,
                mods.as_mut_ptr(),
                ext.as_mut_ptr(),
                &mut n,
            ) == 0
            {
                return Vec::new();
            }
            mods.truncate(n.max(0) as usize);
            mods
        }
    }

    /// Import one dmabuf into an owned CUDA buffer. `modifier` is the negotiated 64-bit DRM
    /// modifier, or `None` for the buffer's implicit modifier (`EGL_EXT_image_dma_buf_import`).
    pub fn import(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_inner(plane, width, height, fourcc, modifier, Convert::Rgb)
    }

    /// Like [`import`](Self::import), then GPU-convert to NV12 (BT.709 limited) so NVENC encodes
    /// native YUV. Tiled EGL/GL only — LINEAR/Vulkan stays RGB. See [`DeviceBuffer::is_nv12`].
    pub fn import_nv12(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_inner(plane, width, height, fourcc, modifier, Convert::Nv12)
    }

    /// Like [`import_nv12`](Self::import_nv12), but planar YUV444 into one stacked
    /// [`DeviceBuffer`] (`DeviceBuffer::yuv444`). Tiled EGL/GL only.
    pub fn import_yuv444(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        self.import_inner(plane, width, height, fourcc, modifier, Convert::Yuv444)
    }

    fn import_inner(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
        convert: Convert,
    ) -> Result<DeviceBuffer> {
        let mut attrs: Vec<egl::Attrib> = vec![
            egl::WIDTH as egl::Attrib,
            width as egl::Attrib,
            egl::HEIGHT as egl::Attrib,
            height as egl::Attrib,
            EGL_LINUX_DRM_FOURCC_EXT,
            fourcc as egl::Attrib,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            plane.fd as egl::Attrib,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            plane.offset as egl::Attrib,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            plane.stride as egl::Attrib,
        ];
        if let Some(m) = modifier {
            attrs.extend_from_slice(&[
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                (m & 0xFFFF_FFFF) as egl::Attrib,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (m >> 32) as egl::Attrib,
            ]);
        }
        attrs.push(egl::ATTRIB_NONE);
        // SAFETY: `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)` requires a NULL `EGLClientBuffer`
        // (the source is the attribute list). `from_ptr` only stores the pointer.
        let client = unsafe { egl::ClientBuffer::from_ptr(std::ptr::null_mut()) };
        let image = self
            .egl
            .create_image(
                self.display,
                self.no_ctx,
                EGL_LINUX_DMA_BUF_EXT,
                client,
                &attrs,
            )
            .context("eglCreateImage(EGL_LINUX_DMA_BUF_EXT) — modifier mismatch?")?;

        // Blit into a CUDA-registrable render target. Registering the EGLImage texture itself
        // fails — its layout is not a CUDA-registrable format.
        let result = match convert {
            Convert::Nv12 => self.blit_and_copy_nv12(image.as_ptr(), width, height),
            Convert::Yuv444 => self.blit_and_copy_yuv444(image.as_ptr(), width, height),
            Convert::Rgb => self.blit_and_copy(image.as_ptr(), width, height),
        };
        let _ = self.egl.destroy_image(self.display, image);
        result
    }

    /// Blit `image` into the registrable RGBA8 texture and copy to an owned CUDA buffer.
    /// Recreates the per-size GL blit when the frame size changes.
    fn blit_and_copy(
        &mut self,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `GlBlit::new` needs GL and CUDA current. Both hold: capture thread, GL
            // made current in `EglImporter::new` and never released; `cuda::make_current()?` ran
            // above.
            self.blit = Some(unsafe { GlBlit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.blit.as_mut().unwrap();
        // SAFETY: `GlBlit::run` needs GL current and a valid `EGLImage`. GL is current on this
        // capture thread (never released); `image` is the live `eglCreateImage` handle
        // `import_inner` destroys only after this call returns.
        unsafe { blit.run(egl_image_target, image)? };
        // Persistent registration + pool: do not `cuGraphicsGLRegisterImage` / `cuMemAllocPitch` per frame.
        let dst = blit.pool.get()?;
        blit.registered.copy_mapped_to(&dst)?;
        Ok(dst)
    }

    /// Convert `image` to NV12 and copy both planes into a pooled [`DeviceBuffer`]. Recreates
    /// the per-size convert when the frame size changes.
    fn blit_and_copy_nv12(
        &mut self,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.nv12_blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `Nv12Blit::new` needs GL and CUDA current. Both hold: capture thread, GL
            // made current in `EglImporter::new` and never released; `cuda::make_current()?` ran
            // above.
            self.nv12_blit = Some(unsafe { Nv12Blit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.nv12_blit.as_mut().unwrap();
        // SAFETY: `Nv12Blit::run` needs GL current and a valid `EGLImage`. GL is current on this
        // capture thread (never released); `image` is the live `eglCreateImage` handle
        // `import_inner` destroys only after this call returns.
        unsafe { blit.run(egl_image_target, image)? };
        let dst = blit.pool.get()?;
        cuda::copy_mapped_nv12(&mut blit.y_registered, &mut blit.uv_registered, &dst)?;
        Ok(dst)
    }

    /// Convert `image` to planar YUV444 and copy into a pooled stacked [`DeviceBuffer`].
    /// Recreates the per-size convert when the frame size changes.
    fn blit_and_copy_yuv444(
        &mut self,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        cuda::make_current()?;
        if self.yuv444_blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `Yuv444Blit::new` needs GL and CUDA current. Both hold: capture thread, GL
            // made current in `EglImporter::new` and never released; `cuda::make_current()?` ran
            // above.
            self.yuv444_blit = Some(unsafe { Yuv444Blit::new(width, height)? });
        }
        let egl_image_target = self.egl_image_target;
        let blit = self.yuv444_blit.as_mut().unwrap();
        // SAFETY: `Yuv444Blit::run` needs GL current and a valid `EGLImage`. GL is current on this
        // capture thread (never released); `image` is the live `eglCreateImage` handle
        // `import_inner` destroys only after this call returns.
        unsafe { blit.run(egl_image_target, image)? };
        let dst = blit.pool.get()?;
        let [y, u, v] = &mut blit.registered;
        cuda::copy_mapped_yuv444(y, u, v, &dst)?;
        Ok(dst)
    }

    /// Test helper: upload packed RGBA8 (`rgba` is 4 B/px, no row padding), run the live NV12
    /// shaders + CUDA copy, return a pooled NV12 [`DeviceBuffer`]. No compositor / EGLImage.
    pub fn convert_rgba_for_test(
        &mut self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        anyhow::ensure!(
            rgba.len() == width as usize * height as usize * 4,
            "test RGBA buffer {} bytes != {}x{}x4",
            rgba.len(),
            width,
            height
        );
        cuda::make_current()?;
        if self.nv12_blit.as_ref().map(|b| (b.width, b.height)) != Some((width, height)) {
            // SAFETY: `Nv12Blit::new` needs GL and CUDA current. This test path runs on the
            // thread that owns this `EglImporter` with GL current; `cuda::make_current()?` ran above.
            self.nv12_blit = Some(unsafe { Nv12Blit::new(width, height)? });
        }
        let blit = self.nv12_blit.as_mut().unwrap();
        // SAFETY: GL is current on the owning thread. `src_tex` is this blit; `glTexStorage2D`
        // allocates immutable RGBA8 once (`test_src_storage`). `glTexSubImage2D` uploads
        // `width×height` RGBA8 texels from `rgba.as_ptr()`; caller asserted
        // `rgba.len() == width*height*4`, rows are `width*4` (multiple of 4-byte unpack
        // alignment). `rgba` outlives the upload. `run_passes` needs only current GL.
        unsafe {
            glBindTexture(GL_TEXTURE_2D, blit.src_tex);
            if !blit.test_src_storage {
                glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, width as c_int, height as c_int);
                blit.test_src_storage = true;
            }
            let _ = glGetError();
            glTexSubImage2D(
                GL_TEXTURE_2D,
                0,
                0,
                0,
                width as c_int,
                height as c_int,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                rgba.as_ptr() as *const c_void,
            );
            let e = glGetError();
            glBindTexture(GL_TEXTURE_2D, 0);
            ensure!(e == 0, "glTexSubImage2D(test source) failed ({e:#x})");
            blit.run_passes()?;
        }
        let dst = blit.pool.get()?;
        cuda::copy_mapped_nv12(&mut blit.y_registered, &mut blit.uv_registered, &dst)?;
        Ok(dst)
    }
}

// No `Drop` on `EglImporter`: `Drop::drop` runs before field drops, which would destroy the GBM
// device while blit destructors still call into the driver. Teardown is field order: blits and
// bridge first, `GbmDevice` last.

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    /// Fake `/dev/dri` + `/sys/class/drm`. `vendor: Some` writes sysfs `device/vendor`.
    fn fixture(nodes: &[(&str, Option<&str>)]) -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let dri = tmp.path().join("dev/dri");
        let sys = tmp.path().join("sys/class/drm");
        std::fs::create_dir_all(&dri).unwrap();
        for (node, vendor) in nodes {
            std::fs::write(dri.join(node), b"").unwrap();
            if let Some(v) = vendor {
                let dev = sys.join(node).join("device");
                std::fs::create_dir_all(&dev).unwrap();
                std::fs::write(dev.join("vendor"), v).unwrap();
            }
        }
        (tmp, dri, sys)
    }

    /// Hybrid host: iGPU owns `renderD128`; NVIDIA is a later node.
    #[test]
    fn picks_the_nvidia_node_not_the_first_one() {
        let (_t, dri, sys) = fixture(&[
            ("renderD128", Some("0x8086\n")),
            ("renderD129", Some("0x10de\n")),
        ]);
        assert_eq!(
            nvidia_render_node_in(&dri, &sys),
            Some(dri.join("renderD129"))
        );
    }

    /// No NVIDIA node / no sysfs → `None`; the caller keeps `/dev/dri/renderD128`.
    #[test]
    fn no_nvidia_node_yields_nothing() {
        let (_t, dri, sys) = fixture(&[("renderD128", Some("0x8086\n")), ("renderD129", None)]);
        assert_eq!(nvidia_render_node_in(&dri, &sys), None);
        // Missing `/dev/dri` or `/sys`.
        assert_eq!(
            nvidia_render_node_in(Path::new("/nonexistent/dri"), &sys),
            None
        );
    }

    /// Skip card/control nodes; name order so two NVIDIA GPUs pick the same node every boot.
    #[test]
    fn scans_render_nodes_only_and_in_order() {
        let (_t, dri, sys) = fixture(&[
            ("card0", Some("0x10de\n")),
            ("renderD130", Some("0x10de\n")),
            ("renderD129", Some("0x10de\n")),
        ]);
        assert_eq!(
            nvidia_render_node_in(&dri, &sys),
            Some(dri.join("renderD129"))
        );
    }
}
