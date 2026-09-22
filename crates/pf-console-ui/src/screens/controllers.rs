//! "Connected controllers": attached pads, their identity lines, and the grants
//! and tests only the platform can perform. Reached from the settings Controller
//! tab (Android; desktop has no USB capture and no grants).
//!
//! Only devices the OS classifies as a gamepad are forwarded. Adapters often
//! enumerate as something else, so the identity line under each name is the
//! support answer, not the name.
//!
//! Grant dialogs and a rumble pulse on a real `InputDevice` stay with the host
//! via [`ConsoleCmd::PadAction`]. The console sees only aggregated `MenuSample`;
//! a per-device axis test needs a wider pad-sample bridge.

use crate::glyphs::{Hint, HintKey};
use crate::model::ConsoleCmd;
use crate::platform::Platform;
use crate::pointer::Pointer;
use crate::screens::{Ctx, Outbox};
use crate::theme::{fg, Fonts, W};
use crate::widgets::{ListMsg, MenuList, RowSpec};
use pf_client_core::menu_nav::{MenuEvent, MenuPulse, PadInfo};
use skia_safe::{Canvas, Rect};

/// Host-only pad work: a permission dialog or a real device handle. Neither
/// exists on this side of the bridge.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PadAction {
    /// Diagnostic: is the focused pad's motor wired.
    Rumble,
    /// Always shown: without `BLUETOOTH_CONNECT` a BLE SC2 is absent, not idle —
    /// detection cannot run, so hiding the row behind it would hide the grant.
    Sc2Bluetooth,
    Sc2Usb,
    DsUsb,
    /// Ungated by session: a stream that is not sending haptics must not hide
    /// the pad-audio test that rules the pad out.
    DsHaptics,
}

impl PadAction {
    /// Stable id the host matches on (JNI inside [`ConsoleCmd::PadAction`]).
    pub(crate) fn id(self) -> &'static str {
        match self {
            PadAction::Rumble => "rumble",
            PadAction::Sc2Bluetooth => "sc2_bluetooth",
            PadAction::Sc2Usb => "sc2_usb",
            PadAction::DsUsb => "ds_usb",
            PadAction::DsHaptics => "ds_haptics",
        }
    }
}

/// Grant/test rows, gated like `settings::row_on`. Desktop has no USB capture
/// and no grants; a row here would change nothing.
const PASSTHROUGH: [(PadAction, &str, &str); 4] = [
    (
        PadAction::Sc2Bluetooth,
        "Steam Controller 2 over Bluetooth",
        "Grant",
    ),
    (PadAction::Sc2Usb, "Steam Controller 2 over USB", "Grant"),
    (PadAction::DsUsb, "DualSense / DualShock over USB", "Grant"),
    (PadAction::DsHaptics, "DualSense haptics self-test", "Test"),
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Row {
    Pad(usize),
    /// Inert placeholder so the list is never empty; grants below stay reachable.
    NoPads,
    Passthrough(usize),
}

fn rows_for(ctx: &Ctx) -> Vec<Row> {
    let mut rows: Vec<Row> = if ctx.pads.is_empty() {
        vec![Row::NoPads]
    } else {
        (0..ctx.pads.len()).map(Row::Pad).collect()
    };
    if ctx.platform == Platform::Android {
        rows.extend((0..PASSTHROUGH.len()).map(Row::Passthrough));
    }
    rows
}

pub(crate) struct ControllersScreen {
    pub(super) list: MenuList,
}

impl ControllersScreen {
    pub(crate) fn new() -> ControllersScreen {
        ControllersScreen {
            list: MenuList::new(),
        }
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
        let rows = rows_for(ctx);
        let (msg, pulse) = self.list.menu(ev, rows.len());
        self.activate(msg, pulse, &rows, ctx, fx)
    }

    pub(crate) fn pointer(&mut self, p: Pointer, ctx: &mut Ctx, fx: &mut Outbox) -> bool {
        let rows = rows_for(ctx);
        let (msg, pulse) = self.list.pointer(p, rows.len());
        if matches!(msg, ListMsg::None) && pulse.is_none() {
            return false;
        }
        self.activate(msg, pulse, &rows, ctx, fx);
        true
    }

    /// Pad and pointer share this so a click and A cannot diverge.
    fn activate(
        &mut self,
        msg: ListMsg,
        pulse: Option<MenuPulse>,
        rows: &[Row],
        ctx: &mut Ctx,
        fx: &mut Outbox,
    ) -> Option<MenuPulse> {
        let Some(&focused) = rows.get(self.list.cursor) else {
            return pulse;
        };
        // No row is adjustable; Adjust is always a thud.
        if matches!(msg, ListMsg::Adjust(_)) {
            return Some(MenuPulse::Boundary);
        }
        if !matches!(msg, ListMsg::Activate) {
            return pulse;
        }
        let (action, pad_key) = match focused {
            Row::NoPads => return Some(MenuPulse::Boundary),
            Row::Pad(i) => {
                // No motor: thud. The host would drop a rumble command silently.
                if !ctx.pads[i].rumble {
                    return Some(MenuPulse::Boundary);
                }
                (PadAction::Rumble, ctx.pads[i].key.clone())
            }
            // Empty key: the device is not an input device yet (SC2 keyboard/mouse mode).
            Row::Passthrough(i) => (PASSTHROUGH[i].0, String::new()),
        };
        fx.cmds.push(ConsoleCmd::PadAction {
            action: action.id().to_string(),
            pad_key,
        });
        pulse
    }

    pub(crate) fn hints(&self, ctx: &Ctx) -> Vec<Hint> {
        let rows = rows_for(ctx);
        let confirm = match rows.get(self.list.cursor) {
            Some(Row::Pad(i)) if ctx.pads[*i].rumble => Some("Test rumble"),
            Some(Row::Passthrough(i)) => Some(match PASSTHROUGH[*i].0 {
                PadAction::DsHaptics => "Test haptics",
                _ => "Grant access",
            }),
            _ => None,
        };
        let mut hints = Vec::new();
        if let Some(label) = confirm {
            hints.push(Hint::new(HintKey::Confirm, label));
        }
        hints.push(Hint::new(HintKey::Back, "Done"));
        hints
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
        // Identity line under the list — same 34 px band as settings; too long for the row.
        let detail_h = 34.0 * k;
        let rows = rows_for(ctx);
        let specs: Vec<RowSpec> = rows.iter().map(|r| spec(*r, ctx)).collect();
        self.list.render(
            canvas,
            Rect::from_ltrb(
                rect.left,
                rect.top,
                rect.right,
                rect.bottom - detail_h as f32,
            ),
            &specs,
            fonts,
            k,
            dt,
            true,
        );
        let detail = rows
            .get(self.list.cursor)
            .map_or_else(String::new, |r| detail(*r, ctx));
        fonts.centered(
            canvas,
            &detail,
            W::Regular,
            13.0 * k,
            fg(0.55),
            f64::from(rect.left) + f64::from(rect.width()) / 2.0,
            f64::from(rect.bottom) - detail_h + 6.0 * k,
            f64::from(rect.width()) * 0.8,
        );
    }
}

fn spec(row: Row, ctx: &Ctx) -> RowSpec {
    match row {
        Row::NoPads => RowSpec {
            header: Some("Gamepads"),
            ..RowSpec::action("No controller detected", false)
        },
        Row::Pad(i) => {
            let pad = &ctx.pads[i];
            RowSpec {
                header: (i == 0).then_some("Gamepads"),
                label: pad.name.clone(),
                value: Some(
                    if pad.rumble {
                        "Test rumble"
                    } else {
                        "No rumble"
                    }
                    .into(),
                ),
                value_dim: !pad.rumble,
                caret: false,
                adjustable: false,
                enabled: pad.rumble,
                ..RowSpec::default()
            }
        }
        Row::Passthrough(i) => {
            let (_, label, verb) = PASSTHROUGH[i];
            RowSpec {
                header: (i == 0).then_some("Passthrough"),
                label: label.into(),
                value: Some(verb.into()),
                value_dim: false,
                caret: false,
                adjustable: false,
                enabled: true,
                ..RowSpec::default()
            }
        }
    }
}

fn detail(row: Row, ctx: &Ctx) -> String {
    match row {
        Row::NoPads => "Punktfunk only forwards devices the system classifies as a gamepad or \
                        joystick — a pad behind an adapter or hub may enumerate with the \
                        adapter's identity, or not at all."
            .into(),
        Row::Pad(i) => pad_detail(&ctx.pads[i]),
        Row::Passthrough(i) => match PASSTHROUGH[i].0 {
            PadAction::Sc2Bluetooth => {
                "A Steam Controller 2 paired over Bluetooth can't be detected at all without \
                 Bluetooth access. Wired and Puck-dongle controllers need no permission."
                    .into()
            }
            PadAction::Sc2Usb => {
                "A wired or Puck-dongle Steam Controller 2 needs USB access to be captured; \
                 until then it stays in its built-in keyboard/mouse mode."
                    .into()
            }
            PadAction::DsUsb => {
                "A wired DualSense or DualShock 4 needs USB access to be captured — with it, \
                 streams drive rumble, adaptive triggers, lightbar and gyro directly."
                    .into()
            }
            PadAction::DsHaptics => {
                "Play a short tone through a wired DualSense's audio endpoint, to tell a pad \
                 that can't do haptics from a stream that is not sending them."
                    .into()
            }
            // Not a passthrough row; pads carry rumble.
            PadAction::Rumble => String::new(),
        },
    }
}

fn pad_detail(pad: &PadInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !pad.detail.is_empty() {
        parts.push(pad.detail.clone());
    }
    if !pad.forwarded {
        parts.push("not forwarded — not classified as a gamepad".into());
    }
    let kind = pad.kind_label();
    parts.push(format!(
        "streams as {}",
        if kind.is_empty() { "Xbox 360" } else { kind }
    ));
    if let Some(b) = pad.battery {
        parts.push(if b.charging {
            format!("battery {} %, charging", b.percent)
        } else {
            format!("battery {} %", b.percent)
        });
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::trust::Settings;
    use punktfunk_core::config::GamepadPref;

    fn pad(name: &str, rumble: bool) -> PadInfo {
        PadInfo {
            name: name.into(),
            key: format!("054c:0ce6:{name}"),
            pref: GamepadPref::DualSense,
            steam_virtual: false,
            battery: None,
            detail: "054C:0CE6 · gamepad".into(),
            forwarded: true,
            rumble,
        }
    }

    fn drive(
        screen: &mut ControllersScreen,
        platform: Platform,
        pads: &[PadInfo],
        ev: MenuEvent,
    ) -> (Outbox, Option<MenuPulse>) {
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        let mut ctx = Ctx {
            hosts: &[],
            library: &library,
            settings: &mut settings,
            store: crate::store::file_store(),
            platform,
            screen: None,
            pads,
            deck: false,
            fallback_ui: false,
            pyrowave_ok: true,
            av1_ok: true,
            device_name: "t",
            t: 0.0,
        };
        let mut fx = Outbox::default();
        let pulse = screen.menu(ev, &mut ctx, &mut fx);
        (fx, pulse)
    }

    #[test]
    fn a_on_a_pad_asks_the_host_for_a_rumble_pulse() {
        let pads = [pad("DualSense", true)];
        let mut s = ControllersScreen::new();
        let (fx, _) = drive(&mut s, Platform::Android, &pads, MenuEvent::Confirm);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PadAction {
                action: "rumble".into(),
                pad_key: "054c:0ce6:DualSense".into(),
            }]
        );
    }

    #[test]
    fn a_pad_with_no_motor_thuds_instead_of_sending_a_pulse() {
        let pads = [pad("Adapter", false)];
        let mut s = ControllersScreen::new();
        let (fx, pulse) = drive(&mut s, Platform::Android, &pads, MenuEvent::Confirm);
        assert!(fx.cmds.is_empty());
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }

    #[test]
    fn the_grant_rows_are_androids_alone_and_carry_no_pad_key() {
        let pads = [pad("DualSense", true)];
        let mut settings = Settings::default();
        let library = crate::library::LibraryShared::default();
        fn ctx<'a>(
            platform: Platform,
            settings: &'a mut Settings,
            library: &'a crate::library::LibraryShared,
            pads: &'a [PadInfo],
        ) -> Ctx<'a> {
            Ctx {
                hosts: &[],
                library,
                settings,
                store: crate::store::file_store(),
                platform,
                screen: None,
                pads,
                deck: false,
                fallback_ui: false,
                pyrowave_ok: true,
                av1_ok: true,
                device_name: "t",
                t: 0.0,
            }
        }
        assert_eq!(
            rows_for(&ctx(Platform::Desktop, &mut settings, &library, &pads)).len(),
            1
        );
        assert_eq!(
            rows_for(&ctx(Platform::Android, &mut settings, &library, &pads)).len(),
            1 + PASSTHROUGH.len()
        );

        let mut s = ControllersScreen::new();
        drive(
            &mut s,
            Platform::Android,
            &pads,
            MenuEvent::Move(pf_client_core::menu_nav::MenuDir::Down),
        );
        let (fx, _) = drive(&mut s, Platform::Android, &pads, MenuEvent::Confirm);
        assert_eq!(
            fx.cmds,
            vec![ConsoleCmd::PadAction {
                action: "sc2_bluetooth".into(),
                pad_key: String::new(),
            }]
        );
    }

    #[test]
    fn with_no_pads_the_list_still_has_the_grants_under_an_inert_row() {
        let mut s = ControllersScreen::new();
        let (fx, pulse) = drive(&mut s, Platform::Android, &[], MenuEvent::Confirm);
        assert!(fx.cmds.is_empty(), "the empty-state row does nothing");
        assert!(matches!(pulse, Some(MenuPulse::Boundary)));
    }
}
