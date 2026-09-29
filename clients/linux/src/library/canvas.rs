//! The Library's rows, laid out virtually. Every row of a kind has one height at a given width,
//! so the tops of thousands of rows are a running sum, and only the rows on screen hold widgets.
//! A row leaving the screen goes back to a pool of its kind and is shown again for the next one.
//!
//! GTK's own list keeps up to 200 rows alive around its anchor and rebinds them as the anchor
//! moves; with a grid row of posters each, that is the cost this widget exists to avoid.

use super::rows::{Caption, Row};
use super::{tile, View};
use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Pixels bound past each edge, so a slow scroll finds the next row ready.
const OVERSCAN: i32 = 400;

/// A row's kind, which fixes its height at a width: a band's or a grid row's caption line
/// shows in every tile or in none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Kind {
    Heading,
    Desktops,
    Band(Caption),
    Posters(Caption),
}

fn kind(row: &Row) -> Kind {
    match row {
        Row::Heading(_) => Kind::Heading,
        Row::Desktops => Kind::Desktops,
        Row::Band { caption, .. } => Kind::Band(*caption),
        Row::Posters { caption, .. } => Kind::Posters(*caption),
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Canvas {
        pub(super) view: RefCell<Option<Rc<View>>>,
        pub(super) rows: RefCell<Vec<Row>>,
        /// Row tops at `laid_w`, one past the last row being the total height.
        pub(super) tops: RefCell<Vec<i32>>,
        /// The content width the tops were summed at; 0 when rows or captions changed.
        pub(super) laid_w: Cell<i32>,
        pub(super) max_w: Cell<i32>,
        pub(super) live: RefCell<HashMap<usize, gtk::Box>>,
        pub(super) pool: RefCell<HashMap<Kind, Vec<gtk::Box>>>,
        pub(super) hadj: RefCell<Option<gtk::Adjustment>>,
        pub(super) vadj: RefCell<Option<gtk::Adjustment>>,
        pub(super) watch: RefCell<Option<(gtk::Adjustment, glib::SignalHandlerId)>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Canvas {
        const NAME: &'static str = "PfLibraryCanvas";
        type Type = super::Canvas;
        type ParentType = gtk::Widget;
        type Interfaces = (gtk::Scrollable,);
    }

    impl ObjectImpl for Canvas {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPS: std::sync::OnceLock<Vec<glib::ParamSpec>> = std::sync::OnceLock::new();
            PROPS.get_or_init(|| {
                [
                    "hadjustment",
                    "vadjustment",
                    "hscroll-policy",
                    "vscroll-policy",
                ]
                .into_iter()
                .map(glib::ParamSpecOverride::for_interface::<gtk::Scrollable>)
                .collect()
            })
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let adj = || value.get::<Option<gtk::Adjustment>>().ok().flatten();
            match pspec.name() {
                "hadjustment" => {
                    self.hadj.replace(adj());
                }
                "vadjustment" => self.set_vadjustment(adj()),
                // Always Minimum: the rows fill the width and the height is theirs.
                _ => {}
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "hadjustment" => self.hadj.borrow().to_value(),
                "vadjustment" => self.vadj.borrow().to_value(),
                _ => gtk::ScrollablePolicy::Minimum.to_value(),
            }
        }

        fn dispose(&self) {
            if let Some((adj, id)) = self.watch.take() {
                adj.disconnect(id);
            }
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Canvas {
        /// Nothing: the scrolled window gives the size, and the adjustments carry the rest.
        fn measure(&self, _o: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (0, 0, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            self.lay_out(width, height);
        }
    }

    impl ScrollableImpl for Canvas {}

    impl Canvas {
        fn set_vadjustment(&self, adj: Option<gtk::Adjustment>) {
            if let Some((old, id)) = self.watch.take() {
                old.disconnect(id);
            }
            if let Some(adj) = &adj {
                let canvas = self.obj().downgrade();
                let id = adj.connect_value_changed(move |_| {
                    if let Some(c) = canvas.upgrade() {
                        c.queue_allocate();
                    }
                });
                self.watch.replace(Some((adj.clone(), id)));
            }
            self.vadj.replace(adj);
        }

        /// Sum the tops at `w` when they are stale, bind the rows in view, place them.
        fn lay_out(&self, width: i32, height: i32) {
            let w = width.min(self.max_w.get().max(1));
            let x0 = (width - w) / 2;
            if self.laid_w.get() != w {
                self.sum_tops(w);
            }
            let tops = self.tops.borrow();
            let total = tops.last().copied().unwrap_or(0);
            let value = match self.vadj.borrow().as_ref() {
                Some(adj) => {
                    let value = adj.value().clamp(0.0, f64::from((total - height).max(0)));
                    let h = f64::from(height);
                    adj.configure(value, 0.0, f64::from(total.max(height)), 48.0, h * 0.9, h);
                    value as i32
                }
                None => 0,
            };
            if let Some(adj) = self.hadj.borrow().as_ref() {
                // The page size is the content width: the page reads its columns from it.
                let w = f64::from(w);
                adj.configure(0.0, 0.0, w, 1.0, w, w);
            }
            let rows = self.rows.borrow();
            let n = rows.len();
            let first = tops
                .partition_point(|&t| t <= value - OVERSCAN)
                .saturating_sub(1);
            let end = tops[..n].partition_point(|&t| t < value + height + OVERSCAN);
            // Rows that left the view go back to their pool.
            let gone: Vec<usize> = self
                .live
                .borrow()
                .keys()
                .copied()
                .filter(|i| *i < first || *i >= end)
                .collect();
            for i in gone {
                let holder = self.live.borrow_mut().remove(&i);
                if let Some(holder) = holder {
                    self.release(rows.get(i).map(kind), holder);
                }
            }
            for i in first..end.min(n) {
                let kept = self.live.borrow().get(&i).cloned();
                let holder = match kept {
                    Some(h) => h,
                    None => {
                        let h = self.take(kind(&rows[i]));
                        self.bind(&h, &rows[i]);
                        self.live.borrow_mut().insert(i, h.clone());
                        h
                    }
                };
                holder.set_child_visible(true);
                let h = tops[i + 1] - tops[i];
                let w = fit(&holder, w);
                holder.measure(gtk::Orientation::Vertical, w);
                let at = gtk::graphene::Point::new(x0 as f32, (tops[i] - value) as f32);
                holder.allocate(w, h, -1, Some(gtk::gsk::Transform::new().translate(&at)));
            }
        }

        /// One height per kind at `w`, from a row of that kind actually bound and measured.
        fn sum_tops(&self, w: i32) {
            let rows = self.rows.borrow();
            let mut heights: HashMap<Kind, i32> = HashMap::new();
            let mut tops = Vec::with_capacity(rows.len() + 1);
            let mut y = 0;
            for row in rows.iter() {
                tops.push(y);
                let k = kind(row);
                let h = match heights.get(&k) {
                    Some(h) => *h,
                    None => {
                        let h = self.measure_kind(k, row, w);
                        heights.insert(k, h);
                        h
                    }
                };
                y += h;
            }
            tops.push(y);
            self.tops.replace(tops);
            self.laid_w.set(w);
        }

        fn measure_kind(&self, k: Kind, row: &Row, w: i32) -> i32 {
            let probe = self.take(k);
            self.bind(&probe, row);
            let w = fit(&probe, w);
            let (_, natural, _, _) = probe.measure(gtk::Orientation::Vertical, w);
            self.release(Some(k), probe);
            natural
        }

        fn bind(&self, holder: &gtk::Box, row: &Row) {
            if let Some(view) = self.view.borrow().as_ref() {
                tile::bind(holder, view, row);
            }
        }

        /// A holder of kind `k` from the pool, or a new one.
        fn take(&self, k: Kind) -> gtk::Box {
            if let Some(h) = self.pool.borrow_mut().get_mut(&k).and_then(Vec::pop) {
                return h;
            }
            let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
            holder.set_margin_start(8);
            holder.set_margin_end(8);
            holder.set_margin_top(6);
            holder.set_margin_bottom(6);
            holder.set_child_visible(false);
            holder.set_parent(&*self.obj());
            holder
        }

        pub(super) fn release(&self, k: Option<Kind>, holder: gtk::Box) {
            holder.set_child_visible(false);
            match k {
                Some(k) => self.pool.borrow_mut().entry(k).or_default().push(holder),
                None => holder.unparent(),
            }
        }
    }
}

/// `w`, or the row's own minimum when that is wider: for one layout after a resize a grid row
/// can still have the columns of the old width. The overflow is clipped until they follow.
fn fit(holder: &gtk::Box, w: i32) -> i32 {
    w.max(holder.measure(gtk::Orientation::Horizontal, -1).0)
}

glib::wrapper! {
    pub struct Canvas(ObjectSubclass<imp::Canvas>)
        @extends gtk::Widget,
        @implements gtk::Scrollable, gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Canvas {
    pub fn new(view: &Rc<View>, max_width: i32) -> Canvas {
        let canvas: Canvas = glib::Object::new();
        canvas.imp().view.replace(Some(view.clone()));
        canvas.imp().max_w.set(max_width);
        canvas.set_overflow(gtk::Overflow::Hidden);
        canvas
    }

    /// Show `rows`. The rows on screen bind again, since equal rows may now show new data.
    pub fn set_rows(&self, rows: Vec<Row>) {
        let imp = self.imp();
        let live: Vec<(usize, gtk::Box)> = imp.live.borrow_mut().drain().collect();
        {
            let old = imp.rows.borrow();
            for (i, holder) in live {
                imp.release(old.get(i).map(kind), holder);
            }
        }
        imp.rows.replace(rows);
        imp.laid_w.set(0);
        self.queue_allocate();
    }
}
