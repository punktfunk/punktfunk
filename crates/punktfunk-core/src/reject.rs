//! Typed QUIC application close codes and the [`RejectReason`] vocabulary a
//! host uses to turn a connection away. Lives outside the `quic` feature
//! because [`PunktfunkError::Rejected`](crate::error::PunktfunkError::Rejected)
//! carries it in every build; `crate::quic` re-exports it.

/// `mode_conflict = reject` admission close. Distinct from a transport
/// failure so a client can render "host busy". Reason bytes carry live mode
/// + client label.
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
            #[cfg(test)]
            const ALL: &[RejectReason] = &[$(Self::$variant),*];

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
        for code in [0u32, 1, 0x41, 0x51, 0x52, 0x5f, 0x70, u32::MAX] {
            assert_eq!(RejectReason::from_close_code(code), None);
        }
    }
}
