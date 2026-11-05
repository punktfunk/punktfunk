//! The password prompt in front of a dial to a profile on a person's own account, and its
//! return when the host refuses the unlock. The rules are `pf_client_core::unlock`. The
//! console holds no device key, so its one switch is "Stay signed in on this device".
//!
//! What was typed lives in one wiping buffer with room for the longest password the wire
//! takes, so typing never moves it. Rows, announcements and the host's [`EditField`] show
//! dots or nothing. Back dials nothing.

use crate::glyphs::{Hint, HintKey};
use crate::pointer::Pointer;
use crate::screens::{ConnectIntent, Ctx, EditField, Outbox, ScreenView, TextEntry};
use crate::theme::{Fonts, ERROR, W};
use crate::widgets::{blurb, field_key, type_text, Entry, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use pf_client_core::unlock;
use punktfunk_core::quic::v2::msg::{Zeroizing, UNLOCK_PASSWORD_MAX};
use skia_safe::{Canvas, Rect};

const PASSWORD: usize = 0;
const STAY: usize = 1;
const SIGN_IN: usize = 2;
const STAY_LABEL: &str = "Stay signed in on this device";

pub(crate) struct UnlockScreen {
    /// The dial this prompt opens, its profile named.
    intent: ConnectIntent,
    /// The profile's name, and its host's.
    name: String,
    host: String,
    list: MenuList,
    keyboard: TextEntry,
    password: Zeroizing<String>,
    editing: bool,
    stay: bool,
    /// The host's sentence after a refusal, or why the password can't be sent.
    error: Option<String>,
}

/// An empty buffer that holds the longest password without growing.
fn buffer() -> Zeroizing<String> {
    Zeroizing::new(String::with_capacity(UNLOCK_PASSWORD_MAX))
}

/// The field takes `ch` while the password stays inside the wire's bound, so the buffer never
/// reallocates and leaves a copy behind. Nothing typed is changed.
fn admits(text: &str, ch: char) -> bool {
    text.len() + ch.len_utf8() <= UNLOCK_PASSWORD_MAX
}

impl UnlockScreen {
    /// The prompt for `intent`'s profile `name` on `host`, `error` under the field.
    pub(crate) fn new(
        intent: ConnectIntent,
        name: String,
        host: String,
        error: Option<String>,
    ) -> UnlockScreen {
        UnlockScreen {
            intent,
            name,
            host,
            list: MenuList::new(),
            keyboard: TextEntry::default(),
            password: buffer(),
            editing: false,
            stay: false,
            error,
        }
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Sign In: the dial with this password, past the checks it already passed. A password
    /// the wire refuses stays in the field with the reason under it.
    fn submit(&mut self, fx: &mut Outbox) -> Option<MenuPulse> {
        self.editing = false;
        if let Some(line) = unlock::password_problem(self.password.as_bytes()) {
            self.error = Some(line.into());
            return Some(MenuPulse::Boundary);
        }
        // The buffer itself moves into the unlock, which wipes it; no copy is made.
        let typed = std::mem::replace(
            &mut *self.password,
            String::with_capacity(UNLOCK_PASSWORD_MAX),
        );
        let typed = Zeroizing::new(typed.into_bytes());
        match unlock::prompt_unlock(typed, self.stay, None) {
            Ok(u) => {
                fx.connect = Some(self.intent.clone());
                fx.unlock = Some(u);
                fx.pop();
                Some(MenuPulse::Confirm)
            }
            Err(line) => {
                self.error = Some(line.into());
                Some(MenuPulse::Boundary)
            }
        }
    }

    fn activate(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if msg != ListMsg::Activate {
            return pulse;
        }
        match self.list.cursor {
            STAY => self.stay = !self.stay,
            SIGN_IN if !self.password.is_empty() => return self.submit(fx),
            // The field, or Sign In with nothing typed: type first.
            _ => {
                self.list.cursor = PASSWORD;
                self.editing = true;
            }
        }
        pulse
    }

    fn rows(&self) -> Vec<RowSpec> {
        let dots = "\u{2022}".repeat(self.password.chars().count());
        let mut field = RowSpec::field("Password", dots, "Account password");
        field.caret = self.editing;
        vec![
            field,
            RowSpec::toggle(STAY_LABEL, self.stay),
            RowSpec::action("Sign In", !self.password.is_empty()),
        ]
    }

    /// An open field's end: Done signs in, anything else puts the field away.
    fn close(&mut self, entry: Entry, fx: &mut Outbox) -> Option<MenuPulse> {
        match entry {
            Entry::Stay => None,
            Entry::Close => {
                self.editing = false;
                None
            }
            Entry::Done => self.submit(fx),
        }
    }
}

impl ScreenView for UnlockScreen {
    fn editing(&self) -> bool {
        self.editing
    }

    fn edit_field(&self) -> Option<EditField> {
        self.editing
            .then(|| EditField::secret("Password"))
            .flatten()
    }

    fn text_input(&mut self, typed: &str) {
        if self.editing {
            type_text(&mut self.password, typed, admits);
        }
    }

    fn edit_key(&mut self, key: crate::input::Key, _ctx: &mut Ctx) -> bool {
        if !self.editing {
            return false;
        }
        let Some(entry) = field_key(key, &mut self.password) else {
            return false;
        };
        if entry != Entry::Stay {
            self.editing = false;
        }
        true
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if self.editing {
            let (entry, pulse) = self
                .keyboard
                .menu(ev, ctx.device, &mut self.password, admits);
            return self.close(entry, fx).or(pulse);
        }
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, SIGN_IN + 1);
        self.activate(msg, pulse, fx)
    }

    /// The raised keyboard is modal, as on the pair screen.
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if self.editing {
            if let Some(entry) = self
                .keyboard
                .pointer(p, ctx.device, &mut self.password, admits)
            {
                if let Some(entry) = entry {
                    self.close(entry, fx);
                }
                return entry.is_some();
            }
        }
        let (msg, pulse) = self.list.pointer(p, SIGN_IN + 1);
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, fx);
        true
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing {
            return TextEntry::hints(ctx.device, "Sign In");
        }
        vec![
            Hint::new(HintKey::Confirm, "Select"),
            Hint::new(HintKey::Back, "Cancel"),
        ]
    }

    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        Some(match self.list.cursor {
            PASSWORD if self.password.is_empty() => "Password, empty".into(),
            PASSWORD => "Password, entered".into(),
            STAY => format!("{STAY_LABEL}, {}", if self.stay { "on" } else { "off" }),
            _ => "Sign In".into(),
        })
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
        let ui = ctx.ui;
        let intro = format!("Enter the password for this account on {}.", self.host);
        let below = blurb(canvas, ui, fonts, &intro, rect, k);
        self.keyboard.seat(self.editing, ctx.device, dt);
        let tray_h = self.keyboard.reserve(k);
        // The error band; 34 matches the pair screen's.
        let status_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            below.top,
            rect.right,
            rect.bottom - tray_h as f32 - status_h as f32,
        );
        let rows = self.rows();
        self.list.lift = self.keyboard.lift(rect, k);
        self.list
            .render(canvas, ui, list_rect, &rows, fonts, k, dt, !self.editing);
        if let Some(err) = &self.error {
            let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
            let y = f64::from(rect.bottom) - tray_h - status_h + 6.0 * k;
            let max_w = f64::from(rect.width()) * 0.8;
            fonts.centered(canvas, err, W::Regular, 13.0 * k, ERROR, cx, y, max_w);
        }
        self.keyboard.render(canvas, ui, fonts, rect, k);
    }

    fn title(&self) -> String {
        format!("Sign in as {}", self.name)
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

#[cfg(test)]
mod tests;
