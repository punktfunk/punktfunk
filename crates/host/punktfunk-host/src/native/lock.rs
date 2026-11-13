//! A locked profile at connect (`profiles::locks`): a device on its list goes on, any other
//! gives the profile's PIN, or its enrolled key's proof, or is refused. Every host asks, right
//! after the profile resolved and before anything places or plays it, so a profile that plays
//! here and one sent to its seat pass the same check.
//!
//! The proof is the `ProfileUnlock` the hello announced: kind 1 the PIN, kind 2 a signature over
//! the profile's message; the box's own host checks it. A refusal never tells a device that may
//! not see the profile whether it has a PIN, and nothing logged or answered holds the PIN.

use super::{link, Said, SessionHost};
use crate::profiles::locks::{PinAnswer, Presence};
use crate::profiles::{Opens, Resolved};
use anyhow::{bail, Result};
use pf_seats::ipc::{KeyAlg, UnlockKey};
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
const KEY_FAILED: Words = (
    "close_lock_key_failed",
    "Couldn't confirm it's you on this device. Type this profile's PIN.",
);

fn said((id, text): Words) -> Said {
    Said::new(id, text)
}

/// Whether device `fp` may open `resolved` here, with the PIN or key proof the hello announced
/// when it needs one, and its hold on the profile's list when it is on one. Any other answer
/// closes the connect, typed, and errs.
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
    let fp = match (opens, fp) {
        (Opens::Yes, fp) => return Ok(fp.and_then(|fp| Presence::join(profiles, id, fp))),
        (Opens::Pin | Opens::Key, Some(fp)) => fp,
        _ => {
            refuse(conn, RejectReason::SeatUnavailable, said(NOT_ALLOWED)).await;
            bail!("profile {id} is locked to other devices")
        }
    };
    let offered = first.features.has(FEATURE_PROFILE_UNLOCK);
    if !offered || !first.unlock_follows() {
        let (reason, words) = locked(offered, Said::default());
        refuse(conn, reason, words).await;
        bail!("profile {id} needs its PIN")
    }
    let deadline = tokio::time::Instant::now() + PROOF_WAIT;
    let Ok(unlock) = recv.read_profile_unlock(deadline).await else {
        refuse(conn, RejectReason::ProfileLocked, said(PIN_UNREADABLE)).await;
        bail!("the PIN the hello announced didn't arrive whole")
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
    let (owned, host_fp) = (id.to_string(), host.host_fp);
    let answer = tokio::task::spawn_blocking(move || {
        try_unlock(&checking, &owned, &device, unlock, &host_fp)
    })
    .await
    .unwrap_or(PinAnswer::NotSaved);
    // A device the list holds a key for, on a profile with no PIN to fall back on.
    let holder = profiles.lock_holder(id);
    let key_only = opens == Opens::Key && profiles.locks().get(&holder).pin.is_none();
    let (reason, words) = match answer {
        PinAnswer::Opened => return Ok(Presence::join(profiles, id, fp)),
        PinAnswer::NoPin | PinAnswer::KeyFailed if key_only => {
            (RejectReason::SeatUnavailable, said(NOT_ALLOWED))
        }
        PinAnswer::Wrong => (RejectReason::ProfileLocked, said(PIN_WRONG)),
        PinAnswer::Wait(wait) => (RejectReason::ProfileLocked, wait_words(wait)),
        PinAnswer::NoPin => (RejectReason::ProfileLocked, Said::default()),
        PinAnswer::KeyFailed => (RejectReason::ProfileLocked, said(KEY_FAILED)),
        PinAnswer::Full => (RejectReason::ProfileLocked, said(FULL)),
        PinAnswer::NotSaved => (RejectReason::SeatUnavailable, said(NOT_SAVED)),
    };
    refuse(conn, reason, words).await;
    bail!("profile {id}'s unlock didn't open it: {answer:?}")
}

/// `unlock` for `id`'s lock ([`crate::profiles::Profiles::lock_holder`]): kind 1 its secret as
/// the PIN, with any key it enrols for `id`; kind 2 a key proof over `id`'s challenge and
/// `host_fp`, from the key enrolled for `id`. The wire's copy wipes as `unlock` drops. Blocking.
fn try_unlock(
    profiles: &crate::profiles::Profiles,
    id: &str,
    device: &pf_seats::ipc::Device,
    unlock: ProfileUnlock,
    host_fp: &[u8; 32],
) -> PinAnswer {
    let holder = profiles.lock_holder(id);
    let locks = profiles.locks();
    if unlock.kind == v2msg::UNLOCK_KEY_PROOF {
        return locks.try_key((id, &holder), device, &unlock.secret, host_fp);
    }
    let pin = std::str::from_utf8(&unlock.secret[..]).unwrap_or("");
    let enroll = enrolled(&unlock).map(|key| (id, key));
    locks.try_pin(&holder, device, pin, (unlock.stay_signed_in, enroll))
}

/// The key a PIN brings to enrol. One this host wouldn't verify with is left out, and the PIN
/// alone lets the device in.
fn enrolled(unlock: &ProfileUnlock) -> Option<UnlockKey> {
    let alg = match unlock.enroll_alg {
        v2msg::ENROLL_P256 => KeyAlg::P256,
        v2msg::ENROLL_RSA2048 => KeyAlg::Rsa2048,
        _ => return None,
    };
    let key = UnlockKey {
        alg,
        public: hex::encode(&unlock.enroll_key),
    };
    pf_seats::unlock::key_bytes(&key).map(|_| key)
}

/// The refusal for wrong PINs that lasts `wait`: seconds under a minute, minutes up to two
/// hours, else hours, each rounded up. A wait grows to a day ([`pf_seats::locks::PIN_WINDOW_MAX`]).
fn wait_words(wait: Duration) -> Said {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let (id, count, unit) = if seconds < 60 {
        ("close_lock_pin_wait_seconds", seconds.max(1), "second")
    } else if seconds <= 2 * 60 * 60 {
        (
            "close_lock_pin_wait_minutes",
            seconds.div_ceil(60),
            "minute",
        )
    } else {
        ("close_lock_pin_wait_hours", seconds.div_ceil(3600), "hour")
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
    fn a_lockout_reads_in_whole_seconds_minutes_or_hours() {
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
        assert_eq!(
            read(s(2 * 60 * 60)).1,
            "Too many wrong PINs. Try again in 120 minutes."
        );
        assert_eq!(
            read(s(pf_seats::locks::PIN_WINDOW_MAX as u64)),
            (
                "close_lock_pin_wait_hours".into(),
                "Too many wrong PINs. Try again in 24 hours.".into(),
                count(24)
            )
        );
        assert_eq!(
            read(s(2 * 60 * 60 + 1)).1,
            "Too many wrong PINs. Try again in 3 hours."
        );
    }
}
