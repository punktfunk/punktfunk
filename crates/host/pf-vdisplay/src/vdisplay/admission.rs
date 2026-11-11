//! Mode-conflict **admission** (`design/display-management.md`).
//!
//! When a *different* client connects while a session is live, `mode_conflict`
//! decides before Welcome / RTSP so the client gets a typed answer, not a
//! mid-build failure:
//!
//! * `separate` — fresh display at the requested mode (Linux default).
//! * `join` — admit onto the live display: its mode, compositor, route and colour
//!   (Welcome carries the real mode). A joiner that can't take an HDR display is refused.
//! * `steal` — signal victim stop flags, wait for their release ([`all_gone`]), then serve.
//! * `reject` — handshake error naming the live mode and client.
//!
//! [`admit`] puts the connect's row in the table under the lock that decides, as a
//! [`Reservation`]: identity, mode and stop flag. [`Reservation::register`] adds its display
//! once it streams; the session drops [`LiveGuard`] on end. [`decide`] is pure over that slice.

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
    /// The display shows HDR. Its owner republishes it on a codec change ([`publish_hdr`]).
    pub hdr: bool,
}

/// What a joiner refused by [`joined_hdr`] reads.
pub const JOIN_NEEDS_HDR: &str = "Couldn't join the shared screen — it's in HDR, and this device \
     isn't streaming in HDR. Turn on HDR in this app's settings, or ask whoever connected first \
     to turn it off.";

/// The colour a joiner streams the owner's display at: the owner's. `None` when that display
/// is HDR and the joiner can't carry HDR: its capture would read PQ as SDR, so it is refused.
pub fn joined_hdr(owner_hdr: bool, joiner_can_hdr: bool) -> Option<bool> {
    (!owner_hdr || joiner_can_hdr).then_some(owner_hdr)
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
    /// `None` while the row is a [`Reservation`]: admitted, not streaming yet.
    pub display: Option<LiveDisplay>,
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

/// Pure over the live slice, reservations included. A conflict is a session owned by a
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
        // Oldest other session that streams: the established primary the desktop is built on.
        // A reservation has no display to share yet.
        ModeConflict::Join => match others
            .iter()
            .find_map(|s| Some((s.mode, s.display.clone()?)))
        {
            Some((mode, display)) => Admission::Join(mode, display),
            None => Admission::Separate,
        },
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
/// Shared by the native and GameStream admission paths. Reads `get()`, not `configured()`: a
/// seat host with no policy of its own still answers the box's per-device overlays, as its
/// placement does.
pub fn effective_conflict(fp: Option<[u8; 32]>) -> ModeConflict {
    policy::prefs()
        .get()
        .effective_for(policy::fp_hex(fp).as_deref())
        .mode_conflict
}

/// [`decide`] under `conflict` against the live set, and this connect's [`Reservation`] in it,
/// under one lock, so a second connect inside this one's bring-up conflicts with it. A row
/// holding `stop` is the requester's own (GameStream's plane has one flag) and never conflicts
/// with it. No display budget: [`admit`] adds that. A refusal still returns the reservation:
/// drop it before answering.
pub fn admit_under(
    conflict: ModeConflict,
    req_identity: Option<[u8; 32]>,
    mode: (u32, u32, u32),
    stop: Arc<AtomicBool>,
) -> (Admission, Reservation) {
    let mut live = table().lock().unwrap();
    let theirs: Vec<LiveSession> = (live.iter())
        .filter(|s| !Arc::ptr_eq(&s.stop, &stop))
        .cloned()
        .collect();
    let decision = decide(conflict, req_identity, &theirs);
    (decision, reserve(&mut live, req_identity, mode, stop))
}

/// [`admit_under`] this client's [`effective_conflict`]. When `Separate` would mint a second
/// display, also apply `max_displays`. Fail-closed: a display we cannot afford is declined
/// here, never admitted then degrading a live sibling
/// (`design/windows-parallel-virtual-displays.md`).
pub fn admit(
    req_identity: Option<[u8; 32]>,
    mode: (u32, u32, u32),
    stop: Arc<AtomicBool>,
) -> (Admission, Reservation) {
    // The table lock covers `decide` and the row only: the budget check below takes
    // `manager::snapshot`, which stalls on DDC/SetupAPI, and every connect and mgmt read waits
    // on this table.
    let conflict = effective_conflict(req_identity);
    let (decision, reservation) = admit_under(conflict, req_identity, mode, stop);

    // Enforce `max_displays` here, not in `acquire`. A mid-stream rebuild
    // (capture loss, Game↔Desktop) mints the new display before dropping
    // the old one, so a ceiling there would count the session against
    // itself and refuse recovery.
    #[cfg(target_os = "linux")]
    if matches!(decision, Admission::Separate) {
        // Lingering displays are not counted: `acquire` reuses the requester's or evicts one.
        // Pinned ones count with nobody live, since nothing evicts them, except the requester's
        // own: its create takes that one back.
        let max = policy::prefs().get().effective().max_displays;
        let held = super::registry::budget_display_count(req_identity);
        if held >= max {
            let why = format!(
                "host display budget exhausted: {held} display(s) live/pinned, max_displays = {max}"
            );
            return (Admission::Reject(why), reservation);
        }
    }
    // Only OTHER clients count: a same-client reconnect whose zombie has not dropped yet is
    // about to reuse its own slot, not take a second one.
    #[cfg(windows)]
    if matches!(decision, Admission::Separate)
        && (table().lock().unwrap().iter())
            .any(|s| s.id != reservation.id && !same_client(s.identity, req_identity))
    {
        let max = policy::prefs().get().effective().max_displays;
        // The requester's own slot is the monitor its create reuses or resizes in place.
        let own =
            super::identity::peek_slot(req_identity, (mode.0, mode.1), policy::Identity::PerClient);
        let slots = (super::manager::snapshot().iter())
            .filter(|i| Some(i.slot_id) != own)
            .count() as u32;
        if slots >= max {
            let why = format!(
                "host display budget exhausted: {slots} display(s) live/kept, max_displays = {max}"
            );
            return (Admission::Reject(why), reservation);
        }
        // No encoder-headroom gate: the encoders live in the driver's WUDFHost now, and the
        // host-side session counter it used to read never moves.
    }
    (decision, reservation)
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

/// A row for a connect still in bring-up: what later connects decide against until
/// [`Reservation::register`] adds its display.
fn reserve(
    live: &mut Vec<LiveSession>,
    identity: Option<[u8; 32]>,
    mode: (u32, u32, u32),
    stop: Arc<AtomicBool>,
) -> Reservation {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    live.push(LiveSession {
        id,
        identity,
        mode,
        stop,
        label: "a connecting client".into(),
        display: None,
        resume_id: None,
    });
    Reservation { id }
}

/// Register a session that streams without an [`admit`] of its own (GameStream's stream); the
/// guard removes it on drop.
pub fn register(
    identity: Option<[u8; 32]>,
    mode: (u32, u32, u32),
    stop: Arc<AtomicBool>,
    label: String,
    display: LiveDisplay,
    resume_id: Option<[u8; 16]>,
) -> LiveGuard {
    let reservation = reserve(&mut table().lock().unwrap(), identity, mode, stop);
    reservation.register(mode, label, display, resume_id)
}

/// The colour the display of the session holding `stop` now shows, for the next joiner.
pub fn publish_hdr(stop: &Arc<AtomicBool>, hdr: bool) {
    let mut live = table().lock().unwrap();
    for s in live.iter_mut().filter(|s| Arc::ptr_eq(&s.stop, stop)) {
        if let Some(d) = s.display.as_mut() {
            d.hdr = hdr;
        }
    }
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

/// A connect's row in the table from [`admit`] on. Later connects count it: `reject` refuses
/// them and `steal` raises its stop flag. Dropping it takes the row out.
pub struct Reservation {
    id: u64,
}

impl Reservation {
    /// The session streams: later clients see its mode, label and display, a `join` shares it,
    /// and a `punktfunk/2` resume finds it by `resume_id`.
    pub fn register(
        self,
        mode: (u32, u32, u32),
        label: String,
        display: LiveDisplay,
        resume_id: Option<[u8; 16]>,
    ) -> LiveGuard {
        if let Some(s) = table().lock().unwrap().iter_mut().find(|s| s.id == self.id) {
            s.mode = mode;
            s.label = label;
            s.display = Some(display);
            s.resume_id = resume_id;
        }
        LiveGuard { _row: self }
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        table().lock().unwrap().retain(|s| s.id != self.id);
    }
}

/// A streaming session's row; dropping it takes the row out.
pub struct LiveGuard {
    _row: Reservation,
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
            display: Some(LiveDisplay::default()),
            resume_id: None,
        }
    }
    /// A connect admitted but not streaming yet.
    fn pending(identity: Option<u8>, mode: (u32, u32, u32)) -> LiveSession {
        LiveSession {
            label: "a connecting client".into(),
            display: None,
            ..sess(identity, mode)
        }
    }
    fn fp(n: u8) -> Option<[u8; 32]> {
        let mut f = [0u8; 32];
        f[0] = n;
        Some(f)
    }

    /// [`decide`]'s answer as a row reads it.
    fn verdict(a: Admission) -> String {
        match a {
            Admission::Separate => "separate".into(),
            Admission::Join(m, _) => format!("join {}x{}@{}", m.0, m.1, m.2),
            Admission::Steal(v) => format!("steal {}", v.len()),
            Admission::Reject(why) => format!("reject: {why}"),
        }
    }

    /// Each policy against each live set. A conflict is another client's session: the same
    /// identity never conflicts, two anonymous clients always do, and the oldest other session
    /// is the one a join takes and a refusal names.
    #[test]
    fn decide_table() {
        use ModeConflict::{Join, Reject, Separate, Steal};
        let a = sess(Some(1), (2560, 1440, 60));
        let b = sess(Some(2), (1280, 720, 120));
        let anon = sess(None, (1920, 1080, 60));
        let busy = "reject: host busy: streaming 2560x1440@60 to peer";
        type Row = (
            ModeConflict,
            Option<[u8; 32]>,
            Vec<LiveSession>,
            &'static str,
        );
        let rows: Vec<Row> = vec![
            (Separate, fp(1), vec![], "separate"),
            (Join, fp(1), vec![], "separate"),
            (Steal, fp(1), vec![], "separate"),
            (Reject, fp(1), vec![], "separate"),
            // The same client again.
            (Join, fp(1), vec![a.clone()], "separate"),
            (Steal, fp(1), vec![a.clone()], "separate"),
            (Reject, fp(1), vec![a.clone()], "separate"),
            // Another client.
            (Separate, fp(2), vec![a.clone()], "separate"),
            (Join, fp(2), vec![a.clone()], "join 2560x1440@60"),
            (Steal, fp(2), vec![a.clone()], "steal 1"),
            (Reject, fp(2), vec![a.clone()], busy),
            // Two others.
            (Join, fp(3), vec![a.clone(), b.clone()], "join 2560x1440@60"),
            (Steal, fp(3), vec![a.clone(), b.clone()], "steal 2"),
            (Reject, fp(3), vec![a.clone(), b.clone()], busy),
            // The requester's own session is neither joined nor stolen.
            (Join, fp(1), vec![a.clone(), b.clone()], "join 1280x720@120"),
            (Steal, fp(1), vec![a, b], "steal 1"),
            // Anonymous matches nobody, itself included.
            (
                Reject,
                None,
                vec![anon.clone()],
                "reject: host busy: streaming 1920x1080@60 to peer",
            ),
            (Steal, None, vec![anon.clone()], "steal 1"),
            (Steal, fp(1), vec![anon], "steal 1"),
            // A connect still in bring-up conflicts like a streaming one, with no display to join.
            (
                Reject,
                fp(2),
                vec![pending(Some(1), (2560, 1440, 60))],
                "reject: host busy: streaming 2560x1440@60 to a connecting client",
            ),
            (
                Steal,
                fp(2),
                vec![pending(Some(1), (2560, 1440, 60))],
                "steal 1",
            ),
            (
                Join,
                fp(2),
                vec![pending(Some(1), (2560, 1440, 60))],
                "separate",
            ),
            (
                Join,
                fp(3),
                vec![
                    pending(Some(1), (2560, 1440, 60)),
                    sess(Some(2), (1280, 720, 120)),
                ],
                "join 1280x720@120",
            ),
        ];
        for (i, (policy, req, live, want)) in rows.into_iter().enumerate() {
            assert_eq!(verdict(decide(policy, req, &live)), want, "row {i}");
        }
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
        owner.display = Some(LiveDisplay {
            compositor: Some(crate::Compositor::Gamescope),
            route: Some(crate::GamescopeRoute::Spawn),
            ..LiveDisplay::default()
        });
        let live = [owner, sess(Some(2), (1280, 720, 120))];
        let Admission::Join(mode, display) = decide(ModeConflict::Join, fp(3), &live) else {
            panic!("join policy must admit onto the live display");
        };
        assert_eq!(mode, (3840, 2160, 60));
        // The joiner takes the owner's display, not only its mode.
        assert_eq!(display.compositor, Some(crate::Compositor::Gamescope));
        assert_eq!(display.route, Some(crate::GamescopeRoute::Spawn));
    }

    /// A joiner streams at the owner's colour: SDR under an SDR display whatever it could
    /// carry, HDR under an HDR one, and nothing when it can't carry that HDR.
    #[test]
    fn a_joiner_follows_the_owners_colour() {
        assert_eq!(joined_hdr(false, true), Some(false));
        assert_eq!(joined_hdr(false, false), Some(false));
        assert_eq!(joined_hdr(true, true), Some(true));
        assert_eq!(joined_hdr(true, false), None);
    }

    /// An owner's codec change reaches the next joiner through its row.
    #[test]
    fn a_published_colour_reaches_the_next_join() {
        let stop = Arc::new(AtomicBool::new(false));
        let display = LiveDisplay {
            hdr: true,
            ..LiveDisplay::default()
        };
        let _guard = register(
            Some([0xED; 32]),
            MODE,
            stop.clone(),
            "hdr".into(),
            display,
            None,
        );
        let row_hdr = || {
            let live = table().lock().unwrap();
            let row = live.iter().find(|s| Arc::ptr_eq(&s.stop, &stop)).cloned();
            row.and_then(|s| s.display).map(|d| d.hdr)
        };
        assert_eq!(row_hdr(), Some(true));
        publish_hdr(&stop, false);
        assert_eq!(row_hdr(), Some(false));
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

    const MODE: (u32, u32, u32) = (1920, 1080, 60);

    /// This child host's console says `mode_conflict: <policy>`.
    fn configure(policy: &str) {
        let doc = format!(r#"{{"mode_conflict":"{policy}"}}"#);
        std::fs::write(pf_paths::config_dir().join("display-settings.json"), doc).unwrap();
    }

    fn rows() -> usize {
        table().lock().unwrap().len()
    }

    /// Two connects inside one bring-up: the second sees the first from its admission on.
    #[test]
    fn a_second_connect_inside_the_first_ones_bring_up_is_refused() {
        let name = "a_second_connect_inside_the_first_ones_bring_up_is_refused";
        crate::fake::in_child(module_path!(), name, &[], || {
            configure("reject");
            let first_stop = Arc::new(AtomicBool::new(false));
            let (first, _first) = admit(fp(1), MODE, first_stop);
            assert_eq!(verdict(first), "separate");
            let (second, _second) = admit(fp(2), (1280, 720, 60), Arc::default());
            assert_eq!(
                verdict(second),
                "reject: host busy: streaming 1920x1080@60 to a connecting client"
            );
        });
    }

    /// `steal` reaches a connect still in bring-up through the stop flag it was admitted with.
    #[test]
    fn a_steal_raises_the_stop_of_a_connect_in_bring_up() {
        let name = "a_steal_raises_the_stop_of_a_connect_in_bring_up";
        crate::fake::in_child(module_path!(), name, &[], || {
            configure("steal");
            let first_stop = Arc::new(AtomicBool::new(false));
            let (_, _first) = admit(fp(1), MODE, first_stop.clone());
            let (second, _second) = admit(fp(2), MODE, Arc::default());
            let Admission::Steal(victims) = second else {
                panic!("steal must name the connect in bring-up");
            };
            assert!(victims.len() == 1 && Arc::ptr_eq(&victims[0], &first_stop));
        });
    }

    /// The guard covers every way out: a refused connect's reservation leaves nothing behind,
    /// and a registered one leaves with its session.
    #[test]
    fn a_refused_connect_leaves_the_table_empty() {
        let name = "a_refused_connect_leaves_the_table_empty";
        crate::fake::in_child(module_path!(), name, &[], || {
            configure("reject");
            let (_, first) = admit(fp(1), MODE, Arc::default());
            let (second, refused) = admit(fp(2), MODE, Arc::default());
            assert!(matches!(second, Admission::Reject(_)));
            drop(refused);
            assert_eq!(rows(), 1, "only the admitted connect");
            let live = first.register(MODE, "first".into(), LiveDisplay::default(), None);
            let (third, refused) = admit(fp(3), MODE, Arc::default());
            assert_eq!(
                verdict(third),
                "reject: host busy: streaming 1920x1080@60 to first"
            );
            drop(refused);
            drop(live);
            assert_eq!(rows(), 0);
        });
    }

    /// A row holding the requester's own stop flag is its own plane's, as GameStream's stream
    /// beside its next launch, and never conflicts with it. Anyone else's still does.
    #[test]
    fn a_row_holding_the_requesters_stop_is_its_own() {
        let name = "a_row_holding_the_requesters_stop_is_its_own";
        crate::fake::in_child(module_path!(), name, &[], || {
            let plane = Arc::new(AtomicBool::new(false));
            let display = LiveDisplay::default();
            let _stream = register(None, MODE, plane.clone(), "gs".into(), display, None);
            let (own, _own) = admit_under(ModeConflict::Reject, None, MODE, plane);
            assert_eq!(verdict(own), "separate");
            let (other, _other) = admit_under(ModeConflict::Reject, None, MODE, Arc::default());
            assert_eq!(
                verdict(other),
                "reject: host busy: streaming 1920x1080@60 to gs"
            );
        });
    }

    /// A reconnect at the cap is admitted when its own kept display is what fills it: that
    /// display is the one its create takes back. A third client is still refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_owners_kept_display_at_the_cap_is_admitted() {
        let name = "the_owners_kept_display_at_the_cap_is_admitted";
        crate::fake::in_child(module_path!(), name, &[], || {
            let doc = r#"{"keep_alive":{"mode":"forever"},"max_displays":2}"#;
            std::fs::write(pf_paths::config_dir().join("display-settings.json"), doc).unwrap();
            let display = |n: u8| {
                let fake = crate::fake::FakeDisplay::new("kwin");
                let slot = crate::identity::resolve_slot(
                    fp(n),
                    (MODE.0, MODE.1),
                    crate::policy::Identity::PerClient,
                );
                fake.script(|s| s.identity_slot = slot);
                let mode = crate::Mode {
                    width: MODE.0,
                    height: MODE.1,
                    refresh_hz: MODE.2,
                };
                crate::registry::acquire(&mut fake.boxed(), mode, Arc::default(), None).unwrap()
            };
            let other = display(2);
            let _live = register(
                fp(2),
                MODE,
                Arc::default(),
                "two".into(),
                Default::default(),
                None,
            );
            drop(display(1));
            let (own, _own) = admit(fp(1), (2560, 1440, 60), Arc::default());
            assert_eq!(verdict(own), "separate");
            let (third, _third) = admit(fp(3), MODE, Arc::default());
            assert!(verdict(third).starts_with("reject: host display budget exhausted: 2"));
            drop(other);
        });
    }
}
