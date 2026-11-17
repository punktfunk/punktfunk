//! Every `PUNKTFUNK_*` switch the crate reads by name. [`Knobs::from_env`] is the crate's only
//! such read of the environment or the settings registry, and each knob keeps its own parser.
//! A few switches arrive resolved, through `pf_host_config::config()` and `pf_zerocopy`.
//!
//! An `open_*` entry point resolves one [`Knobs`] and hands it down, so a console change
//! reaches the next session. [`Knobs::process`] is the first resolution, kept for the process:
//! the frame-path hold and the IDD diagnostics read it, all env-only.

/// One resolution of every knob the crate reads.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default))]
pub(crate) struct Knobs {
    /// `PUNKTFUNK_SYNTH_NOISE=1`: the fast synthetic source paints noise.
    pub synth_noise: bool,
    /// See [`direct_capture`].
    #[cfg(target_os = "linux")]
    pub direct_capture: bool,
    /// Drive a virtual output as a PipeWire lazy driver. A producer that emits RequestProcess
    /// (Mutter ≥ 49 virtual monitors) paints only in a graph cycle the consumer starts, so the
    /// encode loop's slot is the one tick: no compositor timer to beat against, no throttle to
    /// lose frames in, no extra render. Producers without it are never driven.
    /// `PUNKTFUNK_LAZY_CAPTURE=0` restores the producer-driven stream.
    #[cfg(target_os = "linux")]
    pub lazy_capture: bool,
    /// Offer PipeWire explicit sync (`SPA_META_SyncTimeline`) on the dmabuf lane. A producer
    /// that takes it hands over a fence at each buffer's acquire point and waits on the release
    /// point this side signals, instead of finishing the GPU itself. KWin on NVIDIA
    /// `glFinish()`es its compositor thread per cast frame otherwise — ~9 ms under a game's
    /// load, the whole 120 → 111 fps gap. `PUNKTFUNK_EXPLICIT_SYNC=0` keeps the implicit path.
    #[cfg(target_os = "linux")]
    pub explicit_sync: bool,
    /// `PUNKTFUNK_KWIN_PACED=1` sends KWin no ceiling, so it fixates its own: the output's
    /// refresh. KWin up to 6.7 throttles that on a QTimer counted in whole milliseconds
    /// (8.333 ms → 8), so the cadence jitters against the real refresh.
    /// Unpaced, KWin up to 6.7 gets no ceiling and delivers on its damage signal; KWin 6.8
    /// (millihertz offer) gets the stream rate as its ceiling and paces the cast at it.
    #[cfg(target_os = "linux")]
    pub kwin_paced: bool,
    /// `PUNKTFUNK_FORCE_SHM=1`: the race-free SHM download path, no dmabuf. SHM is 8-bit, so
    /// it drops an HDR offer.
    #[cfg(target_os = "linux")]
    pub force_shm: bool,
    /// Negotiate the producer's own NV12. `PUNKTFUNK_PIPEWIRE_NV12=0` restores packed RGB.
    #[cfg(target_os = "linux")]
    pub pipewire_nv12: bool,
    /// Keep a published buffer from the producer until encode lets go.
    /// `PUNKTFUNK_ZEROCOPY_HOLD=0` restores the immediate requeue (racy); the raw passthrough
    /// then never publishes an unheld dmabuf and takes the CPU fallback. Once per process.
    #[cfg(target_os = "linux")]
    pub zerocopy_hold: bool,
    /// Stamp the wire from the buffer header. `PUNKTFUNK_CAPTURE_HDR_PTS=0` keeps the wire on
    /// delivery stamps, on the portal and the direct path alike.
    #[cfg(target_os = "linux")]
    pub header_pts: bool,
    /// `PUNKTFUNK_PW_FIXED_POD="WxH"`: one fixed format, to bisect against a producer's
    /// EnumFormat.
    #[cfg(target_os = "linux")]
    pub fixed_pod: Option<(u32, u32)>,
    /// `PUNKTFUNK_NVENC_CLOCK_BOOST=1`: hold NVIDIA's low-latency clock boost while a driver
    /// NVENC session is live.
    #[cfg(target_os = "windows")]
    pub nvenc_clock_boost: bool,
    /// `PUNKTFUNK_GPU_PRIORITY_CLASS` as the stall lines name it; `pf_frame::dxgi` applies the
    /// class itself. Once per process.
    #[cfg(target_os = "windows")]
    pub gpu_priority_class: &'static str,
    /// `PFVD_NO_RT_GPU` is set: the driver's REALTIME GPU priority is off. WUDFHost reads the
    /// same variable. Once per process.
    #[cfg(target_os = "windows")]
    pub no_rt_gpu: bool,
    /// `PUNKTFUNK_IDD_FLOW`: the DxgKrnl ETW session alone, printing a present-flow line every
    /// `FLOW_EVERY`. Once per process.
    #[cfg(target_os = "windows")]
    pub idd_flow: bool,
    /// `PUNKTFUNK_IDD_DIAG`: the IDD capturer's full diagnostics — the micro-probe engine, the
    /// DxgKrnl ETW session and the access-unit dump. All three are off in a normal session,
    /// because standing fence/scanline/DWM traffic and an ETW session alter the very path a
    /// disturbance report describes. `1` puts the dump beside the driver log
    /// (`%SystemRoot%\Temp`); any other non-empty value is the directory it goes in. Once per
    /// process.
    #[cfg(target_os = "windows")]
    pub idd_diag: Option<std::path::PathBuf>,
}

impl Knobs {
    /// Every knob, read now.
    pub(crate) fn from_env() -> Self {
        use pf_host_config::env_on;
        Knobs {
            synth_noise: env_on("PUNKTFUNK_SYNTH_NOISE") == Some(true),
            #[cfg(target_os = "linux")]
            direct_capture: pf_host_config::row_bool("PUNKTFUNK_DIRECT_CAPTURE"),
            #[cfg(target_os = "linux")]
            lazy_capture: pf_host_config::row_bool("PUNKTFUNK_LAZY_CAPTURE"),
            #[cfg(target_os = "linux")]
            explicit_sync: env_on("PUNKTFUNK_EXPLICIT_SYNC").unwrap_or(true),
            #[cfg(target_os = "linux")]
            kwin_paced: pf_host_config::row_bool("PUNKTFUNK_KWIN_PACED"),
            #[cfg(target_os = "linux")]
            force_shm: env_on("PUNKTFUNK_FORCE_SHM").unwrap_or(false),
            #[cfg(target_os = "linux")]
            pipewire_nv12: env_on("PUNKTFUNK_PIPEWIRE_NV12").unwrap_or(true),
            #[cfg(target_os = "linux")]
            zerocopy_hold: env_on("PUNKTFUNK_ZEROCOPY_HOLD").unwrap_or(true),
            #[cfg(target_os = "linux")]
            header_pts: env_on("PUNKTFUNK_CAPTURE_HDR_PTS").unwrap_or(true),
            #[cfg(target_os = "linux")]
            fixed_pod: std::env::var("PUNKTFUNK_PW_FIXED_POD")
                .ok()
                .and_then(|v| v.split_once('x').map(|(w, h)| (w.parse(), h.parse())))
                .and_then(|(w, h)| Some((w.ok()?, h.ok()?))),
            #[cfg(target_os = "windows")]
            nvenc_clock_boost: env_on("PUNKTFUNK_NVENC_CLOCK_BOOST") == Some(true),
            #[cfg(target_os = "windows")]
            gpu_priority_class: match std::env::var("PUNKTFUNK_GPU_PRIORITY_CLASS")
                .ok()
                .as_deref()
            {
                Some("off") => "off",
                Some("normal") => "normal",
                Some("high") => "high",
                _ => "REALTIME (default)",
            },
            #[cfg(target_os = "windows")]
            no_rt_gpu: std::env::var_os("PFVD_NO_RT_GPU").is_some(),
            #[cfg(target_os = "windows")]
            idd_flow: std::env::var("PUNKTFUNK_IDD_FLOW")
                .is_ok_and(|v| !matches!(v.trim(), "" | "0" | "off" | "false")),
            #[cfg(target_os = "windows")]
            idd_diag: idd_diag(),
        }
    }

    /// The first [`Knobs::from_env`], kept for the process: an env edited later is stale.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    pub(crate) fn process() -> &'static Knobs {
        static ONCE: std::sync::OnceLock<Knobs> = std::sync::OnceLock::new();
        ONCE.get_or_init(Knobs::from_env)
    }
}

#[cfg(target_os = "windows")]
fn idd_diag() -> Option<std::path::PathBuf> {
    let raw = std::env::var("PUNKTFUNK_IDD_DIAG").ok()?;
    match raw.trim() {
        "" | "0" | "off" | "false" => None,
        "1" | "on" | "true" => {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            Some(std::path::PathBuf::from(root).join("Temp"))
        }
        path => Some(std::path::PathBuf::from(path)),
    }
}

/// Whether to capture a compositor's output directly with `ext-image-copy-capture-v1`
/// instead of going through the xdg ScreenCast portal.
///
/// The portal is a second clock in the path: xdg-desktop-portal-hyprland (1.3–1.5) re-requests
/// each frame on a millisecond timer with a 6 ms floor, which halves the rate at 165 Hz and
/// adds ~3 ms to every frame's age. Capturing the protocol ourselves removes both.
/// `PUNKTFUNK_DIRECT_CAPTURE=0` keeps the portal.
#[cfg(target_os = "linux")]
pub fn direct_capture() -> bool {
    Knobs::from_env().direct_capture
}
