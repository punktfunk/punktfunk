//! The hosts page's dialogs: add a host, and where a shortcut went.

use super::form::ConnectionRows;
use super::*;

impl HostsPage {
    /// Write the shortcut, or — inside the flatpak sandbox, which cannot reach
    /// `~/.local/share/applications` — hand the user the URL to place themselves. The
    /// DynamicLauncher portal is the intended upgrade for that case (design §5); until then
    /// the fallback is the one the design already sanctions, not a dead end.
    pub(super) fn shortcut_result(&self, sender: &ComponentSender<Self>, label: &str, url: &str) {
        if crate::desktop::shortcuts::sandboxed() {
            let dialog = adw::AlertDialog::new(
                Some("Create Shortcut"),
                Some(
                    "Punktfunk is sandboxed here, so it can't add the shortcut itself. Copy \
                     this link and make a launcher for it \u{2014} it opens the same stream.",
                ),
            );
            let entry = gtk::Entry::builder().text(url).editable(false).build();
            dialog.set_extra_child(Some(&entry));
            dialog.add_responses(&[("close", "Close"), ("copy", "Copy link")]);
            dialog.set_response_appearance("copy", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("copy"));
            dialog.set_close_response("close");
            {
                let url = url.to_string();
                dialog.connect_response(Some("copy"), move |_, _| {
                    if let Some(display) = gtk::gdk::Display::default() {
                        display.clipboard().set_text(&url);
                    }
                });
            }
            dialog.present(Some(&self.widgets.stack));
            return;
        }
        let msg = match crate::desktop::shortcuts::write_desktop_entry(label, url) {
            Ok(_) => format!("Shortcut for \u{201c}{label}\u{201d} added to your applications"),
            Err(e) => {
                tracing::warn!(error = %e, "writing the shortcut");
                format!("Couldn't create the shortcut \u{2014} {e}")
            }
        };
        let _ = sender.output(HostsOutput::Toast(msg));
    }

    /// "+": name (optional), address, port and Wake-on-LAN MACs. Add saves the host without
    /// dialing it, so a sleeping machine can be added with the MAC that wakes it; the first
    /// click on its card runs the trust gate.
    pub(super) fn add_host_dialog(&self, sender: &ComponentSender<Self>) {
        let list = gtk::ListBox::new();
        list.add_css_class("boxed-list");
        list.set_selection_mode(gtk::SelectionMode::None);
        let name_row = adw::EntryRow::builder().title("Name (optional)").build();
        list.append(&name_row);
        let connection = ConnectionRows::new("", 9777, &[]);
        connection.append_to(&list);
        list.set_size_request(320, -1);

        let dialog = adw::AlertDialog::new(Some("Add Host"), None);
        dialog.set_extra_child(Some(&connection.framed(&list)));
        dialog.add_responses(&[("cancel", "Cancel"), ("add", "Add")]);
        dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("add"));
        dialog.set_close_response("cancel");
        connection.enable_when_valid(&dialog, "add");
        {
            let (sender, store) = (sender.clone(), self.store.clone());
            dialog.connect_response(Some("add"), move |_, _| {
                let Some(edit) = connection.edit() else {
                    return;
                };
                let edit = HostEdit {
                    name: Some(name_row.text().to_string()),
                    ..edit
                };
                let msg = match trust::add_host(&edit) {
                    Ok(_) => {
                        let name = edit.name.as_deref().map(str::trim).unwrap_or_default();
                        let shown = if name.is_empty() {
                            edit.addr.as_deref().unwrap_or_default()
                        } else {
                            name
                        };
                        format!("Added {shown}. Click it to connect.")
                    }
                    Err(e) => format!("Couldn't save the host \u{2014} {e:#}"),
                };
                let _ = sender.output(HostsOutput::Toast(msg));
                store.reload(Changed::Hosts);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }
}
