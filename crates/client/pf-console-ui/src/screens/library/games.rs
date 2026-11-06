//! The Games tab: the Apple library's rows over the shelf's field, and Customize.
//!
//! Focus walks lines top to bottom: the sort/view pills, the host pill and Customize, the
//! enabled sections in `library_sections` order, and the Games field (grid or shelf) where
//! Games sits among them. The host pill names the shelf's host and who plays there; OK
//! opens Host and profile. An empty section hides. Desktops and Launchers never sit in the
//! field, row on or off. A list that failed, is empty or still loading keeps the chips and
//! the Desktops row, with the state's action where the field was.
//! A collection's shelf has only the pills and its field.

use super::super::collections::{paint_tile, TILE_CORNER, TILE_H, TILE_W};
use super::bar::{pill_id, Pill};
use super::card::{self, Card, DESK_H, DESK_W};
use super::{store_sort, store_view, LibraryScreen};
use crate::el::{Axis, El, Id};
use crate::glyphs::{Hint, HintKey};
use crate::grid::GRID_GAP;
use crate::icons::{self, Icon};
use crate::library::{LibraryGame, LibraryPhase, LibraryView, Section};
use crate::model::HostRow;
use crate::pointer::Pointer;
use crate::screens::card_menu::CardMenu;
use crate::screens::profiles::{paired_hosts, ProfileBook, ProfilesScreen};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen, ScreenView};
use crate::theme::{Fonts, Frame, PanelStroke, W};
use crate::widgets::{button, button_w, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use pf_client_core::profiles::ProfilePick;
use skia_safe::{Canvas, Rect};
use std::cell::RefCell;

/// Recently played shows at most this many.
const RECENT_MAX: usize = 12;
// Design units. A band is its heading, its row, then air.
const HEADING_H: f64 = 30.0;
const BAND_AIR: f64 = 22.0;
const CHIP_H: f64 = 38.0;
/// The host pill and Customize.
const CHIPS: usize = 2;
/// The host pill's label, its face's radius and its side padding.
const PILL_TEXT: f64 = 15.0;
const PILL_FACE: f64 = 11.0;
const PILL_PAD: f64 = 14.0;
const TOP_AIR: f64 = 4.0;

/// Where the D-pad is. The field keeps its own cursor under [`Zone::Grid`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Zone {
    /// The field: the grid or the shelf.
    Grid,
    /// Pill `i` of the sort/view row.
    Bar(usize),
    /// The host pill (0) or Customize (1).
    Chip(usize),
    Band {
        band: usize,
        item: usize,
    },
    /// The action a failed or empty list offers.
    State,
}

pub(super) fn chip_id(i: usize) -> Id {
    Id::new("games-chip", i)
}

/// The chips line's own sideways scroll.
pub(super) fn chips_id() -> Id {
    Id::new("games-chips", 0)
}

pub(super) fn band_item_id(band: usize, item: usize) -> Id {
    Id::new("games-band", band * 10_000 + item)
}

/// Band `band`'s row: its own sideways scroll.
pub(super) fn band_id(band: usize) -> Id {
    Id::new("games-band-row", band)
}

/// Room under a band's row for the plate's shadow, design units: it sits 10 down with a 12
/// blur past the plate's outset.
const PLATE_REACH: f64 = 36.0;

pub(super) fn state_id() -> Id {
    Id::new("games-state", 0)
}

/// The focus target for `zone`, with the field's focused node `field`.
pub(super) fn zone_id(zone: Zone, field: Id) -> Id {
    match zone {
        Zone::Grid => field,
        Zone::Bar(i) => pill_id(i),
        Zone::Chip(i) => chip_id(i),
        Zone::Band { band, item } => band_item_id(band, item),
        Zone::State => state_id(),
    }
}

pub(super) enum Item {
    Desktop(Box<HostRow>),
    /// An index into the shelf's `games`.
    Game(usize),
    /// An index into the shelf's `collections`.
    Collection(usize),
}

pub(super) struct Band {
    pub section: Section,
    pub items: Vec<Item>,
}

/// One row of focus, top to bottom.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Line {
    Bar,
    Chips,
    Band(usize),
    Grid,
    State,
}

pub(super) fn line_of(z: Zone) -> Line {
    match z {
        Zone::Bar(_) => Line::Bar,
        Zone::Chip(_) => Line::Chips,
        Zone::Band { band, .. } => Line::Band(band),
        Zone::Grid => Line::Grid,
        Zone::State => Line::State,
    }
}

/// Where focus lands on arrival: the state's action, else the first band above the field,
/// else the field, else the first line there is.
pub(super) fn seat(lines: &[Line]) -> Zone {
    let above = || lines.iter().take_while(|l| **l != Line::Grid);
    let line = (lines.iter().find(|l| **l == Line::State))
        .or_else(|| above().find(|l| matches!(l, Line::Band(_))))
        .or_else(|| lines.iter().find(|l| **l == Line::Grid))
        .or_else(|| lines.first());
    match line {
        Some(Line::Band(band)) => Zone::Band {
            band: *band,
            item: 0,
        },
        Some(Line::Chips) => Zone::Chip(0),
        Some(Line::Bar) => Zone::Bar(0),
        Some(Line::State) => Zone::State,
        _ => Zone::Grid,
    }
}

/// The host pill's face: the remembered profile, `Some(None)` for Ask every time, `None`
/// where who plays is no choice.
type PillFace = Option<Option<ProfilePick>>;

impl LibraryScreen {
    /// The shelf's host as the carousel has it now: a pick made since the shelf opened.
    fn live_host<'a>(&'a self, hosts: &'a [HostRow]) -> &'a HostRow {
        let key = self.host.host_key();
        hosts.iter().find(|h| h.key == key).unwrap_or(&self.host)
    }

    /// Focus the host pill, as a shelf swapped in from Host and profile does.
    pub(crate) fn focus_pill(&mut self) {
        self.zone = Zone::Chip(0);
        self.seated = true;
    }

    /// The shelf lays out as the Games tab: not a collection or a search, not under the
    /// Hosts row.
    pub(super) fn sectioned(&self) -> bool {
        !self.drilled && !self.embedded
    }

    /// `h` is this shelf's host; a pinned card's shelf counts its primary row.
    fn own(&self, h: &HostRow) -> bool {
        h.key == self.host.host_key()
    }

    pub(super) fn shows(&self, s: Section) -> bool {
        self.sections.iter().any(|&(x, on)| x == s && on)
    }

    /// The field keeps this title out. The desktop and the launchers live only in their own
    /// rows: a row switched off hides them, it does not drop them into the grid. The shelf
    /// under the Hosts row follows the Games tab.
    pub(super) fn banded(&self, g: &LibraryGame) -> bool {
        if self.embedded {
            return g.leads();
        }
        self.sectioned() && (!self.shows(Section::Games) || g.leads())
    }

    /// The bands with something in them, in order; `.1` of them sit above the field. Until
    /// the list is ready only Desktops, which needs no list, shows.
    pub(super) fn bands(&self, ctx: &Ctx) -> (Vec<Band>, usize) {
        let mut out = Vec::new();
        let mut before = None;
        if !self.sectioned() {
            return (out, 0);
        }
        let ready = matches!(self.phase, LibraryPhase::Ready);
        let favorites = crate::library::favorites(ctx.settings, &self.host.fp_hex);
        for &(section, on) in &self.sections {
            if section == Section::Games {
                before = Some(out.len());
                continue;
            }
            if !on || (!ready && section != Section::Desktops) {
                continue;
            }
            let items: Vec<Item> = match section {
                Section::Desktops => {
                    // This shelf's host first: its tile led the grid.
                    let mut hosts: Vec<&HostRow> = paired_hosts(ctx.hosts).collect();
                    hosts.sort_by_key(|h| !self.own(h));
                    if !hosts.iter().any(|h| self.own(h)) {
                        hosts.insert(0, &self.host);
                    }
                    hosts
                        .into_iter()
                        .map(|h| Item::Desktop(Box::new(h.clone())))
                        .collect()
                }
                Section::Recent => {
                    let mut played: Vec<(u64, usize)> = (self.games.iter().enumerate())
                        .filter(|(_, g)| !g.leads())
                        .filter_map(|(i, g)| Some((g.stats.as_ref()?.last_played_unix_ms, i)))
                        .filter(|&(at, _)| at > 0)
                        .collect();
                    played.sort_by_key(|&(at, _)| std::cmp::Reverse(at));
                    (played.into_iter().take(RECENT_MAX))
                        .map(|(_, i)| Item::Game(i))
                        .collect()
                }
                Section::Favorites => {
                    // The shelf's sort, then what a band keeps out of the grid.
                    let mut marked: Vec<usize> = (0..self.games.len())
                        .filter(|&i| favorites.contains(&self.games[i].id))
                        .collect();
                    marked.sort_by_key(|i| self.view.iter().position(|v| v == i));
                    marked.sort_by_key(|i| !self.view.contains(i));
                    marked.into_iter().map(Item::Game).collect()
                }
                Section::Launchers => (0..self.games.len())
                    .filter(|&i| self.games[i].launcher)
                    .map(Item::Game)
                    .collect(),
                Section::Collections => (0..self.collections.len()).map(Item::Collection).collect(),
                Section::Games => unreachable!("handled above"),
            };
            if !items.is_empty() {
                out.push(Band { section, items });
            }
        }
        let before = before.unwrap_or(out.len());
        (out, before)
    }

    /// Every line this frame draws, top to bottom. The state card is one while the list is
    /// not ready, focusable only when it offers an action ([`Self::focus_lines`]). A plain
    /// shelf shows a spinner until its entrance, so its pills wait for that too.
    pub(super) fn lines(&self, bands: &[Band], before: usize) -> Vec<Line> {
        let ready = matches!(self.phase, LibraryPhase::Ready);
        let field = ready && self.len() > 0;
        let mut out = Vec::new();
        if field && !self.embedded && (self.sectioned() || self.entrance_armed) {
            out.push(Line::Bar);
        }
        if self.sectioned() {
            out.push(Line::Chips);
        }
        out.extend((0..before).map(Line::Band));
        if field {
            out.push(Line::Grid);
        }
        if !ready || self.no_match() {
            out.push(Line::State);
        }
        out.extend((before..bands.len()).map(Line::Band));
        out
    }

    /// The lines focus can stand on.
    fn focus_lines(&self, mut lines: Vec<Line>) -> Vec<Line> {
        if self.state_action().is_none() || self.embedded {
            lines.retain(|l| *l != Line::State);
        }
        lines
    }

    /// This frame's bands and lines, the zone moved onto one that exists. Until the list is
    /// ready and seated, the zone is where focus lands on arrival ([`seat`]).
    pub(super) fn place_zone(&mut self, ctx: &Ctx) -> (Vec<Band>, Vec<Line>) {
        let (bands, before) = self.bands(ctx);
        let lines = self.lines(&bands, before);
        let focus = self.focus_lines(lines.clone());
        let chips = CHIPS;
        self.zone = if self.seated {
            self.clamp_zone(&focus, &bands, chips)
        } else {
            seat(&focus)
        };
        if matches!(self.phase, LibraryPhase::Ready) {
            self.seated = true;
        }
        (bands, lines)
    }

    /// The zone, moved onto something that still exists: bands come and go with the data.
    fn clamp_zone(&self, lines: &[Line], bands: &[Band], chips: usize) -> Zone {
        let has = |l: Line| lines.contains(&l);
        match self.zone {
            Zone::Chip(i) if has(Line::Chips) => Zone::Chip(i.min(chips - 1)),
            Zone::Band { band, item } if has(Line::Band(band)) => Zone::Band {
                band,
                item: item.min(bands[band].items.len() - 1),
            },
            Zone::Bar(i) if has(Line::Bar) => Zone::Bar(i.min(Pill::all(true).len() - 1)),
            z @ (Zone::Grid | Zone::State) if has(line_of(z)) => z,
            _ => seat(lines),
        }
    }

    /// The field hands a vertical move to the next line: always on the shelf, from the
    /// grid's top or bottom row. Undrawn, the grid hands off whole.
    fn field_edge(&self, down: bool) -> bool {
        if self.view_mode == LibraryView::Shelf {
            return true;
        }
        self.grid_shape().is_none_or(|shape| {
            let row = shape.cell_of(self.cursor.max(0) as usize).0;
            if down {
                row + 1 >= shape.rows()
            } else {
                row == 0
            }
        })
    }

    /// The D-pad across the lines and the field's edges. `None` leaves it to the field.
    pub(super) fn zone_menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<Option<MenuPulse>> {
        if matches!(ev, MenuEvent::Move(_)) {
            self.follow = true;
        }
        let (bands, lines) = self.place_zone(ctx);
        let lines = self.focus_lines(lines);
        let chips = CHIPS;
        let Some(at) = lines.iter().position(|&l| l == line_of(self.zone)) else {
            // Nothing to stand on: Up still reaches the tabs, Back still leaves.
            return Some(match ev {
                MenuEvent::Move(MenuDir::Up) => Some(MenuPulse::Boundary),
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                _ => None,
            });
        };
        match ev {
            MenuEvent::Move(dir @ (MenuDir::Up | MenuDir::Down)) => {
                let down = dir == MenuDir::Down;
                if self.zone == Zone::Grid && !self.field_edge(down) {
                    return None;
                }
                let next = if down {
                    lines.get(at + 1).copied()
                } else {
                    at.checked_sub(1).map(|i| lines[i])
                };
                let Some(line) = next else {
                    return Some(Some(MenuPulse::Boundary));
                };
                self.enter(line, down, &bands, chips);
                self.seated = true;
                Some(Some(MenuPulse::Move))
            }
            _ if self.zone == Zone::Grid => None,
            MenuEvent::Move(dir) => {
                let (i, len) = match self.zone {
                    Zone::Chip(i) => (i, chips),
                    Zone::Bar(i) => (i, Pill::all(true).len()),
                    Zone::Band { band, item } => (item, bands[band].items.len()),
                    Zone::Grid | Zone::State => (0, 1),
                };
                let to = match dir {
                    MenuDir::Left => i.checked_sub(1),
                    _ => Some(i + 1).filter(|&j| j < len),
                };
                let Some(to) = to else {
                    return Some(Some(MenuPulse::Boundary));
                };
                self.zone = match self.zone {
                    Zone::Band { band, .. } => Zone::Band { band, item: to },
                    Zone::Bar(_) => Zone::Bar(to),
                    _ => Zone::Chip(to),
                };
                self.seated = true;
                Some(Some(MenuPulse::Move))
            }
            MenuEvent::Confirm => Some(match self.zone {
                Zone::Band { band, item } => {
                    let intent = match &bands[band].items[item] {
                        Item::Desktop(h) if self.own(h) => self.desktop_intent(),
                        Item::Desktop(h) => ConnectIntent::to_host(h, None),
                        Item::Game(i) => self.launch_intent(&self.games[*i]),
                        Item::Collection(c) => {
                            self.open_collection(*c, fx);
                            return Some(Some(MenuPulse::Confirm));
                        }
                    };
                    fx.connect = Some(intent);
                    Some(MenuPulse::Confirm)
                }
                Zone::Bar(i) => self.apply_pill(i, ctx, fx),
                Zone::State => self.state_confirm(fx),
                _ => self.chip_confirm(ctx, fx),
            }),
            MenuEvent::Secondary => {
                match self.zone {
                    Zone::Band { band, item } => match &bands[band].items[item] {
                        Item::Desktop(h) => fx.options(CardMenu::for_host(h)),
                        Item::Game(i) => {
                            let g = &self.games[*i];
                            let cover = self.art.get(&g.id).cloned();
                            fx.options(CardMenu::for_game(&self.host, g, cover));
                        }
                        Item::Collection(_) => return Some(Some(MenuPulse::Boundary)),
                    },
                    Zone::Chip(0) => fx.options(CardMenu::for_host(self.live_host(ctx.hosts))),
                    _ => return Some(Some(MenuPulse::Boundary)),
                }
                Some(Some(MenuPulse::Confirm))
            }
            MenuEvent::Back => {
                fx.pop();
                Some(None)
            }
            MenuEvent::Tertiary
            | MenuEvent::JumpBack
            | MenuEvent::JumpForward
            | MenuEvent::Sector(_) => Some(None),
        }
    }

    /// Collection `c`'s shelf: this list filtered, so nothing is fetched, with the covers
    /// already decoded here.
    fn open_collection(&self, c: usize, fx: &mut Outbox) {
        let c = &self.collections[c];
        let mut shelf = LibraryScreen::new(&self.host);
        shelf.set_filter(c.key.clone(), c.label.clone());
        shelf.adopt_art(self.art.clone());
        fx.push(Screen::Library(shelf));
    }

    /// Pill `i`'s sort or arrangement, written to the setting; the screen adopts it next.
    /// Search opens its own screen, handing over the covers this shelf already decoded.
    fn apply_pill(&mut self, i: usize, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        match Pill::all(true)[i] {
            Pill::Search => {
                let search = super::super::search::SearchScreen::new(&self.host, &self.art);
                fx.push(crate::screens::Screen::Search(search));
                Some(MenuPulse::Confirm)
            }
            Pill::Sort(s) if s == self.sort => Some(MenuPulse::Boundary),
            Pill::View(v) if v == self.view_mode => Some(MenuPulse::Boundary),
            Pill::Sort(s) => {
                store_sort(s, ctx);
                Some(MenuPulse::Confirm)
            }
            Pill::View(v) => {
                store_view(v, ctx);
                Some(MenuPulse::Confirm)
            }
        }
    }

    /// The sort's pill and the arrangement's, in row order.
    pub(super) fn applied(&self) -> [usize; 2] {
        let all = Pill::all(true);
        let at = |p: Pill| all.iter().position(|&x| x == p).unwrap_or(0);
        [at(Pill::Sort(self.sort)), at(Pill::View(self.view_mode))]
    }

    /// The host pill opens Host and profile, where the shell swaps this shelf for the host
    /// picked there ([`Outbox::shelf`]); the other chip opens Customize.
    fn chip_confirm(&mut self, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.zone != Zone::Chip(0) {
            fx.push(Screen::Customize(CustomizeScreen::new()));
            return Some(MenuPulse::Confirm);
        }
        let host = self.live_host(ctx.hosts);
        let screen = ProfilesScreen::games(host, ctx.hosts, ctx.profiles).opened(host, fx);
        fx.push(screen);
        Some(MenuPulse::Confirm)
    }

    /// The host pill's words and face.
    fn pill(&self, hosts: &[HostRow], book: &ProfileBook) -> (String, PillFace) {
        let host = self.live_host(hosts);
        let face = book.choice(host).then(|| host.remembered().cloned());
        let label = match face.as_ref().and_then(Option::as_ref) {
            Some(p) => format!("{} \u{b7} {}", host.name, p.display_name),
            None => host.name.clone(),
        };
        (label, face)
    }

    /// Focus `line`, on what was drawn nearest the old focus's centre; by index when
    /// nothing there was drawn. The shelf keeps its own cursor.
    fn enter(&mut self, line: Line, down: bool, bands: &[Band], chips: usize) {
        let x = self.focus_x();
        let index = match self.zone {
            Zone::Chip(i) | Zone::Bar(i) | Zone::Band { item: i, .. } => i,
            Zone::Grid => self.grid_col,
            Zone::State => 0,
        };
        let nearest = |drawn: Vec<(usize, Rect)>| -> Option<usize> {
            let x = x?;
            (drawn.into_iter().filter(|(_, r)| !r.is_empty()))
                .min_by(|a, b| {
                    let (da, db) = ((a.1.center_x() - x).abs(), (b.1.center_x() - x).abs());
                    da.total_cmp(&db)
                })
                .map(|(i, _)| i)
        };
        let drawn = |want: Line| -> Vec<(usize, Rect)> {
            (self.hits.iter())
                .filter(|(z, _)| line_of(*z) == want)
                .map(|&(z, r)| match z {
                    Zone::Chip(i) | Zone::Bar(i) | Zone::Band { item: i, .. } => (i, r),
                    Zone::Grid | Zone::State => (0, r),
                })
                .collect()
        };
        let pick = |len: usize| nearest(drawn(line)).unwrap_or(index).min(len - 1);
        self.zone = match line {
            Line::Chips => Zone::Chip(pick(chips)),
            Line::Bar => Zone::Bar(pick(Pill::all(true).len())),
            Line::Band(band) => Zone::Band {
                band,
                item: pick(bands[band].items.len()),
            },
            Line::State => Zone::State,
            Line::Grid => {
                if let Some(shape) = self
                    .grid_shape()
                    .filter(|_| self.view_mode == LibraryView::Grid)
                {
                    let row = if down { 0 } else { shape.rows() - 1 };
                    let start = shape.row_start(row);
                    let cells = (start..start + shape.row_len(row))
                        .map(|i| (i, self.geom.get(i).copied().unwrap_or_else(Rect::new_empty)))
                        .collect();
                    let col = index.min(shape.row_len(row) - 1);
                    self.cursor = nearest(cells).unwrap_or(start + col) as i32;
                    self.seat_grid_col();
                }
                self.follow = true;
                Zone::Grid
            }
        };
    }

    /// Drawn x-centre of what has focus, if it was drawn last frame.
    fn focus_x(&self) -> Option<f32> {
        let r = match self.zone {
            Zone::Grid => *self.geom.get(self.cursor.max(0) as usize)?,
            z => self.hits.iter().find(|(h, _)| *h == z)?.1,
        };
        (!r.is_empty()).then(|| r.center_x())
    }

    /// A pointer over a pill, a chip, a band item or the state's button: hover focuses it,
    /// a press on the focused one is OK. A pill or a button acts on the first press. `None`
    /// when it is over none of them.
    pub(super) fn zone_pointer(&mut self, p: Pointer, press: bool) -> Option<bool> {
        let z = self.hits.iter().rev().find(|(_, r)| p.hits(*r))?.0;
        let direct = press && matches!(z, Zone::Bar(_) | Zone::State);
        if z == self.zone || direct {
            self.zone = z;
            self.seated = true;
            return Some(press);
        }
        self.zone = z;
        self.seated = true;
        Some(false)
    }

    /// What the focused line item is called, for the title band; `None` on the field and
    /// the pills.
    pub(super) fn zone_title(&self, ctx: &Ctx) -> Option<String> {
        match self.zone {
            Zone::Grid | Zone::Bar(_) | Zone::State => None,
            Zone::Chip(0) => Some(self.pill(ctx.hosts, ctx.profiles).0),
            Zone::Chip(_) => Some(pf_l10n::client::lib_customize().into()),
            Zone::Band { band, item } => {
                let (bands, _) = self.bands(ctx);
                Some(match bands.get(band)?.items.get(item)? {
                    Item::Desktop(h) if h.running.is_empty() => {
                        format!("{} \u{b7} {}", h.name, pf_l10n::client::lib_desktop())
                    }
                    Item::Desktop(h) => format!(
                        "{} \u{b7} {}",
                        h.name,
                        pf_l10n::client::lib_resume_title(&h.running)
                    ),
                    Item::Game(i) => self.games[*i].title.clone(),
                    Item::Collection(c) => self.collections[*c].label.clone(),
                })
            }
        }
    }

    /// The band item's title under focus, for the provenance line.
    pub(super) fn zone_game(&self, ctx: &Ctx) -> Option<&LibraryGame> {
        let Zone::Band { band, item } = self.zone else {
            return None;
        };
        match self.bands(ctx).0.get(band)?.items.get(item)? {
            Item::Game(i) => self.games.get(*i),
            Item::Desktop(_) | Item::Collection(_) => None,
        }
    }

    /// The legend off the field; `None` on it.
    pub(super) fn zone_hints(&self, ctx: &Ctx) -> Option<Vec<Hint>> {
        use pf_l10n::{client as m, common as c};
        let (bands, _) = self.bands(ctx);
        let ok = match self.zone {
            Zone::Grid => return None,
            Zone::Bar(_) => c::common_select(),
            Zone::State => self.state_action()?.label(),
            Zone::Chip(0) => c::common_change(),
            Zone::Chip(_) => m::lib_customize(),
            Zone::Band { band, item } => match bands.get(band)?.items.get(item)? {
                Item::Desktop(h) if h.running.is_empty() => m::lib_desktop_action(),
                Item::Desktop(_) => m::lib_resume(),
                Item::Game(i) if self.games[*i].running => m::lib_resume(),
                Item::Game(i) if self.games[*i].launcher => c::common_open(),
                Item::Game(_) => c::common_play(),
                Item::Collection(_) => c::common_open(),
            },
        };
        let mut hints = vec![Hint::new(HintKey::Confirm, ok)];
        let options = match self.zone {
            Zone::Band { band, item } => {
                !matches!(bands[band].items.get(item), Some(Item::Collection(_)))
            }
            z => z == Zone::Chip(0),
        };
        if options {
            hints.push(Hint::new(
                HintKey::Secondary,
                pf_l10n::console::console_lib_options(),
            ));
        }
        hints.push(Hint::new(HintKey::Back, c::common_back()));
        Some(hints)
    }

    /// Scroll height of the chips line and of band `band`, at grid cell height `ch`.
    pub(super) fn chips_h(k: f64) -> f64 {
        (TOP_AIR + CHIP_H + BAND_AIR) * k
    }

    pub(super) fn band_h(band: &Band, ch: f64, k: f64) -> f64 {
        (HEADING_H + BAND_AIR) * k + row_h(band, ch, k)
    }

    /// The chips line, as a child of the screen's scroll: the host pill, then Customize, in
    /// a row that scrolls sideways. `draw_field` keeps the focused chip in view.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn chips_el<'a>(
        &'a self,
        ui: &'a Frame,
        hosts: &'a [HostRow],
        book: &ProfileBook,
        fonts: &'a Fonts,
        width: f64,
        k: f64,
        hits: &'a RefCell<Vec<(Zone, Rect)>>,
    ) -> El<'a> {
        let (label, face) = self.pill(hosts, book);
        let face_w = face.as_ref().map_or(0.0, |_| (2.0 * PILL_FACE + 8.0) * k);
        let text_w = f64::from(fonts.measure(&label, W::SemiBold, PILL_TEXT * k));
        let pill_w = 2.0 * PILL_PAD * k + face_w + text_w + (8.0 + 14.0) * k;
        let (top, h) = (TOP_AIR * k, CHIP_H * k);
        // Room round the line for the plate's outset: a scroll clips what it draws, and a
        // plate cut at a chip's edge reads as a second chip.
        let air = (super::PLATE_AIR * k) as f32;
        let place =
            |x: f64, w: f64| Rect::from_xywh(air + x as f32, air + top as f32, w as f32, h as f32);
        let pill = El::paint(move |canvas, r| {
            hits.borrow_mut().push((Zone::Chip(0), r));
            draw_host_pill(canvas, ui, fonts, &label, face.as_ref(), r, k);
        })
        .id(chip_id(0))
        .focusable((h / 2.0) as f32)
        .place(place(0.0, pill_w));
        let custom = pf_l10n::client::lib_customize();
        let custom_w = button_w(fonts, custom, k);
        let mut x = pill_w + 10.0 * k;
        let customize = El::paint(move |canvas, r| {
            hits.borrow_mut().push((Zone::Chip(1), r));
            button(canvas, ui, fonts, custom, r, k);
        })
        .id(chip_id(1))
        .focusable((h / 2.0) as f32)
        .place(place(x, custom_w));
        x += custom_w + 10.0 * k;
        let row = El::column().child(pill).child(customize);
        let line_h = Self::chips_h(k) as f32;
        let span = (x - 10.0 * k).max(width) as f32 + 2.0 * air;
        El::column().size(width as f32, line_h).child(
            El::scroll(chips_id(), Axis::Horizontal)
                .child(row.size(span, line_h + 2.0 * air))
                .place(Rect::from_xywh(
                    -air,
                    -air,
                    width as f32 + 2.0 * air,
                    line_h + 2.0 * air,
                )),
        )
    }

    /// Band `b`, as a child of the screen's scroll: its heading, then its row as a sideways
    /// scroll that `draw_field` follows and a finger swipes. Items out of view are not drawn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn band_el<'a>(
        &'a self,
        ui: &'a Frame,
        band: &'a Band,
        b: usize,
        fonts: &'a Fonts,
        width: f64,
        (cw, ch): (f64, f64),
        k: f64,
        hits: &'a RefCell<Vec<(Zone, Rect)>>,
    ) -> El<'a> {
        let label = band.section.label();
        let heading = El::paint(move |canvas, r| {
            let base = f64::from(r.top) + HEADING_H * 0.62 * k;
            card::heading(canvas, ui, fonts, label, f64::from(r.left), base, k);
        })
        .place(Rect::from_xywh(
            0.0,
            0.0,
            width as f32,
            (HEADING_H * k) as f32,
        ));
        let (iw, pitch) = item_pitch(band, cw, k);
        let corner = match band.section {
            Section::Desktops => 14.0,
            Section::Collections => TILE_CORNER,
            _ => card::COVER_CORNER,
        };
        let rh = row_h(band, ch, k) as f32;
        // Room round the row for the plate's outset, and under it for the plate's shadow: a
        // scroll clips what it draws, and a plate cut at a tile's edge reads as a second tile.
        let air = (super::PLATE_AIR * k) as f32;
        let reach = (PLATE_REACH * k) as f32;
        let mut row = El::column();
        for (i, it) in band.items.iter().enumerate() {
            let slot = Rect::from_xywh(air + (i as f64 * pitch) as f32, air, iw as f32, rh);
            let z = Zone::Band { band: b, item: i };
            row = row.child(
                El::paint(move |canvas, slot| {
                    hits.borrow_mut().push((z, slot));
                    match it {
                        Item::Desktop(h) => card::desk_tile(canvas, ui, fonts, h, slot, k),
                        Item::Game(g) => {
                            self.band_card(canvas, ui, fonts, band.section, *g, slot, ch, k, z)
                        }
                        Item::Collection(c) => {
                            let c = &self.collections[*c];
                            paint_tile(canvas, ui, fonts, c, &self.games, &self.art, slot, k)
                        }
                    }
                })
                .id(band_item_id(b, i))
                .focusable((corner * k) as f32)
                .place(slot),
            );
        }
        let span = (band.items.len() as f64 * pitch - (pitch - iw)).max(width) as f32 + 2.0 * air;
        El::column()
            .size(width as f32, Self::band_h(band, ch, k) as f32)
            .child(heading)
            .child(
                El::scroll(band_id(b), Axis::Horizontal)
                    .child(row.size(span, rh + air + reach))
                    .place(Rect::from_xywh(
                        -air,
                        (HEADING_H * k) as f32 - air,
                        width as f32 + 2.0 * air,
                        rh + air + reach,
                    )),
            )
    }

    /// A card in a band; Recently played captions it with when it was played.
    #[allow(clippy::too_many_arguments)]
    fn band_card(
        &self,
        canvas: &Canvas,
        ui: &Frame,
        fonts: &Fonts,
        section: Section,
        i: usize,
        slot: Rect,
        ch: f64,
        k: f64,
        z: Zone,
    ) {
        let g = &self.games[i];
        let played = g.stats.as_ref().map_or(0, |s| s.last_played_unix_ms);
        let caption = (section == Section::Recent && played > 0).then(|| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            crate::library::ago(now.saturating_sub(played))
        });
        let card = Card {
            game: g,
            art: self.art.get(&g.id),
            title: &g.title,
            host: &self.host,
            caption: caption.as_deref(),
            focused: self.zone == z && !self.quiet,
        };
        card.paint(canvas, ui, fonts, slot, ch, k, 1.0);
    }
}

/// The host pill: who plays (where that is a choice), the label, then a chevron that says
/// it opens a choice.
fn draw_host_pill(
    canvas: &Canvas,
    ui: &Frame,
    fonts: &Fonts,
    label: &str,
    face: Option<&Option<ProfilePick>>,
    r: Rect,
    k: f64,
) {
    let corner = (f64::from(r.height()) / 2.0 / k) as f32;
    crate::theme::panel(
        canvas,
        ui,
        r,
        corner,
        None,
        PanelStroke::Plain(0.08),
        k as f32,
    );
    let cy = f64::from(r.center_y());
    let mut x = f64::from(r.left) + PILL_PAD * k;
    if let Some(pick) = face {
        let rad = PILL_FACE * k;
        let cx = (x + rad) as f32;
        super::super::profiles::draw_pick(canvas, ui, fonts, pick.as_ref(), cx, cy as f32, rad);
        x += (2.0 * PILL_FACE + 8.0) * k;
    }
    let size = PILL_TEXT * k;
    fonts.draw(
        canvas,
        label,
        x,
        cy + size * 0.36,
        W::SemiBold,
        size,
        ui.fg(0.95),
    );
    let chevron = Icon(pf_client_core::lucide::CHEVRON_DOWN);
    let cx = (f64::from(r.right) - (PILL_PAD + 7.0) * k) as f32;
    icons::draw_icon(
        canvas,
        chevron,
        cx,
        cy as f32,
        (14.0 * k) as f32,
        k as f32,
        ui.fg(0.7),
    );
}

/// A band's row height at grid poster height `ch`. Recently played holds a caption line.
fn row_h(band: &Band, ch: f64, k: f64) -> f64 {
    match band.section {
        Section::Desktops => DESK_H * k,
        Section::Collections => TILE_H * k,
        s => ch + card::text_h(s == Section::Recent) * k,
    }
}

/// An item's width and the distance to the next, at grid cell width `cw`.
fn item_pitch(band: &Band, cw: f64, k: f64) -> (f64, f64) {
    let w = match band.section {
        Section::Desktops => DESK_W * k,
        Section::Collections => TILE_W * k,
        _ => cw,
    };
    (w, w + GRID_GAP * k)
}

/// Customize: the sections' order and switches, stored as `library_sections`. OK picks a
/// row up, Up and Down carry it, OK or Back sets it down; Left hides a section, Right
/// shows it. A pointer press flips the switch.
pub(crate) struct CustomizeScreen {
    list: MenuList,
    held: bool,
}

impl CustomizeScreen {
    pub(crate) fn new() -> CustomizeScreen {
        CustomizeScreen {
            list: MenuList::new(),
            held: false,
        }
    }

    fn save(rows: &[(Section, bool)], ctx: &mut Ctx) {
        ctx.write(|c| {
            c.settings.library_sections = crate::library::stored_sections(rows);
            true
        });
    }

    fn set(&mut self, rows: &mut [(Section, bool)], on: bool, ctx: &mut Ctx) -> Option<MenuPulse> {
        let i = self.list.cursor.min(rows.len() - 1);
        if rows[i].1 == on {
            return Some(MenuPulse::Boundary);
        }
        rows[i].1 = on;
        Self::save(rows, ctx);
        Some(MenuPulse::Move)
    }
}

impl ScreenView for CustomizeScreen {
    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let mut rows = crate::library::sections(&ctx.settings.library_sections);
        let i = self.list.cursor.min(rows.len() - 1);
        match ev {
            MenuEvent::Move(dir @ (MenuDir::Up | MenuDir::Down)) if self.held => {
                let to = if dir == MenuDir::Up {
                    i.checked_sub(1)
                } else {
                    Some(i + 1).filter(|&j| j < rows.len())
                };
                let Some(to) = to else {
                    return Some(MenuPulse::Boundary);
                };
                rows.swap(i, to);
                Self::save(&rows, ctx);
                self.list.cursor = to;
                return Some(MenuPulse::Move);
            }
            MenuEvent::Confirm | MenuEvent::Back if self.held => {
                self.held = false;
                return Some(MenuPulse::Confirm);
            }
            MenuEvent::Back => {
                fx.pop();
                return None;
            }
            _ => {}
        }
        let (msg, pulse) = self.list.menu(ev, rows.len());
        match msg {
            ListMsg::Activate => {
                self.held = true;
                pulse
            }
            ListMsg::Adjust(d) => self.set(&mut rows, d > 0, ctx),
            ListMsg::None => pulse,
        }
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
        let mut rows = crate::library::sections(&ctx.settings.library_sections);
        let (msg, pulse) = self.list.pointer(p, rows.len());
        match msg {
            ListMsg::Activate => {
                let on = !rows[self.list.cursor.min(rows.len() - 1)].1;
                self.set(&mut rows, on, ctx);
                true
            }
            ListMsg::Adjust(_) => true,
            ListMsg::None => pulse.is_some(),
        }
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        use pf_l10n::console as m;
        if self.held {
            return vec![
                Hint::new(HintKey::Confirm, m::console_lib_sections_set_down()),
                Hint::new(HintKey::Back, m::console_lib_sections_set_down()),
            ];
        }
        vec![
            Hint::new(HintKey::Confirm, m::console_lib_sections_move()),
            Hint::new(HintKey::Adjust, m::console_lib_sections_toggle()),
            Hint::new(HintKey::Back, pf_l10n::common::common_done()),
        ]
    }

    /// A spoken list: the section, whether it shows, and whether it is held.
    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        use pf_l10n::console as m;
        let rows = crate::library::sections(&ctx.settings.library_sections);
        let (s, on) = rows.get(self.list.cursor)?;
        let state = match on {
            true => m::console_lib_section_shown_a11y(),
            false => m::console_lib_section_hidden_a11y(),
        };
        Some(match self.held {
            true => format!(
                "{}, {state}, {}",
                s.label(),
                m::console_lib_section_moving_a11y()
            ),
            false => format!("{}, {state}", s.label()),
        })
    }

    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        let ui = ctx.ui;
        let foot = (crate::widgets::FOOT_DETAIL_H * k) as f32;
        let list = Rect::from_ltrb(rect.left, rect.top, rect.right, rect.bottom - foot);
        let rows: Vec<RowSpec> = crate::library::sections(&ctx.settings.library_sections)
            .iter()
            .enumerate()
            .map(|(i, (s, on))| {
                RowSpec::toggle(s.label(), *on).with_handle(self.held && i == self.list.cursor)
            })
            .collect();
        self.list
            .render(canvas, ui, list, &rows, fonts, k, dt, true);
    }

    fn foot(&self, _ctx: &Ctx) -> Option<std::borrow::Cow<'static, str>> {
        Some(pf_l10n::console::console_lib_sections_foot().into())
    }

    fn title(&self) -> String {
        pf_l10n::client::player_library_sections().into()
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OK picks a row up, Down carries it, OK sets it down; Left hides the section.
    /// Each step lands in `library_sections` in the Mac's format.
    #[test]
    fn customize_reorders_and_hides_with_the_remotes_keys() {
        crate::screens::settings::tests::fake_home();
        let library = crate::library::LibraryShared::default();
        let mut settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let mut s = CustomizeScreen::new();
        let mut fx = Outbox::default();
        let mut press = |s: &mut CustomizeScreen, ev| s.menu(ev, &mut ctx, &mut fx);
        press(&mut s, MenuEvent::Confirm);
        press(&mut s, MenuEvent::Move(MenuDir::Down));
        press(&mut s, MenuEvent::Confirm);
        press(&mut s, MenuEvent::Move(MenuDir::Left));
        let pulse = press(&mut s, MenuEvent::Move(MenuDir::Left));
        assert!(matches!(pulse, Some(MenuPulse::Boundary)), "already hidden");
        assert_eq!(
            ctx.settings.library_sections,
            "recent,-desktops,favorites,launchers,collections,games"
        );
    }

    /// Sort, view, and Customize rebase before they save: a value another writer stored
    /// after the console's last load survives each of them.
    #[test]
    fn library_writes_keep_another_writers_save() {
        let library = crate::library::LibraryShared::default();
        let store = crate::store::file_store();
        let mut settings = pf_client_core::trust::Settings::default();
        let mut ctx = Ctx {
            store,
            ..Ctx::test(&mut settings, &library)
        };
        let writes: [fn(&mut Ctx); 3] = [
            |c| store_sort(crate::collate::SortKey::Title, c),
            |c| store_view(LibraryView::Shelf, c),
            |c| {
                let mut s = CustomizeScreen::new();
                s.menu(MenuEvent::Move(MenuDir::Left), c, &mut Outbox::default());
            },
        ];
        for (i, write) in writes.into_iter().enumerate() {
            let mut other = store.load();
            other.bitrate_kbps = 7_000 + i as u32;
            store.save(&other);
            write(&mut ctx);
            assert_eq!(store.load().bitrate_kbps, 7_000 + i as u32, "write {i}");
        }
        let saved = store.load();
        assert_eq!(saved.library_sort, "title");
        assert_eq!(saved.library_view, "shelf");
        assert!(saved.library_sections.contains("-desktops"));
    }
}
