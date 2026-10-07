//! Native (punktfunk/1) pairing: arm/disarm a PIN window, list and unpair
//! clients, and approve pending knocks.

use super::shared::*;
use crate::native_pairing::{Access, PairedClient};
use punktfunk_core::quic::{
    GRANT_ALL, GRANT_PRESET_CONTROLLER_ONLY, GRANT_PRESET_FULL, GRANT_PRESET_VIEW_ONLY,
    GRANT_RESERVED,
};

/// Saturating add: a `u64` of seconds can overflow `i64`. Wrap would look
/// like a past deadline; overflow means "effectively forever".
fn absolute_expiry(expires_in_secs: u64) -> i64 {
    crate::clock::unix_secs().saturating_add(i64::try_from(expires_in_secs).unwrap_or(i64::MAX))
}

/// 400 when reserved bits are set. Never silently cleared: a newer console
/// must learn its bit did not take, not vanish.
/// `until_disconnect` rides alongside an access choice. On its own it would mean "replace this
/// device's access with full and permanent, then end it at disconnect" — an escalation nobody
/// asked for — so say so instead of guessing. Editing one field of a stored record is what
/// `PATCH /native/clients/{fingerprint}` is for.
fn reject_lone_until_disconnect(
    grants: Option<u32>,
    expires_in_secs: Option<u64>,
    until_disconnect: Option<bool>,
) -> Option<Response> {
    if until_disconnect.is_some() && grants.is_none() && expires_in_secs.is_none() {
        return Some(api_error(
            StatusCode::BAD_REQUEST,
            "until_disconnect needs an access level beside it. Send grants as well, or edit the \
             device's access instead.",
        ));
    }
    None
}

pub(crate) fn reject_reserved(grants: u32) -> Option<Response> {
    if grants & GRANT_RESERVED != 0 {
        return Some(api_error(
            StatusCode::BAD_REQUEST,
            &format!("grants has reserved bits set (the valid mask is 0x{GRANT_ALL:x})"),
        ));
    }
    None
}

/// Merge optional `grants` + `expires_in_secs`. `None` when both omitted
/// (pre-grants behavior). Else grants-only is permanent, expiry-only is full
/// until then. Caller already ran [`reject_reserved`].
fn chosen_access(
    grants: Option<u32>,
    expires_in_secs: Option<u64>,
    until_disconnect: Option<bool>,
) -> Option<Access> {
    // `until_disconnect` alone is NOT a choice: `Access` replaces the whole record, so honouring
    // it by itself would hand a re-pairing device full permanent control it never had. Callers
    // that send it alone are turned away by `reject_lone_until_disconnect`.
    if grants.is_none() && expires_in_secs.is_none() {
        return None;
    }
    Some(Access {
        grants: grants.unwrap_or(GRANT_ALL),
        expires_unix: expires_in_secs.map(absolute_expiry),
        until_disconnect: until_disconnect.unwrap_or(false),
    })
}

/// The three named levels the console offers, as masks. `None` = not one of them;
/// a caller that wants another shape sends `grants` instead.
pub(crate) fn grants_for_level(level: &str) -> Option<u32> {
    match level {
        "full" => Some(GRANT_PRESET_FULL),
        "controller" => Some(GRANT_PRESET_CONTROLLER_ONLY),
        "view" => Some(GRANT_PRESET_VIEW_ONLY),
        _ => None,
    }
}

/// Display name of a grant mask. Derived, never stored — two copies would
/// drift. Absent = full; reserved bits ignored (same reading as enforcement).
pub(crate) fn access_level(grants: Option<u32>) -> &'static str {
    match grants.unwrap_or(GRANT_ALL) & GRANT_ALL {
        m if m == GRANT_PRESET_FULL => "full",
        m if m == GRANT_PRESET_CONTROLLER_ONLY => "controller",
        m if m == GRANT_PRESET_VIEW_ONLY => "view",
        _ => "custom",
    }
}

/// Native pairing window. Unlike GameStream, the host mints the PIN (SPAKE2
/// needs it client-side first); the console displays it.
#[derive(Serialize, ToSchema)]
pub(crate) struct NativePairStatus {
    /// True when this process started with `--native`.
    enabled: bool,
    armed: bool,
    #[schema(example = "1234")]
    pin: Option<String>,
    /// Seconds left in the window. `null` if disarmed, or armed with no expiry (CLI flag).
    expires_in_secs: Option<u64>,
    paired_clients: u32,
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct ArmNativePairing {
    /// Window length, seconds. Default 120; clamped to 15–600.
    #[schema(example = 120)]
    ttl_secs: Option<u32>,
    /// Hex SHA-256 fingerprint that may consume this window. Omit for any
    /// device (trusted-LAN only).
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: Option<String>,
    /// Grant mask for the device that completes the window. Reserved bits are
    /// 400. Omit both access fields for full (new) or preserved (re-pair).
    #[schema(example = 1)]
    grants: Option<u32>,
    /// Access lifetime from now, seconds — not `ttl_secs`. Host stores the
    /// absolute deadline. Omit for permanent (`grants` set) or preserved (neither).
    #[schema(example = 14400)]
    expires_in_secs: Option<u64>,
    /// Drop the device's record once its last session ends, rather than at a clock time.
    /// Combines with `expires_in_secs`: whichever comes first ends the grant. Send it with an
    /// access level, never alone (400) — like the other access fields it is part of a whole
    /// replacement, so sending `grants` without it clears it.
    until_disconnect: Option<bool>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct NativeClient {
    #[schema(example = "Living Room iPad")]
    name: String,
    /// Hex SHA-256 of the client certificate — the stable id.
    fingerprint: String,
    /// `GRANT_*` bits 0–5. `null` is a pre-grants record, which means full.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// Absolute expiry, unix seconds on the host clock. `null` = permanent.
    /// An expired device stays listed; it is just not authorized.
    expires_unix: Option<i64>,
    /// Last grant time, unix seconds. Display/audit only; never enforced.
    granted_unix: Option<i64>,
    /// `full` | `controller` | `view` | `custom`. Derived from `grants`;
    /// absent only on hosts older than this field.
    #[schema(example = "controller")]
    access_level: Option<String>,
    /// The record is dropped when the device's last session ends, rather than at a clock
    /// time. `expires_unix` may also be set; whichever comes first ends the grant.
    until_disconnect: bool,
    /// Player slot this device's pads take, 0-based; `null` = whichever comes free.
    /// Set through `PUT /session/{id}/player` while the device streams.
    // `value_type`: an `Option<u8>` alone generates as `never` in the SDK.
    #[schema(value_type = u32, required = false)]
    preferred_pad_slot: Option<u8>,
}

impl NativeClient {
    /// One place derives `access_level`, so list / approve / PATCH never disagree.
    fn from_record(c: PairedClient) -> NativeClient {
        NativeClient {
            access_level: Some(access_level(c.grants).to_string()),
            name: c.name,
            fingerprint: c.fingerprint,
            grants: c.grants,
            expires_unix: c.expires_unix,
            granted_unix: c.granted_unix,
            until_disconnect: c.until_disconnect,
            preferred_pad_slot: c.preferred_pad_slot,
        }
    }
}

/// Knock awaiting delegated approval (pair here instead of fetching a PIN).
#[derive(Serialize, ToSchema)]
pub(crate) struct PendingDevice {
    /// Approve/deny id. Per-process; entries expire after ~10 minutes.
    id: u32,
    /// Client's own name, else fingerprint-derived.
    #[schema(example = "Enrico's MacBook")]
    name: String,
    /// Hex SHA-256 of the device certificate — what approval pins.
    fingerprint: String,
    age_secs: u64,
    /// Stored mask if this fingerprint was paired before (expired-guest
    /// re-knock). `null` if unknown, or a pre-grants record (= full).
    grants: Option<u32>,
    /// Stored absolute expiry (unix seconds; often already past). `null` if
    /// unknown or permanent.
    expires_unix: Option<i64>,
    /// Stored grant time, unix seconds. `null` if unknown.
    granted_unix: Option<i64>,
    /// Stored mask's preset. `null` with no stored record — unlike
    /// [`NativeClient`], where it is always derivable.
    #[schema(example = "controller")]
    access_level: Option<String>,
    /// Where the knock came from: `"lan"` or `"wan"`. A `"wan"` device cannot be admitted by
    /// approve — arm a PIN bound to its fingerprint instead.
    #[schema(example = "lan")]
    source: String,
    /// Stored "this session" setting if this fingerprint was paired before. `false` if unknown.
    until_disconnect: bool,
    /// The profile the device asked to play as. Absent when it named none this host knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<crate::events::ProfileRef>,
}

/// Approve body. `{}` keeps the knock name and, on re-approve, stored access
/// (full/permanent on a first pairing).
#[derive(Deserialize, ToSchema)]
pub(crate) struct ApprovePending {
    /// Label; defaults to the name the device knocked with.
    #[schema(example = "Living Room TV")]
    name: Option<String>,
    /// Grant mask. Reserved bits are 400. Omit both access fields to keep
    /// stored access; `grants` alone is permanent.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// Access lifetime from now, seconds. Host stores the absolute deadline.
    /// Alone: full control until then.
    #[schema(example = 14400)]
    expires_in_secs: Option<u64>,
    /// Drop the device's record once its last session ends, rather than at a clock time.
    /// Combines with `expires_in_secs`: whichever comes first ends the grant. Send it with an
    /// access level, never alone (400) — like the other access fields it is part of a whole
    /// replacement, so sending `grants` without it clears it.
    until_disconnect: Option<bool>,
}

/// Partial PATCH of a paired device's grants/expiry. Omitted fields keep their
/// current value.
#[derive(Deserialize, ToSchema)]
pub(crate) struct UpdateNativeAccess {
    /// New `GRANT_*` mask; reserved bits are 400. Omit to keep current grants.
    #[schema(example = 1)]
    grants: Option<u32>,
    /// New expiry from now, seconds. `0` expires now. Omit to keep current.
    /// Mutually exclusive with `clear_expiry` (400).
    #[schema(example = 14400)]
    expires_in_secs: Option<u64>,
    /// `true` makes access permanent. Mutually exclusive with `expires_in_secs` (400).
    clear_expiry: Option<bool>,
    /// Drop the record once the device's last session ends. Omit to keep the current setting.
    until_disconnect: Option<bool>,
}

pub(crate) fn native_status(st: &MgmtState) -> NativePairStatus {
    match &st.native {
        Some(np) => {
            let s = np.status();
            NativePairStatus {
                enabled: true,
                armed: s.armed,
                pin: s.pin,
                expires_in_secs: s.expires_in_secs,
                paired_clients: s.paired_clients,
            }
        }
        None => NativePairStatus {
            enabled: false,
            armed: false,
            pin: None,
            expires_in_secs: None,
            paired_clients: 0,
        },
    }
}

/// Native pairing status
///
/// Poll while armed to show the PIN and countdown. `enabled: false` means
/// GameStream only (no `--native`).
#[utoipa::path(
    get,
    path = "/native/pair",
    tag = "native",
    operation_id = "getNativePairing",
    responses(
        (status = OK, description = "Native pairing status", body = NativePairStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_native_pairing(State(st): State<Arc<MgmtState>>) -> Json<NativePairStatus> {
    Json(native_status(&st))
}

/// Arm native pairing
///
/// Opens a window and mints a PIN. `grants` / `expires_in_secs` apply to
/// whichever device completes the ceremony.
#[utoipa::path(
    post,
    path = "/native/pair/arm",
    tag = "native",
    operation_id = "armNativePairing",
    request_body = ArmNativePairing,
    responses(
        (status = OK, description = "Pairing armed; the response carries the PIN to display", body = NativePairStatus),
        (status = BAD_REQUEST, description = "Reserved grant bits set", body = ApiError),
        (status = CONFLICT, description = "A seat host: devices pair with the box", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not available in this process", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn arm_native_pairing(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<ArmNativePairing>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "native host not available in this process",
        );
    };
    if np.pairing_refused() {
        return api_error(
            StatusCode::CONFLICT,
            "This is a seat. Pair devices with the box in its console.",
        );
    }
    // 400 must not leave a window open — validate grants before `arm_for`.
    if let Some(resp) = req.grants.and_then(reject_reserved) {
        return resp;
    }
    if let Some(resp) =
        reject_lone_until_disconnect(req.grants, req.expires_in_secs, req.until_disconnect)
    {
        return resp;
    }
    let access = chosen_access(req.grants, req.expires_in_secs, req.until_disconnect);
    let ttl = req.ttl_secs.unwrap_or(120).clamp(15, 600);
    // Empty/missing fingerprint is unbound; do not treat "" as bound-to-empty.
    let bound = req
        .fingerprint
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase());
    let bound_to_device = bound.is_some();
    // `None` access: full/permanent for a new device, preserved for a re-pair.
    let _pin = np.arm_for(std::time::Duration::from_secs(ttl as u64), bound, access);
    tracing::info!(
        ttl_secs = ttl,
        bound_to_device,
        with_access = access.is_some(),
        "management API: native pairing armed"
    );
    Json(native_status(&st)).into_response()
}

/// Disarm native pairing
#[utoipa::path(
    delete,
    path = "/native/pair",
    tag = "native",
    operation_id = "disarmNativePairing",
    responses(
        (status = NO_CONTENT, description = "Pairing disarmed"),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn disarm_native_pairing(State(st): State<Arc<MgmtState>>) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    np.disarm();
    StatusCode::NO_CONTENT.into_response()
}

/// List native paired clients
#[utoipa::path(
    get,
    path = "/native/clients",
    tag = "native",
    operation_id = "listNativeClients",
    responses(
        (status = OK, description = "Paired native clients", body = [NativeClient]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_native_clients(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<NativeClient>> {
    let clients = match &st.native {
        Some(np) => np
            .list()
            .into_iter()
            .map(NativeClient::from_record)
            .collect(),
        None => Vec::new(),
    };
    Json(clients)
}

/// Unpair a native client
#[utoipa::path(
    delete,
    path = "/native/clients/{fingerprint}",
    tag = "native",
    operation_id = "unpairNativeClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 of the client certificate (case-insensitive)")
    ),
    responses(
        (status = NO_CONTENT, description = "Client unpaired"),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = NOT_FOUND, description = "No paired native client with that fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn unpair_native_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.remove(&fingerprint) {
        Ok(true) => {
            // Unpair also stops a live session; the store write alone would leave it streaming.
            let stopped =
                crate::session_status::stop_by_fingerprint(&fingerprint.to_ascii_lowercase());
            if stopped > 0 {
                tracing::info!(
                    fingerprint,
                    stopped,
                    "unpair: live native session(s) stopped"
                );
            }
            // The overlay is keyed by fingerprint, and a fingerprint is a
            // certificate — but a device re-paired after a key rotation is a
            // different device to the operator, and inheriting settings made for
            // the old one is a surprise. Drop them with the pairing.
            crate::mgmt::display::forget_display_overlay(&fingerprint);
            tracing::info!(fingerprint, "management API: native client unpaired");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "no paired native client with that fingerprint",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the paired-device list — {e}"),
        ),
    }
}

/// Update a native client's access
///
/// Partial edit of grants/expiry. Omitted fields keep their current value;
/// live sessions pick it up immediately. 404 if the fingerprint is not paired
/// — this is not a pair path.
#[utoipa::path(
    patch,
    path = "/native/clients/{fingerprint}",
    tag = "native",
    operation_id = "updateNativeClientAccess",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 of the client certificate (case-insensitive)")
    ),
    request_body = UpdateNativeAccess,
    responses(
        (status = OK, description = "Access updated; the stored record as now in force", body = NativeClient),
        (status = BAD_REQUEST, description = "Reserved grant bits set, or expires_in_secs together with clear_expiry", body = ApiError),
        (status = NOT_FOUND, description = "No paired native client with that fingerprint", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the paired-device list", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn update_native_client_access(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
    ApiJson(req): ApiJson<UpdateNativeAccess>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    if let Some(resp) = req.grants.and_then(reject_reserved) {
        return resp;
    }
    if req.clear_expiry == Some(true) && req.expires_in_secs.is_some() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "expires_in_secs and clear_expiry conflict — send one or the other",
        );
    }
    // `set_access` overwrites the whole Access; merge omitted halves here.
    let Some(current) = np
        .list()
        .into_iter()
        .find(|c| c.fingerprint.eq_ignore_ascii_case(&fingerprint))
    else {
        return api_error(
            StatusCode::NOT_FOUND,
            "no paired native client with that fingerprint",
        );
    };
    let access = Access {
        grants: match req.grants {
            Some(g) => g,
            // Same reading as enforcement (absent = full, reserved bits off) so
            // an expiry-only PATCH re-stores what is in force.
            None => current.grants.unwrap_or(GRANT_ALL) & GRANT_ALL,
        },
        expires_unix: if req.clear_expiry == Some(true) {
            None
        } else {
            match req.expires_in_secs {
                Some(s) => Some(absolute_expiry(s)),
                None => current.expires_unix,
            }
        },
        // Omitted keeps what is stored: `set_access` overwrites the whole record, so a PATCH
        // that only moves the expiry must not quietly turn a this-session grant permanent.
        until_disconnect: req.until_disconnect.unwrap_or(current.until_disconnect),
    };
    match np.set_access(&fingerprint, access) {
        Ok(true) => {
            tracing::info!(
                fingerprint,
                grants = access.grants,
                expires_unix = access.expires_unix,
                until_disconnect = access.until_disconnect,
                "management API: native client access updated"
            );
            // Re-read: the store stamped `granted_unix`. If an unpair won the
            // race, fall back to what this write stored — it did land.
            let stored = np
                .list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(&fingerprint))
                .unwrap_or(PairedClient {
                    name: current.name,
                    fingerprint: current.fingerprint,
                    grants: Some(access.grants),
                    expires_unix: access.expires_unix,
                    granted_unix: Some(crate::clock::unix_secs()),
                    until_disconnect: access.until_disconnect,
                    preferred_pad_slot: current.preferred_pad_slot,
                });
            Json(NativeClient::from_record(stored)).into_response()
        }
        // Record vanished between the read above and the write (unpair raced).
        Ok(false) => api_error(
            StatusCode::NOT_FOUND,
            "no paired native client with that fingerprint",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the paired-device list — {e}"),
        ),
    }
}

/// Unpair every native client
///
/// One persisted write, not a loop (a mid-loop failure would half-empty the
/// store), and every live native session those clients own is stopped.
/// Idempotent, so 200 rather than the single unpair's 204/404: an empty store
/// still succeeds, and `unpaired` tells the operator what it meant.
#[utoipa::path(
    delete,
    path = "/native/clients",
    tag = "native",
    operation_id = "unpairAllNativeClients",
    responses(
        (status = OK, description = "Every native client unpaired (possibly none)", body = UnpairAllResult),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the paired-device list", body = ApiError),
    )
)]
pub(crate) async fn unpair_all_native_clients(State(st): State<Arc<MgmtState>>) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    match np.remove_all() {
        Ok(removed) => {
            // Same live-session stop as the single unpair, applied to the set.
            let stopped: usize = removed
                .iter()
                .map(|fp| crate::session_status::stop_by_fingerprint(&fp.to_ascii_lowercase()))
                .sum();
            if stopped > 0 {
                tracing::info!(stopped, "unpair-all: live native session(s) stopped");
            }
            for fp in &removed {
                crate::mgmt::display::forget_display_overlay(fp);
            }
            let unpaired = removed.len() as u32;
            tracing::info!(unpaired, "management API: all native clients unpaired");
            Json(UnpairAllResult { unpaired }).into_response()
        }
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the paired-device list — {e}"),
        ),
    }
}

/// List devices awaiting pairing approval
///
/// Unpaired knocks while pairing is required. Approve to pair without a PIN.
/// Entries expire after ~10 minutes.
#[utoipa::path(
    get,
    path = "/native/pending",
    tag = "native",
    operation_id = "listPendingDevices",
    responses(
        (status = OK, description = "Devices awaiting approval (empty when none, or when the \
                                     native host is not enabled)", body = Vec<PendingDevice>),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_pending_devices(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<PendingDevice>> {
    // A listed fingerprint can knock again (expired guest). Surface its stored
    // access so the dialog can re-grant it.
    let (pending, paired) = match &st.native {
        Some(np) => (np.pending(), np.list()),
        None => (Vec::new(), Vec::new()),
    };
    Json(
        pending
            .into_iter()
            .map(|p| {
                let stored = paired
                    .iter()
                    .find(|c| c.fingerprint.eq_ignore_ascii_case(&p.fingerprint));
                PendingDevice {
                    id: p.id,
                    name: p.name,
                    fingerprint: p.fingerprint,
                    age_secs: p.age_secs,
                    grants: stored.and_then(|c| c.grants),
                    expires_unix: stored.and_then(|c| c.expires_unix),
                    granted_unix: stored.and_then(|c| c.granted_unix),
                    access_level: stored.map(|c| access_level(c.grants).to_string()),
                    until_disconnect: stored.is_some_and(|c| c.until_disconnect),
                    source: match p.source {
                        crate::native_pairing::KnockSource::Lan => "lan".into(),
                        crate::native_pairing::KnockSource::Wan => "wan".into(),
                    },
                    profile: p
                        .profile
                        .as_deref()
                        .and_then(|id| st.app.profiles.get()?.get(id))
                        .map(|p| crate::events::ProfileRef {
                            id: p.id,
                            display_name: p.display_name,
                        }),
                }
            })
            .collect(),
    )
}

/// Approve a pending device
///
/// Pairs the fingerprint immediately (no PIN). `{}` keeps the knock name and
/// stored access (full/permanent on first pairing). The response is the stored
/// record, not necessarily this request's inputs.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/approve",
    tag = "native",
    operation_id = "approvePendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    request_body = ApprovePending,
    responses(
        (status = OK, description = "Device paired; the stored record as now in force", body = NativeClient),
        (status = BAD_REQUEST, description = "Reserved grant bits set", body = ApiError),
        (status = NOT_FOUND, description = "No pending request with that id (expired?)", body = ApiError),
        (status = CONFLICT, description = "Knock came from the internet; arm a fingerprint-bound PIN instead", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the paired-device list", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn approve_pending_device(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<u32>,
    ApiJson(req): ApiJson<ApprovePending>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    // Reserved bits 400 before `approve_pending` so a bad mask does not consume the knock.
    if let Some(resp) = req.grants.and_then(reject_reserved) {
        return resp;
    }
    if let Some(resp) =
        reject_lone_until_disconnect(req.grants, req.expires_in_secs, req.until_disconnect)
    {
        return resp;
    }
    let access = chosen_access(req.grants, req.expires_in_secs, req.until_disconnect);
    match np.approve_pending(id, req.name.as_deref(), access) {
        Ok(crate::native_pairing::ApproveOutcome::Paired(client)) => {
            tracing::info!(name = %client.name, fingerprint = %client.fingerprint,
                with_access = access.is_some(),
                "management API: pending device approved (delegated pairing)");
            Json(NativeClient::from_record(client)).into_response()
        }
        Ok(crate::native_pairing::ApproveOutcome::NotFound) => api_error(
            StatusCode::NOT_FOUND,
            "no pending request with that id (it may have expired — have the device retry)",
        ),
        Ok(crate::native_pairing::ApproveOutcome::WanNeedsBoundPin) => api_error(
            StatusCode::CONFLICT,
            "Couldn't admit this device: it knocked from the internet, where the name it sent \
             proves nothing. Arm a PIN bound to its fingerprint instead, then read the PIN out \
             to whoever is holding it.",
        ),
        Err(e) => api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the paired-device list — {e}"),
        ),
    }
}

/// Deny a pending device
///
/// Drops the request. Not a blocklist — the next attempt knocks again.
#[utoipa::path(
    post,
    path = "/native/pending/{id}/deny",
    tag = "native",
    operation_id = "denyPendingDevice",
    params(("id" = u32, Path, description = "Pending-request id from the pending list")),
    responses(
        (status = NO_CONTENT, description = "Request dropped"),
        (status = NOT_FOUND, description = "No pending request with that id", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Native host not enabled", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn deny_pending_device(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<u32>,
) -> Response {
    let Some(np) = &st.native else {
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "native host not enabled");
    };
    if np.deny_pending(id) {
        tracing::info!(id, "management API: pending device denied");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, "no pending request with that id")
    }
}
