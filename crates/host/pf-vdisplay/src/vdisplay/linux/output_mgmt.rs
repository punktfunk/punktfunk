//! One head's configuration over `zwlr_output_manager_v1`, and the list of every head.
//!
//! Only the target head is sent. The protocol forbids omitting a head; wlroots 0.20 does not
//! enforce that, and Umbriel and mango apply only the heads named, leaving the rest as they are.
//! Umbriel's `output-enable` reuses the head's current mode, which a monitor switched off while
//! disabled no longer has, so [`enable`] names one. A mango headless head lists no mode at all
//! and takes a custom one.

use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
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
    description: String,
    make: String,
    model: String,
    enabled: bool,
    current: Option<ObjectId>,
    x: i32,
    y: i32,
    scale: f64,
    modes: Vec<ZwlrOutputModeV1>,
}

/// One head as the compositor reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct HeadState {
    pub name: String,
    /// wlroots writes `Headless output N`, `Wayland output N` or `X11 output N` for its own
    /// backends, and make, model and connector for a real head.
    pub description: String,
    pub make: String,
    pub model: String,
    pub enabled: bool,
    /// The current mode as `(width, height, millihertz)`, a custom one included.
    pub mode: Option<(u32, u32, u32)>,
    pub x: i32,
    pub y: i32,
    pub scale: f64,
}

/// What [`configure`] sends for the head.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Set {
    /// Enabled at this `(w, h, Hz)`, Hz rounded, when the head lists it; else at its preferred
    /// mode; else, for a head that lists none, at this one as a custom mode.
    Listed(Option<(u32, u32, u32)>),
    /// Enabled at `(w, h, millihertz)` as a custom mode.
    Custom(u32, u32, u32),
    Off,
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

impl State {
    fn head_state(&self, h: &HeadInfo) -> HeadState {
        let mode = h
            .current
            .as_ref()
            .and_then(|id| self.modes.get(id))
            .map(|m| {
                let d = |v: i32| u32::try_from(v).unwrap_or(0);
                (d(m.width), d(m.height), d(m.refresh_mhz))
            });
        HeadState {
            name: h.name.clone(),
            description: h.description.clone(),
            make: h.make.clone(),
            model: h.model.clone(),
            enabled: h.enabled,
            mode,
            x: h.x,
            y: h.y,
            scale: h.scale,
        }
    }
}

/// A connection that has read the head list: every head, its modes, and the serial to
/// configure against.
struct Listed {
    conn: Connection,
    queue: EventQueue<State>,
    st: State,
    deadline: Instant,
    _registry: WlRegistry,
}

fn list() -> Result<Listed> {
    let conn = crate::session::wayland_connect().context("connect to the wayland socket")?;
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let registry = conn.display().get_registry(&qh, ());
    let mut st = State::default();
    let deadline = Instant::now() + OP_BUDGET;
    let pumped = crate::wl_pump::sync_barrier(&conn, &mut queue, &mut st, 1, deadline, None)?;
    if !matches!(pumped, crate::wl_pump::Pumped::Done) {
        bail!("registry not read in time");
    }
    st.manager.as_ref().context("no zwlr_output_manager_v1")?;
    crate::wl_pump::pump_until(&conn, &mut queue, &mut st, Some(deadline), None, |s| {
        s.serial.is_some()
    })?;
    st.serial.context("head list not read in time")?;
    Ok(Listed {
        conn,
        queue,
        st,
        deadline,
        _registry: registry,
    })
}

/// Every head the compositor lists, disabled ones included.
pub(crate) fn heads() -> Result<Vec<HeadState>> {
    let l = list()?;
    Ok(l.st.heads.values().map(|h| l.st.head_state(h)).collect())
}

/// Enable `name` at `want` (`(w, h, Hz)`, Hz rounded) when it lists that mode, else at its
/// preferred one. `Ok` once the compositor answers `succeeded`.
pub(crate) fn enable(name: &str, want: Option<(u32, u32, u32)>) -> Result<()> {
    configure(name, Set::Listed(want), None)
}

/// Send `set` for `name` alone, enabled at `at` when given. `Ok` once the compositor answers
/// `succeeded`. A head list that changed between the read and the apply answers `cancelled`; it
/// is read again, twice at most.
pub(crate) fn configure(name: &str, set: Set, at: Option<(i32, i32)>) -> Result<()> {
    let mut tries = 0;
    loop {
        tries += 1;
        match configure_once(name, set, at)? {
            (config::Event::Succeeded, _) => return Ok(()),
            (config::Event::Cancelled, _) if tries < 3 => continue,
            (other, what) => bail!("{name} at {what}: {other:?}"),
        }
    }
}

/// One apply of `set`, and what the compositor answered with the setting it was asked for.
fn configure_once(name: &str, set: Set, at: Option<(i32, i32)>) -> Result<(config::Event, String)> {
    let Listed {
        conn,
        mut queue,
        mut st,
        deadline,
        _registry,
    } = list()?;
    let qh = queue.handle();
    let (Some(manager), Some(serial)) = (st.manager.clone(), st.serial) else {
        bail!("head list not read");
    };
    let head = st
        .heads
        .values()
        .find(|h| h.name == name)
        .with_context(|| format!("{name} not listed"))?;
    let cfg = manager.create_configuration(serial, &qh, ());
    let place = |on: &ZwlrOutputConfigurationHeadV1| {
        if let Some((x, y)) = at {
            on.set_position(x, y);
        }
    };
    let what = match set {
        Set::Off => {
            cfg.disable_head(&head.proxy);
            "off".to_string()
        }
        Set::Custom(w, h, mhz) => {
            let i = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
            let on = cfg.enable_head(&head.proxy, &qh, ());
            on.set_custom_mode(i(w), i(h), i(mhz));
            place(&on);
            format!("{w}x{h}@{mhz}mHz")
        }
        Set::Listed(want) => {
            let infos: Vec<ModeInfo> = head
                .modes
                .iter()
                .map(|m| st.modes.get(&m.id()).copied().unwrap_or_default())
                .collect();
            let on = match (pick(&infos, want), want) {
                (Some(i), _) => {
                    let on = cfg.enable_head(&head.proxy, &qh, ());
                    on.set_mode(&head.modes[i]);
                    on
                }
                (None, Some((w, h, hz))) => {
                    let i = |v: u32| i32::try_from(v).unwrap_or(i32::MAX);
                    let on = cfg.enable_head(&head.proxy, &qh, ());
                    on.set_custom_mode(i(w), i(h), i(hz.saturating_mul(1000)));
                    on
                }
                (None, None) => {
                    cfg.destroy();
                    bail!("{name} lists no mode");
                }
            };
            place(&on);
            format!("{want:?}")
        }
    };
    cfg.apply();
    crate::wl_pump::pump_until(&conn, &mut queue, &mut st, Some(deadline), None, |s| {
        s.answer.is_some()
    })?;
    cfg.destroy();
    match st.answer {
        Some(answer) => Ok((answer, what)),
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
                    description: String::new(),
                    make: String::new(),
                    model: String::new(),
                    enabled: false,
                    current: None,
                    x: 0,
                    y: 0,
                    scale: 1.0,
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
            head::Event::Description { description } => info.description = description,
            head::Event::Make { make } => info.make = make,
            head::Event::Model { model } => info.model = model,
            head::Event::Enabled { enabled } => info.enabled = enabled != 0,
            head::Event::CurrentMode { mode } => info.current = Some(mode.id()),
            head::Event::Position { x, y } => (info.x, info.y) = (x, y),
            head::Event::Scale { scale } => info.scale = scale,
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
