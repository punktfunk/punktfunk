//! The profile picker in front of a paired host's connect, and the rejections that follow.

use super::*;
use pf_client_core::profiles;
use pf_client_core::trust::connect_reject_message;
use punktfunk_core::reject::RejectReason;
use std::time::Duration;

/// The longest a connect waits for the host's profile list.
const ASK_BUDGET: Duration = Duration::from_secs(3);

/// The profile refusal a session's error line carries, if it is one. The line is the typed
/// reason's own sentence.
pub(super) fn profile_reject(msg: &str) -> Option<RejectReason> {
    use RejectReason as R;
    [
        R::ProfileUnknown,
        R::NoSeat,
        R::SeatOccupied,
        R::SeatUnavailable,
    ]
    .into_iter()
    .find(|r| connect_reject_message(*r) == msg)
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
        let mgmt = self
            .store
            .hosts()
            .find_by_fp(&fp_hex)
            .map_or(pf_client_core::library::DEFAULT_MGMT_PORT, |h| {
                h.effective_mgmt_port()
            });
        if !switch {
            self.busy = true;
            self.hosts.emit(HostsMsg::SetSession(Some((
                req.card_key(),
                Phase::Connecting,
            ))));
        }
        let (identity, out) = (self.identity.clone(), sender.input_sender().clone());
        let pin = trust::parse_hex32(&fp_hex);
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let addr = req.addr.clone();
            std::thread::spawn(move || {
                let _ = tx.send(profiles::fetch_enumerate(&addr, mgmt, &identity, pin));
            });
            let listed = rx.recv_timeout(ASK_BUDGET).ok().and_then(Result::ok);
            let _ = out.send(AppMsg::ProfileAsked {
                req,
                fp_hex,
                switch,
                listed,
            });
        });
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
        sender.input(AppMsg::StartSession {
            req,
            fp_hex,
            tofu: false,
            opts: SpawnOpts {
                profile: Some(d.send),
                ..SpawnOpts::default()
            },
        });
    }

    pub(super) fn profile_picked(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        pick: ProfilePick,
        connect: bool,
        sender: &ComponentSender<Self>,
    ) {
        self.save_profile(&fp_hex, Some(pick.clone()));
        if connect {
            sender.input(AppMsg::StartSession {
                req,
                fp_hex,
                tofu: false,
                opts: SpawnOpts {
                    profile: Some(Some(pick.id)),
                    ..SpawnOpts::default()
                },
            });
        }
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
            let (out, req, fp_hex, pick, dialog) = (
                sender.input_sender().clone(),
                req.clone(),
                fp_hex.clone(),
                p.pick(),
                dialog.clone(),
            );
            button.connect_clicked(move |_| {
                let _ = out.send(AppMsg::ProfilePicked {
                    req: req.clone(),
                    fp_hex: fp_hex.clone(),
                    pick: pick.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_four_profile_refusals_are_recognised() {
        for r in [
            RejectReason::ProfileUnknown,
            RejectReason::NoSeat,
            RejectReason::SeatOccupied,
            RejectReason::SeatUnavailable,
        ] {
            assert_eq!(profile_reject(&connect_reject_message(r)), Some(r));
        }
        let busy = connect_reject_message(RejectReason::Busy);
        assert_eq!(profile_reject(&busy), None);
    }
}
