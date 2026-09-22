//! Pin a preset onto saved hosts as extra connect cards
//! (`KnownHost::pinned_presets`). Reached from the settings Presets section.
//!
//! A toggle emits [`ConsoleCmd::SetPin`]; pinned/off is read back from the
//! host rows, never stored here. Binding is the sibling (`bind_preset.rs`):
//! that changes what the primary tile does; this adds a card.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse};
use skia_safe::{Canvas, Rect};

pub(crate) struct PinHostsScreen {
    preset_id: String,
    preset_name: String,
    pub(super) list: MenuList,
}

/// Saved hosts, primary tiles only: a pinned card is this screen's output, not a row.
fn host_indices(ctx: &Ctx) -> Vec<usize> {
    ctx.hosts
        .iter()
        .enumerate()
        .filter(|(_, h)| h.saved && h.pin.is_none())
        .map(|(i, _)| i)
        .collect()
}

impl PinHostsScreen {
    pub(crate) fn new(preset_id: String, preset_name: String) -> PinHostsScreen {
        PinHostsScreen {
            preset_id,
            preset_name,
            list: MenuList::new(),
        }
    }

    pub(crate) fn preset_name(&self) -> &str {
        &self.preset_name
    }

    /// Read from the model — the pinned card's row is the state, so the toggle cannot
    /// disagree with the carousel.
    fn pinned(&self, ctx: &Ctx, host_idx: usize) -> bool {
        let host = &ctx.hosts[host_idx];
        // The host half of the key (a pinned card appends its preset id past a NUL), not the
        // address: two OS installs of a dual-boot box are two hosts at one address.
        fn host_key(k: &str) -> &str {
            k.split('\0').next().unwrap_or(k)
        }
        let key = host_key(&host.key);
        ctx.hosts.iter().any(|r| {
            host_key(&r.key) == key && r.pin.as_ref().is_some_and(|p| p.id == self.preset_id)
        })
    }

    pub(crate) fn menu(
        &mut self,
        ev: MenuEvent,
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        if ev == MenuEvent::Back {
            fx.pop();
            return None;
        }
        let indices = host_indices(ctx);
        let (msg, pulse) = self.list.menu(ev, indices.len());
        self.toggle(msg, pulse, &indices, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let indices = host_indices(ctx);
        let (msg, pulse) = self.list.pointer(p, indices.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.toggle(msg, pulse, &indices, ctx, fx);
        true
    }

    /// Shared by pad and pointer. Left unpins, right pins, A flips; already-in-state is a thud.
    fn toggle(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        indices: &[usize],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(&host_idx) = indices.get(self.list.cursor) else {
            return pulse;
        };
        let target = match msg {
            ListMsg::Adjust(delta) => delta > 0,
            ListMsg::Activate => !self.pinned(ctx, host_idx),
            ListMsg::None => return pulse,
        };
        if self.pinned(ctx, host_idx) == target {
            return Some(MenuPulse::Boundary);
        }
        fx.cmds.push(ConsoleCmd::SetPin {
            key: ctx.hosts[host_idx].key.clone(),
            preset_id: self.preset_id.clone(),
            pin: target,
        });
        Some(MenuPulse::Move)
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        if host_indices(ctx).is_empty() {
            return vec![Hint::new(HintKey::Back, "Done")];
        }
        vec![
            Hint::new(HintKey::Confirm, "Pin / Unpin"),
            Hint::new(HintKey::Back, "Done"),
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
        let indices = host_indices(ctx);
        let cx = f64::from(rect.left) + f64::from(rect.width()) / 2.0;
        if indices.is_empty() {
            fonts.centered(
                canvas,
                "No saved hosts yet — pair with a host first, then pin this preset to it.",
                W::Regular,
                14.0 * k,
                fg(0.55),
                cx,
                f64::from(rect.top) + f64::from(rect.height()) / 2.0,
                f64::from(rect.width()) * 0.7,
            );
            return;
        }
        // Detail band under the list; 34 matches the settings screen.
        let detail_h = 34.0 * k;
        let list_rect = Rect::from_ltrb(
            rect.left,
            rect.top,
            rect.right,
            rect.bottom - detail_h as f32,
        );
        let rows: Vec<RowSpec> = indices
            .iter()
            .map(|&i| {
                let h = &ctx.hosts[i];
                let pinned = self.pinned(ctx, i);
                RowSpec {
                    header: None,
                    label: h.name.clone(),
                    value: Some(if pinned {
                        "Pinned".into()
                    } else {
                        "Off".into()
                    }),
                    value_dim: !pinned,
                    caret: false,
                    adjustable: true,
                    enabled: true,
                    ..RowSpec::default()
                }
            })
            .collect();
        self.list
            .render(canvas, list_rect, &rows, fonts, k, dt, true);
        fonts.centered(
            canvas,
            "A pinned preset appears as its own card on the host — one press connects with it.",
            W::Regular,
            13.0 * k,
            fg(0.55),
            cx,
            f64::from(rect.bottom) - detail_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{HostRow, PresetChip};
    use crate::screens::Outbox;
    use pf_client_core::trust::Settings;

    fn host(key: &str, saved: bool, pin: Option<&str>) -> HostRow {
        HostRow {
            key: key.into(),
            id: None,
            name: key.into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            fp_hex: key.into(),
            paired: true,
            saved,
            online: true,
            mgmt_port: 47990,
            can_wake: false,
            clipboard_sync: false,
            last_used: None,
            os: String::new(),
            actions: Vec::new(),
            pin: pin.map(|id| PresetChip {
                id: id.into(),
                name: "Work".into(),
                accent: None,
                bitrate_kbps: None,
            }),
            bound_preset: None,
            running: String::new(),
            game_presets: Default::default(),
        }
    }

    #[test]
    fn toggling_sends_set_pin_for_the_focused_host() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        let hosts = [host("aa", true, None), host("bb", true, None)];
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = PinHostsScreen::new("p1".into(), "Work".into());
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetPin {
                key: "aa".into(),
                preset_id: "p1".into(),
                pin: true,
            }]
        );

        // Left on an unpinned host is already off: boundary, no command.
        let mut fx = Outbox::default();
        let pulse = s.menu(
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Left),
            &mut ctx,
            &mut fx,
        );
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }

    #[test]
    fn state_reads_from_the_models_pinned_rows() {
        let mut settings = Settings::default();
        let pads = Vec::new();
        let library = crate::library::LibraryShared::default();
        // Primary "aa" plus a pinned-card row for p1: Confirm on the primary unpins.
        let hosts = [host("aa", true, None), host("aa\0p1", true, Some("p1"))];
        let mut ctx = Ctx {
            hosts: &hosts,
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform: crate::platform::Platform::Desktop,
            screen: None,
            pads: &pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut s = PinHostsScreen::new("p1".into(), "Work".into());
        assert_eq!(host_indices(&ctx).len(), 1);
        let mut fx = Outbox::default();
        s.menu(MenuEvent::Confirm, &mut ctx, &mut fx);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::SetPin {
                key: "aa".into(),
                preset_id: "p1".into(),
                pin: false,
            }]
        );
    }
}
