//! `VkVideoSessionKHR` + `VkVideoSessionParametersKHR` lifecycle for AV1.
//!
//! One sequence header, no add-info, nothing `vkUpdateVideoSessionParametersKHR`
//! can add. Identical stored header ⇒ [`ParamsActionAv1::Current`]; anything
//! else ⇒ [`ParamsActionAv1::Recreate`] (Vulkan cannot replace a stored set).
//!
//! An AV1 parameters object has no empty form (`pStdSequenceHeader` must be
//! valid), so [`VideoSessionAv1::create`] leaves the handle NULL and the first
//! [`VideoSessionAv1::ensure_parameters`] creates it. The decoder calls that
//! before every submission.
//!
//! The Std header's heap must outlive the parameters OBJECT, not just create:
//! a driver retains `pColorConfig` and `pStdSequenceHeader` and reads them at
//! decode record. [`StoredParamsAv1`] holds object and backing together; the
//! Std struct is boxed so the address create is given survives the wrapper
//! move. Evidence: `the_sequence_header_address_the_create_call_is_given_survives_being_stored`.
//!
//! `ParamsLedgerAv1` is the pure half (unit-tested); [`VideoSessionAv1`] is the
//! Vulkan half.

use std::rc::Rc;

use ash::vk;
use cros_codecs::codec::av1::parser::SequenceHeaderObu;
use tracing::debug;

use crate::caps::DecodeCaps;
use crate::caps::DecodeProfile;
use crate::caps_av1::Av1ProfileKey;
use crate::device::DecodeDevice;
use crate::params_av1::sequence_to_std;
use crate::params_av1::OwnedStdAv1SequenceHeader;
use crate::session::CodecSession;
use crate::session::ParametersObject;
use crate::session::RawVideoSession;
use crate::session::SessionError;

/// What the ledger decided for one sequence-header activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamsActionAv1 {
    /// The identical sequence header is already stored — nothing to do.
    Current,
    /// First header, or stored content changed. No `Add`: AV1 holds one
    /// sequence header and Vulkan has no update path for it (module docs).
    Recreate,
}

/// Which sequence header the parameters object holds, keyed by content.
///
/// The parser re-parses the in-band header at every keyframe, so an unchanged
/// stream hands out a fresh `Rc` each time. Identity would Recreate — and drain
/// the pipeline — on a steady stream.
#[derive(Debug, Default)]
pub(crate) struct ParamsLedgerAv1 {
    sequence: Option<Rc<SequenceHeaderObu>>,
}

impl ParamsLedgerAv1 {
    /// Action for activating `sequence`. Pure; mutate via [`Self::commit`].
    pub(crate) fn plan(&self, sequence: &Rc<SequenceHeaderObu>) -> ParamsActionAv1 {
        match &self.sequence {
            Some(stored) if **stored == **sequence => ParamsActionAv1::Current,
            _ => ParamsActionAv1::Recreate,
        }
    }

    pub(crate) fn commit(&mut self, action: ParamsActionAv1, sequence: &Rc<SequenceHeaderObu>) {
        match action {
            ParamsActionAv1::Current => {}
            ParamsActionAv1::Recreate => self.sequence = Some(Rc::clone(sequence)),
        }
    }
}

/// The session's create-time shape; a plan disagreeing with it forces a rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionConfigAv1 {
    pub max_coded_extent: vk::Extent2D,
    pub max_dpb_slots: u32,
    pub max_active_references: u32,
    /// Std profile, sampling, bit depth, and film-grain. A stream can
    /// renegotiate any of them; that is a session rebuild, not a parameters
    /// update.
    pub profile: Av1ProfileKey,
}

/// Parameters object and the Std sequence header it was created from.
///
/// One value so the backing cannot drop while the object lives: a driver
/// dereferences `pColorConfig` at decode record (module docs).
struct StoredParamsAv1 {
    object: vk::VideoSessionParametersKHR,
    /// Lives as long as `object`. This crate does not read it after create;
    /// the driver does, including through `pStdSequenceHeader` — hence boxed
    /// Std, not inline (module docs).
    _sequence: OwnedStdAv1SequenceHeader,
}

impl ParametersObject for Option<StoredParamsAv1> {
    fn object(&self) -> vk::VideoSessionParametersKHR {
        self.as_ref()
            .map_or(vk::VideoSessionParametersKHR::null(), |p| p.object)
    }
}

/// AV1 session: [`RawVideoSession`] plus the sequence-header parameters object.
pub struct VideoSessionAv1 {
    raw: RawVideoSession,
    /// `None` until the first [`Self::ensure_parameters`]: no empty form (module docs).
    parameters: Option<StoredParamsAv1>,
    ledger: ParamsLedgerAv1,
    pub(crate) config: SessionConfigAv1,
}

impl VideoSessionAv1 {
    /// Session only. Parameters follow at the first [`Self::ensure_parameters`].
    ///
    /// # Safety
    ///
    /// `dev` wraps live handles ([`crate::DeviceHandles`] contract).
    pub(crate) unsafe fn create(
        dev: &DecodeDevice,
        caps: &DecodeCaps,
        config: SessionConfigAv1,
    ) -> Result<Self, SessionError> {
        // SAFETY: fn contract.
        let raw = unsafe {
            RawVideoSession::create(
                dev,
                caps,
                DecodeProfile::Av1(config.profile),
                config.max_coded_extent,
                config.max_dpb_slots,
                config.max_active_references,
            )?
        };
        Ok(Self {
            raw,
            parameters: None,
            ledger: ParamsLedgerAv1::default(),
            config,
        })
    }

    /// Ledger action for `sequence`, no mutation. The decoder consults this
    /// before [`Self::ensure_parameters`] so a Recreate of an existing object
    /// can drain in-flight work first (destroy must not race a submitted decode).
    pub(crate) fn parameters_action(&self, sequence: &Rc<SequenceHeaderObu>) -> ParamsActionAv1 {
        self.ledger.plan(sequence)
    }

    /// Whether a parameters object exists. Paired with [`Self::parameters_action`]:
    /// the first Recreate destroys nothing and needs no drain; later ones do.
    pub(crate) fn has_parameters(&self) -> bool {
        self.parameters.is_some()
    }

    /// Install this frame's active sequence header on the parameters object.
    ///
    /// # Safety
    ///
    /// Live device; when [`Self::parameters_action`] says `Recreate` AND
    /// [`Self::has_parameters`] is true, the caller has ALREADY drained every
    /// in-flight decode — the old object is destroyed here, and a still-executing
    /// decode reading it would be use-after-free at the driver level.
    /// `Current` touches no object a submitted decode can be reading.
    pub(crate) unsafe fn ensure_parameters(
        &mut self,
        sequence: &Rc<SequenceHeaderObu>,
    ) -> Result<(), SessionError> {
        let action = self.ledger.plan(sequence);
        match action {
            ParamsActionAv1::Current => Ok(()),
            ParamsActionAv1::Recreate => {
                debug!(
                    first = !self.has_parameters(),
                    "creating AV1 session parameters (first activation or a \
                     sequence-header content change)"
                );
                // Moved into stored parameters below; lives as long as the object,
                // not merely across create. A driver reads `pColorConfig` at every
                // `vkCmdDecodeVideoKHR` (module docs).
                let owned = sequence_to_std(sequence).map_err(SessionError::ParamsAv1)?;
                let mut av1 = vk::VideoDecodeAV1SessionParametersCreateInfoKHR::default()
                    .std_sequence_header(owned.std());
                let ci = vk::VideoSessionParametersCreateInfoKHR::default()
                    .video_session(self.raw.session())
                    .push_next(&mut av1);
                let mut fresh = vk::VideoSessionParametersKHR::null();
                // SAFETY: live device + live session; `ci` roots locals including
                // the OwnedStd backing, which outlives the call and the object
                // it creates.
                let r = unsafe {
                    (self
                        .raw
                        .video_queue()
                        .fp()
                        .create_video_session_parameters_khr)(
                        self.raw.device().handle(),
                        &ci,
                        std::ptr::null(),
                        &mut fresh,
                    )
                };
                if r != vk::Result::SUCCESS {
                    return Err(SessionError::Vk(r));
                }
                let fresh = Some(StoredParamsAv1 {
                    object: fresh,
                    _sequence: owned,
                });
                // SAFETY: the fn-level contract — the caller drained every
                // in-flight decode before a Recreate over an existing object
                // reached here (checked via parameters_action + has_parameters).
                unsafe { self.raw.replace_parameters(&mut self.parameters, fresh) };
                self.ledger.commit(action, sequence);
                Ok(())
            }
        }
    }
}

impl CodecSession for VideoSessionAv1 {
    fn raw_mut(&mut self) -> &mut RawVideoSession {
        &mut self.raw
    }

    fn parameters(&self) -> vk::VideoSessionParametersKHR {
        self.parameters.object()
    }

    fn max_active_references(&self) -> u32 {
        self.config.max_active_references
    }
}

impl Drop for VideoSessionAv1 {
    fn drop(&mut self) {
        // SAFETY: this session's own parameters object (NULL before the first
        // activation); the decoder drains GPU work first. `raw` drops next and
        // destroys the session; the sequence-header backing drops after that.
        unsafe { self.raw.destroy_parameters(self.parameters.object()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture with the fields the ledger compares. The vendored parser has
    /// no builders; `SequenceHeaderObu` is `Default` + `PartialEq`.
    fn authored(max_frame_width_minus_1: u16, film_grain: bool) -> Rc<SequenceHeaderObu> {
        Rc::new(SequenceHeaderObu {
            max_frame_width_minus_1,
            max_frame_height_minus_1: 1079,
            film_grain_params_present: film_grain,
            ..Default::default()
        })
    }

    /// `pStdSequenceHeader` still addresses the stored wrapper's Std struct
    /// after the wrapper is moved into [`StoredParamsAv1`].
    ///
    /// [`crate::params_av1`]'s `moving_the_wrapper_leaves_the_driver_s_pointers_put`
    /// pins the inner pointers; this pins the create-info pointer.
    /// `ensure_parameters` hands `owned.std()` to create, then moves the wrapper
    /// into stored parameters — inline Std would leave a moved-from slot.
    /// Boxing keeps the two addresses equal.
    #[test]
    fn the_sequence_header_address_the_create_call_is_given_survives_being_stored() {
        let seq = authored(1919, false);
        let owned = sequence_to_std(&seq).expect("a plain 8-bit header converts");
        // Same address `ensure_parameters` puts in `pStdSequenceHeader`.
        let handed = std::ptr::from_ref(owned.std());
        // Same final move as `ensure_parameters`: `self.parameters = Some(…)`.
        let parameters = Some(StoredParamsAv1 {
            object: vk::VideoSessionParametersKHR::null(),
            _sequence: owned,
        });
        let Some(stored) = parameters else {
            unreachable!("just installed")
        };
        assert_eq!(
            std::ptr::from_ref(stored._sequence.std()),
            handed,
            "the address handed to Vulkan must be the address the object keeps"
        );
        // SAFETY: `stored` owns the header — which is exactly the property here.
        let width = unsafe { (*handed).max_frame_width_minus_1 };
        assert_eq!(width, 1919, "the fixture's width, read back through it");
    }

    #[test]
    fn the_first_activation_recreates_because_there_is_no_empty_parameters_object() {
        let seq = authored(1919, false);
        let mut ledger = ParamsLedgerAv1::default();
        // Not `Add`: AV1 session parameters have no update path, and no object
        // exists yet — the session was created without one.
        assert_eq!(ledger.plan(&seq), ParamsActionAv1::Recreate);
        ledger.commit(ParamsActionAv1::Recreate, &seq);
        assert_eq!(ledger.plan(&seq), ParamsActionAv1::Current);
    }

    #[test]
    fn a_reparsed_identical_sequence_header_is_current_not_a_recreate() {
        // Same content, a new Rc: the parser re-parses at every keyframe.
        // Identity would Recreate (and drain) on a steady stream.
        let a = authored(1919, false);
        let b = authored(1919, false);
        assert!(!Rc::ptr_eq(&a, &b));

        let mut ledger = ParamsLedgerAv1::default();
        ledger.commit(ParamsActionAv1::Recreate, &a);
        assert_eq!(ledger.plan(&b), ParamsActionAv1::Current);
    }

    #[test]
    fn a_changed_sequence_header_recreates_and_the_new_one_is_then_current() {
        let small = authored(1279, false);
        let large = authored(1919, false);
        let mut ledger = ParamsLedgerAv1::default();
        ledger.commit(ParamsActionAv1::Recreate, &small);
        assert_eq!(ledger.plan(&small), ParamsActionAv1::Current);

        // Content change ⇒ Recreate. AV1 has no in-place replacement for a
        // stored sequence header.
        assert_eq!(ledger.plan(&large), ParamsActionAv1::Recreate);
        ledger.commit(ParamsActionAv1::Recreate, &large);
        assert_eq!(ledger.plan(&large), ParamsActionAv1::Current);
        assert_eq!(
            ledger.plan(&small),
            ParamsActionAv1::Recreate,
            "the ledger holds exactly one header — the old one is gone"
        );
    }

    #[test]
    fn turning_film_grain_on_is_a_content_change_the_ledger_sees() {
        // Also a profile change (session rebuild via SessionConfigAv1::profile).
        // The ledger must not depend on the session layer: a grain-flag-only
        // difference is still a different stored set.
        let plain = authored(1919, false);
        let grainy = authored(1919, true);
        assert_ne!(plain, grainy);
        let mut ledger = ParamsLedgerAv1::default();
        ledger.commit(ParamsActionAv1::Recreate, &plain);
        assert_eq!(ledger.plan(&grainy), ParamsActionAv1::Recreate);
    }

    #[test]
    fn committing_current_leaves_the_stored_header_alone() {
        // `commit(Current, ..)` is reachable on every steady-state frame; it must
        // be a genuine no-op rather than a silent re-store of an equal value.
        let a = authored(1919, false);
        let mut ledger = ParamsLedgerAv1::default();
        assert!(ledger.sequence.is_none());
        ledger.commit(ParamsActionAv1::Current, &a);
        assert!(
            ledger.sequence.is_none(),
            "Current must not install a header the object does not hold"
        );
        assert_eq!(ledger.plan(&a), ParamsActionAv1::Recreate);
    }
}
