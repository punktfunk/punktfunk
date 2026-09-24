//! In-process KDE output management (`kde_output_management_v2` + `kde_output_device_v2`).
//!
//! Topology, restore, position, and custom mode over the compositor's own Wayland.
//! [`super::kwin`] can still fall back to `kscreen-doctor`, which talks libkscreen /
//! KDED over D-Bus; a wedge there hangs the stream thread. Every wait here is
//! bounded by [`OP_BUDGET`]; a miss returns `handled = false` so the caller falls
//! back rather than hanging.
//!
//! Bind every advertised output (classic: one `kde_output_device_v2` global; KWin ≥ 6.7:
//! `kde_output_device_registry_v2` `output` events — both models are kept), read name /
//! enabled / priority / current-mode size, then `kde_output_configuration_v2.apply()` and
//! wait for `applied` / `failed`.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
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

/// Bind `min(advertised, MAX)`. Requests we issue are `since ≤ 3`; events we read are
/// `since ≤ 18` (`priority`). Binding high and calling low stays in range on any KWin
/// that advertises the globals.
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

/// Poll slice on the Wayland fd (same cadence as the keepalive loop in `kwin.rs`).
const POLL_MS: i32 = 100;

// KWin's CVT generator aligns custom-mode width down to this grain, so the generated
// mode may be a few px narrower than asked. Imported from `kwin.rs` — a second copy
// of `CVT_H_GRANULARITY` / `MANAGED_PREFIX` would silently diverge on the two values
// that decide which output is ours and which mode we asked for.
use crate::kwin::{CVT_H_GRANULARITY, MANAGED_PREFIX};

/// `kde_output_management_v2.set_replication_source` (and the device's `replication_source` event)
/// arrived in v13. wayland-rs does not range-check requests, so sending one to a lower-version bind
/// would be a protocol error that kills the connection — every call site gates on this.
const REPLICATION_SOURCE_SINCE: u32 = 13;

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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TopologyKind {
    /// Make ours the sole desktop: primary + disable every other enabled output.
    Exclusive,
    /// Make ours primary but leave the other outputs enabled.
    Primary,
    /// Add ours beside the others: no primary, nothing disabled. Still an apply —
    /// KWin restores a stored setup onto our name, and the origin has to be sane.
    Extend,
}

pub(crate) struct TopologyOutcome {
    /// UUID of our resolved virtual output — survives a mode-switch supersede (the
    /// shared name does not) — for later [`set_position`] / restore addressing.
    pub our_uuid: Option<String>,
    /// Outputs we disabled, each `(name, "WxH@Hz")`, so teardown restores the exact
    /// mode. Empty for `Primary`, or when nothing else was enabled.
    pub disabled: Vec<(String, String)>,
    /// `true` if we bound management, resolved our output, and applied (or tried to).
    /// `false` ⇒ compositor missed the budget or our output never appeared; caller
    /// falls back to `kscreen-doctor`.
    pub handled: bool,
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

    /// Drop a `kde_output_device_mode_v2` the compositor has destroyed.
    ///
    /// `removed` is not `type="destructor"`; the compositor destroys the object right
    /// after sending it, but wayland-rs keeps the proxy. Handing that id back in
    /// `kde_output_configuration_v2.mode` is a protocol error that kills the connection.
    /// `set_custom_modes` replaces the custom list, so a previous session's mode is
    /// destroyed the moment this one installs its own.
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
            wl_registry::Event::GlobalRemove { .. } => {}
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

struct Session {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    next_sync: u32,
}

/// Why [`Session::open`] declined. The variant is the log: connect vs wedge vs
/// missing global vs unfinished devices. KWin ≥ 6.7 advertises the registry and
/// no per-output `kde_output_device_v2` globals — that is not "not KDE".
enum OpenFailure {
    /// No Wayland connection (`WAYLAND_DISPLAY` unset/stale).
    Connect(String),
    /// Connected, but the registry barrier missed the budget — the wedge this module exists for.
    RegistryBarrier,
    /// Connected and answering, but `kde_output_management_v2` is not advertised
    /// (too old a KWin, or not KWin).
    NoManagementGlobal,
    /// Management is there, but the outputs' property bursts never completed in budget.
    DeviceBarrier,
}

impl std::fmt::Display for OpenFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenFailure::Connect(e) => write!(f, "no Wayland connection ({e})"),
            OpenFailure::RegistryBarrier => {
                write!(
                    f,
                    "the compositor did not answer the registry roundtrip in budget"
                )
            }
            OpenFailure::NoManagementGlobal => {
                write!(
                    f,
                    "kde_output_management_v2 is not advertised to this client"
                )
            }
            OpenFailure::DeviceBarrier => {
                write!(
                    f,
                    "the outputs never finished announcing their state in budget"
                )
            }
        }
    }
}

impl Session {
    /// [`Session::connect`] named by `op`. One log site for every caller: without it a
    /// miss just looks like `kscreen-doctor` was slow.
    fn open(op: &'static str) -> Result<Session, OpenFailure> {
        let opened = Session::connect();
        if let Err(reason) = &opened {
            tracing::warn!(
                op,
                %reason,
                "KWin in-process output management unavailable — falling back to kscreen-doctor"
            );
        }
        opened
    }

    /// Connect, bind management + every device, read each output — bounded by
    /// [`OP_BUDGET`]. Every [`OpenFailure`] sends the caller to `kscreen-doctor`.
    fn connect() -> Result<Session, OpenFailure> {
        let conn = Connection::connect_to_env().map_err(|e| OpenFailure::Connect(e.to_string()))?;
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
            return Err(OpenFailure::RegistryBarrier);
        }
        if s.state.management.is_none() {
            return Err(OpenFailure::NoManagementGlobal);
        }
        if !s.sync_barrier(deadline) {
            return Err(OpenFailure::DeviceBarrier);
        }
        // Registry-model devices only arrive as `output` events on the previous
        // barrier, so their property bursts are one round further out than per-output
        // globals. Skip when every device already has `done`.
        if s.state.device_registry.is_some()
            && s.state.devices.values().any(|d| !d.seen_done)
            && !s.sync_barrier(deadline)
        {
            return Err(OpenFailure::DeviceBarrier);
        }
        Ok(s)
    }

    fn sync_barrier(&mut self, deadline: Instant) -> bool {
        self.next_sync += 1;
        let serial = self.next_sync;
        let qh = self.queue.handle();
        let _cb = self.conn.display().sync(&qh, serial);
        self.pump_until(deadline, |st| st.sync_done >= serial)
    }

    /// Bounded event loop: flush, dispatch, poll the fd up to [`POLL_MS`].
    /// `blocking_dispatch` cannot be interrupted, so we poll instead (same as
    /// `kwin.rs::run`). Returns `true` once `done(&state)` holds.
    fn pump_until(&mut self, deadline: Instant, done: impl Fn(&State) -> bool) -> bool {
        loop {
            if done(&self.state) {
                return true;
            }
            if self.queue.dispatch_pending(&mut self.state).is_err() {
                return false;
            }
            if done(&self.state) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            if self.conn.flush().is_err() {
                return false;
            }
            let Some(guard) = self.conn.prepare_read() else {
                continue; // events already queued — loop dispatches them
            };
            let mut pfd = libc::pollfd {
                fd: self.conn.as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            let timeout = (remaining.as_millis() as i32).clamp(0, POLL_MS);
            // SAFETY: `&mut pfd` points at one live, fully-initialized `libc::pollfd` on the stack and
            // the count `1` matches that single element, so `poll` reads `fd`/`events` and writes
            // `revents` strictly within `pfd`. `pfd.fd` is the Wayland connection's fd, valid because
            // `self.conn` (and the `prepare_read` guard) outlive the call. `poll` blocks up to
            // `timeout` ms and writes only `revents`; `pfd` is a fresh local that aliases nothing.
            let r = unsafe { libc::poll(&mut pfd, 1, timeout) };
            if r > 0 && (pfd.revents & libc::POLLIN) != 0 {
                let _ = guard.read();
            } // else: timeout/signal — drop the guard, re-check the deadline
        }
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

    /// Our just-created virtual output: managed-prefix name AND current size equal to
    /// the size we created it at. During a supersede the replacement reuses the
    /// per-slot name while the predecessor is still alive; only the new one sits at
    /// this size. Newest wins remaining ties: global `name`, else [`DeviceState::seq`].
    fn resolve_ours(&self, our_prefix: &str, our_w: u32, our_h: u32) -> Option<DeviceState> {
        self.state
            .devices
            .values()
            .filter(|d| {
                d.name.as_deref().is_some_and(|n| n.starts_with(our_prefix))
                    && self.current_dims(d).map(|(w, h, _)| (w, h)) == Some((our_w, our_h))
            })
            .max_by_key(|d| (d.global, d.seq))
            .cloned()
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
pub(crate) fn list_monitors() -> anyhow::Result<Vec<crate::monitors::PhysicalMonitor>> {
    let session = Session::open("list_monitors")
        .map_err(|e| anyhow::anyhow!("KWin did not answer kde_output_management_v2: {e}"))?;
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

/// Place the streamed output (name starts with `our_prefix`, current size
/// `our_w`×`our_h`) in the desktop: primary unless `Extend`, and for `Exclusive`
/// every other enabled output goes dark. In-process; a miss leaves `handled = false` for the `kscreen-doctor` path.
pub(crate) fn apply_topology(
    our_prefix: &str,
    our_w: u32,
    our_h: u32,
    kind: TopologyKind,
) -> TopologyOutcome {
    let miss = || TopologyOutcome {
        our_uuid: None,
        disabled: Vec::new(),
        handled: false,
    };
    let Ok(mut sess) = Session::open("topology") else {
        return miss();
    };
    let deadline = Instant::now() + OP_BUDGET;

    let Some(ours) = sess.resolve_ours(our_prefix, our_w, our_h) else {
        tracing::warn!(
            our_prefix,
            our_w,
            our_h,
            "KWin output management: our virtual output hasn't appeared yet — kscreen-doctor fallback"
        );
        return miss();
    };
    let our_uuid = ours.uuid.clone();
    let our_id = ours.proxy.as_ref().map(|p| p.id());
    if is_mirroring(ours.replication_source.as_deref()) {
        // Stored `kwinoutputconfig.json` `replicationSource` for this monitor set;
        // it reproduces every session until this config clears it.
        tracing::warn!(
            source_uuid = ?ours.replication_source,
            our_prefix,
            "KWin had our streamed output MIRRORING another screen (a stored kwinoutputconfig.json \
             replicationSource for this monitor set) — clearing it so the output is its own desktop"
        );
    }

    // Do not steal primary from another managed sibling (priority 1): a second
    // exclusive session joins as a secondary. Same name is this slot's predecessor
    // mid-supersede, not a sibling — deferring would hand primary to whatever KWin
    // promotes when that output disappears (usually the physical).
    let sibling_is_primary = sess.state.devices.values().any(|d| {
        d.enabled
            && d.priority == Some(1)
            && d.proxy.as_ref().map(|p| p.id()) != our_id
            && d.name != ours.name
            && d.name
                .as_deref()
                .is_some_and(|n| n.starts_with(MANAGED_PREFIX))
    });

    // Extend adds a screen beside the others and never takes primary.
    let take_primary = kind != TopologyKind::Extend && !sibling_is_primary;

    // Monitors the operator asked to keep lit through an exclusive stream (§5.5). Matched
    // case-insensitively: a connector reaches us from the console, from a config file and
    // from KWin itself, and `DP-1` / `dp-1` are the same screen to the person who typed it.
    let keep_lit = crate::policy::prefs().get().keep_monitors;
    let kept = |name: &str| keep_lit.iter().any(|k| k.eq_ignore_ascii_case(name));

    let mut to_disable: Vec<(OutputDevice, String, String)> = Vec::new();
    if kind == TopologyKind::Exclusive {
        for d in sess.state.devices.values() {
            let is_ours = d.proxy.as_ref().map(|p| p.id()) == our_id;
            let managed = d
                .name
                .as_deref()
                .is_some_and(|n| n.starts_with(MANAGED_PREFIX));
            let stays_lit = d.name.as_deref().is_some_and(kept);
            if d.enabled && !is_ours && !managed && !stays_lit {
                if let (Some(name), Some(proxy)) = (d.name.clone(), d.proxy.clone()) {
                    let spec = sess.current_dims(d).map(mode_spec).unwrap_or_default();
                    to_disable.push((proxy, name, spec));
                }
            }
        }
    }

    // Where ours sits. KWin parks a new output right of the existing row and never
    // re-normalizes, so once the physicals go dark the sole screen keeps a non-zero
    // origin — an arrangement no display KCM can produce, and Plasma then places
    // popups off it. `origin_for` holds the rule; this only feeds it the lit rects.
    let lit: Vec<crate::layout::Rect> = sess
        .state
        .devices
        .values()
        .filter(|d| d.enabled && d.proxy.as_ref().map(|p| p.id()) != our_id)
        .filter(|d| {
            let id = d.proxy.as_ref().map(|p| p.id());
            !to_disable.iter().any(|(p, _, _)| Some(p.id()) == id)
        })
        .filter_map(|d| sess.logical_rect(d))
        .collect();
    let origin = crate::layout::origin_for(&lit, sess.logical_rect(&ours));

    // One atomic apply: enable ours, place it, take primary unless a sibling holds
    // it, disable the others. Drive primary through `set_priority` (management ≥ 3): KWin's
    // `set_primary_output` handler is `// intentionally ignored`. Still send
    // `set_primary_output` for pre-`set_priority` compositors that honored it.
    let mgmt_version = sess
        .state
        .mgmt_name_version
        .map(|(_, v)| v)
        .unwrap_or_default();
    let config = sess.new_config();
    if let Some(proxy) = ours.proxy.as_ref() {
        config.enable(proxy, 1);
        // Ours is its own desktop, not a replica. A stored `replicationSource` for our
        // stable name is reapplied on every matching monitor set and survives enable /
        // priority (`applyMirroring`). Unconditional: KWin may restore the stored setup
        // between enumerate and apply; clearing an already-empty source is a no-op.
        if mgmt_version >= REPLICATION_SOURCE_SINCE {
            config.set_replication_source(proxy, NO_REPLICATION_SOURCE.to_string());
        }
        if let Some(at) = origin {
            config.position(proxy, at.x, at.y);
        }
        if take_primary {
            config.set_primary_output(proxy);
            if mgmt_version >= 3 {
                config.set_priority(proxy, 1);
                // Remaining enabled outputs, existing order, from 2. Skip ones this apply
                // disables — a disabled output's priority is meaningless.
                let disabling: Vec<ObjectId> = to_disable.iter().map(|(p, _, _)| p.id()).collect();
                let mut others: Vec<&DeviceState> = sess
                    .state
                    .devices
                    .values()
                    .filter(|d| {
                        d.enabled
                            && d.proxy.as_ref().map(|p| p.id()) != our_id
                            && d.proxy
                                .as_ref()
                                .is_some_and(|p| !disabling.contains(&p.id()))
                    })
                    .collect();
                others.sort_by_key(|d| d.priority.unwrap_or(u32::MAX));
                for (i, d) in others.iter().enumerate() {
                    if let Some(proxy) = d.proxy.as_ref() {
                        config.set_priority(proxy, 2 + i as u32);
                    }
                }
            }
        }
    }
    for (proxy, _, _) in &to_disable {
        config.enable(proxy, 0);
    }
    let applied = sess.apply(&config, deadline);
    config.destroy();

    // Report every output we asked to disable so teardown restores them. Re-enabling
    // a still-on head is a no-op; dropping the list here would leave a physical dark
    // if KWin processed the disable but the `applied` ack missed the budget.
    let disabled: Vec<(String, String)> = to_disable
        .into_iter()
        .map(|(_, name, spec)| (name, spec))
        .collect();
    if applied {
        // Read priority back; do not echo the request. One sync drains KWin's post-apply
        // `priority` events. Primary means ours holds the lowest priority among enabled
        // outputs — `set_primary_output` can ack while leaving us at priority 2.
        let verified = {
            let vdeadline = Instant::now() + Duration::from_millis(500);
            sess.sync_barrier(vdeadline);
            let ours_now = sess
                .state
                .devices
                .values()
                .find(|d| d.proxy.as_ref().map(|p| p.id()) == our_id)
                .and_then(|d| d.priority);
            let min_enabled = sess
                .state
                .devices
                .values()
                .filter(|d| d.enabled)
                .filter_map(|d| d.priority)
                .min();
            match (ours_now, min_enabled) {
                (Some(p), Some(m)) => Some(p <= m),
                _ => None, // pre-v18 devices carry no priority event
            }
        };
        if !take_primary || verified != Some(false) {
            tracing::info!(
                also_disabled = ?disabled,
                primary_requested = take_primary,
                primary_verified = ?verified,
                "KWin output management: streamed output set as the desktop (in-process)"
            );
        } else {
            tracing::warn!(
                also_disabled = ?disabled,
                "KWin output management: apply acked but the streamed output did NOT become the \
                 primary (priority read-back) — the desktop stays on another output"
            );
        }
    } else {
        tracing::warn!(
            reason = ?sess.state.failure_reason,
            also_disabled = ?disabled,
            "KWin output management: apply() not confirmed in budget — proceeding (restore will re-enable)"
        );
    }
    // We resolved our output and drove the config; do not also run kscreen-doctor
    // (double-apply). `handled` is true even on an unconfirmed apply; a missing
    // global / unresolved output took the `handled = false` early returns above.
    TopologyOutcome {
        our_uuid,
        disabled,
        handled: true,
    }
}

/// De-mirror the just-created virtual output without touching the rest of the
/// topology — the `Extend`/`Auto` counterpart of the clear [`apply_topology`]
/// folds into its own config.
///
/// Those topologies issue no other output-management calls: rearranging the
/// user's screens is what they exist to avoid. A stored `replicationSource`
/// is not an arrangement; it makes our output show a physical panel. Apply
/// only when ours is actually mirroring; the ordinary session pays one
/// bounded enumerate and no apply.
pub(crate) fn clear_replication_source(our_prefix: &str, our_w: u32, our_h: u32) {
    let Ok(mut sess) = Session::open("clear_replication_source") else {
        return;
    };
    let deadline = Instant::now() + OP_BUDGET;
    let mgmt_version = sess
        .state
        .mgmt_name_version
        .map(|(_, v)| v)
        .unwrap_or_default();
    if mgmt_version < REPLICATION_SOURCE_SINCE {
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
        "KWin had our streamed output MIRRORING another screen (a stored kwinoutputconfig.json \
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
/// Resolve by exact name alone ([`newest_named`]): a supersede or a kept display
/// leaves a predecessor with our name, and KWin gives the new one its stored mode.
/// `None` while our output is mid-announce.
///
/// Returns `(width, height, refresh_mHz, scale)`. Scale is for the log:
/// KWin's screencast streams pixel size, so a restored scale shifts logical
/// layout without changing capture.
pub(crate) fn actual_dims(our_name: &str) -> Option<(u32, u32, u32, f64)> {
    let sess = Session::open("verify_dims").ok()?;
    let ours = newest_named(sess.state.devices.values(), our_name)?;
    // Mid-announce has no coherent `current_mode`; reading one anyway is a
    // 0×0 "correction" that stomps a healthy output.
    if !ours.seen_done {
        return None;
    }
    let (w, h, mhz) = sess.current_dims(ours)?;
    Some((w, h, mhz, ours.scale.filter(|s| *s > 0.0).unwrap_or(1.0)))
}

/// The newest output named exactly `name`. KWin announces a new output after every
/// live one, so this is the one just created even when a predecessor shares its name
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

/// Install and select a `want_w`×`want_h`@`want_hz` custom mode on the virtual output
/// named `our_prefix`, which sits at `at_w`×`at_h` right now — its sacrificial birth size,
/// or wherever KWin's stored setup put it. Addressing it by a size it has already left
/// resolves nothing. In-process replacement for `kscreen-doctor` `addCustomMode`.
///
/// `set_custom_modes` hands KWin a one-entry list; KWin generates CVT timing
/// (width may align down — [`CVT_H_GRANULARITY`]) and we then select it, which
/// changes size and renegotiates the screencast (`kwin::create`). Returns the
/// active mode read back (Hz rounded), or `None` so the caller falls back.
/// `set_custom_modes` replaces the custom list (`since 18`).
pub(crate) fn set_custom_mode(
    our_prefix: &str,
    at_w: u32,
    at_h: u32,
    want_w: u32,
    want_h: u32,
    want_hz: u32,
) -> Option<(u32, u32, u32)> {
    let mut sess = Session::open("custom_mode").ok()?;
    let deadline = Instant::now() + OP_BUDGET;

    // `set_custom_modes` is `since 18`; calling it on an older bind is a protocol
    // error. Bound version is `min(advertised, MGMT_MAX)`.
    if sess.state.mgmt_name_version.map(|(_, v)| v).unwrap_or(0) < 18 {
        return None;
    }

    let our_proxy = sess
        .resolve_ours(our_prefix, at_w, at_h)
        .and_then(|d| d.proxy.clone())?;
    let our_key = our_proxy.id();

    let want_mhz = want_hz.saturating_mul(1000);
    // Exact height, width at/just-below the request (CVT alignment), refresh within
    // 1 Hz — excludes the sacrificial 60 Hz birth mode.
    let mode_matches = move |st: &State, mid: &ObjectId| -> bool {
        st.mode_dims.get(mid).is_some_and(|&(w, h, mhz)| {
            h == want_h
                && w <= want_w
                && want_w - w < CVT_H_GRANULARITY
                && (mhz as i64 - want_mhz as i64).abs() <= 1000
        })
    };

    // Full blanking, matching kscreen-doctor's `.full`.
    let mode_list = sess.new_mode_list();
    mode_list.set_resolution(want_w, want_h);
    mode_list.set_refresh_rate(want_mhz);
    mode_list.set_reduced_blanking(0);
    mode_list.add_mode();
    let config = sess.new_config();
    config.set_custom_modes(&our_proxy, &mode_list);
    let installed = sess.apply(&config, deadline);
    config.destroy();
    mode_list.destroy();
    if !installed {
        return None;
    }

    let found = |st: &State| -> bool {
        st.devices
            .get(&our_key)
            .is_some_and(|d| d.modes.iter().any(|(mid, _)| mode_matches(st, mid)))
    };
    if !sess.pump_until(deadline, found) {
        tracing::warn!(
            want_w,
            want_h,
            want_hz,
            "KWin output management: generated custom mode never appeared — kscreen-doctor fallback"
        );
        return None;
    }

    // Newest match wins: `modes` is announce order and the mode we just generated
    // is last. An earlier session's identical custom mode may still be listed —
    // KWin only sends `removed` when it processes our `set_custom_modes`, and that
    // event may not have been dispatched yet.
    let mode_proxy = {
        let dev = sess.state.devices.get(&our_key)?;
        dev.modes
            .iter()
            .rev()
            .find(|(mid, _)| mode_matches(&sess.state, mid))
            .map(|(_, p)| p.clone())?
    };
    let config = sess.new_config();
    config.mode(&our_proxy, &mode_proxy);
    let selected = sess.apply(&config, deadline);
    config.destroy();
    if !selected {
        return None;
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
    let dev = sess.state.devices.get(&our_key)?;
    let (cw, ch, cmhz) = sess.current_dims(dev)?;
    let hz = ((cmhz as f64) / 1000.0).round() as u32;
    tracing::info!(
        want_w,
        want_h,
        want_hz,
        active_w = cw,
        active_h = ch,
        active_hz = hz,
        "KWin output management: custom mode installed + selected (in-process)"
    );
    Some((cw, ch, hz.max(1)))
}

/// Re-enable outputs by name at their captured `WxH@Hz` (teardown). `true` only
/// if every requested output was staged and the config applied; `false` sends
/// the caller to `kscreen-doctor`.
///
/// Names were captured on a different connection during [`apply_topology`];
/// this restore opens a fresh session later, so a name may no longer resolve.
/// An empty `kde_output_configuration_v2` still gets `applied`, so returning
/// the apply verdict alone would report success for a total no-op and leave
/// a physical dark.
pub(crate) fn reenable_outputs(outputs: &[(String, String)]) -> bool {
    if outputs.is_empty() {
        return true;
    }
    let Ok(mut sess) = Session::open("restore_outputs") else {
        return false;
    };
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
    if matched == 0 {
        // Applying would ack an empty config and read as success. Hand the whole
        // restore to kscreen-doctor, which addresses outputs by name with no live proxy.
        config.destroy();
        tracing::warn!(
            requested = ?outputs,
            "KWin output management: none of the outputs to restore are addressable on this \
             connection — kscreen-doctor fallback"
        );
        return false;
    }
    let ok = sess.apply(&config, deadline);
    config.destroy();
    let complete = ok && matched == outputs.len();
    if complete {
        tracing::info!(reenabled = ?outputs, "KWin output management: restored outputs (in-process)");
    } else {
        tracing::warn!(
            requested = ?outputs,
            matched,
            applied = ok,
            reason = ?sess.state.failure_reason,
            "KWin output management: restore incomplete — kscreen-doctor backstop takes the rest \
             (an output left disabled is a physical left dark)"
        );
    }
    complete
}

/// Enable a virtual output KWin created but left disabled, addressed by the
/// `Virtual-<name>` prefix. Returns the head's name when one matched, was
/// disabled, and the enable applied.
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
pub(crate) fn enable_disabled_output(prefix: &str) -> Option<String> {
    let mut sess = Session::open("enable_disabled").ok()?;
    let deadline = Instant::now() + OP_BUDGET;
    // Newest-wins, as elsewhere: a reconnect can leave a same-name predecessor
    // briefly announced, and enabling that one repairs an output already going away.
    let dev = sess
        .state
        .devices
        .values()
        .filter(|d| d.name.as_deref().is_some_and(|n| n.starts_with(prefix)) && d.proxy.is_some())
        .max_by_key(|d| (d.global, d.seq))
        .cloned()?;
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
        "KWin output management: KWin created our virtual output DISABLED and refused to stream \
         it; enabled it — KWin persists that, so the retry's request comes back enabled"
    );
    Some(name)
}

/// Position the output identified by `uuid` at `(x, y)`. `false` → `kscreen-doctor`.
pub(crate) fn set_position(uuid: &str, x: i32, y: i32) -> bool {
    let Ok(mut sess) = Session::open("position") else {
        return false;
    };
    let deadline = Instant::now() + OP_BUDGET;
    let Some(dev) = sess
        .state
        .devices
        .values()
        .find(|d| d.uuid.as_deref() == Some(uuid))
        .cloned()
    else {
        return false;
    };
    let Some(proxy) = dev.proxy.as_ref() else {
        return false;
    };
    let config = sess.new_config();
    config.position(proxy, x, y);
    let ok = sess.apply(&config, deadline);
    config.destroy();
    if ok {
        tracing::info!(
            uuid,
            x,
            y,
            "KWin output management: placed output (in-process)"
        );
    }
    ok
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
}
