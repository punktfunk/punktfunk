//! Gamescope overlay input mask. Tells [`crate::gamepad::GamepadService::set_masked`]
//! to stop forwarding the real pad while any Steam UI owns it. SDL's
//! unfocused-window drop cannot fire here: gamescope focuses per Xwayland ctx,
//! Steam lives in the root ctx, and the client sits alone in its own.
//!
//! Signal: `GAMESCOPE_FOCUSED_APP` (input) on the root ctx's root window
//! (`gamescope -e` only). Pads forward while it is this client's app id. Without
//! that id, they forward while it equals `GAMESCOPE_FOCUSED_APP_GFX` (base
//! layer), which misses Steam's own pages: Settings or Downloads make Steam both.
//!
//! Gaming Mode uses two Xwaylands; atoms live on the first, `$DISPLAY` is
//! the second. Candidates are `$DISPLAY`, the sockets in `/tmp/.X11-unix` and
//! the abstract `@/tmp/.X11-unix/X<n>` sockets in `/proc/net/unix`; keep the
//! first root that carries both atoms. No cookie: gamescope Xwayland accepts
//! local connections.
//!
//! A flatpak's `/tmp/.X11-unix` is a private tmpfs that no `--filesystem` grant
//! fills (flatpak 1.18), so there the abstract socket, open through
//! `--share=network`, is the only way to the root ctx.
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
    /// `None` when not under gamescope, or when `PUNKTFUNK_OVERLAY_MASK=0`. The caller
    /// then keeps its window-focus path, which is the right signal everywhere the
    /// compositor actually moves focus.
    pub fn start() -> Option<OverlayFocus> {
        if crate::env_on("PUNKTFUNK_OVERLAY_MASK") == Some(false) {
            tracing::info!("overlay input mask disabled by PUNKTFUNK_OVERLAY_MASK");
            return None;
        }
        if !crate::gamescope::under_gamescope() {
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

/// A Steam launch env value as gamescope's app id. A non-Steam shortcut's
/// 64-bit game id (`appid << 32 | 0x02000000`) carries it in the high half.
fn steam_appid(value: &str) -> Option<u32> {
    let v: u64 = value.trim().parse().ok()?;
    let id = u32::try_from(v).unwrap_or((v >> 32) as u32);
    (id != 0).then_some(id)
}

/// This client's app id. `launch` comes from Steam's env, which the flatpak
/// passes through (its `/proc` hides the `reaper` gamescope reads). A launch id
/// counts once gamescope reports it as the input focus, so a wrong one never masks.
#[derive(Default)]
struct OwnApp {
    launch: Vec<u32>,
    focused: Option<u32>,
}

impl OwnApp {
    fn from_env() -> OwnApp {
        let launch = ["SteamAppId", "SteamGameId"]
            .into_iter()
            .filter_map(|k| steam_appid(&std::env::var(k).ok()?))
            .collect();
        OwnApp {
            launch,
            focused: None,
        }
    }

    /// Mask iff input is on another app than ours, or, until ours is known, on
    /// another app than the base layer. An absent input id never masks: a
    /// latched mask would kill the pad for the rest of the session.
    fn masked(&mut self, app: Option<u32>, gfx: Option<u32>) -> bool {
        if self.focused.is_none() {
            self.focused = app.filter(|a| self.launch.contains(a));
        }
        match (app, self.focused.or(gfx)) {
            (Some(a), Some(owner)) => a != owner,
            _ => false,
        }
    }
}

fn overlay_open(
    conn: &RustConnection,
    root: Window,
    app: Atom,
    gfx: Atom,
    own: &mut OwnApp,
) -> bool {
    own.masked(read_appid(conn, root, app), read_appid(conn, root, gfx))
}

/// Block on PropertyNotify for the two atoms. Any X error returns so the
/// outer loop can rebuild after a session restart.
fn watch(flag: &Arc<AtomicBool>) {
    let mut own = OwnApp::from_env();
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
        flag.store(
            overlay_open(&conn, root, app, gfx, &mut own),
            Ordering::Relaxed,
        );
        loop {
            match conn.wait_for_event() {
                Ok(Event::PropertyNotify(e)) if e.atom == app || e.atom == gfx => {
                    let open = overlay_open(&conn, root, app, gfx, &mut own);
                    if flag.swap(open, Ordering::Relaxed) != open {
                        tracing::info!(
                            open,
                            launch = ?own.launch,
                            own = ?own.focused,
                            "gamescope overlay focus changed"
                        );
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

    const STEAM: u32 = 769;
    const CLIENT: u32 = 3856846079;

    #[test]
    fn divergent_appids_are_an_overlay() {
        let mut own = OwnApp::default();
        assert!(!own.masked(Some(CLIENT), Some(CLIENT)));
        assert!(own.masked(Some(STEAM), Some(CLIENT)));
        // No own id: Steam's own pages read as play.
        assert!(!own.masked(Some(STEAM), Some(STEAM)));
    }

    #[test]
    fn a_missing_appid_is_never_an_overlay() {
        let mut own = OwnApp::default();
        assert!(!own.masked(None, Some(CLIENT)));
        assert!(!own.masked(Some(STEAM), None));
        assert!(!own.masked(None, None));
        let mut own = OwnApp {
            launch: vec![CLIENT],
            focused: Some(CLIENT),
        };
        assert!(!own.masked(None, Some(STEAM)));
    }

    #[test]
    fn steam_pages_mask_once_the_client_holds_focus() {
        let mut own = OwnApp {
            launch: vec![CLIENT],
            focused: None,
        };
        // Launching: Steam still has the screen, the id is not confirmed yet.
        assert!(!own.masked(Some(STEAM), Some(STEAM)));
        assert_eq!(own.focused, None);
        assert!(!own.masked(Some(CLIENT), Some(CLIENT)));
        assert_eq!(own.focused, Some(CLIENT));
        // QAM, then Settings or Downloads, then back to the stream.
        assert!(own.masked(Some(STEAM), Some(CLIENT)));
        assert!(own.masked(Some(STEAM), Some(STEAM)));
        assert!(!own.masked(Some(CLIENT), Some(CLIENT)));
    }

    #[test]
    fn a_launch_id_gamescope_never_focuses_keeps_the_base_layer_rule() {
        let mut own = OwnApp {
            launch: vec![CLIENT],
            focused: None,
        };
        assert!(!own.masked(Some(42), Some(42)));
        assert!(own.masked(Some(STEAM), Some(42)));
        assert!(!own.masked(Some(42), Some(42)));
        assert_eq!(own.focused, None);
    }

    #[test]
    fn steam_env_ids_decode_to_the_32_bit_appid() {
        assert_eq!(steam_appid("3856846079"), Some(CLIENT));
        let game_id = (u64::from(CLIENT) << 32) | 0x0200_0000;
        assert_eq!(steam_appid(&game_id.to_string()), Some(CLIENT));
        assert_eq!(steam_appid("570"), Some(570));
        assert_eq!(steam_appid("0"), None);
        assert_eq!(steam_appid(""), None);
        assert_eq!(steam_appid("-1"), None);
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
}
