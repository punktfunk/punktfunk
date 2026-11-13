//! The Hosts destination: saved hosts as cards in the bands the device's host sort asks for
//! (`host_order`), the hosts found on this network under them, and the Add host sheet. A click
//! on a card connects; its ⓘ opens the host's page ([`super::host_page`]), where everything
//! about the host that is not "connect to it" lives.

use super::connect::{initiate, initiate_waking};
use super::library::open_library;
use super::style::*;
use super::{lucide, saved, AsyncSetState, Svc, Target};
use crate::trust::{HostEdit, KnownHosts, Settings};
use pf_client_core::discovery::DiscoveredHost;
use pf_client_core::display_targets::{self, BoundPreset, DisplayTarget, Pick, Targets};
use pf_client_core::host_order::{self, Arrangeable};
use pf_client_core::host_status::Status;
use pf_client_core::orchestrate::HostTarget;
use pf_client_core::trust::KnownHost;
use pf_fluent::{Palette, Token};
use pf_l10n::{client, common, desktop};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use windows_reactor::*;

/// Whether the console (gamepad) UI is available in this build: the session binary ships
/// its Skia `ui` feature on x64 only (no skia prebuilts for aarch64 yet) — the entry
/// points compile everywhere but only show where `--browse` can actually run.
pub(crate) const CONSOLE_UI_AVAILABLE: bool = cfg!(target_arch = "x86_64");

/// The narrowest a card gets before the grid drops a column, and the gap between cards.
const CARD_MIN_WIDTH: f64 = 280.0;
const CARD_GAP: f64 = 12.0;
/// The card's tile, and the width the trailing ⓘ takes from the text beside it.
const TILE: f64 = 44.0;
const INFO_WIDTH: f64 = 36.0;

/// Which saved record a per-host action means. NOT the fingerprint: a host added by address
/// has none, and keying on `""` matched the first such record. `addr`/`port` are the fallback
/// for a record older than the minted ids, and are also why the id is the durable key: the
/// host's page can change them under itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct HostRef {
    pub(crate) id: Option<String>,
    pub(crate) addr: String,
    pub(crate) port: u16,
    pub(crate) name: String,
}

impl HostRef {
    pub(super) fn of(h: &KnownHost) -> Self {
        Self {
            id: h.id.clone(),
            addr: h.addr.clone(),
            port: h.port,
            name: h.name.clone(),
        }
    }

    /// The record this names, if it is still in the store.
    pub(super) fn index(&self, known: &KnownHosts) -> Option<usize> {
        known.index_of_card(self.id.as_deref(), &self.addr, self.port)
    }

    /// One string per record — the minted id, or addr:port for a record older than ids.
    pub(super) fn seed_key(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", self.addr, self.port))
    }
}

/// The hosts page's props: the services, the root's discovery and probe state, and the Add
/// host sheet's root state. Equal on the values alone.
#[derive(Clone)]
pub(crate) struct HostsProps {
    pub(crate) svc: Svc,
    pub(crate) hosts: Vec<DiscoveredHost>,
    /// Saved hosts proven reachable by the periodic QUIC probe, keyed by `KnownHost::card_key` —
    /// the whole of the Online dot. A routed host (Tailscale/VPN) that never advertises reads
    /// Online here, and a sleeping one whose advert has not aged out yet reads Offline.
    pub(crate) probed: HashMap<String, bool>,
    pub(crate) show_add: bool,
    /// The Add host sheet's entrance (0 → 1): opacity and rise.
    pub(crate) add_anim: f64,
    /// Bumped when an action changes what the page should SHOW without changing any state it
    /// already reads: a pin, a screen pick, a new record.
    pub(crate) hosts_rev: u64,
    /// The width the page has beside the pane, which the card columns follow.
    pub(crate) width: f64,
    pub(crate) set_show_add: AsyncSetState<bool>,
}

impl PartialEq for HostsProps {
    fn eq(&self, other: &Self) -> bool {
        self.svc == other.svc
            && self.hosts == other.hosts
            && self.probed == other.probed
            && self.show_add == other.show_add
            && self.add_anim == other.add_anim
            && self.hosts_rev == other.hosts_rev
            && self.width == other.width
    }
}

/// A text box's live value and the value it opened with. The box shows `seed` and writes
/// every keystroke to `live`, which a click reads; re-declaring the live value would move
/// the caret on every redraw.
#[derive(Default)]
pub(super) struct Draft<T> {
    pub(super) seed: T,
    pub(super) live: Rc<RefCell<T>>,
}

impl<T: Clone> Draft<T> {
    pub(super) fn reset(&mut self, value: T) {
        *self.live.borrow_mut() = value.clone();
        self.seed = value;
    }
}

/// The Add host sheet's fields.
#[derive(Default)]
struct AddDrafts {
    name: Draft<String>,
    addr: Draft<String>,
    port: Draft<String>,
    mac: Draft<String>,
}

/// The hosts page. Its own state is the Add host sheet: the drafts, and what the last Add
/// was refused for.
pub(crate) struct HostsPage {
    add: AddDrafts,
    refused: String,
    /// The sheet was up at the last props, so its next opening starts it afresh.
    was_open: bool,
}

impl HostsPage {
    fn fresh_sheet(&mut self) {
        let a = &mut self.add;
        a.name.reset(String::new());
        a.addr.reset(String::new());
        a.port
            .reset(pf_client_core::deeplink::DEFAULT_PORT.to_string());
        a.mac.reset(String::new());
        self.refused.clear();
    }
}

impl Component for HostsPage {
    type Input = HostsProps;
    /// What an Add was refused for; empty clears it.
    type Message = String;

    fn create(props: &HostsProps, _cx: &ComponentContext<Self>) -> Self {
        let mut page = HostsPage {
            add: AddDrafts::default(),
            refused: String::new(),
            was_open: props.show_add,
        };
        page.fresh_sheet();
        page
    }

    fn input_changed(&mut self, props: &HostsProps, _cx: &ComponentContext<Self>) {
        if props.show_add && !self.was_open {
            self.fresh_sheet();
        }
        self.was_open = props.show_add;
    }

    fn update(&mut self, refused: String, _cx: &ComponentContext<Self>) {
        self.refused = refused;
    }

    fn view(&self, props: &HostsProps, cx: &mut ViewContext<'_, Self>) -> View {
        hosts_page(self, props, cx.callback(|refused: String| refused))
    }
}

/// Re-query mDNS. The browse runs for the app's lifetime, and `mdns-sd` backs its re-query
/// interval off to as much as an hour — so a host that appeared since startup, or whose
/// announcement was lost to multicast, may need an actual ask.
pub(super) fn rescan(svc: &Svc) {
    if let Some(r) = svc.ctx.shared.rescan.lock().unwrap().as_ref() {
        r.request();
    }
    svc.note(client::hosts_scanning().to_string());
}

/// A click on a saved host: a wake packet first when the probe did not reach it and it has a
/// MAC, then the dial at once. Looking unreachable is not unreachable (a routed host answers
/// a dial it never advertised for); only a failed dial falls into the "Waking…" wait.
pub(super) fn connect_saved(svc: &Svc, target: Target, online: bool) {
    if !online && !target.host.mac.is_empty() {
        initiate_waking(&svc.ctx, target, &svc.set_screen, &svc.set_status);
    } else {
        initiate(&svc.ctx, target, &svc.set_screen, &svc.set_status);
    }
}

/// What a saved host's card says under its name, from the probe sweep and what the host has
/// up. This shell covers its window while it connects, so no card shows a session.
pub(super) fn status_of(k: &KnownHost, online: bool, auto_wake: bool) -> Status {
    let playing = match online && k.paired {
        true => pf_client_core::library::now_playing(&k.fp_hex),
        false => String::new(),
    };
    Status::of(k, online, None, &playing, auto_wake)
}

/// The status line: the presence dot, then the sentence. A running game reads in green.
pub(super) fn status_line(pal: &Palette, status: &Status) -> View {
    let sentence = text(status.sentence())
        .font_size(12.0)
        .text_trimming(TextTrimming::CharacterEllipsis)
        .vertical_alignment(VerticalAlignment::Center);
    let sentence = match status {
        Status::Playing(_) => sentence.foreground(pal.get(Token::SystemSuccess)),
        _ => sentence.foreground(pal.get(Token::SecondaryText)),
    };
    Grid::new()
        .children((
            presence_dot(pal, status.live())
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            sentence.grid_column(1),
        ))
        .columns([GridLength::Auto, GridLength::STAR])
        .column_spacing(6.0)
        .into()
}

/// The preset a click connects with, as a pill: a dot in the preset's own colour and its name
/// on a tint of it. A value that is no colour falls back to grey rather than being trusted.
/// A pinned card's is `strong`: the preset is what that card is.
fn preset_chip(pal: &Palette, name: &str, accent: Option<&str>, strong: bool) -> View {
    let colour = accent
        .and_then(super::settings::hex_color)
        .unwrap_or(Color::rgb(128, 128, 128));
    let tint = Color {
        a: if strong { 56 } else { 30 },
        ..colour
    };
    let label = muted(pal, name)
        .font_size(12.0)
        .vertical_alignment(VerticalAlignment::Center);
    Border::new()
        .content(
            hstack((
                blank(7.0, 7.0)
                    .background(colour)
                    .corner_radius(3.5)
                    .vertical_alignment(VerticalAlignment::Center),
                match strong {
                    true => label.font_weight(FontWeight::SEMI_BOLD),
                    false => label,
                },
            ))
            .spacing(5.0),
        )
        .background(tint)
        .corner_radius(10.0)
        .padding(edges(8.0, 2.0, 9.0, 3.0))
        .vertical_alignment(VerticalAlignment::Center)
        .into()
}

/// A small disc on a corner of the tile: the default host's star, or the saved profile.
fn tile_badge(face: impl Into<View>, top: bool) -> View {
    Border::new()
        .content(face)
        .background(ThemeBrush::SolidBackground)
        .corner_radius(9.0)
        .padding(uniform(1.5))
        .horizontal_alignment(HorizontalAlignment::Right)
        .vertical_alignment(match top {
            true => VerticalAlignment::Top,
            false => VerticalAlignment::Bottom,
        })
        .margin(match top {
            true => edges(0.0, -4.0, -4.0, 0.0),
            false => edges(0.0, 0.0, -4.0, -4.0),
        })
        .into()
}

/// What one card shows.
struct Face<'a> {
    name: &'a str,
    os: &'a str,
    paired: bool,
    /// The tile dims while the host does not answer.
    dim: bool,
    /// Beside the name: the preset's chip.
    chip: Option<View>,
    /// Under the name, top to bottom.
    lines: Vec<View>,
    /// The default host's star, on its own card.
    star: bool,
    profile: Option<View>,
    /// address:port, on hover.
    tooltip: String,
}

/// One host card: the tile, the name and its lines behind one button, so the card takes
/// focus, Enter and the control's hover and pressed fills. `info` is the trailing ⓘ, a
/// SIBLING of the button so a click on it never also connects; `menu` opens on a right-click,
/// a long press or Shift+F10.
fn host_card(
    pal: &Palette,
    face: Face,
    click: impl Fn() + 'static,
    info: Option<(String, Box<dyn Fn()>)>,
    menu: Option<Menu>,
) -> View {
    let mut tile: Vec<View> = vec![host_tile(pal, face.name, face.os, face.paired, TILE)
        .opacity(if face.dim { 0.55 } else { 1.0 })
        .into()];
    if face.star {
        let star = Border::new()
            .content(
                lucide::mark("star")
                    .font_size(9.0)
                    .foreground(pal.get(Token::TextOnAccent))
                    .horizontal_alignment(HorizontalAlignment::Center)
                    .vertical_alignment(VerticalAlignment::Center),
            )
            .background(ThemeBrush::Accent)
            .corner_radius(8.0)
            .width(15.0)
            .height(15.0)
            .automation_name(client::hostcard_default_host())
            .tooltip(client::hostcard_default_host());
        tile.push(tile_badge(star, false));
    }
    tile.extend(face.profile.map(|p| tile_badge(p, true)));

    let name = text(face.name)
        .font_size(15.0)
        .font_weight(FontWeight::SEMI_BOLD)
        .text_trimming(TextTrimming::CharacterEllipsis);
    // The name gives way to its chip, never the other way round.
    let mut words: Vec<View> = vec![match face.chip {
        Some(chip) => Grid::new()
            .children((
                name.grid_column(0)
                    .vertical_alignment(VerticalAlignment::Center),
                Border::new().content(chip).grid_column(1),
            ))
            .columns([GridLength::STAR, GridLength::Auto])
            .column_spacing(8.0)
            .into(),
        None => name.into(),
    }];
    words.extend(face.lines);
    let row = Grid::new()
        .children((
            Grid::new()
                .children(tile)
                .width(TILE)
                .height(TILE)
                .grid_column(0)
                .vertical_alignment(VerticalAlignment::Center),
            vstack(words)
                .spacing(3.0)
                .grid_column(1)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .columns([GridLength::Auto, GridLength::STAR])
        .column_spacing(12.0)
        // The button pads 11 px inside the card's 3; this clears the ⓘ.
        .margin(edges(
            0.0,
            6.0,
            if info.is_some() { INFO_WIDTH } else { 0.0 },
            6.0,
        ));
    let card = card_button(row, face.name, click).tooltip(face.tooltip);
    let mut layers: Vec<View> = vec![card];
    if let Some((name, open)) = info {
        layers.push(
            mark_button("info", &name)
                .style(ButtonStyle::Subtle)
                .on_click(open)
                .horizontal_alignment(HorizontalAlignment::Right)
                .vertical_alignment(VerticalAlignment::Center)
                .margin(edges(0.0, 0.0, 6.0, 0.0))
                .tooltip(client::hostcard_details_hint()),
        );
    }
    let card = Grid::new().children(layers);
    match menu {
        Some(menu) => card.menu(menu),
        None => card.into(),
    }
}

/// A menu whose rows are keyed by their index into an action table, so two rows may carry
/// the same label.
pub(super) struct MenuRows<A> {
    pub(super) items: Vec<MenuItem>,
    actions: Vec<A>,
    separators: usize,
}

impl<A: 'static> MenuRows<A> {
    pub(super) fn new() -> Self {
        MenuRows {
            items: Vec::new(),
            actions: Vec::new(),
            separators: 0,
        }
    }

    /// A row, for the top level or a submenu's leaves.
    pub(super) fn row(&mut self, label: impl Into<std::rc::Rc<str>>, action: A) -> MenuItem {
        self.actions.push(action);
        MenuItem::item((self.actions.len() - 1).to_string(), label)
    }

    pub(super) fn push(&mut self, label: impl Into<std::rc::Rc<str>>, action: A) {
        let item = self.row(label, action);
        self.items.push(item);
    }

    pub(super) fn separator(&mut self) {
        self.separators += 1;
        self.items
            .push(MenuItem::separator(format!("sep{}", self.separators)));
    }

    /// The menu, `on_pick` running the clicked row's action.
    pub(super) fn build(self, on_pick: impl Fn(&A) + 'static) -> Menu {
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

/// The header's Sort and Group flyout over the device's `host_sort` and `host_grouping` keys,
/// the order the GTK shell and the console show.
fn arrange_button(props: &HostsProps, settings: &Settings) -> View {
    let groups = [
        (
            client::hosts_sort(),
            host_order::HOST_SORT_KEY,
            host_order::sort(settings),
            &host_order::HOST_SORTS,
        ),
        (
            client::hosts_group(),
            host_order::HOST_GROUPING_KEY,
            host_order::grouping(settings),
            &host_order::HOST_GROUPINGS,
        ),
    ]
    .into_iter()
    .map(|(title, key, current, values)| {
        let bump = props.svc.bump_hosts.clone();
        choice_group(
            title,
            values
                .iter()
                .map(|(_, label)| label().to_string())
                .collect(),
            values.iter().position(|(value, _)| *value == current),
            move |i| {
                let Some((value, _)) = values.get(i) else {
                    return;
                };
                Settings::update(|s| {
                    s.extra.insert(key.into(), (*value).into());
                    true
                });
                bump.call(());
            },
        )
    })
    .collect();
    arrange_flyout(client::hosts_arrange(), groups)
}

/// A preset as the cards read it.
#[derive(Clone)]
pub(super) struct CardPreset {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) accent: Option<String>,
    /// The host screen it pins.
    host_screen: Option<String>,
}

impl CardPreset {
    /// Every preset in the catalog: one read per render, shared by every card.
    pub(super) fn catalog() -> Vec<CardPreset> {
        pf_client_core::presets::PresetsFile::load()
            .presets
            .into_iter()
            .map(|p| CardPreset {
                id: p.id,
                name: p.name,
                accent: p.accent,
                host_screen: p.overrides.host_screen,
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn named(id: &str, name: &str) -> CardPreset {
        CardPreset {
            id: id.into(),
            name: name.into(),
            accent: None,
            host_screen: None,
        }
    }

    /// This preset as core's Screen rows read it, when it pins the host screen.
    fn bound(&self) -> Option<BoundPreset<'_>> {
        Some(BoundPreset {
            name: &self.name,
            host_screen: self.host_screen.as_deref()?,
        })
    }
}

/// The host's binding as core's Screen rows read it, when its preset pins the host screen.
pub(super) fn bound_screen_preset<'a>(
    k: &KnownHost,
    presets: &'a [CardPreset],
) -> Option<BoundPreset<'a>> {
    let id = k.preset_id.as_ref()?;
    presets
        .iter()
        .find(|p| &p.id == id)
        .and_then(CardPreset::bound)
}

/// One saved-host card as the order sees it: a host's own card or a pinned one.
struct SavedCard<'a> {
    host: &'a KnownHost,
    status: Status,
    /// A pinned card's preset; `None` on the host's own.
    pinned: Option<&'a CardPreset>,
    /// What a plain click connects with: the pinned preset, else the host's binding.
    preset: Option<&'a CardPreset>,
}

impl Arrangeable for SavedCard<'_> {
    fn name(&self) -> &str {
        &self.host.name
    }
    fn online(&self) -> bool {
        self.status.live()
    }
    fn last_used(&self) -> Option<u64> {
        self.host.last_used
    }
    fn preset_name(&self) -> Option<&str> {
        self.preset.map(|p| p.name.as_str())
    }
}

/// What the advert teaches a saved record, and the host's answers the cards read, kept warm:
/// its own actions, its screens and what it has up. Called on the list's own refresh, gated by
/// each cache's TTL, so a menu is BUILT from a settled answer: rows that appeared while a menu
/// was open would land under a cursor already moving.
pub(super) fn keep_warm(k: &KnownHost, adverts: &[DiscoveredHost], online: bool) {
    // Name, wake MAC(s), OS chain (so the mark survives going offline), management port, and
    // its address as a place the probe sweep asks. No disk write when unchanged.
    if let Some(a) = adverts
        .iter()
        .find(|h| pf_client_core::discovery::same_host(k, h))
    {
        crate::trust::learn_from_advert(&k.fp_hex, &k.addr, k.port, a);
    }
    if k.paired && online {
        let mgmt = k
            .mgmt_port
            .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
        pf_client_core::host_actions::refresh(&k.addr, mgmt, k.port, &k.fp_hex);
        display_targets::refresh(&k.addr, mgmt, k.port, &k.fp_hex);
        pf_client_core::library::refresh_running(&k.addr, mgmt, k.port, &k.fp_hex);
    }
}

/// A saved host's cards: its own, then its pinned host+preset cards in pin order. A pin whose
/// preset is gone has none.
fn saved_cards<'a>(
    props: &HostsProps,
    k: &'a KnownHost,
    presets: &'a [CardPreset],
    auto_wake: bool,
) -> Vec<SavedCard<'a>> {
    // Online = the last probe sweep reached it, and nothing else. An advert is a cache entry
    // with a 75-minute TTL that a suspending host sends no goodbye for: counting it would keep
    // a sleeping machine's dot green, and every wake gate reads `!online`.
    let online = props.probed.get(&k.card_key()).copied().unwrap_or(false);
    keep_warm(k, &props.hosts, online);
    let status = status_of(k, online, auto_wake);
    let preset = |id: &String| presets.iter().find(|p| &p.id == id);
    let mut cards = vec![SavedCard {
        host: k,
        status: status.clone(),
        pinned: None,
        preset: k.preset_id.as_ref().and_then(preset),
    }];
    for p in k.pinned_presets.iter().filter_map(preset) {
        cards.push(SavedCard {
            host: k,
            status: status.clone(),
            pinned: Some(p),
            preset: Some(p),
        });
    }
    cards
}

/// What a row of a saved host's menu does.
#[derive(Clone)]
enum MenuAction {
    /// A one-off connect with this preset id; `""` forces Default settings on a bound host.
    ConnectWith(String),
    /// Write a Screen row's pick on the host, with the row's name.
    Screen(Pick, String),
    Library,
    Wake,
    Profile,
    CopyLink,
    Details,
    Unpin,
}

/// The Screen submenu from core's rows: the roles, then the rows from the first header on in a
/// submenu under it. A leaf reads name · detail · why it is greyed, else its hint; the checked
/// one carries ✓ and writes nothing. Under a note, the row the host or the card's `preset`
/// leaves, greyed, then the note. `None` when there is nothing to show.
fn screen_submenu(
    rows: &mut MenuRows<MenuAction>,
    t: &Targets,
    preset: Option<BoundPreset>,
) -> Option<MenuItem> {
    let (mut leaves, mut group, mut monitor_checked) = (Vec::new(), None, false);
    for r in t.rows_under(preset) {
        let mut label = [Some(r.label.as_str()), Some(r.detail.as_str())]
            .into_iter()
            .chain([r.reason.as_deref().or(r.hint)])
            .flatten()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        if r.checked {
            label.push_str(" \u{2713}");
        }
        // A key that is not an index runs nothing.
        let key = format!("screen:{}", r.id);
        let leaf = match r.pick {
            Some(pick) => rows.row(label, MenuAction::Screen(pick, r.label)),
            None if r.pickable => MenuItem::item(key, label),
            None => MenuItem::disabled(key, label),
        };
        if let Some(header) = r.header {
            group = Some((header, Vec::new()));
        }
        monitor_checked |= group.is_some() && r.checked;
        match &mut group {
            Some((_, monitors)) => monitors.push(leaf),
            None => leaves.push(leaf),
        }
    }
    if let Some((header, monitors)) = group {
        // The pick shows one level up too, or a streamed monitor would hide inside the submenu.
        let header = match monitor_checked {
            true => format!("{header} \u{2713}"),
            false => header.to_string(),
        };
        leaves.push(MenuItem::separator("sep:screen"));
        leaves.push(MenuItem::submenu("sub:screen:monitors", header, monitors));
    }
    leaves.extend(
        t.note_under(preset)
            .map(|n| MenuItem::disabled("screen:note", n)),
    );
    (!leaves.is_empty())
        .then(|| MenuItem::submenu("sub:screen", client::hostcard_section_screen(), leaves))
}

/// The rows of a host's own card: what a connect uses, then where else the card leads. What
/// configures the host is on its page.
fn saved_menu_rows(c: &SavedCard, presets: &[CardPreset]) -> MenuRows<MenuAction> {
    use MenuAction as A;
    let (k, online) = (c.host, c.status.live());
    let mut rows = MenuRows::new();
    // One-off connects: "Connect with" NEVER rebinds the host.
    if !presets.is_empty() {
        let mut leaves = vec![rows.row(
            client::hostcard_default_settings(),
            A::ConnectWith(String::new()),
        )];
        for p in presets {
            leaves.push(rows.row(p.name.as_str(), A::ConnectWith(p.id.clone())));
        }
        rows.items.push(MenuItem::submenu(
            "sub:with",
            client::hostcard_connect_with(),
            leaves,
        ));
    }
    // Which screen a connect streams, under the binding's preset. Only a reachable host can
    // take a pick.
    if let Some(t) = display_targets::cached(&k.fp_hex).filter(|_| k.paired && online) {
        let screen = screen_submenu(&mut rows, &t, bound_screen_preset(k, presets));
        rows.items.extend(screen);
    }
    // The library needs the paired identity.
    if k.paired {
        rows.push(client::hostcard_browse_library(), A::Library);
    }
    if !online && !k.mac.is_empty() {
        rows.push(client::hostcard_wake(), A::Wake);
    }
    // Re-pick who plays on this host, only where a pick is saved.
    if k.paired && k.profile.is_some() {
        rows.push(client::hostcard_switch_profile(), A::Profile);
    }
    rows.push(client::hostcard_copy_link(), A::CopyLink);
    rows.push(client::hostcard_details(), A::Details);
    rows
}

/// A pinned card is a shortcut: its menu opens its library, copies its link or unpins it.
fn pinned_menu_rows(k: &KnownHost) -> MenuRows<MenuAction> {
    use MenuAction as A;
    let mut rows = MenuRows::new();
    if k.paired {
        rows.push(client::hostcard_browse_library(), A::Library);
    }
    rows.push(client::hostcard_copy_link(), A::CopyLink);
    rows.separator();
    rows.push(client::hostcard_unpin_card(), A::Unpin);
    rows
}

/// One saved card: the host's own, or a pinned host+preset one.
fn saved_card(props: &HostsProps, c: &SavedCard, presets: &[CardPreset], default: bool) -> View {
    let (svc, pal) = (&props.svc, &props.svc.pal);
    let (k, online) = (c.host, c.status.live());
    let who = HostRef::of(k);
    // A pinned card connects with its own preset; the host's own honors the binding.
    let target = Target {
        host: HostTarget::from(k),
        preset: c.pinned.map(|p| p.id.clone()),
        ..Target::default()
    };
    let rows = match c.pinned {
        Some(_) => pinned_menu_rows(k),
        None => saved_menu_rows(c, presets),
    };
    let menu = {
        let (svc, target, who) = (svc.clone(), target.clone(), who.clone());
        let (link_host, pin_id) = (k.clone(), c.pinned.map(|p| p.id.clone()));
        let (saved_pick, pin) = (k.profile.clone(), crate::trust::parse_hex32(&k.fp_hex));
        rows.build(move |action| match action {
            MenuAction::ConnectWith(id) => {
                let target = Target {
                    preset: Some(id.clone()),
                    ..target.clone()
                };
                initiate(&svc.ctx, target, &svc.set_screen, &svc.set_status)
            }
            MenuAction::Screen(pick, name) => set_host_screen(&svc, &target, pick, name),
            MenuAction::Library => open_library(&svc, target.clone()),
            MenuAction::Wake => crate::wol::wake(&target.host.mac, target.host.addr.parse().ok()),
            MenuAction::Profile => {
                let bump = svc.bump_hosts.clone();
                super::profiles::switch(
                    &svc.ctx,
                    target.clone(),
                    saved_pick.clone(),
                    pin,
                    move || bump.call(()),
                );
            }
            MenuAction::CopyLink => {
                let link = pf_client_core::deeplink::DeepLink::for_host(
                    &link_host,
                    None,
                    pin_id.as_deref(),
                );
                pf_client_core::clipboard::set_text(&link.to_url());
                svc.note(client::hostcard_link_copied().to_string());
            }
            MenuAction::Details => svc.set_host_page.call(Some(who.clone())),
            MenuAction::Unpin => {
                if let Some(id) = &pin_id {
                    pin_card(&svc, &who, id, false);
                }
            }
        })
    };

    let chip = c
        .preset
        .map(|p| preset_chip(pal, &p.name, p.accent.as_deref(), c.pinned.is_some()));
    let mut lines: Vec<View> = vec![status_line(pal, &c.status)];
    // Core's screen line: "Streams LG TV", "Second screen". The only screen adds nothing.
    let screen = display_targets::cached(&k.fp_hex)
        .filter(|_| k.paired)
        .and_then(|t| {
            t.card_line_under(match c.pinned {
                Some(p) => p.bound(),
                None => bound_screen_preset(k, presets),
            })
        });
    lines.extend(screen.map(|line| {
        muted(pal, line)
            .font_size(12.0)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .into()
    }));
    // The saved profile: its picture, else its initials on the app accent.
    let profile = k.profile.as_ref().map(|p| {
        let bump = svc.bump_hosts.clone();
        let picture = super::profiles::card_picture(&svc.ctx, k, move |_| bump.call(()));
        profile_face(pal, &p.display_name, picture, 15.0, None)
            .tooltip(client::hostcard_playing_as(&p.display_name))
    });
    let face = Face {
        name: &k.name,
        os: &k.os,
        // A pin that was only trusted on first use is still this device's host.
        paired: !k.fp_hex.is_empty(),
        dim: !online,
        chip,
        lines,
        star: default && c.pinned.is_none(),
        profile,
        tooltip: format!("{}:{}", k.addr, k.port),
    };
    let click = {
        let (svc, target) = (svc.clone(), target.clone());
        move || connect_saved(&svc, target.clone(), online)
    };
    // Everything that configures the host is on its own card's page.
    let info: Option<(String, Box<dyn Fn()>)> = c.pinned.is_none().then(|| {
        let (set, who) = (svc.set_host_page.clone(), who.clone());
        let open: Box<dyn Fn()> = Box::new(move || set.call(Some(who.clone())));
        (client::hostcard_details_a11y(&k.name), open)
    });
    host_card(pal, face, click, info, Some(menu))
}

/// Pin (`on`) or unpin preset `id`'s card on the host `who` names.
pub(super) fn pin_card(svc: &Svc, who: &HostRef, id: &str, on: bool) {
    tracing::info!(pin = %id, host = %who.name, on, "pin toggle");
    let r = KnownHosts::update(|known| {
        if let Some(h) = who.index(known).and_then(|i| known.hosts.get_mut(i)) {
            h.set_pinned(id, on);
        }
    });
    saved(r, &svc.set_status, None);
    svc.bump_hosts.call(());
}

/// Writes a Screen row's pick on a worker thread. `set_pick` reads the host's rows back, so
/// the status line says the pick and the cards redraw from them; a refusal says why.
pub(super) fn set_host_screen(svc: &Svc, target: &Target, pick: &Pick, name: &str) {
    let identity = svc.ctx.identity.clone();
    let (svc, target, pick) = (svc.clone(), target.clone(), pick.clone());
    let picked = match &pick.screen {
        DisplayTarget::Own => name.to_string(),
        DisplayTarget::Monitor(_) => client::screen_streams(name),
    };
    let name = name.to_string();
    let _ = std::thread::Builder::new()
        .name("punktfunk-screen".into())
        .spawn(move || {
            let h = &target.host;
            let fp = h.fp_hex.as_deref().unwrap_or_default();
            let mgmt = h
                .mgmt_port
                .unwrap_or(pf_client_core::library::DEFAULT_MGMT_PORT);
            match display_targets::set_pick(&h.addr, mgmt, h.port, &identity, fp, &pick) {
                Ok(()) => svc.report(Ok(picked)),
                Err(e) => {
                    tracing::warn!(?pick, error = %e, "screen pick refused");
                    svc.report(Err(client::hostcard_action_failed(&name, e)));
                }
            }
            svc.bump_hosts.call(());
        });
}

/// The discovered hosts no saved record matches, by name.
fn discovered<'a>(props: &'a HostsProps, known: &KnownHosts) -> Vec<&'a DiscoveredHost> {
    let mut found: Vec<&DiscoveredHost> = props
        .hosts
        .iter()
        .filter(|h| {
            !known
                .hosts
                .iter()
                .any(|k| pf_client_core::discovery::same_host(k, h))
        })
        .collect();
    found.sort_by(|a, b| (&a.name, &a.key).cmp(&(&b.name, &b.key)));
    found
}

/// A host found on this network and not saved: a click saves and connects it, through the
/// trust gate.
fn discovered_card(props: &HostsProps, h: &DiscoveredHost) -> View {
    let (svc, pal) = (&props.svc, &props.svc.pal);
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
    let (mark, state) = match h.pair == "required" {
        true => ("lock", client::hostcard_status_pairing_required()),
        false => ("monitor", client::hostcard_status_found()),
    };
    let address = format!("{}:{}", h.addr, h.port);
    let lines: Vec<View> = vec![
        muted(pal, &address)
            .font_size(12.0)
            .text_trimming(TextTrimming::CharacterEllipsis)
            .into(),
        hstack((
            lucide::mark(mark)
                .font_size(12.0)
                .foreground(pal.get(Token::SecondaryText))
                .vertical_alignment(VerticalAlignment::Center),
            muted(pal, state)
                .font_size(12.0)
                .vertical_alignment(VerticalAlignment::Center),
        ))
        .spacing(5.0)
        .into(),
    ];
    let face = Face {
        name: &h.name,
        os: &h.os,
        paired: false,
        dim: false,
        chip: None,
        lines,
        star: false,
        profile: None,
        tooltip: address.clone(),
    };
    let click = {
        let svc = svc.clone();
        move || initiate(&svc.ctx, target.clone(), &svc.set_screen, &svc.set_status)
    };
    host_card(pal, face, click, None, None)
}

fn hosts_page(page: &HostsPage, props: &HostsProps, refuse: Callback<String>) -> View {
    let (svc, pal) = (&props.svc, &props.svc.pal);
    let known = KnownHosts::load();
    let settings = Settings::load();
    let found = discovered(props, &known);

    let mut actions: Vec<View> = vec![icon_button("plus", client::hosts_add())
        .style(ButtonStyle::Accent)
        .on_click({
            let sa = props.set_show_add.clone();
            move || sa.call(true)
        })
        .into()];
    actions.push(
        mark_button("refresh-cw", client::hosts_rescan())
            .on_click({
                let svc = svc.clone();
                move || rescan(&svc)
            })
            .tooltip(client::hosts_rescan_hint()),
    );
    if known.hosts.len() > 1 {
        actions.push(arrange_button(props, &settings));
    }
    let header = page_header(client::hosts_tab(), actions);

    // Nothing saved and nothing found: the page says what a host is and how one gets here.
    if known.hosts.is_empty() && found.is_empty() {
        let lead = lucide::mark("monitor")
            .font_size(40.0)
            .foreground(pal.get(Token::SecondaryText))
            .horizontal_alignment(HorizontalAlignment::Center);
        let add = icon_button("plus", client::hosts_add())
            .style(ButtonStyle::Accent)
            .on_click({
                let sa = props.set_show_add.clone();
                move || sa.call(true)
            });
        let scan = button(client::hosts_rescan()).on_click({
            let svc = svc.clone();
            move || rescan(&svc)
        });
        let empty = notice_page(
            pal,
            Some(lead.into()),
            client::hosts_empty_title(),
            client::hosts_empty_body(),
            vec![hstack((add, scan))
                .spacing(8.0)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into()],
        );
        let body = Grid::new()
            .children((
                Border::new()
                    .content(header)
                    .margin(edges(GUTTER, 20.0, GUTTER, 0.0)),
                Border::new().content(empty).grid_row(1),
            ))
            .rows([GridLength::Auto, GridLength::STAR]);
        return Grid::new()
            .children((body, add_host_slot(page, props, refuse)))
            .into();
    }

    // As many CARD_MIN_WIDTH columns as fit the page, at least one.
    let content_w = (props.width - 2.0 * GUTTER).max(CARD_MIN_WIDTH);
    let cols = (((content_w + CARD_GAP) / (CARD_MIN_WIDTH + CARD_GAP)).floor() as usize).max(1);
    let mut body: Vec<View> = vec![header];

    // Saved hosts — reachable even when mDNS isn't, in the order and bands the device's host
    // sort asks for.
    if !known.hosts.is_empty() {
        let presets = CardPreset::catalog();
        let mut cards: Vec<SavedCard> = known
            .hosts
            .iter()
            .flat_map(|k| saved_cards(props, k, &presets, settings.auto_wake))
            .collect();
        host_order::arrange(&mut cards, &settings);
        let grouping = host_order::grouping(&settings);
        // The host Start in opens on: the explicit pointer, or the only paired host.
        let default = pf_client_core::start::default_host(&settings, &known)
            .and_then(|i| known.hosts[i].id.clone());
        let mut bands: Vec<(Option<host_order::Band>, Vec<View>)> = Vec::new();
        for c in &cards {
            let band = host_order::band_of(c, grouping);
            let is_default = default.is_some() && c.host.id == default;
            let card = saved_card(props, c, &presets, is_default);
            match bands.last_mut() {
                Some((b, cards)) if *b == band => cards.push(card),
                _ => bands.push((band, vec![card])),
            }
        }
        for (band, cards) in bands {
            // An ungrouped list is the page's own subject and needs no heading.
            body.extend(band.map(|b| section(pal, &b.label())));
            body.push(tile_grid(cards, cols, CARD_GAP));
        }
    }

    if !found.is_empty() {
        body.push(section(pal, client::hosts_section_discovered()));
        let cards = found.iter().map(|h| discovered_card(props, h)).collect();
        body.push(tile_grid(cards, cols, CARD_GAP));
    }
    Grid::new()
        .children((page_fill(body), add_host_slot(page, props, refuse)))
        .into()
}

/// One field of a sheet: its label over a box that keeps what is typed in `draft`.
pub(super) fn sheet_field(pal: &Palette, label: &str, draft: &Draft<String>, hint: &str) -> View {
    let live = draft.live.clone();
    vstack((
        muted(pal, label)
            .font_size(12.0)
            .horizontal_alignment(HorizontalAlignment::Left),
        TextBox::new()
            .text(&draft.seed)
            .placeholder_text(hint)
            .on_text_changed(move |t: Rc<str>| *live.borrow_mut() = t.to_string()),
    ))
    .spacing(4.0)
    .into()
}

/// "Add host": name, address, port and Wake-on-LAN MAC. Add saves the host and dials nothing,
/// so a host can be added while it is off; its card's first click runs the trust gate. What
/// the fields refuse shows under them and the sheet stays up.
fn add_host_slot(page: &HostsPage, props: &HostsProps, refuse: Callback<String>) -> View {
    if !props.show_add {
        return Border::new().into();
    }
    let (svc, pal) = (&props.svc, &props.svc.pal);
    let d = &page.add;
    let add = {
        let (svc, sa, refuse) = (svc.clone(), props.set_show_add.clone(), refuse.clone());
        let (name, addr, port, mac) = (
            d.name.live.clone(),
            d.addr.live.clone(),
            d.port.live.clone(),
            d.mac.live.clone(),
        );
        move || {
            let edit = match HostEdit::parse(&addr.borrow(), &port.borrow(), &mac.borrow()) {
                Ok(edit) => HostEdit {
                    name: Some(name.borrow().trim().to_string()),
                    ..edit
                },
                Err(e) => return refuse.call(e.message),
            };
            if let Err(e) = pf_client_core::trust::add_host(&edit) {
                return refuse.call(desktop::desktop_hosts_save_failed(format!("{e:#}")));
            }
            let shown = match edit.name.as_deref() {
                Some(name) if !name.is_empty() => name.to_string(),
                _ => edit.addr.clone().unwrap_or_default(),
            };
            sa.call(false);
            svc.bump_hosts.call(());
            svc.report(Ok(desktop::desktop_hosts_added_click(&shown)));
        }
    };
    let mut body: Vec<View> = vec![
        text(client::hosts_add_title())
            .font_size(20.0)
            .font_weight(FontWeight::SEMI_BOLD)
            .into(),
        muted(pal, client::hosts_add_hint())
            .text_wrapping(TextWrapping::Wrap)
            .into(),
        sheet_field(
            pal,
            client::hosts_field_name(),
            &d.name,
            client::hosts_field_name_placeholder(),
        ),
        sheet_field(
            pal,
            client::hosts_field_address(),
            &d.addr,
            client::hosts_field_address_placeholder(),
        ),
        sheet_field(pal, client::hosts_field_port(), &d.port, ""),
        sheet_field(
            pal,
            client::hosts_field_mac(),
            &d.mac,
            client::hosts_field_mac_placeholder(),
        ),
    ];
    if !page.refused.is_empty() {
        body.push(
            InfoBar::new()
                .message(&page.refused)
                .severity(InfoBarSeverity::Error)
                .is_open(true)
                .is_closable(false)
                .into(),
        );
    }
    body.push(
        hstack((
            icon_button("plus", client::hosts_add_confirm())
                .style(ButtonStyle::Accent)
                .on_click(add),
            button(common::common_cancel()).on_click({
                let sa = props.set_show_add.clone();
                move || sa.call(false)
            }),
        ))
        .spacing(8.0)
        .horizontal_alignment(HorizontalAlignment::Right)
        .margin(edges(0.0, 6.0, 0.0, 0.0))
        .into(),
    );
    let sa = props.set_show_add.clone();
    sheet(
        pal,
        vstack(body).spacing(12.0),
        460.0,
        props.add_anim,
        move || sa.call(false),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(paired: bool, profile: bool, mac: bool) -> KnownHost {
        KnownHost {
            name: "Desk".into(),
            addr: "10.0.0.2".into(),
            port: 9777,
            fp_hex: if paired { "ab".into() } else { String::new() },
            paired,
            mac: if mac {
                vec!["aa:bb:cc:dd:ee:ff".into()]
            } else {
                Vec::new()
            },
            profile: profile.then(|| pf_client_core::profiles::ProfilePick {
                id: "p1".into(),
                display_name: "Ada".into(),
                ..Default::default()
            }),
            ..KnownHost::default()
        }
    }

    fn labels(rows: &MenuRows<MenuAction>) -> usize {
        rows.items.len()
    }

    /// The card's menu is the short one: what a connect uses and where the card leads. The
    /// rows that configure, power or remove the host are its page's.
    #[test]
    fn a_cards_menu_offers_only_what_its_host_can_do() {
        let presets = [CardPreset {
            id: "couch".into(),
            name: "Couch".into(),
            accent: None,
            host_screen: None,
        }];
        let card = |k: &KnownHost, status: Status| {
            let c = SavedCard {
                host: k,
                status,
                pinned: None,
                preset: None,
            };
            labels(&saved_menu_rows(&c, &presets))
        };
        // Paired and online with a saved profile: connect with, library, profile, link, details.
        assert_eq!(card(&host(true, true, true), Status::Online), 5);
        // Offline with a MAC adds Wake; no screens are asked of a host that is away.
        assert_eq!(card(&host(true, true, true), Status::Offline), 6);
        // Not paired: no library, no profile.
        assert_eq!(card(&host(false, false, false), Status::NotPaired), 3);
        // No presets: no Connect with.
        let c = SavedCard {
            host: &host(true, false, false),
            status: Status::Online,
            pinned: None,
            preset: None,
        };
        assert_eq!(labels(&saved_menu_rows(&c, &[])), 3);
        // A pinned card: library, link, the separator, unpin.
        assert_eq!(labels(&pinned_menu_rows(&host(true, false, false))), 4);
        assert_eq!(labels(&pinned_menu_rows(&host(false, false, false))), 3);
    }
}
