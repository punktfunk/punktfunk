//! The hosts page's dialogs: edit, forget, add and create-shortcut.

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

    /// The host edit sheet — what belongs to the HOST, not the stream: its name, where it
    /// answers (address, port, Wake-on-LAN MACs), whether this machine shares its clipboard
    /// with it, which preset a plain click uses, and its pinned preset cards.
    pub(super) fn edit_host_dialog(
        &self,
        sender: &ComponentSender<Self>,
        id: Option<&str>,
        addr: &str,
        port: u16,
        current: &str,
    ) {
        let known = KnownHosts::load();
        let stored = known
            .index_of_card(id, addr, port)
            .and_then(|i| known.hosts.get(i))
            .cloned();
        let name_row = adw::EntryRow::builder().title("Name").build();
        name_row.set_text(current);
        let connection = ConnectionRows::new(
            addr,
            port,
            stored.as_ref().map_or(&[][..], |h| h.mac.as_slice()),
        );
        let clipboard_row = adw::SwitchRow::builder()
            .title("Share clipboard")
            .subtitle(
                "Copy and paste between this machine and that host. Per host \u{2014} handing a \
                 host your clipboard is a decision about that host.",
            )
            .build();
        clipboard_row.set_active(stored.as_ref().is_some_and(|h| h.clipboard_sync));

        // Preset picker: "Default settings" plus the catalog, seeded to the current binding.
        let catalog = pf_client_core::presets::PresetsFile::load();
        let mut labels = vec!["Default settings".to_string()];
        let mut ids: Vec<String> = vec![String::new()];
        for p in &catalog.presets {
            labels.push(p.name.clone());
            ids.push(p.id.clone());
        }
        let bound = stored.as_ref().and_then(|h| h.preset_id.clone());
        // A binding whose preset is gone reads as Default settings and is cleaned up on save
        // — the same "dangling resolves as none" rule the connect path follows.
        let selected = bound
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .unwrap_or(0);
        let preset_row = adw::ComboRow::builder()
            .title("Preset")
            .subtitle("The settings a plain click uses for this host")
            .model(&gtk::StringList::new(
                &labels.iter().map(String::as_str).collect::<Vec<_>>(),
            ))
            .build();
        preset_row.set_selected(selected as u32);

        // Pinned cards: which presets get their own one-click card for this host. They used
        // to be a third submenu on the card, which is what tipped that menu over — and this is
        // where they belong anyway, next to the default they sit beside (design §5.2a).
        let pin_rows: Vec<(String, adw::SwitchRow)> = catalog
            .presets
            .iter()
            .map(|p| {
                let row = adw::SwitchRow::builder()
                    .title(&p.name)
                    .subtitle("Show as its own card")
                    .build();
                row.set_active(
                    stored
                        .as_ref()
                        .is_some_and(|h| h.pinned_presets.iter().any(|id| id == &p.id)),
                );
                (p.id.clone(), row)
            })
            .collect();

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&name_row);
        connection.append_to(&list);
        list.append(&preset_row);
        list.append(&clipboard_row);
        for (_, row) in &pin_rows {
            list.append(row);
        }

        let dialog = adw::AlertDialog::new(Some("Edit Host"), None);
        dialog.set_extra_child(Some(&connection.framed(&list)));
        dialog.add_responses(&[("cancel", "Cancel"), ("save", "Save")]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");
        connection.enable_when_valid(&dialog, "save");
        {
            let sender = sender.clone();
            let (id, addr, port) = (id.map(str::to_string), addr.to_string(), port);
            dialog.connect_response(Some("save"), move |_, _| {
                // The response is only enabled while the rows parse.
                let Some(edit) = connection.edit() else {
                    return;
                };
                let edit = HostEdit {
                    name: Some(name_row.text().to_string()),
                    ..edit
                };
                let mut known = KnownHosts::load();
                let target = known.index_of_card(id.as_deref(), &addr, port);
                if let Some(h) = target.and_then(|i| known.hosts.get_mut(i)) {
                    h.apply_edit(&edit);
                    h.clipboard_sync = clipboard_row.is_active();
                    h.preset_id = ids
                        .get(preset_row.selected() as usize)
                        .filter(|id| !id.is_empty())
                        .cloned();
                    // Rebuilt from the switches rather than toggled, so the card order follows
                    // the catalog and a preset deleted meanwhile simply drops out.
                    h.pinned_presets = pin_rows
                        .iter()
                        .filter(|(_, row)| row.is_active())
                        .map(|(id, _)| id.clone())
                        .collect();
                    if let Err(e) = known.save() {
                        let _ = sender.output(HostsOutput::Toast(format!("Couldn't save — {e:#}")));
                    }
                }
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }

    /// Forget this host (drops the pinned fingerprint — a later connect re-pairs).
    pub(super) fn forget_dialog(
        &self,
        sender: &ComponentSender<Self>,
        id: Option<&str>,
        addr: &str,
        port: u16,
        name: &str,
    ) {
        let dialog = adw::AlertDialog::new(
            Some("Remove saved host?"),
            Some(&format!(
                "Forget “{name}”? You'll need to pair (or trust) it again to reconnect."
            )),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        {
            let sender = sender.clone();
            let (id, addr, port) = (id.map(str::to_string), addr.to_string(), port);
            dialog.connect_response(Some("remove"), move |_, _| {
                let mut known = KnownHosts::load();
                if let Some(i) = known.index_of_card(id.as_deref(), &addr, port) {
                    if let Err(e) = pf_client_core::orchestrate::forget_host(&mut known, i) {
                        let _ = sender.output(HostsOutput::Toast(format!("Couldn't save — {e:#}")));
                    }
                }
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
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
            let sender = sender.clone();
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
                sender.input(HostsMsg::Refresh);
            });
        }
        dialog.present(Some(&self.widgets.stack));
    }
}
