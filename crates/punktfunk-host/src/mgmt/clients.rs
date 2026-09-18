//! Paired Moonlight clients and the GameStream pairing PIN flow.

use super::shared::*;
use sha2::{Digest, Sha256};

/// A paired (certificate-pinned) Moonlight client.
#[derive(Serialize, ToSchema)]
pub(crate) struct PairedClient {
    /// Lowercase hex SHA-256 of the client certificate DER — the client's stable id here.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: String,
    /// Certificate subject if the DER parses. Do not display as a device name: every
    /// moonlight-common-c client self-signs `CN=NVIDIA GameStream Client`, so this names
    /// the protocol. [`Self::label`] is the field to show.
    subject: Option<String>,
    /// Operator-assigned display name (`PATCH /clients/{fp}`). The only way to tell two
    /// paired Moonlight devices apart; absent until somebody names the device.
    #[schema(example = "Living Room TV")]
    label: Option<String>,
    /// Certificate validity start (unix seconds).
    not_before_unix: Option<i64>,
    /// Certificate validity end (unix seconds).
    not_after_unix: Option<i64>,
}

/// Pairing-flow status.
#[cfg(feature = "gamestream")]
#[derive(Serialize, ToSchema)]
pub(crate) struct PairingStatus {
    pin_pending: bool,
    /// Parked ceremonies. Echo this identity in the submit so the PIN addresses the
    /// ceremony the operator saw, not a later arrival.
    pending: Vec<PendingCeremony>,
}

/// One pairing handshake parked waiting for its PIN.
#[cfg(feature = "gamestream")]
#[derive(Serialize, ToSchema)]
pub(crate) struct PendingCeremony {
    #[schema(example = "0123456789ABCDEF")]
    uniqueid: String,
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: String,
    #[schema(example = "192.168.1.42")]
    peer_ip: String,
}

/// PIN plus the exact ceremony selected from `GET /pair`.
#[cfg(feature = "gamestream")]
#[derive(Deserialize, ToSchema)]
pub(crate) struct SubmitPin {
    #[schema(example = "1234")]
    pin: String,
    uniqueid: String,
    fingerprint: String,
    peer_ip: String,
    /// Name for this device, scrubbed like `PATCH /clients/{fp}` and stored once the pairing
    /// completes. Every Moonlight client names itself the same, so without one the device is
    /// listed by fingerprint until somebody renames it.
    #[schema(example = "Living Room TV")]
    label: Option<String>,
}

/// List paired clients
#[utoipa::path(
    get,
    path = "/clients",
    tag = "clients",
    operation_id = "listPairedClients",
    responses(
        (status = OK, description = "All certificate-pinned clients", body = [PairedClient]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_paired_clients(
    State(st): State<Arc<MgmtState>>,
) -> Json<Vec<PairedClient>> {
    let ders = st
        .app
        .paired
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    // One read of the label sidecar for the whole list, not one per row.
    let labels = crate::gamestream::load_client_labels();
    Json(ders.iter().map(|der| client_info(der, &labels)).collect())
}

pub(crate) fn client_info(
    der: &[u8],
    labels: &std::collections::BTreeMap<String, String>,
) -> PairedClient {
    let fingerprint = hex::encode(Sha256::digest(der));
    let label = labels.get(&fingerprint).cloned();
    match x509_parser::parse_x509_certificate(der) {
        Ok((_, x509)) => PairedClient {
            subject: Some(x509.subject().to_string()),
            not_before_unix: Some(x509.validity().not_before.timestamp()),
            not_after_unix: Some(x509.validity().not_after.timestamp()),
            label,
            fingerprint,
        },
        Err(_) => PairedClient {
            subject: None,
            not_before_unix: None,
            not_after_unix: None,
            label,
            fingerprint,
        },
    }
}

/// Body of `PATCH /clients/{fingerprint}` — the device's display name.
#[derive(Deserialize, ToSchema)]
pub(crate) struct RenameClient {
    /// Display name. `null` or empty/whitespace clears it (listed by fingerprint alone).
    ///
    /// Scrubbed with the native-plane sanitizer: control characters and Unicode bidi
    /// overrides stripped (one device could impersonate another in this list), whitespace
    /// collapsed, capped at 64 characters.
    #[schema(example = "Living Room TV")]
    label: Option<String>,
}

/// Rename a paired client
///
/// Cosmetic: no certificate, no trust change. Stored beside the pairing store; unpairing
/// the device forgets it.
#[utoipa::path(
    patch,
    path = "/clients/{fingerprint}",
    tag = "clients",
    operation_id = "renameClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 fingerprint of the client certificate DER (64 chars, case-insensitive)")
    ),
    request_body = RenameClient,
    responses(
        (status = OK, description = "The client as it now reads", body = PairedClient),
        (status = BAD_REQUEST, description = "Malformed fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No paired client with that fingerprint", body = ApiError),
    )
)]
pub(crate) async fn rename_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
    Json(body): Json<RenameClient>,
) -> Response {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "fingerprint must be the 64-char hex SHA-256 of the client certificate DER",
        );
    }
    // A label for an unknown fingerprint is invisible and sits in the file forever: unpair
    // cleanup only removes labels whose device was paired.
    let paired = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    let Some(der) = paired
        .iter()
        .find(|der| hex::encode(Sha256::digest(der)).eq_ignore_ascii_case(&fingerprint))
        .cloned()
    else {
        return api_error(
            StatusCode::NOT_FOUND,
            "no paired client with that fingerprint",
        );
    };
    drop(paired);
    // All-whitespace is a clear, not a device named "   ": the sanitizer would otherwise
    // turn it into the "device <fp8>" fallback and the row would look renamed.
    let wanted = body
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty());
    crate::gamestream::set_client_label(&fingerprint, wanted);
    let labels = crate::gamestream::load_client_labels();
    (StatusCode::OK, Json(client_info(&der, &labels))).into_response()
}

/// Unpair a client
///
/// Persisted. A live GameStream session owned by this certificate is ended
/// (TERMINATION+disconnect). Removing the last pairing closes the ENet control port.
/// nvhttp TLS still completes a handshake with any well-formed client cert — authorization
/// is per-request via the paired-fingerprint check.
#[utoipa::path(
    delete,
    path = "/clients/{fingerprint}",
    tag = "clients",
    operation_id = "unpairClient",
    params(
        ("fingerprint" = String, Path,
         description = "Hex SHA-256 fingerprint of the client certificate DER (64 chars, case-insensitive)")
    ),
    responses(
        (status = NO_CONTENT, description = "Client unpaired"),
        (status = BAD_REQUEST, description = "Malformed fingerprint", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No paired client with that fingerprint", body = ApiError),
    )
)]
pub(crate) async fn unpair_client(
    State(st): State<Arc<MgmtState>>,
    Path(fingerprint): Path<String>,
) -> Response {
    if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "fingerprint must be the 64-char hex SHA-256 of the client certificate DER",
        );
    }
    let mut paired = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    let before = paired.len();
    paired.retain(|der| !hex::encode(Sha256::digest(der)).eq_ignore_ascii_case(&fingerprint));
    if paired.len() < before {
        // Without this, a restart would resurrect the pairing and silently re-open the
        // control port.
        crate::gamestream::save_paired(&paired);
        // Forget the label with the pairing so the file cannot grow without bound; a later
        // re-pair of the same cert starts unnamed.
        crate::gamestream::retain_client_labels(&paired);
        drop(paired);
        // A mid-stream client must not keep streaming. Clearing the launch makes the ENet
        // thread send TERMINATION+disconnect. An owner-less launch (cert unreadable at
        // /launch) cannot be attributed; last-pairing port teardown covers it.
        let removed_fp: Option<[u8; 32]> = hex::decode(&fingerprint)
            .ok()
            .and_then(|v| v.try_into().ok());
        let live_owner = st
            .app
            .launch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .and_then(|l| l.owner_fp);
        if removed_fp.is_some() && removed_fp == live_owner {
            st.app.quit_session("client unpaired");
        }
        // Last pairing closes the ENet control port. No-op while others remain, or on a
        // native-only host where the gate is never armed.
        if let Err(e) = crate::gamestream::sync_control(&st.app) {
            tracing::warn!(error = %format!("{e:#}"), "control port sync after unpair failed");
        }
        tracing::info!(fingerprint, "management API: client unpaired");
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(
            StatusCode::NOT_FOUND,
            "no paired client with that fingerprint",
        )
    }
}

/// Unpair every client
///
/// Collection form of [`unpair_client`]: one persisted write, same revocation across the
/// set. Idempotent, so 200 rather than 204/404 — an already-empty store still satisfies
/// "unpair everything", and the body says whether that was three devices or none.
#[utoipa::path(
    delete,
    path = "/clients",
    tag = "clients",
    operation_id = "unpairAllClients",
    responses(
        (status = OK, description = "Every client unpaired (possibly none)", body = UnpairAllResult),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn unpair_all_clients(State(st): State<Arc<MgmtState>>) -> Response {
    let mut paired = st.app.paired.lock().unwrap_or_else(|e| e.into_inner());
    if paired.is_empty() {
        return Json(UnpairAllResult { unpaired: 0 }).into_response();
    }
    let removed: Vec<[u8; 32]> = paired
        .iter()
        .map(|der| Sha256::digest(der).into())
        .collect();
    paired.clear();
    // Persist under the lock, as the single unpair does: a pairing resurrected by a restart
    // would silently re-open the control port.
    crate::gamestream::save_paired(&paired);
    crate::gamestream::retain_client_labels(&paired);
    drop(paired);
    // Clearing the launch sends TERMINATION+disconnect. An owner-less launch cannot be
    // attributed; port teardown always fires here because no pairing remains.
    let live_owner = st
        .app
        .launch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .and_then(|l| l.owner_fp);
    if live_owner.is_some_and(|fp| removed.contains(&fp)) {
        st.app.quit_session("client unpaired");
    }
    if let Err(e) = crate::gamestream::sync_control(&st.app) {
        tracing::warn!(error = %format!("{e:#}"), "control port sync after unpair-all failed");
    }
    let unpaired = removed.len() as u32;
    tracing::info!(unpaired, "management API: all clients unpaired");
    Json(UnpairAllResult { unpaired }).into_response()
}

/// Pairing-flow status
///
/// Poll this to know when to prompt for the PIN Moonlight displays.
#[cfg(feature = "gamestream")]
#[utoipa::path(
    get,
    path = "/pair",
    tag = "pairing",
    operation_id = "getPairingStatus",
    responses(
        (status = OK, description = "Whether a pairing handshake is waiting for a PIN", body = PairingStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_pairing_status(State(st): State<Arc<MgmtState>>) -> Json<PairingStatus> {
    let pending: Vec<PendingCeremony> = st
        .app
        .pairing
        .pin
        .pending()
        .into_iter()
        .map(|c| PendingCeremony {
            uniqueid: c.uniqueid,
            fingerprint: c.fingerprint,
            peer_ip: c.peer_ip.to_string(),
        })
        .collect();
    Json(PairingStatus {
        pin_pending: !pending.is_empty(),
        pending,
    })
}

/// Submit the pairing PIN
///
/// Completes the out-of-band half of the handshake.
#[cfg(feature = "gamestream")]
#[utoipa::path(
    post,
    path = "/pair/pin",
    tag = "pairing",
    operation_id = "submitPairingPin",
    request_body = SubmitPin,
    responses(
        (status = NO_CONTENT, description = "PIN delivered to the waiting handshake"),
        (status = BAD_REQUEST, description = "Malformed PIN or unparseable JSON body", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No pairing handshake is waiting for a PIN", body = ApiError),
        (status = UNSUPPORTED_MEDIA_TYPE, description = "Body is not application/json", body = ApiError),
        (status = UNPROCESSABLE_ENTITY, description = "JSON body does not match the schema", body = ApiError),
    )
)]
pub(crate) async fn submit_pairing_pin(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<SubmitPin>,
) -> Response {
    let pin = req.pin.trim();
    if pin.is_empty() || pin.len() > 16 || !pin.bytes().all(|b| b.is_ascii_digit()) {
        return api_error(StatusCode::BAD_REQUEST, "pin must be 1-16 ASCII digits");
    }
    if req.uniqueid.is_empty()
        || req.uniqueid.len() > 128
        || req.fingerprint.len() != 64
        || !req.fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return api_error(StatusCode::BAD_REQUEST, "invalid pairing ceremony identity");
    }
    let Ok(peer_ip) = req.peer_ip.parse() else {
        return api_error(StatusCode::BAD_REQUEST, "invalid pairing ceremony peer_ip");
    };
    // All-whitespace is no name, as in `rename_client`. The sanitizer caps the length.
    let label = req
        .label
        .as_deref()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string);
    let target = crate::gamestream::pairing::CeremonyId {
        uniqueid: req.uniqueid,
        fingerprint: req.fingerprint,
        peer_ip,
    };
    use crate::gamestream::pairing::SubmitOutcome;
    match st.app.pairing.pin.submit(pin.to_string(), label, &target) {
        SubmitOutcome::Delivered(_) => StatusCode::NO_CONTENT.into_response(),
        SubmitOutcome::NoWaiter => api_error(
            StatusCode::CONFLICT,
            "no pairing handshake is waiting for a PIN",
        ),
        SubmitOutcome::NoMatch => api_error(
            StatusCode::CONFLICT,
            "the selected pairing ceremony is no longer waiting — refresh and retry",
        ),
    }
}
