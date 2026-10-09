//! Deterministic decoder-input damage for integrity tests.
//!
//! [`AuFault::from_spec`] parses `PUNKTFUNK_AU_FAULT=<mode>[:<period>]`.
//! Invalid or zero-period specs stay inert. Default period is 60.
//! Every `period`-th offered AU is selected, counting from the first.
//! AUs shorter than 16 bytes pass unchanged.
//!
//! `Drop` withholds the AU. `Truncate` keeps the first three quarters.
//! `Flip` XORs one payload byte and may stay valid syntax, so neither planner
//! nor driver is guaranteed to report it. Missing `queryResultStatusSupport`
//! is unmeasured, not clean. Unselected AUs are borrowed; owned bytes exist
//! only on the selected call. Runs after `PUNKTFUNK_AU_DUMP`, so dumps stay
//! clean wire input. Evidence: `tests/fault_detection.rs`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultMode {
    /// Network loss: the decoder never sees the AU.
    Drop,
    /// Mid-picture cut, not a refuse-at-byte-0.
    Truncate,
    /// In-picture corruption no parser can see.
    Flip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaultAction {
    Pass,
    Drop,
    /// Owned because the damaged bytes are not a borrow of the input.
    /// Allocated only on the faulted AU.
    Corrupt(Vec<u8>),
}

/// 60 fps → one fault per second. Sparse enough that recovery finishes
/// between faults instead of freezing the stream.
pub const DEFAULT_FAULT_PERIOD: u32 = 60;

/// Three quarters in: past parameter sets and the first slice header, so
/// `Flip` stays parser-invisible and `Truncate` cuts mid-picture. A fraction
/// so it scales from a small P-frame to a large IDR.
const FAULT_POINT_NUMERATOR: usize = 3;
const FAULT_POINT_DENOMINATOR: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct AuFault {
    mode: FaultMode,
    period: u32,
    seen: u32,
}

impl AuFault {
    /// Parse `PUNKTFUNK_AU_FAULT=<mode>[:<period>]`. `None` leaves the injector
    /// inert — a typo must not half-arm a live stream.
    pub fn from_spec(spec: &str) -> Option<AuFault> {
        let spec = spec.trim();
        let (mode, period) = match spec.split_once(':') {
            Some((m, p)) => (m.trim(), p.trim().parse::<u32>().ok()?),
            None => (spec, DEFAULT_FAULT_PERIOD),
        };
        // Zero would fault every AU, including the opening IDR, so there is no
        // stream left to damage.
        if period == 0 {
            return None;
        }
        let mode = match mode {
            "drop" => FaultMode::Drop,
            "truncate" => FaultMode::Truncate,
            "flip" => FaultMode::Flip,
            _ => return None,
        };
        Some(AuFault {
            mode,
            period,
            seen: 0,
        })
    }

    pub fn new(mode: FaultMode, period: u32) -> AuFault {
        AuFault {
            mode,
            period: period.max(1),
            seen: 0,
        }
    }

    pub fn mode(&self) -> FaultMode {
        self.mode
    }

    pub fn period(&self) -> u32 {
        self.period
    }

    /// Counter advances on every call, faulted or not, so cadence is a
    /// property of the stream, not of the damage.
    pub fn apply(&mut self, au: &[u8]) -> FaultAction {
        self.seen = self.seen.wrapping_add(1);
        if self.seen % self.period != 0 {
            return FaultAction::Pass;
        }
        // Under 16 bytes is a parameter-set AU or a fragment. Damaging it tests
        // parser bounds, not decoder integrity. Pass and let the next multiple
        // carry the fault.
        if au.len() < 16 {
            return FaultAction::Pass;
        }
        let point = au.len() * FAULT_POINT_NUMERATOR / FAULT_POINT_DENOMINATOR;
        match self.mode {
            FaultMode::Drop => FaultAction::Drop,
            FaultMode::Truncate => FaultAction::Corrupt(au[..point].to_vec()),
            FaultMode::Flip => {
                let mut bytes = au.to_vec();
                // XOR 0x40 rather than invert: moves entropy-coded samples without
                // being likely to produce a `00 00 01` start code from surrounding bytes
                // (payload corruption would become a framing test).
                bytes[point] ^= 0x40;
                FaultAction::Corrupt(bytes)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unset or mistyped specs must stay inert so a live stream is not
    /// damaged by a typo'd environment variable.
    #[test]
    fn only_a_well_formed_spec_arms_the_injector() {
        assert_eq!(
            AuFault::from_spec("drop").map(|f| (f.mode(), f.period())),
            Some((FaultMode::Drop, DEFAULT_FAULT_PERIOD))
        );
        assert_eq!(
            AuFault::from_spec("truncate:5").map(|f| (f.mode(), f.period())),
            Some((FaultMode::Truncate, 5))
        );
        assert_eq!(
            AuFault::from_spec(" flip : 30 ").map(|f| (f.mode(), f.period())),
            Some((FaultMode::Flip, 30))
        );
        for bad in [
            "", "1", "off", "drop:", "drop:0", "drop:x", "corrupt", "flip:-1", ":30",
        ] {
            assert!(AuFault::from_spec(bad).is_none(), "{bad:?} must not arm");
        }
    }

    /// Count starts at the first offered AU, so `period > 1` never faults
    /// the opening parameter sets or IDR.
    #[test]
    fn every_nth_au_is_faulted_and_the_rest_pass_through_untouched() {
        let au = vec![0xA5u8; 64];
        let mut f = AuFault::new(FaultMode::Drop, 3);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Drop);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Pass);
        assert_eq!(f.apply(&au), FaultAction::Drop);
    }

    /// Prefix, not a zero-length AU (that is a different failure).
    #[test]
    fn truncation_delivers_a_prefix_of_the_original() {
        let au: Vec<u8> = (0..100u8).collect();
        let mut f = AuFault::new(FaultMode::Truncate, 1);
        let FaultAction::Corrupt(short) = f.apply(&au) else {
            panic!("truncate must corrupt");
        };
        assert_eq!(short.len(), 75, "three quarters of the AU survive");
        assert_eq!(short[..], au[..75], "and they are the ORIGINAL bytes");
    }

    /// Exactly one deep payload byte, deterministically — parser-invisible
    /// damage that the spec string alone can replay.
    #[test]
    fn a_flip_changes_exactly_one_deep_payload_byte_and_is_reproducible() {
        let au: Vec<u8> = (0..=255u8).collect();
        let run = || {
            let mut f = AuFault::new(FaultMode::Flip, 1);
            match f.apply(&au) {
                FaultAction::Corrupt(bytes) => bytes,
                other => panic!("flip must corrupt, got {other:?}"),
            }
        };
        let bytes = run();
        assert_eq!(
            bytes.len(),
            au.len(),
            "length is untouched — this is not a cut"
        );
        let differing: Vec<usize> = (0..au.len()).filter(|&i| bytes[i] != au[i]).collect();
        assert_eq!(differing.len(), 1, "exactly one byte moves");
        let at = differing[0];
        assert_eq!(at, 192, "three quarters in — past the headers");
        assert_eq!(bytes[at], au[at] ^ 0x40);
        assert_eq!(run(), bytes, "the same spec produces the same damage");
    }

    /// Modes damage a picture; cutting a handful of bytes only tests the
    /// parser's bounds checks.
    #[test]
    fn an_au_too_short_to_damage_meaningfully_is_left_alone() {
        let mut f = AuFault::new(FaultMode::Truncate, 1);
        assert_eq!(f.apply(&[0u8; 8]), FaultAction::Pass);
        // Counter still advanced, so cadence does not stall on a tiny AU.
        assert!(matches!(f.apply(&[0u8; 64]), FaultAction::Corrupt(_)));
    }
}
