//! The connect pipeline: a press on a host or a game, the profile and seat checks before
//! the dial, the connecting card, and the launch hold until the game is up.

use super::{Shell, ToastKind};
use crate::anim::Spring;
use crate::model::{ConsoleCmd, ProfilesAnswer};
use crate::screens::{ConnectIntent, Nav, ProfileAsk, Screen, Seated};
use pf_client_core::console::OverlayAction;
use pf_client_core::profiles::{seat_gate, SeatGate};
use skia_safe::Rect;

/// A connect waiting on its box's profile list (§10.1) before it dials.
pub(super) struct Asking {
    pub(super) intent: ConnectIntent,
    pub(super) ask: ProfileAsk,
    pub(super) since: f64,
}

/// Seconds a connect waits for the profile list before it dials with the card's saved pick.
pub(super) const PROFILES_WAIT: f64 = 3.0;
/// A list slower than this puts the connect card up; a fast one shows nothing.
pub(super) const ASKING_CARD_AFTER: f64 = 0.25;

/// A connect waiting for its profile's seat to come up (§9.2). There is no timeout: Back is
/// the way out.
pub(super) struct SeatWait {
    pub(super) intent: ConnectIntent,
    pub(super) mgmt: u16,
    /// The profile's id and name, for the poll and the title.
    pub(super) id: String,
    pub(super) name: String,
    /// The progress line the host last gave.
    pub(super) detail: Option<String>,
    /// When the list was last asked for.
    pub(super) polled: f64,
    /// Takeover fade-in, 0 → 1.
    pub(super) appear: f64,
}

/// Seconds between asks for the profile list while a seat comes up.
pub(super) const SEAT_POLL: f64 = 2.0;

pub(super) struct Connecting {
    pub(super) title: String,
    pub(super) appear: f64,
    /// Host is parked pending operator approval. Takeover title is
    /// "Waiting for approval", not "Connecting".
    pub(super) request_access: bool,
}

/// Where the launch hold asks after its title: the shelf's host, on the
/// management lane the shelf already reads `/status` from.
pub(super) struct LaunchHost {
    pub(super) id: String,
    pub(super) addr: String,
    pub(super) mgmt: u16,
    pub(super) fp_hex: String,
}

/// One screen from the press to the game: the cover leaves its shelf tile, and
/// holds — through the dial, then over the stream — until the game is up.
///
/// Every launch begins with the launcher's own window (Steam booting, a
/// desktop) — the first thing a player used to see of a game. The host tells
/// `launching` from `running` per lease (`punktfunk-host::gamelease`), and a
/// paired client may read it, so the hold polls that until it changes.
///
/// Raised at the press rather than at the first frame, because the shelf is
/// only on screen then — it is the one moment the cover has somewhere to fly
/// FROM — and holding from there means the launcher is never seen at all.
/// Replaces the [`Connecting`] card for a game launch; the two would otherwise
/// be two takeovers for one act.
pub(super) struct Launching {
    pub(super) host: LaunchHost,
    pub(super) title: String,
    /// `PC · 2024 · Steam` — where the host filed it, in the order a player scans it.
    pub(super) facts: String,
    /// Studio, and the genres joined; either may be empty, and an older host sends neither.
    pub(super) developer: String,
    pub(super) genres: String,
    /// Backdrop and copy fade, 0 → 1.
    pub(super) appear: f64,
    /// The cover's flight out of its tile, 0 (tile) → 1 (settled).
    pub(super) flight: Spring,
    /// The tile it leaves, in the shell's layout space. Empty = no tile to
    /// leave (a keyboard launch off a culled row), so it arrives in place.
    pub(super) from: Rect,
    /// The dial landed. Before it, a press cancels the connect and there is no
    /// session to ask the host about; after it, a press shows the stream.
    pub(super) connected: bool,
    pub(super) since: f64,
    pub(super) last_poll: f64,
    /// `status_gen` when the hold began; a state read before that describes
    /// an earlier launch of the same title and must not end this one.
    pub(super) base_gen: u64,
    /// `status_gen` when the last poll went out — the next waits for it to move.
    pub(super) poll_gen: u64,
    /// The game is up and the host is waiting for its window.
    pub(super) window_wait: bool,
    /// The title's files, while the host fetches them before it opens the stream.
    pub(super) download: Option<pf_client_core::library::DownloadProgress>,
    /// Why the hold gave up, once it has. Latched: the hold holds the screen and says this
    /// instead of sliding away onto a desktop nobody asked for.
    pub(super) failed: Option<String>,
}

/// Why the hold is giving up, in one sentence, or `None` while it should keep waiting.
///
/// `state` is the host's own `games[]` word for this title, `None` when the host lists nothing
/// for it at all — which is what a refused launch looks like from here. The touch shell's
/// `launchGaveUp` says the same three sentences, so a report quotes one line whichever shell
/// it came from. `running`, `untracked` and `grace` keep waiting or reveal: those launches worked.
pub(super) fn launch_gave_up(title: &str, state: Option<&str>, elapsed: f64) -> Option<String> {
    match state {
        None if elapsed >= LAUNCH_NO_LEASE => Some(format!(
            "The host didn't start {title} — nothing is running for it."
        )),
        Some("launching") if elapsed >= LAUNCH_HOLD_MAX => {
            Some(format!("{title} is still starting after 2 minutes."))
        }
        Some("exited") => Some(format!("{title} closed right after starting.")),
        _ => None,
    }
}

/// Poll interval for the launch hold, and the retry when an answer never lands.
pub(super) const LAUNCH_POLL: f64 = 1.0;
pub(super) const LAUNCH_POLL_STALL: f64 = 5.0;
/// The host lists nothing for the title: the launch did not resolve
/// (no recipe, launcher missing). The host logs it and streams on; so do we.
pub(super) const LAUNCH_NO_LEASE: f64 = 15.0;
/// A game still `launching`, or `running` without its window, this long is one
/// the player wants to see for themselves — a cold Steam boot with shader work
/// runs to minutes, and the host waits five for it.
pub(super) const LAUNCH_HOLD_MAX: f64 = 120.0;

impl Shell {
    pub(crate) fn set_connecting(&mut self, title: Option<String>) {
        match title {
            Some(title) => {
                self.last_connect_title = Some(title.clone());
                self.connecting = Some(Connecting {
                    title,
                    appear: 0.0,
                    request_access: false,
                })
            }
            None => self.connecting = None,
        }
    }

    pub(crate) fn session_failed(&mut self, msg: &str) {
        self.connecting = None;
        self.launching = None;
        self.in_stream = false;
        self.reask = None;
        if let Some(intent) = self.reask_now.take() {
            self.reasking = true;
            return self.start_connect(intent);
        }
        self.show_toast_kind(format!("Couldn't connect — {msg}"), ToastKind::Error);
    }

    /// The box no longer has the profile the last connect named: forget the card's pick and
    /// ask its list once more. A second miss fails as any other refusal does.
    pub(crate) fn profile_gone(&mut self) {
        let Some(intent) = self.reask.take() else {
            return;
        };
        if let Some(ask) = &intent.ask {
            self.send_cmd(ConsoleCmd::SetProfile {
                key: ask.key.clone(),
                profile: None,
            });
        }
        self.reask_now = Some(intent);
    }

    pub(crate) fn session_streaming(&mut self) {
        self.connecting = None;
        self.reask = None;
        let t = self.t();
        let reads = self.library.status_gen();
        let Some(l) = &mut self.launching else {
            self.in_stream = true;
            return;
        };
        l.connected = true;
        // The host has no lease to report until the session that launched the title
        // exists, so the "never listed it" clock only starts making sense here — and a read
        // taken before it, while the files downloaded, is not this session's answer.
        l.since = t;
        l.base_gen = reads;
        l.last_poll = t - LAUNCH_POLL_STALL;
        l.download = None;
        self.in_stream = false;
    }

    /// The hold for a launched title, or `None` when there is nothing to wait for: a launcher
    /// tile (the host never tracks those), or a title the shelf no longer lists.
    ///
    /// Every platform, not just the desktop. The console is the launch screen wherever it is
    /// the launcher — a host that has a stream view of its own waits for
    /// [`OverlayAction::ShowStream`] before switching to it, rather than drawing a second
    /// launch screen of its own on top of this one.
    fn launch_hold(&self, host: LaunchHost, from: Rect) -> Option<Launching> {
        let snap = self.library.snapshot();
        let g = snap.games.iter().find(|g| g.id == host.id)?;
        if g.launcher {
            return None;
        }
        let year = g.year.map(|y| y.to_string());
        let facts = [
            g.platform.as_deref(),
            year.as_deref(),
            Some(crate::library::store_label(&g.store)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" \u{b7} ");
        let t = self.t();
        let reads = self.library.status_gen();
        Some(Launching {
            host,
            title: g.title.clone(),
            facts,
            developer: g.developer.clone().unwrap_or_default(),
            genres: g.genres.join(" \u{b7} "),
            appear: 0.0,
            flight: Spring::rest(0.0),
            from,
            connected: false,
            since: t,
            // The first poll goes out on the next frame.
            last_poll: t - LAUNCH_POLL_STALL,
            base_gen: reads,
            poll_gen: reads,
            window_wait: false,
            download: None,
            failed: None,
        })
    }

    /// Drop the hold and let the stream through.
    pub(super) fn reveal_stream(&mut self) {
        self.launching = None;
        self.in_stream = true;
        // Hosts that swap to a stream view of their own have been holding the session since
        // the dial landed; this is what releases it. A host that composites this console over
        // its stream ignores it.
        self.actions.push_back(OverlayAction::ShowStream);
    }

    /// One frame of the launch hold: reveal when the host has answered, or
    /// when it never will; else keep the poll going.
    pub(super) fn tick_launch(&mut self) {
        let t = self.t();
        let reads = self.library.status_gen();
        let Some(l) = &self.launching else { return };
        if l.failed.is_some() {
            return;
        }
        let fresh = reads > l.base_gen;
        let download = fresh
            .then(|| self.library.launch_download(&l.host.id))
            .flatten();
        // Before the dial lands the host may be fetching the title's files: the only thing to
        // read is how far they are. The lease is the SESSION's, so a title that was already up
        // would otherwise read as "running" and reveal a stream that does not exist.
        if !l.connected {
            if let Some(l) = &mut self.launching {
                l.download = download.filter(|d| d.live());
            }
            self.poll_launch(t, reads);
            return;
        }
        let state = fresh
            .then(|| self.library.launch_state(&l.host.id))
            .flatten();
        let elapsed = t - l.since;
        let window_wait = matches!(&state, Some((s, true)) if s == "running");
        let word = state.as_ref().map(|(s, _)| s.as_str());
        // A launch that produced no game ends with a sentence, not by sliding away: a bare
        // desktop reads the same whether the host refused it or the game is merely slow. A
        // download that stopped is that sentence, at once.
        let stopped = word
            .is_none()
            .then(|| download.and_then(|d| d.stopped(&l.title)))
            .flatten();
        if let Some(why) = stopped.or_else(|| launch_gave_up(&l.title, word, elapsed)) {
            if let Some(l) = &mut self.launching {
                l.failed = Some(why);
            }
            return;
        }
        let done = match word {
            // Both handled above, once they run out of patience.
            Some("launching") => false,
            // A Proton prefix or a splash can sit behind a running process for a minute.
            Some("running") if window_wait => elapsed >= LAUNCH_HOLD_MAX,
            // window, running, untracked, grace: the host has said all it will.
            Some(_) => true,
            None => false,
        };
        if done {
            self.reveal_stream();
            return;
        }
        self.poll_launch(t, reads);
        if let Some(l) = &mut self.launching {
            l.window_wait = window_wait;
        }
    }

    /// Ask the host again once the last answer landed, or once it is overdue.
    fn poll_launch(&mut self, t: f64, reads: u64) {
        let Some(l) = &self.launching else { return };
        let waited = t - l.last_poll;
        if (reads != l.poll_gen && waited >= LAUNCH_POLL) || waited >= LAUNCH_POLL_STALL {
            let poll = ConsoleCmd::RefreshRunning {
                addr: l.host.addr.clone(),
                mgmt: l.host.mgmt,
                fp_hex: l.host.fp_hex.clone(),
            };
            if let Some(l) = &mut self.launching {
                l.last_poll = t;
                l.poll_gen = reads;
            }
            self.bus.send(poll);
        }
    }

    pub(crate) fn session_ended(&mut self, reason: Option<&str>) {
        self.connecting = None;
        self.launching = None;
        self.in_stream = false;
        // Stack survives a stream, so nothing else refreshes the running set:
        // without this the Resume badge still names the title they just quit.
        // Catalog is left alone — a re-fetch would swap the shelf for a spinner.
        if let Some(lib) = self.stack.last().and_then(Screen::shelf) {
            self.bus.send(ConsoleCmd::RefreshRunning {
                addr: lib.host_addr().to_string(),
                mgmt: lib.host_mgmt_port(),
                fp_hex: lib.host_fp_hex().to_string(),
            });
        }
        if let Some(reason) = reason {
            self.show_toast(format!("Session ended — {reason}"));
        }
    }

    /// Client is redialing on its own (codec fallback). Raise the connecting
    /// modal: nothing sends `Launch` for this retry, so without it the shell
    /// is not streaming, not connecting, and a live pump is behind the
    /// console — A would launch a second session. Back → `CancelConnect`.
    ///
    /// `appear = 1.0`: the retry follows a live stream; fading in is a flash.
    pub(crate) fn session_reconnecting(&mut self, msg: &str) {
        self.in_stream = false;
        self.launching = None;
        self.connecting = Some(Connecting {
            // `None` only if the shell never raised the connect (`--connect`
            // has no console). Prefer a codec-change name over empty string.
            title: self
                .last_connect_title
                .clone()
                .unwrap_or_else(|| "the host".to_string()),
            appear: 1.0,
            request_access: false,
        });
        self.show_toast(msg.to_string());
    }

    /// Every connect starts here. One that asks first waits for the box's profile list
    /// ([`Self::tick_asking`]); the rest dial now.
    pub(crate) fn start_connect(&mut self, mut intent: ConnectIntent) {
        let Some(ask) = intent.ask.take().filter(|_| self.device.profiles) else {
            return self.dial_when_seated(intent);
        };
        // An answer left from an earlier ask is not this one's.
        self.console.take_profiles(&intent.fp_hex);
        self.send_cmd(ConsoleCmd::FetchProfiles {
            addr: intent.addr.clone(),
            mgmt: ask.mgmt,
            fp_hex: intent.fp_hex.clone(),
        });
        let since = self.t();
        self.asking = Some(Asking { intent, ask, since });
    }

    /// The asking connect, once its list lands: §10.1 through `picker_decision`. No answer
    /// in [`PROFILES_WAIT`], or a failed one, dials with the card's saved pick.
    pub(super) fn tick_asking(&mut self) {
        let Some(a) = &self.asking else { return };
        let answer = self.console.take_profiles(&a.intent.fp_hex);
        let waited = self.t() - a.since;
        if answer.is_none() && waited < PROFILES_WAIT {
            if waited >= ASKING_CARD_AFTER && self.connecting.is_none() {
                let title = a.intent.title.clone();
                self.set_connecting(Some(title));
            }
            return;
        }
        let Some(Asking {
            mut intent, ask, ..
        }) = self.asking.take()
        else {
            return;
        };
        self.connecting = None;
        // A first ask may come back `profile-unknown` and ask again; the second may not.
        let first = !std::mem::take(&mut self.reasking);
        self.reask = first.then(|| ConnectIntent {
            profile: None,
            seat: None,
            ask: Some(ProfileAsk {
                saved: None,
                ..ask.clone()
            }),
            ..intent.clone()
        });
        let listed = match answer {
            Some(ProfilesAnswer::Listed(l)) if !l.is_empty() => Some(l),
            Some(ProfilesAnswer::Listed(_) | ProfilesAnswer::NoProfiles) => None,
            Some(ProfilesAnswer::Failed(_)) | None => return self.dial(intent),
        };
        // The row as it stands now: a shelf's copy predates a pick made since it opened.
        let saved = self
            .hosts
            .iter()
            .find(|h| h.host_key() == ask.key)
            .map_or_else(|| ask.saved.clone(), |h| h.profile.clone());
        let d = pf_client_core::profiles::picker_decision(listed.as_deref(), saved.as_ref(), None);
        let seat = d
            .send
            .as_deref()
            .zip(listed.as_deref())
            .and_then(|(id, l)| l.iter().find(|p| p.id == id))
            .map(|row| Seated {
                row: row.clone(),
                mgmt: ask.mgmt,
            });
        if d.picker {
            let screen = crate::screens::profiles::ProfilesScreen::before(
                intent,
                ProfileAsk { saved, ..ask },
                listed.unwrap_or_default(),
                d.gone,
            );
            return self.apply_nav(Nav::Push(Box::new(Screen::Profiles(screen))));
        }
        if d.remember != saved {
            self.send_cmd(ConsoleCmd::SetProfile {
                key: ask.key,
                profile: d.remember,
            });
        }
        intent.profile = d.send;
        intent.seat = seat;
        self.dial_when_seated(intent);
    }

    /// The picked profile's seat decides the dial (§9.2): dial now, wake a stopped seat, or
    /// wait for a starting one. An unavailable seat says why and does not dial.
    fn dial_when_seated(&mut self, mut intent: ConnectIntent) {
        let Some(Seated { row, mgmt }) = intent.seat.take() else {
            return self.dial(intent);
        };
        let detail = match seat_gate(&row) {
            SeatGate::Dial => return self.dial(intent),
            SeatGate::Refuse(line) => return self.show_toast_kind(line, ToastKind::Error),
            SeatGate::Wake => {
                self.send_cmd(ConsoleCmd::WakeProfile {
                    addr: intent.addr.clone(),
                    mgmt,
                    fp_hex: intent.fp_hex.clone(),
                    id: row.id.clone(),
                });
                None
            }
            SeatGate::Wait { detail } => detail,
        };
        // An answer left from an earlier ask is not this wait's.
        self.console.take_profiles(&intent.fp_hex);
        self.seat_wait = Some(SeatWait {
            intent,
            mgmt,
            id: row.id,
            name: row.display_name,
            detail,
            polled: self.t(),
            appear: 0.0,
        });
    }

    /// While a seat comes up: read the profile list every [`SEAT_POLL`] seconds. `ready` or
    /// `occupied` dials; `unavailable` says why and stops. A failed read waits for the next.
    pub(super) fn tick_seat_wait(&mut self) {
        let Some(mut w) = self.seat_wait.take() else {
            return;
        };
        if let Some(ProfilesAnswer::Listed(listed)) = self.console.take_profiles(&w.intent.fp_hex) {
            // A profile the box no longer lists dials as it stands: the host answers for it.
            let gate = listed.iter().find(|p| p.id == w.id).map(seat_gate);
            match gate {
                None | Some(SeatGate::Dial) => return self.dial(w.intent),
                Some(SeatGate::Refuse(line)) => {
                    return self.show_toast_kind(line, ToastKind::Error);
                }
                Some(SeatGate::Wait { detail }) => w.detail = detail,
                Some(SeatGate::Wake) => {}
            }
        }
        let now = self.t();
        if now - w.polled >= SEAT_POLL {
            w.polled = now;
            self.send_cmd(ConsoleCmd::FetchProfiles {
                addr: w.intent.addr.clone(),
                mgmt: w.mgmt,
                fp_hex: w.intent.fp_hex.clone(),
            });
        }
        self.seat_wait = Some(w);
    }

    fn dial(&mut self, intent: ConnectIntent) {
        // A game launch comes off a shelf, which knows both the host's management
        // port and where it just drew the tile. A picker on top is leaving: its shelf is under it.
        let shelf = self
            .stack
            .iter()
            .rev()
            .find(|s| !matches!(s, Screen::Profiles(_)))
            .and_then(Screen::shelf);
        let launch = match (&intent.launch, shelf) {
            (Some(id), Some(lib)) => Some((
                LaunchHost {
                    id: id.clone(),
                    addr: intent.addr.clone(),
                    mgmt: lib.host_mgmt_port(),
                    fp_hex: intent.fp_hex.clone(),
                },
                lib.tile_rect(id),
            )),
            _ => None,
        };
        self.launching = launch.and_then(|(host, from)| self.launch_hold(host, from));
        if self.launching.is_none() {
            self.set_connecting(Some(intent.title.clone()));
            if let Some(c) = &mut self.connecting {
                c.request_access = intent.request_access;
            }
        }
        self.actions.push_back(OverlayAction::Launch {
            addr: intent.addr,
            port: intent.port,
            fp_hex: intent.fp_hex,
            launch: intent.launch,
            title: intent.title,
            request_access: intent.request_access,
            preset: intent.preset,
            profile: intent.profile,
        });
    }
}
