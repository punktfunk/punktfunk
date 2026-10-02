//! Native Wayland lane: a decoded picture's dma-buf becomes the window's own buffer.
//!
//! The Vulkan presenter draws every picture through a colour-conversion pass into a
//! swapchain the compositor then composites. Here a dma-buf goes straight on SDL's
//! `wl_surface` through `zwp_linux_dmabuf_v1`, so the compositor can put it on a plane, and
//! `wp_presentation` stamps the glass for the HUD. The buffer is a VAAPI surface as decoded,
//! or a copy of a Vulkan Video or PyroWave picture (`vk::export_ring`). The lane takes a
//! picture only when the surface feedback lists its format and modifier, the compositor
//! takes its colour, and it fills the window; anything else is declined and the Vulkan path
//! draws that frame. The presenter
//! suspends its swapchain while the lane owns the window: Mesa's explicit-sync object on the
//! surface would make a plain dma-buf commit a fatal protocol error.
//!
//! Each buffer is imported once under a key and reused; the caller's hold is kept until the
//! compositor releases the buffer. The overlay rides on its own subsurface above the picture,
//! with an empty input region. Opt-in (`PUNKTFUNK_NATIVE_SCANOUT=1`) until measured on a display.
//!
//! SDL owns the socket. A private queue takes this lane's events; SDL's pump reads them in
//! and [`NativeLane::pump`] dispatches them. Presentation times arrive on CLOCK_MONOTONIC
//! and are moved onto the session's realtime clock as they land.

use anyhow::{Context as _, Result};
use pf_client_core::video::{ColorDesc, DmabufFrame};
use punktfunk_core::quic::HdrMeta;
use punktfunk_core::video_fit::VideoFit;
use sdl3::video::WindowContext;
use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::os::fd::BorrowedFd;
use std::sync::Arc;
use wayland_backend::client::{Backend, ObjectId};
use wayland_client::globals::{registry_queue_init, GlobalList, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_registry, wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::color_management::v1::client::{
    wp_color_management_surface_v1 as cms, wp_color_manager_v1 as cmm,
    wp_image_description_creator_params_v1 as cmp, wp_image_description_v1 as cmd,
};
use wayland_protocols::wp::color_representation::v1::client::{
    wp_color_representation_manager_v1 as crm, wp_color_representation_surface_v1 as crs,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1 as params, zwp_linux_dmabuf_feedback_v1 as feedback,
    zwp_linux_dmabuf_v1 as dmabuf,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1 as sym, wp_linux_drm_syncobj_surface_v1 as sys,
    wp_linux_drm_syncobj_timeline_v1 as syt,
};
use wayland_protocols::wp::presentation_time::client::{
    wp_presentation, wp_presentation_feedback as pfb,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};

const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
const CLOCK_MONOTONIC: u32 = 1;
/// `wp_presentation_feedback.kind` bit: the buffer reached the screen without a copy.
const KIND_ZERO_COPY: u32 = 8;
/// linux-dmabuf tranche flag: this tranche's buffers can go straight to a plane.
const TRANCHE_SCANOUT: u32 = 1;

/// `PUNKTFUNK_NATIVE_SCANOUT=1` arms the lane.
pub fn enabled() -> bool {
    pf_client_core::video::native_scanout_wanted()
}

/// `PUNKTFUNK_NATIVE_SCANOUT=flip`: a bring-up diagnostic that hands the window between the
/// lane and the swapchain every [`FLIP_PERIOD`], so both transitions run on any compositor.
pub fn flip_mode() -> bool {
    matches!(
        std::env::var("PUNKTFUNK_NATIVE_SCANOUT").as_deref(),
        Ok("flip")
    )
}

pub const FLIP_PERIOD: std::time::Duration = std::time::Duration::from_secs(3);

/// What the lane did with a VAAPI frame.
pub enum Outcome {
    Shown,
    /// The lane owns the window but this frame's buffer is busy: the frame is skipped.
    Dropped,
    /// Not this lane's frame (or not yet): the caller draws it through Vulkan.
    Declined(DmabufFrame),
}

/// Where a keyed buffer stands with the compositor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotState {
    /// Never offered.
    Unknown,
    Pending,
    /// Imported and not on screen: a commit may use it.
    Free,
    /// The compositor may still read it.
    Held,
    Failed,
}

/// One presented frame's stamps, client realtime clock.
pub struct NativeSample {
    pub pts_ns: u64,
    pub decoded_ns: u64,
    pub submitted_ns: u64,
    pub displayed_ns: u64,
    pub zero_copy: bool,
}

struct Slot {
    buffer: wl_buffer::WlBuffer,
    /// The caller's hold (a decoder slot, a ring slot), kept while the compositor may read.
    held: Option<Box<dyn Any>>,
}

enum Import {
    Pending(params::ZwpLinuxBufferParamsV1),
    Ready(Slot),
    Failed,
}

struct Job {
    pts_ns: u64,
    decoded_ns: u64,
    submitted_ns: u64,
    /// Committed with no overlay above: the frame counts toward the scanout trial.
    judged: bool,
}

/// Picture frames on glass, with no overlay above, before the lane judges the compositor:
/// two seconds at 60 Hz.
const TRIAL_FRAMES: u32 = 120;
/// A frame neither presented nor discarded this long means the compositor is stuck on the
/// lane's buffers: the picture has frozen.
const STALL_NS: u64 = 1_000_000_000;

#[derive(Default)]
struct LaneState {
    clock_id: Option<u32>,
    table: Vec<(u32, u64)>,
    /// Every (fourcc, modifier) pair the surface's tranches list.
    pairs: Vec<(u32, u64)>,
    /// The pairs a scanout tranche lists.
    scanout: Vec<(u32, u64)>,
    tranche_flags: u32,
    feedback_done: bool,
    /// Bumped on every complete feedback: a caller that chose a modifier re-chooses.
    feedback_gen: u64,
    /// Buffer params in flight, by protocol id, to the key they import.
    pending: HashMap<u32, u64>,
    imports: HashMap<u64, Import>,
    by_buffer: HashMap<ObjectId, u64>,
    jobs: HashMap<usize, Job>,
    samples: Vec<NativeSample>,
    zero_copy: u32,
    presented: u32,
    /// Judged frames on glass, and how many of those went out zero-copy.
    trial_presented: u32,
    trial_zero_copy: u32,
    /// Keys committed with explicit sync: their `wl_buffer.release` is not the release.
    explicit_keys: HashSet<u64>,
    /// `wp_color_manager_v1` advertisements, as raw enum values.
    cm_features: Vec<u32>,
    cm_tfs: Vec<u32>,
    cm_primaries: Vec<u32>,
    cm_intents: Vec<u32>,
    /// The pending image description answered: `Some(true)` ready, `Some(false)` failed.
    desc_answer: Option<bool>,
    /// (coefficients, range) pairs the colour-representation manager advertised; any other
    /// pair is a fatal protocol error.
    repr_pairs: Vec<(u32, u32)>,
}

// wp_color_manager_v1 enum values the lane asks for.
const CM_FEATURE_PARAMETRIC: u32 = 1;
const CM_FEATURE_MASTERING: u32 = 5;
const CM_TF_ST2084_PQ: u32 = 11;
const CM_PRIMARIES_BT2020: u32 = 6;
const CM_INTENT_PERCEPTUAL: u32 = 0;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for LaneState {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wp_presentation::WpPresentation, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.clock_id = Some(clk_id);
        }
    }
}

impl Dispatch<pfb::WpPresentationFeedback, usize> for LaneState {
    fn event(
        state: &mut Self,
        _: &pfb::WpPresentationFeedback,
        event: pfb::Event,
        seq: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            pfb::Event::Presented {
                tv_sec_hi,
                tv_sec_lo,
                tv_nsec,
                flags,
                ..
            } => {
                let Some(job) = state.jobs.remove(seq) else {
                    return;
                };
                let sec = (u64::from(tv_sec_hi) << 32) | u64::from(tv_sec_lo);
                let mono = sec * 1_000_000_000 + u64::from(tv_nsec);
                let kind = match flags {
                    WEnum::Value(k) => k.bits(),
                    WEnum::Unknown(v) => v,
                };
                let zero_copy = kind & KIND_ZERO_COPY != 0;
                state.presented += 1;
                state.zero_copy += u32::from(zero_copy);
                if job.judged {
                    state.trial_presented += 1;
                    state.trial_zero_copy += u32::from(zero_copy);
                }
                state.samples.push(NativeSample {
                    pts_ns: job.pts_ns,
                    decoded_ns: job.decoded_ns,
                    submitted_ns: job.submitted_ns,
                    displayed_ns: monotonic_to_realtime(mono),
                    zero_copy,
                });
            }
            pfb::Event::Discarded => {
                state.jobs.remove(seq);
            }
            _ => {}
        }
    }
}

impl Dispatch<feedback::ZwpLinuxDmabufFeedbackV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &feedback::ZwpLinuxDmabufFeedbackV1,
        event: feedback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            feedback::Event::FormatTable { fd, size } => {
                use std::os::unix::fs::FileExt as _;
                let file = std::fs::File::from(fd);
                let mut bytes = vec![0u8; size as usize];
                if file.read_exact_at(&mut bytes, 0).is_ok() {
                    state.table = bytes
                        .chunks_exact(16)
                        .map(|c| {
                            let f = u32::from_ne_bytes([c[0], c[1], c[2], c[3]]);
                            let m = u64::from_ne_bytes([
                                c[8], c[9], c[10], c[11], c[12], c[13], c[14], c[15],
                            ]);
                            (f, m)
                        })
                        .collect();
                }
                // A new table starts a new answer.
                state.pairs.clear();
                state.scanout.clear();
                state.feedback_done = false;
            }
            feedback::Event::TrancheFlags { flags } => {
                state.tranche_flags = match flags {
                    WEnum::Value(f) => f.bits(),
                    WEnum::Unknown(v) => v,
                };
            }
            feedback::Event::TrancheFormats { indices } => {
                for c in indices.chunks_exact(2) {
                    let i = u16::from_ne_bytes([c[0], c[1]]) as usize;
                    if let Some(&pair) = state.table.get(i) {
                        if !state.pairs.contains(&pair) {
                            state.pairs.push(pair);
                        }
                        if state.tranche_flags & TRANCHE_SCANOUT != 0
                            && !state.scanout.contains(&pair)
                        {
                            state.scanout.push(pair);
                        }
                    }
                }
            }
            feedback::Event::TrancheDone => state.tranche_flags = 0,
            feedback::Event::Done => {
                state.feedback_done = true;
                state.feedback_gen += 1;
            }
            _ => {}
        }
    }
}

impl Dispatch<params::ZwpLinuxBufferParamsV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        prm: &params::ZwpLinuxBufferParamsV1,
        event: params::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(key) = state.pending.remove(&prm.id().protocol_id()) else {
            return;
        };
        match event {
            params::Event::Created { buffer } => {
                state.by_buffer.insert(buffer.id(), key);
                state
                    .imports
                    .insert(key, Import::Ready(Slot { buffer, held: None }));
            }
            params::Event::Failed => {
                state.imports.insert(key, Import::Failed);
            }
            _ => {}
        }
        prm.destroy();
    }

    wayland_client::event_created_child!(LaneState, params::ZwpLinuxBufferParamsV1, [
        params::EVT_CREATED_OPCODE => (wl_buffer::WlBuffer, ()),
    ]);
}

impl Dispatch<wl_buffer::WlBuffer, ()> for LaneState {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(key) = state.by_buffer.get(&buffer.id()) {
                // Under explicit sync the release point says when, not this event.
                if state.explicit_keys.contains(key) {
                    return;
                }
                if let Some(Import::Ready(slot)) = state.imports.get_mut(key) {
                    slot.held = None;
                }
            }
        }
    }
}

/// A protocol enum as its wire value, known or not.
fn raw<T: Into<u32>>(v: WEnum<T>) -> u32 {
    match v {
        WEnum::Value(x) => x.into(),
        WEnum::Unknown(u) => u,
    }
}

impl Dispatch<cmm::WpColorManagerV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &cmm::WpColorManagerV1,
        event: cmm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            cmm::Event::SupportedFeature { feature } => state.cm_features.push(raw(feature)),
            cmm::Event::SupportedTfNamed { tf } => state.cm_tfs.push(raw(tf)),
            cmm::Event::SupportedPrimariesNamed { primaries } => {
                state.cm_primaries.push(raw(primaries));
            }
            cmm::Event::SupportedIntent { render_intent } => {
                state.cm_intents.push(raw(render_intent));
            }
            _ => {}
        }
    }
}

impl Dispatch<cmd::WpImageDescriptionV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &cmd::WpImageDescriptionV1,
        event: cmd::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            cmd::Event::Ready { .. } | cmd::Event::Ready2 { .. } => state.desc_answer = Some(true),
            cmd::Event::Failed { msg, .. } => {
                tracing::info!(
                    reason = msg,
                    "native scanout: the compositor refused the HDR description"
                );
                state.desc_answer = Some(false);
            }
            _ => {}
        }
    }
}

delegate_noop!(LaneState: ignore dmabuf::ZwpLinuxDmabufV1);
impl Dispatch<crm::WpColorRepresentationManagerV1, ()> for LaneState {
    fn event(
        state: &mut Self,
        _: &crm::WpColorRepresentationManagerV1,
        event: crm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let crm::Event::SupportedCoefficientsAndRanges {
            coefficients,
            range,
        } = event
        {
            state.repr_pairs.push((raw(coefficients), raw(range)));
        }
    }
}

/// The colour-representation (coefficients, range) for a frame's H.273 matrix and range.
fn representation(color: ColorDesc) -> (crs::Coefficients, crs::Range) {
    let coefficients = match color.matrix {
        5 | 6 => crs::Coefficients::Bt601,
        9 | 10 => crs::Coefficients::Bt2020,
        _ => crs::Coefficients::Bt709,
    };
    let range = if color.full_range {
        crs::Range::Full
    } else {
        crs::Range::Limited
    };
    (coefficients, range)
}

/// ST.2086 and CTA-861 metadata in colour-management units.
#[derive(Debug, PartialEq, Eq)]
struct HdrStatic {
    /// Red, green, blue, white (x, y) × 1M.
    primaries: [i32; 8],
    /// (min × 10000, max) cd/m², when max is above min.
    luminance: Option<(u32, u32)>,
    max_cll: Option<u32>,
    max_fall: Option<u32>,
}

/// `m` in the protocol's units. Version 1 also wants MaxCLL and MaxFALL inside the mastering
/// range; version 2 dropped that rule. MaxFALL above MaxCLL is left out either way.
fn hdr_static(m: &HdrMeta, version: u32) -> HdrStatic {
    // 1/50000 → 1/1000000.
    let c = |v: u16| i32::from(v) * 20;
    let [g, b, r] = m.display_primaries;
    let max = m.max_display_mastering_luminance / 10_000;
    let min = m.min_display_mastering_luminance;
    let luminance = (u64::from(max) * 10_000 > u64::from(min)).then_some((min, max));
    let in_range = |v: u16| {
        version >= 2
            || luminance
                .is_some_and(|(lo, hi)| u64::from(v) * 10_000 > u64::from(lo) && u32::from(v) <= hi)
    };
    let max_cll = (m.max_cll > 0 && in_range(m.max_cll)).then_some(u32::from(m.max_cll));
    let max_fall = (m.max_fall > 0 && in_range(m.max_fall))
        .then_some(u32::from(m.max_fall))
        .filter(|f| max_cll.is_none_or(|cll| *f <= cll));
    HdrStatic {
        primaries: [
            c(r[0]),
            c(r[1]),
            c(g[0]),
            c(g[1]),
            c(b[0]),
            c(b[1]),
            c(m.white_point[0]),
            c(m.white_point[1]),
        ],
        luminance,
        max_cll,
        max_fall,
    }
}
delegate_noop!(LaneState: ignore crs::WpColorRepresentationSurfaceV1);
delegate_noop!(LaneState: ignore wl_compositor::WlCompositor);
delegate_noop!(LaneState: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(LaneState: ignore wl_subsurface::WlSubsurface);
delegate_noop!(LaneState: ignore wl_region::WlRegion);
delegate_noop!(LaneState: ignore wl_surface::WlSurface);
delegate_noop!(LaneState: ignore wp_viewporter::WpViewporter);
delegate_noop!(LaneState: ignore wp_viewport::WpViewport);
delegate_noop!(LaneState: ignore cms::WpColorManagementSurfaceV1);
delegate_noop!(LaneState: ignore cmp::WpImageDescriptionCreatorParamsV1);
delegate_noop!(LaneState: ignore sym::WpLinuxDrmSyncobjManagerV1);
delegate_noop!(LaneState: ignore sys::WpLinuxDrmSyncobjSurfaceV1);
delegate_noop!(LaneState: ignore syt::WpLinuxDrmSyncobjTimelineV1);

/// The overlay's own surface above the picture: input passes through to SDL's surface.
struct Hud {
    surface: wl_surface::WlSurface,
    sub: wl_subsurface::WlSubsurface,
    viewport: wp_viewport::WpViewport,
    /// Explicit sync on the overlay's own surface; no Vulkan swapchain ever shares it.
    sync: Option<sys::WpLinuxDrmSyncobjSurfaceV1>,
    mapped: bool,
}

/// The HDR image description on SDL's surface, and the mastering metadata it was built from.
struct HdrDesc {
    meta: Option<HdrMeta>,
    desc: cmd::WpImageDescriptionV1,
}

/// What the overlay's surface needs from the compositor.
struct HudGlobals {
    compositor: wl_compositor::WlCompositor,
    subcompositor: wl_subcompositor::WlSubcompositor,
    viewporter: wp_viewporter::WpViewporter,
}

fn monotonic_to_realtime(mono_ns: u64) -> u64 {
    let now_mono = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let now_mono = now_mono.tv_sec as u64 * 1_000_000_000 + now_mono.tv_nsec as u64;
    let now_real = pf_client_core::session::now_ns();
    now_real.wrapping_add(mono_ns).wrapping_sub(now_mono)
}

pub struct NativeLane {
    conn: Connection,
    queue: EventQueue<LaneState>,
    qh: QueueHandle<LaneState>,
    state: LaneState,
    globals: Option<GlobalList>,
    surface: wl_surface::WlSurface,
    dmabuf: dmabuf::ZwpLinuxDmabufV1,
    presentation: wp_presentation::WpPresentation,
    color_repr: Option<crm::WpColorRepresentationManagerV1>,
    repr: Option<crs::WpColorRepresentationSurfaceV1>,
    /// (matrix, full range) last told to the compositor.
    repr_set: Option<(u8, bool)>,
    /// SDL scales the buffer to the window through its viewport; without one the buffer must
    /// match the window.
    has_viewport: bool,
    /// `None` where the compositor lacks a subcompositor or viewporter: no overlay surface.
    hud_globals: Option<HudGlobals>,
    hud: Option<Hud>,
    /// `None` without `wp_linux_drm_syncobj_manager_v1`: buffers go with implicit sync.
    syncobj: Option<sym::WpLinuxDrmSyncobjManagerV1>,
    /// Explicit sync on SDL's surface, only while the lane owns the window: Mesa's swapchain
    /// makes its own there, and a second is a fatal `surface_exists`.
    main_sync: Option<sys::WpLinuxDrmSyncobjSurfaceV1>,
    /// Each buffer's imported (acquire, release) timelines.
    timelines: HashMap<
        u64,
        (
            syt::WpLinuxDrmSyncobjTimelineV1,
            syt::WpLinuxDrmSyncobjTimelineV1,
        ),
    >,
    color_mgr: Option<cmm::WpColorManagerV1>,
    /// Colour management on SDL's surface, while the lane owns the window.
    cm_surface: Option<cms::WpColorManagementSurfaceV1>,
    hdr_desc: Option<HdrDesc>,
    /// The description set on the surface now carries PQ.
    hdr_set: bool,
    /// The compositor refused a PQ description: HDR stays on the swapchain path.
    hdr_refused: bool,
    /// After [`TRIAL_FRAMES`]: whether the compositor scans the picture out. A compositor
    /// that composites it gains nothing from the lane, and can scan out the swapchain's.
    verdict: Option<bool>,
    seq: usize,
    dead: bool,
    // SAFETY: field drop order keeps SDL's display alive past every borrowed proxy and queue.
    _window: Arc<WindowContext>,
}

impl NativeLane {
    /// The lane on SDL's Wayland connection, or `None` where the compositor lacks dma-buf
    /// feedback or presentation timing (or the window is not Wayland). `timelines`: the
    /// presenter can hand out acquire and release timelines, so explicit sync may be used.
    pub fn new(window: &sdl3::video::Window, timelines: bool) -> Result<Option<Self>> {
        let driver = window.subsystem().current_video_driver();
        if driver != "wayland" {
            tracing::info!(
                driver,
                "native scanout: SDL is not on Wayland — Vulkan presents"
            );
            return Ok(None);
        }
        // SAFETY: the live window owns the pointers; none transfers ownership.
        let (display, surface_ptr, viewport_ptr) = unsafe {
            let props = sdl3::sys::video::SDL_GetWindowProperties(window.raw());
            let get = |name| {
                sdl3::sys::properties::SDL_GetPointerProperty(props, name, std::ptr::null_mut())
            };
            (
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_DISPLAY_POINTER),
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_SURFACE_POINTER),
                get(sdl3::sys::video::SDL_PROP_WINDOW_WAYLAND_VIEWPORT_POINTER),
            )
        };
        if display.is_null() || surface_ptr.is_null() {
            return Ok(None);
        }
        // SAFETY: window.context() is retained until after the foreign backend is dropped.
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let conn = Connection::from_backend(backend);
        let (globals, mut queue) =
            registry_queue_init::<LaneState>(&conn).context("native lane registry")?;
        let qh = queue.handle();
        let dmabuf: dmabuf::ZwpLinuxDmabufV1 = match globals.bind(&qh, 4..=5, ()) {
            Ok(d) => d,
            Err(e) => {
                tracing::info!(error = %e, "native scanout: no linux-dmabuf v4 — Vulkan presents");
                return Ok(None);
            }
        };
        let presentation: wp_presentation::WpPresentation = match globals.bind(&qh, 1..=2, ()) {
            Ok(p) => p,
            Err(e) => {
                tracing::info!(error = %e, "native scanout: no wp_presentation — Vulkan presents");
                return Ok(None);
            }
        };
        let color_repr: Option<crm::WpColorRepresentationManagerV1> =
            globals.bind(&qh, 1..=1, ()).ok();
        let syncobj: Option<sym::WpLinuxDrmSyncobjManagerV1> = timelines
            .then(|| globals.bind(&qh, 1..=1, ()).ok())
            .flatten();
        // v2 answers with `ready2`; v1's `ready` is handled too.
        let color_mgr: Option<cmm::WpColorManagerV1> = globals.bind(&qh, 1..=2, ()).ok();
        let hud_globals = match (
            globals.bind::<wl_compositor::WlCompositor, _, _>(&qh, 4..=6, ()),
            globals.bind::<wl_subcompositor::WlSubcompositor, _, _>(&qh, 1..=1, ()),
            globals.bind::<wp_viewporter::WpViewporter, _, _>(&qh, 1..=1, ()),
        ) {
            (Ok(compositor), Ok(subcompositor), Ok(viewporter)) => Some(HudGlobals {
                compositor,
                subcompositor,
                viewporter,
            }),
            _ => None,
        };
        // SAFETY: SDL's live wl_surface proxy on this display; the interface matches.
        let surface_id =
            unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface_ptr.cast()) }
                .context("SDL wl_surface id")?;
        let surface =
            wl_surface::WlSurface::from_id(&conn, surface_id).context("SDL wl_surface proxy")?;
        dmabuf.get_surface_feedback(&surface, &qh, ());
        let mut state = LaneState::default();
        for _ in 0..4 {
            queue
                .roundtrip(&mut state)
                .context("native lane feedback")?;
            if state.feedback_done && state.clock_id.is_some() {
                break;
            }
        }
        if state.clock_id != Some(CLOCK_MONOTONIC) {
            tracing::info!(
                clock = ?state.clock_id,
                "native scanout: presentation clock is not CLOCK_MONOTONIC — Vulkan presents"
            );
            return Ok(None);
        }
        tracing::info!(
            pairs = state.pairs.len(),
            scanout_pairs = state.scanout.len(),
            color_representation = color_repr.is_some(),
            viewport = !viewport_ptr.is_null(),
            overlay_surface = hud_globals.is_some(),
            explicit_sync = syncobj.is_some(),
            hdr = color_mgr.is_some()
                && state.cm_features.contains(&CM_FEATURE_PARAMETRIC)
                && state.cm_tfs.contains(&CM_TF_ST2084_PQ)
                && state.cm_primaries.contains(&CM_PRIMARIES_BT2020),
            "native scanout lane armed on SDL's surface"
        );
        Ok(Some(Self {
            conn,
            queue,
            qh,
            state,
            globals: Some(globals),
            surface,
            dmabuf,
            presentation,
            color_repr,
            repr: None,
            repr_set: None,
            has_viewport: !viewport_ptr.is_null(),
            hud_globals,
            hud: None,
            syncobj,
            main_sync: None,
            timelines: HashMap::new(),
            color_mgr,
            cm_surface: None,
            hdr_desc: None,
            hdr_set: false,
            hdr_refused: false,
            seq: 0,
            verdict: None,
            dead: false,
            _window: window.context(),
        }))
    }

    /// Buffers must carry acquire and release points: the compositor speaks explicit sync.
    pub fn explicit_sync(&self) -> bool {
        self.syncobj.is_some()
    }

    /// The compositor takes PQ BT.2020 pictures through a parametric image description.
    pub fn hdr_capable(&self) -> bool {
        let s = &self.state;
        self.color_mgr.is_some()
            && !self.hdr_refused
            && s.cm_features.contains(&CM_FEATURE_PARAMETRIC)
            && s.cm_tfs.contains(&CM_TF_ST2084_PQ)
            && s.cm_primaries.contains(&CM_PRIMARIES_BT2020)
            && s.cm_intents.contains(&CM_INTENT_PERCEPTUAL)
    }

    /// The swapchain is gone: SDL's surface is the lane's, so its explicit sync object is
    /// too. Colour objects are made on first use.
    pub fn take_window(&mut self) {
        if let Some(m) = &self.syncobj {
            self.main_sync = Some(m.get_surface(&self.surface, &self.qh, ()));
        }
        self.flush();
    }

    /// Hand SDL's surface back before a swapchain returns: Mesa makes its own sync (and
    /// colour) objects there, and the YCbCr coefficients must not meet its RGB buffers.
    pub fn release_window(&mut self) {
        if let Some(s) = self.main_sync.take() {
            s.destroy();
        }
        if let Some(s) = self.cm_surface.take() {
            s.destroy();
        }
        if let Some(r) = self.repr.take() {
            r.destroy();
        }
        self.repr_set = None;
        self.hdr_set = false;
        self.flush();
    }

    /// Import `key`'s acquire and release timelines, for commits under explicit sync.
    pub fn import_timelines(&mut self, key: u64, acquire: BorrowedFd<'_>, release: BorrowedFd<'_>) {
        let Some(m) = &self.syncobj else {
            return;
        };
        let a = m.import_timeline(acquire, &self.qh, ());
        let r = m.import_timeline(release, &self.qh, ());
        if let Some((old_a, old_r)) = self.timelines.insert(key, (a, r)) {
            old_a.destroy();
            old_r.destroy();
        }
        self.flush();
    }

    /// `key`'s release point passed: the buffer and its hold are free again.
    pub fn released(&mut self, key: u64) {
        if let Some(Import::Ready(slot)) = self.state.imports.get_mut(&key) {
            slot.held = None;
        }
    }

    /// Set `key`'s acquire and release point on `sync` for the coming commit.
    fn set_points(
        sync: &sys::WpLinuxDrmSyncobjSurfaceV1,
        timelines: &HashMap<
            u64,
            (
                syt::WpLinuxDrmSyncobjTimelineV1,
                syt::WpLinuxDrmSyncobjTimelineV1,
            ),
        >,
        key: u64,
        point: Option<u64>,
    ) -> bool {
        let (Some(point), Some((a, r))) = (point, timelines.get(&key)) else {
            return false;
        };
        let (hi, lo) = ((point >> 32) as u32, point as u32);
        sync.set_acquire_point(a, hi, lo);
        sync.set_release_point(r, hi, lo);
        true
    }

    /// The compositor can take the overlay on its own surface above the picture.
    pub fn overlay_supported(&self) -> bool {
        self.hud_globals.is_some() && !self.dead
    }

    /// Show the free buffer under `key` as the overlay, scaled to the window's `logical`
    /// size, and keep `hold` until the compositor releases it. Under explicit sync `point`
    /// is the buffer's acquire and release point; a commit without one is refused.
    pub fn overlay_show(
        &mut self,
        key: u64,
        logical: (u32, u32),
        hold: Box<dyn Any>,
        point: Option<u64>,
    ) -> bool {
        if self.dead || self.slot_state(key) != SlotState::Free {
            return false;
        }
        let Some(g) = &self.hud_globals else {
            return false;
        };
        let hud = self.hud.get_or_insert_with(|| {
            let surface = g.compositor.create_surface(&self.qh, ());
            let sub = g
                .subcompositor
                .get_subsurface(&surface, &self.surface, &self.qh, ());
            sub.set_position(0, 0);
            sub.set_desync();
            // An empty input region: the pointer stays on SDL's surface underneath.
            let region = g.compositor.create_region(&self.qh, ());
            surface.set_input_region(Some(&region));
            region.destroy();
            let viewport = g.viewporter.get_viewport(&surface, &self.qh, ());
            let sync = self
                .syncobj
                .as_ref()
                .map(|m| m.get_surface(&surface, &self.qh, ()));
            Hud {
                surface,
                sub,
                viewport,
                sync,
                mapped: false,
            }
        });
        if let Some(sync) = &hud.sync {
            if !Self::set_points(sync, &self.timelines, key, point) {
                return false;
            }
            self.state.explicit_keys.insert(key);
        }
        let Some(Import::Ready(slot)) = self.state.imports.get_mut(&key) else {
            return false;
        };
        hud.viewport
            .set_destination(logical.0.max(1) as i32, logical.1.max(1) as i32);
        hud.surface.attach(Some(&slot.buffer), 0, 0);
        hud.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        hud.surface.commit();
        hud.mapped = true;
        slot.held = Some(hold);
        self.flush();
        true
    }

    /// Unmap the overlay's surface; the compositor releases its buffer.
    pub fn overlay_hide(&mut self) {
        let Some(hud) = self.hud.as_mut().filter(|h| h.mapped) else {
            return;
        };
        hud.surface.attach(None, 0, 0);
        hud.surface.commit();
        hud.mapped = false;
        self.flush();
    }

    /// Dispatch what SDL's socket reads brought for this lane. A protocol error retires the
    /// lane; the connection itself is SDL's to fail on, and so does a frame the compositor
    /// leaves unanswered past [`STALL_NS`]. After [`TRIAL_FRAMES`] judged frames on glass the
    /// lane stays only if most of them went out zero-copy.
    pub fn pump(&mut self) {
        if self.dead {
            return;
        }
        if self.queue.dispatch_pending(&mut self.state).is_err()
            || self.conn.protocol_error().is_some()
        {
            tracing::warn!("native scanout: Wayland error — the lane retires, Vulkan presents");
            self.dead = true;
            return;
        }
        let now = pf_client_core::session::now_ns();
        let oldest = self.state.jobs.values().map(|j| j.submitted_ns).min();
        if oldest.is_some_and(|t| now.saturating_sub(t) > STALL_NS) {
            tracing::warn!(
                "native scanout: the compositor answered no frame for a second — the lane \
                 retires, Vulkan presents"
            );
            self.dead = true;
            return;
        }
        let (on_glass, zero_copy) = (self.state.trial_presented, self.state.trial_zero_copy);
        if self.verdict.is_none() && on_glass >= TRIAL_FRAMES {
            let scans_out = zero_copy * 2 >= on_glass;
            self.verdict = Some(scans_out);
            tracing::info!(
                on_glass,
                zero_copy,
                "native scanout: {}",
                if scans_out {
                    "the compositor scans the picture out — the lane keeps the window"
                } else {
                    "the compositor composites the picture — the lane retires, Vulkan presents"
                }
            );
            self.dead = !scans_out;
        }
    }

    /// The trial showed the compositor scanning the picture out.
    pub fn scans_out(&self) -> bool {
        self.verdict == Some(true)
    }

    fn flush(&mut self) {
        if let Err(wayland_backend::client::WaylandError::Protocol(_)) = self.conn.flush() {
            self.dead = true;
        }
    }

    /// Whether a picture of this size and colour can be the window's buffer: SDR, or PQ
    /// where the compositor takes a PQ BT.2020 description and has scanned the lane out,
    /// and it fills the window (through SDL's viewport, or at the window's own size without
    /// one).
    pub fn fits(
        &self,
        color: ColorDesc,
        frame: (u32, u32),
        view: (u32, u32),
        fit: VideoFit,
    ) -> bool {
        // KWin 6.7 answers no P010 frame carrying a PQ description, so PQ waits for a
        // compositor that has already scanned SDR frames out.
        if self.dead || (color.is_pq() && !(self.hdr_capable() && self.scans_out())) {
            return false;
        }
        if self.color_repr.is_some() {
            let (c, r) = representation(color);
            if !self.state.repr_pairs.contains(&(c as u32, r as u32)) {
                return false;
            }
        }
        let (vw, vh) = (u64::from(view.0), u64::from(view.1));
        let (fw, fh) = (u64::from(frame.0), u64::from(frame.1));
        if vw == 0 || vh == 0 || fw == 0 || fh == 0 {
            return false;
        }
        if !self.has_viewport {
            return (fw, fh) == (vw, vh);
        }
        // Stretch fills by definition; fit and crop only when no bars or cut would show.
        fit == VideoFit::Stretch || (vw * fh).abs_diff(vh * fw) * 100 <= vw * fh
    }

    /// The surface feedback lists this (fourcc, modifier).
    pub fn lists(&self, fourcc: u32, modifier: u64) -> bool {
        modifier != DRM_FORMAT_MOD_INVALID && self.state.pairs.contains(&(fourcc, modifier))
    }

    /// Modifiers the surface takes for `fourcc`, scanout tranches first.
    pub fn modifiers_for(&self, fourcc: u32) -> Vec<u64> {
        let scanout = self.state.scanout.iter().filter(|(f, _)| *f == fourcc);
        let rest = self.state.pairs.iter().filter(|(f, _)| *f == fourcc);
        let mut out: Vec<u64> = Vec::new();
        for &(_, m) in scanout.chain(rest) {
            if m != DRM_FORMAT_MOD_INVALID && !out.contains(&m) {
                out.push(m);
            }
        }
        out
    }

    /// Changes whenever the compositor sends new surface feedback.
    pub fn feedback_generation(&self) -> u64 {
        self.state.feedback_gen
    }

    pub fn is_dead(&self) -> bool {
        self.dead
    }

    pub fn slot_state(&self, key: u64) -> SlotState {
        match self.state.imports.get(&key) {
            None => SlotState::Unknown,
            Some(Import::Pending(_)) => SlotState::Pending,
            Some(Import::Failed) => SlotState::Failed,
            Some(Import::Ready(slot)) if slot.held.is_some() => SlotState::Held,
            Some(Import::Ready(_)) => SlotState::Free,
        }
    }

    /// Offer a dma-buf under `key`: one `(fd, offset, pitch)` per memory plane. The answer
    /// arrives with a later [`Self::pump`]; libwayland dups each fd while marshalling.
    pub fn import(
        &mut self,
        key: u64,
        size: (u32, u32),
        fourcc: u32,
        modifier: u64,
        planes: &[(BorrowedFd<'_>, u32, u32)],
    ) {
        let prm = self.dmabuf.create_params(&self.qh, ());
        for (i, (fd, offset, pitch)) in planes.iter().enumerate() {
            prm.add(
                *fd,
                i as u32,
                *offset,
                *pitch,
                (modifier >> 32) as u32,
                modifier as u32,
            );
        }
        prm.create(size.0 as i32, size.1 as i32, fourcc, params::Flags::empty());
        self.state.pending.insert(prm.id().protocol_id(), key);
        self.state.imports.insert(key, Import::Pending(prm));
        self.flush();
    }

    /// Drop the buffer under `key` and its timelines. The compositor keeps its own reference
    /// to a buffer it still shows; the hold goes now.
    pub fn forget(&mut self, key: u64) {
        if let Some((a, r)) = self.timelines.remove(&key) {
            a.destroy();
            r.destroy();
        }
        self.state.explicit_keys.remove(&key);
        match self.state.imports.remove(&key) {
            Some(Import::Ready(slot)) => {
                self.state.by_buffer.remove(&slot.buffer.id());
                slot.buffer.destroy();
            }
            Some(Import::Pending(prm)) => {
                self.state.pending.remove(&prm.id().protocol_id());
                prm.destroy();
            }
            _ => {}
        }
        self.flush();
    }

    /// The HDR description for `meta`, built once per metadata and waited until the compositor
    /// calls it ready (one roundtrip); a refusal keeps PQ off this lane for the session.
    fn ensure_hdr_desc(&mut self, meta: Option<HdrMeta>) -> bool {
        if self.hdr_desc.as_ref().is_some_and(|d| d.meta == meta) {
            return true;
        }
        let Some(mgr) = &self.color_mgr else {
            return false;
        };
        let creator = mgr.create_parametric_creator(&self.qh, ());
        creator.set_tf_named(cmm::TransferFunction::St2084Pq);
        creator.set_primaries_named(cmm::Primaries::Bt2020);
        if let Some(m) = meta.filter(|_| self.state.cm_features.contains(&CM_FEATURE_MASTERING)) {
            let s = hdr_static(&m, mgr.version());
            let p = s.primaries;
            creator.set_mastering_display_primaries(p[0], p[1], p[2], p[3], p[4], p[5], p[6], p[7]);
            if let Some((min, max)) = s.luminance {
                creator.set_mastering_luminance(min, max);
            }
            if let Some(cll) = s.max_cll {
                creator.set_max_cll(cll);
            }
            if let Some(fall) = s.max_fall {
                creator.set_max_fall(fall);
            }
        }
        let desc = creator.create(&self.qh, ());
        self.state.desc_answer = None;
        for _ in 0..4 {
            if self.state.desc_answer.is_some() || self.queue.roundtrip(&mut self.state).is_err() {
                break;
            }
        }
        if self.state.desc_answer != Some(true) {
            desc.destroy();
            self.hdr_refused = true;
            return false;
        }
        if let Some(old) = self.hdr_desc.replace(HdrDesc { meta, desc }) {
            old.desc.destroy();
        }
        self.hdr_set = false;
        true
    }

    /// Tell the compositor how to read this frame: the YCbCr matrix and range (only a pair it
    /// advertised), H.273's default chroma siting, and for PQ the BT.2020 PQ description.
    /// `false` when it cannot be told, and the frame must not be committed.
    fn apply_color(&mut self, color: ColorDesc, meta: Option<HdrMeta>) -> bool {
        if color.is_pq() {
            if !self.ensure_hdr_desc(meta) {
                return false;
            }
            if !self.hdr_set {
                let (Some(mgr), Some(d)) = (&self.color_mgr, &self.hdr_desc) else {
                    return false;
                };
                let cm = self
                    .cm_surface
                    .get_or_insert_with(|| mgr.get_surface(&self.surface, &self.qh, ()));
                cm.set_image_description(&d.desc, cmm::RenderIntent::Perceptual);
                self.hdr_set = true;
            }
        } else if self.hdr_set {
            if let Some(cm) = &self.cm_surface {
                cm.unset_image_description();
            }
            self.hdr_set = false;
        }
        let Some(manager) = &self.color_repr else {
            return true;
        };
        let want = (color.matrix, color.full_range);
        if self.repr_set == Some(want) {
            return true;
        }
        let (coefficients, range) = representation(color);
        if !self
            .state
            .repr_pairs
            .contains(&(coefficients as u32, range as u32))
        {
            return false;
        }
        let repr = self.repr.get_or_insert_with(|| {
            let r = manager.get_surface(&self.surface, &self.qh, ());
            r.set_chroma_location(crs::ChromaLocation::Type0);
            r
        });
        repr.set_coefficients_and_range(coefficients, range);
        self.repr_set = Some(want);
        true
    }

    /// Put the free buffer under `key` on the window and keep `hold` until the compositor
    /// releases it. Under explicit sync `point` is the buffer's acquire and release point.
    /// `false` when the buffer is not free or cannot be described; `hold` is dropped then.
    #[allow(clippy::too_many_arguments)]
    pub fn commit(
        &mut self,
        key: u64,
        color: ColorDesc,
        meta: Option<HdrMeta>,
        hold: Box<dyn Any>,
        pts_ns: u64,
        decoded_ns: u64,
        point: Option<u64>,
    ) -> bool {
        if self.dead || self.slot_state(key) != SlotState::Free {
            return false;
        }
        if !self.apply_color(color, meta) {
            return false;
        }
        if let Some(sync) = &self.main_sync {
            if !Self::set_points(sync, &self.timelines, key, point) {
                return false;
            }
            self.state.explicit_keys.insert(key);
        }
        let seq = self.seq;
        self.seq += 1;
        let judged = !self.hud.as_ref().is_some_and(|h| h.mapped);
        let Some(Import::Ready(slot)) = self.state.imports.get_mut(&key) else {
            return false;
        };
        self.surface.attach(Some(&slot.buffer), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.presentation.feedback(&self.surface, &self.qh, seq);
        self.state.jobs.insert(
            seq,
            Job {
                pts_ns,
                decoded_ns,
                submitted_ns: pf_client_core::session::now_ns(),
                judged,
            },
        );
        self.surface.commit();
        slot.held = Some(hold);
        self.flush();
        true
    }

    /// A refused import of a listed pair retires the lane for the session.
    pub fn refused(&mut self, fourcc: u32, modifier: u64) {
        // NVIDIA under Mutter refuses every YUV import: expected, so not a warning.
        tracing::info!(
            fourcc = format!("{fourcc:#010x}"),
            modifier = format!("{modifier:#x}"),
            "native scanout: the compositor refused a listed dma-buf — the lane retires"
        );
        self.dead = true;
    }

    /// Whether a VAAPI frame can be the window's buffer: listed pair and it [`Self::fits`].
    pub fn takes(&self, d: &DmabufFrame, view: (u32, u32), fit: VideoFit) -> bool {
        self.lists(d.fourcc, d.modifier) && self.fits(d.color, (d.width, d.height), view, fit)
    }

    /// Where a VAAPI frame's pool slot stands, importing it on first sight: in the
    /// background, or with `now` on the spot (one roundtrip). `Failed` also when the lane is
    /// dead.
    pub fn prepare(&mut self, d: &DmabufFrame, now: bool) -> SlotState {
        self.pump();
        if self.dead {
            return SlotState::Failed;
        }
        if self.slot_state(d.pool_key) == SlotState::Unknown {
            let planes: Vec<_> = d
                .planes
                .iter()
                // SAFETY: the frame owns each fd for this call; libwayland dups it.
                .map(|p| (unsafe { BorrowedFd::borrow_raw(p.fd) }, p.offset, p.stride))
                .collect();
            self.import(
                d.pool_key,
                (d.width, d.height),
                d.fourcc,
                d.modifier,
                &planes,
            );
            if now && self.queue.roundtrip(&mut self.state).is_err() {
                self.dead = true;
                return SlotState::Failed;
            }
        }
        self.slot_state(d.pool_key)
    }

    /// Commit a VAAPI frame whose slot [`Self::prepare`] reported free; the frame's guard
    /// stays with the buffer until the compositor releases it. The pump waited the decode
    /// already, so its acquire point (if any) is signalled by the caller from the host.
    pub fn commit_vaapi(
        &mut self,
        d: DmabufFrame,
        meta: Option<HdrMeta>,
        pts_ns: u64,
        decoded_ns: u64,
        point: Option<u64>,
    ) -> bool {
        // A leftover sync_file costs a poll.
        for fd in &d.sync_fds {
            use std::os::fd::AsFd as _;
            let _ = pf_dmabuf::fence::wait_sync_file(fd.as_fd(), 50);
        }
        let (key, color) = (d.pool_key, d.color);
        let DmabufFrame { guard, .. } = d;
        self.commit(key, color, meta, Box::new(guard), pts_ns, decoded_ns, point)
    }

    /// Frames the compositor reported on glass since the last call.
    pub fn take_samples(&mut self) -> Vec<NativeSample> {
        self.pump();
        std::mem::take(&mut self.state.samples)
    }

    /// (zero-copy, presented) since the last call.
    pub fn take_zero_copy(&mut self) -> (u32, u32) {
        let out = (self.state.zero_copy, self.state.presented);
        self.state.zero_copy = 0;
        self.state.presented = 0;
        out
    }
}

impl Drop for NativeLane {
    fn drop(&mut self) {
        if let Some(hud) = self.hud.take() {
            if let Some(sync) = hud.sync {
                sync.destroy();
            }
            hud.viewport.destroy();
            hud.sub.destroy();
            hud.surface.destroy();
        }
        if let Some(s) = self.main_sync.take() {
            s.destroy();
        }
        for (_, (a, r)) in self.timelines.drain() {
            a.destroy();
            r.destroy();
        }
        if let Some(m) = self.syncobj.take() {
            m.destroy();
        }
        if let Some(s) = self.cm_surface.take() {
            s.destroy();
        }
        if let Some(d) = self.hdr_desc.take() {
            d.desc.destroy();
        }
        if let Some(m) = self.color_mgr.take() {
            m.destroy();
        }
        if let Some(g) = self.hud_globals.take() {
            g.subcompositor.destroy();
            g.viewporter.destroy();
        }
        for import in self.state.imports.values() {
            match import {
                Import::Ready(slot) => slot.buffer.destroy(),
                Import::Pending(prm) => prm.destroy(),
                Import::Failed => {}
            }
        }
        if let Some(repr) = self.repr.take() {
            repr.destroy();
        }
        self.dmabuf.destroy();
        self.presentation.destroy();
        if let Some(globals) = self.globals.take() {
            let id = globals.registry().id();
            globals.destroy();
            let _ = self.conn.backend().destroy_object(&id);
        }
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod tests {
    use punktfunk_core::quic::HdrMeta;

    fn meta(max_cll: u16, max_fall: u16) -> HdrMeta {
        HdrMeta {
            // G, B, R in ST.2086 order, 1/50000.
            display_primaries: [[8500, 39850], [6550, 2300], [35400, 14600]],
            white_point: [15635, 16450],
            max_display_mastering_luminance: 10_000_000,
            min_display_mastering_luminance: 1,
            max_cll,
            max_fall,
        }
    }

    /// Primaries leave in R, G, B, W order at 1/1M; luminance splits into ×10000 and whole nits.
    #[test]
    fn mastering_metadata_reaches_the_protocol_in_its_units() {
        let s = super::hdr_static(&meta(1000, 400), 2);
        assert_eq!(
            s.primaries,
            [708_000, 292_000, 170_000, 797_000, 131_000, 46_000, 312_700, 329_000]
        );
        assert_eq!(s.luminance, Some((1, 1000)));
        assert_eq!((s.max_cll, s.max_fall), (Some(1000), Some(400)));
    }

    /// MaxFALL above MaxCLL is a protocol error, and version 1 also wants both inside the
    /// mastering range: either case drops the value rather than the description.
    #[test]
    fn light_levels_the_protocol_would_refuse_are_left_out() {
        let s = super::hdr_static(&meta(300, 500), 2);
        assert_eq!((s.max_cll, s.max_fall), (Some(300), None));
        let s = super::hdr_static(&meta(4000, 400), 1);
        assert_eq!(
            (s.max_cll, s.max_fall),
            (None, Some(400)),
            "4000 nits > 1000-nit mastering"
        );
        let s = super::hdr_static(&meta(4000, 400), 2);
        assert_eq!(s.max_cll, Some(4000), "version 2 lifts the range rule");
    }

    /// The lane's realtime stamp moves with the wall clock, never with the monotonic offset
    /// alone: a presented time "now" lands at "now" on the session clock.
    #[test]
    fn a_presented_now_lands_on_the_session_clock_now() {
        let mono = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        let mono = mono.tv_sec as u64 * 1_000_000_000 + mono.tv_nsec as u64;
        let real = pf_client_core::session::now_ns();
        let got = super::monotonic_to_realtime(mono);
        assert!(got.abs_diff(real) < 50_000_000, "{got} vs {real}");
    }
}
