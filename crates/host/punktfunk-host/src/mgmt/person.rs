//! The person lane: a device on a locked profile's list changes that profile's own library view
//! and game-exit choice, through `mgmt_routes::PERSON` alone. Asked on every call, so signing
//! the device out, unlocking the profile or the device's presence ending takes it away at once.
//!
//! The box's own host decides, from its lock file: the plain host, Windows' box host, and the
//! door. The door passes the call to the owner's host, which writes the library, naming the
//! person beside the device ([`VOUCH_HEADER`]). That host takes the door's word only with its own
//! token: its lock copy holds the owner's lock alone, and its pairing copy, while the owner is
//! locked, the owner's devices alone. Every other seat host decides nothing.

use super::shared::*;
use crate::profiles::{Opens, OsAccount, Profile, Profiles};
pub(crate) use punktfunk_core::mgmt_routes::person_route;

/// The profile the door names a vouched device the person of.
pub(crate) const VOUCH_HEADER: &str = "x-punktfunk-person";

/// The profile the door vouches the device is the person of. `require_auth` stamps it only on
/// the owner's host ([`takes_vouch`]), with that host's own token, on a [`person_route`].
#[derive(Clone, Debug)]
pub(crate) struct Vouched(pub String);

/// The answer to a device that is not the named profile's person. One sentence for an open
/// profile, a device off its list and one that needs its PIN: no device learns which are locked.
pub(crate) fn refused() -> Response {
    api_error_coded(
        StatusCode::FORBIDDEN,
        "profile_change_not_allowed",
        "This device can't change this profile. Ask the host's owner to change it in the web \
         console.",
    )
}

/// The profile `as` names in `uri`, read as the handler's extractor reads it. Empty names none.
pub(crate) fn named(uri: &axum::http::Uri) -> Option<String> {
    #[derive(Deserialize)]
    struct Named {
        #[serde(rename = "as")]
        profile: Option<String>,
    }
    let Query(named) = Query::<Named>::try_from_uri(uri).ok()?;
    named.profile.filter(|p| !p.is_empty())
}

/// Whether this host takes the door's word for a person: the owner's host, which reads the box's
/// profiles and writes its library. A seat that follows the contract writes no library.
pub(crate) fn takes_vouch(profiles: &Profiles) -> bool {
    profiles.reads_box() && !pf_paths::seat::role().follows_contract()
}

/// Profile `named`, when device `fp` is its person here now. `vouched` is the door's word, which
/// only [`takes_vouch`] hears. A profile this host doesn't hold is the 404 an unknown id gets;
/// every other refusal is [`refused`].
pub(crate) async fn check(
    st: &MgmtState,
    named: Option<&str>,
    fp: &str,
    vouched: Option<&str>,
) -> Result<String, Response> {
    let id = named.filter(|p| !p.is_empty()).ok_or_else(refused)?;
    let profiles = st
        .app
        .profiles
        .get()
        .filter(|_| crate::library::is_profile_id(id))
        .cloned()
        .ok_or_else(super::library::profile_unknown)?;
    let profile = profiles
        .get(id)
        .ok_or_else(super::library::profile_unknown)?;
    // The owner's layer is the box's library: the operator's alone, whoever shares its lock.
    let owner = profiles.owner_id().as_deref() == Some(id);
    let person = if profiles.reads_box() {
        takes_vouch(&profiles) && vouched == Some(id) && !owner
    } else {
        !owner && opens_locked(&profiles, id, fp) && !own_account(st, &profile).await?
    };
    if person {
        Ok(id.to_string())
    } else {
        Err(refused())
    }
}

/// The refusal when profile `named` is not the person session's `vouched` one, or is the owner.
/// The proof is the account its seat host runs as, so a lock or an own account changes nothing.
/// A profile this host doesn't hold is the 404 an unknown id gets. Not `Result`: a `Response` in
/// `Err` trips `clippy::result_large_err`.
pub(crate) fn session_refusal(
    st: &MgmtState,
    named: Option<&str>,
    vouched: Option<&str>,
) -> Option<Response> {
    let not_theirs = || {
        api_error(
            StatusCode::FORBIDDEN,
            "a person session changes its own profile alone, never the owner's",
        )
    };
    let Some(id) = named.filter(|p| Some(*p) == vouched) else {
        return Some(not_theirs());
    };
    let held = st
        .app
        .profiles
        .get()
        .filter(|p| crate::library::is_profile_id(id) && p.get(id).is_some());
    let Some(profiles) = held else {
        return Some(super::library::profile_unknown());
    };
    (profiles.owner_id().as_deref() == Some(id)).then(not_theirs)
}

/// Profile `id`'s lock (its holder's) is locked and admits device `fp` with nothing asked. An open
/// profile has no person: any paired device could claim it.
fn opens_locked(profiles: &Profiles, id: &str, fp: &str) -> bool {
    let holder = profiles.lock_holder(id);
    profiles.locks().get(&holder).locked && profiles.opens(id, Some(fp)) == Opens::Yes
}

/// Whether `profile` plays on a person's own account, whose lock is its password. A door whose
/// supervisor doesn't answer can't say, and counts it as one.
async fn own_account(st: &MgmtState, profile: &Profile) -> Result<bool, Response> {
    let OsAccount::Seat { seat: Some(seat) } = &profile.os_account else {
        return Ok(false);
    };
    if cfg!(windows) {
        return super::library::on_own_account(seat.clone()).await;
    }
    let Some(door) = st.door.clone() else {
        return Ok(false);
    };
    let ledger = blocking("seat_ledger", move || door.ledger()).await?;
    Ok(!ledger.on || ledger.person_row(&profile.os_account).is_some())
}
