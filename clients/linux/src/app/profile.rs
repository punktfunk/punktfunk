//! The profile picker in front of a paired host's connect, the wait for a seat that is
//! starting, and the rejections that follow.

use super::*;
use pf_client_core::profiles::{self, SeatGate};
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The longest a connect waits for the host's profile list.
const ASK_BUDGET: Duration = Duration::from_secs(3);
/// How often the waiting dialog re-reads the seat.
const SEAT_POLL: Duration = Duration::from_secs(2);

/// A seat coming up: the dialog that shows it, the flag that stops its poll, and the dial
/// that follows once it is ready.
pub(super) struct SeatWait {
    dialog: adw::AlertDialog,
    stop: Arc<AtomicBool>,
    req: ConnectRequest,
    fp_hex: String,
    id: String,
    redial: bool,
}

impl AppModel {
    /// Asks the host for its profiles off the UI thread. A connect holds the card in its
    /// connecting state meanwhile; `switch` only opens the picker.
    pub(super) fn ask_profile(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        switch: bool,
        sender: &ComponentSender<Self>,
    ) {
        if !switch {
            self.busy = true;
            self.hosts.emit(HostsMsg::SetSession(Some((
                req.card_key(),
                Phase::Connecting,
            ))));
        }
        self.fetch_profiles(req, fp_hex, sender, move |req, fp_hex, listed| {
            AppMsg::ProfileAsked {
                req,
                fp_hex,
                switch,
                listed,
            }
        });
    }

    /// Reads the host's list within [`ASK_BUDGET`] and posts `done` with it. `None`: the
    /// read failed or ran late.
    fn fetch_profiles(
        &self,
        req: ConnectRequest,
        fp_hex: String,
        sender: &ComponentSender<Self>,
        done: impl FnOnce(ConnectRequest, String, Option<Option<Vec<ProfileRow>>>) -> AppMsg
            + Send
            + 'static,
    ) {
        let mgmt = self.mgmt_port(&fp_hex);
        let (identity, out) = (self.identity.clone(), sender.input_sender().clone());
        let pin = trust::parse_hex32(&fp_hex);
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let addr = req.addr.clone();
            std::thread::spawn(move || {
                let _ = tx.send(profiles::fetch_enumerate(&addr, mgmt, &identity, pin));
            });
            let listed = rx.recv_timeout(ASK_BUDGET).ok().and_then(Result::ok);
            let _ = out.send(done(req, fp_hex, listed));
        });
    }

    fn mgmt_port(&self, fp_hex: &str) -> u16 {
        self.store
            .hosts()
            .find_by_fp(fp_hex)
            .map_or(pf_client_core::library::DEFAULT_MGMT_PORT, |h| {
                h.effective_mgmt_port()
            })
    }

    pub(super) fn profile_asked(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        switch: bool,
        listed: Option<Option<Vec<ProfileRow>>>,
        sender: &ComponentSender<Self>,
    ) {
        let saved = self
            .store
            .hosts()
            .find_by_fp(&fp_hex)
            .and_then(|h| h.profile.clone());
        if switch {
            let rows = listed.map(Option::unwrap_or_default);
            return self.profile_dialog(req, fp_hex, rows, saved, None, false, sender);
        }
        self.busy = false;
        self.hosts.emit(HostsMsg::SetSession(None));
        // A failed or late ask dials with the link's `as=`, else the saved pick.
        let Some(listed) = listed else {
            let profile = req.profile.clone().map(Some);
            return sender.input(AppMsg::StartSession {
                req,
                fp_hex,
                tofu: false,
                opts: SpawnOpts {
                    profile,
                    ..SpawnOpts::default()
                },
            });
        };
        let d =
            profiles::picker_decision(listed.as_deref(), saved.as_ref(), req.profile.as_deref());
        if d.remember != saved {
            self.save_profile(&fp_hex, d.remember.clone());
        }
        if d.picker {
            let rows = Some(listed.unwrap_or_default());
            return self.profile_dialog(req, fp_hex, rows, d.remember, d.gone, true, sender);
        }
        let row = d
            .send
            .as_deref()
            .and_then(|id| listed.iter().flatten().find(|p| p.id == id));
        self.dial_profile(req, fp_hex, d.send, row, false, sender);
    }

    pub(super) fn profile_picked(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        row: ProfileRow,
        connect: bool,
        sender: &ComponentSender<Self>,
    ) {
        self.save_profile(&fp_hex, Some(row.pick()));
        if connect {
            let id = Some(row.id.clone());
            self.dial_profile(req, fp_hex, id, Some(&row), false, sender);
        }
    }

    /// Dials as profile `id` (`None` names none). `row` is its row in the list it came from:
    /// its seat decides between dialing now, waiting for the seat and stopping. `redial` marks
    /// the one retry after a refusal.
    fn dial_profile(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        id: Option<String>,
        row: Option<&ProfileRow>,
        redial: bool,
        sender: &ComponentSender<Self>,
    ) {
        match row.map(|r| (r, profiles::seat_gate(r))) {
            Some((row, SeatGate::Wake)) => {
                self.wait_for_seat(req, fp_hex, row, true, redial, sender)
            }
            Some((row, SeatGate::Wait { .. })) => {
                self.wait_for_seat(req, fp_hex, row, false, redial, sender)
            }
            Some((_, SeatGate::Refuse(line))) => self.hosts.emit(HostsMsg::ShowError(line)),
            Some((_, SeatGate::Dial)) | None => sender.input(AppMsg::StartSession {
                req,
                fp_hex,
                tofu: false,
                opts: SpawnOpts {
                    profile: Some(id),
                    redial,
                    ..SpawnOpts::default()
                },
            }),
        }
    }

    /// The host refused the profile the session dialed as unknown. A seat host also says so
    /// for a seat that went stale, so the list is read again first.
    pub(super) fn reask_profile(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        id: String,
        msg: String,
        sender: &ComponentSender<Self>,
    ) {
        self.busy = true;
        self.hosts.emit(HostsMsg::SetSession(Some((
            req.card_key(),
            Phase::Connecting,
        ))));
        self.fetch_profiles(req, fp_hex, sender, move |req, fp_hex, listed| {
            AppMsg::ProfileReasked {
                req,
                fp_hex,
                id,
                msg,
                listed,
            }
        });
    }

    /// One more dial when the profile is still listed. Otherwise the refusal stands and the
    /// saved pick goes.
    pub(super) fn profile_reasked(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        id: String,
        msg: String,
        listed: Option<Option<Vec<ProfileRow>>>,
        sender: &ComponentSender<Self>,
    ) {
        self.busy = false;
        self.hosts.emit(HostsMsg::SetSession(None));
        let rows = listed.flatten().unwrap_or_default();
        match profiles::find(&rows, &id) {
            Some(row) => {
                let id = Some(row.id.clone());
                self.dial_profile(req, fp_hex, id, Some(row), true, sender);
            }
            None => {
                self.forget_profile(&req);
                self.hosts.emit(HostsMsg::ShowError(msg));
            }
        }
    }

    /// Opens the dialog that waits for `row`'s seat; `wake` starts a stopped seat first. A
    /// worker re-reads the seat every [`SEAT_POLL`] until [`AppMsg::SeatPolled`] ends the
    /// wait or the dialog's Cancel does.
    fn wait_for_seat(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        row: &ProfileRow,
        wake: bool,
        redial: bool,
        sender: &ComponentSender<Self>,
    ) {
        let detail = row.seat.as_ref().and_then(|s| s.detail.as_deref());
        let dialog = adw::AlertDialog::new(
            Some(&profiles::waking_line(&row.display_name)),
            detail.filter(|_| !wake),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.set_close_response("cancel");
        let stop = Arc::new(AtomicBool::new(false));
        let (out, flag) = (sender.input_sender().clone(), stop.clone());
        dialog.connect_response(None, move |_, _| {
            flag.store(true, Ordering::Relaxed);
            let _ = out.send(AppMsg::SeatCancelled(flag.clone()));
        });
        dialog.present(Some(&self.window));
        self.busy = true;
        self.hosts.emit(HostsMsg::SetSession(Some((
            req.card_key(),
            Phase::Connecting,
        ))));

        let mgmt = self.mgmt_port(&fp_hex);
        let (identity, pin) = (self.identity.clone(), trust::parse_hex32(&fp_hex));
        let (out, flag) = (sender.input_sender().clone(), stop.clone());
        let (addr, id, name) = (req.addr.clone(), row.id.clone(), row.display_name.clone());
        std::thread::spawn(move || {
            let post = |row| {
                let _ = out.send(AppMsg::SeatPolled {
                    stop: flag.clone(),
                    row,
                });
            };
            if wake {
                match profiles::wake(&addr, mgmt, &identity, pin, &id) {
                    Ok(row) => post(Ok(row)),
                    Err(e) => return post(Err(format!("Couldn't wake {name}'s desk — {e}"))),
                }
            }
            loop {
                std::thread::sleep(SEAT_POLL);
                if flag.load(Ordering::Relaxed) {
                    return;
                }
                match profiles::fetch_enumerate(&addr, mgmt, &identity, pin) {
                    Ok(rows) => match rows.and_then(|r| r.into_iter().find(|p| p.id == id)) {
                        Some(row) => post(Ok(row)),
                        None => return post(Err(format!("{name} is gone from this host."))),
                    },
                    // A poll that fails waits for the next one.
                    Err(e) => tracing::debug!(error = %e, "seat poll"),
                }
            }
        });
        self.seat_wait = Some(SeatWait {
            dialog,
            stop,
            req,
            fp_hex,
            id: row.id.clone(),
            redial,
        });
    }

    /// A poll's row: the dialog follows the seat, `ready` dials, `unavailable` stops. `Err`
    /// is a line that ends the wait.
    pub(super) fn seat_polled(
        &mut self,
        stop: Arc<AtomicBool>,
        row: Result<ProfileRow, String>,
        sender: &ComponentSender<Self>,
    ) {
        let Some(wait) = self
            .seat_wait
            .as_ref()
            .filter(|w| Arc::ptr_eq(&w.stop, &stop))
        else {
            return;
        };
        let gate = match &row {
            Ok(row) => profiles::seat_gate(row),
            Err(line) => SeatGate::Refuse(line.clone()),
        };
        match gate {
            SeatGate::Wait { detail } => wait.dialog.set_body(detail.as_deref().unwrap_or("")),
            SeatGate::Wake => {}
            SeatGate::Dial => {
                if let Some(w) = self.end_seat_wait() {
                    sender.input(AppMsg::StartSession {
                        req: w.req,
                        fp_hex: w.fp_hex,
                        tofu: false,
                        opts: SpawnOpts {
                            profile: Some(Some(w.id)),
                            redial: w.redial,
                            ..SpawnOpts::default()
                        },
                    });
                }
            }
            SeatGate::Refuse(line) => {
                self.end_seat_wait();
                self.hosts.emit(HostsMsg::ShowError(line));
            }
        }
    }

    /// The dialog closed by Cancel or Escape.
    pub(super) fn seat_cancelled(&mut self, stop: &Arc<AtomicBool>) {
        if self
            .seat_wait
            .as_ref()
            .is_some_and(|w| Arc::ptr_eq(&w.stop, stop))
        {
            self.end_seat_wait();
        }
    }

    /// Stops the poll, closes the dialog and frees the card.
    fn end_seat_wait(&mut self) -> Option<SeatWait> {
        let wait = self.seat_wait.take()?;
        wait.stop.store(true, Ordering::Relaxed);
        wait.dialog.close();
        self.busy = false;
        self.hosts.emit(HostsMsg::SetSession(None));
        Some(wait)
    }

    /// The host refused the saved profile: drop the pick so the next connect asks again.
    pub(super) fn forget_profile(&self, req: &ConnectRequest) {
        if let Some(fp_hex) = &req.fp_hex {
            self.save_profile(fp_hex, None);
        }
    }

    /// Writes the host's saved pick, keeping every other field of its record.
    fn save_profile(&self, fp_hex: &str, pick: Option<ProfilePick>) {
        let saved = self.store.update_hosts(|known| {
            if let Some(h) = known.hosts.iter_mut().find(|h| h.fp_hex == fp_hex) {
                h.profile = pick;
            }
        });
        if let Err(e) = saved {
            self.toast(&format!("Couldn't save the profile \u{2014} {e:#}"));
        }
    }

    /// The picker. `rows` is `None` when the list couldn't load. The saved pick comes first
    /// and stands out; a circle saves its profile and, for a connect, dials with it.
    #[allow(clippy::too_many_arguments)]
    fn profile_dialog(
        &self,
        req: ConnectRequest,
        fp_hex: String,
        rows: Option<Vec<ProfileRow>>,
        saved: Option<ProfilePick>,
        gone: Option<String>,
        connect: bool,
        sender: &ComponentSender<Self>,
    ) {
        let body = match (&rows, gone) {
            (None, _) => Some("Couldn't load the profiles.".to_string()),
            (Some(r), _) if r.is_empty() => Some("No profiles on this host.".to_string()),
            (_, Some(g)) => Some(format!("{g} is gone from this host.")),
            _ => None,
        };
        let dialog = adw::AlertDialog::new(
            Some(&format!("Who\u{2019}s playing on {}?", req.name)),
            body.as_deref(),
        );
        dialog.add_response("cancel", if rows.is_some() { "Cancel" } else { "Close" });
        dialog.set_close_response("cancel");
        let mut rows = rows.unwrap_or_default();
        if let Some(i) = saved
            .as_ref()
            .and_then(|s| rows.iter().position(|p| p.id == s.id))
        {
            rows[..=i].rotate_right(1);
        }
        let flow = gtk::FlowBox::new();
        flow.set_selection_mode(gtk::SelectionMode::None);
        flow.set_max_children_per_line(4);
        flow.set_halign(gtk::Align::Center);
        for p in rows {
            let card = gtk::Box::new(gtk::Orientation::Vertical, 4);
            card.append(&adw::Avatar::new(64, Some(&p.display_name), true));
            card.append(
                &gtk::Label::builder()
                    .label(&p.display_name)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(12)
                    .build(),
            );
            if let Some(note) = p.note() {
                card.append(
                    &gtk::Label::builder()
                        .label(note)
                        .css_classes(["caption", "dim-label"])
                        .wrap(true)
                        .max_width_chars(14)
                        .justify(gtk::Justification::Center)
                        .build(),
                );
            }
            let is_saved = saved.as_ref().is_some_and(|s| s.id == p.id);
            let button = gtk::Button::builder()
                .child(&card)
                .css_classes([if is_saved { "suggested-action" } else { "flat" }])
                .build();
            let (out, req, fp_hex, dialog) = (
                sender.input_sender().clone(),
                req.clone(),
                fp_hex.clone(),
                dialog.clone(),
            );
            button.connect_clicked(move |_| {
                let _ = out.send(AppMsg::ProfilePicked {
                    req: req.clone(),
                    fp_hex: fp_hex.clone(),
                    row: p.clone(),
                    connect,
                });
                dialog.close();
            });
            flow.append(&button);
        }
        dialog.set_extra_child(Some(&flow));
        dialog.present(Some(&self.window));
    }
}
