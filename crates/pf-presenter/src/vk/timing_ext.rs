//! `VK_EXT_present_timing`, hand-declared: ash 0.38 predates it.
//!
//! The presentation engine reports when a present reached the screen, on a clock the
//! swapchain names. A present-wait stamp is the instant a waiter thread woke; this one is
//! the engine's. Values are the registry's (`vk.xml`, extension 209).
//!
//! Results are polled, never waited: the present-wait2 waiter asks after each wait
//! completes, under the swapchain lock it already holds. The swapchain keeps a fixed
//! queue of pending results, and a present that asks for a stamp into a full queue
//! fails outright, so [`Engine::may_ask`] stops asking two short of [`QUEUE`].
//!
//! How exact a stamp is depends on the compositor and the kernel driver behind it: a
//! flip time read off the scanout position is exact, one taken when the flip interrupt
//! was handled is not. Nothing here may assume the former.

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use ash::vk;

pub(super) const NAME: &std::ffi::CStr = c"VK_EXT_present_timing";
/// `VK_SWAPCHAIN_CREATE_PRESENT_TIMING_BIT_EXT`.
pub(crate) const SWAPCHAIN_FLAG: vk::SwapchainCreateFlagsKHR =
    vk::SwapchainCreateFlagsKHR::from_raw(0x200);
/// `VK_ERROR_PRESENT_TIMING_QUEUE_FULL_EXT`.
pub(crate) const QUEUE_FULL: vk::Result = vk::Result::from_raw(-1_000_208_000);

const BASE: i32 = 1_000_208_000;
const S_FEATURES: i32 = BASE;
const S_TIMING_PROPS: i32 = BASE + 1;
const S_TIME_DOMAIN_PROPS: i32 = BASE + 2;
const S_TIMINGS_INFO: i32 = BASE + 3;
const S_TIMING_INFO: i32 = BASE + 4;
const S_PAST_INFO: i32 = BASE + 5;
const S_PAST_PROPS: i32 = BASE + 6;
const S_PAST_TIMING: i32 = BASE + 7;
const S_SURFACE_CAPS: i32 = BASE + 8;
const S_CALIBRATED: i32 = BASE + 9;

/// `VK_TIME_DOMAIN_PRESENT_STAGE_LOCAL_EXT` and `VK_TIME_DOMAIN_SWAPCHAIN_LOCAL_EXT`:
/// clocks only the swapchain can read, compared through a chained calibration.
const DOMAIN_STAGE_LOCAL: i32 = BASE;
const DOMAIN_SWAPCHAIN_LOCAL: i32 = BASE + 1;

/// `VK_PRESENT_STAGE_IMAGE_FIRST_PIXEL_OUT_BIT_EXT`: scanout began.
pub(crate) const STAGE_PIXEL_OUT: u32 = 0x4;
/// `VK_PRESENT_STAGE_IMAGE_FIRST_PIXEL_VISIBLE_BIT_EXT`: the panel lit it.
pub(crate) const STAGE_PIXEL_VISIBLE: u32 = 0x8;
/// `VK_PAST_PRESENTATION_TIMING_ALLOW_OUT_OF_ORDER_RESULTS_BIT_EXT`: a present the
/// compositor never answers must not hold back the ones after it.
const PAST_OUT_OF_ORDER: u32 = 0x2;

/// Pending results the swapchain holds. 16 is a quarter second at 60 Hz.
const QUEUE: u32 = 16;
/// Results read per poll.
const BATCH: usize = 8;
/// Time domains read per swapchain.
const DOMAINS: usize = 8;

/// `VkPhysicalDevicePresentTimingFeaturesEXT`.
#[repr(C)]
pub(super) struct Features {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub present_timing: vk::Bool32,
    pub present_at_absolute_time: vk::Bool32,
    pub present_at_relative_time: vk::Bool32,
}

impl Features {
    pub(super) fn new(present_timing: vk::Bool32) -> Features {
        Features {
            s_type: vk::StructureType::from_raw(S_FEATURES),
            p_next: std::ptr::null_mut(),
            present_timing,
            present_at_absolute_time: vk::FALSE,
            present_at_relative_time: vk::FALSE,
        }
    }
}

/// `VkPresentTimingSurfaceCapabilitiesEXT`.
#[repr(C)]
pub(super) struct SurfaceCaps {
    pub s_type: vk::StructureType,
    pub p_next: *mut c_void,
    pub present_timing_supported: vk::Bool32,
    pub present_at_absolute_time_supported: vk::Bool32,
    pub present_at_relative_time_supported: vk::Bool32,
    pub present_stage_queries: u32,
}

impl Default for SurfaceCaps {
    fn default() -> SurfaceCaps {
        SurfaceCaps {
            s_type: vk::StructureType::from_raw(S_SURFACE_CAPS),
            p_next: std::ptr::null_mut(),
            present_timing_supported: vk::FALSE,
            present_at_absolute_time_supported: vk::FALSE,
            present_at_relative_time_supported: vk::FALSE,
            present_stage_queries: 0,
        }
    }
}

/// `VkPresentTimingInfoEXT`: what one present asks for.
#[repr(C)]
pub(crate) struct TimingInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    flags: u32,
    target_time: u64,
    time_domain_id: u64,
    present_stage_queries: u32,
    target_time_domain_present_stage: u32,
}

/// `VkPresentTimingsInfoEXT`, chained onto `VkPresentInfoKHR`. Borrows `info` by pointer:
/// it must outlive the present call.
#[repr(C)]
pub(crate) struct TimingsInfo {
    s_type: vk::StructureType,
    pub(crate) p_next: *const c_void,
    swapchain_count: u32,
    p_timing_infos: *const TimingInfo,
}

impl TimingInfo {
    /// Ask for `stages` on the clock `time_domain_id` names, with no target time.
    pub(crate) fn stamps(stages: u32, time_domain_id: u64) -> TimingInfo {
        TimingInfo {
            s_type: vk::StructureType::from_raw(S_TIMING_INFO),
            p_next: std::ptr::null(),
            flags: 0,
            target_time: 0,
            time_domain_id,
            present_stage_queries: stages,
            target_time_domain_present_stage: 0,
        }
    }
}

impl TimingsInfo {
    pub(crate) fn new(info: &TimingInfo) -> TimingsInfo {
        TimingsInfo {
            s_type: vk::StructureType::from_raw(S_TIMINGS_INFO),
            p_next: std::ptr::null(),
            swapchain_count: 1,
            p_timing_infos: info,
        }
    }
}

/// `VkPresentStageTimeEXT`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct StageTime {
    stage: u32,
    time: u64,
}

/// `VkPastPresentationTimingEXT`.
#[repr(C)]
struct PastTiming {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    present_id: u64,
    target_time: u64,
    present_stage_count: u32,
    p_present_stages: *mut StageTime,
    time_domain: vk::TimeDomainKHR,
    time_domain_id: u64,
    report_complete: vk::Bool32,
}

/// `VkPastPresentationTimingInfoEXT`.
#[repr(C)]
struct PastInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    flags: u32,
    swapchain: vk::SwapchainKHR,
}

/// `VkPastPresentationTimingPropertiesEXT`.
#[repr(C)]
struct PastProps {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    timing_properties_counter: u64,
    time_domains_counter: u64,
    presentation_timing_count: u32,
    p_presentation_timings: *mut PastTiming,
}

/// `VkSwapchainTimingPropertiesEXT`.
#[repr(C)]
struct TimingProps {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    refresh_duration: u64,
    refresh_interval: u64,
}

/// `VkSwapchainTimeDomainPropertiesEXT`.
#[repr(C)]
struct TimeDomainProps {
    s_type: vk::StructureType,
    p_next: *mut c_void,
    time_domain_count: u32,
    p_time_domains: *mut vk::TimeDomainKHR,
    p_time_domain_ids: *mut u64,
}

/// `VkSwapchainCalibratedTimestampInfoEXT`, chained onto `VkCalibratedTimestampInfoKHR`.
#[repr(C)]
struct CalibratedInfo {
    s_type: vk::StructureType,
    p_next: *const c_void,
    swapchain: vk::SwapchainKHR,
    present_stage: u32,
    time_domain_id: u64,
}

type SetQueueSizeFn = unsafe extern "system" fn(vk::Device, vk::SwapchainKHR, u32) -> vk::Result;
type GetTimingPropsFn = unsafe extern "system" fn(
    vk::Device,
    vk::SwapchainKHR,
    *mut TimingProps,
    *mut u64,
) -> vk::Result;
type GetTimeDomainsFn = unsafe extern "system" fn(
    vk::Device,
    vk::SwapchainKHR,
    *mut TimeDomainProps,
    *mut u64,
) -> vk::Result;
type GetPastFn =
    unsafe extern "system" fn(vk::Device, *const PastInfo, *mut PastProps) -> vk::Result;

/// One present the engine reported on.
pub(crate) struct Stamp {
    pub present_id: u64,
    /// On the session clock. `None`: the picture never reached the screen.
    pub displayed_ns: Option<u64>,
}

/// What the presenter reads of the engine's refresh. Both 0 until reported.
#[derive(Default)]
pub(crate) struct Refresh {
    /// One refresh cycle, the shortest under variable refresh.
    pub duration_ns: AtomicU64,
    /// Cycle start to cycle start: `duration_ns` on a fixed panel, `u64::MAX` under
    /// variable refresh, 0 where the engine cannot tell.
    pub interval_ns: AtomicU64,
}

/// The extension on one device: entry points, the stages to ask for, and the clock the
/// live swapchain stamps on.
pub(crate) struct Engine {
    device: vk::Device,
    set_queue_size: SetQueueSizeFn,
    get_timing_props: GetTimingPropsFn,
    get_time_domains: GetTimeDomainsFn,
    get_past: GetPastFn,
    calibrated: ash::khr::calibrated_timestamps::Device,
    /// The display stages this surface reports.
    stages: u32,
    /// The live swapchain's clock: `VkTimeDomainKHR` and the id presents name it by.
    domain: AtomicI32,
    domain_id: AtomicU64,
    /// Presents asked for a stamp and not yet reported.
    asked: AtomicUsize,
    pub(crate) refresh: Arc<Refresh>,
}

// SAFETY: the raw device handle and entry points are plain values; every call is made
// under the swapchain's host sync by its caller.
unsafe impl Send for Engine {}
// SAFETY: as above; the rest are atomics.
unsafe impl Sync for Engine {}

/// Session clock minus engine clock, re-read every [`RECALIBRATE`] results.
pub(crate) struct Offset {
    ns: i128,
    age: u32,
    /// The clock and stage it was read for.
    key: (i32, u64, u32),
}

/// Results between two clock comparisons: a second or so of frames.
const RECALIBRATE: u32 = 64;

impl Default for Offset {
    fn default() -> Offset {
        Offset {
            ns: 0,
            age: u32::MAX,
            key: (0, 0, 0),
        }
    }
}

/// Best first: a clock this process can read itself, then the swapchain's own.
fn domain_rank(domain: vk::TimeDomainKHR) -> u8 {
    match domain {
        vk::TimeDomainKHR::CLOCK_MONOTONIC => 0,
        vk::TimeDomainKHR::CLOCK_MONOTONIC_RAW => 1,
        vk::TimeDomainKHR::QUERY_PERFORMANCE_COUNTER => 2,
        d if d.as_raw() == DOMAIN_SWAPCHAIN_LOCAL => 3,
        d if d.as_raw() == DOMAIN_STAGE_LOCAL => 4,
        _ => 5,
    }
}

impl Engine {
    /// Load the entry points from `device`. `None` when the driver exports none of them.
    ///
    /// # Safety
    /// `device` is live, with the extension and `VK_KHR_calibrated_timestamps` enabled.
    pub(super) unsafe fn load(
        instance: &ash::Instance,
        device: &ash::Device,
        stages: u32,
    ) -> Option<Engine> {
        let get = |name: &std::ffi::CStr| {
            // SAFETY: a name lookup on the live device.
            unsafe { instance.get_device_proc_addr(device.handle(), name.as_ptr()) }
        };
        let set_queue_size = get(c"vkSetSwapchainPresentTimingQueueSizeEXT")?;
        let get_timing_props = get(c"vkGetSwapchainTimingPropertiesEXT")?;
        let get_time_domains = get(c"vkGetSwapchainTimeDomainPropertiesEXT")?;
        let get_past = get(c"vkGetPastPresentationTimingEXT")?;
        type Raw = unsafe extern "system" fn();
        // SAFETY: the registry declares these entry points with these signatures.
        let (set_queue_size, get_timing_props, get_time_domains, get_past) = unsafe {
            (
                std::mem::transmute::<Raw, SetQueueSizeFn>(set_queue_size),
                std::mem::transmute::<Raw, GetTimingPropsFn>(get_timing_props),
                std::mem::transmute::<Raw, GetTimeDomainsFn>(get_time_domains),
                std::mem::transmute::<Raw, GetPastFn>(get_past),
            )
        };
        Some(Engine {
            device: device.handle(),
            set_queue_size,
            get_timing_props,
            get_time_domains,
            get_past,
            calibrated: ash::khr::calibrated_timestamps::Device::new(instance, device),
            stages,
            domain: AtomicI32::new(0),
            domain_id: AtomicU64::new(0),
            asked: AtomicUsize::new(0),
            refresh: Arc::new(Refresh::default()),
        })
    }

    /// What a present asks for: the display stages, on the live swapchain's clock.
    pub(crate) fn request(&self) -> TimingInfo {
        TimingInfo::stamps(self.stages, self.domain_id.load(Ordering::Acquire))
    }

    /// Size a new swapchain's result queue, pick its clock and read its refresh. `false`:
    /// the swapchain takes no stamps.
    ///
    /// # Safety
    /// `swapchain` is live and host-synchronised by the caller.
    pub(crate) unsafe fn arm(&self, swapchain: vk::SwapchainKHR) -> bool {
        self.asked.store(0, Ordering::Release);
        // SAFETY: the caller's contract.
        if unsafe { (self.set_queue_size)(self.device, swapchain, QUEUE) } != vk::Result::SUCCESS {
            return false;
        }
        let mut domains = [vk::TimeDomainKHR::DEVICE; DOMAINS];
        let mut ids = [0u64; DOMAINS];
        let mut props = TimeDomainProps {
            s_type: vk::StructureType::from_raw(S_TIME_DOMAIN_PROPS),
            p_next: std::ptr::null_mut(),
            time_domain_count: DOMAINS as u32,
            p_time_domains: domains.as_mut_ptr(),
            p_time_domain_ids: ids.as_mut_ptr(),
        };
        // SAFETY: the caller's contract; the arrays outlive the call.
        let r = unsafe {
            (self.get_time_domains)(self.device, swapchain, &mut props, std::ptr::null_mut())
        };
        let n = (props.time_domain_count as usize).min(DOMAINS);
        if (r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE) || n == 0 {
            return false;
        }
        let best = (0..n)
            .min_by_key(|&i| domain_rank(domains[i]))
            .expect("n > 0");
        self.domain.store(domains[best].as_raw(), Ordering::Release);
        self.domain_id.store(ids[best], Ordering::Release);
        // SAFETY: the caller's contract.
        unsafe { self.read_refresh(swapchain) };
        tracing::info!(
            offered = ?domains[..n].iter().map(|d| d.as_raw()).collect::<Vec<_>>(),
            domain = domains[best].as_raw(),
            domain_id = ids[best],
            refresh_us = self.refresh.duration_ns.load(Ordering::Relaxed) / 1000,
            interval_us = self.refresh.interval_ns.load(Ordering::Relaxed) / 1000,
            "present stamps armed on the swapchain"
        );
        true
    }

    /// Whether the next present may ask for a stamp: the result queue has room.
    pub(crate) fn may_ask(&self) -> bool {
        self.outstanding() + 2 < QUEUE as usize
    }

    /// Presents asked for a stamp whose result has not been read.
    pub(crate) fn outstanding(&self) -> usize {
        self.asked.load(Ordering::Acquire)
    }

    /// A present that asked for a stamp was queued.
    pub(crate) fn note_asked(&self) {
        self.asked.fetch_add(1, Ordering::AcqRel);
    }

    /// # Safety
    /// `swapchain` is live and host-synchronised by the caller.
    unsafe fn read_refresh(&self, swapchain: vk::SwapchainKHR) {
        let mut props = TimingProps {
            s_type: vk::StructureType::from_raw(S_TIMING_PROPS),
            p_next: std::ptr::null_mut(),
            refresh_duration: 0,
            refresh_interval: 0,
        };
        // SAFETY: the caller's contract; `props` outlives the call.
        let r = unsafe {
            (self.get_timing_props)(self.device, swapchain, &mut props, std::ptr::null_mut())
        };
        if r == vk::Result::SUCCESS {
            self.refresh
                .duration_ns
                .store(props.refresh_duration, Ordering::Relaxed);
            self.refresh
                .interval_ns
                .store(props.refresh_interval, Ordering::Relaxed);
        }
    }

    /// Session clock minus the engine's clock `key` names, for `swapchain`. `None` where
    /// the driver cannot compare them.
    ///
    /// # Safety
    /// `swapchain` is live and host-synchronised by the caller.
    unsafe fn calibrate(&self, swapchain: vk::SwapchainKHR, key: (i32, u64, u32)) -> Option<i128> {
        let chained = CalibratedInfo {
            s_type: vk::StructureType::from_raw(S_CALIBRATED),
            p_next: std::ptr::null(),
            swapchain,
            present_stage: key.2,
            time_domain_id: key.1,
        };
        let mut info = vk::CalibratedTimestampInfoKHR::default()
            .time_domain(vk::TimeDomainKHR::from_raw(key.0));
        // Only the swapchain's own clocks take the chained struct.
        if key.0 == DOMAIN_STAGE_LOCAL || key.0 == DOMAIN_SWAPCHAIN_LOCAL {
            info.p_next = (&chained) as *const _ as *const c_void;
        }
        let before = pf_client_core::session::now_ns();
        // SAFETY: the caller's contract; `info` and `chained` outlive the call.
        let (stamps, _deviation) =
            unsafe { self.calibrated.get_calibrated_timestamps(&[info]) }.ok()?;
        let after = pf_client_core::session::now_ns();
        let engine = *stamps.first()?;
        let session = before + after.saturating_sub(before) / 2;
        tracing::debug!(
            domain = key.0,
            domain_id = key.1,
            stage = key.2,
            engine_ns = engine,
            session_ns = session,
            took_ns = after.saturating_sub(before),
            "engine clock compared with the session clock"
        );
        Some(i128::from(session) - i128::from(engine))
    }

    /// Read every finished result, as session-clock stamps. Empty when nothing finished
    /// or the driver refused the call.
    ///
    /// # Safety
    /// `swapchain` is live and host-synchronised by the caller.
    pub(crate) unsafe fn poll(
        &self,
        swapchain: vk::SwapchainKHR,
        offset: &mut Offset,
    ) -> Vec<Stamp> {
        let mut stages = [[StageTime::default(); 4]; BATCH];
        let mut timings: [PastTiming; BATCH] = std::array::from_fn(|i| PastTiming {
            s_type: vk::StructureType::from_raw(S_PAST_TIMING),
            p_next: std::ptr::null_mut(),
            present_id: 0,
            target_time: 0,
            present_stage_count: 4,
            p_present_stages: stages[i].as_mut_ptr(),
            time_domain: vk::TimeDomainKHR::from_raw(self.domain.load(Ordering::Acquire)),
            time_domain_id: self.domain_id.load(Ordering::Acquire),
            report_complete: vk::FALSE,
        });
        let info = PastInfo {
            s_type: vk::StructureType::from_raw(S_PAST_INFO),
            p_next: std::ptr::null(),
            flags: PAST_OUT_OF_ORDER,
            swapchain,
        };
        let mut props = PastProps {
            s_type: vk::StructureType::from_raw(S_PAST_PROPS),
            p_next: std::ptr::null_mut(),
            timing_properties_counter: 0,
            time_domains_counter: 0,
            presentation_timing_count: BATCH as u32,
            p_presentation_timings: timings.as_mut_ptr(),
        };
        // SAFETY: the caller's contract; every pointer names a local that outlives the call.
        let r = unsafe { (self.get_past)(self.device, &info, &mut props) };
        if r != vk::Result::SUCCESS && r != vk::Result::INCOMPLETE {
            return Vec::new();
        }
        let n = (props.presentation_timing_count as usize).min(BATCH);
        let mut out = Vec::with_capacity(n);
        for (t, stage_times) in timings.iter().zip(stages.iter()).take(n) {
            if t.report_complete != vk::TRUE {
                continue;
            }
            self.asked
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |a| {
                    Some(a.saturating_sub(1))
                })
                .ok();
            let count = (t.present_stage_count as usize).min(4);
            tracing::trace!(
                id = t.present_id,
                domain = t.time_domain.as_raw(),
                domain_id = t.time_domain_id,
                stages = ?stage_times[..count].iter().map(|s| (s.stage, s.time)).collect::<Vec<_>>(),
                "engine present result"
            );
            // The latest stage reported is the one nearest the eye.
            let shown = [STAGE_PIXEL_VISIBLE, STAGE_PIXEL_OUT]
                .iter()
                .find_map(|want| {
                    stage_times[..count]
                        .iter()
                        .find(|s| s.stage == *want && s.time != 0)
                        .copied()
                });
            let displayed_ns = shown.and_then(|s| {
                let key = (t.time_domain.as_raw(), t.time_domain_id, s.stage);
                if offset.age >= RECALIBRATE || offset.key != key {
                    // SAFETY: the caller's contract.
                    offset.ns = unsafe { self.calibrate(swapchain, key) }?;
                    offset.key = key;
                    offset.age = 0;
                }
                offset.age += 1;
                u64::try_from(i128::from(s.time) + offset.ns).ok()
            });
            out.push(Stamp {
                present_id: t.present_id,
                displayed_ns,
            });
        }
        if n > 0 {
            // SAFETY: the caller's contract.
            unsafe { self.read_refresh(swapchain) };
        }
        out
    }
}
