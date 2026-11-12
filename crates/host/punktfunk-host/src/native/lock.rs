//! A locked profile at connect (`profiles::locks`): a device on its list goes on, any other
//! gives the profile's PIN or is refused. Every host asks, right after the profile resolved and
//! before anything places or plays it, so a profile that plays here and one sent to its seat
//! pass the same check.
//!
//! A refusal never tells a device that may not see the profile whether it has a PIN.

use super::{link, Said};
use crate::profiles::locks::Presence;
use crate::profiles::{Opens, Profiles, Resolved};
use anyhow::{bail, Result};
use punktfunk_core::quic::v2::hello::ClientHello;
use punktfunk_core::quic::v2::registry::FEATURE_PROFILE_UNLOCK;
use punktfunk_core::reject::RejectReason;
use std::sync::Arc;

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
pub(super) const SIGNED_OUT: Words = (
    "close_lock_signed_out",
    "This device was signed out of this profile.",
);

fn said((id, text): Words) -> Said {
    Said::new(id, text)
}

/// Whether device `fp` may open `resolved` here, and its hold on the profile's list when it is
/// on one. Any other answer closes the connect, typed, and errs.
pub(super) async fn admit(
    profiles: &Arc<Profiles>,
    conn: &link::SessionLink,
    first: &ClientHello,
    resolved: &Resolved,
    fp: Option<&str>,
) -> Result<Option<Presence>> {
    match profiles.opens(&resolved.id, fp) {
        Opens::Yes => Ok(fp.and_then(|fp| Presence::join(profiles, &resolved.id, fp))),
        Opens::No => {
            refuse(conn, RejectReason::SeatUnavailable, said(NOT_ALLOWED)).await;
            bail!("profile {} is locked to other devices", resolved.id)
        }
        Opens::Pin => {
            let (reason, words) =
                locked(first.features.has(FEATURE_PROFILE_UNLOCK), Said::default());
            refuse(conn, reason, words).await;
            bail!("profile {} needs its PIN", resolved.id)
        }
    }
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
    tracing::info!(reason = %reason, "locked profile refused");
    conn.refuse_said(reason.close_code(), &words).await;
}

/// Whether device `fp` still opens profile `id`: a sign-out, a lock or a PIN's day ending
/// closes its session.
pub(super) fn still_opens(profiles: &Profiles, id: &str, fp: Option<&str>) -> bool {
    profiles.opens(id, fp) == Opens::Yes
}
