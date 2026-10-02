//! `ext-data-control-v1` clipboard backend (`design/clipboard-and-file-transfer.md`).
//!
//! A dedicated thread owns the `wayland-client` [`EventQueue`] and dispatches
//! selection + paste events onto a channel. Installing a lazy source and
//! `receive()`-ing the host selection happen on the session thread via shared
//! `Send + Sync` proxies; only dispatch is single-threaded (wayland-client
//! contract).
//!
//! `open` binds the session's `WAYLAND_DISPLAY` (env already applied by
//! `vdisplay::apply_session_env`). Missing protocol is `BackendUnavailable`.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};

use super::{ClipEvent, PasteResponder};

/// Dispatch thread writes; session thread reads for `receive()`.
struct CurrentSelection {
    offer: ExtDataControlOfferV1,
    mimes: Vec<String>,
}

/// Bind roundtrip fills `mgr` + `seat` before the loop starts.
struct State {
    mgr: Option<ExtDataControlManagerV1>,
    seat: Option<WlSeat>,
    /// MIME lists land here; `selection` promotes one.
    pending: HashMap<ObjectId, Vec<String>>,
    current: Arc<Mutex<Option<CurrentSelection>>>,
    /// Own `set_selection` echoes still to drop. Session bumps; dispatch is the only decrementer.
    /// A counter, not a bool: back-to-back offers would leak a self-echo through a flag.
    suppress_echoes: Arc<AtomicU32>,
    /// Our source that holds the selection, until the compositor cancels it.
    owner: Arc<Mutex<Option<ObjectId>>>,
    tx: tokio::sync::mpsc::UnboundedSender<ClipEvent>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
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
            match interface.as_str() {
                "ext_data_control_manager_v1" => {
                    state.mgr = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// Manager + seat emit nothing we consume.
impl Dispatch<ExtDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ExtDataControlManagerV1,
        _: <ExtDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _dev: &ExtDataControlDeviceV1,
        event: ext_data_control_device_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_device_v1::Event;
        match event {
            Event::DataOffer { id } => {
                state.pending.insert(id.id(), Vec::new());
            }
            Event::Selection { id } => {
                // `Ok` = a pending self-echo; drop it. Dispatch is the only decrementer.
                let suppressed = state
                    .suppress_echoes
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |c| c.checked_sub(1))
                    .is_ok();
                // Every offer this drops is destroyed: the compositor keeps one alive until then.
                match id {
                    Some(offer) => {
                        let mimes = state.pending.remove(&offer.id()).unwrap_or_default();
                        if suppressed {
                            offer.destroy();
                            return;
                        }
                        let wire = super::offer_wire_mimes(&mimes)
                            .into_iter()
                            .map(str::to_string)
                            .collect::<Vec<_>>();
                        let next = CurrentSelection { offer, mimes };
                        if let Some(old) = state.current.lock().unwrap().replace(next) {
                            old.offer.destroy();
                        }
                        let _ = state.tx.send(ClipEvent::Selection { mimes: wire });
                    }
                    None => {
                        if let Some(old) = state.current.lock().unwrap().take() {
                            old.offer.destroy();
                        }
                        if !suppressed {
                            let _ = state.tx.send(ClipEvent::Selection { mimes: Vec::new() });
                        }
                    }
                }
            }
            Event::PrimarySelection { id: Some(offer) } => {
                state.pending.remove(&offer.id());
                offer.destroy();
            }
            Event::Finished => {
                let _ = state.tx.send(ClipEvent::Closed);
            }
            // Primary selection is out of scope.
            _ => {}
        }
    }

    event_created_child!(State, ExtDataControlDeviceV1, [
        ext_data_control_device_v1::EVT_DATA_OFFER_OPCODE => (ExtDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ExtDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ExtDataControlOfferV1,
        event: ext_data_control_offer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let ext_data_control_offer_v1::Event::Offer { mime_type } = event {
            if let Some(list) = state.pending.get_mut(&offer.id()) {
                list.push(mime_type);
            }
        }
    }
}

impl Dispatch<ExtDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        src: &ExtDataControlSourceV1,
        event: ext_data_control_source_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use ext_data_control_source_v1::Event;
        match event {
            Event::Send { mime_type, fd } => match super::wayland_to_wire(&mime_type) {
                Some(wire) => {
                    let _ = state.tx.send(ClipEvent::Paste {
                        mime: wire.to_string(),
                        responder: PasteResponder::Fd(fd),
                    });
                }
                // Unknown MIME: closing the fd is an empty paste, not a hang.
                None => drop(fd),
            },
            // Another client took the selection. By id: a replaced source's late cancel is not ours.
            Event::Cancelled => {
                let mut owner = state.owner.lock().unwrap();
                if owner.as_ref() == Some(&src.id()) {
                    *owner = None;
                }
            }
            _ => {}
        }
    }
}

pub struct ClipboardBackend {
    conn: Connection,
    mgr: ExtDataControlManagerV1,
    device: ExtDataControlDeviceV1,
    qh: QueueHandle<State>,
    current: Arc<Mutex<Option<CurrentSelection>>>,
    suppress_echoes: Arc<AtomicU32>,
    owner: Arc<Mutex<Option<ObjectId>>>,
    active_source: Mutex<Option<ExtDataControlSourceV1>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipboardBackend {
    /// Session env must already be applied (`vdisplay::apply_session_env`).
    /// Missing protocol: caller reports `BackendUnavailable`.
    pub fn open() -> Result<(
        ClipboardBackend,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        let conn = Connection::connect_to_env()
            .context("connect to Wayland for clipboard (WAYLAND_DISPLAY/XDG_RUNTIME_DIR set?)")?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let current = Arc::new(Mutex::new(None));
        let suppress_echoes = Arc::new(AtomicU32::new(0));
        let owner = Arc::new(Mutex::new(None));
        let mut state = State {
            mgr: None,
            seat: None,
            pending: HashMap::new(),
            current: current.clone(),
            suppress_echoes: suppress_echoes.clone(),
            owner: owner.clone(),
            tx,
        };
        queue
            .roundtrip(&mut state)
            .context("Wayland registry roundtrip")?;

        let mgr = state
            .mgr
            .clone()
            .context("compositor lacks ext_data_control_manager_v1")?;
        let seat = state
            .seat
            .clone()
            .context("compositor advertised no wl_seat")?;
        let device = mgr.get_data_device(&seat, &qh, ());
        // Device bind delivers the current selection; the session announces it.
        queue
            .roundtrip(&mut state)
            .context("Wayland get_data_device roundtrip")?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let conn = conn.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("punktfunk-clipboard".into())
                .spawn(move || dispatch_loop(conn, queue, state, stop))
                .context("spawn clipboard dispatch thread")?
        };

        Ok((
            ClipboardBackend {
                conn,
                mgr,
                device,
                qh,
                current,
                suppress_echoes,
                owner,
                active_source: Mutex::new(None),
                stop,
                thread: Some(thread),
            },
            rx,
        ))
    }

    /// Bytes stay with the client until a host paste emits [`ClipEvent::Paste`].
    pub fn set_offer(&self, wire_mimes: &[String]) -> Result<()> {
        let wl_mimes = super::wayland_offers_for(wire_mimes);
        if wl_mimes.is_empty() {
            return self.clear_offer();
        }
        let src = self.mgr.create_data_source(&self.qh, ());
        for m in &wl_mimes {
            src.offer(m.clone());
        }
        self.suppress_echoes.fetch_add(1, Ordering::SeqCst);
        *self.owner.lock().unwrap() = Some(src.id());
        self.device.set_selection(Some(&src));
        self.conn.flush().context("flush set_selection")?;
        let mut slot = self.active_source.lock().unwrap();
        if let Some(old) = slot.take() {
            old.destroy();
        }
        *slot = Some(src);
        Ok(())
    }

    /// Unsets the selection only while our source still holds it: a null `set_selection`
    /// clears whatever does, including a host copy made since.
    pub fn clear_offer(&self) -> Result<()> {
        let mut slot = self.active_source.lock().unwrap();
        if let Some(old) = slot.take() {
            let ours = self.owner.lock().unwrap().take() == Some(old.id());
            if ours {
                self.suppress_echoes.fetch_add(1, Ordering::SeqCst);
                self.device.set_selection(None);
            }
            old.destroy();
            self.conn.flush().context("flush clear selection")?;
        }
        Ok(())
    }

    pub fn current_wire_mimes(&self) -> Vec<String> {
        match self.current.lock().unwrap().as_ref() {
            Some(sel) => super::offer_wire_mimes(&sel.mimes)
                .into_iter()
                .map(str::to_string)
                .collect(),
            None => Vec::new(),
        }
    }

    /// Blocks on the pipe until the source finishes; call from `spawn_blocking`.
    pub fn read_current(&self, wire_mime: &str) -> Result<Vec<u8>> {
        let (offer, wl_mime) = {
            let cur = self.current.lock().unwrap();
            let sel = cur.as_ref().context("no current host selection")?;
            let wl = super::pick_wayland_mime(wire_mime, &sel.mimes)
                .context("format not offered by the host clipboard")?;
            (sel.offer.clone(), wl)
        };
        let (reader, writer) = std::io::pipe().context("pipe")?;
        offer.receive(wl_mime, writer.as_fd());
        self.conn.flush().context("flush receive")?;
        // Drop our write end so the pipe EOFs when the source closes its dup.
        drop(writer);
        super::read_capped(reader).context("read clipboard transfer")
    }
}

impl Drop for ClipboardBackend {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Poll the Wayland socket (200 ms) so `stop` is seen promptly.
fn dispatch_loop(
    conn: Connection,
    mut queue: wayland_client::EventQueue<State>,
    mut state: State,
    stop: Arc<AtomicBool>,
) {
    while !stop.load(Ordering::SeqCst) {
        if queue.dispatch_pending(&mut state).is_err() {
            break;
        }
        if conn.flush().is_err() {
            break;
        }
        let Some(guard) = conn.prepare_read() else {
            // `prepare_read` lost the race; dispatch the already-queued events.
            continue;
        };
        let raw_fd = guard.connection_fd().as_raw_fd();
        let mut pfd = libc::pollfd {
            fd: raw_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a single valid pollfd; `poll` reads/writes exactly it for 200 ms.
        let rc = unsafe { libc::poll(&mut pfd, 1, 200) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            drop(guard);
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue; // EINTR — recheck stop, retry
            }
            break;
        }
        if rc == 0 {
            drop(guard); // timeout — recheck stop
            continue;
        }
        if pfd.revents & libc::POLLIN != 0 {
            if guard.read().is_err() {
                break;
            }
        } else {
            drop(guard); // POLLHUP / POLLERR — connection gone
            break;
        }
    }
    let _ = state.tx.send(ClipEvent::Closed);
}

/// Ignored live tests. Needs `wl-clipboard` and a `data-control` compositor.
///
/// ```text
/// WAYLAND_DISPLAY=wayland-1 cargo test -p punktfunk-host --bin punktfunk-host \
///     -- --ignored --nocapture clipboard::wayland::live
/// ```
///
/// `open()` missing a backend skips; `--ignored` on a headless runner is a no-op.
#[cfg(test)]
mod live {
    use super::*;
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Sync `try_recv` until `pred` or `timeout`. No tokio runtime on this thread.
    fn wait_event(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
        timeout: Duration,
        mut pred: impl FnMut(&ClipEvent) -> bool,
    ) -> Option<ClipEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            match rx.try_recv() {
                Ok(ev) if pred(&ev) => return Some(ev),
                Ok(_) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            }
        }
    }

    /// `wl-copy` forks a server that holds the selection; the child we wait on is the parent.
    fn wl_copy(bytes: &[u8], mime: &str) {
        let mut child = Command::new("wl-copy")
            .arg("--type")
            .arg(mime)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn wl-copy");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(bytes)
            .expect("write to wl-copy");
        let _ = child.wait(); // foreground exits; the fork keeps serving
    }

    fn open_or_skip() -> Option<(
        ClipboardBackend,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        if Command::new("wl-copy").arg("--version").output().is_err() {
            eprintln!("SKIP: wl-clipboard not installed");
            return None;
        }
        match ClipboardBackend::open() {
            Ok(v) => Some(v),
            Err(e) => {
                eprintln!("SKIP: no data-control backend on this compositor: {e:#}");
                None
            }
        }
    }

    #[test]
    #[ignore = "needs a live data-control compositor (WAYLAND_DISPLAY)"]
    fn live_host_copy_is_readable() {
        let Some((backend, mut rx)) = open_or_skip() else {
            return;
        };

        wl_copy(b"hello-from-host-app", "text/plain;charset=utf-8");
        let ev = wait_event(&mut rx, Duration::from_secs(3), |e| {
            matches!(e, ClipEvent::Selection { mimes } if mimes.iter().any(|m| m == super::super::WIRE_TEXT))
        })
        .expect("Selection event carrying text after wl-copy");
        assert!(matches!(ev, ClipEvent::Selection { .. }));
        assert_eq!(
            backend.read_current(super::super::WIRE_TEXT).unwrap(),
            b"hello-from-host-app"
        );

        // Tagged `image/png`; the bytes need not be a valid PNG.
        let png = b"\x89PNG\r\n\x1a\n-fake-but-tagged-image/png";
        wl_copy(png, "image/png");
        wait_event(&mut rx, Duration::from_secs(3), |e| {
            matches!(e, ClipEvent::Selection { mimes } if mimes.iter().any(|m| m == super::super::WIRE_PNG))
        })
        .expect("Selection event carrying image/png");
        assert_eq!(backend.read_current(super::super::WIRE_PNG).unwrap(), png);
    }

    #[test]
    #[ignore = "needs a live data-control compositor (WAYLAND_DISPLAY)"]
    fn live_set_offer_is_pasteable() {
        let Some((backend, mut rx)) = open_or_skip() else {
            return;
        };

        backend
            .set_offer(&[super::super::WIRE_TEXT.to_string()])
            .expect("install offer");

        let child = Command::new("wl-paste")
            .arg("-n")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn wl-paste");

        let paste = wait_event(&mut rx, Duration::from_secs(3), |e| {
            matches!(e, ClipEvent::Paste { .. })
        })
        .expect("Paste event after wl-paste reads our offer");
        match paste {
            ClipEvent::Paste { mime, responder } => {
                assert_eq!(
                    mime,
                    super::super::WIRE_TEXT,
                    "paste requested the text format"
                );
                match responder {
                    PasteResponder::Fd(fd) => {
                        super::super::fulfill_paste(fd, b"served-by-punktfunk").expect("fulfill");
                    }
                    PasteResponder::Channel(_) => panic!("data-control paste must carry an fd"),
                }
            }
            _ => unreachable!(),
        }

        let out = child.wait_with_output().expect("wl-paste output");
        assert_eq!(out.stdout, b"served-by-punktfunk");
    }
}
