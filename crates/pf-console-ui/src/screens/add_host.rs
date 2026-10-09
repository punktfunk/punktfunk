//! Add or edit a host by address on the controller console.
//!
//! Deck never draws the tray: Steam's overlay types through SDL text input, so
//! pad events only dismiss the field. Elsewhere A raises the on-screen keyboard.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::Fonts;
use crate::widgets::{
    blurb, entry_hints, field_key, permits, type_text, Charset, Entry, Keyboard, ListMsg, MenuList,
    RowSpec,
};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Field {
    Name,
    Address,
    Port,
}

const FIELDS: [Field; 3] = [Field::Name, Field::Address, Field::Port];

pub(crate) struct AddHostScreen {
    list: MenuList,
    keyboard: Keyboard,
    name: String,
    address: String,
    port: String,
    editing: Option<Field>,
    /// `Some(host key)`: save over that host. `None`: append.
    edits: Option<String>,
}

impl AddHostScreen {
    pub(crate) fn new() -> AddHostScreen {
        AddHostScreen {
            list: MenuList::new(),
            keyboard: Keyboard::new(),
            name: String::new(),
            address: String::new(),
            port: "9777".into(),
            editing: None,
            edits: None,
        }
    }

    pub(crate) fn edit(host: &HostRow) -> AddHostScreen {
        AddHostScreen {
            name: host.name.clone(),
            address: host.addr.clone(),
            port: host.port.to_string(),
            edits: Some(host.host_key().to_string()),
            ..AddHostScreen::new()
        }
    }

    fn commit_label(&self) -> &'static str {
        if self.edits.is_some() {
            "Save changes"
        } else {
            "Add host"
        }
    }

    fn can_add(&self) -> bool {
        !self.address.trim().is_empty() && self.port.parse::<u16>().is_ok_and(|p| p > 0)
    }

    /// The open field, its text, and the keyboard that types into it.
    fn open(&mut self) -> Option<(Field, &mut Keyboard, &mut String)> {
        let f = self.editing?;
        let text = match f {
            Field::Name => &mut self.name,
            Field::Address => &mut self.address,
            Field::Port => &mut self.port,
        };
        Some((f, &mut self.keyboard, text))
    }

    fn charset(f: Field) -> Charset {
        match f {
            Field::Name => Charset::Free,
            Field::Address => Charset::Hostname,
            Field::Port => Charset::Digits,
        }
    }

    /// Whether field `f` takes `ch` after `text`. A port is five digits: u16 max is 65535.
    fn admits(f: Field, text: &str, ch: char) -> bool {
        permits(Self::charset(f), ch) && !(f == Field::Port && text.chars().count() >= 5)
    }

    fn activate(&mut self, msg: ListMsg, fx: &mut Outbox) {
        if !matches!(msg, ListMsg::Activate) {
            return;
        }
        if self.list.cursor < FIELDS.len() {
            self.editing = Some(FIELDS[self.list.cursor]);
            return;
        }
        if !self.can_add() {
            // Incomplete: jump to address instead of a dead press.
            self.list.cursor = 1;
            self.editing = Some(Field::Address);
            return;
        }
        let (name, addr) = (
            self.name.trim().to_string(),
            self.address.trim().to_string(),
        );
        let port = self.port.parse().unwrap_or(9777);
        match &self.edits {
            Some(key) => {
                // Same unnamed-host fallback as the store: nickname, else address.
                let label = if name.is_empty() {
                    addr.clone()
                } else {
                    name.clone()
                };
                fx.cmds.push(ConsoleCmd::UpdateHost {
                    key: key.clone(),
                    name,
                    addr,
                    port,
                });
                fx.toast = Some(format!("Saved {label}"));
            }
            None => {
                fx.toast = Some(format!("Added {addr}"));
                fx.cmds.push(ConsoleCmd::SaveHost { name, addr, port });
            }
        }
        fx.pop();
    }

    fn rows(&self) -> Vec<RowSpec> {
        let field_row = |label: &str, value: &str, placeholder: &str, f: Field| {
            let mut row = RowSpec::field(label, value.to_string(), placeholder);
            row.caret = self.editing == Some(f);
            row
        };
        vec![
            field_row(
                "Name",
                &self.name,
                "Optional — e.g. Living Room",
                Field::Name,
            ),
            field_row("Address", &self.address, "IP or hostname", Field::Address),
            field_row("Port", &self.port, "9777", Field::Port),
            RowSpec::action(self.commit_label(), self.can_add()),
        ]
    }
}

impl ScreenView for AddHostScreen {
    fn title(&self) -> String {
        if self.edits.is_some() {
            "Edit Host".into()
        } else {
            "Add Host".into()
        }
    }

    /// A press outside the tray closes it; the row underneath is not activated.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if let Some((f, keyboard, text)) = self.open().filter(|_| !ctx.device.deck) {
            let Some(entry) = keyboard.edit_pointer(p, text, |t, c| Self::admits(f, t, c)) else {
                return false;
            };
            if entry != Entry::Stay {
                self.editing = None;
            }
            return true;
        }
        let (msg, pulse) = self.list.pointer(p, FIELDS.len() + 1);
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, fx);
        true
    }

    fn editing(&self) -> bool {
        self.editing.is_some()
    }

    fn edit_field(&self) -> Option<crate::screens::EditField> {
        let (label, text) = match self.editing? {
            Field::Name => ("Name", &self.name),
            Field::Address => ("Address", &self.address),
            Field::Port => ("Port", &self.port),
        };
        crate::screens::EditField::new(label, text, self.editing == Some(Field::Port))
    }

    fn text_input(&mut self, typed: &str) {
        if let Some((f, _, text)) = self.open() {
            type_text(text, typed, |t, c| Self::admits(f, t, c));
        }
    }

    fn edit_key(&mut self, key: crate::input::Key, _ctx: &mut Ctx) -> bool {
        let Some((_, _, text)) = self.open() else {
            return false;
        };
        let Some(entry) = field_key(key, text) else {
            return false;
        };
        if entry != Entry::Stay {
            self.editing = None;
        }
        true
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        let deck = ctx.device.deck;
        if let Some((f, keyboard, text)) = self.open() {
            let (entry, pulse) = keyboard.edit_menu(ev, deck, text, |t, c| Self::admits(f, t, c));
            if entry != Entry::Stay {
                self.editing = None;
            }
            return pulse;
        }

        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, FIELDS.len() + 1);
        match msg {
            ListMsg::Activate => {
                self.activate(msg, fx);
                pulse
            }
            _ => pulse,
        }
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing.is_some() {
            return entry_hints(ctx.device.deck, "Done");
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
            "Hosts on this network appear automatically — add one by address for everything else.",
            rect,
            k,
        );

        let seat = self
            .keyboard
            .seat(self.editing.is_some() && !ctx.device.deck, dt);
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
        let rows = self.rows();
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.editing.is_none(),
        );
        if seat > 0.0 {
            self.keyboard.render(
                canvas,
                fonts,
                f64::from(rect.width()),
                f64::from(rect.bottom),
                seat,
                k,
            );
        }
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
    use pf_client_core::trust::Settings;

    /// The field's plate carries on into the keyboard: it starts row-wide, then lands on a key.
    #[test]
    fn the_plate_glides_from_the_field_into_the_keyboard() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut c = Ctx::test(&mut settings, &library);
        let mut s = AddHostScreen::new();
        let mut fx = Outbox::default();
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let rect = Rect::from_wh(1280.0, 800.0);
        let mut frame = |s: &mut AddHostScreen, c: &mut Ctx| {
            crate::el::begin_frame();
            s.render(surface.canvas(), rect, 1.0, 1.0 / 60.0, &fonts, c);
        };
        for _ in 0..30 {
            frame(&mut s, &mut c);
        }
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert!(s.editing.is_some());
        for _ in 0..2 {
            frame(&mut s, &mut c);
        }
        let first = s.keyboard.plate().expect("the plate came along");
        assert!(first.width() > 300.0, "row-wide at first: {first:?}");
        for _ in 0..90 {
            frame(&mut s, &mut c);
        }
        let landed = s.keyboard.plate().expect("on a key");
        assert!(landed.width() < 100.0, "a key wide: {landed:?}");
    }

    #[test]
    fn end_to_end_add_flow() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut c = Ctx::test(&mut settings, &library);
        let mut s = AddHostScreen::new();
        let mut fx = Outbox::default();

        s.list.cursor = 3;
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert_eq!(s.editing, Some(Field::Address));
        assert_eq!(s.list.cursor, 1);

        s.text_input("deck tower.local");
        assert_eq!(s.address, "decktower.local");
        s.edit_key(crate::input::Key::Backspace, &mut c);
        assert_eq!(s.address, "decktower.loca");
        s.text_input("l");
        s.edit_key(crate::input::Key::Return, &mut c);
        assert!(s.editing.is_none());

        s.list.cursor = 3;
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::SaveHost { addr, port: 9777, .. }) if addr == "decktower.local"
        ));
        assert!(matches!(fx.nav, Some(Nav::Pop)));
    }

    #[test]
    fn port_caps_at_five_digits() {
        let mut s = AddHostScreen::new();
        s.editing = Some(Field::Port);
        s.port.clear();
        s.text_input("123456789");
        assert_eq!(s.port, "12345");
        s.port.clear();
        s.text_input("x");
        assert!(s.port.is_empty(), "digits only");
    }

    /// A drag over the tray must not scroll the form under it.
    #[test]
    fn the_form_takes_a_drag_only_while_no_field_is_edited() {
        use crate::pointer::{Pointer, PointerKind};
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((1280, 800)).unwrap();
        let mut s = AddHostScreen::new();
        s.render(
            surface.canvas(),
            Rect::from_xywh(0.0, 0.0, 1280.0, 800.0),
            1.0,
            1.0 / 60.0,
            &fonts,
            &mut Ctx::test(&mut settings, &library),
        );
        let row = s.list.row_rect(0).expect("the form drew");
        let drag = Pointer {
            x: f64::from(row.center_x()),
            y: f64::from(row.center_y()),
            kind: PointerKind::PanStart { horizontal: false },
        };
        s.editing = Some(Field::Port);
        let mut screen = crate::screens::Screen::AddHost(s);
        assert!(!screen.pan(drag), "the tray is up");
        if let crate::screens::Screen::AddHost(s) = &mut screen {
            s.editing = None;
        }
        assert!(screen.pan(drag), "the form takes it");
    }

    #[test]
    fn deck_mode_never_uses_the_grid() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let deck = crate::screens::Device {
            deck: true,
            ..crate::screens::Device::test()
        };
        let mut c = Ctx {
            device: &deck,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = AddHostScreen::new();
        let mut fx = Outbox::default();
        s.list.cursor = 1;
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert!(s.editing());
        assert!(s
            .menu(
                MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Right),
                &mut c,
                &mut fx
            )
            .is_none());
        s.menu(MenuEvent::Back, &mut c, &mut fx);
        assert!(!s.editing());
        assert!(fx.nav.is_none(), "B closed the field, not the screen");
    }
}
