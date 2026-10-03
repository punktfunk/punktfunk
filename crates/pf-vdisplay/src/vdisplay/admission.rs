//! Mode-conflict **admission** (`design/display-management.md`).
//!
//! When a *different* client connects while a session is live, `mode_conflict`
//! decides before Welcome / RTSP so the client gets a typed answer, not a
//! mid-build failure:
//!
//! * `separate` — fresh display at the requested mode (Linux default).
//! * `join` — admit onto the live display: its mode, compositor and route
//!   (Welcome carries the real mode).
//! * `steal` — signal victim stop flags, wait for their release ([`all_gone`]), then serve.
//! * `reject` — handshake error naming the live mode and client.
//!
//! [`register`] exposes identity + mode + stop flag; the session drops
//! [`LiveGuard`] on end. [`decide`] is pure over that slice.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::policy::{self, ModeConflict};

/// The display a live session streams, which a `join` session takes as its own.
#[derive(Clone, Debug, Default)]
pub struct LiveDisplay {
    pub compositor: Option<crate::Compositor>,
    pub route: Option<crate::GamescopeRoute>,
    /// The owner's isolated planes (`design/gamescope-multiuser.md`): a joiner reads its input
    /// relay and taps its sink.
    pub isolation: Option<crate::SessionIsolation>,
    /// `node.name` of the sink this session captures, published once its capturer is open.
    /// A joiner on the shared path taps it instead of claiming a second default sink.
    pub audio_sink: Arc<Mutex<Option<String>>>,
}

#[derive(Clone)]
pub struct LiveSession {
    id: u64,
    /// Cert fingerprint; `None` = anonymous / no client cert.
    pub identity: Option<[u8; 32]>,
    pub mode: (u32, u32, u32),
    /// Signaled to preempt this session on `steal`.
    pub stop: Arc<AtomicBool>,
    /// Client label interpolated into `reject` messages.
    pub label: String,
    pub display: LiveDisplay,
    /// The `punktfunk/2` session id a reconnect presents to retire exactly this session.
    pub resume_id: Option<[u8; 16]>,
}

#[derive(Debug)]
pub enum Admission {
    Separate,
    /// Admit at this live mode onto the owner's display; Welcome must carry the mode, not the
    /// request.
    Join((u32, u32, u32), LiveDisplay),
    /// Victim stop flags; caller signals them and waits until [`all_gone`].
    Steal(Vec<Arc<AtomicBool>>),
    Reject(String),
}

fn table() -> &'static Mutex<Vec<LiveSession>> {
    static T: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(Vec::new()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Two identities match only when both are `Some` and equal. Anonymous
/// (`None`) never matches, so two anonymous clients conflict under
/// `steal` / `reject`.
fn same_client(a: Option<[u8; 32]>, b: Option<[u8; 32]>) -> bool {
    matches!((a, b), (Some(x), Some(y)) if x == y)
}

/// Pure over the live slice. A conflict is a live session owned by a
/// *different* client; a same-client reconnect is always `Separate` here
/// and preempts downstream.
pub fn decide(
    conflict: ModeConflict,
    req_identity: Option<[u8; 32]>,
    live: &[LiveSession],
) -> Admission {
    let others: Vec<&LiveSession> = live
        .iter()
        .filter(|s| !same_client(s.identity, req_identity))
        .collect();
    if others.is_empty() {
        return Admission::Separate;
    }
    match conflict {
        ModeConflict::Separate => Admission::Separate,
        // Oldest other session: the established primary the desktop is built on.
        ModeConflict::Join => Admission::Join(others[0].mode, others[0].display.clone()),
        ModeConflict::Steal => {
            Admission::Steal(others.iter().map(|s| Arc::clone(&s.stop)).collect())
        }
        ModeConflict::Reject => {
            let v = others[0];
            Admission::Reject(format!(
                "host busy: streaming {}x{}@{} to {}",
                v.mode.0, v.mode.1, v.mode.2, v.label
            ))
        }
    }
}

/// Console `mode_conflict` for THIS client, default `Separate` when unconfigured.
///
/// Per device (`design/web-console-overhaul.md` §6.1): the TV wants to take over,
/// the tablet wants its own screen, and one host policy cannot serve both.
///
/// Every platform means the same `separate`: on Windows each identity gets its own
/// monitor slot and sealed ring (`design/windows-parallel-virtual-displays.md`).
/// Shared by the native and GameStream admission paths.
pub fn effective_conflict(fp: Option<[u8; 32]>) -> ModeConflict {
    policy::prefs()
        .configured()
        .map(|p| p.effective_for(policy::fp_hex(fp).as_deref()).mode_conflict)
        .unwrap_or(ModeConflict::Separate)
}

/// [`effective_conflict`] + [`decide`] against the live set. When
/// `Separate` would mint a second display, also apply `max_displays` and
/// encoder headroom. Fail-closed: a display we cannot afford is declined
/// here, never admitted then degrading a live sibling
/// (`design/windows-parallel-virtual-displays.md`).
pub fn admit(req_identity: Option<[u8; 32]>) -> Admission {
    // The table lock covers `decide` only: the budget check below takes `manager::snapshot`,
    // which stalls on DDC/SetupAPI, and every connect and mgmt read waits on this table.
    // Only OTHER clients count: a same-client reconnect whose zombie has not dropped yet is
    // about to reuse its own slot, not take a second one.
    let (decision, any_live) = {
        let live = table().lock().unwrap();
        (
            decide(effective_conflict(req_identity), req_identity, &live),
            live.iter().any(|s| !same_client(s.identity, req_identity)),
        )
    };
    let _ = any_live; // used only by the cfg-gated budget blocks below

    // Enforce `max_displays` here, not in `acquire`. A mid-stream rebuild
    // (capture loss, Game↔Desktop) mints the new display before dropping
    // the old one, so a ceiling there would count the session against
    // itself and refuse recovery.
    #[cfg(target_os = "linux")]
    if matches!(decision, Admission::Separate) && any_live {
        // Lingering displays are not counted: `acquire` reuses the requester's or evicts one,
        // so the pool stays within the cap either way.
        let max = policy::prefs().get().effective().max_displays;
        let held = super::registry::budget_display_count();
        if held >= max {
            return Admission::Reject(format!(
                "host display budget exhausted: {held} display(s) live/pinned, max_displays = {max}"
            ));
        }
    }
    #[cfg(windows)]
    if matches!(decision, Admission::Separate) && any_live {
        let max = policy::prefs().get().effective().max_displays;
        let slots = super::manager::snapshot().len() as u32;
        if slots >= max {
            return Admission::Reject(format!(
                "host display budget exhausted: {slots} display(s) live/kept, max_displays = {max}"
            ));
        }
        // No encoder-headroom gate: the encoders live in the driver's WUDFHost now, and the
        // host-side session counter it used to read never moves.
    }
    decision
}

/// Stop flags of live sessions owned by `req_identity` (its own zombies).
/// Testable over a slice; the public fn locks the global table.
fn same_identity_stops(
    req_identity: Option<[u8; 32]>,
    live: &[LiveSession],
) -> Vec<Arc<AtomicBool>> {
    live.iter()
        .filter(|s| same_client(s.identity, req_identity))
        .map(|s| Arc::clone(&s.stop))
        .collect()
}

/// Stop flags of this client's still-live session(s).
///
/// A new connection from an already-registered identity is a reconnect:
/// the old session is a zombie whose QUIC idle timer has not fired
/// (`max_idle_timeout`, seconds). The caller signals these flags and waits
/// until [`all_gone`], so this reconnect reuses the kept display instead of
/// landing on a second one. Anonymous (`None`) never matches. Call before
/// [`admit`] and before this session [`register`]s, so only a *prior*
/// session's flag is signaled.
pub fn preempt_same_identity(req_identity: Option<[u8; 32]>) -> Vec<Arc<AtomicBool>> {
    same_identity_stops(req_identity, &table().lock().unwrap())
}

/// Stop flags of the session a `punktfunk/2` reconnect names by `resume_id`. Only the identity
/// that owned it may retire it: the id is a bearer secret, and this keeps it one.
pub fn preempt_resumed(
    resume_id: [u8; 16],
    req_identity: Option<[u8; 32]>,
) -> Vec<Arc<AtomicBool>> {
    resumed_stops(resume_id, req_identity, &table().lock().unwrap())
}

fn resumed_stops(
    resume_id: [u8; 16],
    req_identity: Option<[u8; 32]>,
    live: &[LiveSession],
) -> Vec<Arc<AtomicBool>> {
    live.iter()
        .filter(|s| s.resume_id == Some(resume_id) && s.identity == req_identity)
        .map(|s| Arc::clone(&s.stop))
        .collect()
}

/// Whether every session behind `stops` has left the live set. Its guard drops at the end of
/// its teardown, so its display lease is released by then.
pub fn all_gone(stops: &[Arc<AtomicBool>]) -> bool {
    let live = table().lock().unwrap();
    !live
        .iter()
        .any(|s| stops.iter().any(|x| Arc::ptr_eq(x, &s.stop)))
}

/// Register an admitted session; the guard removes it on drop. Call after
/// [`admit`] (so a session never conflicts with itself) once mode, stop and
/// display are known.
pub fn register(
    identity: Option<[u8; 32]>,
    mode: (u32, u32, u32),
    stop: Arc<AtomicBool>,
    label: String,
    display: LiveDisplay,
    resume_id: Option<[u8; 16]>,
) -> LiveGuard {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    table().lock().unwrap().push(LiveSession {
        id,
        identity,
        mode,
        stop,
        label,
        display,
        resume_id,
    });
    LiveGuard { id }
}

/// Is this identity streaming right now? Asked before the host pre-warms that seat: a live
/// session already owns its planes, and a second compositor under the same id fights for them.
pub fn has_live_session(identity: [u8; 32]) -> bool {
    table()
        .lock()
        .unwrap()
        .iter()
        .any(|s| same_client(s.identity, Some(identity)))
}

pub struct LiveGuard {
    id: u64,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        table().lock().unwrap().retain(|s| s.id != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(identity: Option<u8>, mode: (u32, u32, u32)) -> LiveSession {
        LiveSession {
            id: 0,
            identity: identity.map(|n| {
                let mut f = [0u8; 32];
                f[0] = n;
                f
            }),
            mode,
            stop: Arc::new(AtomicBool::new(false)),
            label: "peer".into(),
            display: LiveDisplay::default(),
            resume_id: None,
        }
    }
    fn fp(n: u8) -> Option<[u8; 32]> {
        let mut f = [0u8; 32];
        f[0] = n;
        Some(f)
    }

    #[test]
    fn no_live_session_is_always_separate() {
        for c in [
            ModeConflict::Separate,
            ModeConflict::Join,
            ModeConflict::Steal,
            ModeConflict::Reject,
        ] {
            assert!(matches!(decide(c, fp(1), &[]), Admission::Separate));
        }
    }

    #[test]
    fn same_client_never_conflicts() {
        let live = [sess(Some(1), (2560, 1440, 60))];
        assert!(matches!(
            decide(ModeConflict::Reject, fp(1), &live),
            Admission::Separate
        ));
        assert!(matches!(
            decide(ModeConflict::Steal, fp(1), &live),
            Admission::Separate
        ));
    }

    #[test]
    fn different_client_applies_policy() {
        let live = [sess(Some(1), (2560, 1440, 60))];
        assert!(matches!(
            decide(ModeConflict::Separate, fp(2), &live),
            Admission::Separate
        ));
        assert!(matches!(
            decide(ModeConflict::Join, fp(2), &live),
            Admission::Join((2560, 1440, 60), _)
        ));
        assert!(matches!(
            decide(ModeConflict::Steal, fp(2), &live),
            Admission::Steal(v) if v.len() == 1
        ));
        assert!(matches!(
            decide(ModeConflict::Reject, fp(2), &live),
            Admission::Reject(r) if r.contains("2560x1440@60")
        ));
    }

    #[test]
    fn two_anonymous_clients_conflict() {
        let live = [sess(None, (1920, 1080, 60))];
        assert!(matches!(
            decide(ModeConflict::Reject, None, &live),
            Admission::Reject(_)
        ));
    }

    #[test]
    fn same_identity_stops_targets_own_zombie_only() {
        let live = [
            sess(Some(1), (2560, 1440, 60)),
            sess(Some(2), (1920, 1080, 60)),
        ];
        assert_eq!(same_identity_stops(fp(1), &live).len(), 1);
        assert_eq!(same_identity_stops(fp(3), &live).len(), 0);
        assert_eq!(same_identity_stops(None, &live).len(), 0);
    }

    #[test]
    fn join_targets_the_oldest_other_session() {
        let mut owner = sess(Some(1), (3840, 2160, 60));
        owner.display.compositor = Some(crate::Compositor::Gamescope);
        owner.display.route = Some(crate::GamescopeRoute::Spawn);
        let live = [owner, sess(Some(2), (1280, 720, 120))];
        let Admission::Join(mode, display) = decide(ModeConflict::Join, fp(3), &live) else {
            panic!("join policy must admit onto the live display");
        };
        assert_eq!(mode, (3840, 2160, 60));
        // The joiner takes the owner's display, not only its mode.
        assert_eq!(display.compositor, Some(crate::Compositor::Gamescope));
        assert_eq!(display.route, Some(crate::GamescopeRoute::Spawn));
    }

    #[test]
    fn a_resume_retires_only_its_own_identitys_session() {
        let mut a = sess(Some(1), (1920, 1080, 60));
        a.resume_id = Some([7; 16]);
        let mut b = sess(None, (1280, 720, 60));
        b.resume_id = Some([8; 16]);
        let live = [a.clone(), b.clone()];
        assert_eq!(resumed_stops([7; 16], a.identity, &live).len(), 1);
        assert!(resumed_stops([7; 16], Some([9; 32]), &live).is_empty());
        assert!(resumed_stops([7; 16], None, &live).is_empty());
        assert_eq!(
            resumed_stops([8; 16], None, &live).len(),
            1,
            "anonymous resumes anonymous"
        );
        assert!(resumed_stops([9; 16], a.identity, &live).is_empty());
    }

    #[test]
    fn a_stopped_session_is_gone_once_its_guard_drops() {
        let stop = Arc::new(AtomicBool::new(false));
        let guard = register(
            Some([0xEE; 32]),
            (640, 480, 30),
            stop.clone(),
            "gone".into(),
            LiveDisplay::default(),
            None,
        );
        assert!(!all_gone(std::slice::from_ref(&stop)));
        drop(guard);
        assert!(all_gone(&[stop]));
    }
}
