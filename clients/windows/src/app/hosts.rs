//! The hosts page: saved (trusted/paired) hosts and live mDNS discovery as tap-to-connect
//! tiles in a responsive grid, with a per-host "…" menu (connect / speed test / edit /
//! forget) and a manual connect entry — the same card layout as the Linux and Apple clients.
//! The saved tiles follow the device's host sort and grouping (`host_order`).

use super::connect::{initiate, initiate_waking, open_console};
use super::library::open_library;
use super::speed::SpeedState;
use super::style::*;
use super::{saved, AsyncSetState, Screen, Svc, Target};
use crate::trust::{HostEdit, KnownHosts, Settings};
use pf_client_core::discovery::DiscoveredHost;
use pf_client_core::host_order::{self, Arrangeable};
use pf_client_core::orchestrate::HostTarget;
use pf_client_core::profiles::ProfilePick;
use pf_fluent::{Palette, Token};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use windows_reactor::*;

const MENU_CONNECT: &str = "Connect";
const MENU_LIBRARY: &str = "Browse library\u{2026}";
const MENU_SPEED: &str = "Test network speed\u{2026}";
/// Upload this device's recent log ring to the host (`logring::send_to_host`), where the web
/// console's Logs page lists it beside the host's own log. Paired + online only — the same
/// gate as the console UI's row, because the upload authenticates with the paired identity
/// and an offline host could only ever report an error.
const MENU_SEND_LOGS: &str = "Send logs to host";
const MENU_WAKE: &str = "Wake host";
/// The host's OWN actions — sleep / restart / shut down it (`design/host-actions.md` §7). The
/// rows come from what the HOST said it lets this device do, so a device without the
/// Host-power grant sees none, and a later host can add one without a client release.
const MENU_HOST_ACTION: &str = "\u{23fb} ";

/// One host action's menu label.
fn host_action_label(a: &pf_client_core::host_actions::ActionInfo) -> String {
    format!(
        "{MENU_HOST_ACTION}{}{}",
        a.label(),
        if a.available { "" } else { " (unavailable)" }
    )
}
/// One entry for every per-host property (name, address, MAC, clipboard sharing) — the
/// Apple client's add/edit sheet. A menu item per field read as clutter and buried the ones
/// that matter.
const MENU_EDIT: &str = "Edit\u{2026}";
/// Re-pick who plays on this host. Only where a pick is saved: a one-person box looks as before.
const MENU_PROFILE: &str = "Switch profile\u{2026}";
/// The per-preset families nest in submenus: "Connect with"'s leaves are the bare preset
/// names plus [`SUB_WITH_DEFAULT`], "Pin tiles"'s keep a verb prefix.
const SUB_WITH: &str = "Connect with";
const SUB_WITH_DEFAULT: &str = "Default settings";
const SUB_PIN: &str = "Pin tiles";
const MENU_COPY_LINK: &str = "Copy link";
const MENU_SHORTCUT: &str = "Create shortcut\u{2026}";
const MENU_PIN: &str = "Pin tile: ";
const MENU_UNPIN: &str = "Unpin tile: ";
const MENU_FORGET: &str = "Forget\u{2026}";
/// Point `Settings::default_host` at this host, or clear it.
const MENU_DEFAULT: &str = "Make default host";
const MENU_DEFAULT_SET: &str = "Default host \u{2713}";

/// Whether the console (gamepad) UI is available in this build: the session binary ships
/// its Skia `ui` feature on x64 only (no skia prebuilts for aarch64 yet) — the entry
/// points compile everywhere but only show where `--browse` can actually run.
pub(crate) const CONSOLE_UI_AVAILABLE: bool = cfg!(target_arch = "x86_64");

/// Tile-grid metrics: minimum tile width before dropping a column, and the gap between tiles.
const TILE_MIN_WIDTH: f64 = 320.0;
const TILE_GAP: f64 = 12.0;

/// Which saved record a per-host action means. NOT the fingerprint: a host added by address
/// has none, and keying on `""` matched the first such record — Forget removed the wrong set,
/// Edit and pinning wrote to the wrong host. `addr`/`port` are the fallback for a record older
/// than the minted ids, and are also why the id is the durable key: the edit sheet can change
/// them under itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct HostRef {
    pub(crate) id: Option<String>,
    pub(crate) addr: String,
    pub(crate) port: u16,
    pub(crate) name: String,
}

impl HostRef {
    fn of(h: &pf_client_core::trust::KnownHost) -> Self {
        Self {
            id: h.id.clone(),
            addr: h.addr.clone(),
            port: h.port,
            name: h.name.clone(),
        }
    }

    /// The record this names, if it is still in the store.
    fn index(&self, known: &KnownHosts) -> Option<usize> {
        known.index_of_card(self.id.as_deref(), &self.addr, self.port)
    }

    /// One string per record — the minted id, or addr:port for a record older than ids.
    fn seed_key(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.addr, self.port))
    }
}

/// The hosts page's props: the services, the root's discovery and probe state, and the root
/// state behind its sheets and dialogs (`forget`, `rename` = the host being edited,
/// `show_add`), which the page sets through the matching setters. Equal on the values alone.
#[derive(Clone)]
pub(crate) struct HostsProps {
    pub(crate) svc: Svc,
    pub(crate) hosts: Vec<DiscoveredHost>,
    /// Saved hosts proven reachable by the periodic QUIC probe, keyed by `KnownHost::card_key` —
    /// the whole of the Online pip. A routed host (Tailscale/VPN) that never advertises reads
    /// Online here, and a sleeping one whose advert has not aged out yet reads Offline.
    pub(crate) probed: HashMap<String, bool>,
    pub(crate) status: String,
    pub(crate) forget: Option<HostRef>,
    pub(crate) rename: Option<HostRef>,
    pub(crate) show_add: bool,
    /// The Add-host sheet's entrance (0 → 1): opacity and rise.
    pub(crate) add_anim: f64,
    /// The hovered tile's stable id (saved: fp_hex, discovered: `addr:port`).
    pub(crate) hover: Option<String>,
    /// Bumped when a menu action changes what the page should SHOW without changing any
    /// state it already reads — pinning/unpinning a preset tile, which rewrites the
    /// known-hosts store behind the tiles.
    pub(crate) hosts_rev: u64,
    /// The window's client width, which the tile columns follow.
    pub(crate) width: f64,
    pub(crate) set_forget: AsyncSetState<Option<HostRef>>,
    pub(crate) set_rename: AsyncSetState<Option<HostRef>>,
    pub(crate) set_show_add: AsyncSetState<bool>,
    pub(crate) set_hover: AsyncSetState<Option<String>>,
    pub(crate) set_hosts_rev: AsyncSetState<u64>,
}

impl PartialEq for HostsProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc
            && self.hosts == other.hosts
            && self.probed == other.probed
            && self.status == other.status
            && self.forget == other.forget
            && self.rename == other.rename
            && self.show_add == other.show_add
            && self.add_anim == other.add_anim
            && self.hover == other.hover
            && self.hosts_rev == other.hosts_rev
            && self.width == other.width
    }
}

/// A text box's live value and the value it opened with. The box shows `seed` and writes
/// every keystroke to `live`, which a click reads; re-declaring the live value would move
/// the caret on every redraw.
#[derive(Default)]
struct Draft<T> {
    seed: T,
    live: Rc<RefCell<T>>,
}

impl<T: Clone> Draft<T> {
    fn reset(&mut self, value: T) {
        *self.live.borrow_mut() = value.clone();
        self.seed = value;
    }
}

/// The host editor's drafts, read at Save time (see `edit_editor`).
#[derive(Default)]
struct EditDrafts {
    name: Draft<String>,
    addr: Draft<String>,
    port: Draft<String>,
    mac: Draft<String>,
    clip: Draft<bool>,
}

/// The hosts page. Its own state is the two sheets' drafts: the Add-host address and the
/// host editor's fields, seeded from the stored host when the edit target changes.
pub(crate) struct HostsPage {
    manual: Draft<String>,
    drafts: EditDrafts,
    /// Which record the editor's drafts were seeded from.
    seeded: Option<String>,
}

impl HostsPage {
    fn seed(&mut self, rename: Option<&HostRef>) {
        let key = rename.map(HostRef::seed_key);
        if self.seeded == key {
            return;
        }
        let stored = rename.and_then(|who| {
            let known = KnownHosts::load();
            who.index(&known).map(|i| known.hosts[i].clone())
        });
        let d = &mut self.drafts;
        d.name
            .reset(stored.as_ref().map(|h| h.name.clone()).unwrap_or_default());
        d.addr
            .reset(stored.as_ref().map(|h| h.addr.clone()).unwrap_or_default());
        d.port.reset(
            stored
                .as_ref()
                .map(|h| h.port.to_string())
                .unwrap_or_default(),
        );
        d.mac.reset(
            stored
                .as_ref()
                .map(|h| h.mac.join(", "))
                .unwrap_or_default(),
        );
        d.clip
            .reset(stored.as_ref().is_some_and(|h| h.clipboard_sync));
        self.seeded = key;
    }
}

impl Component for HostsPage {
    type Input = HostsProps;
    type Message = ();

    fn create(props: &HostsProps, _cx: &ComponentContext<Self>) -> Self {
        let mut page = HostsPage {
            manual: Draft::default(),
            drafts: EditDrafts::default(),
            seeded: None,
        };
        page.seed(props.rename.as_ref());
        page
    }

    fn input_changed(&mut self, props: &HostsProps, _cx: &ComponentContext<Self>) {
        self.seed(props.rename.as_ref());
        // The Add-host box opens on what was typed last time.
        if !props.show_add {
            let typed = self.manual.live.borrow().clone();
            self.manual.seed = typed;
        }
    }

    fn view(&self, props: &HostsProps, _cx: &mut ViewContext<'_, Self>) -> View {
        hosts_page(self, props)
    }
}

/// A host tile. The tap-to-connect summary (monogram, name, address, status row) and the
/// optional "…" menu button are SIBLINGS overlaid in one grid cell, never nested, so a click
/// on the menu never also connects.
///
/// Hover renders the WinUI card pointer-over look — the card background lifts to the control
/// hover fill while the pointer is inside the tile.
// Three call sites, each passing a different mix of the optional tail — grouping the eight into a
// props struct would make every one of them construct it inline for no reader gain.
#[allow(clippy::too_many_arguments)]
fn host_tile(
    pal: &Palette,
    id: &str,
    hover: &Hover,
    name: &str,
    // The host's OS-identity chain — the avatar's mark. Empty for a host that advertises
    // none, which falls the avatar back to the name's initial.
    os: &str,
    sub: &str,
    status_row: View,
    menu: Option<View>,
    tap: Box<dyn Fn()>,
) -> View {
    let summary = on_tap(
        Border::new()
            .content(vstack((
                avatar(pal, name, os).horizontal_alignment(HorizontalAlignment::Left),
                text(name)
                    .font_size(15.0)
                    .font_weight(FontWeight::SEMI_BOLD)
                    .text_wrapping(TextWrapping::Wrap)
                    .margin(edges(0.0, 12.0, 0.0, 0.0)),
                muted(pal, sub)
                    .font_size(12.0)
                    .font_family("Consolas")
                    .margin(edges(0.0, 2.0, 0.0, 0.0)),
                status_row,
            )))
            .background(hit_test_backstop())
            .padding(uniform(18.0)),
        tap,
    );
    let mut layers: Vec<View> = vec![summary.into()];
    layers.extend(menu);
    let mut tile = card_flush(Grid::new().children(layers));
    if hover.current.as_deref() == Some(id) {
        tile = tile.background(pal.get(Token::ControlFillSecondary));
    }
    let enter = {
        let (set, id) = (hover.set.clone(), id.to_string());
        move |_: PointerEventInfo| set.call(Some(id.clone()))
    };
    let exit = {
        let set = hover.set.clone();
        move |_: PointerEventInfo| set.call(None)
    };
    tile.on_pointer_entered(enter)
        .on_pointer_exited(exit)
        .into()
}

/// The hover-tracking pair `host_tile` needs: the currently hovered tile id + its root setter.
pub(crate) struct Hover {
    pub(crate) current: Option<String>,
    pub(crate) set: AsyncSetState<Option<String>>,
}

/// The status row at the bottom of a tile: presence dot + Online/Offline, plus a trust chip
/// only where it says something (see [`status_row_with`]).
fn status_row(pal: &Palette, online: Option<bool>, badge: Option<(&str, Pill)>) -> View {
    status_row_with(pal, online, badge, None, None)
}

/// [`status_row`] plus the preset: what a plain click on THIS tile will use — its own
/// preset on a pinned tile, the host's binding on the primary one. A binding whose preset
/// was deleted shows nothing and resolves as the defaults, which is what will happen on
/// connect (design §6).
///
/// The row is METADATA, not a badge shelf — three chips side by side read as noise. Paired
/// is the normal resting state of a saved host, so it earns NO chip at all; a chip appears
/// only where it carries a decision ("Trusted" = TOFU without pairing, "PIN"/"Open" on a
/// discovered host). The preset is a small dot in the preset's own colour plus its name
/// in plain caption text — recognisable at a glance without competing with the host name.
fn status_row_with(
    pal: &Palette,
    online: Option<bool>,
    badge: Option<(&str, Pill)>,
    preset: Option<(&str, Option<String>)>,
    profile: Option<(&ProfilePick, Option<EncodedImage>)>,
) -> View {
    let mut items: Vec<View> = Vec::new();
    if let Some(online) = online {
        items.push(
            presence_dot(pal, online)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
        items.push(
            muted(pal, if online { "Online" } else { "Offline" })
                .font_size(11.0)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    if let Some((badge, kind)) = badge {
        items.push(
            pill(pal, badge, kind)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    if let Some((name, accent)) = preset {
        // The preset's own colour where it has one, a neutral disc where it doesn't — the
        // palette stays opt-in, and an unparsable value falls back rather than being trusted.
        let colour = accent
            .as_deref()
            .and_then(super::settings::hex_color)
            .unwrap_or(Color::argb(120, 128, 128, 128));
        items.push(
            blank(8.0, 8.0)
                .background(colour)
                .corner_radius(4.0)
                .margin(edges(
                    if items.is_empty() { 0.0 } else { 4.0 },
                    0.0,
                    0.0,
                    0.0,
                ))
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
        items.push(
            muted(pal, name)
                .font_size(11.0)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    // The saved profile: its picture, else its initials on the app accent, the pick storing
    // no colour.
    if let Some((p, picture)) = profile {
        items.push(
            profile_face(pal, &p.display_name, picture, 20.0, None)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    hstack(items)
        .spacing(6.0)
        .margin(edges(0.0, 12.0, 0.0, 0.0))
        .into()
}

/// The host editor sheet: name, address, port, Wake-on-LAN MACs, preset and clipboard sharing
/// in one place, like the Apple client's sheet.
///
/// Save checks the connection fields with [`HostEdit::parse`]; a refused value or a failed
/// write goes to the status line and the sheet stays open, drafts and all.
fn edit_editor(page: &HostsPage, props: &HostsProps, who: &HostRef) -> View {
    let pal = &props.svc.pal;
    let (set_edit, set_status) = (props.set_rename.clone(), props.svc.set_status.clone());
    let (hosts_rev, set_hosts_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
    let d = &page.drafts;
    let commit = {
        let (who, se, st) = (who.clone(), set_edit.clone(), set_status.clone());
        let set_hosts_rev = set_hosts_rev.clone();
        let (name, addr, port, mac, clip) = (
            d.name.live.clone(),
            d.addr.live.clone(),
            d.port.live.clone(),
            d.mac.live.clone(),
            d.clip.live.clone(),
        );
        move || {
            let parsed = HostEdit::parse(&addr.borrow(), &port.borrow(), &mac.borrow());
            let edit = match parsed {
                Ok(edit) => HostEdit {
                    name: Some(name.borrow().clone()),
                    ..edit
                },
                Err(e) => {
                    st.call(e.message);
                    return;
                }
            };
            let r = KnownHosts::update(|known| {
                if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
                    h.apply_edit(&edit);
                    h.clipboard_sync = *clip.borrow();
                }
            });
            if saved(r, &st, Some((hosts_rev, &set_hosts_rev))).is_some() {
                st.call(String::new());
                se.call(None);
            }
        }
    };
    // The preset binding: what a plain click on this tile will use. It commits on change
    // rather than at Save — it is a picker with no draft, and the rest of the sheet's
    // fields are text boxes that genuinely need one.
    let preset_picker = {
        let catalog = pf_client_core::presets::PresetsFile::load();
        let known = KnownHosts::load();
        let stored = who
            .index(&known)
            .and_then(|i| known.hosts[i].preset_id.clone());
        let mut names = vec!["Default settings".to_string()];
        let mut ids: Vec<String> = vec![String::new()];
        for p in &catalog.presets {
            names.push(p.name.clone());
            ids.push(p.id.clone());
        }
        // A binding whose preset is gone reads as Default settings — the same "dangling
        // resolves as none" rule the connect path follows — and is cleaned up on the next pick.
        let current = stored
            .as_ref()
            .and_then(|id| ids.iter().position(|i| i == id))
            .unwrap_or(0);
        let (who, set_status, set_hosts_rev) =
            (who.clone(), set_status.clone(), set_hosts_rev.clone());
        ComboBox::new()
            .header("Preset")
            .items_source(names)
            .selected_index(current)
            .on_selection_changed(move |i: Option<usize>| {
                let Some(id) = i.and_then(|i| ids.get(i)) else {
                    return;
                };
                let r = KnownHosts::update(|known| {
                    if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
                        h.preset_id = (!id.is_empty()).then(|| id.clone());
                    }
                });
                saved(r, &set_status, Some((hosts_rev, &set_hosts_rev)));
            })
    };
    let field = |label: &str, draft: &Draft<String>, placeholder: &str| {
        let live = draft.live.clone();
        vstack((
            muted(pal, label)
                .font_size(12.0)
                .horizontal_alignment(HorizontalAlignment::Left),
            TextBox::new()
                .text(&draft.seed)
                .placeholder_text(placeholder)
                .on_text_changed(move |t: Rc<str>| *live.borrow_mut() = t.to_string()),
        ))
        .spacing(2.0)
    };
    let clip_live = d.clip.live.clone();
    let initial_name = &who.name;
    let body = vstack((
        text(format!("Edit \u{201c}{initial_name}\u{201d}"))
            .font_size(20.0)
            .font_weight(FontWeight::BOLD),
        field("Name", &d.name, "e.g. Living Room"),
        field("Address", &d.addr, "IP or hostname"),
        field("Port", &d.port, "9777"),
        field("MAC (Wake-on-LAN)", &d.mac, "auto-filled when known"),
        vstack((
            preset_picker,
            muted(
                pal,
                "The settings a plain click on this host uses. \u{201c}Connect with\u{201d} \
                 in the tile\u{2019}s menu overrides it for one session without changing it.",
            )
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .horizontal_alignment(HorizontalAlignment::Left),
        ))
        .spacing(4.0),
        vstack((
            ToggleSwitch::new()
                .is_on(d.clip.seed)
                .header("Share clipboard with this host")
                .on_content("On")
                .off_content("Off")
                .on_toggled(move |v: bool| *clip_live.borrow_mut() = v),
            muted(
                pal,
                "Copy on one machine, paste on the other. Off for every host until you \
                 turn it on here; the host must allow it too.",
            )
            .font_size(12.0)
            .text_wrapping(TextWrapping::Wrap)
            .horizontal_alignment(HorizontalAlignment::Left),
        ))
        .spacing(4.0),
        hstack((
            icon_button("check", "Save")
                .style(ButtonStyle::Accent)
                .on_click(commit),
            button("Cancel").style(ButtonStyle::Subtle).on_click({
                let se = set_edit.clone();
                move || se.call(None)
            }),
        ))
        .spacing(8.0)
        .horizontal_alignment(HorizontalAlignment::Right),
    ))
    .spacing(10.0);
    // A centred sheet, not an in-grid tile: a sheet centres at its own height and scrolls
    // its content, so a short window never clips the lower fields.
    sheet(pal, body, 460.0, 1.0, move || set_edit.call(None))
}

fn hosts_page(page: &HostsPage, props: &HostsProps) -> View {
    let pal = &props.svc.pal;
    let status = props.status.as_str();
    let hover = Hover {
        current: props.hover.clone(),
        set: props.set_hover.clone(),
    };
    let known = KnownHosts::load();
    let settings = Settings::load();

    // As many TILE_MIN_WIDTH columns as fit the page's content width, at least one.
    let content_w = (props.width - 64.0).clamp(TILE_MIN_WIDTH, 1120.0);
    let cols = (((content_w + TILE_GAP) / (TILE_MIN_WIDTH + TILE_GAP)).floor() as usize).max(1);
    let mut body: Vec<View> = vec![header(props, &settings, known.hosts.len())];

    // Connect errors, editor refusals and host-action outcomes share this line. Each message
    // is a whole sentence, so the bar carries no title of its own.
    if !status.is_empty() {
        body.push(
            InfoBar::new()
                .message(status)
                .severity(InfoBarSeverity::Error)
                .is_open(true)
                .is_closable(false)
                .into(),
        );
    }

    // Saved (trusted/paired) hosts — reachable even when mDNS isn't, in the order and bands
    // the device's host sort asks for.
    if !known.hosts.is_empty() {
        // One catalog read per render, shared by every tile's menu and chip.
        let presets: Vec<(String, String, Option<String>)> =
            pf_client_core::presets::PresetsFile::load()
                .presets
                .into_iter()
                .map(|p| (p.id, p.name, p.accent))
                .collect();
        let mut cards: Vec<SavedCard> = known
            .hosts
            .iter()
            .flat_map(|k| saved_cards(props, k, &presets))
            .collect();
        host_order::arrange(&mut cards, &settings);
        let grouping = host_order::grouping(&settings);
        let mut bands: Vec<(Option<String>, Vec<View>)> = Vec::new();
        for c in &cards {
            let title = host_order::group_of(c, grouping);
            let tile = saved_tile(props, c, &hover, &presets, &settings.default_host);
            match bands.last_mut() {
                Some((t, tiles)) if *t == title => tiles.push(tile),
                _ => bands.push((title, vec![tile])),
            }
        }
        for (title, tiles) in bands {
            body.push(section(
                pal,
                &title.map_or_else(|| "SAVED HOSTS".into(), |t| t.to_uppercase()),
            ));
            body.push(tile_grid(tiles, cols, TILE_GAP));
        }
    }

    body.push(section(pal, "ON THIS NETWORK"));
    body.push(discovered_tiles(props, &known, &hover, cols));
    let edit: View = match &props.rename {
        Some(who) => edit_editor(page, props, who),
        None => Border::new().into(),
    };
    Grid::new()
        .children((page_wide(body), add_host_slot(page, props), edit))
        .content_dialog(forget_dialog(props))
}

/// The page header: the title block and the page actions — ONE labelled primary (Add host,
/// in accent), the rest icon-only with tooltips. Sort and group shows from two saved hosts.
fn header(props: &HostsProps, settings: &Settings, saved: usize) -> View {
    let (svc, set_show_add) = (&props.svc, &props.set_show_add);
    let (ctx, set_screen, set_status) = (&svc.ctx, &svc.set_screen, &svc.set_status);
    let mut actions: Vec<View> = vec![icon_button("plus", "Add host")
        .style(ButtonStyle::Accent)
        .on_click({
            let sa = set_show_add.clone();
            move || sa.call(true)
        })
        .into()];
    // Re-query mDNS. The browse runs for the app's lifetime, and `mdns-sd` backs its
    // re-query interval off to as much as an hour — so a host that appeared since
    // startup, or whose announcement was lost to multicast, may need an actual ask.
    let rescan = "Scan the network for hosts again";
    actions.push(
        mark_button("refresh-cw", rescan)
            .on_click({
                let (c, st) = (ctx.clone(), set_status.clone());
                move || {
                    if let Some(r) = c.shared.rescan.lock().unwrap().as_ref() {
                        r.request();
                    }
                    st.call("Scanning the network\u{2026}".to_string());
                }
            })
            .tooltip(rescan),
    );
    if saved > 1 {
        actions.push(arrange_button(props, settings));
    }
    // The couch UI's front door, beside the other page actions. Absent on ARM64,
    // where the session binary ships without its Skia console.
    if CONSOLE_UI_AVAILABLE {
        let console = "Console UI \u{2014} the controller-driven couch interface";
        actions.push(
            mark_button("gamepad-2", console)
                .on_click({
                    let (c, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
                    move || open_console(&c, &ss, &st, false)
                })
                .tooltip(console),
        );
    }
    actions.push(
        mark_button("keyboard", "Keyboard shortcuts")
            .on_click({
                let ss = set_screen.clone();
                move || ss.call(Screen::Help)
            })
            .tooltip("Keyboard shortcuts"),
    );
    actions.push(
        mark_button("settings", "Settings")
            .on_click({
                let (c, ss) = (ctx.clone(), set_screen.clone());
                move || {
                    // Re-base the settings snapshot on the file before the page renders —
                    // this process is not its only writer (see settings::refresh_snapshot).
                    super::settings::refresh_snapshot(&c);
                    ss.call(Screen::Settings)
                }
            })
            .tooltip("Settings"),
    );
    Grid::new()
        .children((
            vstack((
                text("Punktfunk")
                    .font_size(30.0)
                    .font_weight(FontWeight::BOLD),
                muted(&svc.pal, "Stream from a host on your network.")
                    .text_wrapping(TextWrapping::Wrap),
            ))
            .spacing(2.0)
            .grid_column(0)
            .vertical_alignment(VerticalAlignment::Center),
            hstack(actions)
                .spacing(8.0)
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::STAR, GridLength::Auto])
        .margin(edges(0.0, 0.0, 0.0, 10.0))
        .into()
}

/// A menu whose rows are keyed by their index into an action table, so two rows may carry
/// the same label.
struct MenuRows<A> {
    items: Vec<MenuItem>,
    actions: Vec<A>,
    separators: usize,
}

impl<A: 'static> MenuRows<A> {
    fn new() -> Self {
        MenuRows {
            items: Vec::new(),
            actions: Vec::new(),
            separators: 0,
        }
    }

    /// A row, for the top level or a submenu's leaves.
    fn row(&mut self, label: impl Into<std::rc::Rc<str>>, action: A) -> MenuItem {
        self.actions.push(action);
        MenuItem::item((self.actions.len() - 1).to_string(), label)
    }

    fn push(&mut self, label: impl Into<std::rc::Rc<str>>, action: A) {
        let item = self.row(label, action);
        self.items.push(item);
    }

    fn separator(&mut self) {
        self.separators += 1;
        self.items
            .push(MenuItem::separator(format!("sep{}", self.separators)));
    }

    /// The menu, `on_pick` running the clicked row's action.
    fn build(self, on_pick: impl Fn(&A) + 'static) -> Menu {
        let actions = self.actions;
        Menu::new(self.items, move |key: Key| {
            let picked = key
                .as_str()
                .and_then(|k| k.parse::<usize>().ok())
                .and_then(|i| actions.get(i));
            if let Some(a) = picked {
                on_pick(a);
            }
        })
    }
}

/// A subtle "…" button that opens `menu`, at a tile's top-right corner.
fn overflow(menu: Menu, name: &str) -> View {
    mark_button("ellipsis", name)
        .style(ButtonStyle::Subtle)
        .horizontal_alignment(HorizontalAlignment::Right)
        .vertical_alignment(VerticalAlignment::Top)
        .margin(edges(0.0, 8.0, 8.0, 0.0))
        .menu(menu)
        .tooltip("More options")
}

/// The header's Sort and Group menu over the device's `host_sort` and `host_grouping` keys,
/// the order the GTK shell and the console show. The one in force carries a check.
fn arrange_button(props: &HostsProps, settings: &Settings) -> View {
    let mut rows = MenuRows::<(&'static str, &'static str)>::new();
    for (title, key, current, values) in [
        (
            "Sort",
            host_order::HOST_SORT_KEY,
            host_order::sort(settings),
            &host_order::HOST_SORTS,
        ),
        (
            "Group",
            host_order::HOST_GROUPING_KEY,
            host_order::grouping(settings),
            &host_order::HOST_GROUPINGS,
        ),
    ] {
        let mut leaves = Vec::new();
        for (value, label) in values {
            let label = if *value == current {
                format!("{label} \u{2713}")
            } else {
                label.to_string()
            };
            leaves.push(rows.row(label, (key, *value)));
        }
        rows.items
            .push(MenuItem::submenu(format!("sub:{title}"), title, leaves));
    }
    let (hosts_rev, set_hosts_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
    let menu = rows.build(move |(key, value)| {
        Settings::update(|s| {
            s.extra.insert((*key).into(), (*value).into());
            true
        });
        set_hosts_rev.call(hosts_rev + 1);
    });
    mark_button("arrow-up-down", "Sort and group")
        .menu(menu)
        .tooltip("Sort and group")
}

/// One saved-host tile as the order sees it: a host's primary tile or a pinned one.
struct SavedCard<'a> {
    host: &'a pf_client_core::trust::KnownHost,
    online: bool,
    /// A pinned tile's preset `(id, name, accent)`; `None` on the primary.
    pinned: Option<&'a (String, String, Option<String>)>,
    /// What a plain click connects with: the pinned preset, else the host's binding.
    preset: Option<&'a str>,
}

impl Arrangeable for SavedCard<'_> {
    fn name(&self) -> &str {
        &self.host.name
    }
    fn online(&self) -> bool {
        self.online
    }
    fn last_used(&self) -> Option<u64> {
        self.host.last_used
    }
    fn preset_name(&self) -> Option<&str> {
        self.preset
    }
}

/// A saved host's cards: its primary, then its pinned host+preset tiles in pin order (design
/// §5.2a). A pin whose preset is gone has none. On the way the record learns what its advert
/// knows, and the host's own actions are kept warm.
fn saved_cards<'a>(
    props: &HostsProps,
    k: &'a pf_client_core::trust::KnownHost,
    presets: &'a [(String, String, Option<String>)],
) -> Vec<SavedCard<'a>> {
    let hosts = props.hosts.as_slice();
    // Online = the last probe sweep reached it, and nothing else. An advert is a cache entry
    // with a 75-minute TTL that a suspending host sends no goodbye for: counting it would keep
    // a sleeping machine's pip green, and every wake gate reads `!online`.
    let online = props.probed.get(&k.card_key()).copied().unwrap_or(false);
    // Everything the advert teaches: name, wake MAC(s), OS chain (so the mark survives going
    // offline), management port, and its address as a place the probe sweep asks —
    // the card moves there only once its pin answers. No disk write when unchanged.
    if let Some(a) = hosts
        .iter()
        .find(|h| pf_client_core::discovery::same_host(k, h))
    {
        crate::trust::learn_from_advert(&k.fp_hex, &k.addr, k.port, a);
    }
    // What this host last said it lets this device do to it. Kept warm here — on the
    // list's own refresh, gated by the cache's TTL — so the menu is BUILT from a
    // settled answer: rows that appeared while a menu was open would land under a
    // cursor already moving, and two of these rows shut a machine down.
    if k.paired && online {
        pf_client_core::host_actions::refresh(
            &k.addr,
            k.mgmt_port
                .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
            k.port,
            &k.fp_hex,
        );
    }
    let preset = |id: &String| presets.iter().find(|(pid, ..)| pid == id);
    let mut cards = vec![SavedCard {
        host: k,
        online,
        pinned: None,
        preset: k.preset_id.as_ref().and_then(preset).map(|p| p.1.as_str()),
    }];
    for p in k.pinned_presets.iter().filter_map(preset) {
        cards.push(SavedCard {
            host: k,
            online,
            pinned: Some(p),
            preset: Some(&p.1),
        });
    }
    cards
}

/// A saved card's tile: the host's primary tile, or a pinned one through [`pinned_tile`].
fn saved_tile(
    props: &HostsProps,
    c: &SavedCard,
    hover: &Hover,
    presets: &[(String, String, Option<String>)],
    settings_default: &Option<String>,
) -> View {
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let (k, online) = (c.host, c.online);
    let target = Target {
        host: HostTarget::from(k),
        ..Target::default()
    };
    let can_wake = !online && !k.mac.is_empty();
    if let Some(preset) = c.pinned {
        return pinned_tile(props, k, hover, online, can_wake, &target, preset);
    }
    let menu = saved_menu(
        props,
        k,
        &target,
        online,
        can_wake,
        presets,
        settings_default,
    );
    let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let pal = &props.svc.pal;
    host_tile(
        pal,
        &k.fp_hex,
        hover,
        &k.name,
        &k.os,
        &format!("{}:{}", k.addr, k.port),
        status_row_with(
            pal,
            Some(online),
            // Paired is the resting state — no chip; TOFU-only trust is worth one.
            (!k.paired).then_some(("Trusted", Pill::Info)),
            // The dot carries the preset's own colour where it has one —
            // that is what makes two bound hosts tell apart at a glance.
            k.preset_id
                .as_ref()
                .and_then(|id| presets.iter().find(|(pid, _, _)| pid == id))
                .map(|(_, name, accent)| (name.as_str(), accent.clone())),
            k.profile.as_ref().map(|p| {
                let (rev, set_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
                let redraw = move |held: u64| set_rev.call(rev + held);
                (p, super::profiles::card_picture(ctx, k, redraw))
            }),
        ),
        Some(menu),
        Box::new(move || {
            // Saved host with a known MAC the probe did not reach: fire a wake packet
            // and DIAL IMMEDIATELY — looking unreachable ≠ unreachable (a routed/Tailscale
            // host answers a dial it never advertised for); only a failed dial falls into
            // the "Waking…" wait. A reachable host dials straight away.
            if can_wake {
                initiate_waking(&ctx2, target.clone(), &ss, &st);
            } else {
                initiate(&ctx2, target.clone(), &ss, &st);
            }
        }),
    )
}

/// What a row of a saved host's "…" menu does.
#[derive(Clone)]
enum MenuAction {
    Connect,
    /// A one-off connect with this preset id; `""` forces Default settings on a bound host.
    ConnectWith(String),
    Library,
    Speed,
    SendLogs,
    Wake,
    HostAction(pf_client_core::host_actions::ActionInfo),
    CopyLink,
    Shortcut,
    /// Pin (`true`) or unpin this preset's tile.
    Pin(String, bool),
    ToggleDefault,
    Profile,
    Edit,
    Forget,
}

/// The rows of a saved host's "…" menu, in sections. The per-preset families nest in
/// submenus, so the top level stays a fixed handful whatever the catalog grows to.
fn saved_menu_rows(
    k: &pf_client_core::trust::KnownHost,
    online: bool,
    can_wake: bool,
    presets: &[(String, String, Option<String>)],
    is_default: bool,
) -> MenuRows<MenuAction> {
    use MenuAction as A;
    let mut rows = MenuRows::new();
    rows.push(MENU_CONNECT, A::Connect);
    // One-off connects: "Connect with" NEVER rebinds the host.
    if !presets.is_empty() {
        let mut leaves: Vec<MenuItem> = presets
            .iter()
            .map(|(id, name, _)| rows.row(name.as_str(), A::ConnectWith(id.clone())))
            .collect();
        leaves.push(rows.row(SUB_WITH_DEFAULT, A::ConnectWith(String::new())));
        rows.items
            .push(MenuItem::submenu("sub:with", SUB_WITH, leaves));
    }

    rows.separator();
    // The library surfaces — mouse/KB page and the gamepad console UI — for paired hosts
    // only, because the mgmt API needs the paired identity.
    if k.paired {
        rows.push(MENU_LIBRARY, A::Library);
    }
    rows.push(MENU_SPEED, A::Speed);
    // See [`MENU_SEND_LOGS`] for the gate.
    if k.paired && online {
        rows.push(MENU_SEND_LOGS, A::SendLogs);
    }
    // An explicit wake only when the host is offline and we have a MAC.
    if can_wake {
        rows.push(MENU_WAKE, A::Wake);
    }
    // The host's own actions, from the cache the host list keeps warm. Empty unless the host
    // answered AND this device's access carries the grant, so no row can be refused.
    for a in pf_client_core::host_actions::cached(&k.fp_hex) {
        rows.push(host_action_label(&a), A::HostAction(a));
    }

    rows.separator();
    rows.push(MENU_COPY_LINK, A::CopyLink);
    rows.push(MENU_SHORTCUT, A::Shortcut);
    // Pin/unpin a preset's one-click tile.
    if !presets.is_empty() {
        let leaves: Vec<MenuItem> = presets
            .iter()
            .map(|(id, name, _)| {
                let on = !k.pinned_presets.iter().any(|x| x == id);
                let verb = if on { MENU_PIN } else { MENU_UNPIN };
                rows.row(format!("{verb}{name}"), A::Pin(id.clone(), on))
            })
            .collect();
        rows.items
            .push(MenuItem::submenu("sub:pin", SUB_PIN, leaves));
    }

    rows.separator();
    // Which host the app opens on. Needs a pairing to point at: the start screen skips an
    // unpaired host. Unchecked is not "not the default": a lone paired host is the default
    // with nothing written.
    if k.paired {
        let label = if is_default {
            MENU_DEFAULT_SET
        } else {
            MENU_DEFAULT
        };
        rows.push(label, A::ToggleDefault);
    }
    if k.paired && k.profile.is_some() {
        rows.push(MENU_PROFILE, A::Profile);
    }
    rows.push(MENU_EDIT, A::Edit);
    rows.push(MENU_FORGET, A::Forget);
    rows
}

/// A saved host's "…" menu: connect, the library surfaces, the host's own actions, links,
/// pins, the default-host pointer, edit and forget.
fn saved_menu(
    props: &HostsProps,
    k: &pf_client_core::trust::KnownHost,
    target: &Target,
    online: bool,
    can_wake: bool,
    presets: &[(String, String, Option<String>)],
    settings_default: &Option<String>,
) -> View {
    let record_id = k.id.clone();
    let is_default = record_id.is_some() && *settings_default == record_id;
    let rows = saved_menu_rows(k, online, can_wake, presets, is_default);
    let (svc, target) = (props.svc.clone(), target.clone());
    let (sf, sr) = (props.set_forget.clone(), props.set_rename.clone());
    let who = HostRef::of(k);
    let (hosts_rev, set_hosts_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
    let link_host = k.clone();
    let (saved_pick, pin) = (k.profile.clone(), crate::trust::parse_hex32(&k.fp_hex));
    let menu = rows.build(move |action| match action {
        MenuAction::Connect => initiate(&svc.ctx, target.clone(), &svc.set_screen, &svc.set_status),
        MenuAction::ConnectWith(id) => {
            let mut target = target.clone();
            target.preset = Some(id.clone());
            initiate(&svc.ctx, target, &svc.set_screen, &svc.set_status)
        }
        MenuAction::Library => open_library(&svc, target.clone()),
        MenuAction::Speed => {
            *svc.ctx.shared.target.lock().unwrap() = target.clone();
            // New run: invalidate any still-in-flight probe, reset the screen.
            svc.ctx
                .shared
                .speed_gen
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            svc.set_speed.call(SpeedState::Running);
            svc.set_screen.call(Screen::SpeedTest);
        }
        MenuAction::SendLogs => send_logs(&svc, &target),
        MenuAction::Wake => crate::wol::wake(&target.host.mac, target.host.addr.parse().ok()),
        MenuAction::HostAction(a) => run_host_action(&svc, &target, a),
        MenuAction::CopyLink => {
            let url = pf_client_core::deeplink::DeepLink::for_host(&link_host, None, None).to_url();
            pf_client_core::clipboard::set_text(&url);
        }
        MenuAction::Shortcut => {
            let url = pf_client_core::deeplink::DeepLink::for_host(&link_host, None, None).to_url();
            match crate::deeplink::write_shortcut(&link_host.name, &url) {
                Ok(p) => tracing::info!(path = %p.display(), "shortcut written"),
                Err(e) => tracing::warn!(error = %e, "writing the shortcut"),
            }
        }
        MenuAction::Pin(id, on) => pin_tile(&svc, &who, id, *on, (hosts_rev, &set_hosts_rev)),
        MenuAction::ToggleDefault => {
            Settings::update(|s| {
                let on = s.default_host != record_id;
                s.default_host = on.then(|| record_id.clone()).flatten();
                true
            });
            svc.set_status.call(String::new());
            set_hosts_rev.call(hosts_rev + 1);
        }
        MenuAction::Profile => {
            let bump = set_hosts_rev.clone();
            super::profiles::switch(
                &svc.ctx,
                target.clone(),
                saved_pick.clone(),
                pin,
                move || bump.call(hosts_rev + 1),
            );
        }
        MenuAction::Edit => sr.call(Some(who.clone())),
        MenuAction::Forget => sf.call(Some(who.clone())),
    });
    overflow(menu, "More options")
}

/// Pin (`on`) or unpin preset `id`'s tile on the host `who` names. The store changes behind
/// the tiles and nothing the page reads as state does, so the bump is what makes the tile
/// appear or vanish now rather than on the next discovery tick.
fn pin_tile(svc: &Svc, who: &HostRef, id: &str, on: bool, rev: (u64, &AsyncSetState<u64>)) {
    tracing::info!(pin = %id, host = %who.name, on, "pin toggle");
    let r = KnownHosts::update(|known| {
        if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
            h.set_pinned(id, on);
        }
    });
    saved(r, &svc.set_status, Some(rev));
}

/// Runs one of the host's own actions on a worker thread, the outcome on the status line.
fn run_host_action(svc: &Svc, target: &Target, a: &pf_client_core::host_actions::ActionInfo) {
    let set_status = svc.set_status.clone();
    let (action_id, label) = (a.id.clone(), a.label().to_string());
    if !a.available {
        // The host already said it cannot do this right now.
        set_status.call(
            a.unavailable_reason
                .clone()
                .unwrap_or_else(|| format!("{label} isn't available")),
        );
        return;
    }
    let identity = svc.ctx.identity.clone();
    let target = target.clone();
    set_status.call(format!("{label} — asking {}…", target.host.name));
    let _ = std::thread::Builder::new()
        .name("punktfunk-hostaction".into())
        .spawn(move || {
            set_status.call(pf_client_core::host_actions::run(
                &target.host.name,
                &target.host.addr,
                target
                    .host
                    .mgmt_port
                    .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
                target.host.port,
                &identity,
                target.host.fp_hex.as_deref().unwrap_or_default(),
                &action_id,
                &label,
            ));
        });
}

/// Uploads this device's log ring to the host. Blocking network (the library agent's 5 s
/// connect / 10 s global budgets), so on a worker thread, the outcome on the status line.
fn send_logs(svc: &Svc, target: &Target) {
    let identity = svc.ctx.identity.clone();
    let target = target.clone();
    let set_status = svc.set_status.clone();
    set_status.call(format!("Sending logs to {}…", target.host.name));
    let _ = std::thread::Builder::new()
        .name("punktfunk-sendlogs".into())
        .spawn(move || {
            set_status.call(pf_client_core::logring::send_bundle(
                "punktfunk-client",
                &target.host.name,
                &target.host.addr,
                (
                    target
                        .host
                        .mgmt_port
                        .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT),
                    target.host.port,
                ),
                &identity,
                target.host.fp_hex.as_deref().unwrap_or_default(),
            ));
        });
}

/// What a row of a pinned tile's menu does.
enum PinnedAction {
    Library,
    CopyLink,
    Unpin,
}

/// One pinned host+preset tile (design §5.2a). A pinned tile is a shortcut: its menu starts
/// it (the library opens with its preset), copies its link or unpins it. Everything that
/// configures the host stays on the primary tile.
fn pinned_tile(
    props: &HostsProps,
    k: &pf_client_core::trust::KnownHost,
    hover: &Hover,
    online: bool,
    can_wake: bool,
    base: &Target,
    (id, name, accent): &(String, String, Option<String>),
) -> View {
    let pal = &props.svc.pal;
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let (ctx3, ss3, st3) = (ctx.clone(), set_screen.clone(), set_status.clone());
    let mut pinned_target = base.clone();
    pinned_target.preset = Some(id.clone());
    let pinned_menu = {
        let mut rows = MenuRows::new();
        // Same gate as the primary tile's: the mgmt API needs the paired identity, so an
        // unpaired host has nothing to show.
        if k.paired {
            rows.push(MENU_LIBRARY, PinnedAction::Library);
        }
        rows.push(MENU_COPY_LINK, PinnedAction::CopyLink);
        rows.separator();
        rows.push(format!("{MENU_UNPIN}{name}"), PinnedAction::Unpin);
        let (svc, target) = (props.svc.clone(), pinned_target.clone());
        let (unpin_who, pin_id) = (HostRef::of(k), id.clone());
        let (hosts_rev, set_hosts_rev) = (props.hosts_rev, props.set_hosts_rev.clone());
        let (link_host, link_preset) = (k.clone(), id.clone());
        overflow(
            rows.build(move |action| match action {
                PinnedAction::Library => open_library(&svc, target.clone()),
                PinnedAction::CopyLink => {
                    let url = pf_client_core::deeplink::DeepLink::for_host(
                        &link_host,
                        None,
                        Some(link_preset.as_str()),
                    )
                    .to_url();
                    pf_client_core::clipboard::set_text(&url);
                }
                PinnedAction::Unpin => pin_tile(
                    &svc,
                    &unpin_who,
                    &pin_id,
                    false,
                    (hosts_rev, &set_hosts_rev),
                ),
            }),
            "More options",
        )
    };
    host_tile(
        pal,
        // Its own hover key: two tiles for one host must not light up together.
        &format!("{}#{id}", k.fp_hex),
        hover,
        &k.name,
        &k.os,
        &format!("{}:{}", k.addr, k.port),
        status_row_with(
            pal,
            Some(online),
            (!k.paired).then_some(("Trusted", Pill::Info)),
            Some((name.as_str(), accent.clone())),
            None,
        ),
        Some(pinned_menu),
        Box::new(move || {
            if can_wake {
                initiate_waking(&ctx3, pinned_target.clone(), &ss3, &st3);
            } else {
                initiate(&ctx3, pinned_target.clone(), &ss3, &st3);
            }
        }),
    )
}

/// Discovered hosts not already saved, or a searching card while there are none.
fn discovered_tiles(props: &HostsProps, known: &KnownHosts, hover: &Hover, cols: usize) -> View {
    let pal = &props.svc.pal;
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let discovered: Vec<&DiscoveredHost> = props
        .hosts
        .iter()
        .filter(|h| {
            !known
                .hosts
                .iter()
                .any(|k| pf_client_core::discovery::same_host(k, h))
        })
        .collect();
    if discovered.is_empty() {
        return card(
            hstack((
                ProgressRing::new()
                    .is_indeterminate(true)
                    .width(18.0)
                    .height(18.0),
                muted(pal, "Searching the LAN\u{2026}"),
            ))
            .spacing(12.0),
        )
        .into();
    }
    let mut tiles: Vec<View> = Vec::new();
    for h in discovered {
        let target = Target {
            host: HostTarget {
                name: h.name.clone(),
                addr: h.addr.clone(),
                port: h.port,
                fp_hex: (!h.fp_hex.is_empty()).then(|| h.fp_hex.clone()),
                mac: h.mac.clone(),
                mgmt_port: h.mgmt_port,
                ..HostTarget::default()
            },
            pair_optional: h.pair == "optional",
            ..Target::default()
        };
        let (ctx2, ss, st) = (ctx.clone(), set_screen.clone(), set_status.clone());
        let (badge, kind) = if h.pair == "required" {
            ("PIN", Pill::Info)
        } else {
            ("Open", Pill::Neutral)
        };
        tiles.push(host_tile(
            pal,
            &format!("{}:{}", h.addr, h.port),
            hover,
            &h.name,
            &h.os,
            &format!("{}:{}", h.addr, h.port),
            status_row(pal, None, Some((badge, kind))),
            None,
            Box::new(move || initiate(&ctx2, target.clone(), &ss, &st)),
        ));
    }
    tile_grid(tiles, cols, TILE_GAP)
}

/// The Forget confirmation, attached to the page and armed by `is_open`. Confirmed first:
/// undoing a forget needs a fresh pairing.
fn forget_dialog(props: &HostsProps) -> ContentDialog {
    let (sf, st) = (props.set_forget.clone(), props.svc.set_status.clone());
    let pending = props.forget.clone();
    let content = pending
        .as_ref()
        .map(|who| {
            let name = &who.name;
            format!(
                "Forget \u{201C}{name}\u{201D}? You'll need to pair (or trust) it again to \
                 reconnect."
            )
        })
        .unwrap_or_default();
    ContentDialog::new()
        .title("Remove saved host?")
        .content(content)
        .primary_button_text("Remove")
        .close_button_text("Cancel")
        .is_open(pending.is_some())
        .on_closed(move |r: ContentDialogResult| {
            if r == ContentDialogResult::Primary
                && let Some(who) = &pending
                && let Err(e) = pf_client_core::orchestrate::forget_host(|known| who.index(known))
            {
                st.call(format!("Couldn't save — {e:#}"));
            }
            sf.call(None); // re-renders the page; the row is gone on the next load
        })
}

/// "Add host": a [`sheet`] with the address field, Connect and Cancel. The address box keeps
/// what was typed while the page lives; Connect reads it at click time.
fn add_host_slot(page: &HostsPage, props: &HostsProps) -> View {
    if !props.show_add {
        return Border::new().into();
    }
    let pal = &props.svc.pal;
    let (ctx, set_screen, set_status) =
        (&props.svc.ctx, &props.svc.set_screen, &props.svc.set_status);
    let set_show_add = &props.set_show_add;
    let live = page.manual.live.clone();
    let connect_manual = {
        let (ctx2, ss, st, live, sa) = (
            ctx.clone(),
            set_screen.clone(),
            set_status.clone(),
            live.clone(),
            set_show_add.clone(),
        );
        move || {
            let text = live.borrow();
            let text = text.trim();
            if text.is_empty() {
                return;
            }
            // Shared parser: a pasted `::1` is one address, not host `:` port `1`, and this
            // value is persisted and compared against saved records.
            let (addr, port) = pf_client_core::deeplink::parse_addr_port(text)
                .unwrap_or_else(|| (text.to_string(), pf_client_core::deeplink::DEFAULT_PORT));
            sa.call(false);
            initiate(
                &ctx2,
                // A typed address: no pin, and no mgmt port until an advert teaches one, so the
                // library falls back to 47990.
                Target {
                    host: HostTarget {
                        name: addr.clone(),
                        addr,
                        port,
                        ..HostTarget::default()
                    },
                    ..Target::default()
                },
                &ss,
                &st,
            );
        }
    };
    let body = vstack((
        text("Add a host")
            .font_size(20.0)
            .font_weight(FontWeight::BOLD),
        muted(
            pal,
            "Enter the host's IP address or name. Append :port only for a non-standard port \
             (the default is 9777).",
        )
        .font_size(13.0)
        .text_wrapping(TextWrapping::Wrap),
        TextBox::new()
            .text(&page.manual.seed)
            .header("Address")
            .placeholder_text("192.168.1.20  or  my-pc.local")
            .on_text_changed(move |s: Rc<str>| *live.borrow_mut() = s.to_string())
            .margin(edges(0.0, 6.0, 0.0, 0.0)),
        hstack((
            icon_button("arrow-right", "Connect")
                .style(ButtonStyle::Accent)
                .on_click(connect_manual),
            button("Cancel").on_click({
                let sa = set_show_add.clone();
                move || sa.call(false)
            }),
        ))
        .spacing(8.0)
        .horizontal_alignment(HorizontalAlignment::Right)
        .margin(edges(0.0, 6.0, 0.0, 0.0)),
    ))
    .spacing(12.0);
    let sa = set_show_add.clone();
    sheet(pal, body, 460.0, props.add_anim, move || sa.call(false))
}
