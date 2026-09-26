//! One host's titles: the Games tab's rows over a grid or a coverflow shelf.
//!
//! One screen on the shell stack. B pops; A launches the focused title in this window.
//! The shell owns aurora, chrome and the connecting overlay. Every line lives in one
//! scroll and one focus tree ([`games`]): the sort/view pills ([`bar`]), the host chips,
//! the section rows, and the field, which is the grid or the shelf by `library_view`.
//! A collection's shelf has only the pills and its field.
//!
//! `host.pin` is load-bearing: a pinned card launches with that preset. Posters decode
//! here ([`decode_poster`]), so every screen keeps one cache size.
//! Entrance waits for neighbourhood art or 400 ms. Pin with the tests in this module.

use crate::anim::{entrances, Entrance, EntranceAt, Spring};
use crate::el::{Axis, El, Id, Tree};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    grid_col_hint, grid_step, initials, project, shelf_matrix, step_cursor, store_label, GridDir,
    GridShape, LibraryGame, LibraryPhase, LibraryShared, LibraryView, Stale, StepResult, BUMP_C,
    BUMP_K, BUMP_V, ENTER_RISE, ENTER_SCALE, ENTER_TURN_DEG, GRID_GAP, GRID_H, GRID_W, JUMP,
    POSTER_H, RECEDE_FADE, RECEDE_SCALE, ROTATE_DEG, SHELF_CORNER, SHELF_COVER_MIN, SHELF_EYE,
    SHELF_SPACING, SPRING_C, SPRING_K,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox};
use crate::theme::{art_sampling, edge, fg, fill, stroke, Fonts, W};
use crate::widgets::{button, button_w, BUTTON_H};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Point, RRect, Rect, M44};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

pub(crate) mod bar;
mod card;
mod games;
pub(crate) use games::CustomizeScreen;
use games::{Line, Zone};

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
/// ~nine grid rows. At [`ART_CACHE_W`] each raster is ~0.7 MB, so this is also RAM.
const ART_BUDGET: usize = 160;
/// Twice the grid cell: mip levels both arrangements sample. Smaller magnifies the shelf.
const ART_CACHE_W: f64 = GRID_W * 2.0;
const ART_CACHE_H: f64 = GRID_H * 2.0;
/// Fit `src` into [`ART_CACHE_W`]×[`ART_CACHE_H`] at `k`. Source aspect; never enlarge.
///
/// A 460×215 header squeezed to 2:3 stretches what the draw already centre-crops.
fn art_cache_size(src: (i32, i32), k: f64) -> (i32, i32) {
    let (iw, ih) = (f64::from(src.0), f64::from(src.1));
    if iw <= 0.0 || ih <= 0.0 {
        return src;
    }
    // Source is the sharpness ceiling; enlarging only spends RAM to magnify sooner.
    let s = (ART_CACHE_W * k / iw).min(ART_CACHE_H * k / ih).min(1.0);
    (
        (iw * s).round().max(1.0) as i32,
        (ih * s).round().max(1.0) as i32,
    )
}

/// Decode a poster on a thread that is not drawing, ready to hand to the shell.
///
/// The whole point of the seam: on a 2020 TV a full-size PNG cover costs ~90 ms, and paying
/// that in the frame loop stops the shelf five frames at a time. A host with a fetch thread
/// already has somewhere better to spend it. `k` comes from [`LibraryShared::art_scale`], so
/// the size matches what this screen would have cached anyway.
///
/// `None` when the bytes will not decode, or when the result cannot be moved between threads —
/// either way the caller still has its encoded bytes and can push those instead.
pub fn decode_poster_off_thread(bytes: &[u8], k: f64) -> Option<crate::library::DecodedPoster> {
    crate::library::DecodedPoster::new(decode_poster(bytes, k)?)
}

thread_local! {
    /// Covers every screen on the drawing thread shares, by host fingerprint and title. One
    /// decode and one upload serve the Hosts shelf, the Games tab and a collection alike.
    static SHARED_ART: RefCell<SharedArt> = RefCell::new(SharedArt::default());
}

/// [`SHARED_ART`]: at most [`ART_BUDGET`] covers, the oldest shared first out, all at one scale.
#[derive(Default)]
struct SharedArt {
    k: f64,
    covers: HashMap<(String, String), Image>,
    order: std::collections::VecDeque<(String, String)>,
}

/// The cover another screen already holds for `id` on the host `fp`, at scale `k`.
pub(super) fn shared_cover(fp: &str, id: &str, k: f64) -> Option<Image> {
    SHARED_ART.with(|a| {
        let a = a.borrow();
        (a.k == k)
            .then(|| a.covers.get(&(fp.to_string(), id.to_string())).cloned())
            .flatten()
    })
}

/// Offer a cover to every screen. A new scale starts the cache over.
pub(super) fn share_cover(fp: &str, id: &str, k: f64, img: &Image) {
    SHARED_ART.with(|a| {
        let mut a = a.borrow_mut();
        if a.k != k {
            *a = SharedArt {
                k,
                ..SharedArt::default()
            };
        }
        let key = (fp.to_string(), id.to_string());
        if a.covers.insert(key.clone(), img.clone()).is_none() {
            a.order.push_back(key);
        }
        while a.order.len() > ART_BUDGET {
            if let Some(old) = a.order.pop_front() {
                a.covers.remove(&old);
            }
        }
    });
}

/// A cover to decode: its id, its bytes, the scale; and what came of it.
type ArtJob = (String, Arc<[u8]>, f64);
type ArtDone = (String, Option<crate::library::DecodedPoster>);

/// A screen's covers decoded off the thread that draws: one worker, started on first use and
/// stopped with the screen. A TV spends 15–90 ms on one cover, a dropped frame each on the
/// render thread, and a screen that only has a cover's bytes must decode it itself.
#[derive(Default)]
pub(super) struct ArtDecoder {
    #[cfg_attr(test, allow(dead_code))]
    jobs: Option<std::sync::mpsc::Sender<ArtJob>>,
    #[cfg_attr(test, allow(dead_code))]
    done: Option<std::sync::mpsc::Receiver<ArtDone>>,
    pending: std::collections::HashSet<String>,
    /// Tests decode inline so a frame count stays deterministic.
    #[cfg(test)]
    ready: Vec<(String, Option<Image>)>,
}

impl ArtDecoder {
    /// Decode `bytes` at `k` for `id`, unless it is already on its way.
    pub(super) fn want(&mut self, id: String, bytes: Arc<[u8]>, k: f64) {
        if !self.pending.insert(id.clone()) {
            return;
        }
        #[cfg(test)]
        {
            self.ready.push((id, decode_poster(&bytes, k)));
        }
        #[cfg(not(test))]
        {
            let jobs = self.jobs.get_or_insert_with(|| {
                let (jobs, rx) = std::sync::mpsc::channel::<ArtJob>();
                let (tx, done) = std::sync::mpsc::channel();
                self.done = Some(done);
                let _ = std::thread::Builder::new()
                    .name("pf-console-art".into())
                    .spawn(move || {
                        for (id, bytes, k) in rx {
                            if tx.send((id, decode_poster_off_thread(&bytes, k))).is_err() {
                                return;
                            }
                        }
                    });
                jobs
            });
            let _ = jobs.send((id, bytes, k));
        }
    }

    pub(super) fn pending(&self, id: &str) -> bool {
        self.pending.contains(id)
    }

    /// Decodes finished since the last call; `None` for a cover that would not decode.
    pub(super) fn finished(&mut self) -> Vec<(String, Option<Image>)> {
        #[cfg(test)]
        let out: Vec<_> = std::mem::take(&mut self.ready);
        #[cfg(not(test))]
        let out: Vec<_> = self.done.as_ref().map_or_else(Vec::new, |rx| {
            rx.try_iter()
                .map(|(id, p)| (id, p.map(crate::library::DecodedPoster::into_image)))
                .collect()
        });
        for (id, _) in &out {
            self.pending.remove(id);
        }
        out
    }
}

/// Decode here (not at first draw) and bake mips at [`art_cache_size`].
///
/// `Image::from_encoded` defers decode until use; a GPU purge then re-decodes JPEG on
/// the render thread.
pub(super) fn decode_poster(bytes: &[u8], k: f64) -> Option<Image> {
    let started = std::time::Instant::now();
    let data = Data::new_copy(bytes);
    // Decoding at the size we keep, rather than in full and then resampling, is most of the
    // cost of filling a shelf: see [`decode_near_cache_size`].
    let (img, native_scaled) = match decode_near_cache_size(&data, k) {
        Some(img) => (img, true),
        None => (Image::from_encoded(data)?, false),
    };
    let want = art_cache_size((img.width(), img.height()), k);
    let scaled = if want == (img.width(), img.height()) {
        None
    } else {
        // Overlay targets are `new_n32_premul` with no colour space (`ensure_slot`).
        let info = skia_safe::ImageInfo::new_n32_premul(want, None);
        img.make_scaled(&info, art_sampling())
    };
    // A refused scale keeps the full-size image rather than dropping the cover. Raster either
    // way: a lazy image takes no mips and decodes at first draw, on the render thread.
    let out = scaled.or_else(|| img.make_raster_image(None, None))?;
    let mipped = out.with_default_mipmaps();
    crate::art_stats::record(started.elapsed(), native_scaled);
    Some(mipped.unwrap_or(out))
}

/// Decode straight to (or just above) the size the cache keeps, using the codec's own scaling.
///
/// A JPEG scales in the DCT — 1/2, 3/8, 1/4 and so on come out of the decoder for a fraction
/// of the work a full decode costs, and a 600×900 cover into a 300×450 cache is exactly the
/// 1/2 case. The full-size decode this replaces threw most of those pixels away in the resample
/// on the very next line.
///
/// `None` when the codec cannot help — an unsupported format, or art already small enough to
/// keep whole — and the caller falls back to decoding it in full.
fn decode_near_cache_size(data: &Data, k: f64) -> Option<Image> {
    let mut codec = skia_safe::codec::Codec::from_data(data.clone())?;
    let src = codec.dimensions();
    let want = art_cache_size((src.width, src.height), k);
    if want == (src.width, src.height) {
        return None;
    }
    // Width alone: `art_cache_size` keeps the source aspect, so both axes carry one scale.
    let desired = want.0 as f32 / src.width as f32;
    let native = codec.get_scaled_dimensions(desired);
    // A codec that only offers the original size has nothing to give here — and one that
    // UNDERSHOOTS is refused rather than accepted: `get_scaled_dimensions` approximates, and a
    // decode below the cache size would quietly make every cover softer than the resample it
    // replaced. Both cases fall back to the full decode.
    if native == src || native.width < want.0 || native.height < want.1 {
        return None;
    }
    let info = skia_safe::ImageInfo::new_n32_premul((native.width, native.height), None);
    codec.get_image(info, None).ok()
}

/// Coldest stamps first, past [`ART_BUDGET`]. Split out so the policy tests without Skia.
fn art_to_evict(live: &[String], seen: &HashMap<String, u64>) -> Vec<String> {
    if live.len() <= ART_BUDGET {
        return Vec::new();
    }
    let mut by_age: Vec<(u64, &String)> = live
        .iter()
        // Never-drawn stamps 0. The model keeps the bytes, so `sync` refills what comes back
        // on screen.
        .map(|id| (seen.get(id).copied().unwrap_or(0), id))
        .collect();
    by_age.sort_unstable();
    by_age
        .into_iter()
        .take(live.len() - ART_BUDGET)
        .map(|(_, id)| id.clone())
        .collect()
}

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

/// Accent mixed into an opaque face ([`crate::theme::card_face`]). Launcher is louder.
const FACE_TINT: f32 = 0.20;
const LAUNCHER_FACE_TINT: f32 = 0.38;

/// Own function so the contrast test asserts this colour, not a re-derived one.
fn placeholder_face(launcher: bool) -> Color4f {
    crate::theme::card_face(if launcher {
        LAUNCHER_FACE_TINT
    } else {
        FACE_TINT
    })
}

/// Coverless cell. Brand mark, else UI mark, else a monogram (launcher: its name). `None`: stale index.
/// `alpha` fades each piece on its own, so an entrance needs no layer per card: on a tiled GPU
/// each layer stores and reloads the whole framebuffer.
pub(crate) fn draw_poster_placeholder(
    canvas: &Canvas,
    fonts: &Fonts,
    game: Option<&LibraryGame>,
    rect: Rect,
    k: f64,
    alpha: f32,
) {
    let ink = |a: f32| {
        let c = fg(a);
        Color4f::new(c.r, c.g, c.b, c.a * alpha)
    };
    // Side cards overlap; glass shows the neighbour. `card_face` tints without alpha.
    let launcher = matches!(game, Some(g) if g.launcher);
    let face = placeholder_face(launcher);
    canvas.draw_rect(
        rect,
        &fill(Color4f::new(face.r, face.g, face.b, face.a * alpha)),
    );
    let Some(game) = game else { return };
    // ~44 % so the mark reads as a glyph, not a cropped cover; `launcher_mark` letterboxes.
    let mark = (!game.icon.is_empty())
        .then(|| {
            let side = rect.width().min(rect.height()) * 0.44;
            crate::launcher_icons::launcher_mark(
                &game.icon,
                Rect::from_xywh(
                    rect.left + (rect.width() - side) / 2.0,
                    rect.top + (rect.height() - side) / 2.0,
                    side,
                    side,
                ),
            )
        })
        .flatten();
    if let Some(path) = mark {
        canvas.draw_path(&path, &fill(ink(0.85)));
        return;
    }
    // Not a brand: the desktop tile names a Lucide mark. Stroked, not filled — Lucide's paths
    // are outlines, so filling one gives a blob.
    if let Some(icon) = crate::icons::by_name(&game.icon) {
        let side = rect.width().min(rect.height()) * 0.34;
        crate::icons::draw_icon(
            canvas,
            icon,
            rect.center_x(),
            rect.center_y(),
            side,
            ink(0.85),
        );
        return;
    }
    // Size off the card, not `k`: grid cells are two-thirds the shelf.
    let (glyph, size) = if game.launcher {
        (
            store_label(&game.store).to_string(),
            f64::from(rect.height()) * 0.067,
        )
    } else {
        (initials(&game.title), f64::from(rect.height()) * 0.115)
    };
    let font = fonts.font(W::Bold, size.max(9.0 * k));
    let tw = font.measure_str(&glyph, None).0;
    canvas.draw_str(
        &glyph,
        Point::new(
            rect.left + (rect.width() - tw) / 2.0,
            rect.center_y() + (size * 0.36) as f32,
        ),
        &font,
        &fill(ink(0.85)),
    );
}

/// Stream `h` itself, launching nothing — asking a host to launch what it is already
/// showing is how a second copy starts. The takeover names the running game when there
/// is one, the host otherwise; a pinned card's preset rides along.
fn desk_intent(h: &HostRow) -> ConnectIntent {
    let subject = if h.running.is_empty() {
        &h.name
    } else {
        &h.running
    };
    ConnectIntent {
        addr: h.addr.clone(),
        port: h.port,
        fp_hex: h.fp_hex.clone(),
        launch: None,
        title: match &h.pin {
            Some(p) => format!("{subject} \u{b7} {}", p.name),
            None => subject.clone(),
        },
        request_access: false,
        preset: h.pin.as_ref().map(|p| p.id.clone()),
    }
}

/// Write `library_sort` only. Screens re-read it each frame; assigning the field reverts.
pub(super) fn store_sort(sort: crate::collate::SortKey, ctx: &mut Ctx) {
    ctx.settings.library_sort = sort.id().to_string();
    ctx.store.save(ctx.settings);
}

/// Write `library_view`. Settings and this bar share the key; last write wins next frame.
fn store_view(view: LibraryView, ctx: &mut Ctx) {
    ctx.settings.library_view = view.id().to_string();
    ctx.store.save(ctx.settings);
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

    /// The row has focus (`true`) or has handed it down.
    /// OK went down: the plate dips under the focused poster.
    pub(crate) fn press(&mut self) {
        self.grid.get_mut().press();
    }

    pub(crate) fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
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

    pub(crate) fn title(&self) -> String {
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
            share_cover(&self.host.fp_hex, &id, self.art_k, &img);
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

    /// Titles to decode next: on screen last frame first, rows included, then the view from
    /// its first drawn title on, so a scroll decodes ahead. Capped well under
    /// [`ART_BUDGET`], or eviction and decode would chase each other round a long library.
    fn art_wanted(&self) -> Vec<String> {
        const AHEAD: usize = 48;
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
        for &g in self.view.iter().skip(first_seen.unwrap_or(0)) {
            if out.len() >= AHEAD {
                break;
            }
            let id = &self.games[g].id;
            if lacking(id) && !out.contains(id) {
                out.push(id.clone());
            }
        }
        out
    }

    /// The pad. Off the field, [`games`] routes it between lines; on it, the grid or the
    /// shelf moves its cursor. Under the Hosts row the home owns the lines.
    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
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

    /// A finger drag on the screen's scroll: the lines follow it and a lift flings them.
    pub(crate) fn pan(&mut self, p: Pointer) -> bool {
        let taken = self.grid.get_mut().drag(Id::new(GRID, 0), p);
        if taken && matches!(p.kind, PointerKind::PanStart { .. }) {
            self.follow = false;
        }
        taken
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

    /// This shelf's host itself ([`desk_intent`]).
    fn desktop_intent(&self) -> ConnectIntent {
        desk_intent(&self.host)
    }

    /// Launch `g` on this shelf's host. Pinned card: that preset as a one-off; primary
    /// tile: the host's default.
    fn launch_intent(&self, g: &LibraryGame) -> ConnectIntent {
        ConnectIntent {
            addr: self.host.addr.clone(),
            port: self.host.port,
            fp_hex: self.host.fp_hex.clone(),
            launch: Some(g.id.clone()),
            title: match &self.host.pin {
                Some(p) => format!("{} \u{b7} {}", g.title, p.name),
                None => g.title.clone(),
            },
            request_access: false,
            preset: self.host.pin.as_ref().map(|p| p.id.clone()),
        }
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

    /// Hover focuses; a press on the focused card launches, on another brings it to focus.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
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

    /// What a screen reader speaks: a pill and whether it is applied, the state's button,
    /// or the title the band shows. Nothing while a plain shelf is still loading.
    pub(crate) fn announcement(&self, ctx: &Ctx) -> Option<String> {
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

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
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

    pub(crate) fn render(
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
        for id in art_to_evict(&live, &self.art_seen) {
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
        let shelf = self.view_mode == LibraryView::Shelf && !self.embedded;
        let avail = f64::from(rect.width()) - 2.0 * edge(k);
        // The lines run on to the screen's foot and blur under the band there.
        let clip = canvas.local_clip_bounds().unwrap_or(rect);
        let foot = clip.bottom.max(rect.bottom);
        let tray = if self.band_shown() {
            crate::widgets::FOOT_TITLE_H * k
        } else {
            0.0
        };
        let band_top = f64::from(rect.bottom) - tray;
        let usable = band_top - f64::from(rect.top);
        let view_h = f64::from(foot - rect.top);

        let cols = self.grid_cols(rect, k);
        // Navigation reads last-drawn columns. A resize is a different grid; re-seat.
        if self.grid_cols_last != Some(cols) {
            self.grid_cols_last = Some(cols);
            self.seat_grid_col();
        }
        let shape = GridShape::new(self.len(), cols, self.lead_count());
        // Two-column clamp can overflow a narrow rect; shrink cells only, never headings.
        let fit = (avail / ((cols as f64 * (GRID_W + GRID_GAP) - GRID_GAP) * k)).clamp(0.25, 1.0);
        let (cw, ch) = (GRID_W * k * fit, GRID_H * k * fit);
        let card_h = ch + card::text_h(self.sort_captions()) * k;
        let pitch_x = cw + GRID_GAP * k * fit;
        let gap_y = ROW_GAP * k;
        let grid_w = cols as f64 * pitch_x - GRID_GAP * k * fit;
        let split_row = (shape.split > 0).then(|| shape.split_row());
        let heading_h = if self.embedded {
            EMBED_AIR
        } else {
            GRID_HEADING
        } * k;
        // Top inset is always on: it is also the air row 0 needs.
        let row_top = |row: usize| -> f64 {
            let section_gap = match split_row {
                Some(s) if row >= s => heading_h,
                _ => 0.0,
            };
            row as f64 * (card_h + gap_y) + heading_h + section_gap
        };
        let geo = shelf.then(|| self.shelf_geo(usable, avail, k));
        let block_h = |line: Line| -> f64 {
            match line {
                Line::Bar => (bar::BAR_H + BAR_AIR) * k,
                Line::Chips => Self::chips_h(k),
                Line::Band(b) => Self::band_h(&bands[b], ch, k),
                Line::Grid => match geo {
                    Some(g) => g.block,
                    None => row_top(shape.rows().saturating_sub(1)) + card_h + gap_y,
                },
                Line::State if sectioned => STATE_H * k,
                Line::State => usable,
            }
        };
        let mut tops = Vec::with_capacity(lines.len());
        let mut content_h = 0.0;
        for &l in &lines {
            tops.push(content_h);
            content_h += block_h(l);
        }
        // Matching bottom inset: the last row clears the band.
        let pad = heading_h + (f64::from(foot) - band_top);
        content_h += pad;
        let top_of = |l: Line| lines.iter().position(|&x| x == l).map_or(0.0, |i| tops[i]);
        let (focus_row, _) = shape.cell_of(self.cursor.max(0) as usize);
        // The focused item's span: a grid row (its heading too on row 0), or a whole line.
        let (item_top, item_h) = match (self.zone, geo) {
            (Zone::Grid, None) if focus_row > 0 => {
                (top_of(Line::Grid) + row_top(focus_row), card_h)
            }
            (Zone::Grid, None) => (top_of(Line::Grid), row_top(0) + card_h),
            (z, _) => {
                let line = games::line_of(z);
                (top_of(line), block_h(line))
            }
        };
        // Scroll only as far as it takes to show the item and a breath round it, above the
        // band; a line taller than the view shows its top. Never centred: on a phone that
        // walked the first row up over the host's verbs.
        let breath = REVEAL_AIR * k;
        let mut want = self.scroll.pos;
        if item_top + item_h + breath > want + usable {
            want = item_top + item_h + breath - usable;
        }
        if item_top - breath < want {
            want = item_top - breath;
        }
        let want = want.clamp(0.0, (content_h - view_h).max(0.0));
        let grid = Id::new(GRID, 0);
        let snap = std::mem::take(&mut self.snap_scroll);
        self.follow |= snap;
        self.step_bands(&bands, avail, cw, k, snap);
        let applied = self.applied();
        self.bar.step(fonts, k, avail, true, applied, dt, snap);
        let tree = self.grid.get_mut();
        tree.tick(dt as f32);
        // Follow focus while no finger has the scroll. After a pan the spring starts from
        // wherever the finger left it, so the next move glides instead of jumping.
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

        // The recoil moves the drawing on its axis, never the cells a pointer hits.
        let bump = self.bump.pos * k;
        let (bump_x, bump_y) = if self.bump_vertical {
            (0.0, bump)
        } else {
            (bump, 0.0)
        };
        // Room left of the first column for the plate's outset; the padding gives it back.
        // With a band to treat them, the lines run on up under the chrome as well.
        let air = PLATE_AIR * k;
        let bleed = crate::blur::active();
        let top = if bleed {
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
                Line::State => {
                    root.child(this.state_el(fonts, avail, block_h(Line::State), k, t, hits))
                }
            };
        }
        root = root.child(El::column().size(avail as f32, pad as f32));
        let mut tree = this.grid.borrow_mut();
        let frame = tree.layout(root, viewport);
        let field = match geo {
            Some(_) => shelf_cover(this.cursor.max(0) as usize),
            None => grid_cell(this.cursor.max(0) as usize),
        };
        tree.set_focus((!this.quiet).then(|| games::zone_id(this.zone, field)));
        let cheap = super::settings::reduce_ui_res(ctx.settings, ctx.platform, ctx.fallback_ui);
        if bleed {
            tree.paint_focus(canvas, frame, k as f32, dt, cheap);
        } else {
            let scrolled = (tree.offset(grid), frame.scroll(grid).map_or(0.0, |s| s.1));
            crate::widgets::soft_scroll(canvas, viewport, viewport, scrolled, k, || {
                tree.paint_focus(canvas, frame, k as f32, dt, cheap);
            });
        }
        drop(tree);

        // Hit rects are the cells as laid out; covers drawn this frame stay warm.
        let painted = painted.take();
        self.geom.clear();
        self.geom.resize(self.len(), Rect::new_empty());
        let tree = self.grid.get_mut();
        for &i in &painted {
            self.geom[i] = tree.rect(grid_cell(i)).unwrap_or_else(Rect::new_empty);
        }
        if let (Some(cards), Some(strip)) = (&shelf_cards, tree.rect(shelf_strip())) {
            for c in cards {
                self.geom[c.i] = c.bounds.with_offset((strip.left, strip.top));
            }
        }
        let seen: Vec<usize> = painted
            .into_iter()
            .chain(shelf_cards.iter().flatten().map(|c| c.i))
            .collect();
        for i in seen {
            if let Some(id) = self.game(i).map(|g| g.id.clone()) {
                self.art_seen.insert(id, self.frame);
            }
        }
        self.hits = hits.take();
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
mod tests {
    use super::*;
    use crate::library::POSTER_W;
    use crate::screens::Screen;

    #[test]
    fn a_cover_already_at_cache_size_decodes_here_with_mips() {
        let mut surface = skia_safe::surfaces::raster_n32_premul((60, 90)).unwrap();
        surface
            .canvas()
            .clear(skia_safe::Color::from_rgb(200, 40, 40));
        let png = surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap();
        let img = decode_poster(png.as_bytes(), 1.0).expect("decodes");
        assert_eq!((img.width(), img.height()), (60, 90));
        assert!(!img.is_lazy_generated());
        assert!(img.has_mipmaps());
    }

    fn host() -> HostRow {
        HostRow {
            key: "aa".into(),
            id: None,
            name: "Desk".into(),
            addr: "10.0.0.5".into(),
            port: 9777,
            fp_hex: "aa".into(),
            paired: true,
            saved: true,
            online: true,
            mgmt_port: 9778,
            can_wake: false,
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

    /// The coverflow arrangement, which these tests were written against.
    fn shelf_settings() -> pf_client_core::trust::Settings {
        pf_client_core::trust::Settings {
            library_view: LibraryView::Shelf.id().to_string(),
            ..Default::default()
        }
    }

    /// Live model + armed entrance, in the coverflow. `menu` re-syncs; a hand-built shelf
    /// is wiped. Titles are not A–Z, so a sort actually changes display order.
    fn live_shelf() -> (LibraryScreen, LibraryShared) {
        // Bar steps save settings; point the store at a throwaway HOME first.
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        library.set_games(
            ["Zeta", "Alpha", "Nimbus", "Bravo", "Kilo", "Delta"]
                .iter()
                .enumerate()
                .map(|(i, t)| LibraryGame {
                    id: format!("g{i}"),
                    title: (*t).to_string(),
                    store: "steam".into(),
                    launcher: false,
                    icon: String::new(),
                    platform: None,
                    developer: None,
                    year: None,
                    genres: Vec::new(),
                    stats: None,
                    running: false,
                })
                .collect(),
        );
        let mut s = LibraryScreen::new(&host());
        s.view_mode = LibraryView::Shelf;
        s.sync(&library);
        s.entrance_armed = true;
        (s, library)
    }

    fn ctx<'a>(
        library: &'a LibraryShared,
        settings: &'a mut pf_client_core::trust::Settings,
    ) -> Ctx<'a> {
        Ctx {
            hosts: &[],
            library,
            settings,
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
        }
    }

    fn press(
        s: &mut LibraryScreen,
        library: &LibraryShared,
        settings: &mut pf_client_core::trust::Settings,
        ev: MenuEvent,
    ) -> (Option<MenuPulse>, Outbox) {
        let mut fx = Outbox::default();
        let pulse = s.menu(ev, &mut ctx(library, settings), &mut fx);
        (pulse, fx)
    }

    fn hint_keys(
        s: &LibraryScreen,
        library: &LibraryShared,
        settings: &mut pf_client_core::trust::Settings,
    ) -> Vec<HintKey> {
        s.hints(&ctx(library, settings))
            .iter()
            .map(|h| h.key)
            .collect()
    }

    /// A drilled shelf's coverflow: its pills, then its field.
    fn plain_shelf() -> (LibraryScreen, LibraryShared) {
        let (mut s, library) = live_shelf();
        s.all_titles();
        (s, library)
    }

    /// A query keeps the titles containing it, any case; one that matches nothing says so
    /// on the state line instead of an empty field.
    #[test]
    fn a_search_keeps_only_the_matching_titles() {
        let (mut s, _library) = live_shelf();
        s.set_query("A");
        let titles: Vec<&str> = (0..s.len())
            .filter_map(|i| s.game(i).map(|g| g.title.as_str()))
            .collect();
        assert_eq!(titles.len(), 4, "{titles:?}");
        assert!(titles.iter().all(|t| t.to_lowercase().contains('a')));
        assert!(!s.no_match());

        let (mut s, _library) = live_shelf();
        s.set_query("xyz");
        assert!(s.no_match());
        assert!(s.lines(&[], 0).contains(&Line::State));
    }

    fn up() -> MenuEvent {
        MenuEvent::Move(MenuDir::Up)
    }

    fn down() -> MenuEvent {
        MenuEvent::Move(MenuDir::Down)
    }

    fn right() -> MenuEvent {
        MenuEvent::Move(MenuDir::Right)
    }

    /// Up from the field reaches the pills like any line. OK on a pill writes the setting
    /// and never reaches `ready_action`; Up past the pills is the edge, Down returns.
    #[test]
    fn the_pills_are_a_line_above_the_field() {
        let (mut s, library) = plain_shelf();
        let mut settings = shelf_settings();
        press(&mut s, &library, &mut settings, right());
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert_eq!(s.zone, Zone::Bar(0), "up from the shelf reaches the pills");

        let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(
            matches!(pulse, Some(MenuPulse::Boundary)),
            "Default is applied"
        );
        assert!(fx.connect.is_none() && fx.nav.is_none());
        press(&mut s, &library, &mut settings, right());
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(fx.connect.is_none(), "OK on a pill launched a game");
        assert!(fx.nav.is_none(), "and pushed a screen");
        assert_eq!(settings.library_sort, crate::collate::SortKey::Title.id());
        assert_eq!(s.zone, Zone::Bar(1), "the pill keeps the pad");

        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(
            matches!(pulse, Some(MenuPulse::Boundary)),
            "up past the pills is the edge the shell hands to the tabs"
        );
        press(&mut s, &library, &mut settings, down());
        assert_eq!(s.zone, Zone::Grid);
        s.adopt_settings(&ctx(&library, &mut settings));
        assert_eq!(s.sort, crate::collate::SortKey::Title);
        assert_eq!(
            s.game(1).map(|g| g.title.as_str()),
            Some("Alpha"),
            "the display order follows the sort"
        );
    }

    /// Walk to the VIEW group: Grid is one press of OK. Search ends the row, and OK there
    /// opens the search.
    #[test]
    fn the_view_pills_swap_the_arrangement() {
        let (mut s, library) = plain_shelf();
        let mut settings = shelf_settings();
        press(&mut s, &library, &mut settings, up());
        for _ in 0..8 {
            press(&mut s, &library, &mut settings, right());
        }
        assert_eq!(s.zone, Zone::Bar(8));
        let (pulse, _) = press(&mut s, &library, &mut settings, right());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)), "the last pill");
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(
            matches!(&fx.nav, Some(crate::screens::Nav::Push(b)) if matches!(**b, Screen::Search(_))),
            "Search opens its screen"
        );
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Left),
        );
        assert_eq!(s.zone, Zone::Bar(7));
        press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert_eq!(settings.library_view, LibraryView::Grid.id());
        s.adopt_settings(&ctx(&library, &mut settings));
        assert_eq!(s.view_mode, LibraryView::Grid);
        assert!(s.snap_scroll, "a new arrangement seats rather than glides");
        assert_eq!(s.applied(), [0, 7]);
    }

    /// A screen reader hears the tile it stepped onto, then the pill and whether it is
    /// the one applied.
    #[test]
    fn the_announcement_names_the_tile_then_the_pill() {
        let (mut s, library) = plain_shelf();
        let mut settings = shelf_settings();
        // The leading tile is the desktop, and it speaks the caption it draws.
        assert_eq!(
            s.announcement(&ctx(&library, &mut settings)).as_deref(),
            Some("Desktop")
        );
        press(&mut s, &library, &mut settings, right());
        assert_eq!(
            s.announcement(&ctx(&library, &mut settings)).as_deref(),
            Some("Zeta")
        );
        press(&mut s, &library, &mut settings, up());
        assert_eq!(
            s.announcement(&ctx(&library, &mut settings)).as_deref(),
            Some("Sort Default, selected")
        );
        press(&mut s, &library, &mut settings, right());
        assert_eq!(
            s.announcement(&ctx(&library, &mut settings)).as_deref(),
            Some("Sort A–Z")
        );
    }

    /// B on a pill leaves the screen, as it does from any other line.
    #[test]
    fn back_on_a_pill_leaves_like_any_line() {
        let (mut s, library) = plain_shelf();
        let mut settings = shelf_settings();
        press(&mut s, &library, &mut settings, up());
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Back);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    }

    /// On a plain grid, Up from row 0 reaches the pills; anywhere else it is a row move.
    #[test]
    fn up_reaches_the_pills_from_the_grids_top_row_only() {
        let (mut s, library) = plain_shelf();
        let mut settings = pf_client_core::trust::Settings {
            library_view: LibraryView::Grid.id().to_string(),
            ..Default::default()
        };
        // No test draws a frame; navigation reads this.
        s.grid_cols_last = Some(3);
        press(&mut s, &library, &mut settings, down());
        assert_eq!(s.view_mode, LibraryView::Grid);
        // The desktop tile is a one-tile lead band, so it owns row 0 and the games
        // section restarts at column 0 below it — the same split launchers get.
        assert_eq!(s.cursor, 1, "down moved a row");
        press(&mut s, &library, &mut settings, down());
        assert_eq!(s.cursor, 4, "and a second row inside the games section");

        press(&mut s, &library, &mut settings, up());
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert_eq!(s.zone, Zone::Grid, "up out of the second row is a row move");
        assert_eq!(s.cursor, 0);

        press(&mut s, &library, &mut settings, up());
        assert!(
            matches!(s.zone, Zone::Bar(_)),
            "…and from the top row it reaches the pills"
        );
    }

    /// The Games tab's lines: the desktop leaves the grid for its band, focus arrives on
    /// the band, Down enters the grid's top row, and above the band sit the chips, then
    /// the pills, then the edge. Every step is a direction.
    #[test]
    fn the_games_tab_walks_chips_bands_and_grid_as_lines() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings {
            library_view: LibraryView::Grid.id().to_string(),
            ..Default::default()
        };
        s.grid_cols_last = Some(3);
        press(&mut s, &library, &mut settings, down());
        assert_eq!(
            s.zone,
            Zone::Grid,
            "arrival on the Desktops band, Down into the grid"
        );
        assert_eq!(s.focused().map(|g| g.title.as_str()), Some("Zeta"));
        assert!(
            !s.view
                .iter()
                .any(|&i| s.games[i].id == crate::library::DESKTOP_ID),
            "the desktop tile left the grid for its band"
        );
        press(&mut s, &library, &mut settings, down());
        assert_eq!(s.cursor, 3, "inside the grid, Down is a row");
        press(&mut s, &library, &mut settings, up());
        press(&mut s, &library, &mut settings, up());
        assert_eq!(
            s.zone,
            Zone::Band { band: 0, item: 0 },
            "the top row hands up"
        );
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        let desk = fx.connect.expect("OK on a desktop tile streams");
        assert_eq!(desk.launch, None, "and launches nothing");
        press(&mut s, &library, &mut settings, up());
        assert_eq!(s.zone, Zone::Chip(0));
        press(&mut s, &library, &mut settings, up());
        assert!(
            matches!(s.zone, Zone::Bar(_)),
            "Up from the chips reaches the pills"
        );
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)), "then the tabs");
        press(&mut s, &library, &mut settings, down());
        assert_eq!(s.zone, Zone::Chip(0), "and Down comes back");
        // No other paired host here, so the only chip is Customize.
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(matches!(
            fx.nav,
            Some(crate::screens::Nav::Push(ref screen)) if matches!(**screen, Screen::Customize(_))
        ));
    }

    /// The shelf is the Games section's other arrangement: the tab keeps its rows, and the
    /// coverflow is one line — Left and Right walk it, Up and Down leave it.
    #[test]
    fn the_shelf_is_one_line_among_the_tabs_rows() {
        let (mut s, library) = live_shelf();
        let mut settings = shelf_settings();
        press(&mut s, &library, &mut settings, down());
        assert_eq!(
            s.zone,
            Zone::Grid,
            "Down from the Desktops band enters the shelf"
        );
        press(&mut s, &library, &mut settings, right());
        assert_eq!(s.cursor, 1, "Right walks the covers");
        let (pulse, _) = press(&mut s, &library, &mut settings, down());
        assert!(
            matches!(pulse, Some(MenuPulse::Boundary)),
            "nothing below the shelf"
        );
        press(&mut s, &library, &mut settings, up());
        assert_eq!(s.zone, Zone::Band { band: 0, item: 0 });
        assert_eq!(s.cursor, 1, "and the shelf keeps its place");
    }

    /// A failed list keeps the chips and the Desktops row, with Retry where the field was.
    /// Focus lands on Retry; Up walks to the tabs. A drilled shelf has only the button.
    #[test]
    fn a_failed_list_offers_retry_and_a_way_up() {
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        library.set_phase(LibraryPhase::Error {
            title: "Couldn't load the library".into(),
            body: "The host refused.".into(),
            can_retry: true,
        });
        let mut settings = pf_client_core::trust::Settings::default();
        let mut s = LibraryScreen::new(&host());
        let (pulse, _) = press(&mut s, &library, &mut settings, right());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(s.zone, Zone::State, "focus lands on Retry");
        assert!(hint_keys(&s, &library, &mut settings).contains(&HintKey::Confirm));
        press(&mut s, &library, &mut settings, up());
        assert_eq!(
            s.zone,
            Zone::Band { band: 0, item: 0 },
            "the desk still streams"
        );
        press(&mut s, &library, &mut settings, up());
        assert_eq!(s.zone, Zone::Chip(0));
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(
            matches!(pulse, Some(MenuPulse::Boundary)),
            "the tabs are one more Up"
        );
        press(&mut s, &library, &mut settings, down());
        press(&mut s, &library, &mut settings, down());
        let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(matches!(pulse, Some(MenuPulse::Confirm)));
        assert!(
            matches!(fx.cmds.as_slice(), [ConsoleCmd::FetchLibrary { .. }]),
            "Retry fetches again"
        );

        let mut s = LibraryScreen::new(&host());
        s.all_titles();
        library.set_phase(LibraryPhase::Error {
            title: "Couldn't load the library".into(),
            body: "The host refused.".into(),
            can_retry: true,
        });
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(s.zone, Zone::State);
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(!fx.cmds.is_empty());
    }

    /// While the list loads the tab's Desktops row holds focus, and Up still leaves.
    #[test]
    fn a_loading_list_keeps_focus_on_the_desktops_row() {
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        let mut settings = pf_client_core::trust::Settings::default();
        let mut s = LibraryScreen::new(&host());
        let (pulse, _) = press(&mut s, &library, &mut settings, down());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(s.zone, Zone::Band { band: 0, item: 0 });
        press(&mut s, &library, &mut settings, up());
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(s.zone, Zone::Chip(0));
    }

    /// A band holds its titles out of the grid only while it shows; Recently played is
    /// newest first and skips what was never played.
    #[test]
    fn a_band_holds_its_titles_out_of_the_grid_only_while_it_shows() {
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        let mut list = games(&[
            ("Steam", None),
            ("Old", None),
            ("New", None),
            ("Never", None),
        ]);
        list[0].launcher = true;
        let played = |at| {
            Some(pf_client_core::library::GameStats {
                last_played_unix_ms: at,
                ..Default::default()
            })
        };
        list[1].stats = played(5);
        list[2].stats = played(9);
        library.set_games(list);
        let mut s = LibraryScreen::new(&host());
        s.sync(&library);
        let mut settings = pf_client_core::trust::Settings::default();
        let titles = |s: &LibraryScreen, band: &games::Band| -> Vec<String> {
            (band.items.iter())
                .map(|it| match it {
                    games::Item::Game(i) => s.games[*i].title.clone(),
                    games::Item::Desktop(h) => h.name.clone(),
                    games::Item::Collection(c) => s.collections[*c].label.clone(),
                })
                .collect()
        };
        s.adopt_settings(&ctx(&library, &mut settings));
        let (bands, before) = s.bands(&ctx(&library, &mut settings));
        let sections: Vec<_> = bands.iter().map(|b| b.section).collect();
        use crate::library::Section;
        assert_eq!(
            sections,
            vec![Section::Desktops, Section::Recent, Section::Launchers]
        );
        assert_eq!(
            before, 3,
            "all three sit above the grid, favorites hidden empty"
        );
        assert_eq!(titles(&s, &bands[1]), vec!["New", "Old"]);
        assert!(!s.view.iter().any(|&i| s.games[i].launcher));

        settings.library_sections = "-launchers".into();
        s.adopt_settings(&ctx(&library, &mut settings));
        let (bands, _) = s.bands(&ctx(&library, &mut settings));
        assert!(bands.iter().all(|b| b.section != Section::Launchers));
        assert!(
            s.view.iter().any(|&i| s.games[i].launcher),
            "switched off, the launchers rejoin the grid"
        );
    }

    #[test]
    fn the_legend_swaps_with_the_focus() {
        let (mut s, library) = plain_shelf();
        let mut settings = shelf_settings();
        let field = hint_keys(&s, &library, &mut settings);
        assert!(field.contains(&HintKey::Confirm) && field.contains(&HintKey::Secondary));
        assert!(
            !field.contains(&HintKey::Up),
            "the pills are in sight, not a hint"
        );

        press(&mut s, &library, &mut settings, up());
        let pills = hint_keys(&s, &library, &mut settings);
        assert!(pills.contains(&HintKey::Confirm) && pills.contains(&HintKey::Back));
        assert!(
            !pills.contains(&HintKey::Secondary),
            "the pills' legend still offered the field's options"
        );
        assert!(
            pills.len() <= 4,
            "the legend is read from a sofa: {} entries",
            pills.len()
        );
    }

    /// A plain shelf in its art wait draws a spinner, so it offers no pills to walk onto.
    #[test]
    fn a_plain_shelf_offers_no_pills_until_its_field_shows() {
        let (mut s, library) = plain_shelf();
        s.entrance_armed = false;
        let mut settings = shelf_settings();
        let (pulse, _) = press(&mut s, &library, &mut settings, up());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(s.zone, Zone::Grid);
    }

    /// List landed, no art yet.
    fn waiting_shelf() -> LibraryScreen {
        let mut s = LibraryScreen::new(&host());
        s.phase = LibraryPhase::Ready;
        s.games = (0..6)
            .map(|i| LibraryGame {
                id: format!("g{i}"),
                title: format!("Game {i}"),
                store: "steam".into(),
                launcher: false,
                icon: String::new(),
                platform: None,
                developer: None,
                year: None,
                genres: Vec::new(),
                stats: None,
                running: false,
            })
            .collect();
        s.recollate();
        s
    }

    /// Unarmed `None` is not settled. First draw is entrance frame 1 (fade 0).
    #[test]
    fn a_shelf_is_hidden_until_its_entrance_begins_not_settled() {
        let mut s = waiting_shelf();
        s.arm_entrance(10.0);
        assert!(!s.entrance_armed, "armed with nothing to show");
        for steps in 0..s.len() {
            let at = s.entrance_at(steps, 10.0);
            assert_eq!(
                (at.travel, at.fade),
                (0.0, 0.0),
                "card {steps} was drawn before the entrance began"
            );
        }
        s.arm_entrance(10.39);
        assert!(!s.entrance_armed, "the deadline is 400 ms");

        s.arm_entrance(10.4);
        assert!(s.entrance_armed);
        assert_eq!(s.entrance_at(0, 10.4).fade, 0.0, "the shelf flashed");

        s.entrance = None;
        assert_eq!(s.entrance_at(3, 99.0), EntranceAt::SETTLED);
    }

    #[test]
    fn decoded_art_arms_the_entrance_without_waiting_out_the_deadline() {
        let mut s = waiting_shelf();
        s.arm_entrance(4.0);
        assert!(!s.entrance_armed);
        let mut surface = skia_safe::surfaces::raster_n32_premul((2, 2)).expect("2×2 raster");
        s.art.insert("g1".into(), surface.image_snapshot());
        s.arm_entrance(4.05);
        assert!(s.entrance_armed, "a decoded poster is the whole point");
    }

    fn over(src: Color4f, dst: Color4f) -> Color4f {
        let m = |s: f32, d: f32| s * src.a + d * (1.0 - src.a);
        Color4f::new(m(src.r, dst.r), m(src.g, dst.g), m(src.b, dst.b), 1.0)
    }

    /// WCAG contrast: sRGB → linear, Rec. 709 luminance.
    fn contrast(a: Color4f, b: Color4f) -> f32 {
        let lin = |c: f32| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        let lum = |c: Color4f| 0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b);
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// Coverless monogram vs face must contrast on every palette. Side cards overlap: alpha leaks.
    #[test]
    fn a_coverless_card_reads_on_every_palette() {
        for p in &crate::library::PALETTES {
            crate::theme::set_ink(crate::theme::Ink::of(p));
            for launcher in [false, true] {
                let face = placeholder_face(launcher);
                assert_eq!(face.a, 1.0, "{} face is translucent", p.id);
                let c = contrast(over(fg(0.85), face), face);
                assert!(c > 3.0, "the monogram is unreadable on {}: {c:.2}:1", p.id);
            }
        }
        crate::theme::set_ink(crate::theme::Ink::of(crate::library::palette("violet")));
    }

    /// Stamp after draw. Arrival-order LRU drops the neighbourhood the cursor is in.
    #[test]
    fn eviction_drops_the_coldest_and_keeps_the_focused_neighbourhood() {
        let live: Vec<String> = (0..ART_BUDGET + 40).map(|i| format!("g{i}")).collect();
        let mut seen = HashMap::new();
        for id in &live {
            seen.insert(id.clone(), 10u64);
        }
        let hot: Vec<String> = (100..112).map(|i| format!("g{i}")).collect();
        for id in &hot {
            seen.insert(id.clone(), 9_000);
        }
        let dropped = art_to_evict(&live, &seen);
        assert_eq!(dropped.len(), 40, "trimmed back to exactly the budget");
        for id in &hot {
            assert!(!dropped.contains(id), "{id} was on screen and got evicted");
        }
    }

    #[test]
    fn eviction_does_nothing_under_the_budget() {
        let live: Vec<String> = (0..ART_BUDGET).map(|i| format!("g{i}")).collect();
        assert!(art_to_evict(&live, &HashMap::new()).is_empty());
    }

    /// Cache ≥ focused shelf card, ≤ source, source aspect. Smaller magnifies; larger wastes GPU.
    #[test]
    fn a_cached_poster_is_bounded_by_the_size_it_is_drawn_at() {
        for k in [0.75, 1.0, 1.25, 1.5, 1.8, 2.7, 3.0] {
            for src in [(600, 900), (1000, 1500), (460, 215), (300, 450), (64, 64)] {
                let (w, h) = art_cache_size(src, k);
                assert!(
                    w <= src.0 && h <= src.1,
                    "{src:?} at k={k} was enlarged to {w}×{h}"
                );
                let drawn = (POSTER_W * k).min(f64::from(src.0));
                assert!(
                    f64::from(w) >= drawn - 1.0,
                    "{src:?} at k={k} cached {w} px wide, under the {drawn:.0} px drawn"
                );
                assert!(
                    f64::from(w) <= ART_CACHE_W * k + 1.0 && f64::from(h) <= ART_CACHE_H * k + 1.0,
                    "{src:?} at k={k} cached {w}×{h}, outside the cache box"
                );
                let (want, got) = (
                    f64::from(src.0) / f64::from(src.1),
                    f64::from(w) / f64::from(h),
                );
                assert!(
                    (want - got).abs() < 0.02,
                    "{src:?} at k={k} came back {w}×{h} — aspect {got:.3} for a {want:.3} source"
                );
            }
        }
    }

    /// 8-col clamp × ~3 visible rows; 6 is generous. `k` is height/800, so row count is stable.
    const SCREENFUL: usize = 8 * 6;

    /// What one screen of covers plus the render targets holds at `k`, bytes.
    fn screenful_bytes(src: (i32, i32), k: f64) -> usize {
        // RGBA + 1/3 for the mip chain `decode_poster` bakes.
        let bytes = |(w, h): (i32, i32)| (w as usize) * (h as usize) * 4 * 4 / 3;
        bytes(art_cache_size(src, k)) * SCREENFUL
            + 2 * (1280.0 * k) as usize * (800.0 * k) as usize * 4
    }

    /// Over budget, Skia purges and the next frame re-decodes JPEG on the render thread.
    #[test]
    fn a_screenful_of_covers_fits_the_gpu_budget() {
        // Through 1440p. 4K + 1000×1500 art is outside `DEFAULT_GPU_CACHE_BYTES`.
        for k in [0.75, 1.0, 1.35, 1.8] {
            for src in [(600, 900), (1000, 1500)] {
                let need = screenful_bytes(src, k);
                let budget = crate::shell::DEFAULT_GPU_CACHE_BYTES;
                assert!(
                    need < budget,
                    "{src:?} art at k={k}: {} MB needed over a {} MB budget",
                    need >> 20,
                    budget >> 20
                );
            }
        }
    }

    /// The floor a memory-tight host may pass, pinned to what 1080p actually needs.
    ///
    /// A television is `k` 1.35, and this is the number a TV client copies instead of
    /// guessing a fraction of the desktop default — a guess that put webOS on 64 MB,
    /// under the working set, re-decoding covers on the render thread every frame.
    #[test]
    fn the_gpu_budget_floor_covers_a_1080p_screenful() {
        for src in [(600, 900), (1000, 1500)] {
            let need = screenful_bytes(src, 1.35);
            let floor = crate::shell::MIN_GPU_CACHE_BYTES;
            assert!(
                need < floor,
                "{src:?} art at 1080p: {} MB needed under a {} MB floor",
                need >> 20,
                floor >> 20
            );
        }
    }

    #[test]
    fn never_drawn_posters_are_evicted_before_merely_old_ones() {
        let live: Vec<String> = (0..ART_BUDGET + 2).map(|i| format!("g{i}")).collect();
        let mut seen: HashMap<String, u64> = live.iter().map(|id| (id.clone(), 5)).collect();
        seen.remove("g7");
        seen.remove("g9");
        let dropped = art_to_evict(&live, &seen);
        assert_eq!(dropped.len(), 2);
        assert!(dropped.contains(&"g7".to_string()));
        assert!(dropped.contains(&"g9".to_string()));
    }

    /// Platform-less Steam collates as store: `[None, None]` is one collection, `[Some, None]` two.
    /// 200 titles in the grid, the desktop tile's band above them.
    fn long_grid() -> (
        LibraryScreen,
        LibraryShared,
        pf_client_core::trust::Settings,
    ) {
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        let titles: Vec<String> = (0..200).map(|i| format!("Title {i:03}")).collect();
        let spec: Vec<(&str, Option<&str>)> = titles.iter().map(|t| (t.as_str(), None)).collect();
        library.set_games(games(&spec));
        let mut s = LibraryScreen::new(&host());
        // A drilled shelf's plain grid: no sections above.
        s.all_titles();
        s.sync(&library);
        s.entrance_armed = true;
        let settings = pf_client_core::trust::Settings {
            library_view: LibraryView::Grid.id().to_string(),
            ..Default::default()
        };
        (s, library, settings)
    }

    /// Frames at 60 Hz; the drawn cells' indices. `PF_GRID_DUMP=<dir>` also writes the frame.
    fn grid_frames(
        s: &mut LibraryScreen,
        library: &LibraryShared,
        settings: &mut pf_client_core::trust::Settings,
        frames: usize,
        name: &str,
    ) -> Vec<usize> {
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 1280.0, 800.0);
        for _ in 0..frames {
            surface
                .canvas()
                .clear(skia_safe::Color4f::new(0.0, 0.0, 0.0, 1.0));
            s.render(
                surface.canvas(),
                rect,
                1.0,
                1.0 / 60.0,
                &fonts,
                &mut ctx(library, settings),
            );
        }
        if let Ok(dir) = std::env::var("PF_GRID_DUMP") {
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, None)
                .expect("png");
            std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).expect("write");
        }
        (0..s.geom.len())
            .filter(|i| !s.geom[*i].is_empty())
            .collect()
    }

    #[test]
    fn a_long_grid_draws_only_the_rows_in_view() {
        let (mut s, library, mut settings) = long_grid();
        let drawn = grid_frames(&mut s, &library, &mut settings, 60, "grid-top");
        assert_eq!(drawn.first(), Some(&0), "the desktop tile");
        assert!(drawn.len() < 40, "a screenful, not the library: {drawn:?}");
        assert!(!drawn.contains(&200));

        s.cursor = 57;
        s.seat_grid_col();
        let drawn = grid_frames(&mut s, &library, &mut settings, 90, "grid-mid");
        assert!(drawn.contains(&57) && !drawn.contains(&0), "{drawn:?}");

        s.cursor = 200;
        s.seat_grid_col();
        let drawn = grid_frames(&mut s, &library, &mut settings, 90, "grid-end");
        assert_eq!(drawn.last(), Some(&200), "the last title in view");
        assert!(drawn.len() < 40, "{drawn:?}");
    }

    /// A finger drags the grid one to one and a flick carries on; the pad's next move
    /// brings the focus row back into view.
    #[test]
    fn a_finger_pans_the_grid_until_the_pad_moves_focus() {
        let (mut s, library, mut settings) = long_grid();
        grid_frames(&mut s, &library, &mut settings, 30, "pan-0");
        let offset = |s: &LibraryScreen| s.grid.borrow().offset(Id::new(GRID, 0));
        // Title 3 sits in the grid's second row, on screen.
        let cell = s.geom[3];
        assert!(!cell.is_empty());
        let finger = |kind| Pointer {
            x: f64::from(cell.center_x()),
            y: f64::from(cell.center_y()),
            kind,
        };
        assert!(s.pan(finger(PointerKind::PanStart { horizontal: false })));
        assert!(s.pan(finger(PointerKind::Pan {
            dx: 0.0,
            dy: -120.0
        })));
        grid_frames(&mut s, &library, &mut settings, 1, "pan-1");
        assert_eq!(offset(&s), 120.0);
        assert_eq!(
            s.geom[3].top,
            cell.top - 120.0,
            "the cards follow the finger"
        );

        assert!(s.pan(finger(PointerKind::Fling {
            vx: 0.0,
            vy: -1500.0
        })));
        grid_frames(&mut s, &library, &mut settings, 120, "pan-2");
        assert!(offset(&s) > 500.0, "the flick carried on: {}", offset(&s));
        assert!(
            s.geom[0].is_empty() || s.geom[0].bottom <= 0.0,
            "the focus row is left behind"
        );

        let mut fx = Outbox::default();
        s.menu(
            MenuEvent::Move(MenuDir::Down),
            &mut ctx(&library, &mut settings),
            &mut fx,
        );
        grid_frames(&mut s, &library, &mut settings, 120, "pan-3");
        let focus = s.cursor as usize;
        assert!(!s.geom[focus].is_empty(), "focus {focus} back in view");
    }

    fn games(spec: &[(&str, Option<&str>)]) -> Vec<LibraryGame> {
        spec.iter()
            .enumerate()
            .map(|(i, (title, platform))| LibraryGame {
                id: format!("g{i}"),
                title: (*title).to_string(),
                store: "steam".into(),
                launcher: false,
                icon: String::new(),
                platform: platform.map(str::to_string),
                developer: None,
                year: None,
                genres: Vec::new(),
                stats: None,
                running: false,
            })
            .collect()
    }

    /// First list is always "fresh" on an empty screen; adopted art must survive that, not a later one.
    #[test]
    fn handed_over_posters_survive_the_first_list_and_only_that_one() {
        let poster = || {
            skia_safe::surfaces::raster_n32_premul((4, 6))
                .expect("a raster surface")
                .image_snapshot()
        };
        let library = LibraryShared::default();
        library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
        let mut s = LibraryScreen::new(&host());
        s.adopt_art(HashMap::from([("g0".to_string(), poster())]));
        s.sync(&library);
        assert_eq!(s.art.len(), 1, "the hand-over was wiped by the first list");

        library.set_games(games(&[("Rez", Some("PS2"))]));
        s.sync(&library);
        assert!(s.art.is_empty(), "a different library kept the old covers");
    }

    /// The tile is the host, not a title: Confirm streams it with no launch id, and Y
    /// opens the HOST's options — a title menu here would offer a per-title preset
    /// binding for a title that does not exist.
    #[test]
    fn confirm_on_the_desktop_tile_launches_nothing() {
        let (mut s, library) = plain_shelf();
        let mut settings = shelf_settings();
        assert_eq!(
            s.focused().map(|g| g.id.as_str()),
            Some(crate::library::DESKTOP_ID),
            "the tile is where the cursor starts"
        );

        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        let intent = fx.connect.expect("a connect intent");
        assert_eq!(intent.launch, None, "the desktop tile launched a title");
        assert_eq!(intent.title, "Desk", "the takeover names the host");

        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Secondary);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(_))),
            "Y on the tile opens the host's options"
        );
    }

    /// A host with no plugins is still one press from its desk. The catalog's verdict
    /// stands — the empty copy is what explains the missing shelf.
    #[test]
    fn an_empty_library_still_streams_the_desktop() {
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        library.set_games(Vec::new());
        let mut s = LibraryScreen::new(&host());
        s.sync(&library);
        s.entrance_armed = true;
        assert!(matches!(s.phase, LibraryPhase::Empty));

        let mut settings = shelf_settings();
        let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(matches!(pulse, Some(MenuPulse::Confirm)));
        let intent = fx.connect.expect("a connect intent");
        assert_eq!(intent.launch, None);
        assert_eq!(intent.addr, "10.0.0.5");
        assert!(
            hint_keys(&s, &library, &mut settings).contains(&HintKey::Confirm),
            "the legend must offer the press that works"
        );
    }

    /// The caption is the verb: it names the game the host already has up, so the tile
    /// does not read "Desktop" while pressing it resumes Elden Ring.
    #[test]
    fn the_desktop_tile_says_resume_when_the_host_has_a_game_up() {
        let busy = HostRow {
            running: "Elden Ring".into(),
            ..host()
        };
        let s = LibraryScreen::new(&busy);
        assert_eq!(s.desktop_caption(), "Resume Elden Ring");
        assert_eq!(s.desktop_intent().title, "Elden Ring");

        let idle = LibraryScreen::new(&host());
        assert_eq!(idle.desktop_caption(), "Desktop");
        assert_eq!(idle.desktop_intent().title, "Desk");
    }

    /// A cover one screen took off the decoded queue still reaches a second screen: its bytes
    /// stay in the model, and the second screen decodes them for itself.
    #[test]
    fn a_cover_one_screen_took_reaches_another() {
        let library = LibraryShared::default();
        library.set_games(games(&[("Alpha", None)]));
        let bytes = poster_png(1);
        library.push_art("g0".into(), bytes.clone());
        library.push_decoded("g0".into(), decode_poster_off_thread(&bytes, 1.0).unwrap());
        let mut first = LibraryScreen::new(&host());
        let mut second = LibraryScreen::new(&host());
        first.sync(&library);
        assert!(
            first.art.contains_key("g0"),
            "the first takes the decoded cover"
        );
        second.sync(&library);
        second.sync(&library);
        assert!(
            second.art.contains_key("g0"),
            "the second decodes the bytes"
        );
    }

    /// Two screens on one host share a cover: the second takes the first's image, with no
    /// bytes to decode from and nothing sent to its decoder.
    #[test]
    fn screens_on_one_host_share_a_cover() {
        let library = LibraryShared::default();
        library.set_games(games(&[("Alpha", None)]));
        let poster = decode_poster_off_thread(&poster_png(2), 1.0).unwrap();
        library.push_decoded("g0".into(), poster);
        let mut first = LibraryScreen::new(&host());
        let mut second = LibraryScreen::new(&host());
        first.sync(&library);
        second.sync(&library);
        assert!(second.art.contains_key("g0"), "shared, not decoded");
        assert!(!second.decoder.pending("g0"));
    }

    /// A 2:3 poster, PNG-encoded: a two-hue gradient with a pale disc, colour from `seed`.
    fn poster_png(seed: usize) -> Vec<u8> {
        let hues = [
            (0.85, 0.30, 0.35),
            (0.25, 0.50, 0.85),
            (0.30, 0.72, 0.45),
            (0.88, 0.62, 0.22),
            (0.55, 0.30, 0.80),
            (0.20, 0.70, 0.75),
        ];
        let (a, b) = (hues[seed % hues.len()], hues[(seed + 2) % hues.len()]);
        let mut surface = skia_safe::surfaces::raster_n32_premul((300, 450)).unwrap();
        let canvas = surface.canvas();
        let mut p = crate::theme::shaded();
        p.set_shader(skia_safe::gradient::shaders::linear_gradient(
            (Point::new(0.0, 0.0), Point::new(300.0, 450.0)),
            &skia_safe::gradient::Gradient::new(
                skia_safe::gradient::Colors::new_evenly_spaced(
                    &[
                        Color4f::new(a.0, a.1, a.2, 1.0),
                        Color4f::new(b.0 * 0.4, b.1 * 0.4, b.2 * 0.4, 1.0),
                    ],
                    skia_safe::TileMode::Clamp,
                    None,
                ),
                skia_safe::gradient::Interpolation::default(),
            ),
            None,
        ));
        canvas.draw_rect(Rect::from_wh(300.0, 450.0), &p);
        let disc = Color4f::new(1.0, 1.0, 1.0, 0.35);
        canvas.draw_circle((150.0, 170.0 + (seed % 3) as f32 * 30.0), 80.0, &fill(disc));
        surface
            .image_snapshot()
            .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
            .unwrap()
            .as_bytes()
            .to_vec()
    }

    /// Ignored eyeball dump of the Games tab, `PF_CONSOLE_DUMP=<dir> cargo test -p
    /// pf-console-ui --release --lib -- --ignored dump_library`: the rows, the pills, the
    /// shelf, the three list states, the Collections row, and a phone.
    #[test]
    #[ignore]
    fn dump_library() {
        use crate::shell::{ConsoleOptions, Shell};
        let dir = std::env::var("PF_CONSOLE_DUMP").expect("set PF_CONSOLE_DUMP to an output dir");
        crate::screens::settings::tests::fake_home();
        let fonts = crate::theme::build_fonts().unwrap();
        let hosts = || {
            let one = HostRow {
                key: "aa11".into(),
                name: "Living Room PC".into(),
                fp_hex: "aa11".into(),
                os: "windows".into(),
                ..host()
            };
            let two = HostRow {
                key: "bb22".into(),
                name: "Office Tower".into(),
                fp_hex: "bb22".into(),
                os: "fedora".into(),
                online: false,
                running: "Hades II".into(),
                ..host()
            };
            vec![one, two]
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let titles = [
            "Steam",
            "Hades II",
            "Elden Ring",
            "Hollow Knight",
            "Celeste",
            "Tunic",
            "Baldur's Gate 3",
            "Deep Rock Galactic",
            "Portal 2",
            "Outer Wilds",
            "Disco Elysium",
            "Stardew Valley",
            "Cyberpunk 2077",
            "Inside",
        ];
        let list = || -> Vec<LibraryGame> {
            let mut list: Vec<LibraryGame> = (titles.iter().enumerate())
                .map(|(i, t)| LibraryGame {
                    id: format!("steam:{i}"),
                    title: (*t).to_string(),
                    store: if i % 5 == 3 { "epic" } else { "steam" }.into(),
                    launcher: i == 0,
                    icon: if i == 0 {
                        "steam".into()
                    } else {
                        String::new()
                    },
                    platform: (i % 4 == 2).then(|| "PC".to_string()),
                    developer: None,
                    year: None,
                    genres: Vec::new(),
                    stats: None,
                    running: i == 1,
                })
                .collect();
            for (i, hours) in [(2, 2), (3, 30), (5, 80)] {
                list[i].stats = Some(pf_client_core::library::GameStats {
                    last_played_unix_ms: now - hours * 3_600_000,
                    play_time_ms: hours * 1_900_000,
                    ..Default::default()
                });
            }
            list
        };
        let shell = |settings: pf_client_core::trust::Settings, library: &LibraryShared, stack| {
            let console = crate::model::ConsoleShared::default();
            console.set_hosts(hosts());
            let mut opts = ConsoleOptions::desktop("deck".into(), false);
            opts.store = Some(std::sync::Arc::new(crate::store::SnapshotStore::new(
                settings,
                Vec::new(),
            )));
            let bus = crate::model::ConsoleBus::default();
            let mut s = Shell::new(console, library.clone(), bus, opts, stack).unwrap();
            s.fake_clock = Some((0.0, 1.0 / 60.0));
            s
        };
        let dump = |s: &mut Shell, frames: usize, name: &str| {
            let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
            for _ in 0..frames {
                s.render(
                    surface.canvas(),
                    1280,
                    800,
                    &fonts,
                    Some("Xbox Wireless Controller"),
                    Some(punktfunk_core::config::GamepadPref::Xbox360),
                    &[],
                );
            }
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
        };
        let tab = |view: LibraryView, sort: &str, palette: &str, library: &LibraryShared| {
            let mut settings = pf_client_core::trust::Settings {
                library_view: view.id().to_string(),
                library_sort: sort.to_string(),
                ui_palette: palette.to_string(),
                ..Default::default()
            };
            crate::library::toggle_favorite(&mut settings, "aa11", "steam:4");
            let root = Screen::Library(LibraryScreen::new(&hosts()[0]));
            shell(settings, library, vec![root])
        };
        let games_tab =
            |view: LibraryView, library: &LibraryShared| tab(view, "", "violet", library);
        let full = || {
            let library = LibraryShared::default();
            library.set_games(list());
            for i in 1..titles.len() {
                if i != 6 {
                    library.push_art(format!("steam:{i}"), poster_png(i));
                }
            }
            library
        };
        let menu = |s: &mut Shell, evs: &[MenuEvent]| {
            for ev in evs {
                s.handle_menu(*ev);
            }
        };

        let library = full();
        let mut s = games_tab(LibraryView::Grid, &library);
        dump(&mut s, 90, "L1-games-arrival");
        menu(&mut s, &[down(), down(), down()]);
        dump(&mut s, 50, "L2-games-favorites");
        menu(&mut s, &[down(), down(), right()]);
        dump(&mut s, 50, "L3-games-grid");
        // Under the Recent sort each card captions when it was played; a pale palette.
        let mut s = tab(LibraryView::Grid, "recent", "sky", &full());
        dump(&mut s, 60, "_settle");
        menu(&mut s, &[down(), down(), down(), down()]);
        dump(&mut s, 50, "L3b-games-recent-mint");
        let mut s = games_tab(LibraryView::Grid, &full());
        dump(&mut s, 60, "_settle");
        menu(&mut s, &[up(), up(), right()]);
        dump(&mut s, 12, "L4-games-pills-travel");
        dump(&mut s, 50, "L4-games-pills");

        let mut s = games_tab(LibraryView::Shelf, &full());
        dump(&mut s, 60, "_settle");
        menu(&mut s, &[down(), down(), down(), down(), right(), right()]);
        dump(&mut s, 60, "L5-games-shelf");

        for (name, phase) in [
            (
                "L6-games-error",
                Some(LibraryPhase::Error {
                    title: "Couldn't load the library".into(),
                    body: "The host didn't answer. Check that it is awake and on this network."
                        .into(),
                    can_retry: true,
                }),
            ),
            ("L7-games-loading", None),
            ("L8-games-empty", Some(LibraryPhase::Empty)),
        ] {
            let library = LibraryShared::default();
            match phase {
                Some(LibraryPhase::Empty) => library.set_games(Vec::new()),
                Some(p) => library.set_phase(p),
                None => {}
            }
            let mut s = games_tab(LibraryView::Grid, &library);
            dump(&mut s, 60, name);
        }

        // Drilled: the pills and a coverflow of the whole library.
        let settings = pf_client_core::trust::Settings {
            library_view: LibraryView::Shelf.id().to_string(),
            ..Default::default()
        };
        let mut drilled = LibraryScreen::new(&hosts()[0]);
        drilled.all_titles();
        let root = Screen::Library(LibraryScreen::new(&hosts()[0]));
        let mut s = shell(settings, &full(), vec![root, Screen::Library(drilled)]);
        dump(&mut s, 60, "_settle");
        menu(&mut s, &[right(), right(), right()]);
        dump(&mut s, 60, "L9-shelf-drilled");

        let mixed = LibraryShared::default();
        let mut games = list();
        for (i, g) in games.iter_mut().enumerate() {
            g.platform = Some(["PC", "PS2", "SNES"][i % 3].into());
        }
        mixed.set_games(games);
        for i in 1..titles.len() {
            mixed.push_art(format!("steam:{i}"), poster_png(i));
        }
        let mut s = games_tab(LibraryView::Grid, &mixed);
        dump(&mut s, 60, "LA-collections-row");

        // A phone in landscape, as the shell's own phone dump sizes it.
        let (w, h) = (2868_i32, 1320_i32);
        let viewport = crate::console::Viewport {
            width: w as u32,
            height: h as u32,
            insets: crate::console::Insets {
                left: 186.0,
                top: 0.0,
                right: 186.0,
                bottom: 63.0,
            },
            scale: Some(2.25),
        };
        let mut s = games_tab(LibraryView::Grid, &full());
        s.platform = crate::platform::Platform::Apple;
        let phone = |s: &mut Shell, frames: usize, name: &str| {
            let mut surface = skia_safe::surfaces::raster_n32_premul((w, h)).unwrap();
            for _ in 0..frames {
                s.render_in(surface.canvas(), &viewport, &fonts, None, None, &[]);
            }
            let png = surface
                .image_snapshot()
                .encode(None, skia_safe::EncodedImageFormat::PNG, 100)
                .unwrap();
            std::fs::write(format!("{dir}/{name}.png"), png.as_bytes()).unwrap();
        };
        phone(&mut s, 90, "LP1-phone-games");
        menu(&mut s, &[down(), down(), down(), down()]);
        phone(&mut s, 60, "LP2-phone-grid");
    }
}
