//! A poster tile: a 2:3 face as wide as its column, the title and a caption under it. A tile is
//! built once and shown again for whatever its row binds next, so scrolling a large library
//! creates no widgets. Its menu is built when opened, from the title's state at that moment.

use super::rows::Caption;
use super::{LibraryMsg, View};
use adw::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, gio, glib};
use pf_client_core::library::{initials, store_label, GameEntry};
use pf_client_core::library_layout::ago;
use std::cell::RefCell;
use std::rc::Rc;

/// A face is never narrower than this: its badge and a launcher's mark still fit.
const MIN_W: i32 = 100;
/// A face's width where nothing stretches it, as in a band.
pub const NATURAL_W: i32 = 150;

/// The launcher marks this shell ships symbolic art for (`data/icons/…`).
const LAUNCHER_ICON_TOKENS: &[&str] = &[
    "steam", "lutris", "heroic", "playnite", "epic", "gog", "xbox",
];

/// What a tile shows now.
#[derive(Clone, Debug, Default, PartialEq)]
enum Shows {
    #[default]
    Nothing,
    Game(String),
    Desktop,
}

/// The widgets a tile updates when it is shown again.
struct Parts {
    face: gtk::Overlay,
    monogram: gtk::Label,
    /// A brand or Lucide mark in place of the monogram, and the token it draws.
    mark: RefCell<Option<(String, gtk::Widget)>>,
    placeholder: gtk::Box,
    picture: gtk::Picture,
    badge: gtk::Label,
    running: gtk::Label,
    title: gtk::Label,
    caption: gtk::Label,
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Face {
        pub(super) parts: RefCell<Option<Parts>>,
        pub(super) shows: RefCell<Shows>,
        /// A band's face: always the natural size. A box that is not homogeneous expects a
        /// height that does not grow with the width.
        pub(super) fixed: std::cell::Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Face {
        const NAME: &'static str = "PfPosterFace";
        type Type = super::Face;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Face {
        fn dispose(&self) {
            self.parts.take();
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Face {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            if self.fixed.get() {
                gtk::SizeRequestMode::ConstantSize
            } else {
                gtk::SizeRequestMode::HeightForWidth
            }
        }

        /// 2:3 at whatever width the column gives, or the natural size when fixed.
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let fixed = self.fixed.get();
            match orientation {
                gtk::Orientation::Horizontal if fixed => (NATURAL_W, NATURAL_W, -1, -1),
                gtk::Orientation::Horizontal => (MIN_W, NATURAL_W, -1, -1),
                _ if fixed => (NATURAL_W * 3 / 2, NATURAL_W * 3 / 2, -1, -1),
                _ => {
                    // With no width given, the least height is the narrowest face's.
                    let (min_w, nat_w) = if for_size < 0 {
                        (MIN_W, NATURAL_W)
                    } else {
                        (for_size, for_size)
                    };
                    (min_w * 3 / 2, nat_w * 3 / 2, -1, -1)
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                // Measured first: GTK refuses to allocate a widget it never sized.
                child.measure(gtk::Orientation::Horizontal, -1);
                child.allocate(width, height, baseline, None);
            }
        }
    }
}

glib::wrapper! {
    pub struct Face(ObjectSubclass<imp::Face>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

/// A tile: its button, which a row keeps, and its face, which holds the parts.
#[derive(Clone)]
pub struct Poster {
    button: gtk::Button,
    face: Face,
}

impl Poster {
    pub fn new(view: &Rc<View>) -> Poster {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Cover)
            .can_shrink(true)
            .build();
        let monogram = gtk::Label::builder()
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .build();
        let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
        placeholder.append(&monogram);
        let pill = |valign: gtk::Align| {
            gtk::Label::builder()
                .css_classes(["pf-pill", "pf-store-badge"])
                .halign(gtk::Align::Start)
                .valign(valign)
                .margin_start(6)
                .margin_top(6)
                .margin_bottom(6)
                .build()
        };
        let (badge, running) = (pill(gtk::Align::Start), pill(gtk::Align::End));
        running.set_label("Running");
        let overlay = gtk::Overlay::builder()
            .css_classes(["pf-poster"])
            .overflow(gtk::Overflow::Hidden)
            .child(&placeholder)
            .build();
        overlay.add_overlay(&picture);
        overlay.add_overlay(&badge);
        overlay.add_overlay(&running);
        let face: Face = glib::Object::new();
        overlay.set_parent(&face);
        let title = gtk::Label::builder()
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(16)
            .css_classes(["caption-heading"])
            .build();
        let caption = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .visible(false)
            .build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.append(&face);
        body.append(&title);
        body.append(&caption);
        let button = gtk::Button::builder()
            .child(&body)
            .css_classes(["flat", "pf-tile"])
            .hexpand(true)
            .build();
        face.imp().parts.replace(Some(Parts {
            face: overlay,
            monogram,
            mark: RefCell::default(),
            placeholder,
            picture,
            badge,
            running,
            title,
            caption,
        }));
        let poster = Poster { button, face };
        poster.wire(view);
        poster
    }

    /// The tile a row kept, from its button.
    pub fn of(w: &gtk::Widget) -> Option<Poster> {
        let button = w.downcast_ref::<gtk::Button>()?.clone();
        let face = button.child()?.first_child()?.downcast::<Face>().ok()?;
        Some(Poster { button, face })
    }

    /// Keep this tile at its natural size, as a band's are.
    pub fn set_fixed(&self) {
        self.face.imp().fixed.set(true);
        self.button.set_hexpand(false);
        self.face.queue_resize();
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    fn shows(&self) -> Shows {
        self.face.imp().shows.borrow().clone()
    }

    /// Click plays; right-click, a long press or the Menu key opens the title's menu.
    fn wire(&self, view: &Rc<View>) {
        let (view, face) = (Rc::downgrade(view), self.face.downgrade());
        let shows = move || face.upgrade().map(|f| f.imp().shows.borrow().clone());
        {
            let (view, shows) = (view.clone(), shows.clone());
            self.button.connect_clicked(move |_| {
                let Some(view) = view.upgrade() else { return };
                match shows() {
                    Some(Shows::Game(id)) => view.sender.emit(LibraryMsg::Play(id)),
                    Some(Shows::Desktop) => view.sender.emit(LibraryMsg::Desktop(None)),
                    _ => {}
                }
            });
        }
        let open = {
            let (view, shows, button) = (view.clone(), shows.clone(), self.button.downgrade());
            Rc::new(move |at: Option<(f64, f64)>| {
                if let (Some(view), Some(Shows::Game(id)), Some(button)) =
                    (view.upgrade(), shows(), button.upgrade())
                {
                    open_menu(&button, &view, &id, at);
                }
            })
        };
        let right = gtk::GestureClick::builder().button(3).build();
        {
            let open = open.clone();
            right.connect_pressed(move |_, _, x, y| open(Some((x, y))));
        }
        self.button.add_controller(right);
        let long = gtk::GestureLongPress::builder().touch_only(true).build();
        {
            let open = open.clone();
            long.connect_pressed(move |_, x, y| open(Some((x, y))));
        }
        self.button.add_controller(long);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, state| {
            let menu_key = key == gdk::Key::Menu
                || (key == gdk::Key::F10 && state.contains(gdk::ModifierType::SHIFT_MASK));
            if !menu_key {
                return glib::Propagation::Proceed;
            }
            open(None);
            glib::Propagation::Stop
        });
        self.button.add_controller(keys);
    }

    fn with_parts(&self, f: impl FnOnce(&Parts)) {
        if let Some(parts) = self.face.imp().parts.borrow().as_ref() {
            f(parts);
        }
    }

    fn set_shows(&self, shows: Shows) {
        let present = shows != Shows::Nothing;
        self.button.set_opacity(if present { 1.0 } else { 0.0 });
        self.button.set_can_target(present);
        self.button.set_can_focus(present);
        *self.face.imp().shows.borrow_mut() = shows;
    }

    pub fn show_game(&self, view: &View, g: &GameEntry, caption: Caption) {
        if self.shows() != Shows::Game(g.id.clone()) {
            self.set_shows(Shows::Game(g.id.clone()));
        }
        let launcher = g.is_launcher();
        self.with_parts(|p| {
            p.title.set_label(&g.title);
            self.button.set_tooltip_text(Some(&g.title));
            // Every tile of a captioned row keeps the line, so all its rows are one height.
            p.caption.set_visible(caption != Caption::None);
            let text = caption_text(g, caption);
            p.caption.set_label(text.as_deref().unwrap_or("\u{a0}"));
            set_class(p.face.upcast_ref(), "pf-launcher", launcher);
            p.badge.set_visible(true);
            p.badge.set_label(store_label(&g.store));
            set_class(p.badge.upcast_ref(), "pf-launcher", launcher);
            p.running
                .set_visible(view.running.borrow().contains_key(&g.id));
            // A launcher usually ships no poster: its mark, or its name on an accent face, says
            // "opens Steam" where a monogram would say a cover failed to load.
            let mark = g.art.is_empty().then(|| g.icon_token()).flatten();
            let mark = mark.filter(|t| {
                LAUNCHER_ICON_TOKENS.contains(t) || pf_client_core::lucide::path(t).is_some()
            });
            match mark {
                Some(token) => self.show_mark(p, token),
                None => {
                    self.show_mark(p, "");
                    let (text, class, other) = if launcher {
                        (
                            store_label(&g.store).to_string(),
                            "pf-poster-launcher-name",
                            "pf-poster-monogram",
                        )
                    } else {
                        (
                            initials(&g.title),
                            "pf-poster-monogram",
                            "pf-poster-launcher-name",
                        )
                    };
                    p.monogram.set_label(&text);
                    p.monogram.remove_css_class(other);
                    p.monogram.add_css_class(class);
                }
            }
            view.art.show(g, &p.picture);
        });
    }

    /// The host itself, in the grid when Desktops is off.
    pub fn show_desktop(&self) {
        self.set_shows(Shows::Desktop);
        self.with_parts(|p| {
            p.title.set_label("Desktop");
            self.button
                .set_tooltip_text(Some("Stream the host's desktop"));
            p.caption.set_visible(false);
            p.badge.set_visible(false);
            p.running.set_visible(false);
            set_class(p.face.upcast_ref(), "pf-launcher", true);
            self.show_mark(p, "monitor");
            p.picture.set_widget_name("");
            p.picture.set_paintable(None::<&gdk::Paintable>);
        });
    }

    /// An empty slot at the end of a short row: it keeps its width and takes no input.
    pub fn show_nothing(&self) {
        self.set_shows(Shows::Nothing);
        self.with_parts(|p| {
            p.picture.set_widget_name("");
            p.picture.set_paintable(None::<&gdk::Paintable>);
        });
    }

    /// Show `token`'s mark in place of the monogram; an empty token shows the monogram.
    fn show_mark(&self, p: &Parts, token: &str) {
        let mut mark = p.mark.borrow_mut();
        if mark.as_ref().map(|(t, _)| t.as_str()) == Some(token) {
            return;
        }
        if let Some((_, w)) = mark.take() {
            p.placeholder.remove(&w);
        }
        p.monogram.set_visible(token.is_empty());
        if token.is_empty() {
            return;
        }
        let w: gtk::Widget = if LAUNCHER_ICON_TOKENS.contains(&token) {
            let img = gtk::Image::from_icon_name(&format!("pf-launcher-{token}-symbolic"));
            img.set_pixel_size(72);
            img.upcast()
        } else {
            crate::widgets::lucide::icon(token, 56).upcast()
        };
        w.add_css_class("pf-poster-launcher-mark");
        w.set_halign(gtk::Align::Center);
        w.set_valign(gtk::Align::Center);
        w.set_vexpand(true);
        p.placeholder.append(&w);
        *mark = Some((token.to_string(), w));
    }
}

fn set_class(w: &gtk::Widget, class: &str, on: bool) {
    if on {
        w.add_css_class(class);
    } else {
        w.remove_css_class(class);
    }
}

/// Under Recently played or the Recent sort, when it was last played; under Most played, how
/// long. Nothing for a title never played here.
fn caption_text(g: &GameEntry, caption: Caption) -> Option<String> {
    let s = g.stats.as_ref()?;
    match caption {
        Caption::LastPlayed if s.last_played_unix_ms > 0 => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as u64);
            Some(ago(now.saturating_sub(s.last_played_unix_ms)))
        }
        Caption::PlayTime if s.play_time_ms > 0 => Some(match s.play_time_ms / 3_600_000 {
            0 => "Under an hour".into(),
            h => format!("{h} h"),
        }),
        _ => None,
    }
}

/// The title's menu, built from its state now: End Game only while the host can end it. The
/// rows name the page's `title.*` actions with the title's id.
fn open_menu(button: &gtk::Button, view: &View, id: &str, at: Option<(f64, f64)>) {
    let running = view.running.borrow().get(id).cloned();
    let favorite = view.favorites.borrow().iter().any(|f| f == id);
    let menu = gio::Menu::new();
    let item = |label: &str, action: &str| {
        let item = gio::MenuItem::new(Some(label), None);
        item.set_action_and_target_value(Some(action), Some(&id.to_variant()));
        menu.append_item(&item);
    };
    item(
        if running.is_some() { "Resume" } else { "Play" },
        "title.play",
    );
    item(
        if favorite {
            "Remove from Favorites"
        } else {
            "Add to Favorites"
        },
        "title.favorite",
    );
    item("Details\u{2026}", "title.details");
    item("Copy Link", "title.copy-link");
    if running.is_some_and(|r| r.endable) {
        item("End Game", "title.end-game");
    }
    let popover = gtk::PopoverMenu::from_model(Some(&menu));
    popover.set_parent(button);
    popover.set_has_arrow(false);
    if let Some((x, y)) = at {
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    }
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}
