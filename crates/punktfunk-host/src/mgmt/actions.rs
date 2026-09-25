//! `/api/v1/actions` — discovery of what this host can run, filtered per caller, and the
//! id-only invoke. See `design/host-actions.md`. Built-ins: four power actions and
//! `display.next`; later host- or plugin-provided actions reuse these two routes.
//!
//! Admin bearer: both routes, everything permitted. Paired streaming cert: both routes; a
//! power invoke re-reads `effective(fp, now)` and demands `GRANT_POWER`, `display.next`
//! demands the caller's own live session. Plugin token: neither.
//!
//! Invoke is a trigger: the id selects a fixed host-side behavior, the body is empty, and no
//! request field reaches the privileged path. Power on accept: `202` → typed `HostPower`
//! close of every session → ~1 s so the reply flushes → act. `display.next` ends nothing.

use super::auth::AuthLane;
use super::shared::*;
use crate::gamestream::tls::PeerCertFingerprint;
use crate::power::{Availability, PowerVerb};
use crate::vdisplay::monitors::PhysicalMonitor;
use axum::Extension;
use std::sync::atomic::{AtomicBool, Ordering};

/// What a built-in runs. Only `Power` takes the power tail (end every session, then act).
#[derive(Clone, Copy)]
enum Verb {
    Power(PowerVerb),
    /// Move the streamed-monitor pin to the next head ([`next_monitor`]).
    DisplayNext,
}

/// Built-in: a stable id (`<group>.<verb>`; `plugin:<id>:<verb>` is reserved) bound to a
/// fixed executor. The registry is code so a request cannot add to it.
struct Builtin {
    id: &'static str,
    /// English fallback. Clients localize known ids and use this for unknown ones.
    title: &'static str,
    /// Two-press confirm hint. Reboot/shutdown lose state; sleep is reversible.
    danger: bool,
    group: &'static str,
    verb: Verb,
}

const BUILTINS: [Builtin; 5] = [
    Builtin {
        id: "power.sleep",
        title: "Sleep host",
        danger: false,
        group: "power",
        verb: Verb::Power(PowerVerb::Sleep),
    },
    Builtin {
        id: "power.reboot",
        title: "Restart host",
        danger: true,
        group: "power",
        verb: Verb::Power(PowerVerb::Reboot),
    },
    Builtin {
        id: "power.shutdown",
        title: "Shut down host",
        danger: true,
        group: "power",
        verb: Verb::Power(PowerVerb::Shutdown),
    },
    Builtin {
        id: "host.restart",
        title: "Restart Punktfunk",
        danger: true,
        group: "host",
        verb: Verb::Power(PowerVerb::Restart),
    },
    Builtin {
        id: "display.next",
        title: "Next monitor",
        danger: false,
        group: "display",
        verb: Verb::DisplayNext,
    },
];

/// One action as the caller sees it (`GET /actions`).
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionInfo {
    /// Invoke path parameter (`power.sleep`, …).
    #[schema(example = "power.sleep")]
    pub id: String,
    /// Clients localize known ids and fall back to this for unknown ones.
    pub title: String,
    pub group: String,
    /// Double-confirm hint: reboot/shutdown lose state.
    pub danger: bool,
    /// Platform probe. A VM that cannot S3 lists sleep as unavailable rather than a dead switch.
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_reason: Option<String>,
    /// Whether THIS caller may invoke it. Admin: always. Cert: the live `GRANT_POWER` bit for
    /// power, its own live session for `display.next`.
    pub permitted: bool,
}

#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionList {
    pub actions: Vec<ActionInfo>,
}

/// What `display.next` did (`200`). Power actions answer `202` with no body.
#[derive(Serialize, Deserialize, ToSchema)]
pub(crate) struct ActionOutcome {
    /// Connector streamed from now on.
    #[schema(example = "HDMI-A-1")]
    pub monitor: String,
}

/// Admin always; a paired cert iff its live mask (re-read now — expiry- and edit-aware)
/// carries [`punktfunk_core::quic::GRANT_POWER`]. Any other lane: no.
fn power_permitted(st: &MgmtState, lane: AuthLane, fp: Option<&str>) -> bool {
    match lane {
        AuthLane::Admin => true,
        AuthLane::Cert => fp.is_some_and(|fp| {
            st.native
                .as_ref()
                .and_then(|n| n.effective(fp, unix_now()))
                .is_some_and(|mask| mask & punktfunk_core::quic::GRANT_POWER != 0)
        }),
        AuthLane::Plugin | AuthLane::Public => false,
    }
}

/// Admin always; a paired cert iff it owns a live session, not a join onto another's display.
/// No grant bit: choosing what you watch is not power over the machine.
fn display_permitted(lane: AuthLane, fp: Option<&str>) -> bool {
    match lane {
        AuthLane::Admin => true,
        AuthLane::Cert => fp.is_some_and(crate::session_status::owns_live_session),
        AuthLane::Plugin | AuthLane::Public => false,
    }
}

/// Platform probe per verb. Blocking (logind D-Bus, compositor IPC): call it off the worker.
fn probe(verb: Verb) -> Availability {
    let no = |reason: &str| Availability {
        available: false,
        reason: Some(reason.into()),
    };
    match verb {
        Verb::Power(v) if crate::power::supported() => crate::power::probe(v),
        Verb::Power(_) => no("not supported on this host platform"),
        Verb::DisplayNext => match next_monitor_target() {
            Ok(_) => Availability {
                available: true,
                reason: None,
            },
            Err(reason) => no(reason),
        },
    }
}

/// The enabled, unmanaged head after `current` in `(x, y, connector)` order, wrapping; the
/// first one when `current` is not among them. `None` below two: nothing to switch to.
fn next_monitor(heads: &[PhysicalMonitor], current: &str) -> Option<String> {
    let mut heads: Vec<&PhysicalMonitor> =
        heads.iter().filter(|m| m.enabled && !m.managed).collect();
    if heads.len() < 2 {
        return None;
    }
    heads.sort_by(|a, b| (a.x, a.y, &a.connector).cmp(&(b.x, b.y, &b.connector)));
    let at = heads
        .iter()
        .position(|m| m.connector.eq_ignore_ascii_case(current));
    let next = at.map_or(0, |i| (i + 1) % heads.len());
    Some(heads[next].connector.clone())
}

/// `(current, next)` for `display.next`, or the user-facing reason it is unavailable.
///
/// The env pin outranks the stored one, so a policy write would not take effect under it.
/// `heads` runs only once a stored pin exists: an unpinned host pays no compositor IPC.
fn plan_display_next(
    env_pin: Option<&str>,
    stored_pin: Option<String>,
    heads: impl FnOnce() -> anyhow::Result<Vec<PhysicalMonitor>>,
) -> Result<(String, String), &'static str> {
    if env_pin.is_some() {
        return Err("The host's settings file fixes which monitor is streamed.");
    }
    let current =
        stored_pin.ok_or("The host streams a virtual display, not one of its monitors.")?;
    let heads = heads().map_err(|_| "Couldn't read the host's monitors.")?;
    let next = next_monitor(&heads, &current).ok_or("The host has no other monitor to stream.")?;
    Ok((current, next))
}

/// [`plan_display_next`] on this host. Blocking: one monitor-list IPC when a pin is stored.
fn next_monitor_target() -> Result<(String, String), &'static str> {
    if !cfg!(target_os = "linux") {
        return Err("Switching the streamed monitor needs a Linux host.");
    }
    plan_display_next(
        pf_host_config::config().capture_monitor.as_deref(),
        crate::vdisplay::policy::prefs().get().capture_monitor,
        || crate::vdisplay::detect().and_then(crate::vdisplay::monitors::list),
    )
}

/// Host wall clock, unix seconds — the clock stored access deadlines are expressed in.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// List host actions
///
/// Per-caller view: platform availability plus whether this caller may invoke each one.
/// Admin: everything permitted. Paired cert: power per the device's live Host-power grant,
/// `display.next` while the device owns a live session. Unknown ids still render with the
/// server-supplied title.
#[utoipa::path(
    get,
    path = "/actions",
    tag = "actions",
    operation_id = "listActions",
    responses(
        (status = OK, description = "The actions, per-caller", body = ActionList),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn list_actions(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    fp: Option<Extension<PeerCertFingerprint>>,
) -> Json<ActionList> {
    let fp = fp.as_ref().and_then(|e| e.0 .0.as_deref());
    let power = power_permitted(&st, lane, fp);
    let display = display_permitted(lane, fp);
    // D-Bus and compositor round trips — off the async worker, all of them in one hop.
    let probed = tokio::task::spawn_blocking(|| BUILTINS.map(|b| probe(b.verb)))
        .await
        .expect("action probe task panicked");
    let actions = BUILTINS
        .iter()
        .zip(probed)
        .map(|(b, avail)| ActionInfo {
            id: b.id.into(),
            title: b.title.into(),
            group: b.group.into(),
            danger: b.danger,
            available: avail.available,
            unavailable_reason: avail.reason,
            permitted: match b.verb {
                Verb::Power(_) => power,
                Verb::DisplayNext => display,
            },
        })
        .collect();
    Json(ActionList { actions })
}

/// One action in flight host-wide (`409` otherwise). The actions end the conversation, so
/// this is all the rate limiting v1 needs.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Once per (fingerprint, action) per boot — a retrying client must not turn the host log
/// into the DoS (`GrantDrops`).
fn log_denial_once(fp: &str, action: &str, device: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static LOGGED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let mut set = LOGGED
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if set.insert((fp.to_string(), action.to_string())) {
        tracing::info!(
            device,
            fingerprint = fp,
            action,
            "denied a host action — this device's access lacks the Host power grant \
             (further denials of this pair are silent this boot)"
        );
    }
}

/// Invoke a host action
///
/// Id-only, empty body: nothing in the request reaches the privileged path. Power actions
/// answer `202`, end every session (typed HostPower close), wait ~1 s so this response
/// flushes, then act; paired-cert callers need Host power and are `409` while another
/// device's session is live, the admin console never is. One power action at a time.
/// `display.next` answers `200` with the monitor now streamed and ends nothing; a paired
/// cert needs a live session of its own.
#[utoipa::path(
    post,
    path = "/actions/{id}",
    tag = "actions",
    operation_id = "invokeAction",
    params(("id" = String, Path, description = "Action id (`power.sleep`, `power.reboot`, `power.shutdown`, `host.restart`, `display.next`)")),
    responses(
        (status = OK, description = "Done (`display.next`): the monitor streamed from now on", body = ActionOutcome),
        (status = ACCEPTED, description = "Accepted — sessions are being ended and the action follows in about a second"),
        (status = FORBIDDEN, description = "This caller's access does not include this action (no Host power grant, or no live session of its own for `display.next`)", body = ApiError),
        (status = NOT_FOUND, description = "Unknown action id", body = ApiError),
        (status = CONFLICT, description = "Refused: an action is already in flight, another device's session is live (cert lane), or the platform said no (a foreign sleep inhibitor, a second local user, a single monitor, …)", body = ApiError),
        (status = NOT_IMPLEMENTED, description = "This host platform has no executor for it (macOS host; `display.next` off Linux)", body = ApiError),
        (status = INTERNAL_SERVER_ERROR, description = "`display.next` could not store the new monitor", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid credentials", body = ApiError),
    )
)]
pub(crate) async fn invoke_action(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    fp: Option<Extension<PeerCertFingerprint>>,
    Path(id): Path<String>,
) -> Response {
    let Some(builtin) = BUILTINS.iter().find(|b| b.id == id) else {
        return api_error(StatusCode::NOT_FOUND, "unknown action id");
    };
    let fp = fp.as_ref().and_then(|e| e.0 .0.as_deref());
    let device = fp.and_then(|fp| {
        st.native.as_ref().and_then(|n| {
            n.list()
                .into_iter()
                .find(|c| c.fingerprint.eq_ignore_ascii_case(fp))
                .map(|c| crate::events::DeviceRef {
                    name: c.name,
                    fingerprint: c.fingerprint,
                    plane: crate::events::Plane::Native,
                })
        })
    });
    let verb = match builtin.verb {
        Verb::Power(verb) => verb,
        Verb::DisplayNext => return invoke_display_next(builtin.id, lane, fp, device).await,
    };
    if !power_permitted(&st, lane, fp) {
        let device_name = device.as_ref().map(|d| d.name.as_str()).unwrap_or("");
        log_denial_once(fp.unwrap_or(""), builtin.id, device_name);
        return api_error(
            StatusCode::FORBIDDEN,
            "this device's access does not include host power — ask the host's operator to \
             enable the Host power grant",
        );
    }
    if !crate::power::supported() {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "host power actions are not supported on this host platform",
        );
    }
    // Another device's LIVE session blocks a cert-lane invoke; your own does not. Admin is
    // never blocked. A GameStream stream is always another device: its cert is never native.
    if lane == AuthLane::Cert {
        let others_native = crate::session_status::other_client_live(fp.unwrap_or(""));
        let gamestream = st.app.streaming.load(Ordering::SeqCst);
        if others_native || gamestream {
            return api_error(
                StatusCode::CONFLICT,
                "Another device is streaming from this host right now",
            );
        }
    }
    let avail = tokio::task::spawn_blocking(move || crate::power::probe(verb))
        .await
        .expect("power probe task panicked");
    if !avail.available {
        return api_error(
            StatusCode::CONFLICT,
            avail.reason.as_deref().unwrap_or("the platform said no"),
        );
    }
    if IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return api_error(
            StatusCode::CONFLICT,
            "a host action is already in flight — the host is on its way down",
        );
    }
    let invoker = device
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "the host console".into());
    tracing::info!(action = builtin.id, invoked_by = %invoker, "host action accepted");
    crate::events::emit(crate::events::EventKind::ActionInvoked {
        id: builtin.id.into(),
        device: device.clone(),
        outcome: "accepted".into(),
    });
    // 202 must flush before the NIC goes away. Typed close now; quit-flavored stop catches
    // anonymous sessions; the compat plane tears down on its own path.
    let id_owned: String = builtin.id.into();
    let app = st.app.clone();
    tokio::spawn(async move {
        crate::power::set_closing(true);
        crate::session_status::stop_all_quit();
        let _ = app.quit_session("host power action");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        // Displays first: going under on a still-up display is not a transient.
        drain_displays().await;
        // Drop our suspend veto before asking logind — we never hold -ignore-inhibit rights.
        crate::sleep_inhibit::release_now();
        let outcome = tokio::task::spawn_blocking(move || crate::power::act(verb))
            .await
            .unwrap_or_else(|e| Err(format!("executor task panicked: {e}")));
        match outcome {
            // Reboot/shutdown ends this process shortly; sleep resumes here on wake.
            Ok(()) => tracing::info!(action = %id_owned, "host power action handed to the OS"),
            Err(e) => {
                tracing::warn!(action = %id_owned, error = %e, "host power action FAILED");
                crate::events::emit(crate::events::EventKind::ActionInvoked {
                    id: id_owned,
                    device,
                    outcome: format!("failed: {e}"),
                });
            }
        }
        crate::power::set_closing(false);
        IN_FLIGHT.store(false, Ordering::SeqCst);
    });
    StatusCode::ACCEPTED.into_response()
}

/// `display.next`: write the next head as the pin through the same policy write the console
/// radio uses, which persists it and re-aims absolute input. Ends nothing, so none of the
/// power tail (live-device `409`, [`IN_FLIGHT`], session close) applies.
async fn invoke_display_next(
    id: &'static str,
    lane: AuthLane,
    fp: Option<&str>,
    device: Option<crate::events::DeviceRef>,
) -> Response {
    if !display_permitted(lane, fp) {
        return api_error(
            StatusCode::FORBIDDEN,
            "Only the device that is streaming can switch the monitor.",
        );
    }
    if !cfg!(target_os = "linux") {
        return api_error(
            StatusCode::NOT_IMPLEMENTED,
            "Switching the streamed monitor needs a Linux host.",
        );
    }
    let switched = tokio::task::spawn_blocking(|| {
        let (from, to) = next_monitor_target().map_err(|r| (StatusCode::CONFLICT, r.into()))?;
        let mut policy = crate::vdisplay::policy::prefs().get();
        policy.capture_monitor = Some(to.clone());
        super::display::write(policy).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Couldn't save the streamed monitor — {e:#}"),
            )
        })?;
        Ok::<_, (StatusCode, String)>((from, to))
    })
    .await
    .unwrap_or_else(|e| {
        Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Couldn't switch the monitor — {e}"),
        ))
    });
    let (from, to) = match switched {
        Ok(pair) => pair,
        Err((status, message)) => return api_error(status, &message),
    };
    let invoker = device
        .as_ref()
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "the host console".into());
    tracing::info!(action = id, %from, %to, invoked_by = %invoker, "streamed monitor switched");
    crate::events::emit(crate::events::EventKind::ActionInvoked {
        id: id.into(),
        device,
        outcome: "accepted".into(),
    });
    Json(ActionOutcome { monitor: to }).into_response()
}

/// Wait for signaled sessions to drop their virtual displays before going under anyway.
/// Longer than the 1.5 s handshake release grace, short enough a wedged teardown cannot
/// strand the sleep.
const DISPLAY_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
const DISPLAY_DRAIN_TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// Lingering slots to release now, and how many displays we are still waiting on. Pinned
/// displays are exempt from both — see [`drain_displays`]. Pure so the exemption is
/// testable; the registry is a process global.
fn drain_plan<'a>(displays: impl Iterator<Item = (&'a str, u64)>) -> (Vec<u64>, usize) {
    let (mut release, mut waiting) = (Vec::new(), 0usize);
    for (state, slot) in displays {
        match state {
            "lingering" => {
                release.push(slot);
                waiting += 1;
            }
            // Active = still tearing down; wait. Pinned = deliberate; leave it.
            "pinned" => {}
            _ => waiting += 1,
        }
    }
    (release, waiting)
}

/// Tear virtual displays down before the box goes under.
///
/// [`crate::session_status::stop_all_quit`] marks teardown deliberate so the display should
/// skip linger, but that drop is async and the 1 s reply-flush grace is shorter than the
/// 1.5 s the same wait gets in `native/handshake.rs`.
///
/// Going under on a still-up display is not a transient. The linger deadline is
/// `std::time::Instant`; on Linux that clock does not advance while suspended, so the box
/// wakes with the stale display standing and its window unspent.
///
/// [`crate::vdisplay::registry::release`] refuses ACTIVE displays, so the poll is the wait:
/// each tick sweeps `lingering`, and we return once nothing but pinned displays is left.
///
/// A pinned display stays. `KeepAlive::Forever` means until host shutdown or
/// `POST /display/release`; force-releasing it here would kill the nested gamescope session
/// and its game on every sleep — the thing the operator pinned it to avoid.
async fn drain_displays() {
    let deadline = std::time::Instant::now() + DISPLAY_DRAIN_BUDGET;
    loop {
        let snap = crate::vdisplay::registry::snapshot();
        let (release, waiting) =
            drain_plan(snap.displays.iter().map(|d| (d.state.as_str(), d.slot)));
        let pinned = snap.displays.len() - waiting;
        if waiting == 0 {
            if pinned > 0 {
                tracing::info!(
                    pinned,
                    "pinned display(s) left standing across the power action (keep-alive is \
                     `forever`) — free them with POST /display/release if a wake lands dark"
                );
            }
            return;
        }
        // Before the release so a slot that refuses to clear cannot spin past the budget.
        if std::time::Instant::now() >= deadline {
            tracing::warn!(
                displays = waiting,
                "virtual display(s) still up at the power-action budget — going under anyway; \
                 a wake may land on a stale display"
            );
            return;
        }
        if !release.is_empty() {
            // `release` tears the display down inline (gamescope + topology/DPMS restore), so
            // not on a runtime worker — same reason `power::act` is spawned blocking.
            let _ = tokio::task::spawn_blocking(move || {
                for slot in release {
                    crate::vdisplay::registry::release(Some(slot));
                }
            })
            .await;
        }
        tokio::time::sleep(DISPLAY_DRAIN_TICK).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{
        drain_displays, drain_plan, next_monitor, plan_display_next, DISPLAY_DRAIN_BUDGET,
    };
    use crate::vdisplay::monitors::PhysicalMonitor;

    fn head(connector: &str, x: i32, enabled: bool, managed: bool) -> PhysicalMonitor {
        PhysicalMonitor {
            connector: connector.into(),
            description: connector.into(),
            width: 1920,
            height: 1080,
            refresh_mhz: 60_000,
            x,
            y: 0,
            scale: 1.0,
            primary: false,
            enabled,
            managed,
        }
    }

    /// Left to right by origin whatever order the compositor listed them in, wrapping at the end.
    /// A disabled head and one of our own virtual outputs are never a target.
    #[test]
    fn next_monitor_walks_real_heads_left_to_right_and_wraps() {
        let heads = [
            head("HDMI-A-1", 3840, true, false),
            head("Virtual-1", 5760, true, true),
            head("DP-1", 0, true, false),
            head("DP-2", 1920, false, false),
            head("DP-3", 1920, true, false),
        ];
        assert_eq!(next_monitor(&heads, "DP-1").as_deref(), Some("DP-3"));
        assert_eq!(next_monitor(&heads, "dp-3").as_deref(), Some("HDMI-A-1"));
        assert_eq!(next_monitor(&heads, "HDMI-A-1").as_deref(), Some("DP-1"));
        assert_eq!(
            next_monitor(&heads, "DP-2").as_deref(),
            Some("DP-1"),
            "a pin on a head that is off moves to the first real one"
        );
    }

    #[test]
    fn next_monitor_needs_two_real_heads() {
        let one = [
            head("DP-1", 0, true, false),
            head("DP-2", 1920, false, false),
        ];
        assert_eq!(next_monitor(&one, "DP-1"), None);
        assert_eq!(next_monitor(&[], "DP-1"), None);
    }

    /// Offered only while sessions mirror a monitor the action can move: a stored pin, no env
    /// pin over it, and a second head. An unpinned host never pays the compositor round trip.
    #[test]
    fn display_next_is_offered_only_for_a_stored_pin_with_a_second_head() {
        let two = || {
            Ok(vec![
                head("DP-1", 0, true, false),
                head("DP-2", 1920, true, false),
            ])
        };
        assert_eq!(
            plan_display_next(None, Some("DP-1".into()), two),
            Ok(("DP-1".into(), "DP-2".into()))
        );
        assert!(
            plan_display_next(Some("DP-1"), Some("DP-1".into()), two).is_err(),
            "the env pin outranks the policy, so writing it would change nothing"
        );
        assert!(
            plan_display_next(None, None, || -> anyhow::Result<Vec<PhysicalMonitor>> {
                panic!("an unpinned host must not list monitors")
            })
            .is_err(),
            "a virtual-display host has nothing to switch"
        );
        assert!(
            plan_display_next(None, Some("DP-1".into()), || Ok(vec![head(
                "DP-1", 0, true, false
            )]))
            .is_err()
        );
        assert!(
            plan_display_next(None, Some("DP-1".into()), || Err(anyhow::anyhow!(
                "no kwin"
            )))
            .is_err()
        );
    }

    /// No display registered: drain must cost nothing. An inverted emptiness check would
    /// add the whole budget to every power action.
    #[tokio::test]
    async fn drain_displays_returns_at_once_when_no_display_is_up() {
        let t0 = std::time::Instant::now();
        drain_displays().await;
        assert!(
            t0.elapsed() < DISPLAY_DRAIN_BUDGET / 2,
            "drain burned the budget with no displays up: {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn lingering_is_released_active_is_waited_out_pinned_is_left_alone() {
        let (release, waiting) =
            drain_plan([("lingering", 1u64), ("active", 2), ("pinned", 3)].into_iter());
        assert_eq!(release, vec![1], "only a lingering display is released");
        assert_eq!(
            waiting, 2,
            "active + lingering are waited on, pinned is not"
        );
    }

    /// Force-releasing a pin would kill the nested gamescope session and its game on every
    /// sleep. A box with only pinned displays must drain clean, releasing none of them.
    #[test]
    fn a_pinned_only_box_drains_clean_and_releases_nothing() {
        let (release, waiting) = drain_plan([("pinned", 7u64), ("pinned", 8)].into_iter());
        assert!(release.is_empty(), "a pin must survive a power action");
        assert_eq!(waiting, 0, "pinned displays must not hold the drain open");
    }
}
