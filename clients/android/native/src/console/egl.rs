//! The dozen EGL entry points the console host needs, hand-declared. `libEGL.so` is already
//! a `NEEDED` of this `.so` (skia-bindings links it for Skia's GL backend), so a plain
//! `#[link]` costs nothing new; a binding crate would be a dependency for twelve functions
//! whose signatures have not changed since 2008.
//!
//! One display, one context, one window surface at a time: the console draws into the
//! `SurfaceView` Kotlin hands over, and re-creates only the surface when that view comes and
//! goes. The context (and Skia's `DirectContext` on it) survives across surfaces so the
//! poster/glyph caches survive a trip through the stream.

use anyhow::{anyhow, bail, Result};
use std::ffi::c_void;

pub(super) type EGLDisplay = *mut c_void;
pub(super) type EGLConfig = *mut c_void;
pub(super) type EGLContext = *mut c_void;
pub(super) type EGLSurface = *mut c_void;
type EGLNativeWindowType = *mut c_void;
type EGLBoolean = u32;
type EGLint = i32;

const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_NO_DISPLAY: EGLDisplay = std::ptr::null_mut();
const EGL_NO_CONTEXT: EGLContext = std::ptr::null_mut();
const EGL_NO_SURFACE: EGLSurface = std::ptr::null_mut();
const EGL_TRUE: EGLBoolean = 1;

const EGL_NONE: EGLint = 0x3038;
const EGL_SURFACE_TYPE: EGLint = 0x3033;
const EGL_WINDOW_BIT: EGLint = 0x0004;
const EGL_RENDERABLE_TYPE: EGLint = 0x3040;
const EGL_OPENGL_ES2_BIT: EGLint = 0x0004;
const EGL_OPENGL_ES3_BIT: EGLint = 0x0040;
const EGL_RED_SIZE: EGLint = 0x3024;
const EGL_GREEN_SIZE: EGLint = 0x3023;
const EGL_BLUE_SIZE: EGLint = 0x3022;
const EGL_ALPHA_SIZE: EGLint = 0x3021;
const EGL_STENCIL_SIZE: EGLint = 0x3026;
const EGL_DEPTH_SIZE: EGLint = 0x3025;
const EGL_SAMPLES: EGLint = 0x3031;
const EGL_CONTEXT_CLIENT_VERSION: EGLint = 0x3098;
const EGL_WIDTH: EGLint = 0x3057;
const EGL_HEIGHT: EGLint = 0x3056;

#[link(name = "EGL")]
unsafe extern "C" {
    fn eglGetDisplay(display_id: *mut c_void) -> EGLDisplay;
    fn eglInitialize(dpy: EGLDisplay, major: *mut EGLint, minor: *mut EGLint) -> EGLBoolean;
    fn eglChooseConfig(
        dpy: EGLDisplay,
        attrib_list: *const EGLint,
        configs: *mut EGLConfig,
        config_size: EGLint,
        num_config: *mut EGLint,
    ) -> EGLBoolean;
    fn eglGetConfigAttrib(
        dpy: EGLDisplay,
        config: EGLConfig,
        attribute: EGLint,
        value: *mut EGLint,
    ) -> EGLBoolean;
    fn eglCreateContext(
        dpy: EGLDisplay,
        config: EGLConfig,
        share_context: EGLContext,
        attrib_list: *const EGLint,
    ) -> EGLContext;
    fn eglCreateWindowSurface(
        dpy: EGLDisplay,
        config: EGLConfig,
        win: EGLNativeWindowType,
        attrib_list: *const EGLint,
    ) -> EGLSurface;
    fn eglMakeCurrent(
        dpy: EGLDisplay,
        draw: EGLSurface,
        read: EGLSurface,
        ctx: EGLContext,
    ) -> EGLBoolean;
    fn eglSwapBuffers(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    fn eglSwapInterval(dpy: EGLDisplay, interval: EGLint) -> EGLBoolean;
    fn eglQuerySurface(
        dpy: EGLDisplay,
        surface: EGLSurface,
        attribute: EGLint,
        value: *mut EGLint,
    ) -> EGLBoolean;
    fn eglDestroySurface(dpy: EGLDisplay, surface: EGLSurface) -> EGLBoolean;
    fn eglDestroyContext(dpy: EGLDisplay, ctx: EGLContext) -> EGLBoolean;
    fn eglGetError() -> EGLint;
}

/// The GL client version the context was created for — Skia's `Interface::new_native()`
/// discovers the rest itself, but the SkSL mesh backdrop compiles under ES2 restrictions on
/// a 2.0 context, and the host wants to know which world it is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GlesVersion {
    Es2,
    Es3,
}

/// The display + config + context, created once per host and kept for its lifetime.
pub(super) struct EglContext {
    display: EGLDisplay,
    config: EGLConfig,
    context: EGLContext,
    pub(super) version: GlesVersion,
    /// The config's stencil depth — what Skia's `BackendRenderTarget` for FBO 0 declares.
    pub(super) stencil_bits: i32,
    /// The config's MSAA sample count (0 = none) — likewise.
    pub(super) samples: i32,
}

// SAFETY: EGL handles are process-wide tokens; the render thread is the only thread that
// makes the context current, and `EglContext` is only ever moved onto it (never shared).
unsafe impl Send for EglContext {}

impl EglContext {
    /// Initialise the default display and create an ES 3 context, falling back to ES 2 on
    /// the boxes that have nothing newer. RGBA8888 with an 8-bit stencil (Skia's path
    /// rendering wants one), no depth, no MSAA (the shell anti-aliases in Skia).
    pub(super) fn new() -> Result<EglContext> {
        // SAFETY: plain EGL calls with valid arguments; every handle is checked before use.
        unsafe {
            let display = eglGetDisplay(EGL_DEFAULT_DISPLAY);
            if display == EGL_NO_DISPLAY {
                bail!("eglGetDisplay: no default display");
            }
            let (mut major, mut minor) = (0, 0);
            if eglInitialize(display, &mut major, &mut minor) != EGL_TRUE {
                bail!("eglInitialize: 0x{:x}", eglGetError());
            }
            for (version, renderable) in [
                (GlesVersion::Es3, EGL_OPENGL_ES3_BIT),
                (GlesVersion::Es2, EGL_OPENGL_ES2_BIT),
            ] {
                let attribs = [
                    EGL_SURFACE_TYPE,
                    EGL_WINDOW_BIT,
                    EGL_RENDERABLE_TYPE,
                    renderable,
                    EGL_RED_SIZE,
                    8,
                    EGL_GREEN_SIZE,
                    8,
                    EGL_BLUE_SIZE,
                    8,
                    EGL_ALPHA_SIZE,
                    8,
                    EGL_STENCIL_SIZE,
                    8,
                    EGL_DEPTH_SIZE,
                    0,
                    EGL_NONE,
                ];
                let mut config: EGLConfig = std::ptr::null_mut();
                let mut n: EGLint = 0;
                if eglChooseConfig(display, attribs.as_ptr(), &mut config, 1, &mut n) != EGL_TRUE
                    || n < 1
                {
                    continue;
                }
                let client_version = match version {
                    GlesVersion::Es3 => 3,
                    GlesVersion::Es2 => 2,
                };
                let ctx_attribs = [EGL_CONTEXT_CLIENT_VERSION, client_version, EGL_NONE];
                let context =
                    eglCreateContext(display, config, EGL_NO_CONTEXT, ctx_attribs.as_ptr());
                if context == EGL_NO_CONTEXT {
                    continue;
                }
                let attr = |a: EGLint| {
                    let mut v: EGLint = 0;
                    if eglGetConfigAttrib(display, config, a, &mut v) == EGL_TRUE {
                        v
                    } else {
                        0
                    }
                };
                let stencil_bits = attr(EGL_STENCIL_SIZE);
                let samples = attr(EGL_SAMPLES);
                log::info!(
                    "console: EGL {major}.{minor}, GLES {client_version} context, stencil {stencil_bits}, samples {samples}"
                );
                return Ok(EglContext {
                    display,
                    config,
                    context,
                    version,
                    stencil_bits,
                    samples,
                });
            }
            bail!(
                "no EGL config/context for GLES 3 or 2 (0x{:x})",
                eglGetError()
            )
        }
    }

    /// A window surface over `window` (an `ANativeWindow*`), made current on the calling
    /// thread with a vsync-locked swap interval. Returns the surface and its pixel size.
    pub(super) fn window_surface(&self, window: *mut c_void) -> Result<EglSurface> {
        // SAFETY: `window` is a live ANativeWindow the caller holds a reference to for the
        // surface's lifetime; the display/config/context are this object's own.
        unsafe {
            let surface =
                eglCreateWindowSurface(self.display, self.config, window, std::ptr::null());
            if surface == EGL_NO_SURFACE {
                bail!("eglCreateWindowSurface: 0x{:x}", eglGetError());
            }
            if eglMakeCurrent(self.display, surface, surface, self.context) != EGL_TRUE {
                let e = eglGetError();
                eglDestroySurface(self.display, surface);
                bail!("eglMakeCurrent: 0x{e:x}");
            }
            // 1 = present on the panel's cadence, never faster: `eglSwapBuffers` blocks and
            // paces the render loop, which is the whole frame-timing story of this host.
            eglSwapInterval(self.display, 1);
            let (mut w, mut h) = (0, 0);
            eglQuerySurface(self.display, surface, EGL_WIDTH, &mut w);
            eglQuerySurface(self.display, surface, EGL_HEIGHT, &mut h);
            Ok(EglSurface {
                display: self.display,
                surface,
                width: w.max(1) as u32,
                height: h.max(1) as u32,
            })
        }
    }

    /// Release the current surface from this thread (before the surface is destroyed).
    pub(super) fn release_current(&self) {
        // SAFETY: valid display; NO_SURFACE/NO_CONTEXT is the documented "unbind" call.
        unsafe {
            eglMakeCurrent(self.display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        }
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        // SAFETY: the context is this object's own and nothing is current on this thread once
        // the host has released its surface (the render loop releases before it exits).
        unsafe {
            eglDestroyContext(self.display, self.context);
        }
    }
}

/// One window surface. Dropping it destroys the EGL surface — the caller must have released
/// it from the current thread first ([`EglContext::release_current`]).
pub(super) struct EglSurface {
    display: EGLDisplay,
    surface: EGLSurface,
    pub(super) width: u32,
    pub(super) height: u32,
}

// SAFETY: as for `EglContext` — moved onto the render thread, never shared.
unsafe impl Send for EglSurface {}

impl EglSurface {
    /// Present. `Err` = the surface is gone underneath us (the window was destroyed) — the
    /// caller drops it and waits for the next one.
    pub(super) fn swap(&self) -> Result<()> {
        // SAFETY: valid display + surface owned by this object.
        if unsafe { eglSwapBuffers(self.display, self.surface) } == EGL_TRUE {
            Ok(())
        } else {
            // SAFETY: plain query.
            Err(anyhow!("eglSwapBuffers: 0x{:x}", unsafe { eglGetError() }))
        }
    }

    /// Re-read the surface's pixel size. Called once per frame: the window resizes on the
    /// system's schedule, not on the one command that announces it.
    pub(super) fn refresh_size(&mut self) {
        let (mut w, mut h) = (0, 0);
        // SAFETY: valid display + surface owned by this object.
        unsafe {
            eglQuerySurface(self.display, self.surface, EGL_WIDTH, &mut w);
            eglQuerySurface(self.display, self.surface, EGL_HEIGHT, &mut h);
        }
        self.width = w.max(1) as u32;
        self.height = h.max(1) as u32;
    }
}

impl Drop for EglSurface {
    fn drop(&mut self) {
        // SAFETY: the surface is this object's own; the render loop released it from the
        // thread before dropping.
        unsafe {
            eglDestroySurface(self.display, self.surface);
        }
    }
}
