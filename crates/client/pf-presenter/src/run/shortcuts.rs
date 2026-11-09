//! The stream's shortcuts on this side: the keyboard's engine, and what a fired action does.
//! Each forwarded pad runs its own engine in the gamepad worker, which sends what it fires
//! here; a pad's Guide and Quick Access go to that pad's wire there instead.

use super::shell::ACCESS_NOTICE_S;
use super::stream::streamed_game;
use super::*;
use pf_client_core::overlay_actions::{key_vk, OverlayConfig, RingPlatform, SlotId};
use pf_client_core::shortcuts::{
    self, Action, Binding, Client, Device, Engine, Out, Phase, Shortcuts,
};
use pf_l10n::client as c;
use sdl3::keyboard::{Keycode, Scancode};
use std::rc::Rc;

/// End game, or a dangerous host action, fired once waits this long for the press that
/// confirms it.
const ARM: Duration = Duration::from_secs(2);

/// A launch's shortcuts: the hints' copy, the keyboard's engine, and the pads' bindings sent
/// to the worker. The session binary resolves the blob; empty is the desktop's defaults.
pub(super) fn adopt(blob: &str, gamepad: &GamepadService, ring: bool) -> (Rc<Shortcuts>, Keys) {
    let s = shortcuts::resolve(blob, Client::Desktop, in_gamescope(), "auto");
    let takes = |b: &&Binding| match &b.action {
        // No console, no dial to open.
        Action::Ring => ring,
        // Nothing performs these here, so their press stays the game's.
        Action::Slot(SlotId::Clipboard | SlotId::Pad | SlotId::SendText | SlotId::SwapScreens)
        | Action::Unknown(_) => false,
        Action::Slot(_) => true,
    };
    let on = |device| -> Vec<Binding> {
        let mine = s.bindings.iter().filter(|b| b.trigger.device() == device);
        mine.filter(takes).cloned().collect()
    };
    gamepad.set_shortcuts(on(Device::Pad));
    let keys = Keys::new(on(Device::Keyboard));
    (Rc::new(s), keys)
}

/// The keyboard's engine, and the id each key went into it as.
pub(super) struct Keys {
    engine: Engine,
    /// Every id a binding names. A key outside them never enters the engine, so a held W
    /// leaves Ctrl+Alt+Shift+Q whole.
    bound: Vec<u32>,
    down: Vec<(Scancode, u32)>,
}

impl Keys {
    fn new(bindings: Vec<Binding>) -> Keys {
        let mut bound: Vec<u32> = bindings.iter().flat_map(|b| b.trigger.ids()).collect();
        bound.sort_unstable();
        bound.dedup();
        Keys {
            engine: Engine::new(&bindings, Device::Keyboard),
            bound,
            down: Vec::new(),
        }
    }

    /// A first press. A fire comes back with the lift of its chord's keys; anything else is
    /// capture's to send, since the engine never holds a key back.
    pub(super) fn press(&mut self, keycode: Option<Keycode>, sc: Scancode) -> Vec<Out> {
        let Some(id) = key_id(keycode, sc, &self.bound) else {
            return Vec::new();
        };
        self.down.retain(|&(s, _)| s != sc);
        self.down.push((sc, id));
        self.engine
            .step(shortcuts::Event::Press(id), Instant::now())
    }

    /// A release; capture still sends its own. Left and right Ctrl are one key to the engine,
    /// which lets go with the last of the two.
    pub(super) fn release(&mut self, sc: Scancode) {
        let Some(i) = self.down.iter().position(|&(s, _)| s == sc) else {
            return;
        };
        let (_, id) = self.down.remove(i);
        if !self.down.iter().any(|&(_, d)| d == id) {
            self.engine
                .step(shortcuts::Event::Release(id), Instant::now());
        }
    }

    /// The keys down now that went in as one of `ids`.
    fn held(&self, ids: &[u32]) -> Vec<Scancode> {
        let of = self.down.iter().filter(|(_, id)| ids.contains(id));
        of.map(|&(sc, _)| sc).collect()
    }
}

/// The id a key goes into the engine as, when a binding names it: Ctrl, Alt or Shift on either
/// side; another key by the letter it prints (AZERTY's Q), else by its US position.
fn key_id(keycode: Option<Keycode>, sc: Scancode, bound: &[u32]) -> Option<u32> {
    let printed = keycode
        .and_then(|k| char::from_u32(k as u32))
        .and_then(|c| key_vk(c.encode_utf8(&mut [0; 4])));
    let placed = match sc {
        Scancode::LCtrl | Scancode::RCtrl => key_vk("ctrl"),
        Scancode::LAlt | Scancode::RAlt => key_vk("alt"),
        Scancode::LShift | Scancode::RShift => key_vk("shift"),
        sc => crate::keymap_sdl::scancode_to_vk(sc),
    };
    let ids = [printed, placed].into_iter().flatten().map(u32::from);
    ids.into_iter().find(|id| bound.contains(id))
}

impl Shell {
    /// A key completed a keyboard shortcut: the chord's keys still down on the host come up,
    /// then the action runs. The completing key never reaches the host.
    pub(super) fn on_key_shortcut(&mut self, stream: &mut Option<StreamState>, outs: Vec<Out>) {
        for out in outs {
            match out {
                Out::Lift(ids) => {
                    let held = self.keys.held(&ids);
                    if let Some(cap) = capture_mut(stream) {
                        held.into_iter().for_each(|sc| cap.on_key_up(sc));
                    }
                }
                Out::Fire(action, phase) => self.fire_action(stream, None, &action, phase),
                Out::Forward(..) => {}
            }
        }
    }

    /// What a shortcut does: the dial's own command where it has one. `pad` is the wire index
    /// of the pad that fired it, `None` for the keyboard. With no stream only the window's
    /// fullscreen acts.
    pub(super) fn fire_action(
        &mut self,
        stream: &mut Option<StreamState>,
        pad: Option<u8>,
        action: &Action,
        phase: Phase,
    ) {
        tracing::info!(action = %action.id(), ?phase, ?pad, "shortcut");
        let slot = match action {
            _ if stream.is_none() => {
                if *action == Action::Slot(SlotId::Fullscreen) {
                    self.toggle_fullscreen(None);
                }
                return;
            }
            Action::Ring => return self.toggle_ring(pad),
            Action::Unknown(_) => return,
            Action::Slot(slot) => slot,
        };
        match slot {
            // A pad's way back in and out, as the exit chord always was.
            SlotId::ReleaseInput if pad.is_some() => self.pad_escape(stream),
            SlotId::Shortcut(id) => self.send_key_combination(stream, id, phase, pad),
            SlotId::Host(_) | SlotId::EndGame => self.arm(stream, slot),
            slot => match RingCommand::for_slot(slot) {
                Some(cmd) => self.run_ring_command(stream, cmd, pad),
                None => tracing::debug!(slot = %slot.id(), "shortcut does nothing on this client"),
            },
        }
    }

    /// The dial at the window centre (a locked pointer has no position worth opening at). Its
    /// highlight starts on the centre, so Select+A then A opens the sheet.
    fn toggle_ring(&mut self, pad: Option<u8>) {
        let Some(o) = self.overlay.as_mut() else {
            return;
        };
        self.ring_opener = pad;
        let (pw, ph) = self.window.size_in_pixels();
        o.ring_input(RingInput::Toggle {
            x: pw as f32 / 2.0,
            y: ph as f32 / 2.0,
        });
    }

    /// A pad's Release input: capture out, or back in, so a pad alone gets back in. A release
    /// leaves fullscreen that the press back in restores. Gamescope has nothing to release
    /// into, nor has a stream under the ring or out of focus.
    fn pad_escape(&mut self, stream: &mut Option<StreamState>) {
        if in_gamescope() || self.ring_was_open {
            return;
        }
        let back_in = capture_mut(stream).is_some_and(|c| !c.captured() && c.can_capture());
        if back_in {
            if self.focus_lost {
                return;
            }
            if let Some(cap) = capture_mut(stream) {
                if cap.engage() {
                    self.capture_on(cap);
                }
            }
        } else if let Some(cap) = capture_mut(stream) {
            if cap.release(true) {
                self.capture_off();
            }
        }
        tracing::info!(captured = back_in, "controller shortcut: release/engage");
        let leave = !back_in && self.fullscreen && !self.opts.fullscreen;
        let restore = back_in && std::mem::take(&mut self.pad_left_fullscreen);
        if leave || (restore && !self.fullscreen) {
            self.pad_left_fullscreen = leave;
            self.fullscreen = restore;
            let _ = self.window.set_fullscreen(restore);
            if let Some(st) = stream.as_mut() {
                st.forget_refresh_verdict();
            }
        }
    }

    /// A key combination from the ring's list: tapped, or held down until its `Up`.
    fn send_key_combination(
        &mut self,
        stream: &mut Option<StreamState>,
        id: &str,
        phase: Phase,
        pad: Option<u8>,
    ) {
        let Some(st) = stream.as_mut() else {
            return;
        };
        let cfg = OverlayConfig::parse(&st.params.overlay_actions, RingPlatform::Desktop);
        let Some(keys) = cfg.shortcut(id).map(|s| s.keys.clone()) else {
            tracing::debug!(id, "shortcut names no key combination");
            return;
        };
        let down = match phase {
            Phase::Down => true,
            Phase::Up => false,
            Phase::Once | Phase::Tap => {
                return self.run_ring_command(stream, RingCommand::Shortcut(keys), pad);
            }
        };
        let vks: Vec<u8> = keys.iter().filter_map(|k| key_vk(k)).collect();
        if let (true, Some(cap)) = (vks.len() == keys.len(), st.capture.as_mut()) {
            cap.send_keys(&vks, down);
        }
    }

    /// End game, and a host action the host calls dangerous, fire on a second press within
    /// [`ARM`], as on the dial; the first says what the next one does.
    fn arm(&mut self, stream: &mut Option<StreamState>, slot: &SlotId) {
        let Some(st) = stream.as_mut() else {
            return;
        };
        let mgmt = st.connector.as_ref().map_or(0, |c| c.mgmt_port());
        let host = match slot {
            SlotId::Host(id) => {
                let cached = pf_client_core::host_actions::cached(&st.fp_hex);
                match cached.into_iter().find(|a| &a.id == id) {
                    Some(a) if a.available => Some(a),
                    other => {
                        let why = other.and_then(|a| a.unavailable_reason);
                        let why = why.unwrap_or_else(|| c::ring_host_not_offered().into());
                        st.session_notice = Some((why, Instant::now()));
                        return;
                    }
                }
            }
            _ => None,
        };
        let game = streamed_game(&st.params.host, (mgmt, st.params.port), &st.fp_hex);
        if *slot == SlotId::EndGame && game.is_none() {
            st.session_notice = Some((c::ring_end_game_unavailable().into(), Instant::now()));
            return;
        }
        let confirmed = self
            .armed
            .take()
            .is_some_and(|(armed, at)| armed == *slot && at.elapsed() < ARM);
        if !confirmed && host.as_ref().is_none_or(|a| a.danger) {
            let label = host.as_ref().map_or("", |a| a.label());
            let ask = match slot {
                SlotId::EndGame => c::ring_end_game_confirm().into(),
                SlotId::Host(id) if id == "power.sleep" => c::ring_host_sleep_confirm().into(),
                SlotId::Host(id) if id == "power.reboot" => c::ring_host_reboot_confirm().into(),
                SlotId::Host(id) if id == "power.shutdown" => {
                    c::ring_host_shutdown_confirm().into()
                }
                _ => c::ring_host_action_confirm(label),
            };
            // Dated so the toast clears with the arm.
            let shown = Duration::from_secs(ACCESS_NOTICE_S) - ARM;
            let at = Instant::now()
                .checked_sub(shown)
                .unwrap_or_else(Instant::now);
            st.session_notice = Some((ask, at));
            self.armed = Some((slot.clone(), Instant::now()));
            return;
        }
        match (host, game) {
            (Some(a), _) => self.run_host_action(st, &a.id, a.label()),
            (None, Some((app_id, title))) => {
                self.ring_command(RingCommand::EndGame { app_id, title }, st)
            }
            (None, None) => {}
        }
    }

    /// A host action off the render thread, as the dial's: the host's answer is the notice.
    fn run_host_action(&self, st: &mut StreamState, id: &str, label: &str) {
        let Some(conn) = &st.connector else {
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let host_name = self.opts.window_title.clone();
        let (addr, mgmt, port) = (st.params.host.clone(), conn.mgmt_port(), st.params.port);
        let (fp, id, label) = (st.fp_hex.clone(), id.to_string(), label.to_string());
        std::thread::Builder::new()
            .name("punktfunk-hostaction".into())
            .spawn(move || {
                let said = match pf_client_core::trust::load_or_create_identity() {
                    Ok(identity) => pf_client_core::host_actions::run(
                        &host_name, &addr, mgmt, port, &identity, &fp, &id, &label,
                    )
                    .unwrap_or_else(|e| e),
                    Err(e) => c::hostcard_action_failed(&label, e),
                };
                let _ = tx.send(said);
            })
            .ok();
        st.host_notice = Some(rx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys() -> Keys {
        let s = shortcuts::defaults(Client::Desktop, false, "auto");
        let mine = s
            .bindings
            .into_iter()
            .filter(|b| b.trigger.device() == Device::Keyboard);
        Keys::new(mine.collect())
    }
    fn vk(name: &str) -> Option<u32> {
        key_vk(name).map(u32::from)
    }

    /// A key matches by the letter it prints or by its position, and a key no binding names
    /// never enters the engine.
    #[test]
    fn a_key_goes_in_by_its_letter_or_its_position() {
        let k = keys();
        let id = |keycode, sc| key_id(keycode, sc, &k.bound);
        assert_eq!(id(Some(Keycode::Q), Scancode::Q), vk("q"));
        assert_eq!(id(Some(Keycode::Q), Scancode::A), vk("q"), "AZERTY's Q");
        assert_eq!(
            id(None, Scancode::D),
            vk("d"),
            "a layout without the letter"
        );
        assert_eq!(id(None, Scancode::F11), vk("f11"), "a media key's F11");
        assert_eq!(id(Some(Keycode::RCtrl), Scancode::RCtrl), vk("ctrl"));
        assert_eq!(
            id(Some(Keycode::A), Scancode::A),
            None,
            "no binding names A"
        );
    }

    /// Ctrl+Alt+Shift+Q fires on the Q, lifts the three modifiers, and a W held through it
    /// changes nothing.
    #[test]
    fn a_key_chord_fires_and_lifts_its_modifiers() {
        let mut k = keys();
        assert!(k.press(Some(Keycode::W), Scancode::W).is_empty());
        for (code, sc) in [
            (Keycode::LCtrl, Scancode::LCtrl),
            (Keycode::RAlt, Scancode::RAlt),
            (Keycode::LShift, Scancode::LShift),
        ] {
            let id = key_id(Some(code), sc, &k.bound).unwrap();
            assert_eq!(k.press(Some(code), sc), [Out::Forward(id, true)]);
        }
        let outs = k.press(Some(Keycode::Q), Scancode::A);
        let fire = Out::Fire(Action::Slot(SlotId::ReleaseInput), Phase::Once);
        assert_eq!(outs.last(), Some(&fire), "{outs:?}");
        let Some(Out::Lift(ids)) = outs.first() else {
            panic!("no lift: {outs:?}");
        };
        let held = k.held(ids);
        assert_eq!(held.len(), 3, "{held:?}");
        for sc in [Scancode::LCtrl, Scancode::RAlt, Scancode::LShift] {
            assert!(held.contains(&sc), "{sc:?} in {held:?}");
        }
    }
}
