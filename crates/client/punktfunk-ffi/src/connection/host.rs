//! What the host granted and said: management port, capabilities, grants, access
//! expiry, launch notices and end/reject reasons.

#[cfg(feature = "quic")]
use crate::*;

/// Host management-API port from `Welcome`. `0` = unknown (do not dial 0; fall
/// back to 47990). Prefer this over mDNS/cached after connect.
///
/// # Safety
/// `c` is a valid connection handle; `port` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_mgmt_port(
    c: *const PunktfunkConnection,
    port: *mut u16,
) -> PunktfunkStatus {
    conn_out!(c, port => c.inner.mgmt_port())
}

/// Host capability bitfield from `Welcome` (`PUNKTFUNK_HOST_CAP_*`). Test
/// `CLIPBOARD` before offering the toggle, `PEN` before sending stylus batches.
/// Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `caps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_host_caps(
    c: *const PunktfunkConnection,
    caps: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, caps => c.inner.host_caps())
}

/// Second host capability byte from `Welcome` — today `PUNKTFUNK_HOST_CAP2_TOUCH`.
/// `0` toward a host that never sends the byte. Safe any time after connect.
///
/// # Safety
/// `c` is a valid connection handle; `caps` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_host_caps2(
    c: *const PunktfunkConnection,
    caps: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, caps => c.inner.host_caps2())
}

/// Live `PUNKTFUNK_GRANT_*` mask (`design/per-client-access.md`). Latest
/// `AccessUpdate` wins; hosts that omit it read `PUNKTFUNK_GRANT_ALL`. Courtesy
/// only — the host enforces. Poll; do not cache for the session.
///
/// # Safety
/// `c` is a valid connection handle; `grants` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_grants(
    c: *const PunktfunkConnection,
    grants: *mut u32,
) -> PunktfunkStatus {
    conn_out!(c, grants => c.inner.access_grants())
}

/// Seconds until access expires. `0` = permanent. While a deadline is set the
/// value never reads `0` (clamps to 1 past expiry until the typed close).
/// Anchored to the client clock at receipt.
///
/// # Safety
/// `c` is a valid connection handle; `secs` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_access_expires_in(
    c: *const PunktfunkConnection,
    secs: *mut u32,
) -> PunktfunkStatus {
    with_conn!(c => {
        let remaining = c.inner.access_expires_in_secs();
        // SAFETY: the caller passes `secs` null or writable for one value.
        unsafe { put(secs, remaining) };
        PunktfunkStatus::Ok
    })
}

/// The sentence the host sent with its mid-session rejection, NUL-terminated, into
/// the caller's buffer; empty when it sent none — render the client's own wording
/// for the code then. Ask alongside [`punktfunk_connection_end_reject`]. A 512-byte
/// buffer is ample: the wire caps this at 256.
///
/// Already stripped of control characters and capped by the core; a host cannot make
/// this longer or make it move a terminal's cursor.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_end_reject_said(
    c: *const PunktfunkConnection,
    out: *mut c_char,
    cap: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() || cap == 0 {
            return PunktfunkStatus::NullPointer;
        }
        let said = c.inner.end_reject_said().unwrap_or_default();
        // SAFETY: `out` is writable for `cap` bytes, per this function's contract.
        if !unsafe { write_cstr(out, cap, said) } {
            return PunktfunkStatus::InvalidArg;
        }
        PunktfunkStatus::Ok
    })
}

/// The profile the host resolved this session to, NUL-terminated, into the caller's buffer;
/// empty from a host without profiles. A buffer of [`PUNKTFUNK_PROFILE_ID_MAX`] + 1 bytes fits
/// every id.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_profile(
    c: *const PunktfunkConnection,
    out: *mut c_char,
    cap: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() || cap == 0 {
            return PunktfunkStatus::NullPointer;
        }
        let profile = c.inner.profile().unwrap_or_default();
        // SAFETY: `out` is writable for `cap` bytes, per this function's contract.
        if !unsafe { write_cstr(out, cap, profile) } {
            return PunktfunkStatus::InvalidArg;
        }
        PunktfunkStatus::Ok
    })
}

/// `true` when this session started on a screen another device opened and sized. As the session
/// started: that device may leave later. `false` from a host that does not say.
///
/// # Safety
/// `c` is a valid connection handle; `shared` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_shares_screen(
    c: *const PunktfunkConnection,
    shared: *mut bool,
) -> PunktfunkStatus {
    use punktfunk_core::quic::v2::hello::ScreenEcho;
    conn_out!(c, shared => c.inner.screen() == Some(&ScreenEcho::Shared))
}

/// [`punktfunk_connection_screen_role`]: no role, or a host that does not say.
pub const PUNKTFUNK_SCREEN_ROLE_NONE: u8 = 0;
/// [`punktfunk_connection_screen_role`]: the only screen; the host's monitors are off.
pub const PUNKTFUNK_SCREEN_ROLE_ONLY: u8 = 1;
/// [`punktfunk_connection_screen_role`]: the main screen beside the host's monitors.
pub const PUNKTFUNK_SCREEN_ROLE_MAIN: u8 = 2;
/// [`punktfunk_connection_screen_role`]: one more screen beside the host's monitors.
pub const PUNKTFUNK_SCREEN_ROLE_SECOND: u8 = 3;
/// [`punktfunk_connection_screen_role_reason`]: as asked, or a host that does not say.
pub const PUNKTFUNK_SCREEN_ROLE_REASON_AS_ASKED: u8 = 0;
/// [`punktfunk_connection_screen_role_reason`]: this device may not choose its screen there.
pub const PUNKTFUNK_SCREEN_ROLE_REASON_NOT_ALLOWED: u8 = 1;
/// [`punktfunk_connection_screen_role_reason`]: an earlier session decided the monitors.
pub const PUNKTFUNK_SCREEN_ROLE_REASON_MONITORS_HELD: u8 = 2;
/// [`punktfunk_connection_screen_role_reason`]: the monitor asked for is off, taken or missing.
pub const PUNKTFUNK_SCREEN_ROLE_REASON_UNAVAILABLE: u8 = 3;
/// [`punktfunk_connection_screen_role_reason`]: the host's desktop has no monitors.
pub const PUNKTFUNK_SCREEN_ROLE_REASON_NO_MONITORS: u8 = 4;

/// The role this session got among the host's screens, `PUNKTFUNK_SCREEN_ROLE_*`. As the
/// session started.
///
/// # Safety
/// `c` is a valid connection handle; `role` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_screen_role(
    c: *const PunktfunkConnection,
    role: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, role => c.inner.role().and_then(|r| r.role).map_or(0, |r| r.to_u8()))
}

/// Why this session's role is not the one its connect asked for,
/// `PUNKTFUNK_SCREEN_ROLE_REASON_*`. As the session started.
///
/// # Safety
/// `c` is a valid connection handle; `reason` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_screen_role_reason(
    c: *const PunktfunkConnection,
    reason: *mut u8,
) -> PunktfunkStatus {
    conn_out!(c, reason => c.inner.role().map_or(0, |r| r.reason.to_u8()))
}

/// The host's sentence when this session's launch did not give the player their game,
/// NUL-terminated, into the caller's buffer; empty otherwise. The latest verdict wins, so
/// poll it. A 256-byte buffer is ample: the wire caps this at 200.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_launch_notice(
    c: *const PunktfunkConnection,
    out: *mut c_char,
    cap: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() || cap == 0 {
            return PunktfunkStatus::NullPointer;
        }
        let outcome = c.inner.launch_outcome();
        let notice = outcome
            .as_ref()
            .and_then(|o| o.notice())
            .unwrap_or_default();
        // SAFETY: `out` is writable for `cap` bytes, per this function's contract.
        if !unsafe { write_cstr(out, cap, notice) } {
            return PunktfunkStatus::InvalidArg;
        }
        PunktfunkStatus::Ok
    })
}

/// [`punktfunk_connection_launch_notice`] by message: the host message the sentence is the
/// English of, then the sentence, then each placeholder as `name<TAB>value`, one per line,
/// NUL-terminated, into the caller's buffer; empty when the launch needs no words. The id is
/// empty from a host that names none. A 1200-byte buffer is ample: the wire caps the id at 64
/// bytes, the sentence at 200 and each of at most 3 placeholders at 64 + 200.
///
/// # Safety
/// `c` is a valid connection handle; `out` is writable for `cap` bytes.
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_launch_message(
    c: *const PunktfunkConnection,
    out: *mut c_char,
    cap: usize,
) -> PunktfunkStatus {
    with_conn!(c => {
        if out.is_null() || cap == 0 {
            return PunktfunkStatus::NullPointer;
        }
        let outcome = c.inner.launch_outcome();
        let said = match outcome.as_ref().filter(|o| o.notice().is_some()) {
            Some(o) => launch_message_lines(o),
            None => String::new(),
        };
        // SAFETY: `out` is writable for `cap` bytes, per this function's contract.
        if !unsafe { write_cstr(out, cap, &said) } {
            return PunktfunkStatus::InvalidArg;
        }
        PunktfunkStatus::Ok
    })
}

/// The lines [`punktfunk_connection_launch_message`] writes. A field is the host's text, so
/// a separator inside one reads as a space.
#[cfg(feature = "quic")]
fn launch_message_lines(o: &punktfunk_core::quic::LaunchOutcome) -> String {
    let flat = |s: &str| s.replace(['\n', '\t'], " ");
    let mut said = format!("{}\n{}", flat(&o.message_id), flat(&o.message));
    for (name, value) in &o.message_params {
        said.push_str(&format!("\n{}\t{}", flat(name), flat(value)));
    }
    said
}

/// Mid-session typed rejection (`PUNKTFUNK_STATUS_REJECTED_*`); `0` = none.
/// Ask after `Closed`, before free. Connect-time rejections come from connect.
///
/// # Safety
/// `c` is a valid connection handle; `status` is writable (NULL is skipped).
#[cfg(feature = "quic")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn punktfunk_connection_end_reject(
    c: *const PunktfunkConnection,
    status: *mut i32,
) -> PunktfunkStatus {
    with_conn!(c => {
        let value = match c.inner.end_reject() {
            Some(reason) => {
                punktfunk_core::error::PunktfunkError::Rejected(reason, String::new()).status() as i32
            }
            None => 0,
        };
        // SAFETY: the caller passes `status` null or writable for one value.
        unsafe { put(status, value) };
        PunktfunkStatus::Ok
    })
}

#[cfg(all(test, feature = "quic"))]
mod tests {
    use super::launch_message_lines;
    use punktfunk_core::quic::{LaunchOutcome, LaunchOutcomeKind};

    /// The C values are the wire's.
    #[test]
    fn screen_role_values_are_the_wires() {
        use super::*;
        use punktfunk_core::quic::v2::hello::{Role, RoleReason};
        let roles = [Role::Only, Role::Main, Role::Second].map(Role::to_u8);
        assert_eq!(
            roles,
            [
                PUNKTFUNK_SCREEN_ROLE_ONLY,
                PUNKTFUNK_SCREEN_ROLE_MAIN,
                PUNKTFUNK_SCREEN_ROLE_SECOND
            ]
        );
        assert_eq!(Role::from_u8(PUNKTFUNK_SCREEN_ROLE_NONE), None);
        let reasons = [
            RoleReason::AsAsked,
            RoleReason::NotAllowed,
            RoleReason::MonitorsHeld,
            RoleReason::Unavailable,
            RoleReason::NoMonitors,
        ];
        assert_eq!(
            reasons.map(RoleReason::to_u8),
            [
                PUNKTFUNK_SCREEN_ROLE_REASON_AS_ASKED,
                PUNKTFUNK_SCREEN_ROLE_REASON_NOT_ALLOWED,
                PUNKTFUNK_SCREEN_ROLE_REASON_MONITORS_HELD,
                PUNKTFUNK_SCREEN_ROLE_REASON_UNAVAILABLE,
                PUNKTFUNK_SCREEN_ROLE_REASON_NO_MONITORS
            ]
        );
    }

    #[test]
    fn a_launch_verdict_is_its_message_then_its_placeholders() {
        let said = LaunchOutcome::new(LaunchOutcomeKind::Failed, "Quail closed at once.").worded(
            "launch_closed_at_once",
            &[("title", "Quail"), ("seconds", "4")],
        );
        assert_eq!(
            launch_message_lines(&said),
            "launch_closed_at_once\nQuail closed at once.\ntitle\tQuail\nseconds\t4"
        );
        let bare = LaunchOutcome::new(LaunchOutcomeKind::Refused, "Nothing to run.");
        assert_eq!(launch_message_lines(&bare), "\nNothing to run.");
        // A separator in the host's text cannot start another field.
        let mut hostile = said;
        hostile.message_params[0].1 = "a\nb\tc".into();
        assert!(launch_message_lines(&hostile).contains("title\ta b c"));
    }
}
