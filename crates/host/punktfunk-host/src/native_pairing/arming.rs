//! On-demand PIN window that arms native pairing.
//!
//! Owns [`Armed`] behind a [`Mutex`]. A window is a PIN, an optional expiry, an
//! optional bound fingerprint, and an optional [`super::Access`] grant. CLI
//! `--allow-pairing` arms with no expiry; the web console arms a timed window.
//! A bound fingerprint rejects other clients without consuming the PIN.
//!
//! Pin this via [`ArmState`]. Pairing ceremony tests live on the [`super`] facade.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// PIN window. `pin == None` is disarmed. `expires_at == None` is CLI
/// `--allow-pairing` (no auto-disarm). `bound_fp == Some` rejects other
/// fingerprints without consuming the PIN.
#[derive(Default)]
struct Armed {
    pin: Option<String>,
    expires_at: Option<Instant>,
    bound_fp: Option<String>,
    /// Applied to the device that completes this window. `None` is full/permanent.
    /// Disarm/expiry wipes it; the ceremony must read this before consuming the PIN.
    access: Option<super::Access>,
}

pub enum PinAttempt {
    Disarmed,
    /// Bound to a different fingerprint. Reject without consuming the PIN.
    BoundToOther,
    /// Armed for anyone, and this knock came from the internet. Reject without consuming
    /// the PIN: a window the operator opened for the couch must not answer the WAN.
    UnboundForWan,
    Pin(String),
}

fn random_pin() -> String {
    use rand::RngExt;
    format!("{:04}", rand::rng().random_range(0..10_000u32))
}

/// Management-API snapshot: `(armed, pin, expires_in_secs)`.
pub(super) type ArmSnapshot = (bool, Option<String>, Option<u64>);

pub(super) struct ArmState {
    arm: Mutex<Armed>,
}

impl ArmState {
    /// Disarmed unless `arm_at_start`: then a PIN with no expiry (CLI `--allow-pairing`).
    pub(super) fn new(arm_at_start: bool, fixed_pin: Option<String>) -> ArmState {
        let arm = if arm_at_start {
            Armed {
                pin: Some(fixed_pin.unwrap_or_else(random_pin)),
                expires_at: None,
                bound_fp: None,
                access: None,
            }
        } else {
            Armed::default()
        };
        ArmState {
            arm: Mutex::new(arm),
        }
    }

    /// Arm a timed PIN. A bound fingerprint is the only client that can consume it.
    /// `access` is applied on success (`None` = full/permanent).
    pub(super) fn arm_for(
        &self,
        ttl: Duration,
        bound_fp: Option<String>,
        access: Option<super::Access>,
    ) -> String {
        let pin = random_pin();
        *self.arm.lock().unwrap() = Armed {
            pin: Some(pin.clone()),
            expires_at: Some(Instant::now() + ttl),
            bound_fp,
            access,
        };
        pin
    }

    /// Window access, or `None` if disarmed/expired. Read before consuming the PIN.
    pub(super) fn armed_access(&self) -> Option<super::Access> {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        arm.access
    }

    /// PIN for this fingerprint knocking from `source`, or a refusal that does not consume the
    /// window. Binding and PIN are read under one lock, so a re-arm cannot land between them.
    pub(super) fn pin_for_attempt(
        &self,
        client_fp_hex: &str,
        source: super::KnockSource,
    ) -> PinAttempt {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        match &arm.pin {
            None => PinAttempt::Disarmed,
            Some(pin) => match &arm.bound_fp {
                Some(bound) if !bound.eq_ignore_ascii_case(client_fp_hex) => {
                    PinAttempt::BoundToOther
                }
                // An open window answers whoever knocks first. On the LAN that is the device
                // in the operator's hands; from the internet it is whoever found the port, so
                // a WAN ceremony needs a window named for its fingerprint.
                None if source == super::KnockSource::Wan => PinAttempt::UnboundForWan,
                _ => PinAttempt::Pin(pin.clone()),
            },
        }
    }

    pub(super) fn disarm(&self) {
        *self.arm.lock().unwrap() = Armed::default();
    }

    /// Drop a timed window whose deadline passed. Call under the lock before any read.
    fn expire(arm: &mut Armed) {
        if let Some(t) = arm.expires_at {
            if Instant::now() >= t {
                *arm = Armed::default();
            }
        }
    }

    /// Live PIN, or `None` if disarmed/expired. Re-read per attempt so a lapsed window cannot pair.
    pub(super) fn current_pin(&self) -> Option<String> {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        arm.pin.clone()
    }

    pub(super) fn snapshot(&self) -> ArmSnapshot {
        let mut arm = self.arm.lock().unwrap();
        Self::expire(&mut arm);
        let expires_in_secs = arm
            .expires_at
            .map(|t| t.saturating_duration_since(Instant::now()).as_secs());
        (arm.pin.is_some(), arm.pin.clone(), expires_in_secs)
    }
}
