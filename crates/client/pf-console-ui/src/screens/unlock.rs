//! The prompt in front of a dial to a profile on a person's own account, which asks for its
//! password, or to a locked profile, which asks for its PIN; and its return when the host
//! refuses the unlock. The rules are `pf_client_core::unlock`. The console holds no device
//! key, so its one switch is "Stay signed in on this device".
//!
//! What was typed lives in one wiping buffer with room for the longest password the wire
//! takes, so typing never moves it, and goes when Sign In takes it or the screen closes. The
//! tray types it on every device, even where the device's own keyboard types other fields; for
//! a PIN it opens on the digits. Rows, announcements and the host's [`EditField`] show dots or
//! nothing. Back dials nothing.

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

pub(crate) struct UnlockScreen {
    /// The dial this prompt opens, its profile named.
    intent: ConnectIntent,
    /// The profile's name, and its host's.
    name: String,
    host: String,
    list: MenuList,
    keyboard: TextEntry,
    /// The password, or the PIN.
    password: Zeroizing<String>,
    /// The profile opens with its PIN, not an account password.
    pin: bool,
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

/// The PIN field takes a digit, a full-width one too, while it holds fewer than the longest PIN.
fn admits_digit(text: &str, ch: char) -> bool {
    let digit = ch.is_ascii_digit() || ('\u{FF10}'..='\u{FF19}').contains(&ch);
    digit && text.chars().count() < *unlock::PIN_LEN.end()
}

impl UnlockScreen {
    /// The prompt for `intent`'s profile `name` on `host`, `error` under the field; for its
    /// PIN when `pin`.
    pub(crate) fn new(
        intent: ConnectIntent,
        (name, host): (String, String),
        pin: bool,
        error: Option<String>,
    ) -> UnlockScreen {
        UnlockScreen {
            intent,
            name,
            host,
            list: MenuList::new(),
            keyboard: match pin {
                true => TextEntry::secret_digits(),
                false => TextEntry::secret(),
            },
            password: buffer(),
            pin,
            editing: false,
            stay: false,
            error,
        }
    }

    #[cfg(test)]
    pub(crate) fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn admits(&self) -> fn(&str, char) -> bool {
        match self.pin {
            true => admits_digit,
            false => admits,
        }
    }

    /// Sign In: the dial with this password or PIN, past the checks it already passed. One
    /// the wire refuses stays in the field with the reason under it.
    fn submit(&mut self, fx: &mut Outbox) -> Option<MenuPulse> {
        self.editing = false;
        let problem = match self.pin {
            true => unlock::pin_digits(self.password.as_bytes()).err(),
            false => unlock::password_problem(self.password.as_bytes()),
        };
        if let Some(line) = problem {
            self.error = Some(line.into());
            return Some(MenuPulse::Boundary);
        }
        // The buffer itself moves into the unlock, which wipes it; no copy is made.
        let typed = std::mem::replace(
            &mut *self.password,
            String::with_capacity(UNLOCK_PASSWORD_MAX),
        );
        let typed = Zeroizing::new(typed.into_bytes());
        let made = match self.pin {
            true => unlock::pin_unlock(&typed, self.stay, None),
            false => unlock::prompt_unlock(typed, self.stay, None),
        };
        match made {
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

    /// The field's label.
    fn label(&self) -> &'static str {
        match self.pin {
            true => pf_l10n::client::unlock_pin(),
            false => pf_l10n::client::unlock_password(),
        }
    }

    fn rows(&self) -> Vec<RowSpec> {
        let dots = "\u{2022}".repeat(self.password.chars().count());
        let placeholder = match self.pin {
            true => pf_l10n::client::unlock_pin_placeholder(),
            false => pf_l10n::client::unlock_password_placeholder(),
        };
        let mut field = RowSpec::field(self.label(), dots, placeholder);
        field.caret = self.editing;
        vec![
            field,
            RowSpec::toggle(pf_l10n::client::unlock_stay_signed_in(), self.stay),
            RowSpec::action(pf_l10n::client::unlock_sign_in(), !self.password.is_empty()),
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
        let field = self
            .editing
            .then(|| EditField::secret(self.label()))
            .flatten();
        field.map(|f| EditField {
            digits: self.pin,
            ..f
        })
    }

    fn text_input(&mut self, typed: &str) {
        if self.editing {
            let admits = self.admits();
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
            let admits = self.admits();
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
            let admits = self.admits();
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
            return self
                .keyboard
                .hints(ctx.device, pf_l10n::client::unlock_sign_in());
        }
        vec![
            Hint::new(HintKey::Confirm, pf_l10n::common::common_select()),
            Hint::new(HintKey::Back, pf_l10n::common::common_cancel()),
        ]
    }

    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        use pf_l10n::console as m;
        let empty = self.password.is_empty();
        let said = match self.list.cursor {
            PASSWORD if self.pin && empty => m::console_unlock_pin_empty_a11y(),
            PASSWORD if self.pin => m::console_unlock_pin_entered_a11y(),
            PASSWORD if empty => m::console_unlock_password_empty_a11y(),
            PASSWORD => m::console_unlock_password_entered_a11y(),
            STAY if self.stay => m::console_unlock_stay_signed_in_on_a11y(),
            STAY => m::console_unlock_stay_signed_in_off_a11y(),
            _ => pf_l10n::client::unlock_sign_in(),
        };
        Some(said.into())
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
        let intro = match self.pin {
            true => pf_l10n::client::unlock_pin_intro(&self.host),
            false => pf_l10n::client::unlock_intro(&self.host),
        };
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
        pf_l10n::client::unlock_title(&self.name)
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
