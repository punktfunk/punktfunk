//! The connect flow: the trust gate, the session child's life, deep links and the console.

use super::*;
use std::collections::BTreeMap;
use std::time::Duration;

/// What holds a host's slot. Whatever asks for it while it is taken is ignored.
#[derive(Default)]
pub(super) enum Slot {
    #[default]
    Free,
    /// A profile read: it ends on its own.
    Held,
    /// The dialog waiting for a profile's seat to come up.
    Seat(Box<profile::SeatWait>),
    /// The session child, from spawn to exit, and the host's name from its first frame.
    Session(CancelHandle, Option<String>),
}

/// One host's connect: its facts, what holds its slot, and the dialog a request for access
/// leaves up.
#[derive(Default)]
pub(super) struct Flight {
    /// What its session's ready and exit are read against.
    pub(super) attempt: ConnectAttempt,
    pub(super) slot: Slot,
    /// The "waiting for approval" dialog with its Cancel handler. A shared slot, not a
    /// message: a dialog is a main-thread object and `AppMsg` is `Send`.
    pub(super) waiting: crate::app::gate::WaitingSlot,
}

impl Flight {
    fn busy(&self) -> bool {
        !matches!(self.slot, Slot::Free)
    }

    /// Dismisses the waiting dialog without its Cancel handler running. `close()` emits the
    /// close response, so the handler goes first, or closing it here reads as the user's
    /// Cancel.
    pub(super) fn close_waiting(&self) {
        if let Some((w, handler)) = self.waiting.borrow_mut().take() {
            w.disconnect(handler);
            w.close();
        }
    }
}

/// Every host's flight by [`ConnectRequest::card_key`]: hosts connect and stream side by side,
/// each with one session at most.
#[derive(Default)]
pub(super) struct Flights {
    pub(super) hosts: BTreeMap<String, Flight>,
    /// The console or a speed test runs. The console owns the screen and the pads, a speed
    /// test owns the link, so neither shares the shell with a connect.
    alone: bool,
}

impl Flights {
    pub(super) fn of(&mut self, key: &str) -> &mut Flight {
        self.hosts.entry(key.to_string()).or_default()
    }

    /// `key`'s host can't take a connect now.
    pub(super) fn busy(&self, key: &str) -> bool {
        self.alone || self.hosts.get(key).is_some_and(Flight::busy)
    }

    /// Anything at all is running.
    pub(super) fn any(&self) -> bool {
        self.alone || self.hosts.values().any(Flight::busy)
    }

    /// Takes `key`'s slot for something that ends on its own. `false`: it is taken.
    pub(super) fn hold(&mut self, key: &str) -> bool {
        let free = !self.busy(key);
        if free {
            self.of(key).slot = Slot::Held;
        }
        free
    }

    /// Frees a slot [`Flights::hold`] took.
    pub(super) fn release(&mut self, key: &str) {
        let flight = self.of(key);
        if matches!(flight.slot, Slot::Held) {
            flight.slot = Slot::Free;
        }
    }

    /// Takes the whole shell for the console or a speed test. `false`: something is running.
    pub(super) fn hold_alone(&mut self) -> bool {
        let free = !self.any();
        self.alone |= free;
        free
    }

    pub(super) fn release_alone(&mut self) {
        self.alone = false;
    }

    /// Ends `key`'s session, or every session with `None`. `terminate` records our own end on
    /// the child's handle, so an escalated kill does not read as a crash.
    pub(super) fn terminate(&self, key: Option<&str>, grace: Duration) {
        for (at, flight) in &self.hosts {
            if let (true, Slot::Session(child, _)) = (key.is_none_or(|k| k == at), &flight.slot) {
                child.terminate(grace);
            }
        }
    }

    /// The names of the hosts streaming, and whether any stream is on its way out.
    fn streams(&self) -> (Vec<&str>, bool) {
        let (mut names, mut closing) = (Vec::new(), false);
        for flight in self.hosts.values() {
            match &flight.slot {
                Slot::Session(child, Some(_)) if child.is_cancelled() => closing = true,
                Slot::Session(_, Some(name)) => names.push(name.as_str()),
                _ => {}
            }
        }
        (names, closing)
    }

    /// The sessions the banner's button would end, the ones still dialing among them.
    fn live_sessions(&self) -> usize {
        let live = |f: &&Flight| matches!(&f.slot, Slot::Session(c, _) if !c.is_cancelled());
        self.hosts.values().filter(live).count()
    }
}

/// The banner's line for the hosts streaming.
fn streaming_line(names: &[&str]) -> String {
    match names {
        [one] => format!("Streaming from {one}"),
        [a, b] => format!("Streaming from {a} and {b}"),
        many => format!("Streaming from {} hosts", many.len()),
    }
}

impl AppModel {
    /// Opens the surface [`trust_route`] picks: the stored pin dials, a changed fingerprint
    /// gets the PIN dialog, a new `pair=optional` host gets the TOFU offer, and anything
    /// else gets delegated approval or PIN.
    pub(super) fn connect(&mut self, req: ConnectRequest, sender: &ComponentSender<Self>) {
        let key = req.card_key();
        if self.flights.busy(&key) {
            return;
        }
        let route = trust_route(
            &self.store.hosts(),
            req.host.fp_hex.as_deref(),
            &req.host.addr,
            req.host.port,
            req.pair_optional,
        );
        match route {
            TrustRoute::Pinned(fp_hex) => self.ask_profile(req, fp_hex, false, sender),
            TrustRoute::FingerprintChanged => {
                self.toast(FINGERPRINT_CHANGED);
                crate::app::gate::pin_dialog(
                    &self.window,
                    sender,
                    self.identity.clone(),
                    req,
                    false,
                );
            }
            TrustRoute::OfferTofu(_) => crate::app::gate::tofu_dialog(&self.window, sender, req),
            TrustRoute::NeedsPairing => {
                crate::app::gate::approval_dialog(
                    &self.window,
                    sender,
                    self.flights.of(&key).waiting.clone(),
                    self.identity.clone(),
                    req,
                );
            }
        }
    }

    /// Spawns the session for a connect whose trust is decided, unless its slot is taken.
    pub(super) fn start_session(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        opts: SpawnOpts,
        sender: &ComponentSender<Self>,
    ) {
        let key = req.card_key();
        if !self.flights.hold(&key) {
            return;
        }
        let profile = opts.profile.as_ref().and_then(|p| p.as_deref());
        self.flights
            .of(&key)
            .attempt
            .dial(tofu, profile, opts.redial);
        self.hosts.emit(HostsMsg::ClearError);
        self.hosts
            .emit(HostsMsg::SetSession(key.clone(), Some(Phase::Connecting)));
        // No settings ride along: the spawner resolves this host's effective ones
        // (globals + its preset) for both the argv and the child's spec.
        match spawn::spawn_session(sender.input_sender().clone(), req, fp_hex, tofu, opts) {
            Ok(child) => {
                self.flights.of(&key).slot = Slot::Session(child, None);
                self.show_streams();
            }
            Err(e) => {
                self.flights.release(&key);
                self.hosts.emit(HostsMsg::SetSession(key, None));
                self.hosts.emit(HostsMsg::ShowError(e));
            }
        }
    }

    /// The child is streaming. A pin trusted on first use is saved now, with the request's
    /// MACs.
    pub(super) fn session_ready(&mut self, req: ConnectRequest, fp_hex: String) {
        let key = req.card_key();
        let flight = self.flights.of(&key);
        flight.close_waiting();
        if let Slot::Session(_, name) = &mut flight.slot {
            *name = Some(req.host.name.clone());
        }
        let tofu = flight.attempt.on_ready();
        self.hosts
            .emit(HostsMsg::SetSession(key, Some(Phase::Streaming)));
        self.show_streams();
        // Ready proved the advertised fingerprint. The stream is up on a pin carried in
        // memory, so a failed save says so, or the host is gone at the next launch.
        if tofu {
            let saved = orchestrate::persist_on_ready(&req.host, &fp_hex, false);
            self.store.reload(Changed::Hosts);
            match saved {
                Ok(()) => self.toast(&format!(
                    "Trusted on first use — fingerprint {}…",
                    &fp_hex[..16.min(fp_hex.len())]
                )),
                Err(e) => self.toast(&format!("Connected, but couldn't save — {e:#}")),
            }
        }
        self.hosts.emit(HostsMsg::Refresh);
    }

    /// The banner follows the streams: who is streaming, or that the last ones are closing.
    pub(super) fn show_streams(&self) {
        let (names, closing) = self.flights.streams();
        match names[..] {
            [] if closing => self.streaming.show("Disconnecting\u{2026}", None),
            [] => self.streaming.hide(),
            _ => self.streaming.show(
                &streaming_line(&names),
                Some(match self.flights.live_sessions() {
                    1 => "Disconnect",
                    _ => "Disconnect All",
                }),
            ),
        }
    }

    /// The child is gone: frees its host's slot and opens the surface
    /// [`ConnectAttempt::on_exit`] picks.
    pub(super) fn session_exited(
        &mut self,
        req: ConnectRequest,
        code: i32,
        error: Option<pf_client_core::orchestrate::SessionError>,
        ended: Option<String>,
        sender: &ComponentSender<Self>,
    ) {
        let key = req.card_key();
        let flight = self.flights.of(&key);
        flight.close_waiting();
        // Our own Disconnect is on the child's handle, whatever exit code the kill leaves.
        let held = std::mem::take(&mut flight.slot);
        let cancelled = matches!(held, Slot::Session(child, _) if child.is_cancelled());
        let outcome = ConnectOutcome::from_exit(code, error, ended, cancelled);
        let route = flight
            .attempt
            .on_exit(&req.host, &outcome, "the client log");
        self.show_streams();
        self.hosts.emit(HostsMsg::SetSession(key, None));
        match route {
            ExitRoute::Silent => {}
            ExitRoute::Banner(msg) => {
                // Beside another stream the line has to say which host it is about.
                self.hosts.emit(HostsMsg::ShowError(if self.flights.any() {
                    format!("{}: {msg}", req.host.name)
                } else {
                    msg
                }));
            }
            ExitRoute::Repair(msg) => {
                self.toast(&msg);
                crate::app::gate::pin_dialog(
                    &self.window,
                    sender,
                    self.identity.clone(),
                    req,
                    false,
                );
            }
            ExitRoute::RedialProfile { id, msg } => match req.host.pin().map(str::to_string) {
                Some(fp_hex) => self.reask_profile(req, fp_hex, id, msg, sender),
                None => self.hosts.emit(HostsMsg::ShowError(msg)),
            },
            ExitRoute::ForgetProfileThen(msg) => {
                self.forget_profile(&req);
                self.hosts.emit(HostsMsg::ShowError(msg));
            }
            ExitRoute::Wake => crate::app::gate::wake_and_connect(&self.window, sender, req),
        }
    }

    /// `until_no_pads`: a controller connecting opened it, so it hands the desk back once the
    /// last one is gone (never mid-stream; the session decides that).
    pub(super) fn open_console(&mut self, sender: &ComponentSender<Self>, until_no_pads: bool) {
        if !self.flights.hold_alone() {
            return;
        }
        // The session also fullscreens itself on the Deck and under gamescope.
        let settings = self.store.settings();
        let fullscreen = settings.fullscreen_on_stream || settings.fullscreen_always();
        drop(settings);
        let out = sender.input_sender().clone();
        if let Err(e) = spawn::spawn_console(out, fullscreen, until_no_pads) {
            self.flights.release_alone();
            self.hosts.emit(HostsMsg::ShowError(e));
        }
    }

    /// Route a `punktfunk://` URL (design/client-deep-links.md §4.1). Parsing, host/preset
    /// resolution and every refusal rule — including "only a stable record id may dial
    /// unattended" — live in the shared brain (`plan_from_link`); this is only the GTK end of
    /// it: turn the outcome into the same messages a card click raises, so a link gets the
    /// identical wake, trust and error surfaces and NOT a second connect path of its own.
    pub(super) fn open_deep_link(&mut self, url: &str, sender: &ComponentSender<AppModel>) {
        use pf_client_core::deeplink;
        use pf_client_core::orchestrate::{plan_from_link, PlanOutcome};

        tracing::debug!(%url, "deep link");
        let link = match deeplink::parse(url) {
            Ok(l) => l,
            Err(e) => return self.toast(&e.message()),
        };
        // A shelf dials nothing, so a guessable name needs no confirmation here.
        if link.route == deeplink::Route::Browse {
            let known = self.store.hosts();
            return match deeplink::resolve_host(&link, &known) {
                deeplink::HostResolution::Known(i) | deeplink::HostResolution::Confirm(i) => sender
                    .input(AppMsg::OpenLibrary(ConnectRequest {
                        host: HostTarget::from(&known.hosts[i]),
                        ..ConnectRequest::default()
                    })),
                _ => {
                    drop(known);
                    self.toast("That host isn't saved on this device.")
                }
            };
        }
        let outcome = plan_from_link(
            &link,
            &self.store.hosts(),
            &self.store.presets(),
            &self.store.settings(),
        );
        match outcome {
            Ok(PlanOutcome::Connect(plan)) => {
                let req = ConnectRequest {
                    host: plan.host.clone(),
                    launch: plan.launch.clone(),
                    // `preset=` in a URL is a one-off, exactly like "Connect with ▸": it
                    // shapes this session and leaves the host's binding alone.
                    preset: plan.preset_override.clone(),
                    profile: link.as_profile.clone(),
                    ..ConnectRequest::default()
                };
                // Rule 2 of §3: never preempt a live session; another host streams beside it.
                // Only this layer knows what runs, which is why the brain leaves the check here.
                if self.flights.busy(&req.card_key()) {
                    return self.toast("A session is already running — end it first.");
                }
                // A link is a launch like any other: with a MAC it takes the dial-first wake
                // path, so a sleeping host wakes instead of erroring.
                sender.input(if plan.wake {
                    AppMsg::WakeConnect(req)
                } else {
                    AppMsg::Connect(req)
                });
            }
            Ok(PlanOutcome::ConfirmConnect(plan)) => {
                // The link named this (saved, pinned) host by its LABEL or its ADDRESS rather
                // than by its record id. `x-scheme-handler/punktfunk` is registered by our
                // .desktop, so any web page can hand us such a URL and both of those are
                // guessable — the dial waits for a person. Deliberately not the PIN ceremony
                // below: this host is already pinned, and re-pairing it would throw that away.
                let req = ConnectRequest {
                    host: plan.host.clone(),
                    launch: plan.launch.clone(),
                    preset: plan.preset_override.clone(),
                    profile: link.as_profile.clone(),
                    ..ConnectRequest::default()
                };
                if self.flights.busy(&req.card_key()) {
                    return self.toast("A session is already running — end it first.");
                }
                let mut body = format!(
                    "A link asks to connect to {} ({}).",
                    req.host.name, req.host.addr
                );
                if let Some(id) = &req.launch {
                    body.push_str(&format!("\n\nIt also asks the host to launch “{id}”."));
                }
                body.push_str(
                    "\n\nIt names the host by its label or address, which anything that can \
                     open a link could guess. A link that names the host's id connects without \
                     asking.",
                );
                let dialog = adw::AlertDialog::new(Some("Open this link?"), Some(&body));
                dialog.add_responses(&[("cancel", "Cancel"), ("connect", "Connect")]);
                dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("connect"));
                dialog.set_close_response("cancel");
                let sender = sender.clone();
                let wake = plan.wake;
                dialog.connect_response(Some("connect"), move |_, _| {
                    // The same two messages the `Connect` arm raises, so the confirmed link
                    // gets the identical wake / trust / error surfaces a card click gets.
                    sender.input(if wake {
                        AppMsg::WakeConnect(req.clone())
                    } else {
                        AppMsg::Connect(req.clone())
                    });
                });
                dialog.present(Some(&self.window));
            }
            Ok(PlanOutcome::ConfirmUnknown(unknown)) => {
                // Known-but-unpinned, or not known at all: the link may not pair and may not
                // trust on its own, so it opens the ordinary ceremony under the user's eyes —
                // the PIN dialog, seeded with what the link claimed.
                let req = ConnectRequest {
                    host: HostTarget {
                        name: unknown.name.clone().unwrap_or_else(|| unknown.addr.clone()),
                        addr: unknown.addr.clone(),
                        port: unknown.port,
                        fp_hex: unknown.fp.clone(),
                        ..HostTarget::default()
                    },
                    launch: unknown.launch.clone(),
                    preset: unknown.preset.clone(),
                    profile: unknown.profile.clone(),
                    ..ConnectRequest::default()
                };
                if self.flights.busy(&req.card_key()) {
                    return self.toast("A session is already running — end it first.");
                }
                self.toast(&format!(
                    "{} isn't paired with this device yet — pair it to continue.",
                    req.host.name
                ));
                crate::app::gate::pin_dialog(
                    &self.window,
                    sender,
                    self.identity.clone(),
                    req,
                    true,
                );
            }
            Ok(PlanOutcome::Unsupported(route)) => self.toast(&format!(
                "Punktfunk can't open “{}” links yet.",
                route.as_str()
            )),
            Err(e) => self.toast(&e.message()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slot has one owner: a second taker is refused, and `release` frees only what `hold`
    /// took.
    #[test]
    fn the_slot_has_one_owner() {
        let mut flights = Flights::default();
        assert!(flights.hold("a"));
        assert!(!flights.hold("a"));
        flights.release("a");
        assert!(!flights.busy("a"));

        flights.of("a").slot = Slot::Session(CancelHandle::default(), None);
        assert!(!flights.hold("a"));
        flights.release("a");
        assert!(
            flights.busy("a"),
            "a session's slot is not `hold`'s to free"
        );
    }

    /// A stream takes its own host only: a second host dials beside it.
    #[test]
    fn hosts_connect_side_by_side() {
        let mut flights = Flights::default();
        flights.of("a").slot = Slot::Session(CancelHandle::default(), Some("A".into()));
        assert!(flights.busy("a"));
        assert!(!flights.busy("b"));
        assert!(flights.hold("b"));
        flights.release("b");
        assert!(flights.any(), "the stream still runs");
    }

    /// The console and a speed test run only when nothing else does, and hold off every host
    /// until they end.
    #[test]
    fn the_console_and_a_speed_test_run_alone() {
        let mut flights = Flights::default();
        assert!(flights.hold_alone());
        assert!(!flights.hold_alone());
        assert!(flights.busy("a"));
        assert!(!flights.hold("a"));
        flights.release_alone();
        assert!(!flights.any());

        flights.of("a").slot = Slot::Session(CancelHandle::default(), None);
        assert!(!flights.hold_alone(), "a stream keeps the shell shared");
    }

    /// The banner lists what streams, a dialing session stays off it, and a disconnect takes
    /// only the host it names.
    #[test]
    fn the_banner_follows_each_stream() {
        let mut flights = Flights::default();
        for (key, name) in [("a", Some("Alpha")), ("b", Some("Beta")), ("c", None)] {
            flights.of(key).slot = Slot::Session(CancelHandle::default(), name.map(str::to_string));
        }
        assert_eq!(flights.streams(), (vec!["Alpha", "Beta"], false));
        assert_eq!(flights.live_sessions(), 3, "the dial counts for the button");

        flights.terminate(Some("a"), Duration::ZERO);
        assert_eq!(flights.streams(), (vec!["Beta"], true));
        assert_eq!(flights.live_sessions(), 2);
        flights.terminate(None, Duration::ZERO);
        assert_eq!(flights.streams(), (vec![], true));
        assert_eq!(flights.live_sessions(), 0);
    }

    #[test]
    fn the_banner_counts_past_two() {
        assert_eq!(streaming_line(&["Desk"]), "Streaming from Desk");
        assert_eq!(
            streaming_line(&["Desk", "Den"]),
            "Streaming from Desk and Den"
        );
        assert_eq!(streaming_line(&["A", "B", "C"]), "Streaming from 3 hosts");
    }
}
