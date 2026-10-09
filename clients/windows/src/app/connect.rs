//! The trust gate and session lifecycle glue: `initiate` routes a connect through the shared
//! `trust_route` (pinned → silent, `pair=optional` → TOFU, otherwise → PIN), `connect_with`
//! starts the session worker and drives navigation from its events, and the "request access"
//! (delegated-approval) flow parks an identified connect until the operator approves it.

use super::lucide;
use super::profiles;
use super::style::*;
use super::{AppCtx, Screen, Svc, Target};
use crate::trust::{self, KnownHosts};
use pf_client_core::orchestrate::{
    self, trust_route, wake_parked_line, CancelHandle, ConnectOutcome, ExitRoute, HostTarget,
    TrustRoute, WakeOutcome, WakeWait, FINGERPRINT_CHANGED, WAKE_PARKED_HINT,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows_reactor::*;

/// A tile's plain connect, through the trust gate in [`initiate_opts`].
pub(crate) fn initiate(
    ctx: &Arc<AppCtx>,
    target: Target,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    initiate_opts(ctx, target, None, set_screen, set_status, false)
}

/// Dial-first for a saved host that isn't advertising but has a known MAC: the magic packet goes
/// out and the dial starts at once. mDNS absence is not unreachable: a host on a routed network
/// (Tailscale, VPN, another subnet) never advertises. With auto-wake on, read once here, a
/// failed dial falls into the visible [`wake_and_connect`] wait.
pub(crate) fn initiate_waking(
    ctx: &Arc<AppCtx>,
    target: Target,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    let wake = send_wake(ctx, &target);
    initiate_opts(ctx, target, None, set_screen, set_status, wake)
}

/// The dial-first magic packet, when auto-wake is on. `true` arms the wake fallback.
fn send_wake(ctx: &Arc<AppCtx>, target: &Target) -> bool {
    let on = ctx.settings.lock().unwrap().auto_wake;
    if on {
        crate::wol::wake(&target.host.mac, target.host.addr.parse().ok());
    }
    on
}

/// Opens the surface [`trust_route`] picks: the stored pin dials, a changed fingerprint or an
/// unpaired host gets [`Screen::Pair`], and a new `pair=optional` host is pinned on its first
/// connect (this shell has no TOFU confirmation screen). `launch` rides the connect.
fn initiate_opts(
    ctx: &Arc<AppCtx>,
    target: Target,
    launch: Option<String>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    wake_on_fail: bool,
) {
    // Every route reads the target back for its screen copy ("Connecting to X",
    // "Streaming to X") — stash it up front, not just on the pairing route.
    *ctx.shared.target.lock().unwrap() = target.clone();
    let known = KnownHosts::load();
    let fp = target.host.fp_hex.as_deref();
    let pin = match trust_route(
        &known,
        fp,
        &target.host.addr,
        target.host.port,
        target.pair_optional,
    ) {
        TrustRoute::Pinned(fp_hex) => trust::parse_hex32(&fp_hex),
        TrustRoute::OfferTofu(_) => None,
        TrustRoute::FingerprintChanged => {
            set_status.call(FINGERPRINT_CHANGED.to_string());
            set_screen.call(Screen::Pair);
            return;
        }
        TrustRoute::NeedsPairing => {
            set_screen.call(Screen::Pair);
            return;
        }
    };
    let opts = ConnectOpts {
        launch,
        wake_on_fail,
        ..ConnectOpts::default()
    };
    ask_then_connect(ctx, target, pin, set_screen, set_status, opts);
}

/// A paired host is asked who plays first; its pick (or the link's `as=`) rides the connect.
/// Any other host dials at once, with the link's `as=`.
fn ask_then_connect(
    ctx: &Arc<AppCtx>,
    target: Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    mut opts: ConnectOpts,
) {
    let fp = pin
        .map(|p| trust::hex(&p))
        .or_else(|| target.host.fp_hex.clone());
    let saved = KnownHosts::load()
        .resolve(fp.as_deref(), &target.host.addr, target.host.port)
        .filter(|h| h.paired)
        .map(|h| h.profile.clone());
    let Some(saved) = saved else {
        opts.profile = target.link_profile.clone();
        // `None` is TOFU: the spawn pins the advertised fingerprint.
        return connect_with(ctx, &target, pin, set_screen, set_status, opts);
    };
    let (ctx2, t, ss, st) = (
        ctx.clone(),
        target.clone(),
        set_screen.clone(),
        set_status.clone(),
    );
    profiles::then_connect(
        ctx,
        target,
        saved,
        pin,
        set_screen,
        set_status,
        move |profile| {
            let opts = ConnectOpts { profile, ..opts };
            connect_with(&ctx2, &t, pin, &ss, &st, opts)
        },
    );
}

/// The host refused `id` as unknown. A seat host says so for a seat that went stale too, so its
/// list is read again: while it still lists the profile, one more dial goes through the seat
/// gate. Otherwise `give_up` shows the refusal.
fn redial_profile(
    ctx: &Arc<AppCtx>,
    target: Target,
    pin: Option<[u8; 32]>,
    id: String,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    give_up: impl FnOnce() + Send + 'static,
) {
    let (ctx, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let _ = std::thread::Builder::new()
        .name("pf-profiles".into())
        .spawn(move || {
            let Some(row) = profiles::relisted(&ctx, &target, pin, &id) else {
                return give_up();
            };
            let (ctx2, t, ss2, st2, id) = (
                ctx.clone(),
                target.clone(),
                ss.clone(),
                st.clone(),
                row.id.clone(),
            );
            profiles::seat_then(&ctx, target, pin, &ss, &st, Some(row), move || {
                let opts = ConnectOpts {
                    profile: Some(id),
                    retried: true,
                    ..ConnectOpts::default()
                };
                connect_with(&ctx2, &t, pin, &ss2, &st2, opts)
            });
        });
}

/// Start a stream that launches a library title on connect (`--launch id`): the library page's
/// tap-to-play and a deep link's `launch=`. Same trust gate as [`initiate`], so a host forgotten
/// mid-visit routes to the PIN ceremony.
pub(crate) fn initiate_launch(
    ctx: &Arc<AppCtx>,
    target: Target,
    launch: String,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    initiate_opts(ctx, target, Some(launch), set_screen, set_status, false)
}

/// [`initiate_launch`] with the dial-first wake of [`initiate_waking`] — a deep link's
/// `launch=` toward a saved host that isn't advertising but has a known MAC.
pub(crate) fn initiate_launch_waking(
    ctx: &Arc<AppCtx>,
    target: Target,
    launch: String,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    let wake = send_wake(ctx, &target);
    initiate_opts(ctx, target, Some(launch), set_screen, set_status, wake)
}

/// Tunables that differ between the normal connect and the no-PIN "request access" flow.
/// `Default` is the normal connect: short handshake budget, persist *unpaired* on TOFU, and the
/// plain "Connecting" screen.
pub(crate) struct ConnectOpts {
    /// Handshake budget. Request-access uses a long one because the host PARKS the connection
    /// until the operator clicks Approve in its console (see the host's `PENDING_APPROVAL_WAIT`).
    connect_timeout: Duration,
    /// Persist the host as *paired* on a successful connect. Set for request-access, where the
    /// operator's approval IS the pairing, so future connects are silent (rule 1). Normal TOFU
    /// persists the host *unpaired* (pinned, but not PIN/approval-verified).
    persist_paired: bool,
    /// Show the cancelable "waiting for approval" screen instead of "Connecting" (request-access).
    awaiting_approval: bool,
    /// Set by the waiting screen's Cancel button. `NativeClient::connect` is blocking with no
    /// abort, so Cancel returns the UI immediately and leaves the parked connect to resolve/time
    /// out; this request's event loop (which captured the same `Arc` at spawn) then tears down
    /// silently when the parked connect finally resolves — without touching a screen a new
    /// session may already own.
    cancel: Option<Arc<AtomicBool>>,
    /// Fall into the Wake-on-LAN wait ([`wake_and_connect`]) when THIS dial fails with a plain
    /// connect failure (not a trust rejection). Set by the dial-first path, with auto-wake on,
    /// for a saved host that isn't advertising but has a known MAC. The wait's own redial
    /// clears the flag, so it can't loop.
    wake_on_fail: bool,
    /// A library title id (`steam:570`, …) the host launches during the connect handshake —
    /// the library page's tap-to-play, passed to the spawned session child as `--launch`.
    launch: Option<String>,
    /// The profile id the session names: the picker's answer, or a link's `as=`. `None` names none.
    profile: Option<String>,
    /// This dial is the one retry after the host refused the profile as unknown.
    retried: bool,
}

impl Default for ConnectOpts {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(15),
            persist_paired: false,
            awaiting_approval: false,
            cancel: None,
            wake_on_fail: false,
            launch: None,
            profile: None,
            retried: false,
        }
    }
}

/// A fresh pairing's first connect: it asks who plays, as any connect to a paired host does.
pub(crate) fn connect(
    ctx: &Arc<AppCtx>,
    target: &Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    ask_then_connect(
        ctx,
        target.clone(),
        pin,
        set_screen,
        set_status,
        ConnectOpts::default(),
    );
}

fn connect_with(
    ctx: &Arc<AppCtx>,
    target: &Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    opts: ConnectOpts,
) {
    // Session-always: every stream runs in the spawned punktfunk-session Vulkan binary.
    connect_spawn(ctx, target, pin, set_screen, set_status, opts)
}

/// Spawn-mode connect: run the stream in the punktfunk-session binary and turn its events
/// into screens. The child never connects unpinned: a stored or ceremony pin, else the
/// advertised fingerprint (TOFU, saved once ready proves the host holds it); no fingerprint
/// routes to the PIN ceremony. Its exit goes where [`orchestrate::exit_route`] says.
fn connect_spawn(
    ctx: &Arc<AppCtx>,
    target: &Target,
    pin: Option<[u8; 32]>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
    opts: ConnectOpts,
) {
    let tofu = pin.is_none();
    let fp_hex = pin.map(|p| trust::hex(&p)).or_else(|| {
        target
            .host
            .fp_hex
            .clone()
            .filter(|f| trust::parse_hex32(f).is_some())
    });
    let Some(fp_hex) = fp_hex else {
        *ctx.shared.target.lock().unwrap() = target.clone();
        set_screen.call(Screen::Pair);
        return;
    };

    // A fresh child slot per spawn, installed where Disconnect/Cancel can reach it.
    let child = CancelHandle::default();
    *ctx.shared.session.lock().unwrap() = child.clone();
    *ctx.shared.stats.lock().unwrap() = None;
    ctx.shared.browse.store(false, Ordering::SeqCst);
    set_status.call(String::new());
    set_screen.call(if opts.awaiting_approval {
        Screen::RequestAccess
    } else {
        Screen::Connecting
    });

    let persist_paired = opts.persist_paired;
    let cancel = opts.cancel;
    let mut wake_on_fail = opts.wake_on_fail;
    let ctx2 = ctx.clone();
    let shared = ctx.shared.clone();
    let (ss, st) = (set_screen.clone(), set_status.clone());
    let target = target.clone();
    // The closure owns `target`/`fp_hex`; the call takes copies, the pin it dials included.
    let host = HostTarget {
        fp_hex: Some(fp_hex.clone()),
        ..target.host.clone()
    };
    let preset_arg = target.preset.clone();
    let profile_arg = opts.profile.clone();
    // The launch id: an explicit opts pick (the library's tap-to-play), else one riding
    // the target — a deep link's `launch=` that detoured through the PIN ceremony.
    let launch_arg = opts.launch.clone().or_else(|| target.launch.clone());
    // What a `PROFILE_UNKNOWN` refusal may dial again: never the retry itself.
    let (retry, retry_launch) = (
        opts.profile.clone().filter(|_| !opts.retried),
        launch_arg.clone(),
    );
    let spawned = crate::spawn::spawn_session(
        host.clone(),
        opts.connect_timeout.as_secs(),
        launch_arg.as_deref(),
        preset_arg.as_deref(),
        profile_arg.as_deref(),
        child,
        move |event| {
            use crate::spawn::SpawnEvent;
            // The child is gone — bring the shell back BEFORE the cancel gate below, so a
            // Ready that raced a Cancel (and hid the shell) can never strand it hidden.
            if matches!(event, SpawnEvent::Exited { .. }) {
                crate::shell_window::restore();
            }
            // A cancelled request-access connect that resolved late: tear down silently —
            // Cancel already killed the child and returned the UI to the host list.
            if cancel.as_ref().is_some_and(|c| c.load(Ordering::SeqCst)) {
                return;
            }
            match event {
                SpawnEvent::Ready => {
                    // Ready proves the host answered, so no later exit is the asleep case.
                    wake_on_fail = false;
                    // Request access saves the host PAIRED, plain TOFU pinned but unpaired. A
                    // failed save waits on the status line, which a clean exit leaves for the
                    // host list.
                    if (persist_paired || tofu)
                        && let Err(e) =
                            orchestrate::persist_on_ready(&target.host, &fp_hex, persist_paired)
                    {
                        st.call(format!("Connected, but couldn't save — {e:#}"));
                    }
                    // The child presented its first frame — its window is up, so the
                    // shell yields: one visible Punktfunk window at a time. Every exit
                    // path restores it (the `Exited` handling above).
                    crate::shell_window::hide();
                    ss.call(Screen::Stream);
                }
                SpawnEvent::Stats(s) => *shared.stats.lock().unwrap() = Some(*s),
                SpawnEvent::Exited(outcome) => {
                    let forget_then = |msg: String| {
                        let (host, ss, st) = (host.clone(), ss.clone(), st.clone());
                        move || {
                            profiles::save_pick(&host, None);
                            st.call(msg);
                            ss.call(Screen::Hosts);
                        }
                    };
                    let log = crate::spawn::log_hint();
                    match orchestrate::exit_route(
                        &outcome,
                        tofu,
                        retry.as_deref(),
                        wake_on_fail,
                        &log,
                    ) {
                        // The user closed the stream window, or Disconnect killed it.
                        ExitRoute::Silent => ss.call(Screen::Hosts),
                        ExitRoute::Banner(msg) => {
                            st.call(msg);
                            ss.call(Screen::Hosts);
                        }
                        ExitRoute::Repair(msg) => {
                            st.call(msg);
                            *shared.target.lock().unwrap() = target.clone();
                            ss.call(Screen::Pair);
                        }
                        ExitRoute::RedialProfile { id, msg } => {
                            let target = Target {
                                launch: retry_launch.clone(),
                                ..target.clone()
                            };
                            let give_up = forget_then(msg);
                            redial_profile(&ctx2, target, pin, id, &ss, &st, give_up);
                        }
                        ExitRoute::ForgetProfileThen(msg) => forget_then(msg)(),
                        ExitRoute::Wake => wake_and_connect(&ctx2, target.clone(), &ss, &st),
                    }
                }
            }
        },
    );
    if let Err(e) = spawned {
        set_status.call(e);
        set_screen.call(Screen::Hosts);
    }
}

/// "Open console UI": run the console (`punktfunk-session --browse`) in the session window.
/// The shell yields exactly like a stream — hidden on the console window's `ready`, restored
/// when the child exits (launched titles stream in that same window, so the whole couch
/// round-trip happens without the shell).
pub(crate) fn open_console(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    let child = CancelHandle::default();
    *ctx.shared.session.lock().unwrap() = child.clone();
    *ctx.shared.stats.lock().unwrap() = None;
    ctx.shared.browse.store(true, Ordering::SeqCst);
    let fullscreen = ctx.settings.lock().unwrap().fullscreen_on_stream;
    set_status.call(String::new());
    set_screen.call(Screen::Connecting);

    let shared = ctx.shared.clone();
    let (ss, st) = (set_screen.clone(), set_status.clone());
    let spawned = crate::spawn::spawn_browse(fullscreen, child, move |event| {
        use crate::spawn::SpawnEvent;
        match event {
            SpawnEvent::Ready => {
                // The library window presented — the shell yields (same one-visible-
                // window rule as a stream).
                crate::shell_window::hide();
                ss.call(Screen::Stream);
            }
            SpawnEvent::Stats(s) => *shared.stats.lock().unwrap() = Some(*s),
            SpawnEvent::Exited(outcome) => {
                crate::shell_window::restore();
                // Quit from the library (B / closing the window) or Disconnect returns
                // silently; a failed start surfaces its error line, or the exit code when
                // it died without producing one.
                match outcome {
                    ConnectOutcome::TrustRejected(msg)
                    | ConnectOutcome::ConnectFailed(msg)
                    | ConnectOutcome::Refused { msg, .. }
                    | ConnectOutcome::Ended(Some(msg)) => st.call(msg),
                    ConnectOutcome::RendererFailed { code } => {
                        st.call(ConnectOutcome::died_banner(code, &crate::spawn::log_hint()))
                    }
                    ConnectOutcome::Ended(None) | ConnectOutcome::Cancelled => {}
                }
                ss.call(Screen::Hosts);
            }
        }
    });
    if let Err(e) = spawned {
        set_status.call(e);
        set_screen.call(Screen::Hosts);
    }
}

/// The no-PIN "request access" flow: open an identified connect that the host PARKS until the
/// operator approves this device in its console (or web UI), showing a cancelable "waiting"
/// screen meanwhile. On approval the SAME connection is admitted (no reconnect) and the host is
/// saved as paired, so later connects are silent.
pub(crate) fn request_access(props: &Svc, target: &Target) {
    let ctx = &props.ctx;
    // Pin the advertised certificate for a discovered host (defence against a host impostor while
    // we wait); a manually-typed host has no advertised fingerprint, so trust-on-first-use.
    let pin = target.host.fp_hex.as_deref().and_then(trust::parse_hex32);
    // A fresh cancel flag per request, installed where the waiting screen's Cancel button can read
    // it back; this request's event loop captures the same `Arc` (via ConnectOpts) below.
    let cancel = Arc::new(AtomicBool::new(false));
    *ctx.shared.cancel.lock().unwrap() = Some(cancel.clone());
    connect_with(
        ctx,
        target,
        pin,
        &props.set_screen,
        &props.set_status,
        ConnectOpts {
            // Must exceed the host's approval window (PENDING_APPROVAL_WAIT) so a slow operator
            // approval still lands on this connection rather than timing the client out first.
            connect_timeout: Duration::from_secs(185),
            persist_paired: true,
            awaiting_approval: true,
            cancel: Some(cancel),
            ..ConnectOpts::default()
        },
    );
}

/// The Wake-on-LAN "wait until up" flow: the FALLBACK after a failed dial-first attempt
/// ([`initiate_waking`]) to a non-advertising saved host with a MAC. A magic packet, a
/// cancelable "Waking…" screen, and mDNS polled until the host advertises — re-sending the
/// packet periodically — on a bounded deadline. On reappearance it dials the address the host
/// came back on; on timeout it parks on [`wake_parked_page`]; Cancel returns to the host list.
///
/// The cadence is [`WakeWait`] and the advert match `AdvertWatch`, both shared with the GTK
/// shell (design/client-architecture-split.md §3).
fn wake_and_connect(
    ctx: &Arc<AppCtx>,
    target: Target,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) {
    // The packets are the wait's business: `WakeWait` asks for one on its first tick and every
    // 6 s after (a single one can be missed, and some NICs only wake on a fresh packet after
    // dropping into a deeper sleep state).
    // A fresh cancel flag per wake, installed where the "Waking…" screen's Cancel button reads it
    // back (the same shared channel as the request-access flow); the poll loop checks the same `Arc`.
    let cancel = Arc::new(AtomicBool::new(false));
    *ctx.shared.cancel.lock().unwrap() = Some(cancel.clone());
    // The busy page reads the host name from the shared target.
    *ctx.shared.target.lock().unwrap() = target.clone();
    set_status.call(String::new());
    set_screen.call(Screen::Waking);

    let (ctx, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
    std::thread::spawn(move || {
        let mut adverts = pf_client_core::discovery::AdvertWatch::start();
        let mut wait = WakeWait::new();
        loop {
            // Cancel already returned the UI to the host list — stop re-sending and tear down.
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            let resolved = adverts.poll(
                target.host.fp_hex.as_deref(),
                &target.host.addr,
                target.host.port,
            );
            let tick = wait.tick(resolved.is_some());
            if tick.send_packet {
                crate::wol::wake(&target.host.mac, target.host.addr.parse().ok());
            }
            match tick.outcome {
                Some(WakeOutcome::Online) => {
                    let mut target = target.clone();
                    // Came back on a new IP (DHCP): dial the fresh address. The saved card
                    // moves only when the probe sweep hears its pin there — an advert's
                    // address can be another machine's.
                    if let Some((addr, port)) =
                        resolved.filter(|(a, p)| *a != target.host.addr || *p != target.host.port)
                    {
                        target.host.addr = addr;
                        target.host.port = port;
                    }
                    initiate(&ctx, target, &ss, &st);
                    return;
                }
                Some(WakeOutcome::TimedOut) => {
                    ss.call(Screen::WakeParked);
                    return;
                }
                None => {}
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

/// The plain "Connecting…" screen shown while the session worker handshakes. No hooks.
pub(crate) fn connecting_page(ctx: &Arc<AppCtx>, status: &str) -> Element {
    let target_name = ctx.shared.target.lock().unwrap().host.name.clone();
    let headline = if target_name.is_empty() {
        "Connecting\u{2026}".to_string()
    } else {
        format!("Connecting to {target_name}\u{2026}")
    };
    let detail = if status.is_empty() {
        "Negotiating the session and creating the virtual display\u{2026}"
    } else {
        status
    };
    busy_page(&headline, detail, Vec::new())
}

/// The cancelable "waiting for approval" screen (request-access flow): a spinner + guidance while
/// the identified connect sits parked on the host, plus a Cancel that returns to the host list and
/// trips the shared cancel flag so the parked connect tears down silently if it resolves after the
/// user has walked away. No hooks.
pub(crate) fn request_access_page(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
) -> Element {
    let target_name = ctx.shared.target.lock().unwrap().host.name.clone();
    let headline = if target_name.is_empty() {
        "Waiting for approval\u{2026}".to_string()
    } else {
        format!("Waiting for {target_name} to approve\u{2026}")
    };
    let cancel_btn = {
        let (ctx, ss) = (ctx.clone(), set_screen.clone());
        button("Cancel")
            .icon(lucide::icon("x"))
            .on_click(move || {
                // Return the UI immediately; trip the flag this request's event loop
                // captured so it tears down silently when the connect resolves (see
                // ConnectOpts::cancel). Killing the parked session child IS the abort.
                if let Some(c) = ctx.shared.cancel.lock().unwrap().as_ref() {
                    c.store(true, Ordering::SeqCst);
                }
                ctx.shared.session.lock().unwrap().kill();
                ss.call(Screen::Hosts);
            })
            .horizontal_alignment(HorizontalAlignment::Center)
    };
    busy_page(
        &headline,
        "Approve this device in the host's console or web UI \u{2014} it connects automatically \
         once you approve it. No PIN needed.",
        vec![cancel_btn.into()],
    )
}

/// The cancelable "Waking…" screen (Wake-on-LAN wait-until-up flow): a spinner + guidance while the
/// poll loop waits for the woken host to reappear on mDNS, plus a Cancel that returns to the host
/// list and trips the shared cancel flag so the poll loop stops re-sending and tears down. No hooks.
pub(crate) fn waking_page(ctx: &Arc<AppCtx>, set_screen: &AsyncSetState<Screen>) -> Element {
    let target_name = ctx.shared.target.lock().unwrap().host.name.clone();
    let headline = if target_name.is_empty() {
        "Waking the host\u{2026}".to_string()
    } else {
        format!("Waking {target_name}\u{2026}")
    };
    let cancel_btn = {
        let (ctx, ss) = (ctx.clone(), set_screen.clone());
        button("Cancel")
            .icon(lucide::icon("x"))
            .on_click(move || {
                // Return the UI immediately and trip the flag the poll loop is watching so it stops
                // re-sending and exits without touching a screen a later action may already own.
                if let Some(c) = ctx.shared.cancel.lock().unwrap().as_ref() {
                    c.store(true, Ordering::SeqCst);
                }
                ss.call(Screen::Hosts);
            })
            .horizontal_alignment(HorizontalAlignment::Center)
    };
    busy_page(
        &headline,
        "Sent a wake signal and waiting for the host to come online \u{2014} this can take up to a \
         minute for a sleeping or powered-off machine.",
        vec![cancel_btn.into()],
    )
}

/// The parked wake: the host didn't come online in budget. Try again is a fresh wake of the
/// shared target; Cancel returns to the host list. No hooks.
pub(crate) fn wake_parked_page(
    ctx: &Arc<AppCtx>,
    set_screen: &AsyncSetState<Screen>,
    set_status: &AsyncSetState<String>,
) -> Element {
    let target = ctx.shared.target.lock().unwrap().clone();
    let retry_btn = {
        let (ctx, ss, st, target) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            target.clone(),
        );
        button("Try again")
            .accent()
            .icon(lucide::icon("rotate-cw"))
            .on_click(move || wake_and_connect(&ctx, target.clone(), &ss, &st))
    };
    let cancel_btn = {
        let ss = set_screen.clone();
        button("Cancel")
            .icon(lucide::icon("x"))
            .on_click(move || ss.call(Screen::Hosts))
    };
    let actions = hstack((retry_btn, cancel_btn))
        .spacing(8.0)
        .horizontal_alignment(HorizontalAlignment::Center);
    notice_page(
        None,
        &wake_parked_line(&target.host.name),
        WAKE_PARKED_HINT,
        vec![actions.into()],
    )
}
