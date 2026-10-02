//! gamescope's clipboard (`design/clipboard-and-file-transfer.md`).
//!
//! gamescope serves no data-control protocol. Its clipboard is one UTF-8 string mirrored across
//! its Xwaylands: when another window takes `CLIPBOARD`, gamescope converts it to `UTF8_STRING`
//! and owns the copy itself. So this backend carries text only, speaks ICCCM selections on the
//! session's Xwayland, and drops the copy gamescope makes of text this backend just served.
//!
//! gamescope starts after the clipboard does, so a thread waits for [`GamescopeXwayland`] and
//! reconnects whenever the session's gamescope is replaced.

use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use punktfunk_core::clipboard::CLIP_FETCH_CAP;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, SelectionNotifyEvent,
    SelectionRequestEvent, Window, WindowClass, SELECTION_NOTIFY_EVENT,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::{DefaultStream, RustConnection};
use x11rb::wrapper::ConnectionExt as _;

use super::{ClipEvent, PasteResponder, WIRE_HTML, WIRE_RTF, WIRE_TEXT};
use crate::GamescopeXwayland;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        TARGETS,
        UTF8_STRING,
        STRING,
        TEXT,
        INCR,
        TEXT_PLAIN: b"text/plain",
        TEXT_PLAIN_UTF8: b"text/plain;charset=utf-8",
        PF_CLIP,
    }
}

impl Atoms {
    /// The targets gamescope itself answers for its text.
    fn text(&self) -> [Atom; 5] {
        [
            self.UTF8_STRING,
            self.TEXT_PLAIN_UTF8,
            self.TEXT_PLAIN,
            self.STRING,
            self.TEXT,
        ]
    }
}

/// Retry pace while the session has no gamescope yet, or after its gamescope went away.
const RECONNECT: Duration = Duration::from_millis(500);
/// gamescope takes `CLIPBOARD` a few milliseconds after a host app does; one offer covers both.
const SETTLE: Duration = Duration::from_millis(150);
/// Upper bound on events another thread's reply read buffered, and on [`SETTLE`]'s lateness.
const POLL_MS: i32 = 50;
/// A local owner answers a conversion at once; a silent one is gone or broken.
const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// One connection to the session's Xwayland.
struct Live {
    conn: RustConnection,
    /// Our selection owner and conversion requestor. Never mapped.
    window: Window,
    atoms: Atoms,
}

#[derive(Default)]
struct Shared {
    live: Mutex<Option<Arc<Live>>>,
    /// A host window owns `CLIPBOARD`.
    foreign: AtomicBool,
    /// The read waiting on our `PF_CLIP` conversion.
    read: Mutex<Option<oneshot::Sender<Result<Vec<u8>>>>>,
}

pub struct GamescopeClipboard {
    shared: Arc<Shared>,
    /// One conversion in flight: they share `PF_CLIP`.
    reads: tokio::sync::Mutex<()>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl GamescopeClipboard {
    pub fn open(xwayland: GamescopeXwayland) -> Result<(Self, UnboundedReceiver<ClipEvent>)> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let shared = Arc::new(Shared::default());
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (shared, stop) = (Arc::clone(&shared), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("punktfunk-clipboard".into())
                .spawn(move || run(&xwayland, &shared, &stop, &tx))
                .context("spawn gamescope clipboard thread")?
        };
        let backend = GamescopeClipboard {
            shared,
            reads: tokio::sync::Mutex::new(()),
            stop,
            thread: Some(thread),
        };
        Ok((backend, rx))
    }

    fn live(&self) -> Option<Arc<Live>> {
        self.shared.live.lock().unwrap().clone()
    }

    pub fn current_wire_mimes(&self) -> Vec<String> {
        if self.shared.foreign.load(Ordering::SeqCst) {
            vec![WIRE_TEXT.to_string()]
        } else {
            Vec::new()
        }
    }

    /// Takes `CLIPBOARD` when the offer has text, which is all gamescope keeps. A rich-only offer
    /// counts: the client derives its plain text.
    pub fn set_offer(&self, wire_mimes: &[String]) -> Result<()> {
        let text = wire_mimes
            .iter()
            .any(|m| matches!(m.as_str(), WIRE_TEXT | WIRE_HTML | WIRE_RTF));
        if !text {
            return self.clear_offer();
        }
        let Some(live) = self.live() else {
            return Ok(());
        };
        live.conn
            .set_selection_owner(live.window, live.atoms.CLIPBOARD, x11rb::CURRENT_TIME)?;
        live.conn.flush().context("flush set_selection_owner")
    }

    /// Lets go of `CLIPBOARD` only while we still own it; gamescope's own copy stays.
    pub fn clear_offer(&self) -> Result<()> {
        let Some(live) = self.live() else {
            return Ok(());
        };
        let owner = live
            .conn
            .get_selection_owner(live.atoms.CLIPBOARD)?
            .reply()?
            .owner;
        if owner == live.window {
            live.conn.set_selection_owner(
                x11rb::NONE,
                live.atoms.CLIPBOARD,
                x11rb::CURRENT_TIME,
            )?;
            live.conn.flush().context("flush clear selection")?;
        }
        Ok(())
    }

    pub async fn read_current(&self, wire_mime: &str) -> Result<Vec<u8>> {
        anyhow::ensure!(
            wire_mime == WIRE_TEXT,
            "gamescope's clipboard holds text only"
        );
        let _one = self.reads.lock().await;
        let live = self.live().context("no gamescope clipboard yet")?;
        let (tx, rx) = oneshot::channel();
        *self.shared.read.lock().unwrap() = Some(tx);
        live.conn.convert_selection(
            live.window,
            live.atoms.CLIPBOARD,
            live.atoms.UTF8_STRING,
            live.atoms.PF_CLIP,
            x11rb::CURRENT_TIME,
        )?;
        live.conn.flush().context("flush convert_selection")?;
        tokio::time::timeout(READ_TIMEOUT, rx)
            .await
            .context("clipboard owner did not answer")?
            .context("gamescope clipboard closed")?
    }
}

impl Drop for GamescopeClipboard {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Connect, serve until the connection drops, and connect again, until `stop`.
fn run(
    xwayland: &GamescopeXwayland,
    shared: &Shared,
    stop: &AtomicBool,
    tx: &UnboundedSender<ClipEvent>,
) {
    while !stop.load(Ordering::SeqCst) {
        let Some(dpy) = xwayland.displays().into_iter().next() else {
            std::thread::sleep(RECONNECT);
            continue;
        };
        let live = match Live::connect(&dpy) {
            Ok(live) => Arc::new(live),
            Err(e) => {
                tracing::debug!(%dpy, error = format!("{e:#}"), "gamescope clipboard: no connection");
                std::thread::sleep(RECONNECT);
                continue;
            }
        };
        tracing::info!(%dpy, "gamescope clipboard: following the session's Xwayland");
        *shared.live.lock().unwrap() = Some(Arc::clone(&live));
        let served = serve(&live, shared, stop, tx);
        *shared.live.lock().unwrap() = None;
        // Not an empty offer: the client would clear its own clipboard for a gamescope restart.
        shared.foreign.store(false, Ordering::SeqCst);
        if let Some(read) = shared.read.lock().unwrap().take() {
            let _ = read.send(Err(anyhow::anyhow!("gamescope went away")));
        }
        if let Err(e) = served {
            tracing::debug!(%dpy, error = format!("{e:#}"), "gamescope clipboard: connection lost");
            std::thread::sleep(RECONNECT);
        }
    }
    let _ = tx.send(ClipEvent::Closed);
}

impl Live {
    fn connect(display: &str) -> Result<Live> {
        let (conn, screen) = connect_unauthenticated(display)?;
        conn.xfixes_query_version(5, 0)?.reply().context("XFixes")?;
        let root = conn.setup().roots.get(screen).context("no X screen")?.root;
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )?
        .check()
        .context("create clipboard window")?;
        let atoms = Atoms::new(&conn)?.reply().context("intern atoms")?;
        conn.xfixes_select_selection_input(
            root,
            atoms.CLIPBOARD,
            xfixes::SelectionEventMask::SET_SELECTION_OWNER
                | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        )?
        .check()
        .context("select CLIPBOARD owner changes")?;
        Ok(Live {
            conn,
            window,
            atoms,
        })
    }
}

/// gamescope starts Xwayland without `-auth`, so an empty token connects. Never `setenv`
/// `XAUTHORITY` to reach it: glibc rewrites `environ` and `getenv` takes no lock.
fn connect_unauthenticated(display: &str) -> Result<(RustConnection, usize)> {
    let parsed = x11rb::reexports::x11rb_protocol::parse_display::parse_display(Some(display))
        .context("parse display")?;
    let screen = usize::from(parsed.screen);
    let (stream, _) = parsed
        .connect_instruction()
        .find_map(|addr| DefaultStream::connect(&addr).ok())
        .context("connect")?;
    let conn =
        RustConnection::connect_to_stream_with_auth_info(stream, screen, Vec::new(), Vec::new())
            .context("X setup")?;
    Ok((conn, screen))
}

/// Which `CLIPBOARD` owner changes become offers.
#[derive(Default)]
struct Owners {
    /// Windows we answered with text since we took `CLIPBOARD`. One of them taking it next is
    /// gamescope keeping the client's copy, not a host copy.
    served: Vec<Window>,
    announce_at: Option<Instant>,
}

impl Owners {
    fn changed(&mut self, owner: Window, ours: Window, foreign: &AtomicBool) {
        let host = owner != x11rb::NONE && owner != ours;
        foreign.store(host, Ordering::SeqCst);
        let echo = self.served.contains(&owner);
        self.served.clear();
        if host && !echo {
            self.announce_at.get_or_insert(Instant::now() + SETTLE);
        } else {
            self.announce_at = None;
        }
    }

    /// `true` once a host copy has settled.
    fn due(&mut self, now: Instant) -> bool {
        let due = self.announce_at.is_some_and(|at| at <= now);
        if due {
            self.announce_at = None;
        }
        due
    }
}

fn serve(
    live: &Arc<Live>,
    shared: &Shared,
    stop: &AtomicBool,
    tx: &UnboundedSender<ClipEvent>,
) -> Result<()> {
    let (conn, atoms) = (&live.conn, &live.atoms);
    let mut owners = Owners::default();
    let owner = conn.get_selection_owner(atoms.CLIPBOARD)?.reply()?.owner;
    owners.changed(owner, live.window, &shared.foreign);
    while !stop.load(Ordering::SeqCst) {
        while let Some(event) = conn.poll_for_event()? {
            match event {
                Event::XfixesSelectionNotify(ev) if ev.selection == atoms.CLIPBOARD => {
                    owners.changed(ev.owner, live.window, &shared.foreign);
                }
                Event::SelectionRequest(req) => {
                    let requestor = req.requestor;
                    if answer(live, req, tx)? {
                        owners.served.push(requestor);
                    }
                }
                Event::SelectionNotify(ev) if ev.requestor == live.window => {
                    if let Some(read) = shared.read.lock().unwrap().take() {
                        let _ = read.send(read_property(live, ev.property));
                    }
                }
                _ => {}
            }
        }
        if owners.due(Instant::now()) && shared.foreign.load(Ordering::SeqCst) {
            let _ = tx.send(ClipEvent::Selection {
                mimes: vec![WIRE_TEXT.to_string()],
            });
        }
        conn.flush()?;
        wait_readable(conn)?;
    }
    Ok(())
}

/// Answer a request for our selection. `true` when it took text, which the client sends later.
fn answer(
    live: &Arc<Live>,
    req: SelectionRequestEvent,
    tx: &UnboundedSender<ClipEvent>,
) -> Result<bool> {
    let atoms = &live.atoms;
    // ICCCM: a NONE property comes from an obsolete client and means the target.
    let property = match req.property {
        x11rb::NONE => req.target,
        p => p,
    };
    // Our own read of a selection we hold would fetch from the client it came from.
    if req.selection != atoms.CLIPBOARD || req.requestor == live.window {
        notify(&live.conn, req, x11rb::NONE)?;
        return Ok(false);
    }
    if req.target == atoms.TARGETS {
        let mut list = vec![atoms.TARGETS];
        list.extend(atoms.text());
        live.conn.change_property32(
            PropMode::REPLACE,
            req.requestor,
            property,
            AtomEnum::ATOM,
            &list,
        )?;
        notify(&live.conn, req, property)?;
        return Ok(false);
    }
    if !atoms.text().contains(&req.target) {
        notify(&live.conn, req, x11rb::NONE)?;
        return Ok(false);
    }
    let (paste, bytes) = oneshot::channel();
    let _ = tx.send(ClipEvent::Paste {
        mime: WIRE_TEXT.to_string(),
        responder: PasteResponder::Channel(paste),
    });
    // The client answers over the network; the event loop keeps serving meanwhile.
    let live = Arc::clone(live);
    std::thread::spawn(move || {
        let data = bytes.blocking_recv().unwrap_or_default();
        // A property is one request; the 64 bytes are its header with room to spare.
        let fits = data.len() + 64 <= live.conn.maximum_request_bytes();
        let stored = fits
            && live
                .conn
                .change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    property,
                    req.target,
                    &data,
                )
                .is_ok();
        let _ = notify(&live.conn, req, if stored { property } else { x11rb::NONE });
        let _ = live.conn.flush();
    });
    Ok(true)
}

/// Tell `req`'s requestor where its data is; `NONE` refuses.
fn notify(conn: &RustConnection, req: SelectionRequestEvent, property: Atom) -> Result<()> {
    let event = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property,
    };
    conn.send_event(false, req.requestor, EventMask::NO_EVENT, event)?;
    Ok(())
}

/// The text a conversion left in `property` on our window.
fn read_property(live: &Live, property: Atom) -> Result<Vec<u8>> {
    anyhow::ensure!(property != x11rb::NONE, "the clipboard owner refused text");
    let words = u32::try_from(CLIP_FETCH_CAP / 4 + 1).unwrap_or(u32::MAX);
    let reply = live
        .conn
        .get_property(true, live.window, property, AtomEnum::ANY, 0, words)?
        .reply()?;
    // No INCR: gamescope never sends it, and text that needs it is past gamescope's own read.
    anyhow::ensure!(
        reply.type_ != live.atoms.INCR,
        "the clipboard owner sent its text in increments"
    );
    anyhow::ensure!(
        reply.bytes_after == 0 && reply.value.len() <= CLIP_FETCH_CAP,
        "clipboard selection exceeds the {CLIP_FETCH_CAP}-byte transfer cap"
    );
    Ok(reply.value)
}

/// Wait up to [`POLL_MS`] for the socket to turn readable. An interrupted wait is a short one.
fn wait_readable(conn: &RustConnection) -> Result<()> {
    let mut pfd = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `pfd` is one valid pollfd that outlives the call, which reads and writes only it.
    let rc = unsafe { libc::poll(&mut pfd, 1, POLL_MS) };
    if rc < 0 {
        let err = std::io::Error::last_os_error();
        anyhow::ensure!(err.kind() == std::io::ErrorKind::Interrupted, err);
    }
    anyhow::ensure!(
        pfd.revents & (libc::POLLHUP | libc::POLLERR) == 0,
        "Xwayland closed the connection"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OURS: Window = 7;

    #[test]
    fn a_host_copy_is_offered_once_after_gamescope_takes_it() {
        let foreign = AtomicBool::new(false);
        let mut owners = Owners::default();
        owners.changed(40, OURS, &foreign); // the game copies
        owners.changed(41, OURS, &foreign); // gamescope takes its copy
        assert!(foreign.load(Ordering::SeqCst));
        assert!(!owners.due(Instant::now()));
        let settled = Instant::now() + SETTLE;
        assert!(owners.due(settled));
        assert!(!owners.due(settled), "one offer per settled copy");
    }

    #[test]
    fn gamescope_keeping_the_clients_copy_is_no_host_copy() {
        let foreign = AtomicBool::new(false);
        let mut owners = Owners::default();
        owners.changed(OURS, OURS, &foreign); // we took the client's offer
        assert!(!foreign.load(Ordering::SeqCst));
        owners.served.push(41); // gamescope read our text
        owners.changed(41, OURS, &foreign); // and owns its copy
        assert!(foreign.load(Ordering::SeqCst), "the text is readable");
        assert!(
            !owners.due(Instant::now() + SETTLE),
            "but it is the client's own"
        );
        owners.changed(42, OURS, &foreign); // a later host copy
        assert!(owners.due(Instant::now() + SETTLE));
    }

    #[test]
    fn a_cleared_clipboard_offers_nothing() {
        let foreign = AtomicBool::new(true);
        let mut owners = Owners::default();
        owners.changed(x11rb::NONE, OURS, &foreign);
        assert!(!foreign.load(Ordering::SeqCst));
        assert!(!owners.due(Instant::now() + SETTLE));
    }
}
