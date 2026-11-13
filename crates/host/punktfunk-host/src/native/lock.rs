//! A locked profile at connect (`profiles::locks`): a device on its list goes on, any other
//! gives the profile's PIN or is refused. Every host asks, right after the profile resolved and
//! before anything places or plays it, so a profile that plays here and one sent to its seat
//! pass the same check.
//!
//! The PIN is the `ProfileUnlock` the hello announced, kind 1; the box's own host checks it. A
//! refusal never tells a device that may not see the profile whether it has a PIN, and nothing
//! logged or answered holds the PIN.

use super::{link, Said, SessionHost};
use crate::profiles::locks::{PinAnswer, Presence};
use crate::profiles::{Opens, Resolved};
use anyhow::{bail, Result};
use punktfunk_core::quic::v2::hello::ClientHello;
use punktfunk_core::quic::v2::msg::{self as v2msg, ProfileUnlock};
use punktfunk_core::quic::v2::registry::FEATURE_PROFILE_UNLOCK;
use punktfunk_core::reject::RejectReason;
use std::time::Duration;

/// How long the unlock a hello announced may take to follow it.
const PROOF_WAIT: Duration = Duration::from_secs(5);

/// A refusal's message id and its English.
type Words = (&'static str, &'static str);

const NOT_ALLOWED: Words = (
    "close_lock_not_allowed",
    "This device can't open this profile. Ask the host's owner to add it.",
);
const PIN_UPDATE: Words = (
    "close_lock_pin_update",
    "This profile needs its PIN. Update Punktfunk on this device.",
);
const PIN_WRONG: Words = ("close_lock_pin_wrong", "That isn't this profile's PIN.");
const PIN_UNREADABLE: Words = (
    "close_lock_pin_unreadable",
    "Couldn't read the PIN this device sent. Try again.",
);
const FULL: Words = (
    "close_lock_full",
    "This profile has all the devices it can take. Ask the host's owner to sign one out.",
);
const NOT_SAVED: Words = (
    "close_lock_not_saved",
    "This host couldn't save that. Try again in a moment.",
);
pub(super) const SIGNED_OUT: Words = (
    "close_lock_signed_out",
    "This device was signed out of this profile.",
);

fn said((id, text): Words) -> Said {
    Said::new(id, text)
}

/// Whether device `fp` may open `resolved` here, with the PIN the hello announced when it
/// needs one, and its hold on the profile's list when it is on one. Any other answer closes the
/// connect, typed, and errs.
pub(super) async fn admit(
    host: &SessionHost,
    conn: &link::SessionLink,
    (send, recv): (&mut link::CtlSend, &mut link::CtlReader),
    first: &ClientHello,
    resolved: &Resolved,
    fp: Option<&str>,
) -> Result<Option<Presence>> {
    let profiles = &host.profiles;
    let id = resolved.id.as_str();
    let opens = profiles.opens(id, fp);
    let (Opens::Pin, Some(fp)) = (opens, fp) else {
        if opens == Opens::Yes {
            return Ok(fp.and_then(|fp| Presence::join(profiles, id, fp)));
        }
        refuse(conn, RejectReason::SeatUnavailable, said(NOT_ALLOWED)).await;
        bail!("profile {id} is locked to other devices")
    };
    let offered = first.features.has(FEATURE_PROFILE_UNLOCK);
    if !offered || !first.unlock_follows() {
        let (reason, words) = locked(offered, Said::default());
        refuse(conn, reason, words).await;
        bail!("profile {id} needs its PIN")
    }
    let deadline = tokio::time::Instant::now() + PROOF_WAIT;
    let unlock = match recv.read_profile_unlock(deadline).await {
        Ok(unlock) if unlock.kind == v2msg::UNLOCK_PASSWORD => unlock,
        // A key opens a PIN's profile only once the key message names a profile.
        Ok(_) => {
            refuse(conn, RejectReason::ProfileLocked, Said::default()).await;
            bail!("a key proof for profile {id}, which takes a PIN")
        }
        Err(_) => {
            refuse(conn, RejectReason::ProfileLocked, said(PIN_UNREADABLE)).await;
            bail!("the PIN the hello announced didn't arrive whole")
        }
    };
    // The check is slow by design: the client hears the host is still deciding.
    let pending = punktfunk_core::quic::v2::io::send(&mut *send, &v2msg::Pending {});
    let _ = tokio::time::timeout(super::PENDING_EVERY, pending).await;
    let paired = host.np.list().into_iter();
    let label = paired
        .filter(|c| c.fingerprint.eq_ignore_ascii_case(fp))
        .map(|c| c.name)
        .next();
    let device = super::unlock::device(fp, label.as_deref());
    let checking = std::sync::Arc::clone(profiles);
    let owned = id.to_string();
    let answer = tokio::task::spawn_blocking(move || try_pin(&checking, &owned, &device, unlock))
        .await
        .unwrap_or(PinAnswer::NotSaved);
    let (reason, words) = match answer {
        PinAnswer::Opened => return Ok(Presence::join(profiles, id, fp)),
        PinAnswer::Wrong => (RejectReason::ProfileLocked, said(PIN_WRONG)),
        PinAnswer::Wait(wait) => (RejectReason::ProfileLocked, wait_words(wait)),
        PinAnswer::NoPin => (RejectReason::ProfileLocked, Said::default()),
        PinAnswer::Full => (RejectReason::ProfileLocked, said(FULL)),
        PinAnswer::NotSaved => (RejectReason::SeatUnavailable, said(NOT_SAVED)),
    };
    refuse(conn, reason, words).await;
    bail!("profile {id}'s PIN didn't open it: {answer:?}")
}

/// `unlock`'s secret as the PIN of `id`'s lock
/// ([`crate::profiles::Profiles::lock_holder`]); the wire's copy wipes as `unlock` drops.
/// Blocking.
fn try_pin(
    profiles: &crate::profiles::Profiles,
    id: &str,
    device: &pf_seats::ipc::Device,
    unlock: ProfileUnlock,
) -> PinAnswer {
    let pin = std::str::from_utf8(&unlock.secret[..]).unwrap_or("");
    let holder = profiles.lock_holder(id);
    profiles
        .locks()
        .try_pin(&holder, device, pin, unlock.stay_signed_in)
}

/// The refusal for wrong PINs that lasts `wait`: seconds under a minute, else minutes, rounded
/// up.
fn wait_words(wait: Duration) -> Said {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let (id, count, unit) = if seconds < 60 {
        ("close_lock_pin_wait_seconds", seconds.max(1), "second")
    } else {
        (
            "close_lock_pin_wait_minutes",
            seconds.div_ceil(60),
            "minute",
        )
    };
    let plural = if count == 1 { "" } else { "s" };
    let text = format!("Too many wrong PINs. Try again in {count} {unit}{plural}.");
    Said::new(id, &text).param("count", count)
}

/// A refusal at the prompt: `ProfileLocked` with `words`, none to show the prompt, for a client
/// that offered the unlock; the update sentence for one that can't send it.
fn locked(offered: bool, words: Said) -> (RejectReason, Said) {
    if offered {
        (RejectReason::ProfileLocked, words)
    } else {
        (RejectReason::SeatUnavailable, said(PIN_UPDATE))
    }
}

async fn refuse(conn: &link::SessionLink, reason: RejectReason, words: Said) {
    tracing::info!(reason = %reason, said = %words.id, "locked profile refused");
    conn.refuse_said(reason.close_code(), &words).await;
}

/// Whether device `fp` still opens profile `id`: a sign-out, a lock or a PIN's day ending
/// closes its session.
pub(super) fn still_opens(
    profiles: &crate::profiles::Profiles,
    id: &str,
    fp: Option<&str>,
) -> bool {
    profiles.opens(id, fp) == Opens::Yes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lockout_reads_in_whole_seconds_or_minutes() {
        let s = Duration::from_secs;
        let read = |wait| {
            let words = wait_words(wait);
            (words.id, words.text, words.params)
        };
        let count = |n: u64| vec![("count".to_string(), n.to_string())];
        assert_eq!(
            read(Duration::from_millis(3200)),
            (
                "close_lock_pin_wait_seconds".into(),
                "Too many wrong PINs. Try again in 4 seconds.".into(),
                count(4)
            )
        );
        assert_eq!(read(s(1)).1, "Too many wrong PINs. Try again in 1 second.");
        assert_eq!(
            read(s(15 * 60)),
            (
                "close_lock_pin_wait_minutes".into(),
                "Too many wrong PINs. Try again in 15 minutes.".into(),
                count(15)
            )
        );
        assert_eq!(
            read(s(61)).1,
            "Too many wrong PINs. Try again in 2 minutes."
        );
    }
}
