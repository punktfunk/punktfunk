//! Desk pads: a game controller plugged into this PC, given to one profile's seat. Mounted under
//! `/profiles`, admin lane only: the physical desk is the box's, so the operator alone moves a
//! pad. The seat supervisor reads the hardware and decides what may be given; nothing a caller
//! says about a pad counts. The name the operator gives one lives here, not in the supervisor.

use super::profiles::{refused, seat_error, seat_of, seats_now, unexpected};
use super::shared::*;
use pf_seats::ipc::{ApiError as SeatError, Command, CommandResult, ErrorCode, PadState};
use std::collections::BTreeMap;

/// Who has a controller now.
#[derive(Serialize, ToSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeskPadState {
    /// Not given to anyone: it plays at this PC's own desktop.
    Desk,
    /// Given to a profile whose seat runs: only that seat's games see it.
    Live,
    /// Given to a profile whose seat doesn't run: nobody sees it until the seat starts.
    Parked,
}

/// A game controller plugged into this PC, or one a profile holds.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct DeskPad {
    /// Names the controller in `PUT /profiles/pads/{key}`. Stable across a replug and a restart.
    #[schema(example = "usb-28de-1304-s-FXA99614007BD")]
    key: String,
    /// What the hardware calls itself.
    #[schema(example = "Steam Controller Puck")]
    name: String,
    /// The name the operator gave it, shown in place of `name`.
    #[schema(example = "Couch puck")]
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// The USB vendor id.
    #[schema(example = 10462)]
    vendor_id: u16,
    /// The USB product id.
    #[schema(example = 4868)]
    product_id: u16,
    /// Plugged in now. A controller a profile holds stays listed while it is unplugged.
    present: bool,
    state: DeskPadState,
    /// The profile it is given to.
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<String>,
}

/// The controllers plugged into this PC.
#[derive(Serialize, ToSchema, Debug)]
pub(crate) struct DeskPads {
    /// Whether this host can give a controller to a profile. `false` lists nothing.
    supported: bool,
    pads: Vec<DeskPad>,
}

/// Who a controller goes to.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DeskPadHolder {
    /// The profile to give it to. `null` gives it back to this PC.
    profile: Option<String>,
}

/// A controller's name.
#[derive(Deserialize, ToSchema)]
pub(crate) struct DeskPadLabel {
    /// The name to show. `null` or blank shows what the hardware calls itself again.
    #[schema(example = "Couch puck")]
    label: Option<String>,
}

/// A controller's key → the name the operator gave it.
const LABELS: &str = "desk-pad-labels.json";

/// Absent or malformed: no names. A name is cosmetic, so a bad file costs only the names.
fn read_labels(path: &std::path::Path) -> BTreeMap<String, String> {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

/// Names `key`, or forgets its name for `None`. Load-edit-save runs under one lock.
fn write_label(path: &std::path::Path, key: &str, label: Option<String>) -> std::io::Result<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut labels = read_labels(path);
    match label {
        Some(label) => labels.insert(key.to_owned(), label),
        None => labels.remove(key),
    };
    pf_paths::replace_secret_file(path, &serde_json::to_vec_pretty(&labels)?)
}

/// The code a backend with no lending answers.
const UNSUPPORTED: &str = "pads_unsupported";

/// One request about pads to the seat supervisor: the list it answers, or `None` from a
/// supervisor that can't lend at all.
async fn ask(command: Command) -> Result<Option<Vec<pf_seats::ipc::DeskPad>>, Response> {
    match blocking("seat_supervisor", move || crate::seats::call(command)).await? {
        Ok(CommandResult::Pads { pads }) => Ok(Some(pads)),
        Ok(_) => Err(unexpected()),
        Err(e) if e.said.as_deref() == Some(UNSUPPORTED) => Ok(None),
        Err(e) => Err(supervisor_error(e)),
    }
}

/// A supervisor's refusal as its sentence; one that didn't answer is a 503.
fn supervisor_error(e: SeatError) -> Response {
    match e.code {
        ErrorCode::Backend
        | ErrorCode::Capacity
        | ErrorCode::Conflict
        | ErrorCode::NotFound
        | ErrorCode::InvalidRequest => refused(seat_error(e)),
        _ => {
            tracing::warn!(code = ?e.code, "seat supervisor request failed");
            api_error_coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "seat_supervisor_unreachable",
                "Couldn't reach the seat supervisor. Try again in a moment.",
            )
        }
    }
}

fn bad_key() -> Response {
    api_error_coded(
        StatusCode::BAD_REQUEST,
        "pad_key_invalid",
        "That isn't one of this PC's controllers. Reload the page, then try again.",
    )
}

fn unsupported() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        UNSUPPORTED,
        "Giving a controller to a profile isn't available on this system.",
    )
}

/// The supervisor's list as the console reads it: each seat named by the profile that plays
/// on it, each controller with the name the operator gave it.
async fn rows(st: &MgmtState, pads: Vec<pf_seats::ipc::DeskPad>) -> DeskPads {
    let labels = read_labels(&pf_paths::config_dir().join(LABELS));
    let seats = seats_now(st).await;
    let profiles = st.app.profiles.get().map(|p| p.list()).unwrap_or_default();
    let profile_of = |seat: &pf_seats::SeatId| {
        let (row, _) = seats.seat(seat.as_str())?;
        crate::seats::profile_on(row, &profiles)
    };
    let pads = pads
        .into_iter()
        .map(|pad| DeskPad {
            profile: pad.seat.as_ref().and_then(profile_of),
            label: labels.get(&pad.key).cloned(),
            key: pad.key,
            name: pad.name,
            vendor_id: pad.vendor_id,
            product_id: pad.product_id,
            present: pad.present,
            state: match pad.state {
                PadState::Desk => DeskPadState::Desk,
                PadState::Live => DeskPadState::Live,
                PadState::Parked => DeskPadState::Parked,
            },
        })
        .collect();
    DeskPads {
        supported: true,
        pads,
    }
}

/// List the controllers plugged into this PC
///
/// Every game controller plugged into the PC that can be given to a profile, and every one a
/// profile holds, plugged in or not. A keyboard, a mouse or any other device is never listed.
/// A host that can't give controllers away answers `supported: false`. Admin lane only.
#[utoipa::path(
    get,
    path = "/profiles/pads",
    tag = "profiles",
    operation_id = "listDeskPads",
    responses(
        (status = OK, description = "The controllers", body = DeskPads),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn list_desk_pads(State(st): State<Arc<MgmtState>>) -> Response {
    let none = DeskPads {
        supported: false,
        pads: Vec::new(),
    };
    if !crate::seats::has_supervisor() {
        return Json(none).into_response();
    }
    match ask(Command::Pads).await {
        Ok(Some(pads)) => Json(rows(&st, pads).await).into_response(),
        Ok(None) => Json(none).into_response(),
        Err(resp) => resp,
    }
}

/// Give a controller to a profile, or back to this PC
///
/// With a profile: only that profile's seat sees the controller, from its next start or at once
/// when it runs, and this PC's own desktop and every other seat stop seeing it. With `null`: it
/// plays at this PC's desktop again. Either way the controller reconnects once. A controller a
/// profile holds stays that profile's while it is unplugged; giving one away needs it plugged
/// in. Answers the list as it is after. Admin lane only.
#[utoipa::path(
    put,
    path = "/profiles/pads/{key}",
    tag = "profiles",
    operation_id = "setDeskPadHolder",
    params(("key" = String, Path, description = "The controller's key, from the list")),
    request_body = DeskPadHolder,
    responses(
        (status = OK, description = "The controllers, after the change", body = DeskPads),
        (status = BAD_REQUEST, description = "Not a controller's key", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No profile with that id", body = ApiError),
        (status = CONFLICT, description = "The profile has no seat, holds four controllers already, the controller isn't plugged in or isn't a game controller, or this host can't give controllers away", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer, or couldn't hand the controller over", body = ApiError),
    )
)]
pub(crate) async fn set_desk_pad_holder(
    State(st): State<Arc<MgmtState>>,
    Path(key): Path<String>,
    Json(input): Json<DeskPadHolder>,
) -> Response {
    if !pf_seats::model::valid_pad_key(&key) {
        return bad_key();
    }
    if !crate::seats::has_supervisor() {
        return unsupported();
    }
    let command = match input.profile {
        None => Command::TakeBack { key },
        Some(profile) => {
            let row = match seat_of(&st, &profile, &*seats_now(&st).await) {
                Ok((_, row)) => row,
                Err(refusal) => return refused(refusal),
            };
            match pf_seats::SeatId::parse(row) {
                Ok(id) => Command::Lend { id, key },
                Err(_) => return unexpected(),
            }
        }
    };
    match ask(command).await {
        Ok(Some(pads)) => Json(rows(&st, pads).await).into_response(),
        Ok(None) => unsupported(),
        Err(resp) => resp,
    }
}

/// Name a controller plugged into this PC
///
/// The console shows the name in place of what the hardware calls itself. `null` or a blank
/// name goes back to the hardware's. Who uses the controller doesn't change. The controller must
/// be in the list. Answers the list as it is after. Admin lane only.
#[utoipa::path(
    put,
    path = "/profiles/pads/{key}/label",
    tag = "profiles",
    operation_id = "setDeskPadLabel",
    params(("key" = String, Path, description = "The controller's key, from the list")),
    request_body = DeskPadLabel,
    responses(
        (status = OK, description = "The controllers, after the change", body = DeskPads),
        (status = BAD_REQUEST, description = "Not the key of a listed controller", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "This host can't give controllers away", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "The name wasn't saved", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "The seat supervisor didn't answer", body = ApiError),
    )
)]
pub(crate) async fn set_desk_pad_label(
    State(st): State<Arc<MgmtState>>,
    Path(key): Path<String>,
    Json(input): Json<DeskPadLabel>,
) -> Response {
    if !pf_seats::model::valid_pad_key(&key) {
        return bad_key();
    }
    if !crate::seats::has_supervisor() {
        return unsupported();
    }
    // Only a listed controller takes a name, so the file holds no more than the PC has seen.
    let pads = match ask(Command::Pads).await {
        Ok(Some(pads)) => pads,
        Ok(None) => return unsupported(),
        Err(resp) => return resp,
    };
    if !pads.iter().any(|pad| pad.key == key) {
        return bad_key();
    }
    let label = input
        .label
        .as_deref()
        .and_then(crate::native_pairing::clean_name);
    if let Err(e) = write_label(&pf_paths::config_dir().join(LABELS), &key, label) {
        return api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "pad_label_not_saved",
            "Couldn't save the controller's name",
            &e,
        );
    }
    Json(rows(&st, pads).await).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_label_is_set_replaced_and_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LABELS);
        assert!(read_labels(&path).is_empty());
        write_label(&path, "usb-a", Some("Couch".into())).unwrap();
        write_label(&path, "usb-b", Some("Desk".into())).unwrap();
        write_label(&path, "usb-a", Some("Leon".into())).unwrap();
        write_label(&path, "usb-b", None).unwrap();
        let labels = read_labels(&path);
        assert_eq!(labels.len(), 1);
        assert_eq!(labels["usb-a"], "Leon");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(read_labels(&path).is_empty());
    }
}
