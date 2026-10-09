//! KWin's window list over `org_kde_plasma_window_management`: every toplevel, Wayland and
//! Xwayland alike, with the pid that owns it.
//!
//! The global is restricted like `zkde_screencast`: KWin advertises it only to a client whose
//! `.desktop` lists it under `X-KDE-Wayland-Interfaces` (`io.unom.Punktfunk.Host.desktop`). A host
//! installed before that line shipped sees no global until the session restarts.
//!
//! One short connection per read: bind, collect the windows KWin announces on bind, ask each for its
//! state, disconnect. Nothing is kept between reads, so a restarted KWin costs nothing.

use crate::toplevels::Toplevel;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};

// Vendored `protocols/plasma-window-management.xml`, generated inline.
#[allow(clippy::all, dead_code, non_camel_case_types, non_snake_case, unused)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocols/plasma-window-management.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocols/plasma-window-management.xml");
}

use protocol::org_kde_plasma_window::{Event as WindowEvent, OrgKdePlasmaWindow as Window};
use protocol::org_kde_plasma_window_management::{
    Event as ManagementEvent, OrgKdePlasmaWindowManagement as Management,
};

/// The protocol requires binding at 17 or later; 21 is the vendored XML's version.
const MANAGEMENT_MIN: u32 = 17;
const MANAGEMENT_MAX: u32 = 21;
/// `mapped` arrived in 21. Below it, a window is announced only once mapped.
const MAPPED_SINCE: u32 = 21;

/// `org_kde_plasma_window_management.state` bits this reader uses.
const STATE_ACTIVE: u32 = 0x1;
const STATE_MINIMIZED: u32 = 0x2;
const STATE_FULLSCREEN: u32 = 0x8;

/// One read, bind to answers. The lease watcher asks every second; a KWin slower than this is
/// read again on the next tick.
const READ_BUDGET: Duration = Duration::from_millis(800);

/// The missing global is a packaging fact; say it once per process, not every second.
static NO_GLOBAL_LOGGED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct WindowState {
    uuid: String,
    title: String,
    app_id: String,
    pid: Option<u32>,
    flags: u32,
    initial: bool,
    mapped: bool,
    unmapped: bool,
}

#[derive(Default)]
struct State {
    manager: Option<(Management, u32)>,
    announced: Vec<String>,
    windows: Vec<WindowState>,
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
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            if interface == Management::interface().name && version >= MANAGEMENT_MIN {
                let v = version.min(MANAGEMENT_MAX);
                state.manager = Some((registry.bind::<Management, _, _>(name, v, qh, ()), v));
            }
        }
    }
}

impl Dispatch<Management, ()> for State {
    fn event(
        state: &mut Self,
        _: &Management,
        event: ManagementEvent,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ManagementEvent::WindowWithUuid { uuid, .. } = event {
            state.announced.push(uuid);
        }
    }
}

impl Dispatch<Window, usize> for State {
    fn event(
        state: &mut Self,
        _: &Window,
        event: WindowEvent,
        index: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(w) = state.windows.get_mut(*index) else {
            return;
        };
        match event {
            WindowEvent::TitleChanged { title } => w.title = title,
            WindowEvent::AppIdChanged { app_id } => w.app_id = app_id,
            WindowEvent::StateChanged { flags } => w.flags = flags,
            WindowEvent::PidChanged { pid } => w.pid = (pid != 0).then_some(pid),
            WindowEvent::InitialState => w.initial = true,
            WindowEvent::Mapped => w.mapped = true,
            WindowEvent::Unmapped => w.unmapped = true,
            _ => {}
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

/// Every window KWin has, on every head. `None` when this is not a KWin session or the global is
/// not granted; empty when KWin did not answer inside [`READ_BUDGET`], so the next read tries again.
pub(crate) fn toplevels() -> Option<Vec<Toplevel>> {
    let Ok(conn) = Connection::connect_to_env() else {
        return None;
    };
    let mut queue = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    let mut state = State::default();
    let deadline = Instant::now() + READ_BUDGET;
    // Registry, then the windows KWin announces on bind.
    if !barrier(&conn, &mut queue, &mut state, 1, deadline) {
        return Some(Vec::new());
    }
    let Some((manager, version)) = state.manager.clone() else {
        if !NO_GLOBAL_LOGGED.swap(true, Ordering::Relaxed) {
            tracing::info!(
                "kwin: org_kde_plasma_window_management is not granted to the host — launch holds \
                 end when the game's process starts; re-login after updating so KWin reads the \
                 host's .desktop grant"
            );
        }
        return None;
    };
    if !barrier(&conn, &mut queue, &mut state, 2, deadline) {
        return Some(Vec::new());
    }
    for uuid in std::mem::take(&mut state.announced) {
        let index = state.windows.len();
        manager.get_window_by_uuid(uuid.clone(), &qh, index);
        state.windows.push(WindowState {
            uuid,
            ..Default::default()
        });
    }
    if !barrier(&conn, &mut queue, &mut state, 3, deadline) {
        return Some(Vec::new());
    }
    Some(
        state
            .windows
            .iter()
            .filter_map(|w| on_screen(w, version))
            .collect(),
    )
}

/// A window the player can see, as a [`Toplevel`]. KWin names no head per window, so `output` and
/// `workspace` stay empty.
fn on_screen(w: &WindowState, version: u32) -> Option<Toplevel> {
    let mapped = version < MAPPED_SINCE || w.mapped;
    if !w.initial || !mapped || w.unmapped || w.flags & STATE_MINIMIZED != 0 {
        return None;
    }
    Some(Toplevel {
        id: w.uuid.clone(),
        title: w.title.clone(),
        app_id: w.app_id.clone(),
        pid: w.pid,
        focused: w.flags & STATE_ACTIVE != 0,
        fullscreen: w.flags & STATE_FULLSCREEN != 0,
        workspace: String::new(),
        output: String::new(),
    })
}

/// A `wl_display.sync` barrier bounded by `deadline`: `true` once every event sent before it has
/// been dispatched.
fn barrier(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    serial: u32,
    deadline: Instant,
) -> bool {
    matches!(
        crate::wl_pump::sync_barrier(conn, queue, state, serial, deadline, None),
        Ok(crate::wl_pump::Pumped::Done)
    )
}

impl crate::wl_pump::SyncDone for State {
    fn sync_done(&self) -> u32 {
        self.sync_done
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(flags: u32) -> WindowState {
        WindowState {
            uuid: "u".into(),
            title: "Celeste".into(),
            app_id: "steam_app_504230".into(),
            pid: Some(4242),
            flags,
            initial: true,
            ..Default::default()
        }
    }

    /// Only a mapped, unminimized window whose state has arrived is on screen, and `mapped` only
    /// counts where KWin sends it.
    #[test]
    fn only_a_window_the_player_can_see_is_listed() {
        let seen = on_screen(&window(STATE_ACTIVE | STATE_FULLSCREEN), 20).expect("listed");
        assert_eq!(seen.pid, Some(4242));
        assert!(seen.focused && seen.fullscreen);
        assert!(on_screen(&window(STATE_MINIMIZED), 20).is_none());
        assert!(on_screen(&window(0), 21).is_none(), "v21 waits for mapped");
        let mut mapped = window(0);
        mapped.mapped = true;
        assert!(on_screen(&mapped, 21).is_some());
        let mut partial = window(0);
        partial.initial = false;
        assert!(on_screen(&partial, 20).is_none(), "state still arriving");
        let mut gone = window(0);
        gone.unmapped = true;
        assert!(on_screen(&gone, 20).is_none());
    }
}
