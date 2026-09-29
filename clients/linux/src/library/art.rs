//! Posters, fetched as they are drawn. A poster bound on screen asks for its art; the asks of
//! one frame go to the core fetcher as one batch (disk cache first, then the host), and each
//! texture lands on every picture still showing that title. A shelf change closes the old run.

use gtk::prelude::*;
use gtk::{gdk, glib};
use pf_client_core::library::{self, GameEntry};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// Where a shelf's art comes from: the host's base URL, this device's identity, the host's pin.
pub type Source = (String, (String, String), Option<[u8; 32]>);

/// A title id and its poster's bytes.
type Arrival = (String, Vec<u8>);

#[derive(Default)]
pub struct Art {
    source: RefCell<Option<Source>>,
    /// Every poster seen this session stays decoded. An LRU goes here if memory ever matters.
    textures: RefCell<HashMap<String, gdk::Texture>>,
    /// Pictures waiting for a title's art.
    waiting: RefCell<HashMap<String, Vec<glib::WeakRef<gtk::Picture>>>>,
    asked: RefCell<HashSet<String>>,
    pending: RefCell<VecDeque<(String, Vec<String>)>>,
    flush_queued: Cell<bool>,
    /// The live runs' channels, closed on a shelf change so their workers stop.
    runs: RefCell<Vec<async_channel::Receiver<Arrival>>>,
}

impl Art {
    /// A new shelf: forget what was asked and stop the old fetches. Textures stay, keyed by
    /// title id, which a store qualifies per host.
    pub fn reset(&self, source: Option<Source>) {
        for rx in self.runs.borrow_mut().drain(..) {
            rx.close();
        }
        self.waiting.borrow_mut().clear();
        self.asked.borrow_mut().clear();
        self.pending.borrow_mut().clear();
        *self.source.borrow_mut() = source;
    }

    /// Screenshot scenes hand textures in directly.
    pub fn insert(&self, id: String, texture: gdk::Texture) {
        self.textures.borrow_mut().insert(id, texture);
    }

    /// Put `game`'s poster on `pic`, now or once it arrives.
    pub fn show(self: &Rc<Self>, game: &GameEntry, pic: &gtk::Picture) {
        if let Some(tex) = self.textures.borrow().get(&game.id) {
            pic.set_paintable(Some(tex));
            return;
        }
        pic.set_paintable(None::<&gdk::Paintable>);
        self.waiting
            .borrow_mut()
            .entry(game.id.clone())
            .or_default()
            .push(pic.downgrade());
        let Some((base, _, _)) = self.source.borrow().clone() else {
            return;
        };
        if !self.asked.borrow_mut().insert(game.id.clone()) {
            return;
        }
        let candidates = game.art.poster_candidates(&base);
        if candidates.is_empty() {
            return;
        }
        self.pending
            .borrow_mut()
            .push_back((game.id.clone(), candidates));
        if !self.flush_queued.replace(true) {
            let art = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(art) = art.upgrade() {
                    art.flush();
                }
            });
        }
    }

    fn flush(self: &Rc<Self>) {
        self.flush_queued.set(false);
        let jobs: VecDeque<_> = self.pending.borrow_mut().drain(..).collect();
        let Some((base, identity, pin)) = self.source.borrow().clone() else {
            return;
        };
        if jobs.is_empty() {
            return;
        }
        let rx = library::spawn_art_fetch(base, identity, pin, jobs);
        self.runs.borrow_mut().push(rx.clone());
        let art = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok((id, bytes)) = rx.recv().await {
                let Some(art) = art.upgrade() else { break };
                // Posters are tens of KB; decoding on the main loop keeps this simple.
                match gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                    Ok(tex) => art.land(id, tex),
                    Err(e) => tracing::debug!(%id, error = %e, "undecodable poster"),
                }
            }
        });
    }

    fn land(&self, id: String, tex: gdk::Texture) {
        for pic in self.waiting.borrow_mut().remove(&id).unwrap_or_default() {
            if let Some(pic) = pic.upgrade() {
                pic.set_paintable(Some(&tex));
            }
        }
        self.textures.borrow_mut().insert(id, tex);
    }
}
