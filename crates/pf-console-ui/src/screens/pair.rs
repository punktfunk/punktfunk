//! PIN pairing on the controller console — counterpart of the desktop PairSheet.
//!
//! Type the PIN the host shows (on-screen tray, or Steam's keyboard on Deck).
//! SPAKE2 runs on the binary's service thread; success pins the host and the
//! shell pops Home. A discovered host with an advertised fingerprint also
//! offers Request access (connect and wait for operator approval). A typed
//! host with no advert is PIN-only.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow, PairPhase};
use crate::pointer::Pointer;
use crate::screens::{ConnectIntent, Ctx, Outbox};
use crate::theme::{fg, Fonts, ERROR, W};
use crate::widgets::{
    blurb, entry_hints, field_key, permits, type_text, Charset, Entry, Keyboard, ListMsg, MenuList,
    RowSpec,
};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Pin,
    Device,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    RequestAccess,
    Pin,
    Device,
    Pair,
}

pub(crate) struct PairScreen {
    host_name: String,
    addr: String,
    port: u16,
    /// Empty = typed host with no advert, so no Request access row.
    fp_hex: String,
    pub(super) list: MenuList,
    keyboard: Keyboard,
    pin: String,
    device: String,
    editing: Option<Field>,
    /// Local Busy so a second A cannot double-submit before the service thread sees the command.
    busy: bool,
    error: Option<String>,
}

impl PairScreen {
    pub(crate) fn new(host: &HostRow, device_name: &str) -> PairScreen {
        PairScreen {
            host_name: host.name.clone(),
            addr: host.addr.clone(),
            port: host.port,
            fp_hex: host.fp_hex.clone(),
            list: MenuList::new(),
            keyboard: Keyboard::new(),
            pin: String::new(),
            device: device_name.to_string(),
            editing: None,
            busy: false,
            error: None,
        }
    }

    /// Stable across `busy` so the row list never reshuffles mid-ceremony.
    fn can_request(&self) -> bool {
        !self.fp_hex.is_empty()
    }

    /// Shared by `rows` and `activate` so the cursor cannot fire a stale row.
    fn roles(&self) -> Vec<Role> {
        let mut roles = Vec::with_capacity(4);
        if self.can_request() {
            roles.push(Role::RequestAccess);
        }
        roles.push(Role::Pin);
        roles.push(Role::Device);
        roles.push(Role::Pair);
        roles
    }

    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    /// The address and port its PIN goes to.
    pub(crate) fn target(&self) -> (&str, u16) {
        (&self.addr, self.port)
    }

    /// Paired is popped by the shell, not this screen.
    pub(crate) fn apply_phase(&mut self, phase: &PairPhase) {
        match phase {
            PairPhase::Busy => self.busy = true,
            PairPhase::Failed(msg) => {
                self.busy = false;
                self.error = Some(msg.clone());
            }
            PairPhase::Idle | PairPhase::Paired { .. } => self.busy = false,
        }
    }

    pub(crate) fn editing(&self) -> bool {
        self.editing.is_some()
    }

    pub(crate) fn edit_field(&self) -> Option<crate::screens::EditField> {
        match self.editing? {
            Field::Pin => crate::screens::EditField::new("PIN", &self.pin, true),
            Field::Device => crate::screens::EditField::new("Device name", &self.device, false),
        }
    }

    fn can_pair(&self) -> bool {
        !self.pin.trim().is_empty() && !self.busy
    }

    /// The open field, its text, and the keyboard that types into it.
    fn open(&mut self) -> Option<(Field, &mut Keyboard, &mut String)> {
        let f = self.editing?;
        let text = match f {
            Field::Pin => &mut self.pin,
            Field::Device => &mut self.device,
        };
        Some((f, &mut self.keyboard, text))
    }

    fn charset(f: Field) -> Charset {
        match f {
            Field::Pin => Charset::Digits,
            Field::Device => Charset::Free,
        }
    }

    /// Whether field `f` takes `ch` after `text`. PINs are 4 digits today; 8 is headroom,
    /// not a passphrase.
    fn admits(f: Field, text: &str, ch: char) -> bool {
        permits(Self::charset(f), ch) && !(f == Field::Pin && text.chars().count() >= 8)
    }

    pub(crate) fn text_input(&mut self, typed: &str) {
        if let Some((f, _, text)) = self.open() {
            type_text(text, typed, |t, c| Self::admits(f, t, c));
        }
    }

    pub(crate) fn edit_key(&mut self, key: crate::input::Key) -> bool {
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

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let deck = ctx.device.deck;
        if let Some((f, keyboard, text)) = self.open() {
            let (entry, pulse) = keyboard.edit_menu(ev, deck, text, |t, c| Self::admits(f, t, c));
            if entry != Entry::Stay {
                self.editing = None;
            }
            return pulse;
        }

        if ev == MenuEvent::Back {
            // Leave is fine mid-ceremony: success still pins and toasts globally.
            fx.pop();
            return None;
        }
        let roles = self.roles();
        let (msg, pulse) = self.list.menu(ev, roles.len());
        self.activate(msg, pulse, &roles, ctx, fx)
    }

    /// Raised keyboard is modal: hits on it stay here; a press outside closes it rather
    /// than reaching the row underneath.
    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        if let Some((f, keyboard, text)) = self.open().filter(|_| !ctx.device.deck) {
            let Some(entry) = keyboard.edit_pointer(p, text, |t, c| Self::admits(f, t, c)) else {
                return false;
            };
            if entry != Entry::Stay {
                self.editing = None;
            }
            return true;
        }
        let roles = self.roles();
        let (msg, pulse) = self.list.pointer(p, roles.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, &roles, ctx, fx);
        true
    }

    fn activate(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        roles: &[Role],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match msg {
            ListMsg::Activate => {
                match roles.get(self.list.cursor) {
                    Some(Role::RequestAccess) if !self.busy => {
                        // Leave so a canceled or finished approval returns to Home, not here.
                        fx.connect = Some(ConnectIntent {
                            addr: self.addr.clone(),
                            port: self.port,
                            fp_hex: self.fp_hex.clone(),
                            launch: None,
                            title: self.host_name.clone(),
                            request_access: true,
                            preset: None,
                        });
                        fx.pop();
                    }
                    Some(Role::Pin) => self.editing = Some(Field::Pin),
                    Some(Role::Device) => self.editing = Some(Field::Device),
                    Some(Role::Pair) if self.can_pair() => {
                        self.busy = true;
                        self.error = None;
                        fx.cmds.push(ConsoleCmd::Pair {
                            addr: self.addr.clone(),
                            port: self.port,
                            pin: self.pin.trim().to_string(),
                            device_name: if self.device.trim().is_empty() {
                                ctx.device.name.clone()
                            } else {
                                self.device.trim().to_string()
                            },
                        });
                    }
                    _ => {
                        // No PIN yet, or Request while busy: open the PIN field, not a dead press.
                        if let Some(i) = roles.iter().position(|r| *r == Role::Pin) {
                            self.list.cursor = i;
                        }
                        self.editing = Some(Field::Pin);
                    }
                }
                pulse
            }
            _ => pulse,
        }
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing.is_some() {
            return entry_hints(ctx.device.deck, "Done");
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
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        let intro = if self.can_request() {
            "Request access and approve this device on the host, or enter the PIN it shows."
        } else {
            "Enter the PIN from the host's web console (Pairing page) or its log."
        };
        let below = blurb(canvas, fonts, intro, rect, k);

        let seat = self
            .keyboard
            .seat(self.editing.is_some() && !ctx.device.deck, dt);
        let tray_h = if seat > 0.0 {
            (Keyboard::tray_height() + 12.0) * k * seat
        } else {
            0.0
        };
        // Status band (spinner / error); 34 matches the settings detail band.
        let status_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            below.top,
            rect.right,
            rect.bottom - tray_h as f32 - status_h as f32,
        );
        let rows = self.rows();
        self.list.render(
            canvas,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.editing.is_none() && !self.busy,
        );

        let status_y = f64::from(rect.bottom) - tray_h - status_h + 6.0 * k;
        if self.busy {
            crate::theme::spinner(canvas, cx - 70.0 * k, status_y + 8.0 * k, 7.0 * k, ctx.t);
            fonts.centered(
                canvas,
                "Pairing… confirm the PIN on the host",
                W::Regular,
                13.0 * k,
                fg(0.55),
                cx + 10.0 * k,
                status_y,
                f64::from(rect.width()) * 0.6,
            );
        } else if let Some(err) = &self.error {
            fonts.centered(
                canvas,
                err,
                W::Regular,
                13.0 * k,
                ERROR,
                cx,
                status_y,
                f64::from(rect.width()) * 0.8,
            );
        }

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
        // Spaced digits, not ●: the host already shows the PIN, and ● hides typos.
        let pin_display: String = self
            .pin
            .chars()
            .flat_map(|c| [c, ' '])
            .collect::<String>()
            .trim_end()
            .to_string();
        let has_request = self.can_request();
        self.roles()
            .into_iter()
            .map(|role| match role {
                Role::RequestAccess => {
                    let mut r = RowSpec::action("Request access — approve on the host", !self.busy);
                    r.header = Some("No PIN needed");
                    r
                }
                Role::Pin => {
                    let mut pin = RowSpec::field("PIN", pin_display.clone(), "From the host");
                    pin.caret = self.editing == Some(Field::Pin);
                    // Only when Request access sits above, so the two paths read as alternatives.
                    if has_request {
                        pin.header = Some("Or pair with a PIN");
                    }
                    pin
                }
                Role::Device => {
                    let mut device = RowSpec::field(
                        "Device name",
                        self.device.clone(),
                        "How the host lists this device",
                    );
                    device.caret = self.editing == Some(Field::Device);
                    device
                }
                Role::Pair => {
                    RowSpec::action(if self.busy { "Pairing…" } else { "Pair" }, self.can_pair())
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    fn host() -> HostRow {
        HostRow {
            addr: "10.0.0.7".into(),
            fp_hex: String::new(),
            paired: false,
            ..HostRow::fixture("10.0.0.7:9777", "Tower")
        }
    }

    #[test]
    fn pair_submits_with_pin_and_device_fallback() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let device = crate::screens::Device {
            name: "living-room-deck".into(),
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            device: &device,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = PairScreen::new(&host(), "living-room-deck");
        s.device.clear(); // empty field falls back to `&ctx.device.name`
        s.editing = Some(Field::Pin);
        s.text_input("1234");
        s.edit_key(crate::input::Key::Return);
        s.list.cursor = 2;
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::Pair { pin, device_name, .. })
                if pin == "1234" && device_name == "living-room-deck"
        ));
        assert!(s.busy);
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
    }

    #[test]
    fn request_access_connects_and_leaves() {
        let mut host = host();
        host.fp_hex = "abcd".into();
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let device = crate::screens::Device {
            name: "deck".into(),
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            device: &device,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = PairScreen::new(&host, "deck");
        assert_eq!(s.roles().len(), 4, "Request Access + PIN + Device + Pair");
        s.list.cursor = 0;
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        let intent = fx.connect.expect("request-access raises a connect intent");
        assert!(intent.request_access);
        assert_eq!(intent.fp_hex, "abcd");
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    }

    #[test]
    fn no_request_access_without_an_advert() {
        let s = PairScreen::new(&host(), "deck");
        assert!(!s.can_request());
        assert_eq!(s.roles().len(), 3, "PIN + Device + Pair only");
    }

    #[test]
    fn pin_is_digits_only() {
        let mut s = PairScreen::new(&host(), "d");
        s.editing = Some(Field::Pin);
        s.text_input("12ab34");
        assert_eq!(s.pin, "1234");
    }

    #[test]
    fn failure_lands_in_the_error_line() {
        let mut s = PairScreen::new(&host(), "d");
        s.busy = true;
        s.apply_phase(&PairPhase::Failed("Wrong PIN".into()));
        assert!(!s.busy);
        assert_eq!(s.error.as_deref(), Some("Wrong PIN"));
    }
}
