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
use super::gbm::GbmDevice;
use super::proto::ImportKind;
use anyhow::{ensure, Context as _, Result};
use khronos_egl as egl;
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

/// NVIDIA PCI vendor: the render node this importer falls back to when CUDA cannot name its own.
const PCI_VENDOR_NVIDIA: u32 = 0x10de;

/// NVIDIA DRM render node: `PUNKTFUNK_ZEROCOPY_RENDER_NODE`, else the node on CUDA device 0's
/// PCI slot, else the first `/dev/dri/renderD*` whose sysfs PCI vendor is NVIDIA, else
/// `/dev/dri/renderD128`.
///
/// CUDA's device first, so GL interop and the Vulkan bridge ([`super::vkdev`]) stay on one GPU
/// of a dual-NVIDIA box. Scan by vendor, not first node: on a hybrid host `renderD128` is the
/// iGPU. Do not call `pf_gpu::linux_render_node` — this crate is a leaf worker, and that
/// helper follows the operator VAAPI preference, which may name the iGPU.
fn nvidia_render_node() -> std::path::PathBuf {
    use std::path::{Path, PathBuf};
    if let Some(p) = std::env::var_os("PUNKTFUNK_ZEROCOPY_RENDER_NODE").filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    let cuda_slot = cuda::device_pci_bus_id().ok();
    // No NVIDIA node (or no /sys): keep `/dev/dri/renderD128`. CUDA construction fails if it is wrong.
    nvidia_render_node_in(
        Path::new("/dev/dri"),
        Path::new("/sys/class/drm"),
        cuda_slot.as_deref(),
    )
    .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"))
}

/// Scan half of [`nvidia_render_node`]. Roots are parameters so tests can pin
/// `<sys_class_drm>/<node>/device/{vendor,uevent}`. Name order keeps the pick stable across boots.
fn nvidia_render_node_in(
    dri: &std::path::Path,
    sys_class_drm: &std::path::Path,
    cuda_slot: Option<&str>,
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
    let device = |node: &std::ffi::OsString, file: &str| {
        std::fs::read_to_string(sys_class_drm.join(node).join("device").join(file)).ok()
    };
    let on_cuda_slot = |node: &std::ffi::OsString| {
        let (Some(want), Some(uevent)) = (cuda_slot, device(node, "uevent")) else {
            return false;
        };
        uevent
            .lines()
            .filter_map(|l| l.strip_prefix("PCI_SLOT_NAME="))
            .any(|slot| slot.trim().eq_ignore_ascii_case(want))
    };
    let nvidia = |node: &std::ffi::OsString| {
        device(node, "vendor")
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            == Some(PCI_VENDOR_NVIDIA)
    };
    nodes
        .iter()
        .find(|n| on_cuda_slot(n))
        .or_else(|| nodes.iter().find(|n| nvidia(n)))
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

/// One render pass: a fragment shader drawing `src_tex` into a target CUDA reads.
struct Pass {
    program: u32,
    fbo: u32,
    tex: u32,
    /// Target size in texels.
    size: (u32, u32),
    /// `tex` registered once; mapped and copied each frame.
    registered: cuda::RegisteredTexture,
}

/// Each pass's fragment shader, target format and bytes per texel, in plane order.
///
/// Packed32 swizzles to BGRx (`GL_RGBA8`, dest linear so CUDA can register it). NV12 writes
/// BT.709 limited full-res `GL_R8` luma and half-res `GL_RG8` chroma (R=U, G=V), so NVENC
/// skips its own CSC. YUV444 writes three full-res `GL_R8` planes, studio or full range.
fn pass_specs(layout: cuda::PlaneLayout) -> Vec<(std::borrow::Cow<'static, [u8]>, u32, usize)> {
    match layout {
        cuda::PlaneLayout::Packed32 => vec![(FRAG_SRC.into(), GL_RGBA8, 4)],
        cuda::PlaneLayout::Nv12 => vec![
            (FRAG_Y_SRC.into(), GL_R8, 1),
            (FRAG_UV_SRC.into(), GL_RG8, 2),
        ],
        cuda::PlaneLayout::Yuv444 => {
            let full_range = yuv444_full_range();
            if full_range {
                tracing::info!("YUV444 zero-copy convert: FULL range (PUNKTFUNK_444_FULLRANGE=1)");
            }
            let (y, u, v) = yuv444_frag_sources(full_range);
            [y, u, v]
                .into_iter()
                .map(|frag| (frag.into(), GL_R8, 1))
                .collect()
        }
    }
}

/// Per-size GL convert: the de-tiled `EGLImage` in `src_tex`, drawn once per plane of
/// `layout` ([`pass_specs`]) into CUDA-registered targets, then copied into a pooled buffer.
struct GlConvert {
    vao: u32,
    /// Retargeted per frame. `GL_LINEAR` so the NV12 chroma pass's two taps each average two
    /// texels; exact at 1:1 (texel centres) for the full-res passes.
    src_tex: u32,
    passes: Vec<Pass>,
    width: u32,
    height: u32,
    pool: cuda::BufferPool,
    /// Test path only: `src_tex` already has immutable RGBA8 storage. Live path retargets via EGLImage.
    test_src_storage: bool,
}

impl GlConvert {
    unsafe fn new(layout: cuda::PlaneLayout, width: u32, height: u32) -> Result<GlConvert> {
        // SAFETY: caller contract (`convert_for`): GL and the shared CUDA context are current
        // on this thread. GL calls pass live locals; every created name is owned by `guard`
        // until the struct exists.
        unsafe {
            ensure!(
                layout == cuda::PlaneLayout::Packed32 || (width % 2 == 0 && height % 2 == 0),
                "{layout:?} convert needs even dimensions (got {width}x{height})"
            );
            // Guard first so it drops last on unwind, after `passes` unregisters from CUDA.
            let mut guard = GlNameGuard::default();
            let mut vao = 0u32;
            glGenVertexArrays(1, &mut vao); // core profile: glDrawArrays needs a bound VAO
            guard.vaos.push(vao);
            let mut src_tex = 0u32;
            glGenTextures(1, &mut src_tex);
            guard.textures.push(src_tex);
            glBindTexture(GL_TEXTURE_2D, src_tex);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glBindTexture(GL_TEXTURE_2D, 0);

            let mut passes = Vec::new();
            for ((frag, format, texel_bytes), (row_bytes, rows)) in pass_specs(layout)
                .into_iter()
                .zip(layout.planes(width, height))
            {
                let program = compile_program_with(&frag)?;
                guard.programs.push(program);
                let mut fbo = 0u32;
                glGenFramebuffers(1, &mut fbo);
                guard.fbos.push(fbo);
                let mut tex = 0u32;
                glGenTextures(1, &mut tex);
                guard.textures.push(tex);
                let size = ((row_bytes / texel_bytes) as u32, rows as u32);
                glBindTexture(GL_TEXTURE_2D, tex);
                glTexStorage2D(GL_TEXTURE_2D, 1, format, size.0 as c_int, size.1 as c_int);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
                glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
                glBindTexture(GL_TEXTURE_2D, 0);
                glBindFramebuffer(GL_FRAMEBUFFER, fbo);
                glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, tex, 0);
                let status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
                glBindFramebuffer(GL_FRAMEBUFFER, 0);
                ensure!(
                    status == GL_FRAMEBUFFER_COMPLETE,
                    "{layout:?} convert FBO incomplete ({status:#x}) — {format:#x} not renderable?"
                );
                passes.push(Pass {
                    program,
                    fbo,
                    tex,
                    size,
                    registered: cuda::RegisteredTexture::register_gl(tex)?,
                });
            }
            let pool = cuda::BufferPool::new(layout, width, height)?;
            guard.defuse();
            Ok(GlConvert {
                vao,
                src_tex,
                passes,
                width,
                height,
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
            for p in &self.passes {
                glBindFramebuffer(GL_FRAMEBUFFER, p.fbo);
                glViewport(0, 0, p.size.0 as c_int, p.size.1 as c_int);
                glUseProgram(p.program);
                glBindTexture(GL_TEXTURE_2D, self.src_tex);
                glDrawArrays(GL_TRIANGLES, 0, 3);
            }
            glBindVertexArray(0);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
            glFlush(); // GL must finish before CUDA maps the targets
            Ok(())
        }
    }

    /// Copy every target into a pooled buffer. Persistent registrations and pool: no
    /// per-frame `cuGraphicsGLRegisterImage` or `cuMemAllocPitch`.
    fn copy_out(&mut self) -> Result<DeviceBuffer> {
        let dst = self.pool.get()?;
        cuda::copy_mapped_planes(self.passes.iter_mut().map(|p| &mut p.registered), &dst)?;
        Ok(dst)
    }
}

impl Drop for GlConvert {
    fn drop(&mut self) {
        // Unregister CUDA before `glDelete*` — `Drop::drop` runs before field drops, so deleting
        // first would leave a registration on freed GL state.
        for p in &mut self.passes {
            p.registered.release();
        }
        // SAFETY: names created by this `GlConvert` on the GL context still current (`EglImporter`
        // never releases it; capture thread; this field drops before `GbmDevice`). Each
        // `glDelete*` is n=1 on a live field, once, after the CUDA release above.
        unsafe {
            for p in &self.passes {
                glDeleteTextures(1, &p.tex);
                glDeleteFramebuffers(1, &p.fbo);
                glDeleteProgram(p.program);
            }
            glDeleteTextures(1, &self.src_tex);
            glDeleteVertexArrays(1, &self.vao);
        }
    }
}

/// `PUNKTFUNK_444_FULLRANGE=1`: the YUV444 convert writes full range. The encoder signals the
/// same answer in the VUI, so both read it here.
pub fn yuv444_full_range() -> bool {
    std::env::var("PUNKTFUNK_444_FULLRANGE").is_ok_and(|v| v.trim() == "1")
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
    /// One tiled convert per [`cuda::PlaneLayout`], indexed by it and recreated on a size
    /// change. Kept apart so an RGB fallback mid-stream does not rebuild the NV12 one.
    converts: [Option<GlConvert>; 3],
    /// LINEAR path: Vulkan bridge (dmabuf → exportable OPAQUE_FD → CUDA), lazy on first frame.
    vk: Option<super::vulkan::VkBridge>,
    linear_pool: Option<cuda::BufferPool>,
    /// NV12 twin of [`linear_pool`](Self::linear_pool). Separate because a session may fall back to RGB mid-stream.
    linear_nv12_pool: Option<cuda::BufferPool>,
    /// `EglImporter` has no `Drop`, so fields drop in declaration order. The EGL handles below
    /// drop after the converts / CUDA / Vulkan above: `egl` owns the libEGL mapping their `Drop`s
    /// call through.
    egl_image_target: EglImageTargetFn,
    no_ctx: egl::Context,
    _gl_ctx: egl::Context,
    display: egl::Display,
    egl: Egl,
    /// Last: everything above releases against a live GBM display.
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
        let render_node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&node)
            .with_context(|| format!("open {} for GBM", node.display()))?;
        let gbm = GbmDevice::open(render_node)
            .with_context(|| format!("open the GBM device on {}", node.display()))?;

        // SAFETY: `Egl::load_required` dlopens libEGL and binds EGL 1.5 entry points matching
        // the `khronos_egl` `EGL1_5` ABI. No Rust memory is passed; later use is through the
        // safe wrappers.
        let egl: Egl =
            unsafe { Egl::load_required() }.context("load libEGL (EGL 1.5 dynamic instance)")?;
        // SAFETY: `gbm.as_ptr()` is the live `gbm_device*` just created; `EGL_PLATFORM_GBM_KHR`
        // is the platform enum that pairs with a GBM device as native display. `&[ATTRIB_NONE]`
        // is a terminated empty attrib list borrowed for this call; EGL does not retain it.
        let display = unsafe {
            egl.get_platform_display(
                EGL_PLATFORM_GBM_KHR,
                gbm.as_ptr() as egl::NativeDisplayType,
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
            converts: [None, None, None],
            vk: None,
            linear_pool: None,
            linear_nv12_pool: None,
            egl_image_target,
            no_ctx,
            _gl_ctx: gl_ctx,
            display,
            egl,
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

    /// Import a LINEAR dmabuf via the Vulkan bridge as packed RGB, or as two-plane NV12 through
    /// the bridge's compute CSC. NVIDIA EGL cannot sample LINEAR; CUDA rejects raw dmabuf fds.
    /// See [`super::vulkan`].
    fn import_linear(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        layout: cuda::PlaneLayout,
    ) -> Result<DeviceBuffer> {
        let nv12 = layout == cuda::PlaneLayout::Nv12;
        // NVENC takes 4:2:0 at even dimensions only.
        anyhow::ensure!(
            !nv12 || (width % 2 == 0 && height % 2 == 0),
            "LINEAR NV12 needs even dimensions (got {width}x{height})"
        );
        cuda::make_current()?;
        let pool = if nv12 {
            &mut self.linear_nv12_pool
        } else {
            &mut self.linear_pool
        };
        if pool.as_ref().map(|p| (p.width(), p.height())) != Some((width, height)) {
            *pool = Some(cuda::BufferPool::new(layout, width, height)?);
        }
        let pool = pool.as_ref().expect("set above");
        if self.vk.is_none() {
            self.vk = Some(super::vulkan::VkBridge::new()?);
        }
        let vk = self.vk.as_mut().expect("set above");
        if nv12 {
            vk.import_linear_nv12(plane.fd, plane.offset, plane.stride, width, height, pool)
        } else {
            vk.import_linear(plane.fd, plane.offset, plane.stride, height, pool)
        }
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

    /// Import one dmabuf into an owned CUDA buffer as `kind`. Tiled kinds de-tile through
    /// EGL/GL with `modifier`, the negotiated 64-bit DRM modifier (`None` for the buffer's
    /// implicit one). LINEAR kinds go through the Vulkan bridge and ignore `fourcc` and
    /// `modifier`.
    pub fn import(
        &mut self,
        kind: ImportKind,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<DeviceBuffer> {
        if kind.is_tiled() {
            self.import_tiled(plane, width, height, fourcc, modifier, kind.layout())
        } else {
            self.import_linear(plane, width, height, kind.layout())
        }
    }

    fn import_tiled(
        &mut self,
        plane: &DmabufPlane,
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: Option<u64>,
        layout: cuda::PlaneLayout,
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
        let result = self.blit_and_copy(layout, image.as_ptr(), width, height);
        let _ = self.egl.destroy_image(self.display, image);
        result
    }

    /// The tiled convert for `layout` at this size, rebuilt when the size changed.
    fn convert_for(
        &mut self,
        layout: cuda::PlaneLayout,
        width: u32,
        height: u32,
    ) -> Result<&mut GlConvert> {
        cuda::make_current()?;
        let slot = &mut self.converts[layout as usize];
        if slot.as_ref().map(|c| (c.width, c.height)) != Some((width, height)) {
            // SAFETY: `GlConvert::new` needs GL and CUDA current. Both hold: the thread that
            // owns this importer, GL made current in `EglImporter::new` and never released;
            // `cuda::make_current()?` ran above.
            *slot = Some(unsafe { GlConvert::new(layout, width, height)? });
        }
        Ok(slot.as_mut().expect("set above"))
    }

    /// Convert `image` into `layout` and copy it into a pooled [`DeviceBuffer`].
    fn blit_and_copy(
        &mut self,
        layout: cuda::PlaneLayout,
        image: *mut c_void,
        width: u32,
        height: u32,
    ) -> Result<DeviceBuffer> {
        let egl_image_target = self.egl_image_target;
        let convert = self.convert_for(layout, width, height)?;
        // SAFETY: `GlConvert::run` needs GL current and a valid `EGLImage`. GL is current on this
        // capture thread (never released); `image` is the live `eglCreateImage` handle
        // `import_tiled` destroys only after this call returns.
        unsafe { convert.run(egl_image_target, image)? };
        convert.copy_out()
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
        let convert = self.convert_for(cuda::PlaneLayout::Nv12, width, height)?;
        // SAFETY: GL is current on the owning thread. `src_tex` is this convert's; `glTexStorage2D`
        // allocates immutable RGBA8 once (`test_src_storage`). `glTexSubImage2D` uploads
        // `width×height` RGBA8 texels from `rgba.as_ptr()`; caller asserted
        // `rgba.len() == width*height*4`, rows are `width*4` (multiple of 4-byte unpack
        // alignment). `rgba` outlives the upload. `run_passes` needs only current GL.
        unsafe {
            glBindTexture(GL_TEXTURE_2D, convert.src_tex);
            if !convert.test_src_storage {
                glTexStorage2D(GL_TEXTURE_2D, 1, GL_RGBA8, width as c_int, height as c_int);
                convert.test_src_storage = true;
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
            convert.run_passes()?;
        }
        convert.copy_out()
    }
}

// No `Drop` on `EglImporter`: `Drop::drop` runs before field drops, which would destroy the GBM
// device while convert destructors still call into the driver. Teardown is field order:
// converts and bridge first, `GbmDevice` last.

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
            nvidia_render_node_in(&dri, &sys, None),
            Some(dri.join("renderD129"))
        );
    }

    /// No NVIDIA node / no sysfs → `None`; the caller keeps `/dev/dri/renderD128`.
    #[test]
    fn no_nvidia_node_yields_nothing() {
        let (_t, dri, sys) = fixture(&[("renderD128", Some("0x8086\n")), ("renderD129", None)]);
        assert_eq!(nvidia_render_node_in(&dri, &sys, None), None);
        // Missing `/dev/dri` or `/sys`.
        assert_eq!(
            nvidia_render_node_in(Path::new("/nonexistent/dri"), &sys, None),
            None
        );
    }

    /// A dual-NVIDIA box takes the node on CUDA's PCI slot, whatever the name order says.
    #[test]
    fn the_cuda_slot_wins_over_name_order() {
        let (_t, dri, sys) = fixture(&[
            ("renderD128", Some("0x10de\n")),
            ("renderD129", Some("0x10de\n")),
        ]);
        for (node, slot) in [
            ("renderD128", "0000:01:00.0"),
            ("renderD129", "0000:0a:00.0"),
        ] {
            let uevent = format!("DRIVER=nvidia\nPCI_SLOT_NAME={slot}\n");
            std::fs::write(sys.join(node).join("device").join("uevent"), uevent).unwrap();
        }
        assert_eq!(
            nvidia_render_node_in(&dri, &sys, Some("0000:0A:00.0")),
            Some(dri.join("renderD129"))
        );
        assert_eq!(
            nvidia_render_node_in(&dri, &sys, Some("0000:02:00.0")),
            Some(dri.join("renderD128")),
            "an unknown slot keeps the vendor scan"
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
            nvidia_render_node_in(&dri, &sys, None),
            Some(dri.join("renderD129"))
        );
    }
}
