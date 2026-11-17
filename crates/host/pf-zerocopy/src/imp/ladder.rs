//! What one capture identity has learned about its zero-copy lanes, as a plain value.
//!
//! Two lanes can fail. [`Lane::Raw`] hands the dmabuf to the encoder (libva, PyroWave's Vulkan,
//! NVENC's fused convert); [`Lane::Gpu`] is the EGL→CUDA importer NVENC reads through. Each lane
//! has two causes, an import that failed and an offer the compositor never took, and each cause
//! a strike budget. A spent cause turns its lane off for the identity until the process exits; a
//! success clears that cause's strikes, never a spent one. Three [`Refusal`]s narrow what a live
//! lane offers, each on its first sighting.
//!
//! [`step`] is pure. [`crate::ZeroCopyHealth`] holds one [`Ladder`] per identity; the capture's
//! negotiation plan turns a snapshot into the rung it builds. Pinned by the table test below.

/// Below the host's `MAX_ENCODER_RESETS`, so the raw lane goes off before that session ends.
pub(super) const RAW_IMPORT_BUDGET: u8 = 3;
/// One retry. Each failed offer costs pf-capture's `FIRST_FRAME_BUDGET` stall, and a
/// compositor mid-restart fails once.
pub(super) const RAW_OFFER_BUDGET: u8 = 2;
/// One compositor crash costs one rebuild, not the lane.
pub(super) const GPU_IMPORT_BUDGET: u8 = 3;
/// A compositor that allocates none of the importer's modifiers refuses them on every retry.
const GPU_OFFER_BUDGET: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lane {
    Raw,
    Gpu,
}

/// An offer a live lane stops making.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Tiled modifiers on the raw lanes; NVENC moves to the CUDA import.
    Tiled,
    /// Tiled 10-bit on NVENC's raw lane.
    HdrTiled,
    /// The producer's own NV12 and P010: the CPU de-pad reads one packed plane.
    Planar,
}

/// What a capture or its encoder saw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Evidence {
    Imported(Lane),
    /// Raw: the encoder refused a LINEAR dmabuf. GPU: a stream lost its import, the
    /// worker dead or its tiled imports failing.
    ImportFailed(Lane),
    /// The first frame arrived on this lane's offer.
    OfferTaken(Lane),
    /// A full first-frame wait on this lane's offer ended with no format.
    OfferTimedOut(Lane),
    Refused(Refusal),
}

/// One cause's standing: live after this many consecutive strikes, or off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rung {
    Live(u8),
    Off,
}

impl Rung {
    fn strike(self, budget: u8) -> Rung {
        match self {
            Rung::Live(n) if n + 1 < budget => Rung::Live(n + 1),
            _ => Rung::Off,
        }
    }

    fn clear(self) -> Rung {
        match self {
            Rung::Live(_) => Rung::Live(0),
            Rung::Off => Rung::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Causes {
    import: Rung,
    offer: Rung,
}

/// One identity's lanes and refusals. `Default` trusts everything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ladder {
    raw: Causes,
    gpu: Causes,
    tiled: bool,
    hdr_tiled: bool,
    planar: bool,
}

impl Default for Ladder {
    fn default() -> Self {
        let live = Causes {
            import: Rung::Live(0),
            offer: Rung::Live(0),
        };
        Ladder {
            raw: live,
            gpu: live,
            tiled: false,
            hdr_tiled: false,
            planar: false,
        }
    }
}

impl Ladder {
    fn causes(&mut self, lane: Lane) -> &mut Causes {
        match lane {
            Lane::Raw => &mut self.raw,
            Lane::Gpu => &mut self.gpu,
        }
    }

    fn standing(&self, lane: Lane) -> Causes {
        match lane {
            Lane::Raw => self.raw,
            Lane::Gpu => self.gpu,
        }
    }

    pub fn off(&self, lane: Lane) -> bool {
        self.import_off(lane) || self.offer_off(lane)
    }

    pub(super) fn import_off(&self, lane: Lane) -> bool {
        self.standing(lane).import == Rung::Off
    }

    pub(super) fn offer_off(&self, lane: Lane) -> bool {
        self.standing(lane).offer == Rung::Off
    }

    pub fn refused(&self, refusal: Refusal) -> bool {
        match refusal {
            Refusal::Tiled => self.tiled,
            Refusal::HdrTiled => self.hdr_tiled,
            Refusal::Planar => self.planar,
        }
    }

    /// NVENC's raw lane: the raw rung is live and no tiled buffer was refused. After a tiled
    /// refusal the raw offer would carry the same tiles, so NVENC takes the CUDA import.
    pub fn nvenc_raw_ok(&self) -> bool {
        !self.off(Lane::Raw) && !self.tiled
    }

    /// Which cause holds the raw lane off, for the session-open line.
    pub fn raw_state(&self) -> &'static str {
        match (self.import_off(Lane::Raw), self.offer_off(Lane::Raw)) {
            (true, true) => "latched: encoder-import + negotiation",
            (true, false) => "latched: encoder-import failures",
            (false, true) => "latched: negotiation timeouts",
            (false, false) => "live",
        }
    }
}

/// The ladder after `evidence`.
pub fn step(mut ladder: Ladder, evidence: Evidence) -> Ladder {
    let budget = |lane, import| match (lane, import) {
        (Lane::Raw, true) => RAW_IMPORT_BUDGET,
        (Lane::Raw, false) => RAW_OFFER_BUDGET,
        (Lane::Gpu, true) => GPU_IMPORT_BUDGET,
        (Lane::Gpu, false) => GPU_OFFER_BUDGET,
    };
    match evidence {
        Evidence::Imported(lane) => {
            let c = ladder.causes(lane);
            c.import = c.import.clear();
        }
        Evidence::ImportFailed(lane) => {
            let c = ladder.causes(lane);
            c.import = c.import.strike(budget(lane, true));
        }
        Evidence::OfferTaken(lane) => {
            let c = ladder.causes(lane);
            c.offer = c.offer.clear();
        }
        Evidence::OfferTimedOut(lane) => {
            let c = ladder.causes(lane);
            c.offer = c.offer.strike(budget(lane, false));
        }
        Evidence::Refused(Refusal::Tiled) => ladder.tiled = true,
        Evidence::Refused(Refusal::HdrTiled) => ladder.hdr_tiled = true,
        Evidence::Refused(Refusal::Planar) => ladder.planar = true,
    }
    ladder
}

#[cfg(test)]
mod tests {
    use super::Evidence::{ImportFailed, Imported, OfferTaken, OfferTimedOut, Refused};
    use super::Lane::{Gpu, Raw};
    use super::{step, Ladder, Refusal, Rung};

    fn after(evidence: &[super::Evidence]) -> Ladder {
        evidence.iter().fold(Ladder::default(), |l, e| step(l, *e))
    }

    fn with(edit: impl FnOnce(&mut Ladder)) -> Ladder {
        let mut l = Ladder::default();
        edit(&mut l);
        l
    }

    /// Each cause against its budget, the credit a success gives, and what nothing undoes.
    #[test]
    fn each_cause_spends_its_own_budget() {
        let rows: Vec<(&str, Vec<super::Evidence>, Ladder)> = vec![
            (
                "two raw import failures leave the lane live",
                vec![ImportFailed(Raw); 2],
                with(|l| l.raw.import = Rung::Live(2)),
            ),
            (
                "the third turns it off",
                vec![ImportFailed(Raw); 3],
                with(|l| l.raw.import = Rung::Off),
            ),
            (
                "a raw lane that fails once recovers",
                vec![
                    ImportFailed(Raw),
                    ImportFailed(Raw),
                    Imported(Raw),
                    ImportFailed(Raw),
                ],
                with(|l| l.raw.import = Rung::Live(1)),
            ),
            (
                "a lane that is off stays off",
                vec![
                    ImportFailed(Raw),
                    ImportFailed(Raw),
                    ImportFailed(Raw),
                    Imported(Raw),
                    OfferTaken(Raw),
                ],
                with(|l| l.raw.import = Rung::Off),
            ),
            (
                "one offer timeout retries",
                vec![OfferTimedOut(Raw)],
                with(|l| l.raw.offer = Rung::Live(1)),
            ),
            (
                "the second turns the raw lane off, imports untouched",
                vec![OfferTimedOut(Raw); 2],
                with(|l| l.raw.offer = Rung::Off),
            ),
            (
                "a taken offer credits the budget",
                [OfferTimedOut(Raw), OfferTaken(Raw)].repeat(10),
                Ladder::default(),
            ),
            (
                "the GPU lane takes three lost imports",
                vec![ImportFailed(Gpu); 3],
                with(|l| l.gpu.import = Rung::Off),
            ),
            (
                "a GPU lane that fails once recovers",
                vec![
                    ImportFailed(Gpu),
                    Imported(Gpu),
                    ImportFailed(Gpu),
                    ImportFailed(Gpu),
                ],
                with(|l| l.gpu.import = Rung::Live(2)),
            ),
            (
                "one GPU offer timeout is conclusive",
                vec![OfferTimedOut(Gpu)],
                with(|l| l.gpu.offer = Rung::Off),
            ),
            (
                "a refusal is one sighting and leaves the lanes live",
                vec![Refused(Refusal::Tiled), Imported(Raw)],
                with(|l| l.tiled = true),
            ),
        ];
        for (why, evidence, want) in rows {
            assert_eq!(after(&evidence), want, "{why}");
        }
    }

    /// The session-open line names the cause: "never offered" and "failed earlier" are
    /// different bugs.
    #[test]
    fn the_raw_state_names_the_cause() {
        assert_eq!(Ladder::default().raw_state(), "live");
        let offer = after(&[OfferTimedOut(Raw); 2]);
        assert_eq!(offer.raw_state(), "latched: negotiation timeouts");
        let import = after(&[ImportFailed(Raw); 3]);
        assert_eq!(import.raw_state(), "latched: encoder-import failures");
        let both = [OfferTimedOut(Raw); 2]
            .iter()
            .fold(import, |l, e| step(l, *e));
        assert_eq!(both.raw_state(), "latched: encoder-import + negotiation");
    }

    /// NVENC's raw lane needs the raw rung and no tiled refusal; the other refusals leave it.
    #[test]
    fn nvenc_raw_needs_the_raw_rung_and_no_tiled_refusal() {
        assert!(Ladder::default().nvenc_raw_ok());
        assert!(!after(&[Refused(Refusal::Tiled)]).nvenc_raw_ok());
        assert!(!after(&[ImportFailed(Raw); 3]).nvenc_raw_ok());
        assert!(!after(&[OfferTimedOut(Raw); 2]).nvenc_raw_ok());
        assert!(after(&[Refused(Refusal::HdrTiled), Refused(Refusal::Planar)]).nvenc_raw_ok());
        assert!(
            after(&[ImportFailed(Gpu); 3]).nvenc_raw_ok(),
            "the GPU lane is the plan's"
        );
    }
}
