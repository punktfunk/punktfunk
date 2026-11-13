//! Controller access: the grants and tests only the host can perform, one row each.
//! Reached from the Controllers tab's "Not showing?" card, Android's alone.
//!
//! OK on a row asks the host through [`ConsoleCmd::PadAction`]; nothing here edits.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::players::{PadAction, PASSTHROUGH};
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::Fonts;
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

pub(crate) struct GrantsScreen {
    list: MenuList,
}

impl GrantsScreen {
    pub(crate) fn new() -> GrantsScreen {
        GrantsScreen {
            list: MenuList::new(),
        }
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
                // Empty key: the device is not an input device yet (SC2 keyboard/mouse mode).
                fx.cmds.push(ConsoleCmd::PadAction {
                    action: PASSTHROUGH[self.list.cursor].0.id().to_string(),
                    pad_key: String::new(),
                });
                Some(MenuPulse::Confirm)
            }
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
        }
    }
}

impl ScreenView for GrantsScreen {
    fn menu(&mut self, ev: MenuEvent, _ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, PASSTHROUGH.len());
        self.run(msg, pulse, fx)
    }

    fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, PASSTHROUGH.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.run(msg, pulse, fx);
        true
    }

    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        let (_, label, verb) = PASSTHROUGH[self.list.cursor];
        Some(format!("{}, {}", label(), verb()))
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        vec![
            Hint::new(HintKey::Confirm, (PASSTHROUGH[self.list.cursor].2)()),
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
        let rows: Vec<RowSpec> = PASSTHROUGH
            .iter()
            .map(|(_, label, verb)| RowSpec {
                label: label().into(),
                value: Some(verb().into()),
                ..RowSpec::default()
            })
            .collect();
        self.list
            .render(canvas, ui, rect, &rows, fonts, k, dt, true);
    }

    /// Why the focused row exists.
    fn foot(&self, _ctx: &Ctx) -> Option<std::borrow::Cow<'static, str>> {
        Some(detail(PASSTHROUGH[self.list.cursor].0).into())
    }

    fn title(&self) -> String {
        pf_l10n::console::console_pad_access_title().into()
    }

    fn press(&mut self) {
        self.list.dip();
    }

    fn pan(&mut self, p: Pointer) -> bool {
        self.list.pan(p)
    }
}

fn detail(action: PadAction) -> &'static str {
    use pf_l10n::console as c;
    match action {
        PadAction::Sc2Bluetooth => pf_l10n::client::pad_sc2_bluetooth_detail(),
        PadAction::Sc2Usb => c::console_pad_sc2_usb_detail(),
        PadAction::DsUsb => c::console_pad_ds_usb_detail(),
        PadAction::DsHaptics => c::console_pad_ds_haptics_detail(),
        // Not a row; pads carry rumble.
        PadAction::Rumble => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::Settings;

    #[test]
    fn ok_on_a_row_asks_the_host_for_that_grant() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let device = crate::screens::Device {
            platform: crate::platform::Platform::Android,
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            device: &device,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = GrantsScreen::new();
        let mut fx = Outbox::default();
        s.menu(
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Down),
            &mut ctx,
            &mut fx,
        );
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PadAction {
                action: "sc2_usb".into(),
                pad_key: String::new(),
            }]
        );
        assert_eq!(
            s.announcement(&ctx).as_deref(),
            Some("Steam Controller 2 over USB, Grant")
        );
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Back, &mut ctx, &mut fx);
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    }
}
