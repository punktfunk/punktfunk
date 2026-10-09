//! One host's titles: the Games tab's rows over a grid or a coverflow shelf.
//!
//! One screen on the shell stack. B pops; A launches the focused title in this window.
//! The shell owns aurora, chrome and the connecting overlay. Every line lives in one
//! scroll and one focus tree ([`games`]): the sort/view pills ([`bar`]), the host chips,
//! the section rows, and the field, which is the grid or the shelf by `library_view`.
//! A collection's shelf has only the pills and its field.
//!
//! `host.pin` is load-bearing: a pinned card launches with that preset. Posters decode in
//! [`art`], so every screen keeps one cache size.
//! Entrance waits for neighbourhood art or 400 ms. Pin with this screen's tests.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::anim::{
    BUMP_C, BUMP_K, BUMP_V, ENTER_RISE, ENTER_SCALE, ENTER_TURN_DEG, SPRING_C, SPRING_K,
};
use crate::coverflow::{
    project, shelf_matrix, POSTER_H, RECEDE_FADE, RECEDE_SCALE, ROTATE_DEG, SHELF_CORNER,
    SHELF_COVER_MIN, SHELF_EYE, SHELF_SPACING,
};
use crate::el::{Axis, El, Id, Tree};
use crate::glyphs::{Hint, HintKey};
use crate::grid::{
    grid_col_hint, grid_step, step_cursor, GridDir, GridShape, StepResult, GRID_GAP, GRID_H,
    GRID_W, JUMP,
};
use crate::library::{store_label, LibraryGame, LibraryPhase, LibraryShared, LibraryView, Stale};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, ScreenView};
use crate::theme::{art_sampling, edge, fg, fill, stroke, Fonts, W};
use crate::widgets::{button, button_w, BUTTON_H};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Image, RRect, Rect, M44};
use std::cell::RefCell;
use std::collections::HashMap;

pub(crate) mod art;
pub(crate) mod bar;
mod card;
mod games;
use art::{
    art_to_evict, draw_poster_placeholder, share_cover, shared_cover, ArtBudget, ArtDecoder,
};
pub(crate) use games::CustomizeScreen;
use games::{Band, Line, Zone};

/// The screen's scroll node, title `i`'s grid cell, the shelf strip and its cover `i`.
const GRID: &str = "library-grid";
fn grid_cell(i: usize) -> Id {
    Id::new("library-cell", i)
}
fn shelf_strip() -> Id {
    Id::new("library-shelf", 0)
}
fn shelf_cover(i: usize) -> Id {
    Id::new("library-cover", i)
}
/// Air between grid rows: a card's text and the plate's outset.
const ROW_GAP: f64 = 22.0;
/// A heading over the grid: its line and the air to row 0.
const GRID_HEADING: f64 = 34.0;
/// Row 0's air under the Hosts row: the plate's outset and a breath.
const EMBED_AIR: f64 = 14.0;
/// New covers a screen adopts a frame. Each is a texture upload on its first draw, and a
/// shelf's worth landing together stalls the GPU for a frame.
const ADOPT_PER_FRAME: usize = 4;
/// A grid row down counts as this many entrance steps, 120 ms at [`entrances::GRID`], so
/// rows follow one another instead of each rippling at once.
const ROW_STEPS: usize = 3;
/// Room for the plate's outset past the first column.
const PLATE_AIR: f64 = 32.0;
/// Air under the sort/view row before the next line.
const BAR_AIR: f64 = 4.0;
/// The shelf's group heading, and the air round its covers (the Apple strip's 44).
const SHELF_HEAD: f64 = 26.0;
const SHELF_AIR: f64 = 44.0;
/// The state card's height among the tab's rows.
const STATE_H: f64 = 190.0;
/// Air a scroll keeps round the focused item, design units.
const REVEAL_AIR: f64 = 20.0;
/// Spinner for list-in-flight and the 400 ms art wait; nothing draws until entrance frame 1.
fn draw_loading(canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, t: f64) {
    let w = f64::from(rect.width());
    let cx = f64::from(rect.left) + w / 2.0;
    let cy = f64::from(rect.top) + f64::from(rect.height()) / 2.0;
    // Same arc as `shell::overlays::draw_takeover`. A frozen arc under reduced motion reads hung.
    fonts.centered(
        canvas,
        "Loading library…",
        W::Regular,
        14.0 * k,
        fg(0.55),
        cx,
        cy + 26.0 * k,
        w * 0.8,
    );
    crate::theme::spinner(canvas, cx, cy - 26.0 * k, 22.0 * k, t);
}

/// Write `library_sort` only. Screens re-read it each frame; assigning the field reverts.
pub(super) fn store_sort(sort: crate::collate::SortKey, ctx: &mut Ctx) {
    ctx.write(|c| {
        c.settings.library_sort = sort.id().to_string();
        true
    });
}

/// Write `library_view`. Settings and this bar share the key; last write wins next frame.
fn store_view(view: LibraryView, ctx: &mut Ctx) {
    ctx.write(|c| {
        c.settings.library_view = view.id().to_string();
        true
    });
}

pub(crate) struct LibraryScreen {
    /// Whole row: a collection's shelf is built from it. `pin` is the one-off preset.
    host: HostRow,
    shared: Option<LibraryShared>,
    // Snapshot of the shared model; re-pulled when `generation` bumps.
    generation: u64,
    phase: LibraryPhase,
    games: Vec<LibraryGame>,
    /// Disk-cache titles, not an error: a sleeping host still has a working library.
    stale: Stale,
    /// Display order into `games`. Cursor indexes this; art keys on model ids, not positions.
    view: Vec<usize>,
    sort: crate::collate::SortKey,
    /// One collated group. Index-level, so the art pump never learns a filter exists.
    filter: Option<crate::collate::GroupKey>,
    /// Held, not re-derived: `Store("Steam")` and `Platform("Steam")` both read "Steam".
    filter_label: Option<String>,
    /// A title search, lowercased: only titles containing it stay.
    query: Option<String>,
    /// A collection's or a search's shelf: the pills and the field, no rows.
    drilled: bool,
    /// The Collections row's tiles, collated with the view.
    collections: Vec<super::collections::Collection>,
    // Integer cursor is the authority; the eased position chases it.
    cursor: i32,
    /// Last drawn card rects (axis-aligned; tilt is inside finger slop). Empty if culled.
    geom: Vec<Rect>,
    anim: Spring,
    bump: Spring,
    view_mode: LibraryView,
    /// The sort/view pills' sliding capsules. Boxed: `Screen` moves by value and this
    /// variant is already the largest.
    bar: Box<bar::Bar>,
    /// The scroll while it follows focus; after a pan, where the finger left it.
    scroll: Spring,
    /// Seat scroll and capsules next frame: a new arrangement does not glide from the old.
    snap_scroll: bool,
    /// The grid's layout, scroll and hit rects. A cell: the card painters borrow the
    /// screen while the tree lays them out. Boxed, as `bar` is.
    grid: RefCell<Box<Tree>>,
    /// The grid scroll keeps the focus row in view. A finger pan lets go until focus moves.
    follow: bool,
    /// Columns the last grid frame drew. `None` until then — do not invent a count.
    grid_cols_last: Option<usize>,
    /// Last chosen column, carried across vertical moves ([`grid_col_hint`]).
    grid_col: usize,
    /// Recoil axis. A vertical refuse must not nudge the field sideways.
    bump_vertical: bool,
    /// Decoded rasters at draw size, mips baked ([`decode_poster`]). Not deferred encoded.
    art: HashMap<String, Image>,
    /// Posters Skia could not decode; asked once, not every frame.
    art_failed: std::collections::HashSet<String>,
    /// Covers this screen has only the bytes of, decoding off the render thread.
    decoder: ArtDecoder,
    /// Covers decoded and waiting to join `art`, [`ADOPT_PER_FRAME`] at a time.
    arriving: std::collections::VecDeque<(String, Image)>,
    /// Decode scale. This screen does not republish `k`; a grow cannot re-decode.
    art_k: f64,
    /// This device's, from the last render.
    art_budget: ArtBudget,
    /// Last-draw frame per id. Grid pages the whole library; unstamped covers stay forever.
    art_seen: HashMap<String, u64>,
    frame: u64,
    /// Armed after neighbourhood art or 400 ms. Unarmed, nothing draws.
    entrance: Option<Entrance>,
    entrance_armed: bool,
    /// Fan origin. [`Entrance`] wants item distance; the grid measures cells.
    entrance_anchor: usize,
    ready_at: Option<f64>,
    /// `library_sections`, re-read each frame.
    sections: Vec<(crate::library::Section, bool)>,
    /// Where the D-pad is ([`games`]); the field's own cursor is `cursor`.
    zone: Zone,
    /// The zone is the player's or the ready list's, not the arrival guess.
    seated: bool,
    /// Each band's horizontal scroll.
    band_x: Vec<Spring>,
    /// Pills, chips, band items and the state button as last drawn, for the pointer and
    /// the line handoff.
    hits: Vec<(Zone, Rect)>,
    /// Under the Hosts row on the combined home: the plain grid, no pills.
    embedded: bool,
    /// Embedded, with the row holding focus: no focused card, no title line.
    quiet: bool,
}

impl LibraryScreen {
    pub(crate) fn new(host: &HostRow) -> LibraryScreen {
        LibraryScreen {
            host: host.clone(),
            shared: None, // first render adopts from Ctx; the shell owns the handle
            generation: u64::MAX,
            phase: LibraryPhase::Loading,
            games: Vec::new(),
            stale: Stale::No,
            view: Vec::new(),
            sort: crate::collate::SortKey::default(),
            filter: None,
            filter_label: None,
            query: None,
            drilled: false,
            collections: Vec::new(),
            cursor: 0,
            geom: Vec::new(),
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            view_mode: LibraryView::default(),
            bar: Box::default(),
            scroll: Spring::rest(0.0),
            snap_scroll: true,
            grid: RefCell::new(Box::new(Tree::new())),
            follow: true,
            grid_cols_last: None,
            grid_col: 0,
            bump_vertical: false,
            art: HashMap::new(),
            art_failed: std::collections::HashSet::new(),
            decoder: ArtDecoder::default(),
            arriving: std::collections::VecDeque::new(),
            // Design scale. Decode runs at this `k` for the life of the screen.
            art_k: 1.0,
            art_budget: ArtBudget::DESKTOP,
            art_seen: HashMap::new(),
            frame: 0,
            entrance: None,
            entrance_armed: false,
            entrance_anchor: 0,
            ready_at: None,
            sections: crate::library::sections(""),
            zone: Zone::Grid,
            seated: false,
            band_x: Vec::new(),
            hits: Vec::new(),
            embedded: false,
            quiet: false,
        }
    }

    /// The focused host's shelf under the Hosts row.
    pub(crate) fn embedded(host: &HostRow) -> LibraryScreen {
        LibraryScreen {
            embedded: true,
            quiet: true,
            ..LibraryScreen::new(host)
        }
    }

    /// The row has focus (`true`) or has handed it down. A quiet shelf rests on its top
    /// row, where the pad leaves it: a pointer can leave from any row.
    pub(crate) fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
        if let Some(shape) = self.grid_shape().filter(|_| quiet) {
            self.cursor = self.grid_col.min(shape.row_len(0).saturating_sub(1)) as i32;
            self.follow = true;
        }
    }

    /// Titles to walk: Down from the row has somewhere to land.
    pub(crate) fn has_titles(&self) -> bool {
        matches!(self.phase, LibraryPhase::Ready) && self.len() > 0
    }

    /// The cursor is on the grid's top row, where Up leaves an embedded shelf.
    pub(crate) fn at_top(&self) -> bool {
        self.grid_shape()
            .is_none_or(|s| s.cell_of(self.cursor.max(0) as usize).0 == 0)
    }

    /// Arm once neighbourhood posters exist, or after 400 ms (art-less libraries still enter).
    fn arm_entrance(&mut self, t: f64) {
        if self.entrance_armed || !matches!(self.phase, LibraryPhase::Ready) {
            return;
        }
        let since = *self.ready_at.get_or_insert(t);
        let cursor = self.cursor.max(0) as usize;
        let lo = cursor.saturating_sub(2);
        let hi = (cursor + 3).min(self.len());
        let have_art = (lo..hi)
            .filter_map(|i| self.game(i))
            .any(|g| self.art.contains_key(&g.id));
        // Empty filter: no art is coming; do not hold the spinner for the 400 ms deadline.
        if have_art || self.len() == 0 || t - since >= 0.4 {
            self.entrance_armed = true;
            self.entrance_anchor = cursor;
            let shelf = self.view_mode == LibraryView::Shelf && !self.embedded;
            let spec = if shelf {
                entrances::CARDS
            } else {
                entrances::GRID
            };
            self.entrance = Some(Entrance::new(spec, cursor, t));
        }
    }

    /// Fan sample `steps` from the anchor. Grid passes cell distance; a strip passes index.
    ///
    /// `None` is settled only after arming. Unarmed `None` has not begun: fade and travel stay 0.
    fn entrance_at(&self, steps: usize, t: f64) -> EntranceAt {
        match self.entrance {
            Some(e) => e.at(self.entrance_anchor + steps, t),
            None if self.entrance_armed => EntranceAt::SETTLED,
            None => EntranceAt {
                travel: 0.0,
                fade: 0.0,
            },
        }
    }

    /// Re-read sort/view each frame: Settings can change while this screen is on the stack.
    fn adopt_settings(&mut self, ctx: &Ctx) {
        let sort = crate::collate::SortKey::parse(&ctx.settings.library_sort);
        if sort != self.sort {
            self.sort = sort;
            self.recollate();
        }
        let view = match self.embedded {
            true => LibraryView::Grid,
            false => LibraryView::parse(&ctx.settings.library_view),
        };
        let sections = crate::library::sections(&ctx.settings.library_sections);
        if view != self.view_mode || sections != self.sections {
            if view != self.view_mode {
                // A new arrangement lays the field out afresh: seat, do not glide from the old.
                self.snap_scroll = true;
            }
            self.view_mode = view;
            self.sections = sections;
            // Which titles a band holds out of the grid follows both.
            self.recollate();
        }
        // The row was cloned when the shelf opened; what the host has UP moves under it.
        // Only that field: the rest is frozen on purpose (a pin the carousel dropped must
        // not retarget this shelf).
        if let Some(row) = ctx.hosts.iter().find(|h| h.key == self.host.key) {
            self.host.running.clone_from(&row.running);
        }
    }

    /// Columns that fit `rect` at `k`. Per-frame: a stale count puts cursor and layout on different grids.
    fn grid_cols(&self, rect: Rect, k: f64) -> usize {
        let avail = f64::from(rect.width()) - 2.0 * edge(k);
        let pitch = (GRID_W + GRID_GAP) * k;
        // Last column has no trailing gap.
        (((avail + GRID_GAP * k) / pitch).floor() as i64).clamp(2, 8) as usize
    }

    /// Last drawn grid. `None` until a frame: do not invent a column count.
    fn grid_shape(&self) -> Option<GridShape> {
        Some(GridShape::new(
            self.len(),
            self.grid_cols_last?,
            self.lead_count(),
        ))
    }

    /// Remember the cursor's column after a re-sort, press, or resize — not a chosen column.
    fn seat_grid_col(&mut self) {
        if let Some(shape) = self.grid_shape() {
            self.grid_col = shape.cell_of(self.cursor.max(0) as usize).1;
        }
    }

    /// Rebuild display order. Clamp the cursor; identity follow is [`Self::sync`]'s job.
    fn recollate(&mut self) {
        let view = crate::collate::filtered(&self.games, self.sort, self.filter.as_ref());
        let query = self.query.as_deref();
        self.view = (view.into_iter())
            .filter(|&i| !self.banded(&self.games[i]))
            .filter(|&i| query.is_none_or(|q| self.games[i].title.to_lowercase().contains(q)))
            .collect();
        self.cursor = self.cursor.clamp(0, (self.view.len() as i32 - 1).max(0));
        self.seat_grid_col();
        let row = self.sectioned() && self.shows(crate::library::Section::Collections);
        self.collections = match row {
            true => super::collections::collections(&self.games, self.sort),
            false => Vec::new(),
        };
    }

    /// `None` if `i` is past the end or the order is stale.
    fn game(&self, i: usize) -> Option<&LibraryGame> {
        self.games.get(*self.view.get(i)?)
    }

    fn focused(&self) -> Option<&LibraryGame> {
        self.game(self.cursor.max(0) as usize)
    }

    /// Filtered tile count, not the full library.
    fn len(&self) -> usize {
        self.view.len()
    }

    // Narrow host readers for `RefreshRunning`. A `&HostRow` also hands over pin and preset.
    pub(crate) fn host_addr(&self) -> &str {
        &self.host.addr
    }

    pub(crate) fn host_mgmt_port(&self) -> u16 {
        self.host.mgmt_port
    }

    pub(crate) fn host_fp_hex(&self) -> &str {
        &self.host.fp_hex
    }

    /// Whether this is `host`'s shelf: same row key and the same pin, if any.
    pub(crate) fn shelf_of(&self, host: &HostRow) -> bool {
        fn pin(h: &HostRow) -> Option<&str> {
            h.pin.as_ref().map(|p| p.id.as_str())
        }
        self.host.key == host.key && pin(&self.host) == pin(host)
    }

    /// A decoded poster, for the launch hold drawn over this shelf. The focused tile
    /// keeps drawing underneath, so its poster is never the one evicted.
    pub(crate) fn poster(&self, id: &str) -> Option<&Image> {
        self.art.get(id)
    }

    /// Where this title's tile was last drawn — what the launch hold flies its cover
    /// out of. Empty when the shelf has not drawn it (culled, or never laid out), which
    /// the hold reads as "no tile" and arrives in place instead.
    pub(crate) fn tile_rect(&self, id: &str) -> Rect {
        (0..self.geom.len())
            .find(|&i| self.game(i).is_some_and(|g| g.id == id))
            .map_or(Rect::new_empty(), |i| self.geom[i])
    }

    /// Filtered length for tests in another module, which cannot reach [`Self::len`].
    #[cfg(test)]
    pub(crate) fn len_for_test(&self) -> usize {
        self.len()
    }

    /// One collated group. Call before first render so the whole library never flashes.
    pub(crate) fn set_filter(&mut self, key: crate::collate::GroupKey, label: String) {
        self.filter = Some(key);
        self.filter_label = Some(label);
        self.drilled = true;
        self.recollate();
    }

    /// The titles whose name contains `query`, any case. Called before first render, as
    /// [`Self::set_filter`] is.
    pub(crate) fn set_query(&mut self, query: &str) {
        self.query = Some(query.to_lowercase());
        self.filter_label = Some(format!("\u{201c}{query}\u{201d}"));
        self.drilled = true;
        self.recollate();
    }

    /// The whole library with no rows: drilled without a filter.
    #[cfg(test)]
    pub(crate) fn all_titles(&mut self) {
        self.drilled = true;
        self.recollate();
    }

    /// Take decoded posters from the screen that pushed this one. The model's queue is already drained.
    pub(crate) fn adopt_art(&mut self, art: HashMap<String, Image>) {
        self.art = art;
    }

    fn fetch_cmd(&self) -> ConsoleCmd {
        ConsoleCmd::FetchLibrary {
            addr: self.host.addr.clone(),
            mgmt: self.host.mgmt_port,
            fp_hex: self.host.fp_hex.clone(),
        }
    }

    fn sync(&mut self, library: &LibraryShared) {
        if self.shared.is_none() {
            self.shared = Some(library.clone());
        }
        // `LibraryShared` is Arc; a borrow of `self.shared` forbids the `&mut self` recollate.
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if shared.generation() != self.generation {
            let snap = shared.snapshot();
            let (phase, games, generation) = (snap.phase, snap.games, snap.generation);
            // Multiset of ids. An order-only change (running-first from `/status`) is not freshness.
            let fresh = self.games.len() != games.len() || {
                let mut before: Vec<&str> = self.games.iter().map(|g| g.id.as_str()).collect();
                let mut after: Vec<&str> = games.iter().map(|g| g.id.as_str()).collect();
                before.sort_unstable();
                after.sort_unstable();
                before != after
            };
            // Title under the cursor. Stack survives a stream; only a moving list can lose it.
            let anchor = (!fresh)
                .then(|| self.focused().map(|g| g.id.clone()))
                .flatten();
            self.stale = snap.stale;
            // Mount vs later list: an empty screen is "fresh" against every list.
            let was_empty = self.games.is_empty();
            self.phase = phase;
            self.games = games;
            self.generation = generation;
            if fresh {
                self.cursor = 0;
                self.anim = Spring::rest(0.0);
                self.bump = Spring::rest(0.0);
                // First list inherits adopted art (the queue is already drained). A later list clears.
                if !was_empty {
                    self.art.clear();
                    self.art_seen.clear();
                    self.art_failed.clear();
                }
                self.entrance = None;
                self.entrance_armed = false;
                self.ready_at = None;
            }
            self.recollate();
            // After recollate: the filtered view may no longer contain the anchor.
            if let Some(id) = anchor {
                if let Some(i) = self.view.iter().position(|&i| self.games[i].id == id) {
                    self.cursor = i as i32;
                    self.seat_grid_col();
                }
            }
        }
        let k = self.art_k;
        // Publish the size a host should decode at, so one that can decode off-thread produces
        // exactly what this screen would have cached.
        shared.set_art_scale(k);
        // Already-decoded posters cost a move; they queue with the decoder's for adoption.
        let decoded = shared.drain_decoded().into_iter();
        self.arriving
            .extend(decoded.map(|(id, poster)| (id, poster.into_image())));
        self.adopt_decoded();
        // What this screen lacks goes to its decoder. The bytes stay in the model, so a cover
        // another screen took, or one this screen evicted, comes back here.
        // Another screen's cover is a clone, no decode and no upload; the rest decode here.
        let mut wanted = self.art_wanted();
        wanted.retain(|id| match shared_cover(&self.host.fp_hex, id, k) {
            Some(img) => {
                self.art.insert(id.clone(), img);
                false
            }
            None => true,
        });
        let ask = wanted.iter().filter(|id| !self.decoder.pending(id));
        for (id, bytes) in shared.art_for(ask.map(String::as_str), 8) {
            self.decoder.want(id, bytes, k);
        }
    }

    /// Covers the host or the decoder finished join `art` a few a frame, and one this screen
    /// holds stays: a new copy would only upload the same cover again. Undecodable ones are
    /// marked so they are asked once.
    fn adopt_decoded(&mut self) {
        for (id, img) in self.decoder.finished() {
            match img {
                Some(img) => self.arriving.push_back((id, img)),
                None => {
                    tracing::info!(%id, "undecodable poster");
                    self.art_failed.insert(id);
                }
            }
        }
        for _ in 0..ADOPT_PER_FRAME {
            let Some((id, img)) = self.arriving.pop_front() else {
                break;
            };
            share_cover(
                &self.host.fp_hex,
                &id,
                self.art_k,
                &img,
                self.art_budget.held,
            );
            self.art.entry(id).or_insert(img);
        }
    }

    /// Warm-up only: `art` for the titles, bare leading tiles and every third title, so the
    /// warm-up draws each placeholder beside covers. The entrance runs as on a first visit;
    /// both are programs the GPU would otherwise compile then.
    pub(crate) fn warm(&mut self, art: &Image) {
        for (i, g) in self.games.iter().enumerate() {
            if !g.leads() && i % 3 != 2 {
                self.art.insert(g.id.clone(), art.clone());
            }
        }
    }

    /// Titles to decode next: on screen last frame first, rows included, then the next
    /// [`ArtBudget::ahead`] places of the view from its first drawn title, so a scroll decodes
    /// ahead. A window of places, well under [`ArtBudget::held`]: counting only the covers still
    /// missing walks the whole library as they land, and eviction then chases the decoder.
    fn art_wanted(&self) -> Vec<String> {
        let recent = self.frame.saturating_sub(2);
        let seen = |id: &String| self.art_seen.get(id).is_some_and(|&f| f >= recent);
        let lacking = |id: &String| {
            !self.art.contains_key(id)
                && !self.art_failed.contains(id)
                && !self.arriving.iter().any(|(a, _)| a == id)
        };
        let mut out: Vec<String> = (self.games.iter().map(|g| &g.id))
            .filter(|id| seen(id) && lacking(id))
            .cloned()
            .collect();
        let first_seen = (self.view.iter()).position(|&g| seen(&self.games[g].id));
        for &g in (self.view.iter())
            .skip(first_seen.unwrap_or(0))
            .take(self.art_budget.ahead)
        {
            let id = &self.games[g].id;
            if lacking(id) && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    /// The grid's own D-pad: cells, rows and pages. Its edges belong to [`games`].
    fn grid_menu(&mut self, ev: MenuEvent, fx: &mut Outbox) -> Option<MenuPulse> {
        match ev {
            MenuEvent::Move(MenuDir::Left) => self.grid_move(GridDir::Left),
            MenuEvent::Move(MenuDir::Right) => self.grid_move(GridDir::Right),
            MenuEvent::Move(MenuDir::Up) => self.grid_move(GridDir::Up),
            MenuEvent::Move(MenuDir::Down) => self.grid_move(GridDir::Down),
            MenuEvent::JumpBack => self.grid_move(GridDir::PageBack),
            MenuEvent::JumpForward => self.grid_move(GridDir::PageForward),
            _ => self.ready_action(ev, fx),
        }
    }

    fn grid_move(&mut self, dir: GridDir) -> Option<MenuPulse> {
        self.follow = true;
        // Last drawn shape. Before the first grid frame there is nothing to guess from.
        let shape = self.grid_shape()?;
        match grid_step(self.cursor, shape, self.grid_col, dir) {
            StepResult::Moved(c) => {
                self.grid_col = grid_col_hint(shape, self.grid_col, dir, c);
                self.cursor = c;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                // Against the push, on its axis. Zero recoil reads as a dropped input.
                let forward = matches!(dir, GridDir::Right | GridDir::Down | GridDir::PageForward);
                self.bump = Spring {
                    pos: self.bump.pos,
                    vel: -BUMP_V * if forward { 1.0 } else { -1.0 },
                };
                self.bump_vertical = !matches!(dir, GridDir::Left | GridDir::Right);
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// The desktop tile's caption: the host's desk, or the game it already has up. Same
    /// `/status` field the Options screen's Connect row reads, so the two cannot disagree.
    pub(crate) fn desktop_caption(&self) -> String {
        if self.host.running.is_empty() {
            "Desktop".into()
        } else {
            format!("Resume {}", self.host.running)
        }
    }

    /// This shelf's host itself, launching nothing — asking a host to launch what it is
    /// already showing is how a second copy starts.
    fn desktop_intent(&self) -> ConnectIntent {
        ConnectIntent::to_host(&self.host, None)
    }

    /// Launch `g` on this shelf's host. Pinned card: that preset as a one-off; primary
    /// tile: the host's default.
    fn launch_intent(&self, g: &LibraryGame) -> ConnectIntent {
        ConnectIntent::to_host(&self.host, Some((&g.id, &g.title)))
    }

    /// The button the state card offers: Retry after a failure that can retry, the desk
    /// when the host has no titles.
    /// A search that found nothing: the shelf says so instead of standing empty.
    pub(crate) fn no_match(&self) -> bool {
        self.query.is_some() && self.len() == 0 && matches!(self.phase, LibraryPhase::Ready)
    }

    fn state_action(&self) -> Option<&'static str> {
        match self.phase {
            LibraryPhase::Error {
                can_retry: true, ..
            } => Some("Retry"),
            LibraryPhase::Empty => Some("Stream desktop"),
            _ => None,
        }
    }

    /// OK on the state card's button.
    fn state_confirm(&mut self, fx: &mut Outbox) -> Option<MenuPulse> {
        match self.state_action()? {
            "Retry" => {
                self.phase = LibraryPhase::Loading; // optimistic; the fetch re-syncs
                fx.cmds.push(self.fetch_cmd());
            }
            _ => fx.connect = Some(self.desktop_intent()),
        }
        Some(MenuPulse::Confirm)
    }

    fn ready_action(&mut self, ev: MenuEvent, fx: &mut Outbox) -> Option<MenuPulse> {
        if !matches!(self.phase, LibraryPhase::Ready) {
            return None;
        }
        match ev {
            MenuEvent::Confirm => {
                let g = self.focused()?;
                // The desktop tile streams the host and launches nothing — asking a host
                // to launch what it is already showing is how a second copy starts.
                let desktop = g.id == crate::library::DESKTOP_ID;
                if desktop {
                    fx.connect = Some(self.desktop_intent());
                    return Some(MenuPulse::Confirm);
                }
                fx.connect = Some(self.launch_intent(g));
                Some(MenuPulse::Confirm)
            }
            // The poster's menu: Y on a pad, OK held on a remote.
            MenuEvent::Secondary => {
                let g = self.focused()?;
                // The desktop tile IS the host, so its menu is the host's.
                if g.id == crate::library::DESKTOP_ID {
                    fx.options(super::card_menu::CardMenu::for_host(&self.host));
                } else {
                    let cover = self.art.get(&g.id).cloned();
                    fx.options(super::card_menu::CardMenu::for_game(&self.host, g, cover));
                }
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop();
                None
            }
            MenuEvent::Tertiary
            | MenuEvent::Move(_)
            | MenuEvent::Sector(_)
            | MenuEvent::JumpBack
            | MenuEvent::JumpForward => None,
        }
    }

    /// The card under `p`, nearest the cursor first — shelf covers can overlap, and
    /// first-by-index would pick a buried one. Geometry is a frame old, so a refresh that
    /// shortened the shelf cannot produce an index past its end.
    fn card_under(&self, p: Pointer) -> Option<usize> {
        self.geom
            .iter()
            .enumerate()
            .filter(|(i, r)| *i < self.len() && p.hits(**r))
            .min_by_key(|(i, _)| (*i as i32 - self.cursor).abs())
            .map(|(i, _)| i)
    }

    fn step(&mut self, delta: i32, clamp: bool) -> Option<MenuPulse> {
        match step_cursor(self.cursor, self.len(), delta, clamp) {
            StepResult::Moved(to) => {
                self.cursor = to;
                Some(MenuPulse::Move)
            }
            StepResult::Boundary => {
                self.bump = Spring {
                    pos: self.bump.pos,
                    vel: -BUMP_V * f64::from(delta.signum()),
                };
                self.bump_vertical = false; // shelf is a line; recoil is horizontal
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// Length of the leading band ([`LibraryGame::leads`]): the desktop tile and the
    /// launchers behind it. This is [`GridShape`]'s split, not a count of launchers.
    fn lead_count(&self) -> usize {
        self.view
            .iter()
            .map_while(|&i| self.games.get(i))
            .take_while(|g| g.leads())
            .count()
    }

    /// The shelf or grid without the focus claim: Home draws an embedded shelf through this,
    /// the Games tab through [`ScreenView::render`].
    pub(crate) fn draw(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        // Published before the sync that reads it: the poster cache is sized against the
        // scale its covers will be drawn at, and a decode is not something this can redo.
        self.art_k = k;
        self.art_budget = ArtBudget::of(ctx.device);
        self.sync(ctx.library);
        self.adopt_settings(ctx);
        self.frame = self.frame.wrapping_add(1);
        self.anim
            .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
        self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
        self.bump.step(0.0, BUMP_K, BUMP_C, dt);
        self.bump.settle(0.0, 0.3, 4.0);
        // Twin in `home.rs`: haptic survives, travel does not.
        if crate::theme::reduce_motion() {
            self.bump = Spring::rest(0.0);
        }
        let ready = matches!(self.phase, LibraryPhase::Ready);
        if ready {
            self.arm_entrance(ctx.t);
            if self.entrance.is_some_and(|e| e.done(ctx.t)) {
                self.entrance = None;
            }
        }
        // A shelf with no rows around it shows the spinner until its first cards can enter;
        // the tab draws its rows meanwhile and the cards fade in under them.
        let waiting = !self.sectioned()
            && (matches!(self.phase, LibraryPhase::Loading) || (ready && !self.entrance_armed));
        if waiting {
            // Clear geom: `pointer` reads it a frame late.
            self.geom.clear();
            draw_loading(canvas, rect, k, fonts, ctx.t);
            return;
        }
        self.draw_field(canvas, rect, k, dt, fonts, ctx);
        self.evict_art();
    }

    /// After draw: coldest = longest off screen, not longest since decode.
    fn evict_art(&mut self) {
        let live: Vec<String> = self.art.keys().cloned().collect();
        for id in art_to_evict(&live, &self.art_seen, self.art_budget.held) {
            self.art.remove(&id);
            self.art_seen.remove(&id);
        }
    }

    /// The focused-title band shows: on the tab and on a drilled shelf, not under Hosts.
    fn band_shown(&self) -> bool {
        !self.embedded && !self.quiet
    }

    /// The sort says something a card can caption: when it was played, or for how long.
    fn sort_captions(&self) -> bool {
        use crate::collate::SortKey;
        matches!(self.sort, SortKey::Recent | SortKey::PlayTime)
    }

    /// The caption the sort gives `g`, if it has one to give.
    fn sort_caption(&self, g: &LibraryGame) -> Option<String> {
        use crate::collate::SortKey;
        let s = g.stats.as_ref()?;
        match self.sort {
            SortKey::Recent if s.last_played_unix_ms > 0 => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_millis() as u64);
                Some(crate::library::ago(
                    now.saturating_sub(s.last_played_unix_ms),
                ))
            }
            SortKey::PlayTime if s.play_time_ms >= 60_000 => {
                let min = s.play_time_ms / 60_000;
                Some(match min {
                    0..60 => format!("{min} min played"),
                    _ => format!("{} h played", min / 60),
                })
            }
            _ => None,
        }
    }

    /// The screen's lines in one scroll: the pills, the chips and the bands, then the
    /// field — grid, shelf, or the card a failed or empty list leaves — and the title band
    /// over its foot.
    fn draw_field(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &Ctx,
    ) {
        let t = ctx.t;
        let (bands, lines) = self.place_zone(ctx);
        let sectioned = self.sectioned();
        // The lines run on to the screen's foot and blur under the band there.
        let clip = canvas.local_clip_bounds().unwrap_or(rect);
        let foot = clip.bottom.max(rect.bottom);
        let g = self.field_geom(rect, foot, k, &lines, &bands);
        let FieldGeom {
            shape,
            cw,
            ch,
            card_h,
            pitch_x,
            gap_y,
            grid_w,
            heading_h,
            split_row,
            shelf: geo,
            avail,
            view_h,
            ..
        } = g;
        let (item_top, item_h) = g.item_span(self.zone, self.cursor, &lines, &bands);
        // Scroll only as far as it takes to show the item and a breath round it, above the
        // band; a line taller than the view shows its top. Never centred: on a phone that
        // walked the first row up over the host's verbs.
        let breath = REVEAL_AIR * k;
        let mut want = self.scroll.pos;
        if item_top + item_h + breath > want + g.usable {
            want = item_top + item_h + breath - g.usable;
        }
        if item_top - breath < want {
            want = item_top - breath;
        }
        let want = want.clamp(0.0, (g.content_h - view_h).max(0.0));
        let grid = Id::new(GRID, 0);
        let snap = std::mem::take(&mut self.snap_scroll);
        self.follow |= snap;
        self.step_bands(&bands, avail, cw, k, snap);
        let applied = self.applied();
        self.bar.step(fonts, k, avail, true, applied, dt, snap);
        self.follow_scroll(want, snap, dt);

        // The recoil moves the drawing on its axis, never the cells a pointer hits.
        let bump = self.bump.pos * k;
        let (bump_x, bump_y) = if self.bump_vertical {
            (0.0, bump)
        } else {
            (bump, 0.0)
        };
        // Room left of the first column for the plate's outset; the padding gives it back.
        // With a band to treat them, the lines run on up under the chrome as well. A quiet
        // shelf clips at its top: the Hosts row is drawn there.
        let air = PLATE_AIR * k;
        let bleed = crate::blur::active();
        let top = if bleed && !(self.embedded && self.quiet) {
            clip.top.min(rect.top)
        } else {
            rect.top
        };
        let pad_top = rect.top - top;
        let viewport = Rect::from_xywh(
            rect.left - air as f32,
            top,
            rect.width() + air as f32,
            view_h as f32 + pad_top,
        );
        let (anchor_row, anchor_col) =
            shape.cell_of(self.entrance_anchor.min(self.len().saturating_sub(1)));
        let shelf_cards = geo.map(|g| self.shelf_cards(g, avail, k, t));
        let painted = RefCell::new(Vec::new());
        let hits = RefCell::new(Vec::new());
        let (painted, hits) = (&painted, &hits);
        let this = &*self;
        let pill_seen = |i: usize, r: Rect| hits.borrow_mut().push((Zone::Bar(i), r));
        let focus_pill = match this.zone {
            Zone::Bar(i) => Some(i),
            _ => None,
        };

        // A heading in its band, `down` of the band's height above its bottom.
        let heading = |label: &'static str, down: f64| {
            El::paint(move |canvas, band| {
                let (x, y) = (
                    f64::from(band.left) + bump_x,
                    f64::from(band.top) + down + bump_y,
                );
                card::heading(canvas, fonts, label, x, y, k);
            })
        };
        let cell = move |i: usize, row: usize, col: usize| {
            El::paint(move |canvas, slot| {
                painted.borrow_mut().push(i);
                let Some(game) = this.game(i) else { return };
                let steps = ROW_STEPS * anchor_row.abs_diff(row) + anchor_col.abs_diff(col);
                let ent = this.entrance_at(steps, t);
                let focused = i == this.cursor.max(0) as usize && this.zone == Zone::Grid;
                let arrive = (ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel) as f32;
                let (cx, cy) = (slot.center_x(), slot.center_y());
                let rise = ((1.0 - ent.travel) * ENTER_RISE * k) as f32;
                canvas.save();
                canvas.translate((cx + bump_x as f32, cy + bump_y as f32 + rise));
                canvas.scale((arrive, arrive));
                canvas.translate((-cx, -cy));
                let art = this.art.get(&game.id);
                let desk = (game.id == crate::library::DESKTOP_ID).then(|| this.desktop_caption());
                let caption = this.sort_caption(game);
                let card = card::Card {
                    game,
                    art,
                    title: desk.as_deref().unwrap_or(&game.title),
                    host: &this.host,
                    caption: caption.as_deref(),
                    focused: focused && !this.quiet,
                };
                card.paint(canvas, fonts, slot, ch, k, ent.fade as f32);
                canvas.restore();
            })
            .id(grid_cell(i))
            .focusable((card::COVER_CORNER * k) as f32)
            .size(cw as f32, card_h as f32)
        };
        // Rows `from..to` of the grid, built only while in view.
        let rows = move |from: usize, to: usize| {
            El::virtual_list(
                Axis::Vertical,
                to - from,
                card_h as f32,
                gap_y as f32,
                move |r| {
                    let row = from + r;
                    let start = shape.row_start(row);
                    El::row()
                        .gap((pitch_x - cw) as f32)
                        .children((0..shape.row_len(row)).map(|col| cell(start + col, row, col)))
                },
            )
            .width(grid_w as f32)
        };
        let top = |label| heading(label, heading_h * 0.56).size(grid_w as f32, heading_h as f32);
        let mut root = El::scroll(grid, Axis::Vertical).style(|s| {
            s.align_items = Some(taffy::AlignItems::START);
            s.padding.left = taffy::LengthPercentage::length((edge(k) + air) as f32);
            s.padding.top = taffy::LengthPercentage::length(pad_top);
        });
        for &line in &lines {
            root = match line {
                Line::Bar => root
                    .child(
                        this.bar
                            .el(fonts, k, avail, true, applied, focus_pill, &pill_seen),
                    )
                    .child(El::column().size(avail as f32, (BAR_AIR * k) as f32)),
                Line::Chips => root.child(this.chips_el(ctx.hosts, fonts, avail, k, hits)),
                Line::Band(b) => {
                    let band =
                        this.band_el(&bands[b], b, fonts, avail, (cw, ch), viewport, k, hits);
                    root.child(band)
                }
                Line::Grid => match (geo, &shelf_cards) {
                    (Some(g), Some(cards)) => root.child(this.shelf_el(g, cards, fonts, avail, k)),
                    _ => {
                        let air = El::column().size(grid_w as f32, gap_y as f32);
                        match split_row {
                            Some(split) => root
                                .child(top("Launchers"))
                                .child(rows(0, split))
                                .child(
                                    heading("Games", gap_y + heading_h * 0.56)
                                        .size(grid_w as f32, (gap_y + heading_h) as f32),
                                )
                                .child(rows(split, shape.rows())),
                            // Among the tab's rows, the grid is named like the rest.
                            None if sectioned => {
                                root.child(top("Games")).child(rows(0, shape.rows()))
                            }
                            None => root
                                .child(El::column().size(grid_w as f32, heading_h as f32))
                                .child(rows(0, shape.rows())),
                        }
                        .child(air)
                    }
                },
                Line::State => root.child(this.state_el(
                    fonts,
                    avail,
                    g.block_h(Line::State, &bands),
                    k,
                    t,
                    hits,
                )),
            };
        }
        root = root.child(El::column().size(avail as f32, g.pad as f32));
        let mut tree = this.grid.borrow_mut();
        let frame = tree.layout(root, viewport);
        let field = match geo {
            Some(_) => shelf_cover(this.cursor.max(0) as usize),
            None => grid_cell(this.cursor.max(0) as usize),
        };
        tree.set_focus((!this.quiet).then(|| games::zone_id(this.zone, field)));
        let cheap = super::settings::rows::reduce_ui_res(
            ctx.settings,
            ctx.device.platform,
            ctx.device.fallback_ui,
        );
        if bleed {
            tree.paint_focus(canvas, frame, k as f32, dt, cheap);
        } else {
            let scrolled = (tree.offset(grid), frame.scroll(grid).map_or(0.0, |s| s.1));
            crate::widgets::soft_scroll(canvas, viewport, viewport, scrolled, k, || {
                tree.paint_focus(canvas, frame, k as f32, dt, cheap);
            });
        }
        drop(tree);

        self.record_frame(painted.take(), shelf_cards.as_deref(), hits.take(), &bands);
    }

    /// This frame's field metrics in `rect`, the lines running on to `foot`. Navigation
    /// reads last-drawn columns, so a resize re-seats the cursor's column.
    fn field_geom(
        &mut self,
        rect: Rect,
        foot: f32,
        k: f64,
        lines: &[Line],
        bands: &[Band],
    ) -> FieldGeom {
        let shelf = self.view_mode == LibraryView::Shelf && !self.embedded;
        let avail = f64::from(rect.width()) - 2.0 * edge(k);
        let tray = if self.band_shown() {
            crate::widgets::FOOT_TITLE_H * k
        } else {
            0.0
        };
        let band_top = f64::from(rect.bottom) - tray;
        let usable = band_top - f64::from(rect.top);
        let cols = self.grid_cols(rect, k);
        if self.grid_cols_last != Some(cols) {
            self.grid_cols_last = Some(cols);
            self.seat_grid_col();
        }
        let shape = GridShape::new(self.len(), cols, self.lead_count());
        // Two-column clamp can overflow a narrow rect; shrink cells only, never headings.
        let fit = (avail / ((cols as f64 * (GRID_W + GRID_GAP) - GRID_GAP) * k)).clamp(0.25, 1.0);
        let (cw, ch) = (GRID_W * k * fit, GRID_H * k * fit);
        let pitch_x = cw + GRID_GAP * k * fit;
        let heading_h = if self.embedded {
            EMBED_AIR
        } else {
            GRID_HEADING
        } * k;
        let mut g = FieldGeom {
            k,
            shape,
            cw,
            ch,
            card_h: ch + card::text_h(self.sort_captions()) * k,
            pitch_x,
            gap_y: ROW_GAP * k,
            grid_w: cols as f64 * pitch_x - GRID_GAP * k * fit,
            heading_h,
            split_row: (shape.split > 0).then(|| shape.split_row()),
            shelf: shelf.then(|| self.shelf_geo(usable, avail, k)),
            sectioned: self.sectioned(),
            avail,
            usable,
            view_h: f64::from(foot - rect.top),
            tops: Vec::with_capacity(lines.len()),
            content_h: 0.0,
            pad: 0.0,
        };
        for &l in lines {
            g.tops.push(g.content_h);
            let h = g.block_h(l, bands);
            g.content_h += h;
        }
        // Matching bottom inset: the last row clears the band.
        g.pad = heading_h + (f64::from(foot) - band_top);
        g.content_h += g.pad;
        g
    }

    /// Follow focus to `want` while no finger has the scroll. After a pan the spring starts
    /// from wherever the finger left it, so the next move glides instead of jumping.
    fn follow_scroll(&mut self, want: f64, snap: bool, dt: f64) {
        let grid = Id::new(GRID, 0);
        let tree = self.grid.get_mut();
        tree.tick(dt as f32);
        if self.follow && !tree.moving(grid) {
            if snap || crate::theme::reduce_motion() {
                self.scroll = Spring::rest(want);
            } else {
                self.scroll
                    .step_spec(want, crate::anim::springs::FOCUS, 1.0 / 60.0);
                self.scroll.settle(want, 0.05, 0.5);
            }
            tree.set_offset(grid, self.scroll.pos as f32);
        } else {
            self.scroll = Spring::rest(f64::from(tree.offset(grid)));
        }
    }

    /// Hit rects are the cells as laid out; covers drawn this frame stay warm.
    fn record_frame(
        &mut self,
        painted: Vec<usize>,
        shelf_cards: Option<&[ShelfCard]>,
        hits: Vec<(Zone, Rect)>,
        bands: &[Band],
    ) {
        self.geom.clear();
        self.geom.resize(self.len(), Rect::new_empty());
        let tree = self.grid.get_mut();
        for &i in &painted {
            self.geom[i] = tree.rect(grid_cell(i)).unwrap_or_else(Rect::new_empty);
        }
        if let (Some(cards), Some(strip)) = (shelf_cards, tree.rect(shelf_strip())) {
            for c in cards {
                self.geom[c.i] = c.bounds.with_offset((strip.left, strip.top));
            }
        }
        let seen: Vec<usize> = painted
            .into_iter()
            .chain(shelf_cards.into_iter().flatten().map(|c| c.i))
            .collect();
        for i in seen {
            if let Some(id) = self.game(i).map(|g| g.id.clone()) {
                self.art_seen.insert(id, self.frame);
            }
        }
        self.hits = hits;
        for (z, _) in &self.hits {
            let Zone::Band { band, item } = *z else {
                continue;
            };
            let drawn: &[usize] = match bands[band].items.get(item) {
                Some(games::Item::Game(g)) => std::slice::from_ref(g),
                Some(games::Item::Collection(c)) => &self.collections[*c].fan,
                _ => &[],
            };
            for &g in drawn {
                self.art_seen.insert(self.games[g].id.clone(), self.frame);
            }
        }
    }

    /// The focused title's band reaches the shell's tray in this far, over the lines' foot.
    pub(crate) fn pinned(&self, k: f64) -> (f32, f32) {
        if self.band_shown() {
            (0.0, (crate::widgets::FOOT_TITLE_H * k) as f32)
        } else {
            (0.0, 0.0)
        }
    }

    /// The focused title on the shell's tray: drawn after the trays, over the lines.
    pub(crate) fn render_pinned(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        fonts: &Fonts,
        ctx: &Ctx,
    ) {
        if !self.band_shown() {
            return;
        }
        let band = self.title_band(ctx);
        let h = (crate::widgets::FOOT_TITLE_H * k) as f32;
        crate::widgets::Foot {
            title: band.title.as_deref(),
            subtitle: band.subtitle.as_deref(),
            note: band.note,
            deep: false,
            ..Default::default()
        }
        .paint(
            canvas,
            fonts,
            Rect::from_ltrb(rect.left, rect.bottom - h, rect.right, rect.bottom),
            (0.0, 0.0),
            k,
        );
    }

    /// What the title band says for the focus: the title and, on the grid, where it comes
    /// from — `STORE · PLATFORM`, or `STORE · LAUNCHER`.
    fn title_band(&self, ctx: &Ctx) -> card::TitleBand<'static> {
        let note = self.stale.note();
        let game = match self.zone {
            Zone::Grid => self.focused(),
            _ => self.zone_game(ctx),
        };
        let title = match (self.zone_title(ctx), game) {
            (Some(title), _) => Some(title),
            (None, Some(g)) if g.id == crate::library::DESKTOP_ID => Some(self.desktop_caption()),
            (None, Some(g)) => Some(g.title.clone()),
            (None, None) => None,
        };
        let grid = self.view_mode == LibraryView::Grid || self.zone != Zone::Grid;
        let subtitle = game
            .filter(|g| grid && g.id != crate::library::DESKTOP_ID)
            .map(|g| {
                let store = store_label(&g.store).to_uppercase();
                match (g.launcher, g.platform.as_deref().map(str::trim)) {
                    (true, _) => format!("{store} \u{b7} LAUNCHER"),
                    (false, Some(p)) if !p.is_empty() => {
                        format!("{store} \u{b7} {}", p.to_uppercase())
                    }
                    _ => store,
                }
            });
        card::TitleBand {
            title,
            subtitle,
            note,
        }
    }

    /// The shelf's cover size and its block's height, with a heading on the tab or when the
    /// strip holds both the lead tiles and titles.
    fn shelf_geo(&self, usable: f64, avail: f64, k: f64) -> ShelfGeo {
        let lead = self.lead_count();
        let head = if self.sectioned() || (lead > 0 && lead < self.len()) {
            SHELF_HEAD * k
        } else {
            0.0
        };
        let reserve = (bar::BAR_H + BAR_AIR + SHELF_AIR) * k + head;
        let h = (usable - reserve)
            .max(SHELF_COVER_MIN * k)
            .min(POSTER_H * k)
            .min(avail * 0.9);
        ShelfGeo {
            w: h * 2.0 / 3.0,
            h,
            head,
            block: head + h + SHELF_AIR * k,
        }
    }

    /// Every cover in reach, strip-local, farthest from the cursor first so nearer covers
    /// draw over them. One step off focus a cover shrinks, fades and turns about the edge
    /// facing focus; further ones hold that pose and keep their spacing.
    fn shelf_cards(&self, g: ShelfGeo, width: f64, k: f64, t: f64) -> Vec<ShelfCard> {
        let pitch = g.w + SHELF_SPACING * k;
        let pos = self.anim.pos;
        let cx0 = width / 2.0 + self.bump.pos * k;
        let cy = g.head + SHELF_AIR * k / 2.0 + g.h / 2.0;
        let reach = (width / 2.0 + PLATE_AIR * k) / pitch + 1.0;
        let mut out: Vec<ShelfCard> = (0..self.len())
            .filter(|&i| (i as f64 - pos).abs() <= reach)
            .map(|i| {
                let d = i as f64 - pos;
                let v = d.clamp(-1.0, 1.0);
                let prox = v.abs();
                let ent = self.entrance_at(i.abs_diff(self.entrance_anchor), t);
                let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
                let side = if (i as i32) < self.cursor { -1.0 } else { 1.0 };
                let turn = (1.0 - ent.travel) * ENTER_TURN_DEG * side;
                let scale = (1.0 - prox * RECEDE_SCALE) * arrive;
                let m = shelf_matrix(
                    (cx0 + d * pitch, cy + (1.0 - ent.travel) * ENTER_RISE * k),
                    (g.w, g.h),
                    scale,
                    -v * ROTATE_DEG + turn,
                    (1.0 - v) * g.w / 2.0,
                    g.h / SHELF_EYE,
                );
                let corners = [(0.0, 0.0), (g.w, 0.0), (0.0, g.h), (g.w, g.h)]
                    .map(|(x, y)| project(&m, x, y));
                let (mut l, mut tp, mut r, mut b) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
                for (x, y) in corners {
                    (l, tp, r, b) = (l.min(x), tp.min(y), r.max(x), b.max(y));
                }
                ShelfCard {
                    i,
                    m,
                    size: (g.w, g.h),
                    prox,
                    fade: ent.fade * (1.0 - RECEDE_FADE * prox),
                    bounds: Rect::from_ltrb(l as f32, tp as f32, r as f32, b as f32),
                }
            })
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse((c.i as i32 - self.cursor).abs()));
        out
    }

    /// The shelf as a line: its heading, the covers, and the focused one as its own node so
    /// the plate stands behind it and a press dips it.
    fn shelf_el<'a>(
        &'a self,
        g: ShelfGeo,
        cards: &'a [ShelfCard],
        fonts: &'a Fonts,
        width: f64,
        k: f64,
    ) -> El<'a> {
        let focus = self.cursor.max(0) as usize;
        let lead = self.lead_count();
        // A coverflow is one line: the heading names the group the cursor is in. On the
        // tab it sits on the margin like every row's; on its own it is centred.
        let heading = match (g.head > 0.0, focus >= lead || lead == self.len()) {
            (false, _) => None,
            (true, true) => Some("Games"),
            (true, false) if lead == 1 => Some("Host"),
            (true, false) => Some("Launchers"),
        };
        let centred = !self.sectioned();
        let strip = El::paint(move |canvas, r| {
            if let Some(label) = heading {
                let (size, track) = (14.0 * k, 1.1 * k);
                let tw = f64::from(fonts.measure(label, W::SemiBold, size))
                    + track * label.chars().count().saturating_sub(1) as f64;
                let x = match centred {
                    true => f64::from(r.center_x()) - tw / 2.0,
                    false => f64::from(r.left),
                };
                card::heading(canvas, fonts, label, x, f64::from(r.top) + g.head * 0.7, k);
            }
            for c in cards.iter().filter(|c| c.i != focus) {
                self.paint_shelf_cover(canvas, fonts, c, (r.left, r.top), k);
            }
        })
        .id(shelf_strip())
        .place(Rect::from_xywh(0.0, 0.0, width as f32, g.block as f32));
        let mut el = El::column().size(width as f32, g.block as f32).child(strip);
        if let Some(c) = cards.iter().find(|c| c.i == focus) {
            let b = c.bounds;
            el = el.child(
                El::paint(move |canvas, r| {
                    self.paint_shelf_cover(canvas, fonts, c, (r.left - b.left, r.top - b.top), k);
                })
                .id(shelf_cover(focus))
                .focusable((SHELF_CORNER * k) as f32)
                .place(b),
            );
        }
        el
    }

    /// One shelf cover through its transform, `origin` the strip's top-left on screen.
    fn paint_shelf_cover(
        &self,
        canvas: &Canvas,
        fonts: &Fonts,
        c: &ShelfCard,
        origin: (f32, f32),
        k: f64,
    ) {
        let Some(game) = self.game(c.i) else { return };
        canvas.save();
        canvas.translate(origin);
        canvas.concat_44(&M44::row_major(&c.m.map(|x| x as f32)));
        let crect = Rect::from_wh(c.size.0 as f32, c.size.1 as f32);
        let corner = (SHELF_CORNER * k) as f32;
        let rr = RRect::new_rect_xy(crect, corner, corner);
        canvas.clip_rrect(rr, None, true);
        // Fade and recede ride each piece's paint: a layer per cover is a framebuffer round
        // trip on a tiled GPU, every frame for every neighbour. A placeholder is flat pieces
        // that must recede as one, so it alone keeps a layer.
        let recede = (c.prox > 0.001).then(|| {
            skia_safe::color_filters::matrix_row_major(&crate::theme::recede_matrix(c.prox), None)
        });
        let art = self.art.get(&game.id);
        let layered = art.is_none() && (c.fade < 0.999 || recede.is_some());
        if layered {
            let mut lp = crate::theme::layer();
            lp.set_alpha_f(c.fade as f32);
            lp.set_color_filter(recede.clone());
            // After the clip so `None` bounds are the cover, not the screen.
            canvas.save_layer(
                &skia_safe::canvas::SaveLayerRec::default()
                    .paint(&lp)
                    .flags(crate::theme::layer_flags(canvas)),
            );
        }
        let alpha = if layered { 1.0 } else { c.fade as f32 };
        match art {
            Some(img) => {
                let src = card::crop(img, crect);
                let mut p = fill(fg(1.0));
                p.set_alpha_f(alpha);
                p.set_color_filter(recede);
                canvas.draw_image_rect_with_sampling_options(
                    img,
                    Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                    crect,
                    art_sampling(),
                    &p,
                );
            }
            None => draw_poster_placeholder(canvas, fonts, Some(game), crect, k, 1.0),
        }
        // The cover's alpha, so a neighbour's badges fade with it.
        card::store_badge(canvas, fonts, game, crect, k, true, alpha);
        if game.running {
            card::running_badge(canvas, fonts, crect, k, alpha);
        }
        canvas.draw_rrect(rr.with_inset((0.5, 0.5)), &stroke(fg(0.12 * alpha), 1.0));
        if layered {
            canvas.restore();
        }
        canvas.restore();
    }

    /// What a list that is not ready says, and the button it offers, centred in a block
    /// `h` tall. A pushed or failed shelf with nothing else to stand on focuses the button.
    #[allow(clippy::too_many_arguments)]
    fn state_el<'a>(
        &'a self,
        fonts: &'a Fonts,
        width: f64,
        h: f64,
        k: f64,
        t: f64,
        hits: &'a RefCell<Vec<(Zone, Rect)>>,
    ) -> El<'a> {
        let (title, body): (String, String) = match &self.phase {
            LibraryPhase::Error { title, body, .. } => (title.clone(), body.clone()),
            LibraryPhase::Empty => (
                "No games found".into(),
                "Install Steam titles or add custom entries in the host's web console. \
                 This host still streams its desktop."
                    .into(),
            ),
            LibraryPhase::Ready if self.no_match() => (
                "No matches".into(),
                format!(
                    "No title on this host contains {}.",
                    self.filter_label.as_deref().unwrap_or_default()
                ),
            ),
            _ => (String::new(), "Loading library\u{2026}".into()),
        };
        let loading = matches!(self.phase, LibraryPhase::Loading)
            || (matches!(self.phase, LibraryPhase::Ready) && !self.no_match());
        let action = (!self.embedded).then(|| self.state_action()).flatten();
        let button_h = BUTTON_H * k;
        let content = 30.0 * k
            + 44.0 * k
            + if action.is_some() {
                18.0 * k + button_h
            } else {
                0.0
            };
        let y0 = ((h - content) / 2.0).max(0.0);
        let copy = El::paint(move |canvas, r| {
            let cx = f64::from(r.center_x());
            let top = f64::from(r.top) + y0;
            let max_w = (600.0 * k).min(width * 0.85);
            if loading {
                crate::theme::spinner(canvas, cx, top + 14.0 * k, 16.0 * k, t);
            } else {
                fonts.centered(canvas, &title, W::Bold, 22.0 * k, fg(1.0), cx, top, max_w);
            }
            fonts.centered(
                canvas,
                &body,
                W::Regular,
                14.0 * k,
                fg(0.6),
                cx,
                top + 36.0 * k,
                max_w,
            );
        })
        .place(Rect::from_xywh(0.0, 0.0, width as f32, h as f32));
        let mut el = El::column().size(width as f32, h as f32).child(copy);
        if let Some(label) = action {
            let bw = button_w(fonts, label, k);
            let r = Rect::from_xywh(
                ((width - bw) / 2.0) as f32,
                (y0 + content - button_h) as f32,
                bw as f32,
                button_h as f32,
            );
            el = el.child(
                El::paint(move |canvas, r| {
                    hits.borrow_mut().push((Zone::State, r));
                    button(canvas, fonts, label, r, k);
                })
                .id(games::state_id())
                .focusable((button_h / 2.0) as f32)
                .place(r),
            );
        }
        el
    }
}

impl ScreenView for LibraryScreen {
    /// OK went down: the plate dips under the focused poster.
    fn press(&mut self) {
        self.grid.get_mut().press();
    }

    fn title(&self) -> String {
        let mut t = match &self.host.pin {
            Some(p) => format!("{} \u{b7} {}", self.host.name, p.name),
            None => self.host.name.clone(),
        };
        if let Some(label) = &self.filter_label {
            t.push_str(" \u{b7} ");
            t.push_str(label);
        }
        t
    }

    /// The pad. Off the field, [`games`] routes it between lines; on it, the grid or the
    /// shelf moves its cursor. Under the Hosts row the home owns the lines.
    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        self.sync(ctx.library);
        self.adopt_settings(ctx);
        if self.embedded {
            if matches!(self.phase, LibraryPhase::Ready) {
                return self.grid_menu(ev, fx);
            }
            if ev == MenuEvent::Back {
                fx.pop();
            }
            return None;
        }
        if let Some(pulse) = self.zone_menu(ev, ctx, fx) {
            return pulse;
        }
        match self.view_mode {
            LibraryView::Grid => self.grid_menu(ev, fx),
            LibraryView::Shelf => match ev {
                MenuEvent::Move(MenuDir::Left) => self.step(-1, false),
                MenuEvent::Move(MenuDir::Right) => self.step(1, false),
                MenuEvent::JumpBack => self.step(-JUMP, true),
                MenuEvent::JumpForward => self.step(JUMP, true),
                _ => self.ready_action(ev, fx),
            },
        }
    }

    /// A finger drag on the screen's scroll: the lines follow it and a lift flings them.
    fn pan(&mut self, p: Pointer) -> bool {
        let taken = self.grid.get_mut().drag(Id::new(GRID, 0), p);
        if taken && matches!(p.kind, PointerKind::PanStart { .. }) {
            self.follow = false;
        }
        taken
    }

    /// Hover focuses; a press on the focused card launches, on another brings it to focus.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        match p.kind {
            PointerKind::Scroll { up } => {
                // One card on the shelf, one row on the grid.
                if self.view_mode == LibraryView::Grid {
                    self.grid_move(if up { GridDir::Up } else { GridDir::Down });
                } else {
                    self.step(if up { -1 } else { 1 }, false);
                }
                true
            }
            // Hover focuses, so the press that follows opens the card rather than reaching
            // it. A touchscreen sends Press with no Move first, so its two-press path stands.
            PointerKind::Move => {
                if let Some(same) = self.zone_pointer(p, false) {
                    return !same;
                }
                match self.card_under(p) {
                    Some(i) if i != self.cursor as usize || self.zone != Zone::Grid => {
                        self.cursor = i as i32;
                        self.zone = Zone::Grid;
                        self.seat_grid_col();
                        true
                    }
                    _ => false,
                }
            }
            PointerKind::Press => {
                if let Some(ok) = self.zone_pointer(p, true) {
                    if ok {
                        self.menu(MenuEvent::Confirm, ctx, fx);
                    }
                    return true;
                }
                match self.card_under(p) {
                    Some(i) if i == self.cursor as usize && self.zone == Zone::Grid => {
                        self.menu(MenuEvent::Confirm, ctx, fx);
                        true
                    }
                    Some(i) => {
                        self.cursor = i as i32;
                        self.zone = Zone::Grid;
                        self.seat_grid_col();
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// What a screen reader speaks: a pill and whether it is applied, the state's button,
    /// or the title the band shows. Nothing while a plain shelf is still loading.
    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        match self.zone {
            Zone::Bar(i) if !self.embedded => {
                let pill = bar::Pill::all(true)[i];
                let group = match pill {
                    bar::Pill::Sort(_) => "Sort",
                    bar::Pill::View(_) => "View",
                    bar::Pill::Search => return Some("Search titles".into()),
                };
                let on = self.applied().contains(&i);
                let state = if on { ", selected" } else { "" };
                return Some(format!("{group} {}{state}", pill.label()));
            }
            Zone::State if !self.embedded => return self.state_action().map(str::to_string),
            _ => {}
        }
        if !self.embedded {
            if let Some(title) = self.zone_title(ctx) {
                return Some(title);
            }
        }
        if !matches!(self.phase, LibraryPhase::Ready) {
            return None;
        }
        let game = self.focused()?;
        Some(if game.id == crate::library::DESKTOP_ID {
            self.desktop_caption()
        } else {
            game.title.clone()
        })
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if !self.embedded {
            if let Some(hints) = self.zone_hints(ctx) {
                return hints;
            }
        }
        if !matches!(self.phase, LibraryPhase::Ready) || self.focused().is_none() {
            return vec![Hint::new(HintKey::Back, "Back")];
        }
        let desktop = self
            .focused()
            .is_some_and(|g| g.id == crate::library::DESKTOP_ID);
        let running = self.focused().is_some_and(|g| g.running);
        let launcher = self.focused().is_some_and(|g| g.launcher);
        let ok = match (desktop, running, launcher) {
            // The desktop tile resumes when the host has a game up, and it is the ONE tile
            // that never launches anything.
            (true, _, _) if !self.host.running.is_empty() => "Resume",
            (true, _, _) => "Stream",
            (_, true, _) => "Resume",
            (_, false, true) => "Open",
            (_, false, false) => "Play",
        };
        vec![
            Hint::new(HintKey::Confirm, ok),
            Hint::new(HintKey::Secondary, "Options"),
            Hint::new(HintKey::Back, "Back"),
        ]
    }

    // The shelf view draws its focus outside el: titles to walk are its targets.
    fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        ctx: &mut Ctx,
    ) {
        self.draw(canvas, rect, k, dt, fonts, ctx);
        crate::el::claim(usize::from(self.has_titles()));
    }
}

/// The field's metrics this frame, px: the grid's cells and rows, the shelf when it shows,
/// and each line's top in the one scroll.
struct FieldGeom {
    k: f64,
    shape: GridShape,
    /// A grid cell, and the card under it with its caption.
    cw: f64,
    ch: f64,
    card_h: f64,
    pitch_x: f64,
    gap_y: f64,
    grid_w: f64,
    heading_h: f64,
    /// The games section's first row when the launchers hold rows of their own.
    split_row: Option<usize>,
    shelf: Option<ShelfGeo>,
    sectioned: bool,
    /// Width inside the edges, height above the band, height to the screen's foot.
    avail: f64,
    usable: f64,
    view_h: f64,
    /// Each line's top in `lines` order; the scroll's height, bottom inset `pad` included.
    tops: Vec<f64>,
    content_h: f64,
    pad: f64,
}

impl FieldGeom {
    /// Grid row `row`'s top. The top inset is always on: it is also the air row 0 needs.
    fn row_top(&self, row: usize) -> f64 {
        let section_gap = match self.split_row {
            Some(s) if row >= s => self.heading_h,
            _ => 0.0,
        };
        row as f64 * (self.card_h + self.gap_y) + self.heading_h + section_gap
    }

    fn block_h(&self, line: Line, bands: &[Band]) -> f64 {
        let k = self.k;
        match line {
            Line::Bar => (bar::BAR_H + BAR_AIR) * k,
            Line::Chips => LibraryScreen::chips_h(k),
            Line::Band(b) => LibraryScreen::band_h(&bands[b], self.ch, k),
            Line::Grid => match self.shelf {
                Some(g) => g.block,
                None => {
                    self.row_top(self.shape.rows().saturating_sub(1)) + self.card_h + self.gap_y
                }
            },
            Line::State if self.sectioned => STATE_H * k,
            Line::State => self.usable,
        }
    }

    /// The focused item's `(top, height)`: a grid row (its heading too on row 0), or a
    /// whole line.
    fn item_span(&self, zone: Zone, cursor: i32, lines: &[Line], bands: &[Band]) -> (f64, f64) {
        let top_of = |l: Line| {
            lines
                .iter()
                .position(|&x| x == l)
                .map_or(0.0, |i| self.tops[i])
        };
        let (focus_row, _) = self.shape.cell_of(cursor.max(0) as usize);
        match (zone, self.shelf) {
            (Zone::Grid, None) if focus_row > 0 => {
                (top_of(Line::Grid) + self.row_top(focus_row), self.card_h)
            }
            (Zone::Grid, None) => (top_of(Line::Grid), self.row_top(0) + self.card_h),
            (z, _) => {
                let line = games::line_of(z);
                (top_of(line), self.block_h(line, bands))
            }
        }
    }
}

/// The shelf's metrics this frame, px: cover size, heading, and the whole block.
#[derive(Clone, Copy)]
struct ShelfGeo {
    w: f64,
    h: f64,
    head: f64,
    block: f64,
}

/// One cover on the shelf: its transform and projected bounds, strip-local, and how far
/// it has receded (0 focused, 1 one step off or more).
struct ShelfCard {
    i: usize,
    m: [f64; 16],
    size: (f64, f64),
    prox: f64,
    fade: f64,
    bounds: Rect,
}

#[cfg(test)]
mod tests;
