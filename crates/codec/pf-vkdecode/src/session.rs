//! `VkVideoSessionKHR` + `VkVideoSessionParametersKHR` lifecycle for H.264.
//!
//! The session is created from the stream's coded extent and DPB depth. New
//! (sps-id / pps-id) pairs are Added with `updateSequenceCount` = previous + 1;
//! an existing id whose content changed, or a capacity overflow, Recreates the
//! parameters object (Vulkan cannot replace a stored set). A DPB or extent
//! resize Recreates the whole session.
//!
//! A driver may retain `pStdSPSs` / `pStdPPSs` and the embedded Std pointers
//! (`pOffsetForRefFrame`, `pScalingLists`) past create/update. [`StoredParams`]
//! holds the object and those backings in one value; Drop and Recreate destroy
//! the object first. Create-info plumbing stays function-local.
//!
//! [`ParamsLedger`] is the pure decision table; [`VideoSession`] is the Vulkan
//! half. [`RawVideoSession`] (create, memory bind, RESET arm, destroy) and
//! [`SetTable`] are shared with the H.265 and AV1 sessions. Pin: tests in this
//! file; the AV1 retention case is [`crate::session_av1`].

use std::rc::Rc;

use ash::vk;
use ash::vk::native as hh;
use cros_codecs::codec::h264::parser::Pps;
use cros_codecs::codec::h264::parser::Sps;
use tracing::debug;

use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::device::find_memory_type_preferring;
use crate::device::AllocError;
use crate::device::DecodeDevice;
use crate::params::pps_to_std;
use crate::params::sps_to_std;
use crate::params::OwnedStdPps;
use crate::params::OwnedStdSps;
use crate::params::ParamsError;
use crate::params_av1::ParamsAv1Error;
use crate::params_h265::H265ParamsError;

/// Headroom for id churn. Hosts emit one SPS + one PPS; overflow Recreates.
pub(crate) const MAX_STD_SPS: usize = 4;
pub(crate) const MAX_STD_PPS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsAction {
    /// Identical content already stored.
    Current,
    /// New id; one update (seq += 1) may add both sets.
    Add { add_sps: bool, add_pps: bool },
    /// No object yet, content changed under a stored id, or capacity would
    /// overflow.
    Recreate,
}

/// One kind of parameter set a parameters object stores, keyed by id AND
/// content. In-band sets re-parse every keyframe, so pointer identity is not
/// identity. Each codec ledger composes one table per set kind.
#[derive(Debug)]
pub(crate) struct SetTable<K, V> {
    entries: Vec<(K, V)>,
    cap: usize,
}

/// One set's standing against its table, in precedence order: a ledger acts on
/// the worst (`max`) verdict among its tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SetVerdict {
    /// Identical content stored under this id.
    Stored,
    /// Id not stored, and the table has room.
    New,
    /// Id not stored, and the table is at capacity.
    Full,
    /// Id stored with different content. Vulkan cannot replace a stored set.
    Conflict,
}

impl<K: PartialEq, V: PartialEq> SetTable<K, V> {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: Vec::new(),
            cap,
        }
    }

    pub(crate) fn verdict(&self, key: &K, value: &V) -> SetVerdict {
        match self.entries.iter().find(|(id, _)| id == key) {
            Some((_, stored)) if stored == value => SetVerdict::Stored,
            Some(_) => SetVerdict::Conflict,
            None if self.entries.len() >= self.cap => SetVerdict::Full,
            None => SetVerdict::New,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn push(&mut self, key: K, value: V) {
        self.entries.push((key, value));
    }

    /// Keep only this set: a recreated object holds just the current activation.
    pub(crate) fn reset_to(&mut self, key: K, value: V) {
        self.entries.clear();
        self.entries.push((key, value));
    }
}

/// Sets the parameters object holds ([`SetTable`] per kind).
#[derive(Debug)]
pub(crate) struct ParamsLedger {
    sps: SetTable<u8, Rc<Sps>>,
    pps: SetTable<(u8, u8), Rc<Pps>>,
    update_seq: u32,
}

impl Default for ParamsLedger {
    fn default() -> Self {
        Self {
            sps: SetTable::new(MAX_STD_SPS),
            pps: SetTable::new(MAX_STD_PPS),
            update_seq: 0,
        }
    }
}

impl ParamsLedger {
    /// Decide without mutating. Apply via [`Self::commit`].
    pub(crate) fn plan(&self, sps: &Rc<Sps>, pps: &Rc<Pps>) -> ParamsAction {
        // The first pair creates the object. One created with no set at all
        // faults Intel's Windows driver; FFmpeg never creates one either.
        if self.sps.is_empty() && self.pps.is_empty() {
            return ParamsAction::Recreate;
        }
        let sps_verdict = self.sps.verdict(&sps.seq_parameter_set_id, sps);
        let pps_verdict = self
            .pps
            .verdict(&(pps.seq_parameter_set_id, pps.pic_parameter_set_id), pps);
        match sps_verdict.max(pps_verdict) {
            SetVerdict::Stored => ParamsAction::Current,
            SetVerdict::New => ParamsAction::Add {
                add_sps: sps_verdict == SetVerdict::New,
                add_pps: pps_verdict == SetVerdict::New,
            },
            SetVerdict::Full | SetVerdict::Conflict => ParamsAction::Recreate,
        }
    }

    /// `Add` bumps the sequence by exactly one (one Vulkan call, even if both
    /// sets). `Recreate` keeps only this pair and resets the counter to 0.
    pub(crate) fn commit(&mut self, action: ParamsAction, sps: &Rc<Sps>, pps: &Rc<Pps>) {
        let sps_key = sps.seq_parameter_set_id;
        let pps_key = (pps.seq_parameter_set_id, pps.pic_parameter_set_id);
        match action {
            ParamsAction::Current => {}
            ParamsAction::Add { add_sps, add_pps } => {
                if add_sps {
                    self.sps.push(sps_key, Rc::clone(sps));
                }
                if add_pps {
                    self.pps.push(pps_key, Rc::clone(pps));
                }
                self.update_seq += 1;
            }
            ParamsAction::Recreate => {
                self.sps.reset_to(sps_key, Rc::clone(sps));
                self.pps.reset_to(pps_key, Rc::clone(pps));
                self.update_seq = 0;
            }
        }
    }

    pub(crate) fn next_update_seq(&self) -> u32 {
        self.update_seq + 1
    }
}

/// Create-time shape. A plan that disagrees rebuilds the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfig {
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    /// Profile change is a renegotiation.
    pub std_profile_idc: hh::StdVideoH264ProfileIdc,
    /// Device `maxLevelIdc` for this profile. Every SPS is clamped to this.
    pub max_level_idc: hh::StdVideoH264LevelIdc,
}

#[derive(Debug)]
pub(crate) enum SessionError {
    Vk(vk::Result),
    Params(ParamsError),
    /// H.265 Std conversion (this error type is shared across codecs).
    ParamsH265(H265ParamsError),
    /// AV1 Std conversion (this error type is shared across codecs).
    ParamsAv1(ParamsAv1Error),
    /// No matching memory type. Never a fallback.
    NoMemoryType {
        type_bits: u32,
        flags: vk::MemoryPropertyFlags,
    },
}

impl From<vk::Result> for SessionError {
    fn from(r: vk::Result) -> Self {
        SessionError::Vk(r)
    }
}

impl From<ParamsError> for SessionError {
    fn from(e: ParamsError) -> Self {
        SessionError::Params(e)
    }
}

impl From<H265ParamsError> for SessionError {
    fn from(e: H265ParamsError) -> Self {
        SessionError::ParamsH265(e)
    }
}

impl From<ParamsAv1Error> for SessionError {
    fn from(e: ParamsAv1Error) -> Self {
        SessionError::ParamsAv1(e)
    }
}

impl From<AllocError> for SessionError {
    fn from(e: AllocError) -> Self {
        match e {
            AllocError::Vk(r) => SessionError::Vk(r),
            AllocError::NoMemoryType { type_bits, flags } => {
                SessionError::NoMemoryType { type_bits, flags }
            }
        }
    }
}

/// [`bind_session_memory`] failure plus allocations the caller must adopt.
///
/// Vulkan has no partial-bind rollback. After `vkBindVideoSessionMemoryKHR`,
/// bound memory must not be freed while the session lives. Allocate-stage
/// failure frees here (`allocations` empty). Bind-stage failure returns them
/// unfreed so the caller destroys the session first ([`VideoSession`] field
/// order).
struct BindFailure {
    /// May be bound into the session — free only AFTER it is destroyed. Empty
    /// if the failure preceded any bind.
    allocations: Vec<vk::DeviceMemory>,
    error: SessionError,
}

/// Query and bind one video session's memory ([`RawVideoSession::create`]).
///
/// Bind-stage failure: adopt [`BindFailure::allocations`] and destroy the
/// session before freeing. Allocate-stage failure already freed; empty vec.
///
/// # Safety
///
/// `dev` wraps live handles ([`crate::DeviceHandles`] contract) and `session`
/// is a live, not-yet-memory-bound session created on it.
unsafe fn bind_session_memory(
    dev: &DecodeDevice,
    session: vk::VideoSessionKHR,
) -> Result<Vec<vk::DeviceMemory>, BindFailure> {
    let device = dev.ash();
    let get = dev
        .video_queue()
        .fp()
        .get_video_session_memory_requirements_khr;
    let mut count = 0u32;
    // SAFETY: live device + session (fn contract); null pointer is the
    // count-query form.
    let _ = unsafe { get(device.handle(), session, &mut count, std::ptr::null_mut()) };
    let mut reqs = vec![vk::VideoSessionMemoryRequirementsKHR::default(); count as usize];
    // SAFETY: as above with an array of the reported count.
    let _ = unsafe { get(device.handle(), session, &mut count, reqs.as_mut_ptr()) };

    let props = dev.memory_properties();
    let mut allocated: Vec<vk::DeviceMemory> = Vec::with_capacity(reqs.len());
    let mut binds = Vec::with_capacity(reqs.len());
    // Allocate-stage only: bind has not run, so none of these is session-bound.
    let unwind = |device: &ash::Device, allocated: &[vk::DeviceMemory]| {
        for &memory in allocated {
            // SAFETY: allocated here on this live device; bind has not run, so
            // none is session-bound.
            unsafe { device.free_memory(memory, None) };
        }
    };
    for rq in &reqs {
        let mr = rq.memory_requirements;
        // Prefer DEVICE_LOCAL; accept any type from `memoryTypeBits`. Some
        // drivers expose session bindings as host-visible-only.
        let type_index = match find_memory_type_preferring(
            &props,
            mr.memory_type_bits,
            vk::MemoryPropertyFlags::DEVICE_LOCAL,
        ) {
            Ok(index) => index,
            Err(e) => {
                unwind(device, &allocated);
                return Err(BindFailure {
                    allocations: Vec::new(),
                    error: e.into(),
                });
            }
        };
        let alloc = vk::MemoryAllocateInfo::default()
            .allocation_size(mr.size)
            .memory_type_index(type_index);
        // SAFETY: live device (fn contract).
        let memory = match unsafe { device.allocate_memory(&alloc, None) } {
            Ok(memory) => memory,
            Err(e) => {
                unwind(device, &allocated);
                return Err(BindFailure {
                    allocations: Vec::new(),
                    error: SessionError::Vk(e),
                });
            }
        };
        allocated.push(memory);
        binds.push(
            vk::BindVideoSessionMemoryInfoKHR::default()
                .memory_bind_index(rq.memory_bind_index)
                .memory(memory)
                .memory_offset(0)
                .memory_size(mr.size),
        );
    }
    // A session with no memory requirements is bound already. The bind call with
    // zero entries is invalid (VUID arraylength), and Intel's Windows driver takes it
    // as a poisoned session: the next parameters create faults inside the driver.
    if binds.is_empty() {
        return Ok(allocated);
    }
    // SAFETY: session + freshly allocated memory, one bind per requirement.
    let r = unsafe {
        (dev.video_queue().fp().bind_video_session_memory_khr)(
            device.handle(),
            session,
            binds.len() as u32,
            binds.as_ptr(),
        )
    };
    if r != vk::Result::SUCCESS {
        // Not freed: a partial bind may have attached some of these. Caller
        // destroys the session first.
        return Err(BindFailure {
            allocations: allocated,
            error: SessionError::Vk(r),
        });
    }
    Ok(allocated)
}

/// Parameters object and every Std set it was given, as one value.
///
/// A driver may retain embedded Std pointers past create/update. One value
/// makes "object whose backing is gone" unrepresentable. `std_sps` / `std_pps`
/// are the outer `pStdSPSs` / `pStdPPSs` arrays, assembled at their final
/// address. Moving a wrapper does not move the boxed blocks the driver kept.
struct StoredParams {
    object: vk::VideoSessionParametersKHR,
    /// One entry per set the object stores. The driver reads the boxed blocks.
    sps: Vec<OwnedStdSps>,
    pps: Vec<OwnedStdPps>,
    /// Outer `pStdSPSs` / `pStdPPSs` arrays, held for the object's life.
    /// Assembled at their final address so the pointer handed over never moves.
    std_sps: Vec<hh::StdVideoH264SequenceParameterSet>,
    std_pps: Vec<hh::StdVideoH264PictureParameterSet>,
}

impl StoredParams {
    /// Wrappers plus the contiguous Std arrays the create call reads.
    /// Built before the call so the arrays sit at the address they keep.
    /// Object handle is NULL until create succeeds.
    fn assemble(sps: Vec<OwnedStdSps>, pps: Vec<OwnedStdPps>) -> Self {
        // Copies of each wrapper's Std struct (`Copy`); embedded pointers
        // still address the wrappers' boxed blocks — both halves stay.
        let std_sps = sps.iter().map(|o| *o.std()).collect();
        let std_pps = pps.iter().map(|o| *o.std()).collect();
        Self {
            object: vk::VideoSessionParametersKHR::null(),
            sps,
            pps,
            std_sps,
            std_pps,
        }
    }

    /// Placeholder for a half-built session. Destroy ignores a NULL handle.
    fn none() -> Self {
        Self::assemble(Vec::new(), Vec::new())
    }

    /// Take sets an `Add` just stored. Only after that update succeeded.
    fn adopt(&mut self, sps: Option<OwnedStdSps>, pps: Option<OwnedStdPps>) {
        self.sps.extend(sps);
        self.pps.extend(pps);
    }
}

/// `VkVideoSessionKHR`, its bound memory, and the one-shot RESET arm. Each codec
/// session owns one ahead of its parameters, so the session is destroyed before
/// the Std backings drop.
pub struct RawVideoSession {
    device: ash::Device,
    video_queue: ash::khr::video_queue::Device,
    session: vk::VideoSessionKHR,
    memory: Vec<vk::DeviceMemory>,
    /// First coding scope records `VK_VIDEO_CODING_CONTROL_RESET_BIT_KHR`
    /// (spec initialization). Re-arm if that recording never reaches the queue.
    needs_reset: ResetArm,
}

impl RawVideoSession {
    /// Session for `profile` with its memory bound. The parameters object is
    /// the codec's.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        profile: DecodeProfile,
        max_coded_extent: vk::Extent2D,
        max_dpb_slots: u32,
        max_active_references: u32,
    ) -> Result<Self, SessionError> {
        let mut chain = profile.chain();
        let wired = chain.wire();
        let std_header_version = caps.std_header_version;
        let session_ci = vk::VideoSessionCreateInfoKHR::default()
            .queue_family_index(dev.decode_qf())
            .video_profile(wired)
            .picture_format(caps.output_format)
            .max_coded_extent(max_coded_extent)
            .reference_picture_format(caps.dpb_format)
            .max_dpb_slots(max_dpb_slots)
            .max_active_reference_pictures(max_active_references)
            .std_header_version(&std_header_version);
        let mut session = vk::VideoSessionKHR::null();
        // SAFETY: live device; `session_ci` roots locals (chain, header version)
        // that outlive the call.
        let r = unsafe {
            (dev.video_queue().fp().create_video_session_khr)(
                dev.ash().handle(),
                &session_ci,
                std::ptr::null(),
                &mut session,
            )
        };
        if r != vk::Result::SUCCESS {
            return Err(SessionError::Vk(r));
        }

        let mut built = Self {
            device: dev.ash().clone(),
            video_queue: dev.video_queue().clone(),
            session,
            memory: Vec::new(),
            needs_reset: ResetArm::armed(),
        };
        // SAFETY: fn contract; `session` is live and not yet memory-bound. On
        // error `built` drops the session and any bound memory.
        match unsafe { bind_session_memory(dev, session) } {
            Ok(memory) => built.memory = memory,
            // Park allocations on `built` so Drop destroys the session first.
            Err(failure) => {
                built.memory = failure.allocations;
                return Err(failure.error);
            }
        }
        Ok(built)
    }

    pub(crate) fn device(&self) -> &ash::Device {
        &self.device
    }

    pub(crate) fn video_queue(&self) -> &ash::khr::video_queue::Device {
        &self.video_queue
    }

    pub(crate) fn session(&self) -> vk::VideoSessionKHR {
        self.session
    }

    /// Whether the next coding scope must record the initialization RESET.
    /// True once per session. If that recording never reaches the queue, call
    /// [`Self::re_arm_reset`].
    pub(crate) fn take_needs_reset(&mut self) -> bool {
        self.needs_reset.take()
    }

    /// Undo a consumed [`Self::take_needs_reset`] whose RESET never queued.
    pub(crate) fn re_arm_reset(&mut self) {
        self.needs_reset.re_arm();
    }

    /// Destroy a parameters object of this session; NULL (no object yet) is a
    /// no-op. The spec lets destroy take NULL; AMD's Windows driver reads
    /// through it.
    ///
    /// # Safety
    ///
    /// `object` is NULL or this session's own, and no submitted decode still
    /// reads it.
    pub(crate) unsafe fn destroy_parameters(&self, object: vk::VideoSessionParametersKHR) {
        if object == vk::VideoSessionParametersKHR::null() {
            return;
        }
        // SAFETY: fn contract; live device.
        unsafe {
            (self.video_queue.fp().destroy_video_session_parameters_khr)(
                self.device.handle(),
                object,
                std::ptr::null(),
            );
        }
    }

    /// Install `fresh` in `slot`, destroy the object `slot` held, then drop its
    /// Std backings — the order a driver still holding the old pointers needs.
    ///
    /// # Safety
    ///
    /// The caller drained every in-flight decode, so no submitted work reads
    /// the old object.
    pub(crate) unsafe fn replace_parameters<P: ParametersObject>(&self, slot: &mut P, fresh: P) {
        let old = std::mem::replace(slot, fresh);
        // SAFETY: fn contract; `old` is this session's own (or NULL).
        unsafe { self.destroy_parameters(old.object()) };
        drop(old);
    }
}

impl Drop for RawVideoSession {
    fn drop(&mut self) {
        // SAFETY: this session's handles on a live device; the decoder drains GPU
        // work first and the codec session destroyed its parameters object.
        // Bound memory must not be freed while the session lives, so destroy the
        // session first ([`BindFailure`]).
        unsafe {
            (self.video_queue.fp().destroy_video_session_khr)(
                self.device.handle(),
                self.session,
                std::ptr::null(),
            );
            for memory in self.memory.drain(..) {
                self.device.free_memory(memory, None);
            }
        }
    }
}

/// What the shared decoder reads off a codec session.
pub trait CodecSession {
    fn raw_mut(&mut self) -> &mut RawVideoSession;
    fn parameters(&self) -> vk::VideoSessionParametersKHR;
    /// `maxActiveReferencePictures` the session was created with.
    fn max_active_references(&self) -> u32;
}

/// A codec's stored parameters: the object handle, NULL until one exists.
pub(crate) trait ParametersObject {
    fn object(&self) -> vk::VideoSessionParametersKHR;
}

impl ParametersObject for StoredParams {
    fn object(&self) -> vk::VideoSessionParametersKHR {
        self.object
    }
}

/// H.264 session: [`RawVideoSession`] plus the SPS/PPS parameters object.
pub struct VideoSession {
    raw: RawVideoSession,
    parameters: StoredParams,
    ledger: ParamsLedger,
    pub(crate) config: SessionConfig,
}

impl VideoSession {
    /// Session with no parameters object yet: the first
    /// [`Self::ensure_parameters`] creates it holding its sets.
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        config: SessionConfig,
    ) -> Result<Self, SessionError> {
        // SAFETY: fn contract.
        let raw = unsafe {
            RawVideoSession::create(
                dev,
                caps,
                DecodeProfile::H264(config.std_profile_idc),
                config.max_coded_extent,
                config.max_dpb_slots,
                config.max_active_references,
            )?
        };
        Ok(Self {
            raw,
            parameters: StoredParams::none(),
            ledger: ParamsLedger::default(),
            config,
        })
    }

    /// Parameters object holding exactly `sps` / `pps`, fused with the wrappers
    /// those Std pointers address.
    ///
    /// # Safety
    ///
    /// Live device + live session.
    unsafe fn create_parameters_object(
        &self,
        sps: Vec<OwnedStdSps>,
        pps: Vec<OwnedStdPps>,
    ) -> Result<StoredParams, SessionError> {
        // Assemble first so `pStdSPSs` / `pStdPPSs` address `stored`'s arrays.
        // Moving the `Vec` moves the handle, not the block the driver was given.
        let mut stored = StoredParams::assemble(sps, pps);
        let add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
            .std_sp_ss(&stored.std_sps)
            .std_pp_ss(&stored.std_pps);
        let mut h264 = vk::VideoDecodeH264SessionParametersCreateInfoKHR::default()
            .max_std_sps_count(MAX_STD_SPS as u32)
            .max_std_pps_count(MAX_STD_PPS as u32)
            .parameters_add_info(&add);
        let ci = vk::VideoSessionParametersCreateInfoKHR::default()
            .video_session(self.raw.session())
            .push_next(&mut h264);
        let mut object = vk::VideoSessionParametersKHR::null();
        // SAFETY: fn contract; `ci` roots locals outliving the call. Std arrays
        // and the boxed blocks their embedded pointers address live in `stored`.
        let r = unsafe {
            (self
                .raw
                .video_queue()
                .fp()
                .create_video_session_parameters_khr)(
                self.raw.device().handle(),
                &ci,
                std::ptr::null(),
                &mut object,
            )
        };
        if r != vk::Result::SUCCESS {
            return Err(SessionError::Vk(r));
        }
        stored.object = object;
        Ok(stored)
    }

    /// Ledger verdict without mutating. Consult before
    /// [`Self::ensure_parameters`] so a Recreate can drain in-flight work
    /// first — destroy must not race a submitted decode.
    pub(crate) fn parameters_action(&self, sps: &Rc<Sps>, pps: &Rc<Pps>) -> ParamsAction {
        self.ledger.plan(sps, pps)
    }

    /// Make the parameters object hold this AU's activated (SPS, PPS).
    ///
    /// # Safety
    ///
    /// Live device. On Recreate the caller has drained every in-flight decode
    /// (waited each output slot's newest submitted timeline). The old object
    /// is destroyed here; a still-executing decode reading it is
    /// use-after-free. `Current` / `Add` do not destroy an object a submitted
    /// decode can read.
    pub(crate) unsafe fn ensure_parameters(
        &mut self,
        sps: &Rc<Sps>,
        pps: &Rc<Pps>,
    ) -> Result<(), SessionError> {
        let action = self.ledger.plan(sps, pps);
        match action {
            ParamsAction::Current => Ok(()),
            ParamsAction::Add { add_sps, add_pps } => {
                let mut owned_sps = if add_sps {
                    Some(sps_to_std(sps)?)
                } else {
                    None
                };
                if let Some(s) = owned_sps.as_mut() {
                    s.clamp_level(self.config.max_level_idc);
                }
                let owned_pps = if add_pps {
                    Some(pps_to_std(pps)?)
                } else {
                    None
                };
                let sps_slice: &[hh::StdVideoH264SequenceParameterSet] = match &owned_sps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let pps_slice: &[hh::StdVideoH264PictureParameterSet] = match &owned_pps {
                    Some(o) => std::slice::from_ref(o.std()),
                    None => &[],
                };
                let mut add = vk::VideoDecodeH264SessionParametersAddInfoKHR::default()
                    .std_sp_ss(sps_slice)
                    .std_pp_ss(pps_slice);
                let update = vk::VideoSessionParametersUpdateInfoKHR::default()
                    .update_sequence_count(self.ledger.next_update_seq())
                    .push_next(&mut add);
                // SAFETY: live device + parameters object; `update` roots locals
                // (incl. the OwnedStd backings) outliving the call — and the
                // backings go on outliving it, adopted below.
                let r = unsafe {
                    (self
                        .raw
                        .video_queue()
                        .fp()
                        .update_video_session_parameters_khr)(
                        self.raw.device().handle(),
                        self.parameters.object,
                        &update,
                    )
                };
                if r != vk::Result::SUCCESS {
                    return Err(SessionError::Vk(r));
                }
                // Added sets belong to the object; adopt so the blocks outlive
                // this arm.
                self.parameters.adopt(owned_sps, owned_pps);
                self.ledger.commit(action, sps, pps);
                Ok(())
            }
            ParamsAction::Recreate => {
                debug!(
                    sps_id = sps.seq_parameter_set_id,
                    pps_id = pps.pic_parameter_set_id,
                    "fresh session parameters object"
                );
                let mut owned_sps = sps_to_std(sps)?;
                owned_sps.clamp_level(self.config.max_level_idc);
                let owned_pps = pps_to_std(pps)?;
                // SAFETY: fn contract — live device + live session. The wrappers
                // are MOVED IN and come back owned by the fresh object, so they
                // live as long as it does rather than merely across the call.
                let fresh =
                    unsafe { self.create_parameters_object(vec![owned_sps], vec![owned_pps])? };
                // SAFETY: the fn-level contract — the caller drained every
                // in-flight decode before a Recreate reached here (checked via
                // parameters_action).
                unsafe { self.raw.replace_parameters(&mut self.parameters, fresh) };
                self.ledger.commit(action, sps, pps);
                Ok(())
            }
        }
    }
}

impl CodecSession for VideoSession {
    fn raw_mut(&mut self) -> &mut RawVideoSession {
        &mut self.raw
    }

    fn parameters(&self) -> vk::VideoSessionParametersKHR {
        self.parameters.object
    }

    fn max_active_references(&self) -> u32 {
        self.config.max_active_references
    }
}

/// One-shot session-RESET arm, testable without a live session.
#[derive(Debug)]
pub(crate) struct ResetArm(bool);

impl ResetArm {
    pub(crate) fn armed() -> Self {
        Self(true)
    }

    pub(crate) fn take(&mut self) -> bool {
        std::mem::take(&mut self.0)
    }

    pub(crate) fn re_arm(&mut self) {
        self.0 = true;
    }
}

impl Drop for VideoSession {
    fn drop(&mut self) {
        // SAFETY: this session's own parameters object (NULL if it never
        // decoded); the decoder drains GPU work first. `raw` drops next and
        // destroys the session; the Std backings drop after that.
        unsafe { self.raw.destroy_parameters(self.parameters.object) };
    }
}

#[cfg(test)]
mod tests {
    use cros_codecs::codec::h264::parser::PpsBuilder;
    use cros_codecs::codec::h264::parser::Profile;
    use cros_codecs::codec::h264::parser::SpsBuilder;
    use pf_bitstream::h264::Level;

    use super::*;

    fn authored(sps_id: u8, pps_id: u8, qp: u8) -> (Rc<Sps>, Rc<Pps>) {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(sps_id)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .resolution(64, 64)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(pps_id)
            .pic_init_qp(qp)
            .build();
        (sps, pps)
    }

    /// An `Add` transfers the Std blocks to the live object.
    /// [`StoredParams::adopt`] is the transfer; update-call locals drop when
    /// `ensure_parameters` returns.
    #[test]
    fn an_added_set_keeps_its_blocks_alive_past_the_update_call() {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .resolution(64, 64)
            // Non-null inner pointer for the read-back below.
            .seq_scaling_matrix_present_flag(true)
            .build();
        let owned = sps_to_std(&sps).expect("converts");
        let lists = owned.std().pScalingLists;
        assert!(!lists.is_null(), "the fixture attaches scaling lists");

        let mut stored = StoredParams::none();
        stored.adopt(Some(owned), None);
        assert_eq!(
            (stored.sps.len(), stored.pps.len()),
            (1, 0),
            "the object took the set itself, not a borrow of it"
        );
        assert_eq!(
            stored.sps[0].std().pScalingLists,
            lists,
            "and it is the same block the driver was handed"
        );
        // SAFETY: `stored` owns the block — which is exactly the property here.
        let read_back = unsafe { (*lists).ScalingList4x4[0] };
        assert_eq!(read_back, [0; 16], "the fixture's lists, read back live");
    }

    /// Add hands `from_ref(o.std())` then moves the wrapper into
    /// [`StoredParams`]. Boxing keeps that address; unboxing would hand the
    /// driver a moved-from slot.
    #[test]
    fn an_added_set_keeps_the_address_the_update_call_was_given() {
        let (sps, pps) = authored(0, 0, 26);
        let owned_sps = sps_to_std(&sps).expect("converts");
        let owned_pps = pps_to_std(&pps).expect("converts");
        let handed_sps = std::ptr::from_ref(owned_sps.std());
        let handed_pps = std::ptr::from_ref(owned_pps.std());

        let mut stored = StoredParams::none();
        stored.adopt(Some(owned_sps), Some(owned_pps));
        assert_eq!(
            std::ptr::from_ref(stored.sps[0].std()),
            handed_sps,
            "the SPS address handed to Vulkan must be the one the object keeps"
        );
        assert_eq!(
            std::ptr::from_ref(stored.pps[0].std()),
            handed_pps,
            "and likewise the PPS"
        );
        // SAFETY: `stored` owns both structs — which is exactly the property here.
        let ids = unsafe {
            (
                (*handed_sps).seq_parameter_set_id,
                (*handed_pps).pic_parameter_set_id,
            )
        };
        assert_eq!(
            ids,
            (0, 0),
            "read back through the pointers the driver holds"
        );
    }

    /// Create-path outer pointers: [`StoredParams::assemble`] builds the
    /// contiguous Std arrays at the address they keep after the value moves.
    #[test]
    fn the_std_arrays_the_create_call_is_given_are_the_ones_the_object_keeps() {
        let sps = SpsBuilder::new()
            .seq_parameter_set_id(0)
            .profile_idc(Profile::Main)
            .level_idc(Level::L4)
            .frame_mbs_only_flag(true)
            .direct_8x8_inference_flag(true)
            .max_num_ref_frames(4)
            .resolution(64, 64)
            // Non-null embedded pointer so the copy-still-addresses-wrapper
            // assertion below can fail.
            .seq_scaling_matrix_present_flag(true)
            .build();
        let pps = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(0)
            .pic_init_qp(26)
            .build();
        let owned_sps = sps_to_std(&sps).expect("converts");
        let owned_pps = pps_to_std(&pps).expect("converts");

        let stored = StoredParams::assemble(vec![owned_sps], vec![owned_pps]);
        let (handed_sps, handed_pps) = (stored.std_sps.as_ptr(), stored.std_pps.as_ptr());
        // The move `create_parameters_object` ends with: `Ok(stored)`.
        let stored = std::hint::black_box(stored);
        assert_eq!((stored.std_sps.len(), stored.std_pps.len()), (1, 1));
        assert_eq!(
            (stored.std_sps.as_ptr(), stored.std_pps.as_ptr()),
            (handed_sps, handed_pps),
            "the arrays handed to Vulkan must be the ones the object keeps"
        );
        let lists = stored.std_sps[0].pScalingLists;
        assert!(!lists.is_null(), "the fixture attaches scaling lists");
        assert_eq!(lists, stored.sps[0].std().pScalingLists);
        // SAFETY: `stored` owns the block the copy points at — the property here.
        let read_back = unsafe { (*lists).ScalingList4x4[0] };
        assert_eq!(read_back, [0; 16], "the fixture's lists, read back live");
        assert_eq!(
            stored.std_pps[0].pic_parameter_set_id,
            stored.pps[0].std().pic_parameter_set_id,
            "the PPS copy is the wrapper's, field for field"
        );
    }

    #[test]
    fn a_reactivated_identical_pair_is_current_even_across_reparses() {
        let (sps_a, pps_a) = authored(0, 0, 26);
        // Parser re-parses in-band sets each keyframe: same content, new Rcs.
        let (sps_b, pps_b) = authored(0, 0, 26);
        assert!(!Rc::ptr_eq(&sps_a, &sps_b));

        let mut ledger = ParamsLedger::default();
        let first = ledger.plan(&sps_a, &pps_a);
        assert_eq!(first, ParamsAction::Recreate);
        ledger.commit(first, &sps_a, &pps_a);
        assert_eq!(ledger.plan(&sps_b, &pps_b), ParamsAction::Current);
    }

    #[test]
    fn a_new_pps_id_over_a_stored_sps_adds_only_the_pps() {
        let (sps, pps0) = authored(0, 0, 26);
        let pps1 = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(1)
            .pic_init_qp(26)
            .build();

        let mut ledger = ParamsLedger::default();
        let a = ledger.plan(&sps, &pps0);
        ledger.commit(a, &sps, &pps0);
        assert_eq!(
            ledger.plan(&sps, &pps1),
            ParamsAction::Add {
                add_sps: false,
                add_pps: true
            }
        );
    }

    #[test]
    fn changed_content_under_a_stored_id_recreates_and_resets_the_sequence() {
        let (sps, pps) = authored(0, 0, 26);
        let mut ledger = ParamsLedger::default();
        let a = ledger.plan(&sps, &pps);
        ledger.commit(a, &sps, &pps);
        let (_, pps1) = authored(0, 1, 26);
        let a = ledger.plan(&sps, &pps1);
        ledger.commit(a, &sps, &pps1);
        assert_eq!(ledger.next_update_seq(), 2, "one Add happened");

        // Same ids, different content: Vulkan cannot replace a stored set.
        let (sps2, pps2) = authored(0, 0, 30);
        let action = ledger.plan(&sps2, &pps2);
        assert_eq!(action, ParamsAction::Recreate);
        ledger.commit(action, &sps2, &pps2);
        assert_eq!(
            ledger.next_update_seq(),
            1,
            "a fresh object restarts its counter"
        );
        assert_eq!(ledger.plan(&sps2, &pps2), ParamsAction::Current);
    }

    #[test]
    fn capacity_overflow_recreates_with_just_the_current_pair() {
        let mut ledger = ParamsLedger::default();
        let (sps, first) = authored(0, 0, 26);
        let a = ledger.plan(&sps, &first);
        ledger.commit(a, &sps, &first);
        for pps_id in 1..MAX_STD_PPS as u8 {
            let pps = PpsBuilder::new(Rc::clone(&sps))
                .pic_parameter_set_id(pps_id)
                .pic_init_qp(26)
                .build();
            let a = ledger.plan(&sps, &pps);
            assert!(matches!(a, ParamsAction::Add { .. }));
            ledger.commit(a, &sps, &pps);
        }
        // The first PPS came with the object, not through an update.
        assert_eq!(ledger.next_update_seq() - 1, MAX_STD_PPS as u32 - 1);

        // One past capacity: Recreate; the evicted first PPS re-Adds later.
        let overflow = PpsBuilder::new(Rc::clone(&sps))
            .pic_parameter_set_id(MAX_STD_PPS as u8)
            .pic_init_qp(26)
            .build();
        let action = ledger.plan(&sps, &overflow);
        assert_eq!(action, ParamsAction::Recreate);
        ledger.commit(action, &sps, &overflow);
        assert_eq!(
            ledger.plan(&sps, &first),
            ParamsAction::Add {
                add_sps: false,
                add_pps: true
            },
            "sets evicted by a recreate re-add on next activation"
        );
    }

    /// Both codec ledgers act on the worst verdict: a conflict or a full table
    /// recreates, any new set adds, all stored is current.
    #[test]
    fn a_set_table_answers_stored_new_full_or_conflict() {
        let mut table: SetTable<u8, u32> = SetTable::new(2);
        assert_eq!(table.verdict(&0, &10), SetVerdict::New);
        table.push(0, 10);
        assert_eq!(table.verdict(&0, &10), SetVerdict::Stored);
        assert_eq!(
            table.verdict(&0, &11),
            SetVerdict::Conflict,
            "same id, new content"
        );
        table.push(1, 20);
        assert_eq!(table.verdict(&2, &30), SetVerdict::Full);
        assert_eq!(
            table.verdict(&1, &20),
            SetVerdict::Stored,
            "a stored set is current even in a full table"
        );
        assert!(
            SetVerdict::Stored < SetVerdict::New
                && SetVerdict::New < SetVerdict::Full
                && SetVerdict::Full < SetVerdict::Conflict
        );
        table.reset_to(2, 30);
        assert_eq!(
            table.verdict(&0, &10),
            SetVerdict::New,
            "a recreate keeps only the current set"
        );
        assert_eq!(table.verdict(&2, &30), SetVerdict::Stored);
    }

    #[test]
    fn the_reset_arm_fires_once_unless_the_failed_submit_re_arms_it() {
        let mut arm = ResetArm::armed();
        assert!(arm.take(), "a fresh session needs its RESET");
        assert!(
            !arm.take(),
            "consumed — the next scope must NOT reset again"
        );

        // Recorded RESET never reached the queue: re-arm so the next
        // successful recording carries it.
        arm.re_arm();
        assert!(arm.take());
        assert!(!arm.take());
    }

    #[test]
    fn update_sequence_counts_one_per_add_call_not_per_set() {
        let (sps, pps) = authored(0, 0, 26);
        let mut ledger = ParamsLedger::default();
        let a = ledger.plan(&sps, &pps);
        assert_eq!(
            a,
            ParamsAction::Recreate,
            "the first pair creates the object"
        );
        ledger.commit(a, &sps, &pps);
        assert_eq!(ledger.next_update_seq(), 1);
        // One call carries both new sets: the counter moves by exactly one.
        let (sps1, pps1) = authored(1, 1, 26);
        let a = ledger.plan(&sps1, &pps1);
        assert_eq!(
            a,
            ParamsAction::Add {
                add_sps: true,
                add_pps: true
            }
        );
        ledger.commit(a, &sps1, &pps1);
        assert_eq!(ledger.next_update_seq(), 2);
    }
}
