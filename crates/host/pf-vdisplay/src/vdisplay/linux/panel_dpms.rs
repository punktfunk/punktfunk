//! DPMS dispatcher that honors [`Topology::Exclusive`](crate::policy::Topology::Exclusive)
//! for a gamescope session: darken the box's own physical panels.
//!
//! Gamescope is its own compositor. A desktop output-config with zero enabled outputs is
//! refused, and the session has no virtual output to leave enabled. DPMS darkens without
//! topology churn; stream input never wakes the panels (it is injected into gamescope's
//! EIS socket).
//!
//! Each arm self-gates: KDE in-process `org_kde_kwin_dpms` then `kscreen-doctor`; sway /
//! Hyprland / niri via native IPC; DRM CRTCs when no compositor holds master. GNOME cannot be
//! served (Mutter exposes no client DPMS and holds DRM master); [`darken`] logs that at
//! `warn!`.
//!
//! The hold is refcounted here, not floated through the registry's per-group restore —
//! every gamescope spawn is its own display group, so group restore would re-light under
//! a second still-streaming session. [`acquire_stream_darken`] on 0→1,
//! [`release_stream_darken`] as per-display topology restore on 1→0. KWin and niri wake a
//! blanked panel on input; sway and Hyprland do not, so their heads are journaled as
//! `EnableHeads` before they go dark, and the next host wakes them when this one dies.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::topology_journal as journal;
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};

// Vendored `protocols/dpms.xml`, generated inline. Only foreign object is core
// `wl_output`, already in `wayland_client::protocol`.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/dpms.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/dpms.xml");
}

use protocol::org_kde_kwin_dpms::{Event as DpmsEvent, OrgKdeKwinDpms as Dpms};
use protocol::org_kde_kwin_dpms_manager::OrgKdeKwinDpmsManager as DpmsManager;

// Wire `org_kde_kwin_dpms.mode`. XML types the args as `uint` (no `enum=`),
// so the generated signatures take `u32`. Values match vendored `dpms.xml`.
const DPMS_MODE_ON: u32 = 0;
const DPMS_MODE_OFF: u32 = 3;

/// Frozen v1 (`org_kde_kwin_dpms_manager`); bind `min(advertised, 1)`.
const MANAGER_MAX: u32 = 1;
/// `wl_output.name` arrived in v4. A lower advert just costs the log its names.
const WL_OUTPUT_MAX: u32 = 4;

/// 3 s budget for one darken/re-light so a wedged compositor cannot pin the
/// session-create or group-teardown thread. Same as `kwin::mgmt::OP_BUDGET`.
const OP_BUDGET: Duration = Duration::from_secs(3);

#[derive(Default)]
struct OutputState {
    proxy: Option<WlOutput>,
    /// Connector name from `wl_output.name` (v4). Logging only; the global number is the address.
    connector: Option<String>,
    dpms: Option<Dpms>,
    /// `org_kde_kwin_dpms.supported`. `None` until the bind burst arrives.
    supported: Option<bool>,
    /// Last `org_kde_kwin_dpms.mode` seen; the post-`set` wait watches this flip.
    mode: Option<u32>,
}

#[derive(Default)]
struct State {
    manager: Option<DpmsManager>,
    /// Keyed by `wl_output` global name: stable for the compositor's lifetime, so
    /// a later re-light connection can find the same outputs.
    outputs: HashMap<u32, OutputState>,
    /// Highest `wl_callback` serial whose `done` has arrived; the pump waits on this.
    sync_done: u32,
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
                if interface == DpmsManager::interface().name {
                    let v = version.min(MANAGER_MAX);
                    state.manager = Some(registry.bind::<DpmsManager, _, _>(name, v, qh, ()));
                } else if interface == WlOutput::interface().name {
                    let v = version.min(WL_OUTPUT_MAX);
                    // Global name in UserData so this output's events (and its dpms object's) find the entry.
                    let out = registry.bind::<WlOutput, _, _>(name, v, qh, name);
                    state.outputs.entry(name).or_default().proxy = Some(out);
                }
            }
            // Unplugged mid-operation: drop the entry so we never `set` on its corpse.
            wl_registry::Event::GlobalRemove { name } => {
                state.outputs.remove(&name);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(o) = state.outputs.get_mut(global) {
                o.connector = Some(name);
            }
        }
    }
}

impl Dispatch<Dpms, u32> for State {
    fn event(
        state: &mut Self,
        _: &Dpms,
        event: DpmsEvent,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.outputs.get_mut(global) else {
            return;
        };
        match event {
            DpmsEvent::Supported { supported } => o.supported = Some(supported != 0),
            DpmsEvent::Mode { mode } => o.mode = Some(mode),
            DpmsEvent::Done => {}
        }
    }
}

// The manager has no events; the impl exists because `WlRegistry::bind` demands one.
impl Dispatch<DpmsManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &DpmsManager,
        _: protocol::org_kde_kwin_dpms_manager::Event,
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

/// Why [`Session::open`] declined. Which rung said no decides the log level
/// and whether `kscreen-doctor` is worth attempting.
enum OpenFailure {
    /// No Wayland connection (`WAYLAND_DISPLAY` unset/stale). Everyday headless case.
    Connect(String),
    /// Connected, but the registry barrier missed the budget: live but wedged.
    /// The `kscreen-doctor` fallback exists for this.
    RegistryBarrier,
    /// Connected, but `org_kde_kwin_dpms_manager` is not advertised — not KWin.
    /// `kscreen-doctor` drives the same KDE-only machinery, so no fallback here.
    NoDpmsGlobal,
    /// Manager bound, but the per-output DPMS state bursts missed the budget.
    StateBarrier,
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
            OpenFailure::NoDpmsGlobal => {
                write!(f, "org_kde_kwin_dpms_manager is not advertised (not KWin)")
            }
            OpenFailure::StateBarrier => {
                write!(
                    f,
                    "the outputs' DPMS state never finished announcing in budget"
                )
            }
        }
    }
}

struct Session {
    conn: Connection,
    queue: wayland_client::EventQueue<State>,
    state: State,
    next_sync: u32,
}

impl Session {
    /// [`Session::connect`], logging the decline: `Connect`/`NoDpmsGlobal` are
    /// everyday non-KDE answers (`debug`); a barrier miss is a live session that
    /// stopped answering — a panel left lit, so `warn`.
    fn open(op: &'static str) -> Result<Session, OpenFailure> {
        let opened = Session::connect();
        if let Err(reason) = &opened {
            match reason {
                OpenFailure::Connect(_) | OpenFailure::NoDpmsGlobal => {
                    tracing::debug!(op, %reason, "KWin DPMS unavailable");
                }
                OpenFailure::RegistryBarrier | OpenFailure::StateBarrier => {
                    tracing::warn!(
                        op,
                        %reason,
                        "KWin DPMS: in-process path unavailable — falling back to kscreen-doctor"
                    );
                }
            }
        }
        opened
    }

    fn connect() -> Result<Session, OpenFailure> {
        let conn =
            crate::session::wayland_connect().map_err(|e| OpenFailure::Connect(e.to_string()))?;
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
        let Some(mgr) = s.state.manager.clone() else {
            return Err(OpenFailure::NoDpmsGlobal);
        };
        // One dpms object per output (UserData = global name). Barrier drains
        // `wl_output.name` and the dpms supported/mode/done bursts.
        let qh = s.queue.handle();
        let bound: Vec<(u32, WlOutput)> = s
            .state
            .outputs
            .iter()
            .filter_map(|(g, o)| o.proxy.clone().map(|p| (*g, p)))
            .collect();
        for (global, out) in bound {
            let d = mgr.get(&out, &qh, global);
            if let Some(o) = s.state.outputs.get_mut(&global) {
                o.dpms = Some(d);
            }
        }
        if !s.sync_barrier(deadline) {
            return Err(OpenFailure::StateBarrier);
        }
        Ok(s)
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

    /// Request `target` on every DPMS-supporting output not already there.
    /// `only` restricts to those globals (re-light must not wake a panel the
    /// user had already put to sleep). `set` is a request the compositor may
    /// decline — wait for the `mode` event; log if it never comes.
    fn set_mode(&mut self, target: u32, only: Option<&[u32]>) -> Vec<(u32, Option<String>)> {
        let deadline = Instant::now() + OP_BUDGET;
        let mut touched: Vec<(u32, Option<String>)> = Vec::new();
        for (global, o) in &self.state.outputs {
            if only.is_some_and(|list| !list.contains(global)) {
                continue;
            }
            if o.supported != Some(true) || o.mode == Some(target) {
                continue;
            }
            if let Some(dpms) = &o.dpms {
                dpms.set(target);
                touched.push((*global, o.connector.clone()));
            }
        }
        if touched.is_empty() {
            return touched;
        }
        let want: Vec<u32> = touched.iter().map(|(g, _)| *g).collect();
        // Vanished mid-wait (`GlobalRemove`) counts as settled: nothing left to flip.
        let confirmed = self.pump_until(deadline, |st| {
            want.iter()
                .all(|g| st.outputs.get(g).is_none_or(|o| o.mode == Some(target)))
        });
        if !confirmed {
            tracing::warn!(
                outputs = ?touched,
                target,
                "KWin DPMS: the compositor did not confirm the mode change in budget (the \
                 requests are flushed; it may still land, or KWin may have declined)"
            );
        }
        touched
    }
}

/// What the 0→1 darken achieved; the 1→0 re-light undoes it. Which arm
/// did the work selects the undo path.
enum Darkened {
    /// In-process path; `(wl_output global, connector)`. Globals are stable
    /// for the compositor's lifetime. A KWin restart in between matches
    /// nothing — the correct no-op, because a fresh KWin comes up lit.
    Wayland(Vec<(u32, Option<String>)>),
    /// `kscreen-doctor --dpms off` ran. It takes no per-output address, so
    /// re-light is the symmetric `--dpms on`.
    Kscreen,
    /// sway: `swaymsg output <name> dpms off`. Re-light undoes exactly these
    /// names, never a sibling session's head.
    Sway(Journaled),
    /// Hyprland: `hyprctl dispatch dpms off <name>`. Same per-name discipline as [`Darkened::Sway`].
    Hyprland(Journaled),
    /// niri: `PowerOffMonitors`, which has no per-output form. Re-light is `PowerOnMonitors`.
    Niri,
    /// Umbriel: `dpms-off` per name. Same per-name discipline as [`Darkened::Sway`].
    Umbriel(Journaled),
    /// [`crate::drm_dpms`] hold: open `/dev/dri/cardN` fds. Re-light is
    /// [`crate::drm_dpms::release`]; the kernel restores the console on last close.
    Drm(crate::drm_dpms::DrmDarken),
}

/// Heads blanked by name on a desktop that input never wakes, and the journal entry that wakes
/// them if this host dies first.
struct Journaled {
    heads: Vec<String>,
    seq: u64,
}

/// Blank heads through `darken`, which hands its plan to `record` before the first goes dark;
/// the plan is journaled there as `EnableHeads` for `backend`. The entry ends naming exactly
/// the heads that went dark — none settles it, fewer are journaled again first — so a replay
/// never wakes a head this left alone.
fn journaled(
    backend: &'static str,
    darken: impl FnOnce(&mut dyn FnMut(&[String])) -> Vec<String>,
) -> Option<Journaled> {
    let mut planned = None;
    let heads = darken(&mut |plan: &[String]| {
        if !plan.is_empty() {
            planned = Some((plan.to_vec(), journal::push_heads(backend, plan)));
        }
    });
    let (plan, seq) = planned?;
    if heads == plan {
        return Some(Journaled { heads, seq });
    }
    let narrowed = (!heads.is_empty()).then(|| Journaled {
        seq: journal::push_heads(backend, &heads),
        heads,
    });
    journal::done(seq);
    narrowed
}

/// Wake `blanked` through `relight`, which answers the heads now on. Every head back settles
/// the journal entry; one still dark keeps it for the next host.
fn relit(blanked: &Journaled, relight: impl FnOnce(&[String]) -> Vec<String>) -> Vec<String> {
    let back = relight(&blanked.heads);
    if back.len() == blanked.heads.len() {
        journal::done(blanked.seq);
    }
    back
}

/// Host-wide darken hold. 0→1 darkens, 1→0 re-lights; in between is a count.
struct Holds {
    count: u32,
    /// 0→1 outcome, held until 1→0. `None` while count > 0 means nothing was
    /// darkened — the release then has nothing to undo.
    darkened: Option<Darkened>,
}

impl Holds {
    /// `true` when the caller darkens and [`record`](Self::record)s: the 0→1 edge, or a
    /// later hold while the first achieved nothing (a barrier miss, a declined request).
    /// The retry costs a few compositor calls per overlapping session on a desk with
    /// nothing to darken.
    fn acquire_edge(&mut self) -> bool {
        self.count += 1;
        self.count == 1 || self.darkened.is_none()
    }

    fn record(&mut self, d: Option<Darkened>) {
        self.darkened = d;
    }

    /// `Some` on the 1→0 edge: the record to undo. A release with no hold
    /// is an unbalanced restore — logged, never underflowed.
    fn release_edge(&mut self) -> Option<Darkened> {
        if self.count == 0 {
            tracing::warn!("KWin DPMS: release without a matching acquire (unbalanced restore)");
            return None;
        }
        self.count -= 1;
        if self.count == 0 {
            self.darkened.take()
        } else {
            None
        }
    }
}

static HOLDS: Mutex<Holds> = Mutex::new(Holds {
    count: 0,
    darkened: None,
});

/// Take one exclusive-topology darken hold. 0→1 darkens; later holds count.
/// Balance each call with [`release_stream_darken`].
///
/// The lock is held across the darken: a racing second acquire must queue
/// behind it, not observe count 2 with nothing darkened. Same on release,
/// so a teardown-overlapping-connect stays ordered (re-light, then darken).
pub fn acquire_stream_darken() {
    let mut h = HOLDS.lock().unwrap_or_else(|e| e.into_inner());
    if h.acquire_edge() {
        let d = darken();
        h.record(d);
    }
}

/// Drop one hold; the last one out re-lights whatever the first darken achieved.
pub fn release_stream_darken() {
    let mut h = HOLDS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(d) = h.release_edge() {
        relight(d);
    }
}

/// Non-KDE desktops, preference order. Each self-gates on its IPC
/// (`SWAYSOCK` / `HYPRLAND_INSTANCE_SIGNATURE` / Umbriel's or niri's socket). Heads are
/// addressed by name so re-light never wakes a sibling session's output, and
/// journaled on sway, Hyprland and Umbriel, which input never wakes; niri has no
/// per-output DPMS. GNOME is absent on purpose — see the module docs.
fn non_kde_desktop_darken() -> Option<Darkened> {
    if let Some(sway) = journaled("wlroots", |record| crate::wlroots::dpms_other_heads(record)) {
        tracing::info!(
            outputs = ?sway.heads,
            "sway: desktop outputs off for the exclusive gamescope stream"
        );
        return Some(Darkened::Sway(sway));
    }
    if let Some(hypr) = journaled("hyprland", |record| {
        crate::hyprland::dpms_other_heads(record)
    }) {
        tracing::info!(
            outputs = ?hypr.heads,
            "hyprland: desktop monitors off for the exclusive gamescope stream"
        );
        return Some(Darkened::Hyprland(hypr));
    }
    if let Some(umbriel) = journaled("umbriel", |record| {
        crate::umbriel::desktop::dpms_other_heads(record)
    }) {
        tracing::info!(
            outputs = ?umbriel.heads,
            "umbriel: desktop outputs off for the exclusive gamescope stream"
        );
        return Some(Darkened::Umbriel(umbriel));
    }
    if crate::niri::is_available() && crate::niri::desktop::power_off_others() {
        tracing::info!("niri: desktop monitors off for the exclusive gamescope stream");
        return Some(Darkened::Niri);
    }
    None
}

/// 0→1 darken: in-process KWin DPMS, then `kscreen-doctor`, then other
/// desktops, then DRM. `None` means nothing darkened, so nothing to restore.
fn darken() -> Option<Darkened> {
    match Session::open("darken") {
        Ok(mut s) => {
            let touched = s.set_mode(DPMS_MODE_OFF, None);
            if touched.is_empty() {
                tracing::debug!(
                    "KWin DPMS: no output to darken (none supported, or all already off)"
                );
                None
            } else {
                tracing::info!(
                    outputs = ?touched,
                    "KWin DPMS: desktop outputs off for the exclusive gamescope stream"
                );
                Some(Darkened::Wayland(touched))
            }
        }
        // Not KDE / no desktop: `kscreen-doctor` drives the same KDE-only
        // machinery, so skip it. Try the other desktops, then DRM.
        Err(e @ (OpenFailure::NoDpmsGlobal | OpenFailure::Connect(_))) => {
            if let Some(d) = non_kde_desktop_darken() {
                return Some(d);
            }
            match crate::drm_dpms::darken() {
                Some(d) => {
                    tracing::info!(
                        cards = ?d.darkened,
                        "DRM: the box's own CRTCs are off for the exclusive gamescope stream (no \
                         desktop compositor to ask — a session in Game Mode has none)"
                    );
                    Some(Darkened::Drm(d))
                }
                // No desktop answered, and no card was ours (foreign master —
                // including a gamescope Attach we must not darken — or nothing
                // lit). Exclusive asked for dark screens; log at `warn!`.
                None => {
                    tracing::warn!(
                        %e,
                        "exclusive topology asked for the box's own screens to go dark: no \
                         desktop compositor on this box could be asked (GNOME/Mutter exposes no \
                         DPMS to clients), and no DRM card was ours to turn off either — the \
                         panel stays as it is for this stream"
                    );
                    None
                }
            }
        }
        // Live session stopped answering: `kscreen-doctor` rides libkscreen/KDED
        // and may still get through.
        Err(_) => match kscreen_dpms("off") {
            Some(true) => {
                tracing::info!(
                    "KWin DPMS: desktop outputs off for the exclusive gamescope stream \
                     (kscreen-doctor fallback)"
                );
                Some(Darkened::Kscreen)
            }
            // Budget kill is not a refusal: kscreen-doctor applies first, then
            // waits. Record the darken so teardown re-lights; `--dpms on` on a
            // lit panel is a no-op.
            None => Some(Darkened::Kscreen),
            Some(false) => {
                tracing::warn!(
                    "KWin DPMS: could not darken the desktop outputs for the exclusive topology \
                     (in-process path and kscreen-doctor both declined) — the panel stays lit"
                );
                None
            }
        },
    }
}

/// 1→0 re-light. Every arm that gives up logs it: a dark panel with no
/// line is the failure this chain exists to prevent. Local input still wakes a
/// KWin or niri panel; a sway, Hyprland or Umbriel head stays journaled until it is back.
fn relight(d: Darkened) {
    match d {
        Darkened::Wayland(outputs) => {
            let globals: Vec<u32> = outputs.iter().map(|(g, _)| *g).collect();
            match Session::open("re-light") {
                Ok(mut s) => {
                    s.set_mode(DPMS_MODE_ON, Some(&globals));
                    tracing::info!(outputs = ?outputs, "KWin DPMS: desktop outputs back on");
                }
                Err(_) => match kscreen_dpms("on") {
                    Some(true) | None => {
                        tracing::info!(
                            "KWin DPMS: desktop outputs back on (kscreen-doctor fallback)"
                        );
                    }
                    Some(false) => {
                        tracing::error!(
                            outputs = ?outputs,
                            "KWin DPMS: the desktop outputs did not re-light (in-process \
                             restore and kscreen-doctor both declined) — the panel stays dark \
                             until local input wakes it"
                        );
                    }
                },
            }
        }
        Darkened::Kscreen => {
            if kscreen_dpms("on") == Some(false) {
                tracing::error!(
                    "KWin DPMS: the desktop outputs did not re-light (kscreen-doctor refused \
                     the --dpms on it earlier accepted the off for) — the panel stays dark until \
                     local input wakes it"
                );
            }
        }
        // Per-name: a head unplugged meanwhile fails its one command; the others still re-light.
        Darkened::Sway(blanked) => {
            let back = relit(&blanked, crate::wlroots::relight_heads);
            if back.is_empty() {
                tracing::error!(
                    outputs = ?blanked.heads,
                    "sway: the desktop outputs did not re-light — they stay dark until \
                     `swaymsg output '*' dpms on`"
                );
            } else {
                tracing::info!(outputs = ?back, "sway: desktop outputs back on");
            }
        }
        Darkened::Hyprland(blanked) => {
            let back = relit(&blanked, crate::hyprland::relight_heads);
            if back.is_empty() {
                tracing::error!(
                    outputs = ?blanked.heads,
                    "hyprland: the desktop monitors did not re-light — they stay dark until \
                     `hyprctl dispatch dpms on`"
                );
            } else {
                tracing::info!(outputs = ?back, "hyprland: desktop monitors back on");
            }
        }
        Darkened::Umbriel(blanked) => {
            let back = relit(&blanked, crate::umbriel::desktop::relight_heads);
            if back.is_empty() {
                tracing::error!(
                    outputs = ?blanked.heads,
                    "umbriel: the desktop outputs did not re-light — they stay dark until \
                     `umbriel msg dpms-on`"
                );
            } else {
                tracing::info!(outputs = ?back, "umbriel: desktop outputs back on");
            }
        }
        Darkened::Niri => match crate::niri::desktop::power_on_all() {
            Ok(()) => tracing::info!("niri: desktop monitors back on"),
            Err(e) => tracing::error!(
                error = %format!("{e:#}"),
                "niri: the desktop monitors did not re-light — they stay dark until local input \
                 or `niri msg action power-on-monitors`"
            ),
        },
        // Hold is the open fds: the release closes them, reads the heads back and logs.
        Darkened::Drm(d) => crate::drm_dpms::release(d),
    }
}

/// `kscreen-doctor --dpms <on|off>`. Shared `kwin.rs` budget: `Some(true)`
/// succeeded, `Some(false)` refused, `None` killed at budget (applies first,
/// then waits — a kill usually means it landed).
fn kscreen_dpms(mode: &'static str) -> Option<bool> {
    crate::kwin::kscreen_verdict(&["--dpms".to_string(), mode.to_string()])
}

#[cfg(test)]
mod tests {
    use super::{journaled, relit, Darkened, Holds};
    use crate::fake::{Call, FakeDisplay};
    use crate::topology_journal::{Head, Undo};

    fn fresh() -> Holds {
        Holds {
            count: 0,
            darkened: None,
        }
    }

    /// [`crate::fake::in_child`] for this module: the journal is a process global.
    fn in_child(test: &str, body: fn()) {
        crate::fake::in_child(module_path!(), test, &[], body)
    }

    /// Every journal file of this process's state dir, concatenated; empty when none is left.
    fn journal_text() -> String {
        let dir = pf_paths::state_dir().join("topology-journal");
        std::fs::read_dir(dir).map_or(String::new(), |files| {
            files
                .flatten()
                .map(|f| std::fs::read_to_string(f.path()).unwrap_or_default())
                .collect()
        })
    }

    fn names(heads: &[&str]) -> Vec<String> {
        heads.iter().map(|h| h.to_string()).collect()
    }

    /// A fake desktop's darken: hands `plan` to `record`, then blanks `dark`.
    fn blank(record: &mut dyn FnMut(&[String]), plan: &[&str], dark: &[&str]) -> Vec<String> {
        record(&names(plan));
        names(dark)
    }

    /// The hold's sway darken is journaled while the head is dark; the re-light that wakes
    /// it settles the entry.
    #[test]
    fn a_blanked_sway_head_is_journaled_until_its_relight() {
        in_child("a_blanked_sway_head_is_journaled_until_its_relight", || {
            let mut h = fresh();
            assert!(h.acquire_edge());
            h.record(journaled("wlroots", |r| blank(r, &["DP-1"], &["DP-1"])).map(Darkened::Sway));
            let dark = journal_text();
            assert!(dark.contains(r#""backend": "wlroots""#), "{dark}");
            assert!(dark.contains(r#""name": "DP-1""#), "{dark}");
            let Some(Darkened::Sway(blanked)) = h.release_edge() else {
                panic!("the hold blanked sway");
            };
            assert_eq!(relit(&blanked, |heads| heads.to_vec()), ["DP-1"]);
            assert_eq!(journal_text(), "", "the re-light settles the entry");
        });
    }

    /// A planned head that did not go dark (Hyprland's was already off) leaves the entry
    /// naming only the blanked one; a head that does not wake keeps the entry for the next
    /// host; a darken that blanked nothing journals nothing.
    #[test]
    fn the_entry_names_exactly_the_heads_that_went_dark() {
        in_child("the_entry_names_exactly_the_heads_that_went_dark", || {
            let blanked = journaled("hyprland", |r| blank(r, &["DP-1", "HDMI-A-1"], &["DP-1"]))
                .expect("one head went dark");
            assert_eq!(blanked.heads, ["DP-1"]);
            let dark = journal_text();
            assert!(
                dark.contains("DP-1") && !dark.contains("HDMI-A-1"),
                "{dark}"
            );
            assert!(relit(&blanked, |_| Vec::new()).is_empty());
            assert_eq!(journal_text(), dark, "a head still dark keeps its entry");
            crate::topology_journal::done(blanked.seq);

            assert!(journaled("hyprland", |r| blank(r, &["DP-1"], &[])).is_none());
            assert!(journaled("wlroots", |r| blank(r, &[], &[])).is_none());
            assert_eq!(journal_text(), "");
        });
    }

    /// A host killed while sway is blanked: the next host's reclaim runs the entry through
    /// the backend it names, whose `EnableHeads` undo sends `dpms on`, and the file goes.
    #[test]
    fn the_next_host_wakes_a_head_a_dead_host_blanked() {
        in_child("the_next_host_wakes_a_head_a_dead_host_blanked", || {
            assert!(journaled("wlroots", |r| blank(r, &["DP-1"], &["DP-1"])).is_some());
            crate::topology_journal::crash();
            let sway = FakeDisplay::new("wlroots");
            sway.serve_by_name();
            crate::topology_journal::reclaim(&crate::backend_by_id);
            let woken = Undo::EnableHeads {
                heads: vec![Head {
                    name: "DP-1".into(),
                    mode: None,
                }],
            };
            assert_eq!(sway.take_calls(), [Call::Undo(woken)]);
            assert_eq!(journal_text(), "");
        });
    }

    #[test]
    fn first_acquire_darkens_later_ones_count() {
        let mut h = fresh();
        assert!(h.acquire_edge(), "0→1 must darken");
        h.record(Some(Darkened::Kscreen));
        assert!(
            !h.acquire_edge(),
            "a second concurrent stream must not re-darken"
        );
        assert!(!h.acquire_edge());
    }

    /// A barrier miss or a declined request on the first hold must not leave every later
    /// overlapping exclusive stream with the desk lit.
    #[test]
    fn a_first_darken_that_achieved_nothing_is_retried_by_the_next_hold() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(None);
        assert!(h.acquire_edge(), "the second hold tries again");
        h.record(Some(Darkened::Kscreen));
        assert!(!h.acquire_edge(), "once darkened, later holds only count");
    }
    #[test]
    fn only_the_last_release_relights() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(Some(Darkened::Wayland(vec![(7, Some("DP-1".into()))])));
        assert!(!h.acquire_edge());
        // Sibling still streams: panel stays dark.
        assert!(h.release_edge().is_none());
        let d = h.release_edge();
        assert!(matches!(d, Some(Darkened::Wayland(v)) if v == vec![(7, Some("DP-1".into()))]));
    }

    #[test]
    fn a_darken_that_did_nothing_restores_nothing() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(None); // nothing changed
        assert!(h.release_edge().is_none(), "nothing to undo");
        assert_eq!(h.count, 0);
    }

    #[test]
    fn unbalanced_release_never_underflows() {
        let mut h = fresh();
        assert!(h.release_edge().is_none());
        assert_eq!(h.count, 0, "count must not wrap");
        assert!(h.acquire_edge());
        h.record(Some(Darkened::Kscreen));
        assert!(matches!(h.release_edge(), Some(Darkened::Kscreen)));
    }

    #[test]
    fn a_full_cycle_rearms_the_darken() {
        let mut h = fresh();
        assert!(h.acquire_edge());
        h.record(Some(Darkened::Kscreen));
        assert!(h.release_edge().is_some());
        assert!(
            h.acquire_edge(),
            "the 0→1 edge must re-arm after a full cycle"
        );
    }
}
