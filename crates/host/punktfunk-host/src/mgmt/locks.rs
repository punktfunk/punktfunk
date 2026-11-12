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

fn state(profiles: &Profiles, id: &str) -> ProfileLock {
    let lock = profiles.locks().get(id);
    ProfileLock {
        locked: lock.locked,
        pin: lock.pin.is_some(),
        failed_pins: u32::try_from(profiles.locks().failures(id)).unwrap_or(u32::MAX),
    }
}

/// Applies `change` to profile `id`'s lock and answers its state.
async fn change(st: &MgmtState, id: &str, change: impl FnOnce(&mut Lock)) -> Response {
    let profiles = match lockable(st, id).await {
        Ok(profiles) => profiles,
        Err(resp) => return resp,
    };
    match profiles.locks().edit(id, |lock, _| change(lock)) {
        Ok(()) => Json(state(&profiles, id)).into_response(),
        Err(e) => edit_error(e),
    }
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
        Ok(profiles) => Json(state(&profiles, &id)).into_response(),
        Err(resp) => resp,
    }
}

/// Lock a profile
///
/// From now only the devices on its list open it, and a device that brings its PIN when it has
/// one. A session from any other device ends. Admin lane only.
#[utoipa::path(
    post,
    path = "/profiles/{id}/lock",
    tag = "profiles",
    operation_id = "lockProfile",
    params(("id" = String, Path, description = "The profile id")),
    responses(
        (status = OK, description = "Locked", body = ProfileLock),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "A person's own account, a profile on the owner's lock, or a lock file that doesn't read", body = ApiError),
    )
)]
pub(crate) async fn lock_profile(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<String>,
) -> Response {
    change(&st, &id, |lock| lock.locked = true).await
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
    let Some(label) = paired_label(&st, &fp) else {
        return api_error_coded(
            StatusCode::NOT_FOUND,
            "profile_device_not_paired",
            "That device isn't paired with this host.",
        );
    };
    let device = crate::native::unlock::device(&fp, label.as_deref());
    match profiles
        .locks()
        .edit(&id, |lock, now| lock.admit(&device, now, true))
    {
        Ok(true) => devices(&st, &id).await,
        Ok(false) => api_error_coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "profile_devices_full",
            "This profile has all the devices it can take. Sign one out first.",
        ),
        Err(e) => edit_error(e),
    }
}

/// The name device `fp` paired with here, when it is paired: natively, else with Moonlight.
fn paired_label(st: &MgmtState, fp: &str) -> Option<Option<String>> {
    let native = st.native.as_ref().and_then(|np| {
        let paired = np.list().into_iter();
        paired
            .filter(|c| c.fingerprint.eq_ignore_ascii_case(fp))
            .map(|c| c.name)
            .next()
    });
    if native.is_some() {
        return Some(native);
    }
    let ders = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    let moonlight = ders
        .iter()
        .any(|der| hex::encode(crate::https::sha256(der)) == fp);
    moonlight.then(|| crate::gamestream::load_client_labels().remove(fp))
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
