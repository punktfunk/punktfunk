//! A question the app asks through the console, where a system alert would be out of a
//! pad's reach: "Open this link?" for a link that names a saved host by a guessable
//! reference. One row per answer; Back answers none of them.
//!
//! The answer leaves as [`ConsoleCmd::PromptAnswer`] carrying the prompt's `id`, so the app
//! can drop a reply to a question it no longer holds. The shell's own exit question
//! ([`PromptScreen::exit`]) answers nobody: its yes quits.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
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

pub(crate) struct PromptScreen {
    prompt: Prompt,
    pub(super) list: MenuList,
    /// The shell's own exit question: its first row quits, and the app hears no answer.
    exit: bool,
}

impl PromptScreen {
    pub(crate) fn new(prompt: Prompt) -> PromptScreen {
        PromptScreen {
            prompt,
            list: MenuList::new(),
            exit: false,
        }
    }

    pub(crate) fn exit() -> PromptScreen {
        PromptScreen {
            exit: true,
            ..PromptScreen::new(Prompt {
                id: String::new(),
                title: "Exit".into(),
                message: "Exit punktfunk?".into(),
                choices: vec!["Exit".into(), "Cancel".into()],
            })
        }
    }

    pub(crate) fn title(&self) -> String {
        self.prompt.title.clone()
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        _ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            self.answer(None, fx);
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.prompt.choices.len());
        self.run(msg, pulse, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, _ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, self.prompt.choices.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.run(msg, pulse, fx);
        true
    }

    fn run(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        match msg {
            ListMsg::Activate => {
                self.answer(Some(self.list.cursor), fx);
                Some(MenuPulse::Confirm)
            }
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
        }
    }

    fn answer(&self, choice: Option<usize>, fx: &mut Outbox) {
        if self.exit {
            fx.quit = choice == Some(0);
        } else {
            fx.cmds.push(ConsoleCmd::PromptAnswer {
                id: self.prompt.id.clone(),
                choice,
            });
        }
        fx.pop();
    }

    pub(crate) fn announcement(&self) -> Option<String> {
        let choice = self.prompt.choices.get(self.list.cursor)?;
        Some(format!("{} {choice}", self.prompt.message))
    }

    pub(crate) fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        let choice = self.prompt.choices.get(self.list.cursor);
        let mut hints = Vec::new();
        if let Some(choice) = choice {
            hints.push(Hint::new(HintKey::Confirm, choice.clone()));
        }
        hints.push(Hint::new(HintKey::Back, "Cancel"));
        hints
    }

    pub(crate) fn render(
        &mut self,
        canvas: &Canvas,
        rect: Rect,
        k: f64,
        dt: f64,
        fonts: &Fonts,
        _ctx: &mut Ctx,
    ) {
        let rows: Vec<RowSpec> = (self.prompt.choices.iter())
            .map(|label| RowSpec {
                label: label.clone(),
                ..RowSpec::default()
            })
            .collect();
        let rest = blurb(canvas, fonts, &self.prompt.message, rect, k);
        self.list.render(canvas, rest, &rows, fonts, k, dt, true);
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
        let pads = Vec::new();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Apple,
            screen: None,
            pads: &pads,
            deck: false,
            tv: false,
            fallback_ui: true,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
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
