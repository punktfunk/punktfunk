//! Find a title on this host's shelf by name. The console's keyboard types it (Steam's on a
//! Deck); searching swaps this screen for the shelf filtered to the titles whose name
//! contains it, so Back from the results lands on the whole shelf.

use crate::glyphs::{Hint, HintKey};
use crate::model::HostRow;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, Screen};
use crate::theme::Fonts;
use crate::widgets::{blurb, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Image, Rect};
use std::collections::HashMap;

/// The field, then the Search row.
const ROWS: usize = 2;

pub(crate) struct SearchScreen {
    host: HostRow,
    /// Covers the shelf already decoded, handed on so the results show them at once.
    art: HashMap<String, Image>,
    pub(super) list: MenuList,
    keyboard: Keyboard,
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
            keyboard: Keyboard::new(),
            query: String::new(),
            editing: true,
        }
    }

    pub(crate) fn title(&self) -> String {
        format!("Search {}", self.host.name)
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing
    }

    pub(crate) fn edit_field(&self) -> Option<crate::screens::EditField> {
        let query = self.query.as_str();
        self.editing
            .then(|| crate::screens::EditField::new("Title", query, false))
            .flatten()
    }

    fn type_char(&mut self, ch: char) -> bool {
        if !self.editing || ch.is_control() {
            return false;
        }
        self.query.push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        self.editing && self.query.pop().is_some()
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    /// Return closes the keyboard onto the Search row; the next Return searches.
    pub(crate) fn edit_key(&mut self, key: crate::input::Key) -> bool {
        use crate::input::Key as K;
        if !self.editing {
            return false;
        }
        match key {
            K::Backspace => {
                self.backspace();
                true
            }
            K::Return | K::Escape => {
                self.editing = false;
                self.list.cursor = 1;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if self.editing {
            if ev == MenuEvent::Back {
                self.editing = false;
                return Some(MenuPulse::Confirm);
            }
            if ctx.deck {
                // Steam types on a Deck; the pad only searches or dismisses.
                return match ev {
                    MenuEvent::Confirm => self.search(fx),
                    _ => None,
                };
            }
            let (msg, pulse) = self.keyboard.menu(ev);
            let moved = |ok: bool| {
                Some(if ok {
                    MenuPulse::Move
                } else {
                    MenuPulse::Boundary
                })
            };
            return match msg {
                KeyMsg::Type(c) => moved(self.type_char(c)),
                KeyMsg::Backspace => moved(self.backspace()),
                KeyMsg::Done => self.search(fx),
                KeyMsg::None => pulse,
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
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.editing && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.editing = false;
                    return true;
                }
                return false;
            }
            match self.keyboard.pointer(p).0 {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    self.backspace();
                }
                KeyMsg::Done => {
                    self.search(fx);
                }
                KeyMsg::None => {}
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

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        match (self.editing, ctx.deck) {
            (true, true) => vec![
                Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                Hint::new(HintKey::Confirm, "Search"),
                Hint::new(HintKey::Back, "Done"),
            ],
            (true, false) => vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ],
            (false, _) => vec![
                Hint::new(HintKey::Confirm, "Select"),
                Hint::new(HintKey::Back, "Cancel"),
            ],
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
        let below = blurb(
            canvas,
            fonts,
            "Finds the titles on this host whose name contains what you type.",
            rect,
            k,
        );
        let seat = self.keyboard.seat(self.editing && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
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
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, !self.editing);
        if seat > 0.0 {
            let (w, bottom) = (f64::from(rect.width()), f64::from(rect.bottom));
            self.keyboard.render(canvas, fonts, w, bottom, seat, k);
        }
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

    fn with_ctx<R>(deck: bool, f: impl FnOnce(&mut Ctx) -> R) -> R {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &[],
            deck,
            tv: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
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
