//! Coverflow and grid of one host's titles.
//!
//! One screen on the shell stack. B pops to the host list; A launches the
//! focused title in this window. The shell owns aurora, chrome, and the
//! connecting overlay.
//!
//! `host.pin` is load-bearing: a pinned card launches with that preset.
//! Posters decode here ([`decode_poster`]) so collections and this screen
//! share one cache size. Entrance waits for neighbourhood art or 400 ms.
//!
//! Pin with the tests in this module: entrance, eviction, cache, GPU budget,
//! and collections hand-over.

use crate::anim::{approach, entrances, Entrance, EntranceAt, Spring};
use crate::glyphs::{Hint, HintKey};
use crate::library::{
    card_matrix, grid_col_hint, grid_step, initials, step_cursor, store_label, GridDir, GridShape,
    LibraryGame, LibraryPhase, LibraryShared, LibraryView, Stale, StepResult, BUMP_C, BUMP_K,
    BUMP_PX, ENTER_RISE, ENTER_SCALE, ENTER_TURN_DEG, FOCUS_GAP, GRID_GAP, GRID_H, GRID_W, JUMP,
    PERSPECTIVE, POSTER_H, POSTER_W, RECEDE_DIM, RECEDE_SCALE, ROTATE_DEG, SIDE_SPACING, SPRING_C,
    SPRING_K, VISIBLE_RANGE,
};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::{Pointer, PointerKind};
use crate::screens::{ConnectIntent, Ctx, Outbox, Screen};
use crate::theme::{accent, art_sampling, fg, fill, Fonts, EDGE_INSET, W};
use crate::widgets::{TabStrip, TAB_PILL_H, TAB_PILL_TOP, TAB_STRIP_H};
use pf_client_core::menu_nav::{MenuDir, MenuEvent, MenuPulse};
use skia_safe::{Canvas, Color4f, Data, Image, Matrix, Point, RRect, Rect, TileMode, M44};
use std::collections::HashMap;

const GRID_MARGIN: f64 = 48.0;
/// Row air only; the title lives in the shared detail band.
const GRID_LABEL: f64 = 10.0;
const GRID_HEADING: f64 = 30.0;
/// Title line only; the store lives on the cover badge.
const DETAIL_BAND: f64 = 64.0;
const BAR_CORNER: f64 = 14.0;
/// Air above row 0. Flush covers read as clipped chrome.
const BAR_GAP: f64 = 12.0;
const CAPTION_GAP: f64 = 10.0;
/// ~nine grid rows. At [`ART_CACHE_W`] each raster is ~0.7 MB, so this is also RAM.
const ART_BUDGET: usize = 160;
/// Twice the grid cell: mip levels both arrangements sample. Smaller magnifies the shelf.
const ART_CACHE_W: f64 = GRID_W * 2.0;
const ART_CACHE_H: f64 = GRID_H * 2.0;
/// How much of a frame poster decoding may spend before it yields to the next one.
///
/// A COUNT cannot be right for two machines an order of magnitude apart: two per frame is
/// nothing on a desktop and a dropped frame on a 2020 TV, where one 600×900 JPEG costs more
/// than the whole 16 ms budget. A time budget self-tunes — fast hardware fills the shelf in
/// the same few frames it always did, slow hardware decodes one and gets on with drawing.
///
/// Always at least one per frame regardless: a budget already spent must still make progress,
/// or a slow panel would never finish loading at all.
pub(super) const ART_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(6);

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

/// Decode here (not at first draw) and bake mips at [`art_cache_size`].
///
/// `Image::from_encoded` defers decode until use; a GPU purge then re-decodes JPEG on
/// the render thread. Shared with collections so both screens agree on cache size.
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
    // A refused scale keeps the full-size image rather than dropping the cover.
    let out = scaled.unwrap_or_else(|| img.clone());
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
        // Never-drawn stamps 0. Encoded bytes are gone after decode; this does not refill.
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
pub(crate) fn draw_poster_placeholder(
    canvas: &Canvas,
    fonts: &Fonts,
    game: Option<&LibraryGame>,
    rect: Rect,
    k: f64,
) {
    // Side cards overlap; glass shows the neighbour. `card_face` tints without alpha.
    let launcher = matches!(game, Some(g) if g.launcher);
    canvas.draw_rect(rect, &fill(placeholder_face(launcher)));
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
        canvas.draw_path(&path, &fill(fg(0.85)));
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
            fg(0.85),
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
        &fill(fg(0.85)),
    );
}

/// `RESUME` in `rect`'s top-right (store chip owns top-left). Opaque: coverflow cards composite offscreen.
fn draw_running_badge(canvas: &Canvas, fonts: &Fonts, rect: Rect, k: f64) {
    const LABEL: &str = "RESUME";
    let size = 11.0 * k;
    let tw = fonts.measure(LABEL, W::SemiBold, size) as f64;
    let (bw, bh) = (tw + 16.0 * k, 20.0 * k);
    let pad = 8.0 * k;
    let x = f64::from(rect.right) - pad - bw;
    let y = f64::from(rect.top) + pad;
    canvas.draw_rrect(
        RRect::new_rect_xy(
            Rect::from_xywh(x as f32, y as f32, bw as f32, bh as f32),
            (bh / 2.0) as f32,
            (bh / 2.0) as f32,
        ),
        &fill(crate::theme::ONLINE_GREEN),
    );
    // Fixed green on every palette; `fg()` is white on the pale ones.
    fonts.draw(
        canvas,
        LABEL,
        x + 8.0 * k,
        y + 14.0 * k,
        W::SemiBold,
        size,
        Color4f::new(0.04, 0.10, 0.05, 1.0),
    );
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

/// Width [`strip_caption`] draws. Trailing groups need this before they choose `x`.
pub(super) fn caption_width(fonts: &Fonts, label: &str, k: f64) -> f64 {
    let size = 11.0 * k;
    // Tracking is added after the last character too, so ink ends one gap short of the pen.
    f64::from(fonts.measure(label, W::SemiBold, size))
        + 1.4 * k * (label.chars().count().saturating_sub(1)) as f64
}

/// Caption on the pill row's centre line. Caller adds the width, then hands the rest to [`TabStrip`].
pub(super) fn strip_caption(
    canvas: &Canvas,
    fonts: &Fonts,
    label: &str,
    band: Rect,
    x: f64,
    k: f64,
) -> f64 {
    let size = 11.0 * k;
    let track = 1.4 * k;
    // `TabStrip` seats 2 dp in and draws 30 dp; the band centre sits half a pill low.
    let baseline = f64::from(band.top) + 17.0 * k + size * 0.36;
    fonts.draw_tracked(
        canvas,
        label,
        x,
        baseline,
        W::SemiBold,
        size,
        track,
        fg(0.45),
    );
    // Tracking is added after the last character too, so ink ends one gap short of the pen.
    f64::from(fonts.measure(label, W::SemiBold, size))
        + track * (label.chars().count().saturating_sub(1)) as f64
}

/// One control: whole-bar focus. Strips only draw and hit-test; settings are the state.
struct LibraryBar {
    focus: bool,
    /// 0–1 presence, chased toward `focus`. Hidden, the field takes the band back.
    reveal: f64,
    sort_tabs: TabStrip,
    view_tabs: TabStrip,
}

impl LibraryBar {
    fn new() -> LibraryBar {
        LibraryBar {
            focus: false,
            reveal: 0.0,
            sort_tabs: TabStrip::new(),
            view_tabs: TabStrip::new(),
        }
    }
}

pub(crate) struct LibraryScreen {
    /// Whole row: collections builds a second shelf from it. `pin` is the one-off preset.
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
    /// Reached from collections (group or "All titles"). Y must refuse both, or it loops.
    drilled: bool,
    /// Collections hand-over, once. A later rescan must not replace a shelf already in use.
    pending_collections: bool,
    /// [`LibraryShared::fetch_epoch`] at push, before `FetchLibrary` is queued.
    /// Until that fetch lands, the model still holds the previous host's Ready list.
    /// Do not infer "mine" from a phase: a warm cache can skip `Loading` in one frame.
    entry_epoch: u64,
    // Integer cursor is the authority; the eased position chases it.
    cursor: i32,
    /// Last drawn card rects (axis-aligned; tilt is inside finger slop). Empty if culled.
    geom: Vec<Rect>,
    anim: Spring,
    bump: Spring,
    view_mode: LibraryView,
    /// Boxed: `Screen` moves by value and this variant is already the largest.
    bar: Box<LibraryBar>,
    scroll: Spring,
    /// Seat scroll next frame; the two arrangements do not share a position.
    snap_scroll: bool,
    /// Columns the last grid frame drew. `None` until then — do not invent a count.
    grid_cols_last: Option<usize>,
    /// Last chosen column, carried across vertical moves ([`grid_col_hint`]).
    grid_col: usize,
    /// Recoil axis. A vertical refuse must not nudge the field sideways.
    bump_vertical: bool,
    /// Decoded rasters at draw size, mips baked ([`decode_poster`]). Not deferred encoded.
    art: HashMap<String, Image>,
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
}

impl LibraryScreen {
    /// [`LibraryShared::fetch_epoch`] at push, before `FetchLibrary` is queued.
    pub(crate) fn new(host: &HostRow, entry_epoch: u64) -> LibraryScreen {
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
            drilled: false,
            pending_collections: true,
            entry_epoch,
            cursor: 0,
            geom: Vec::new(),
            anim: Spring::rest(0.0),
            bump: Spring::rest(0.0),
            view_mode: LibraryView::default(),
            bar: Box::new(LibraryBar::new()),
            scroll: Spring::rest(0.0),
            snap_scroll: true,
            grid_cols_last: None,
            grid_col: 0,
            bump_vertical: false,
            art: HashMap::new(),
            // Design scale. Decode runs at this `k` for the life of the screen.
            art_k: 1.0,
            art_seen: HashMap::new(),
            frame: 0,
            entrance: None,
            entrance_armed: false,
            entrance_anchor: 0,
            ready_at: None,
        }
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
            self.entrance = Some(Entrance::new(entrances::CARDS, cursor, t));
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
        let view = LibraryView::parse(&ctx.settings.library_view);
        if view != self.view_mode {
            self.view_mode = view;
            // Shelf scroll is horizontal; grid is vertical. Seat, do not glide from the other.
            self.snap_scroll = true;
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
        let avail = f64::from(rect.width()) - 2.0 * GRID_MARGIN * k;
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
        self.view = crate::collate::filtered(&self.games, self.sort, self.filter.as_ref());
        self.cursor = self.cursor.clamp(0, (self.view.len() as i32 - 1).max(0));
        self.seat_grid_col();
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

    /// Whole library from collections' "All titles": drilled without a filter.
    pub(crate) fn all_titles(&mut self) {
        self.drilled = true;
    }

    /// Hand over to collections once the list lands and is worth browsing.
    ///
    /// The list arrives after the push; `render` cannot navigate. `Some` replaces this
    /// shelf. Swap only on a settled transition. Loading / Empty / Retry stay here.
    pub(crate) fn collections_upgrade(
        &mut self,
        library: &LibraryShared,
        settings: &pf_client_core::trust::Settings,
    ) -> Option<super::collections::CollectionsScreen> {
        // Per-frame for the life of the screen; fall-through collates the whole library at 60 Hz.
        if !self.pending_collections {
            return None;
        }
        if !settings.library_collections || self.drilled {
            self.pending_collections = false;
            return None;
        }
        self.sync(library);
        // Pending is spent only on Ready. Failed fetch keeps Retry; a later retry still upgrades.
        if !matches!(self.phase, LibraryPhase::Ready) {
            return None;
        }
        // Same epoch as push = previous host's list, still in the model.
        if library.fetch_epoch() == self.entry_epoch {
            return None;
        }
        self.pending_collections = false;
        // Same predicate Y and the legend use. One collection is not worth a screen.
        if !crate::collate::worth_browsing(&self.games) {
            return None;
        }
        // Setting, not `self.sort`: this can fire before `adopt_settings`.
        let mut screen = super::collections::CollectionsScreen::new(
            &self.host,
            crate::collate::SortKey::parse(&settings.library_sort),
        );
        screen.adopt_art(std::mem::take(&mut self.art));
        screen.own_library();
        Some(screen)
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
                }
                self.entrance = None;
                self.entrance_armed = false;
                self.ready_at = None;
                // Do not leave pad focus on a bar that a rescan has replaced with a spinner.
                self.bar.focus = false;
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
        // Already-decoded posters cost a move, so there is no budget to spend on them.
        for (id, poster) in shared.drain_decoded() {
            self.art.insert(id, poster.into_image());
        }
        // One at a time against the clock rather than a fixed count — see [`ART_FRAME_BUDGET`].
        // The deadline is checked AFTER a decode so every frame lands at least one.
        let started = std::time::Instant::now();
        while let Some((id, bytes)) = shared.drain_art(1).pop() {
            match decode_poster(&bytes, k) {
                Some(img) => {
                    self.art.insert(id, img);
                }
                None => tracing::debug!(%id, "undecodable poster"),
            }
            if started.elapsed() >= ART_FRAME_BUDGET {
                break;
            }
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        self.sync(ctx.library);
        self.adopt_settings(ctx);
        // Bar first, above the phase match: Confirm must not reach `ready_action` from a sort.
        if self.bar.focus {
            if self.bar_shown() {
                return self.bar_menu(ev, ctx);
            }
            self.bar.focus = false; // the field went away under it
        }
        match &self.phase {
            // Grid spends up/down on rows and shoulders on pages; shelf has those spare.
            LibraryPhase::Ready if self.view_mode == LibraryView::Grid => match ev {
                MenuEvent::Move(MenuDir::Left) => self.grid_move(GridDir::Left),
                MenuEvent::Move(MenuDir::Right) => self.grid_move(GridDir::Right),
                // Top row only, from the same `GridShape` the renderer laid out.
                MenuEvent::Move(MenuDir::Up) => match self.grid_shape() {
                    Some(shape) if shape.cell_of(self.cursor.max(0) as usize).0 == 0 => {
                        self.focus_bar()
                    }
                    _ => self.grid_move(GridDir::Up),
                },
                MenuEvent::Move(MenuDir::Down) => self.grid_move(GridDir::Down),
                MenuEvent::JumpBack => self.grid_move(GridDir::PageBack),
                MenuEvent::JumpForward => self.grid_move(GridDir::PageForward),
                _ => self.ready_action(ev, fx),
            },
            LibraryPhase::Ready => match ev {
                MenuEvent::Move(MenuDir::Left) => self.step(-1, false),
                MenuEvent::Move(MenuDir::Right) => self.step(1, false),
                MenuEvent::Move(MenuDir::Up) => self.focus_bar(),
                MenuEvent::JumpBack => self.step(-JUMP, true),
                MenuEvent::JumpForward => self.step(JUMP, true),
                _ => self.ready_action(ev, fx),
            },
            LibraryPhase::Error { can_retry, .. } => match ev {
                MenuEvent::Confirm if *can_retry => {
                    self.phase = LibraryPhase::Loading; // optimistic; fetch re-syncs
                    fx.cmds.push(self.fetch_cmd());
                    Some(MenuPulse::Confirm)
                }
                MenuEvent::Back => {
                    fx.pop();
                    None
                }
                _ => None,
            },
            // A host with no plugins still has a desktop, so Confirm streams it rather
            // than doing nothing. Loading has nothing to offer yet.
            LibraryPhase::Empty if ev == MenuEvent::Confirm => {
                fx.connect = Some(self.desktop_intent());
                Some(MenuPulse::Confirm)
            }
            LibraryPhase::Loading | LibraryPhase::Empty => {
                if ev == MenuEvent::Back {
                    fx.pop();
                }
                None
            }
        }
    }

    /// Bar is on screen. Draw, pad, pointer, and legend all read this.
    fn bar_shown(&self) -> bool {
        matches!(self.phase, LibraryPhase::Ready) && self.entrance_armed
    }

    fn focus_bar(&mut self) -> Option<MenuPulse> {
        if !self.bar_shown() {
            return None;
        }
        self.bar.focus = true;
        Some(MenuPulse::Move)
    }

    /// Write the setting; [`Self::adopt_settings`] applies it next. Direct field assigns revert.
    fn bar_menu(&mut self, ev: MenuEvent, ctx: &mut Ctx) -> Option<MenuPulse> {
        match ev {
            // Clamped, like a settings value. Wrap rewrites the settings file on a hold.
            MenuEvent::Move(MenuDir::Left) => self.step_sort(-1, ctx),
            MenuEvent::Move(MenuDir::Right) => self.step_sort(1, ctx),
            // Two values: wrap makes L1 and R1 the same button; a hold flip-flops.
            MenuEvent::JumpBack => self.step_view(-1, ctx),
            MenuEvent::JumpForward => self.step_view(1, ctx),
            // Live already. B here, not pop: leaving the bar must not leave the library.
            MenuEvent::Move(MenuDir::Down) | MenuEvent::Back | MenuEvent::Confirm => {
                self.bar.focus = false;
                Some(MenuPulse::Move)
            }
            MenuEvent::Move(MenuDir::Up) => Some(MenuPulse::Boundary),
            MenuEvent::Secondary | MenuEvent::Tertiary | MenuEvent::Sector(_) => None,
        }
    }

    fn step_sort(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = crate::collate::SortKey::ALL;
        let at = all.iter().position(|s| *s == self.sort);
        match super::settings::step_option(at, all.len(), delta, false) {
            Some(i) => {
                store_sort(all[i], ctx);
                Some(MenuPulse::Move)
            }
            None => Some(MenuPulse::Boundary),
        }
    }

    fn step_view(&mut self, delta: i32, ctx: &mut Ctx) -> Option<MenuPulse> {
        let all = LibraryView::ALL;
        let at = all.iter().position(|v| *v == self.view_mode);
        match super::settings::step_option(at, all.len(), delta, false) {
            Some(i) => {
                store_view(all[i], ctx);
                Some(MenuPulse::Move)
            }
            None => Some(MenuPulse::Boundary),
        }
    }

    fn grid_move(&mut self, dir: GridDir) -> Option<MenuPulse> {
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
                    pos: -BUMP_PX * if forward { 1.0 } else { -1.0 },
                    vel: 0.0,
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

    /// Stream the host itself, launching nothing — asking a host to launch what it is
    /// already showing is how a second copy starts. The takeover names the running game
    /// when there is one, the host otherwise: the verb lives on the tile.
    fn desktop_intent(&self) -> ConnectIntent {
        let subject = if self.host.running.is_empty() {
            &self.host.name
        } else {
            &self.host.running
        };
        ConnectIntent {
            addr: self.host.addr.clone(),
            port: self.host.port,
            fp_hex: self.host.fp_hex.clone(),
            launch: None,
            title: match &self.host.pin {
                Some(p) => format!("{subject} \u{b7} {}", p.name),
                None => subject.clone(),
            },
            request_access: false,
            preset: self.host.pin.as_ref().map(|p| p.id.clone()),
        }
    }

    fn ready_action(&mut self, ev: MenuEvent, fx: &mut Outbox) -> Option<MenuPulse> {
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
                fx.connect = Some(ConnectIntent {
                    addr: self.host.addr.clone(),
                    port: self.host.port,
                    fp_hex: self.host.fp_hex.clone(),
                    launch: Some(g.id.clone()),
                    title: match &self.host.pin {
                        Some(p) => format!("{} \u{b7} {}", g.title, p.name),
                        None => g.title.clone(),
                    },
                    request_access: false,
                    // Pinned card: that preset as a one-off. Primary tile: host default.
                    preset: self.host.pin.as_ref().map(|p| p.id.clone()),
                });
                Some(MenuPulse::Confirm)
            }
            // X, not Up: the grid spends Up on rows.
            MenuEvent::Tertiary => {
                let g = self.focused()?;
                // The desktop tile IS the host, so its menu is the host's.
                if g.id == crate::library::DESKTOP_ID {
                    fx.options(super::options::OptionsScreen::for_host(&self.host));
                } else {
                    fx.options(super::options::OptionsScreen::for_game(&self.host, g));
                }
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Back => {
                fx.pop();
                None
            }
            // Boundary, not silence, when there is nothing to collect or this shelf is drilled.
            MenuEvent::Secondary => {
                if self.drilled || !crate::collate::worth_browsing(&self.games) {
                    return Some(MenuPulse::Boundary);
                }
                let mut screen = super::collections::CollectionsScreen::new(&self.host, self.sort);
                screen.adopt_art(self.art.clone());
                fx.push(Screen::Collections(screen));
                Some(MenuPulse::Confirm)
            }
            MenuEvent::Move(_)
            | MenuEvent::Sector(_)
            | MenuEvent::JumpBack
            | MenuEvent::JumpForward => None,
        }
    }

    /// Centre card launches; any other only comes to the front.
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
            PointerKind::Move => match self.card_under(p) {
                Some(i) if i != self.cursor as usize => {
                    self.cursor = i as i32;
                    self.seat_grid_col();
                    true
                }
                _ => false,
            },
            PointerKind::Press => {
                // Pills sit over the field. `TabStrip` keeps last-drawn geometry; faded pills must not hit.
                let (sort_hit, view_hit) = if self.bar_shown() && self.bar.focus {
                    (
                        self.bar
                            .sort_tabs
                            .pointer(p)
                            .and_then(|i| crate::collate::SortKey::ALL.get(i).copied()),
                        self.bar
                            .view_tabs
                            .pointer(p)
                            .and_then(|i| LibraryView::ALL.get(i).copied()),
                    )
                } else {
                    (None, None)
                };
                if let Some(sort) = sort_hit {
                    store_sort(sort, ctx);
                    return true;
                }
                if let Some(view) = view_hit {
                    store_view(view, ctx);
                    return true;
                }
                match self.card_under(p) {
                    Some(i) if i == self.cursor as usize => {
                        self.menu(MenuEvent::Confirm, ctx, fx);
                        true
                    }
                    Some(i) => {
                        self.cursor = i as i32;
                        self.seat_grid_col();
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// The card under `p`, nearest the cursor first — cards overlap on the shelf, and
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
                    pos: -BUMP_PX * f64::from(delta.signum()),
                    vel: 0.0,
                };
                self.bump_vertical = false; // shelf is a line; recoil is horizontal
                Some(MenuPulse::Boundary)
            }
        }
    }

    /// Launcher prefix length. [`LibraryShared::set_games`] groups them at the front.
    /// Length of the leading band ([`LibraryGame::leads`]): the desktop tile and the
    /// launchers behind it. This is [`GridShape`]'s split, not a count of launchers.
    fn lead_count(&self) -> usize {
        self.view
            .iter()
            .map_while(|&i| self.games.get(i))
            .take_while(|g| g.leads())
            .count()
    }

    /// Through [`Self::focused`]: `games[cursor]` is only right under the default sort.
    fn focused_is_launcher(&self) -> bool {
        self.focused().is_some_and(|g| g.launcher)
    }

    /// What a screen reader speaks: the bar names both its pills, a tile its detail-band
    /// title. Nothing while the shelf is still loading — there is no focus to name yet.
    pub(crate) fn announcement(&self) -> Option<String> {
        if !matches!(self.phase, LibraryPhase::Ready) {
            return None;
        }
        if self.bar.focus && self.bar_shown() {
            let (sort, view) = (self.sort.label(), self.view_mode.label());
            return Some(format!("Sort {sort}, view {view}"));
        }
        let game = self.focused()?;
        Some(if game.id == crate::library::DESKTOP_ID {
            self.desktop_caption()
        } else {
            game.title.clone()
        })
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        // Replace the field legend; extending it advertises Play/Options that go nowhere.
        if self.bar.focus && self.bar_shown() {
            return vec![
                Hint::new(HintKey::Adjust, "Sort"),
                Hint::new(HintKey::Shoulders, "View"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        match &self.phase {
            LibraryPhase::Ready => {
                let desktop = self
                    .focused()
                    .is_some_and(|g| g.id == crate::library::DESKTOP_ID);
                let mut hints = vec![Hint::new(
                    HintKey::Confirm,
                    match (
                        desktop,
                        self.focused().is_some_and(|g| g.running),
                        self.focused_is_launcher(),
                    ) {
                        // The desktop tile resumes when the host has a game up, and it is
                        // the ONE tile that never launches anything.
                        (true, _, _) if !self.host.running.is_empty() => "Resume",
                        (true, _, _) => "Stream",
                        (_, true, _) => "Resume",
                        (_, false, true) => "Open",
                        (_, false, false) => "Play",
                    },
                )];
                if !self.drilled && crate::collate::worth_browsing(&self.games) {
                    hints.push(Hint::new(HintKey::Secondary, "Collections"));
                }
                hints.push(Hint::new(HintKey::Tertiary, "Options"));
                hints.push(Hint::new(HintKey::Shoulders, "Jump"));
                if self.bar_shown() {
                    hints.push(Hint::new(HintKey::Up, "Sort & view"));
                }
                hints.push(Hint::new(HintKey::Back, "Back"));
                hints
            }
            LibraryPhase::Error {
                can_retry: true, ..
            } => vec![
                Hint::new(HintKey::Confirm, "Retry"),
                Hint::new(HintKey::Back, "Back"),
            ],
            // No catalog is not no host: Confirm still streams the desk.
            LibraryPhase::Empty => vec![
                Hint::new(HintKey::Confirm, "Stream"),
                Hint::new(HintKey::Back, "Back"),
            ],
            _ => vec![Hint::new(HintKey::Back, "Back")],
        }
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
        let (w, cy_all) = (
            f64::from(rect.width()),
            f64::from(rect.top) + f64::from(rect.height()) / 2.0,
        );
        let cx = f64::from(rect.left) + w / 2.0;
        match self.phase.clone() {
            LibraryPhase::Ready => {
                self.anim
                    .step(f64::from(self.cursor), SPRING_K, SPRING_C, dt);
                self.anim.settle(f64::from(self.cursor), 0.001, 0.01);
                self.bump.step(0.0, BUMP_K, BUMP_C, dt);
                self.bump.settle(0.0, 0.3, 4.0);
                // Twin in `home.rs`: haptic survives, travel does not.
                if crate::theme::reduce_motion() {
                    self.bump = Spring::rest(0.0);
                }
                self.arm_entrance(ctx.t);
                if !self.entrance_armed {
                    // Spinner until entrance frame 1. Clear geom: `pointer` reads it a frame late.
                    self.geom.clear();
                    draw_loading(canvas, rect, k, fonts, ctx.t);
                    return;
                }
                if self.entrance.is_some_and(|e| e.done(ctx.t)) {
                    self.entrance = None;
                }
                let bar_target = if self.bar.focus { 1.0 } else { 0.0 };
                self.bar.reveal = if crate::theme::reduce_motion() {
                    bar_target
                } else {
                    approach(self.bar.reveal, bar_target, dt, 0.10)
                };
                if (self.bar.reveal - bar_target).abs() < 0.005 {
                    self.bar.reveal = bar_target;
                }
                let reveal = self.bar.reveal;
                let bar = Rect::from_ltrb(
                    rect.left,
                    rect.top,
                    rect.right,
                    rect.top + (TAB_STRIP_H * k) as f32,
                );
                let field = Rect::from_ltrb(
                    rect.left,
                    rect.top + ((TAB_STRIP_H + BAR_GAP) * k * reveal) as f32,
                    rect.right,
                    rect.bottom,
                );
                match self.view_mode {
                    LibraryView::Shelf => self.draw_carousel(canvas, field, k, fonts, ctx.t),
                    LibraryView::Grid => self.draw_grid(canvas, field, k, fonts, ctx.t),
                }
                // After the cards. Bounded layer: unbounded allocates a surface-sized offscreen.
                if reveal > 0.01 {
                    let bounds = Rect::from_ltrb(
                        bar.left,
                        bar.top - (12.0 * k) as f32,
                        bar.right,
                        bar.bottom + (12.0 * k) as f32,
                    );
                    canvas.save_layer_alpha_f(bounds, reveal as f32);
                    canvas.translate((0.0f32, (-(1.0 - reveal) * 10.0 * k) as f32));
                    self.draw_bar(canvas, bar, k, fonts, dt);
                    canvas.restore();
                }
                self.draw_detail_band(canvas, rect, k, fonts);
                self.evict_art();
            }
            LibraryPhase::Loading => draw_loading(canvas, rect, k, fonts, ctx.t),
            LibraryPhase::Empty => {
                fonts.centered(
                    canvas,
                    "No games found",
                    W::Bold,
                    22.0 * k,
                    fg(1.0),
                    cx,
                    cy_all - 20.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    "Install Steam titles or add custom entries in the host's web console.",
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
                    cx,
                    cy_all + 12.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    "This host still streams its desktop.",
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
                    cx,
                    cy_all + 34.0 * k,
                    w * 0.8,
                );
            }
            LibraryPhase::Error { title, body, .. } => {
                fonts.centered(
                    canvas,
                    &title,
                    W::Bold,
                    22.0 * k,
                    fg(1.0),
                    cx,
                    cy_all - 32.0 * k,
                    w * 0.8,
                );
                fonts.centered(
                    canvas,
                    &body,
                    W::Regular,
                    14.0 * k,
                    fg(0.55),
                    cx,
                    cy_all + 4.0 * k,
                    (600.0 * k).min(w * 0.85),
                );
            }
        }
    }

    /// After draw: coldest = longest off screen, not longest since decode.
    fn evict_art(&mut self) {
        let live: Vec<String> = self.art.keys().cloned().collect();
        for id in art_to_evict(&live, &self.art_seen) {
            self.art.remove(&id);
            self.art_seen.remove(&id);
        }
    }

    /// Sort + view while the bar holds the pad. Wash is the whole control, not one pill row.
    fn draw_bar(&mut self, canvas: &Canvas, bar: Rect, k: f64, fonts: &Fonts, dt: f64) {
        if self.bar.focus {
            // Pill-row height, not the band: the leftover 12 dp is the gap before the field.
            let wash_h = (TAB_PILL_TOP * 2.0 + TAB_PILL_H) * k;
            let wash = Rect::from_ltrb(bar.left, bar.top, bar.right, bar.top + wash_h as f32);
            canvas.draw_rrect(
                RRect::new_rect_xy(wash, (BAR_CORNER * k) as f32, (BAR_CORNER * k) as f32),
                &fill(accent(0.14)),
            );
        }
        let sorts: Vec<&str> = crate::collate::SortKey::ALL
            .iter()
            .map(|s| s.label())
            .collect();
        let sort_at = crate::collate::SortKey::ALL
            .iter()
            .position(|s| *s == self.sort)
            .unwrap_or(0);
        let views: Vec<&str> = LibraryView::ALL.iter().map(|v| v.label()).collect();

        // Named gap: `TabStrip`'s inset only appears when its rect has slack.
        let gap = CAPTION_GAP * k;
        let sort_cap = caption_width(fonts, "SORT", k);
        let view_cap = caption_width(fonts, "VIEW", k);
        let sort_pills = crate::widgets::TabStrip::width(&sorts, fonts, k);
        let view_pills = crate::widgets::TabStrip::width(&views, fonts, k);

        let sort_x = f64::from(bar.left) + EDGE_INSET * k;
        // Scale follows height; a tall-narrow window must crowd, not slide off the leading edge.
        let view_x = (f64::from(bar.right) - EDGE_INSET * k - view_cap - gap - view_pills)
            .max(sort_x + sort_cap + gap + sort_pills + gap);

        strip_caption(canvas, fonts, "SORT", bar, sort_x, k);
        strip_caption(canvas, fonts, "VIEW", bar, view_x, k);
        let sort_left = sort_x + sort_cap + gap;
        let view_left = view_x + view_cap + gap;

        self.bar.sort_tabs.render(
            canvas,
            Rect::from_ltrb(
                sort_left as f32,
                bar.top,
                (sort_left + sort_pills) as f32,
                bar.bottom,
            ),
            &sorts,
            sort_at,
            false,
            fonts,
            k,
            dt,
        );

        let view_at = LibraryView::ALL
            .iter()
            .position(|v| *v == self.view_mode)
            .unwrap_or(0);
        self.bar.view_tabs.render(
            canvas,
            Rect::from_ltrb(
                view_left as f32,
                bar.top,
                (view_left + view_pills) as f32,
                bar.bottom,
            ),
            &views,
            view_at,
            false,
            fonts,
            k,
            dt,
        );
    }

    /// Same cursor, order, detail band, and art cache as the shelf; only placement differs.
    fn draw_grid(&mut self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, t: f64) {
        let cols = self.grid_cols(rect, k);
        // Navigation reads last-drawn columns. A resize is a different grid; re-seat.
        if self.grid_cols_last != Some(cols) {
            self.grid_cols_last = Some(cols);
            self.seat_grid_col();
        }
        let shape = GridShape::new(self.len(), cols, self.lead_count());
        // Two-column clamp can overflow a narrow rect; shrink cells only, never headings.
        let fit = ((f64::from(rect.width()) - 2.0 * GRID_MARGIN * k)
            / ((cols as f64 * (GRID_W + GRID_GAP) - GRID_GAP) * k))
            .clamp(0.25, 1.0);
        let (cw, ch) = (GRID_W * k * fit, GRID_H * k * fit);
        let pitch_x = cw + GRID_GAP * k * fit;
        let pitch_y = ch + GRID_GAP * k + GRID_LABEL * k;
        let split_row = (shape.split > 0).then(|| shape.split_row());
        let heading_h = GRID_HEADING * k;
        // Top inset is always on: it is also the air row 0 needs. GAMES heading stays conditional.
        let row_top = |row: usize| -> f64 {
            let section_gap = match split_row {
                Some(s) if row >= s => heading_h,
                _ => 0.0,
            };
            row as f64 * pitch_y + heading_h + section_gap
        };

        // Matching bottom inset lives in `content_h`; the clamp only knows this number.
        let content_h = row_top(shape.rows().saturating_sub(1)) + ch + heading_h;
        let view_h = f64::from(rect.height()) - DETAIL_BAND * k;
        let (focus_row, _) = shape.cell_of(self.cursor.max(0) as usize);
        // 0.34 keeps a row of context above and below the focus.
        let want = (row_top(focus_row) - view_h * 0.34).clamp(0.0, (content_h - view_h).max(0.0));
        if std::mem::take(&mut self.snap_scroll) || crate::theme::reduce_motion() {
            self.scroll = Spring::rest(want);
        } else {
            self.scroll
                .step_spec(want, crate::anim::springs::FOCUS, 1.0 / 60.0);
            self.scroll.settle(want, 0.05, 0.5);
        }

        let bump = self.bump.pos * k;
        let (bump_x, scroll) = if self.bump_vertical {
            (0.0, self.scroll.pos - bump)
        } else {
            (bump, self.scroll.pos)
        };

        let grid_w = cols as f64 * pitch_x - GRID_GAP * k * fit;
        let x0 = f64::from(rect.left) + (f64::from(rect.width()) - grid_w) / 2.0 + bump_x;
        let y0 = f64::from(rect.top);
        let viewport = Rect::from_xywh(rect.left, rect.top, rect.width(), (view_h.max(0.0)) as f32);

        self.geom.clear();
        self.geom.resize(self.len(), Rect::new_empty());
        canvas.save();
        canvas.clip_rect(viewport, None, true);
        if let Some(_s) = split_row {
            let head = |canvas: &Canvas, label: &str, y: f64| {
                fonts.draw_tracked(
                    canvas,
                    label,
                    x0,
                    y,
                    W::SemiBold,
                    12.0 * k,
                    1.4 * k,
                    fg(0.45),
                );
            };
            head(canvas, "LAUNCHERS", y0 + heading_h * 0.62 - scroll);
            head(
                canvas,
                "GAMES",
                y0 + row_top(split_row.expect("checked")) - heading_h * 0.38 - scroll,
            );
        }

        // Entrance lift + 6 % focus swell. Settled cards sit on berth; cull can match the clip.
        let reach = if self.entrance.is_some() {
            ENTER_RISE * k + 0.06 * ch
        } else {
            0.0
        };
        let (anchor_row, anchor_col) =
            shape.cell_of(self.entrance_anchor.min(self.len().saturating_sub(1)));
        for i in 0..self.len() {
            let (row, col) = shape.cell_of(i);
            let top = y0 + row_top(row) - scroll;
            if top + ch + reach < f64::from(rect.top) || top > y0 + view_h {
                continue; // not drawn, not stamped — cull must not keep covers warm
            }
            let ent = self.entrance_at(anchor_row.abs_diff(row) + anchor_col.abs_diff(col), t);
            let f = if i == self.cursor.max(0) as usize {
                1.0
            } else {
                0.0
            };
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let scale = (1.0 + 0.06 * f) * arrive;
            let cx = x0 + col as f64 * pitch_x + cw / 2.0;
            let cy = top + ch / 2.0 + (1.0 - ent.travel) * ENTER_RISE * k;
            let cell = Rect::from_xywh(
                (cx - cw * scale / 2.0) as f32,
                (cy - ch * scale / 2.0) as f32,
                (cw * scale) as f32,
                (ch * scale) as f32,
            );
            self.geom[i] = cell;
            let Some(game) = self.game(i) else { continue };
            let id = game.id.clone();
            // Stamp at the end needs `&mut self`; drop the `&LibraryGame` first.
            let running = game.running;

            crate::theme::focus_halo(canvas, cell, 12.0, k as f32, f as f32);
            let art = self.art.get(&id);
            let rr = RRect::new_rect_xy(cell, (12.0 * k) as f32, (12.0 * k) as f32);
            // Layer only for multi-piece fades (placeholder, focus ring). Paint alpha otherwise.
            let layered = ent.fade < 1.0 && (art.is_none() || f > 0.0);
            if layered {
                canvas.save_layer_alpha_f(cell, ent.fade as f32);
            }
            match art {
                Some(img) => {
                    let (iw, ih) = (img.width() as f32, img.height() as f32);
                    let aspect = cell.width() / cell.height();
                    let src = if iw / ih > aspect {
                        let sw = ih * aspect;
                        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                    } else {
                        let sh = iw / aspect;
                        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                    };
                    // Shader rrect, not clip+image: coverage AA, no clip-stack per card.
                    let (sx, sy) = (cell.width() / src.width(), cell.height() / src.height());
                    let mut local = Matrix::scale((sx, sy));
                    local.post_translate((cell.left - src.left * sx, cell.top - src.top * sy));
                    if let Some(shader) = img.to_shader(
                        (TileMode::Clamp, TileMode::Clamp),
                        art_sampling(),
                        Some(&local),
                    ) {
                        // Opaque: Skia modulates the shader by paint alpha; 0 draws nothing.
                        let mut p = crate::theme::shaded();
                        p.set_shader(shader);
                        if !layered {
                            p.set_alpha_f(ent.fade as f32);
                        }
                        canvas.draw_rrect(rr, &p);
                    }
                }
                None => {
                    canvas.save();
                    canvas.clip_rrect(rr, None, true);
                    draw_poster_placeholder(canvas, fonts, self.game(i), cell, k);
                    canvas.restore();
                }
            }
            if f > 0.0 {
                crate::theme::panel(
                    canvas,
                    cell,
                    12.0,
                    None,
                    crate::theme::PanelStroke::Brand(0.9),
                    k as f32,
                );
            }
            if running {
                draw_running_badge(canvas, fonts, cell, k);
            }
            if layered {
                canvas.restore();
            }
            self.art_seen.insert(id, self.frame);
        }
        canvas.restore();
    }

    fn draw_carousel(&mut self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts, t: f64) {
        let (card_w, card_h) = (POSTER_W * k, POSTER_H * k);
        let w = f64::from(rect.width());
        // 0.44 leaves the detail band below.
        let cy = f64::from(rect.top) + f64::from(rect.height()) * 0.44;
        let pos = self.anim.pos;
        let bump = self.bump.pos * k;

        // Coverflow is 1-D: name the group at the cursor. Skip if the shelf is one group.
        let lead = self.lead_count();
        if lead > 0 && lead < self.len() {
            // The desktop tile sits under the launcher heading: both open something.
            // A shelf whose whole lead band IS the tile says so instead.
            let heading = if (self.cursor as usize) >= lead {
                "GAMES"
            } else if lead == 1 {
                "HOST"
            } else {
                "LAUNCHERS"
            };
            fonts.centered(
                canvas,
                heading,
                W::SemiBold,
                12.0 * k,
                fg(0.45), // same section-header weight as `MenuList`
                f64::from(rect.left) + w / 2.0,
                cy - card_h / 2.0 - 22.0 * k,
                w * 0.5,
            );
        }

        // Farthest from the integer cursor first, so stacks overlap toward focus.
        let mut order: Vec<usize> = (0..self.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse((i as i32 - self.cursor).abs()));
        self.geom.clear();
        self.geom.resize(self.len(), Rect::new_empty());

        for i in order {
            let d = i as f64 - pos;
            let a = d.abs();
            if a > VISIBLE_RANGE {
                continue;
            }
            let prox = a.min(1.0);
            let ent = self.entrance_at(i.abs_diff(self.entrance_anchor), t);
            let arrive = ENTER_SCALE + (1.0 - ENTER_SCALE) * ent.travel;
            let turn = (1.0 - ent.travel)
                * ENTER_TURN_DEG
                * if (i as i32) < self.cursor { -1.0 } else { 1.0 };
            let scale = (1.0 - prox * RECEDE_SCALE) * arrive;
            let angle = -d.clamp(-1.0, 1.0) * ROTATE_DEG + turn;
            let offset = if a <= 1.0 {
                d * FOCUS_GAP * k
            } else {
                d.signum() * (FOCUS_GAP + (a - 1.0) * SIDE_SPACING) * k
            };
            let ccx = f64::from(rect.left) + w / 2.0 + offset + bump;
            let cy = cy + (1.0 - ent.travel) * ENTER_RISE * k;
            self.geom[i] = Rect::from_xywh(
                (ccx - card_w * scale / 2.0) as f32,
                (cy - card_h * scale / 2.0) as f32,
                (card_w * scale) as f32,
                (card_h * scale) as f32,
            );
            let m = card_matrix(ccx, cy, angle, scale, card_w, card_h, PERSPECTIVE * k);

            let Some(game) = self.game(i) else { continue };
            // Screen-space, before the card transform: glow around the clip, not inside it.
            crate::theme::focus_halo(canvas, self.geom[i], 16.0, k as f32, (1.0 - prox) as f32);
            canvas.save();
            canvas.concat_44(&M44::row_major(&m));
            let crect = Rect::from_wh(card_w as f32, card_h as f32);
            let rr = RRect::new_rect_xy(crect, 16.0 * k as f32, 16.0 * k as f32);
            canvas.clip_rrect(rr, None, true);
            // After the clip so `None` bounds are the card, not the screen.
            let fading = ent.fade < 1.0;
            let layered = fading || prox > 0.001;
            if layered {
                let mut lp = crate::theme::layer();
                lp.set_alpha_f(ent.fade as f32);
                if prox > 0.001 {
                    lp.set_color_filter(skia_safe::color_filters::matrix_row_major(
                        &crate::theme::recede_matrix(prox),
                        None,
                    ));
                }
                canvas.save_layer(&skia_safe::canvas::SaveLayerRec::default().paint(&lp));
            }
            let drawn_id = game.id.clone();
            match self.art.get(&game.id) {
                Some(img) => {
                    let (iw, ih) = (img.width() as f32, img.height() as f32);
                    let card_aspect = crect.width() / crect.height();
                    let src = if iw / ih > card_aspect {
                        let sw = ih * card_aspect;
                        Rect::from_xywh((iw - sw) / 2.0, 0.0, sw, ih)
                    } else {
                        let sh = iw / card_aspect;
                        Rect::from_xywh(0.0, (ih - sh) / 2.0, iw, sh)
                    };
                    canvas.draw_image_rect_with_sampling_options(
                        img,
                        Some((&src, skia_safe::canvas::SrcRectConstraint::Fast)),
                        crect,
                        art_sampling(),
                        &fill(fg(1.0)),
                    );
                }
                None => draw_poster_placeholder(canvas, fonts, Some(game), crect, k),
            }
            {
                let label = store_label(&game.store);
                let size = 11.0 * k;
                let tw = fonts.measure(label, W::SemiBold, size) as f64;
                let (px, py) = (8.0 * k, 8.0 * k);
                let (bw, bh) = (tw + 16.0 * k, 20.0 * k);
                // Brand fill vs smoked glass: the cue that survives recede.
                let pill = if game.launcher {
                    accent(0.85)
                } else {
                    crate::theme::shade(0.55)
                };
                canvas.draw_rrect(
                    RRect::new_rect_xy(
                        Rect::from_xywh(px as f32, py as f32, bw as f32, bh as f32),
                        (bh / 2.0) as f32,
                        (bh / 2.0) as f32,
                    ),
                    &fill(pill),
                );
                fonts.draw(
                    canvas,
                    label,
                    px + 8.0 * k,
                    py + 14.0 * k,
                    W::SemiBold,
                    size,
                    fg(1.0),
                );
            }
            // Inside the recede layer so a neighbour's badge fades with its card.
            if game.running {
                draw_running_badge(canvas, fonts, crect, k);
            }
            // Ground-side scrim, not whole-card alpha. Hardcoded black fights `recede_matrix` on pale palettes.
            if prox > 0.0 {
                canvas.draw_rect(
                    crect,
                    &fill(crate::theme::shade((prox * RECEDE_DIM) as f32)),
                );
            }
            if layered {
                canvas.restore();
            }
            canvas.restore();
            self.art_seen.insert(drawn_id, self.frame);
        }
    }

    /// Shared by shelf and grid so the title line cannot drift between arrangements.
    fn draw_detail_band(&self, canvas: &Canvas, rect: Rect, k: f64, fonts: &Fonts) {
        // Cache note describes the shelf, not the focused title — leading, not centred, and
        // above it: the title is anchored by its TOP edge, so its line reaches the band's
        // floor and anything placed under it lands inside the glyphs.
        if let Some(note) = self.stale.note() {
            fonts.draw(
                canvas,
                note,
                f64::from(rect.left) + EDGE_INSET * k,
                f64::from(rect.bottom) - NOTE_BASE * k,
                W::Regular,
                12.0 * k,
                fg(0.55),
            );
        }
        let Some(g) = self.focused() else { return };
        // The desktop tile's own caption names what the press does — "Resume <title>"
        // once the host has something up. Every other tile is its title.
        let title = if g.id == crate::library::DESKTOP_ID {
            self.desktop_caption()
        } else {
            g.title.clone()
        };
        let w = f64::from(rect.width());
        let cx = f64::from(rect.left) + w / 2.0;
        fonts.centered(
            canvas,
            &title,
            W::Bold,
            27.0 * k,
            fg(1.0),
            cx,
            f64::from(rect.bottom) - TITLE_TOP * k,
            w * 0.8,
        );
    }
}

/// Baseline of the cache note, up from the detail band's floor. It clears [`TITLE_TOP`] by
/// its own 12 dp line, and both stay inside [`DETAIL_BAND`].
const NOTE_BASE: f64 = 48.0;
/// TOP edge of the focused title, same origin — `Fonts::centered` anchors a paragraph's
/// top, not its baseline, so a 27 dp line from here reaches the floor.
const TITLE_TOP: f64 = 34.0;

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Live model + armed entrance. `menu` re-syncs; a hand-built shelf is wiped.
    /// Titles are not A–Z, so a sort actually changes display order.
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
        let mut s = LibraryScreen::new(&host(), 0);
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

    /// Confirm in the bar must not reach `ready_action` and start a session.
    #[test]
    fn the_bar_owns_the_pad_and_confirm_cannot_launch_from_it() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert!(s.bar.focus, "up from the shelf reaches the bar");

        let cursor = s.cursor;
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        assert!(fx.connect.is_none(), "A in the bar launched a game");
        assert!(fx.nav.is_none(), "and pushed a screen");
        assert!(!s.bar.focus, "A means done");
        assert_eq!(s.cursor, cursor, "and the field never moved under it");
    }

    /// A screen reader hears the tile it stepped onto, and the bar's own pills once the
    /// bar takes focus — the shelf title underneath would be a lie there.
    #[test]
    fn the_announcement_names_the_tile_then_the_bar() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        // The leading tile is the desktop, and it speaks the caption it draws.
        assert_eq!(s.announcement().as_deref(), Some("Desktop"));
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Right),
        );
        assert_eq!(s.announcement().as_deref(), Some("Zeta"));
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert_eq!(
            s.announcement().as_deref(),
            Some("Sort Default, view Shelf")
        );
    }

    #[test]
    fn the_field_does_not_move_while_the_bar_is_up() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Right),
        );
        assert_eq!(
            s.cursor, 0,
            "the shelf stepped on a press meant for the sort"
        );
    }

    /// Write the setting; assigning `self.sort` reverts on the next `adopt_settings`.
    #[test]
    fn stepping_the_bar_writes_the_setting_and_the_field_follows() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Right),
        );
        assert_eq!(
            settings.library_sort,
            crate::collate::SortKey::Title.id(),
            "the sort is persisted, not held on the screen"
        );

        s.adopt_settings(&ctx(&library, &mut settings));
        assert_eq!(s.sort, crate::collate::SortKey::Title);
        assert_eq!(
            s.game(0).map(|g| g.id.as_str()),
            Some(crate::library::DESKTOP_ID),
            "the desktop tile leads whatever the sort"
        );
        assert_eq!(
            s.game(1).map(|g| g.title.as_str()),
            Some("Alpha"),
            "the display order did not follow the sort"
        );
        assert_eq!(s.focused().map(|g| g.title.as_str()), Some("Desktop"));

        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Left),
        );
        assert_eq!(
            settings.library_sort,
            crate::collate::SortKey::HostOrder.id()
        );
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Left),
        );
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
        assert_eq!(
            settings.library_sort,
            crate::collate::SortKey::HostOrder.id()
        );
    }

    /// Two values: wrap makes both shoulders the same button.
    #[test]
    fn the_shoulders_swap_the_arrangement_and_stop_at_the_ends() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let (pulse, _) = press(&mut s, &library, &mut settings, MenuEvent::JumpForward);
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert_eq!(settings.library_view, LibraryView::Grid.id());

        s.adopt_settings(&ctx(&library, &mut settings));
        assert_eq!(s.view_mode, LibraryView::Grid);
        assert!(
            s.snap_scroll,
            "the two arrangements do not share a scroll, so the new one seats"
        );

        let (pulse, _) = press(&mut s, &library, &mut settings, MenuEvent::JumpForward);
        assert!(
            matches!(pulse, Some(MenuPulse::Boundary)),
            "already the grid"
        );
        press(&mut s, &library, &mut settings, MenuEvent::JumpBack);
        assert_eq!(settings.library_view, LibraryView::Shelf.id());
    }

    #[test]
    fn back_leaves_the_bar_before_it_leaves_the_library() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Back);
        assert!(fx.nav.is_none(), "B in the bar popped the screen");
        assert!(!s.bar.focus);
        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Back);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Pop)),
            "…and B on the field still leaves"
        );
    }

    /// Up from row 0 reaches the bar; anywhere else it is a row move.
    #[test]
    fn up_reaches_the_bar_from_the_grids_top_row_only() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings {
            library_view: LibraryView::Grid.id().to_string(),
            ..Default::default()
        };
        // No test draws a frame; navigation reads this.
        s.grid_cols_last = Some(3);
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Down),
        );
        assert_eq!(s.view_mode, LibraryView::Grid);
        // The desktop tile is a one-tile lead band, so it owns row 0 and the games
        // section restarts at column 0 below it — the same split launchers get.
        assert_eq!(s.cursor, 1, "down moved a row");
        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Down),
        );
        assert_eq!(s.cursor, 4, "and a second row inside the games section");

        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(matches!(pulse, Some(MenuPulse::Move)));
        assert!(!s.bar.focus, "up out of the second row is a row move");
        assert_eq!(s.cursor, 0);

        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(s.bar.focus, "…and from the top row it reaches the bar");
    }

    #[test]
    fn the_legend_swaps_with_the_focus() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        let closed = hint_keys(&s, &library, &mut settings);
        assert!(closed.contains(&HintKey::Up), "nothing leads to the bar");
        assert!(closed.contains(&HintKey::Confirm) && closed.contains(&HintKey::Tertiary));

        press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        let open = hint_keys(&s, &library, &mut settings);
        assert!(open.contains(&HintKey::Adjust) && open.contains(&HintKey::Shoulders));
        assert!(
            !open.contains(&HintKey::Confirm) && !open.contains(&HintKey::Tertiary),
            "the bar's legend still offered the field's actions"
        );
        assert!(
            open.len() <= 4,
            "the legend is read from a sofa: {} entries",
            open.len()
        );
    }

    /// Ready during the art wait is still a spinner; the bar must not answer.
    #[test]
    fn there_is_no_bar_until_there_is_a_field() {
        let (mut s, library) = live_shelf();
        s.entrance_armed = false;
        let mut settings = pf_client_core::trust::Settings::default();
        assert!(!hint_keys(&s, &library, &mut settings).contains(&HintKey::Up));
        let (pulse, _) = press(
            &mut s,
            &library,
            &mut settings,
            MenuEvent::Move(MenuDir::Up),
        );
        assert!(pulse.is_none(), "the shelf's dead press became a live one");
        assert!(!s.bar.focus);
    }

    /// List landed, no art yet.
    fn waiting_shelf() -> LibraryScreen {
        let mut s = LibraryScreen::new(&host(), 0);
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

    fn setting_on() -> pf_client_core::trust::Settings {
        pf_client_core::trust::Settings {
            library_collections: true,
            ..Default::default()
        }
    }

    /// Push + queued fetch. Model is `Loading`; this is the only state the entry decision runs in.
    fn pushed_shelf(
        library: &LibraryShared,
        settings: &pf_client_core::trust::Settings,
    ) -> LibraryScreen {
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        library.begin_fetch();
        s.collections_upgrade(library, settings);
        s
    }

    /// Setting on AND more than one collection. One-shot either way — a miss still costs a collate.
    #[test]
    fn the_shelf_hands_over_only_when_the_setting_and_the_library_agree() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let one = games(&[("Ico", None), ("Journey", None)]);

        let off = pf_client_core::trust::Settings::default();
        let library = LibraryShared::default();
        library.set_games(mixed.clone());
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        assert!(
            s.collections_upgrade(&library, &off).is_none(),
            "setting off"
        );
        assert!(!s.pending_collections, "and it is not asked again");

        let library = LibraryShared::default();
        let mut s = pushed_shelf(&library, &setting_on());
        library.set_games(one);
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_none(),
            "one collection is not worth a screen"
        );
        assert!(!s.pending_collections);
        assert!(!s.drilled, "…and the shelf it stayed still offers Y");

        let library = LibraryShared::default();
        let mut s = pushed_shelf(&library, &setting_on());
        library.set_games(mixed);
        assert!(s.collections_upgrade(&library, &setting_on()).is_some());
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_none(),
            "handed over twice"
        );
    }

    /// Until this shelf's fetch lands, the model still holds the previous host's Ready list.
    #[test]
    fn a_shelf_never_hands_over_on_the_library_it_inherited() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let library = LibraryShared::default();
        library.set_games(mixed.clone());
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        assert!(s.collections_upgrade(&library, &setting_on()).is_none());
        assert!(
            s.pending_collections,
            "the decision was deferred, not spent"
        );

        library.begin_fetch();
        assert!(s.collections_upgrade(&library, &setting_on()).is_none());
        library.set_games(mixed);
        assert!(s.collections_upgrade(&library, &setting_on()).is_some());
    }

    /// Warm cache can skip `Loading` inside one frame. Do not insert an upgrade call in between.
    #[test]
    fn a_warm_cache_still_hands_over_though_no_frame_ever_saw_loading() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let library = LibraryShared::default();
        library.set_games(mixed.clone());
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());

        library.begin_fetch();
        library.set_games_cached(mixed);
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_some(),
            "a cached catalog is this shelf's own list and must upgrade"
        );
    }

    /// Same epoch as the push is the previous host, however ready the model looks.
    #[test]
    fn a_cached_list_from_before_the_push_is_still_refused() {
        let mixed = games(&[("Ico", Some("PS2")), ("Journey", None)]);
        let library = LibraryShared::default();
        library.begin_fetch();
        library.set_games_cached(mixed);
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());
        assert!(
            s.collections_upgrade(&library, &setting_on()).is_none(),
            "same epoch as the push — this list belongs to the host we came from"
        );
        assert!(s.pending_collections, "and the decision is still pending");
    }

    /// Failed fetch keeps Retry here; pending stays so a later retry can still hand over.
    #[test]
    fn a_failed_fetch_keeps_the_shelf_and_still_hands_over_on_retry() {
        let library = LibraryShared::default();
        let mut s = LibraryScreen::new(&host(), library.fetch_epoch());

        library.begin_fetch();
        library.set_phase(LibraryPhase::Error {
            title: "Couldn't load the library".into(),
            body: "refused".into(),
            can_retry: true,
        });
        assert!(s.collections_upgrade(&library, &setting_on()).is_none());
        assert!(
            s.pending_collections,
            "the decision was deferred, not spent"
        );
        assert!(
            matches!(s.phase, LibraryPhase::Error { .. }),
            "the retry is still here"
        );

        library.begin_fetch();
        library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
        assert!(s.collections_upgrade(&library, &setting_on()).is_some());
    }

    /// Drilled (group or "All titles") must not loop back to collections.
    #[test]
    fn a_drilled_shelf_neither_hands_over_nor_offers_y() {
        let library = LibraryShared::default();
        library.set_games(games(&[("Ico", Some("PS2")), ("Journey", None)]));
        let mut settings = setting_on();
        for drill in [0, 1] {
            let mut s = LibraryScreen::new(&host(), 0);
            if drill == 0 {
                s.set_filter(
                    crate::collate::GroupKey::Platform("PS2".into()),
                    "PS2".into(),
                );
            } else {
                s.all_titles();
            }
            assert!(s.collections_upgrade(&library, &settings).is_none());
            let (pulse, fx) = press(&mut s, &library, &mut settings, MenuEvent::Secondary);
            assert!(matches!(pulse, Some(MenuPulse::Boundary)), "Y was answered");
            assert!(fx.nav.is_none(), "…and pushed a screen");
            assert!(
                !hint_keys(&s, &library, &mut settings).contains(&HintKey::Secondary),
                "the legend offered a press that only thuds"
            );
        }
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
        let mut s = LibraryScreen::new(&host(), 0);
        s.adopt_art(HashMap::from([("g0".to_string(), poster())]));
        s.sync(&library);
        assert_eq!(s.art.len(), 1, "the hand-over was wiped by the first list");

        library.set_games(games(&[("Rez", Some("PS2"))]));
        s.sync(&library);
        assert!(s.art.is_empty(), "a different library kept the old covers");
    }

    /// The tile is the host, not a title: Confirm streams it with no launch id, and X
    /// opens the HOST's options — a title menu here would offer a per-title preset
    /// binding for a title that does not exist.
    #[test]
    fn confirm_on_the_desktop_tile_launches_nothing() {
        let (mut s, library) = live_shelf();
        let mut settings = pf_client_core::trust::Settings::default();
        assert_eq!(
            s.focused().map(|g| g.id.as_str()),
            Some(crate::library::DESKTOP_ID),
            "the tile is where the cursor starts"
        );

        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Confirm);
        let intent = fx.connect.expect("a connect intent");
        assert_eq!(intent.launch, None, "the desktop tile launched a title");
        assert_eq!(intent.title, "Desk", "the takeover names the host");

        let (_, fx) = press(&mut s, &library, &mut settings, MenuEvent::Tertiary);
        assert!(
            matches!(fx.nav, Some(crate::screens::Nav::Push(_))),
            "X on the tile opens the host's options"
        );
    }

    /// A host with no plugins is still one press from its desk. The catalog's verdict
    /// stands — the empty copy is what explains the missing shelf.
    #[test]
    fn an_empty_library_still_streams_the_desktop() {
        crate::screens::settings::tests::fake_home();
        let library = LibraryShared::default();
        library.set_games(Vec::new());
        let mut s = LibraryScreen::new(&host(), 0);
        s.sync(&library);
        s.entrance_armed = true;
        assert!(matches!(s.phase, LibraryPhase::Empty));

        let mut settings = pf_client_core::trust::Settings::default();
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
        let s = LibraryScreen::new(&busy, 0);
        assert_eq!(s.desktop_caption(), "Resume Elden Ring");
        assert_eq!(s.desktop_intent().title, "Elden Ring");

        let idle = LibraryScreen::new(&host(), 0);
        assert_eq!(idle.desktop_caption(), "Desktop");
        assert_eq!(idle.desktop_intent().title, "Desk");
    }
}
