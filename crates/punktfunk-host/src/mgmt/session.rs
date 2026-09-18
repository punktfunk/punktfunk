//! Session-tagged management HTTP handlers: stop, IDR, mute, live access,
//! end-game, session⇄game lifetime.
//!
//! `DELETE /session` is a deliberate stop: skip keep-alive linger and apply
//! `game_on_session_end`. Policy: `design/session-game-lifetime.md`.
//!
//! Each verb comes in two forms. The id-less one is host-wide and unchanged; the
//! `/{id}` one takes an id from `GET /status` and touches that session only. An id
//! nothing is streaming is a 404, never another session's teardown.
//!
//! `GET /{id}/pads` is the read-only live view of what the host injects
//! ([`crate::pad_feed`]).
//! `GET /session/last` is the other side of the same registry: what a session came
//! to, once nothing is streaming any more.

use super::shared::*;
use crate::pad_feed::PadFrame;
use crate::vdisplay::Toplevel;
use axum::response::sse::{Event, KeepAlive, Sse};
use std::sync::atomic::Ordering;

/// Stop the session
///
/// A deliberate stop: skip keep-alive linger and apply `game_on_session_end`.
#[utoipa::path(
    delete,
    path = "/session",
    tag = "session",
    operation_id = "stopSession",
    responses(
        (status = NO_CONTENT, description = "Session stopped (or none was active)"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn stop_session(State(st): State<Arc<MgmtState>>) -> StatusCode {
    let was_streaming = st.app.quit_session("management API stop");
    // Native sessions run off the GameStream registry; `quit_session` does not reach them.
    let native = crate::session_status::count();
    crate::session_status::stop_all_quit();
    tracing::info!(
        was_streaming,
        native_sessions = native,
        "management API: session stopped"
    );
    StatusCode::NO_CONTENT
}

/// Stop one session
///
/// Same deliberate stop as `DELETE /session`, for the one id. Every other live
/// session keeps streaming.
#[utoipa::path(
    delete,
    path = "/session/{id}",
    tag = "session",
    operation_id = "stopOneSession",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    responses(
        (status = NO_CONTENT, description = "Session stopped"),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn stop_one_session(Path(id): Path<u64>) -> Response {
    if !crate::session_status::stop_quit(id) {
        return no_such_session();
    }
    tracing::info!(session = id, "management API: one session stopped");
    StatusCode::NO_CONTENT.into_response()
}

/// Request a keyframe on one session
#[utoipa::path(
    post,
    path = "/session/{id}/idr",
    tag = "session",
    operation_id = "requestSessionIdr",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    responses(
        (status = ACCEPTED, description = "Keyframe requested"),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn request_session_idr(Path(id): Path<u64>) -> Response {
    if !crate::session_status::force_idr(id) {
        return no_such_session();
    }
    StatusCode::ACCEPTED.into_response()
}

/// Mute or unmute one session
///
/// Drops this session's audio at the wire, nothing else: the capturer and the sink
/// stay up, so a session sharing the display keeps hearing. The client is told, so
/// its overlay can name the silence instead of the player guessing.
#[utoipa::path(
    put,
    path = "/session/{id}/audio",
    tag = "session",
    operation_id = "setSessionAudio",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    request_body = SessionAudioRequest,
    responses(
        (status = NO_CONTENT, description = "Applied"),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_session_audio(
    Path(id): Path<u64>,
    ApiJson(req): ApiJson<SessionAudioRequest>,
) -> Response {
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    controls.set_muted(req.muted);
    tracing::info!(
        session = id,
        muted = req.muted,
        "management API: session audio"
    );
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SessionAudioRequest {
    /// `true` stops audio leaving for this session.
    muted: bool,
}

/// Change one session's access level
///
/// Re-points the LIVE grant set — the mask the input thread already checks every
/// event against — so a guest gets the pad, or loses it, without reconnecting.
/// The pairing's stored access is untouched, and a later edit to it wins.
///
/// The ceiling is the pairing's own mask: what is asked for is ANDed with it, so this
/// route can narrow or restore, never grant a device something it was not paired for.
/// The applied mask comes back, which is how a caller sees the clamp.
#[utoipa::path(
    put,
    path = "/session/{id}/access",
    tag = "session",
    operation_id = "setSessionAccess",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    request_body = SessionAccessRequest,
    responses(
        (status = OK, description = "Applied mask, after the pairing clamp", body = SessionAccess),
        (status = BAD_REQUEST, description = "No level or grants, an unknown level, or reserved bits", body = ApiError),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_session_access(
    Path(id): Path<u64>,
    ApiJson(req): ApiJson<SessionAccessRequest>,
) -> Response {
    let requested = match (req.grants, req.level.as_deref()) {
        (Some(g), _) => {
            if let Some(bad) = super::native::reject_reserved(g) {
                return bad;
            }
            g
        }
        (None, Some(level)) => match super::native::grants_for_level(level) {
            Some(g) => g,
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "Access level must be full, controller or view.",
                )
            }
        },
        (None, None) => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "Send an access level or a grants mask.",
            )
        }
    };
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    let grants = controls.set_grants(requested);
    tracing::info!(
        session = id,
        requested,
        grants,
        "management API: session access re-pointed"
    );
    Json(SessionAccess {
        grants,
        level: super::native::access_level(Some(grants)).to_string(),
    })
    .into_response()
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SessionAccessRequest {
    /// `full` | `controller` | `view`. Ignored when `grants` is present.
    #[schema(example = "controller")]
    level: Option<String>,
    /// Exact `GRANT_*` mask, for a level the three names do not cover. Reserved bits are 400.
    #[schema(value_type = u32, required = false, example = 1)]
    grants: Option<u32>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionAccess {
    /// What now governs the session — the request ANDed with the pairing's mask.
    grants: u32,
    /// `full` | `controller` | `view` | `custom`, derived from `grants`.
    level: String,
}

/// Place one session's player slot
///
/// Which controller this session is: slot 0 is Player 1, and a local co-op game
/// reads that order. Without a pick the slot is whichever comes free, so the pad
/// that moves first takes Player 1 and the order changes on every reconnect.
///
/// The pick is a reservation, not a seizure. A pad already built keeps the slot it
/// was created under until it re-plugs; a slot another live session asked for first
/// stays theirs and this call answers `reserved: false`. It is remembered against
/// the device's pairing, so the same device reconnects as the same player — an
/// anonymous session's pick lasts only as long as the session.
#[utoipa::path(
    put,
    path = "/session/{id}/player",
    tag = "session",
    operation_id = "setSessionPlayer",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    request_body = SessionPlayerRequest,
    responses(
        (status = OK, description = "The pick, and whether it took the slot", body = SessionPlayer),
        (status = BAD_REQUEST, description = "Slot past the host's pad count", body = ApiError),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_session_player(
    State(st): State<Arc<MgmtState>>,
    Path(id): Path<u64>,
    ApiJson(req): ApiJson<SessionPlayerRequest>,
) -> Response {
    if req
        .slot
        .is_some_and(|s| s as usize >= punktfunk_core::input::MAX_PADS)
    {
        return api_error(
            StatusCode::BAD_REQUEST,
            "That player number is higher than this host has controllers for.",
        );
    }
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    let reserved = controls.set_player(req.slot);
    // Remember it against the pairing, so the next connect is the same player. A
    // session with no record (anonymous) keeps the pick for this session only.
    if let (Some(np), Some(fp)) = (st.native.as_ref(), controls.fingerprint.as_deref()) {
        if let Err(e) = np.set_pad_slot(fp, req.slot) {
            tracing::warn!(session = id, error = %format!("{e:#}"), "store player pick");
        }
    }
    tracing::info!(
        session = id,
        slot = ?req.slot,
        reserved,
        "management API: session player slot"
    );
    Json(SessionPlayer {
        slot: req.slot,
        reserved,
        pads: controls.pads(),
    })
    .into_response()
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct SessionPlayerRequest {
    /// Player slot, 0-based: `0` is Player 1. Omit or `null` to hand the session
    /// back to the first-free claim.
    #[schema(value_type = u32, required = false, example = 1)]
    #[serde(default)]
    slot: Option<u8>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionPlayer {
    /// The pick now stored for this session.
    #[schema(value_type = u32, required = false)]
    slot: Option<u8>,
    /// `false` = another live session asked for that slot first and keeps it; this
    /// session stays on the first-free claim.
    reserved: bool,
    /// OS pad slots the session holds right now. A pad already built keeps its slot
    /// until it re-plugs, so this can still name the old player for a moment.
    pads: Vec<u8>,
}

/// Recently finished sessions
///
/// What each session came to — mode, codec, bitrate, frames, bring-up, and why it ended
/// — newest first, at most the last eight. This is the whole of what the host knows, so
/// the answer is what a bug report should carry.
///
/// A host that has not streamed since it started answers with an empty list.
#[utoipa::path(
    get,
    path = "/session/last",
    tag = "session",
    operation_id = "getRecentSessions",
    responses(
        (status = OK, description = "Finished sessions, newest first; empty if none", body = RecentSessions),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_recent_sessions() -> Json<RecentSessions> {
    Json(RecentSessions {
        sessions: crate::session_status::recent(),
    })
}

#[derive(Serialize, ToSchema)]
pub(crate) struct RecentSessions {
    /// Newest first. Bounded in memory and lost on a host restart — this is the last
    /// few sessions, not a history.
    sessions: Vec<crate::events::SessionSummary>,
}

/// List this session's windows
///
/// The toplevels on the head this session streams, so a client in a full-screen
/// game can see what is behind it. Free to every session: those windows are
/// already in the pixels it receives. The operator's other monitors are not in
/// the payload, whatever the caller's access.
#[utoipa::path(
    get,
    path = "/session/{id}/windows",
    tag = "session",
    operation_id = "getSessionWindows",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    responses(
        (status = OK, description = "Windows on this session's head", body = Vec<Toplevel>),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_session_windows(Path(id): Path<u64>) -> Response {
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    Json(session_windows(&controls)).into_response()
}

/// Act on one of this session's windows
///
/// Focus, full-screen or close by id, gated on the session's LIVE grants — the
/// same mask the input thread checks every event against. A view-only guest is
/// refused all three; a controller-only guest raises a window but never closes
/// one. An id this session's head does not currently hold is a 404, so a stale
/// id cannot act on whatever now answers to it.
#[utoipa::path(
    post,
    path = "/session/{id}/windows/{window}",
    tag = "session",
    operation_id = "actOnSessionWindow",
    params(
        ("id" = u64, Path, description = "Session id from `GET /status`"),
        ("window" = String, Path, description = "Window id from `GET /session/{id}/windows`"),
    ),
    request_body = WindowActionRequest,
    responses(
        (status = NO_CONTENT, description = "The compositor accepted the verb"),
        (status = FORBIDDEN, description = "This session's access does not cover the verb", body = ApiError),
        (status = NOT_FOUND, description = "No such session, or no such window on its head", body = ApiError),
        (status = BAD_GATEWAY, description = "The compositor refused the verb", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn act_on_session_window(
    Path((id, window)): Path<(u64, String)>,
    ApiJson(req): ApiJson<WindowActionRequest>,
) -> Response {
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    // The live mask, read now: a console re-point or an expiry between the
    // client's fetch and this call must land before the verb does.
    let grants = controls.grants.load(Ordering::Relaxed);
    if !req.action.permitted_by(grants) {
        tracing::info!(
            session = id,
            verb = req.action.as_str(),
            grants,
            "management API: a window verb this session's access does not cover"
        );
        return api_error(
            StatusCode::FORBIDDEN,
            "This device's access doesn't cover that — ask the host's operator to widen it.",
        );
    }
    act_on_window(&controls, &window, req.action, id)
}

/// The verb, once the grant gate has passed. Split out so the platform arms do
/// not sit inside the handler.
#[cfg(target_os = "linux")]
fn act_on_window(
    controls: &crate::session_status::SessionControls,
    window: &str,
    verb: crate::vdisplay::WindowVerb,
    id: u64,
) -> Response {
    let Some(head) = controls.head() else {
        return no_such_window();
    };
    match crate::vdisplay::window_action(head.compositor, &head.output, verb, window) {
        Ok(()) => {
            tracing::info!(
                session = id,
                verb = verb.as_str(),
                window,
                "management API: acted on a window of this session's head"
            );
            StatusCode::NO_CONTENT.into_response()
        }
        // The id check and the dispatch share one error: either way this session
        // has no window to act on, and neither answer names the operator's desk.
        Err(e) => {
            tracing::info!(
                session = id, verb = verb.as_str(), window, error = %format!("{e:#}"),
                "management API: window verb not applied"
            );
            no_such_window()
        }
    }
}

/// A host with no compositor to ask has no window to act on.
#[cfg(not(target_os = "linux"))]
fn act_on_window(
    _controls: &crate::session_status::SessionControls,
    _window: &str,
    _verb: crate::vdisplay::WindowVerb,
    _id: u64,
) -> Response {
    no_such_window()
}

/// Windows on this session's head, or none before capture names it.
#[cfg(target_os = "linux")]
fn session_windows(
    controls: &crate::session_status::SessionControls,
) -> Vec<crate::vdisplay::Toplevel> {
    controls.head().map_or_else(Vec::new, |h| {
        crate::vdisplay::list_toplevels(h.compositor, &h.output)
    })
}

/// No compositor here reports toplevels.
#[cfg(not(target_os = "linux"))]
fn session_windows(
    _controls: &crate::session_status::SessionControls,
) -> Vec<crate::vdisplay::Toplevel> {
    Vec::new()
}

/// Watch this session's pads (SSE)
///
/// One frame per pad state the host applies — what it injects, not what the client
/// says it sent — so a controller question is answered from the host's own hand
/// instead of an evdev dump. `data:` is a [`PadFrame`]; `event:` is `pad.state`.
/// Attaching replays every live pad, so a button already held draws at once.
///
/// Console lane only, like the window routes: a paired certificate is not bound to
/// a session id, and this is the operator's own machine watching its own input.
///
/// Nothing is published while nobody is attached, so a console on another page —
/// or none at all — costs the input thread one atomic load per pad event.
#[utoipa::path(
    get,
    path = "/session/{id}/pads",
    tag = "session",
    operation_id = "streamSessionPads",
    params(("id" = u64, Path, description = "Session id from `GET /status`")),
    responses(
        (status = OK, description = "SSE stream; each frame's `data:` is one PadFrame", body = PadFrame, content_type = "text/event-stream"),
        (status = NOT_FOUND, description = "No live session with that id", body = ApiError),
        (status = SERVICE_UNAVAILABLE, description = "Concurrent event-stream cap reached — retry shortly", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn stream_session_pads(Path(id): Path<u64>) -> Response {
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    let Some(slot) = super::events::try_acquire_slot() else {
        return super::events::stream_cap_reached();
    };
    // Subscribing arms the resync; the input thread answers on its next wake (≤ 4 ms).
    let rx = controls.pads.subscribe();
    let stream = futures_util::stream::unfold((rx, slot), |(mut rx, slot)| async move {
        // Lagged: drop a consumer too slow for a stick sweep rather than buffer it.
        // Closed: the session ended, and its pads went with it.
        let frame = rx.recv().await.ok()?;
        let ev = Event::default()
            .event("pad.state")
            .data(serde_json::to_string(&frame).unwrap_or_else(|_| "{}".to_string()));
        Some((Ok::<_, std::convert::Infallible>(ev), (rx, slot)))
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(super::events::KEEP_ALIVE))
        .into_response()
}

/// One refusal for "gone" and "not yours": telling them apart would confirm a
/// window exists on a head this session may not see.
fn no_such_window() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "That window isn't on this session's screen any more.",
    )
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct WindowActionRequest {
    /// `focus` | `fullscreen` | `close`.
    #[schema(example = "focus")]
    action: crate::vdisplay::WindowVerb,
}

/// One id, one 404 — never a reach across to another session.
fn no_such_session() -> Response {
    api_error(
        StatusCode::NOT_FOUND,
        "No session with that id is streaming.",
    )
}

/// The session is live but its plane cannot carry this. GameStream has no message for a
/// mute, an access change or a player slot, so the honest answer is that it did not happen.
fn not_on_this_plane() -> Response {
    api_error(
        StatusCode::CONFLICT,
        "This session is a GameStream one, which can't do that. Stop it or ask its client.",
    )
}

/// End waiting games
///
/// Ends games waiting out the reconnect window. With `streaming` and an
/// `app_id`, also ends that title where it is still on a live session — the
/// move a player has after a launch that never produced a game. The session
/// itself stays up (`DELETE /session` plus `game_on_session_end`).
#[utoipa::path(
    post,
    path = "/game/end",
    tag = "session",
    operation_id = "endGame",
    request_body = EndGameRequest,
    responses(
        (status = OK, description = "How many waiting games were ended", body = EndGameResult),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No game is waiting to be ended", body = ApiError),
    )
)]
pub(crate) async fn end_game(ApiJson(req): ApiJson<EndGameRequest>) -> Response {
    let mut ended = crate::gamelease::end_pending(req.app_id.as_deref());
    // Named title only. The id-less form stays "every waiting game", which is
    // what the console's one button has always meant.
    if req.streaming && req.app_id.is_some() {
        for shared in crate::session_status::live_games(req.app_id.as_deref()) {
            if !shared.is_trackable() || shared.is_terminating() {
                continue;
            }
            crate::gamelease::terminate(shared, "ended from the management API");
            ended += 1;
        }
    }
    if ended == 0 {
        return api_error(StatusCode::CONFLICT, "no game is waiting to be ended");
    }
    tracing::info!(app_id = ?req.app_id, ended, "management API: game ended");
    Json(EndGameResult { ended }).into_response()
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct EndGameRequest {
    /// Store-qualified id (`steam:570`); omit to end every waiting game.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Also end `app_id` where it is on a live session, not only where it is
    /// waiting out a reconnect window. Ignored without `app_id`.
    #[serde(default)]
    #[schema(required = false)]
    pub streaming: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct EndGameResult {
    ended: usize,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionSettingsState {
    settings: crate::session_settings::SessionSettings,
    /// `false` means `settings` are the built-in defaults.
    configured: bool,
    /// Axes this build enforces. Empty with no launch path (macOS) so the console
    /// does not offer a no-op switch.
    enforced: Vec<String>,
}

fn session_settings_state() -> SessionSettingsState {
    let store = crate::session_settings::store();
    SessionSettingsState {
        settings: store.get(),
        configured: store.configured(),
        enforced: crate::session_settings::enforced(),
    }
}

#[utoipa::path(
    get,
    path = "/session/settings",
    tag = "session",
    operation_id = "getSessionSettings",
    responses(
        (status = OK, description = "Stored settings + which axes this build enforces", body = SessionSettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_session_settings() -> Json<SessionSettingsState> {
    Json(session_settings_state())
}

/// Set the session policy
///
/// Persisted clamped. Takes effect on the next decision, including a session
/// already streaming — policy is read at session end, not start.
#[utoipa::path(
    put,
    path = "/session/settings",
    tag = "session",
    operation_id = "setSessionSettings",
    request_body = crate::session_settings::SessionSettings,
    responses(
        (status = OK, description = "Settings stored; the new state", body = SessionSettingsState),
        (status = BAD_REQUEST, description = "Malformed settings body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the session settings", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn set_session_settings(
    ApiJson(settings): ApiJson<crate::session_settings::SessionSettings>,
) -> Response {
    if let Err(e) = crate::session_settings::store().set(settings) {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Couldn't save the session settings — {e:#}"),
        );
    }
    let state = session_settings_state();
    tracing::info!(
        game_on_session_end = state.settings.game_on_session_end.as_str(),
        session_on_game_exit = state.settings.session_on_game_exit,
        grace_s = state.settings.disconnect_grace_seconds,
        "management API: session⇄game lifetime settings updated"
    );
    Json(state).into_response()
}

#[utoipa::path(
    post,
    path = "/session/idr",
    tag = "session",
    operation_id = "requestIdr",
    responses(
        (status = ACCEPTED, description = "Keyframe requested"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = CONFLICT, description = "No active video stream", body = ApiError),
    )
)]
pub(crate) async fn request_idr(State(st): State<Arc<MgmtState>>) -> Response {
    let gs = st.app.streaming.load(Ordering::SeqCst);
    let native = crate::session_status::count();
    if !gs && native == 0 {
        return api_error(StatusCode::CONFLICT, "no active video stream");
    }
    if gs {
        st.app.force_idr.store(true, Ordering::SeqCst);
    }
    // Native sessions take IDR from the registry flag, not `app.force_idr`.
    crate::session_status::force_idr_all();
    StatusCode::ACCEPTED.into_response()
}
