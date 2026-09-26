//! Native VAAPI decode: `pf-vaapi` plans one `AuPlan`; this module owns the libva
//! display, config, context, surface pool, submission, and DRM-PRIME export. Output
//! is [`DecodedImage::NativeDmabuf`]. H.264, H.265, and AV1 Profile 0 in NV12 or P010.
//! `libva.so.2` and `libva-drm.so.2` are dlopen'd — no build-time libva — and a missing
//! runtime refuses at [`NativeVaapiDecoder::new`] so the ladder in [`crate::video`]
//! can fall through.
//!
//! A VAAPI slot is not a surface. [`pf_vaapi::SlotMap::assign`] reuses the lowest
//! free slot, so binding by slot index would decode into the picture the presenter
//! still holds. Surfaces outnumber slots by [`pf_vaapi::config::PRESENTER_HEADROOM`];
//! a surface is free when no picture is bound to it AND no consumer holds it.
//! [`Session::acquire_target`] returns the decode target and the reference table from
//! one snapshot so the target cannot be a named reference.
//!
//! [`NativeVaapiDecoder::decode`] hands the pump at most one frame. Surplus waits in
//! `deliverable`, bounded by [`max_deliverable`]; [`flush`] drains the tail. Hosts
//! emit zero-reorder low-delay, so the queue is empty on the wire. Pin with
//! `PUNKTFUNK_DECODER=native-vaapi`. Evidence: `video::native_evidence` and the
//! ignored tests in this file.

use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::fd::OwnedFd;
use std::os::raw::c_int;
use std::os::raw::c_uint;
use std::os::raw::c_void;
use std::sync::mpsc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;

use crate::video::DecodeHealth;
use crate::video::DmabufFrame;
use crate::video::DmabufPlane;
use crate::video::DrmFrameGuard;
use crate::video::StreamFormat;
use crate::video_color::ColorDesc;

/// `PUNKTFUNK_DECODER=native-vaapi`. Skips the vendor order so a box that would
/// pick Vulkan first can still reach this rung; gating the pin would make the
/// missing `video::native_evidence` ungeneratable.
pub(crate) const DECODER_PIN: &str = "native-vaapi";

// The libva runtime lives in `pf-vaapi` so the host encoder binds the same one.
use pf_libva::Display;
use pf_libva::Libva;
use pf_libva::VaBufferId;
use pf_libva::VaConfigId;
use pf_libva::VaContextId;
use pf_libva::VaGenericValue;
use pf_libva::VaSurfaceAttrib;
use pf_libva::VaSurfaceId;
use pf_libva::VA_INVALID_ID;
use pf_libva::VA_PROGRESSIVE;
use pf_libva::VA_SURFACE_ATTRIB_PIXEL_FORMAT;
use pf_libva::VA_SURFACE_ATTRIB_SETTABLE;

/// Anything that sizes or configures a session. A change rebuilds the whole
/// thing — a half-rebuilt session hands out surfaces the pool does not have.
/// Depth and chroma belong here: an in-band SPS can flip 8-bit to 10-bit at
/// unchanged size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamShape {
    coded_width: u32,
    coded_height: u32,
    display_width: u32,
    display_height: u32,
    max_dpb_frames: usize,
    chroma_format_idc: u8,
    bit_depth: u8,
}

enum Planner {
    H264(Box<pf_vaapi::H264Planner>),
    H265(Box<pf_vaapi::H265Planner>),
    Av1(Box<pf_vaapi::Av1Planner>),
}

impl Planner {
    fn name(&self) -> &'static str {
        match self {
            Planner::H264(_) => "native-vaapi h264",
            Planner::H265(_) => "native-vaapi h265",
            Planner::Av1(_) => "native-vaapi av1",
        }
    }
}

/// Token a shipped frame hands back. A retired pool's index must not free a
/// surface in the new one.
#[derive(Debug, Clone, Copy)]
struct VaRelease {
    surface: usize,
    generation: u64,
}

/// Pins one shipped picture's surface until the presenter has waited its fence.
/// The presenter dups every imported fd; drop means the GPU is done reading.
pub struct VaFrameGuard {
    /// One `OwnedFd` per OBJECT, not per plane. Planes share objects; closing a
    /// shared fd twice would close an unrelated file.
    _fds: Vec<OwnedFd>,
    tx: mpsc::Sender<VaRelease>,
    release: VaRelease,
}

impl Drop for VaFrameGuard {
    fn drop(&mut self) {
        let _ = self.tx.send(self.release);
    }
}

/// Facts of the PICTURE, recorded at decode. A later display AU can disagree:
/// `keyframe` is the pump's re-anchor; `references_clean` corroborates a host
/// recovery anchor; `color` is the active SPS/VUI (HDR can flip in-band); AV1
/// `display` is per-frame (5.9.6) and must not take the newest crop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PictureFacts {
    keyframe: bool,
    /// Whole prediction chain was fully available — see [`DmabufFrame::references_clean`].
    references_clean: bool,
    color: ColorDesc,
    display: (u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingPicture {
    id: u64,
    surface: usize,
    facts: PictureFacts,
}

struct Session {
    shape: StreamShape,
    config: VaConfigId,
    context: VaContextId,
    surfaces: Vec<VaSurfaceId>,
    /// Cleared when the release token returns. Also covers a frame waiting in
    /// [`NativeVaapiDecoder::deliverable`]: the guard returns the surface either way.
    held: Vec<bool>,
    /// DPB slot → pool index, rebound at activation. `None` if the slot holds nothing.
    slot_surface: Vec<Option<usize>>,
    /// Separate from the slot binding: a non-reference picture leaves the DPB
    /// immediately but still owes an output.
    pending: Vec<PendingPicture>,
    slots: pf_vaapi::SlotMap,
    fourcc: u32,
    /// Bumped on every rebuild; stamped into release tokens.
    generation: u64,
}

impl Session {
    /// Free when unbound, not pending, and not held. Those three end at different
    /// times; display is usually last. `held` also covers a queued deliverable —
    /// [`trim_deliverable`] frees by the same guard-drop path as a presented frame.
    fn free_surface(&self) -> Option<usize> {
        (0..self.surfaces.len()).find(|i| {
            !self.held[*i]
                && !self.slot_surface.contains(&Some(*i))
                && !self.pending.iter().any(|p| p.surface == *i)
        })
    }

    /// Bindings follow the ledger, not a local `removed` list. `plan_to_va` has
    /// already applied this AU's removals.
    fn sync_slot_bindings(&mut self) {
        let mut live = vec![false; self.slot_surface.len()];
        for (slot, _) in self.slots.held() {
            if let Some(l) = live.get_mut(usize::from(slot)) {
                *l = true;
            }
        }
        for (slot, bound) in self.slot_surface.iter_mut().enumerate() {
            if !live[slot] {
                *bound = None;
            }
        }
    }

    /// Pre-removal DPB: references resolve against this. Unbound slots are
    /// [`VA_INVALID_ID`], never 0 — 0 is a plausible surface id.
    fn surface_table(&self) -> Vec<VaSurfaceId> {
        self.slot_surface
            .iter()
            .map(|b| b.map_or(VA_INVALID_ID, |i| self.surfaces[i]))
            .collect()
    }

    /// Target and reference table from one snapshot: a free surface is unbound, so
    /// it cannot appear in the table. Split them and the free list after removals
    /// offers a surface the pre-removal table still names.
    fn acquire_target(&self) -> Option<(usize, VaSurfaceId, Vec<VaSurfaceId>)> {
        let index = self.free_surface()?;
        Some((index, self.surfaces[index], self.surface_table()))
    }

    /// Creation-reverse order. Explicit: a `Drop` here could not reach the display.
    fn destroy(mut self, d: &Display) {
        // SAFETY: every handle was created on this display by `build` and is
        // destroyed exactly once — `destroy` consumes `self`. Surfaces are freed
        // after the context that referenced them, which is the order libva documents.
        unsafe {
            (d.va.destroy_context)(d.display, self.context);
            (d.va.destroy_surfaces)(
                d.display,
                self.surfaces.as_mut_ptr(),
                self.surfaces.len() as c_int,
            );
            (d.va.destroy_config)(d.display, self.config);
        }
    }

    fn build(d: &Display, codec: pf_vaapi::Codec, shape: StreamShape) -> Result<Session> {
        let profile = pf_vaapi::profile_for(codec, shape.chroma_format_idc, shape.bit_depth)
            .map_err(|e| anyhow!("{e}"))?;
        let rt_format = pf_vaapi::rt_format(shape.chroma_format_idc, shape.bit_depth)
            .map_err(|e| anyhow!("{e}"))?;
        let fourcc = match shape.bit_depth {
            8 => pf_vaapi::VA_FOURCC_NV12,
            10 => pf_vaapi::VA_FOURCC_P010,
            other => bail!("no VAAPI surface format for {other}-bit output"),
        };
        d.require_entrypoint(profile.value)?;

        // Named rather than default: on Main 10 the driver default is 8-bit, and
        // writing 10-bit samples into an 8-bit surface is silent narrowing.
        let mut attrib = VaConfigAttrib {
            kind: VA_CONFIG_ATTRIB_RT_FORMAT,
            value: rt_format,
        };
        let mut config: VaConfigId = VA_INVALID_ID;
        // SAFETY: a live display; `attrib` and `config` are locals that outlive the
        // call, and the count matches the slice length.
        d.va.check("vaCreateConfig", unsafe {
            (d.va.create_config)(
                d.display,
                profile.value,
                pf_vaapi::VA_ENTRYPOINT_VLD as c_int,
                (&mut attrib as *mut VaConfigAttrib).cast::<c_void>(),
                1,
                &mut config,
            )
        })?;

        // Every early return must destroy what was created; one closure, one unwind.
        let built = (|| -> Result<Session> {
            let count = pf_vaapi::surface_count(shape.max_dpb_frames);
            let mut surfaces: Vec<VaSurfaceId> = vec![VA_INVALID_ID; count];
            let mut pixel = VaSurfaceAttrib {
                kind: VA_SURFACE_ATTRIB_PIXEL_FORMAT,
                flags: VA_SURFACE_ATTRIB_SETTABLE,
                // Integer arm is i32; every fourcc here has the top bit clear.
                value: VaGenericValue::integer(fourcc as i32),
            };
            // Coded size: a display-sized pool is short by granule padding and smears rows.
            // SAFETY: live display; the surface array and the attribute outlive the
            // call and the counts match their lengths.
            d.va.check("vaCreateSurfaces", unsafe {
                (d.va.create_surfaces)(
                    d.display,
                    rt_format,
                    shape.coded_width,
                    shape.coded_height,
                    surfaces.as_mut_ptr(),
                    count as c_uint,
                    &mut pixel,
                    1,
                )
            })?;

            let mut context: VaContextId = VA_INVALID_ID;
            // SAFETY: live display and the config/surfaces just created; `context` is
            // a local that outlives the call. libva copies the surface array.
            let status = unsafe {
                (d.va.create_context)(
                    d.display,
                    config,
                    shape.coded_width as c_int,
                    shape.coded_height as c_int,
                    VA_PROGRESSIVE as c_int,
                    surfaces.as_mut_ptr(),
                    count as c_int,
                    &mut context,
                )
            };
            if let Err(e) = d.va.check("vaCreateContext", status) {
                // SAFETY: destroying the surfaces this closure just created, on the
                // unwind path, before they are moved into a Session.
                unsafe {
                    (d.va.destroy_surfaces)(
                        d.display,
                        surfaces.as_mut_ptr(),
                        surfaces.len() as c_int,
                    )
                };
                return Err(e);
            }

            let slots = pf_vaapi::SlotMap::new(shape.max_dpb_frames);
            let slot_count = slots.capacity();
            tracing::info!(
                node = %d.path,
                va = format_args!("{}.{}", d.version.0, d.version.1),
                profile = profile.name,
                coded = format_args!("{}x{}", shape.coded_width, shape.coded_height),
                display = format_args!("{}x{}", shape.display_width, shape.display_height),
                bit_depth = shape.bit_depth,
                surfaces = count,
                dpb_slots = slot_count,
                "native VAAPI decode session built"
            );
            Ok(Session {
                shape,
                config,
                context,
                surfaces,
                held: vec![false; count],
                slot_surface: vec![None; slot_count],
                pending: Vec::new(),
                slots,
                fourcc,
                generation: 0,
            })
        })();
        if built.is_err() {
            // SAFETY: destroying the config created above, on the unwind path; no
            // Session took ownership of it.
            unsafe { (d.va.destroy_config)(d.display, config) };
        }
        built
    }
}

/// 8 bytes, `{type, value}` at 0 and 4 (`pf-vaapi/layout-probe.c`).
#[repr(C)]
#[derive(Clone, Copy)]
struct VaConfigAttrib {
    kind: c_int,
    value: c_uint,
}

/// Measured. 0 is a real enumerator, not "left unset".
const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;

/// Yields [`pf_vaapi::VaDrmPrimeSurfaceDescriptor`]. The older `DRM_PRIME`
/// (0x2000_0000) hands back a different, smaller structure.
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: c_uint = 0x4000_0000;

const _: () = {
    assert!(size_of::<VaConfigAttrib>() == 8);
    assert!(std::mem::offset_of!(VaConfigAttrib, value) == 4);
};

/// Carry-over bound: the DPB's own depth. A bump can leave at most that many
/// frames behind the one that ships now. The queue inherits the DPB claim
/// ([`settle`] then [`ship`]), so it costs the pool at most one surface — unlike
/// Vulkan's `MAX_DELIVERABLE = 1`, where a delivered frame is extra residency.
/// Unbounded, a two-shown-per-AU stream grows until the pool is exhausted and
/// demotes the rung. H.264/H.265 decode one picture per AU so the bound is
/// defence; AV1 temporal units may decode several. Hosts emit zero-reorder, so
/// the queue is empty on the wire. See `the_queue_never_needs_a_surface_the_pool_does_not_have`.
fn max_deliverable(s: &Session) -> usize {
    s.shape.max_dpb_frames
}

/// First drop in full, then a heartbeat (~every 5 s at 60 fps). A drop-every-AU
/// shape would bury the log at frame rate.
const DROP_WARN_EVERY: u64 = 300;

/// Drop oldest first after this AU's own frame is taken off the front, so `cap`
/// bounds carry-over. Trimming before the take would invert display order inside
/// one AU. Returned frames drop via [`VaFrameGuard`]; the caller counts first.
fn trim_deliverable(
    queue: &mut std::collections::VecDeque<DmabufFrame>,
    cap: usize,
) -> Vec<DmabufFrame> {
    let mut dropped = Vec::new();
    while queue.len() > cap {
        match queue.pop_front() {
            Some(frame) => dropped.push(frame),
            // `len() > cap` means non-empty. Break, not `expect`: a bound of 0
            // on an empty queue must not panic in the decode path.
            None => break,
        }
    }
    dropped
}

pub(crate) struct NativeVaapiDecoder {
    display: Display,
    planner: Planner,
    session: Option<Session>,
    /// Surplus of a multi-picture bump, oldest first. Bounded by [`max_deliverable`].
    /// PRIME fds keep pixels alive across a rebuild; stale-generation tokens are
    /// counted, not applied.
    deliverable: std::collections::VecDeque<DmabufFrame>,
    health: DecodeHealth,
    recovery_request: bool,
    generation: u64,
    release_tx: mpsc::Sender<VaRelease>,
    release_rx: mpsc::Receiver<VaRelease>,
    /// Releases for a retired generation. Counted so the outstanding log stays honest.
    stale_releases: u64,
}

/// Does the node this rung would open for `presenter_vendor` decode AV1 Profile 0?
/// Opens and closes a display, so callers ask once and keep the answer.
pub(crate) fn av1_decodable(presenter_vendor: u32) -> bool {
    let answer = pf_vaapi::profile_for(pf_vaapi::Codec::Av1, 1, 8)
        .map_err(|e| anyhow!("{e}"))
        .and_then(|profile| {
            let va = Libva::load().context("libva")?;
            Display::open_for_vendor(va, Some(presenter_vendor))?.require_entrypoint(profile.value)
        });
    match &answer {
        Ok(()) => tracing::info!("the presenter's VAAPI node decodes AV1"),
        Err(e) => tracing::info!(
            reason = %format!("{e:#}"),
            "the presenter's VAAPI node does not decode AV1"
        ),
    }
    answer.is_ok()
}

impl NativeVaapiDecoder {
    /// Tests use render-node name order unless they set `PUNKTFUNK_VAAPI_DEVICE`.
    #[cfg(test)]
    pub(crate) fn new(codec: pf_vaapi::Codec, stream: StreamFormat) -> Result<NativeVaapiDecoder> {
        Self::new_for_presenter(codec, stream, None)
    }

    /// Probe [`StreamFormat`] on the presenter's GPU where possible. A first-AU
    /// refusal burns the demotion streak and skips the ladder's fall-through.
    pub(crate) fn new_for_presenter(
        codec: pf_vaapi::Codec,
        stream: StreamFormat,
        presenter_vendor: Option<u32>,
    ) -> Result<NativeVaapiDecoder> {
        let depth = stream.bit_depth;
        pf_vaapi::profile_for(codec, stream.chroma_format_idc, depth)
            .map_err(|e| anyhow!("{e}"))
            .context("the negotiated stream shape has no VAAPI decode profile")?;
        let va = Libva::load().context("libva")?;
        let display = Display::open_for_vendor(va, presenter_vendor)?;
        let planner = match codec {
            pf_vaapi::Codec::H264 => Planner::H264(Box::new(pf_vaapi::H264Planner::new())),
            pf_vaapi::Codec::H265 => Planner::H265(Box::new(pf_vaapi::H265Planner::new())),
            pf_vaapi::Codec::Av1 => Planner::Av1(Box::new(pf_vaapi::Av1Planner::new())),
        };
        let (release_tx, release_rx) = mpsc::channel();
        Ok(NativeVaapiDecoder {
            display,
            planner,
            session: None,
            deliverable: std::collections::VecDeque::new(),
            health: DecodeHealth {
                // No per-picture status query. `failed` is structurally 0;
                // `DecodeHealth::note` enforces that.
                status_queries: false,
                ..DecodeHealth::default()
            },
            recovery_request: false,
            generation: 0,
            release_tx,
            release_rx,
            stale_releases: 0,
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        self.planner.name()
    }

    pub(crate) fn health(&self) -> DecodeHealth {
        self.health
    }

    pub(crate) fn take_recovery_request(&mut self) -> bool {
        std::mem::take(&mut self.recovery_request)
    }

    /// The gate lifted on intra refresh marks: the planner's damaged-chain marks are stale
    /// (`CleanLedger::clear`).
    pub(crate) fn forgive_unclean(&mut self) {
        match &mut self.planner {
            Planner::H264(p) => p.forgive_unclean(),
            Planner::H265(p) => p.forgive_unclean(),
            Planner::Av1(p) => p.forgive_unclean(),
        }
    }

    fn drain_releases(&mut self) {
        drain_releases_into(
            &self.release_rx,
            self.session.as_mut(),
            &mut self.stale_releases,
        );
    }

    /// One AU in, at most one frame out. Surplus waits in [`Self::deliverable`].
    /// `Ok(None)` is not an error: buffering, concealment (re-anchor, not demotion),
    /// HEVC RASL skip (8.1.3 NOTE — must not re-anchor), or no session yet.
    pub(crate) fn decode(&mut self, au: &[u8]) -> Result<Option<DmabufFrame>> {
        self.drain_releases();
        let result = match self.planner {
            Planner::H264(_) => self.decode_h264(au),
            Planner::H265(_) => self.decode_h265(au),
            // AV1's "AU" is a temporal unit and may carry several frames.
            Planner::Av1(_) => self.decode_av1(au),
        };
        // One verdict, here. Damage is reported by the codec arm; a failure after a
        // clean plan is a refusal only — not also a clean AU that would reset the run.
        match &result {
            Ok((_, damaged)) => self.health.note(*damaged, false, 0),
            Err(_) => self.health.note(false, true, 0),
        }
        // Codec arms return exported frames only on `Ok`; an error never reaches the queue.
        let (fresh, damaged) = result?;
        if damaged {
            // Do not drain the queue. Shipping here reports `delivered` on a concealed
            // AU and zeros the demotion streak (`delivered || !concealed`).
            debug_assert!(
                fresh.is_empty(),
                "finish ships nothing from a damaged access unit"
            );
            return Ok(None);
        }
        self.deliverable.extend(fresh);
        Ok(self.take_deliverable())
    }

    /// This AU's frame off the front first so [`trim_deliverable`] bounds carry-over.
    fn take_deliverable(&mut self) -> Option<DmabufFrame> {
        let shipped = self.deliverable.pop_front();
        // No session means no pool; cap 0 is "no surfaces exist".
        let cap = self.session.as_ref().map_or(0, max_deliverable);
        // Pre-trim depth: after the trim this would be `cap` every time.
        let queued = self.deliverable.len();
        for frame in trim_deliverable(&mut self.deliverable, cap) {
            self.health.note_dropped();
            if self.health.dropped == 1 || self.health.dropped % DROP_WARN_EVERY == 0 {
                tracing::warn!(
                    queued,
                    cap,
                    dropped_total = self.health.dropped,
                    "native VAAPI: more display-ready frames than the pump can take — \
                     dropping the oldest so its surface is not held forever"
                );
            }
            drop(frame);
        }
        shipped
    }

    /// Drain the DPB in display order. The pump has no EOS; callers are [`Drop`]
    /// (release before the pool dies) and the conformance harness. AV1 shows at
    /// most one frame per temporal unit and has no planner flush. Export failure
    /// at teardown is logged, not panicked.
    pub(crate) fn flush(&mut self) -> Vec<DmabufFrame> {
        self.drain_releases();
        let mut out: Vec<DmabufFrame> = std::mem::take(&mut self.deliverable).into();
        let Self {
            display,
            planner,
            session,
            release_tx,
            ..
        } = self;
        let Some(s) = session.as_mut() else {
            return out;
        };
        // Two types (`h264::DpbUpdate` / `h265::DpbUpdate`), same shape.
        let (outputs, removed) = match planner {
            Planner::H264(p) => {
                let update = p.flush();
                (update.outputs, update.removed)
            }
            Planner::H265(p) => {
                let update = p.flush();
                (update.outputs, update.removed)
            }
            Planner::Av1(_) => (Vec::new(), Vec::new()),
        };
        let claimed = settle(s, &outputs, &removed);
        for picture in claimed {
            match ship(display, s, picture, release_tx) {
                Ok(frame) => out.push(frame),
                Err(e) => tracing::warn!(
                    error = %e,
                    id = picture.id,
                    "native VAAPI: flushed-picture export failed"
                ),
            }
        }
        // No conversion here; without this a resumed stream finds every slot taken.
        for id in &removed {
            s.slots.release(*id);
        }
        s.sync_slot_bindings();
        out
    }

    fn decode_h264(&mut self, au: &[u8]) -> Result<(Vec<DmabufFrame>, bool)> {
        let plan = match &mut self.planner {
            Planner::H264(p) => p.plan_au(au).map_err(|e| anyhow!("{e:?}"))?,
            _ => unreachable!("dispatched on the planner's own arm"),
        };
        let shape = shape_of(
            plan.picture.coded_width,
            plan.picture.coded_height,
            plan.picture.display_crop,
            plan.picture.max_dpb_frames,
            plan.picture.chroma_format_idc,
            8 + plan.picture.bit_depth_luma_minus8,
        )?;
        let damaged = plan.warnings.iter().any(pf_vaapi::is_integrity_warning);
        if !plan.warnings.is_empty() {
            tracing::debug!(warnings = ?plan.warnings, damaged, "native VAAPI plan warnings");
        }

        let Self {
            display, session, ..
        } = self;
        let s = ensure_session(
            display,
            session,
            pf_vaapi::Codec::H264,
            shape,
            &mut self.generation,
        )?;
        let (free, target, table) = s
            .acquire_target()
            .ok_or_else(|| anyhow!("surface pool exhausted ({} surfaces)", s.surfaces.len()))?;
        let converted = pf_vaapi::plan_to_va(&plan, au, &mut s.slots, &table, target)
            .map_err(|e| anyhow!("{e}"))?;

        // This picture's facts, not the later display AU's ([`PictureFacts`]).
        let facts = PictureFacts {
            keyframe: plan.picture.is_idr,
            references_clean: plan.picture.references_clean,
            color: colour_of(&plan.picture.colour),
            display: (s.shape.display_width, s.shape.display_height),
        };
        bind_setup(s, plan.dpb.stored, Some(free), facts);

        let iq = Some(as_ptr(&converted.iq_matrix));
        let slices = one_record_each(&converted.slices, &converted.slice_data)?;
        submit(
            display,
            s,
            target,
            as_ptr(&converted.pic_params),
            iq,
            &slices,
            au,
        )?;

        let frames = finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            &mut self.recovery_request,
            &self.release_tx,
        )?;
        Ok((frames, damaged))
    }

    fn decode_h265(&mut self, au: &[u8]) -> Result<(Vec<DmabufFrame>, bool)> {
        let plan = match &mut self.planner {
            Planner::H265(p) => match p.plan_au(au) {
                Ok(plan) => plan,
                // Spec 8.1.3 NOTE: skipped RASL is Ok, never a re-anchor.
                Err(pf_vaapi::PlanErrorH265::RaslSkipped { .. }) => return Ok((Vec::new(), false)),
                Err(e) => return Err(anyhow!("{e:?}")),
            },
            _ => unreachable!("dispatched on the planner's own arm"),
        };
        let shape = shape_of(
            plan.picture.coded_width,
            plan.picture.coded_height,
            plan.picture.display_crop,
            plan.picture.max_dpb_frames,
            plan.picture.chroma_format_idc,
            8 + plan.picture.bit_depth_luma_minus8,
        )?;
        let damaged = plan
            .warnings
            .iter()
            .any(pf_vaapi::is_integrity_warning_h265);
        if !plan.warnings.is_empty() {
            tracing::debug!(warnings = ?plan.warnings, damaged, "native VAAPI plan warnings");
        }

        let Self {
            display, session, ..
        } = self;
        let s = ensure_session(
            display,
            session,
            pf_vaapi::Codec::H265,
            shape,
            &mut self.generation,
        )?;
        let (free, target, table) = s
            .acquire_target()
            .ok_or_else(|| anyhow!("surface pool exhausted ({} surfaces)", s.surfaces.len()))?;
        let converted = pf_vaapi::plan_to_va_h265(&plan, au, &mut s.slots, &table, target)
            .map_err(|e| anyhow!("{e}"))?;

        let facts = PictureFacts {
            keyframe: plan.picture.is_idr,
            references_clean: plan.picture.references_clean,
            color: colour_of(&plan.picture.colour),
            display: (s.shape.display_width, s.shape.display_height),
        };
        bind_setup(s, plan.dpb.stored, Some(free), facts);

        // Only when the sequence codes scaling lists. An all-zero matrix on a
        // "use the defaults" stream dequantises every residual to zero.
        let iq = converted.iq_matrix.as_ref().map(as_ptr);
        let slices = one_record_each(&converted.slices, &converted.slice_data)?;
        submit(
            display,
            s,
            target,
            as_ptr(&converted.pic_params),
            iq,
            &slices,
            au,
        )?;

        let frames = finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            &mut self.recovery_request,
            &self.release_tx,
        )?;
        Ok((frames, damaged))
    }

    /// One temporal unit: decode every frame, present at most one. Hidden frames
    /// are references and must be decoded; they must not reach the presenter.
    /// Concealment is per unit: still convert and submit (slot map stays in sync;
    /// a later `show_existing_frame` must not export unwritten memory) but withhold
    /// display. Lost-ref slots get a live surface (`va_dec_av1.h:352`); lost tile
    /// groups bind nothing ([`Self::frame_av1`]).
    fn decode_av1(&mut self, au: &[u8]) -> Result<(Vec<DmabufFrame>, bool)> {
        let plans = match &mut self.planner {
            Planner::Av1(p) => p.plan_au(au).map_err(|e| anyhow!("{e}"))?,
            _ => unreachable!("dispatched on the planner's own arm"),
        };
        let mut shown: Vec<DmabufFrame> = Vec::new();
        let mut damaged_unit = false;
        for plan in &plans {
            let damaged = plan.warnings.iter().any(pf_vaapi::is_integrity_warning_av1);
            damaged_unit |= damaged;
            if !plan.warnings.is_empty() {
                tracing::debug!(warnings = ?plan.warnings, damaged, "native VAAPI AV1 plan warnings");
            }
            shown.extend(self.frame_av1(au, plan, damaged)?);
        }
        if damaged_unit {
            // A later frame of the same unit may have been the damaged one; the
            // guard returns already-exported surfaces.
            drop(shown);
            return Ok((Vec::new(), true));
        }
        Ok((shown, false))
    }

    /// Convert and submit. `damaged` gates display ([`finish`]) and turns a lost
    /// tile group on an already-damaged plan into concealment rather than an error.
    fn frame_av1(
        &mut self,
        au: &[u8],
        plan: &pf_vaapi::AuPlanAv1,
        damaged: bool,
    ) -> Result<Vec<DmabufFrame>> {
        // Re-display a picture an earlier hidden frame left in a reference slot.
        if plan.dpb.stored.is_none() {
            return self.show_existing_av1(plan, damaged);
        }
        let shape = shape_of_av1(plan);
        let Self {
            display, session, ..
        } = self;
        let s = ensure_session(
            display,
            session,
            pf_vaapi::Codec::Av1,
            shape,
            &mut self.generation,
        )?;
        // Per-frame render size, clamped to the coded picture: 5.9.6 has no upper
        // bound, and an unclamped crop would exceed the surface. Crop, not SAR —
        // same as the other native rungs, unlike libavcodec.
        let facts = PictureFacts {
            keyframe: plan.picture.is_key,
            references_clean: plan.picture.references_clean,
            color: colour_of(&plan.picture.colour),
            display: (
                plan.picture.render_width.min(plan.picture.upscaled_width),
                plan.picture.render_height.min(plan.picture.frame_height),
            ),
        };
        let (free, target, table) = s
            .acquire_target()
            .ok_or_else(|| anyhow!("surface pool exhausted ({} surfaces)", s.surfaces.len()))?;
        let converted = match pf_vaapi::plan_to_va_av1(plan, au, &mut s.slots, &table, target) {
            Ok(converted) => converted,
            Err(e) => {
                // Conversion assigns the setup slot before the tile walk; bind
                // nothing so a refusal cannot leave a wrong surface on that slot.
                bind_setup(s, plan.dpb.stored, None, facts);
                // Lost tiles on an already-damaged plan: conceal, do not demote.
                if damaged && e.lost_tiles() {
                    tracing::debug!(
                        error = %e,
                        id = plan.dpb.stored,
                        "native VAAPI AV1: concealed a truncated access unit"
                    );
                    finish(
                        display,
                        s,
                        &plan.dpb.outputs,
                        &plan.dpb.removed,
                        true,
                        &mut self.recovery_request,
                        &self.release_tx,
                    )?;
                    return Ok(Vec::new());
                }
                return Err(anyhow!("{e}"));
            }
        };

        // Ask the ledger: a no-refresh AV1 frame has already given the slot back,
        // so only `pending` keeps the surface off the free list.
        bind_setup(s, plan.dpb.stored, Some(free), facts);

        if converted.substituted_refs != 0 {
            tracing::debug!(
                slots = format_args!("{:#010b}", converted.substituted_refs),
                "native VAAPI AV1: concealed reference slot(s) with a live surface"
            );
        }
        let mut slices: Vec<SlicePair> = Vec::with_capacity(converted.tile_groups.len());
        for group in &converted.tile_groups {
            slices.push(SlicePair {
                params: group.tiles.as_ptr().cast::<c_void>(),
                record_size: size_of::<pf_vaapi::va_av1::VaSliceParameterBufferAV1>(),
                // Several records in one buffer; H.264/H.265 always pass 1.
                records: group.tiles.len(),
                data: group.data.clone(),
            });
        }
        submit(
            display,
            s,
            target,
            as_ptr(&converted.pic_params),
            None,
            &slices,
            au,
        )?;

        let frames = finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            &mut self.recovery_request,
            &self.release_tx,
        )?;

        // Never stored and never output: nothing else retires it, so drop pending
        // or the surface is held for the session and the pool exhausts.
        if converted.setup_slot.is_none() && !plan.dpb.outputs.contains(&converted.setup_id) {
            s.pending.retain(|p| p.id != converted.setup_id);
        }
        Ok(frames)
    }

    /// Export a surface already in [`Session::pending`]. No conversion. Facts
    /// were recorded at decode; the vendored vector never hits this path.
    fn show_existing_av1(
        &mut self,
        plan: &pf_vaapi::AuPlanAv1,
        damaged: bool,
    ) -> Result<Vec<DmabufFrame>> {
        let Self {
            display, session, ..
        } = self;
        // Concealed `MissingShowExisting` before any session existed.
        let Some(s) = session.as_mut() else {
            return Ok(Vec::new());
        };
        // A key `show_existing_frame` resets the store (AV1 7.20). No conversion
        // runs here, so this is the only place the ledger can follow those removals.
        for &id in &plan.dpb.removed {
            s.slots.release(id);
        }
        s.sync_slot_bindings();
        finish(
            display,
            s,
            &plan.dpb.outputs,
            &plan.dpb.removed,
            damaged,
            &mut self.recovery_request,
            &self.release_tx,
        )
    }
}

impl Drop for NativeVaapiDecoder {
    fn drop(&mut self) {
        // Only EOS this rung sees: release queue and DPB before the pool dies.
        let tail = self.flush().len();
        if tail > 0 {
            tracing::debug!(
                count = tail,
                "native VAAPI: released frames never shown at teardown"
            );
        }
        if self.stale_releases > 0 {
            tracing::debug!(
                count = self.stale_releases,
                "native VAAPI: releases for retired surface pools"
            );
        }
        if let Some(s) = self.session.take() {
            s.destroy(&self.display);
        }
    }
}

/// Pure bookkeeping so a retired-generation token can be tested without a `Display`.
fn drain_releases_into(
    rx: &mpsc::Receiver<VaRelease>,
    mut session: Option<&mut Session>,
    stale: &mut u64,
) {
    while let Ok(token) = rx.try_recv() {
        let Some(s) = session.as_deref_mut() else {
            *stale += 1;
            continue;
        };
        if token.generation != s.generation {
            // Retired pool. Freeing this index in the current pool would spare a live surface.
            *stale += 1;
            continue;
        }
        match s.held.get_mut(token.surface) {
            Some(h) => *h = false,
            None => *stale += 1,
        }
    }
}

/// `vaCreateBuffer` copies; the struct may die immediately after.
fn as_ptr<T>(value: &T) -> (*const c_void, usize) {
    ((value as *const T).cast::<c_void>(), size_of::<T>())
}

/// Active SPS/VUI, per frame, never latched: HDR can flip in-band at unchanged size.
fn colour_of(c: &pf_vaapi::ColourDescription) -> ColorDesc {
    ColorDesc {
        primaries: c.colour_primaries,
        transfer: c.transfer_characteristics,
        matrix: c.matrix_coefficients,
        full_range: c.video_full_range,
    }
}

fn shape_of(
    coded_width: u32,
    coded_height: u32,
    crop: pf_vaapi::DisplayCrop,
    max_dpb_frames: usize,
    chroma_format_idc: u8,
    bit_depth: u8,
) -> Result<StreamShape> {
    // Nothing downstream carries an origin; planes are sampled from (0,0). Refuse
    // rather than crop from the wrong corner.
    if crop.x != 0 || crop.y != 0 {
        bail!(
            "conformance window at ({}, {}) — this rung hands the surface over \
             uncropped and cannot express a non-zero origin",
            crop.x,
            crop.y
        );
    }
    Ok(StreamShape {
        coded_width,
        coded_height,
        display_width: crop.width,
        display_height: crop.height,
        max_dpb_frames,
        chroma_format_idc,
        bit_depth,
    })
}

/// Pool from the sequence maximum, not this frame: a mid-GOP resize must not
/// rebuild and drop every reference. Display fields are that maximum too; the
/// presenter gets [`finish`]'s per-frame region. DPB depth is `NUM_REF_FRAMES`.
fn shape_of_av1(plan: &pf_vaapi::AuPlanAv1) -> StreamShape {
    let coded_width = u32::from(plan.sequence.max_frame_width_minus_1) + 1;
    let coded_height = u32::from(plan.sequence.max_frame_height_minus_1) + 1;
    StreamShape {
        coded_width,
        coded_height,
        display_width: coded_width,
        display_height: coded_height,
        max_dpb_frames: pf_vaapi::AV1_MAX_DPB_FRAMES,
        chroma_format_idc: plan.picture.chroma_format_idc,
        bit_depth: plan.picture.bit_depth,
    }
}

fn ensure_session<'a>(
    d: &Display,
    slot: &'a mut Option<Session>,
    codec: pf_vaapi::Codec,
    shape: StreamShape,
    generation: &mut u64,
) -> Result<&'a mut Session> {
    if slot.as_ref().is_some_and(|s| s.shape == shape) {
        return Ok(slot.as_mut().expect("just matched"));
    }
    if let Some(old) = slot.take() {
        tracing::info!(
            from = ?old.shape,
            to = ?shape,
            "native VAAPI stream renegotiated — rebuilding the session"
        );
        // Destroy first so the old pool's memory is released. Consumer-held
        // surfaces are fine: exported PRIME fds keep the pixels alive.
        old.destroy(d);
    }
    // New generation: a stale token must not free an index in the replacement pool.
    *generation += 1;
    let mut built = Session::build(d, codec, shape)?;
    built.generation = *generation;
    Ok(slot.insert(built))
}

/// Bindings from the ledger first, then the setup picture by `slot_of`. A
/// store-and-evict in one plan holds no slot; `pending` keeps its surface off the
/// free list. `surface = None` on AV1 refusal: the conversion already reassigned
/// the slot, so leaving the previous surface would name a wrong picture. An
/// undecoded surface is never pushed to `pending`.
fn bind_setup(s: &mut Session, stored: Option<u64>, surface: Option<usize>, facts: PictureFacts) {
    s.sync_slot_bindings();
    let Some(id) = stored else { return };
    if let Some(slot) = s.slots.slot_of(id) {
        s.slot_surface[usize::from(slot)] = surface;
    }
    if let Some(surface) = surface {
        s.pending.push(PendingPicture { id, surface, facts });
    }
}

/// Parameter buffer plus the AU-coordinate bitstream its records address.
/// `vaRenderPicture` is what makes `slice_data_offset` relative to that data.
struct SlicePair {
    /// Borrowed from the converted plan; must outlive `submit`.
    params: *const c_void,
    /// Element size for `vaCreateBuffer`; not interchangeable with `records`.
    record_size: usize,
    /// 1 for H.264/H.265; a whole tile group for AV1.
    records: usize,
    data: std::ops::Range<usize>,
}

/// One record per buffer. `zip` would silently truncate a length mismatch into a
/// partial frame that decodes rather than fails.
fn one_record_each<T>(records: &[T], data: &[std::ops::Range<usize>]) -> Result<Vec<SlicePair>> {
    if records.len() != data.len() {
        bail!(
            "{} slice record(s) for {} data range(s) — the conversion's two halves \
             disagree",
            records.len(),
            data.len()
        );
    }
    Ok(records
        .iter()
        .zip(data)
        .map(|(record, range)| SlicePair {
            params: (record as *const T).cast::<c_void>(),
            record_size: size_of::<T>(),
            records: 1,
            data: range.clone(),
        })
        .collect())
}

/// Parameters in one `vaRenderPicture`, then interleaved slice-parameter/data
/// pairs — the order drivers are validated against.
fn submit(
    d: &Display,
    s: &Session,
    target: VaSurfaceId,
    pic: (*const c_void, usize),
    iq: Option<(*const c_void, usize)>,
    slices: &[SlicePair],
    au: &[u8],
) -> Result<()> {
    let mut params: Vec<VaBufferId> = Vec::with_capacity(2);
    let mut slice_buffers: Vec<VaBufferId> = Vec::with_capacity(slices.len() * 2);
    // A begun picture must be ended or later `vaBeginPicture` fails on a recoverable stream.
    let mut begun = false;
    // libva does not reclaim buffers at `vaEndPicture`.
    let result = (|| -> Result<()> {
        params.push(
            d.create_buffer(
                s.context,
                pf_vaapi::va::VA_PICTURE_PARAMETER_BUFFER_TYPE,
                pic.1,
                1,
                pic.0,
            )
            .context("picture parameter buffer")?,
        );
        if let Some((ptr, size)) = iq {
            params.push(
                d.create_buffer(
                    s.context,
                    pf_vaapi::va::VA_IQ_MATRIX_BUFFER_TYPE,
                    size,
                    1,
                    ptr,
                )
                .context("IQ matrix buffer")?,
            );
        }
        for (n, pair) in slices.iter().enumerate() {
            let range = pair.data.clone();
            let data = au.get(range.clone()).ok_or_else(|| {
                anyhow!(
                    "slice {n}: range {range:?} is outside a {}-byte access unit",
                    au.len()
                )
            })?;
            if pair.records == 0 {
                bail!("slice {n}: a parameter buffer with no records");
            }
            slice_buffers.push(
                d.create_buffer(
                    s.context,
                    pf_vaapi::va::VA_SLICE_PARAMETER_BUFFER_TYPE,
                    pair.record_size,
                    pair.records,
                    pair.params,
                )
                .with_context(|| format!("slice {n} parameter buffer"))?,
            );
            slice_buffers.push(
                d.create_buffer(
                    s.context,
                    pf_vaapi::va::VA_SLICE_DATA_BUFFER_TYPE,
                    data.len(),
                    1,
                    data.as_ptr().cast::<c_void>(),
                )
                .with_context(|| format!("slice {n} data buffer"))?,
            );
        }

        // SAFETY: a live display, context and target surface; both buffer arrays are
        // locals that outlive their calls and their counts match their lengths.
        unsafe {
            d.va.check(
                "vaBeginPicture",
                (d.va.begin_picture)(d.display, s.context, target),
            )?;
            begun = true;
            d.va.check(
                "vaRenderPicture(parameters)",
                (d.va.render_picture)(
                    d.display,
                    s.context,
                    params.as_mut_ptr(),
                    params.len() as c_int,
                ),
            )?;
            d.va.check(
                "vaRenderPicture(slices)",
                (d.va.render_picture)(
                    d.display,
                    s.context,
                    slice_buffers.as_mut_ptr(),
                    slice_buffers.len() as c_int,
                ),
            )?;
            begun = false;
            d.va.check("vaEndPicture", (d.va.end_picture)(d.display, s.context))?;
        }
        Ok(())
    })();
    if begun {
        // SAFETY: a live display and context with a picture open; the status is
        // deliberately discarded — the real failure is `result`, and reporting this
        // one would replace the cause with its consequence.
        unsafe { (d.va.end_picture)(d.display, s.context) };
    }
    d.destroy_buffers(&params);
    d.destroy_buffers(&slice_buffers);
    result
}

/// Claim every output in display order, then retire `removed` even if never shown.
/// Claim before retire: bumping outputs and removes in the same AU. A claimed
/// picture leaves `pending`, so ship or drop in the same breath or the surface
/// has only a slot it may no longer hold. A missing pending id is a display-order
/// gap, not an error.
fn settle(s: &mut Session, outputs: &[u64], removed: &[u64]) -> Vec<PendingPicture> {
    let mut claimed = Vec::with_capacity(outputs.len());
    for id in outputs {
        match s.pending.iter().position(|p| p.id == *id) {
            Some(index) => claimed.push(s.pending.remove(index)),
            None => tracing::trace!(id, "output id without a pending picture"),
        }
    }
    for id in removed {
        s.pending.retain(|p| p.id != *id);
    }
    claimed
}

/// Export and take the consumer's hold. Flush uses the same walk as [`finish`].
fn ship(
    d: &Display,
    s: &mut Session,
    picture: PendingPicture,
    tx: &mpsc::Sender<VaRelease>,
) -> Result<DmabufFrame> {
    let surface_index = picture.surface;
    let surface = s.surfaces[surface_index];

    // Fds are owned from the successful export; later refusals close them by drop.
    let (exported, fds, sync_fds) = export(d, surface)?;
    if exported.fourcc != s.fourcc {
        // Driver silently substituted a different layout than the pool was created with.
        bail!(
            "surface exported fourcc {:#010x}, the pool was created as {:#010x}",
            exported.fourcc,
            s.fourcc
        );
    }
    if exported.planes.len() < 2 {
        bail!(
            "a two-plane surface exported {} plane(s) — the chroma is missing",
            exported.planes.len()
        );
    }

    s.held[surface_index] = true;
    let planes = exported
        .planes
        .iter()
        .map(|p| DmabufPlane {
            fd: p.fd,
            offset: p.offset,
            stride: p.stride,
        })
        .collect();
    Ok(DmabufFrame {
        // Visible picture; the coded export extent below carries the padding
        // the importer crops off.
        width: picture.facts.display.0,
        height: picture.facts.display.1,
        coded_width: exported.width,
        coded_height: exported.height,
        fourcc: exported.fourcc,
        modifier: exported.modifier,
        planes,
        color: picture.facts.color,
        keyframe: picture.facts.keyframe,
        references_clean: picture.facts.references_clean,
        sync_fds,
        pool_key: (s.generation << 32) | surface_index as u64,
        guard: DrmFrameGuard(VaFrameGuard {
            _fds: fds,
            tx: tx.clone(),
            release: VaRelease {
                surface: surface_index,
                generation: s.generation,
            },
        }),
    })
}

/// [`settle`] then [`ship`]. A mid-loop `?` drops already-exported frames via their
/// guards; pictures not yet reached already left `pending` and return to the free list.
fn finish(
    d: &Display,
    s: &mut Session,
    outputs: &[u64],
    removed: &[u64],
    damaged: bool,
    recovery_request: &mut bool,
    tx: &mpsc::Sender<VaRelease>,
) -> Result<Vec<DmabufFrame>> {
    let claimed = settle(s, outputs, removed);
    // Substitute reference: do not paint it. Re-anchor; surfaces were never held.
    if damaged {
        *recovery_request = true;
        return Ok(Vec::new());
    }
    let mut frames = Vec::with_capacity(claimed.len());
    for picture in claimed {
        frames.push(ship(d, s, picture, tx)?);
    }
    Ok(frames)
}

/// How the importer learns the decode is done. The kernel fence on the surface's
/// dma-buf is the default; `PUNKTFUNK_VAAPI_EXPLICIT_SYNC=0`, or a kernel without
/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`, falls back to `vaSyncSurface` on the pump.
/// Process-wide: a kernel fact, not a per-decoder one.
static SYNC_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(SYNC_UNDECIDED);
const SYNC_UNDECIDED: u8 = 0;
const SYNC_EXPLICIT: u8 = 1;
const SYNC_CPU: u8 = 2;

fn sync_mode() -> u8 {
    use std::sync::atomic::Ordering;
    match SYNC_MODE.load(Ordering::Relaxed) {
        SYNC_UNDECIDED => {
            let off = std::env::var_os("PUNKTFUNK_VAAPI_EXPLICIT_SYNC").is_some_and(|v| v == "0");
            let mode = if off { SYNC_CPU } else { SYNC_EXPLICIT };
            SYNC_MODE.store(mode, Ordering::Relaxed);
            mode
        }
        mode => mode,
    }
}

/// Export, then hand out the decode's write fence as sync_files (one per object).
/// The pump never waits: the presenter waits them on the GPU. Without the ioctl the
/// old CPU wait runs here and `sync_fds` is empty. Fds are owned from success so
/// later refusals close them. One fd per object, even when planes share it.
fn export(
    d: &Display,
    surface: VaSurfaceId,
) -> Result<(pf_vaapi::ExportedSurface, Vec<OwnedFd>, Vec<OwnedFd>)> {
    if sync_mode() == SYNC_CPU {
        // SAFETY: a live display and a surface from its own pool.
        d.va.check("vaSyncSurface", unsafe {
            (d.va.sync_surface)(d.display, surface)
        })?;
    }
    let (exported, fds) = export_handle(d, surface)?;
    if sync_mode() == SYNC_CPU {
        return Ok((exported, fds, Vec::new()));
    }
    let mut sync_fds = Vec::with_capacity(fds.len());
    for fd in &fds {
        match pf_zerocopy::dmabuf_fence::export_sync_file(fd.as_raw_fd()) {
            Ok(Some(sync)) => sync_fds.push(sync),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "native VAAPI: the kernel exports no dma-buf sync_file — the pump \
                     waits each decode on the CPU instead"
                );
                SYNC_MODE.store(SYNC_CPU, std::sync::atomic::Ordering::Relaxed);
                // SAFETY: a live display and a surface from its own pool.
                d.va.check("vaSyncSurface", unsafe {
                    (d.va.sync_surface)(d.display, surface)
                })?;
                return Ok((exported, fds, Vec::new()));
            }
        }
    }
    Ok((exported, fds, sync_fds))
}

fn export_handle(
    d: &Display,
    surface: VaSurfaceId,
) -> Result<(pf_vaapi::ExportedSurface, Vec<OwnedFd>)> {
    let mut desc = pf_vaapi::VaDrmPrimeSurfaceDescriptor::zeroed();
    // SAFETY: a live display and surface; `desc` is a local of exactly the layout
    // `VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2` writes (measured by
    // `pf-vaapi/layout-probe.c` and compile-asserted), and it outlives the call.
    d.va.check("vaExportSurfaceHandle", unsafe {
        (d.va.export_surface_handle)(
            d.display,
            surface,
            VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
            pf_vaapi::VA_EXPORT_SURFACE_SEPARATE_LAYERS | pf_vaapi::VA_EXPORT_SURFACE_READ_ONLY,
            (&mut desc as *mut pf_vaapi::VaDrmPrimeSurfaceDescriptor).cast::<c_void>(),
        )
    })?;

    match pf_vaapi::flatten(&desc) {
        Ok(exported) => {
            let fds = exported
                .object_fds
                .iter()
                // SAFETY: each fd came out of a successful `vaExportSurfaceHandle`
                // and is owned by this process exactly once. `flatten` lists one per
                // OBJECT, so no fd is wrapped twice even where planes share it.
                .map(|fd| unsafe { OwnedFd::from_raw_fd(*fd) })
                .collect();
            Ok((exported, fds))
        }
        Err(e) => {
            // Export succeeded: the fds are ours even if flatten failed. Sweep every
            // slot — a bogus `num_objects` is why that count cannot bound the close.
            for object in &desc.objects {
                if object.fd >= 0 {
                    // SAFETY: an fd this process owns from the successful export;
                    // wrapping it in an `OwnedFd` that immediately drops closes it once.
                    drop(unsafe { OwnedFd::from_raw_fd(object.fd) });
                }
            }
            Err(anyhow!("{e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bookkeeping only; libva handles are never used. Small so exhaustion is reachable.
    fn session(surfaces: usize, slots: usize) -> Session {
        Session {
            shape: StreamShape {
                coded_width: 64,
                coded_height: 64,
                display_width: 64,
                display_height: 64,
                max_dpb_frames: slots - 1,
                chroma_format_idc: 1,
                bit_depth: 8,
            },
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            surfaces: (0..surfaces as u32).map(|i| 0x100 + i).collect(),
            held: vec![false; surfaces],
            slot_surface: vec![None; slots],
            pending: Vec::new(),
            slots: pf_vaapi::SlotMap::new(slots - 1),
            fourcc: pf_vaapi::VA_FOURCC_NV12,
            generation: 1,
        }
    }

    /// Tests that care about facts build their own so this default cannot pass them.
    const PLAIN: PictureFacts = PictureFacts {
        keyframe: false,
        references_clean: false,
        color: ColorDesc {
            primaries: 1,
            transfer: 1,
            matrix: 1,
            full_range: false,
        },
        display: (64, 64),
    };

    fn pending(id: u64, surface: usize) -> PendingPicture {
        PendingPicture {
            id,
            surface,
            facts: PLAIN,
        }
    }

    #[test]
    fn a_surface_is_free_only_when_no_slot_no_output_and_no_consumer_claims_it() {
        let mut s = session(4, 3);
        assert_eq!(
            s.free_surface(),
            Some(0),
            "a fresh pool starts at the front"
        );

        // Slot, pending, held (including a queued deliverable).
        s.slot_surface[0] = Some(0);
        s.pending.push(pending(7, 1));
        s.held[2] = true;
        assert_eq!(
            s.free_surface(),
            Some(3),
            "the first three are each claimed a different way"
        );

        s.held[3] = true;
        s.slot_surface[1] = Some(1);
        assert_eq!(
            s.free_surface(),
            None,
            "an exhausted pool must say so rather than hand out a claimed surface"
        );

        // Slot-bound and pending: clearing only pending is not enough.
        s.pending.clear();
        assert_eq!(s.free_surface(), None);
        s.slot_surface[1] = None;
        assert_eq!(s.free_surface(), Some(1));
    }

    #[test]
    fn a_release_from_a_retired_pool_never_frees_a_surface_in_the_new_one() {
        let mut s = session(4, 3);
        s.held[2] = true;
        let (tx, rx) = mpsc::channel();
        let mut stale = 0u64;

        // Index 2 exists in both pools; only generation says it is a different surface.
        tx.send(VaRelease {
            surface: 2,
            generation: 0,
        })
        .expect("the receiver is alive");
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert!(
            s.held[2],
            "a stale generation must not clear a hold in the CURRENT pool"
        );
        assert_eq!(stale, 1, "and it must be counted, not silent");

        tx.send(VaRelease {
            surface: 2,
            generation: 1,
        })
        .expect("the receiver is alive");
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert!(!s.held[2]);
        assert_eq!(stale, 1);

        tx.send(VaRelease {
            surface: 99,
            generation: 1,
        })
        .expect("the receiver is alive");
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert_eq!(stale, 2);
    }

    #[test]
    fn syncing_bindings_drops_the_slots_the_ledger_no_longer_holds() {
        let mut s = session(4, 3);
        s.slot_surface[0] = Some(0);
        s.slot_surface[1] = Some(1);
        s.slots.assign(11).expect("a free slot");
        s.sync_slot_bindings();
        assert_eq!(
            s.slot_surface,
            vec![Some(0), None, None],
            "slot 0 is held by picture 11; slot 1's picture is gone"
        );
    }

    /// Sweep: a free surface is unbound, so it cannot appear in the table. `held`
    /// and `pending` only remove candidates; a future claim that added one back
    /// would fail here.
    #[test]
    fn the_decode_target_can_never_be_a_surface_the_reference_table_names() {
        const SURFACES: usize = 4;
        const SLOTS: usize = 3;
        let choices: Vec<Option<usize>> = std::iter::once(None)
            .chain((0..SURFACES).map(Some))
            .collect();

        let (mut states, mut with_a_target, mut exhausted) = (0usize, 0usize, 0usize);
        for a in &choices {
            for b in &choices {
                for c in &choices {
                    let bound = [*a, *b, *c];
                    // Two slots on one surface is unreachable; skip it.
                    let mut distinct: Vec<usize> = bound.iter().flatten().copied().collect();
                    let claimed = distinct.len();
                    distinct.sort_unstable();
                    distinct.dedup();
                    if distinct.len() != claimed {
                        continue;
                    }
                    for held_mask in 0..(1u32 << SURFACES) {
                        for pending_mask in 0..(1u32 << SURFACES) {
                            let mut s = session(SURFACES, SLOTS);
                            s.slot_surface = bound.to_vec();
                            s.held = (0..SURFACES).map(|i| held_mask >> i & 1 == 1).collect();
                            s.pending = (0..SURFACES)
                                .filter(|i| pending_mask >> i & 1 == 1)
                                .map(|i| pending(100 + i as u64, i))
                                .collect();
                            states += 1;

                            let Some((index, target, table)) = s.acquire_target() else {
                                exhausted += 1;
                                continue;
                            };
                            with_a_target += 1;
                            assert_eq!(
                                target, s.surfaces[index],
                                "the target must be the pool's surface at the index it \
                                 returned, or the caller binds one and submits another"
                            );
                            assert_eq!(table.len(), SLOTS, "one table entry per slot");
                            assert!(
                                !table.contains(&target),
                                "bindings {bound:?}, held {held_mask:#06b}, pending \
                                 {pending_mask:#06b}: the decode target {target:#x} is \
                                 in the reference table {table:x?} — every submission \
                                 built from that pair decodes into a surface it may be \
                                 predicting from"
                            );
                        }
                    }
                }
            }
        }
        assert!(states > 1000, "only {states} states swept");
        assert!(with_a_target > 0 && exhausted > 0);
    }

    /// Split target and table: the post-removal free list offers a surface the
    /// pre-removal table still names.
    #[test]
    fn taking_the_free_surface_after_the_removals_would_hand_out_a_referenced_surface() {
        let mut s = session(4, 3);

        // Displayed and returned; only the slot binding keeps the surfaces off the free list.
        s.slots.assign(11).expect("a free slot");
        bind_setup(&mut s, Some(11), Some(0), PLAIN);
        s.slots.assign(12).expect("a free slot");
        bind_setup(&mut s, Some(12), Some(1), PLAIN);
        s.pending.clear();

        let table = s.surface_table();
        assert!(
            table.contains(&s.surfaces[0]),
            "picture 11's surface must still be a resolvable reference"
        );

        let (_, target, same_table) = s.acquire_target().expect("the pool has spares");
        assert_eq!(
            same_table, table,
            "acquire_target must not re-derive the table"
        );
        assert!(!table.contains(&target));

        s.slots.release(11);
        s.sync_slot_bindings();
        let late = s.free_surface().expect("the pool has spares");
        assert_eq!(
            s.surfaces[late], table[0],
            "the late free list offers the surface of the picture this access unit just \
             displaced, and the pre-removal table still names it as a reference — \
             decode into that and the driver predicts from the picture it is writing"
        );
    }

    /// Refusal must not leave the previous surface on the reassigned slot, nor a
    /// pending entry that `show_existing_frame` could export as unwritten memory.
    #[test]
    fn a_refused_picture_binds_nothing_and_can_never_be_exported() {
        let mut s = session(4, 3);

        s.slots.assign(11).expect("a free slot");
        bind_setup(&mut s, Some(11), Some(0), PLAIN);
        assert_eq!(s.slot_surface[0], Some(0));
        assert_eq!(s.surface_table()[0], s.surfaces[0]);
        assert_eq!(s.pending, vec![pending(11, 0)]);

        s.slots.release(11);
        assert_eq!(s.slots.assign(12).expect("the slot 11 gave back"), 0);
        bind_setup(&mut s, Some(12), None, PLAIN);

        assert_eq!(
            s.slot_surface[0], None,
            "the slot must not keep picture 11's surface: picture 12 never decoded, \
             and a reference to 12 that reads 11 is a wrong picture, not a missing one"
        );
        assert_eq!(
            s.surface_table()[0],
            VA_INVALID_ID,
            "and the table the conversion reads must say so, so it can substitute"
        );
        assert!(
            !s.pending.iter().any(|p| p.id == 12),
            "an undecoded picture owes no output — a pending entry is what would let \
             a later show_existing_frame export a surface the driver never wrote"
        );

        assert_eq!(s.slots.slot_of(12), Some(0));
    }

    #[test]
    fn settle_claims_every_output_in_display_order_not_only_the_last() {
        let mut s = session(8, 5);
        for (index, id) in [11u64, 12, 13, 14].iter().enumerate() {
            s.pending.push(pending(*id, index));
        }
        // Planner order, neither decode order nor sorted.
        let outputs = [13u64, 11, 14, 12];
        let claimed = settle(&mut s, &outputs, &outputs);

        assert_eq!(
            claimed.iter().map(|p| p.id).collect::<Vec<_>>(),
            outputs,
            "every bumped picture must come back, in the order the planner listed them"
        );
        assert_eq!(
            claimed.iter().map(|p| p.surface).collect::<Vec<_>>(),
            vec![2, 0, 3, 1],
            "and each must resolve to ITS OWN surface, not to its position in the list"
        );
        assert!(
            s.pending.is_empty(),
            "a claimed picture no longer owes an output"
        );

        // Counterfactual: last-only would ship 12 and drop the other three.
        let old_rule: Vec<u64> = outputs.last().copied().into_iter().collect();
        assert_eq!(old_rule, vec![12]);
        assert_eq!(
            claimed.len() - old_rule.len(),
            3,
            "the old rule dropped three of these four pictures — on the vendored H.264 \
             vector that is 18 frames at three access units, and nothing counted them"
        );
    }

    #[test]
    fn settle_retires_what_left_the_dpb_unshown_and_traces_an_output_it_cannot_place() {
        let mut s = session(4, 3);
        s.pending.push(pending(11, 0));
        s.pending.push(pending(12, 1));

        let claimed = settle(&mut s, &[11], &[11, 12]);
        assert_eq!(claimed.iter().map(|p| p.id).collect::<Vec<_>>(), vec![11]);
        assert!(s.pending.is_empty(), "12 was retired unshown");

        assert!(settle(&mut s, &[99], &[]).is_empty());
    }

    #[test]
    fn a_displayed_picture_carries_its_own_facts_not_its_display_units() {
        /// Stamp the bumping AU's flag on whatever picture it displayed.
        fn old_label(bumping_au_is_idr: bool) -> bool {
            bumping_au_is_idr
        }

        let mut s = session(4, 3);
        let idr = PictureFacts {
            keyframe: true,
            references_clean: true,
            color: ColorDesc {
                primaries: 9,
                transfer: 16,
                matrix: 9,
                full_range: false,
            },
            display: (1920, 1080),
        };
        let trail = PictureFacts {
            keyframe: false,
            references_clean: false,
            color: ColorDesc {
                primaries: 1,
                transfer: 1,
                matrix: 1,
                full_range: false,
            },
            display: (1280, 720),
        };
        s.pending.push(PendingPicture {
            id: 11,
            surface: 0,
            facts: idr,
        });
        s.pending.push(PendingPicture {
            id: 12,
            surface: 1,
            facts: trail,
        });

        let claimed = settle(&mut s, &[11, 12], &[11, 12]);
        assert_eq!(claimed[0].facts, idr);
        assert_eq!(claimed[1].facts, trail);

        assert_ne!(
            old_label(false),
            claimed[0].facts.keyframe,
            "the IDR is bumped out by an ordinary TRAILING access unit several units \
             later, so the old rule flagged the pump's one re-anchor frame as not a \
             keyframe — measured on all three hardware legs' first delivered frame"
        );
        assert_ne!(
            old_label(true),
            claimed[1].facts.keyframe,
            "and the access unit that DRAINS the DPB at a later IDR flagged every old \
             trailing picture draining with it as a keyframe — a re-anchor on a frame \
             that is not one"
        );
        assert_ne!(
            claimed[0].facts.color, claimed[1].facts.color,
            "the same access unit displays pictures decoded under different SPS/VUIs \
             when the host switches an HDR desktop to PQ in-band"
        );
        assert_ne!(
            claimed[0].facts.display, claimed[1].facts.display,
            "and AV1's render region is a per-FRAME value, so a queued frame shown two \
             units later would be cropped to whatever the newest frame asked for"
        );
    }

    /// Guard is real so drop actually releases. No fds; no device.
    fn queued_frame(tx: &mpsc::Sender<VaRelease>, surface: usize) -> DmabufFrame {
        DmabufFrame {
            width: 64,
            height: 64,
            coded_width: 64,
            coded_height: 64,
            fourcc: pf_vaapi::VA_FOURCC_NV12,
            modifier: 0,
            planes: Vec::new(),
            color: PLAIN.color,
            keyframe: PLAIN.keyframe,
            references_clean: PLAIN.references_clean,
            sync_fds: Vec::new(),
            pool_key: (1 << 32) | surface as u64,
            guard: DrmFrameGuard(VaFrameGuard {
                _fds: Vec::new(),
                tx: tx.clone(),
                release: VaRelease {
                    surface,
                    generation: 1,
                },
            }),
        }
    }

    /// Dropping without releasing would trade a queue leak for a surface leak.
    #[test]
    fn the_deliverable_queue_drops_its_oldest_and_frees_the_surface_it_held() {
        let (tx, rx) = mpsc::channel();
        let mut s = session(8, 5);
        let mut queue: std::collections::VecDeque<DmabufFrame> =
            (0..5).map(|i| queued_frame(&tx, i)).collect();
        for i in 0..5 {
            s.held[i] = true;
        }

        let dropped = trim_deliverable(&mut queue, 3);
        assert_eq!(
            dropped
                .iter()
                .map(|f| f.guard.0.release.surface)
                .collect::<Vec<_>>(),
            vec![0, 1],
            "the OLDEST two go: dropping the newest would keep the stalest picture and \
             present the stream in ever-lagging order"
        );
        assert_eq!(
            queue
                .iter()
                .map(|f| f.guard.0.release.surface)
                .collect::<Vec<_>>(),
            vec![2, 3, 4],
            "and what survives keeps display order"
        );

        let mut stale = 0u64;
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert!(
            s.held[0] && s.held[1],
            "a frame still owned holds its surface — the trim returns them so the \
             caller can count them, and the release is the drop"
        );
        drop(dropped);
        drain_releases_into(&rx, Some(&mut s), &mut stale);
        assert!(
            !s.held[0] && !s.held[1],
            "dropping a trimmed frame returns its surface by exactly the path a \
             presented frame takes"
        );
        assert_eq!(stale, 0, "and none of it is a stale-generation token");

        assert!(trim_deliverable(&mut queue, 3).is_empty());
        assert_eq!(trim_deliverable(&mut queue, 0).len(), 3);
        assert!(trim_deliverable(&mut queue, 0).is_empty());
    }

    #[test]
    fn a_non_zero_crop_origin_is_refused() {
        let ok = shape_of(
            1920,
            1088,
            pf_vaapi::DisplayCrop {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
            4,
            1,
            8,
        )
        .expect("the ordinary 1088-coded 1080 picture");
        assert_eq!((ok.display_width, ok.display_height), (1920, 1080));
        assert_eq!((ok.coded_width, ok.coded_height), (1920, 1088));

        assert!(shape_of(
            1920,
            1088,
            pf_vaapi::DisplayCrop {
                x: 8,
                y: 0,
                width: 1912,
                height: 1080,
            },
            4,
            1,
            8,
        )
        .is_err());
    }

    fn av1_plan(max: (u16, u16), frame: (u32, u32), render: (u32, u32)) -> pf_vaapi::AuPlanAv1 {
        let sequence = pf_vaapi::ParsedSequenceHeaderAv1 {
            max_frame_width_minus_1: max.0 - 1,
            max_frame_height_minus_1: max.1 - 1,
            ..Default::default()
        };
        pf_vaapi::AuPlanAv1 {
            picture: pf_vaapi::PicturePlanAv1 {
                frame_type: pf_vaapi::FrameTypeAv1::KeyFrame,
                is_key: true,
                // Key frame: `false` would withhold a re-anchor.
                references_clean: true,
                show_frame: true,
                showable_frame: false,
                order_hint: 0,
                upscaled_width: frame.0,
                frame_width: frame.0,
                frame_height: frame.1,
                render_width: render.0,
                render_height: render.1,
                bit_depth: 8,
                chroma_format_idc: 1,
                colour: pf_vaapi::ColourDescription {
                    colour_primaries: 1,
                    transfer_characteristics: 1,
                    matrix_coefficients: 1,
                    video_full_range: false,
                },
            },
            tiles: Vec::new(),
            refs: [None; 7],
            dpb: pf_vaapi::DpbUpdateAv1::default(),
            dpb_refs: Vec::new(),
            warnings: Vec::new(),
            sequence: std::rc::Rc::new(sequence),
            header: std::rc::Rc::new(pf_vaapi::ParsedFrameHeaderAv1::default()),
        }
    }

    #[test]
    fn an_av1_session_is_sized_from_the_sequence_maximum_not_the_frame() {
        let big = shape_of_av1(&av1_plan((1920, 1080), (1920, 1080), (1920, 1080)));
        assert_eq!((big.coded_width, big.coded_height), (1920, 1080));
        assert_eq!(
            big.max_dpb_frames, 8,
            "NUM_REF_FRAMES, not a stream property"
        );

        let small = shape_of_av1(&av1_plan((1920, 1080), (1280, 720), (960, 540)));
        assert_eq!(
            small, big,
            "a frame that resized itself must not rebuild the session"
        );

        let other = shape_of_av1(&av1_plan((1280, 720), (1280, 720), (1280, 720)));
        assert_ne!(other, big);
    }

    /// Restates the `frame_av1` clamp: 5.9.6 has no upper bound.
    #[test]
    fn an_oversized_render_region_is_clamped_to_the_decoded_picture() {
        let plan = av1_plan((1920, 1080), (1280, 720), (4096, 4096));
        let display = (
            plan.picture.render_width.min(plan.picture.upscaled_width),
            plan.picture.render_height.min(plan.picture.frame_height),
        );
        assert_eq!(display, (1280, 720));

        let ordinary = av1_plan((1920, 1080), (1920, 1088), (1920, 1080));
        let display = (
            ordinary
                .picture
                .render_width
                .min(ordinary.picture.upscaled_width),
            ordinary
                .picture
                .render_height
                .min(ordinary.picture.frame_height),
        );
        assert_eq!(display, (1920, 1080), "the ordinary crop still crops");
    }

    #[test]
    fn a_slice_pair_carries_one_record_unless_av1_says_otherwise() {
        let records = [7u32, 8, 9];
        let ranges = vec![0..4, 4..8, 8..12];
        let pairs = one_record_each(&records, &ranges).expect("parallel lengths");
        assert_eq!(pairs.len(), 3);
        for pair in &pairs {
            assert_eq!(pair.records, 1);
            assert_eq!(pair.record_size, size_of::<u32>());
        }
        assert_eq!(pairs[1].data, 4..8);

        assert!(one_record_each(&records, &ranges[..2]).is_err());
        assert!(one_record_each(&records[..1], &ranges).is_err());
    }

    /// Message names the shape, not libva, so the profile probe still comes first.
    #[test]
    fn a_shape_with_no_profile_is_refused_before_libva_is_loaded() {
        let e = NativeVaapiDecoder::new(
            pf_vaapi::Codec::H264,
            StreamFormat {
                chroma_format_idc: 3,
                bit_depth: 8,
            },
        )
        .err()
        .expect("4:4:4 H.264 has no VAAPI profile in this rung's envelope");
        let text = format!("{e:#}");
        assert!(
            text.contains("profile"),
            "the refusal must name the stream shape, not whatever libva said: {text}"
        );
    }

    /// `dlsym` is a string: a mistyped name compiles. A node that will not
    /// initialise is printed, not failed.
    #[test]
    #[ignore = "needs a machine with a libva runtime"]
    fn probe_this_machines_libva() {
        let va = match Libva::load() {
            Ok(va) => {
                eprintln!("libva: every entry point resolved");
                va
            }
            Err(e) => {
                eprintln!("libva: NOT LOADED — {e:#}");
                eprintln!("(this is the clean-refusal path; the ladder falls through here)");
                return;
            }
        };

        let mut nodes: Vec<std::path::PathBuf> = std::fs::read_dir("/dev/dri")
            .expect("/dev/dri")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        eprintln!("render nodes: {nodes:?}");

        let mut opened = 0;
        for node in &nodes {
            let path = node.to_string_lossy().into_owned();
            match Display::probe(&va, &path) {
                Ok((display, fd, version)) => {
                    opened += 1;
                    eprintln!("  {path}: VA-API {}.{}", version.0, version.1);
                    let d = Display {
                        va: Libva::load().expect("libva loaded once already"),
                        display,
                        node: Some(fd),
                        path: path.clone(),
                        version,
                    };
                    for (name, profile) in [
                        ("H.264 High", pf_vaapi::config::VA_PROFILE_H264_HIGH),
                        ("HEVC Main", pf_vaapi::config::VA_PROFILE_HEVC_MAIN),
                        ("HEVC Main 10", pf_vaapi::config::VA_PROFILE_HEVC_MAIN10),
                        // `profile_for` maps both 8- and 10-bit AV1 4:2:0 onto Profile 0.
                        ("AV1 Profile 0", pf_vaapi::config::VA_PROFILE_AV1_PROFILE0),
                        ("AV1 Profile 1", pf_vaapi::config::VA_PROFILE_AV1_PROFILE1),
                    ] {
                        match d.require_entrypoint(profile) {
                            Ok(()) => eprintln!("    {name}: VLD decode"),
                            Err(e) => eprintln!("    {name}: no ({e})"),
                        }
                    }
                }
                Err(e) => eprintln!("  {path}: {e:#}"),
            }
        }
        eprintln!(
            "{opened} of {} node(s) initialised a VAAPI display",
            nodes.len()
        );
    }

    /// 250 temporal units, 274 coded, 24 hidden, 250 shown. Same file the other rungs walk.
    pub(super) const AV1_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/av1/test_data/test-25fps.ivf.av1"
    );

    /// `[u32 size][u64 pts][size bytes]` after the `DKIF` header. No vendored parser dep.
    pub(super) fn split_ivf(stream: &[u8]) -> Vec<&[u8]> {
        assert_eq!(&stream[0..4], b"DKIF", "the AV1 vector must be an IVF file");
        let header = usize::from(u16::from_le_bytes([stream[6], stream[7]]));
        let mut out = Vec::new();
        let mut at = header;
        while at + 12 <= stream.len() {
            let size =
                u32::from_le_bytes(stream[at..at + 4].try_into().expect("four bytes")) as usize;
            at += 12;
            assert!(
                at + size <= stream.len(),
                "an IVF frame header claims {size} bytes past the end of the file"
            );
            out.push(&stream[at..at + size]);
            at += size;
        }
        out
    }

    /// Decode measurement, not pixels. Pixels are the `parity` module. `#[ignore]`
    /// so a missing entry point fails loudly rather than skips.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an AV1 VLD entry point"]
    fn av1_decodes_the_vendored_vector_on_this_machines_vaapi() {
        let units = split_ivf(AV1_25FPS);
        assert_eq!(
            units.len(),
            250,
            "the vendored AV1 vector is 250 temporal units"
        );

        let mut decoder = NativeVaapiDecoder::new(pf_vaapi::Codec::Av1, StreamFormat::SDR_420_8)
            .expect("this box is supposed to have a VAAPI AV1 decode entry point");
        eprintln!("VAAPI AV1 rung constructed: {}", decoder.name());

        let mut delivered = 0usize;
        let mut first: Option<(u32, u32, u32, u64)> = None;
        for (index, unit) in units.iter().enumerate() {
            match decoder.decode(unit) {
                Ok(Some(frame)) => {
                    assert!(
                        !frame.planes.is_empty(),
                        "unit {index}: a delivered frame exported no dmabuf planes"
                    );
                    if first.is_none() {
                        assert!(
                            frame.keyframe,
                            "the vector opens on a keyframe, so the first delivered \
                             frame must be flagged as one"
                        );
                        first = Some((frame.width, frame.height, frame.fourcc, frame.modifier));
                    }
                    delivered += 1;
                }
                Ok(None) => {}
                Err(e) => panic!("unit {index}: VAAPI AV1 decode failed: {e:#}"),
            }
        }
        assert!(
            decoder.flush().is_empty(),
            "AV1 strands nothing in the DPB: every temporal unit's shown frame is \
             delivered by the unit itself"
        );

        let (w, h, fourcc, modifier) = first.expect("not one frame came back");
        eprintln!(
            "VAAPI AV1: {delivered} frames delivered, first {w}x{h} \
             fourcc={:?} modifier={modifier:#x}",
            std::str::from_utf8(&fourcc.to_le_bytes()).unwrap_or("?")
        );
        assert_eq!((w, h), (320, 240), "the vector is 320x240");
        assert_eq!(
            delivered, 250,
            "the vector displays 250 frames (274 coded, 24 hidden)"
        );
    }

    /// The presenter's AV1 fact on this box, for `PF_VAAPI_VENDOR` (hex, default Intel).
    #[test]
    #[ignore = "needs a machine with a libva runtime"]
    fn av1_decodable_answers_on_this_machine() {
        let vendor = std::env::var("PF_VAAPI_VENDOR")
            .ok()
            .and_then(|v| u32::from_str_radix(v.trim_start_matches("0x"), 16).ok())
            .unwrap_or(0x8086);
        eprintln!(
            "VAAPI AV1 for vendor {vendor:#06x}: {}",
            av1_decodable(vendor)
        );
    }

    /// Wall time of each `decode` call — submit, sync and export, the span the
    /// session reports as decode time — on `PF_VAAPI_BENCH_STREAM` (Annex-B H.265).
    /// Three frames stay held, as a presenter holds them.
    #[test]
    #[ignore = "needs a machine with a libva runtime and PF_VAAPI_BENCH_STREAM"]
    fn h265_decode_time_on_this_machines_vaapi() {
        let path = std::env::var("PF_VAAPI_BENCH_STREAM").expect("PF_VAAPI_BENCH_STREAM");
        let stream = std::fs::read(&path).expect("read the bench stream");
        let units = split_h265_aus(&stream);
        let mut decoder = NativeVaapiDecoder::new(pf_vaapi::Codec::H265, StreamFormat::SDR_420_8)
            .expect("this box is supposed to have a VAAPI HEVC decode entry point");
        let mut us = Vec::with_capacity(units.len());
        let mut fence_us = Vec::with_capacity(units.len());
        let mut fence_outcomes = [0u32; 4]; // cpu-synced, signaled, already-done, timed-out
        let mut held = std::collections::VecDeque::new();
        let start = std::time::Instant::now();
        for (index, unit) in units.iter().enumerate() {
            let t = std::time::Instant::now();
            let frame = decoder
                .decode(unit)
                .unwrap_or_else(|e| panic!("unit {index}: {e:#}"));
            us.push(t.elapsed().as_micros() as u64);
            // The fence the presenter would wait: how long after the call it signals, and
            // whether the kernel handed one out at all.
            for f in &frame {
                use pf_zerocopy::dmabuf_fence::{wait_sync_file, WaitOutcome};
                match f.sync_fds.first() {
                    None => fence_outcomes[0] += 1,
                    Some(fd) => {
                        let w = std::time::Instant::now();
                        let slot = match wait_sync_file(fd.as_raw_fd(), 100) {
                            Ok(WaitOutcome::Signaled) => 1,
                            Ok(WaitOutcome::NoFence) => 2,
                            _ => 3,
                        };
                        fence_outcomes[slot] += 1;
                        fence_us.push(w.elapsed().as_micros() as u64);
                    }
                }
            }
            held.extend(frame);
            if held.len() > 3 {
                held.pop_front();
            }
        }
        let total = start.elapsed();
        us.sort_unstable();
        fence_us.sort_unstable();
        let at = |v: &[u64], p: usize| v.get((v.len().max(1) - 1) * p / 100).copied().unwrap_or(0);
        eprintln!(
            "{path}: {} AUs in {total:?} ({:.0} fps); decode call p50 {} us, p95 {} us, max {} us; \
             fence: cpu-synced {} signaled {} already-done {} timed-out {}; wait p50 {} us p95 {} us",
            us.len(),
            us.len() as f64 / total.as_secs_f64(),
            at(&us, 50),
            at(&us, 95),
            us[us.len() - 1],
            fence_outcomes[0],
            fence_outcomes[1],
            fence_outcomes[2],
            fence_outcomes[3],
            at(&fence_us, 50),
            at(&fence_us, 95),
        );
    }

    /// 250 AUs, two slices per picture. Same file the other rungs decode.
    pub(super) const H264_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h264/test_data/test-25fps.h264"
    );

    /// 250 AUs, one slice per picture.
    pub(super) const H265_25FPS: &[u8] = include_bytes!(
        "../../pf-bitstream/vendor/cros-codecs/src/codec/h265/test_data/test-25fps.h265"
    );

    /// 50 AUs, Main 10: different profile, RT format, and P010. Catches NV12 for 10-bit.
    pub(super) const MAIN10_H265: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/test-main10.h265");

    const H26X_AU_COUNT: usize = 250;
    const MAIN10_AU_COUNT: usize = 50;

    /// Every displayed picture: settle-all + deliverable + flush. Derived by
    /// [`the_planner_already_says_how_many_frames_these_legs_can_deliver`].
    const H264_DELIVERED: usize = 250;
    const H265_DELIVERED: usize = 250;
    const MAIN10_DELIVERED: usize = 50;

    /// Last-only and no flush, so the counterfactual has an expected value.
    const H264_LAST_ONLY: usize = 225;
    const H265_LAST_ONLY: usize = 204;
    const MAIN10_LAST_ONLY: usize = 45;

    /// Start-code scan. Emulation prevention means `00 00 01` is never payload.
    fn nal_headers(stream: &[u8]) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i + 3 <= stream.len() {
            if stream[i..i + 3] == [0x00, 0x00, 0x01] {
                out.push(i + 3);
                i += 3;
            } else {
                i += 1;
            }
        }
        out
    }

    /// New AU at a non-VCL after slices, or a first-of-picture slice while the AU already has slices.
    fn split_aus(stream: &[u8], classify: impl Fn(&[u8], usize) -> (bool, bool)) -> Vec<&[u8]> {
        let mut aus = Vec::new();
        let mut au_start = 0usize;
        let mut au_has_slice = false;
        for header in nal_headers(stream) {
            let (is_slice, first_in_picture) = classify(stream, header);
            // Three-byte start code, plus the optional leading zero of the four-byte form.
            let mut start = header - 3;
            if start > 0 && stream[start - 1] == 0x00 {
                start -= 1;
            }
            if au_has_slice && (!is_slice || first_in_picture) {
                aus.push(&stream[au_start..start]);
                au_start = start;
                au_has_slice = false;
            }
            au_has_slice |= is_slice;
        }
        aus.push(&stream[au_start..]);
        aus
    }

    /// `first_mb_in_slice == 0` is load-bearing: this vector codes two slices per picture.
    pub(super) fn split_h264_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = matches!(s[h] & 0x1f, 1 | 5);
            let first = is_slice && s.get(h + 1).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// Two-byte NAL header; first-slice flag is at `+2`, not H.264's `+1`.
    pub(super) fn split_h265_aus(stream: &[u8]) -> Vec<&[u8]> {
        split_aus(stream, |s, h| {
            let is_slice = (s[h] >> 1) & 0x3f < 32;
            let first = is_slice && s.get(h + 2).is_some_and(|b| b & 0x80 != 0);
            (is_slice, first)
        })
    }

    /// Not ignored: a regenerated 8-bit "Main 10" vector would pass the ten-bit leg.
    #[test]
    fn the_annex_b_splitters_still_cut_the_vendored_vectors() {
        assert_eq!(
            split_h264_aus(H264_25FPS).len(),
            H26X_AU_COUNT,
            "H.264 vector access units"
        );
        assert_eq!(
            split_h265_aus(H265_25FPS).len(),
            H26X_AU_COUNT,
            "H.265 vector access units"
        );

        let main10 = split_h265_aus(MAIN10_H265);
        assert_eq!(main10.len(), MAIN10_AU_COUNT, "Main 10 vector access units");

        let mut planner = pf_vaapi::H265Planner::new();
        let plan = planner
            .plan_au(main10[0])
            .expect("the Main 10 vector's first access unit must plan");
        assert_eq!(
            (
                plan.picture.chroma_format_idc,
                plan.picture.bit_depth_luma_minus8
            ),
            (1, 2),
            "the Main 10 vector must be 4:2:0 at ten bits"
        );
    }

    /// AV1 may store several pictures per temporal unit; H.26x store nought or one.
    #[derive(Debug, Default, Clone)]
    struct AuEffect {
        stored: Vec<u64>,
        outputs: Vec<u64>,
        removed: Vec<u64>,
    }

    struct VectorEffects {
        aus: Vec<AuEffect>,
        flush: AuEffect,
        /// From the stream; also the [`max_deliverable`] bound.
        max_dpb_frames: usize,
    }

    /// Two walks: the planners share no trait.
    fn effects_h264(aus: &[&[u8]]) -> VectorEffects {
        let mut planner = pf_vaapi::H264Planner::new();
        let mut max_dpb_frames = 0usize;
        let walked = aus
            .iter()
            .map(|au| {
                let plan = planner
                    .plan_au(au)
                    .expect("the vendored H.264 vector plans");
                max_dpb_frames = max_dpb_frames.max(plan.picture.max_dpb_frames);
                AuEffect {
                    stored: plan.dpb.stored.into_iter().collect(),
                    outputs: plan.dpb.outputs.clone(),
                    removed: plan.dpb.removed.clone(),
                }
            })
            .collect();
        let update = planner.flush();
        VectorEffects {
            aus: walked,
            flush: AuEffect {
                stored: Vec::new(),
                outputs: update.outputs,
                removed: update.removed,
            },
            max_dpb_frames,
        }
    }

    /// RASL skip is an empty effect, matching [`NativeVaapiDecoder::decode_h265`].
    fn effects_h265(aus: &[&[u8]]) -> VectorEffects {
        let mut planner = pf_vaapi::H265Planner::new();
        let mut max_dpb_frames = 0usize;
        let walked = aus
            .iter()
            .map(|au| match planner.plan_au(au) {
                Ok(plan) => {
                    max_dpb_frames = max_dpb_frames.max(plan.picture.max_dpb_frames);
                    AuEffect {
                        stored: plan.dpb.stored.into_iter().collect(),
                        outputs: plan.dpb.outputs.clone(),
                        removed: plan.dpb.removed.clone(),
                    }
                }
                Err(pf_vaapi::PlanErrorH265::RaslSkipped { .. }) => AuEffect::default(),
                Err(e) => panic!("the vendored HEVC vector must plan: {e:?}"),
            })
            .collect();
        let update = planner.flush();
        VectorEffects {
            aus: walked,
            flush: AuEffect {
                stored: Vec::new(),
                outputs: update.outputs,
                removed: update.removed,
            },
            max_dpb_frames,
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Delivery {
        delivered: usize,
        dropped: usize,
        /// Live slot + pending + queued + consumer-held, at `acquire_target`.
        peak_claim: usize,
    }

    /// Same order as decode: target before removals, claim before retire, trim after ship.
    /// `flush = None` is last-only with no EOS.
    fn simulate(effects: &[AuEffect], flush: Option<&AuEffect>, cap: usize) -> Delivery {
        let mut live: std::collections::BTreeSet<u64> = Default::default();
        let mut pending: Vec<u64> = Vec::new();
        let mut queue: std::collections::VecDeque<u64> = Default::default();
        let (mut delivered, mut dropped, mut peak_claim) = (0usize, 0usize, 0usize);
        let mut consumer = 0usize;

        /// Claim outputs, then retire `removed` whether shown or not.
        fn settle_ids(
            effect: &AuEffect,
            live: &mut std::collections::BTreeSet<u64>,
            pending: &mut Vec<u64>,
            queue: &mut std::collections::VecDeque<u64>,
        ) {
            for id in &effect.outputs {
                if let Some(index) = pending.iter().position(|p| p == id) {
                    queue.push_back(pending.remove(index));
                }
            }
            for id in &effect.removed {
                live.remove(id);
                pending.retain(|p| p != id);
            }
        }

        for effect in effects {
            let claimed =
                live.len() + pending.iter().filter(|p| !live.contains(p)).count() + queue.len();
            peak_claim = peak_claim.max(claimed + consumer + 1);

            for id in &effect.removed {
                live.remove(id);
            }
            for id in &effect.stored {
                live.insert(*id);
                pending.push(*id);
            }
            settle_ids(effect, &mut live, &mut pending, &mut queue);

            consumer = usize::from(queue.pop_front().is_some());
            delivered += consumer;
            while queue.len() > cap {
                queue.pop_front();
                dropped += 1;
            }
        }

        if let Some(effect) = flush {
            settle_ids(effect, &mut live, &mut pending, &mut queue);
            delivered += queue.len();
            queue.clear();
            assert!(
                pending.is_empty(),
                "a flush leaves nothing owing an output: {pending:?}"
            );
        }
        Delivery {
            delivered,
            dropped,
            peak_claim,
        }
    }

    #[test]
    fn the_planner_already_says_how_many_frames_these_legs_can_deliver() {
        let vectors = [
            (
                "H.264",
                effects_h264(&split_h264_aus(H264_25FPS)),
                H264_DELIVERED,
                H264_LAST_ONLY,
            ),
            (
                "H.265",
                effects_h265(&split_h265_aus(H265_25FPS)),
                H265_DELIVERED,
                H265_LAST_ONLY,
            ),
            (
                "Main 10",
                effects_h265(&split_h265_aus(MAIN10_H265)),
                MAIN10_DELIVERED,
                MAIN10_LAST_ONLY,
            ),
        ];

        for (label, vector, expected, last_only) in &vectors {
            let cap = vector.max_dpb_frames;
            let run = simulate(&vector.aus, Some(&vector.flush), cap);
            assert_eq!(
                run.delivered, *expected,
                "{label}: every displayed picture must reach the pump (cap {cap}, \
                 {run:?})"
            );
            assert_eq!(
                run.dropped, 0,
                "{label}: and none of them may be dropped for want of queue depth"
            );

            let old = simulate(&vector.aus, None, 0);
            assert_eq!(
                old.delivered, *last_only,
                "{label}: the pre-fix model must still reproduce the number the \
                 hardware legs measured, or this is not the defect that was fixed"
            );
            assert!(
                old.delivered < run.delivered,
                "{label}: and it must be SHORT — a counterfactual that delivers \
                 everything is not a counterfactual"
            );
        }
    }

    /// Queued frames inherit the DPB claim. Unbounded two-shown-per-AU grows until
    /// the pool is exhausted.
    #[test]
    fn the_queue_never_needs_a_surface_the_pool_does_not_have() {
        for (label, vector, peak, without) in [
            ("H.264", effects_h264(&split_h264_aus(H264_25FPS)), 9, 9),
            ("H.265", effects_h265(&split_h265_aus(H265_25FPS)), 8, 7),
            ("Main 10", effects_h265(&split_h265_aus(MAIN10_H265)), 8, 7),
        ] {
            let dpb = vector.max_dpb_frames;
            let pool = pf_vaapi::surface_count(dpb);
            let run = simulate(&vector.aus, Some(&vector.flush), dpb);
            let queueless = simulate(&vector.aus, Some(&vector.flush), 0);

            assert_eq!(
                (run.peak_claim, queueless.peak_claim),
                (peak, without),
                "{label}: peak surfaces claimed at once, with the queue and without it \
                 (dpb {dpb}, pool {pool})"
            );
            assert!(
                run.peak_claim <= queueless.peak_claim + 1,
                "{label}: the queue must INHERIT the DPB's claim, not add to it — \
                 {} against {}",
                run.peak_claim,
                queueless.peak_claim
            );
            assert!(
                pool - run.peak_claim >= pf_vaapi::config::PRESENTER_HEADROOM - 2,
                "{label}: {} of a {pool}-surface pool claimed, leaving {} of the \
                 {}-surface presenter headroom — a session that cannot find a free \
                 surface refuses the access unit and demotes the rung",
                run.peak_claim,
                pool - run.peak_claim,
                pf_vaapi::config::PRESENTER_HEADROOM,
            );
        }

        // Two shown per unit: H.26x cannot do this; the bound is for AV1.
        let relentless: Vec<AuEffect> = (0..64u64)
            .map(|i| AuEffect {
                stored: vec![i * 2, i * 2 + 1],
                outputs: vec![i * 2, i * 2 + 1],
                removed: vec![i * 2, i * 2 + 1],
            })
            .collect();
        let bounded = simulate(&relentless, None, 4);
        assert!(
            bounded.dropped > 0,
            "the bound must engage on a temporal unit shape that never lets the queue \
             drain"
        );
        assert!(
            bounded.peak_claim <= 4 + 4,
            "and hold the claim flat: peak {}",
            bounded.peak_claim
        );
        let unbounded = simulate(&relentless, None, usize::MAX);
        assert_eq!(
            unbounded.dropped, 0,
            "unbounded drops nothing — it just grows"
        );
        assert!(
            unbounded.peak_claim > bounded.peak_claim * 2,
            "without the bound the same stream grows a queue nothing can drain — peak \
             {} against {}",
            unbounded.peak_claim,
            bounded.peak_claim
        );
    }

    #[derive(Clone, Copy)]
    struct FirstFrame {
        width: u32,
        height: u32,
        fourcc: u32,
        modifier: u64,
        keyframe: bool,
    }

    /// Decode measurement, not pixels. Shared so the three H.26x legs cannot diverge.
    fn run_annex_b(
        codec: pf_vaapi::Codec,
        stream: StreamFormat,
        aus: &[&[u8]],
        label: &str,
    ) -> (usize, FirstFrame) {
        let mut decoder = NativeVaapiDecoder::new(codec, stream).unwrap_or_else(|e| {
            panic!(
                "{label}: this box is supposed to have a VAAPI {label} decode entry point: {e:#}"
            )
        });
        eprintln!("VAAPI {label} rung constructed: {}", decoder.name());

        let mut delivered = 0usize;
        let mut first: Option<FirstFrame> = None;
        let mut record = |frame: &DmabufFrame, where_: &str| {
            assert!(
                !frame.planes.is_empty(),
                "{label} {where_}: a delivered frame exported no dmabuf planes"
            );
            if first.is_none() {
                first = Some(FirstFrame {
                    width: frame.width,
                    height: frame.height,
                    fourcc: frame.fourcc,
                    modifier: frame.modifier,
                    keyframe: frame.keyframe,
                });
            }
        };
        for (index, au) in aus.iter().enumerate() {
            match decoder.decode(au) {
                Ok(Some(frame)) => {
                    record(&frame, &format!("AU {index}"));
                    delivered += 1;
                }
                Ok(None) => {}
                Err(e) => panic!("{label} AU {index}: VAAPI decode failed: {e:#}"),
            }
        }
        let flushed = decoder.flush();
        for (index, frame) in flushed.iter().enumerate() {
            record(frame, &format!("flushed frame {index}"));
        }
        let tail = flushed.len();
        delivered += tail;

        let first = first.unwrap_or_else(|| panic!("{label}: not one frame came back"));
        let FirstFrame {
            width,
            height,
            fourcc,
            modifier,
            keyframe,
        } = first;
        eprintln!(
            "VAAPI {label}: {delivered} frames from {} access units ({tail} of them \
             flushed at end of stream), first {width}x{height} fourcc={:?} \
             modifier={modifier:#x} keyframe={keyframe}",
            aus.len(),
            std::str::from_utf8(&fourcc.to_le_bytes()).unwrap_or("?"),
        );
        (delivered, first)
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an H.264 VLD entry point"]
    fn h264_decodes_the_vendored_vector_on_this_machines_vaapi() {
        let aus = split_h264_aus(H264_25FPS);
        assert_eq!(aus.len(), H26X_AU_COUNT, "the H.264 vector is 250 AUs");

        let (delivered, first) = run_annex_b(
            pf_vaapi::Codec::H264,
            StreamFormat::SDR_420_8,
            &aus,
            "H.264",
        );
        assert_eq!((first.width, first.height), (320, 240), "320x240");
        assert_eq!(
            first.fourcc,
            pf_vaapi::VA_FOURCC_NV12,
            "an 8-bit pool exports NV12"
        );
        assert_eq!(
            delivered, H264_DELIVERED,
            "every picture the vector displays must reach the pump — see \
             H264_DELIVERED, and the_planner_already_says_how_many_frames_these_legs_\
             can_deliver for the same number derived without a device"
        );

        assert!(
            first.keyframe,
            "the vector opens on an IDR, so the first delivered frame must be flagged \
             as a keyframe"
        );
    }

    /// Different conversion and the `RaslSkipped` Ok-skip no other codec has.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main VLD entry point"]
    fn h265_decodes_the_vendored_vector_on_this_machines_vaapi() {
        let aus = split_h265_aus(H265_25FPS);
        assert_eq!(aus.len(), H26X_AU_COUNT, "the H.265 vector is 250 AUs");

        let (delivered, first) = run_annex_b(
            pf_vaapi::Codec::H265,
            StreamFormat::SDR_420_8,
            &aus,
            "H.265",
        );
        assert_eq!((first.width, first.height), (320, 240), "320x240");
        assert_eq!(
            first.fourcc,
            pf_vaapi::VA_FOURCC_NV12,
            "an 8-bit pool exports NV12"
        );
        assert_eq!(
            delivered, H265_DELIVERED,
            "every picture the vector displays must reach the pump"
        );
        assert!(
            first.keyframe,
            "the vector opens on an IDR_N_LP — the same picture-not-access-unit label \
             the H.264 leg documents"
        );
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main 10 VLD entry point"]
    fn hevc_main10_decodes_the_ten_bit_vector_on_this_machines_vaapi() {
        let aus = split_h265_aus(MAIN10_H265);
        assert_eq!(aus.len(), MAIN10_AU_COUNT, "the Main 10 vector is 50 AUs");

        let (delivered, first) = run_annex_b(
            pf_vaapi::Codec::H265,
            StreamFormat {
                bit_depth: 10,
                ..StreamFormat::SDR_420_8
            },
            &aus,
            "HEVC Main 10",
        );
        assert_eq!((first.width, first.height), (320, 240), "320x240");
        assert_eq!(
            first.fourcc,
            pf_vaapi::VA_FOURCC_P010,
            "a ten-bit stream must build a P010 pool, not an 8-bit one"
        );
        assert_eq!(
            delivered, MAIN10_DELIVERED,
            "every picture the vector displays must reach the pump"
        );
        assert!(
            first.keyframe,
            "the same picture-not-access-unit label the H.264 leg documents"
        );
    }
}

#[cfg(test)]
mod parity {
    //! Ignored frame-parity tests against libavcodec goldens in `pf-vkdecode/tests/data`.
    //! Run: `cargo test -p pf-client-core --lib video_vaapi_native -- --include-ignored --nocapture`.
    //! Pin a GPU with `PUNKTFUNK_VAAPI_DEVICE=/dev/dri/renderD…`.
    //!
    //! Seven H.264 / H.265 / Main10 / AV1 legs hash every delivered frame, flush
    //! included. Hidden AV1 frames are observed only through later displayed
    //! dependants. Main10 is high-bit-aligned P010. The 4K AV1 fixture is two tiles.
    //!
    //! Readback is test-only: [`ImageApi`] lives under `#[cfg(test)]`; production
    //! [`Libva`] stays DRM-PRIME. [`Readback`] tries derive then get-image and fails
    //! rather than skipping. `PF_VAAPI_READBACK=derive|getimage` pins a route.
    //! [`the_readback_entry_points_are_resolved_only_inside_this_module`] pins the
    //! boundary. Evidence is driver-specific, not a proof for every VAAPI.

    use sha2::Digest;

    use super::tests::split_h264_aus;
    use super::tests::split_h265_aus;
    use super::tests::split_ivf;
    // Test-only `ImageApi` re-dlopens libva and names these; the lib itself does not.
    use pf_libva::VaDisplay;
    use pf_libva::VaStatus;
    use pf_libva::VA_STATUS_SUCCESS;

    use super::tests::AV1_25FPS;
    use super::tests::H264_25FPS;
    use super::tests::H265_25FPS;
    use super::tests::MAIN10_H265;
    use super::*;

    const GOLDENS_H264: &str = include_str!("../../pf-vkdecode/tests/data/test-25fps.nv12.sha256");
    const GOLDENS_H265: &str =
        include_str!("../../pf-vkdecode/tests/data/test-25fps-h265.nv12.sha256");
    const GOLDENS_MAIN10: &str =
        include_str!("../../pf-vkdecode/tests/data/test-main10.p010.sha256");
    const GOLDENS_AV1: &str =
        include_str!("../../pf-vkdecode/tests/data/test-25fps-av1.nv12.sha256");

    /// Host low-delay H.264: `max_num_reorder_frames = 0`. Pixels-check of the
    /// one-snapshot exemption (module docs).
    const LOWDELAY_H264: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h264");
    const GOLDENS_LOWDELAY_H264: &str =
        include_str!("../../pf-vkdecode/tests/data/lowdelay-640x480.nv12.sha256");

    /// Host low-delay HEVC twin of [`LOWDELAY_H264`].
    const LOWDELAY_H265: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-640x480.h265");
    const GOLDENS_LOWDELAY_H265: &str =
        include_str!("../../pf-vkdecode/tests/data/lowdelay-640x480-h265.nv12.sha256");

    /// Host 4K AV1, two tiles in one Tile Group OBU. Decode coverage only — not the
    /// wire path; packetisation once shipped only the first tile while this would pass.
    const LOWDELAY_AV1: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/lowdelay-3840x2160.ivf.av1");
    const GOLDENS_LOWDELAY_AV1: &str =
        include_str!("../../pf-vkdecode/tests/data/lowdelay-3840x2160-av1.nv12.sha256");

    /// Frame 0 is intra with no refs: a mismatch is readback geometry or tiles, not refs.
    const AV1_FRAME0_NV12: &[u8] =
        include_bytes!("../../pf-vkdecode/tests/data/test-25fps-av1.frame0.nv12");

    const H26X_AU_COUNT: usize = 250;
    const MAIN10_AU_COUNT: usize = 50;
    const AV1_UNIT_COUNT: usize = 250;
    const AV1_DECODED_COUNT: usize = 274;
    const AV1_SHOWN_COUNT: usize = 250;
    const DISPLAY_AV1: (u32, u32) = (320, 240);

    /// Not derived from each other: a harness that computed hidden=0 from one stream
    /// would stop checking the other.
    const LOWDELAY_H264_AU_COUNT: usize = 120;
    const LOWDELAY_H265_AU_COUNT: usize = 120;
    const LOWDELAY_AV1_UNIT_COUNT: usize = 60;
    const LOWDELAY_AV1_DECODED_COUNT: usize = 60;
    const LOWDELAY_AV1_SHOWN_COUNT: usize = 60;
    const DISPLAY_LOWDELAY_AV1: (u32, u32) = (3840, 2160);

    fn golden_hashes(file: &'static str) -> Vec<&'static str> {
        file.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect()
    }

    fn sha256_hex(data: &[u8]) -> String {
        use std::fmt::Write as _;
        sha2::Sha256::digest(data)
            .iter()
            .fold(String::with_capacity(64), |mut out, byte| {
                let _ = write!(out, "{byte:02x}");
                out
            })
    }

    /// Test-only. A second `dlopen` of `libva.so.2` is a refcount bump on the same
    /// mapped library; the display pointer is the rung's.
    struct ImageApi {
        _va: libloading::Library,
        derive_image:
            unsafe extern "C" fn(VaDisplay, VaSurfaceId, *mut pf_vaapi::VaImage) -> VaStatus,
        create_image: unsafe extern "C" fn(
            VaDisplay,
            *mut pf_vaapi::VaImageFormat,
            c_int,
            c_int,
            *mut pf_vaapi::VaImage,
        ) -> VaStatus,
        get_image: unsafe extern "C" fn(
            VaDisplay,
            VaSurfaceId,
            c_int,
            c_int,
            c_uint,
            c_uint,
            c_uint,
        ) -> VaStatus,
        destroy_image: unsafe extern "C" fn(VaDisplay, c_uint) -> VaStatus,
        map_buffer: unsafe extern "C" fn(VaDisplay, VaBufferId, *mut *mut c_void) -> VaStatus,
        unmap_buffer: unsafe extern "C" fn(VaDisplay, VaBufferId) -> VaStatus,
        max_image_formats: unsafe extern "C" fn(VaDisplay) -> c_int,
        query_image_formats:
            unsafe extern "C" fn(VaDisplay, *mut pf_vaapi::VaImageFormat, *mut c_int) -> VaStatus,
    }

    impl ImageApi {
        fn load() -> Result<ImageApi> {
            // SAFETY: the same contract `Libva::load` documents — `Library::new` runs
            // the trusted system libva's initialisers (already loaded by the rung, so
            // this is a refcount bump), and each `lib.get` resolves a documented libva
            // symbol AT the field's own type, transcribed from `va.h`. The `Library`
            // handle is stored beside the pointers, so every one outlives its uses.
            unsafe {
                let va = libloading::Library::new("libva.so.2")
                    .map_err(|e| anyhow!("libva.so.2 (no VAAPI runtime on this system): {e}"))?;
                macro_rules! get {
                    ($lib:expr, $name:literal) => {
                        *$lib
                            .get(concat!($name, "\0").as_bytes())
                            .map_err(|e| anyhow!(concat!("dlsym ", $name, ": {}"), e))?
                    };
                }
                let derive_image = get!(va, "vaDeriveImage");
                let create_image = get!(va, "vaCreateImage");
                let get_image = get!(va, "vaGetImage");
                let destroy_image = get!(va, "vaDestroyImage");
                let map_buffer = get!(va, "vaMapBuffer");
                let unmap_buffer = get!(va, "vaUnmapBuffer");
                let max_image_formats = get!(va, "vaMaxNumImageFormats");
                let query_image_formats = get!(va, "vaQueryImageFormats");
                Ok(ImageApi {
                    derive_image,
                    create_image,
                    get_image,
                    destroy_image,
                    map_buffer,
                    unmap_buffer,
                    max_image_formats,
                    query_image_formats,
                    _va: va,
                })
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Route {
        Derive,
        GetImage,
    }

    struct Staging {
        image: pf_vaapi::VaImage,
        size: (u32, u32),
        fourcc: u32,
    }

    struct Readback {
        api: ImageApi,
        /// Driver's own formats — not a guessed `bits_per_pixel`.
        formats: Vec<pf_vaapi::VaImageFormat>,
        staging: Option<Staging>,
        forced: Option<Route>,
        /// Latched so a refused derive is paid once, not per frame.
        route: Option<Route>,
        /// Picture, or the whole surface if the driver refuses a sub-region.
        get_size: Option<(u32, u32)>,
        derived: u64,
        fetched: u64,
    }

    impl Readback {
        fn new(d: &Display) -> Readback {
            let api = ImageApi::load().expect("libva's image entry points must resolve");
            // SAFETY: a live display; the vector is allocated to the size libva itself
            // reports and `count` is a local written through by the call.
            let formats = unsafe {
                let max = (api.max_image_formats)(d.display);
                if max <= 0 {
                    Vec::new()
                } else {
                    let mut formats =
                        vec![pf_vaapi::VaImageFormat::default(); max.unsigned_abs() as usize];
                    let mut count: c_int = 0;
                    let status =
                        (api.query_image_formats)(d.display, formats.as_mut_ptr(), &mut count);
                    if status == VA_STATUS_SUCCESS {
                        formats.truncate(count.clamp(0, max) as usize);
                        formats
                    } else {
                        Vec::new()
                    }
                }
            };
            let forced = match std::env::var("PF_VAAPI_READBACK").ok().as_deref() {
                Some("derive") => Some(Route::Derive),
                Some("getimage") => Some(Route::GetImage),
                Some(other) => panic!("PF_VAAPI_READBACK={other} — expected derive or getimage"),
                None => None,
            };
            Readback {
                api,
                formats,
                staging: None,
                forced,
                route: None,
                get_size: None,
                derived: 0,
                fetched: 0,
            }
        }

        fn offered(&self) -> String {
            self.formats
                .iter()
                .map(|f| {
                    let b = f.fourcc.to_le_bytes();
                    std::str::from_utf8(&b)
                        .map(str::to_string)
                        .unwrap_or_else(|_| format!("{:#010x}", f.fourcc))
                })
                .collect::<Vec<_>>()
                .join(" ")
        }

        fn read_mapped(
            &self,
            d: &Display,
            image: &pf_vaapi::VaImage,
            display: (u32, u32),
            fourcc: u32,
        ) -> std::result::Result<Vec<u8>, String> {
            let mut base: *mut c_void = std::ptr::null_mut();
            // SAFETY: a live display and an image id this call site owns; `base` is a
            // local written through.
            let status = unsafe { (self.api.map_buffer)(d.display, image.buf, &mut base) };
            if status != VA_STATUS_SUCCESS {
                return Err(format!("{:#}", d.va.err("vaMapBuffer", status)));
            }
            if base.is_null() {
                // SAFETY: pairing the successful map above.
                unsafe { (self.api.unmap_buffer)(d.display, image.buf) };
                return Err("vaMapBuffer succeeded and returned a null pointer".to_string());
            }
            // SAFETY: `vaMapBuffer` returned a pointer to `data_size` readable bytes —
            // that is what the field means — and the mapping stays valid until the
            // `vaUnmapBuffer` below, which is after the last read. `pack_two_plane`
            // bounds-checks every row it takes against this length, so a descriptor
            // that disagrees with its own buffer is a refusal rather than a read past
            // the end.
            let mapped =
                unsafe { std::slice::from_raw_parts(base.cast::<u8>(), image.data_size as usize) };
            let packed = pf_vaapi::pack_two_plane(image, mapped, display, fourcc).map_err(|e| {
                format!(
                    "{e} — the driver's image is {}x{}, {} plane(s), pitches {:?}, \
                     offsets {:?}, data_size {}",
                    image.width,
                    image.height,
                    image.num_planes,
                    image.pitches,
                    image.offsets,
                    image.data_size
                )
            });
            // SAFETY: pairing the successful map above; nothing reads `mapped` after.
            unsafe { (self.api.unmap_buffer)(d.display, image.buf) };
            packed
        }

        /// Some drivers have no vertical padding; the chroma-offset trap is CPU-tested
        /// in `pf-vaapi`.
        fn describe(&self, d: &Display, surface: VaSurfaceId) -> String {
            let mut image = pf_vaapi::VaImage::zeroed();
            // SAFETY: a live display and a surface from its own pool; `image` is a
            // zeroed local of the measured layout that outlives the call.
            let status = unsafe { (self.api.derive_image)(d.display, surface, &mut image) };
            if status != VA_STATUS_SUCCESS {
                return format!("vaDeriveImage: {:#}", d.va.err("vaDeriveImage", status));
            }
            let text = format!(
                "{}x{}, {} plane(s), pitches {:?}, offsets {:?}, data_size {} — chroma \
                 at pitch*height would be {}, so this surface is {}",
                image.width,
                image.height,
                image.num_planes,
                image.pitches,
                image.offsets,
                image.data_size,
                image.pitches[0] * u32::from(image.height),
                if image.offsets[1] == image.pitches[0] * u32::from(image.height) {
                    "NOT vertically padded (the crop trap is untested here)"
                } else {
                    "vertically PADDED (the crop trap is live here)"
                }
            );
            // SAFETY: destroying the image this call derived, exactly once.
            unsafe { (self.api.destroy_image)(d.display, image.image_id) };
            text
        }

        fn read_via_derive(
            &self,
            d: &Display,
            surface: VaSurfaceId,
            display: (u32, u32),
            fourcc: u32,
        ) -> std::result::Result<Vec<u8>, String> {
            let mut image = pf_vaapi::VaImage::zeroed();
            // SAFETY: a live display and a surface from its own pool; `image` is a
            // zeroed local of the measured layout that outlives the call.
            let status = unsafe { (self.api.derive_image)(d.display, surface, &mut image) };
            if status != VA_STATUS_SUCCESS {
                return Err(format!("{:#}", d.va.err("vaDeriveImage", status)));
            }
            let out = self.read_mapped(d, &image, display, fourcc);
            // SAFETY: destroying the image this call derived, exactly once. Required
            // even on the failure path — the derive succeeded, so the image exists.
            unsafe { (self.api.destroy_image)(d.display, image.image_id) };
            out
        }

        fn ensure_staging(
            &mut self,
            d: &Display,
            size: (u32, u32),
            fourcc: u32,
        ) -> std::result::Result<(), String> {
            if self
                .staging
                .as_ref()
                .is_some_and(|s| s.size == size && s.fourcc == fourcc)
            {
                return Ok(());
            }
            self.destroy_staging(d);
            let mut format = *self
                .formats
                .iter()
                .find(|f| f.fourcc == fourcc)
                .ok_or_else(|| {
                    format!(
                        "this driver offers no VAImageFormat for the surface pool's own \
                         fourcc; it offers [{}]",
                        self.offered()
                    )
                })?;
            let mut image = pf_vaapi::VaImage::zeroed();
            // SAFETY: a live display; `format` and `image` are locals of the measured
            // layouts that outlive the call, and libva copies the format it is handed.
            let status = unsafe {
                (self.api.create_image)(
                    d.display,
                    &mut format,
                    size.0 as c_int,
                    size.1 as c_int,
                    &mut image,
                )
            };
            if status != VA_STATUS_SUCCESS {
                return Err(format!("{:#}", d.va.err("vaCreateImage", status)));
            }
            self.staging = Some(Staging {
                image,
                size,
                fourcc,
            });
            Ok(())
        }

        fn destroy_staging(&mut self, d: &Display) {
            if let Some(s) = self.staging.take() {
                // SAFETY: an image this type created on this display, destroyed once.
                unsafe { (self.api.destroy_image)(d.display, s.image.image_id) };
            }
        }

        fn get_into(
            &mut self,
            d: &Display,
            surface: VaSurfaceId,
            size: (u32, u32),
            display: (u32, u32),
            fourcc: u32,
        ) -> std::result::Result<Vec<u8>, String> {
            self.ensure_staging(d, size, fourcc)?;
            let image = self.staging.as_ref().expect("just ensured").image;
            // SAFETY: a live display, a surface from its own pool and an image this
            // type created on it. The region is inside the surface: `size` is either
            // the picture (which the surface contains) or the surface itself.
            let status = unsafe {
                (self.api.get_image)(
                    d.display,
                    surface,
                    0,
                    0,
                    size.0 as c_uint,
                    size.1 as c_uint,
                    image.image_id,
                )
            };
            if status != VA_STATUS_SUCCESS {
                return Err(format!(
                    "{:#} (image {}x{})",
                    d.va.err("vaGetImage", status),
                    size.0,
                    size.1
                ));
            }
            self.read_mapped(d, &image, display, fourcc)
        }

        /// Picture first, then the whole surface; crop in [`pf_vaapi::pack_two_plane`].
        fn read_via_get_image(
            &mut self,
            d: &Display,
            surface: VaSurfaceId,
            display: (u32, u32),
            coded: (u32, u32),
            fourcc: u32,
        ) -> std::result::Result<Vec<u8>, String> {
            if let Some(size) = self.get_size {
                return self.get_into(d, surface, size, display, fourcc);
            }
            let mut sizes = vec![display];
            if coded != display {
                sizes.push(coded);
            }
            let mut why = Vec::new();
            for size in sizes {
                match self.get_into(d, surface, size, display, fourcc) {
                    Ok(bytes) => {
                        self.get_size = Some(size);
                        return Ok(bytes);
                    }
                    Err(e) => why.push(e),
                }
            }
            Err(why.join("; "))
        }

        fn read_route(
            &mut self,
            route: Route,
            d: &Display,
            surface: VaSurfaceId,
            display: (u32, u32),
            coded: (u32, u32),
            fourcc: u32,
        ) -> std::result::Result<Vec<u8>, String> {
            match route {
                Route::Derive => {
                    let out = self.read_via_derive(d, surface, display, fourcc);
                    if out.is_ok() {
                        self.derived += 1;
                    }
                    out
                }
                Route::GetImage => {
                    let out = self.read_via_get_image(d, surface, display, coded, fourcc);
                    if out.is_ok() {
                        self.fetched += 1;
                    }
                    out
                }
            }
        }

        /// No skip: an unread surface must fail, not pass.
        fn read(
            &mut self,
            d: &Display,
            surface: VaSurfaceId,
            display: (u32, u32),
            coded: (u32, u32),
            fourcc: u32,
            what: &str,
        ) -> Vec<u8> {
            // SAFETY: a live display and a surface from its own pool. VAAPI has no fence.
            let status = unsafe { (d.va.sync_surface)(d.display, surface) };
            if status != VA_STATUS_SUCCESS {
                panic!("{what}: {:#}", d.va.err("vaSyncSurface", status));
            }
            if let Some(route) = self.route {
                return match self.read_route(route, d, surface, display, coded, fourcc) {
                    Ok(bytes) => bytes,
                    Err(e) => panic!(
                        "{what}: the {route:?} readback stopped working part-way through \
                         a run — {e}"
                    ),
                };
            }
            let order = match self.forced {
                Some(r) => vec![r],
                None => vec![Route::Derive, Route::GetImage],
            };
            let mut why = Vec::new();
            for route in order {
                match self.read_route(route, d, surface, display, coded, fourcc) {
                    Ok(bytes) => {
                        eprintln!("readback route: {route:?}");
                        self.route = Some(route);
                        return bytes;
                    }
                    Err(e) => why.push(format!("{route:?}: {e}")),
                }
            }
            panic!(
                "{what}: NO readback route could read the decoded surface, so this leg \
                 can prove nothing and refuses to pass — {}. The driver offers image \
                 formats [{}]",
                why.join(" | "),
                self.offered()
            );
        }

        /// Derive can "succeed" onto tiled bytes. Agreement with get-image is evidence
        /// the mapping is linear. A refused route is reported, not failed.
        fn cross_check(
            &mut self,
            d: &Display,
            surface: VaSurfaceId,
            display: (u32, u32),
            coded: (u32, u32),
            fourcc: u32,
            what: &str,
        ) {
            let derived = self.read_via_derive(d, surface, display, fourcc);
            let fetched = self.read_via_get_image(d, surface, display, coded, fourcc);
            match (&derived, &fetched) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(
                        a.len(),
                        b.len(),
                        "{what}: the two readback routes disagree on the picture's size"
                    );
                    if a != b {
                        let diff = localise(a, b, display, fourcc);
                        panic!(
                            "{what}: vaDeriveImage and vaGetImage read DIFFERENT pixels \
                             out of one surface — {diff}. Derive is handing back memory \
                             this walk cannot address linearly (tiled or swizzled), so \
                             every hash taken through it is meaningless. Re-run with \
                             PF_VAAPI_READBACK=getimage"
                        );
                    }
                    eprintln!("{what}: both readback routes agree ({} bytes)", a.len());
                }
                (Ok(a), Err(e)) => eprintln!(
                    "{what}: vaDeriveImage answers ({} bytes); vaGetImage does not — {e}",
                    a.len()
                ),
                (Err(e), Ok(b)) => eprintln!(
                    "{what}: vaGetImage answers ({} bytes); vaDeriveImage does not — {e}",
                    b.len()
                ),
                (Err(a), Err(b)) => panic!(
                    "{what}: NEITHER readback route can read this surface — derive: {a} \
                     | getimage: {b}. The driver offers image formats [{}]",
                    self.offered()
                ),
            }
        }

        fn summary(&self) -> String {
            format!(
                "readback via {:?} ({} derived, {} vaGetImage)",
                self.route, self.derived, self.fetched
            )
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct Divergence {
        luma_samples: usize,
        chroma_samples: usize,
        luma_box: Option<(u32, u32, u32, u32)>,
        max_delta: u32,
        /// P010 ten bits are in the high end; non-zero low six bits is a format error.
        low_bits_set: usize,
    }

    impl std::fmt::Display for Divergence {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if self.luma_samples == 0 && self.chroma_samples == 0 {
                return write!(f, "identical");
            }
            write!(
                f,
                "{} luma sample(s), {} chroma sample(s), max |delta| {}",
                self.luma_samples, self.chroma_samples, self.max_delta
            )?;
            if let Some((x0, y0, x1, y1)) = self.luma_box {
                write!(
                    f,
                    ", luma bounding box ({x0},{y0})..({x1},{y1}) = {}x{}",
                    x1 - x0 + 1,
                    y1 - y0 + 1
                )?;
            }
            if self.chroma_samples == 0 {
                write!(f, ", chroma CLEAN")?;
            }
            if self.low_bits_set > 0 {
                write!(
                    f,
                    ", and {} sample(s) have their low six bits set — P010's ten bits \
                     belong in the HIGH end of each word, so suspect the FORMAT before \
                     the decode",
                    self.low_bits_set
                )?;
            }
            Ok(())
        }
    }

    fn localise(got: &[u8], want: &[u8], display: (u32, u32), fourcc: u32) -> Divergence {
        let stride = if fourcc == pf_vaapi::VA_FOURCC_P010 {
            2usize
        } else {
            1
        };
        let (width, height) = (display.0 as usize, display.1 as usize);
        let luma_bytes = width * height * stride;
        let sample = |buf: &[u8], at: usize| -> u32 {
            if stride == 2 {
                u32::from(u16::from_le_bytes([buf[at], buf[at + 1]]))
            } else {
                u32::from(buf[at])
            }
        };
        let mut d = Divergence {
            luma_samples: 0,
            chroma_samples: 0,
            luma_box: None,
            max_delta: 0,
            low_bits_set: 0,
        };
        let end = got.len().min(want.len());
        let mut at = 0usize;
        while at + stride <= end {
            let (a, b) = (sample(got, at), sample(want, at));
            if stride == 2 && a & 0x3f != 0 {
                d.low_bits_set += 1;
            }
            if a != b {
                d.max_delta = d.max_delta.max(a.abs_diff(b));
                if at < luma_bytes {
                    d.luma_samples += 1;
                    let index = at / stride;
                    let (x, y) = ((index % width) as u32, (index / width) as u32);
                    d.luma_box = Some(match d.luma_box {
                        None => (x, y, x, y),
                        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
                    });
                } else {
                    d.chroma_samples += 1;
                }
            }
            at += stride;
        }
        d
    }

    /// Planner walk alongside the rung. Hardware legs hash delivery order; CPU
    /// guards check goldens against this. RASL skip contributes an empty `per_unit`.
    struct Order {
        decode: Vec<u64>,
        display: Vec<u64>,
        per_unit: Vec<Vec<u64>>,
    }

    impl Order {
        fn empty() -> Order {
            Order {
                decode: Vec::new(),
                display: Vec::new(),
                per_unit: Vec::new(),
            }
        }
    }

    fn order_h264(aus: &[&[u8]]) -> Order {
        let mut planner = pf_vaapi::H264Planner::new();
        let mut order = Order::empty();
        for (index, au) in aus.iter().enumerate() {
            let plan = planner
                .plan_au(au)
                .unwrap_or_else(|e| panic!("AU {index}: the clean vector must plan, got {e:?}"));
            assert_eq!(
                (plan.picture.display_crop.x, plan.picture.display_crop.y),
                (0, 0),
                "AU {index}: this rung REFUSES a non-zero conformance-window origin \
                 (`shape_of`), so a vector that had one could not be decoded here at all"
            );
            let id = plan
                .dpb
                .stored
                .unwrap_or_else(|| panic!("AU {index}: every picture of this vector is stored"));
            order.decode.push(id);
            order.per_unit.push(vec![id]);
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.display.extend(planner.flush().outputs);
        order
    }

    fn order_h265(aus: &[&[u8]]) -> Order {
        let mut planner = pf_vaapi::H265Planner::new();
        let mut order = Order::empty();
        for (index, au) in aus.iter().enumerate() {
            let plan = match planner.plan_au(au) {
                Ok(plan) => plan,
                Err(pf_vaapi::PlanErrorH265::RaslSkipped { .. }) => {
                    order.per_unit.push(Vec::new());
                    continue;
                }
                Err(e) => panic!("AU {index}: the clean vector must plan, got {e:?}"),
            };
            assert_eq!(
                (plan.picture.display_crop.x, plan.picture.display_crop.y),
                (0, 0),
                "AU {index}: this rung refuses a non-zero conformance-window origin"
            );
            let id = plan
                .dpb
                .stored
                .unwrap_or_else(|| panic!("AU {index}: every picture of this vector is stored"));
            order.decode.push(id);
            order.per_unit.push(vec![id]);
            order.display.extend(plan.dpb.outputs.iter().copied());
        }
        order.display.extend(planner.flush().outputs);
        order
    }

    /// One decoded picture per FRAME; a unit may carry several. No planner flush.
    fn order_av1(units: &[&[u8]], render: (u32, u32)) -> Order {
        let mut planner = pf_vaapi::Av1Planner::new();
        let mut order = Order::empty();
        for (index, unit) in units.iter().enumerate() {
            let plans = planner
                .plan_au(unit)
                .unwrap_or_else(|e| panic!("unit {index}: the clean vector must plan, got {e}"));
            let mut this_unit = Vec::new();
            for plan in &plans {
                assert!(
                    plan.warnings.is_empty(),
                    "unit {index}: a clean vector must plan without warnings, got {:?}",
                    plan.warnings
                );
                assert_eq!(
                    (plan.picture.render_width, plan.picture.render_height),
                    render,
                    "unit {index}: the goldens are the {render:?} render region"
                );
                if let Some(id) = plan.dpb.stored {
                    order.decode.push(id);
                    this_unit.push(id);
                }
                order.display.extend(plan.dpb.outputs.iter().copied());
            }
            order.per_unit.push(this_unit);
        }
        order
    }

    /// Surface, display region, and fourcc from the delivered frame, not re-derived.
    fn read_frame(
        decoder: &NativeVaapiDecoder,
        readback: &mut Readback,
        frame: &DmabufFrame,
        what: &str,
    ) -> Vec<u8> {
        let release = frame.guard.0.release;
        let (surface, coded, fourcc) = {
            let s = decoder
                .session
                .as_ref()
                .unwrap_or_else(|| panic!("{what}: a frame came back with no session behind it"));
            assert_eq!(
                release.generation, s.generation,
                "{what}: this frame names a RETIRED surface pool, so its pixels are not \
                 this session's — nothing in these vectors renegotiates, so a mismatch \
                 here is a bookkeeping defect rather than a stream that resized"
            );
            (
                s.surfaces[release.surface],
                (s.shape.coded_width, s.shape.coded_height),
                s.fourcc,
            )
        };
        assert_eq!(
            frame.fourcc, fourcc,
            "{what}: the frame's fourcc is not the pool's — `finish` is supposed to \
             refuse that before it ships"
        );
        let display = (frame.width, frame.height);
        // Cross-check unless a route was pinned (the pin is for a box where one is wrong).
        if readback.route.is_none() && readback.forced.is_none() {
            readback.cross_check(&decoder.display, surface, display, coded, fourcc, what);
        }
        let bytes = readback.read(&decoder.display, surface, display, coded, fourcc, what);
        assert_eq!(
            bytes.len(),
            pf_vaapi::packed_len(display, fourcc).expect("the pool's fourcc is one of ours"),
            "{what}: the readback is not the golden's own layout"
        );
        bytes
    }

    fn compare(hashes: &[String], goldens: &[&str], label: &str) -> (usize, Option<usize>) {
        let mut mismatches = 0usize;
        let mut first = None;
        for (n, (got, golden)) in hashes.iter().zip(goldens.iter()).enumerate() {
            if got != golden {
                if mismatches < 10 {
                    eprintln!("{label}: display frame {n}: {got} != {golden}");
                }
                if first.is_none() {
                    first = Some(n);
                }
                mismatches += 1;
            }
        }
        (mismatches, first)
    }

    fn verdict(
        mismatches: usize,
        first: Option<usize>,
        total: usize,
        label: &str,
        readback: &str,
        opening: &str,
    ) {
        assert_eq!(
            mismatches, 0,
            "{label}: {mismatches}/{total} frames diverge from libavcodec's software \
             decode (first ten above; first divergence at display frame {first:?}). \
             {opening} Read the signature as evidence about WHERE, not about WHAT — the \
             D3D11VA AV1 defect had two unlike signatures on two vendors and was ONE \
             bug. Readback was {readback}; PF_VAAPI_READBACK=getimage forces the copying \
             route, and PF_VAAPI_DUMP=<tag> writes the raw planes"
        );
        eprintln!(
            "{label}: {total} frames bit-identical to libavcodec software decode ({readback})"
        );
    }

    fn dump(tag: &Option<String>, label: &str, what: &str, bytes: &[u8]) {
        let Some(tag) = tag else { return };
        let path = std::env::temp_dir().join(format!(
            "pf-vaapi-{tag}-{}-{what}.bin",
            label.replace([' ', '(', ')', ',', '.'], "")
        ));
        std::fs::write(&path, bytes).expect("write the dump");
        eprintln!("dumped {what} -> {}", path.display());
    }

    struct Delivered {
        hashes: Vec<String>,
        first_keyframe: bool,
        first_bytes: Vec<u8>,
        dropped: u64,
    }

    /// Hash every delivered frame, then flush: stopping at the last AU misses the DPB tail.
    fn drive(
        decoder: &mut NativeVaapiDecoder,
        readback: &mut Readback,
        units: &[&[u8]],
        label: &str,
    ) -> Delivered {
        let mut hashes = Vec::new();
        let mut first_keyframe = false;
        let mut first_bytes = Vec::new();
        for (index, unit) in units.iter().enumerate() {
            let frame = decoder
                .decode(unit)
                .unwrap_or_else(|e| panic!("{label} AU {index}: decode failed — {e:#}"));
            if let Some(frame) = frame {
                let what = format!("{label} AU {index} -> display frame {}", hashes.len());
                let bytes = read_frame(decoder, readback, &frame, &what);
                if hashes.is_empty() {
                    first_keyframe = frame.keyframe;
                    first_bytes = bytes.clone();
                }
                hashes.push(sha256_hex(&bytes));
            }
        }
        let tail = decoder.flush();
        eprintln!("{label}: {} frame(s) came out of the flush", tail.len());
        for frame in &tail {
            let what = format!("{label} flush -> display frame {}", hashes.len());
            let bytes = read_frame(decoder, readback, frame, &what);
            if hashes.is_empty() {
                first_keyframe = frame.keyframe;
                first_bytes = bytes.clone();
            }
            hashes.push(sha256_hex(&bytes));
        }
        Delivered {
            hashes,
            first_keyframe,
            first_bytes,
            dropped: decoder.health().dropped,
        }
    }

    fn check_delivery(d: &Delivered, goldens: &[&str], label: &str) {
        assert_eq!(
            d.dropped, 0,
            "{label}: the rung DROPPED {} display-ready frame(s) because its deliverable \
             queue overflowed. Every one of them is a golden that can never be checked, \
             so the comparison below would be measuring a shorter stream than the \
             goldens describe",
            d.dropped
        );
        assert_eq!(
            d.hashes.len(),
            goldens.len(),
            "{label}: the rung delivered {} frames and the goldens carry {}. This is the \
             delivery path, not the decode: the rung must hand back every picture the \
             planner outputs, `flush` included",
            d.hashes.len(),
            goldens.len()
        );
        assert!(
            d.first_keyframe,
            "{label}: the FIRST delivered frame is not flagged as a keyframe. Every one \
             of these streams opens on an IDR or an AV1 key frame, and that frame is the \
             first thing displayed — a rung that labels the access unit rather than the \
             picture it displays gets this wrong on any stream that reorders, and \
             `DecodedImage::is_keyframe` is the pump's post-loss re-anchor signal"
        );
    }

    fn parity_run(
        codec: pf_vaapi::Codec,
        stream: StreamFormat,
        aus: &[&[u8]],
        order: &Order,
        goldens: &[&str],
        expected_aus: usize,
        label: &str,
    ) {
        assert_eq!(
            aus.len(),
            expected_aus,
            "{label}: the vector must split into {expected_aus} access units — a \
             different count means this file's splitter disagrees with pf-bitstream's, \
             and nothing below it is meaningful"
        );
        assert_eq!(
            order.display.len(),
            goldens.len(),
            "{label}: the planner outputs {} pictures, the goldens carry {}",
            order.display.len(),
            goldens.len()
        );

        let mut decoder = NativeVaapiDecoder::new(codec, stream)
            .unwrap_or_else(|e| panic!("{label}: this box must host this profile — {e:#}"));
        let mut readback = Readback::new(&decoder.display);
        let dump_tag = std::env::var("PF_VAAPI_DUMP").ok();

        let delivered = drive(&mut decoder, &mut readback, aus, label);
        dump(&dump_tag, label, "display0", &delivered.first_bytes);
        check_delivery(&delivered, goldens, label);

        let (mismatches, first) = compare(&delivered.hashes, goldens, label);
        let readback_note = readback.summary();
        readback.destroy_staging(&decoder.display);
        verdict(
            mismatches,
            first,
            goldens.len(),
            label,
            &readback_note,
            "Display frame 0 is intra-only — if IT diverges suspect the readback \
             geometry (pitch, crop, plane offset) or the surface format rather than the \
             decode.",
        );
    }

    /// Temporal units, not pictures. Frame 0 pixels localise without a second GPU.
    #[allow(clippy::too_many_arguments)]
    fn av1_parity_run(
        units: &[&[u8]],
        order: &Order,
        goldens: &[&str],
        unit_count: usize,
        decoded_count: usize,
        shown_count: usize,
        frame0_golden: Option<&[u8]>,
        label: &str,
    ) {
        assert_eq!(
            units.len(),
            unit_count,
            "{label}: the IVF reader disagrees with the stream's temporal-unit count"
        );
        assert_eq!(order.decode.len(), decoded_count);
        assert_eq!(order.per_unit.len(), units.len());
        assert_eq!(order.display.len(), goldens.len());
        assert_eq!(order.display.len(), shown_count);

        let mut decoder = NativeVaapiDecoder::new(pf_vaapi::Codec::Av1, StreamFormat::SDR_420_8)
            .unwrap_or_else(|e| panic!("{label}: this box must host AV1 Profile 0 — {e:#}"));
        let mut readback = Readback::new(&decoder.display);
        let dump_tag = std::env::var("PF_VAAPI_DUMP").ok();

        let delivered = drive(&mut decoder, &mut readback, units, label);
        dump(&dump_tag, label, "display0", &delivered.first_bytes);
        check_delivery(&delivered, goldens, label);

        let hidden = decoded_count - shown_count;
        assert_eq!(
            delivered.hashes.len(),
            shown_count,
            "{label}: {decoded_count} pictures decode and {shown_count} display, so \
             {hidden} must have been decoded and WITHHELD. On a stream with no hidden \
             frames both sides are equal and this is a tautology — deliberately, so one \
             harness serves both shapes"
        );

        if let Some(golden) = frame0_golden {
            if delivered.first_bytes.as_slice() != golden {
                let diff = localise(
                    &delivered.first_bytes,
                    golden,
                    DISPLAY_AV1,
                    pf_vaapi::VA_FOURCC_NV12,
                );
                panic!(
                    "{label}: display frame 0 does not match libavcodec's own pixels — \
                     {diff}. It is a KEY frame with no references, so this is readback \
                     geometry, the surface format, or the tile records — never reference \
                     handling"
                );
            }
            eprintln!("{label}: display frame 0 is byte-identical to libavcodec's pixels");
        }

        let (mismatches, first) = compare(&delivered.hashes, goldens, label);
        let readback_note = readback.summary();
        readback.destroy_staging(&decoder.display);
        verdict(
            mismatches,
            first,
            goldens.len(),
            label,
            &readback_note,
            &format!(
                "{hidden} hidden frame(s) were decoded and withheld. Display frame 0 is a \
                 key frame — if IT diverges suspect the readback geometry or the tile \
                 records rather than the reference handling."
            ),
        );
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an H.264 VLD entry point"]
    fn h264_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h264_aus(H264_25FPS);
        let order = order_h264(&aus);
        parity_run(
            pf_vaapi::Codec::H264,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_H264),
            H26X_AU_COUNT,
            "H.264",
        );
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an H.264 VLD entry point"]
    fn low_delay_host_h264_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h264_aus(LOWDELAY_H264);
        let order = order_h264(&aus);
        parity_run(
            pf_vaapi::Codec::H264,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_LOWDELAY_H264),
            LOWDELAY_H264_AU_COUNT,
            "H.264 (low-delay host stream)",
        );
    }

    /// `PF_VA_FIELD_STREAM=<capture>`: decode a capture through this rung and write one line
    /// per access unit to `<capture>.pfhash`: the AU index and the SHA-256 of the frame it
    /// delivered, `-` when it delivered none (a concealed AU is withheld). A lossy view aligns
    /// against its lossless twin by AU index. `PF_VA_FIELD_CODEC=h264` for H.264, else HEVC.
    #[test]
    #[ignore = "field triage: set PF_VA_FIELD_STREAM (needs a libva runtime)"]
    fn field_stream_writes_frame_hashes() {
        let path = std::env::var("PF_VA_FIELD_STREAM").expect("PF_VA_FIELD_STREAM=<capture>");
        let bytes = std::fs::read(&path).expect("read the capture");
        let h264 = std::env::var("PF_VA_FIELD_CODEC").is_ok_and(|c| c == "h264");
        let aus = if h264 {
            split_h264_aus(&bytes)
        } else {
            split_h265_aus(&bytes)
        };
        let codec = if h264 {
            pf_vaapi::Codec::H264
        } else {
            pf_vaapi::Codec::H265
        };
        let mut decoder =
            NativeVaapiDecoder::new(codec, StreamFormat::SDR_420_8).expect("open the rung");
        let mut readback = Readback::new(&decoder.display);
        let mut lines = Vec::new();
        for (index, au) in aus.iter().enumerate() {
            match decoder.decode(au) {
                Ok(Some(frame)) => {
                    let what = format!("AU {index}");
                    let bytes = read_frame(&decoder, &mut readback, &frame, &what);
                    lines.push(format!("{index} {}", sha256_hex(&bytes)));
                }
                Ok(None) => lines.push(format!("{index} -")),
                Err(e) => lines.push(format!("{index} refused {e:#}")),
            }
        }
        for frame in decoder.flush() {
            let bytes = read_frame(&decoder, &mut readback, &frame, "flush");
            lines.push(format!("flush {}", sha256_hex(&bytes)));
        }
        readback.destroy_staging(&decoder.display);
        let out = format!("{path}.pfhash");
        std::fs::write(&out, lines.join("\n") + "\n").expect("write the hashes");
        eprintln!("{} AUs hashed through this rung → {out}", aus.len());
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main VLD entry point"]
    fn h265_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(H265_25FPS);
        let order = order_h265(&aus);
        parity_run(
            pf_vaapi::Codec::H265,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_H265),
            H26X_AU_COUNT,
            "H.265",
        );
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main VLD entry point"]
    fn low_delay_host_h265_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(LOWDELAY_H265);
        let order = order_h265(&aus);
        parity_run(
            pf_vaapi::Codec::H265,
            StreamFormat::SDR_420_8,
            &aus,
            &order,
            &golden_hashes(GOLDENS_LOWDELAY_H265),
            LOWDELAY_H265_AU_COUNT,
            "H.265 (low-delay host stream)",
        );
    }

    /// P010 is two bytes per sample; HEVC pads 240 to 256, so chroma is not at display height.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an HEVC Main 10 VLD entry point"]
    fn main10_every_frame_hashes_bit_identical_to_libavcodec() {
        let aus = split_h265_aus(MAIN10_H265);
        let order = order_h265(&aus);
        parity_run(
            pf_vaapi::Codec::H265,
            StreamFormat {
                bit_depth: 10,
                ..StreamFormat::SDR_420_8
            },
            &aus,
            &order,
            &golden_hashes(GOLDENS_MAIN10),
            MAIN10_AU_COUNT,
            "HEVC Main 10",
        );
    }

    #[test]
    #[ignore = "needs a machine with a libva runtime and an AV1 VLD entry point"]
    fn av1_every_delivered_frame_hashes_bit_identical_to_libavcodec() {
        let units = split_ivf(AV1_25FPS);
        let order = order_av1(&units, DISPLAY_AV1);
        av1_parity_run(
            &units,
            &order,
            &golden_hashes(GOLDENS_AV1),
            AV1_UNIT_COUNT,
            AV1_DECODED_COUNT,
            AV1_SHOWN_COUNT,
            Some(AV1_FRAME0_NV12),
            "AV1",
        );
    }

    /// Two tiles; the vendored vector is single-tile so `plan_to_va_av1` would stay degenerate.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an AV1 VLD entry point"]
    fn low_delay_host_av1_every_frame_hashes_bit_identical_to_libavcodec() {
        let units = split_ivf(LOWDELAY_AV1);
        let order = order_av1(&units, DISPLAY_LOWDELAY_AV1);
        av1_parity_run(
            &units,
            &order,
            &golden_hashes(GOLDENS_LOWDELAY_AV1),
            LOWDELAY_AV1_UNIT_COUNT,
            LOWDELAY_AV1_DECODED_COUNT,
            LOWDELAY_AV1_SHOWN_COUNT,
            None,
            "AV1 (low-delay host stream, 4K two-tile)",
        );
    }

    /// Fails rather than skips if neither route works.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an H.264 VLD entry point"]
    fn probe_this_machines_readback_routes() {
        let aus = split_h264_aus(H264_25FPS);
        let mut decoder = NativeVaapiDecoder::new(pf_vaapi::Codec::H264, StreamFormat::SDR_420_8)
            .expect("this box is supposed to have a VAAPI H.264 decode entry point");
        let mut frame = None;
        for (index, au) in aus.iter().enumerate() {
            frame = decoder
                .decode(au)
                .unwrap_or_else(|e| panic!("AU {index}: decode failed — {e:#}"));
            if frame.is_some() {
                break;
            }
        }
        let frame = frame.expect("some access unit of the vendored vector must deliver a frame");
        let release = frame.guard.0.release;
        let (surface, display, coded, fourcc) = {
            let s = decoder.session.as_ref().expect("a session");
            (
                s.surfaces[release.surface],
                (frame.width, frame.height),
                (s.shape.coded_width, s.shape.coded_height),
                s.fourcc,
            )
        };
        let mut readback = Readback::new(&decoder.display);
        eprintln!("driver image formats: [{}]", readback.offered());
        eprintln!("surface {surface:#x}: picture {display:?} in a {coded:?} pool");
        eprintln!(
            "derived layout: {}",
            readback.describe(&decoder.display, surface)
        );
        // SAFETY: a live display and a surface from its own pool.
        let status = unsafe { (decoder.display.va.sync_surface)(decoder.display.display, surface) };
        assert_eq!(status, VA_STATUS_SUCCESS, "vaSyncSurface");
        for route in [Route::Derive, Route::GetImage] {
            match readback.read_route(route, &decoder.display, surface, display, coded, fourcc) {
                Ok(bytes) => eprintln!("  {route:?}: {} bytes", bytes.len()),
                Err(e) => eprintln!("  {route:?}: NO — {e}"),
            }
        }
        readback.cross_check(
            &decoder.display,
            surface,
            display,
            coded,
            fourcc,
            "readback probe",
        );
        assert!(
            readback.derived > 0 || readback.fetched > 0,
            "neither readback route works on this device — parity is impossible here, \
             and saying so is the point of this probe"
        );
        readback.destroy_staging(&decoder.display);
    }

    /// Distinct pictures hash differently; one flipped luma byte is named.
    #[test]
    #[ignore = "needs a machine with a libva runtime and an H.264 VLD entry point"]
    fn the_readback_reads_real_pixels_and_the_comparison_can_fail() {
        let aus = split_h264_aus(H264_25FPS);
        let goldens = golden_hashes(GOLDENS_H264);
        let mut decoder = NativeVaapiDecoder::new(pf_vaapi::Codec::H264, StreamFormat::SDR_420_8)
            .expect("this box is supposed to have a VAAPI H.264 decode entry point");
        let mut readback = Readback::new(&decoder.display);

        let mut frames: Vec<Vec<u8>> = Vec::new();
        for (index, au) in aus.iter().take(20).enumerate() {
            let frame = decoder.decode(au).expect("the clean vector decodes");
            let Some(frame) = frame else { continue };
            let what = format!("counterfactual AU {index}");
            let bytes = read_frame(&decoder, &mut readback, &frame, &what);
            assert!(
                bytes.iter().any(|b| *b != bytes[0]),
                "{what}: the readback handed back {} identical bytes — that is an \
                 unwritten or unmapped surface, not a picture",
                bytes.len()
            );
            frames.push(bytes);
        }
        readback.destroy_staging(&decoder.display);
        assert!(
            frames.len() >= 2,
            "the first twenty access units must deliver at least two pictures"
        );

        let hashes: Vec<String> = frames.iter().map(|b| sha256_hex(b)).collect();
        let distinct: std::collections::HashSet<&String> = hashes.iter().collect();
        assert!(
            distinct.len() > 1,
            "{} decoded pictures produced ONE hash — the readback is reading the same \
             surface, or the same bytes, every time",
            hashes.len()
        );
        let here: Vec<&str> = goldens[..hashes.len()].to_vec();
        assert_eq!(
            compare(&hashes, &here, "counterfactual"),
            (0, None),
            "the first {} display frames must already agree with libavcodec, or this \
             test is measuring a defect rather than its own falsifiability",
            hashes.len()
        );

        let victim = hashes.len() - 1;
        let mut corrupted = frames[victim].clone();
        // Centre luma of 320x240, so the box names a pixel, not a plane edge.
        let at = 120 * 320 + 160;
        corrupted[at] ^= 0x01;
        let diff = localise(
            &corrupted,
            &frames[victim],
            (320, 240),
            pf_vaapi::VA_FOURCC_NV12,
        );
        assert_eq!(
            diff.luma_samples, 1,
            "one flipped luma byte, one differing sample"
        );
        assert_eq!(diff.chroma_samples, 0, "chroma must read CLEAN");
        assert_eq!(diff.max_delta, 1);
        assert_eq!(
            diff.luma_box,
            Some((160, 120, 160, 120)),
            "the divergence must be localised to the pixel that was flipped"
        );

        let mut dirty = hashes.clone();
        dirty[victim] = sha256_hex(&corrupted);
        assert_eq!(
            compare(&dirty, &here, "counterfactual"),
            (1, Some(victim)),
            "the comparison every leg uses must catch a one-byte corruption, and name \
             which display frame carries it"
        );
    }

    /// Quoted `dlsym` strings only; prose elsewhere naming the same calls is ignored.
    #[test]
    fn the_readback_entry_points_are_resolved_only_inside_this_module() {
        const SOURCE: &str = include_str!("video_vaapi_native.rs");
        const MARKER: &str = "mod parity {";
        let at = SOURCE
            .find(MARKER)
            .expect("this module's own header is in this module's own file");
        let (production, harness) = SOURCE.split_at(at);
        for symbol in [
            "\"vaDeriveImage\"",
            "\"vaCreateImage\"",
            "\"vaGetImage\"",
            "\"vaDestroyImage\"",
            "\"vaMapBuffer\"",
            "\"vaUnmapBuffer\"",
            "\"vaQueryImageFormats\"",
            "\"vaMaxNumImageFormats\"",
        ] {
            assert!(
                !production.contains(symbol),
                "{symbol} is resolved OUTSIDE the `#[cfg(test)] mod parity` block. The \
                 surface readback is a test-only facility: a shipped build must not be \
                 able to map a decode surface at all, which is what keeps the zero-copy \
                 guarantee structural rather than a promise"
            );
            assert!(
                harness.contains(symbol),
                "{symbol} is no longer resolved by the parity harness — if the readback \
                 moved, this guard has to move with it or it protects nothing"
            );
        }
    }

    #[test]
    fn every_golden_set_matches_its_planners_display_order() {
        for (label, order, goldens) in [
            (
                "H.264",
                order_h264(&split_h264_aus(H264_25FPS)),
                golden_hashes(GOLDENS_H264),
            ),
            (
                "H.264 low-delay",
                order_h264(&split_h264_aus(LOWDELAY_H264)),
                golden_hashes(GOLDENS_LOWDELAY_H264),
            ),
            (
                "H.265",
                order_h265(&split_h265_aus(H265_25FPS)),
                golden_hashes(GOLDENS_H265),
            ),
            (
                "H.265 low-delay",
                order_h265(&split_h265_aus(LOWDELAY_H265)),
                golden_hashes(GOLDENS_LOWDELAY_H265),
            ),
            (
                "HEVC Main 10",
                order_h265(&split_h265_aus(MAIN10_H265)),
                golden_hashes(GOLDENS_MAIN10),
            ),
            (
                "AV1",
                order_av1(&split_ivf(AV1_25FPS), DISPLAY_AV1),
                golden_hashes(GOLDENS_AV1),
            ),
            (
                "AV1 low-delay 4K",
                order_av1(&split_ivf(LOWDELAY_AV1), DISPLAY_LOWDELAY_AV1),
                golden_hashes(GOLDENS_LOWDELAY_AV1),
            ),
        ] {
            assert_eq!(
                order.display.len(),
                goldens.len(),
                "{label}: the planner outputs {} pictures and the golden file carries {}",
                order.display.len(),
                goldens.len()
            );
            assert!(
                order.decode.len() >= order.display.len(),
                "{label}: a picture cannot be displayed without being decoded"
            );
            for id in &order.display {
                assert!(
                    order.decode.contains(id),
                    "{label}: display order names PicId {id}, which nothing decodes — the \
                     hardware legs would fail on this with a message about the rung"
                );
            }
            assert_eq!(
                goldens.len(),
                goldens
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                "{label}: two display frames carry the SAME golden hash. That is not \
                 impossible in principle, but on these vectors it would mean the golden \
                 file was generated from a stream that repeated a frame — and a parity \
                 leg cannot tell a correctly repeated frame from a rung that delivered \
                 one picture twice"
            );
        }
    }

    #[test]
    fn the_two_av1_streams_are_the_opposite_shapes_the_legs_claim() {
        let vendored = order_av1(&split_ivf(AV1_25FPS), DISPLAY_AV1);
        assert_eq!(vendored.per_unit.len(), AV1_UNIT_COUNT);
        assert_eq!(vendored.decode.len(), AV1_DECODED_COUNT);
        assert_eq!(vendored.display.len(), AV1_SHOWN_COUNT);
        assert_eq!(
            vendored.per_unit.iter().filter(|u| u.len() > 1).count(),
            AV1_DECODED_COUNT - AV1_SHOWN_COUNT,
            "24 units must carry a hidden frame as well as the shown one — without them \
             the AV1 leg proves nothing the H.264 leg does not already prove"
        );

        let ours = order_av1(&split_ivf(LOWDELAY_AV1), DISPLAY_LOWDELAY_AV1);
        assert_eq!(ours.per_unit.len(), LOWDELAY_AV1_UNIT_COUNT);
        assert_eq!(ours.decode.len(), LOWDELAY_AV1_DECODED_COUNT);
        assert_eq!(ours.display.len(), LOWDELAY_AV1_SHOWN_COUNT);
        assert!(
            ours.per_unit.iter().all(|u| u.len() == 1),
            "our host emits one frame per temporal unit and no hidden frames — the \
             OPPOSITE shape to the vendored vector, which is why both legs exist"
        );
    }

    #[test]
    fn the_vendored_vectors_reorder_and_our_own_streams_do_not() {
        for (label, order) in [
            ("H.264", order_h264(&split_h264_aus(H264_25FPS))),
            ("H.265", order_h265(&split_h265_aus(H265_25FPS))),
        ] {
            assert_ne!(
                order.decode, order.display,
                "{label}: this vector no longer reorders — the tail `flush` drains would \
                 then be empty and these legs would stop covering the reordering path"
            );
        }
        for (label, order) in [
            (
                "H.264 low-delay",
                order_h264(&split_h264_aus(LOWDELAY_H264)),
            ),
            (
                "H.265 low-delay",
                order_h265(&split_h265_aus(LOWDELAY_H265)),
            ),
        ] {
            assert_eq!(
                order.decode, order.display,
                "{label}: our host emits zero-reorder output, so decode order IS display \
                 order — if that stops being true these fixtures no longer represent \
                 what punktfunk streams"
            );
        }
    }

    #[test]
    fn the_comparison_catches_a_corrupted_frame() {
        let goldens = ["aa", "bb", "cc"];
        let clean: Vec<String> = goldens.iter().map(|g| (*g).to_string()).collect();
        assert_eq!(
            compare(&clean, &goldens, "cpu"),
            (0, None),
            "an agreeing set must report no divergence"
        );

        let mut one = clean.clone();
        one[1] = "beef".to_string();
        assert_eq!(
            compare(&one, &goldens, "cpu"),
            (1, Some(1)),
            "one wrong frame must be reported once, at its DISPLAY index"
        );

        let mut two = one.clone();
        two[0] = "dead".to_string();
        assert_eq!(
            compare(&two, &goldens, "cpu"),
            (2, Some(0)),
            "the first divergence must be the FIRST one, not the last seen"
        );
    }

    #[test]
    fn a_divergence_names_the_plane_the_box_and_the_magnitude() {
        let (w, h) = (320u32, 240u32);
        let clean = vec![0x40u8; (w * h + w * h / 2) as usize];

        let mut one_block = clean.clone();
        for y in 24..48u32 {
            for x in 16..32u32 {
                one_block[(y * w + x) as usize] = 0x48;
            }
        }
        let d = localise(&one_block, &clean, (w, h), pf_vaapi::VA_FOURCC_NV12);
        assert_eq!(d.luma_samples, 16 * 24);
        assert_eq!(d.chroma_samples, 0);
        assert_eq!(d.luma_box, Some((16, 24, 31, 47)));
        assert_eq!(d.max_delta, 8);
        assert!(format!("{d}").contains("chroma CLEAN"));
        assert!(format!("{d}").contains("16x24"));

        let structural = vec![0xffu8; clean.len()];
        let d = localise(&structural, &clean, (w, h), pf_vaapi::VA_FOURCC_NV12);
        assert_eq!(d.luma_samples, (w * h) as usize);
        assert_eq!(d.chroma_samples, (w * h / 2) as usize);
        assert_eq!(d.max_delta, 0xff - 0x40);
        assert!(!format!("{d}").contains("chroma CLEAN"));

        assert_eq!(
            localise(&clean, &clean, (w, h), pf_vaapi::VA_FOURCC_NV12).luma_samples,
            0
        );
        assert_eq!(
            format!(
                "{}",
                localise(&clean, &clean, (w, h), pf_vaapi::VA_FOURCC_NV12)
            ),
            "identical"
        );
    }

    /// P010 ten bits are high-aligned; LSB-aligned `yuv420p10le` has the right length.
    #[test]
    fn lsb_aligned_ten_bit_samples_are_called_out_as_a_format_problem() {
        let (w, h) = (16u32, 16u32);
        let samples = (w * h + w * h / 2) as usize;
        let msb: Vec<u8> = (0..samples).flat_map(|_| 0x0200u16.to_le_bytes()).collect();
        let lsb: Vec<u8> = (0..samples).flat_map(|_| 0x0008u16.to_le_bytes()).collect();

        let d = localise(&lsb, &msb, (w, h), pf_vaapi::VA_FOURCC_P010);
        assert_eq!(d.low_bits_set, samples, "every sample carries low bits");
        assert!(
            format!("{d}").contains("low six bits"),
            "the report must point at the FORMAT: {d}"
        );

        let d = localise(&msb, &msb, (w, h), pf_vaapi::VA_FOURCC_P010);
        assert_eq!(d.low_bits_set, 0);
        assert_eq!(format!("{d}"), "identical");
    }
}
