//! The D3D11 presenter: one D3D11.4 device on the DXGI adapter the knobs name, a flip-model
//! swapchain on the SDL window's HWND, and the CSC pass that draws a frame placed by
//! `video_fit` with the bars cleared. `backend` picks it on Windows where Vulkan cannot
//! import D3D11 (planning `design/windows-d3d11-presenter.md`).
//!
//! Frames (`frames`): `FrameInput::Cpu` (three R8 planes), a D3D11VA pool slice sampled in
//! place, and `Redraw` (the last picture drawn again); every other input is an error, which
//! the run loop's health policy demotes.
//! The device is the decoder's too: [`Presenter::vulkan_decode`] hands it over in the facts
//! with `d3d11_present` set and every Vulkan handle zero.
//! Glass: `d3d_glass` stamps the presents `note_presented` claims from DXGI frame statistics,
//! and its waiter wakes the run loop on each frame-latency signal.
//! HDR: each frame's stream and the window's output pick the swapchain format (`hdr_model`):
//! PQ passes through an HDR10 swapchain, or the CSC tonemaps it onto 8-bit sRGB.
//! Not built here: the scale ladder (the CSC samples bilinear; `Kernel` is logged at debug)
//! and the console overlay (no Vulkan device).
//! Errors: a removed or reset device is `ash::vk::Result::ERROR_DEVICE_LOST` in the chain,
//! the one `run/` ends the session on; swapchain failures carry `PresentError::Window`.

mod csc;
mod frames;
mod swapchain;

use crate::d3d_glass;
use crate::hdr_model::{self, Format};
use crate::knobs::Knobs;
use crate::overlay::OverlayFrame;
use crate::vk::{BusyOn, FrameInput, PresentPref, Presented, PresentedSample};
use anyhow::{anyhow, bail, Context as _, Result};
use pf_client_core::video::{ColorDesc, D3d11Frame, SlotFormat, VulkanDecodeDevice};
use punktfunk_core::quic::HdrMeta;
use punktfunk_core::video_fit::{self, VideoFit};
use windows::core::Interface;
use windows::Win32::d3d11::{
    D3D11CreateDevice, ID3D11Device5, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_FENCE_FLAG_NONE,
    D3D11_SDK_VERSION,
};
use windows::Win32::d3dcommon::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIFactory2, IDXGIFactory6,
    DXGI_ADAPTER_DESC1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
};
use windows::Win32::winerror::{DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET};
use windows::Win32::winnt::HANDLE;

/// DXGI's vendor id for Qualcomm ("QCOM"). The facts carry Vulkan's PCI-style 0x5143
/// instead, the id every vendor rule in the decode ladder keys on.
const DXGI_VENDOR_QUALCOMM: u32 = 0x4D4F_4351;
const VENDOR_QUALCOMM: u32 = 0x5143;

/// The PQ→SDR peak when `knobs.tonemap_peak` is unset, as the Vulkan CSC: ≈ 1000 nits.
const TONEMAP_PEAK: f32 = 4.9;

/// A kernel handle closed on drop.
pub(crate) struct OwnedHandle(pub(crate) HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a handle this value owns; `Drop` runs once, so it is closed
            // exactly once and never used after.
            let _ = unsafe { windows::Win32::handleapi::CloseHandle(self.0) };
        }
    }
}

/// A failed DXGI or D3D11 call as `run/` classes it: a removed or reset device is the lost
/// device that ends the session, anything else the window's.
fn dxgi_error(e: windows::core::Error, what: &'static str) -> anyhow::Error {
    if removed(&e) {
        frame_error(e, what)
    } else {
        anyhow::Error::new(e)
            .context(crate::vk::PresentError::Window)
            .context(what)
    }
}

/// A failed D3D11 call on a frame: the lost device for a removed or reset one, else an
/// ordinary error the health policy counts.
fn frame_error(e: windows::core::Error, what: &'static str) -> anyhow::Error {
    if removed(&e) {
        anyhow::Error::new(ash::vk::Result::ERROR_DEVICE_LOST).context(format!("{what}: {e}"))
    } else {
        anyhow::Error::new(e).context(what)
    }
}

fn removed(e: &windows::core::Error) -> bool {
    e.code() == DXGI_ERROR_DEVICE_REMOVED || e.code() == DXGI_ERROR_DEVICE_RESET
}

pub(crate) struct Presenter {
    /// First, so it drops first: the waiter is joined while the chain and fence it reads
    /// are still held here, and their last release stays on this thread.
    glass: d3d_glass::Glass,
    device: ID3D11Device5,
    context: ID3D11DeviceContext4,
    swapchain: swapchain::Swapchain,
    csc: csc::Csc,
    /// The software lane's planes and the colour of what they hold; a `Redraw` draws them
    /// again. `shown` is `None` until the first CPU frame, while a slice shows, and after
    /// `drop_video`.
    planes: Option<frames::CpuPlanes>,
    shown: Option<ColorDesc>,
    /// The pool slice last drawn, which a `Redraw` draws again; `None` while the CPU planes
    /// are shown.
    slice: Option<frames::HeldSlice>,
    slice_views: crate::slice_model::ViewCache<frames::PlaneViews>,
    /// The last value signalled on a slice's `read` fence; it only goes up.
    read_value: u64,
    facts: VulkanDecodeDevice,
    /// The context's fence and its last value: `wait_idle` and each claimed present signal
    /// it. `idle_event` is `wait_idle`'s. `None` on a WDDM 1.x driver, which has no fences.
    idle_fence: Option<ID3D11Fence>,
    idle_value: std::cell::Cell<u64>,
    idle_event: OwnedHandle,
    pref: PresentPref,
    knobs: Knobs,
    video_fit: VideoFit,
    kernel_logged: bool,
    /// The host's mastering metadata, applied while the swapchain is HDR10.
    hdr_meta: Option<HdrMeta>,
    output: Output,
}

/// The window's output as the HDR rule reads it. `factory` goes stale when the OS changes an
/// output (HDR switched, a display plugged); a move to another monitor shows in `monitor`.
struct Output {
    monitor: Option<isize>,
    hdr: bool,
    /// The format this output refused; frames that pick it present as 8-bit sRGB instead.
    refused: Option<Format>,
    factory: IDXGIFactory1,
}

impl Output {
    fn read(window: &sdl3::video::Window) -> Result<Output> {
        // SAFETY: factory creation takes no pointer and returns an owned factory or an error.
        let factory = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
        let monitor = crate::win32::window_monitor(window);
        let hdr = pf_client_core::video_d3d11::display_hdr_volume(monitor).is_some();
        tracing::info!(hdr, "D3D11 presenter output");
        Ok(Output {
            monitor,
            hdr,
            refused: None,
            factory,
        })
    }

    fn stale(&self, window: &sdl3::video::Window) -> bool {
        // SAFETY: a COM call on the live factory; it takes no argument.
        let current = unsafe { self.factory.IsCurrent() }.as_bool();
        !current || crate::win32::window_monitor(window) != self.monitor
    }
}

impl Presenter {
    /// Device, swapchain, CSC pipeline and glass waiter over `window`'s HWND. `pref` and
    /// `knobs` choose the present interval and the adapter (`vk_adapter`, matched against
    /// DXGI's names).
    pub(crate) fn new(
        window: &sdl3::video::Window,
        pref: PresentPref,
        knobs: &Knobs,
    ) -> Result<Presenter> {
        let hwnd = crate::win32::window_hwnd(window).context("the SDL window has no HWND")?;
        let hwnd: windows::Win32::windef::HWND = hwnd.cast();
        let (factory, adapter, desc) = adapter_for(knobs)?;
        let (device, context) = create_device(&adapter)?;
        let factory2: IDXGIFactory2 = factory.cast().context("IDXGIFactory2")?;
        let swapchain =
            swapchain::Swapchain::new(&factory2, &device, hwnd, window.size_in_pixels())?;
        let csc = csc::Csc::new(&device)?;
        let output = Output::read(window)?;
        let mut idle_fence: Option<ID3D11Fence> = None;
        // SAFETY: COM call on the live device; the out-param is a local `Option`. The event
        // handle is owned by the `OwnedHandle` built from it.
        let idle_event = unsafe {
            if let Err(e) = device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut idle_fence) {
                tracing::info!(error = %e, "presenting without D3D11 fences");
            }
            OwnedHandle(windows::Win32::synchapi::CreateEventW(
                None,
                false,
                false,
                windows::core::PCWSTR::null(),
            ))
        };
        if idle_event.0.is_null() {
            bail!("create the idle event");
        }
        let glass = d3d_glass::Glass::spawn(
            swapchain.chain(),
            swapchain.waitable(),
            idle_fence.clone(),
            swapchain.slots(),
            swapchain.extent(),
        )?;
        let facts = decode_facts(&device, &desc);
        tracing::info!(
            adapter = %facts.device_name,
            vendor_id = format_args!("0x{:04X}", facts.vendor_id),
            "D3D11 presenter"
        );
        Ok(Presenter {
            device,
            context,
            swapchain,
            csc,
            planes: None,
            shown: None,
            slice: None,
            slice_views: Default::default(),
            read_value: 0,
            facts,
            idle_fence,
            idle_value: std::cell::Cell::new(0),
            idle_event,
            pref,
            knobs: knobs.clone(),
            video_fit: VideoFit::default(),
            kernel_logged: false,
            hdr_meta: None,
            output,
            glass,
        })
    }

    /// Present one frame. `Busy` hands the frame back unconsumed while the frame-latency
    /// object is unsignalled, so the run loop retries instead of blocking in `Present`. A
    /// new frame first sets the swapchain format its stream takes; a `Redraw` keeps it. A
    /// pool slice is drawn after a GPU wait on its `written` fence and before a signal of its
    /// `read` fence; it stays held for `Redraw` until the next frame.
    pub(crate) fn present<'a>(
        &mut self,
        window: &sdl3::video::Window,
        input: FrameInput<'a>,
        _overlay: Option<&OverlayFrame>,
    ) -> Result<Presented<'a>> {
        // (pq, ten_bit); the CPU lane is 8-bit by contract.
        let stream = match &input {
            FrameInput::Redraw => None,
            FrameInput::Cpu(f) => Some((f.color.is_pq(), false)),
            FrameInput::D3d11(D3d11Frame::Slice(s)) => {
                Some((s.color.is_pq(), s.format == SlotFormat::P010))
            }
            FrameInput::D3d11(D3d11Frame::Shared(_)) => {
                bail!("D3D11 ring slots need the Vulkan presenter")
            }
            _ => bail!("the D3D11 presenter takes no Vulkan or dma-buf frames"),
        };
        let extent = window.size_in_pixels();
        if extent.0 == 0 || extent.1 == 0 {
            return Ok(Presented::Shown); // minimized: nothing would show
        }
        if extent != self.swapchain.extent() {
            self.swapchain.resize(&self.context, extent)?;
        }
        if let Some((pq, ten_bit)) = stream {
            self.follow_format(window, pq, ten_bit)?;
        }
        if !self.swapchain.ready() {
            return Ok(Presented::Busy(input, BusyOn::Acquire));
        }
        match input {
            FrameInput::Cpu(f) => {
                frames::CpuPlanes::upload(&mut self.planes, &self.device, &self.context, f)?;
                self.shown = Some(f.color);
                self.slice = None;
            }
            FrameInput::D3d11(D3d11Frame::Slice(s)) => {
                let held =
                    frames::HeldSlice::take(&self.device, &self.context, &mut self.slice_views, s)?;
                self.slice = Some(held);
                self.shown = None;
            }
            _ => {}
        }
        let target = self.swapchain.target(&self.device)?;
        // SAFETY: COM calls on the live immediate context with the back buffer's live view.
        unsafe {
            self.context
                .OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            self.context
                .ClearRenderTargetView(&target, [0.0, 0.0, 0.0, 1.0].as_ptr());
        }
        self.draw_picture(extent)?;
        self.swapchain.present(self.mode())?;
        Ok(Presented::Shown)
    }

    /// Draw what is shown into the bound target: the held slice, then signal its `read`
    /// fence; else the CPU planes. PQ passes through on HDR10 and is tonemapped otherwise.
    fn draw_picture(&mut self, extent: (u32, u32)) -> Result<()> {
        let peak = self.knobs.tonemap_peak.unwrap_or(TONEMAP_PEAK);
        let format = self.swapchain.format();
        let tonemap = |color: ColorDesc| format.tonemaps(color.is_pq()).then_some(peak);
        if let Some(held) = &mut self.slice {
            let place = video_fit::place(self.video_fit, extent, held.size);
            if !place.is_empty() {
                note_kernel(&mut self.kernel_logged, &place);
                let tonemap = tonemap(held.color);
                let constants = csc::Constants::new(held.rows(), tonemap, held.uv_scale);
                self.csc
                    .draw_nv12(&self.context, &held.views, held.size, &place, &constants);
            }
            self.read_value += 1;
            held.signal(&self.context, self.read_value)?;
        } else if let (Some(planes), Some(color)) = (&self.planes, self.shown) {
            let place = video_fit::place(self.video_fit, extent, planes.size());
            if !place.is_empty() {
                note_kernel(&mut self.kernel_logged, &place);
                self.csc.draw_planar(
                    &self.context,
                    &planes.views,
                    planes.size(),
                    &place,
                    color,
                    tonemap(color),
                );
            }
        }
        Ok(())
    }

    /// Set the format a frame of this stream takes, re-reading the window's output first when
    /// it moved or the OS changed it. A refusal is logged and kept until the next read.
    fn follow_format(
        &mut self,
        window: &sdl3::video::Window,
        pq: bool,
        ten_bit: bool,
    ) -> Result<()> {
        if self.output.stale(window) {
            self.output = Output::read(window)?;
        }
        let want = self.format_for(pq, ten_bit);
        if want == self.swapchain.format() {
            return Ok(());
        }
        let meta = self.hdr_meta;
        if !self
            .swapchain
            .set_format(&self.context, want, meta.as_ref())?
        {
            tracing::warn!(
                format = ?want,
                "the output refuses this colour space; presenting 8-bit sRGB"
            );
            self.output.refused = Some(want);
        }
        Ok(())
    }

    /// The format a frame of this stream takes on the window's output now: HDR10 where the
    /// output is in HDR mode and `knobs.hdr10` allows it. A refused format is 8-bit sRGB.
    fn format_for(&self, pq: bool, ten_bit: bool) -> Format {
        let hdr10 = self.output.hdr && self.knobs.hdr10;
        match hdr_model::pick_format(pq, ten_bit, hdr10, self.knobs.sdr_8bit) {
            f if self.output.refused == Some(f) => Format::Sdr8,
            f => f,
        }
    }

    fn mode(&self) -> swapchain::Mode {
        swapchain::mode(
            self.pref,
            self.knobs.present_mode.as_deref(),
            self.swapchain.tearing(),
        )
    }

    /// The window may have a new size: resize the buffers to it.
    pub(crate) fn recreate_swapchain(&mut self, window: &sdl3::video::Window) -> Result<()> {
        let extent = window.size_in_pixels();
        if extent.0 == 0 || extent.1 == 0 || extent == self.swapchain.extent() {
            return Ok(());
        }
        self.swapchain.resize(&self.context, extent)
    }

    /// New V-Sync, VRR and fullscreen state; the next present takes its interval from it.
    pub(crate) fn set_present_pref(&mut self, vsync: bool, allow_vrr: bool, fullscreen: bool) {
        self.pref = PresentPref {
            vsync,
            allow_vrr,
            fullscreen,
            ..self.pref
        };
    }

    pub(crate) fn set_video_fit(&mut self, fit: VideoFit) {
        self.video_fit = fit;
        self.kernel_logged = false;
    }

    /// The device facts the decoder builds on (see the module map), with `d3d11_hdr10` as the
    /// window's output stands now.
    pub(crate) fn vulkan_decode(&self) -> Option<VulkanDecodeDevice> {
        Some(VulkanDecodeDevice {
            d3d11_hdr10: self.format_for(true, true) == Format::Hdr10,
            ..self.facts.clone()
        })
    }

    /// Flush the context and wait for its work. Teardown only; a removed device never
    /// completes the fence, so the wait gives up after a second. Without a fence the flush
    /// is all: D3D11 keeps a released resource alive until the GPU is done with it.
    pub(crate) fn wait_idle(&self) {
        let Some(fence) = &self.idle_fence else {
            // SAFETY: a COM call on the live context.
            unsafe { self.context.Flush() };
            return;
        };
        let value = self.idle_value.get() + 1;
        self.idle_value.set(value);
        // SAFETY: COM calls on the live context and fence; the event is a live handle this
        // presenter owns, waited with a bounded timeout.
        unsafe {
            if self.context.Signal(fence, value).is_err() {
                return;
            }
            self.context.Flush();
            if fence.SetEventOnCompletion(value, self.idle_event.0).is_ok() {
                windows::Win32::synchapi::WaitForSingleObject(self.idle_event.0, 1000);
            }
        }
    }

    /// Let go of the stream's picture: the planes go, the held slice goes back to the
    /// decoder with the pool's views, and a `Redraw` clears to black.
    pub(crate) fn drop_video(&mut self) {
        self.planes = None;
        self.shown = None;
        self.slice = None;
        self.slice_views.clear();
    }

    /// The host's mastering metadata: applied now while HDR10 is live, else with the flip.
    pub(crate) fn set_hdr_metadata(&mut self, meta: HdrMeta) {
        if self.hdr_meta.replace(meta) != Some(meta) && self.hdr_active() {
            self.swapchain.set_metadata(Some(&meta));
        }
    }

    /// The swapchain is HDR10: PQ reaches the display untonemapped.
    pub(crate) fn hdr_active(&self) -> bool {
        self.swapchain.format() == Format::Hdr10
    }

    /// The swapchain holds 10 bits a channel, SDR or HDR10.
    pub(crate) fn ten_bit(&self) -> bool {
        self.swapchain.format() != Format::Sdr8
    }

    /// Back to 8-bit sRGB while HDR10 is live, for the console's SDR presents; a PQ picture
    /// drawn again is tonemapped, and the next PQ frame takes HDR10 again.
    pub(crate) fn leave_hdr(&mut self, _window: &sdl3::video::Window) -> Result<()> {
        if !self.hdr_active() {
            return Ok(());
        }
        tracing::info!("stream over — leaving HDR10 so the console composites as SDR");
        self.swapchain
            .set_format(&self.context, Format::Sdr8, None)
            .map(drop)
    }

    /// Run-loop wake (SDL event push) for each frame-latency signal and settled present.
    pub(crate) fn set_present_wake(&self, cb: Box<dyn Fn() + Send>) {
        self.glass.set_wake(cb);
    }

    /// Claim the present just made for the glass clock, with its frame's capture and decode
    /// stamps. The context's fence is signalled behind it, so the waiter stamps our GPU work
    /// done; no fence or a failed signal leaves the present unwaited.
    pub(crate) fn note_presented(&mut self, pts_ns: u64, decoded_ns: u64) {
        let Some(sent) = self.swapchain.take_sent() else {
            return;
        };
        let value = self.idle_value.get() + 1;
        self.idle_value.set(value);
        let fence = self.idle_fence.as_ref();
        // SAFETY: COM calls on the live immediate context and fence, on the context's thread.
        let signalled = unsafe {
            let ok = fence.is_some_and(|f| self.context.Signal(f, value).is_ok());
            self.context.Flush();
            ok
        };
        let p = d3d_glass::Pending {
            count: sent.count,
            pts_ns,
            decoded_ns,
            submitted_ns: sent.submitted_ns,
            gpu_value: if signalled { value } else { 0 },
            gpu_done_ns: 0,
            tearing: sent.tearing,
        };
        self.glass.claim(p, self.swapchain.extent());
        if let Some(fence) = fence.filter(|_| signalled) {
            // SAFETY: a COM call on the live fence with the waiter's open event. A failed arm
            // leaves the stamp to the waiter's next read.
            let _ = unsafe { fence.SetEventOnCompletion(value, self.glass.gpu_event()) };
        }
    }

    /// True while DXGI statistics arrive, and under tearing, where the GPU-done stamp stands in.
    pub(crate) fn present_timing_active(&self) -> bool {
        self.glass.stats_arrive() || self.glass_estimated()
    }

    /// Under tearing no vblank takes a present: its stamp is our GPU work done, an estimate.
    pub(crate) fn glass_estimated(&self) -> bool {
        self.mode().tears()
    }

    /// The vblank spacing the statistics show, a median once measured.
    pub(crate) fn measured_refresh_ns(&self) -> Option<u64> {
        self.glass.refresh_ns()
    }

    /// Where the display stamp comes from, for the ledger.
    pub(crate) fn glass_source(&self) -> &'static str {
        if self.glass_estimated() {
            "est"
        } else if self.glass.stats_arrive() {
            "dxgi-stats"
        } else {
            "none"
        }
    }

    /// Claimed presents a newer one replaced before a refresh took them, since the last call.
    pub(crate) fn take_unshown(&self) -> u32 {
        self.glass.take_unshown()
    }

    /// Claimed presents the statistics have not settled: the FIFO glass gate's count.
    pub(crate) fn presents_outstanding(&self) -> usize {
        self.glass.outstanding()
    }

    /// `(fence_us, acquire_us, present_us)`: the waitable check never blocks and no fence is
    /// waited, so only the `Present` call takes time.
    pub(crate) fn last_waits(&self) -> (u32, u32, u32) {
        (0, 0, self.swapchain.present_us())
    }

    /// Interval 1 queues presents on the vblank grid, which the glass gate governs; interval
    /// 0 replaces or tears.
    pub(crate) fn needs_glass_gate(&self) -> bool {
        self.vblank_locked()
    }

    /// Presents land on the vblank grid: interval 1.
    pub(crate) fn vblank_locked(&self) -> bool {
        self.mode().interval == 1
    }

    pub(crate) fn present_mode_name(&self) -> &'static str {
        self.mode().name
    }

    /// The glass clock's samples, in present order.
    pub(crate) fn take_presented_samples(&mut self) -> Vec<PresentedSample> {
        self.glass.take_samples()
    }
}

/// Log once per fit that this backend samples bilinear, whatever the placement's kernel.
fn note_kernel(logged: &mut bool, place: &video_fit::Placement) {
    if !std::mem::replace(logged, true) {
        tracing::debug!(
            kernel = ?(place.kernel_x().name(), place.kernel_y().name()),
            "D3D11 presenter scales bilinear, not by the kernel"
        );
    }
}

/// DXGI's factory and the adapter the knobs name (`vk_adapter`), as
/// [`crate::backend::pick_adapter`] ranks the factory's list.
fn adapter_for(knobs: &Knobs) -> Result<(IDXGIFactory1, IDXGIAdapter1, DXGI_ADAPTER_DESC1)> {
    // SAFETY: factory creation takes no pointer and returns an owned factory or an error.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1")?;
    let mut adapters = adapters(&factory);
    let names: Vec<(String, bool)> = adapters
        .iter()
        .map(|(_, d)| {
            (
                adapter_name(d),
                d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE as u32 != 0,
            )
        })
        .collect();
    let pick = crate::backend::pick_adapter(&names, knobs.vk_adapter.as_deref())
        .ok_or_else(|| anyhow!("no hardware DXGI adapter"))?;
    let (adapter, desc) = adapters.swap_remove(pick);
    Ok((factory, adapter, desc))
}

/// The name of the adapter a presenter built now would use (`--probe-decode`).
pub(crate) fn adapter_name_for(knobs: &Knobs) -> Result<String> {
    adapter_for(knobs).map(|(_, _, desc)| adapter_name(&desc))
}

/// Every adapter DXGI lists, high-performance first where the factory can order them.
fn adapters(factory: &IDXGIFactory1) -> Vec<(IDXGIAdapter1, DXGI_ADAPTER_DESC1)> {
    let by_preference = factory.cast::<IDXGIFactory6>().ok();
    (0..)
        .map_while(|i| {
            // SAFETY: COM calls on the live factory; `ok()` keeps only an adapter that came back.
            unsafe {
                match &by_preference {
                    Some(f) => f
                        .EnumAdapterByGpuPreference::<IDXGIAdapter1>(
                            i,
                            DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE,
                        )
                        .ok(),
                    None => factory.EnumAdapters1(i).ok(),
                }
            }
        })
        .filter_map(|adapter| {
            let mut desc = DXGI_ADAPTER_DESC1::default();
            // SAFETY: a COM call on the adapter just enumerated, filling the local descriptor.
            let described = unsafe { adapter.GetDesc1(&mut desc) }.is_ok();
            described.then_some((adapter, desc))
        })
        .collect()
}

fn adapter_name(desc: &DXGI_ADAPTER_DESC1) -> String {
    let len = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    String::from_utf16_lossy(&desc.Description[..len])
}

/// A D3D11.4 device and immediate context on `adapter`, decode-capable, with multithread
/// protection on: the decoder calls into this device from the pump thread.
fn create_device(adapter: &IDXGIAdapter1) -> Result<(ID3D11Device5, ID3D11DeviceContext4)> {
    let (mut device, mut context) = (None, None);
    // SAFETY: `adapter` is live; the two out-params are local `Option`s the callee only writes,
    // both checked before use.
    unsafe {
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            std::ptr::null_mut(),
            (D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT) as u32,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION as u32,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .ok()
    .context("D3D11CreateDevice")?;
    let device = device.context("D3D11CreateDevice returned no device")?;
    let context = context.context("D3D11CreateDevice returned no context")?;
    let device: ID3D11Device5 = device.cast().context("D3D11.4 device")?;
    let context: ID3D11DeviceContext4 = context.cast().context("D3D11.4 device")?;
    let mt: ID3D11Multithread = context.cast().context("ID3D11Multithread")?;
    // SAFETY: a COM call on the live interface from a checked cast; the previous state it
    // returns is not needed.
    let _ = unsafe { mt.SetMultithreadProtected(true) };
    Ok((device, context))
}

/// The facts for the decoder: this device, the adapter's vendor, name and LUID, and no
/// Vulkan device (every handle zero, no extensions, no Vulkan decode lanes).
fn decode_facts(device: &ID3D11Device5, desc: &DXGI_ADAPTER_DESC1) -> VulkanDecodeDevice {
    let mut luid = [0u8; 8];
    luid[..4].copy_from_slice(&desc.AdapterLuid.LowPart.to_le_bytes());
    luid[4..].copy_from_slice(&desc.AdapterLuid.HighPart.to_le_bytes());
    let vendor_id = match desc.VendorId {
        DXGI_VENDOR_QUALCOMM => VENDOR_QUALCOMM,
        v => v,
    };
    VulkanDecodeDevice {
        get_instance_proc_addr: 0,
        instance: 0,
        physical_device: 0,
        device: 0,
        vendor_id,
        device_name: adapter_name(desc),
        graphics_qf: 0,
        decode_qf: 0,
        decode_video_caps: 0,
        instance_extensions: Vec::new(),
        device_extensions: Vec::new(),
        f_sampler_ycbcr: false,
        f_timeline_semaphore: false,
        f_synchronization2: false,
        f_shader_int16: false,
        f_storage_buffer8: false,
        f_storage_buffer16: false,
        f_subgroup_size_control: false,
        f_compute_full_subgroups: false,
        f_shader_float16: false,
        api_version: 0,
        queue_families: Vec::new(),
        pyrowave_decode: false,
        video_decode: false,
        d3d11_present: true,
        d3d11_fence: false,
        d3d11_device: Some(pf_client_core::video::video_vk::D3d11DeviceHandle::new(
            device.clone().into(),
        )),
        d3d11_import: false,
        dmabuf_import: false,
        vaapi_av1_decode: false,
        vaapi_hevc_decode: false,
        d3d11_hdr10: false,
        d3d11_nv12: false,
        d3d11_p010: false,
        adapter_luid: Some(luid),
        queue_lock: std::sync::Arc::new(pf_client_core::video::QueueLock::new()),
    }
}
