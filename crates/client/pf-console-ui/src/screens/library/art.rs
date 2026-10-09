//! Poster art for the library screens: the decode off the drawing thread, the cache size
//! and budget, the covers screens share, and the face a coverless cell draws instead.

use crate::grid::{GRID_H, GRID_W};
use crate::library::{initials, store_label, LibraryGame};
use crate::theme::{art_sampling, fg, fill, Fonts, W};
use skia_safe::{Canvas, Color4f, Data, Image, Point, Rect};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// Covers a screen keeps decoded, and how many places past the first drawn one it decodes.
/// At [`ART_CACHE_W`] each raster is ~0.7 MB of RAM, plus ~1.3 MB uploaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ArtBudget {
    pub(super) held: usize,
    pub(super) ahead: usize,
}

impl ArtBudget {
    /// ~nine grid rows.
    pub(super) const DESKTOP: Self = Self {
        held: 160,
        ahead: 48,
    };
    /// A TV shares 1–2 GB between the app, the GPU and the stream's decoder.
    const TV: Self = Self {
        held: 64,
        ahead: 24,
    };

    pub(super) fn of(device: &crate::screens::Device) -> Self {
        if device.tv {
            Self::TV
        } else {
            Self::DESKTOP
        }
    }
}
/// Twice the grid cell: mip levels both arrangements sample. Smaller magnifies the shelf.
pub(super) const ART_CACHE_W: f64 = GRID_W * 2.0;
pub(super) const ART_CACHE_H: f64 = GRID_H * 2.0;
/// Fit `src` into [`ART_CACHE_W`]×[`ART_CACHE_H`] at `k`. Source aspect; never enlarge.
///
/// A 460×215 header squeezed to 2:3 stretches what the draw already centre-crops.
pub(super) fn art_cache_size(src: (i32, i32), k: f64) -> (i32, i32) {
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
/// already has somewhere better to spend it. `k` comes from
/// [`crate::library::LibraryShared::art_scale`], so the size matches what this screen would have
/// cached anyway.
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

/// [`SHARED_ART`]: at most [`ArtBudget::held`] covers, the oldest shared first out, all at one
/// scale.
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

/// Offer a cover to every screen, keeping at most `held`. A new scale starts the cache over.
pub(super) fn share_cover(fp: &str, id: &str, k: f64, img: &Image, held: usize) {
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
        while a.order.len() > held {
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

/// Coldest stamps first, past `held`. Split out so the policy tests without Skia.
pub(super) fn art_to_evict(
    live: &[String],
    seen: &HashMap<String, u64>,
    held: usize,
) -> Vec<String> {
    if live.len() <= held {
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
        .take(live.len() - held)
        .map(|(_, id)| id.clone())
        .collect()
}

/// Accent mixed into an opaque face ([`crate::theme::card_face`]). Launcher is louder.
const FACE_TINT: f32 = 0.20;
const LAUNCHER_FACE_TINT: f32 = 0.38;

/// Own function so the contrast test asserts this colour, not a re-derived one.
pub(super) fn placeholder_face(launcher: bool) -> Color4f {
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
