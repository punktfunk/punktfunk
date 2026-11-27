//! In-process KDE output management (`kde_output_management_v2` + `kde_output_device_v2`).
//!
//! Topology, restore, position, and custom mode over the compositor's own Wayland.
//! [`super`] can still fall back to `kscreen-doctor`, which talks libkscreen /
//! KDED over D-Bus; a wedge there hangs the stream thread. Every wait here is
//! bounded by [`OP_BUDGET`]; a miss returns a [`Declined`] so the seam
//! ([`super::outputs`]) falls back rather than hanging.
//!
//! Bind every advertised output (classic: one `kde_output_device_v2` global; KWin ≥ 6.7:
//! `kde_output_device_registry_v2` `output` events — both models are kept), read name /
//! enabled / priority / current-mode size, then `kde_output_configuration_v2.apply()` and
//! wait for `applied` / `failed`.

use super::outputs::{self, gate, Declined, Op};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};

// Two vendored protocols, no build.rs. They cannot share one `__interfaces` module:
// each `generate_interfaces!` emits colliding helpers. `management` therefore pulls
// `device`'s interface statics and proxy types first, as `wayland-protocols` does
// for interdependent protocols, so `kde_output_device_v2` / `_mode_v2` args resolve.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod device {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/kde-output-device-v2.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/kde-output-device-v2.xml");
}

#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod management {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use super::super::device::__interfaces::*;
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/kde-output-management-v2.xml");
    }
    use self::__interfaces::*;
    use super::device::*;

    wayland_scanner::generate_client_code!("protocols/kde-output-management-v2.xml");
}

use device::kde_output_device_mode_v2::{Event as ModeEvent, KdeOutputDeviceModeV2 as DeviceMode};
use device::kde_output_device_registry_v2::{
    Event as RegistryEvent, KdeOutputDeviceRegistryV2 as DeviceRegistry,
};
use device::kde_output_device_v2::{Event as DeviceEvent, KdeOutputDeviceV2 as OutputDevice};
use management::kde_mode_list_v2::KdeModeListV2 as ModeList;
use management::kde_output_configuration_v2::{
    Event as ConfigEvent, KdeOutputConfigurationV2 as OutputConfig,
};
use management::kde_output_management_v2::KdeOutputManagementV2 as OutputManagement;

/// Bind `min(advertised, MAX)`. A request newer than v3 is gated on the bound version:
/// [`REPLICATION_SOURCE_SINCE`], [`outputs::CUSTOM_MODES_SINCE`], [`STATED_TIMING_SINCE`]
/// and the device `release` (v21).
const MGMT_MAX: u32 = 22;
const DEVICE_MAX: u32 = 24;
/// `kde_output_device_registry_v2`. KWin ≥ 6.7 advertises this and no per-output
/// `kde_output_device_v2` globals.
const DEVICE_REGISTRY_MAX: u32 = 24;

/// Opcode of `kde_output_device_v2.mode` (0-based) — the event that creates a child
/// `kde_output_device_mode_v2`. Keep in sync with `kde-output-device-v2.xml`.
// `event_created_child!` hard-codes the literal 2; nothing reads this at run time.
// `mode_event_opcode_is_two` is what catches a re-vendored XML that reorders events.
#[allow(dead_code)]
const DEVICE_MODE_EVENT_OPCODE: u16 = 2;
/// Opcode of `kde_output_device_registry_v2.output` — creates a child
/// `kde_output_device_v2`. `finished` is event 0, `output` is event 1.
const REGISTRY_OUTPUT_EVENT_OPCODE: u16 = 1;

/// Budget for one enumerate-then-apply. A healthy roundtrip is a few ms; this only
/// exists so a wedged compositor cannot pin the stream thread.
const OP_BUDGET: Duration = Duration::from_secs(3);

// KWin's CVT generator aligns custom-mode width down to this grain, so the generated
// mode may be a few px narrower than asked. Imported from `super` — a second copy
// of `CVT_H_GRANULARITY` / `MANAGED_PREFIX` would silently diverge on the two values
// that decide which output is ours and which mode we asked for.
use crate::kwin::{CVT_H_GRANULARITY, MANAGED_PREFIX};

/// `kde_output_management_v2.set_replication_source` (and the device's `replication_source` event)
/// arrived in v13. wayland-rs does not range-check requests, so sending one to a lower-version bind
/// would be a protocol error that kills the connection — every call site gates on this.
const REPLICATION_SOURCE_SINCE: u32 = 13;

/// `kde_mode_list_v2.add_cvt` arrived in v22: a mode whose timing the client states.
/// `add_mode` generates CVT timing instead, whose rounded pixel clock moves the rate off
/// the one asked.
const STATED_TIMING_SINCE: u32 = 22;

/// `DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_NVSYNC`, the CVT reduced-blanking polarity.
const STATED_TIMING_FLAGS: u32 = 0x1 | 0x8;

/// The rate KWin derives from a modeline, in millihertz: `CvtModeline::refreshRate`.
fn modeline_mhz(clock_khz: u64, htotal: u64, vtotal: u64) -> u64 {
    (clock_khz * 1_000_000 / htotal + vtotal / 2) / vtotal
}

/// `(pixel clock in kHz, htotal, vtotal)` for `width`×`height` at exactly `mhz`. A clock
/// is whole kilohertz, so the totals are searched for a pair KWin reads back as `mhz`.
fn stated_timing(width: u32, height: u32, mhz: u32) -> Option<(u32, u32, u32)> {
    let (first_h, first_v) = (u64::from(width) + 160, u64::from(height) + 32);
    (first_v..first_v + 64)
        .flat_map(|vtotal| (first_h..first_h + 64).map(move |htotal| (htotal, vtotal)))
        .filter(|&(htotal, vtotal)| htotal <= 0xffff && vtotal <= 0xffff)
        .find_map(|(htotal, vtotal)| {
            let clock = (u64::from(mhz) * htotal * vtotal + 500_000) / 1_000_000;
            let clock = u32::try_from(clock).ok().filter(|&c| c > 0)?;
            (modeline_mhz(u64::from(clock), htotal, vtotal) == u64::from(mhz)).then_some((
                clock,
                htotal as u32,
                vtotal as u32,
            ))
        })
}

/// The `source` value that means "this output mirrors nothing" — KWin's `applyMirroring` looks the
/// source UUID up among the enabled outputs and treats an empty string as no replication at all.
const NO_REPLICATION_SOURCE: &str = "";

/// Whether this output currently mirrors another.
///
/// KWin stores `replicationSource` per monitor-set in `kwinoutputconfig.json` and
/// restores it by output name. Ours is stable (KWin keys scale by it), so a stored
/// source is reapplied every session that reproduces that set: the output then shows
/// the source's viewport, and a mirroring output is not in the priority order.
///
/// The event carries `""` for the ordinary case. `Some("")` is not mirroring —
/// treating event presence as a mirror would de-mirror every output on every apply.
fn is_mirroring(replication_source: Option<&str>) -> bool {
    replication_source.is_some_and(|s| !s.is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TopologyKind {
    /// Make ours the sole desktop: primary + disable every other enabled output.
    Exclusive,
    /// Make ours primary but leave the other outputs enabled.
    Primary,
    /// Add ours beside the others: no primary, nothing disabled. Still an apply —
    /// KWin restores a stored setup onto our name, and the origin has to be sane.
    Extend,
}

#[derive(Default, Clone)]
struct DeviceState {
    /// Global `name` number (higher = more recently advertised). Newest-wins tie-break
    /// during a supersede. Zero on every KWin ≥ 6.7 device: those arrive through
    /// `kde_output_device_registry_v2` and carry no global name (see [`seq`]).
    global: u32,
    /// Announce order on this connection, from 1. Last-resort tie-break behind `global`.
    /// On the registry model every `global` is 0, so without this `max_by_key` follows
    /// `HashMap` iteration (per-process seed) and can pick the predecessor mid-supersede.
    seq: u32,
    name: Option<String>,
    uuid: Option<String>,
    enabled: bool,
    /// Top-left in compositor logical space, from `geometry` — identifies a head when
    /// two share a size (`monitors::PhysicalMonitor`).
    position: (i32, i32),
    make: Option<String>,
    model: Option<String>,
    /// `None` ⇒ the protocol's documented default of 1.
    scale: Option<f64>,
    /// KWin's output priority; 1 is the primary. `None` until the `priority` event (device ≥ v18).
    priority: Option<u32>,
    /// UUID of the output this one mirrors, from `replication_source` (device ≥ v13).
    /// Empty / `None` ⇒ its own desktop. See [`is_mirroring`].
    replication_source: Option<String>,
    /// Current-mode object id; size is looked up in [`State::mode_dims`].
    current_mode: Option<ObjectId>,
    /// Advertised modes in announce order — `(id, proxy)` — so restore can pick a
    /// captured `WxH@Hz`.
    modes: Vec<(ObjectId, DeviceMode)>,
    /// `true` once this output's `done` burst has been seen (state is coherent to read).
    seen_done: bool,
    proxy: Option<OutputDevice>,
}

#[derive(Default)]
struct State {
    management: Option<OutputManagement>,
    mgmt_name_version: Option<(u32, u32)>,
    /// Held for the life of the session; dropping `kde_output_device_registry_v2`
    /// ends the announcements (KWin ≥ 6.7).
    device_registry: Option<DeviceRegistry>,
    devices: HashMap<ObjectId, DeviceState>,
    next_device_seq: u32,
    /// Mode object id → `(width, height, refresh_mHz)`.
    mode_dims: HashMap<ObjectId, (u32, u32, u32)>,
    /// Highest `wl_callback` serial whose `done` has arrived — the barrier the pump waits on.
    sync_done: u32,
    applied: Option<bool>,
    failure_reason: Option<String>,
}

impl State {
    /// Stamp [`DeviceState::seq`] the first time this id is seen. Every path that
    /// creates a device entry goes through here (per-output global, ≥ 6.7 registry,
    /// and the event handler which can race ahead of both), so the counter is announce order.
    fn device_entry(&mut self, id: ObjectId) -> &mut DeviceState {
        // Disjoint field borrows: `entry` holds `devices`, the closure holds only the counter.
        let next = &mut self.next_device_seq;
        self.devices.entry(id).or_insert_with(|| {
            *next += 1;
            DeviceState {
                seq: *next,
                ..Default::default()
            }
        })
    }

    /// Drop the per-output device bound as global `name` (KWin < 6.7, where each output is
    /// its own global and never sends `removed`). Left in place, a vanished output stays an
    /// enabled row that the next apply positions around or prioritises on a dead proxy.
    fn forget_global(&mut self, name: u32) {
        let dead: Vec<ObjectId> = self
            .devices
            .iter()
            .filter(|(_, d)| d.global == name)
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            let Some(d) = self.devices.remove(&id) else {
                continue;
            };
            for (mid, _) in &d.modes {
                self.mode_dims.remove(mid);
            }
            if let Some(mid) = &d.current_mode {
                self.mode_dims.remove(mid);
            }
            if let Some(proxy) = d.proxy {
                if proxy.version() >= 21 {
                    proxy.release();
                }
            }
        }
    }

    /// Drop a `kde_output_device_mode_v2` the compositor has destroyed.
    ///
    /// The vendored XML marks `removed` a destructor, so wayland-rs frees the id KWin
    /// hands its next mode; any proxy kept here names a dead object. `set_custom_modes`
    /// replaces the custom list, so a previous session's mode is destroyed the moment
    /// this one installs its own.
    fn forget_mode(&mut self, id: &ObjectId) {
        self.mode_dims.remove(id);
        for dev in self.devices.values_mut() {
            dev.modes.retain(|(mid, _)| mid != id);
            if dev.current_mode.as_ref() == Some(id) {
                // Do not invent a size for a destroyed mode: a resolve keyed on current
                // dims must miss rather than match a mode that no longer exists.
                dev.current_mode = None;
            }
        }
    }
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
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == OutputManagement::interface().name {
                    let v = version.min(MGMT_MAX);
                    state.management =
                        Some(registry.bind::<OutputManagement, _, _>(name, v, qh, ()));
                    state.mgmt_name_version = Some((name, v));
                } else if interface == OutputDevice::interface().name {
                    let v = version.min(DEVICE_MAX);
                    // Global `name` rides in UserData so the event handler can record it
                    // (newest-wins tie-break during a supersede).
                    let dev = registry.bind::<OutputDevice, _, _>(name, v, qh, name);
                    let id = dev.id();
                    state.device_entry(id).proxy = Some(dev);
                } else if interface == DeviceRegistry::interface().name {
                    // KWin ≥ 6.7 advertises this registry instead of one `kde_output_device_v2`
                    // global per output. Without the bind the device list is empty and every
                    // topology apply silently falls back to `kscreen-doctor`. Per-output globals
                    // still arrive on older KWin; both models stay bound.
                    let v = version.min(DEVICE_REGISTRY_MAX);
                    state.device_registry =
                        Some(registry.bind::<DeviceRegistry, _, _>(name, v, qh, ()));
                }
            }
            wl_registry::Event::GlobalRemove { name } => state.forget_global(name),
            _ => {}
        }
    }
}

/// One `kde_output_device_v2` per `output` event (`new_id`, created by
/// `event_created_child!` below).
///
/// These devices have no global `name`; UserData is `0u32`, so [`DeviceState::global`]
/// is 0 for all of them. Newest-wins on a same-named supersede then lands on
/// [`DeviceState::seq`].
impl Dispatch<DeviceRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &DeviceRegistry,
        event: RegistryEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let RegistryEvent::Output { output } = event {
            let id = output.id();
            state.device_entry(id).proxy = Some(output);
        }
    }

    event_created_child!(State, DeviceRegistry, [
        REGISTRY_OUTPUT_EVENT_OPCODE => (OutputDevice, 0u32),
    ]);
}

// Management has no events.
impl Dispatch<OutputManagement, ()> for State {
    fn event(
        _: &mut Self,
        _: &OutputManagement,
        _: management::kde_output_management_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

// A client-built custom-mode list has no events; it just needs a Dispatch impl to be created.
impl Dispatch<ModeList, ()> for State {
    fn event(
        _: &mut Self,
        _: &ModeList,
        _: management::kde_mode_list_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// The device's UserData is its global `name` number.
impl Dispatch<OutputDevice, u32> for State {
    fn event(
        state: &mut Self,
        device: &OutputDevice,
        event: DeviceEvent,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Handle `removed` (device ≥ v21) before `device_entry` re-creates the row. The XML
        // requires `release`; wayland-rs sends no destructor on drop, so the server object
        // would otherwise leak. Dropping the row also stops a hot-unplug being restored.
        if matches!(event, DeviceEvent::Removed) {
            if let Some(dead) = state.devices.remove(&device.id()) {
                for (mid, _) in &dead.modes {
                    state.mode_dims.remove(mid);
                }
            }
            if device.version() >= 21 {
                device.release();
            }
            return;
        }
        let entry = state.device_entry(device.id());
        entry.global = *global;
        if entry.proxy.is_none() {
            entry.proxy = Some(device.clone());
        }
        match event {
            DeviceEvent::Name { name } => entry.name = Some(name),
            DeviceEvent::Uuid { uuid } => entry.uuid = Some(uuid),
            DeviceEvent::Geometry {
                x, y, make, model, ..
            } => {
                entry.position = (x, y);
                entry.make = Some(make);
                entry.model = Some(model);
            }
            // `fixed` decodes to f64 in wayland-rs.
            DeviceEvent::Scale { factor } => entry.scale = Some(factor),
            DeviceEvent::Enabled { enabled } => entry.enabled = enabled != 0,
            DeviceEvent::Priority { priority } => entry.priority = Some(priority),
            DeviceEvent::ReplicationSource { source } => entry.replication_source = Some(source),
            DeviceEvent::CurrentMode { mode } => entry.current_mode = Some(mode.id()),
            DeviceEvent::Mode { mode } => entry.modes.push((mode.id(), mode)),
            DeviceEvent::Done => entry.seen_done = true,
            _ => {}
        }
    }

    // `mode` hands us a server-created `kde_output_device_mode_v2`. The opcode is a
    // bare literal (the macro rejects a `const` in some wayland-client versions);
    // `mode_event_opcode_is_two` pins it to `DEVICE_MODE_EVENT_OPCODE`.
    event_created_child!(State, OutputDevice, [
        2 => (DeviceMode, ()),
    ]);
}

impl Dispatch<DeviceMode, ()> for State {
    fn event(
        state: &mut Self,
        mode: &DeviceMode,
        event: ModeEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `removed` first, not through the entry below: re-inserting a destroyed mode
        // is the stale row a later `config.mode(...)` would send back (see [`State::forget_mode`]).
        if matches!(event, ModeEvent::Removed) {
            state.forget_mode(&mode.id());
            return;
        }
        let entry = state.mode_dims.entry(mode.id()).or_insert((0, 0, 0));
        match event {
            ModeEvent::Size { width, height } => {
                entry.0 = width.max(0) as u32;
                entry.1 = height.max(0) as u32;
            }
            ModeEvent::Refresh { refresh } => entry.2 = refresh.max(0) as u32,
            _ => {}
        }
    }
}

impl Dispatch<OutputConfig, ()> for State {
    fn event(
        state: &mut Self,
        _: &OutputConfig,
        event: ConfigEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // No catch-all: this protocol has exactly these three events. A re-vendored XML
            // that adds one should fail the build here rather than drop it.
            ConfigEvent::Applied => state.applied = Some(true),
            ConfigEvent::Failed => state.applied = Some(false),
            ConfigEvent::FailureReason { reason } => state.failure_reason = Some(reason),
        }
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

pub(crate) struct Session {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    next_sync: u32,
}

impl Session {
    /// [`Session::connect`] for a caller outside the seam ([`watch`], the disabled-output
    /// repair), which logs its own decline. No Wayland connection is `debug`, as in
    /// [`resolve`](super::outputs::resolve).
    fn open(op: &'static str) -> Result<Session, Declined> {
        let opened = Session::connect();
        match &opened {
            Ok(_) => {}
            Err(reason @ Declined::NoWayland(_)) => {
                tracing::debug!(op, %reason, "KWin output-management connection did not open")
            }
            Err(reason) => {
                tracing::warn!(op, %reason, "KWin output-management connection did not open")
            }
        }
        opened
    }

    /// Connect, bind management + every device, read each output, bounded by [`OP_BUDGET`].
    /// KWin ≥ 6.7 advertises the device registry and no per-output `kde_output_device_v2`
    /// globals; that is still KWin.
    fn connect() -> Result<Session, Declined> {
        let conn =
            crate::session::wayland_connect().map_err(|e| Declined::NoWayland(e.to_string()))?;
        let queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut s = Session {
            conn,
            queue,
            state: State::default(),
            next_sync: 0,
        };
        let deadline = Instant::now() + OP_BUDGET;
        if !s.sync_barrier(deadline) {
            return Err(Declined::Barrier("registry roundtrip"));
        }
        if s.state.management.is_none() {
            return Err(Declined::Unbound(
                "kde_output_management_v2 is not advertised to this client",
            ));
        }
        // Registry-model devices only arrive as `output` events on the first device
        // barrier, so their property bursts are one round further out than per-output
        // globals. Skip when every device already has `done`.
        let announced = s.sync_barrier(deadline)
            && (s.state.device_registry.is_none()
                || s.state.devices.values().all(|d| d.seen_done)
                || s.sync_barrier(deadline));
        if !announced {
            return Err(Declined::Barrier("output announcements"));
        }
        Ok(s)
    }

    /// Bound management version; 0 never reaches a request (connect requires the global).
    fn version(&self) -> u32 {
        self.state
            .mgmt_name_version
            .map(|(_, v)| v)
            .unwrap_or_default()
    }

    /// Whether this connection took in what KWin announced since it last looked: new
    /// outputs on one round, their property bursts on the next.
    fn caught_up(&mut self, deadline: Instant) -> bool {
        self.sync_barrier(deadline)
            && (self.state.devices.values().all(|d| d.seen_done) || self.sync_barrier(deadline))
    }

    fn sync_barrier(&mut self, deadline: Instant) -> bool {
        self.next_sync += 1;
        let serial = self.next_sync;
        let barrier = crate::wl_pump::sync_barrier(
            &self.conn,
            &mut self.queue,
            &mut self.state,
            serial,
            deadline,
            None,
        );
        matches!(barrier, Ok(crate::wl_pump::Pumped::Done))
    }

    /// Dispatch until `done` holds or `deadline` passes ([`crate::wl_pump`]).
    fn pump_until(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> bool {
        let pumped = crate::wl_pump::pump_until(
            &self.conn,
            &mut self.queue,
            &mut self.state,
            Some(deadline),
            None,
            done,
        );
        matches!(pumped, Ok(crate::wl_pump::Pumped::Done))
    }

    fn new_config(&self) -> OutputConfig {
        let qh = self.queue.handle();
        self.state
            .management
            .as_ref()
            .unwrap()
            .create_configuration(&qh, ())
    }

    fn new_mode_list(&self) -> ModeList {
        let qh = self.queue.handle();
        self.state
            .management
            .as_ref()
            .unwrap()
            .create_mode_list(&qh, ())
    }

    fn apply(&mut self, config: &OutputConfig, deadline: Instant) -> bool {
        self.state.applied = None;
        config.apply();
        let ok = self.pump_until(deadline, |st| st.applied.is_some());
        if !ok {
            return false;
        }
        matches!(self.state.applied, Some(true))
    }

    fn current_dims(&self, dev: &DeviceState) -> Option<(u32, u32, u32)> {
        let id = dev.current_mode.as_ref()?;
        self.state.mode_dims.get(id).copied()
    }

    /// The output's rect in the logical space `position` and popup placement use:
    /// mode pixels over applied scale. `None` for an output with no current mode.
    fn logical_rect(&self, dev: &DeviceState) -> Option<crate::layout::Rect> {
        let (w, h, _) = self.current_dims(dev)?;
        let scale = dev.scale.filter(|s| *s > 0.0).unwrap_or(1.0);
        let logical = |px: u32| (f64::from(px) / scale).ceil() as i32;
        Some(crate::layout::Rect {
            x: dev.position.0,
            y: dev.position.1,
            w: logical(w),
            h: logical(h),
        })
    }

    /// Our just-created virtual output: [`newest_named`] `our_name` among the outputs at
    /// `our_w`×`our_h` now. During a supersede the predecessor keeps the per-slot name and
    /// KWin restores its stored mode onto the new one, so both can match; newest is creation
    /// order only on a [`watch`] connection. Slot 1 never matches slot 12's output.
    fn resolve_ours(&self, our_name: &str, our_w: u32, our_h: u32) -> Option<DeviceState> {
        let at_size = self
            .state
            .devices
            .values()
            .filter(|d| self.current_dims(d).map(|(w, h, _)| (w, h)) == Some((our_w, our_h)));
        newest_named(at_size, our_name).cloned()
    }

    /// Every addressable output as an [`outputs::Row`], beside the proxy that stages it.
    fn rows(&self, ours: &ObjectId) -> (Vec<outputs::Row>, Vec<OutputDevice>) {
        self.state
            .devices
            .values()
            .filter_map(|d| {
                let proxy = d.proxy.clone()?;
                let row = outputs::Row {
                    name: d.name.clone(),
                    enabled: d.enabled,
                    priority: d.priority,
                    rect: self.logical_rect(d),
                    mode: self.current_dims(d).map(mode_spec).unwrap_or_default(),
                    ours: &proxy.id() == ours,
                };
                Some((row, proxy))
            })
            .unzip()
    }

    /// Stage `plan` on one atomic `config`: enable ours, clear its mirror source, place it,
    /// take primary, disable the others. Primary goes through `set_priority` (management ≥ 3):
    /// KWin's `set_primary_output` handler is `// intentionally ignored`, though compositors
    /// before `set_priority` honoured it. Every other output gets a priority from 2: KWin
    /// 6.0–6.5 fails an order that misses any output, dark included, and drops the dark ones.
    fn stage(
        &self,
        config: &OutputConfig,
        ours: &OutputDevice,
        proxies: &[OutputDevice],
        plan: &outputs::Plan,
    ) {
        config.enable(ours, 1);
        // Ours is its own desktop, not a replica. A stored `replicationSource` for our
        // stable name is reapplied on every matching monitor set and survives enable /
        // priority (`applyMirroring`). Unconditional: KWin may restore the stored setup
        // between enumerate and apply; clearing an already-empty source is a no-op.
        if self.version() >= REPLICATION_SOURCE_SINCE {
            config.set_replication_source(ours, NO_REPLICATION_SOURCE.to_string());
        }
        if let Some(at) = plan.origin {
            config.position(ours, at.x, at.y);
        }
        if plan.take_primary {
            config.set_primary_output(ours);
            if self.version() >= 3 {
                config.set_priority(ours, 1);
                for (i, &row) in plan.others.iter().enumerate() {
                    config.set_priority(&proxies[row], 2 + i as u32);
                }
            }
        }
        for &row in &plan.disable {
            config.enable(&proxies[row], 0);
        }
    }

    /// Whether ours holds the lowest priority among enabled outputs once KWin's post-apply
    /// `priority` events are in: `set_primary_output` can ack while leaving us at 2. `None`
    /// on devices before v18, which carry no priority.
    fn primary_verified(&mut self, ours: &ObjectId) -> Option<bool> {
        self.sync_barrier(Instant::now() + Duration::from_millis(500));
        let ours_now = self.state.devices.get(ours).and_then(|d| d.priority);
        let min_enabled = self
            .state
            .devices
            .values()
            .filter(|d| d.enabled)
            .filter_map(|d| d.priority)
            .min();
        Some(ours_now? <= min_enabled?)
    }
}

/// `"WxH@Hz"` capture of a device's current mode, Hz rounded — the shape teardown
/// parses so a panel returns at its real refresh.
fn mode_spec(dims: (u32, u32, u32)) -> String {
    let hz = ((dims.2 as f64) / 1000.0).round() as u32;
    format!("{}x{}@{}", dims.0, dims.1, hz)
}

/// Every head KWin reports, for [`crate::monitors::list`].
///
/// Enumerate-only: no configuration is built. A device that never completed its
/// `done` burst is skipped — geometry would be a guess, and callers key on it.
pub(crate) fn list_monitors() -> Result<Vec<crate::monitors::PhysicalMonitor>, Declined> {
    let session = Session::connect()?;
    let mut out: Vec<_> = session
        .state
        .devices
        .values()
        .filter(|d| d.seen_done)
        .filter_map(|d| {
            let connector = d.name.clone()?;
            let dims = session.current_dims(d);
            Some(crate::monitors::PhysicalMonitor {
                managed: connector.starts_with(MANAGED_PREFIX),
                description: crate::monitors::describe(
                    d.make.as_deref().unwrap_or(""),
                    d.model.as_deref().unwrap_or(""),
                    &connector,
                ),
                // A disabled output has no current mode — report 0s rather than inventing one.
                width: dims.map(|d| d.0).unwrap_or(0),
                height: dims.map(|d| d.1).unwrap_or(0),
                refresh_mhz: dims.map(|d| d.2).unwrap_or(0),
                x: d.position.0,
                y: d.position.1,
                scale: d.scale.filter(|s| *s > 0.0).unwrap_or(1.0),
                primary: d.priority == Some(1),
                enabled: d.enabled,
                connector,
            })
        })
        .collect();
    // Bind order is not stable across runs; sort by desktop position so a picker
    // reads left-to-right.
    out.sort_by_key(|m| (m.x, m.y, m.connector.clone()));
    Ok(out)
}

/// The connection for one create, opened before the output exists. KWin announces
/// an output made later after every one this connection already holds, so
/// newest-wins resolves ours while a same-named predecessor shares its name, UUID
/// and restored mode. A later connection cannot: KWin ≥ 6.7 announces existing
/// outputs in hash order.
pub(crate) fn watch() -> Option<Session> {
    Session::open("watch").ok()
}

/// `sess` once it caught up and [`gate`] lets `op` run on it.
fn ready(
    mut sess: Option<&mut Session>,
    op: Op,
    deadline: Instant,
) -> Result<&mut Session, Declined> {
    let seen = sess
        .as_deref_mut()
        .map(|s| (s.version(), s.caught_up(deadline)));
    gate(op, seen)?;
    sess.ok_or(Declined::Unbound("no output-management connection"))
}

/// Place the streamed output (named exactly `our_prefix`, at `dims` now) in the desktop by
/// [`outputs::plan`], `journal` getting each `(name, "WxH@Hz")` an `Exclusive` disables
/// before the apply sends it. Runs on `sess` from [`watch`]; a decline journals nothing.
/// `Ok` carries our output's UUID, also for an unconfirmed apply: kscreen-doctor must not
/// apply a second time.
pub(crate) fn apply_topology(
    sess: Option<&mut Session>,
    our_prefix: &str,
    dims: (u32, u32),
    kind: TopologyKind,
    keep: &[String],
    mut journal: impl FnMut(&[(String, String)]),
) -> Result<Option<String>, Declined> {
    let deadline = Instant::now() + OP_BUDGET;
    let sess = ready(sess, Op::Topology, deadline)?;
    let unresolved = || Declined::Unresolved(format!("{our_prefix} at {}x{}", dims.0, dims.1));
    let ours = sess
        .resolve_ours(our_prefix, dims.0, dims.1)
        .ok_or_else(unresolved)?;
    let our_proxy = ours.proxy.clone().ok_or_else(unresolved)?;
    if is_mirroring(ours.replication_source.as_deref()) {
        // Stored `kwinoutputconfig.json` `replicationSource` for this monitor set;
        // it reproduces every session until this config clears it.
        tracing::warn!(
            source_uuid = ?ours.replication_source,
            our_prefix,
            "KWin had our streamed output mirroring another screen (a stored kwinoutputconfig.json \
             replicationSource for this monitor set) — clearing it so the output is its own desktop"
        );
    }
    let (rows, proxies) = sess.rows(&our_proxy.id());
    let plan = outputs::plan(&rows, kind, keep);
    // Journal every output we ask to disable, whatever the ack says: re-enabling a lit head
    // is a no-op, and a disable KWin processed after the budget would otherwise stay dark.
    let disabled = plan.disabled(&rows);
    if kind == TopologyKind::Exclusive {
        journal(&disabled);
    }
    let config = sess.new_config();
    sess.stage(&config, &our_proxy, &proxies, &plan);
    let applied = sess.apply(&config, deadline);
    config.destroy();
    if !applied {
        tracing::warn!(
            reason = ?sess.state.failure_reason,
            also_disabled = ?disabled,
            "KWin output management: apply() not confirmed in budget — proceeding (restore will re-enable)"
        );
        return Ok(ours.uuid);
    }
    let verified = sess.primary_verified(&our_proxy.id());
    if !plan.take_primary || verified != Some(false) {
        tracing::info!(
            also_disabled = ?disabled,
            primary_requested = plan.take_primary,
            primary_verified = ?verified,
            "KWin output management: streamed output set as the desktop (in-process)"
        );
    } else {
        tracing::warn!(
            also_disabled = ?disabled,
            "KWin output management: apply acked but the streamed output did not become the \
             primary (priority read-back) — the desktop stays on another output"
        );
    }
    Ok(ours.uuid)
}

/// De-mirror the just-created virtual output without touching the rest of the
/// topology — the clear [`apply_topology`] folds into its own config, for an `Extend`
/// placed by the `kscreen-doctor` fallback, which clears no mirror source
/// ([`outputs::topology`]).
///
/// A stored `replicationSource` is not an arrangement; it makes our output show a
/// physical panel. Apply only when ours is actually mirroring; the ordinary session
/// pays one bounded roundtrip on `sess` ([`watch`]) and no apply.
pub(crate) fn clear_replication_source(
    sess: Option<&mut Session>,
    our_prefix: &str,
    our_w: u32,
    our_h: u32,
) {
    let deadline = Instant::now() + OP_BUDGET;
    let Ok(sess) = ready(sess, Op::Topology, deadline) else {
        return;
    };
    if sess.version() < REPLICATION_SOURCE_SINCE {
        return;
    }
    let Some(ours) = sess.resolve_ours(our_prefix, our_w, our_h) else {
        return;
    };
    if !is_mirroring(ours.replication_source.as_deref()) {
        return;
    }
    let Some(proxy) = ours.proxy.as_ref() else {
        return;
    };
    tracing::warn!(
        source_uuid = ?ours.replication_source,
        our_prefix,
        "KWin had our streamed output mirroring another screen (a stored kwinoutputconfig.json \
         replicationSource for this monitor set) — clearing it so the output is its own desktop"
    );
    let config = sess.new_config();
    config.set_replication_source(proxy, NO_REPLICATION_SOURCE.to_string());
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if !ok {
        tracing::warn!(
            reason = ?sess.state.failure_reason,
            "KWin output management: could not clear the streamed output's replication source — \
             the stream will show the mirrored screen's content"
        );
    }
}

/// Size + scale the just-created virtual output actually landed at. Read only.
///
/// [`resolve_ours`] keys on the size we asked for, so it cannot tell "KWin
/// restored a stored mode" from "the output is not there yet". KWin restores
/// per-output mode and scale from `kwinoutputconfig.json` by name, and ours is
/// stable, so a previous session's mode can overlay the one we just requested.
///
/// Resolve by exact name alone ([`newest_named`]) on `sess` from [`watch`]: a
/// supersede or a kept display leaves a predecessor with our name, and KWin gives
/// the new one its stored mode. `None` while our output is mid-announce.
///
/// Returns `(width, height, refresh_mHz, scale)`. Scale is for the log:
/// KWin's screencast streams pixel size, so a restored scale shifts logical
/// layout without changing capture.
pub(crate) fn actual_dims(
    sess: Option<&mut Session>,
    our_name: &str,
) -> Option<(u32, u32, u32, f64)> {
    let sess = ready(sess, Op::List, Instant::now() + OP_BUDGET).ok()?;
    let ours = newest_named(sess.state.devices.values(), our_name)?;
    // Mid-announce has no coherent `current_mode`; reading one anyway is a
    // 0×0 "correction" that stomps a healthy output.
    if !ours.seen_done {
        return None;
    }
    let (w, h, mhz) = sess.current_dims(ours)?;
    Some((w, h, mhz, ours.scale.filter(|s| *s > 0.0).unwrap_or(1.0)))
}

/// The newest output named exactly `name`. On a connection opened before the create
/// ([`watch`]) this is the one just created, even when a predecessor shares its name
/// and, after KWin restored the stored mode, its size.
fn newest_named<'a>(
    devices: impl IntoIterator<Item = &'a DeviceState>,
    name: &str,
) -> Option<&'a DeviceState> {
    devices
        .into_iter()
        .filter(|d| d.name.as_deref() == Some(name))
        .max_by_key(|d| (d.global, d.seq))
}

/// The output [`enable_disabled_output`] repairs: named exactly `name`, a disabled one
/// first, newest among equals. On a fresh connection a same-named predecessor can
/// announce after ours; the refused stream is the disabled one.
fn repair_candidate<'a>(
    devices: impl IntoIterator<Item = &'a DeviceState>,
    name: &str,
) -> Option<&'a DeviceState> {
    let (dark, lit): (Vec<_>, Vec<_>) = devices.into_iter().partition(|d| !d.enabled);
    newest_named(dark, name).or_else(|| newest_named(lit, name))
}

/// The output [`set_position`] moves: the one carrying `uuid` when exactly one does, else
/// [`newest_named`] `name`. A same-named predecessor shares our UUID while it lives.
fn position_target<'a>(
    devices: impl IntoIterator<Item = &'a DeviceState>,
    name: &str,
    uuid: Option<&str>,
) -> Option<&'a DeviceState> {
    let devices: Vec<&DeviceState> = devices.into_iter().collect();
    let mut carrying = devices
        .iter()
        .filter(|d| uuid.is_some() && d.uuid.as_deref() == uuid);
    match (carrying.next(), carrying.next()) {
        (Some(only), None) => Some(only),
        _ => newest_named(devices, name),
    }
}

/// Install and select a `want_w`×`want_h` custom mode at `want_mhz` on the virtual output
/// named `our_prefix`, which sits at `at_w`×`at_h` right now — its sacrificial birth size,
/// or wherever KWin's stored setup put it. Addressing it by a size it has already left
/// resolves nothing. In-process replacement for `kscreen-doctor` `addCustomMode`.
///
/// `set_custom_modes` hands KWin a one-entry list. From v22 the entry states its timing
/// ([`stated_timing`]) and runs at exactly `want_mhz`. Below, KWin generates CVT timing:
/// the width may align down ([`CVT_H_GRANULARITY`]) and the rate lands near the ask.
/// Selecting the mode changes size and renegotiates the screencast (`kwin::create`).
/// Runs on `sess` from [`watch`]. Returns the active mode read back, in millihertz.
/// `set_custom_modes` replaces the custom list; on a bind below v18 it is a protocol
/// error, so [`gate`] declines first.
pub(crate) fn set_custom_mode(
    sess: Option<&mut Session>,
    our_prefix: &str,
    at_w: u32,
    at_h: u32,
    want_w: u32,
    want_h: u32,
    want_mhz: u32,
) -> Result<(u32, u32, u32), Declined> {
    let deadline = Instant::now() + OP_BUDGET;
    let sess = ready(sess, Op::CustomMode, deadline)?;
    let unresolved = || Declined::Unresolved(format!("{our_prefix} at {at_w}x{at_h}"));
    let our_proxy = sess
        .resolve_ours(our_prefix, at_w, at_h)
        .and_then(|d| d.proxy.clone())
        .ok_or_else(unresolved)?;
    let our_key = our_proxy.id();

    let stated =
        stated_timing(want_w, want_h, want_mhz).filter(|_| sess.version() >= STATED_TIMING_SINCE);
    // Exact height, width at/just-below the request (CVT alignment). A generated mode is
    // within 1 Hz, which excludes the sacrificial 60 Hz birth mode. A stated one is exact,
    // which excludes a birth mode 0.06 Hz away.
    let slack = if stated.is_some() { 0 } else { 1000 };
    let mode_matches = move |st: &State, mid: &ObjectId| -> bool {
        st.mode_dims.get(mid).is_some_and(|&(w, h, mhz)| {
            h == want_h
                && w <= want_w
                && want_w - w < CVT_H_GRANULARITY
                && mhz.abs_diff(want_mhz) <= slack
        })
    };

    let mode_list = sess.new_mode_list();
    match stated {
        Some((clock, htotal, vtotal)) => mode_list.add_cvt(
            clock,
            want_w,
            want_w + 48,
            want_w + 80,
            htotal,
            0,
            want_h,
            want_h + 3,
            want_h + 8,
            vtotal,
            1,
            STATED_TIMING_FLAGS,
        ),
        // Full blanking, matching kscreen-doctor's `.full`.
        None => {
            mode_list.set_resolution(want_w, want_h);
            mode_list.set_refresh_rate(want_mhz);
            mode_list.set_reduced_blanking(0);
            mode_list.add_mode();
        }
    }
    let config = sess.new_config();
    config.set_custom_modes(&our_proxy, &mode_list);
    let installed = sess.apply(&config, deadline);
    config.destroy();
    mode_list.destroy();
    if !installed {
        return Err(Declined::Unconfirmed(sess.state.failure_reason.clone()));
    }

    let found = |st: &State| -> bool {
        st.devices
            .get(&our_key)
            .is_some_and(|d| d.modes.iter().any(|(mid, _)| mode_matches(st, mid)))
    };
    if !sess.pump_until(deadline, found) {
        return Err(Declined::Unresolved(format!(
            "the {want_w}x{want_h} custom mode at {want_mhz} mHz"
        )));
    }

    // Newest match wins: `modes` is announce order and the mode we just generated
    // is last. An earlier session's identical custom mode may still be listed —
    // KWin only sends `removed` when it processes our `set_custom_modes`, and that
    // event may not have been dispatched yet.
    let mode_proxy = sess
        .state
        .devices
        .get(&our_key)
        .and_then(|dev| {
            dev.modes
                .iter()
                .rev()
                .find(|(mid, _)| mode_matches(&sess.state, mid))
                .map(|(_, p)| p.clone())
        })
        .ok_or_else(unresolved)?;
    let config = sess.new_config();
    config.mode(&our_proxy, &mode_proxy);
    let selected = sess.apply(&config, deadline);
    config.destroy();
    if !selected {
        return Err(Declined::Unconfirmed(sess.state.failure_reason.clone()));
    }

    // Size that really landed paces the encoder.
    let want_dims = sess
        .state
        .mode_dims
        .get(&mode_proxy.id())
        .map(|&(w, h, _)| (w, h));
    let landed = |st: &State| -> bool {
        st.devices
            .get(&our_key)
            .and_then(|d| d.current_mode.as_ref())
            .and_then(|mid| st.mode_dims.get(mid))
            .map(|&(w, h, _)| (w, h))
            == want_dims
    };
    sess.pump_until(deadline, landed);
    let (cw, ch, cmhz) = sess
        .state
        .devices
        .get(&our_key)
        .and_then(|dev| sess.current_dims(dev))
        .ok_or_else(unresolved)?;
    tracing::info!(
        want_w,
        want_h,
        want_mhz,
        active_w = cw,
        active_h = ch,
        active_mhz = cmhz,
        stated = stated.is_some(),
        "KWin output management: custom mode installed + selected (in-process)"
    );
    Ok((cw, ch, cmhz.max(1000)))
}

/// Put our output, at `dims`, on exactly `want_mhz`, and answer the rate it runs. Only a
/// KWin that takes stated timing can move it: any other keeps the rate it has. `None`
/// when the output cannot be read.
pub(crate) fn settle_rate(
    mut sess: Option<&mut Session>,
    our_prefix: &str,
    dims: (u32, u32),
    want_mhz: u32,
) -> Option<u32> {
    let (_, _, at, _) = actual_dims(sess.as_deref_mut(), our_prefix)?;
    let states = sess
        .as_deref()
        .is_some_and(|s| s.version() >= STATED_TIMING_SINCE);
    if at == want_mhz || !states {
        return Some(at);
    }
    let (w, h) = dims;
    match set_custom_mode(sess, our_prefix, w, h, w, h, want_mhz) {
        Ok((_, _, mhz)) => Some(mhz),
        Err(why) => {
            tracing::warn!(
                our_prefix, want_mhz, at_mhz = at, %why,
                "KWin kept the output's rate — streaming at the one it runs"
            );
            Some(at)
        }
    }
}

/// Re-enable outputs by name at their captured `WxH@Hz` (teardown). `Ok` only if
/// every requested output was staged and the config applied.
///
/// Names were captured on a different connection during [`apply_topology`];
/// this restore opens a fresh session later, so a name may no longer resolve.
/// An empty `kde_output_configuration_v2` still gets `applied`, so returning
/// the apply verdict alone would report success for a total no-op and leave
/// a physical dark.
pub(crate) fn reenable_outputs(outputs: &[(String, String)]) -> Result<(), Declined> {
    if outputs.is_empty() {
        return Ok(());
    }
    let mut sess = Session::connect()?;
    let deadline = Instant::now() + OP_BUDGET;
    let config = sess.new_config();
    let mut matched = 0usize;
    for (name, spec) in outputs {
        // Physical names are stable across a session. Both misses leave `matched`
        // un-incremented: [`State::device_entry`] can stamp a name before the
        // announce that records the proxy, and a name with no proxy is not addressable.
        let Some(dev) = sess
            .state
            .devices
            .values()
            .find(|d| d.name.as_deref() == Some(name.as_str()))
            .cloned()
        else {
            continue;
        };
        let Some(proxy) = dev.proxy.as_ref() else {
            continue;
        };
        matched += 1;
        // Enable first — a bare enable always succeeds, so a physical is never left dark.
        config.enable(proxy, 1);
        // Re-assert the captured mode so a 120 Hz panel does not return at KWin's ~60 Hz default.
        if let Some(mode) = find_mode(&sess, &dev, spec) {
            config.mode(proxy, &mode);
        }
    }
    let missing = || {
        Declined::Unresolved(format!(
            "{} of the heads {outputs:?}",
            outputs.len() - matched
        ))
    };
    if matched == 0 {
        // Applying would ack an empty config and read as success. Hand the whole
        // restore to kscreen-doctor, which addresses outputs by name with no live proxy.
        config.destroy();
        return Err(missing());
    }
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if !ok {
        return Err(Declined::Unconfirmed(sess.state.failure_reason.clone()));
    }
    if matched < outputs.len() {
        return Err(missing());
    }
    tracing::info!(reenabled = ?outputs, "KWin output management: restored outputs (in-process)");
    Ok(())
}

/// Enable a virtual output KWin created but left disabled, named exactly `ours`
/// (`Virtual-<name>`, [`repair_candidate`]). Returns the head's name when one matched,
/// was disabled, and the enable applied.
///
/// From KWin ≥ 6.6, `streamVirtualOutput` refuses an output the workspace
/// does not manage (`isEnabled() && !isNonDesktop()`). The host uses a stable
/// per-client name so KWin persists scale and mode; a stored `enabled: false`
/// is therefore reapplied to every future session.
///
/// `sendFailed` does not emit `finished`, and `removeVirtualOutput` is wired
/// to `finished`, so the disabled output stays alive while the caller holds
/// the failed stream — the window this runs in. `handleOutputAdded` offers
/// every backend output to the device registry, so a disabled head has no
/// `wl_output` but is addressable here. Enable is a user-applied config KWin
/// persists; the caller must retry — the failed request cannot be salvaged.
pub(crate) fn enable_disabled_output(ours: &str) -> Option<String> {
    let mut sess = Session::open("enable_disabled").ok()?;
    let deadline = Instant::now() + OP_BUDGET;
    let addressable = sess.state.devices.values().filter(|d| d.proxy.is_some());
    let dev = repair_candidate(addressable, ours).cloned()?;
    let name = dev.name.clone()?;
    if dev.enabled {
        // Not the shape we repair. A no-op config would `applied` and read as a fix.
        tracing::debug!(
            %name,
            "KWin output management: our virtual output is already enabled — nothing to repair"
        );
        return None;
    }
    let proxy = dev.proxy.as_ref()?;
    let config = sess.new_config();
    config.enable(proxy, 1);
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if !ok {
        tracing::warn!(
            %name,
            reason = ?sess.state.failure_reason,
            "KWin output management: virtual output not enabled (KWin created it disabled)"
        );
        return None;
    }
    tracing::info!(
        %name,
        "KWin output management: KWin created our virtual output disabled and refused to stream \
         it; enabled it — KWin persists that, so the retry's request comes back enabled"
    );
    Some(name)
}

/// Position our output `ours` at `(x, y)` on `sess`, the create's [`watch`]
/// ([`position_target`]). A fresh connection could not tell ours from a predecessor:
/// KWin ≥ 6.7 announces existing outputs in hash order.
pub(crate) fn set_position(
    sess: Option<&mut Session>,
    ours: &str,
    uuid: Option<&str>,
    x: i32,
    y: i32,
) -> Result<(), Declined> {
    let deadline = Instant::now() + OP_BUDGET;
    let sess = ready(sess, Op::Position, deadline)?;
    let proxy = position_target(sess.state.devices.values(), ours, uuid)
        .and_then(|d| d.proxy.clone())
        .ok_or_else(|| Declined::Unresolved(ours.to_string()))?;
    let config = sess.new_config();
    config.position(&proxy, x, y);
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if !ok {
        return Err(Declined::Unconfirmed(sess.state.failure_reason.clone()));
    }
    tracing::info!(
        ours,
        x,
        y,
        "KWin output management: placed output (in-process)"
    );
    Ok(())
}

/// Put our output `ours` at `scale` on `sess`, the create's [`watch`]. KWin files the scale
/// under the output's name and brings it back on every later create.
pub(crate) fn set_scale(
    sess: Option<&mut Session>,
    ours: &str,
    scale: f64,
) -> Result<(), Declined> {
    let deadline = Instant::now() + OP_BUDGET;
    let sess = ready(sess, Op::Scale, deadline)?;
    let proxy = newest_named(sess.state.devices.values(), ours)
        .and_then(|d| d.proxy.clone())
        .ok_or_else(|| Declined::Unresolved(ours.to_string()))?;
    let config = sess.new_config();
    config.scale(&proxy, scale);
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if !ok {
        return Err(Declined::Unconfirmed(sess.state.failure_reason.clone()));
    }
    Ok(())
}

/// Advertised mode proxy matching a captured `"WxH@Hz"` (Hz rounded). `None` if
/// the spec is empty or no mode matches — caller then enables without a mode.
fn find_mode(sess: &Session, dev: &DeviceState, spec: &str) -> Option<DeviceMode> {
    if spec.is_empty() {
        return None;
    }
    let (wh, hz) = spec.split_once('@')?;
    let (w, h) = wh.split_once('x')?;
    let (w, h, hz): (u32, u32, u32) = (w.parse().ok()?, h.parse().ok()?, hz.parse().ok()?);
    dev.modes.iter().find_map(|(id, proxy)| {
        let (mw, mh, mmhz) = sess.state.mode_dims.get(id).copied()?;
        let mhz = ((mmhz as f64) / 1000.0).round() as u32;
        ((mw, mh, mhz) == (w, h, hz)).then(|| proxy.clone())
    })
}

#[cfg(test)]
mod tests {
    /// KWin < 6.7 announces each output as its own global and never sends `removed`; the
    /// registry's `global_remove` is the only word that the output is gone.
    #[test]
    fn a_removed_global_takes_its_device_and_modes_with_it() {
        let mut st = super::State::default();
        let id = super::ObjectId::null();
        st.device_entry(id.clone()).global = 7;
        st.device_entry(id.clone()).current_mode = Some(id.clone());
        st.mode_dims.insert(id.clone(), (1920, 1080, 60_000));
        st.forget_global(8);
        assert_eq!(
            st.devices.len(),
            1,
            "another global's removal changes nothing"
        );
        st.forget_global(7);
        assert!(st.devices.is_empty());
        assert!(st.mode_dims.is_empty());
    }

    use super::*;

    /// KWin sends `replication_source` as `""` for a non-mirroring output. Event
    /// presence is not mirroring.
    #[test]
    fn an_empty_replication_source_is_not_mirroring() {
        assert!(!is_mirroring(None));
        assert!(!is_mirroring(Some("")));
    }

    /// A source UUID means the output shows another panel's viewport.
    #[test]
    fn a_uuid_replication_source_is_mirroring() {
        assert!(is_mirroring(Some("f7a3c1e2-0b44-4c19-9a1d-6f2b8e0c5d31")));
    }

    #[test]
    fn the_clear_value_is_the_empty_source() {
        assert!(!is_mirroring(Some(NO_REPLICATION_SOURCE)));
    }

    /// Request/event pair is `since 13`; wayland-rs does not range-check, so a
    /// lower bind must never reach `set_replication_source` (fatal protocol error).
    #[test]
    fn replication_source_version_gate_matches_the_protocol() {
        assert_eq!(REPLICATION_SOURCE_SINCE, 13);
        const { assert!(MGMT_MAX >= REPLICATION_SOURCE_SINCE) };
    }

    /// Every rate a client asks lands to the millihertz, inside the modeline's 16-bit totals.
    #[test]
    fn stated_timing_reads_back_as_the_rate_asked() {
        const { assert!(MGMT_MAX >= STATED_TIMING_SINCE) };
        let sizes = [
            (640, 360),
            (1280, 720),
            (1920, 1080),
            (2560, 1600),
            (7680, 4320),
        ];
        let rates = [
            23_976, 29_970, 47_952, 59_940, 59_800, 60_000, 119_880, 120_000, 143_856, 239_760,
        ];
        for (w, h) in sizes {
            for mhz in rates {
                let (clock, htotal, vtotal) = stated_timing(w, h, mhz).expect("timing");
                assert!(htotal > w && vtotal > h, "{w}x{h}@{mhz}");
                assert!(htotal <= 0xffff && vtotal <= 0xffff, "{w}x{h}@{mhz}");
                let got = modeline_mhz(clock.into(), htotal.into(), vtotal.into());
                assert_eq!(got, u64::from(mhz), "{w}x{h}");
            }
        }
    }

    /// `PF_KWIN_OUTPUT=<name> PF_KWIN_MHZ=<rate>`: put that output of the live KWin, by
    /// default the first it lists, on the rate and print what it reads back.
    #[test]
    #[ignore = "needs a live KWin; run with --ignored"]
    fn live_kwin_runs_a_stated_rate() {
        let first = || Some(list_monitors().ok()?.first()?.connector.clone());
        let name = std::env::var("PF_KWIN_OUTPUT")
            .ok()
            .or_else(first)
            .expect("an output");
        let want: u32 = std::env::var("PF_KWIN_MHZ")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(59_940);
        let mut sess = watch();
        let bound = sess.as_ref().map(Session::version);
        let (w, h, at, _) = actual_dims(sess.as_mut(), &name).expect("output listed");
        let ran = settle_rate(sess.as_mut(), &name, (w, h), want);
        println!("bound={bound:?} {name} {w}x{h} at={at} want={want} ran={ran:?}");
        assert_eq!(ran, Some(want));
    }

    #[test]
    fn mode_spec_rounds_millihertz() {
        assert_eq!(mode_spec((2560, 1440, 59940)), "2560x1440@60");
        assert_eq!(mode_spec((1920, 1080, 60000)), "1920x1080@60");
        assert_eq!(mode_spec((3840, 2160, 119880)), "3840x2160@120");
    }

    /// Vendored XML must keep `mode` at the opcode `event_created_child!` hardcodes —
    /// a reorder would bind the child to the wrong event and desync mode sizes.
    #[test]
    fn mode_event_opcode_is_two() {
        assert_eq!(DEVICE_MODE_EVENT_OPCODE, 2);
    }

    /// Same hazard for the registry's `output` event (`finished` is 0, `output` is 1).
    #[test]
    fn registry_output_event_opcode_is_one() {
        assert_eq!(REGISTRY_OUTPUT_EVENT_OPCODE, 1);
    }

    fn device(name: &str, global: u32, seq: u32) -> DeviceState {
        DeviceState {
            name: Some(name.to_string()),
            global,
            seq,
            ..Default::default()
        }
    }

    /// A supersede leaves two outputs with our name. The new one is ours on both the
    /// per-global model and the ≥ 6.7 registry model (`global` 0), and a longer slot
    /// name that merely starts with ours is someone else's.
    #[test]
    fn a_same_named_predecessor_resolves_to_the_new_output() {
        let classic = [
            device("Virtual-punktfunk-1", 41, 1),
            device("Virtual-punktfunk-12", 57, 2),
            device("Virtual-punktfunk-1", 52, 3),
        ];
        let ours = newest_named(&classic, "Virtual-punktfunk-1").expect("resolved");
        assert_eq!(ours.global, 52);

        let registry = [
            device("Virtual-punktfunk-1", 0, 4),
            device("Virtual-punktfunk-1", 0, 2),
        ];
        let ours = newest_named(&registry, "Virtual-punktfunk-1").expect("resolved");
        assert_eq!(ours.seq, 4);

        assert!(newest_named(&classic, "Virtual-punktfunk").is_none());
    }

    fn with_uuid(mut d: DeviceState, uuid: &str) -> DeviceState {
        d.uuid = Some(uuid.to_string());
        d
    }

    /// Mid-supersede the predecessor shares our name and UUID: the newest named moves. A
    /// UUID one output alone carries addresses that one.
    #[test]
    fn a_shared_uuid_positions_the_newest_named_output() {
        let shared = [
            with_uuid(device("Virtual-punktfunk-1", 41, 1), "u"),
            with_uuid(device("Virtual-punktfunk-1", 52, 2), "u"),
            with_uuid(device("Virtual-punktfunk-12", 57, 3), "w"),
        ];
        let at = |uuid| position_target(&shared, "Virtual-punktfunk-1", uuid).map(|d| d.global);
        assert_eq!(at(Some("u")), Some(52));
        assert_eq!(at(None), Some(52));
        assert_eq!(at(Some("gone")), Some(52));

        let unique = [
            with_uuid(device("Virtual-punktfunk-1", 0, 1), "u"),
            with_uuid(device("Virtual-punktfunk-1", 0, 2), "v"),
        ];
        let ours = position_target(&unique, "Virtual-punktfunk-1", Some("u")).expect("resolved");
        assert_eq!(ours.seq, 1);
    }

    fn disabled(mut d: DeviceState) -> DeviceState {
        d.enabled = false;
        d
    }

    fn enabled(mut d: DeviceState) -> DeviceState {
        d.enabled = true;
        d
    }

    /// Slot 1's repair never touches slot 12's output, and the refused output is the
    /// disabled one even when a lit predecessor with our name announced later.
    #[test]
    fn the_repair_takes_our_exact_name_and_a_disabled_output_first() {
        let heads = [
            disabled(device("Virtual-punktfunk-12", 57, 3)),
            disabled(device("Virtual-punktfunk-1", 41, 1)),
            enabled(device("Virtual-punktfunk-1", 52, 2)),
        ];
        let got = repair_candidate(&heads, "Virtual-punktfunk-1").expect("resolved");
        assert_eq!((got.global, got.enabled), (41, false));
        assert!(repair_candidate(&heads, "Virtual-punktfunk").is_none());

        let lit = [enabled(device("Virtual-punktfunk-1", 0, 1))];
        let got = repair_candidate(&lit, "Virtual-punktfunk-1").expect("resolved");
        assert!(got.enabled, "nothing to repair is still our output");
    }
}
