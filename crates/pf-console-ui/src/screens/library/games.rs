//! The Games tab: the Apple library's rows over the shelf's field, and Customize.
//!
//! Focus walks lines top to bottom: the sort/view pills, the host chips and a Customize
//! chip, the enabled sections in `library_sections` order, and the Games field (grid or
//! shelf) where Games sits among them. An empty section hides. Desktops and Launchers
//! leave the field while their rows show. A list that failed, is empty or still loading
//! keeps the chips and the Desktops row, with the state's action where the field was.
//! A collection's shelf has only the pills and its field.

use super::super::collections::{paint_tile, TILE_CORNER, TILE_H, TILE_W};
use super::bar::{pill_id, Pill};
use super::card::{self, Card, DESK_H, DESK_W};
use super::{desk_intent, store_sort, store_view, LibraryScreen};
use crate::el::{El, Id};
use crate::glyphs::{Hint, HintKey};
use crate::library::{LibraryGame, LibraryPhase, LibraryView, Section, DESKTOP_ID, GRID_GAP};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::card_menu::CardMenu;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{button, button_w, text_tab, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};
use std::cell::RefCell;

/// Recently played shows at most this many.
const RECENT_MAX: usize = 12;
// Design units. A band is its heading, its row, then air.
const HEADING_H: f64 = 30.0;
const BAND_AIR: f64 = 22.0;
const CHIP_H: f64 = 38.0;
/// A host chip's label size and side padding, as a section tab's.
const TAB_TEXT: f64 = 16.0;
const TAB_PAD: f64 = 10.0;
const TOP_AIR: f64 = 4.0;

/// Where the D-pad is. The field keeps its own cursor under [`Zone::Grid`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Zone {
    /// The field: the grid or the shelf.
    Grid,
    /// Pill `i` of the sort/view row.
    Bar(usize),
    /// A host chip; the one past the last is Customize.
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

pub(super) fn band_item_id(band: usize, item: usize) -> Id {
    Id::new("games-band", band * 10_000 + item)
}

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

/// The hosts a chip opens: each paired host's own shelf.
fn chip_hosts(hosts: &[HostRow]) -> impl Iterator<Item = &HostRow> {
    hosts
        .iter()
        .filter(|h| h.paired && h.saved && h.pin.is_none())
}

impl LibraryScreen {
    /// The shelf lays out as the Games tab: not a collection or a search, not under the
    /// Hosts row.
    pub(super) fn sectioned(&self) -> bool {
        !self.drilled && !self.embedded
    }

    /// `h` is this shelf's host; a pinned card's shelf counts its primary row.
    fn own(&self, h: &HostRow) -> bool {
        Some(h.key.as_str()) == self.host.key.split('\0').next()
    }

    pub(super) fn shows(&self, s: Section) -> bool {
        self.sections.iter().any(|&(x, on)| x == s && on)
    }

    /// A band shows this title, so the field does not. Under the Hosts row the card above
    /// is the desk, and launchers follow the Games tab.
    pub(super) fn banded(&self, g: &LibraryGame) -> bool {
        if self.embedded {
            return g.id == DESKTOP_ID || (g.launcher && self.shows(Section::Launchers));
        }
        self.sectioned()
            && (!self.shows(Section::Games)
                || (g.id == DESKTOP_ID && self.shows(Section::Desktops))
                || (g.launcher && self.shows(Section::Launchers)))
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
                    let mut hosts: Vec<&HostRow> = chip_hosts(ctx.hosts).collect();
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
        let chips = chip_hosts(ctx.hosts).count() + 1;
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
        let chips = chip_hosts(ctx.hosts).count() + 1;
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
                        Item::Desktop(h) => desk_intent(h),
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
                    Zone::Chip(_) => match self.chip_host(ctx) {
                        Some(h) => fx.options(CardMenu::for_host(h)),
                        None => return Some(Some(MenuPulse::Boundary)),
                    },
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

    fn chip_host<'h>(&self, ctx: &Ctx<'h>) -> Option<&'h HostRow> {
        let Zone::Chip(i) = self.zone else {
            return None;
        };
        chip_hosts(ctx.hosts).nth(i)
    }

    /// A host chip swaps this shelf for that host's; the last chip opens Customize.
    fn chip_confirm(&mut self, ctx: &Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let Some(h) = self.chip_host(ctx) else {
            fx.push(Screen::Customize(CustomizeScreen::new()));
            return Some(MenuPulse::Confirm);
        };
        if self.own(h) {
            return Some(MenuPulse::Boundary);
        }
        let mut shelf = LibraryScreen::new(h);
        shelf.zone = self.zone;
        shelf.seated = true;
        fx.cmds.push(ConsoleCmd::FetchLibrary {
            addr: h.addr.clone(),
            mgmt: h.mgmt_port,
            fp_hex: h.fp_hex.clone(),
        });
        fx.replace(Screen::Library(shelf));
        Some(MenuPulse::Confirm)
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
            Zone::Chip(_) => Some(self.chip_host(ctx).map_or("Customize", |h| &h.name).into()),
            Zone::Band { band, item } => {
                let (bands, _) = self.bands(ctx);
                Some(match bands.get(band)?.items.get(item)? {
                    Item::Desktop(h) if h.running.is_empty() => {
                        format!("{} \u{b7} Desktop", h.name)
                    }
                    Item::Desktop(h) => format!("{} \u{b7} Resume {}", h.name, h.running),
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
        let (bands, _) = self.bands(ctx);
        let ok = match self.zone {
            Zone::Grid => return None,
            Zone::Bar(_) => "Select",
            Zone::State => self.state_action()?,
            Zone::Chip(_) if self.chip_host(ctx).is_none() => "Customize",
            Zone::Chip(_) => "Open",
            Zone::Band { band, item } => match bands.get(band)?.items.get(item)? {
                Item::Desktop(h) if h.running.is_empty() => "Stream",
                Item::Desktop(_) => "Resume",
                Item::Game(i) if self.games[*i].running => "Resume",
                Item::Game(i) if self.games[*i].launcher => "Open",
                Item::Game(_) => "Play",
                Item::Collection(_) => "Open",
            },
        };
        let mut hints = vec![Hint::new(HintKey::Confirm, ok)];
        let options = match self.zone {
            Zone::Band { band, item } => {
                !matches!(bands[band].items.get(item), Some(Item::Collection(_)))
            }
            _ => self.chip_host(ctx).is_some(),
        };
        if options {
            hints.push(Hint::new(HintKey::Secondary, "Options"));
        }
        hints.push(Hint::new(HintKey::Back, "Back"));
        Some(hints)
    }

    /// Scroll height of the chips line and of band `band`, at grid cell height `ch`.
    pub(super) fn chips_h(k: f64) -> f64 {
        (TOP_AIR + CHIP_H + BAND_AIR) * k
    }

    pub(super) fn band_h(band: &Band, ch: f64, k: f64) -> f64 {
        (HEADING_H + BAND_AIR) * k + row_h(band, ch, k)
    }

    /// Chase each band's scroll toward its focused item, centred, clamped to its ends.
    pub(super) fn step_bands(&mut self, bands: &[Band], width: f64, cw: f64, k: f64, snap: bool) {
        self.band_x
            .resize(bands.len(), crate::anim::Spring::rest(0.0));
        let Zone::Band { band, item } = self.zone else {
            return;
        };
        let Some(b) = bands.get(band) else { return };
        let (iw, pitch) = item_pitch(b, cw, k);
        let span = pitch * b.items.len() as f64 - (pitch - iw);
        let want =
            (item as f64 * pitch + iw / 2.0 - width / 2.0).clamp(0.0, (span - width).max(0.0));
        let s = &mut self.band_x[band];
        if snap || crate::theme::reduce_motion() {
            *s = crate::anim::Spring::rest(want);
        } else {
            s.step_spec(want, crate::anim::springs::FOCUS, 1.0 / 60.0);
            s.settle(want, 0.05, 0.5);
        }
    }

    /// The chips line, as a child of the screen's scroll.
    pub(super) fn chips_el<'a>(
        &'a self,
        hosts: &'a [HostRow],
        fonts: &'a Fonts,
        width: f64,
        k: f64,
        hits: &'a RefCell<Vec<(Zone, Rect)>>,
    ) -> El<'a> {
        let names = chip_hosts(hosts).map(|h| (h.name.as_str(), Some(self.own(h))));
        let (mut x, top, h) = (0.0, TOP_AIR * k, CHIP_H * k);
        let mut row = El::column().size(width as f32, Self::chips_h(k) as f32);
        // One line, no scroll: past about six hosts the last chips run off the edge.
        for (i, (label, mine)) in names.chain([("Customize", None)]).enumerate() {
            let w = match mine {
                Some(_) => {
                    f64::from(fonts.measure(label, W::Bold, TAB_TEXT * k)) + 2.0 * TAB_PAD * k
                }
                None => button_w(fonts, label, k),
            };
            let chip = Rect::from_xywh(x as f32, top as f32, w as f32, h as f32);
            row = row.child(
                El::paint(move |canvas, r| {
                    hits.borrow_mut().push((Zone::Chip(i), r));
                    match mine {
                        Some(mine) => {
                            let ink = fg(if mine { 1.0 } else { 0.6 });
                            text_tab(canvas, fonts, label, r, TAB_TEXT * k, ink);
                        }
                        None => button(canvas, fonts, label, r, k),
                    }
                })
                .id(chip_id(i))
                .focusable((h / 2.0) as f32)
                .place(chip),
            );
            x += w + 10.0 * k;
        }
        row
    }

    /// Band `b`, as a child of the screen's scroll: its heading, then its row at its own
    /// horizontal scroll. Items past `view`'s sides are not drawn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn band_el<'a>(
        &'a self,
        band: &'a Band,
        b: usize,
        fonts: &'a Fonts,
        width: f64,
        (cw, ch): (f64, f64),
        view: Rect,
        k: f64,
        hits: &'a RefCell<Vec<(Zone, Rect)>>,
    ) -> El<'a> {
        let label = band.section.label();
        let heading = El::paint(move |canvas, r| {
            let base = f64::from(r.top) + HEADING_H * 0.62 * k;
            card::heading(canvas, fonts, label, f64::from(r.left), base, k);
        })
        .place(Rect::from_xywh(
            0.0,
            0.0,
            width as f32,
            (HEADING_H * k) as f32,
        ));
        let (iw, pitch) = item_pitch(band, cw, k);
        let off = self.band_x.get(b).map_or(0.0, |s| s.pos);
        let corner = match band.section {
            Section::Desktops => 14.0,
            Section::Collections => TILE_CORNER,
            _ => card::COVER_CORNER,
        };
        let mut el = El::column()
            .size(width as f32, Self::band_h(band, ch, k) as f32)
            .child(heading);
        for (i, it) in band.items.iter().enumerate() {
            let x = i as f64 * pitch - off;
            let slot = Rect::from_xywh(
                x as f32,
                (HEADING_H * k) as f32,
                iw as f32,
                row_h(band, ch, k) as f32,
            );
            let z = Zone::Band { band: b, item: i };
            el = el.child(
                El::paint(move |canvas, slot| {
                    // Past the viewport's sides the band draws nothing and takes no press.
                    if slot.right < view.left || slot.left > view.right {
                        return;
                    }
                    hits.borrow_mut().push((z, slot));
                    match it {
                        Item::Desktop(h) => card::desk_tile(canvas, fonts, h, slot, k),
                        Item::Game(g) => {
                            self.band_card(canvas, fonts, band.section, *g, slot, ch, k, z)
                        }
                        Item::Collection(c) => {
                            let c = &self.collections[*c];
                            paint_tile(canvas, fonts, c, &self.games, &self.art, slot, k)
                        }
                    }
                })
                .id(band_item_id(b, i))
                .focusable((corner * k) as f32)
                .place(slot),
            );
        }
        el
    }

    /// A card in a band; Recently played captions it with when it was played.
    #[allow(clippy::too_many_arguments)]
    fn band_card(
        &self,
        canvas: &Canvas,
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
        card.paint(canvas, fonts, slot, ch, k, 1.0);
    }
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
    pub(crate) list: MenuList,
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
        ctx.settings.library_sections = crate::library::stored_sections(rows);
        ctx.store.save(ctx.settings);
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
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

    fn set(&mut self, rows: &mut [(Section, bool)], on: bool, ctx: &mut Ctx) -> Option<MenuPulse> {
        let i = self.list.cursor.min(rows.len() - 1);
        if rows[i].1 == on {
            return Some(MenuPulse::Boundary);
        }
        rows[i].1 = on;
        Self::save(rows, ctx);
        Some(MenuPulse::Move)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, _fx: &mut Outbox) -> bool {
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

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        if self.held {
            return vec![
                Hint::new(HintKey::Confirm, "Set down"),
                Hint::new(HintKey::Back, "Set down"),
            ];
        }
        vec![
            Hint::new(HintKey::Confirm, "Move"),
            Hint::new(HintKey::Adjust, "Hide / Show"),
            Hint::new(HintKey::Back, "Done"),
        ]
    }

    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
        let rows = crate::library::sections(&ctx.settings.library_sections);
        let (s, on) = rows.get(self.list.cursor)?;
        let state = if *on { "shown" } else { "hidden" };
        Some(match self.held {
            true => format!("{}, {state}, moving", s.label()),
            false => format!("{}, {state}", s.label()),
        })
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
        let note_h = 34.0 * k;
        let list = Rect::from_ltrb(rect.left, rect.top, rect.right, rect.bottom - note_h as f32);
        let rows: Vec<RowSpec> = crate::library::sections(&ctx.settings.library_sections)
            .iter()
            .enumerate()
            .map(|(i, (s, on))| {
                RowSpec::toggle(s.label(), *on).with_handle(self.held && i == self.list.cursor)
            })
            .collect();
        self.list.render(canvas, list, &rows, fonts, k, dt, true);
        fonts.centered(
            canvas,
            "The Games tab shows these in this order. An empty section stays hidden.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.center_x()),
            f64::from(rect.bottom) - note_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
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
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &[],
            deck: false,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "test",
            t: 0.0,
        };
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
}
