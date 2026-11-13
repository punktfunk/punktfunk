//! Following a mid-stream Gaming↔Desktop flip.
//!
//! Bazzite and SteamOS switch session under a live stream, which changes the compositor and the
//! capture backend. The watcher only signals ([`SessionSwitch`]); the env is applied on the
//! encode thread, so nothing here ever `setenv`s. A Linux seat switches by itself and signals
//! the same way ([`SessionSwitch::seat`]).

use super::*;

/// Mid-stream Gaming↔Desktop flip. Env is applied on the encode thread — the watcher never `setenv`s.
pub(super) struct SessionSwitch {
    pub(super) kind: crate::vdisplay::ActiveKind,
    pub(super) compositor: crate::vdisplay::Compositor,
    /// The new session's env. `None` on a seat: its desktop keeps running under Game Mode.
    pub(super) env: Option<crate::vdisplay::SessionEnv>,
    /// What the new display starts. Only a seat entering Game Mode starts anything.
    pub(super) launch: Option<String>,
}

impl SessionSwitch {
    /// A seat's switch (`seats/session_switch.rs`): its own gamescope with Steam, or its desktop.
    #[cfg(target_os = "linux")]
    pub(super) fn seat(game: bool) -> Self {
        use crate::vdisplay::{ActiveKind, Compositor};
        SessionSwitch {
            kind: if game {
                ActiveKind::Gaming
            } else {
                ActiveKind::DesktopKde
            },
            compositor: if game {
                Compositor::Gamescope
            } else {
                Compositor::Kwin
            },
            env: None,
            launch: game
                .then(crate::library::game_mode_launch)
                .flatten()
                .and_then(|t| t.command),
        }
    }
}

/// The session's `PUNKTFUNK_SESSION_WATCH` (`row`) on/off wins. Auto is on where the box has a
/// Game Mode to flip to mid-stream, and off elsewhere.
pub(super) fn session_watch_enabled(row: Option<bool>) -> bool {
    row.unwrap_or_else(has_game_mode)
}

/// Bazzite, SteamOS, or any box with a gamescope session (Nobara). Only the watcher removes the
/// takeover's idle drop-in when a capture-loss rebuild reaches the desktop first.
fn has_game_mode() -> bool {
    let os = pf_host_config::os_release::os_release();
    os.is("bazzite") || os.is("steamos") || crate::vdisplay::managed_session_available()
}

pub(super) fn session_watcher_loop(
    tx: std::sync::mpsc::Sender<SessionSwitch>,
    stop: Arc<AtomicBool>,
) {
    use crate::vdisplay;
    const DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(3);
    let mut current = vdisplay::detect_active_session().kind;
    let mut pending: Option<(vdisplay::ActiveKind, std::time::Instant)> = None;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let active = vdisplay::detect_active_session();
        // Kind change OR same-kind restart: bump the epoch even when no SessionSwitch will fire.
        vdisplay::observe_session_instance(&active);
        let cur = active.kind;
        if cur == current {
            pending = None;
            continue;
        }
        match pending {
            Some((k, since)) if k == cur && since.elapsed() >= DEBOUNCE => {
                match vdisplay::compositor_for_kind(cur) {
                    Some(comp) => {
                        tracing::info!(from = ?current, to = ?cur, compositor = comp.id(),
                            "session watcher: mid-stream switch — signaling backend rebuild");
                        if tx
                            .send(SessionSwitch {
                                kind: cur,
                                compositor: comp,
                                env: Some(active.env),
                                launch: None,
                            })
                            .is_err()
                        {
                            break;
                        }
                        current = cur;
                    }
                    None => tracing::debug!(to = ?cur,
                        "session watcher: no usable backend for the new session — staying put"),
                }
                // After the signal: it waits out our Steam quitting. Also for a switch we cannot
                // follow, or "Return to Gaming Mode" meets a masked autologin unit.
                vdisplay::release_autologin_mask(cur);
                pending = None;
            }
            Some((k, _)) if k == cur => {}
            _ => pending = Some((cur, std::time::Instant::now())),
        }
    }
}

/// The loop's end of following the HDR state of what its gamescope shows. gamescope
/// composites every app into the range its capture negotiated, so SDR content on an HDR stream
/// is SDR mapped into PQ: the watcher looks at the content, and the loop reopens the capture
/// in its range ([`StreamState::on_content`]).
#[cfg(target_os = "linux")]
pub(super) struct ContentFollow {
    rx: std::sync::mpsc::Receiver<bool>,
    /// The gamescope the watcher looks at: its seat, `None` inside for the box's own. `None`
    /// idles the watcher.
    target: ContentTarget,
    /// What `target` was set from: whether the stream follows content, and on which display.
    pointed: (bool, Option<u64>),
    _watcher: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "linux")]
type ContentTarget = Arc<std::sync::Mutex<Option<Option<String>>>>;

#[cfg(target_os = "linux")]
impl ContentFollow {
    pub(super) fn spawn(stop: Arc<AtomicBool>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let target = ContentTarget::default();
        let seen = target.clone();
        let watcher = std::thread::Builder::new()
            .name("punktfunk1-content".into())
            .spawn(move || content_watcher_loop(tx, seen, stop))
            .ok();
        ContentFollow {
            rx,
            target,
            pointed: (false, None),
            _watcher: watcher,
        }
    }

    /// Point the watcher at the gamescope behind `display_gen` while the stream `follows` its
    /// content.
    pub(super) fn point(&mut self, follows: bool, display_gen: Option<u64>) {
        if self.pointed == (follows, display_gen) {
            return;
        }
        self.pointed = (follows, display_gen);
        let seat = follows.then(|| display_gen.and_then(crate::vdisplay::registry::seat_for));
        *self.target.lock().unwrap_or_else(|e| e.into_inner()) = seat;
    }

    /// The range the stream moves to: the watcher's newest look, where it is not what is
    /// `streamed`.
    pub(super) fn wants(&mut self, streamed: bool) -> Option<bool> {
        self.rx.try_iter().last().filter(|&hdr| hdr != streamed)
    }
}

/// Look at `target`'s content once a second and send every look: the loop asks again by
/// itself while a shared display holds it back.
#[cfg(target_os = "linux")]
fn content_watcher_loop(
    tx: std::sync::mpsc::Sender<bool>,
    target: ContentTarget,
    stop: Arc<AtomicBool>,
) {
    let mut content = pf_vdisplay::GamescopeHdrContent::default();
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let Some(seat) = target.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            continue;
        };
        if let Some(hdr) = content.poll(seat.as_deref()) {
            if tx.send(hdr).is_err() {
                break;
            }
        }
    }
}
