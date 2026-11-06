//! The door's rule for who answers a request. The door opens no device, encodes nothing and
//! runs no plugin: it pairs, places and keeps the box's files. Everything else is a host that
//! streams. So the door answers only what [`DOOR`] names and passes every other admin call to
//! the owner's host; a device's call about its own play goes to the seat it plays on, as that
//! device. `mgmt::tests::routes` holds the table against the API document.

use super::auth::{AuthLane, PairedDevice};
use super::shared::*;
use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, Method};
use axum::middleware::Next;

/// Which host answers a call on the door.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The door: the box's own files, pairing, placement, power, and what it merges itself.
    Door,
    /// The owner's host, which streams and runs the plugins.
    Forward,
}

/// The door's own routes, as `(method, template)`; `*` is any method, a trailing `/*` any
/// depth. A route this never names is the owner's host's.
pub(crate) const DOOR: &[(&str, &str)] = &[
    // Open routes: a credential is proven here, or none is needed.
    ("*", "health"),
    ("*", "webtransport"),
    ("*", "auth/device/*"),
    ("*", "local/summary"),
    // The door merges these with the owner's host's.
    ("GET", "host"),
    ("GET", "host/settings"),
    ("PATCH", "host/settings"),
    ("GET", "status"),
    ("GET", "diagnostics"),
    ("POST", "diagnostics/refresh"),
    // Pairing, placement and the box's files.
    ("*", "native/*"),
    ("*", "profiles"),
    ("*", "profiles/*"),
    // Power is the box's; the door restarts the owner's host with its own.
    ("*", "actions"),
    ("*", "actions/*"),
    ("*", "update/*"),
    ("*", "client-logs"),
    ("*", "client-logs/*"),
    ("GET", "logs"),
    // The door's bus, which carries the seats' events too.
    ("GET", "events"),
    ("POST", "events/relay"),
    // The box's library copy, which every client reads; its writes are the owner's.
    ("GET", "library"),
    ("GET", "library/page"),
    ("GET", "library/art/*"),
];

/// Routes a device may call about its own play, which go to the seat it plays on.
const DEVICE: &[(&str, &str)] = &[
    ("POST", "game/end"),
    ("POST", "library/install/*"),
    ("DELETE", "library/install/*"),
];

/// Answers that never end: a tail the console keeps open.
const STREAMS: &[&str] = &["events", "session/*/pads"];

/// `template` against `rest`, both without a leading slash. `*` alone at the end matches any
/// depth; elsewhere exactly one segment.
pub(crate) fn template_matches(template: &str, rest: &str) -> bool {
    let mut want = template.split('/').peekable();
    let mut got = rest.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some("*"), Some(_)) if want.peek().is_none() => return true,
            (Some("*"), Some(_)) => {}
            (Some(w), Some(g)) if w == g => {}
            _ => return false,
        }
    }
}

fn listed(table: &[(&str, &str)], method: &Method, rest: &str) -> bool {
    table
        .iter()
        .any(|(m, t)| (*m == "*" || *m == method.as_str()) && template_matches(t, rest))
}

/// A path segment that is a dot, however spelled: the door never passes one on.
fn has_dot_segment(rest: &str) -> bool {
    rest.split('/').any(|s| {
        let s = s.to_ascii_lowercase().replace("%2e", ".");
        s == "." || s == ".."
    })
}

/// Who answers `method` `rest` (the path under `/api/v1/`) on the door's admin lane.
pub(crate) fn classify(method: &Method, rest: &str) -> Verdict {
    if has_dot_segment(rest) || listed(DOOR, method, rest) {
        Verdict::Door
    } else {
        Verdict::Forward
    }
}

/// Whether a device's call to `method` `rest` is about its own play.
fn device_route(method: &Method, rest: &str) -> bool {
    !has_dot_segment(rest) && listed(DEVICE, method, rest)
}

/// Whether `rest` is a tail, to hand over as it arrives.
pub(crate) fn tail(rest: &str) -> bool {
    STREAMS.iter().any(|t| template_matches(t, rest))
}

/// Which seat host a call goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Owner,
    Seat(String),
}

/// One call to a seat host, as the door passes it on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Forwarded {
    pub method: String,
    /// Under `/api/v1/`, query included.
    pub target: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
    /// The device the door vouches for: the seat host answers as it would that device.
    pub device: Option<String>,
    /// Hand the body over as it arrives.
    pub stream: bool,
    /// Start a stopped seat; a check that must not wake anyone passes `false`.
    pub start: bool,
}

impl Forwarded {
    /// A plain buffered `GET` of `target`, which wakes no seat.
    pub(crate) fn get(target: String) -> Forwarded {
        Forwarded {
            method: "GET".into(),
            target,
            content_type: None,
            body: Vec::new(),
            device: None,
            stream: false,
            start: false,
        }
    }
}

/// A seat host's answer, or why there is none.
pub(crate) enum Answer {
    /// No such row. Off a door every call is the host's own; on a door a device's own play
    /// with no seat stays on the door, and an owner's call is refused ([`pass_on`]).
    NoSeat,
    /// The seat's session is stopped. It was asked to start.
    Starting,
    /// The seat host didn't answer.
    Down,
    Reply(u16, Option<String>, Vec<u8>),
    Stream(u16, Option<String>, Body),
}

/// The seats as the door reaches them. Live on a door; a test hands in what it wants answered.
pub(crate) trait Seats: Send + Sync {
    /// Blocking.
    fn snapshot(&self) -> Arc<crate::seats::Snapshot>;
    /// Blocking.
    fn call(&self, target: &Target, call: Forwarded) -> Answer;
    /// The ledger, with no seat host asked. Blocking.
    fn ledger(&self) -> crate::seats::Snapshot;
    /// One request to the seat supervisor. Blocking.
    fn ask(
        &self,
        command: pf_seats::ipc::Command,
    ) -> Result<pf_seats::ipc::CommandResult, pf_seats::ipc::ApiError>;
}

/// The live seats, through the supervisor and each seat host's loopback API.
pub(crate) struct LiveSeats;

impl Seats for LiveSeats {
    fn snapshot(&self) -> Arc<crate::seats::Snapshot> {
        crate::seats::snapshot()
    }

    fn ledger(&self) -> crate::seats::Snapshot {
        crate::seats::ledger()
    }

    fn ask(
        &self,
        command: pf_seats::ipc::Command,
    ) -> Result<pf_seats::ipc::CommandResult, pf_seats::ipc::ApiError> {
        crate::seats::call(command)
    }

    fn call(&self, target: &Target, call: Forwarded) -> Answer {
        let seats = crate::seats::snapshot();
        let row = match row(&seats, target) {
            Ok(row) => row,
            Err(answer) => return answer,
        };
        if super::profiles::needs_start(&row.runtime.state) {
            if !call.start {
                return Answer::Down;
            }
            super::profiles::start_seat(row.id.as_str());
            return Answer::Starting;
        }
        crate::seats::forward(row, call).unwrap_or(Answer::Down)
    }
}

/// The row `target` names in `seats`, or the answer a call gets without one: [`Answer::Down`]
/// while the supervisor is silent, [`Answer::NoSeat`] when it lists no such row.
fn row<'a>(
    seats: &'a crate::seats::Snapshot,
    target: &Target,
) -> Result<&'a pf_seats::Seat, Answer> {
    let owner = seats.owner().map_err(|_| Answer::Down)?;
    let row = match target {
        Target::Owner => owner,
        Target::Seat(id) => seats.seat(id).map(|(s, _)| s),
    };
    row.ok_or(Answer::NoSeat)
}

impl MgmtState {
    /// `call` on `target`, off the async workers. [`Answer::NoSeat`] off a door.
    pub(crate) async fn seat_call(&self, target: Target, call: Forwarded) -> Answer {
        let Some(seats) = self.door.clone() else {
            return Answer::NoSeat;
        };
        tokio::task::spawn_blocking(move || seats.call(&target, call))
            .await
            .unwrap_or(Answer::Down)
    }

    /// The owner's host's answer to a buffered `method` `target`. `start` as [`Forwarded::start`].
    pub(crate) async fn owner_call(
        &self,
        method: &str,
        target: &str,
        body: Option<Vec<u8>>,
        start: bool,
    ) -> Answer {
        let json = body.is_some().then(|| "application/json".to_string());
        self.seat_call(
            Target::Owner,
            Forwarded {
                method: method.into(),
                target: target.into(),
                content_type: json,
                body: body.unwrap_or_default(),
                device: None,
                stream: false,
                start,
            },
        )
        .await
    }

    /// The seats as the door sees them, off the async workers. Empty off a door.
    pub(crate) async fn seats_snapshot(&self) -> Arc<crate::seats::Snapshot> {
        let Some(seats) = self.door.clone() else {
            return Arc::default();
        };
        tokio::task::spawn_blocking(move || seats.snapshot())
            .await
            .unwrap_or_default()
    }
}

impl Answer {
    /// The answer as the caller reads it. A seat host's content type passes as it came: the
    /// console serves a plugin's page on the plugin origin, and nothing else is a page.
    pub(crate) fn into_response(self) -> Response {
        let with_type = |mut resp: Response, content_type: Option<String>| {
            if let Some(value) = content_type.and_then(|c| header::HeaderValue::from_str(&c).ok()) {
                resp.headers_mut().insert(header::CONTENT_TYPE, value);
            }
            resp
        };
        let status = |code: u16| StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY);
        match self {
            Answer::Reply(code, content_type, bytes) => {
                with_type((status(code), bytes).into_response(), content_type)
            }
            Answer::Stream(code, content_type, body) => {
                with_type((status(code), body).into_response(), content_type)
            }
            Answer::Starting => api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "That session is starting. Try again in a moment.",
            ),
            Answer::Down | Answer::NoSeat => api_error(
                StatusCode::BAD_GATEWAY,
                "That host didn't answer. Try again in a moment.",
            ),
        }
    }

    /// The body of a `200`, else `None`.
    pub(crate) fn ok_body(self) -> Option<Vec<u8>> {
        match self {
            Answer::Reply(200, _, body) => Some(body),
            _ => None,
        }
    }
}

/// axum's default request limit: a policy, a hook list, a PIN, a plugin page's form.
const BODY_LIMIT: usize = 2 * 1024 * 1024;

/// The seat a device plays on: where its host lists it now, else its own profile's row. A
/// person's own Linux account is only ever the first: a device reaches it by unlocking.
fn device_seat(st: &MgmtState, seats: &crate::seats::Snapshot, fp: &str) -> Option<String> {
    let seen = seats.occupants.iter().find(|(_, who)| {
        who.iter()
            .any(|o| crate::session_status::is_client(&o.client, fp))
    });
    if let Some((id, _)) = seen {
        return Some(id.clone());
    }
    let profile = st.app.profiles.get()?.resolve(Some(fp), None).ok()?;
    if seats.person_row(&profile.os_account).is_some() {
        return None;
    }
    seats.row_of(&profile.os_account).map(str::to_string)
}

/// On a door: an admin call the door does not answer itself goes to the owner's host, and a
/// device's call about its own play to the seat it plays on. Every other lane, and every
/// other host, is answered here.
pub(crate) async fn pass_on(
    State(st): State<Arc<MgmtState>>,
    req: Request,
    next: Next,
) -> Response {
    let Some(seats) = st.door.clone() else {
        return next.run(req).await;
    };
    let lane = req.extensions().get::<AuthLane>().copied();
    let rest = req
        .uri()
        .path()
        .strip_prefix("/api/v1/")
        .unwrap_or_default()
        .to_string();
    let target = match lane {
        Some(AuthLane::Admin) if classify(req.method(), &rest) == Verdict::Forward => Target::Owner,
        Some(AuthLane::Cert) if device_route(req.method(), &rest) => {
            let Some(PairedDevice(fp)) = req.extensions().get::<PairedDevice>().cloned() else {
                return next.run(req).await;
            };
            let snapshot = {
                let seats = seats.clone();
                tokio::task::spawn_blocking(move || seats.snapshot())
                    .await
                    .unwrap_or_default()
            };
            match device_seat(&st, &snapshot, &fp) {
                Some(id) => Target::Seat(id),
                None => return next.run(req).await,
            }
        }
        _ => return next.run(req).await,
    };
    let (parts, body) = req.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, BODY_LIMIT).await else {
        return api_error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
    };
    let call = Forwarded {
        method: parts.method.as_str().to_string(),
        target: parts
            .uri
            .path_and_query()
            .and_then(|p| p.as_str().strip_prefix("/api/v1/"))
            .unwrap_or_default()
            .to_string(),
        content_type: parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: bytes.to_vec(),
        device: parts.extensions.get::<PairedDevice>().map(|d| d.0.clone()),
        stream: tail(&rest),
        start: true,
    };
    let to_owner = target == Target::Owner;
    match tokio::task::spawn_blocking(move || seats.call(&target, call)).await {
        // A running door always has an owner row; its own files are no stand-in for the owner's.
        Ok(Answer::NoSeat) if to_owner => api_error(StatusCode::SERVICE_UNAVAILABLE, NO_OWNER),
        Ok(Answer::NoSeat) => next.run(Request::from_parts(parts, bytes.into())).await,
        Ok(answer) => answer.into_response(),
        Err(_) => Answer::Down.into_response(),
    }
}

/// A door with no owner row: the switch that made the door made none, or one was removed.
pub(crate) const NO_OWNER: &str =
    "This box has no owner's host to answer that. Turn Reachable without logging in off and on again.";

#[cfg(test)]
mod tests {
    use super::*;

    /// The door keeps pairing, placement, power and the box's files, and passes the rest on.
    #[test]
    fn the_door_answers_its_own_routes_and_passes_the_rest_on() {
        let get = |rest| classify(&Method::GET, rest);
        let put = |rest| classify(&Method::PUT, rest);
        for own in [
            "native/clients",
            "profiles/a/proxy/library",
            "actions/host.restart",
            "status",
            "diagnostics",
            "library",
            "library/art/steam:570/cover",
        ] {
            assert_eq!(get(own), Verdict::Door, "{own}");
        }
        for owner in [
            "gpus",
            "display/settings",
            "display/clients/abc",
            "hooks",
            "plugins",
            "plugins/steam/ui-credential",
            "plugins/steam/ui/index.html",
            "session/last",
            "library/scanners",
            "store/catalog",
        ] {
            assert_eq!(get(owner), Verdict::Forward, "{owner}");
        }
        assert_eq!(put("library/hidden/x"), Verdict::Forward);
        assert_eq!(
            put("host/settings"),
            Verdict::Forward,
            "PUT is no door method"
        );
        assert_eq!(classify(&Method::PATCH, "host/settings"), Verdict::Door);
        // A dot segment never leaves the door, however it is spelled.
        assert_eq!(get("plugins/steam/ui/../../native/clients"), Verdict::Door);
        assert_eq!(get("plugins/steam/ui/%2E%2e/x"), Verdict::Door);
    }

    /// A device's own play goes to its seat; its reads of the box stay on the door.
    #[test]
    fn a_device_s_own_play_is_its_seat_s() {
        assert!(device_route(&Method::POST, "game/end"));
        assert!(device_route(&Method::POST, "library/install/steam:570"));
        assert!(device_route(&Method::DELETE, "library/install/steam:570"));
        assert!(device_route(
            &Method::POST,
            "library/install/steam:570/pause"
        ));
        assert!(!device_route(&Method::GET, "status"));
        assert!(!device_route(&Method::GET, "library"));
        assert!(!device_route(&Method::POST, "actions/power.sleep"));
    }

    /// A supervisor that didn't answer is a host that didn't, never a box without the row.
    #[test]
    fn a_silent_supervisor_is_down_and_a_missing_row_is_no_seat() {
        let seat = |owner: bool| -> pf_seats::Seat {
            serde_json::from_value(serde_json::json!({
                "id": "0123456789abcdef0123456789abcdef", "name": "Enrico", "account": "enrico",
                "display_slot": 12, "native_port": 9778, "mgmt_port": 47991, "owner": owner,
            }))
            .unwrap()
        };
        let snapshot = |on: bool, seats| crate::seats::Snapshot {
            on,
            seats,
            ..Default::default()
        };
        let seat_id = Target::Seat("0123456789abcdef0123456789abcdef".into());
        let silent = snapshot(false, Vec::new());
        assert!(matches!(row(&silent, &Target::Owner), Err(Answer::Down)));
        assert!(matches!(row(&silent, &seat_id), Err(Answer::Down)));
        let no_owner = snapshot(true, vec![seat(false)]);
        assert!(matches!(
            row(&no_owner, &Target::Owner),
            Err(Answer::NoSeat)
        ));
        assert!(row(&no_owner, &seat_id).is_ok());
        let owned = snapshot(true, vec![seat(true)]);
        assert!(matches!(row(&owned, &Target::Owner), Ok(s) if s.account == "enrico"));
    }

    #[test]
    fn tails_are_streamed() {
        assert!(tail("events"));
        assert!(tail("session/7/pads"));
        assert!(!tail("session/7"));
        assert!(!tail("stats/capture/live"));
    }
}
