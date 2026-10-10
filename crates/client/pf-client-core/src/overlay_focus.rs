//! Gamescope overlay input mask. Tells [`crate::gamepad::GamepadService::set_masked`]
//! to stop forwarding the real pad while Steam's menu or QAM owns it. SDL's
//! unfocused-window drop cannot fire here: gamescope focuses per Xwayland ctx,
//! the overlay lives in the root ctx, and the client sits alone in its own.
//!
//! Signal: `GAMESCOPE_FOCUSED_APP` vs `GAMESCOPE_FOCUSED_APP_GFX` on the root
//! ctx's root window (`gamescope -e` only). Equal in play; they diverge when
//! something else has taken input. Compare inequality, not "app is Steam" —
//! a non-Steam shortcut's appid is assigned at creation.
//!
//! Gaming Mode uses two Xwaylands; atoms live on the first, `$DISPLAY` is
//! the second. Candidates are `$DISPLAY`, the sockets in `/tmp/.X11-unix` and
//! the abstract `@/tmp/.X11-unix/X<n>` sockets in `/proc/net/unix`; keep the
//! first root that carries both atoms. No cookie: gamescope Xwayland accepts
//! local connections.
//!
//! A flatpak's `/tmp/.X11-unix` is a private tmpfs that no `--filesystem` grant
//! fills, so there the abstract socket, open through `--share=network`, is the
//! only way to the root ctx.
//!
//! Best-effort: no gamescope or no X means "no signal" and fails open (forward
//! as before). A session that has gamescope but no reachable root says so once.

use socket2::{Domain, SockAddr, Socket, Type};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};

/// After X drops. Gaming Mode recreates Xwayland on session restart; a hot
/// retry loop is not worth it.
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// A local X server accepts in microseconds. 250 ms bounds one dead listener.
const DIAL_TIMEOUT: Duration = Duration::from_millis(250);

/// Overlay-owns-input flag from the watcher thread. Relaxed load: the
/// presenter polls each frame and talks to the gamepad service on an edge.
pub struct OverlayFocus {
    open: Arc<AtomicBool>,
}

impl OverlayFocus {
    /// `None` when this is not a gamescope Steam session, or when
    /// `PUNKTFUNK_OVERLAY_MASK=0`. The caller then keeps its window-focus path,
    /// which is the right signal everywhere the compositor actually moves focus.
    pub fn start() -> Option<OverlayFocus> {
        if crate::env_on("PUNKTFUNK_OVERLAY_MASK") == Some(false) {
            tracing::info!("overlay input mask disabled by PUNKTFUNK_OVERLAY_MASK");
            return None;
        }
        if !gamescope_session() {
            return None;
        }
        let open = Arc::new(AtomicBool::new(false));
        let flag = open.clone();
        std::thread::Builder::new()
            .name("punktfunk-overlay-focus".into())
            .spawn(move || watch(&flag))
            .map_err(|e| tracing::warn!(error = %e, "overlay focus watcher start failed"))
            .ok()?;
        Some(OverlayFocus { open })
    }

    pub fn is_open(&self) -> bool {
        self.open.load(Ordering::Relaxed)
    }
}

/// The only place this signal exists. Same env checks the shells use for
/// Gaming Mode.
pub fn gamescope_session() -> bool {
    crate::gamescope::under_gamescope()
        || std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.eq_ignore_ascii_case("gamescope"))
}

/// `$DISPLAY` first (single-server gamescope publishes on the app's display),
/// then every X socket that exists, by path or abstract name. Do not probe
/// `:0..:N` — that would connect to displays that are not there.
fn candidate_displays() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(d) = std::env::var("DISPLAY") {
        if !d.is_empty() {
            out.push(d);
        }
    }
    let mut found: Vec<u32> = std::fs::read_dir("/tmp/.X11-unix")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .into_string()
                .ok()?
                .strip_prefix('X')?
                .parse()
                .ok()
        })
        .collect();
    found.extend(abstract_display_numbers(
        &std::fs::read("/proc/net/unix").unwrap_or_default(),
    ));
    found.sort_unstable();
    found.dedup();
    for n in found {
        let d = format!(":{n}");
        if !out.contains(&d) {
            out.push(d);
        }
    }
    out
}

/// Display numbers of the abstract `@/tmp/.X11-unix/X<n>` sockets in a
/// `/proc/net/unix` table. The path is the last column; lines without one end
/// in the inode and never match.
fn abstract_display_numbers(table: &[u8]) -> Vec<u32> {
    String::from_utf8_lossy(table)
        .lines()
        .filter_map(|l| {
            l.split_whitespace()
                .next_back()?
                .strip_prefix("@/tmp/.X11-unix/X")?
                .parse()
                .ok()
        })
        .collect()
}

/// The path socket first. A sandbox without it falls back to the abstract one:
/// x11rb only dials paths, so the stream is opened here. The deadline is for a
/// listener that never accepts, which a plain `connect` would wait on forever.
fn open(dpy: &str) -> Option<(RustConnection, usize)> {
    if let Ok(c) = RustConnection::connect(Some(dpy)) {
        return Some(c);
    }
    let n: u32 = dpy.strip_prefix(':')?.split('.').next()?.parse().ok()?;
    let addr = SockAddr::unix(format!("\0/tmp/.X11-unix/X{n}")).ok()?;
    let sock = Socket::new(Domain::UNIX, Type::STREAM, None).ok()?;
    sock.connect_timeout(&addr, DIAL_TIMEOUT).ok()?;
    let stream = UnixStream::from(OwnedFd::from(sock));
    let (stream, _) = DefaultStream::from_unix_stream(stream).ok()?;
    Some((RustConnection::connect_to_stream(stream, 0).ok()?, 0))
}

/// `None` if this display is not the root ctx. `only_if_exists` so we do not
/// intern the names into an unrelated X server.
fn gamescope_atoms(conn: &RustConnection) -> Option<(Atom, Atom)> {
    let app = conn
        .intern_atom(true, b"GAMESCOPE_FOCUSED_APP")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let gfx = conn
        .intern_atom(true, b"GAMESCOPE_FOCUSED_APP_GFX")
        .ok()?
        .reply()
        .ok()?
        .atom;
    (app != 0 && gfx != 0).then_some((app, gfx))
}

/// gamescope writes length 0 when the appid is 0 (`focusedAppId != 0 ? 1 : 0`).
/// Empty must be `None`, not `Some(0)` — that would differ from every real id.
fn read_appid(conn: &RustConnection, root: Window, atom: Atom) -> Option<u32> {
    let reply = conn
        .get_property(false, root, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    // Edition 2024: tail-expression temporaries drop before the block's locals,
    // so the iterator borrowing `reply` no longer outlives it.
    reply.value32()?.next()
}

/// Overlay iff both appids are known and differ. Absence is never an overlay:
/// a latched mask would kill the pad for the rest of the session.
fn overlay_open_from(app: Option<u32>, gfx: Option<u32>) -> bool {
    matches!((app, gfx), (Some(a), Some(g)) if a != g)
}

fn overlay_open(conn: &RustConnection, root: Window, app: Atom, gfx: Atom) -> bool {
    overlay_open_from(read_appid(conn, root, app), read_appid(conn, root, gfx))
}

/// Block on PropertyNotify for the two atoms. Any X error returns so the
/// outer loop can rebuild after a session restart.
fn watch(flag: &Arc<AtomicBool>) {
    let mut warned = false;
    loop {
        let Some((conn, root, app, gfx)) = connect() else {
            if !std::mem::replace(&mut warned, true) {
                tracing::warn!(masking = false, "gamescope root display unreachable");
            }
            std::thread::sleep(RECONNECT_DELAY);
            continue;
        };
        warned = false;
        // Seed before the first event: the overlay may already be up.
        flag.store(overlay_open(&conn, root, app, gfx), Ordering::Relaxed);
        loop {
            match conn.wait_for_event() {
                Ok(Event::PropertyNotify(e)) if e.atom == app || e.atom == gfx => {
                    let open = overlay_open(&conn, root, app, gfx);
                    if flag.swap(open, Ordering::Relaxed) != open {
                        tracing::info!(open, "gamescope overlay focus changed");
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::info!(error = %e, "gamescope focus watcher disconnected");
                    break;
                }
            }
        }
        // Unmask: a restart mid-overlay would otherwise leave the pad dead.
        flag.store(false, Ordering::Relaxed);
        std::thread::sleep(RECONNECT_DELAY);
    }
}

fn connect() -> Option<(RustConnection, Window, Atom, Atom)> {
    for dpy in candidate_displays() {
        // `dpy`, not `display`: tracing's value helper would steal a field named
        // `display` inside the macro.
        let Some((conn, screen_num)) = open(&dpy) else {
            continue;
        };
        let Some((app, gfx)) = gamescope_atoms(&conn) else {
            continue;
        };
        let root = conn.setup().roots[screen_num].root;
        // Interned names are not enough: a second gamescope Xwayland knows the
        // strings; only the root ctx publishes values.
        if read_appid(&conn, root, gfx).is_none() {
            continue;
        }
        // Check the event mask applied. A silent fail would block forever on a
        // display that never speaks.
        let selected = match conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        ) {
            Ok(cookie) => cookie.check().is_ok(),
            Err(_) => false,
        };
        if !selected {
            continue;
        }
        tracing::info!(dpy, "watching gamescope focus for overlay input masking");
        return Some((conn, root, app, gfx));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divergent_appids_are_an_overlay() {
        assert!(!overlay_open_from(Some(3856846079), Some(3856846079)));
        assert!(overlay_open_from(Some(769), Some(3856846079)));
    }

    #[test]
    fn a_missing_appid_is_never_an_overlay() {
        assert!(!overlay_open_from(None, Some(3856846079)));
        assert!(!overlay_open_from(Some(769), None));
        assert!(!overlay_open_from(None, None));
    }

    #[test]
    fn abstract_x_sockets_come_out_of_the_unix_table() {
        let table = b"Num       RefCount Protocol Flags    Type St Inode Path\n\
            0000: 00000003 00000000 00000000 0001 03 71204 /tmp/.X11-unix/X1\n\
            0000: 00000002 00000000 00010000 0001 01 71205 @/tmp/.X11-unix/X3\n\
            0000: 00000002 00000000 00010000 0001 01 71206 @/tmp/.X11-unix/X0\n\
            0000: 00000003 00000000 00000000 0001 03 71207\n\
            0000: 00000002 00000000 00010000 0001 01 71208 @/tmp/dbus-abc\n";
        assert_eq!(abstract_display_numbers(table), vec![3, 0]);
        assert!(abstract_display_numbers(b"").is_empty());
    }

    #[test]
    fn absence_fails_open_not_closed() {
        assert!(!overlay_open_from(None, None));
    }
}
