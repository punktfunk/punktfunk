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
    use punktfunk_core::reject::RejectReason as R;
    match reason {
        R::Denied => "The host declined this device's request.".into(),
        R::ApprovalTimeout => {
            "Nobody approved the request on the host in time — approve this device in the \
             host's console or web UI, then request access again."
                .into()
        }
        R::Superseded => {
            "A newer request from this device replaced this one — approve the latest request \
             on the host."
                .into()
        }
        R::IdentityRequired => {
            "The host requires pairing — pair this device (PIN or request access) first.".into()
        }
        R::PairingNotArmed => {
            "Pairing isn't armed on the host — arm it on the host's Pairing page, then try \
             again."
                .into()
        }
        R::PairingBoundToOtherDevice => {
            "The host's pairing window is armed for a different device — arm it for this one."
                .into()
        }
        R::PairingRateLimited => {
            "Too many pairing attempts — wait a couple of seconds and try again.".into()
        }
        R::WireVersionMismatch => {
            "Client and host versions don't match — update both to the same release.".into()
        }
        R::Busy => "The host is busy with another session.".into(),
        R::SetupFailed => {
            "The host accepted the connection but couldn't start the stream — the host's log \
             (web console → Log) has the cause."
                .into()
        }
        R::AccessExpired => {
            "Your access to this host has expired — ask the host's owner to grant it again.".into()
        }
        R::LaunchNotPermitted => {
            "This device isn't permitted to launch games on the host — connect without picking \
             a game, or ask the host's owner to allow launching."
                .into()
        }
        R::HostPower => {
            "The host is going to sleep or shutting down — wake it when you want to play again."
                .into()
        }
        R::ProfileUnknown => "That profile is gone from this host. Pick another one.".into(),
        R::NoSeat => "All seats are taken.".into(),
        R::SeatOccupied => "Someone is already playing as that profile.".into(),
        R::SeatUnavailable => "That profile can't play on this host right now.".into(),
    }
}

/// User-facing sentence for a failed [`pair_with_host`](super::pair_with_host). Crypto is a
/// wrong PIN; do not report a dead path or a disarmed host as one.
pub fn pair_error_message(err: &punktfunk_core::PunktfunkError) -> String {
    use punktfunk_core::PunktfunkError as E;
    match err {
        E::Crypto => "Wrong PIN — check the PIN on the host's Pairing page and try again.".into(),
        E::Rejected(reason) => connect_reject_message(*reason),
        E::Timeout => "The host didn't answer. Is it running and reachable?".into(),
        E::Io(_) => {
            "Couldn't reach the host — check that this device and the host are on the same \
             network (no VPN on this device, no guest-Wi-Fi / AP isolation)."
                .into()
        }
        other => {
            tracing::warn!(error = %other, "pairing failed");
            "Pairing didn't finish — try again from the host's Pairing page.".to_string()
        }
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
