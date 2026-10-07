//! The Vulkan handoff types a decode lane shares with whatever owns the device: the
//! `VkDevice` as plain integers, plus the lock that serializes the one queue both sides
//! submit to.
//!
//! Split out of [`crate::video`] — which re-exports them, so desktop call sites are
//! unchanged — because Android reaches the PyroWave decoder without the `desktop` half of
//! this crate: there the device belongs to the JNI client's own present path, not to a
//! session presenter. std only; ash stays on the callers' side of the seam.

/// Mutex serializing `vkQueueSubmit` / `vkQueuePresentKHR` / `vkQueueWaitIdle`
/// on the queue the presenter shares with the decode lane.
///
/// The presenter has one graphics-family queue; the pump submits decode/CSC
/// to it from another thread. Unsynchronized `vkQueueSubmit` is intermittent
/// `VK_ERROR_DEVICE_LOST`. Lock/unlock stay for callbacks; [`QueueLock::guard`] is RAII.
pub struct QueueLock {
    locked: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
    /// The open present turn: the frame it is for, when it lapses, and whether the
    /// presenter has taken it up.
    turn: std::sync::Mutex<Option<(u64, std::time::Instant, bool)>>,
    turn_cv: std::sync::Condvar,
    /// A presenter on another thread ends turns ([`QueueLock::take_turns`]).
    takes_turns: std::sync::atomic::AtomicBool,
}

/// How long a decode submit holds back for a present under way. A present is one short
/// pass and its record, behind at most the pass before it; past this the presenter is
/// stuck, not slow.
const PRESENT_TURN: std::time::Duration = std::time::Duration::from_millis(4);

impl QueueLock {
    #[allow(clippy::new_without_default)]
    pub fn new() -> QueueLock {
        QueueLock {
            locked: std::sync::Mutex::new(false),
            cv: std::sync::Condvar::new(),
            turn: std::sync::Mutex::new(None),
            turn_cv: std::sync::Condvar::new(),
            takes_turns: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The presenter runs on its own thread and ends every turn it is offered. Without
    /// this no turn opens: a lane that presents on its decode thread has no one to wait for.
    pub fn take_turns(&self) {
        self.takes_turns
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Decode lane: frame `frame` is on its way to the presenter. Its present goes ahead
    /// of the next decode on the shared queue, or it waits a whole decode behind it.
    pub fn offer_present_turn(&self, frame: u64) {
        if self.takes_turns.load(std::sync::atomic::Ordering::Relaxed) {
            *self.turn.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((frame, std::time::Instant::now() + PRESENT_TURN, false));
        }
    }

    /// Presenter: it has frame `frame` in hand and is deciding what to do with it.
    pub fn begin_present_turn(&self, frame: u64) {
        if let Some((open, _, begun)) = self.turn.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            *begun |= *open <= frame;
        }
    }

    /// Presenter: every frame up to `frame` is submitted, dropped, or held for later.
    pub fn end_present_turn(&self, frame: u64) {
        let mut turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        if turn.is_some_and(|(open, ..)| open <= frame) {
            *turn = None;
            self.turn_cv.notify_all();
        }
    }

    /// Decode lane, before its submit: wait out a present turn the presenter has taken
    /// up. One it has not reached yet is forfeit: a slow presenter must not stall decode.
    pub fn yield_to_present(&self) {
        let mut turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        while let Some((_, until, begun)) = *turn {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if !begun || left.is_zero() {
                *turn = None;
                break;
            }
            turn = self
                .turn_cv
                .wait_timeout(turn, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    /// Block until the queue is free, then take it. Pair with [`QueueLock::unlock`], or use [`QueueLock::guard`].
    pub fn lock(&self) {
        let mut g = self
            .locked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while *g {
            g = self
                .cv
                .wait(g)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        *g = true;
    }

    pub fn unlock(&self) {
        let mut g = self
            .locked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *g = false;
        drop(g);
        self.cv.notify_one();
    }

    /// RAII form for Rust call sites (presenter submits/presents, Skia flushes).
    pub fn guard(&self) -> QueueLockGuard<'_> {
        self.lock();
        QueueLockGuard(self)
    }
}

/// Releases the [`QueueLock`] on drop.
pub struct QueueLockGuard<'a>(&'a QueueLock);

impl Drop for QueueLockGuard<'_> {
    fn drop(&mut self) {
        self.0.unlock();
    }
}

/// Selected presenter-device facts plus shared handles: the device the frame is
/// presented from, so decode runs where the pixels are sampled — the VkImage is
/// composited in place. Desktop fills this from the session presenter, Android
/// from the JNI client's own swapchain.
///
/// The bundle exists with or without Vulkan Video: `video_decode` gates that
/// rung while vendor and import facts keep answering either way. Plain integers:
/// this crate has no ash. Handles stay valid for the owner's lifetime, which
/// outlives every session pump.
#[derive(Clone)]
pub struct VulkanDecodeDevice {
    /// `PFN_vkGetInstanceProcAddr` from the loader. Decode lanes resolve everything else through it.
    pub get_instance_proc_addr: usize,
    pub instance: usize,
    pub physical_device: usize,
    pub device: usize,
    /// PCI vendor of the presenter's physical device (0x10DE NVIDIA, 0x1002 AMD,
    /// 0x8086 Intel) — drives [`Self::prefer_vulkan_first`].
    pub vendor_id: u32,
    /// Driver device-name string (logged on admission refusal).
    pub device_name: String,
    /// The presenter's graphics+present family.
    pub graphics_qf: u32,
    /// Video-decode family. May equal `graphics_qf`; the native rung must detect that (`submit_queues_collide`).
    pub decode_qf: u32,
    /// Raw `VkVideoCodecOperationFlagsKHR` the decode family advertises.
    pub decode_video_caps: u32,
    /// Extensions enabled at instance/device creation. Pyrowave replays these
    /// verbatim into pinned create-info, so they must match reality.
    pub instance_extensions: Vec<std::ffi::CString>,
    pub device_extensions: Vec<std::ffi::CString>,
    /// Features enabled at device creation (reported via `device_features`).
    pub f_sampler_ycbcr: bool,
    pub f_timeline_semaphore: bool,
    pub f_synchronization2: bool,
    /// Vulkan Video decode is usable (queue + extensions + features). The bundle
    /// exists without it; gate the Vulkan rung on this, not on `Some`.
    pub video_decode: bool,
    /// PyroWave decode is usable (Vulkan 1.3 + `shaderInt16` / 8-bit storage /
    /// subgroup size control). Gates the `CODEC_PYROWAVE` advertisement.
    pub pyrowave_decode: bool,
    /// Feature facts the pyrowave pinned create-info reconstruction mirrors
    /// so it can share this `VkDevice`.
    pub f_shader_int16: bool,
    pub f_storage_buffer8: bool,
    pub f_subgroup_size_control: bool,
    pub f_compute_full_subgroups: bool,
    pub f_shader_float16: bool,
    /// `VkPhysicalDeviceProperties::apiVersion` of the presenter's device.
    pub api_version: u32,
    /// Queue families the device was created with (one queue each, priority 1.0). Mirrored by reconstruction.
    pub queue_families: Vec<u32>,
    /// Presenter enabled win32 external-memory + keyed mutex. Always `false` off Windows.
    pub d3d11_import: bool,
    /// Presenter enabled Linux dma-buf import. Always `false` off Linux.
    pub dmabuf_import: bool,
    /// The presenter's VAAPI node decodes AV1 where its Vulkan does not
    /// (`video::vaapi_av1_decodable`). Always `false` off Linux.
    pub vaapi_av1_decode: bool,
    /// The presenter's VAAPI node decodes HEVC where its Vulkan does not
    /// (`video::vaapi_hevc_decodable`). Always `false` off Linux.
    pub vaapi_hevc_decode: bool,
    /// Presenter can import RGB10A2 and offers an HDR10 swapchain, so D3D11VA
    /// emits PQ pass-through instead of tonemapping to sRGB. Always `false` off Windows.
    pub d3d11_hdr10: bool,
    /// Presenter imports two-plane NV12 / P010 D3D11 textures for sampling and the vendor
    /// survives it, so D3D11VA copies into planar slots instead of running the video
    /// processor. Always `false` off Windows.
    pub d3d11_nv12: bool,
    pub d3d11_p010: bool,
    /// Adapter LUID when the driver reports one. D3D11VA builds on the same
    /// adapter so shared textures never cross GPUs. `None` off Windows or when unreported.
    pub adapter_luid: Option<[u8; 8]>,
    /// Shared queue lock. Presenter and decode lanes both take it around their submits.
    pub queue_lock: std::sync::Arc<QueueLock>,
}

/// PCI vendor ids `vendor_id` reports.
pub(crate) const VENDOR_NVIDIA: u32 = 0x10DE;
pub(crate) const VENDOR_AMD: u32 = 0x1002;

impl VulkanDecodeDevice {
    /// Should `auto` try Vulkan Video before VAAPI / D3D11VA on this device?
    ///
    /// NVIDIA and AMD: yes. NVIDIA has no usable VAAPI; VanGogh VAAPI chroma-fringes.
    /// This orders attempts only: later admission may skip the platform rung
    /// (NVIDIA VAAPI is barred from auto). Intel/unknown try the platform rung first.
    pub fn prefer_vulkan_first(&self) -> bool {
        self.vendor_id == VENDOR_NVIDIA || self.vendor_id == VENDOR_AMD
    }
}

#[cfg(test)]
mod tests {
    use super::QueueLock;
    use std::time::{Duration, Instant};

    /// A decode submit waits for a present the presenter has taken up, and only for
    /// that: an older frame's end does not release it, a frame the presenter has not
    /// reached is not waited for, and a stuck presenter costs one bounded turn.
    #[test]
    fn a_present_turn_holds_the_decode_submit_until_it_ends() {
        let lock = std::sync::Arc::new(QueueLock::new());
        lock.offer_present_turn(1);
        lock.begin_present_turn(1);
        let t = Instant::now();
        lock.yield_to_present();
        assert!(
            t.elapsed() < Duration::from_millis(2),
            "no presenter takes turns"
        );

        lock.take_turns();
        lock.offer_present_turn(1);
        let t = Instant::now();
        lock.yield_to_present();
        assert!(
            t.elapsed() < Duration::from_millis(2),
            "the presenter never took it up"
        );

        lock.offer_present_turn(2);
        lock.begin_present_turn(2);
        lock.end_present_turn(1);
        let ender = {
            let lock = lock.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1));
                lock.end_present_turn(2);
            })
        };
        let t = Instant::now();
        lock.yield_to_present();
        let waited = t.elapsed();
        ender.join().unwrap();
        assert!(
            waited >= Duration::from_micros(900),
            "frame 1's end released frame 2"
        );
        assert!(
            waited < super::PRESENT_TURN,
            "frame 2's end did not release it"
        );

        lock.offer_present_turn(3);
        lock.begin_present_turn(3);
        let t = Instant::now();
        lock.yield_to_present();
        assert!(
            t.elapsed() >= super::PRESENT_TURN,
            "a stuck presenter costs one turn"
        );
        let t = Instant::now();
        lock.yield_to_present();
        assert!(t.elapsed() < Duration::from_millis(2), "and only one");
    }
}
