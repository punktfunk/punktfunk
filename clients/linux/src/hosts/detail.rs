//! The host page (design §2.4): everything about one saved host beyond the daily act, as
//! grouped rows pushed over the hosts page — presets, connection, pairing, power, support and
//! removal. Opened from a card's ⓘ or its Host Details… row. The hosts page refreshes it on
//! every change it draws; a row being edited keeps what was typed.

use super::card::os_icon_name;
use super::form::{parse_address, parse_macs, parse_port};
use super::model::{Preset, Status};
use super::{saved_request, Act, HostRef, HostsMsg};
use crate::store::{Changed, Store};
use crate::trust::{HostEdit, KnownHost, KnownHosts};
use adw::prelude::*;
use pf_client_core::host_actions::ActionInfo;
use pf_client_core::start::StartIn;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// What the page shows beside the host record, gathered by the hosts page.
pub struct Live {
    pub status: Status,
    pub online: bool,
    pub presets: Vec<Preset>,
    /// The host's own actions this device may run (sleep, restart, shut down).
    pub actions: Vec<ActionInfo>,
    pub is_default: bool,
    pub start_in: StartIn,
}

/// The power rows on screen: id, can run now, why not.
type ActionsShown = Vec<(String, bool, Option<String>)>;

/// State the row handlers share: the record they act on, and a switch that silences them
/// while `refresh` writes rows.
#[derive(Clone)]
struct Ctx {
    store: Rc<Store>,
    sender: relm4::Sender<HostsMsg>,
    host: Rc<RefCell<HostRef>>,
    current: Rc<RefCell<KnownHost>>,
    online: Rc<Cell<bool>>,
    quiet: Rc<Cell<bool>>,
}

impl Ctx {
    fn act(&self, act: Act) {
        self.sender.emit(HostsMsg::Act(act));
    }

    fn toast(&self, msg: impl Into<String>) {
        self.act(Act::Toast(msg.into()));
    }

    fn request(&self) -> super::ConnectRequest {
        saved_request(&self.current.borrow())
    }

    /// Apply `f` to this record in the store; a failed save says so.
    fn edit(&self, f: impl FnOnce(&mut KnownHost)) {
        let host = self.host.borrow().clone();
        let saved = self.store.update_hosts(|known| {
            if let Some(i) = host.index(known) {
                f(&mut known.hosts[i]);
            }
        });
        if let Err(e) = saved {
            self.toast(format!("Couldn't save the host \u{2014} {e:#}"));
        }
    }
}

pub struct DetailPage {
    pub page: adw::NavigationPage,
    ctx: Ctx,
    avatar: adw::Avatar,
    title: gtk::Label,
    status: gtk::Label,
    connect: gtk::Button,
    library: gtk::Button,
    presets_group: adw::PreferencesGroup,
    presets_shown: RefCell<Option<Vec<Preset>>>,
    preset_rows: RefCell<Vec<gtk::Widget>>,
    binding: RefCell<Option<(adw::ComboRow, Vec<String>)>>,
    pins: RefCell<Vec<(String, adw::SwitchRow)>>,
    name_row: adw::EntryRow,
    addr_row: adw::EntryRow,
    port_row: adw::EntryRow,
    mac_row: adw::EntryRow,
    mgmt_row: adw::ActionRow,
    clipboard_row: adw::SwitchRow,
    speed_row: adw::ActionRow,
    pair_row: adw::ActionRow,
    default_row: adw::SwitchRow,
    forget_row: adw::ActionRow,
    power_group: adw::PreferencesGroup,
    wake_row: adw::ActionRow,
    actions_shown: RefCell<Option<ActionsShown>>,
    action_rows: RefCell<Vec<adw::ActionRow>>,
    logs_row: adw::ActionRow,
}

impl DetailPage {
    /// Build the page for `k` and push it onto `nav`. The caller refreshes it straight away.
    pub fn open(
        nav: &adw::NavigationView,
        store: Rc<Store>,
        k: &KnownHost,
        sender: relm4::Sender<HostsMsg>,
    ) -> DetailPage {
        let ctx = Ctx {
            store,
            sender,
            host: Rc::new(RefCell::new(HostRef::of(k))),
            current: Rc::new(RefCell::new(k.clone())),
            online: Rc::new(Cell::new(false)),
            quiet: Rc::new(Cell::new(false)),
        };
        let prefs = adw::PreferencesPage::new();

        // ---- Header: which machine, can I use it now, the two daily acts ----
        let avatar = adw::Avatar::new(64, Some(&k.name), true);
        let title = gtk::Label::builder()
            .css_classes(["title-2"])
            .wrap(true)
            .justify(gtk::Justification::Center)
            .build();
        let status = gtk::Label::builder().css_classes(["dim-label"]).build();
        let connect = gtk::Button::builder()
            .label("Connect")
            .css_classes(["pill", "suggested-action"])
            .build();
        let library = gtk::Button::builder()
            .label("Browse Library")
            .css_classes(["pill"])
            .build();
        {
            let ctx = ctx.clone();
            connect.connect_clicked(move |_| {
                let req = ctx.request();
                ctx.act(if !ctx.online.get() && !req.mac.is_empty() {
                    Act::WakeConnect(req)
                } else {
                    Act::Connect(req)
                });
            });
        }
        {
            let ctx = ctx.clone();
            library.connect_clicked(move |_| ctx.act(Act::Library(ctx.request())));
        }
        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        buttons.set_halign(gtk::Align::Center);
        buttons.set_margin_top(12);
        buttons.append(&connect);
        buttons.append(&library);
        let header = gtk::Box::new(gtk::Orientation::Vertical, 6);
        header.append(&avatar);
        header.append(&title);
        header.append(&status);
        header.append(&buttons);
        let header_group = adw::PreferencesGroup::new();
        header_group.add(&header);
        prefs.add(&header_group);

        // ---- Presets: rows built by `refresh`, which knows the catalog ----
        let presets_group = adw::PreferencesGroup::builder().title("Presets").build();
        prefs.add(&presets_group);

        // ---- Connection ----
        let connection = adw::PreferencesGroup::builder().title("Connection").build();
        let entry = |title: &str| {
            adw::EntryRow::builder()
                .title(title)
                .show_apply_button(true)
                .build()
        };
        let name_row = entry("Name");
        let addr_row = entry("Address");
        let port_row = entry("Port");
        port_row.set_input_purpose(gtk::InputPurpose::Digits);
        let mac_row = entry("Wake-on-LAN MAC addresses");
        mac_row.set_tooltip_text(Some(
            "Wakes this host from sleep. Learned from the network while it is on; separate \
             several with commas.",
        ));
        wire_name(&ctx, &name_row);
        wire_address(&ctx, &addr_row);
        wire_port(&ctx, &port_row);
        wire_macs(&ctx, &mac_row);
        let mgmt_row = adw::ActionRow::builder().title("Management port").build();
        mgmt_row.add_css_class("property");
        let clipboard_row = adw::SwitchRow::builder()
            .title("Share clipboard")
            .subtitle("Copy and paste between this machine and that host")
            .build();
        {
            let ctx = ctx.clone();
            clipboard_row.connect_active_notify(move |row| {
                if !ctx.quiet.get() {
                    let on = row.is_active();
                    ctx.edit(|h| h.clipboard_sync = on);
                }
            });
        }
        let link_row = action_row(
            "Copy Link",
            "A punktfunk:// link that connects to this host",
        );
        {
            let ctx = ctx.clone();
            link_row.connect_activated(move |_| {
                ctx.act(Act::CopyLink {
                    host: ctx.host.borrow().clone(),
                    preset: None,
                })
            });
        }
        let shortcut_row = action_row(
            "Create Shortcut\u{2026}",
            "An app launcher entry that connects to this host",
        );
        {
            let ctx = ctx.clone();
            shortcut_row.connect_activated(move |_| {
                ctx.act(Act::CreateShortcut {
                    host: ctx.host.borrow().clone(),
                    preset: None,
                })
            });
        }
        let speed_row = action_row("Test Network Speed\u{2026}", "");
        {
            let ctx = ctx.clone();
            speed_row.connect_activated(move |_| ctx.act(Act::SpeedTest(ctx.request())));
        }
        for row in [&name_row, &addr_row, &port_row, &mac_row] {
            connection.add(row);
        }
        connection.add(&mgmt_row);
        connection.add(&clipboard_row);
        connection.add(&link_row);
        connection.add(&shortcut_row);
        connection.add(&speed_row);
        prefs.add(&connection);

        // ---- Pairing ----
        let pairing = adw::PreferencesGroup::builder().title("Pairing").build();
        let pair_row = adw::ActionRow::new();
        pair_row.add_css_class("property");
        let pin_row = action_row(
            "Pair With PIN\u{2026}",
            "Enter the PIN the host shows to pair this device",
        );
        {
            let ctx = ctx.clone();
            pin_row.connect_activated(move |_| ctx.act(Act::Pair(ctx.request())));
        }
        let default_row = adw::SwitchRow::builder().title("Default host").build();
        {
            let ctx = ctx.clone();
            default_row.connect_active_notify(move |row| {
                if ctx.quiet.get() {
                    return;
                }
                let (on, id) = (row.is_active(), ctx.current.borrow().id.clone());
                ctx.store.update_settings(|s| {
                    if on {
                        s.default_host = id;
                    } else if s.default_host == id {
                        s.default_host = None;
                    }
                });
            });
        }
        let forget_row = action_row(
            "Forget Identity\u{2026}",
            "Keeps the host saved; it has to be paired again to connect",
        );
        forget_row.add_css_class("error");
        {
            let ctx = ctx.clone();
            forget_row.connect_activated(move |row| forget_identity(&ctx, row));
        }
        pairing.add(&pair_row);
        pairing.add(&pin_row);
        pairing.add(&default_row);
        pairing.add(&forget_row);
        prefs.add(&pairing);

        // ---- Power: wake, then the host's own rows (built by `refresh`) ----
        let power_group = adw::PreferencesGroup::builder().title("Power").build();
        let wake_row = action_row("Wake Host", "");
        {
            let ctx = ctx.clone();
            wake_row.connect_activated(move |_| {
                let k = ctx.current.borrow();
                ctx.act(Act::Wake {
                    mac: k.mac.clone(),
                    addr: k.addr.clone(),
                });
            });
        }
        power_group.add(&wake_row);
        prefs.add(&power_group);

        // ---- Support ----
        let support = adw::PreferencesGroup::builder().title("Support").build();
        let logs_row = action_row("Send Logs to Host", "");
        {
            let ctx = ctx.clone();
            logs_row.connect_activated(move |_| ctx.act(Act::SendLogs(ctx.request())));
        }
        support.add(&logs_row);
        prefs.add(&support);

        // ---- Remove, last ----
        let remove_group = adw::PreferencesGroup::new();
        let remove = adw::ButtonRow::builder().title("Remove Host").build();
        remove.add_css_class("destructive-action");
        {
            let (ctx, nav) = (ctx.clone(), nav.clone());
            remove.connect_activated(move |row| remove_host(&ctx, &nav, row));
        }
        remove_group.add(&remove);
        prefs.add(&remove_group);

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(&prefs));
        let page = adw::NavigationPage::builder()
            .title(&k.name)
            .tag("host")
            .child(&toolbar)
            .build();
        nav.push(&page);

        DetailPage {
            page,
            ctx,
            avatar,
            title,
            status,
            connect,
            library,
            presets_group,
            presets_shown: RefCell::new(None),
            preset_rows: RefCell::default(),
            binding: RefCell::new(None),
            pins: RefCell::default(),
            name_row,
            addr_row,
            port_row,
            mac_row,
            mgmt_row,
            clipboard_row,
            speed_row,
            pair_row,
            default_row,
            forget_row,
            power_group,
            wake_row,
            actions_shown: RefCell::new(None),
            action_rows: RefCell::default(),
            logs_row,
        }
    }

    /// The record this page is about.
    pub fn host(&self) -> HostRef {
        self.ctx.host.borrow().clone()
    }

    /// Show `k` as it is now. Rows being edited keep their text; handlers stay quiet.
    pub fn refresh(&self, k: &KnownHost, live: &Live) {
        self.ctx.quiet.set(true);
        *self.ctx.current.borrow_mut() = k.clone();
        *self.ctx.host.borrow_mut() = HostRef::of(k);
        self.ctx.online.set(live.online);
        let pinned = !k.fp_hex.is_empty();

        self.page.set_title(&k.name);
        self.title.set_label(&k.name);
        self.avatar.set_text(Some(&k.name));
        if let Some(icon) = os_icon_name(&k.os) {
            self.avatar.set_show_initials(false);
            self.avatar.set_icon_name(Some(&icon));
        }
        self.status.set_label(&live.status.sentence());
        self.connect
            .set_label(if matches!(live.status, Status::Playing(_)) {
                "Resume"
            } else {
                "Connect"
            });
        // One session at a time: the window's banner ends the running one.
        self.connect.set_sensitive(!matches!(
            live.status,
            Status::Connecting | Status::Streaming
        ));
        self.library.set_visible(k.paired);

        self.refresh_presets(k, &live.presets);

        set_if_idle(&self.name_row, &k.name);
        set_if_idle(&self.addr_row, &k.addr);
        set_if_idle(&self.port_row, &k.port.to_string());
        set_if_idle(&self.mac_row, &k.mac.join(", "));
        self.mgmt_row.set_subtitle(&match k.mgmt_port {
            Some(p) => p.to_string(),
            None => format!("{} (default)", pf_client_core::library::DEFAULT_MGMT_PORT),
        });
        self.clipboard_row.set_active(k.clipboard_sync);
        gate(
            &self.speed_row,
            pinned,
            "Measures the link and recommends a bitrate",
            "Pair first",
        );

        let (state, detail) = match (pinned, k.paired) {
            (true, true) => ("Paired", short_fp(&k.fp_hex)),
            (true, false) => ("Trusted on first use", short_fp(&k.fp_hex)),
            (false, _) => ("Not paired", "Click the card to pair it".to_string()),
        };
        self.pair_row.set_title(state);
        self.pair_row.set_subtitle(&detail);
        self.default_row.set_visible(k.paired);
        self.default_row.set_active(live.is_default);
        self.default_row.set_subtitle(match live.start_in {
            StartIn::Hosts => "Used when Start in opens the Library or a stream",
            StartIn::Library => "Punktfunk opens on its library",
            StartIn::Stream => "Punktfunk connects to it on launch",
        });
        self.forget_row.set_visible(pinned);

        let wake_reason = if live.online {
            "The host is on"
        } else if k.mac.is_empty() {
            "Add its MAC address under Connection to wake it"
        } else {
            "Sends a Wake-on-LAN packet"
        };
        self.wake_row.set_subtitle(wake_reason);
        self.wake_row
            .set_sensitive(!live.online && !k.mac.is_empty());
        self.refresh_actions(&live.actions, live.online);

        let logs_reason = if !k.paired {
            "Pair first"
        } else if !live.online {
            "The host is offline"
        } else {
            "Uploads this device's recent log to the host's web console"
        };
        self.logs_row.set_subtitle(logs_reason);
        self.logs_row.set_sensitive(k.paired && live.online);
        self.ctx.quiet.set(false);
    }

    /// "Connect with" picks the host's binding; a switch per preset pins its own card.
    fn refresh_presets(&self, k: &KnownHost, presets: &[Preset]) {
        if self.presets_shown.borrow().as_deref() != Some(presets) {
            for row in self.preset_rows.borrow_mut().drain(..) {
                self.presets_group.remove(&row);
            }
            *self.binding.borrow_mut() = None;
            self.pins.borrow_mut().clear();
            if presets.is_empty() {
                let row = action_row(
                    "No presets yet",
                    "Make one in Preferences to connect with other settings",
                );
                row.set_action_name(Some("win.preferences"));
                self.presets_group.add(&row);
                self.preset_rows.borrow_mut().push(row.upcast());
            } else {
                let mut labels = vec!["Default settings"];
                labels.extend(presets.iter().map(|p| p.name.as_str()));
                let ids: Vec<String> = std::iter::once(String::new())
                    .chain(presets.iter().map(|p| p.id.clone()))
                    .collect();
                let combo = adw::ComboRow::builder()
                    .title("Connect with")
                    .subtitle("What a click on its card uses")
                    .model(&gtk::StringList::new(&labels))
                    .build();
                {
                    let (ctx, ids) = (self.ctx.clone(), ids.clone());
                    combo.connect_selected_notify(move |row| {
                        if ctx.quiet.get() {
                            return;
                        }
                        let id = ids.get(row.selected() as usize).filter(|i| !i.is_empty());
                        let id = id.cloned();
                        ctx.edit(|h| h.preset_id = id);
                    });
                }
                self.presets_group.add(&combo);
                self.preset_rows.borrow_mut().push(combo.clone().upcast());
                for p in presets {
                    let row = adw::SwitchRow::builder()
                        .title(format!("Pin \u{201c}{}\u{201d} as a card", p.name))
                        .build();
                    {
                        let (ctx, id) = (self.ctx.clone(), p.id.clone());
                        row.connect_active_notify(move |row| {
                            if ctx.quiet.get() {
                                return;
                            }
                            let on = row.is_active();
                            ctx.edit(|h| {
                                h.pinned_presets.retain(|p| p != &id);
                                if on {
                                    h.pinned_presets.push(id.clone());
                                }
                            });
                        });
                    }
                    self.presets_group.add(&row);
                    self.preset_rows.borrow_mut().push(row.clone().upcast());
                    self.pins.borrow_mut().push((p.id.clone(), row));
                }
                *self.binding.borrow_mut() = Some((combo, ids));
            }
            *self.presets_shown.borrow_mut() = Some(presets.to_vec());
        }
        if let Some((combo, ids)) = self.binding.borrow().as_ref() {
            // A binding whose preset is gone reads as Default settings, as it connects.
            let at = k
                .preset_id
                .as_ref()
                .and_then(|id| ids.iter().position(|i| i == id))
                .unwrap_or(0);
            combo.set_selected(at as u32);
        }
        for (id, row) in self.pins.borrow().iter() {
            row.set_active(k.pinned_presets.contains(id));
        }
    }

    /// One row per action the host grants this device; one it cannot run now stays, disabled,
    /// with the host's reason.
    fn refresh_actions(&self, actions: &[ActionInfo], online: bool) {
        let shown: ActionsShown = actions
            .iter()
            .map(|a| {
                (
                    a.id.clone(),
                    a.available && online,
                    a.unavailable_reason.clone(),
                )
            })
            .collect();
        if self.actions_shown.borrow().as_ref() == Some(&shown) {
            return;
        }
        for row in self.action_rows.borrow_mut().drain(..) {
            self.power_group.remove(&row);
        }
        for a in actions {
            let reason = match (&a.unavailable_reason, online) {
                (_, false) => "The host is offline".to_string(),
                (Some(r), true) if !a.available => r.clone(),
                _ => String::new(),
            };
            let row = action_row(a.label(), &reason);
            row.set_sensitive(a.available && online);
            {
                let (ctx, id, label, danger) = (
                    self.ctx.clone(),
                    a.id.clone(),
                    a.label().to_string(),
                    a.danger,
                );
                row.connect_activated(move |_| {
                    ctx.act(Act::HostAction {
                        req: ctx.request(),
                        action_id: id.clone(),
                        label: label.clone(),
                        danger,
                    })
                });
            }
            self.power_group.add(&row);
            self.action_rows.borrow_mut().push(row);
        }
        *self.actions_shown.borrow_mut() = Some(shown);
    }
}

fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .activatable(true)
        .build();
    if !subtitle.is_empty() {
        row.set_subtitle(subtitle);
    }
    row.add_suffix(&crate::widgets::lucide::row_icon("chevron-right"));
    row
}

/// A row the host must be paired for: enabled with its purpose, or disabled with the reason.
fn gate(row: &adw::ActionRow, open: bool, purpose: &str, reason: &str) {
    row.set_sensitive(open);
    row.set_subtitle(if open { purpose } else { reason });
}

/// Only when the row is not being typed in and shows something else.
fn set_if_idle(row: &adw::EntryRow, text: &str) {
    let editing = row.has_focus() || row.focus_child().is_some();
    if !editing && row.text() != text {
        row.set_text(text);
    }
}

/// `a4b1c2d3…e4f5`: enough to compare with the host's own display, short enough to read.
fn short_fp(fp: &str) -> String {
    if fp.len() < 12 {
        return fp.to_string();
    }
    format!("{}\u{2026}{}", &fp[..8], &fp[fp.len() - 4..])
}

/// A typed value the store refuses goes back to the stored one, and the toast says why.
fn refuse(ctx: &Ctx, row: &adw::EntryRow, stored: &str, why: String) {
    ctx.toast(why);
    row.set_text(stored);
}

fn wire_name(ctx: &Ctx, row: &adw::EntryRow) {
    let ctx = ctx.clone();
    row.connect_apply(move |row| {
        let name = row.text().trim().to_string();
        if name.is_empty() {
            let stored = ctx.current.borrow().name.clone();
            return refuse(&ctx, row, &stored, "Enter a name for the host.".into());
        }
        ctx.edit(|h| {
            h.apply_edit(&HostEdit {
                name: Some(name),
                ..Default::default()
            });
        });
    });
}

fn wire_address(ctx: &Ctx, row: &adw::EntryRow) {
    let ctx = ctx.clone();
    row.connect_apply(move |row| match parse_address(&row.text()) {
        Ok((addr, port)) => ctx.edit(|h| {
            h.apply_edit(&HostEdit {
                addr: Some(addr),
                port,
                ..Default::default()
            });
        }),
        Err(why) => {
            let stored = ctx.current.borrow().addr.clone();
            refuse(&ctx, row, &stored, why);
        }
    });
}

fn wire_port(ctx: &Ctx, row: &adw::EntryRow) {
    let ctx = ctx.clone();
    row.connect_apply(move |row| match parse_port(&row.text()) {
        Ok(port) => ctx.edit(|h| {
            h.apply_edit(&HostEdit {
                port: Some(port),
                ..Default::default()
            });
        }),
        Err(why) => {
            let stored = ctx.current.borrow().port.to_string();
            refuse(&ctx, row, &stored, why);
        }
    });
}

fn wire_macs(ctx: &Ctx, row: &adw::EntryRow) {
    let ctx = ctx.clone();
    row.connect_apply(move |row| match parse_macs(&row.text()) {
        Ok(macs) => ctx.edit(|h| {
            h.apply_edit(&HostEdit {
                macs: Some(macs),
                ..Default::default()
            });
        }),
        Err(why) => {
            let stored = ctx.current.borrow().mac.join(", ");
            refuse(&ctx, row, &stored, why);
        }
    });
}

fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    verb: &str,
    on_yes: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel"), ("go", verb)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(Some("go"), move |_, _| on_yes());
    dialog.present(Some(parent));
}

/// Drop the pin and keep the record: the next connect pairs again. For a host reinstalled with
/// a new certificate, or one that should stop being trusted without losing its setup.
fn forget_identity(ctx: &Ctx, anchor: &adw::ActionRow) {
    let name = ctx.current.borrow().name.clone();
    let ctx = ctx.clone();
    confirm(
        anchor,
        &format!("Forget {name}'s identity?"),
        "It stays saved with its name, address and presets, but has to be paired again \
         before it connects.",
        "Forget",
        move || {
            let fp = ctx.current.borrow().fp_hex.clone();
            ctx.edit(|h| {
                h.fp_hex.clear();
                h.paired = false;
            });
            pf_client_core::library_cache::forget(&fp);
            pf_client_core::host_actions::invalidate(&fp);
        },
    );
}

/// Remove the record, its cached library and a default pointing at it, then leave its page.
fn remove_host(ctx: &Ctx, nav: &adw::NavigationView, anchor: &adw::ButtonRow) {
    let name = ctx.current.borrow().name.clone();
    let (ctx, nav) = (ctx.clone(), nav.clone());
    confirm(
        anchor,
        &format!("Remove {name}?"),
        "It has to be added and paired again to connect.",
        "Remove",
        move || {
            // `forget_host` saves the hosts file and clears a default pointing at the host.
            let mut known = KnownHosts::read();
            let host = ctx.host.borrow().clone();
            if let Some(i) = host.index(&known) {
                if let Err(e) = pf_client_core::orchestrate::forget_host(&mut known, i) {
                    ctx.toast(format!("Couldn't remove the host \u{2014} {e:#}"));
                    return;
                }
            }
            ctx.store.reload(Changed::Hosts);
            ctx.store.reload(Changed::Settings);
            nav.pop_to_tag("main");
        },
    );
}
