//! A pad's menu, raised by OK or a hold on its Controllers card: the live input test where
//! the host can run it ([`super::input_test`]), a rumble pulse where the host holds the
//! device, and the speaker volume where this client plays the pad's audio. One row each;
//! Back leaves. The seats board grows its rows here (`htpc-experience.md` §3.5).

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::players::PadAction;
use crate::screens::{Ctx, Outbox, Screen, ScreenView};
use crate::theme::Fonts;
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use skia_safe::{Canvas, Rect};

/// One Left or Right on the volume row, percent; a pointer seek lands on a multiple of it.
const VOLUME_STEP: i32 = 10;

fn percent(v: u8) -> String {
    pf_l10n::common::common_percent(v)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Row {
    Test,
    Rumble,
    SpeakerVolume,
}

impl Row {
    fn label(self) -> &'static str {
        match self {
            Row::Test => pf_l10n::console::console_pad_test_controller(),
            Row::Rumble => pf_l10n::console::console_pad_rumble(),
            Row::SpeakerVolume => pf_l10n::client::player_pad_speaker_volume(),
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Row::Test => pf_l10n::common::common_open(),
            Row::Rumble => pf_l10n::console::console_pad_test(),
            Row::SpeakerVolume => pf_l10n::common::common_adjust(),
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
            Row::SpeakerVolume => pf_l10n::client::player_pad_speaker_volume_hint().into(),
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
    /// `test`: the host sends this pad's readings; `rumble`: it holds the device; `speaker`:
    /// this client plays the pad's audio. The card opens the menu only when one is true.
    pub(crate) fn new(pad: &PadInfo, test: bool, rumble: bool, speaker: bool) -> PadMenuScreen {
        let mut rows = Vec::new();
        if test {
            rows.push(Row::Test);
        }
        if rumble {
            rows.push(Row::Rumble);
        }
        if speaker {
            rows.push(Row::SpeakerVolume);
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

    /// Saves this pad's speaker volume as `f` maps the stored one, rebased on the store like
    /// every write. `true` when it moved.
    fn set_volume(&self, ctx: &mut Ctx, f: impl FnOnce(i32) -> i32) -> bool {
        let mut moved = false;
        ctx.write(|c| {
            let now = c.settings.pad_speaker_volume(&self.key);
            let next = f(i32::from(now)).clamp(0, 100) as u8;
            moved = next != now;
            if moved {
                c.settings.set_pad_speaker_volume(&self.key, next);
            }
            moved
        });
        moved
    }

    /// Shared by pad and pointer. Adjust moves the volume row and is a boundary elsewhere:
    /// a row runs or it does not.
    fn run(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ctx: &mut Ctx,
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
                    Row::SpeakerVolume => return Some(MenuPulse::Boundary),
                }
                Some(MenuPulse::Confirm)
            }
            ListMsg::Adjust(dir) if self.row() == Row::SpeakerVolume => {
                let moved = self.set_volume(ctx, |v| v + dir * VOLUME_STEP);
                Some(if moved {
                    MenuPulse::Move
                } else {
                    MenuPulse::Boundary
                })
            }
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
        }
    }
}

impl ScreenView for PadMenuScreen {
    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.rows.len());
        self.run(msg, pulse, ctx, fx)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        // A press on the volume track seeks to where it lands, to the nearest step.
        let track = self.rows.iter().position(|r| *r == Row::SpeakerVolume);
        if let Some(i) = track.filter(|i| p.press() && self.list.on_track(*i, p)) {
            if let Some(frac) = self.list.track_frac(i, p.x) {
                self.list.cursor = i;
                let steps = (f64::from(frac) * 100.0 / f64::from(VOLUME_STEP)).round() as i32;
                self.set_volume(ctx, |_| steps * VOLUME_STEP);
                return true;
            }
        }
        let (msg, pulse) = self.list.pointer(p, self.rows.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.run(msg, pulse, ctx, fx);
        true
    }

    fn announcement(&self, ctx: &Ctx) -> Option<String> {
        let row = self.row();
        let what = match row {
            Row::SpeakerVolume => percent(ctx.settings.pad_speaker_volume(&self.key)),
            _ => row.verb().into(),
        };
        Some(format!("{}, {what}", row.label()))
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let key = match self.row() {
            Row::SpeakerVolume => HintKey::Adjust,
            _ => HintKey::Confirm,
        };
        vec![
            Hint::new(key, self.row().verb()),
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
            .map(|r| match r {
                Row::SpeakerVolume => {
                    let v = ctx.settings.pad_speaker_volume(&self.key);
                    RowSpec::slider(r.label(), percent(v), f32::from(v) / 100.0)
                }
                _ => RowSpec {
                    label: r.label().into(),
                    value: Some(r.verb().into()),
                    ..RowSpec::default()
                },
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
            audio: false,
        }
    }

    /// Test replaces the menu with the input test; Rumble asks the host for a pulse and
    /// stays; Back leaves.
    #[test]
    fn the_rows_open_the_test_or_pulse_the_pad() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let mut s = PadMenuScreen::new(&pad(), true, true, false);
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
        let s = PadMenuScreen::new(&pad(), true, false, false);
        assert_eq!(s.rows, vec![Row::Test]);
        // Nothing to offer, no rows: the card does not open the menu.
        assert!(PadMenuScreen::new(&pad(), false, false, false)
            .rows
            .is_empty());
    }

    /// The row is the catalogue's `pad_speaker_volume`, labelled by its message. Right at
    /// 100 % is a boundary, Left stores 90 % under the pad's key, OK does nothing.
    #[test]
    fn the_volume_row_steps_the_pads_stored_volume() {
        let raw = include_str!("../../../../../clients/shared/settings-catalog.json");
        let file: serde_json::Value =
            serde_json::from_str(raw).expect("settings-catalog.json parses");
        let row = file["per_controller"]
            .as_array()
            .expect("per_controller")
            .iter()
            .find(|r| r["key"] == "pad_speaker_volume")
            .expect("the catalogue lists pad_speaker_volume");
        assert_eq!(row["category"], "controllers");
        assert_eq!(row["default"], "100");

        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let mut s = PadMenuScreen::new(&pad(), false, false, true);
        assert_eq!(s.rows, vec![Row::SpeakerVolume]);
        let label = pf_l10n::get("player_pad_speaker_volume").unwrap().text();
        assert_eq!(s.rows[0].label(), label);
        assert_eq!(s.title(), "DualSense");

        let mut fx = Outbox::default();
        let right = s.menu(MenuEvent::Move(MenuDir::Right), &mut ctx, &mut fx);
        assert!(matches!(right, Some(MenuPulse::Boundary)));
        let ok = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(matches!(ok, Some(MenuPulse::Boundary)));
        assert!(fx.nav.is_none() && fx.cmds.is_empty());

        let left = s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        assert!(matches!(left, Some(MenuPulse::Move)));
        assert_eq!(ctx.settings.pad_speaker_volume(&pad().key), 90);
        assert_eq!(ctx.settings.pads[&pad().key].speaker_volume, Some(90));
        assert_eq!(
            s.announcement(&ctx).unwrap(),
            format!("{label}, {}", percent(90))
        );

        // Down to the floor, where Left is a boundary again.
        for _ in 0..9 {
            s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        }
        assert_eq!(ctx.settings.pad_speaker_volume(&pad().key), 0);
        let left = s.menu(MenuEvent::Move(MenuDir::Left), &mut ctx, &mut fx);
        assert!(matches!(left, Some(MenuPulse::Boundary)));
    }

    /// A press on the drawn track seeks to the nearest step: the ends are 0 and 100 %, the
    /// middle 50. Back at 100 % the pad's entry is gone.
    #[test]
    fn a_press_on_the_track_seeks_the_volume() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx::test(&mut settings, &library);
        let mut s = PadMenuScreen::new(&pad(), false, false, true);
        let fonts = crate::theme::build_fonts().unwrap();
        let mut surface = skia_safe::surfaces::raster_n32_premul((900, 300)).unwrap();
        let rect = Rect::from_xywh(0.0, 0.0, 900.0, 300.0);
        for _ in 0..30 {
            s.render(surface.canvas(), rect, 1.0, 1.0 / 60.0, &fonts, &mut ctx);
        }
        let y = f64::from(s.list.row_rect(0).unwrap().center_y());
        let press = |x: f64| Pointer {
            x,
            y,
            kind: crate::pointer::PointerKind::Press,
        };
        let on: Vec<f64> = (0..900)
            .map(f64::from)
            .filter(|x| s.list.on_track(0, press(*x)))
            .collect();
        let (lo, hi) = (on[0], *on.last().unwrap());
        let mut seek = |x: f64| {
            let mut fx = Outbox::default();
            assert!(s.pointer(press(x), &mut ctx, &mut fx));
            assert!(fx.nav.is_none() && fx.cmds.is_empty());
            ctx.settings.pad_speaker_volume(&pad().key)
        };
        assert_eq!(seek(lo), 0);
        assert_eq!(seek((lo + hi) / 2.0), 50);
        assert_eq!(seek(hi), 100);
        assert!(!ctx.settings.pads.contains_key(&pad().key));
    }
}
