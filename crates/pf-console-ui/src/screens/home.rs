//! Console home: a row of host cards plus trailing Add Host and Rescan actions, the
//! focused card's verbs under it, and under those the focused host's games.
//!
//! Every card and verb is a focus target in one [`el::Tree`], so the plate travels from
//! card to verb. The row keeps the focused card on the shared margin until its end reaches
//! the screen's. OK connects, wakes, or pairs; Down reaches the verbs (Games, Connect
//! with…, Wake, Details…, More…), then the games; Y (OK held on a remote) opens the
//! card's menu; X jumps to Settings; Up from the row is the tab strip's; B at the root
//! leaves.
//!
//! The games are the Games tab's grid for that host ([`LibraryScreen::embedded`]). The
//! shell fetches them once the row settles on a paired, online host. With focus in them
//! the row and verbs slide up out of the way; Up from their top row returns to the verbs.
//!
//! Discovery churns the list; focus follows the tile key, not the index. A press on
//! another card only focuses it; a second press connects. Pin with the tests in this
//! module: key-follow, confirm routing, verbs, pinned-card preset, trailing Add Host.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::el::{Axis, El, Group, Id, Tree};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    step_cursor, StepResult, BUMP_C, BUMP_K, BUMP_V, ENTER_RISE, ENTER_SCALE, SPRING_C, SPRING_K,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::card_menu::CardMenu;
use crate::screens::library::LibraryScreen;
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{accent, edge, fg, fill, stroke, Fonts, PanelStroke, W};
use crate::widgets::{button, button_w, BUTTON_H};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, MaskFilter, PathBuilder, RRect, Rect};

const TILE_W: f64 = 340.0;
const TILE_H: f64 = 92.0;
const TILE_GAP: f64 = 24.0;
const TILE_CORNER: f64 = 20.0;
const BADGE: f64 = 52.0;
/// How much the focused card grows over its neighbours.
const FOCUS_LIFT: f64 = 0.04;
/// Air above the row: the plate's outset and a breath.
const ROW_AIR: f64 = 16.0;
/// The air above the verbs, and the gap between two.
const VERB_AIR: f64 = 16.0;
const VERB_GAP: f64 = 12.0;
/// Air between the verbs and the games.
const GAMES_AIR: f64 = 12.0;

/// Air over the row for a group's caption, when the hosts are grouped.
const GROUP_AIR: f64 = 24.0;
const GROUP_CAPTION: f64 = 12.0;

/// The `Settings::extra` keys and values the Apple app's own home stores its order under.
pub(crate) const HOST_SORT_KEY: &str = "host_sort";
pub(crate) const HOST_GROUPING_KEY: &str = "host_grouping";
pub(crate) const HOST_SORTS: [(&str, &str); 3] = [
    ("added", "Date added"),
    ("name", "Name"),
    ("lastConnected", "Last connected"),
];
pub(crate) const HOST_GROUPINGS: [(&str, &str); 3] =
    [("none", "None"), ("preset", "Preset"), ("status", "Status")];

fn extra<'s>(s: &'s pf_client_core::trust::Settings, key: &str, default: &'s str) -> &'s str {
    s.extra.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

/// The grouping in force; Apple's store may still say `profile`, the old name for presets.
fn grouping(s: &pf_client_core::trust::Settings) -> &str {
    match extra(s, HOST_GROUPING_KEY, "none") {
        "profile" => "preset",
        g => g,
    }
}

/// The band a card sits in under `grouping`, or `None` ungrouped. A pinned card goes with
/// the preset it connects with, the host's own card with its binding.
fn group_of(h: &HostRow, grouping: &str) -> Option<String> {
    match grouping {
        "status" => Some(if h.online { "Online" } else { "Offline" }.into()),
        "preset" => Some(match (&h.pin, &h.bound_preset) {
            (Some(p), _) | (None, Some(p)) => p.name.clone(),
            (None, None) => "No preset".into(),
        }),
        _ => None,
    }
}

/// Order the row as Settings asks: bands first (Online before Offline, presets by name with
/// "No preset" last), then the sort inside each. Stable, so equal cards keep the order the
/// host sent, which is the order they were added.
pub(crate) fn arrange(hosts: &mut [HostRow], s: &pf_client_core::trust::Settings) {
    let grouping = grouping(s);
    let sort = extra(s, HOST_SORT_KEY, "added");
    let band = |h: &HostRow| match (grouping, group_of(h, grouping)) {
        ("status", _) => (u8::from(!h.online), String::new()),
        (_, Some(name)) if name == "No preset" => (1, String::new()),
        (_, Some(name)) => (0, name.to_lowercase()),
        (_, None) => (0, String::new()),
    };
    hosts.sort_by(|a, b| {
        band(a).cmp(&band(b)).then_with(|| match sort {
            "name" => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
            // Most recent first; a host never connected to goes last.
            "lastConnected" => b.last_used.cmp(&a.last_used),
            _ => std::cmp::Ordering::Equal,
        })
    });
}

/// Sentinel. Host keys are fingerprints or `addr:port`; neither starts with `\0`.
const ADD_KEY: &str = "\0add";
/// Sentinel for the trailing Rescan tile; same `\0` prefix as [`ADD_KEY`].
const SCAN_KEY: &str = "\0scan";

/// Do not use `hosts.get(i)`: `None` is both trailing actions.
enum Slot<'h> {
    Host(&'h HostRow),
    AddHost,
    /// Re-run discovery. A pad has no pull-to-refresh.
    Rescan,
}

fn slot_at(i: usize, hosts: &[HostRow]) -> Slot<'_> {
    match hosts.get(i) {
        Some(h) => Slot::Host(h),
        None if i == hosts.len() => Slot::AddHost,
        None => Slot::Rescan,
    }
}

/// What a card offers besides OK, each one press away under it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Verb {
    Pair,
    Games,
    ConnectWith,
    Wake,
    Details,
    /// The card's whole menu, the hold's.
    More,
}

impl Verb {
    fn label(self) -> &'static str {
        match self {
            Verb::Pair => "Pair\u{2026}",
            Verb::Games => "Games",
            Verb::ConnectWith => "Connect with\u{2026}",
            Verb::Wake => "Wake",
            Verb::Details => "Details\u{2026}",
            Verb::More => "More\u{2026}",
        }
    }
}

/// The verbs under a card. The action tiles have none: OK is all they do.
fn verbs(slot: &Slot<'_>) -> Vec<Verb> {
    let Slot::Host(h) = slot else {
        return Vec::new();
    };
    let mut v = Vec::new();
    if !h.paired {
        v.push(Verb::Pair);
    } else if h.online {
        v.extend([Verb::Games, Verb::ConnectWith]);
    } else if h.can_wake {
        v.push(Verb::Wake);
    }
    v.extend([Verb::Details, Verb::More]);
    v
}

fn verb_id(i: usize) -> Id {
    Id::new("verb", i)
}

fn run_verb(verb: Verb, h: &HostRow, ctx: &mut Ctx, fx: &mut Outbox) {
    match verb {
        Verb::Pair => fx.push(Screen::Pair(super::pair::PairScreen::new(
            h,
            ctx.device_name,
        ))),
        Verb::Games => fx.tab = Some(crate::shell::Tab::Games),
        Verb::ConnectWith => fx.push(Screen::CardMenu(CardMenu::connect_with(h))),
        Verb::Wake => {
            fx.cmds.push(ConsoleCmd::Wake {
                key: h.key.clone(),
                then_connect: false,
            });
            fx.toast = Some(format!("Waking {}\u{2026}", h.name));
        }
        Verb::Details => fx.push(Screen::CardMenu(CardMenu::host_details(h))),
        Verb::More => fx.options(CardMenu::for_host(h)),
    }
}

pub(crate) struct HomeScreen {
    cursor: i32,
    anim: Spring,
    bump: Spring,
    /// Last-seen tile keys. Discovery churns the list; focus follows the key.
    keys: Vec<String>,
    /// Tiles at their drawn size, culled ones included: a direction reaches past the
    /// edge, and a pointer hits the (0.88) side-tile size, not its neighbour's.
    tree: Tree,
    /// Mount entrance. `None` until the first frame (no clock in the constructor)
    /// and again once finished. [`Self::entrance_armed`] stops it re-arming.
    entrance: Option<Entrance>,
    entrance_armed: bool,
    /// The focused host's games; the shell sets it ([`Self::wants_shelf`]).
    shelf: Option<Box<LibraryScreen>>,
    /// Focus is in the games, not the row.
    below: bool,
    /// Browse games asked for the games; focus goes down once they have titles.
    browse: bool,
    /// Where the games were last drawn, for the pointer.
    shelf_rect: Rect,
    /// Focus is on the focused card's verb `i`, not the card.
    verb: Option<usize>,
    /// How far the row and verbs have slid up for the games, px.
    page: Spring,
}

impl HomeScreen {
    pub(crate) fn new() -> HomeScreen {
        HomeScreen {
            cursor: 0,
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            keys: Vec::new(),
            tree: Tree::new(),
            entrance: None,
            entrance_armed: false,
            shelf: None,
            below: false,
            browse: false,
            shelf_rect: Rect::new_empty(),
            verb: None,
            page: Spring::rest(0.0),
        }
    }

    /// The host whose games the row wants now: the focused one once the carousel rests
    /// on it, when paired, saved and online, and when `library_fp` (whose list the shared
    /// library holds) is not already its. A sleeping host is left alone: a fetch wakes it.
    pub(crate) fn wants_shelf<'h>(
        &self,
        hosts: &'h [HostRow],
        library_fp: Option<&str>,
    ) -> Option<&'h HostRow> {
        let h = self.focused(hosts)?;
        let settled = (self.anim.pos - f64::from(self.cursor)).abs() < 0.05;
        let current = self.shelf.as_ref().is_some_and(|s| s.shelf_of(h))
            && library_fp == Some(h.fp_hex.as_str());
        (settled && h.paired && h.saved && h.online && !current).then_some(h)
    }

    pub(crate) fn set_shelf(&mut self, shelf: LibraryScreen) {
        self.shelf = Some(Box::new(shelf));
        self.below = false;
    }

    /// The embedded games, focused or not: the warm-up fills them.
    pub(crate) fn shelf_mut(&mut self) -> Option<&mut LibraryScreen> {
        self.shelf.as_deref_mut()
    }

    /// The games the focus is in, for the launch hold and the running refresh.
    pub(crate) fn shelf(&self) -> Option<&LibraryScreen> {
        self.shelf.as_deref().filter(|_| self.below)
    }

    /// Browse games from the card's menu. `false` when there will be no games here to
    /// browse: an offline or unpaired card.
    pub(crate) fn browse(&mut self, hosts: &[HostRow]) -> bool {
        self.browse = self.focused(hosts).is_some_and(|h| h.paired && h.online);
        self.browse
    }

    /// The games drawn under the row: the focused card's.
    fn shelf_live(&self, hosts: &[HostRow]) -> bool {
        let focused = self.focused(hosts);
        (self.shelf.as_ref()).is_some_and(|s| focused.is_some_and(|h| s.shelf_of(h)))
    }

    /// Hand focus down to the games, if they have titles.
    fn go_below(&mut self, hosts: &[HostRow]) -> Option<MenuPulse> {
        if !self.shelf_live(hosts) {
            return Some(MenuPulse::Boundary);
        }
        let shelf = self.shelf.as_mut()?;
        if !shelf.has_titles() {
            return Some(MenuPulse::Boundary);
        }
        shelf.set_quiet(false);
        self.below = true;
        self.browse = false;
        self.verb = None;
        Some(MenuPulse::Move)
    }

    /// Out of the games: to the first verb, where Down came from, or to the card.
    fn go_up(&mut self, to_verbs: bool) {
        self.below = false;
        self.verb = to_verbs.then_some(0);
        if let Some(shelf) = self.shelf.as_mut() {
            shelf.set_quiet(true);
        }
    }

    /// OK went down: the plate dips under the focused card, verb, or poster.
    pub(crate) fn press(&mut self) {
        match self.shelf.as_mut().filter(|_| self.below) {
            Some(shelf) => shelf.press(),
            None => self.tree.press(),
        }
    }

    /// A finger drag on the games scrolls them.
    pub(crate) fn pan(&mut self, p: Pointer) -> bool {
        self.below && self.shelf.as_mut().is_some_and(|s| s.pan(p))
    }

    /// Focus follows the tile key, not the index.
    fn reconcile(&mut self, hosts: &[HostRow]) {
        let keys: Vec<String> = hosts
            .iter()
            .map(|h| h.key.clone())
            .chain([ADD_KEY.to_string(), SCAN_KEY.to_string()])
            .collect();
        if keys != self.keys {
            let followed = self
                .keys
                .get(self.cursor as usize)
                .and_then(|old| keys.iter().position(|k| k == old));
            self.cursor = followed.unwrap_or(self.cursor as usize).min(keys.len() - 1) as i32;
            // Leave the spring; render chases the new cursor so the strip animates.
            self.keys = keys;
        }
    }

    fn focused<'h>(&self, hosts: &'h [HostRow]) -> Option<&'h HostRow> {
        hosts.get(self.cursor as usize)
    }

    /// The focused tile's key: a host's, or an action tile's sentinel.
    pub(crate) fn focused_key(&self) -> Option<&str> {
        self.keys
            .get(self.cursor.max(0) as usize)
            .map(String::as_str)
    }

    fn slot<'h>(&self, hosts: &'h [HostRow]) -> Slot<'h> {
        slot_at(self.cursor.max(0) as usize, hosts)
    }

    fn len(hosts: &[HostRow]) -> usize {
        hosts.len() + 2
    }

    fn tile_id(key: &str) -> Id {
        Id::new(key, 0)
    }

    fn index_of(&self, id: Id) -> Option<usize> {
        self.keys.iter().position(|k| Self::tile_id(k) == id)
    }

    /// Left or Right through the tree. With no rects yet, or at an end, the index step
    /// moves or bumps.
    fn travel(&mut self, dir: MenuDir, len: usize) -> Option<MenuPulse> {
        self.browse = false;
        self.verb = None;
        let focused = self
            .keys
            .get(self.cursor.max(0) as usize)
            .map(|k| Self::tile_id(k));
        self.tree.set_focus(focused);
        match self.tree.move_focus(dir).and_then(|id| self.index_of(id)) {
            Some(i) => {
                self.cursor = i as i32;
                Some(MenuPulse::Move)
            }
            None => self.step(if dir == MenuDir::Left { -1 } else { 1 }, len, false),
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.reconcile(ctx.hosts);
        if self.below && !self.shelf_live(ctx.hosts) {
            self.go_up(false);
        }
        if self.below {
            let shelf = self.shelf.as_mut()?;
            let up = ev == MenuEvent::Move(MenuDir::Up) && shelf.at_top();
            if up || ev == MenuEvent::Back {
                self.go_up(up);
                return Some(MenuPulse::Move);
            }
            return shelf.menu(ev, ctx, fx);
        }
        let len = Self::len(ctx.hosts);
        let slot = self.slot(ctx.hosts);
        let on = verbs(&slot);
        if let Some(i) = self.verb {
            match ev {
                MenuEvent::Move(MenuDir::Left) if i > 0 => {
                    self.verb = Some(i - 1);
                    return Some(MenuPulse::Move);
                }
                MenuEvent::Move(MenuDir::Right) if i + 1 < on.len() => {
                    self.verb = Some(i + 1);
                    return Some(MenuPulse::Move);
                }
                MenuEvent::Move(MenuDir::Left | MenuDir::Right) => {
                    return Some(MenuPulse::Boundary)
                }
                MenuEvent::Move(MenuDir::Up) | MenuEvent::Back => {
                    self.verb = None;
                    return Some(MenuPulse::Move);
                }
                MenuEvent::Move(MenuDir::Down) => return self.go_below(ctx.hosts),
                MenuEvent::Confirm => {
                    if let (Some(&v), Slot::Host(h)) = (on.get(i), slot) {
                        run_verb(v, h, ctx, fx);
                    }
                    return Some(MenuPulse::Confirm);
                }
                // The pad shortcuts act on the card, as they do from it.
                _ => {}
            }
        }
        match ev {
            MenuEvent::Move(dir @ (MenuDir::Left | MenuDir::Right)) => self.travel(dir, len),
            MenuEvent::JumpBack => self.step(-5, len, true),
            MenuEvent::JumpForward => self.step(5, len, true),
            MenuEvent::Confirm => {
                match self.slot(ctx.hosts) {
                    Slot::AddHost => {
                        fx.push(Screen::AddHost(super::add_host::AddHostScreen::new()))
                    }
                    Slot::Rescan => {
                        fx.cmds.push(ConsoleCmd::Probe);
                        fx.toast = Some("Scanning for hosts…".into());
                    }
                    Slot::Host(h) if !h.paired => fx.push(Screen::Pair(
                        super::pair::PairScreen::new(h, ctx.device_name),
                    )),
                    Slot::Host(h) if !h.online && h.can_wake => {
                        // Wake first; the overlay connects once the host answers.
                        fx.cmds.push(ConsoleCmd::Wake {
                            key: h.key.clone(),
                            then_connect: true,
                        });
                    }
                    Slot::Host(h) => {
                        // Dial even when the pips say offline: a routed or VPN host
                        // can miss mDNS and still answer.
                        fx.connect = Some(ConnectIntent {
                            addr: h.addr.clone(),
                            port: h.port,
                            fp_hex: h.fp_hex.clone(),
                            launch: None,
                            title: match &h.pin {
                                Some(p) => format!("{} · {}", h.name, p.name),
                                None => h.name.clone(),
                            },
                            request_access: false,
                            preset: h.pin.as_ref().map(|p| p.id.clone()),
                        });
                    }
                }
                Some(MenuPulse::Confirm)
            }
            // The card's menu: Y on a pad, OK held on a remote.
            MenuEvent::Secondary => match self.focused(ctx.hosts) {
                Some(h) => {
                    fx.options(super::card_menu::CardMenu::for_host(h));
                    Some(MenuPulse::Confirm)
                }
                None => Some(MenuPulse::Boundary),
            },
            // Sector is the ring; this carousel steps on `Move`.
            MenuEvent::Sector(_) => None,
            MenuEvent::Tertiary => {
                fx.tab = Some(crate::shell::Tab::Settings);
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop(); // root pop is quit (shell rule)
                None
            }
            // Up is the tab strip's.
            MenuEvent::Move(MenuDir::Up) => Some(MenuPulse::Boundary),
            MenuEvent::Move(MenuDir::Down) if on.is_empty() => self.go_below(ctx.hosts),
            MenuEvent::Move(MenuDir::Down) => {
                self.verb = Some(0);
                Some(MenuPulse::Move)
            }
        }
    }

    /// Only the focused card activates. A press that also connected would start a session
    /// for a host that was merely aimed at. A verb acts on the first press.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        self.reconcile(ctx.hosts);
        if self.shelf_live(ctx.hosts) && p.hits(self.shelf_rect) {
            let Some(shelf) = self.shelf.as_mut() else {
                return false;
            };
            if !self.below && shelf.has_titles() {
                shelf.set_quiet(false);
                self.below = true;
            }
            return shelf.pointer(p, ctx, fx);
        }
        if self.below && matches!(p.kind, PointerKind::Move | PointerKind::Press) {
            self.go_up(false);
        }
        let hit = self.tree.hit(p.x as f32, p.y as f32);
        let verb = (0..verbs(&self.slot(ctx.hosts)).len()).find(|&i| hit == Some(verb_id(i)));
        if let Some(i) = verb {
            let moved = self.verb != Some(i);
            self.verb = Some(i);
            return match p.kind {
                PointerKind::Press => {
                    self.menu(MenuEvent::Confirm, ctx, fx);
                    true
                }
                PointerKind::Move => moved,
                _ => false,
            };
        }
        let len = Self::len(ctx.hosts);
        match p.kind {
            PointerKind::Scroll { up } => {
                self.step(if up { -1 } else { 1 }, len, false);
                true
            }
            // Hover focuses, so the press that follows is the one that OPENS the card rather
            // than the one that reaches it. The move-then-press fallback below stays for a
            // pointer that cannot hover: a touchscreen sends Press with no Move before it.
            PointerKind::Move => match self.pick(p, len) {
                Some(i) if i != self.cursor as usize || self.verb.is_some() => {
                    self.cursor = i as i32;
                    self.verb = None;
                    true
                }
                _ => false,
            },
            PointerKind::Press => match self.pick(p, len) {
                Some(i) if i == self.cursor as usize => {
                    self.verb = None;
                    self.menu(MenuEvent::Confirm, ctx, fx);
                    true
                }
                Some(i) => {
                    self.cursor = i as i32;
                    self.verb = None;
                    true
                }
                None => false,
            },
            _ => false,
        }
    }

    /// The painted card under `p`, by key: discovery can reorder the row between draw and
    /// press.
    fn pick(&self, p: Pointer, len: usize) -> Option<usize> {
        let i = self.index_of(self.tree.hit(p.x as f32, p.y as f32)?)?;
        (i < len).then_some(i)
    }

    fn step(&mut self, delta: i32, len: usize, clamp: bool) -> Option<MenuPulse> {
        self.verb = None;
        match step_cursor(self.cursor, len, delta, clamp) {
            StepResult::Moved(to) => {
                self.cursor = to;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                self.bump = Spring {
                    pos: self.bump.pos,
                    vel: -BUMP_V * f64::from(delta.signum()),
                };
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// The focused tile as a screen reader speaks it: the name, then the line under it.
    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
        if let Some(shelf) = self.shelf() {
            return shelf.announcement(ctx);
        }
        let hosts = ctx.hosts;
        if let Some(v) = self
            .verb
            .and_then(|i| verbs(&self.slot(hosts)).get(i).copied())
        {
            return Some(v.label().trim_end_matches('\u{2026}').to_string());
        }
        let say = |(title, sub): (&str, &str)| format!("{title}, {sub}");
        Some(match self.slot(hosts) {
            Slot::Host(h) => format!("{}, {}", h.name, status(h).0),
            Slot::AddHost => say(action_text(ActionTile::AddHost)),
            Slot::Rescan => say(action_text(ActionTile::Rescan)),
        })
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if let Some(shelf) = self.shelf() {
            return shelf.hints(ctx);
        }
        let mut hints = Vec::new();
        match self.slot(ctx.hosts) {
            Slot::AddHost => hints.push(Hint::new(HintKey::Confirm, "Add Host")),
            Slot::Rescan => hints.push(Hint::new(HintKey::Confirm, "Scan Again")),
            Slot::Host(h) if !h.paired => hints.push(Hint::new(HintKey::Confirm, "Pair…")),
            Slot::Host(h) if !h.online && h.can_wake => {
                hints.push(Hint::new(HintKey::Confirm, "Wake & Connect"))
            }
            // Same press, honest word: a host with a game up is one you get back INTO,
            // and the tile is already naming the title above it.
            Slot::Host(h) if !h.running.is_empty() => {
                hints.push(Hint::new(HintKey::Confirm, "Resume"))
            }
            Slot::Host(_) => hints.push(Hint::new(HintKey::Confirm, "Connect")),
        }
        if self.focused(ctx.hosts).is_some() {
            hints.push(Hint::new(HintKey::Secondary, "Options"));
        }
        hints.push(Hint::new(HintKey::Tertiary, "Settings"));
        hints.push(Hint::new(HintKey::Back, "Quit"));
        hints
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        self.reconcile(ctx.hosts);
        let reduced = super::settings::reduce_ui_res(ctx.settings, ctx.platform, ctx.fallback_ui);
        self.anim
            .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
        self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        // Reduced motion drops bump travel, not the chase. Freezing the cursor
        // spring would jump the row; the refusal is already a Boundary haptic.
        if crate::theme::reduce_motion() {
            self.bump = Spring::rest(0.0);
        }
        // Origin is the cursor, not 0: a restored selection must assemble in place.
        if !self.entrance_armed {
            self.entrance_armed = true;
            self.entrance = Some(Entrance::new(
                entrances::CARDS,
                self.cursor.max(0) as usize,
                ctx.t,
            ));
        }
        if self.entrance.is_some_and(|e| e.done(ctx.t)) {
            self.entrance = None;
        }
        if self.below && !self.shelf_live(ctx.hosts) {
            self.go_up(false);
        }

        let w = f64::from(rect.width());
        let margin = edge(k);
        let tile_w = (TILE_W * k).min((w - 2.0 * margin) * 0.8);
        let tile_h = TILE_H * k;
        let pitch = tile_w + TILE_GAP * k;
        let len = Self::len(ctx.hosts);
        let slot = self.slot(ctx.hosts);
        let verbs = verbs(&slot);
        if self.verb.is_some_and(|i| i >= verbs.len()) {
            self.verb = None;
        }

        // With focus in the games, the row and its verbs slide up out of their way.
        let verbs_h = if verbs.is_empty() {
            0.0
        } else {
            (VERB_AIR + BUTTON_H) * k
        };
        let block = ROW_AIR * k + tile_h + verbs_h;
        let page_to = if self.below { block } else { 0.0 };
        if crate::theme::reduce_motion() {
            self.page = Spring::rest(page_to);
        } else {
            self.page
                .step_spec(page_to, crate::anim::springs::FOCUS, dt);
            self.page.settle(page_to, 0.25, 4.0);
        }
        let top = f64::from(rect.top) - self.page.pos;
        let grouping = grouping(ctx.settings);
        let captions = grouping != "none";
        let row_y = top + (ROW_AIR + if captions { GROUP_AIR } else { 0.0 }) * k;

        // The focused card rests on the margin until the row's end reaches the screen's.
        let span = (len as f64 - 1.0) * pitch + tile_w;
        let most = ((span - (w - 2.0 * margin)) / pitch).max(0.0);
        let s = self.anim.pos.clamp(0.0, most);
        let bump = self.bump.pos * k;
        let x_of = |i: f64| f64::from(rect.left) + margin + (i - s) * pitch + bump;

        // A scroll the row's spring drives, so the plate rides the row and springs only
        // between cards. The viewport spans three widths: a scaled screen in a push must
        // not show its clip.
        let slack = 2.0 * pitch;
        let offset = (slack + s * pitch - bump) as f32;
        let strip = Id::new("hosts", 0);
        self.tree.set_offset(strip, offset);
        let content_w = 2.0 * slack + 3.0 * w + len.saturating_sub(1) as f64 * pitch;
        let viewport = Rect::from_xywh(-w as f32, 0.0, 3.0 * w as f32, rect.height());
        let origin = (rect.left + viewport.left - offset, rect.top);
        let mut row = El::scroll(strip, Axis::Horizontal)
            .group(Group::Row)
            .child(El::column().place(Rect::from_xywh(0.0, 0.0, content_w as f32, 1.0)));
        for i in 0..len {
            let f = 1.0 - (i as f64 - self.anim.pos).abs().min(1.0);
            let ent = self
                .entrance
                .map_or(EntranceAt::SETTLED, |e| e.at(i, ctx.t));
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let scale = (1.0 + FOCUS_LIFT * f) * arrive;
            let x = x_of(i as f64);
            let cx = x + tile_w / 2.0;
            let cy = row_y + tile_h / 2.0 + (1.0 - ent.travel) * ENTER_RISE * k;
            let tile = Rect::from_xywh(
                x as f32,
                (cy - tile_h / 2.0) as f32,
                tile_w as f32,
                tile_h as f32,
            );
            // The node is the drawn card, entrance and lift included.
            let drawn = Rect::from_xywh(
                (cx - tile_w * scale / 2.0) as f32,
                (cy - tile_h * scale / 2.0) as f32,
                (tile_w * scale) as f32,
                (tile_h * scale) as f32,
            );
            let off =
                x + tile_w < f64::from(rect.left) - pitch || x > f64::from(rect.right) + pitch;
            let node = if off {
                El::column()
            } else {
                let look = TileLook {
                    tile,
                    center: (cx, cy),
                    scale,
                    fade: ent.fade,
                    k,
                };
                let slot = slot_at(i, ctx.hosts);
                // The first card of each band carries its name above it.
                let caption = match (&slot, i.checked_sub(1).map(|j| slot_at(j, ctx.hosts))) {
                    (Slot::Host(h), prev) if captions => {
                        let here = group_of(h, grouping);
                        let before = match prev {
                            Some(Slot::Host(p)) => group_of(p, grouping),
                            _ => None,
                        };
                        (here != before).then_some(here).flatten()
                    }
                    _ => None,
                };
                El::paint(move |canvas, _| {
                    if let Some(text) = &caption {
                        let (x, y) = (f64::from(tile.left), f64::from(tile.top) - 10.0 * k);
                        let size = GROUP_CAPTION * k;
                        let tracking = 1.2 * k;
                        let upper = text.to_uppercase();
                        fonts.draw_tracked(
                            canvas,
                            &upper,
                            x,
                            y,
                            W::SemiBold,
                            size,
                            tracking,
                            fg(0.55),
                        );
                    }
                    look.paint(canvas, fonts, &slot)
                })
            };
            row = row.child(
                node.id(Self::tile_id(&self.keys[i]))
                    .focusable((TILE_CORNER * k * scale) as f32)
                    .place(drawn.with_offset((-origin.0, -origin.1))),
            );
        }

        // The verbs sit under the focused card and follow it along the row. A scroll with
        // no travel clips them at the band as they slide up.
        let fade = (1.0 - self.page.pos / block.max(1.0)).clamp(0.0, 1.0) as f32;
        let mut vx = x_of(f64::from(self.cursor));
        let vy = row_y + tile_h + VERB_AIR * k;
        // The clip reaches left past the margin for the plate's outset.
        let air = (32.0 * k) as f32;
        let mut under = El::scroll(Id::new("verbs", 0), Axis::Vertical)
            .group(Group::Row)
            .place(Rect::from_xywh(
                -air,
                0.0,
                rect.width() + air,
                rect.height(),
            ));
        for (i, v) in verbs.iter().enumerate() {
            let label = v.label();
            let bw = button_w(fonts, label, k);
            let r = Rect::from_xywh(
                (vx - f64::from(rect.left)) as f32 + air,
                (vy - f64::from(rect.top)) as f32,
                bw as f32,
                (BUTTON_H * k) as f32,
            );
            under = under.child(
                El::paint(move |canvas, r| {
                    // A layer only mid-scroll: each is a framebuffer round trip on a tiled GPU.
                    let layered = fade < 0.999;
                    if layered {
                        crate::theme::save_layer_alpha(canvas, r.with_outset((8.0, 8.0)), fade);
                    }
                    button(canvas, fonts, label, r, k);
                    if layered {
                        canvas.restore();
                    }
                })
                .id(verb_id(i))
                .focusable((BUTTON_H * k / 2.0) as f32)
                .place(r),
            );
            vx += bw + VERB_GAP * k;
        }
        let root = El::column().child(row.place(viewport)).child(under);
        let frame = self.tree.layout(root, rect);
        let focused = match self.verb {
            _ if self.below => None,
            Some(i) => Some(verb_id(i)),
            None => (self.keys.get(self.cursor.max(0) as usize)).map(|k| Self::tile_id(k)),
        };
        self.tree.set_focus(focused);
        // The plate is the focus mark: it lifts the card, so the card draws no halo.
        self.tree.paint_focus(canvas, frame, k as f32, dt, reduced);

        self.shelf_rect = Rect::new_empty();
        let games_top = top + block + GAMES_AIR * k;
        if self.shelf_live(ctx.hosts) {
            // To the content's foot, not the screen's: the grid runs on under the band by
            // its own clip, and measures its last row against this edge.
            let games = Rect::from_ltrb(rect.left, games_top as f32, rect.right, rect.bottom);
            if let Some(shelf) = self.shelf.as_mut() {
                shelf.render(canvas, games, k, dt, fonts, ctx);
            }
            self.shelf_rect = games;
            if self.browse && self.shelf.as_ref().is_some_and(|s| s.has_titles()) {
                self.go_below(ctx.hosts);
            }
        } else {
            let line = if ctx.hosts.is_empty() {
                Some("Hosts on this network appear automatically. Add one by address for everything else.".to_string())
            } else {
                no_games(&slot)
            };
            if let Some(line) = line {
                fonts.draw_clipped(
                    canvas,
                    &line,
                    f64::from(rect.left) + margin,
                    games_top + 30.0 * k,
                    W::Medium,
                    15.0 * k,
                    fg(0.62),
                    w - 2.0 * margin,
                );
            }
        }
    }
}

/// One card as the row draws it this frame.
#[derive(Clone, Copy)]
struct TileLook {
    tile: Rect,
    center: (f64, f64),
    scale: f64,
    /// Entrance fade.
    fade: f64,
    k: f64,
}

impl TileLook {
    fn paint(&self, canvas: &Canvas, fonts: &Fonts, slot: &Slot<'_>) {
        let TileLook {
            tile,
            center: (cx, cy),
            scale,
            fade,
            k,
        } = *self;
        canvas.save();
        canvas.translate((cx as f32, cy as f32));
        canvas.scale((scale as f32, scale as f32));
        canvas.translate((-cx as f32, -cy as f32));
        // Only the entrance fades a card; a settled one draws straight onto the field.
        let layered = fade < 0.999;
        if layered {
            let bounds = tile.with_outset(((24.0 * k) as f32, (24.0 * k) as f32));
            crate::theme::save_layer_alpha(canvas, bounds, fade as f32);
        }
        match slot {
            Slot::Host(h) => draw_host_tile(canvas, fonts, h, tile, k),
            Slot::AddHost => draw_action_tile(canvas, fonts, tile, k, ActionTile::AddHost),
            Slot::Rescan => draw_action_tile(canvas, fonts, tile, k, ActionTile::Rescan),
        }
        if layered {
            canvas.restore();
        }
        canvas.restore();
    }
}

/// A card: the badge, then what the host is doing over its name, and a lock while OK would
/// pair first.
fn draw_host_tile(canvas: &Canvas, fonts: &Fonts, h: &HostRow, rect: Rect, k: f64) {
    let stroke = if h.saved {
        PanelStroke::Plain(0.08)
    } else {
        PanelStroke::GradientDashed
    };
    crate::theme::panel(canvas, rect, TILE_CORNER as f32, None, stroke, k as f32);
    let pad = 20.0 * k;
    let cy = f64::from(rect.center_y());
    let l = f64::from(rect.left) + pad;
    draw_badge(
        canvas,
        fonts,
        &h.name,
        &h.os,
        h.saved,
        l,
        cy - BADGE * k / 2.0,
        k,
    );
    let tx = l + (BADGE + 16.0) * k;
    let mut right = f64::from(rect.right) - pad;
    if !h.paired {
        draw_lock(canvas, right - 11.0 * k, cy - 9.0 * k, k);
        right -= 23.0 * k;
    }
    let max_w = right - tx;
    let (line, ink) = status(h);
    let sub_base = cy - 7.0 * k;
    let mut x = tx;
    if h.online || !h.running.is_empty() {
        let r = 3.5 * k;
        let dot = ((x + r) as f32, (sub_base - 4.6 * k) as f32);
        canvas.draw_circle(dot, r as f32, &fill(crate::theme::live()));
        x += 2.0 * r + 6.0 * k;
    }
    fonts.draw_clipped(
        canvas,
        &line,
        x,
        sub_base,
        W::SemiBold,
        14.0 * k,
        ink,
        tx + max_w - x,
    );
    fonts.draw_clipped(
        canvas,
        &h.name,
        tx,
        cy + 18.0 * k,
        W::Bold,
        21.0 * k,
        fg(1.0),
        max_w,
    );
}

/// What stands where the games would when the focused card has none to show.
fn no_games(slot: &Slot<'_>) -> Option<String> {
    Some(match slot {
        Slot::Host(h) if !h.paired => format!("Pair with {} to see its games here.", h.name),
        Slot::Host(h) if !h.online && h.can_wake => {
            format!("{} is asleep. Wake it to see its games.", h.name)
        }
        Slot::Host(h) if !h.online => format!(
            "{} is offline. Its games show here once it is back.",
            h.name
        ),
        _ => return None,
    })
}

/// A card's one status line and its ink: what the host is doing, whether OK will reach
/// it, and the preset a pinned or bound card connects with.
fn status(h: &HostRow) -> (String, Color4f) {
    if let Some(p) = &h.pin {
        return (p.name.clone(), accent_color(p.accent.as_deref()));
    }
    let (base, ink) = if !h.running.is_empty() {
        return (format!("Playing {}", h.running), crate::theme::live());
    } else if !h.saved {
        ("Found on this network".to_string(), fg(0.7))
    } else if !h.paired {
        ("Not paired yet".to_string(), fg(0.7))
    } else if h.online {
        ("Online".to_string(), crate::theme::live())
    } else if h.can_wake {
        ("Offline \u{b7} wakes when you connect".to_string(), fg(0.7))
    } else {
        ("Offline".to_string(), fg(0.7))
    };
    match &h.bound_preset {
        Some(b) => (format!("{base} \u{b7} {}", b.name), ink),
        None => (base, ink),
    }
}

/// `#RRGGBB` accent, or the card's ink. A malformed value falls back.
fn accent_color(hex: Option<&str>) -> skia_safe::Color4f {
    let Some(hex) = hex
        .and_then(|a| a.strip_prefix('#'))
        .filter(|h| h.len() == 6)
    else {
        return fg(0.85);
    };
    let Ok(v) = u32::from_str_radix(hex, 16) else {
        return fg(0.85);
    };
    skia_safe::Color4f::new(
        ((v >> 16) & 0xff) as f32 / 255.0,
        ((v >> 8) & 0xff) as f32 / 255.0,
        (v & 0xff) as f32 / 255.0,
        1.0,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActionTile {
    AddHost,
    Rescan,
}

/// The tile's title and the line over it. Drawn and spoken from the same pair.
fn action_text(kind: ActionTile) -> (&'static str, &'static str) {
    match kind {
        ActionTile::AddHost => ("Add Host", "Register a host by address"),
        ActionTile::Rescan => ("Rescan", "Look for hosts on this network again"),
    }
}

fn draw_action_tile(canvas: &Canvas, fonts: &Fonts, rect: Rect, k: f64, kind: ActionTile) {
    crate::theme::panel(
        canvas,
        rect,
        TILE_CORNER as f32,
        None,
        PanelStroke::GradientDashed,
        k as f32,
    );
    let pad = 20.0 * k;
    let cy = f64::from(rect.center_y());
    let l = f64::from(rect.left) + pad;
    let side = BADGE * k;
    let badge = Rect::from_xywh(l as f32, (cy - side / 2.0) as f32, side as f32, side as f32);
    let rr = RRect::new_rect_xy(badge, (14.0 * k) as f32, (14.0 * k) as f32);
    canvas.draw_rrect(rr, &fill(fg(0.14)));
    canvas.draw_rrect(rr, &stroke(fg(0.35), 1.0));
    let (bcx, bcy) = (l + side / 2.0, cy);
    let ink = fg(0.9);
    let mut p = stroke(ink, (3.0 * k) as f32);
    p.set_stroke_cap(skia_safe::PaintCap::Round);
    let r = 9.0 * k;
    match kind {
        ActionTile::AddHost => {
            canvas.draw_line(
                ((bcx - r) as f32, bcy as f32),
                ((bcx + r) as f32, bcy as f32),
                &p,
            );
            canvas.draw_line(
                (bcx as f32, (bcy - r) as f32),
                (bcx as f32, (bcy + r) as f32),
                &p,
            );
        }
        // Static refresh mark. A spinning tile would claim a sweep that is not running.
        ActionTile::Rescan => {
            let mut arc = PathBuilder::new();
            arc.add_arc(
                Rect::from_xywh(
                    (bcx - r) as f32,
                    (bcy - r) as f32,
                    (2.0 * r) as f32,
                    (2.0 * r) as f32,
                ),
                -45.0,
                280.0,
            );
            canvas.draw_path(&arc.detach(), &p);
            let head = 4.6 * k;
            let (hx, hy) = (bcx + r * 0.72, bcy - r * 0.72);
            let mut tip = PathBuilder::new();
            tip.move_to(((hx - head) as f32, (hy - head * 0.2) as f32));
            tip.line_to(((hx + head * 0.5) as f32, (hy - head * 1.1) as f32));
            tip.line_to(((hx + head * 0.2) as f32, (hy + head * 0.7) as f32));
            tip.close();
            canvas.draw_path(&tip.detach(), &fill(ink));
        }
    }

    let (title, sub) = action_text(kind);
    let tx = l + side + 16.0 * k;
    let max_w = f64::from(rect.right) - pad - tx;
    fonts.draw_clipped(
        canvas,
        sub,
        tx,
        cy - 7.0 * k,
        W::SemiBold,
        14.0 * k,
        fg(0.6),
        max_w,
    );
    fonts.draw_clipped(
        canvas,
        title,
        tx,
        cy + 18.0 * k,
        W::Bold,
        21.0 * k,
        fg(1.0),
        max_w,
    );
}

/// The host's badge: a white tile with its OS mark, else its initial, in the accent. A host
/// not saved yet gets a quiet outline instead. Decorative: the name beside it states the host.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_badge(
    canvas: &Canvas,
    fonts: &Fonts,
    name: &str,
    os: &str,
    filled: bool,
    x: f64,
    y: f64,
    k: f64,
) {
    let side = BADGE * k;
    let badge = Rect::from_xywh(x as f32, y as f32, side as f32, side as f32);
    let rr = RRect::new_rect_xy(badge, (14.0 * k) as f32, (14.0 * k) as f32);
    let ink = if filled {
        let mut glow = fill(Color4f::new(1.0, 1.0, 1.0, 0.35));
        glow.set_mask_filter(MaskFilter::blur(
            skia_safe::BlurStyle::Normal,
            (6.0 * k) as f32,
            None,
        ));
        canvas.draw_rrect(rr, &glow);
        canvas.draw_rrect(rr, &fill(Color4f::new(1.0, 1.0, 1.0, 1.0)));
        accent(1.0)
    } else {
        canvas.draw_rrect(rr, &fill(fg(0.14)));
        canvas.draw_rrect(rr, &stroke(fg(0.35), 1.0));
        fg(0.9)
    };
    // ~54% of the badge so the mark sits on it, not cropped to it. `os_mark`
    // letterboxes a non-square master.
    let mark = 28.0 * k;
    let (cx, cy) = (x + side / 2.0, y + side / 2.0);
    let inner = Rect::from_xywh(
        (cx - mark / 2.0) as f32,
        (cy - mark / 2.0) as f32,
        mark as f32,
        mark as f32,
    );
    if let Some(path) = crate::os_marks::os_mark(os, inner) {
        canvas.draw_path(&path, &fill(ink));
        return;
    }
    let letter: String = name
        .trim()
        .chars()
        .next()
        .map(|c| c.to_uppercase().collect())
        .unwrap_or_else(|| "•".to_string());
    let size = 25.0 * k;
    let tw = fonts.measure(&letter, W::Bold, size) as f64;
    fonts.draw(
        canvas,
        &letter,
        cx - tw / 2.0,
        cy + size * 0.36,
        W::Bold,
        size,
        ink,
    );
}

fn draw_lock(canvas: &Canvas, x: f64, y: f64, k: f64) {
    let ink = fg(0.5);
    let body_w = 11.0 * k;
    let body_h = 8.0 * k;
    let body_top = y + 5.0 * k;
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(x as f32, body_top as f32, body_w as f32, body_h as f32),
            (2.0 * k) as f32,
            (2.0 * k) as f32,
        ),
        &fill(ink),
    );
    let p = stroke(ink, (1.6 * k) as f32);
    let mut shackle = PathBuilder::new();
    let (cx, r) = (x + body_w / 2.0, 3.2 * k);
    shackle.move_to(((cx - r) as f32, body_top as f32));
    shackle.arc_to(
        Rect::from_xywh(
            (cx - r) as f32,
            (body_top - r) as f32,
            (2.0 * r) as f32,
            (2.0 * r) as f32,
        ),
        180.0,
        180.0,
        false,
    );
    canvas.draw_path(&shackle.detach(), &p);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(key: &str, paired: bool, online: bool, can_wake: bool) -> HostRow {
        HostRow {
            key: key.into(),
            id: None,
            name: key.into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: if paired { "ab".into() } else { String::new() },
            paired,
            saved: true,
            online,
            mgmt_port: 47990,
            can_wake,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: None,
            bound_preset: None,
            running: String::new(),
            game_presets: Default::default(),
        }
    }

    fn ctx_settings() -> pf_client_core::trust::Settings {
        pf_client_core::trust::Settings::default()
    }

    #[test]
    fn cursor_follows_the_key_through_churn() {
        let mut s = HomeScreen::new();
        let a = host("a", true, true, false);
        let b = host("b", true, true, false);
        s.reconcile(&[a.clone(), b.clone()]);
        s.cursor = 1;
        s.keys = vec!["a".into(), "b".into(), ADD_KEY.into()];
        // A new host inserted in front; focus must stay on "b".
        let c = host("c", false, true, false);
        s.reconcile(&[c, a, b]);
        assert_eq!(s.cursor, 2);
    }

    #[test]
    fn confirm_routes_by_host_state() {
        let mut settings = ctx_settings();
        let hosts = [
            host("paired-online", true, true, false),
            host("unpaired", false, true, false),
            host("asleep", true, false, true),
        ];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();

        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.connect.is_some());

        let mut fx = Outbox::default();
        s.cursor = 1;
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Push(_))));
        assert!(fx.connect.is_none());

        let mut fx = Outbox::default();
        s.cursor = 2;
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::Wake {
                then_connect: true,
                ..
            })
        ));
    }

    /// Up from the row is the tab strip's. Down reaches the card's verbs, which run on OK;
    /// the last is the card's whole menu, also the hold (Secondary) a remote reaches by
    /// holding OK.
    #[test]
    fn down_reaches_the_verbs_and_every_verb_is_one_press_away() {
        let mut settings = ctx_settings();
        let hosts = [host("paired", true, true, false)];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Android,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: true,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        let mut s = HomeScreen::new();
        let mut go = |s: &mut HomeScreen, ev: MenuEvent| {
            let mut fx = Outbox::default();
            (s.menu(ev, &mut ctx, &mut fx), fx)
        };
        let (pulse, fx) = go(&mut s, MenuEvent::Move(MenuDir::Up));
        assert!(matches!(pulse, Some(MenuPulse::Boundary)) && fx.nav.is_none());
        let (pulse, _) = go(&mut s, MenuEvent::Move(MenuDir::Down));
        assert!(matches!(pulse, Some(MenuPulse::Move)) && s.verb == Some(0));
        let (_, fx) = go(&mut s, MenuEvent::Confirm);
        assert_eq!(
            fx.tab,
            Some(crate::shell::Tab::Games),
            "Games opens its tab"
        );
        // Games, Connect with…, Details…, More…, and no further.
        for _ in 0..3 {
            go(&mut s, MenuEvent::Move(MenuDir::Right));
        }
        let (pulse, _) = go(&mut s, MenuEvent::Move(MenuDir::Right));
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        let (_, fx) = go(&mut s, MenuEvent::Confirm);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(ref sc)) if matches!(**sc, Screen::CardMenu(_))),
            "More… is the card's menu"
        );
        // No games here yet: Down past the verbs refuses, Up returns to the card.
        let (pulse, _) = go(&mut s, MenuEvent::Move(MenuDir::Down));
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        go(&mut s, MenuEvent::Move(MenuDir::Up));
        assert_eq!(s.verb, None);
        let (_, fx) = go(&mut s, MenuEvent::Secondary);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(ref sc)) if matches!(**sc, Screen::CardMenu(_))),
            "the hold opens the host options menu"
        );
    }

    /// The row asks for the games of the host it rests on, once, and again only when the
    /// shared list became another host's. A card in flight, a sleeping host and an
    /// unpaired one ask for nothing.
    #[test]
    fn the_row_asks_for_the_games_of_the_host_it_rests_on() {
        let mut desk = host("desk", true, true, false);
        desk.fp_hex = "d1".into();
        let asleep = host("asleep", true, false, true);
        let hosts = [desk.clone(), asleep];
        let mut s = HomeScreen::new();
        s.reconcile(&hosts);
        assert_eq!(
            s.wants_shelf(&hosts, None).map(|h| h.key.as_str()),
            Some("desk")
        );
        s.set_shelf(LibraryScreen::embedded(&desk));
        assert!(s.wants_shelf(&hosts, Some("d1")).is_none(), "asked once");
        assert!(
            s.wants_shelf(&hosts, Some("other")).is_some(),
            "the list became another host's"
        );
        s.cursor = 1;
        assert!(
            s.wants_shelf(&hosts, None).is_none(),
            "the carousel is still moving"
        );
        s.anim = Spring::rest(1.0);
        assert!(
            s.wants_shelf(&hosts, None).is_none(),
            "a fetch would wake it"
        );
    }

    /// A pin's Confirm connects with that preset (one-off); the overlay title
    /// names the host and the preset.
    #[test]
    fn pinned_card_connects_with_its_preset() {
        let mut settings = ctx_settings();
        let mut pinned = host("ab\0p1", true, true, false);
        pinned.name = "Tower".into();
        pinned.pin = Some(crate::model::PresetChip {
            id: "p1".into(),
            name: "Work".into(),
            accent: None,
            bitrate_kbps: None,
        });
        let hosts = [pinned];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        let intent = fx.connect.expect("a pinned card connects");
        assert_eq!(intent.preset.as_deref(), Some("p1"));
        assert_eq!(intent.title, "Tower · Work");
    }

    #[test]
    fn add_tile_is_always_last() {
        let mut settings = ctx_settings();
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        let mut s = HomeScreen::new();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(b)) if matches!(*b, Screen::AddHost(_)))
        );
    }

    /// A host with a game up is one you get back INTO, and the tile says which game.
    /// Same press either way — only the word changes.
    #[test]
    fn a_running_host_relabels_connect_as_resume() {
        let mut settings = ctx_settings();
        let idle = host("idle", true, true, false);
        let busy = HostRow {
            running: "Elden Ring".into(),
            ..host("busy", true, true, false)
        };
        let hosts = [idle, busy];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        let confirm = |s: &HomeScreen| {
            s.hints(&ctx)
                .into_iter()
                .find(|h| h.key == HintKey::Confirm)
                .map(|h| h.label)
                .unwrap_or_default()
        };
        let mut s = HomeScreen::new();
        s.reconcile(&hosts);
        assert_eq!(confirm(&s), "Connect");
        s.cursor = 1;
        assert_eq!(confirm(&s), "Resume");
    }

    /// Right goes through the tree, three presses between frames included (the culled
    /// tiles are still targets), and the plate lands on the tile focus reached.
    #[test]
    fn the_row_moves_through_the_tree_and_the_plate_lands() {
        let mut settings = ctx_settings();
        let hosts = [
            host("a", true, true, false),
            host("b", true, true, false),
            host("c", true, true, false),
            host("d", true, true, false),
        ];
        let pads: Vec<pf_client_core::menu_nav::PadInfo> = Vec::new();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let rect = Rect::from_xywh(0.0, 64.0, 1280.0, 650.0);
        let mut s = HomeScreen::new();
        let frame = |s: &mut HomeScreen, ctx: &mut Ctx, surface: &mut skia_safe::Surface| {
            ctx.t += 1.0 / 60.0;
            s.render(surface.canvas(), rect, 1.0, 1.0 / 60.0, &fonts, ctx);
        };
        for _ in 0..60 {
            frame(&mut s, &mut ctx, &mut surface);
        }
        let mut fx = Outbox::default();
        for _ in 0..3 {
            s.menu(MenuEvent::Move(MenuDir::Right), &mut ctx, &mut fx);
        }
        assert_eq!(
            s.cursor, 3,
            "three presses in one frame reach the fourth tile"
        );
        let mut landed = false;
        for _ in 0..120 {
            frame(&mut s, &mut ctx, &mut surface);
            landed |= !s.tree.plate_busy();
        }
        let (plate, _) = s.tree.plate_rect().unwrap();
        let tile = s.tree.rect(HomeScreen::tile_id("d")).unwrap();
        assert!(
            (plate.center_x() - tile.center_x()).abs() < 0.5,
            "{plate:?} vs {tile:?}"
        );
        assert!(
            landed,
            "the plate lands and its sweep ends within two seconds"
        );
    }

    /// The row follows Settings: a sort inside bands, Online before Offline, presets by name
    /// with "No preset" last, and a host never connected to last under Last connected. Ties
    /// keep the order the host sent.
    #[test]
    fn the_row_follows_the_order_settings() {
        let chip = |name: &str| crate::model::PresetChip {
            id: name.into(),
            name: name.into(),
            accent: None,
            bitrate_kbps: None,
        };
        let row = || {
            let mut c = host("charlie", true, false, false);
            c.last_used = Some(30);
            c.bound_preset = Some(chip("Travel"));
            let mut a = host("alpha", true, true, false);
            a.last_used = Some(10);
            let b = host("Bravo", true, true, false);
            vec![c, a, b]
        };
        let order = |sort: &str, grouping: &str| -> Vec<String> {
            let mut s = pf_client_core::trust::Settings::default();
            s.extra.insert(HOST_SORT_KEY.into(), sort.into());
            s.extra.insert(HOST_GROUPING_KEY.into(), grouping.into());
            let mut hosts = row();
            arrange(&mut hosts, &s);
            hosts.into_iter().map(|h| h.key).collect()
        };
        assert_eq!(order("added", "none"), ["charlie", "alpha", "Bravo"]);
        assert_eq!(order("name", "none"), ["alpha", "Bravo", "charlie"]);
        assert_eq!(
            order("lastConnected", "none"),
            ["charlie", "alpha", "Bravo"]
        );
        assert_eq!(order("name", "status"), ["alpha", "Bravo", "charlie"]);
        assert_eq!(order("added", "status"), ["alpha", "Bravo", "charlie"]);
        assert_eq!(order("name", "preset"), ["charlie", "alpha", "Bravo"]);
        // Apple's older spelling of the preset grouping still groups.
        assert_eq!(order("name", "profile"), ["charlie", "alpha", "Bravo"]);
        let travel = row().remove(0);
        assert_eq!(group_of(&travel, "preset").as_deref(), Some("Travel"));
        assert_eq!(group_of(&travel, "status").as_deref(), Some("Offline"));
        assert_eq!(group_of(&travel, "none"), None);
    }
}
