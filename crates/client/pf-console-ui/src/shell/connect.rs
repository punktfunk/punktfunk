//! The connect pipeline: a press on a host or a game, the profile and seat checks before
//! the dial, the connecting card, and the launch hold until the game is up.

use super::{Shell, Takeover, ToastKind};
use crate::anim::Spring;
use crate::model::{ConsoleCmd, HostRow, ProfilesAnswer};
use crate::screens::{ConnectIntent, Nav, ProfileAsk, Screen, Seated};
use pf_client_core::console::OverlayAction;
use pf_client_core::profiles::{seat_gate, ProfilePick, SeatGate};
use skia_safe::Rect;

/// One connect from the press to the stream, one phase at a time.
#[derive(Default)]
pub(super) enum Connect {
    #[default]
    Idle,
    /// Waiting on the box's profile list; `card` is the connect card a slow list puts up.
    Asking {
        ask: Asking,
        card: Option<Connecting>,
    },
    SeatWait(SeatWait),
    /// Dialed, until the stream or a failure.
    Connecting(Connecting),
    /// Dialed for a title: the hold stays up over the stream until the game is.
    Launching(Launching),
}

impl Connect {
    /// The takeover this phase raises.
    pub(super) fn takeover(&self) -> Option<Takeover> {
        match self {
            Connect::Idle => None,
            Connect::Asking { .. } => Some(Takeover::Asking),
            Connect::SeatWait(_) => Some(Takeover::SeatWait),
            Connect::Connecting(_) => Some(Takeover::Connecting),
            Connect::Launching(_) => Some(Takeover::Launching),
        }
    }

    /// The connect card on screen: the dial's, or a slow ask's.
    pub(super) fn card(&self) -> Option<&Connecting> {
        match self {
            Connect::Connecting(c) | Connect::Asking { card: Some(c), .. } => Some(c),
            _ => None,
        }
    }

    pub(super) fn card_mut(&mut self) -> Option<&mut Connecting> {
        match self {
            Connect::Connecting(c) | Connect::Asking { card: Some(c), .. } => Some(c),
            _ => None,
        }
    }

    /// Up over an ask, else in place of whatever phase there was.
    fn show_card(&mut self, c: Connecting) {
        match self {
            Connect::Asking { card, .. } => *card = Some(c),
            _ => *self = Connect::Connecting(c),
        }
    }

    /// The card goes; an ask under it keeps waiting.
    fn drop_card(&mut self) {
        match self {
            Connect::Connecting(_) => *self = Connect::Idle,
            Connect::Asking { card, .. } => *card = None,
            _ => {}
        }
    }

    /// The dial's takeovers go. An ask or a seat wait comes before any dial and stays.
    fn end_dial(&mut self) {
        self.drop_card();
        if matches!(self, Connect::Launching(_)) {
            *self = Connect::Idle;
        }
    }

    pub(super) fn asking(&self) -> Option<&Asking> {
        match self {
            Connect::Asking { ask, .. } => Some(ask),
            _ => None,
        }
    }

    pub(super) fn seat_wait(&self) -> Option<&SeatWait> {
        match self {
            Connect::SeatWait(w) => Some(w),
            _ => None,
        }
    }

    pub(super) fn launching(&self) -> Option<&Launching> {
        match self {
            Connect::Launching(l) => Some(l),
            _ => None,
        }
    }

    pub(super) fn launching_mut(&mut self) -> Option<&mut Launching> {
        match self {
            Connect::Launching(l) => Some(l),
            _ => None,
        }
    }
}

/// How far a connect is from asking its profiles a second time, after the box answered
/// `profile-unknown`. A connect asks again at most once.
#[derive(Default)]
pub(super) enum Reask {
    #[default]
    None,
    /// Dialed, and might still come back `profile-unknown`: the connect as it would ask again.
    Armed(ConnectIntent),
    /// [`Shell::profile_gone`] came: the failure that follows asks again instead of toasting.
    Due(ConnectIntent),
    /// The ask in flight is the second one.
    Asking,
}

/// A connect waiting on its box's profile list (§10.1) before it dials.
pub(super) struct Asking {
    pub(super) intent: ConnectIntent,
    pub(super) ask: ProfileAsk,
    pub(super) since: f64,
}

/// Seconds a connect waits for the profile list before it dials with the remembered pick.
/// Long enough for a slow box to answer, short enough that a dead one does not hold the press.
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

/// [`pf_client_core::profiles::SEAT_POLL`] on this file's seconds clock.
pub(super) const SEAT_POLL: f64 = pf_client_core::profiles::SEAT_POLL.as_secs_f64();

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
/// Three [`LAUNCH_POLL_STALL`] retries, so a lost answer or two cannot end the hold.
pub(super) const LAUNCH_NO_LEASE: f64 = 15.0;
/// A game still `launching`, or `running` without its window, this long is one
/// the player wants to see for themselves — a cold Steam boot with shader work
/// runs to minutes, and the host waits five for it.
pub(super) const LAUNCH_HOLD_MAX: f64 = 120.0;

impl Shell {
    /// The connect card for `title`, over an ask still waiting; `None` takes the card down.
    pub(crate) fn set_connecting(&mut self, title: Option<String>) {
        match title {
            Some(title) => {
                self.last_connect_title = Some(title.clone());
                self.connect.show_card(Connecting {
                    title,
                    appear: 0.0,
                    request_access: false,
                })
            }
            None => self.connect.drop_card(),
        }
    }

    /// A due re-ask connects again; a second ask in flight stays the second.
    pub(crate) fn session_failed(&mut self, msg: &str) {
        self.connect.end_dial();
        self.in_stream = false;
        match std::mem::take(&mut self.reask) {
            Reask::Due(intent) => {
                self.reask = Reask::Asking;
                return self.start_connect(intent);
            }
            Reask::Asking => self.reask = Reask::Asking,
            Reask::None | Reask::Armed(_) => {}
        }
        self.show_toast_kind(format!("Couldn't connect — {msg}"), ToastKind::Error);
    }

    /// The box no longer has the profile the last connect named: forget the card's pick and
    /// ask its list once more. A second miss fails as any other refusal does.
    pub(crate) fn profile_gone(&mut self) {
        if !matches!(self.reask, Reask::Armed(_)) {
            return;
        }
        let Reask::Armed(intent) = std::mem::take(&mut self.reask) else {
            return;
        };
        if let Some(ask) = &intent.ask {
            self.send_cmd(ConsoleCmd::SetProfile {
                key: ask.key.clone(),
                profile: None,
            });
        }
        self.reask = Reask::Due(intent);
    }

    pub(crate) fn session_streaming(&mut self) {
        self.connect.drop_card();
        if matches!(self.reask, Reask::Armed(_)) {
            self.reask = Reask::None;
        }
        let t = self.t();
        let reads = self.library.status_gen();
        let Some(l) = self.connect.launching_mut() else {
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
        self.connect = Connect::Idle;
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
        let Some(l) = self.connect.launching() else {
            return;
        };
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
            if let Some(l) = self.connect.launching_mut() {
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
            if let Some(l) = self.connect.launching_mut() {
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
        if let Some(l) = self.connect.launching_mut() {
            l.window_wait = window_wait;
        }
    }

    /// Ask the host again once the last answer landed, or once it is overdue.
    fn poll_launch(&mut self, t: f64, reads: u64) {
        let Some(l) = self.connect.launching() else {
            return;
        };
        let waited = t - l.last_poll;
        if (reads != l.poll_gen && waited >= LAUNCH_POLL) || waited >= LAUNCH_POLL_STALL {
            let poll = ConsoleCmd::RefreshRunning {
                addr: l.host.addr.clone(),
                mgmt: l.host.mgmt,
                fp_hex: l.host.fp_hex.clone(),
            };
            if let Some(l) = self.connect.launching_mut() {
                l.last_poll = t;
                l.poll_gen = reads;
            }
            self.bus.send(poll);
        }
    }

    pub(crate) fn session_ended(&mut self, reason: Option<&str>) {
        self.connect.end_dial();
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

    /// The host parked the connect for its operator: the card says "Waiting for approval".
    pub(crate) fn session_awaiting_approval(&mut self) {
        if let Some(c) = self.connect.card_mut() {
            c.request_access = true;
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
        self.connect.end_dial();
        self.connect.show_card(Connecting {
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
    /// ([`Self::tick_asking`]), under any connect card already up; the rest dial now.
    pub(crate) fn start_connect(&mut self, mut intent: ConnectIntent) {
        let Some(ask) = intent.ask.take().filter(|_| self.profiles.asks()) else {
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
        let card = match std::mem::take(&mut self.connect) {
            Connect::Connecting(c) | Connect::Asking { card: Some(c), .. } => Some(c),
            _ => None,
        };
        let ask = Asking { intent, ask, since };
        self.connect = Connect::Asking { ask, card };
    }

    /// The asking connect, once its list lands: §10.1 through `picker_decision`. No answer
    /// in [`PROFILES_WAIT`], or a failed one, dials with the remembered pick, if any.
    pub(super) fn tick_asking(&mut self) {
        let t = self.t();
        let Connect::Asking { ask: a, card } = &self.connect else {
            return;
        };
        let answer = self.console.take_profiles(&a.intent.fp_hex);
        let waited = t - a.since;
        if answer.is_none() && waited < PROFILES_WAIT {
            if waited >= ASKING_CARD_AFTER && card.is_none() {
                let title = a.intent.title.clone();
                self.set_connecting(Some(title));
            }
            return;
        }
        let Connect::Asking {
            ask: Asking {
                mut intent, ask, ..
            },
            ..
        } = std::mem::take(&mut self.connect)
        else {
            return;
        };
        if let Some(answer) = &answer {
            self.profiles.learn(&intent.fp_hex, answer);
        }
        // A first ask may come back `profile-unknown` and ask again; the second may not.
        self.reask = match self.reask {
            Reask::Asking => Reask::None,
            _ => Reask::Armed(ConnectIntent {
                profile: None,
                seat: None,
                ask: Some(ProfileAsk {
                    saved: None,
                    ..ask.clone()
                }),
                ..intent.clone()
            }),
        };
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
        let remembered = saved.clone().filter(|p| p.remember);
        let d =
            pf_client_core::profiles::picker_decision(listed.as_deref(), remembered.as_ref(), None);
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
            // An unremembered pick, else this run's last, leads the circles.
            let lead = (saved.as_ref().map(|p| p.id.as_str()))
                .or_else(|| self.profiles.last_pick(&intent.fp_hex))
                .map(str::to_string);
            let screen = crate::screens::profiles::ProfilesScreen::before(
                intent,
                ProfileAsk { saved, ..ask },
                listed.unwrap_or_default(),
                d.gone,
                lead,
            );
            return self.apply_nav(Nav::Push(Box::new(Screen::Profiles(screen))));
        }
        // The old seat's profile is the host's word for this device: it stays remembered.
        let keep = d.remember.map(|p| ProfilePick {
            remember: true,
            ..p
        });
        if keep != remembered {
            self.send_cmd(ConsoleCmd::SetProfile {
                key: ask.key,
                profile: keep,
            });
        }
        intent.profile = d.send;
        intent.seat = seat;
        self.dial_when_seated(intent);
    }

    /// Ask `host` for its profiles on the shell's own account, so its card and its shelf can
    /// offer who plays. One ask per host at a time; an ask already waited on covers it.
    pub(super) fn ask_profiles(&mut self, host: &HostRow) {
        let fp = &host.fp_hex;
        if !self.profiles.asks() || !host.paired || fp.is_empty() || self.waits_on_profiles(fp) {
            return;
        }
        if self.profiles.ask(fp) {
            self.bus.send(ConsoleCmd::FetchProfiles {
                addr: host.addr.clone(),
                mgmt: host.mgmt_port,
                fp_hex: fp.clone(),
            });
        }
    }

    /// Answers to asks nothing waits on go into the book.
    pub(super) fn collect_profiles(&mut self) {
        for fp in self.profiles.asked().to_vec() {
            if self.waits_on_profiles(&fp) {
                continue;
            }
            if let Some(answer) = self.console.take_profiles(&fp) {
                self.profiles.learn(&fp, &answer);
            }
        }
    }

    /// Takes the pictures that landed, and asks for those a paired host shows and nobody has.
    pub(super) fn collect_pictures(&mut self) {
        if !self.profiles.asks() {
            return;
        }
        for (url, bytes) in self.console.take_avatars() {
            crate::screens::profiles::offer_picture(url, bytes);
        }
        crate::screens::profiles::adopt_pictures();
        for h in self.hosts.iter().filter(|h| h.paired) {
            for url in self.profiles.pictures_to_ask(h) {
                self.bus.send(ConsoleCmd::FetchAvatar {
                    addr: h.addr.clone(),
                    mgmt: h.mgmt_port,
                    fp_hex: h.fp_hex.clone(),
                    url,
                });
            }
        }
    }

    /// A connect, a seat wait or the screen on top takes `fp`'s next answer.
    fn waits_on_profiles(&self, fp: &str) -> bool {
        self.connect.asking().is_some_and(|a| a.intent.fp_hex == fp)
            || self
                .connect
                .seat_wait()
                .is_some_and(|w| w.intent.fp_hex == fp)
            || matches!(self.stack.last(), Some(Screen::Profiles(p)) if p.waiting() && p.fp_hex() == fp)
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
        self.connect = Connect::SeatWait(SeatWait {
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
        if self.connect.seat_wait().is_none() {
            return;
        }
        let Connect::SeatWait(mut w) = std::mem::take(&mut self.connect) else {
            return;
        };
        let answer = self.console.take_profiles(&w.intent.fp_hex);
        if let Some(answer) = &answer {
            self.profiles.learn(&w.intent.fp_hex, answer);
        }
        if let Some(ProfilesAnswer::Listed(listed)) = answer {
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
        self.connect = Connect::SeatWait(w);
    }

    fn dial(&mut self, intent: ConnectIntent) {
        if let Some(id) = &intent.profile {
            self.profiles.note_pick(&intent.fp_hex, id);
        }
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
        match launch.and_then(|(host, from)| self.launch_hold(host, from)) {
            Some(hold) => self.connect = Connect::Launching(hold),
            None => {
                self.set_connecting(Some(intent.title.clone()));
                if let Some(c) = self.connect.card_mut() {
                    c.request_access = intent.request_access;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn asking(card: Option<Connecting>) -> Connect {
        let host = HostRow::default();
        let ask = Asking {
            intent: ConnectIntent::to_host(&host, None),
            ask: ProfileAsk {
                key: String::new(),
                name: String::new(),
                mgmt: 0,
                saved: None,
            },
            since: 0.0,
        };
        Connect::Asking { ask, card }
    }

    /// Each phase raises its own takeover. The card rides an ask and goes without it; the
    /// end of a dial leaves an ask waiting.
    #[test]
    fn a_connect_phase_owns_its_takeover_and_its_card() {
        let card = |title: &str| Connecting {
            title: title.into(),
            appear: 0.0,
            request_access: false,
        };
        let mut c = asking(None);
        assert_eq!(c.takeover(), Some(Takeover::Asking));
        c.show_card(card("Den"));
        assert_eq!(c.card().map(|c| c.title.as_str()), Some("Den"));
        assert_eq!(
            c.takeover(),
            Some(Takeover::Asking),
            "the card waits on the ask"
        );
        c.end_dial();
        assert!(c.card().is_none() && c.asking().is_some());

        let mut c = Connect::Idle;
        c.show_card(card("Den"));
        assert_eq!(c.takeover(), Some(Takeover::Connecting));
        c.drop_card();
        assert!(matches!(c, Connect::Idle));
        assert_eq!(c.takeover(), None);
    }
}
