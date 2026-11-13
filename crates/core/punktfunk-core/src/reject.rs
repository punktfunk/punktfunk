//! Typed QUIC application close codes and the [`RejectReason`] vocabulary a
//! host uses to turn a connection away. Lives outside the `quic` feature
//! because [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected)
//! carries it in every build; `crate::quic` re-exports it.

use std::time::Duration;

/// `mode_conflict = reject` admission close. Distinct from a transport
/// failure so a client can render "host busy". The reason bytes are empty: a
/// client words `reject_busy`, and the live mode stays in the host's log.
pub const REJECT_BUSY_CLOSE_CODE: u32 = 0x42;

/// Pairing-gate close. Occupies 0x60.., disjoint from
/// [`REJECT_BUSY_CLOSE_CODE`] (0x42) and the deliberate-end codes (0x51/0x52).
/// Decode with [`RejectReason::from_close_code`].
pub const PAIR_NOT_ARMED_CLOSE_CODE: u32 = 0x60;
/// Armed window is bound to a different fingerprint; the attempt does not
/// consume it.
pub const PAIR_BOUND_OTHER_CLOSE_CODE: u32 = 0x61;
/// Inside the host's global pairing cooldown.
pub const PAIR_RATE_LIMITED_CLOSE_CODE: u32 = 0x62;
/// No client certificate: SPAKE2 has nothing to bind.
pub const PAIR_NO_IDENTITY_CLOSE_CODE: u32 = 0x63;
pub const PAIR_DENIED_CLOSE_CODE: u32 = 0x64;
pub const PAIR_APPROVAL_TIMEOUT_CLOSE_CODE: u32 = 0x65;
/// Only the newest knock from this device is admitted.
pub const PAIR_SUPERSEDED_CLOSE_CODE: u32 = 0x66;
pub const WIRE_VERSION_CLOSE_CODE: u32 = 0x67;
/// Admitted, then compositor / capture / encoder setup failed. Reason bytes
/// carry the host error; clients render a stable "host-side failure" sentence.
pub const SETUP_FAILED_CLOSE_CODE: u32 = 0x68;
/// Per-client access deadline, or console "Expire now". Only this device's
/// sessions close; a reconnect parks in the pending list.
/// `design/per-client-access.md`.
pub const ACCESS_EXPIRED_CLOSE_CODE: u32 = 0x69;
/// `Hello.launch` named a game this device's grants lack the `LAUNCH` bit
/// for. Refused at handshake; connecting without a launch request still works.
pub const LAUNCH_NOT_PERMITTED_CLOSE_CODE: u32 = 0x6A;
/// Host power action (`power.sleep` / `reboot` / `shutdown`) is ending every
/// session. `design/host-actions.md`.
pub const HOST_POWER_CLOSE_CODE: u32 = 0x6B;
/// `ClientHello.profile` names no profile on this host, or another seat's.
pub const PROFILE_UNKNOWN_CLOSE_CODE: u32 = 0x6C;
/// Every seat is taken. Reason bytes name the occupants.
pub const NO_SEAT_CLOSE_CODE: u32 = 0x6D;
/// The profile's seat is in use by another device.
pub const SEAT_OCCUPIED_CLOSE_CODE: u32 = 0x6E;
/// The profile's seat can't run: removed, or its host would not start.
pub const SEAT_UNAVAILABLE_CLOSE_CODE: u32 = 0x6F;
/// Not a rejection: the answer to an access-only hello. This device may connect now.
/// 0x70 and 0x71 are stream codes.
pub const ACCESS_GRANTED_CLOSE_CODE: u32 = 0x72;
/// The profile needs an unlock this connect did not carry, or the host refused the one it
/// did. Reason bytes carry the sentence to prompt with; empty asks for the password. 0x73 is
/// `PATH_SHRANK_CLOSE_CODE`.
pub const PROFILE_LOCKED_CLOSE_CODE: u32 = 0x74;

/// One row per [`RejectReason`]: variant, close code, FFI token, sentence. Every
/// table below comes from these rows, so a new reason cannot miss one.
macro_rules! reject_reasons {
    ($($variant:ident = $code:ident, $token:literal, $text:literal;)*) => {
        /// Client-side view of the host's QUIC application close code. Surfaces as
        /// [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected).
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum RejectReason {
            $($variant,)*
        }

        impl RejectReason {
            /// Every reason, in table order.
            pub const ALL: &[RejectReason] = &[$(Self::$variant),*];

            /// `None` for codes outside this vocabulary — a bare/legacy close stays a
            /// transport error.
            pub fn from_close_code(code: u32) -> Option<Self> {
                match code {
                    $($code => Some(Self::$variant),)*
                    _ => None,
                }
            }

            /// Inverse of [`Self::from_close_code`].
            pub fn close_code(self) -> u32 {
                match self {
                    $(Self::$variant => $code,)*
                }
            }

            /// Stable kebab-case token for FFI (Android JNI). Do not reword — clients
            /// match on these.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $token,)*
                }
            }

            /// The host message an app words this refusal with: `reject_` and the token,
            /// `-` written `_`.
            pub fn message_id(self) -> String {
                format!("reject_{}", self.as_str().replace('-', "_"))
            }

            /// Inverse of [`Self::as_str`]. `None` for a token this build doesn't know.
            pub fn from_token(token: &str) -> Option<Self> {
                match token {
                    $($token => Some(Self::$variant),)*
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for RejectReason {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(match self {
                    $(Self::$variant => $text,)*
                })
            }
        }
    };
}

reject_reasons! {
    PairingNotArmed = PAIR_NOT_ARMED_CLOSE_CODE, "not-armed",
        "pairing is not armed on the host";
    PairingBoundToOtherDevice = PAIR_BOUND_OTHER_CLOSE_CODE, "bound-other",
        "the host's pairing window is armed for a different device";
    PairingRateLimited = PAIR_RATE_LIMITED_CLOSE_CODE, "rate-limited",
        "pairing attempts are rate-limited — retry shortly";
    IdentityRequired = PAIR_NO_IDENTITY_CLOSE_CODE, "identity-required",
        "the host requires a client identity";
    Denied = PAIR_DENIED_CLOSE_CODE, "denied",
        "the request was denied on the host";
    ApprovalTimeout = PAIR_APPROVAL_TIMEOUT_CLOSE_CODE, "approval-timeout",
        "nobody approved the request on the host in time";
    Superseded = PAIR_SUPERSEDED_CLOSE_CODE, "superseded",
        "a newer request from this device replaced this one";
    WireVersionMismatch = WIRE_VERSION_CLOSE_CODE, "wire-version",
        "client and host versions do not match";
    Busy = REJECT_BUSY_CLOSE_CODE, "busy",
        "the host is busy with another session";
    SetupFailed = SETUP_FAILED_CLOSE_CODE, "setup-failed",
        "the host could not start the stream session";
    AccessExpired = ACCESS_EXPIRED_CLOSE_CODE, "access-expired",
        "your access to this host has expired";
    LaunchNotPermitted = LAUNCH_NOT_PERMITTED_CLOSE_CODE, "launch-not-permitted",
        "this device is not permitted to launch games on the host";
    HostPower = HOST_POWER_CLOSE_CODE, "host-power",
        "the host is going to sleep or shutting down";
    ProfileUnknown = PROFILE_UNKNOWN_CLOSE_CODE, "profile-unknown",
        "that profile is gone from this host — pick another one";
    NoSeat = NO_SEAT_CLOSE_CODE, "no-seat",
        "all seats are taken";
    SeatOccupied = SEAT_OCCUPIED_CLOSE_CODE, "seat-occupied",
        "someone is already playing as that profile";
    SeatUnavailable = SEAT_UNAVAILABLE_CLOSE_CODE, "seat-unavailable",
        "that profile's seat isn't available on this host";
    ProfileLocked = PROFILE_LOCKED_CLOSE_CODE, "profile-locked",
        "that profile is locked until its password is given";
}

/// How long a host that answers but can't stream yet is dialed again, from its first refusal.
/// Room for a resumed display and GPU; a host that stays broken still says so within 20 s.
pub const WAKE_SETTLE: Duration = Duration::from_secs(20);
/// Between those dials: a refusal can come back at once.
pub const WAKE_REDIAL_PAUSE: Duration = Duration::from_secs(2);

/// The wait before dialing again a host that refused as `refused`, `streamed` saying whether a
/// frame came first, `since_first` after its first such refusal (`None`: this is the first).
/// `None` means show the refusal. A host that just woke answers before it can stream, so with
/// auto-wake on a setup refusal before any frame dials again until [`WAKE_SETTLE`] has passed.
/// Every shell dials a waking host on this rule.
pub fn wake_redial_delay(
    auto_wake: bool,
    refused: Option<RejectReason>,
    streamed: bool,
    since_first: Option<Duration>,
) -> Option<Duration> {
    let waking = auto_wake && since_first.is_none_or(|t| t < WAKE_SETTLE);
    let not_ready = refused == Some(RejectReason::SetupFailed) && !streamed;
    (waking && not_ready).then_some(WAKE_REDIAL_PAUSE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_codes_round_trip() {
        for &r in RejectReason::ALL {
            assert_eq!(RejectReason::from_close_code(r.close_code()), Some(r));
            assert_eq!(RejectReason::from_token(r.as_str()), Some(r));
        }
        assert_eq!(RejectReason::from_token("from-a-newer-host"), None);
    }

    #[test]
    fn codes_are_unique() {
        let mut codes: Vec<u32> = RejectReason::ALL.iter().map(|r| r.close_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), RejectReason::ALL.len());
    }

    #[test]
    fn foreign_codes_stay_untyped() {
        // Bare closes, pair-done, and 0x51/0x52 (deliberate-end) must never
        // decode as a rejection. The 0x60 block is full; 0x70 is the clipboard's.
        for code in [
            0u32,
            1,
            0x41,
            0x51,
            0x52,
            0x5f,
            0x70,
            ACCESS_GRANTED_CLOSE_CODE,
            u32::MAX,
        ] {
            assert_eq!(RejectReason::from_close_code(code), None);
        }
    }

    /// Only a setup refusal before any frame, with auto-wake on, dials again, and only for
    /// [`WAKE_SETTLE`] from the first one.
    #[test]
    fn a_waking_host_is_dialed_again_inside_its_window() {
        let setup = Some(RejectReason::SetupFailed);
        let pause = Some(WAKE_REDIAL_PAUSE);
        assert_eq!(wake_redial_delay(true, setup, false, None), pause);
        let inside = Some(WAKE_SETTLE - Duration::from_millis(1));
        assert_eq!(wake_redial_delay(true, setup, false, inside), pause);
        assert_eq!(
            wake_redial_delay(true, setup, false, Some(WAKE_SETTLE)),
            None
        );
        assert_eq!(wake_redial_delay(false, setup, false, None), None);
        assert_eq!(
            wake_redial_delay(true, setup, true, None),
            None,
            "it streamed"
        );
        for other in [
            None,
            Some(RejectReason::Busy),
            Some(RejectReason::AccessExpired),
        ] {
            assert_eq!(wake_redial_delay(true, other, false, None), None);
        }
    }
}
