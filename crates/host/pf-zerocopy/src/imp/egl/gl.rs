//! GL FFI and shaders for [`super`]'s EGL zero-copy blit: `libGL` entry points,
//! fullscreen-triangle sources (BGRA swizzle, NV12 / YUV444 BT.709 convert), and compile
//! helpers. The importer and de-tiling passes live in [`super`].

#![allow(non_upper_case_globals)]

use anyhow::{bail, ensure, Result};
use std::os::raw::{c_char, c_int, c_void};

pub(crate) const GL_TEXTURE_2D: u32 = 0x0DE1;
pub(crate) const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
pub(crate) const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
pub(crate) const GL_LINEAR: c_int = 0x2601;
pub(crate) const GL_NEAREST: c_int = 0x2600;
pub(crate) const GL_RGBA8: u32 = 0x8058;
// NV12 convert targets: R8 luma (full-res), RG8 interleaved chroma (half-res).
pub(crate) const GL_R8: u32 = 0x8229;
pub(crate) const GL_RG8: u32 = 0x822B;
// Client format/type for self-test texture uploads only.
pub(crate) const GL_RGBA: u32 = 0x1908;
pub(crate) const GL_UNSIGNED_BYTE: u32 = 0x1401;
pub(crate) const GL_FRAMEBUFFER: u32 = 0x8D40;
pub(crate) const GL_COLOR_ATTACHMENT0: u32 = 0x8CE0;
pub(crate) const GL_FRAMEBUFFER_COMPLETE: u32 = 0x8CD5;
pub(crate) const GL_TEXTURE0: u32 = 0x84C0;
pub(crate) const GL_TRIANGLES: u32 = 0x0004;
pub(crate) const GL_VERTEX_SHADER: u32 = 0x8B31;
pub(crate) const GL_FRAGMENT_SHADER: u32 = 0x8B30;
pub(crate) const GL_COMPILE_STATUS: u32 = 0x8B81;
pub(crate) const GL_LINK_STATUS: u32 = 0x8B82;

// libglvnd `libGL` dispatches these through the current EGL/GL context.
#[link(name = "GL")]
unsafe extern "C" {
    pub(crate) fn glGenTextures(n: c_int, textures: *mut u32);
    pub(crate) fn glBindTexture(target: u32, texture: u32);
    pub(crate) fn glTexParameteri(target: u32, pname: u32, param: c_int);
    pub(crate) fn glDeleteTextures(n: c_int, textures: *const u32);
    pub(crate) fn glTexStorage2D(
        target: u32,
        levels: c_int,
        internalformat: u32,
        width: c_int,
        height: c_int,
    );
    pub(crate) fn glGetError() -> u32;
    pub(crate) fn glGenFramebuffers(n: c_int, framebuffers: *mut u32);
    pub(crate) fn glDeleteFramebuffers(n: c_int, framebuffers: *const u32);
    pub(crate) fn glBindFramebuffer(target: u32, framebuffer: u32);
    pub(crate) fn glFramebufferTexture2D(
        target: u32,
        attachment: u32,
        textarget: u32,
        texture: u32,
        level: c_int,
    );
    pub(crate) fn glCheckFramebufferStatus(target: u32) -> u32;
    pub(crate) fn glViewport(x: c_int, y: c_int, width: c_int, height: c_int);
    pub(crate) fn glGenVertexArrays(n: c_int, arrays: *mut u32);
    pub(crate) fn glDeleteVertexArrays(n: c_int, arrays: *const u32);
    pub(crate) fn glBindVertexArray(array: u32);
    pub(crate) fn glDrawArrays(mode: u32, first: c_int, count: c_int);
    pub(crate) fn glActiveTexture(texture: u32);
    pub(crate) fn glUseProgram(program: u32);
    pub(crate) fn glFlush();
    pub(crate) fn glCreateShader(shader_type: u32) -> u32;
    pub(crate) fn glShaderSource(
        shader: u32,
        count: c_int,
        string: *const *const c_char,
        length: *const c_int,
    );
    pub(crate) fn glCompileShader(shader: u32);
    pub(crate) fn glGetShaderiv(shader: u32, pname: u32, params: *mut c_int);
    pub(crate) fn glDeleteShader(shader: u32);
    pub(crate) fn glCreateProgram() -> u32;
    pub(crate) fn glAttachShader(program: u32, shader: u32);
    pub(crate) fn glLinkProgram(program: u32);
    pub(crate) fn glGetProgramiv(program: u32, pname: u32, params: *mut c_int);
    pub(crate) fn glGetUniformLocation(program: u32, name: *const c_char) -> c_int;
    pub(crate) fn glUniform1i(location: c_int, v0: c_int);
    pub(crate) fn glDeleteProgram(program: u32);
    pub(crate) fn glTexSubImage2D(
        target: u32,
        level: c_int,
        xoffset: c_int,
        yoffset: c_int,
        width: c_int,
        height: c_int,
        format: u32,
        type_: u32,
        pixels: *const c_void,
    );
}

/// `glEGLImageTargetTexture2DOES` — `eglGetProcAddress`, not `#[link]`.
pub(crate) type EglImageTargetFn = unsafe extern "system" fn(u32, *mut c_void);

// `.bgra` matches the encoder's BGRx; dest is linear `GL_RGBA8` so CUDA can register it.
pub(crate) const VERT_SRC: &[u8] = b"#version 330 core\nout vec2 v_tex;\nvoid main(){vec2 p=vec2(float((gl_VertexID<<1)&2),float(gl_VertexID&2));v_tex=p;gl_Position=vec4(p*2.0-1.0,0.0,1.0);}\n";
pub(crate) const FRAG_SRC: &[u8] = b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){o_color=texture(image,v_tex).bgra;}\n";

/// BT.709 Y, U and V of `c` (RGB in [0,1]) as normalized GLSL: full range (0..255, 128±127.5),
/// or studio (16+219, 128±112).
fn bt709_glsl(full_range: bool) -> [&'static str; 3] {
    if full_range {
        [
            "0.2126*c.r+0.7152*c.g+0.0722*c.b",
            "0.5-0.1146*c.r-0.3854*c.g+0.5000*c.b",
            "0.5+0.5000*c.r-0.4542*c.g-0.0458*c.b",
        ]
    } else {
        [
            "(16.0+219.0*(0.2126*c.r+0.7152*c.g+0.0722*c.b))/255.0",
            "(128.0+224.0*(-0.1146*c.r-0.3854*c.g+0.5000*c.b))/255.0",
            "(128.0+224.0*(0.5000*c.r-0.4542*c.g-0.0458*c.b))/255.0",
        ]
    }
}

const FRAG_HEAD: &str =
    "#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){";

/// NV12's passes at `full_range` or studio: full-res `R8` luma, then half-res `RG8` chroma
/// (R=U, G=V). Chroma is left-sited (H.273 type 0): two `GL_LINEAR` taps one texel apart give
/// [1 2 1] across columns 2i-1..2i+1 over both rows. Same matrix as Windows.
pub(crate) fn nv12_frag_sources(full_range: bool) -> (Vec<u8>, Vec<u8>) {
    let [y, u, v] = bt709_glsl(full_range);
    let luma = format!(
        "{FRAG_HEAD}vec3 c=texture(image,v_tex).rgb;float Y={y};o_color=vec4(clamp(Y,0.0,1.0),0.0,0.0,1.0);}}\n"
    );
    let chroma = format!(
        "{FRAG_HEAD}float w=float(textureSize(image,0).x);vec3 c=0.5*(texture(image,v_tex).rgb+texture(image,vec2(max(v_tex.x-1.0/w,0.5/w),v_tex.y)).rgb);float U={u};float V={v};o_color=vec4(clamp(U,0.0,1.0),clamp(V,0.0,1.0),0.0,1.0);}}\n"
    );
    (luma.into_bytes(), chroma.into_bytes())
}

/// Planar YUV444 at `full_range` or studio: three full-res `R8` passes, NV12's matrix.
pub(crate) fn yuv444_frag_sources(full_range: bool) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let [y, u, v] = bt709_glsl(full_range);
    let plane = |name: &str, expr: &str| {
        format!(
            "{FRAG_HEAD}vec3 c=texture(image,v_tex).rgb;float {name}={expr};o_color=vec4(clamp({name},0.0,1.0),0.0,0.0,1.0);}}\n"
        )
        .into_bytes()
    };
    (plane("Y", y), plane("U", u), plane("V", v))
}

pub(crate) unsafe fn compile_shader(kind: u32, src: &[u8]) -> Result<u32> {
    // SAFETY: the GL context is current on this thread. `src` is a live slice; `ptr`/`len`
    // outlive the synchronous `glShaderSource`. The shader name is deleted on compile failure.
    unsafe {
        let sh = glCreateShader(kind);
        ensure!(sh != 0, "glCreateShader failed");
        let ptr = src.as_ptr() as *const c_char;
        let len = src.len() as c_int;
        glShaderSource(sh, 1, &ptr, &len);
        glCompileShader(sh);
        let mut ok: c_int = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &mut ok);
        if ok == 0 {
            glDeleteShader(sh);
            bail!("GL shader compile failed");
        }
        Ok(sh)
    }
}

pub(crate) unsafe fn compile_program_with(frag: &[u8]) -> Result<u32> {
    // SAFETY: the GL context is current on this thread. Every GL name created here is deleted
    // on failure; `c"image"` is a NUL-terminated literal.
    unsafe {
        // Callers retry per frame; a leak here is unbounded.
        let vs = compile_shader(GL_VERTEX_SHADER, VERT_SRC)?;
        let fs = match compile_shader(GL_FRAGMENT_SHADER, frag) {
            Ok(fs) => fs,
            Err(e) => {
                glDeleteShader(vs);
                return Err(e);
            }
        };
        let prog = glCreateProgram();
        if prog == 0 {
            glDeleteShader(vs);
            glDeleteShader(fs);
            bail!("glCreateProgram failed");
        }
        glAttachShader(prog, vs);
        glAttachShader(prog, fs);
        glLinkProgram(prog);
        glDeleteShader(vs);
        glDeleteShader(fs);
        let mut ok: c_int = 0;
        glGetProgramiv(prog, GL_LINK_STATUS, &mut ok);
        if ok == 0 {
            glDeleteProgram(prog);
            bail!("GL program link failed");
        }
        glUseProgram(prog);
        let loc = glGetUniformLocation(prog, c"image".as_ptr());
        if loc >= 0 {
            glUniform1i(loc, 0);
        }
        glUseProgram(0);
        Ok(prog)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Full range pins the exact unscaled shader source; studio scales every plane into
    /// 16..235 / 16..240.
    #[test]
    fn the_sources_follow_the_range() {
        let (y, uv) = nv12_frag_sources(true);
        assert_eq!(y, b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){vec3 c=texture(image,v_tex).rgb;float Y=0.2126*c.r+0.7152*c.g+0.0722*c.b;o_color=vec4(clamp(Y,0.0,1.0),0.0,0.0,1.0);}\n");
        assert_eq!(uv, b"#version 330 core\nuniform sampler2D image;\nin vec2 v_tex;\nout vec4 o_color;\nvoid main(){float w=float(textureSize(image,0).x);vec3 c=0.5*(texture(image,v_tex).rgb+texture(image,vec2(max(v_tex.x-1.0/w,0.5/w),v_tex.y)).rgb);float U=0.5-0.1146*c.r-0.3854*c.g+0.5000*c.b;float V=0.5+0.5000*c.r-0.4542*c.g-0.0458*c.b;o_color=vec4(clamp(U,0.0,1.0),clamp(V,0.0,1.0),0.0,1.0);}\n");
        let (y444, u444, _) = yuv444_frag_sources(true);
        assert_eq!(y444, y);
        assert!(!String::from_utf8_lossy(&u444).contains("224.0"));

        let has = |src: &[u8], anchor: &str| String::from_utf8_lossy(src).contains(anchor);
        let (y, uv) = nv12_frag_sources(false);
        let (y444, u444, v444) = yuv444_frag_sources(false);
        assert!(has(&y, "float Y=(16.0+219.0*(") && y444 == y);
        assert!(has(&uv, "float U=(128.0+224.0*(") && has(&uv, "float V=(128.0+224.0*("));
        assert!(has(&u444, "float U=(128.0+224.0*(") && has(&v444, "float V=(128.0+224.0*("));
    }
}
