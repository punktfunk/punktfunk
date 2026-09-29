//! Posters, fetched as they are drawn. A poster bound on screen asks the shelf's pool for its
//! art, newest first; the pool decodes it on a worker at about the size a tile draws it, and the
//! texture lands on every picture still showing that title. Decoded posters are kept up to a
//! memory budget, the least recently drawn going first.

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use pf_client_core::library::{ArtJob, ArtPool, GameEntry};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Where a shelf's art comes from: the host's base URL, this device's identity, the host's pin.
pub type Source = (String, (String, String), Option<[u8; 32]>);

/// Decoded posters kept, in bytes. About 700 tiles at 1x, 180 at 2x.
const BUDGET: usize = 192 << 20;

/// The size a poster decodes to, in logical pixels: a grid tile's width before a column is
/// added, so a stretched tile stays sharp.
const DECODE_W: i32 = 320;
const DECODE_H: i32 = 480;

/// A poster decoded on a worker, ready to wrap as a texture.
pub struct Decoded {
    width: i32,
    height: i32,
    stride: usize,
    alpha: bool,
    pixels: glib::Bytes,
}

/// Fit `bytes` into `box_` (device pixels), keeping the aspect ratio.
fn decode(bytes: Vec<u8>, &(w, h): &(i32, i32)) -> Option<Decoded> {
    let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(bytes));
    let pixbuf =
        gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(&stream, w, h, true, gio::Cancellable::NONE)
            .ok()?;
    Some(Decoded {
        width: pixbuf.width(),
        height: pixbuf.height(),
        stride: pixbuf.rowstride() as usize,
        alpha: pixbuf.has_alpha(),
        pixels: pixbuf.read_pixel_bytes(),
    })
}

#[derive(Default)]
pub struct Art {
    base: RefCell<String>,
    pool: RefCell<Option<ArtPool<(i32, i32)>>>,
    textures: RefCell<Lru>,
    /// Pictures waiting for a title's art. A picture names the title it shows, so a tile
    /// shown again for another title never takes the first one's poster.
    waiting: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Picture>>>>,
    /// Titles with a job in the pool.
    asked: RefCell<HashSet<String>>,
    /// Titles with no poster that loads.
    missing: RefCell<HashSet<String>>,
}

impl Art {
    /// A new shelf: stop the old pool and start one for `source`. Textures stay, keyed by title
    /// id, which a store qualifies per host.
    pub fn reset(self: &Rc<Self>, source: Option<Source>) {
        self.pool.take();
        self.waiting.borrow_mut().clear();
        self.asked.borrow_mut().clear();
        self.missing.borrow_mut().clear();
        let Some((base, identity, pin)) = source else {
            return;
        };
        *self.base.borrow_mut() = base.clone();
        let (pool, rx) = ArtPool::start(base, identity, pin, decode);
        *self.pool.borrow_mut() = Some(pool);
        let art = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((id, decoded)) = rx.recv().await {
                let Some(art) = art.upgrade() else { break };
                art.land(id, decoded);
            }
        });
    }

    /// Screenshot scenes hand textures in directly.
    pub fn insert(&self, id: String, texture: gdk::Texture) {
        self.textures.borrow_mut().put(id, texture);
    }

    /// Put `game`'s poster on `pic`, now or once it arrives.
    pub fn show(&self, game: &GameEntry, pic: &gtk::Picture) {
        pic.set_widget_name(&game.id);
        if let Some(tex) = self.textures.borrow_mut().get(&game.id) {
            pic.set_paintable(Some(&tex));
            return;
        }
        pic.set_paintable(None::<&gdk::Paintable>);
        if self.missing.borrow().contains(&game.id) {
            return;
        }
        self.waiting
            .borrow_mut()
            .entry(game.id.clone())
            .or_default()
            .push(pic.downgrade());
        let pool = self.pool.borrow();
        let Some(pool) = pool.as_ref() else { return };
        if !self.asked.borrow_mut().insert(game.id.clone()) {
            return;
        }
        let candidates = game.art.poster_candidates(&self.base.borrow());
        if candidates.is_empty() {
            self.missing.borrow_mut().insert(game.id.clone());
            return;
        }
        let k = pic.scale_factor().max(1);
        let dropped = pool.push(ArtJob {
            id: game.id.clone(),
            candidates,
            hint: (DECODE_W * k, DECODE_H * k),
        });
        for id in dropped {
            self.asked.borrow_mut().remove(&id);
            self.waiting.borrow_mut().remove(&id);
        }
    }

    fn land(&self, id: String, decoded: Option<Decoded>) {
        self.asked.borrow_mut().remove(&id);
        let pics = self.waiting.borrow_mut().remove(&id).unwrap_or_default();
        let Some(d) = decoded else {
            self.missing.borrow_mut().insert(id);
            return;
        };
        let format = if d.alpha {
            gdk::MemoryFormat::R8g8b8a8
        } else {
            gdk::MemoryFormat::R8g8b8
        };
        let tex: gdk::Texture =
            gdk::MemoryTexture::new(d.width, d.height, format, &d.pixels, d.stride).upcast();
        for pic in pics.iter().filter_map(glib::WeakRef::upgrade) {
            if pic.widget_name() == id {
                pic.set_paintable(Some(&tex));
            }
        }
        self.textures.borrow_mut().put(id, tex);
    }
}

/// Textures by title, the least recently drawn dropped past [`BUDGET`].
#[derive(Default)]
struct Lru {
    map: HashMap<String, (gdk::Texture, usize, u64)>,
    bytes: usize,
    clock: u64,
}

impl Lru {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn get(&mut self, id: &str) -> Option<gdk::Texture> {
        let now = self.tick();
        let (tex, _, used) = self.map.get_mut(id)?;
        *used = now;
        Some(tex.clone())
    }

    fn put(&mut self, id: String, tex: gdk::Texture) {
        let size = tex.width() as usize * tex.height() as usize * 4;
        let now = self.tick();
        if let Some((_, old, _)) = self.map.insert(id, (tex, size, now)) {
            self.bytes -= old;
        }
        self.bytes += size;
        while self.bytes > BUDGET && self.map.len() > 1 {
            let Some(oldest) = self
                .map
                .iter()
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((_, size, _)) = self.map.remove(&oldest) {
                self.bytes -= size;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 600×900 cover decodes inside the tile's box, its shape kept.
    #[test]
    fn a_large_cover_decodes_to_the_tile_box() {
        let big =
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 600, 900)
                .expect("a pixbuf");
        big.fill(0x3584_e4ff);
        let jpeg = big.save_to_bufferv("jpeg", &[]).expect("a jpeg");
        let d = decode(jpeg, &(DECODE_W, DECODE_H)).expect("it decodes");
        assert_eq!((d.width, d.height), (DECODE_W, DECODE_H));
        assert!(!d.alpha);
        assert!(d.pixels.len() >= d.stride * (d.height as usize - 1));
        assert!(decode(b"not an image".to_vec(), &(DECODE_W, DECODE_H)).is_none());
    }

    /// Past the budget the least recently drawn texture goes first.
    #[test]
    fn the_cache_drops_the_least_recently_drawn() {
        let tex = || -> gdk::Texture {
            let px = glib::Bytes::from_owned(vec![0u8; 1024 * 1024 * 4]);
            gdk::MemoryTexture::new(1024, 1024, gdk::MemoryFormat::R8g8b8a8, &px, 1024 * 4).upcast()
        };
        let fits = BUDGET / (1024 * 1024 * 4);
        let mut lru = Lru::default();
        for i in 0..fits {
            lru.put(format!("t{i}"), tex());
        }
        assert!(
            lru.get("t0").is_some(),
            "t0 drawn again, so t1 is now the oldest"
        );
        lru.put("new".into(), tex());
        assert!(lru.get("t1").is_none());
        assert!(lru.get("t0").is_some() && lru.get("new").is_some());
        assert!(lru.bytes <= BUDGET);
    }
}
