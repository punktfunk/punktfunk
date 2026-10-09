//! Bind the preset a plain A-press on a saved host connects with
//! (`KnownHost::preset_id`). Reached from the host tile's Options menu.
//!
//! Choosing emits [`ConsoleCmd::BindPreset`]; the checkmark is read back from
//! the host row, never stored here. Pinning is the sibling (`pin_hosts.rs`):
//! a pin adds a card, this changes what the primary tile itself does.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox, ScreenView};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec, FOOT_DETAIL_H};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

/// The title this screen binds for. Absent = the host's own default.
pub(crate) struct GameSubject {
    pub id: String,
    pub title: String,
}

pub(crate) struct BindPresetScreen {
    /// Host primary key (fingerprint or `addr:port`), never a pinned-card composite.
    host_key: String,
    host_name: String,
    /// Catalog snapshot at construction. The console cannot create presets, so
    /// this list cannot change while the screen is open.
    presets: Vec<(String, String)>,
    /// Set when the menu was raised on a title rather than the host tile. Same catalog
    /// and same radio behaviour either way — only the binding it writes differs.
    game: Option<GameSubject>,
    list: MenuList,
}

impl BindPresetScreen {
    pub(crate) fn new(
        host_key: String,
        host_name: String,
        presets: Vec<(String, String)>,
    ) -> BindPresetScreen {
        BindPresetScreen {
            host_key,
            host_name,
            presets,
            game: None,
            list: MenuList::new(),
        }
    }

    /// The same screen bound to one title instead of the host tile.
    pub(crate) fn for_game(
        host_key: String,
        host_name: String,
        game: GameSubject,
        presets: Vec<(String, String)>,
    ) -> BindPresetScreen {
        BindPresetScreen {
            game: Some(game),
            ..BindPresetScreen::new(host_key, host_name, presets)
        }
    }

    pub(crate) fn host_name(&self) -> &str {
        &self.host_name
    }

    /// Read from the model, never remembered: the row's chip and its `game_presets`
    /// ARE the state, so the checkmark cannot disagree with what the carousel shows.
    fn bound(&self, ctx: &Ctx) -> Option<String> {
        let row = ctx.hosts.iter().find(|r| r.key == self.host_key)?;
        match &self.game {
            Some(g) => row.game_presets.get(&g.id).cloned(),
            None => row.bound_preset.as_ref().map(|p| p.id.clone()),
        }
    }

    fn choice(&self, i: usize) -> Option<Option<&str>> {
        if i == 0 {
            Some(None)
        } else {
            self.presets.get(i - 1).map(|(id, _)| Some(id.as_str()))
        }
    }

    fn len(&self) -> usize {
        self.presets.len() + 1
    }

    /// Radio, not toggle: re-selecting the bound row is a boundary; ◀/▶ do nothing.
    fn choose(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(choice) = self.choice(self.list.cursor) else {
            return pulse;
        };
        match msg {
            ListMsg::Adjust(_) => Some(MenuPulse::Boundary),
            ListMsg::None => pulse,
            ListMsg::Activate => {
                let current = self.bound(ctx);
                if current.as_deref() == choice {
                    return Some(MenuPulse::Boundary);
                }
                fx.cmds.push(ConsoleCmd::BindPreset {
                    key: self.host_key.clone(),
                    game: self.game.as_ref().map(|g| g.id.clone()),
                    preset_id: choice.map(str::to_owned),
                });
                Some(MenuPulse::Confirm)
            }
        }
    }
}

impl ScreenView for BindPresetScreen {
    /// Stack title: the subject the user picked is the one named.
    fn title(&self) -> String {
        match &self.game {
            Some(g) => format!("Preset for {}", g.title),
            None => format!("Default for {}", self.host_name()),
        }
    }

    fn menu(&mut self, ev: MenuEvent, ctx: &mut Ctx, fx: &mut Outbox) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let (msg, pulse) = self.list.menu(ev, self.len());
        self.choose(msg, pulse, ctx, fx)
    }

    fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let (msg, pulse) = self.list.pointer(p, self.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.choose(msg, pulse, ctx, fx);
        true
    }

    fn hints(&self, _ctx: &Ctx) -> Vec<Hint> {
        if self.presets.is_empty() {
            return vec![Hint::new(HintKey::Back, "Done")];
        }
        let verb = if self.game.is_some() {
            "Use for this title"
        } else {
            "Set default"
        };
        vec![
            Hint::new(HintKey::Confirm, verb),
            Hint::new(HintKey::Back, "Done"),
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
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        if self.presets.is_empty() {
            fonts.centered(
                canvas,
                "No presets yet \u{2014} create them in the desktop app, then choose one here.",
                W::Regular,
                14.0 * k,
                fg(0.55),
                cx,
                f64::from(rect.top) + f64::from(rect.height()) / 2.0,
                f64::from(rect.width()) * 0.7,
            );
            return;
        }
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom - (FOOT_DETAIL_H * k) as f32,
        );
        let bound = self.bound(ctx);
        let rows: Vec<RowSpec> = (0..self.len())
            .map(|i| {
                let (label, id) = if i == 0 {
                    // Row 0 is what "no binding" MEANS here: globals for a host, the
                    // host's own default for a title. Naming the fallback beats "None".
                    let none = if self.game.is_some() {
                        "Use the host's default"
                    } else {
                        "No default"
                    };
                    (none.to_string(), None)
                } else {
                    let (id, name) = &self.presets[i - 1];
                    (name.clone(), Some(id.as_str()))
                };
                let current = bound.as_deref() == id;
                let marker = if self.game.is_some() {
                    "In use"
                } else {
                    "Default"
                };
                RowSpec {
                    header: None,
                    label,
                    value: Some(if current {
                        marker.into()
                    } else {
                        String::new()
                    }),
                    value_dim: !current,
                    caret: false,
                    adjustable: false,
                    enabled: true,
                    ..RowSpec::default()
                }
            })
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
    }

    /// The explainer under the list, once there is a list.
    fn foot(&self, _ctx: &Ctx) -> Option<std::borrow::Cow<'static, str>> {
        if self.presets.is_empty() {
            None
        } else if self.game.is_some() {
            Some("What this title streams with, overriding the host's default. A pinned card still keeps its own.".into())
        } else {
            Some("What a plain press on this host's tile connects with. Pinned cards keep their own.".into())
        }
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
    use crate::model::{HostRow, PresetChip};
    use pf_client_core::menu_nav::MenuDir;
    use pf_client_core::trust::Settings;

    fn host(bound: Option<&str>) -> HostRow {
        HostRow {
            bound_preset: bound.map(|id| PresetChip {
                id: id.into(),
                name: "Work".into(),
                accent: None,
                bitrate_kbps: None,
            }),
            ..HostRow::fixture("aa", "Desk")
        }
    }

    fn screen() -> BindPresetScreen {
        BindPresetScreen::new(
            "aa".into(),
            "Desk".into(),
            vec![("p1".into(), "Work".into()), ("p2".into(), "Game".into())],
        )
    }

    #[test]
    fn choosing_a_preset_binds_and_no_default_clears() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let hosts = [host(Some("p1"))];
        let mut ctx = Ctx {
            hosts: &hosts,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = screen();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::BindPreset {
                key: "aa".into(),
                game: None,
                preset_id: Some("p2".into()),
            }]
        );
        assert!(matches!(pulse, Some(MenuPulse::Confirm)));

        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::BindPreset {
                key: "aa".into(),
                game: None,
                preset_id: None,
            }]
        );
    }

    #[test]
    fn re_choosing_the_current_binding_is_a_boundary_not_a_command() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let hosts = [host(Some("p1"))];
        let mut ctx = Ctx {
            hosts: &hosts,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = screen();
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));

        let hosts = [host(None)];
        let mut settings = Settings::default();
        let mut ctx = Ctx {
            hosts: &hosts,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = screen();
        let mut fx = Outbox::default();
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }

    /// Raised on a title: the command names the game, the checkmark comes from
    /// `game_presets` (not the host's chip), and re-choosing it is still a boundary.
    #[test]
    fn a_title_binds_its_own_preset_and_reads_its_own_checkmark() {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        // Host bound to p1, title already bound to p2 — the two must not be confused.
        let mut row = host(Some("p1"));
        row.game_presets
            .insert("halo".to_string(), "p2".to_string());
        let hosts = [row];
        let mut ctx = Ctx {
            hosts: &hosts,
            ..Ctx::test(&mut settings, &library)
        };
        let mut s = BindPresetScreen::for_game(
            "aa".into(),
            "Desk".into(),
            GameSubject {
                id: "halo".into(),
                title: "Halo".into(),
            },
            vec![("p1".into(), "Work".into()), ("p2".into(), "Game".into())],
        );
        assert_eq!(s.title(), "Preset for Halo");

        // Row 2 is p2, which this title already uses: a boundary, not a second write.
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        s.menu(MenuEvent::Move(MenuDir::Down), &mut ctx, &mut fx);
        let pulse = s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));

        // Row 1 is p1 — the host's default, but not this title's, so it binds.
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::BindPreset {
                key: "aa".into(),
                game: Some("halo".into()),
                preset_id: Some("p1".into()),
            }]
        );

        // Row 0 clears the title's binding; it does not touch the host's.
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Move(MenuDir::Up), &mut ctx, &mut fx);
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::BindPreset {
                key: "aa".into(),
                game: Some("halo".into()),
                preset_id: None,
            }]
        );
    }
}
