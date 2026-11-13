//! Enable one head over `zwlr_output_manager_v1` at a named mode.
//!
//! Umbriel's `output-enable` reuses the head's current mode. A monitor switched off while
//! disabled comes back disabled with no mode, so that request fails; this one names it.
//! Only the target head is configured: wlroots leaves the others as they are.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::{self as config_head, ZwlrOutputConfigurationHeadV1},
    zwlr_output_configuration_v1::{self as config, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self as head, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self as manager, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self as mode, ZwlrOutputModeV1},
};

/// Connect, list and apply; a wedged compositor must not pin the teardown thread.
const OP_BUDGET: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct ModeInfo {
    width: i32,
    height: i32,
    refresh_mhz: i32,
    preferred: bool,
}

struct HeadInfo {
    proxy: ZwlrOutputHeadV1,
    name: String,
    modes: Vec<ZwlrOutputModeV1>,
}

#[derive(Default)]
struct State {
    manager: Option<ZwlrOutputManagerV1>,
    serial: Option<u32>,
    heads: HashMap<ObjectId, HeadInfo>,
    modes: HashMap<ObjectId, ModeInfo>,
    answer: Option<config::Event>,
    sync_done: u32,
}

/// Enable `name` at `want` (`(w, h, Hz)`, Hz rounded) when it lists that mode, else at its
/// preferred one. `Ok` once Umbriel answers `succeeded`.
pub(crate) fn enable(name: &str, want: Option<(u32, u32, u32)>) -> Result<()> {
    let conn = crate::session::wayland_connect().context("connect to the wayland socket")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut st = State::default();
    let deadline = Instant::now() + OP_BUDGET;
    let pumped = crate::wl_pump::sync_barrier(&conn, &mut queue, &mut st, 1, deadline, None)?;
    if !matches!(pumped, crate::wl_pump::Pumped::Done) {
        bail!("registry not read in time");
    }
    let manager = st.manager.clone().context("no zwlr_output_manager_v1")?;
    crate::wl_pump::pump_until(&conn, &mut queue, &mut st, Some(deadline), None, |s| {
        s.serial.is_some()
    })?;
    let serial = st.serial.context("head list not read in time")?;
    let head = st
        .heads
        .values()
        .find(|h| h.name == name)
        .with_context(|| format!("{name} not listed"))?;
    let infos: Vec<ModeInfo> = head
        .modes
        .iter()
        .map(|m| st.modes.get(&m.id()).copied().unwrap_or_default())
        .collect();
    let at = pick(&infos, want).with_context(|| format!("{name} lists no mode"))?;
    let cfg = manager.create_configuration(serial, &qh, ());
    cfg.enable_head(&head.proxy, &qh, ())
        .set_mode(&head.modes[at]);
    cfg.apply();
    crate::wl_pump::pump_until(&conn, &mut queue, &mut st, Some(deadline), None, |s| {
        s.answer.is_some()
    })?;
    cfg.destroy();
    match st.answer {
        Some(config::Event::Succeeded) => Ok(()),
        Some(other) => bail!("{name} at {:?}: {other:?}", infos[at]),
        None => bail!("{name}: no answer in time"),
    }
}

/// The mode to enable: `want` when listed, else the preferred one, else the first.
fn pick(modes: &[ModeInfo], want: Option<(u32, u32, u32)>) -> Option<usize> {
    let key = |m: &ModeInfo| {
        let hz = (f64::from(m.refresh_mhz) / 1000.0).round() as u32;
        (m.width as u32, m.height as u32, hz)
    };
    want.and_then(|w| modes.iter().position(|m| key(m) == w))
        .or_else(|| modes.iter().position(|m| m.preferred))
        .or((!modes.is_empty()).then_some(0))
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == ZwlrOutputManagerV1::interface().name {
                state.manager = Some(registry.bind(name, version.min(4), qh, ()));
            }
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            manager::Event::Head { head } => {
                let info = HeadInfo {
                    proxy: head.clone(),
                    name: String::new(),
                    modes: Vec::new(),
                };
                state.heads.insert(head.id(), info);
            }
            manager::Event::Done { serial } => state.serial = Some(serial),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        manager::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: head::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(info) = state.heads.get_mut(&proxy.id()) else {
            return;
        };
        match event {
            head::Event::Name { name } => info.name = name,
            head::Event::Mode { mode } => {
                state.modes.insert(mode.id(), ModeInfo::default());
                info.modes.push(mode);
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        head::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: mode::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(m) = state.modes.get_mut(&proxy.id()) else {
            return;
        };
        match event {
            mode::Event::Size { width, height } => (m.width, m.height) = (width, height),
            mode::Event::Refresh { refresh } => m.refresh_mhz = refresh,
            mode::Event::Preferred => m.preferred = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: config::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.answer = Some(event);
    }
}

// No events; `enable_head` demands the impl.
impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: config_head::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlCallback, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        serial: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            state.sync_done = state.sync_done.max(*serial);
        }
    }
}

impl crate::wl_pump::SyncDone for State {
    fn sync_done(&self) -> u32 {
        self.sync_done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(width: i32, height: i32, refresh_mhz: i32, preferred: bool) -> ModeInfo {
        ModeInfo {
            width,
            height,
            refresh_mhz,
            preferred,
        }
    }

    /// The journaled mode wins when listed; else the preferred one; else the first.
    #[test]
    fn picks_the_journaled_mode_then_the_preferred_one() {
        let modes = [
            mode(3840, 2160, 60_000, true),
            mode(2560, 1440, 143_998, false),
            mode(1920, 1080, 60_000, false),
        ];
        assert_eq!(pick(&modes, Some((2560, 1440, 144))), Some(1));
        assert_eq!(pick(&modes, Some((1280, 720, 60))), Some(0));
        assert_eq!(pick(&modes, None), Some(0));
        assert_eq!(pick(&modes[1..], None), Some(0));
        assert_eq!(pick(&[], Some((1920, 1080, 60))), None);
    }
}
