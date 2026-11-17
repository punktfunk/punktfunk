//! Whether the session's compositor reads the uinput devices this crate makes. A headless
//! compositor (`kwin_wayland --virtual`, sway on `WLR_BACKENDS=headless`, `gnome-shell
//! --headless`) has no libinput seat: a device made for it is read by nobody, or by another
//! compositor on the box, and its input is lost.
//!
//! [`reaches`] makes a [`VirtualPen`] and takes either of two proofs that the compositor on
//! this session's Wayland socket reads it. It announces the tablet on `zwp_tablet_manager_v2`,
//! the one Wayland protocol that names a compositor's input devices: KWin and sway do. Or its
//! process holds the tablet's event node open: Mutter announces nothing to this client, and
//! KWin keeps a capability, which closes its file table to us. Every uinput device lands on
//! one seat, so the tablet answers for the mouse and the touchpad as well. The answer is kept
//! for the compositor's process; neither proof in two seconds reads as no.

use crate::pen::{VirtualPen, PEN_NAME};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::tablet::zv2::client::{
    zwp_tablet_manager_v2::ZwpTabletManagerV2,
    zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2,
    zwp_tablet_pad_group_v2::{self, ZwpTabletPadGroupV2},
    zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
    zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
    zwp_tablet_pad_v2::{self, ZwpTabletPadV2},
    zwp_tablet_seat_v2::{self, ZwpTabletSeatV2},
    zwp_tablet_tool_v2::ZwpTabletToolV2,
    zwp_tablet_v2::{self, ZwpTabletV2},
};

/// How long a compositor gets to announce the tablet. udev and libinput take 50 to 300 ms.
const ANNOUNCE_WAIT: Duration = Duration::from_secs(2);
/// A no is asked again after this: the compositor may have been starting up.
const NO_HOLDS: Duration = Duration::from_secs(600);

/// The last answer: the compositor's pid, what it said, and when.
static ANSWER: Mutex<Option<(i32, bool, Instant)>> = Mutex::new(None);
/// Held while one caller asks, so [`known`] never waits behind a probe.
static ASKING: Mutex<()> = Mutex::new(());

/// Whether the compositor on this session's Wayland socket reads this crate's uinput devices.
/// The first call for a compositor process blocks until it announces the probe tablet, at most
/// [`ANNOUNCE_WAIT`]; later calls answer at once.
pub fn reaches() -> bool {
    answer(true)
}

/// What that compositor last said, without asking it: `false` until [`reaches`] has.
pub fn known() -> bool {
    answer(false)
}

fn answer(ask: bool) -> bool {
    // A seat's node is the supervisor's proxy, which refuses a tablet.
    if crate::pad_broker::proxied(std::path::Path::new("/dev/uinput")) {
        return false;
    }
    let Some((stream, pid)) = compositor() else {
        return false;
    };
    let kept = || {
        let answer = *ANSWER.lock().unwrap_or_else(|e| e.into_inner());
        answer.filter(|&(p, said, at)| p == pid && (said || at.elapsed() < NO_HOLDS))
    };
    if let Some((_, said, _)) = kept() {
        return said;
    }
    if !ask {
        return false;
    }
    let _asking = ASKING.lock().unwrap_or_else(|e| e.into_inner());
    // Another caller may have asked while this one waited.
    if let Some((_, said, _)) = kept() {
        return said;
    }
    let said = match read_by(stream, pid) {
        Ok(said) => said,
        Err(e) => {
            tracing::debug!(error = %format!("{e:#}"), "uinput reach probe did not run");
            return false;
        }
    };
    tracing::info!(
        compositor_pid = pid,
        reads_uinput = said,
        "asked the compositor whether it reads uinput devices"
    );
    *ANSWER.lock().unwrap_or_else(|e| e.into_inner()) = Some((pid, said, Instant::now()));
    said
}

/// This session's Wayland socket and the pid of the compositor behind it.
fn compositor() -> Option<(UnixStream, i32)> {
    // Mutter is fed over D-Bus and may name no socket; its first one is `wayland-0`.
    let name = pf_host_config::session::wayland_display().unwrap_or_else(|| "wayland-0".into());
    let path = std::path::PathBuf::from(pf_host_config::session::runtime_dir()?).join(name);
    let stream = UnixStream::connect(path).ok()?;
    let pid = rustix::net::sockopt::socket_peercred(&stream).ok()?.pid;
    Some((stream, pid.as_raw_nonzero().get()))
}

#[derive(Default)]
struct Probe {
    manager: Option<ZwpTabletManagerV2>,
    seats: Vec<WlSeat>,
    found: bool,
}

/// Make the probe tablet and wait for the compositor `pid` to name it or to open its node.
fn read_by(stream: UnixStream, pid: i32) -> anyhow::Result<bool> {
    let pen = VirtualPen::create()?;
    let node = pen.event_node();
    let conn = Connection::from_socket(stream)?;
    let mut queue = conn.new_event_queue::<Probe>();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut probe = Probe::default();
    queue.roundtrip(&mut probe)?;
    if let Some(manager) = &probe.manager {
        for seat in &probe.seats {
            manager.get_tablet_seat(seat, &qh, ());
        }
    }
    let deadline = Instant::now() + ANNOUNCE_WAIT;
    loop {
        queue.roundtrip(&mut probe)?;
        if probe.found || node.as_deref().is_some_and(|node| holds(pid, node)) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether process `pid` has `node` open. `false` too where its file table is not ours to read.
fn holds(pid: i32, node: &std::path::Path) -> bool {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten()
        .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|to| to == node))
}

impl Dispatch<wl_registry::WlRegistry, ()> for Probe {
    fn event(
        probe: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name, interface, ..
        } = event
        else {
            return;
        };
        if interface == WlSeat::interface().name {
            probe.seats.push(registry.bind(name, 1, qh, ()));
        } else if interface == ZwpTabletManagerV2::interface().name {
            // Version 1: no dials, so no event this probe has no child for.
            probe.manager = Some(registry.bind(name, 1, qh, ()));
        }
    }
}

impl Dispatch<ZwpTabletV2, ()> for Probe {
    fn event(
        probe: &mut Self,
        _: &ZwpTabletV2,
        event: zwp_tablet_v2::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if matches!(event, zwp_tablet_v2::Event::Name { name } if name == PEN_NAME) {
            probe.found = true;
        }
    }
}

/// An event that makes an object needs that object's user data, read or not.
macro_rules! children {
    ($parent:ty, [$($opcode:pat => $child:ty),+ $(,)?]) => {
        impl Dispatch<$parent, ()> for Probe {
            fn event(_: &mut Self, _: &$parent, _: <$parent as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
            event_created_child!(Probe, $parent, [$($opcode => ($child, ())),+]);
        }
    };
}

children!(ZwpTabletSeatV2, [
    zwp_tablet_seat_v2::EVT_TABLET_ADDED_OPCODE => ZwpTabletV2,
    zwp_tablet_seat_v2::EVT_TOOL_ADDED_OPCODE => ZwpTabletToolV2,
    zwp_tablet_seat_v2::EVT_PAD_ADDED_OPCODE => ZwpTabletPadV2,
]);
children!(ZwpTabletPadV2, [zwp_tablet_pad_v2::EVT_GROUP_OPCODE => ZwpTabletPadGroupV2]);
children!(ZwpTabletPadGroupV2, [
    zwp_tablet_pad_group_v2::EVT_RING_OPCODE => ZwpTabletPadRingV2,
    zwp_tablet_pad_group_v2::EVT_STRIP_OPCODE => ZwpTabletPadStripV2,
    zwp_tablet_pad_group_v2::EVT_DIAL_OPCODE => ZwpTabletPadDialV2,
]);

macro_rules! ignored {
    ($($t:ty),+) => {$(
        impl Dispatch<$t, ()> for Probe {
            fn event(_: &mut Self, _: &$t, _: <$t as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )+};
}

ignored!(
    WlSeat,
    ZwpTabletManagerV2,
    ZwpTabletToolV2,
    ZwpTabletPadRingV2,
    ZwpTabletPadStripV2,
    ZwpTabletPadDialV2
);

#[cfg(test)]
mod tests {
    /// On a box with a compositor: prints what it says. `WAYLAND_DISPLAY` names it.
    #[test]
    #[ignore = "needs /dev/uinput and a Wayland compositor"]
    fn a_real_compositor_says_whether_it_reads_uinput() {
        let t = std::time::Instant::now();
        let said = super::reaches();
        eprintln!("reaches = {said} after {:?}", t.elapsed());
        let t = std::time::Instant::now();
        assert_eq!(super::reaches(), said, "the answer is kept");
        eprintln!("asked again: {:?}", t.elapsed());
    }
}
