//! A question the app asks through the console, where a system alert would be out of a
//! pad's reach: "Open this link?" for a link that names a saved host by a guessable
//! reference. One row per answer; Back answers none of them.
//!
//! The answer leaves as [`ConsoleCmd::PromptAnswer`] carrying the prompt's `id`, so the app
//! can drop a reply to a question it no longer holds. The shell's own questions answer nobody:
//! [`PromptScreen::exit`]'s yes quits, [`PromptScreen::reset_settings`]'s resets.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::Fonts;
use crate::widgets::{blurb, ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

/// What the app asks. `choices` are row labels, the default first.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Prompt {
    pub id: String,
    pub title: String,
    pub message: String,
    pub choices: Vec<String>,
}

/// The shell's own questions, answered here: the app hears nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Own {
    /// Row 0 quits.
    Exit,
    /// Row 1 resets every setting; row 0, Cancel, sits under the cursor.
    ResetSettings,
}

pub(crate) struct PromptScreen {
    prompt: Prompt,
    list: MenuList,
    own: Option<Own>,
}

impl PromptScreen {
    pub(crate) fn new(prompt: Prompt) -> PromptScreen {
        PromptScreen {
            prompt,
            list: MenuList::new(),
            own: None,
        }
    }

    fn own(own: Own, title: &str, message: &str, choices: [&str; 2]) -> PromptScreen {
        PromptScreen {
            own: Some(own),
            ..PromptScreen::new(Prompt {
                id: String::new(),
                title: title.into(),
                message: message.into(),
                choices: choices.map(String::from).to_vec(),
            })
        }
    }

    pub(crate) fn exit() -> PromptScreen {
        use pf_l10n::{common, console};
        Self::own(
            Own::Exit,
            console::console_exit_title(),
            console::console_exit_message(),
            [console::console_exit_confirm(), common::common_cancel()],
        )
    }

    pub(crate) fn reset_settings() -> PromptScreen {
        use pf_l10n::{client, common};
        Self::own(
            Own::ResetSettings,
            client::prefs_reset_all_title(),
            client::prefs_reset_all_body(),
            [common::common_cancel(), common::common_reset()],
        )
    }

    fn run(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match msg {
            ListMsg::Activate => {
                self.answer(Some(self.list.cursor), ctx, fx);
                Some(MenuPulse::Confirm)
            }
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
        }
    }

    fn answer(&self, choice: Option<usize>, ctx: &mut Ctx, fx: &mut Outbox) {
        match self.own {
            Some(Own::Exit) => fx.quit = choice == Some(0),
            Some(Own::ResetSettings) if choice == Some(1) => {
                ctx.write(|c| {
                    c.settings.reset_to_defaults();
                    true
                });
            }
            Some(Own::ResetSettings) => {}
            None => fx.cmds.push(ConsoleCmd::PromptAnswer {
                id: self.prompt.id.clone(),
                choice,
            }),
        }
        fx.pop();
    }
}

impl ScreenView for PromptScreen {
    fn title(&self) -> String {
        self.prompt.title.clone()
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            self.answer(None, ctx, fx);
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.prompt.choices.len());
        self.run(msg, pulse, ctx, fx)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, self.prompt.choices.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.run(msg, pulse, ctx, fx);
        true
    }

    fn announcement(&self, _ctx: &Ctx) -> Option<String> {
        let choice = self.prompt.choices.get(self.list.cursor)?;
        Some(format!("{} {choice}", self.prompt.message))
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let choice = self.prompt.choices.get(self.list.cursor);
        let mut hints = Vec::new();
        if let Some(choice) = choice {
            hints.push(Hint::new(HintKey::Confirm, choice.clone()));
        }
        hints.push(Hint::new(HintKey::Back, pf_l10n::common::common_cancel()));
        hints
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
        let reset = self.own == Some(Own::ResetSettings);
        let rows: Vec<RowSpec> = (self.prompt.choices.iter().enumerate())
            .map(|(i, label)| RowSpec {
                label: label.clone(),
                danger: reset && i == 1,
                ..RowSpec::default()
            })
            .collect();
        let rest = blurb(canvas, ui, fonts, &self.prompt.message, rect, k);
        self.list
            .render(canvas, ui, rest, &rows, fonts, k, dt, true);
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

    fn prompt() -> PromptScreen {
        PromptScreen::new(Prompt {
            id: "link".into(),
            title: "Open this link?".into(),
            message: "Connect to Desk?".into(),
            choices: vec!["Connect".into(), "Cancel".into()],
        })
    }

    fn answer(s: &mut PromptScreen, events: &[MenuEvent]) -> Outbox {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let device = crate::screens::Device {
            platform: crate::platform::Platform::Apple,
            fallback_ui: true,
            ..crate::screens::Device::test()
        };
        let mut ctx = Ctx {
            device: &device,
            ..Ctx::test(&mut settings, &library)
        };
        let mut fx = Outbox::default();
        for ev in events {
            s.menu(*ev, &mut ctx, &mut fx);
        }
        fx
    }

    #[test]
    fn ok_answers_the_focused_choice_and_closes() {
        let mut s = prompt();
        let fx = answer(
            &mut s,
            &[MenuEvent::Move(MenuDir::Down), MenuEvent::Confirm],
        );
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PromptAnswer {
                id: "link".into(),
                choice: Some(1),
            }]
        );
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    }

    /// The pad's first OK lands on Cancel; Down then OK resets every setting through the store,
    /// and the app hears no answer.
    #[test]
    fn reset_needs_cancel_passed_and_writes_the_store() {
        let changed = Settings {
            codec: "av1".into(),
            show_advanced: true,
            ..Settings::default()
        };
        for (events, reset) in [
            (&[MenuEvent::Confirm][..], false),
            (&[MenuEvent::Back][..], false),
            (
                &[MenuEvent::Move(MenuDir::Down), MenuEvent::Confirm][..],
                true,
            ),
        ] {
            let store = crate::store::SnapshotStore::new(changed.clone(), Vec::new());
            let mut settings = changed.clone();
            let library = crate::library::LibraryShared::default();
            let mut ctx = Ctx {
                store: &store,
                ..Ctx::test(&mut settings, &library)
            };
            let mut s = PromptScreen::reset_settings();
            let mut fx = Outbox::default();
            for ev in events {
                s.menu(*ev, &mut ctx, &mut fx);
            }
            assert!(fx.cmds.is_empty(), "{events:?}");
            assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
            let want = if reset {
                Settings::default()
            } else {
                changed.clone()
            };
            assert_eq!(store.snapshot().0, want, "{events:?}");
            assert_eq!(settings, want, "{events:?}: the open screen shows it");
        }
    }

    #[test]
    fn back_answers_none_and_closes() {
        let mut s = prompt();
        let fx = answer(&mut s, &[MenuEvent::Back]);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PromptAnswer {
                id: "link".into(),
                choice: None,
            }]
        );
        assert!(matches!(fx.nav, Some(crate::screens::Nav::Pop)));
    }
}
