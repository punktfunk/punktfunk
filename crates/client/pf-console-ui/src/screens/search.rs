//! Find a title on this host's shelf by name. The console's keyboard types it, or the
//! device's own (Steam's on a Deck); searching swaps this screen for the shelf filtered to
//! the titles whose name contains it, so Back from the results lands on the whole shelf.

use crate::glyphs::{Hint, HintKey};
use crate::model::HostRow;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen, ScreenView, TextEntry};
use crate::theme::Fonts;
use crate::widgets::{blurb, field_key, type_text, Entry, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Image, Rect};
use std::collections::HashMap;

/// The field, then the Search row.
const ROWS: usize = 2;

pub(crate) struct SearchScreen {
    host: HostRow,
    /// Covers the shelf already decoded, handed on so the results show them at once.
    art: HashMap<String, Image>,
    list: MenuList,
    keyboard: TextEntry,
    query: String,
    editing: bool,
}

impl SearchScreen {
    /// Opens typing: the field is the only reason to be here.
    pub(crate) fn new(host: &HostRow, art: &HashMap<String, Image>) -> SearchScreen {
        SearchScreen {
            host: host.clone(),
            art: art.clone(),
            list: MenuList::new(),
            keyboard: TextEntry::default(),
            query: String::new(),
            editing: true,
        }
    }

    /// A query takes any printable character.
    fn admits(_: &str, ch: char) -> bool {
        !ch.is_control()
    }

    fn activate(&mut self, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.list.cursor == 0 {
            self.editing = true;
            return Some(MenuPulse::Confirm);
        }
        self.search(fx)
    }

    /// Swap in the shelf of matches. An empty query goes back to typing instead.
    fn search(&mut self, fx: &mut Outbox) -> Option<MenuPulse> {
        let query = self.query.trim();
        if query.is_empty() {
            self.editing = true;
            return Some(MenuPulse::Boundary);
        }
        let mut shelf = super::library::LibraryScreen::new(&self.host);
        shelf.set_query(query);
        shelf.adopt_art(self.art.clone());
        fx.replace(Screen::Library(shelf));
        Some(MenuPulse::Confirm)
    }
}

impl ScreenView for SearchScreen {
    fn title(&self) -> String {
        format!("Search {}", self.host.name)
    }

    fn editing(&self) -> bool {
        self.editing
    }

    fn edit_field(&self) -> Option<crate::screens::EditField> {
        let query = self.query.as_str();
        self.editing
            .then(|| crate::screens::EditField::new("Title", query, false))
            .flatten()
    }

    fn text_input(&mut self, typed: &str) {
        if self.editing {
            type_text(&mut self.query, typed, Self::admits);
        }
    }

    /// Return closes the keyboard onto the Search row; the next Return searches.
    fn edit_key(&mut self, key: crate::input::Key, _ctx: &mut Ctx) -> bool {
        if !self.editing {
            return false;
        }
        let Some(entry) = field_key(key, &mut self.query) else {
            return false;
        };
        if entry != Entry::Stay {
            self.editing = false;
            self.list.cursor = 1;
        }
        true
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.editing {
            let (entry, pulse) =
                (self.keyboard).menu(ev, ctx.device, &mut self.query, Self::admits);
            return match entry {
                Entry::Stay => pulse,
                Entry::Close => {
                    self.editing = false;
                    pulse
                }
                Entry::Done => self.search(fx),
            };
        }
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, ROWS);
        match msg {
            ListMsg::Activate => self.activate(fx),
            _ => pulse,
        }
    }

    /// A press outside the tray closes it; the row underneath is not activated.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let tray = self
            .editing
            .then(|| (self.keyboard).pointer(p, ctx.device, &mut self.query, Self::admits));
        if let Some(entry) = tray.flatten() {
            match entry {
                None => return false,
                Some(Entry::Stay) => {}
                Some(Entry::Close) => self.editing = false,
                Some(Entry::Done) => {
                    self.search(fx);
                }
            }
            return true;
        }
        let (msg, pulse) = self.list.pointer(p, ROWS);
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        if matches!(msg, ListMsg::Activate) {
            self.activate(fx);
        }
        true
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing {
            return TextEntry::hints(ctx.device, "Search");
        }
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Cancel"),
        ]
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
        let below = blurb(
            canvas,
            fonts,
            "Finds the titles on this host whose name contains what you type.",
            rect,
            k,
        );
        self.keyboard.seat(self.editing, ctx.device, dt);
        let tray_h = self.keyboard.reserve(k);
        let list_rect = Rect::from_ltrb(
            rect.left,
            below.top,
            rect.right,
            rect.bottom - tray_h as f32,
        );
        let mut field = RowSpec::field("Title", self.query.clone(), "Part of a title");
        field.caret = self.editing;
        let rows = [
            field,
            RowSpec::action("Search", !self.query.trim().is_empty()),
        ];
        self.list.lift = self.keyboard.lift(rect, k);
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, !self.editing);
        self.keyboard.render(canvas, fonts, rect, k);
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
    use crate::screens::Nav;
    use pf_client_core::menu_nav::MenuDir;
    use pf_client_core::trust::Settings;

    fn host() -> HostRow {
        HostRow {
            addr: "10.0.0.5".into(),
            mgmt_port: 9778,
            ..HostRow::fixture("aa", "Desk")
        }
    }

    fn with_ctx<R>(system_keyboard: bool, f: impl FnOnce(&mut Ctx) -> R) -> R {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let device = crate::screens::Device {
            system_keyboard,
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            device: &device,
            ..Ctx::test(&mut settings, &library)
        };
        f(&mut ctx)
    }

    /// Typed text becomes the query, and searching swaps in the filtered shelf.
    #[test]
    fn typing_then_searching_opens_the_matching_shelf() {
        let mut s = SearchScreen::new(&host(), &HashMap::new());
        assert!(s.editing(), "it opens typing");
        s.text_input("star");
        let mut fx = Outbox::default();
        with_ctx(false, |ctx| s.menu(MenuEvent::Back, ctx, &mut fx));
        assert!(
            !s.editing() && fx.nav.is_none(),
            "Back only closes the keyboard"
        );
        let mut fx = Outbox::default();
        with_ctx(false, |ctx| {
            s.menu(MenuEvent::Move(MenuDir::Down), ctx, &mut fx);
            s.menu(MenuEvent::Confirm, ctx, &mut fx);
        });
        let Some(Nav::Replace(screen)) = fx.nav else {
            panic!("the results replace the search");
        };
        let Screen::Library(shelf) = *screen else {
            panic!("a shelf");
        };
        assert_eq!(shelf.title(), "Desk \u{b7} \u{201c}star\u{201d}");
    }

    /// Nothing typed: searching goes back to the keyboard instead of an empty shelf.
    #[test]
    fn an_empty_query_keeps_typing() {
        let mut s = SearchScreen::new(&host(), &HashMap::new());
        s.text_input("   ");
        let mut fx = Outbox::default();
        with_ctx(true, |ctx| s.menu(MenuEvent::Confirm, ctx, &mut fx));
        assert!(fx.nav.is_none());
        assert!(s.editing());
    }
}
