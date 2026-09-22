//! Add or edit a host by address on the controller console.
//!
//! Deck never draws the tray: Steam's overlay types through SDL text input, so
//! pad events only dismiss the field. Elsewhere A raises the on-screen keyboard.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, EDGE_INSET, W};
use crate::widgets::{permits, Charset, KeyMsg, Keyboard, ListMsg, MenuList, RowSpec, ROW_MAX_W};
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
    pub(super) list: MenuList,
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
            // Pinned-card keys are `host\0preset`; only the host side is edited.
            edits: Some(host.key.split('\0').next().unwrap_or(&host.key).to_string()),
            ..AddHostScreen::new()
        }
    }

    pub(crate) fn title(&self) -> String {
        if self.edits.is_some() {
            "Edit Host".into()
        } else {
            "Add Host".into()
        }
    }

    fn commit_label(&self) -> &'static str {
        if self.edits.is_some() {
            "Save changes"
        } else {
            "Add host"
        }
    }

    /// A press outside the tray closes it; the row underneath is not activated.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.editing.is_some() && !ctx.deck {
            if !self.keyboard.covers(p) {
                if p.press() {
                    self.editing = None;
                    return true;
                }
                return false;
            }
            let (msg, _) = self.keyboard.pointer(p);
            match msg {
                KeyMsg::Type(c) => {
                    self.type_char(c);
                }
                KeyMsg::Backspace => {
                    self.backspace();
                }
                KeyMsg::Done => self.editing = None,
                KeyMsg::None => {}
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

    pub(crate) fn editing(&self) -> bool {
        self.editing.is_some()
    }

    fn can_add(&self) -> bool {
        !self.address.trim().is_empty() && self.port.parse::<u16>().is_ok_and(|p| p > 0)
    }

    fn field_mut(&mut self, f: Field) -> &mut String {
        match f {
            Field::Name => &mut self.name,
            Field::Address => &mut self.address,
            Field::Port => &mut self.port,
        }
    }

    fn charset(f: Field) -> Charset {
        match f {
            Field::Name => Charset::Free,
            Field::Address => Charset::Hostname,
            Field::Port => Charset::Digits,
        }
    }

    fn type_char(&mut self, ch: char) -> bool {
        let Some(f) = self.editing else { return false };
        if !permits(Self::charset(f), ch) {
            return false;
        }
        // u16 max is 65535 — five digits.
        if f == Field::Port && self.field_mut(f).chars().count() >= 5 {
            return false;
        }
        self.field_mut(f).push(ch);
        true
    }

    fn backspace(&mut self) -> bool {
        let Some(f) = self.editing else { return false };
        self.field_mut(f).pop().is_some()
    }

    pub(crate) fn text_input(&mut self, text: &str) {
        for ch in text.chars() {
            self.type_char(ch);
        }
    }

    pub(crate) fn edit_key(&mut self, key: crate::input::Key) -> bool {
        use crate::input::Key as K;
        if self.editing.is_none() {
            return false;
        }
        match key {
            K::Backspace => {
                self.backspace();
                true
            }
            K::Return | K::Escape => {
                self.editing = None;
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
        if let Some(_field) = self.editing {
            if ctx.deck {
                // Steam owns typing on Deck; the pad only dismisses the field.
                return match ev {
                    MenuEvent::Back | MenuEvent::Confirm => {
                        self.editing = None;
                        Some(MenuPulse::Confirm)
                    }
                    _ => None,
                };
            }
            let (msg, pulse) = self.keyboard.menu(ev);
            return match msg {
                KeyMsg::Type(c) => {
                    if self.type_char(c) {
                        Some(MenuPulse::Move)
                    } else {
                        Some(MenuPulse::Boundary)
                    }
                }
                KeyMsg::Backspace => {
                    if self.backspace() {
                        Some(MenuPulse::Move)
                    } else {
                        Some(MenuPulse::Boundary)
                    }
                }
                KeyMsg::Done => {
                    self.editing = None;
                    Some(MenuPulse::Confirm)
                }
                KeyMsg::None => pulse,
            };
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

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing.is_some() {
            if ctx.deck {
                return vec![
                    Hint::new(HintKey::Key("STEAM + X"), "Keyboard"),
                    Hint::new(HintKey::Confirm, "Done"),
                    Hint::new(HintKey::Back, "Done"),
                ];
            }
            return vec![
                Hint::new(HintKey::Confirm, "Type"),
                Hint::new(HintKey::Tertiary, "Delete"),
                Hint::new(HintKey::Back, "Done"),
            ];
        }
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Cancel"),
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
        // 2 px ≈ half a heading block, left-aligned to the title column.
        // Width is ROW_MAX_W * 0.72 so the line never runs under the controller chip.
        fonts.leading(
            canvas,
            "Hosts on this network appear automatically — add one by address for everything else.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + EDGE_INSET * k,
            f64::from(rect.top) + 2.0 * k,
            ROW_MAX_W * 0.72 * k,
        );

        let seat = self.keyboard.seat(self.editing.is_some() && !ctx.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top + (34.0 * k) as f32,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screens::Nav;
    use pf_client_core::trust::Settings;

    fn ctx<'a>(
        settings: &'a mut Settings,
        pads: &'a [pf_client_core::menu_nav::PadInfo],
        library: &'a crate::library::LibraryShared,
        deck: bool,
    ) -> Ctx<'a> {
        Ctx {
            hosts: &[],
            library,
            settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads,
            deck,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        }
    }

    #[test]
    fn end_to_end_add_flow() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut c = ctx(&mut settings, &[], &library, false);
        let mut s = AddHostScreen::new();
        let mut fx = Outbox::default();

        s.list.cursor = 3;
        s.menu(MenuEvent::Confirm, &mut c, &mut fx);
        assert_eq!(s.editing, Some(Field::Address));
        assert_eq!(s.list.cursor, 1);

        s.text_input("deck tower.local");
        assert_eq!(s.address, "decktower.local");
        s.edit_key(crate::input::Key::Backspace);
        assert_eq!(s.address, "decktower.loca");
        s.text_input("l");
        s.edit_key(crate::input::Key::Return);
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
        assert!(!s.type_char('x'), "digits only");
    }

    /// A drag over the tray must not scroll the form under it.
    #[test]
    fn the_list_pans_only_while_no_field_is_edited() {
        let mut screen = crate::screens::Screen::AddHost(AddHostScreen::new());
        assert!(screen.pan_list().is_some());
        if let crate::screens::Screen::AddHost(s) = &mut screen {
            s.editing = Some(Field::Port);
        }
        assert!(screen.pan_list().is_none());
    }

    #[test]
    fn deck_mode_never_uses_the_grid() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut c = ctx(&mut settings, &[], &library, true);
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
