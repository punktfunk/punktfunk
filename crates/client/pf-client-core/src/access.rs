//! Client-side session access: grant mask, derived preset label, overlay chip, toast.
//!
//! One snapshot over the shared grant vocabulary. The preset name is derived from
//! the mask, never stored (`design/per-client-access.md`). Mid-session
//! [`AccessUpdate`](punktfunk_core::quic::AccessUpdate) replaces the snapshot
//! (latest wins). Apple and Android clients copy these labels and the
//! derive-not-store rule rather than linking this crate.
//!
//! Presentation only. Tests in this module pin the labels, chip text, and notices.

use punktfunk_core::quic::{
    normalize_legacy_full, GRANT_ALL, GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_VIEW_ONLY,
};
use std::time::{Duration, Instant};

/// Current-build grant vocabulary: legacy pre-power Full becomes `GRANT_ALL`,
/// and bits this build does not know are dropped so a newer host's Full
/// session never labels as "Custom".
fn effective_mask(grants: u32) -> u32 {
    normalize_legacy_full(grants) & GRANT_ALL
}

/// Client snapshot of the host [`Welcome`](punktfunk_core::quic::Welcome)
/// advert, replaced by each [`AccessUpdate`](punktfunk_core::quic::AccessUpdate)
/// (latest wins). Default is full and permanent: an old host's Welcome
/// renders with no chip and every control enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionAccess {
    /// Grant bitmask ([`punktfunk_core::quic::GRANT_GAMEPAD`] family).
    pub grants: u32,
    /// When this access ends, on the CLIENT's monotonic clock; `None` = permanent.
    /// Monotonic so the chip's countdown never jumps with a wall-clock step.
    pub deadline: Option<Instant>,
}

impl Default for SessionAccess {
    fn default() -> Self {
        SessionAccess {
            grants: GRANT_ALL,
            deadline: None,
        }
    }
}

impl SessionAccess {
    /// Grants plus deadline from the connector, moved onto this process's monotonic clock.
    pub fn from_connector(c: &punktfunk_core::client::NativeClient) -> SessionAccess {
        let deadline = match c.access_expires_in_secs() {
            0 => None,
            secs => Some(Instant::now() + Duration::from_secs(secs.into())),
        };
        SessionAccess {
            grants: c.access_grants(),
            deadline,
        }
    }

    pub fn allows(&self, bit: u32) -> bool {
        self.grants & bit != 0
    }

    /// Full control and no deadline. Compared through [`effective_mask`] so neither
    /// a legacy pre-power full mask nor unknown future bits put a chip on Full.
    pub fn is_default(&self) -> bool {
        effective_mask(self.grants) == GRANT_ALL && self.deadline.is_none()
    }

    /// Time left; `None` is permanent, zero is already due.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(now))
    }

    /// Overlay chip text (`"Controller only · ends in 1 h 58 m"`), or `None` when
    /// [`Self::is_default`] — no chip on a full permanent session.
    pub fn chip_text(&self, now: Instant) -> Option<String> {
        if self.is_default() {
            return None;
        }
        let label = preset_label(self.grants);
        match self.remaining(now) {
            Some(left) => Some(format!("{label} · ends in {}", format_remaining(left))),
            None => Some(label.to_string()),
        }
    }
}

/// Preset name derived from the mask, never stored. Matches on [`effective_mask`]
/// so a legacy or future Full mask still labels "Full control"; anything off
/// the three presets is "Custom".
pub fn preset_label(grants: u32) -> &'static str {
    match effective_mask(grants) {
        GRANT_ALL => "Full control",
        GRANT_PRESET_CONTROLLER_ONLY => "Controller only",
        GRANT_PRESET_VIEW_ONLY => "View only",
        _ => "Custom",
    }
}

/// Chip/toast remaining-time text. Sub-minute is `"under 1 m"`: the wire
/// carries whole seconds, so a second-level countdown would overclaim.
pub fn format_remaining(left: Duration) -> String {
    let mins = left.as_secs() / 60;
    match (mins / 60, mins % 60) {
        (0, 0) => "under 1 m".to_string(),
        (0, m) => format!("{m} m"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} m"),
    }
}

/// Toast for a mid-session access change. A grants edit names the new level;
/// same grants with a deadline is the host's T−5 m / T−1 m warning.
/// `None` if the mask was reaffirmed and is still permanent.
pub fn update_notice(prev_grants: u32, next: &SessionAccess, now: Instant) -> Option<String> {
    if next.grants != prev_grants {
        return Some(format!("Access is now {}", preset_label(next.grants)));
    }
    match next.remaining(now) {
        Some(left) if left > Duration::ZERO => {
            Some(format!("Access ends in {}", format_remaining(left)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::quic::{GRANT_CLIPBOARD, GRANT_GAMEPAD, GRANT_KEYBOARD, GRANT_POINTER};

    #[test]
    fn labels_derive_from_the_mask_per_the_design() {
        assert_eq!(preset_label(GRANT_ALL), "Full control");
        assert_eq!(preset_label(GRANT_GAMEPAD), "Controller only");
        assert_eq!(preset_label(0), "View only");
        assert_eq!(preset_label(GRANT_GAMEPAD | GRANT_CLIPBOARD), "Custom");
        assert_eq!(preset_label(GRANT_ALL & !GRANT_KEYBOARD), "Custom");
        assert_eq!(
            preset_label(punktfunk_core::quic::GRANT_ALL_PRE_POWER),
            "Full control"
        );
        assert_eq!(preset_label(GRANT_ALL | (1 << 20)), "Full control");
    }

    /// The level core writes for each mask; the web console and Kotlin replay the same file.
    #[test]
    fn labels_match_the_core_grant_vectors() {
        let raw = include_str!("../../../core/punktfunk-core/testdata/grant-vectors.json");
        let file: serde_json::Value = serde_json::from_str(raw).expect("grant-vectors.json parses");
        for case in file["masks"].as_array().expect("masks array") {
            let mask = case["mask"].as_u64().expect("mask") as u32;
            let want = match case["level"].as_str().expect("level") {
                "full" => "Full control",
                "controller" => "Controller only",
                "view" => "View only",
                _ => "Custom",
            };
            assert_eq!(preset_label(mask), want, "mask {mask:#x}");
        }
    }

    #[test]
    fn the_default_session_wears_no_chip() {
        let now = Instant::now();
        assert!(SessionAccess::default().is_default());
        assert_eq!(SessionAccess::default().chip_text(now), None);
        let limited = SessionAccess {
            grants: GRANT_GAMEPAD,
            deadline: None,
        };
        assert_eq!(limited.chip_text(now).as_deref(), Some("Controller only"));
        let expiring = SessionAccess {
            grants: GRANT_ALL,
            deadline: Some(now + Duration::from_secs(2 * 3600 - 120)),
        };
        assert_eq!(
            expiring.chip_text(now).as_deref(),
            Some("Full control · ends in 1 h 58 m")
        );
    }

    #[test]
    fn remaining_time_formats_at_honest_granularity() {
        assert_eq!(format_remaining(Duration::from_secs(0)), "under 1 m");
        assert_eq!(format_remaining(Duration::from_secs(59)), "under 1 m");
        assert_eq!(format_remaining(Duration::from_secs(60)), "1 m");
        assert_eq!(format_remaining(Duration::from_secs(58 * 60)), "58 m");
        assert_eq!(format_remaining(Duration::from_secs(2 * 3600)), "2 h");
        assert_eq!(
            format_remaining(Duration::from_secs(3600 + 58 * 60 + 30)),
            "1 h 58 m"
        );
    }

    #[test]
    fn notices_name_a_grants_change_first_and_warnings_by_time_left() {
        let now = Instant::now();
        // Grants change wins over a running deadline: name the new level.
        let narrowed = SessionAccess {
            grants: GRANT_GAMEPAD,
            deadline: Some(now + Duration::from_secs(300)),
        };
        assert_eq!(
            update_notice(GRANT_ALL, &narrowed, now).as_deref(),
            Some("Access is now Controller only")
        );
        let warned = SessionAccess {
            grants: GRANT_GAMEPAD,
            deadline: Some(now + Duration::from_secs(300)),
        };
        assert_eq!(
            update_notice(GRANT_GAMEPAD, &warned, now).as_deref(),
            Some("Access ends in 5 m")
        );
        let same = SessionAccess {
            grants: GRANT_POINTER,
            deadline: None,
        };
        assert_eq!(update_notice(GRANT_POINTER, &same, now), None);
    }
}
