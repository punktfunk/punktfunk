//! The password prompt in front of a dial to a profile on a person's own account, and its
//! return when the host refuses the unlock. The rules are `pf_client_core::unlock`. Linux has
//! no device keys, so the prompt's one switch is "Stay signed in on this device".

use super::*;
use pf_client_core::profiles;
use pf_client_core::unlock::{self, Gate};
use punktfunk_core::quic::v2::msg::Zeroizing;

impl AppModel {
    /// Dials as `id` (`None` names none) once `row`'s unlock allows: the password prompt
    /// first where the profile needs one, else at once. `redial` marks the one retry after an
    /// unknown-profile refusal.
    pub(super) fn dial_unlocked(
        &self,
        req: ConnectRequest,
        fp_hex: String,
        id: Option<String>,
        row: Option<&ProfileRow>,
        redial: bool,
        sender: &ComponentSender<Self>,
    ) {
        if let Some(row) = row
            && let Gate::Prompt { error } = unlock::before_dial(row.unlock, false)
        {
            return self.unlock_prompt(req, fp_hex, row.clone(), redial, error, sender);
        }
        sender.input(AppMsg::StartSession {
            req,
            fp_hex,
            tofu: false,
            opts: SpawnOpts {
                profile: Some(id),
                redial,
                ..SpawnOpts::default()
            },
        });
    }

    /// The host closed profile `id`'s dial with `ProfileLocked`; `sent`: it carried an
    /// unlock. Its list is read again for the profile's row, then the prompt opens with `error`.
    pub(super) fn reask_unlock(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        id: String,
        error: Option<String>,
        sent: bool,
        sender: &ComponentSender<Self>,
    ) {
        let key = req.card_key();
        self.flights.hold(&key);
        self.hosts
            .emit(HostsMsg::SetSession(key, Some(Phase::Connecting)));
        self.fetch_profiles(req, fp_hex, sender, move |req, fp_hex, listed| {
            AppMsg::UnlockReasked {
                req,
                fp_hex,
                id,
                error,
                sent,
                listed,
            }
        });
    }

    /// The prompt for the refused profile ([`unlock::after_refusal`]; without device keys it
    /// is always the prompt). One the host no longer lists, or a list that didn't load,
    /// leaves the refusal on the banner.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn unlock_reasked(
        &mut self,
        req: ConnectRequest,
        fp_hex: String,
        id: String,
        error: Option<String>,
        sent: bool,
        listed: Option<Option<Vec<ProfileRow>>>,
        sender: &ComponentSender<Self>,
    ) {
        let key = req.card_key();
        self.flights.release(&key);
        self.hosts.emit(HostsMsg::SetSession(key, None));
        let rows = listed.flatten().unwrap_or_default();
        match profiles::find(&rows, &id) {
            Some(row) => {
                let error = match unlock::after_refusal(row.unlock, None, sent, error) {
                    Gate::Prompt { error } => error,
                    Gate::Dial | Gate::Key => None,
                };
                self.unlock_prompt(req, fp_hex, row.clone(), false, error, sender)
            }
            None => self
                .hosts
                .emit(HostsMsg::ShowError(error.unwrap_or_else(|| {
                    trust::connect_reject_message(
                        punktfunk_core::reject::RejectReason::ProfileLocked,
                    )
                }))),
        }
    }

    /// The prompt: a masked field, the one switch (off), and `error` from the host above the
    /// field. Sign In spawns the session with the unlock on its stdin; Cancel and Escape dial
    /// nothing.
    pub(super) fn unlock_prompt(
        &self,
        req: ConnectRequest,
        fp_hex: String,
        row: ProfileRow,
        redial: bool,
        error: Option<String>,
        sender: &ComponentSender<Self>,
    ) {
        let Prompt {
            dialog,
            entry,
            stay,
        } = prompt(&row.display_name, &req.host.name, error.as_deref());
        let out = sender.input_sender().clone();
        dialog.connect_response(Some("unlock"), move |_, _| {
            let password = take_password(&entry);
            let msg = match unlock::prompt_unlock(password, stay.is_active(), None) {
                Ok(u) => AppMsg::StartSession {
                    req: req.clone(),
                    fp_hex: fp_hex.clone(),
                    tofu: false,
                    opts: SpawnOpts {
                        profile: Some(Some(row.id.clone())),
                        redial,
                        unlock: Some(u),
                    },
                },
                Err(line) => AppMsg::UnlockPrompt {
                    req: req.clone(),
                    fp_hex: fp_hex.clone(),
                    row: row.clone(),
                    redial,
                    error: Some(line.to_string()),
                },
            };
            let _ = out.send(msg);
        });
        dialog.present(Some(&self.window));
    }
}

/// The prompt's dialog and the two widgets Sign In reads.
struct Prompt {
    dialog: adw::AlertDialog,
    entry: gtk::PasswordEntry,
    stay: gtk::Switch,
}

/// The dialog for profile `name` on `host`. Its responses are `cancel`, also its close
/// response, and `unlock`.
fn prompt(name: &str, host: &str, error: Option<&str>) -> Prompt {
    let fields = gtk::Box::new(gtk::Orientation::Vertical, 6);
    if let Some(error) = error {
        fields.append(
            &gtk::Label::builder()
                .label(error)
                .wrap(true)
                .xalign(0.0)
                .css_classes(["error"])
                .build(),
        );
    }
    let caption = gtk::Label::builder()
        .label("Password")
        .halign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();
    let entry = gtk::PasswordEntry::builder()
        .show_peek_icon(true)
        .activates_default(true)
        .build();
    entry.update_relation(&[gtk::accessible::Relation::LabelledBy(&[
        caption.upcast_ref()
    ])]);
    fields.append(&caption);
    fields.append(&entry);
    let stay_label = gtk::Label::builder()
        .label("Stay signed in on this device")
        .hexpand(true)
        .xalign(0.0)
        .wrap(true)
        .build();
    let stay = gtk::Switch::builder().valign(gtk::Align::Center).build();
    stay.update_relation(&[gtk::accessible::Relation::LabelledBy(&[
        stay_label.upcast_ref()
    ])]);
    let stay_line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    stay_line.set_margin_top(6);
    stay_line.append(&stay_label);
    stay_line.append(&stay);
    fields.append(&stay_line);

    let dialog = adw::AlertDialog::new(
        Some(&format!("Sign in as {name}")),
        Some(&format!("Enter the password for this account on {host}.")),
    );
    dialog.set_extra_child(Some(&fields));
    dialog.add_responses(&[("cancel", "Cancel"), ("unlock", "Sign In")]);
    dialog.set_response_appearance("unlock", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("unlock"));
    dialog.set_close_response("cancel");
    dialog.set_focus(Some(&entry));
    Prompt {
        dialog,
        entry,
        stay,
    }
}

/// The entry's text, copied once into a wiping buffer, then cleared. GTK keeps a password
/// entry's text in its own wiping buffer; `text()` would leave an unwiped copy behind.
#[allow(unsafe_code)]
fn take_password(entry: &gtk::PasswordEntry) -> Zeroizing<Vec<u8>> {
    use gtk::glib::translate::ToGlibPtr;
    let editable: &gtk::Editable = entry.upcast_ref();
    // SAFETY: `gtk_editable_get_text` returns the editable's own NUL-terminated text, never
    // null, valid until the text changes. It is copied out before `set_text` changes it.
    let password = unsafe {
        let raw = gtk::ffi::gtk_editable_get_text(editable.to_glib_none().0);
        Zeroizing::new(std::ffi::CStr::from_ptr(raw).to_bytes().to_vec())
    };
    entry.set_text("");
    password
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// The field reads once, as typed, and is empty after. The switch starts off, and closing
    /// the prompt in code is its Cancel, which never reaches Sign In.
    #[test]
    #[ignore = "needs a Wayland/X display"]
    fn the_prompt_reads_its_password_once_and_cancel_sends_nothing() {
        assert!(gtk::init().is_ok(), "no display");
        adw::init().unwrap();
        let p = prompt("Kid", "Desk", Some("Wrong password."));
        assert!(!p.stay.is_active());
        p.entry.set_text(" s3cret pw ");
        assert_eq!(take_password(&p.entry).as_slice(), b" s3cret pw ");
        assert!(p.entry.text().is_empty());

        let seen = Rc::new(RefCell::new(Vec::new()));
        let log = seen.clone();
        p.dialog
            .connect_response(None, move |_, r| log.borrow_mut().push(r.to_string()));
        let window = gtk::Window::new();
        window.present();
        p.dialog.present(Some(&window));
        p.dialog.close();
        while gtk::glib::MainContext::default().iteration(false) {}
        assert_eq!(*seen.borrow(), ["cancel"]);
    }
}
