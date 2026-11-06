//! PIN pairing on the controller console — counterpart of the desktop PairSheet.
//!
//! Type the PIN the host shows (on-screen tray, or the device's own keyboard).
//! SPAKE2 runs on the binary's service thread; success pins the host and the
//! shell pops Home. A discovered host with an advertised fingerprint also
//! offers Request access (connect and wait for operator approval). A typed
//! host with no advert is PIN-only.

use crate::glyphs::{Hint, HintKey};
use crate::model::{ConsoleCmd, HostRow, PairPhase, RequestKey};
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, ScreenView, TextEntry};
use crate::theme::{Fonts, ERROR, W};
use crate::widgets::{
    blurb, field_key, permits, type_text, Charset, Entry, ListMsg, MenuList, RowSpec,
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
    list: MenuList,
    keyboard: TextEntry,
    pin: String,
    device: String,
    editing: Option<Field>,
    /// Local Busy so a second A cannot double-submit before the service thread sees the command.
    busy: bool,
    /// The busy pairing is a request access, waiting on the host's operator.
    requesting: bool,
    error: Option<String>,
    /// The ceremony this screen last sent. Another one's phase is not shown here.
    req: Option<RequestKey>,
}

impl PairScreen {
    pub(crate) fn new(host: &HostRow, device_name: &str) -> PairScreen {
        PairScreen {
            host_name: host.name.clone(),
            addr: host.addr.clone(),
            port: host.port,
            fp_hex: host.fp_hex.clone(),
            list: MenuList::new(),
            keyboard: TextEntry::default(),
            pin: String::new(),
            device: device_name.to_string(),
            editing: None,
            busy: false,
            requesting: false,
            error: None,
            req: None,
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

    /// This screen sent `req`: its phases are this screen's to show.
    pub(crate) fn sent(&self, req: &RequestKey) -> bool {
        self.req.as_ref() == Some(req)
    }

    /// A fresh key for a ceremony with this host, kept as the one this screen waits on.
    fn ask(&mut self) -> RequestKey {
        let req = RequestKey::new(&format!("{}:{}", self.addr, self.port));
        self.req = Some(req.clone());
        req
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

    fn can_pair(&self) -> bool {
        !self.pin.trim().is_empty() && !self.busy
    }

    /// The name the host lists this device under: the typed one, else the device's own.
    fn device_name(&self, ctx: &Ctx) -> String {
        match self.device.trim() {
            "" => ctx.device.name.clone(),
            typed => typed.to_string(),
        }
    }

    /// The open field, its text, and the keyboard that types into it.
    fn open(&mut self) -> Option<(Field, &mut TextEntry, &mut String)> {
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
                        self.busy = true;
                        self.requesting = true;
                        self.error = None;
                        fx.cmds.push(ConsoleCmd::RequestAccess {
                            addr: self.addr.clone(),
                            port: self.port,
                            fp_hex: self.fp_hex.clone(),
                            device_name: self.device_name(ctx),
                            req: self.ask(),
                        });
                    }
                    Some(Role::Pin) => self.editing = Some(Field::Pin),
                    Some(Role::Device) => self.editing = Some(Field::Device),
                    Some(Role::Pair) if self.can_pair() => {
                        self.busy = true;
                        self.requesting = false;
                        self.error = None;
                        fx.cmds.push(ConsoleCmd::Pair {
                            addr: self.addr.clone(),
                            port: self.port,
                            pin: self.pin.trim().to_string(),
                            device_name: self.device_name(ctx),
                            req: self.ask(),
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
                    let label = match self.busy && self.requesting {
                        true => pf_l10n::client::pair_waiting_for_approval(),
                        false => pf_l10n::client::pair_request_access(),
                    };
                    let mut r = RowSpec::action(label, !self.busy);
                    r.header = Some(pf_l10n::console::console_pair_no_pin_needed());
                    r
                }
                Role::Pin => {
                    let mut pin = RowSpec::field(
                        pf_l10n::client::pair_pin(),
                        pin_display.clone(),
                        pf_l10n::client::pair_pin_placeholder(),
                    );
                    pin.caret = self.editing == Some(Field::Pin);
                    // Only when Request access sits above, so the two paths read as alternatives.
                    if has_request {
                        pin.header = Some(pf_l10n::console::console_pair_or_pin());
                    }
                    pin
                }
                Role::Device => {
                    let mut device = RowSpec::field(
                        pf_l10n::client::pair_device_name(),
                        self.device.clone(),
                        pf_l10n::client::pair_device_name_placeholder(),
                    );
                    device.caret = self.editing == Some(Field::Device);
                    device
                }
                Role::Pair => RowSpec::action(
                    if self.busy && !self.requesting {
                        pf_l10n::client::pair_pairing()
                    } else {
                        pf_l10n::client::pair_action()
                    },
                    self.can_pair(),
                ),
            })
            .collect()
    }
}

impl ScreenView for PairScreen {
    fn editing(&self) -> bool {
        self.editing.is_some()
    }

    fn edit_field(&self) -> Option<crate::screens::EditField> {
        match self.editing? {
            Field::Pin => {
                crate::screens::EditField::new(pf_l10n::client::pair_pin(), &self.pin, true)
            }
            Field::Device => crate::screens::EditField::new(
                pf_l10n::client::pair_device_name(),
                &self.device,
                false,
            ),
        }
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
        if let Some((f, keyboard, text)) = self.open() {
            let (entry, pulse) = keyboard.menu(ev, ctx.device, text, |t, c| Self::admits(f, t, c));
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
    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let tray = self.open().and_then(|(f, keyboard, text)| {
            keyboard.pointer(p, ctx.device, text, |t, c| Self::admits(f, t, c))
        });
        if let Some(entry) = tray {
            if entry.is_some_and(|e| e != Entry::Stay) {
                self.editing = None;
            }
            return entry.is_some();
        }
        let roles = self.roles();
        let (msg, pulse) = self.list.pointer(p, roles.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, &roles, ctx, fx);
        true
    }

    fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if self.editing.is_some() {
            return self
                .keyboard
                .hints(ctx.device, pf_l10n::common::common_done());
        }
        vec![
            Hint::new(HintKey::Confirm, pf_l10n::common::common_select()),
            Hint::new(HintKey::Back, pf_l10n::common::common_cancel()),
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
        let ui = ctx.ui;
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        let intro = if self.can_request() {
            pf_l10n::client::pair_intro()
        } else {
            pf_l10n::client::pair_intro_pin_only()
        };
        let below = blurb(canvas, ui, fonts, intro, rect, k);

        self.keyboard.seat(self.editing.is_some(), ctx.device, dt);
        let tray_h = self.keyboard.reserve(k);
        // Status band (spinner / error); 34 matches the settings detail band.
        let status_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            below.top,
            rect.right,
            rect.bottom - tray_h as f32 - status_h as f32,
        );
        let rows = self.rows();
        self.list.lift = self.keyboard.lift(rect, k);
        self.list.render(
            canvas,
            ui,
            list_rect,
            &rows,
            fonts,
            k,
            dt,
            self.editing.is_none() && !self.busy,
        );

        let status_y = f64::from(rect.bottom) - tray_h - status_h + 6.0 * k;
        if self.busy {
            let waiting = match self.requesting {
                true => pf_l10n::console::console_pair_status_requesting(),
                false => pf_l10n::console::console_pair_status_pairing(),
            };
            // Spinner, gap and label centred as one line.
            let (r, gap, size) = (7.0 * k, 10.0 * k, 13.0 * k);
            let label_w = f64::from(fonts.measure(waiting, W::Regular, size));
            let left = cx - (2.0 * r + gap + label_w) / 2.0;
            crate::theme::spinner(canvas, ui, left + r, status_y + 8.0 * k, r, ctx.t);
            fonts.centered(
                canvas,
                waiting,
                W::Regular,
                size,
                ui.fg(0.55),
                left + 2.0 * r + gap + label_w / 2.0,
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

        self.keyboard.render(canvas, ui, fonts, rect, k);
    }

    fn title(&self) -> String {
        pf_l10n::client::pair_title(self.host_name())
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
        s.edit_key(crate::input::Key::Return, &mut ctx);
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
    fn request_access_pairs_without_connecting() {
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
        assert!(matches!(
            fx.cmds.first(),
            Some(ConsoleCmd::RequestAccess { fp_hex, device_name, .. })
                if fp_hex == "abcd" && device_name == "deck"
        ));
        assert!(fx.connect.is_none(), "a request never streams");
        assert!(fx.nav.is_none(), "the screen waits for the answer");
        assert!(s.busy && s.requesting);
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

    /// Every row and the title come from pf-l10n: each reads the pseudo-language.
    #[test]
    fn the_pair_screen_reads_the_pseudo_language() {
        let mut host = host();
        host.fp_hex = "abcd".into();
        let s = PairScreen::new(&host, "deck");
        let pseudo = |t: &str| t.starts_with('[') && t.ends_with(']');
        pf_l10n::with_language("en-XA", || {
            for row in s.rows() {
                assert!(pseudo(&row.label), "{}", row.label);
                assert!(row.header.is_none_or(pseudo), "{:?}", row.header);
            }
            let title = s.title();
            assert!(pseudo(&title) && title.contains("Tower"), "{title}");
        });
    }
}
