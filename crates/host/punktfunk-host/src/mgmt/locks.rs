//! A profile's lock, the console's alone: lock or unlock it, set or clear its PIN, and the
//! devices its list lets in (`profiles/locks.rs`). Listing and signing out share
//! `profiles/{id}/devices` with a person's own account, whose devices are the supervisor's
//! (`profiles.rs`); that profile opens with its password and takes no lock.
//!
//! Every edit is the box host's: a seat host only reads the box's file. A file that doesn't read
//! keeps every profile locked and refuses every edit until it is fixed or removed.

use super::profiles::{edit_error, no_profiles, person_seat, SignedInDevice};
use super::shared::*;
use crate::profiles::Profiles;
use pf_seats::locks::{Lock, PinHash};

/// A profile's lock, as the console shows it.
#[derive(Serialize, ToSchema)]
pub(crate) struct ProfileLock {
    /// Only the devices on its list, and a device that brings its PIN, open it.
    locked: bool,
    /// It has a PIN.
    pin: bool,
    /// Wrong PINs in the last 15 minutes over all devices. At 10, PIN entry waits.
    failed_pins: u32,
    /// The paired devices playing as it now, on its desktop's other profiles too: the ones a
    /// lock would cut off unless they are kept.
    playing: Vec<PlayingDevice>,
}

/// A paired device playing as a profile now.
#[derive(Serialize, ToSchema, Debug, PartialEq, Eq)]
pub(crate) struct PlayingDevice {
    /// SHA-256 of the device's certificate, lowercase hex.
    fingerprint: String,
    /// The name it paired with.
    name: String,
}

/// `POST /profiles/{id}/lock`, optional.
#[derive(Deserialize, ToSchema, Default)]
pub(crate) struct LockProfile {
    /// Paired devices, by certificate SHA-256 hex, that keep opening it: they join its list,
    /// until signed out, as it locks. A profile already locked takes only devices it lets in.
    #[serde(default)]
    devices: Vec<String>,
}

/// `PUT /profiles/{id}/lock/pin`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ProfilePin {
    /// 4 to 8 digits.
    #[schema(example = "2468")]
    pin: String,
}

/// `POST /profiles/{id}/devices`.
#[derive(Deserialize, ToSchema)]
pub(crate) struct ProfileDeviceAdd {
    /// SHA-256 of a paired device's certificate, hex: a Punktfunk device or a Moonlight one.
    fingerprint: String,
}

fn own_account() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "profile_lock_own_account",
        "This profile opens with its account password, so it can't be locked.",
    )
}

fn unreadable() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "profile_locks_unreadable",
        "This host couldn't read its profile locks, so every profile stays locked. Fix or remove profile-locks.json, then try again.",
    )
}

/// A profile that shares `owner`'s desktop opens with the owner's lock.
fn shared(owner: &str) -> Response {
    ApiError::coded(
        "profile_lock_shared",
        format!(
            "This profile shares {owner}'s desktop, so it opens with that profile's lock. Change the lock there."
        ),
    )
    .param("owner", owner)
    .respond(StatusCode::CONFLICT)
}

/// The profiles, when `id` is one that takes a lock and the lock file reads.
async fn lockable(st: &MgmtState, id: &str) -> Result<Arc<Profiles>, Response> {
    if person_seat(st, id).await?.is_some() {
        return Err(own_account());
    }
    readable(st, id).await
}

/// The profiles, when `id` has a lock of its own and the lock file reads. Its caller found the
/// profile.
async fn readable(st: &MgmtState, id: &str) -> Result<Arc<Profiles>, Response> {
    let profiles = st.app.profiles.get().cloned().ok_or_else(no_profiles)?;
    let holder = profiles.lock_holder(id);
    if holder != id {
        let owner = profiles.get(&holder).map(|p| p.display_name);
        return Err(shared(&owner.unwrap_or(holder)));
    }
    if profiles.locks().unreadable() {
        return Err(unreadable());
    }
    Ok(profiles)
}

/// Profile `id`'s lock as it is now.
async fn state(st: &MgmtState, profiles: &Profiles, id: &str) -> Response {
    let lock = profiles.locks().get(id);
    Json(ProfileLock {
        locked: lock.locked,
        pin: lock.pin.is_some(),
        failed_pins: u32::try_from(profiles.locks().failures(id)).unwrap_or(u32::MAX),
        playing: playing(st, profiles, id).await,
    })
    .into_response()
}

/// Applies `change` to profile `id`'s lock and answers its state.
async fn change(st: &MgmtState, id: &str, change: impl FnOnce(&mut Lock)) -> Response {
    let profiles = match lockable(st, id).await {
        Ok(profiles) => profiles,
        Err(resp) => return resp,
    };
    match profiles.locks().edit(id, |lock, _| change(lock)) {
        Ok(()) => state(st, &profiles, id).await,
        Err(e) => edit_error(e),
    }
}

/// The paired devices whose live sessions play as a profile under `id`'s lock: this host's
/// sessions and, on a door, every seat's. A session's client is a fingerprint prefix, so a
/// device is named only when exactly one paired device carries it.
async fn playing(st: &MgmtState, profiles: &Profiles, id: &str) -> Vec<PlayingDevice> {
    let owner = profiles.owner_id();
    // A GameStream session plays as the owner.
    let profile_of = |profile: Option<String>, gamestream: bool| {
        profile.or_else(|| gamestream.then(|| owner.clone()).flatten())
    };
    let mut live: Vec<(String, Option<String>, Option<String>)> = crate::session_status::snapshot()
        .into_iter()
        .map(|s| {
            let gamestream = s.plane == crate::events::Plane::Gamestream;
            let profile = profile_of(s.profile.map(|p| p.id), gamestream);
            (s.client, s.client_name, profile)
        })
        .collect();
    if st.door.is_some() {
        let snapshot = st.seats_snapshot().await;
        let rows = snapshot.statuses.values();
        for row in rows.flat_map(|s| s["sessions"].as_array().cloned().unwrap_or_default()) {
            let text = |v: &serde_json::Value| v.as_str().map(str::to_owned);
            let profile = profile_of(text(&row["profile"]["id"]), row["plane"] == "gamestream");
            let client = text(&row["client"]).unwrap_or_default();
            live.push((client, text(&row["client_name"]), profile));
        }
    }
    let holder = |p: &Option<String>| p.as_deref().is_some_and(|p| profiles.lock_holder(p) == id);
    live.retain(|(_, _, profile)| holder(profile));
    let mut paired = local_paired(st);
    let unknown = |paired: &[(String, Option<String>)]| {
        let named = |c: &String| {
            paired
                .iter()
                .any(|(fp, _)| crate::session_status::is_client(c, fp))
        };
        live.iter().any(|(client, ..)| !named(client))
    };
    if st.door.is_some() && unknown(&paired) {
        paired.extend(owner_moonlight(st).await.unwrap_or_default());
    }
    let mut out: Vec<PlayingDevice> = Vec::new();
    for (client, session_name, _) in &live {
        let mut carrying = paired
            .iter()
            .filter(|(fp, _)| crate::session_status::is_client(client, fp));
        let (Some((fp, label)), None) = (carrying.next(), carrying.next()) else {
            continue;
        };
        if out.iter().any(|d| &d.fingerprint == fp) {
            continue;
        }
        let name = label.as_deref().or(session_name.as_deref());
        let device = crate::native::unlock::device(fp, name);
        out.push(PlayingDevice {
            fingerprint: device.fingerprint,
            name: device.name,
        });
    }
    out
}

/// Read a profile's lock
///
/// Whether it is locked, whether it has a PIN, and the wrong PINs counted against it. Its
/// devices are `GET /profiles/{id}/devices`. Admin lane only.
#[utoipa::path(
    get,
    path = "/profiles/{id}/lock",
    tag = "profiles",
    operation_id = "getProfileLock",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "The lock", body = ProfileLock),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
    )
)]
pub(crate) async fn get_profile_lock(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    match lockable(&st, &id).await {
        Ok(profiles) => state(&st, &profiles, &id).await,
        Err(resp) => resp,
    }
}

/// Lock a profile
///
/// From now only the devices on its list open it, and a device that brings its PIN when it has
/// one. `devices`, paired devices, join the list in the same step, so a lock keeps the ones
/// someone plays on; a session from any other device ends. On a profile already locked it
/// takes only devices the profile lets in, so it never widens who opens it. Admin lane only.
#[utoipa::path(
    post,
    path = "/profiles/{id}/lock",
    tag = "profiles",
    operation_id = "lockProfile",
    params(("id" = String, Path, description = "The profile id")),
    request_body(content = Option<LockProfile>, description = "The devices that keep opening it; none when absent"),
    responses(
        (status = OK, description = "Locked", body = ProfileLock),
        (status = BAD_REQUEST, description = "Not a device fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No such profile, or no paired device with a fingerprint", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, a lock file that doesn't read, or a locked profile asked to let in another device", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "The list is full", body = ApiError),
    )
)]
pub(crate) async fn lock_profile(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    body: Option<Json<LockProfile>>,
) -> Response {
    let asked = body.map(|Json(b)| b.devices).unwrap_or_default();
    let fps: Vec<String> = asked.iter().map(|f| f.to_ascii_lowercase()).collect();
    let too_many = fps.len() > pf_seats::unlock::MAX_DEVICES;
    if too_many || !fps.iter().all(|f| pf_seats::unlock::valid_fingerprint(f)) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a device fingerprint.");
    }
    let profiles = match lockable(&st, &id).await {
        Ok(profiles) => profiles,
        Err(resp) => return resp,
    };
    let devices = match paired_devices(&st, &fps).await {
        Ok(devices) => devices,
        Err(resp) => return resp,
    };
    let locked = profiles.locks().edit(&id, |lock, now| {
        if lock.locked {
            let admitted = lock.admitted(now);
            let kept = devices.iter().all(|d| admitted.contains(&d.fingerprint));
            return kept.then_some(()).ok_or(StatusCode::CONFLICT);
        }
        let before = lock.clone();
        lock.locked = true;
        if devices.iter().all(|d| lock.admit(d, now, true)) {
            return Ok(());
        }
        *lock = before;
        Err(StatusCode::UNPROCESSABLE_ENTITY)
    });
    match locked {
        Ok(Ok(())) => state(&st, &profiles, &id).await,
        Ok(Err(StatusCode::CONFLICT)) => api_error_coded(
            StatusCode::CONFLICT,
            "profile_locked_already",
            "This profile is already locked. Add a device to let another one in.",
        ),
        Ok(Err(_)) => full(),
        Err(e) => edit_error(e),
    }
}

fn full() -> Response {
    api_error_coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "profile_devices_full",
        "This profile has all the devices it can take. Sign one out first.",
    )
}

/// Unlock a profile
///
/// Every paired device opens it again. Its list and its PIN stay for the next lock. Admin lane
/// only.
#[utoipa::path(
    delete,
    path = "/profiles/{id}/lock",
    tag = "profiles",
    operation_id = "unlockProfile",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "Open", body = ProfileLock),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
    )
)]
pub(crate) async fn unlock_profile(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    change(&st, &id, |lock| lock.locked = false).await
}

/// Set a profile's PIN
///
/// While it is locked, a device off its list opens it with this PIN, and the PIN puts it on the
/// list. Replaces any PIN it had. Admin lane only.
#[utoipa::path(
    put,
    path = "/profiles/{id}/lock/pin",
    tag = "profiles",
    operation_id = "setProfilePin",
    params(("id" = String, Path, description = "The profile id")),
    request_body = ProfilePin,
    responses(
        (status = OK, description = "Set", body = ProfileLock),
        (status = BAD_REQUEST, description = "Not 4 to 8 digits", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
    )
)]
pub(crate) async fn set_profile_pin(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Json(body): Json<ProfilePin>,
) -> Response {
    if !pf_seats::locks::valid_pin(&body.pin) {
        return api_error_coded(
            StatusCode::BAD_REQUEST,
            "profile_pin_invalid",
            "A PIN is 4 to 8 digits.",
        );
    }
    let hash = match blocking("profile_store", move || PinHash::new(&body.pin)).await {
        Ok(Some(hash)) => hash,
        Ok(None) => return host_error("profile PIN not hashed", &"no randomness for its salt"),
        Err(resp) => return resp,
    };
    change(&st, &id, |lock| lock.pin = Some(hash)).await
}

/// Clear a profile's PIN
///
/// While it is locked, only the devices on its list open it. Admin lane only.
#[utoipa::path(
    delete,
    path = "/profiles/{id}/lock/pin",
    tag = "profiles",
    operation_id = "clearProfilePin",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "Cleared", body = ProfileLock),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
    )
)]
pub(crate) async fn clear_profile_pin(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    change(&st, &id, |lock| lock.pin = None).await
}

/// Let a paired device open a profile
///
/// Puts a device paired with this host, Punktfunk or Moonlight, on the profile's list until it
/// is signed out. Answers the list. Admin lane only.
#[utoipa::path(
    post,
    path = "/profiles/{id}/devices",
    tag = "profiles",
    operation_id = "addProfileDevice",
    params(("id" = String, Path, description = "The profile id")),
    request_body = ProfileDeviceAdd,
    responses(
        (status = OK, description = "The profile's devices", body = Vec<SignedInDevice>),
        (status = BAD_REQUEST, description = "Not a device fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No such profile, or no paired device with that fingerprint", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "The list is full", body = ApiError),
    )
)]
pub(crate) async fn add_profile_device(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
    Json(body): Json<ProfileDeviceAdd>,
) -> Response {
    let fp = body.fingerprint.to_ascii_lowercase();
    if !pf_seats::unlock::valid_fingerprint(&fp) {
        return api_error(StatusCode::BAD_REQUEST, "That isn't a device fingerprint.");
    }
    let profiles = match lockable(&st, &id).await {
        Ok(profiles) => profiles,
        Err(resp) => return resp,
    };
    let device = match paired_devices(&st, std::slice::from_ref(&fp)).await {
        Ok(mut found) => found.remove(0),
        Err(resp) => return resp,
    };
    match profiles
        .locks()
        .edit(&id, |lock, now| lock.admit(&device, now, true))
    {
        Ok(true) => devices(&st, &id).await,
        Ok(false) => full(),
        Err(e) => edit_error(e),
    }
}

/// Each of `fps`, lowercase, as the device it names, under the name it paired with: natively
/// here, with Moonlight here, or on a door with Moonlight on the owner's host, which holds that
/// list. Any one that isn't paired refuses them all.
async fn paired_devices(
    st: &MgmtState,
    fps: &[String],
) -> Result<Vec<pf_seats::ipc::Device>, Response> {
    let mut owner: Option<std::collections::BTreeMap<String, Option<String>>> = None;
    let mut found = Vec::with_capacity(fps.len());
    for fp in fps {
        let label = match paired_label(st, fp) {
            Some(label) => Some(label),
            None if st.door.is_some() => {
                if owner.is_none() {
                    owner = Some(owner_moonlight(st).await?);
                }
                owner.as_ref().and_then(|list| list.get(fp).cloned())
            }
            None => None,
        };
        let Some(label) = label else {
            return Err(api_error_coded(
                StatusCode::NOT_FOUND,
                "profile_device_not_paired",
                "That device isn't paired with this host.",
            ));
        };
        found.push(crate::native::unlock::device(fp, label.as_deref()));
    }
    Ok(found)
}

/// The Moonlight devices paired with the owner's host, by fingerprint, with their labels: on a
/// door that list is the owner's row's, never the door's.
async fn owner_moonlight(
    st: &MgmtState,
) -> Result<std::collections::BTreeMap<String, Option<String>>, Response> {
    let body = match st.owner_call("GET", "clients", None, true).await {
        super::owner::Answer::Reply(200, _, body) => body,
        other => return Err(other.into_response()),
    };
    let clients: Vec<serde_json::Value> = serde_json::from_slice(&body)
        .map_err(|e| host_error("owner's Moonlight list unreadable", &e))?;
    let entry = |c: &serde_json::Value| {
        let fp = c["fingerprint"].as_str()?.to_ascii_lowercase();
        Some((fp, c["label"].as_str().map(str::to_owned)))
    };
    Ok(clients.iter().filter_map(entry).collect())
}

/// The name device `fp` paired with here, when it is paired: natively, else with Moonlight.
fn paired_label(st: &MgmtState, fp: &str) -> Option<Option<String>> {
    let paired = local_paired(st).into_iter();
    paired.filter(|(f, _)| f == fp).map(|(_, l)| l).next()
}

/// The devices paired with this host, lowercase, under the names they paired with: native ones
/// first, then Moonlight ones and their labels.
fn local_paired(st: &MgmtState) -> Vec<(String, Option<String>)> {
    let native = st.native.as_ref().map(|np| np.list()).unwrap_or_default();
    let mut paired: Vec<_> = native
        .into_iter()
        .map(|c| (c.fingerprint.to_ascii_lowercase(), Some(c.name)))
        .collect();
    let ders = st
        .app
        .paired
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if !ders.is_empty() {
        let mut labels = crate::gamestream::load_client_labels();
        let moonlight = ders.iter().map(|der| {
            let fp = hex::encode(crate::https::sha256(der));
            let label = labels.remove(&fp);
            (fp, label)
        });
        paired.extend(moonlight);
    }
    paired
}

/// `POST /profiles/{id}/unlock-challenge` for any profile not on a person's own account: a
/// challenge for the key `id`'s lock lists for device `fp`, named for `id`.
pub(super) fn challenge(st: &MgmtState, id: &str, fp: &str) -> Response {
    let Some(profiles) = st.app.profiles.get() else {
        return no_profiles();
    };
    let holder = profiles.lock_holder(id);
    match profiles.locks().challenge(id, &holder, fp) {
        Some(bytes) => Json(super::profiles::UnlockChallenge {
            challenge: hex::encode(bytes),
            seat: None,
            profile: Some(id.to_string()),
        })
        .into_response(),
        None => api_error_coded(
            StatusCode::CONFLICT,
            "profile_no_unlock_key",
            "This device has no unlock key for this profile.",
        ),
    }
}

/// `GET /profiles/{id}/devices` for a profile that takes a lock: its list.
pub(super) async fn devices(st: &MgmtState, id: &str) -> Response {
    let profiles = match readable(st, id).await {
        Ok(profiles) => profiles,
        Err(resp) => return resp,
    };
    let entries = profiles.locks().get(id).list.entries();
    let rows: Vec<SignedInDevice> = entries.into_iter().map(SignedInDevice::from).collect();
    Json(rows).into_response()
}

/// Signs `fp`, or every device, off profile `id`'s list: a session it plays there ends.
pub(super) async fn sign_out(st: &MgmtState, id: &str, fp: Option<&str>) -> Response {
    let profiles = match readable(st, id).await {
        Ok(profiles) => profiles,
        Err(resp) => return resp,
    };
    match profiles.locks().edit(id, |lock, _| lock.list.sign_out(fp)) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => edit_error(e),
    }
}
