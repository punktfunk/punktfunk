//! The VideoProc context: ingest colour conversion into the encoder's input surface.
//!
//! One context per session, sized to the encoder's visible picture. A packed-RGB
//! dmabuf and CPU RGB on a staging surface go through it; a producer's own NV12/P010
//! at the session's size is encoded as imported instead (`Encoder::submit_dmabuf`).
//! A larger source — a mirrored head — is scaled down on the same pass.

use anyhow::Context as _;
use anyhow::Result;
use pf_vaapi::vpp::VaProcPipelineParameterBuffer;
use pf_vaapi::vpp::VaRectangle;
use pf_vaapi::vpp::VA_ENTRYPOINT_VIDEO_PROC;
use pf_vaapi::vpp::VA_FILTER_SCALING_HQ;
use pf_vaapi::vpp::VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE;
use pf_vaapi::vpp::VA_PROFILE_NONE;

use crate::Display;
use crate::VaConfigId;
use crate::VaContextId;
use crate::VaSurfaceId;

pub struct Vpp {
    config: VaConfigId,
    context: VaContextId,
    width: u32,
    height: u32,
    /// The source rectangle converted (`x, y, width, height`); `None` is the whole picture.
    pub crop: Option<[u32; 4]>,
}

impl Vpp {
    /// Open a VideoProc context on `display` writing `width`×`height` visible pictures.
    /// No render targets are pinned: the target is named per picture.
    pub fn new(display: &Display, width: u32, height: u32) -> Result<Self> {
        let config = display
            .create_config(VA_PROFILE_NONE, VA_ENTRYPOINT_VIDEO_PROC, &[])
            .context("VideoProc")?;
        let context = display
            .create_context(config.id(), width, height, &mut [])
            .context("VideoProc")?;
        Ok(Self {
            context: context.keep(),
            config: config.keep(),
            width,
            height,
            crop: None,
        })
    }

    /// Convert the `source_size` picture of `source` — its [`crop`](Self::crop) when set —
    /// into this context's size in `target`, scaling with the HQ filter when the two differ, and wait for
    /// it: the encoder reads `target` on another queue, and libva orders nothing across
    /// contexts. The regions are explicit so a target padded to the macroblock grid
    /// is written, not scaled into.
    pub fn convert(
        &self,
        display: &Display,
        source: VaSurfaceId,
        source_size: (u32, u32),
        source_is_rgb: bool,
        colour: [u8; 3],
        target: VaSurfaceId,
    ) -> Result<()> {
        let [x, y, width, height] = self.crop.unwrap_or([0, 0, source_size.0, source_size.1]);
        let source_region = VaRectangle {
            x: x as i16,
            y: y as i16,
            width: width as u16,
            height: height as u16,
        };
        let output_region = VaRectangle {
            x: 0,
            y: 0,
            width: self.width as u16,
            height: self.height as u16,
        };
        let mut params = VaProcPipelineParameterBuffer::convert(source, source_is_rgb, colour);
        params.surface_region = &source_region;
        params.output_region = &output_region;
        if (width, height) != (self.width, self.height) {
            params.filter_flags = VA_FILTER_SCALING_HQ;
        }
        let buf = display.create_buffer(
            self.context,
            VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE,
            std::mem::size_of_val(&params),
            1,
            (&raw const params).cast(),
        )?;

        // SAFETY: `context` and `target` are live on this display; a success here
        // is closed by the `end_picture` below, even if the render fails.
        let begun = display.va.check("vaBeginPicture(VideoProc)", unsafe {
            (display.va.begin_picture)(display.display, self.context, target)
        });
        if let Err(e) = begun {
            display.destroy_buffers(&[buf]);
            return Err(e);
        }
        let mut ids = [buf];
        // SAFETY: one live buffer id created on this context; the call does not
        // retain the array.
        let rendered = display.va.check("vaRenderPicture(VideoProc)", unsafe {
            (display.va.render_picture)(display.display, self.context, ids.as_mut_ptr(), 1)
        });
        // SAFETY: closes the picture opened above; must run after a failed render too.
        let ended = unsafe { (display.va.end_picture)(display.display, self.context) };
        display.destroy_buffers(&[buf]);
        rendered?;
        display.va.check("vaEndPicture(VideoProc)", ended)?;
        // SAFETY: `target` is live; sync returns once the conversion has landed.
        display.va.check("vaSyncSurface(VideoProc)", unsafe {
            (display.va.sync_surface)(display.display, target)
        })
    }

    /// Release the context and config; consuming `self` makes it run once. No `Drop`:
    /// the display is the caller's, and it must outlive this call.
    pub fn destroy(self, display: &Display) {
        // SAFETY: both ids were created on this display in `new`; destroyed once,
        // context before config.
        unsafe {
            (display.va.destroy_context)(display.display, self.context);
            (display.va.destroy_config)(display.display, self.config);
        }
    }
}
