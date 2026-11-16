//! Refcounted, session-scoped Linux suspend inhibition for live streams.
//!
//! The desktop is asked first: the Inhibit portal's `Suspend | Idle` vetoes its
//! idle suspend and blanking while any session lives, whether or not client
//! input reaches the desktop (a headless gamescope's never does). KDE and GNOME
//! back it, and neither refuses a deliberate Sleep.
//!
//! Without such a backend, after [`QUIET_BEFORE_VETO`] without client input the
//! host holds a logind `sleep:idle` block so an idle timer cannot suspend the
//! machine. [`note_input`] releases it synchronously: the same inhibitor would
//! refuse an intentional Sleep. Silence re-arms it. A local Sleep during a
//! passive stream is indistinguishable from idle there and stays blocked.
//! While seat0's active session is another user's, a greeter's too, neither input nor the
//! portal reaches its idle timer, so the block stands for the whole stream.
//!
//! A seat is the door's to hold: its input and portal stop at its own desktop, and its seatless
//! session may not take a logind block. The seat host asks for nothing. The door takes a share
//! while anyone plays on a seat, a block on `sleep` alone, so the desk's screen still goes dark.
//!
//! Acquisition is best-effort, shared by native and GameStream sessions, and
//! a no-op off Linux. The hold count's 0↔1 edges also drive the Windows
//! Instant Replay pause ([`crate::windows::instant_replay`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Quiet window before the host vetoes suspend. Under Steam's 5 min and KDE's
/// 10 min idle timers, and long enough that a Sleep confirmation cannot re-arm
/// mid-click.
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
const QUIET_BEFORE_VETO: Duration = Duration::from_secs(30);

/// [`watch`] poll interval. Only re-arm waits for a tick; [`note_input`] releases synchronously.
#[cfg(target_os = "linux")]
const WATCH_TICK: Duration = Duration::from_secs(5);

/// [`watch`] ticks between seat0 reads: a minute, well inside any desk's idle timer.
#[cfg(target_os = "linux")]
const DESK_TICKS: u64 = 12;

/// Inhibit portal answer budget. A silent portal still leaves the logind veto
/// armed inside [`QUIET_BEFORE_VETO`].
#[cfg(target_os = "linux")]
const PORTAL_REPLY: Duration = Duration::from_secs(10);

/// One RAII share of the host-wide inhibitor. Hold one per live session.
pub struct StreamHold(());

struct State {
    count: u32,
    /// True while [`watch`] is running. Exit is the 1→0 edge; without this flag a
    /// session that ends and restarts inside one tick would spawn a second watcher.
    #[cfg(target_os = "linux")]
    watching: bool,
    /// Logind inhibitor pipe. Inhibition lasts exactly as long as this fd stays open.
    #[cfg(target_os = "linux")]
    fd: Option<ashpd::zbus::zvariant::OwnedFd>,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(State {
            count: 0,
            #[cfg(target_os = "linux")]
            watching: false,
            #[cfg(target_os = "linux")]
            fd: None,
        })
    })
}

/// `Instant` is too fat for one relaxed store on the input path.
fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

static LAST_INPUT_MS: AtomicU64 = AtomicU64::new(0);
/// Set while a veto that input lifts stands. One relaxed load per input event keeps
/// [`note_input`] off the mutex.
static VETOING: AtomicBool = AtomicBool::new(false);

#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn quiet_for(last_input_ms: u64, now_ms: u64) -> bool {
    now_ms.saturating_sub(last_input_ms) >= QUIET_BEFORE_VETO.as_millis() as u64
}

/// Whether client input lifts the logind veto. `desk_uid` owns seat0's active session, `None`
/// while nobody is active there or logind doesn't answer. Input reaches only `own_uid`'s
/// compositors, so another user's desk, a greeter's included, never sees it and its idle timer
/// runs out under the stream. A Sleep chosen inside the stream is then refused too; the host's
/// own Sleep releases first ([`release_now`]).
#[cfg_attr(not(target_os = "linux"), allow(dead_code, reason = "Linux only"))]
fn input_lifts_veto(desk_uid: Option<u32>, own_uid: u32) -> bool {
    desk_uid.is_none_or(|uid| uid == own_uid)
}

/// Stamp last client input. Releases a veto that input lifts, so an intentional Sleep is
/// not refused by our own block.
pub fn note_input() {
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    if VETOING.load(Ordering::Relaxed) {
        release("the client is sending input again — a deliberate suspend now reaches logind");
    }
}

/// Take a share. Nothing is acquired here: [`watch`] takes the desktop's inhibit or the
/// logind block. A seat's session has neither to give, so its host only counts.
pub fn hold() -> StreamHold {
    // Seed the quiet clock so a connect never vetoes and costs no D-Bus round trip.
    LAST_INPUT_MS.store(now_ms(), Ordering::Relaxed);
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    st.count += 1;
    #[cfg(windows)]
    if st.count == 1 {
        crate::windows::instant_replay::on_stream_start();
    }
    #[cfg(target_os = "linux")]
    if !st.watching && !pf_paths::seat::role().runs_seat_session() {
        st.watching = true;
        drop(st);
        if let Err(e) = std::thread::Builder::new()
            .name("punktfunk-sleep-veto".into())
            .spawn(watch)
        {
            state().lock().unwrap_or_else(|e| e.into_inner()).watching = false;
            tracing::warn!(
                error = %e,
                "could not start the sleep-veto watcher — the box may auto-suspend under a \
                 passive (video-only) viewer"
            );
        }
    }
    StreamHold(())
}

impl Drop for StreamHold {
    fn drop(&mut self) {
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        st.count = st.count.saturating_sub(1);
        #[cfg(windows)]
        if st.count == 0 {
            crate::windows::instant_replay::on_stream_end();
        }
        #[cfg(target_os = "linux")]
        if st.count == 0 {
            release_locked(&mut st, "no live sessions");
        }
    }
}

/// Drop a standing veto. Closing the fd is enough — no D-Bus, so the input path can call this.
#[cfg(target_os = "linux")]
fn release(why: &str) {
    let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
    release_locked(&mut st, why);
}

#[cfg(not(target_os = "linux"))]
fn release(_why: &str) {}

/// Drop a standing veto before an explicit `power.sleep` (`design/host-actions.md`).
/// We never hold `-ignore-inhibit`, so our own block would refuse Sleep. Session
/// Drop also releases, but only on the next stop-flag check; this is the sync
/// path so `Suspend()` cannot race a still-open fd.
pub fn release_now() {
    release("a host power action is suspending/stopping this machine");
}

#[cfg(target_os = "linux")]
fn release_locked(st: &mut State, why: &str) {
    if st.fd.take().is_some() {
        VETOING.store(false, Ordering::Relaxed);
        tracing::info!(why, "released the sleep/idle inhibitor");
    }
}

/// Hold the desktop's inhibit while any session lives. Without one, own the
/// logind veto's arm/disarm edges. A veto input cannot lift stands from the first tick: the
/// door's `sleep` veto, as it has no desktop and sees no input, and the one against another
/// user's desk, which is read again every [`DESK_TICKS`].
///
/// Acquire is the expensive edge (thread spawn + D-Bus). It happens here,
/// outside the lock, so a hot `note_input` never queues behind a logind call.
/// A refusal stands for this watcher; the next stream asks again.
#[cfg(target_os = "linux")]
fn watch() {
    let door = pf_paths::seat::role().is_door();
    // SAFETY: a parameterless POSIX call that always succeeds and touches no memory.
    let own = unsafe { libc::getuid() };
    let lifts = || !door && input_lifts_veto(desk_uid(), own);
    let mut input_lifts = lifts();
    // The portal's inhibit lands in our own session, so another user's desk ignores it too.
    if input_lifts && hold_desktop_inhibit() {
        return;
    }
    let what = if door { "sleep" } else { "sleep:idle" };
    let mut refused = false;
    for tick in 1u64.. {
        std::thread::sleep(WATCH_TICK);
        if tick % DESK_TICKS == 0 {
            input_lifts = lifts();
        }
        let quiet = || !input_lifts || quiet_for(LAST_INPUT_MS.load(Ordering::Relaxed), now_ms());
        {
            let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
            if st.count == 0 {
                st.watching = false; // the last `Drop` already released the fd
                return;
            }
            if st.fd.is_some() {
                VETOING.store(input_lifts, Ordering::Relaxed);
                // Input can land after the quiet re-check below and before `VETOING`
                // is stored; that press would not call `release`, so catch it here.
                if !quiet() {
                    release_locked(&mut st, "the client is sending input again");
                }
                continue;
            }
            if !quiet() {
                continue;
            }
        }
        if refused {
            continue; // the next stream's watcher asks logind again
        }
        let Some(fd) = acquire(what) else {
            refused = true; // no logind / refused, and `acquire` said so
            continue;
        };
        let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
        if st.count == 0 || !quiet() {
            drop(fd); // raced: stream ended or viewer returned — do not store the fd
            continue;
        }
        st.fd = Some(fd);
        VETOING.store(input_lifts, Ordering::Relaxed);
    }
}

/// The uid that owns seat0's active session. `None` while nobody is active there or logind
/// doesn't answer. Blocks on its own runtime, so only [`watch`]'s plain thread calls it.
#[cfg(target_os = "linux")]
fn desk_uid() -> Option<u32> {
    use ashpd::zbus::{
        self,
        zvariant::{OwnedObjectPath, OwnedValue},
    };
    async fn get(
        conn: &zbus::Connection,
        path: &str,
        iface: &str,
        name: &str,
    ) -> Option<OwnedValue> {
        let reply = conn
            .call_method(
                Some("org.freedesktop.login1"),
                path,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &(iface, name),
            )
            .await
            .ok()?;
        reply.body().deserialize().ok()
    }
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let conn = zbus::Connection::system().await.ok()?;
        let seat = "/org/freedesktop/login1/seat/seat0";
        let active = get(&conn, seat, "org.freedesktop.login1.Seat", "ActiveSession").await?;
        let (id, session): (String, OwnedObjectPath) = active.try_into().ok()?;
        if id.is_empty() {
            return None;
        }
        let user = get(
            &conn,
            session.as_str(),
            "org.freedesktop.login1.Session",
            "User",
        )
        .await?;
        let (uid, _): (u32, OwnedObjectPath) = user.try_into().ok()?;
        Some(uid)
    })
}

/// Veto the desktop's idle suspend and blanking through the Inhibit portal until the
/// last session ends. False when no backend takes `Suspend`: GTK's without a GNOME
/// session refuses it. The connection is private, so the portal drops the inhibit
/// when the hold ends or this process dies.
#[cfg(target_os = "linux")]
fn hold_desktop_inhibit() -> bool {
    use ashpd::desktop::inhibit::{InhibitFlags, InhibitOptions, InhibitProxy};
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return false;
    };
    // One `block_on` for the whole hold keeps the connection's reader pumped.
    rt.block_on(async {
        let take = async {
            let conn = pf_portal::session_bus().await?;
            InhibitProxy::with_connection(conn)
                .await?
                .inhibit(
                    None,
                    InhibitFlags::Suspend | InhibitFlags::Idle,
                    InhibitOptions::default().set_reason("a client is streaming"),
                )
                .await
        };
        let request = match tokio::time::timeout(PORTAL_REPLY, take).await {
            Ok(Ok(request)) => request,
            Ok(Err(e)) => {
                tracing::info!(error = %e, "desktop suspend inhibit unavailable, using logind");
                return false;
            }
            Err(_) => {
                tracing::info!("desktop suspend inhibit timed out, using logind");
                return false;
            }
        };
        tracing::info!("holding the desktop's suspend inhibit while a client streams");
        loop {
            tokio::time::sleep(WATCH_TICK).await;
            let mut st = state().lock().unwrap_or_else(|e| e.into_inner());
            if st.count == 0 {
                st.watching = false;
                break;
            }
        }
        if let Err(e) = request.close().await {
            tracing::warn!(error = %e, "desktop suspend inhibit did not close");
        }
        tracing::info!("released the desktop's suspend inhibit");
        true
    })
}

/// One logind `Inhibit` of `what` on a dedicated plain thread. zbus's blocking API
/// panics on a tokio worker (`block_on` nested), and [`hold`] callers may be either.
/// The join waits the D-Bus round-trip (~ms); every call site tolerates that.
#[cfg(target_os = "linux")]
fn acquire(what: &'static str) -> Option<ashpd::zbus::zvariant::OwnedFd> {
    let fd = std::thread::spawn(move || -> Option<ashpd::zbus::zvariant::OwnedFd> {
        use ashpd::zbus;
        // ashpd disables zbus's blocking API. Drive the async API on a private
        // current-thread runtime, still on this plain thread (see fn doc).
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        let attempt: zbus::Result<zbus::zvariant::OwnedFd> = rt.block_on(async {
            let conn = zbus::Connection::system().await?;
            let reply = conn
                .call_method(
                    Some("org.freedesktop.login1"),
                    "/org/freedesktop/login1",
                    Some("org.freedesktop.login1.Manager"),
                    "Inhibit",
                    &(what, "Punktfunk", "a client is streaming", "block"),
                )
                .await?;
            reply.body().deserialize()
        });
        match attempt {
            Ok(fd) => Some(fd),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    what,
                    "logind inhibitor refused — the box may auto-suspend under this stream"
                );
                None
            }
        }
    })
    .join()
    .ok()
    .flatten();
    if fd.is_some() {
        tracing::info!(what, "holding a logind inhibitor while a client streams");
    }
    fd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_driven_stream_is_never_vetoed_but_a_quiet_one_is() {
        let quiet_ms = QUIET_BEFORE_VETO.as_millis() as u64;
        assert!(!quiet_for(1_000, 1_000), "input this instant is not quiet");
        assert!(
            !quiet_for(1_000, 1_000 + quiet_ms - 1),
            "one ms short of the window still counts as driven"
        );
        assert!(
            quiet_for(1_000, 1_000 + quiet_ms),
            "the window elapsed — a passive viewer gets the veto"
        );
        // The clock starts at zero, so an unstamped stream looks infinitely quiet.
        // `hold` seeds it so a connect never vetoes before anyone could have pressed.
        assert!(quiet_for(0, quiet_ms), "an unseeded clock reads as quiet");
    }

    #[test]
    fn input_lifts_the_veto_only_under_our_own_desk() {
        let own = 1000;
        for (desk, lifts, why) in [
            (None, true, "nobody at seat0, or no answer"),
            (Some(own), true, "our desktop sees the input"),
            (Some(42), false, "a greeter never sees it"),
            (Some(1001), false, "nor another user's desktop"),
            (Some(0), false, "nor root's"),
        ] {
            assert_eq!(input_lifts_veto(desk, own), lifts, "{why}");
        }
    }

    #[test]
    fn note_input_stamps_the_clock() {
        note_input();
        let stamped = LAST_INPUT_MS.load(Ordering::Relaxed);
        assert!(
            !quiet_for(stamped, now_ms()),
            "input just arrived — the veto must not be armable"
        );
    }
}
