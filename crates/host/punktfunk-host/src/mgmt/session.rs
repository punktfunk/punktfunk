//! Session-tagged management HTTP handlers: stop, IDR, mute, live access,
//! end-game, session⇄game lifetime.
//!
//! `DELETE /session` is a deliberate stop: skip keep-alive linger and apply
//! `game_on_session_end`. Policy: `design/session-game-lifetime.md`.
//!
//! Stop and IDR come in two forms: the id-less one is host-wide, the `/{id}` one touches
//! that session only. Every `/{id}` route takes an id from `GET /status`; an id nothing is
//! streaming is a 404, never another session's teardown.
//!
//! `GET /{id}/pads` is the read-only live view of what the host injects
//! ([`crate::pad_feed`]).
//! `GET /session/last` is the other side of the same registry: what a session came
//! to, once nothing is streaming any more.

use super::auth::{AuthLane, Loopback, PairedDevice};
use super::shared::*;
use crate::pad_feed::PadFrame;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Extension;
use punktfunk_core::quic::GRANT_LAUNCH;
use std::sync::atomic::Ordering;

/// `DELETE /session` options.
#[derive(Deserialize, utoipa::IntoParams)]
pub(crate) struct StopQuery {
    /// Why the host stops. `power` closes every session the way a power action does, so clients
    /// read that the host is going down; the box's door sends it to its seat hosts. A host that
    /// predates it, and any other value, stop plainly.
    reason: Option<String>,
}

/// Stop the session
///
/// A deliberate stop: skip keep-alive linger and apply `game_on_session_end`.
#[utoipa::path(
    delete,
    path = "/session",
    tag = "session",
    operation_id = "stopSession",
    params(StopQuery),
    responses(
        (status = NO_CONTENT, description = "Session stopped (or none was active)"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn stop_session(
    State(st): State<Arc<MgmtState>>,
    Query(q): Query<StopQuery>,
) -> StatusCode {
    // Set before the stop, as in a power action: the sessions read it first and close typed.
    if q.reason.as_deref() == Some("power") {
        crate::power::set_closing(true);
    }
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
///
/// With `share`, this session's pads join the slot's controller instead: buttons
/// combine, each trigger and stick takes the stronger input, and every pad on it
/// rumbles. With no one on the slot, the session takes it as a pick would. A share
/// lasts this session only and leaves the remembered pick alone.
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
        return api_error_coded(
            StatusCode::BAD_REQUEST,
            "player_number_too_high",
            "That player number is higher than this host has controllers for.",
        );
    }
    let Some(controls) = crate::session_status::controls(id) else {
        return no_such_session();
    };
    if crate::session_status::has_native_lanes(id) == Some(false) {
        return not_on_this_plane();
    }
    let share = req.share && req.slot.is_some();
    let reserved = if share {
        controls.set_shared(req.slot);
        true
    } else {
        controls.set_shared(None);
        let reserved = controls.set_player(req.slot);
        // Remember it against the pairing, so the next connect is the same player. A
        // session with no record (anonymous) keeps the pick for this session only.
        if let (Some(np), Some(fp)) = (st.native.as_ref(), controls.fingerprint.as_deref()) {
            if let Err(e) = np.set_pad_slot(fp, req.slot) {
                tracing::warn!(session = id, error = %format!("{e:#}"), "store player pick");
            }
        }
        reserved
    };
    tracing::info!(
        session = id,
        slot = ?req.slot,
        share,
        reserved,
        "management API: session player slot"
    );
    Json(SessionPlayer {
        slot: req.slot,
        share,
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
    /// Join `slot`'s controller with whoever holds it, for this session only. Ignored
    /// without a `slot`.
    #[serde(default)]
    share: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionPlayer {
    /// The pick now stored for this session.
    #[schema(value_type = u32, required = false)]
    slot: Option<u8>,
    /// The session's pads share `slot`'s controller.
    share: bool,
    /// `false` = another live session asked for that slot first and keeps it; this
    /// session stays on the first-free claim. Always `true` for a share.
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

/// Watch this session's pads (SSE)
///
/// One frame per pad state the host applies — what it injects, not what the client
/// says it sent — so a controller question is answered from the host's own hand
/// instead of an evdev dump. `data:` is a [`PadFrame`]; `event:` is `pad.state`.
/// Attaching replays every live pad, so a button already held draws at once.
///
/// Console lane only: a paired certificate is not bound to a session id, and this
/// is the operator's own machine watching its own input.
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
    // Subscribing arms the resync; the input thread answers on its next wake
    // (`InputLoop::poll_interval` in `native::input`).
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

/// One id, one 404 — never a reach across to another session.
fn no_such_session() -> Response {
    api_error_coded(
        StatusCode::NOT_FOUND,
        "session_not_streaming",
        "No session with that id is streaming.",
    )
}

/// The session is live but its plane cannot carry this. GameStream has no message for a
/// mute, an access change or a player slot, so the honest answer is that it did not happen.
fn not_on_this_plane() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "session_gamestream_cannot",
        "This session is a GameStream one, which can't do that. Stop it or ask its client.",
    )
}

/// Who may end which launched game. The operator, the plugin runner and a person on their own
/// seat host reach every launch; a paired device reaches only its own, and the desk's when it
/// holds the launch grant and calls from this machine, and nothing once its access expired.
pub(crate) struct GameEnder {
    /// `None` = every device's launches.
    owner: Option<String>,
    /// The desk's own launches are in reach.
    desk: bool,
    /// A paired device whose access expired, or a lane that ends nothing.
    refused: bool,
}

impl GameEnder {
    pub(crate) fn of(st: &MgmtState, lane: AuthLane, fp: Option<&str>, loopback: bool) -> Self {
        match lane {
            // A person ends what plays on their own seat host.
            AuthLane::Admin | AuthLane::Plugin | AuthLane::Person => Self {
                owner: None,
                desk: true,
                refused: false,
            },
            AuthLane::Cert => {
                let live = fp.is_some_and(|fp| {
                    st.native
                        .as_ref()
                        .is_some_and(|n| n.effective(fp, crate::clock::unix_secs()).is_some())
                });
                Self {
                    owner: fp.map(str::to_string),
                    desk: loopback && super::downloads::granted(st, fp, GRANT_LAUNCH),
                    refused: !live,
                }
            }
            AuthLane::PersonVouched | AuthLane::Public => Self {
                owner: None,
                desk: false,
                refused: true,
            },
        }
    }

    fn scope(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    /// A row the caller could end now: in reach, and a game the host can still end.
    pub(crate) fn may_end(&self, state: &str, launched_by: Option<&str>) -> bool {
        !self.refused
            && !matches!(state, "exited" | "untracked")
            && (crate::gamelease::launched_by(launched_by, self.scope())
                || (self.desk && launched_by == Some(crate::desk::KEY)))
    }
}

/// End a launched game
///
/// Ends games nobody is streaming: those waiting out the reconnect window and launches left
/// running after their session ended (`detached` in `GET /status`). With `streaming`, also ends
/// games still on a live session; the session itself stays up (`DELETE /session` plus
/// `game_on_session_end`). `app_id` narrows it to one title. A game this host started on its
/// own screen (`local`) ends only when `app_id` names it. A paired device ends only games it
/// launched, and a `local` game when it holds the launch grant and calls from this device.
#[utoipa::path(
    post,
    path = "/game/end",
    tag = "session",
    operation_id = "endGame",
    request_body = EndGameRequest,
    responses(
        (status = OK, description = "How many games were ended", body = EndGameResult),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = FORBIDDEN, description = "This device's access has expired", body = ApiError),
        (status = CONFLICT, description = "No game in reach to end", body = ApiError),
    )
)]
pub(crate) async fn end_game(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    device: Option<Extension<PairedDevice>>,
    loopback: Option<Extension<Loopback>>,
    ApiJson(req): ApiJson<EndGameRequest>,
) -> Response {
    let fp = device.as_ref().map(|d| d.0 .0.as_str());
    let ender = GameEnder::of(&st, lane, fp, loopback.is_some_and(|l| l.0 .0));
    if ender.refused {
        return api_error_coded(
            StatusCode::FORBIDDEN,
            "device_access_expired",
            "This device's access to the host has expired.",
        );
    }
    let app = req.app_id.as_deref();
    let mut ended = crate::gamelease::end_pending(ender.scope(), app);
    for d in crate::launchreg::take_detached(ender.scope(), app) {
        crate::gamelease::end_detached(d, "ended from the management API");
        ended += 1;
    }
    // A blanket call leaves the desk's games: someone at the box plays them.
    if let Some(app) = app.filter(|_| ender.desk) {
        ended += crate::desk::end(app);
    }
    if req.streaming {
        for shared in crate::session_status::live_games(app) {
            if !shared.is_trackable()
                || shared.is_terminating()
                || !crate::gamelease::launched_by(shared.fingerprint.as_deref(), ender.scope())
            {
                continue;
            }
            crate::gamelease::terminate(shared, "ended from the management API");
            ended += 1;
        }
    }
    if ended == 0 {
        return api_error_coded(
            StatusCode::CONFLICT,
            "no_game_to_end",
            "No game is waiting to be ended",
        );
    }
    tracing::info!(app_id = ?req.app_id, ended, device = ?ender.scope(), "management API: game ended");
    Json(EndGameResult { ended }).into_response()
}

/// Start a game on this host's own screen
///
/// For a client on this same device: the host starts the title on its own screen, with no
/// stream, and holds it until it is gone. What happens next rides `GET /status` as a `local`
/// row. The operator always may; a paired device may when it holds the launch grant and calls
/// from this device. The title is the owner profile's, whichever device asked.
#[utoipa::path(
    post,
    path = "/game/launch",
    tag = "session",
    operation_id = "launchGame",
    request_body = LaunchGameRequest,
    responses(
        (status = ACCEPTED, description = "Claimed: the title starts", body = LaunchGameResult),
        (status = OK, description = "This host already runs it; nothing new starts", body = LaunchGameResult),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
        (status = FORBIDDEN, description = "This device may not start games, or isn't this device", body = ApiError),
        (status = NOT_FOUND, description = "No such title", body = ApiError),
        (status = CONFLICT, description = "No screen of its own, nobody signed in, no desktop running, or no Steam entry", body = ApiError),
    )
)]
pub(crate) async fn launch_game(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    device: Option<Extension<PairedDevice>>,
    loopback: Option<Extension<Loopback>>,
    ApiJson(req): ApiJson<LaunchGameRequest>,
) -> Response {
    let fp = device.as_ref().map(|d| d.0 .0.as_str());
    match lane {
        AuthLane::Admin => {}
        AuthLane::Cert if super::downloads::granted(&st, fp, GRANT_LAUNCH) => {
            if !loopback.is_some_and(|l| l.0 .0) {
                return api_error_coded(
                    StatusCode::FORBIDDEN,
                    "local_not_this_device",
                    "This host starts games on its own screen only for a client on the same \
                     device.",
                );
            }
        }
        _ => return not_allowed_to_start_games(),
    }
    if !crate::desk::has_desk(pf_paths::seat::role()) {
        return no_desk();
    }
    let (owner, opens) = desk_owner(&st, fp);
    if !opens {
        return not_allowed_to_start_games();
    }
    let (view, profile) = desk_view(owner);
    let app = req.app_id.clone();
    let started =
        tokio::task::spawn_blocking(move || crate::desk::start(&view, &app, profile)).await;
    let refused = match started {
        Ok(Ok((via, running))) => {
            tracing::info!(app_id = %req.app_id, ?via, running, "management API: game started on the desk");
            let status = if running {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            return (status, Json(LaunchGameResult { via, running })).into_response();
        }
        Ok(Err(refused)) => refused,
        Err(e) => {
            tracing::error!(error = %e, "desk launch worker did not finish");
            return api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "desk launch worker did not finish",
            );
        }
    };
    refusal(refused)
}

/// The coded answer for a desk launch the host refused.
fn refusal(refused: crate::desk::Refused) -> Response {
    use crate::desk::Refused;
    match refused {
        Refused::NoSuchGame => api_error_coded(
            StatusCode::NOT_FOUND,
            "local_no_such_game",
            "This game is no longer in the library.",
        ),
        Refused::NobodySignedIn => api_error_coded(
            StatusCode::CONFLICT,
            "local_nobody_signed_in",
            "Nobody is signed in on this PC. Sign in, then start the game again.",
        ),
        Refused::NoSession => api_error_coded(
            StatusCode::CONFLICT,
            "local_no_session",
            "No desktop is running on this host. Sign in, then start the game again.",
        ),
        Refused::NotInSteam => api_error_coded(
            StatusCode::CONFLICT,
            "local_not_in_steam",
            "This game isn't in Steam yet. Add its source to Steam in the Punktfunk panel.",
        ),
    }
}

/// A seat, the owner's headless row or the door: no screen of its own.
fn no_desk() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "local_no_desk",
        "This host has no screen of its own to play on.",
    )
}

/// A device without the launch grant, or one a locked owner profile doesn't let in.
fn not_allowed_to_start_games() -> Response {
    api_error_coded(
        StatusCode::FORBIDDEN,
        "device_not_allowed_to_start_games",
        "This device isn't allowed to start games on this host.",
    )
}

/// What `owner` plays at the desk: its library view, and the profile its play time goes to.
fn desk_view(
    owner: Option<crate::profiles::Resolved>,
) -> (crate::library::View, Option<crate::events::ProfileRef>) {
    let view = owner
        .as_ref()
        .map_or_else(crate::library::View::here, crate::library::View::session);
    let profile = owner.map(|o| crate::events::ProfileRef {
        id: o.id,
        display_name: o.display_name,
    });
    (view, profile)
}

/// [`desk_view`] of the owner's profile, for the operator's lane.
pub(super) fn desk_library(
    st: &MgmtState,
) -> (crate::library::View, Option<crate::events::ProfileRef>) {
    desk_view(desk_owner(st, None).0)
}

/// The owner's profile, which plays the desk, and whether device `fp` opens it as a connect
/// with no PIN would be let in (`profiles::Profiles::opens`). The operator (`None`) always
/// does. No profile store, as in a test that skips `serve`, names no owner and locks nothing.
fn desk_owner(st: &MgmtState, fp: Option<&str>) -> (Option<crate::profiles::Resolved>, bool) {
    let Some(profiles) = st.app.profiles.get() else {
        return (None, true);
    };
    let id = profiles
        .owner_id()
        .unwrap_or_else(|| crate::profiles::OPERATOR_PROFILE_ID.to_string());
    let opens = fp.is_none() || profiles.opens(&id, fp) == crate::profiles::Opens::Yes;
    (profiles.pick(None, Some(&id)).ok(), opens)
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct LaunchGameRequest {
    /// Store-qualified library id (`steam:570`), as `GET /library` lists it.
    pub app_id: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct LaunchGameResult {
    /// `host`: this host starts it. `steam`: the Steam that owns this host's screen was asked to.
    via: crate::desk::Via,
    /// Present and true when this host already runs it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    running: bool,
}

/// Release a seat's held launch
///
/// A seat host runs no plugins, so its `game.launching` carries a `hold` token and the launch
/// waits, up to the longest plugin hold plus 10 s, for the host with the plugins to call this
/// once they are done. Admin lane only: the door, or the Windows box host, holds the token.
#[utoipa::path(
    post,
    path = "/game/holds/{token}/release",
    tag = "session",
    operation_id = "releaseGameHold",
    params(("token" = String, Path, description = "The `hold` of the seat's `game.launching`")),
    responses(
        (status = NO_CONTENT, description = "The launch goes ahead"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No launch waits on that token", body = ApiError),
    )
)]
pub(crate) async fn release_game_hold(Path(token): Path<String>) -> Response {
    if crate::holds::release(&token) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        api_error(StatusCode::NOT_FOUND, "no launch waits on that hold")
    }
}

/// Hand a launch to Steam's tree
///
/// `punktfunk-host play` calls this from inside the Steam entry a title was synced as, with
/// Steam's game id for that entry and the pad filter Steam set for it. The host finds the title
/// in its Steam map and claims it for its own screen, then runs the download wait, the launch
/// hold and emulator prepare with the pads that filter leaves, and answers the command to run
/// and a token to report on. A title the host already runs answers `running`: start nothing, and
/// wait until `GET /status` no longer lists it. Admin lane only.
#[utoipa::path(
    post,
    path = "/game/handoff",
    tag = "session",
    operation_id = "handOffGame",
    request_body = HandoffRequest,
    responses(
        (status = OK, description = "What to run, or that the title already runs", body = HandoffAnswer),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "The title the entry names is no longer in the library", body = ApiError),
        (status = CONFLICT, description = "No screen of its own, no synced title for that entry, or the launch ended before it started", body = ApiError),
    )
)]
pub(crate) async fn hand_off_game(
    State(st): State<Arc<MgmtState>>,
    ApiJson(req): ApiJson<HandoffRequest>,
) -> Response {
    use crate::desk::{Handoff, NoHandoff};
    if !crate::desk::has_desk(pf_paths::seat::role()) {
        return no_desk();
    }
    let (view, profile) = desk_library(&st);
    let handed = tokio::task::spawn_blocking(move || {
        let (ignore, except) = (&req.sdl_ignore, &req.sdl_ignore_except);
        crate::desk::handoff(&view, &req.steam_gameid, ignore, except, profile)
    })
    .await;
    let answer = |token, game: crate::gamelease::GameRef| HandoffAnswer {
        token,
        app_id: game.id.unwrap_or_default(),
        title: game.title,
        command: None,
        env: None,
        cwd: None,
        running: false,
    };
    match handed {
        Ok(Ok(Handoff::Start {
            token,
            game,
            command,
            env,
        })) => {
            tracing::info!(title = %game.title, "management API: launch handed to Steam's tree");
            let env = env.into_iter().map(|(k, v)| (k.into(), v.into()));
            Json(HandoffAnswer {
                command: Some(command),
                env: Some(env.collect()),
                ..answer(token, game)
            })
            .into_response()
        }
        Ok(Ok(Handoff::Running { token, game })) => Json(HandoffAnswer {
            running: true,
            ..answer(token, game)
        })
        .into_response(),
        Ok(Err(NoHandoff::Refused(refused))) => refusal(refused),
        Ok(Err(NoHandoff::Declined(why))) if !why.message.is_empty() => {
            api_error(StatusCode::CONFLICT, &why.message)
        }
        Ok(Err(NoHandoff::Declined(_) | NoHandoff::CalledOff)) => {
            api_error(StatusCode::CONFLICT, "the launch ended before it started")
        }
        Err(e) => {
            tracing::error!(error = %e, "handoff worker did not finish");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "handoff worker did not finish",
            )
        }
    }
}

/// Report on a handed-over launch
///
/// `play` reports `started` once, with the process id of the game; the host watches that
/// process only if it is its own user's and started after the handoff. Then `exited` once, with
/// the exit code. Sent without `started`, `exited` says nothing started. A token nobody reported
/// a start on within 10 minutes is dropped. Admin lane only.
#[utoipa::path(
    post,
    path = "/game/handoff/{token}",
    tag = "session",
    operation_id = "reportHandoff",
    params(("token" = String, Path, description = "The `token` of the handoff's answer")),
    request_body = HandoffEvent,
    responses(
        (status = NO_CONTENT, description = "Taken"),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "No handoff waits on that token: never issued, spent or dropped", body = ApiError),
        (status = CONFLICT, description = "Reported already, or the process is not one this launch started", body = ApiError),
    )
)]
pub(crate) async fn report_handoff(
    Path(token): Path<String>,
    ApiJson(event): ApiJson<HandoffEvent>,
) -> Response {
    use crate::desk::{Report, ReportRefused};
    let report = match event {
        HandoffEvent::Started { pid } => Report::Started(pid),
        HandoffEvent::Exited { code } => Report::Exited(code),
    };
    match crate::desk::report(&token, report) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(ReportRefused::Unknown) => {
            api_error(StatusCode::NOT_FOUND, "no launch waits on that token")
        }
        Err(ReportRefused::OutOfTurn) => {
            api_error(StatusCode::CONFLICT, "that launch reported this already")
        }
        Err(ReportRefused::NotOurs) => api_error(
            StatusCode::CONFLICT,
            "that process is not one this launch started",
        ),
    }
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct HandoffRequest {
    /// Steam's 64-bit game id of the entry (`SteamGameId`), in decimal.
    #[schema(example = "12412508506844823552")]
    steam_gameid: String,
    /// `SDL_GAMECONTROLLER_IGNORE_DEVICES` as Steam set it for the entry; empty when unset.
    sdl_ignore: String,
    /// `SDL_GAMECONTROLLER_IGNORE_DEVICES_EXCEPT` as Steam set it; empty when unset.
    sdl_ignore_except: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct HandoffAnswer {
    /// What the reports name: `POST /game/handoff/{token}`. 64 hex. A `running` answer's
    /// names nothing.
    token: String,
    /// The title's library id, as `GET /status` lists it.
    app_id: String,
    title: String,
    /// What `sh -c` runs. Absent when the title already runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// Set on top of Steam's environment for the game. Absent when the title already runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<std::collections::BTreeMap<String, String>>,
    /// Always null: the command changes into its own directory.
    cwd: Option<String>,
    /// Present and true when this host already runs the title.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    running: bool,
}

/// What `play` reports on a handoff.
#[derive(Deserialize, ToSchema)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(crate) enum HandoffEvent {
    /// The game's process, which `play` started.
    Started { pid: u32 },
    /// That process exited with `code`. Before `started`: nothing started.
    Exited { code: i32 },
}

#[derive(Deserialize, ToSchema)]
pub(crate) struct EndGameRequest {
    /// Store-qualified id (`steam:570`); omit to reach every title.
    #[serde(default)]
    pub app_id: Option<String>,
    /// Also end games on a live session, not only games nobody is streaming.
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

/// What is in force on this host. A contract seat's is its profile's file or the box's defaults.
fn session_settings_state() -> SessionSettingsState {
    let configured = if pf_paths::seat::role().follows_contract() {
        crate::session_settings::of_profile(&crate::library::View::here()).is_some()
    } else {
        crate::session_settings::store().configured()
    };
    SessionSettingsState {
        settings: crate::session_settings::get(),
        configured,
        enforced: crate::session_settings::enforced(),
    }
}

/// What a seat's host plays the profile in `view` with, `own_account` saying whether that seat
/// is a person's own. `configured`: the profile has settings of its own.
fn seat_profile_state(view: &crate::library::View, own_account: bool) -> SessionSettingsState {
    use crate::session_settings as ss;
    let own = ss::of_profile(view);
    SessionSettingsState {
        configured: own.is_some(),
        settings: ss::contract_settings(
            own,
            ss::box_seat_defaults(),
            cfg!(target_os = "linux") && own_account,
        ),
        enforced: ss::enforced(),
    }
}

/// The refusal for session settings `as` a profile that plays on the owner's host.
fn plays_on_the_host() -> Response {
    api_error_coded(
        StatusCode::CONFLICT,
        "profile_session_settings_host",
        "This profile plays on the host's own desktop, so it uses the host's session settings.",
    )
}

#[utoipa::path(
    get,
    path = "/session/settings",
    tag = "session",
    operation_id = "getSessionSettings",
    params(("as" = Option<String>, Query, description = "Profile id: what that profile's own seat plays with")),
    responses(
        (status = OK, description = "Stored settings + which axes this build enforces", body = SessionSettingsState),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = NOT_FOUND, description = "`as` names no profile on this host", body = ApiError),
        (status = CONFLICT, description = "`as` names a profile that plays on the host's own desktop", body = ApiError),
    )
)]
pub(crate) async fn get_session_settings(
    State(st): State<Arc<MgmtState>>,
    who: super::library::AsProfile,
) -> Response {
    use super::library::Whose;
    let scoped = match who.scope(&st).await {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    match scoped.whose {
        Whose::Here | Whose::Owner => Json(session_settings_state()).into_response(),
        Whose::Seat { own_account } => {
            Json(seat_profile_state(&scoped.view, own_account)).into_response()
        }
        Whose::Shares => plays_on_the_host(),
    }
}

/// Set the session policy
///
/// Persisted clamped. Takes effect on the next decision, including a session
/// already streaming — policy is read at session end, not start. With `as`, the settings of
/// that profile's own seat; a profile on the host's own desktop has none of its own. A paired
/// device that is that locked profile's person may set them too.
#[utoipa::path(
    put,
    path = "/session/settings",
    tag = "session",
    operation_id = "setSessionSettings",
    params(("as" = Option<String>, Query, description = "Profile id: set what that profile's own seat plays with")),
    request_body = crate::session_settings::SessionSettings,
    responses(
        (status = OK, description = "Settings stored; the new state", body = SessionSettingsState),
        (status = BAD_REQUEST, description = "Malformed settings body", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't save the session settings", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "`as` on a plugin's lane, or a device that isn't the person of the profile `as` names", body = ApiError),
        (status = NOT_FOUND, description = "`as` names no profile on this host", body = ApiError),
        (status = CONFLICT, description = "A seat's own host, or `as` names a profile that plays on the host's own desktop", body = ApiError),
    )
)]
pub(crate) async fn set_session_settings(
    State(st): State<Arc<MgmtState>>,
    who: super::library::AsProfile,
    ApiJson(settings): ApiJson<crate::session_settings::SessionSettings>,
) -> Response {
    use super::library::Whose;
    if pf_paths::seat::role().follows_contract() {
        return api_error_coded(
            StatusCode::CONFLICT,
            "seat_session_settings_read_only",
            "A seat's session settings can't be changed here. Change the Seat defaults on the box.",
        );
    }
    let scoped = match who.scope(&st).await {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    let saved = match scoped.whose {
        Whose::Here | Whose::Owner => crate::session_settings::store().set(settings),
        Whose::Seat { .. } => crate::session_settings::set_profile(&scoped.view, settings),
        Whose::Shares => return plays_on_the_host(),
    };
    if let Err(e) = saved {
        return api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session_settings_not_saved",
            "Couldn't save the session settings",
            &format!("{e:#}"),
        );
    }
    if let Whose::Seat { own_account } = scoped.whose {
        tracing::info!(profile = ?scoped.view.layer(), "management API: profile session settings updated");
        return Json(seat_profile_state(&scoped.view, own_account)).into_response();
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

/// Drop a profile's own session settings
///
/// Its seat plays with the box's Seat defaults again; the answer is what `GET` with the same
/// `as` answers, `configured: false`. Dropping settings a profile never had is the same answer.
/// `as` is required: the host's own settings are changed with `PUT`, never dropped. A paired
/// device that is that locked profile's person may drop them too.
#[utoipa::path(
    delete,
    path = "/session/settings",
    tag = "session",
    operation_id = "clearSessionSettings",
    params(("as" = String, Query, description = "Profile id: the profile whose own settings to drop")),
    responses(
        (status = OK, description = "Dropped; the profile's state", body = SessionSettingsState),
        (status = BAD_REQUEST, description = "No `as`, or `as` names the owner", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "`as` on a plugin's lane, or a device that isn't the person of the profile `as` names", body = ApiError),
        (status = NOT_FOUND, description = "`as` names no profile on this host", body = ApiError),
        (status = CONFLICT, description = "A seat's own host, or `as` names a profile that plays on the host's own desktop", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "Couldn't remove the settings file", body = ApiError),
    )
)]
pub(crate) async fn clear_session_settings(
    State(st): State<Arc<MgmtState>>,
    who: super::library::AsProfile,
) -> Response {
    use super::library::Whose;
    if pf_paths::seat::role().follows_contract() {
        return api_error_coded(
            StatusCode::CONFLICT,
            "seat_session_settings_read_only",
            "A seat's session settings can't be changed here. Change the Seat defaults on the box.",
        );
    }
    let scoped = match who.scope(&st).await {
        Ok(s) => s,
        Err(refused) => return refused,
    };
    let own_account =
        match scoped.whose {
            Whose::Seat { own_account } => own_account,
            Whose::Shares => return plays_on_the_host(),
            Whose::Here | Whose::Owner => return api_error_coded(
                StatusCode::BAD_REQUEST,
                "session_settings_need_profile",
                "Couldn't remove the host's own session settings. Change them instead, or pick a \
                 profile.",
            ),
        };
    if let Err(e) = crate::session_settings::clear_profile(&scoped.view) {
        return api_error_logged(
            StatusCode::INTERNAL_SERVER_ERROR,
            "session_settings_not_saved",
            "Couldn't save the session settings",
            &format!("{e:#}"),
        );
    }
    tracing::info!(profile = ?scoped.view.layer(), "management API: profile session settings dropped");
    Json(seat_profile_state(&scoped.view, own_account)).into_response()
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

#[cfg(test)]
mod game_ender_tests {
    use super::GameEnder;

    fn device(fp: &str) -> GameEnder {
        GameEnder {
            owner: Some(fp.into()),
            desk: false,
            refused: false,
        }
    }

    /// A device ends its own launches, in any hex case, and nobody else's. An
    /// anonymous launch belongs to no device, and the desk's is not its own.
    #[test]
    fn a_device_may_end_only_what_it_launched() {
        let me = device("abcdef");
        assert!(me.may_end("running", Some("ABCDEF")));
        assert!(me.may_end("detached", Some("abcdef")));
        assert!(!me.may_end("running", Some("123456")));
        assert!(!me.may_end("running", None));
        assert!(!me.may_end("running", Some(crate::desk::KEY)));
    }

    /// A device on this machine with the launch grant reaches the desk's games, and still no
    /// other device's.
    #[test]
    fn a_device_at_the_desk_may_end_the_desk_s_games() {
        let here = GameEnder {
            desk: true,
            ..device("abcdef")
        };
        assert!(here.may_end("running", Some(crate::desk::KEY)));
        assert!(here.may_end("launching", Some(crate::desk::KEY)));
        assert!(!here.may_end("exited", Some(crate::desk::KEY)));
        assert!(!here.may_end("running", Some("123456")));
    }

    #[test]
    fn the_operator_may_end_any_launch_the_host_can_still_end() {
        let op = GameEnder {
            owner: None,
            desk: true,
            refused: false,
        };
        assert!(op.may_end("grace", None));
        assert!(op.may_end("launching", Some("abcdef")));
        assert!(op.may_end("running", Some(crate::desk::KEY)));
        assert!(
            !op.may_end("untracked", Some("abcdef")),
            "exit is never seen"
        );
        assert!(!op.may_end("exited", Some("abcdef")));
    }

    #[test]
    fn an_expired_device_may_end_nothing() {
        let expired = GameEnder {
            owner: Some("abcdef".into()),
            desk: true,
            refused: true,
        };
        assert!(!expired.may_end("running", Some("abcdef")));
        assert!(!expired.may_end("running", Some(crate::desk::KEY)));
    }
}
