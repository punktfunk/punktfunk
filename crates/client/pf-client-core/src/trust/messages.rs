//! User-facing sentences for a host's typed rejection and a failed pairing.

/// The sentence to show for a typed rejection: the host's own words when it sent
/// any, else [`connect_reject_message`] for the code.
///
/// A host knows things no client can — which monitor it was told to capture, and
/// that it no longer has one by that name. Only a host that had nothing specific
/// to say leaves the text empty, and then the generic line is the better one.
pub fn reject_message(reason: punktfunk_core::reject::RejectReason, said: Option<&str>) -> String {
    match said {
        Some(s) if !s.trim().is_empty() => s.to_string(),
        _ => connect_reject_message(reason),
    }
}

/// User-facing sentence for a typed host rejection, shared by every desktop/console
/// surface so "declined" never renders as "timed out". The caller words other errors.
pub fn connect_reject_message(reason: punktfunk_core::reject::RejectReason) -> String {
    use pf_l10n::host as m;
    use punktfunk_core::reject::RejectReason as R;
    match reason {
        R::Denied => m::reject_denied(),
        R::ApprovalTimeout => m::reject_approval_timeout(),
        R::Superseded => m::reject_superseded(),
        R::IdentityRequired => m::reject_identity_required(),
        R::PairingNotArmed => m::reject_not_armed(),
        R::PairingBoundToOtherDevice => m::reject_bound_other(),
        R::PairingRateLimited => m::reject_rate_limited(),
        R::WireVersionMismatch => m::reject_wire_version(),
        R::Busy => m::reject_busy(),
        R::SetupFailed => m::reject_setup_failed(),
        R::AccessExpired => m::reject_access_expired(),
        R::LaunchNotPermitted => m::reject_launch_not_permitted(),
        R::HostPower => m::reject_host_power(),
        R::ProfileUnknown => m::reject_profile_unknown(),
        R::NoSeat => m::reject_no_seat(),
        R::SeatOccupied => m::reject_seat_occupied(),
        R::SeatUnavailable => m::reject_seat_unavailable(),
        R::ProfileLocked => m::reject_profile_locked(),
    }
    .into()
}

/// User-facing sentence for a failed [`pair_with_host`](super::pair_with_host). Crypto is a
/// wrong PIN; do not report a dead path or a disarmed host as one.
pub fn pair_error_message(err: &punktfunk_core::PunktfunkError) -> String {
    use punktfunk_core::PunktfunkError as E;
    match err {
        E::Crypto => pf_l10n::client::pair_wrong_pin().into(),
        E::Rejected(reason, said) => reject_message(*reason, Some(said)),
        E::Timeout => pf_l10n::client::error_host_timeout().into(),
        E::Io(_) => pf_l10n::client::error_host_unreachable().into(),
        other => {
            tracing::warn!(error = %other, "pairing failed");
            pf_l10n::client::pair_failed().into()
        }
    }
}

/// One sentence for a failed access request. A request has no PIN, so a crypto failure is
/// the host not matching its advert.
pub fn access_error_message(err: &punktfunk_core::PunktfunkError) -> String {
    match err {
        punktfunk_core::PunktfunkError::Crypto => pf_l10n::client::pair_identity_mismatch().into(),
        other => pair_error_message(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_own_words_outrank_our_generic_line() {
        use punktfunk_core::reject::RejectReason as R;
        let said = "The host is set to capture a monitor called \"HDMI-A-3\", which isn't \
                    connected to it.";
        assert_eq!(reject_message(R::SetupFailed, Some(said)), said);

        // No sentence, or nothing but space: ours is the better line.
        for empty in [None, Some(""), Some("   ")] {
            assert_eq!(
                reject_message(R::SetupFailed, empty),
                connect_reject_message(R::SetupFailed),
                "{empty:?}"
            );
        }
    }
}
