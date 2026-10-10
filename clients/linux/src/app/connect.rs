//! The connect flow: the trust gate, the session child's life, deep links and the console.

use super::*;

impl AppModel {
    /// Opens the surface [`trust_route`] picks: the stored pin dials, a changed fingerprint
    /// gets the PIN dialog, a new `pair=optional` host gets the TOFU offer, and anything
    /// else gets delegated approval or PIN.
    pub(super) fn connect(&mut self, req: ConnectRequest, sender: &ComponentSender<Self>) {
        if self.busy {
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
                    self.waiting.clone(),
                    self.identity.clone(),
                    req,
                );
            }
        }
    }

    pub(super) fn start_session(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        opts: SpawnOpts,
        sender: &ComponentSender<Self>,
    ) {
        // A request-access start carries the cancel its waiting dialog arms; one that never
        // spawns must close that dialog, or its Cancel tears down a session it does not own.
        let request = opts.cancel.is_some();
        if std::mem::replace(&mut self.busy, true) {
            if request {
                self.close_waiting();
            }
            return;
        }
        self.retry_profile = opts.profile.clone().flatten().filter(|_| !opts.redial);
        self.hosts.emit(HostsMsg::ClearError);
        self.hosts.emit(HostsMsg::SetSession(Some((
            req.card_key(),
            Phase::Connecting,
        ))));
        // No settings ride along: the spawner resolves this host's effective ones
        // (globals + its preset) for both the argv and the child's spec.
        match spawn::spawn_session(sender.input_sender().clone(), req, fp_hex, tofu, opts) {
            Ok(child) => self.session = Some(child),
            Err(e) => {
                if request {
                    self.close_waiting();
                }
                self.busy = false;
                self.hosts.emit(HostsMsg::SetSession(None));
                self.hosts.emit(HostsMsg::ShowError(e));
            }
        }
    }

    /// The child is streaming. A request-access or TOFU pin is saved now, with the request's
    /// MACs.
    pub(super) fn session_ready(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        tofu: bool,
        persist_paired: bool,
        cancel: Option<CancelHandle>,
    ) {
        if ready_was_cancelled(cancel.as_ref()) {
            return;
        }
        self.close_waiting();
        self.hosts.emit(HostsMsg::SetSession(Some((
            req.card_key(),
            Phase::Streaming,
        ))));
        self.streaming.show(
            &format!("Streaming from {}", req.host.name),
            Some("Disconnect"),
        );
        // A child that reported ready proves the host answered — the exact condition
        // the dial-first wake fallback exists to rule out. Left armed, it turns a
        // later ordinary failure into a spurious "waking…".
        self.wake_fallback = None;
        // Request access: the operator's approval is the pairing. TOFU: ready proved the
        // advertised fingerprint. Either way the stream is up on a pin carried in memory, so
        // a failed save says so, or the host is gone at the next launch.
        if persist_paired || tofu {
            let saved = orchestrate::persist_on_ready(&req.host, &fp_hex, persist_paired);
            self.store.reload(Changed::Hosts);
            match saved {
                Ok(()) if persist_paired => self.toast("Approved — connected"),
                Ok(()) => self.toast(&format!(
                    "Trusted on first use — fingerprint {}…",
                    &fp_hex[..16.min(fp_hex.len())]
                )),
                Err(e) => self.toast(&format!("Connected, but couldn't save — {e:#}")),
            }
        }
        self.hosts.emit(HostsMsg::Refresh);
    }

    /// The child is gone: release `busy` and open the surface [`orchestrate::exit_route`]
    /// picks.
    pub(super) fn session_exited(
        &mut self,
        req: ConnectRequest,
        code: i32,
        error: Option<pf_client_core::orchestrate::SessionError>,
        ended: Option<String>,
        tofu: bool,
        sender: &ComponentSender<Self>,
    ) {
        self.close_waiting();
        self.busy = false;
        self.session = None;
        self.streaming.hide();
        self.hosts.emit(HostsMsg::SetSession(None));
        // The dial-first wake fallback (armed by `WakeConnect`, consumed on every exit). Matched
        // by fingerprint (else address) so a stale armed request can never redirect another
        // host's failure.
        let cancelled = std::mem::take(&mut self.session_cancelled);
        let retry = self.retry_profile.take();
        let wake_armed =
            self.wake_fallback
                .take()
                .is_some_and(|fb| match (fb.host.pin(), req.host.pin()) {
                    (Some(a), Some(b)) => a == b,
                    _ => fb.host.addr == req.host.addr && fb.host.port == req.host.port,
                });
        let outcome = ConnectOutcome::from_exit(code, error, ended, cancelled);
        match orchestrate::exit_route(
            &outcome,
            tofu,
            retry.as_deref(),
            wake_armed,
            "the client log",
        ) {
            ExitRoute::Silent => {}
            ExitRoute::Banner(msg) => self.hosts.emit(HostsMsg::ShowError(msg)),
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
        if std::mem::replace(&mut self.busy, true) {
            return;
        }
        // The console owns the screen and the pads while it runs, so it takes `busy`
        // like a stream does. `wait_check_async` lands the exit on this main loop —
        // no thread, no channel — and turns a non-zero exit into the error the
        // banner shows, which is also how a session built without `ui` surfaces.
        let mut argv = vec![
            std::ffi::OsString::from(crate::app::spawn::session_binary()),
            "--browse".into(),
        ];
        // Same knobs a stream uses — the session also fullscreens itself on the Deck
        // and under gamescope regardless.
        let settings = self.store.settings();
        if settings.fullscreen_on_stream || settings.fullscreen_always() {
            argv.push("--fullscreen".into());
        }
        drop(settings);
        if until_no_pads {
            argv.push("--until-no-controller".into());
        }
        let argv: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsString::as_os_str).collect();
        match gio::Subprocess::newv(&argv, gio::SubprocessFlags::NONE) {
            Ok(child) => {
                let sender = sender.clone();
                child.wait_check_async(gio::Cancellable::NONE, move |res| {
                    sender.input(AppMsg::ConsoleExited(res.err().map(|e| e.to_string())));
                });
            }
            Err(e) => {
                self.busy = false;
                self.hosts.emit(HostsMsg::ShowError(format!(
                    "Couldn't start the console UI — {e}"
                )));
            }
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
                // Rule 2 of §3: never preempt a live session. Only this layer knows one is
                // running, which is why the brain leaves the check here.
                if self.busy {
                    return self.toast("A session is already running — end it first.");
                }
                let req = ConnectRequest {
                    host: plan.host.clone(),
                    launch: plan.launch.clone(),
                    // `preset=` in a URL is a one-off, exactly like "Connect with ▸": it
                    // shapes this session and leaves the host's binding alone.
                    preset: plan.preset_override.clone(),
                    profile: link.as_profile.clone(),
                    ..ConnectRequest::default()
                };
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
                if self.busy {
                    return self.toast("A session is already running — end it first.");
                }
                let req = ConnectRequest {
                    host: plan.host.clone(),
                    launch: plan.launch.clone(),
                    preset: plan.preset_override.clone(),
                    profile: link.as_profile.clone(),
                    ..ConnectRequest::default()
                };
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
                if self.busy {
                    return self.toast("A session is already running — end it first.");
                }
                let req = ConnectRequest {
                    host: HostTarget {
                        name: unknown.name.clone().unwrap_or_else(|| unknown.addr.clone()),
                        addr: unknown.addr.clone(),
                        port: unknown.port,
                        fp_hex: unknown.fp.clone(),
                        ..HostTarget::default()
                    },
                    launch: unknown.launch.clone(),
                    ..ConnectRequest::default()
                };
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

    /// Dismiss the waiting dialog without its Cancel handler running. `close()` emits the
    /// close response, so the handler has to go first or the approval that just landed reads
    /// as the user cancelling — and kills the child that reported ready.
    pub(super) fn close_waiting(&mut self) {
        if let Some((w, handler)) = self.waiting.borrow_mut().take() {
            w.disconnect(handler);
            w.close();
        }
    }
}
