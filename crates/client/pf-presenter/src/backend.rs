//! The presenter `run/` draws through: one enum over the backends, one `impl` forwarding
//! every call the shell makes. The frame, outcome and timing types stay in `vk`.
//!
//! `Vk` presents everywhere; `D3d` (Windows) is the DXGI presenter in `d3d/`, and answers
//! the Vulkan-only calls as a presenter without that lane: no overlay device, native lanes
//! declined. The pure picks below build on every platform and are tabled.

/// Which presenter a session builds. Only Windows builds `D3d`.
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Vk,
    D3d,
}

/// D1: `forced` (`d3d11` or `vulkan`; anything else reads as unset) wins; otherwise Vulkan
/// where its loader answers and the device imports D3D11, else D3D11. `console` (`--browse`)
/// needs the overlay, which only Vulkan draws: it keeps Vulkan wherever a loader answers.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pick_backend(
    forced: Option<&str>,
    has_vulkan: bool,
    vulkan_imports_d3d11: bool,
    console: bool,
) -> Backend {
    match forced {
        Some("d3d11") => Backend::D3d,
        Some("vulkan") => Backend::Vk,
        _ if has_vulkan && (vulkan_imports_d3d11 || console) => Backend::Vk,
        _ => Backend::D3d,
    }
}

#[cfg(any(target_os = "linux", windows))]
impl Backend {
    /// SDL's Vulkan flag on a window this backend presents to; DXGI takes a plain window.
    pub(crate) fn flag_window(self, b: &mut sdl3::video::WindowBuilder) {
        if self == Backend::Vk {
            b.vulkan();
        }
    }
}

/// The backend this process presents with, before its window exists (see [`decide`]).
/// `console` is `--browse`.
#[cfg(any(target_os = "linux", windows))]
pub(crate) fn choose(knobs: &crate::knobs::Knobs, console: bool) -> Backend {
    let forced = knobs.present_backend.as_deref();
    if let Some(v) = forced.filter(|v| !matches!(*v, "d3d11" | "vulkan")) {
        tracing::warn!(
            value = v,
            "PUNKTFUNK_PRESENT_BACKEND takes d3d11 or vulkan; ignoring it"
        );
    }
    let (backend, probe) = decide(knobs, console);
    if cfg!(windows) {
        tracing::info!(?backend, forced, console, vulkan_imports_d3d11 = ?probe,
            "presenter backend");
    }
    backend
}

/// The pick and the Vulkan probe's answer. Windows asks Vulkan without a surface
/// (`vk::probe_import`) unless the knob forces a backend; elsewhere it is `Vk`. The answer is
/// `None` when forced, without a Vulkan loader, and off Windows.
#[cfg(any(target_os = "linux", windows))]
fn decide(knobs: &crate::knobs::Knobs, console: bool) -> (Backend, Option<bool>) {
    #[cfg(windows)]
    {
        let forced = knobs.present_backend.as_deref();
        let probe = match forced {
            Some("d3d11" | "vulkan") => None,
            _ => crate::vk::probe_import(knobs),
        };
        let backend = pick_backend(forced, probe.is_some(), probe == Some(true), console);
        (backend, probe)
    }
    #[cfg(not(windows))]
    {
        let _ = (knobs, console);
        (Backend::Vk, None)
    }
}

/// `--probe-decode`'s answer from this process's environment: the backend a session would
/// present with, and why. A D3D11 pick also names the DXGI adapter it would present on.
#[cfg(any(target_os = "linux", windows))]
pub fn probe_backend() -> (&'static str, String) {
    let knobs = crate::knobs::read();
    let forced = matches!(knobs.present_backend.as_deref(), Some("d3d11" | "vulkan"));
    let (backend, probe) = decide(&knobs, false);
    let why = match probe {
        _ if !cfg!(windows) => "the only backend off Windows",
        _ if forced => "forced by PUNKTFUNK_PRESENT_BACKEND",
        None => "no Vulkan loader",
        Some(true) => "the Vulkan device imports D3D11 textures",
        Some(false) => "the Vulkan device can't import D3D11 textures",
    };
    match backend {
        Backend::Vk => ("vulkan", why.to_string()),
        #[cfg(windows)]
        Backend::D3d => match crate::d3d::adapter_name_for(&knobs) {
            Ok(adapter) => ("d3d11", format!("{why}; DXGI adapter {adapter}")),
            Err(e) => ("d3d11", format!("{why}; no DXGI adapter: {e:#}")),
        },
        #[cfg(not(windows))]
        Backend::D3d => ("d3d11", why.to_string()),
    }
}

/// The DXGI adapter to present on: the first hardware one whose name contains `wanted`
/// (case-insensitive), else the first hardware one. `names` is `(description, software)`
/// in DXGI's order; software adapters (WARP) never present.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pick_adapter(names: &[(String, bool)], wanted: Option<&str>) -> Option<usize> {
    let hardware = || names.iter().enumerate().filter(|(_, (_, sw))| !sw);
    let wanted = wanted
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty());
    wanted
        .and_then(|w| hardware().find(|(_, (n, _))| n.to_lowercase().contains(&w)))
        .or_else(|| hardware().next())
        .map(|(i, _)| i)
}

#[cfg(any(target_os = "linux", windows))]
pub(crate) use presenter::Presenter;

#[cfg(any(target_os = "linux", windows))]
mod presenter {
    use super::Backend;
    use crate::overlay::{OverlayFrame, SharedDevice};
    use crate::vk::{FrameInput, NativeVkOutcome, PresentPref, Presented, PresentedSample};
    use anyhow::{Context as _, Result};
    use pf_client_core::video::{NativeVkFrame, VulkanDecodeDevice};
    use sdl3::video::Window;

    // One per window, built once and never moved on a frame path: the size gap costs nothing.
    #[allow(clippy::large_enum_variant)]
    pub(crate) enum Presenter {
        Vk(crate::vk::Presenter),
        #[cfg(windows)]
        D3d(crate::d3d::Presenter),
    }

    /// The same call on whichever backend `$self` holds.
    macro_rules! each {
        ($self:expr, $p:ident => $call:expr) => {
            match $self {
                Presenter::Vk($p) => $call,
                #[cfg(windows)]
                Presenter::D3d($p) => $call,
            }
        };
    }

    /// The Vulkan presenter's answer, or `$d3d` on the D3D11 backend.
    macro_rules! vk_or {
        ($self:expr, $p:ident => $call:expr, $d3d:expr) => {
            match $self {
                Presenter::Vk($p) => $call,
                #[cfg(windows)]
                Presenter::D3d(_) => $d3d,
            }
        };
    }

    impl Presenter {
        /// `backend`'s presenter over `window`. SDL built the window with its Vulkan flag
        /// exactly when `backend` is `Vk`; `D3d` off Windows builds `Vk`.
        pub(crate) fn new(
            backend: Backend,
            window: &Window,
            pref: PresentPref,
            knobs: &crate::knobs::Knobs,
        ) -> Result<Presenter> {
            #[cfg(windows)]
            if backend == Backend::D3d {
                let p =
                    crate::d3d::Presenter::new(window, pref, knobs).context("D3D11 presenter")?;
                return Ok(Presenter::D3d(p));
            }
            let _ = backend;
            let exts = window
                .vulkan_instance_extensions()
                .map_err(|e| anyhow::anyhow!("vulkan instance extensions: {e}"))?;
            let p = crate::vk::Presenter::new(window, &exts, pref, knobs)
                .context("vulkan presenter")?;
            Ok(Presenter::Vk(p))
        }

        pub(crate) fn present<'a>(
            &mut self,
            window: &Window,
            input: FrameInput<'a>,
            overlay: Option<&OverlayFrame>,
        ) -> Result<Presented<'a>> {
            each!(self, p => p.present(window, input, overlay))
        }

        #[cfg(target_os = "linux")]
        pub(crate) fn present_native(
            &mut self,
            d: pf_client_core::video::DmabufFrame,
            pts_ns: u64,
            decoded_ns: u64,
        ) -> crate::wl_native::Outcome {
            each!(self, p => p.present_native(d, pts_ns, decoded_ns))
        }

        pub(crate) fn present_native_vk(
            &mut self,
            f: NativeVkFrame,
            pts_ns: u64,
            decoded_ns: u64,
        ) -> NativeVkOutcome {
            vk_or!(self, p => p.present_native_vk(f, pts_ns, decoded_ns), NativeVkOutcome::Declined(f))
        }

        #[cfg(feature = "pyrowave")]
        pub(crate) fn present_native_pyro(
            &mut self,
            f: pf_client_core::video_pyrowave::PyroWavePlanarFrame,
            pts_ns: u64,
            decoded_ns: u64,
        ) -> NativeVkOutcome<pf_client_core::video_pyrowave::PyroWavePlanarFrame> {
            vk_or!(self, p => p.present_native_pyro(f, pts_ns, decoded_ns), NativeVkOutcome::Declined(f))
        }

        pub(crate) fn recreate_swapchain(&mut self, window: &Window) -> Result<()> {
            each!(self, p => p.recreate_swapchain(window))
        }

        pub(crate) fn set_present_pref(
            &mut self,
            window: &Window,
            vsync: bool,
            allow_vrr: bool,
            fullscreen: bool,
        ) -> Result<()> {
            match self {
                Presenter::Vk(p) => p.set_present_pref(window, vsync, allow_vrr, fullscreen),
                #[cfg(windows)]
                Presenter::D3d(p) => {
                    p.set_present_pref(vsync, allow_vrr, fullscreen);
                    Ok(())
                }
            }
        }

        pub(crate) fn sync_native_overlay(
            &mut self,
            overlay: Option<&OverlayFrame>,
            logical: (u32, u32),
        ) {
            vk_or!(self, p => p.sync_native_overlay(overlay, logical), ())
        }

        pub(crate) fn set_video_fit(&mut self, fit: punktfunk_core::video_fit::VideoFit) {
            each!(self, p => p.set_video_fit(fit))
        }

        #[cfg(target_os = "linux")]
        pub(crate) fn supports_dmabuf(&self) -> bool {
            each!(self, p => p.supports_dmabuf())
        }

        /// Whether D3D11VA frames reach the screen: by import on Vulkan, natively on D3D11.
        #[cfg(windows)]
        pub(crate) fn supports_d3d11(&self) -> bool {
            vk_or!(self, p => p.supports_d3d11(), true)
        }

        /// The overlay's device; `None` where the backend has no Vulkan device to share.
        pub(crate) fn shared_device(&self) -> Option<SharedDevice> {
            vk_or!(self, p => Some(p.shared_device()), None)
        }

        pub(crate) fn vulkan_decode(&self) -> Option<VulkanDecodeDevice> {
            each!(self, p => p.vulkan_decode())
        }

        /// The present turn gates the decode lane's submits on the shared Vulkan queue; the
        /// D3D11 backend shares no queue.
        pub(crate) fn begin_present_turn(&self, turn: u64) {
            vk_or!(self, p => p.begin_present_turn(turn), ())
        }

        pub(crate) fn end_present_turn(&self, turn: u64) {
            vk_or!(self, p => p.end_present_turn(turn), ())
        }

        pub(crate) fn wait_idle(&self) {
            each!(self, p => p.wait_idle())
        }

        /// Idle the presenter's own queue work while the pump decodes; the D3D11 backend
        /// has no overlay to wait for.
        pub(crate) fn quiesce_queue(&mut self) {
            vk_or!(self, p => p.quiesce_queue(), ())
        }

        pub(crate) fn drop_video(&mut self) {
            each!(self, p => p.drop_video())
        }

        pub(crate) fn hdr_active(&self) -> bool {
            each!(self, p => p.hdr_active())
        }

        pub(crate) fn ten_bit(&self) -> bool {
            each!(self, p => p.ten_bit())
        }

        pub(crate) fn leave_hdr(&mut self, window: &Window) -> Result<()> {
            each!(self, p => p.leave_hdr(window))
        }

        pub(crate) fn set_hdr_metadata(&mut self, meta: punktfunk_core::quic::HdrMeta) {
            each!(self, p => p.set_hdr_metadata(meta))
        }

        pub(crate) fn present_timing_active(&self) -> bool {
            each!(self, p => p.present_timing_active())
        }

        pub(crate) fn glass_estimated(&self) -> bool {
            each!(self, p => p.glass_estimated())
        }

        pub(crate) fn measured_refresh_ns(&self) -> Option<u64> {
            each!(self, p => p.measured_refresh_ns())
        }

        /// DXGI reports no refresh of its own; its statistics' spacing is the measured one.
        pub(crate) fn engine_refresh(&self) -> Option<(u64, u64)> {
            vk_or!(self, p => p.engine_refresh(), None)
        }

        pub(crate) fn glass_source(&self) -> &'static str {
            each!(self, p => p.glass_source())
        }

        pub(crate) fn retarget_glass(&self, window: &Window) {
            vk_or!(self, p => p.retarget_glass(window), ())
        }

        pub(crate) fn take_unshown(&self) -> u32 {
            each!(self, p => p.take_unshown())
        }

        #[cfg(target_os = "linux")]
        pub(crate) fn take_scanout(&mut self) -> Option<(u32, u32)> {
            each!(self, p => p.take_scanout())
        }

        #[cfg(target_os = "linux")]
        pub(crate) fn take_native_zero_copy(&mut self) -> (u32, u32) {
            each!(self, p => p.take_native_zero_copy())
        }

        pub(crate) fn note_presented(&mut self, pts_ns: u64, decoded_ns: u64) {
            each!(self, p => p.note_presented(pts_ns, decoded_ns))
        }

        pub(crate) fn presents_outstanding(&self) -> usize {
            each!(self, p => p.presents_outstanding())
        }

        pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
            each!(self, p => p.set_present_wake(cb))
        }

        /// D3D11 imports nothing and submits in `Present`: no import or submit time.
        pub(crate) fn last_timings(&self) -> (u32, u32) {
            vk_or!(self, p => p.last_timings(), (0, 0))
        }

        pub(crate) fn last_waits(&self) -> (u32, u32, u32) {
            each!(self, p => p.last_waits())
        }

        pub(crate) fn present_mode_name(&self) -> &'static str {
            each!(self, p => p.present_mode_name())
        }

        pub(crate) fn needs_glass_gate(&self) -> bool {
            each!(self, p => p.needs_glass_gate())
        }

        pub(crate) fn vblank_locked(&self) -> bool {
            each!(self, p => p.vblank_locked())
        }

        pub(crate) fn take_presented_samples(&mut self) -> Vec<PresentedSample> {
            each!(self, p => p.take_presented_samples())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backend_is_vulkan_only_where_it_imports_d3d11() {
        let rows = [
            (
                "forced d3d11",
                Some("d3d11"),
                true,
                true,
                false,
                Backend::D3d,
            ),
            (
                "forced vulkan",
                Some("vulkan"),
                false,
                false,
                false,
                Backend::Vk,
            ),
            ("no loader", None, false, false, false, Backend::D3d),
            (
                "vulkan without import",
                None,
                true,
                false,
                false,
                Backend::D3d,
            ),
            ("vulkan with import", None, true, true, false, Backend::Vk),
            (
                "the console keeps vulkan",
                None,
                true,
                false,
                true,
                Backend::Vk,
            ),
            (
                "the console without a loader",
                None,
                false,
                false,
                true,
                Backend::D3d,
            ),
            (
                "an unknown value reads as unset",
                Some("dx12"),
                true,
                true,
                false,
                Backend::Vk,
            ),
        ];
        for (name, forced, has_vulkan, imports, console, want) in rows {
            assert_eq!(
                pick_backend(forced, has_vulkan, imports, console),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn the_adapter_pick_skips_software_and_matches_the_knob() {
        let names: Vec<(String, bool)> = [
            ("Microsoft Basic Render Driver", true),
            ("Intel(R) Arc(TM) Graphics", false),
            ("NVIDIA GeForce RTX 4080", false),
        ]
        .iter()
        .map(|(n, s)| (n.to_string(), *s))
        .collect();
        let rows = [
            ("no knob takes the first hardware adapter", None, Some(1)),
            ("the knob matches a substring", Some("rtx 4080"), Some(2)),
            ("the knob ignores case", Some("NVIDIA"), Some(2)),
            ("an unmatched knob falls back", Some("Radeon"), Some(1)),
            (
                "the knob never picks software",
                Some("basic render"),
                Some(1),
            ),
        ];
        for (name, wanted, want) in rows {
            assert_eq!(pick_adapter(&names, wanted), want, "{name}");
        }
        assert_eq!(
            pick_adapter(&names[..1], None),
            None,
            "a software-only box has none"
        );
    }
}
