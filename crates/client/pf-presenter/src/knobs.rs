//! The `PUNKTFUNK_*` variables the presenter reads, read once by [`read`] at `Shell::open`
//! (and by `probe_backend`).
//!
//! The shell keeps the [`Knobs`] and hands them to every `Presenter::new`; no other module
//! of this crate reads one. Each field keeps the parse its use site had: an `env_on` switch
//! is a `bool`, or an `Option<bool>` where unset means a default the site picks; a string a
//! site matches itself stays raw. The docs-site's `configuration.md` lists the documented
//! ones. `PUNKTFUNK_NATIVE_SCANOUT` arms the lane through the decoder's own
//! `native_scanout_wanted`, so the two agree.

use punktfunk_core::env_on;

/// The fault `PUNKTFUNK_HW_FAULT` injects.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HwFault {
    /// `import`: every hardware-frame import fails, so demotion runs without a broken driver.
    Import,
}

#[derive(Clone)]
pub(crate) struct Knobs {
    /// `PUNKTFUNK_VK_DEVICE=<index>`: the present device, raw `vkEnumeratePhysicalDevices` order.
    pub(crate) vk_device: Option<usize>,
    /// `PUNKTFUNK_VK_ADAPTER=<name>`: the marketing name ranked first, exact then substring.
    pub(crate) vk_adapter: Option<String>,
    /// `PUNKTFUNK_HDR10=0` refuses the HDR10 swapchain; PQ stays shader-tonemapped.
    pub(crate) hdr10: bool,
    /// `PUNKTFUNK_SDR_8BIT=1`: an 8-bit SDR swapchain. A compositor may scan out only those.
    pub(crate) sdr_8bit: bool,
    /// `PUNKTFUNK_VRR_FIFO=1` opts into the FIFO-first ladder for variable-refresh panels.
    pub(crate) vrr_fifo: bool,
    /// `PUNKTFUNK_PRESENT_WAIT2=1`: time presents with `VK_KHR_present_wait2`.
    pub(crate) present_wait2: bool,
    /// `PUNKTFUNK_PRESENT_TIMING=1|0`: read the engine's display stamps; unset is on on Linux.
    pub(crate) present_timing: Option<bool>,
    /// `PUNKTFUNK_FULLSCREEN_EXCLUSIVE=1`: take the monitor with `VK_EXT_full_screen_exclusive`.
    #[cfg(windows)]
    pub(crate) fullscreen_exclusive: bool,
    /// `PUNKTFUNK_PRESENT_MODE=fifo|mailbox|immediate|fifo_relaxed` pins the present mode.
    pub(crate) present_mode: Option<String>,
    /// `PUNKTFUNK_PRESENT_BACKEND=d3d11|vulkan` forces the presenter backend (`backend::choose`).
    pub(crate) present_backend: Option<String>,
    /// `PUNKTFUNK_PRESENTER=arrival` forces the latency drain without a rebuild.
    pub(crate) presenter_arrival: bool,
    /// `PUNKTFUNK_OSD_SCALE`: a positive factor on the display's own scale for the overlay.
    pub(crate) osd_scale: Option<f32>,
    /// `PUNKTFUNK_DRM_CARD=<n>`: SDL's KMSDRM device index.
    pub(crate) drm_card: Option<String>,
    /// `PUNKTFUNK_DIRECT_PRESENT=0` keeps every lane on the video image: an A/B on one build.
    pub(crate) direct_present: bool,
    /// `PUNKTFUNK_TONEMAP_PEAK`: the PQ→SDR peak in 203-nit units; unset takes the CSC's default.
    pub(crate) tonemap_peak: Option<f32>,
    /// `PUNKTFUNK_D3D11_NO_MUTEX` set to anything skips the keyed mutex (debugging: torn frames).
    #[cfg(windows)]
    pub(crate) d3d11_no_mutex: bool,
    /// `PUNKTFUNK_D3D11_PLANAR=1|0` overrides the vendor's planar-slot default both ways.
    #[cfg(windows)]
    pub(crate) d3d11_planar: Option<bool>,
    /// `PUNKTFUNK_NATIVE_SCANOUT=flip` hands the window between the lane and the swapchain.
    #[cfg(target_os = "linux")]
    pub(crate) native_scanout: Option<String>,
    /// On unless `PUNKTFUNK_SURFACE_FEEDBACK=0`: one feedback object per present.
    #[cfg(target_os = "linux")]
    pub(crate) surface_feedback: bool,
    /// `PUNKTFUNK_VRR_PACE=0` keeps the latency intent arrival-driven on a measured panel,
    /// variable or fixed.
    pub(crate) vrr_pace: bool,
    /// `PUNKTFUNK_PACE_TAIL=0` holds at the MAD alone, without the late tail.
    pub(crate) pace_tail: bool,
    /// `PUNKTFUNK_HW_FAULT=import` faults every hardware-frame import.
    pub(crate) hw_fault: Option<HwFault>,
}

/// The process environment's knobs, now.
pub(crate) fn read() -> Knobs {
    let var = |name| std::env::var(name).ok();
    Knobs {
        vk_device: var("PUNKTFUNK_VK_DEVICE").and_then(|v| v.parse().ok()),
        vk_adapter: var("PUNKTFUNK_VK_ADAPTER"),
        hdr10: env_on("PUNKTFUNK_HDR10") != Some(false),
        sdr_8bit: env_on("PUNKTFUNK_SDR_8BIT") == Some(true),
        vrr_fifo: env_on("PUNKTFUNK_VRR_FIFO") == Some(true),
        present_wait2: env_on("PUNKTFUNK_PRESENT_WAIT2") == Some(true),
        present_timing: env_on("PUNKTFUNK_PRESENT_TIMING"),
        #[cfg(windows)]
        fullscreen_exclusive: env_on("PUNKTFUNK_FULLSCREEN_EXCLUSIVE") == Some(true),
        present_mode: var("PUNKTFUNK_PRESENT_MODE"),
        present_backend: var("PUNKTFUNK_PRESENT_BACKEND"),
        presenter_arrival: var("PUNKTFUNK_PRESENTER").as_deref() == Some("arrival"),
        osd_scale: var("PUNKTFUNK_OSD_SCALE")
            .and_then(|s| s.trim().parse::<f32>().ok())
            .filter(|v| v.is_finite() && *v > 0.0),
        drm_card: var("PUNKTFUNK_DRM_CARD"),
        direct_present: env_on("PUNKTFUNK_DIRECT_PRESENT") != Some(false),
        tonemap_peak: var("PUNKTFUNK_TONEMAP_PEAK").and_then(|v| v.parse::<f32>().ok()),
        #[cfg(windows)]
        d3d11_no_mutex: std::env::var_os("PUNKTFUNK_D3D11_NO_MUTEX").is_some(),
        #[cfg(windows)]
        d3d11_planar: env_on("PUNKTFUNK_D3D11_PLANAR"),
        #[cfg(target_os = "linux")]
        native_scanout: var("PUNKTFUNK_NATIVE_SCANOUT"),
        #[cfg(target_os = "linux")]
        surface_feedback: env_on("PUNKTFUNK_SURFACE_FEEDBACK") != Some(false),
        vrr_pace: env_on("PUNKTFUNK_VRR_PACE") != Some(false),
        pace_tail: env_on("PUNKTFUNK_PACE_TAIL") != Some(false),
        hw_fault: std::env::var_os("PUNKTFUNK_HW_FAULT")
            .is_some_and(|v| v == "import")
            .then_some(HwFault::Import),
    }
}
