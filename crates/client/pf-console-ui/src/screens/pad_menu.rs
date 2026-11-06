//! A pad's menu, raised by OK or a hold on its Controllers card: the live input test where
//! the host can run it ([`super::input_test`]) and a rumble pulse where the host holds the
//! device. One row each; Back leaves. The seats board grows its rows here
//! (`htpc-experience.md` §3.5).

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::players::PadAction;
use crate::screens::{Ctx, Outbox, Screen, ScreenView};
use crate::theme::Fonts;
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use skia_safe::{Canvas, Rect};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    Test,
    Rumble,
}

impl Row {
    fn label(self) -> &'static str {
        match self {
            Row::Test => pf_l10n::console::console_pad_test_controller(),
            Row::Rumble => pf_l10n::console::console_pad_rumble(),
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Row::Test => pf_l10n::common::common_open(),
            Row::Rumble => pf_l10n::console::console_pad_test(),
        }
    }

    /// What the row does. The test ends on a held B, the back button.
    fn detail(self) -> String {
        use pf_l10n::console as c;
        match self {
            Row::Test => format!(
                "{} {}",
                c::console_pad_test_controller_detail(),
                c::console_pad_test_finish_hold("B")
            ),
            Row::Rumble => c::console_pad_rumble_detail().into(),
        }
    }
}

pub(crate) struct PadMenuScreen {
    name: String,
    key: String,
    rows: Vec<Row>,
    list: MenuList,
}

impl PadMenuScreen {
    /// `test`: the host sends this pad's readings; `rumble`: it holds the device. The card
    /// opens the menu only when one of them is true.
    pub(crate) fn new(pad: &PadInfo, test: bool, rumble: bool) -> PadMenuScreen {
        let mut rows = Vec::new();
        if test {
            rows.push(Row::Test);
        }
        if rumble {
            rows.push(Row::Rumble);
        }
        PadMenuScreen {
            name: pad.name.clone(),
            key: pad.key.clone(),
            rows,
            list: MenuList::new(),
        }
    }

    fn row(&self) -> Row {
        self.rows[self.list.cursor.min(self.rows.len() - 1)]
    }

    /// Shared by pad and pointer. Adjust is a boundary: a row runs or it does not.
    fn run(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match msg {
            ListMsg::Activate => {
                match self.row() {
                    // In place of this menu, so Back from the test returns to the cards.
                    Row::Test => {
                        fx.replace(Screen::InputTest(super::input_test::InputTestScreen::new()))
                    }
                    Row::Rumble => fx.cmds.push(ConsoleCmd::PadAction {
                        action: PadAction::Rumble.id().to_string(),
                        pad_key: self.key.clone(),
                    }),
                }
                Some(MenuPulse::Confirm)
            }
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
        }
    }
}

impl ScreenView for PadMenuScreen {
    fn menu(&mut self, ev: MenuEvent, _ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.rows.len());
        self.run(msg, pulse, fx)
    }

    fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, self.rows.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.run(msg, pulse, fx);
        true
    }

    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        let row = self.row();
        Some(format!("{}, {}", row.label(), row.verb()))
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, self.row().verb()),
            Hint::new(HintKey::Back, pf_l10n::common::common_done()),
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
        let rows: Vec<RowSpec> = self
            .rows
            .iter()
            .map(|r| RowSpec {
                label: r.label().into(),
                value: Some(r.verb().into()),
                ..RowSpec::default()
            })
            .collect();
        self.list
            .render(canvas, ui, rect, &rows, fonts, k, dt, true);
    }

    /// What the focused row does.
    fn foot(&self, _ctx: &Ctx) -> Option<std::borrow::Cow<'static, str>> {
        Some(self.row().detail().into())
    }

    fn title(&self) -> String {
        self.name.clone()
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
    use pf_client_core::menu_nav::MenuDir;
    use pf_client_core::trust::Settings;
    use punktfunk_core::config::GamepadPref;

    fn pad() -> PadInfo {
        PadInfo {
            name: "DualSense".into(),
            key: "054c:0ce6:DualSense".into(),
            pref: GamepadPref::DualSense,
            steam_virtual: false,
            battery: None,
            detail: String::new(),
            forwarded: true,
            rumble: true,
        }
    }

    /// Test replaces the menu with the input test; Rumble asks the host for a pulse and
    /// stays; Back leaves.
    #[test]
    fn the_rows_open_the_test_or_pulse_the_pad() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let mut s = PadMenuScreen::new(&pad(), true, true);
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(
            fx.nav,
            Some(crate::screens::Nav::Replace(ref b)) if matches!(**b, Screen::InputTest(_))
        ));
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PadAction {
                action: "rumble".into(),
                pad_key: "054c:0ce6:DualSense".into(),
            }]
        );
        assert!(fx.nav.is_none(), "rumble keeps the menu up");
        assert_eq!(s.announcement(&ctx).as_deref(), Some("Rumble, Test"));
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Back, &mut ctx, &mut fx);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
        // A host without the device offers the test alone.
        let s = PadMenuScreen::new(&pad(), true, false);
        assert_eq!(s.rows, vec![Row::Test]);
    }
}
