//! libei input injection — the portable EI-sender path.
//!
//! Reach an EIS server through [`EiSource`]: the xdg RemoteDesktop portal, Mutter's
//! direct RemoteDesktop API, or a gamescope `LIBEI_SOCKET` path relayed in a file.
//! `reis` drives the connection as an EI sender: bind seat capabilities, then per
//! device `start_emulating` → emit → `frame`.
//!
//! The portal/Mutter session and the EIS connection must stay alive, and the event
//! stream must be polled (resume/pause/ping). The worker parks on the shared portal
//! runtime (`pf_portal`); the control thread only enqueues via
//! [`LibeiInjector::inject`].
//!
//! Keyboard codes are Linux evdev. The compositor supplies the keymap, so there is
//! no keymap to upload and no modifier mask to serialize — modifier keys arrive as
//! normal key events.

use super::{gs_button_to_evdev, vk_to_evdev, InputInjector};
use crate::held::HeldInput;
use crate::scroll::{mutter_axis_calls, ScrollBackend, ScrollMapper, ScrollOp};
use crate::AbsoluteAnchor;
use anyhow::{anyhow, Result};
use ashpd::desktop::{
    remote_desktop::{
        ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions,
    },
    CreateSessionOptions, PersistMode,
};
use ashpd::zbus;
use futures_util::StreamExt;
use punktfunk_core::input::{InputEvent, InputKind};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

#[derive(Clone, Debug)]
pub enum EiSource {
    /// xdg `RemoteDesktop` via `ashpd`. A pre-seeded grant skips the approval dialog.
    Portal,
    /// Mutter's direct `org.gnome.Mutter.RemoteDesktop` EIS. The xdg portal's `Start()`
    /// waits for an "Allow remote control?" click a headless host cannot answer.
    MutterEis,
    /// File holding the EIS socket path (gamescope's relayed `LIBEI_SOCKET`). Polled
    /// until the compositor is listening.
    SocketPathFile(std::path::PathBuf),
}

pub struct LibeiInjector {
    tx: UnboundedSender<InputEvent>,
}

impl LibeiInjector {
    pub fn open() -> Result<Self> {
        Self::open_with(EiSource::Portal)
    }

    pub fn open_with(source: EiSource) -> Result<Self> {
        let (tx, rx) = unbounded_channel::<InputEvent>();
        std::thread::Builder::new()
            .name("punktfunk-libei".into())
            .spawn(move || worker(rx, source))
            .map_err(|e| anyhow!("spawn libei worker thread: {e}"))?;
        // Handshake stays off the control thread: a slow or denied portal would freeze
        // the ENet stream. Events enqueue until a device resumes; a few early ones drop.
        Ok(Self { tx })
    }
}

impl InputInjector for LibeiInjector {
    fn inject(&mut self, event: &InputEvent) -> Result<()> {
        self.tx
            .send(*event)
            .map_err(|_| anyhow!("libei worker thread has exited"))
    }
}

fn worker(rx: UnboundedReceiver<InputEvent>, source: EiSource) {
    // Shared, never dropped: a per-worker runtime took ashpd's process-global
    // D-Bus connection down with it, and the next session's portal open hung.
    let rt = match pf_portal::portal_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::error!(error = %e, "libei: no portal runtime");
            return;
        }
    };
    rt.block_on(session_main(rx, source));
}

async fn session_main(mut rx: UnboundedReceiver<InputEvent>, source: EiSource) {
    // Closing the portal session (end of fn) closes EIS. Bound setup so an unanswered
    // approval dialog cannot hang the worker.
    let gamescope = matches!(source, EiSource::SocketPathFile(_));
    let (keepalive, context, mut events, output_hint, mutter) = match tokio::time::timeout(
        Duration::from_secs(30),
        connect(source),
    )
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            tracing::error!(error = %format!("{e:#}"), "libei: portal/EIS setup failed");
            return;
        }
        Err(_) => {
            tracing::error!(
                    "libei: EIS setup timed out (headless approval needed / kde-authorized grant not seeded / gamescope socket never appeared)"
                );
            return;
        }
    };
    tracing::info!("libei: EIS connected — awaiting devices");

    let mut state = EiState::new();
    state.output_hint = output_hint;
    state.gamescope = gamescope;
    state.scroll = ScrollMapper::new(if gamescope {
        ScrollBackend::Gamescope
    } else if mutter.is_some() {
        ScrollBackend::Mutter
    } else {
        ScrollBackend::Libei
    });
    state.mutter_axis = mutter.map(mutter_axis_sender);
    // 5s: a live EIS resumes a device right after handshake. Past that the socket
    // was stale — exit so InjectorService reopens instead of swallowing every event.
    let resume_deadline = tokio::time::sleep(Duration::from_secs(5));
    tokio::pin!(resume_deadline);
    let mut resumed_once = false;
    loop {
        let scroll_due = state.scroll.stop_due();
        tokio::select! {
            ei = events.next() => match ei {
                Some(Ok(ev)) => {
                    state.handle_ei(ev, &context);
                    if !resumed_once && state.devices.iter().any(|d| d.resumed) {
                        resumed_once = true;
                    }
                }
                Some(Err(e)) => { tracing::warn!(error = %e, "libei: event stream error"); break; }
                None => { tracing::info!("libei: EIS disconnected"); break; }
            },
            msg = rx.recv() => match msg {
                Some(input) => state.inject(&input, &context),
                None => { tracing::info!("libei: injector closed — ending session"); break; }
            },
            // A held finger stop whose momentum never came goes out now.
            _ = tokio::time::sleep_until(scroll_due.unwrap_or_else(Instant::now).into()),
                if scroll_due.is_some() =>
            {
                let ops = state.scroll.flush_due(Instant::now());
                state.exec_scroll(ops, &context);
            }
            _ = &mut resume_deadline, if !resumed_once => {
                tracing::warn!(
                    "libei: no input device resumed within 5s of connecting — treating the EIS \
                     connection as dead and reopening (stale or half-ready compositor socket)"
                );
                break;
            }
        }
    }
    // Mutter keeps the implicit grab of a destroyed device's held button until the
    // focused app restarts. Release before the EIS connection (and its devices) go.
    state.release_all(&context);
    keepalive.close().await;
}

/// What holds the EIS server's session open, and how it ends. The portal session
/// lives on ashpd's process-global connection, which outlives this worker, so only
/// `Session.Close` ends it; Mutter's rides a private connection that ends with the
/// value; the relayed socket has nothing to close.
enum Keepalive {
    Portal(ashpd::desktop::Session<RemoteDesktop>),
    Mutter(Box<dyn Send>),
    Socket,
}

impl Keepalive {
    /// Ends the session. The portal's `Session.Close` is bounded by `pf_portal::close_session`
    /// so a wedged portal cannot hang the worker's exit.
    async fn close(self) {
        match self {
            Keepalive::Portal(session) => pf_portal::close_session(session.close()).await,
            Keepalive::Mutter(conn) => drop(conn),
            Keepalive::Socket => {}
        }
    }
}

/// Connect-step return. The keep-alive outlives the EIS connection and is closed
/// after it (see [`Keepalive`]).
type Connected = (
    Keepalive,
    ei::Context,
    reis::tokio::EiConvertEventStream,
    // Relay-file "WxH" scale target when EIS advertises a degenerate region
    // (gamescope). `None` on portal/Mutter, whose regions are real.
    Option<(u32, u32)>,
    // Mutter's RemoteDesktop session, which scroll goes through ([`mutter_axis_sender`]).
    Option<zbus::Proxy<'static>>,
);

async fn connect(source: EiSource) -> Result<Connected> {
    let (keepalive, stream, output_hint, mutter) = match source {
        EiSource::Portal => {
            let (_rd, session, fd) = connect_portal().await?;
            (Keepalive::Portal(session), UnixStream::from(fd), None, None)
        }
        EiSource::MutterEis => {
            let (keepalive, session, fd) = connect_mutter().await?;
            let stream = UnixStream::from(fd);
            (Keepalive::Mutter(keepalive), stream, None, Some(session))
        }
        EiSource::SocketPathFile(file) => {
            let (stream, hint) = connect_socket_file(&file).await?;
            (Keepalive::Socket, stream, hint, None)
        }
    };
    let context = ei::Context::new(stream).map_err(|e| anyhow!("reis EI context: {e}"))?;
    // `UnixStream::connect` succeeds as soon as the path exists; a stale gamescope
    // socket never completes the EI handshake. Bound so InjectorService can reopen.
    let (_conn, events) = tokio::time::timeout(
        Duration::from_secs(8),
        context.handshake_tokio("punktfunk-host", ei::handshake::ContextType::Sender),
    )
    .await
    .map_err(|_| {
        anyhow!("EI handshake timed out (EIS server not responding — stale/half-ready socket?)")
    })?
    .map_err(|e| anyhow!("EI handshake: {e}"))?;
    Ok((keepalive, context, events, output_hint, mutter))
}

/// One task sends every `NotifyPointerAxis` in order: a stop must not pass the
/// deltas before it. Mutter accepts the call only from the session's own
/// connection, which `session` rides. Ends when the sender drops.
fn mutter_axis_sender(session: zbus::Proxy<'static>) -> UnboundedSender<(f64, f64, u32)> {
    let (tx, mut rx) = unbounded_channel::<(f64, f64, u32)>();
    tokio::spawn(async move {
        // Mutter makes the session's pointer on first use and re-enters the focused
        // surface while it does; a scroll in that gap is lost. Make it now.
        let _ = session
            .call_noreply("NotifyPointerMotionRelative", &(0.0f64, 0.0f64))
            .await;
        let mut warned = false;
        while let Some(call) = rx.recv().await {
            if let Err(e) = session.call_noreply("NotifyPointerAxis", &call).await {
                if !std::mem::replace(&mut warned, true) {
                    tracing::warn!(error = %e, "libei: scroll did not reach Mutter's NotifyPointerAxis");
                }
            }
        }
    });
    tx
}

async fn connect_portal() -> Result<(
    RemoteDesktop,
    ashpd::desktop::Session<RemoteDesktop>,
    std::os::fd::OwnedFd,
)> {
    let rd = RemoteDesktop::new()
        .await
        .map_err(|e| anyhow!("open RemoteDesktop portal (is xdg-desktop-portal-kde/gnome running and XDG_CURRENT_DESKTOP set?): {e}"))?;
    let session = rd
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| anyhow!("create RemoteDesktop session: {e}"))?;
    rd.select_devices(
        &session,
        SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer | DeviceType::Touchscreen)
            .set_persist_mode(PersistMode::DoNot),
    )
    .await
    .map_err(|e| anyhow!("select_devices: {e}"))?
    .response()
    .map_err(|e| anyhow!("select_devices response: {e}"))?;
    let started = rd
        .start(&session, None, StartOptions::default())
        .await
        .map_err(|e| anyhow!("start RemoteDesktop session: {e}"))?;
    let granted = started
        .response()
        .map_err(|e| anyhow!("RemoteDesktop start denied: {e}"))?;
    tracing::info!(devices = ?granted.devices(), "libei: portal granted devices");

    let fd = rd
        .connect_to_eis(&session, ConnectToEISOptions::default())
        .await
        .map_err(|e| anyhow!("connect_to_eis (RemoteDesktop portal version < 2?): {e}"))?;
    Ok((rd, session, fd))
}

/// EIS fd from Mutter's direct `org.gnome.Mutter.RemoteDesktop` (`CreateSession` →
/// `Start` → `ConnectToEIS`). No portal approval. Keep-alive owns the D-Bus
/// connection + session; dropping it, and the session proxy returned beside it,
/// tears the Mutter session down and closes EIS.
async fn connect_mutter() -> Result<(Box<dyn Send>, zbus::Proxy<'static>, std::os::fd::OwnedFd)> {
    use zbus::zvariant::{OwnedObjectPath, Value};
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| anyhow!("connect session D-Bus (Mutter RemoteDesktop): {e}"))?;
    let rd = zbus::Proxy::new(
        &conn,
        "org.gnome.Mutter.RemoteDesktop",
        "/org/gnome/Mutter/RemoteDesktop",
        "org.gnome.Mutter.RemoteDesktop",
    )
    .await
    .map_err(|e| anyhow!("Mutter RemoteDesktop proxy (is gnome-shell running?): {e}"))?;
    let session_path: OwnedObjectPath = rd
        .call("CreateSession", &())
        .await
        .map_err(|e| anyhow!("Mutter RemoteDesktop.CreateSession: {e}"))?;
    let session = zbus::Proxy::new(
        &conn,
        "org.gnome.Mutter.RemoteDesktop",
        session_path,
        "org.gnome.Mutter.RemoteDesktop.Session",
    )
    .await
    .map_err(|e| anyhow!("Mutter RemoteDesktop.Session proxy: {e}"))?;
    session
        .call_method("Start", &())
        .await
        .map_err(|e| anyhow!("Mutter RemoteDesktop.Session.Start: {e}"))?;
    let options: HashMap<&str, Value> = HashMap::new();
    let fd: zbus::zvariant::OwnedFd = session
        .call("ConnectToEIS", &(options,))
        .await
        .map_err(|e| anyhow!("Mutter RemoteDesktop.Session.ConnectToEIS: {e}"))?;
    tracing::info!("libei: connected to Mutter's direct RemoteDesktop EIS (no portal approval)");
    let axis = session.clone();
    Ok((
        Box::new((conn, session)),
        axis,
        std::os::fd::OwnedFd::from(fd),
    ))
}

/// Poll `file` for the EIS socket path (gamescope relays `LIBEI_SOCKET` there), then
/// connect. A bare name is resolved against `XDG_RUNTIME_DIR`, matching libei.
/// Line 2, when present, is compositor output `WxH` — gamescope's EIS region is
/// degenerate, so geometry cannot come from the protocol. A file still empty at the
/// deadline is a gamescope built without libei: it has no EIS to connect to.
async fn connect_socket_file(file: &std::path::Path) -> Result<(UnixStream, Option<(u32, u32)>)> {
    // Re-read and retry: the file may still name a dead session's socket, or the
    // live one is not listening yet. Bound so a wedged compositor still errors.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut logged = String::new();
    let mut blank = false;
    loop {
        // Refuse a symlink. The file lives under `$XDG_RUNTIME_DIR` (0700), but
        // following one would connect to an attacker-chosen EIS server.
        if std::fs::symlink_metadata(file)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            return Err(anyhow!(
                "EIS relay file {} is a symlink — refusing to follow it",
                file.display()
            ));
        }
        if let Ok(s) = std::fs::read_to_string(file) {
            let mut file_lines = s.lines();
            let name = file_lines.next().unwrap_or("").trim();
            blank = name.is_empty();
            let hint = file_lines.next().and_then(|l| {
                let (w, h) = l.trim().split_once('x')?;
                Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?))
            });
            if !name.is_empty() {
                let full = if name.starts_with('/') {
                    std::path::PathBuf::from(name)
                } else {
                    let runtime = std::env::var("XDG_RUNTIME_DIR").map_err(|_| {
                        anyhow!("XDG_RUNTIME_DIR unset (needed to resolve EIS socket '{name}')")
                    })?;
                    std::path::Path::new(&runtime).join(name)
                };
                if logged != name {
                    tracing::info!(socket = %full.display(), "libei: connecting to EIS socket");
                    logged = name.to_string();
                }
                match UnixStream::connect(&full) {
                    Ok(stream) => return Ok((stream, hint)),
                    // Refused: file exists, no listener yet (or a dead session).
                    // NotFound: path not created yet. Retry. Anything else is fatal.
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                        ) => {}
                    Err(e) => return Err(anyhow!("connect EIS socket {}: {e}", full.display())),
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            if blank {
                return Err(anyhow!(
                    "gamescope left {} empty: built without libei, so keyboard and mouse can't reach it",
                    file.display()
                ));
            }
            return Err(anyhow!(
                "EIS socket from {} never became connectable (gamescope not up, or its EIS crashed)",
                file.display()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Region to map absolute coordinates into. The device advertises one region per
/// logical monitor; `first()` is whichever output the compositor announced first.
///
/// Most identifying first:
/// 1. [`AbsoluteAnchor::mapping_id`] — protocol key correlating a region with a stream.
/// 2. Anchor origin — two outputs can share a size, never a top-left. A mirrored
///    physical monitor's region is not the client's size (`design/per-monitor-portal-capture.md`).
/// 3. The streamed head's mode (`extent`, [`crate::stream_extent`]), then the event's own
///    `w`×`h` — ambiguous once two heads share a mode. The event's size alone is the
///    client's rect: a GameStream window the size of the operator's monitor lands there.
/// 4. `first()`.
///
/// An unmatched anchor falls through: the region set is the truth. The caller logs
/// the miss ([`anchor_missed`]).
fn region_for_mode<'a>(
    regions: &'a [reis::event::Region],
    extent: Option<(u16, u16)>,
    w: f32,
    h: f32,
    anchor: Option<&AbsoluteAnchor>,
) -> Option<&'a reis::event::Region> {
    if let Some(a) = anchor {
        if let Some(id) = a.mapping_id.as_deref() {
            if let Some(r) = regions.iter().find(|r| r.mapping_id.as_deref() == Some(id)) {
                return Some(r);
            }
        }
        if let Some((x, y)) = a.origin {
            // EI region offsets are unsigned; a negative origin matches nothing
            // rather than wrapping to a huge u32 that could hit a real region.
            if x >= 0 && y >= 0 {
                if let Some(r) = regions.iter().find(|r| r.x == x as u32 && r.y == y as u32) {
                    return Some(r);
                }
            }
        }
    }
    let by_size = |w: f32, h: f32| {
        regions
            .iter()
            .find(|r| r.width as f32 == w && r.height as f32 == h)
            // Display scale shrinks the EI region to logical pixels (Mutter: 1280×800
            // at 1.5 → 853×533). Exact size then misses; without this rung we take
            // `regions.first()` — the wrong monitor whenever another region sorts first.
            .or_else(|| regions.iter().find(|r| scaled_region_match(r, w, h)))
    };
    extent
        .and_then(|(ew, eh)| by_size(f32::from(ew), f32::from(eh)))
        .or_else(|| by_size(w, h))
        .or_else(|| regions.first())
}

/// True when `r` is the streamed `w`×`h` surface advertised at display scale > 1.
/// ±2 logical px of slack covers per-axis floor (Mutter: 1280/1.5 → 853). Scales
/// 1..=4 (fractional 1.25/1.5/1.75 included); 1.0 is the exact rung, and >4 is
/// not a real display scale — matching it would pick the wrong monitor.
fn scaled_region_match(r: &reis::event::Region, w: f32, h: f32) -> bool {
    let (rw, rh) = (r.width as f32, r.height as f32);
    if rw < 1.0 || rh < 1.0 {
        return false;
    }
    let s = w / rw;
    if !(1.0..=4.0).contains(&s) {
        return false;
    }
    (rh * s - h).abs() <= 2.0 * s
}

/// Log which region absolute coordinates landed in, once per distinct region so
/// a live session is one line, not one per motion event.
fn note_abs_region(region: &reis::event::Region, anchor: Option<&AbsoluteAnchor>) {
    static LAST: std::sync::Mutex<Option<(u32, u32, u32, u32)>> = std::sync::Mutex::new(None);
    let key = (region.x, region.y, region.width, region.height);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if *last == Some(key) {
        return;
    }
    *last = Some(key);
    tracing::info!(
        region = %format!("{}x{}+{}+{}", region.width, region.height, region.x, region.y),
        mapping_id = ?region.mapping_id,
        anchor_origin = ?anchor.and_then(|a| a.origin),
        anchor_mapping_id = ?anchor.and_then(|a| a.mapping_id.clone()),
        "libei: absolute input maps into this output"
    );
}

/// True when the anchor names an output this region set does not have. Drives
/// the one-shot warning: a miss must be in the log, not inferred from clicks.
fn anchor_missed(regions: &[reis::event::Region], anchor: Option<&AbsoluteAnchor>) -> bool {
    let Some(a) = anchor else {
        return false;
    };
    if let Some(id) = a.mapping_id.as_deref() {
        if regions.iter().any(|r| r.mapping_id.as_deref() == Some(id)) {
            return false;
        }
    }
    if let Some((x, y)) = a.origin {
        if x >= 0 && y >= 0 && regions.iter().any(|r| r.x == x as u32 && r.y == y as u32) {
            return false;
        }
    }
    true
}

struct DeviceSlot {
    device: reis::event::Device,
    /// Devices arrive paused and may pause again.
    resumed: bool,
    /// `start_emulating` issued since the last resume.
    emulating: bool,
}

/// Bound devices plus the serial and sequence the EI protocol requires.
struct EiState {
    devices: Vec<DeviceSlot>,
    last_serial: u32,
    sequence: u32,
    /// inject() count — throttle for diagnostic logging.
    injected: u64,
    /// [`InputKind`]s already logged once (first of each kind).
    seen_kinds: u32,
    /// Presses the compositor saw and has not seen released, as their wire codes (keys as
    /// the truncated VK). Released at session end ([`EiState::release_all`]): a vanished
    /// client must not leave a latched key or Mutter's implicit grab.
    held: HeldInput,
    /// Touch id currently driving the absolute pointer ([`EiState::degrade_touch`]).
    /// `None` between touches.
    degraded_touch: Option<u32>,
    /// Compositor output size (relay-file "WxH") — scale target when the device's
    /// region is degenerate. Without it the fallback is raw client pixels.
    output_hint: Option<(u32, u32)>,
    /// Connected through gamescope's relayed socket, whose EIS counts scroll in clicks.
    gamescope: bool,
    /// Scroll lowering; the backend inside follows the EIS source.
    scroll: ScrollMapper,
    /// Mutter only: scroll goes to the session's `NotifyPointerAxis`, never to EIS.
    mutter_axis: Option<UnboundedSender<(f64, f64, u32)>>,
}

/// Last-warned unmatched anchor, so a sticky miss logs once per change rather
/// than once per pointer sample (absolute motion arrives at client frame rate).
static LAST_WARNED_ANCHOR: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Warn once per distinct anchor that it names no advertised EIS region, so
/// absolute coordinates fell back to size matching. See [`region_for_mode`].
fn warn_anchor_miss(anchor: &AbsoluteAnchor, regions: &[reis::event::Region]) {
    let key = format!("{anchor:?}");
    let mut last = LAST_WARNED_ANCHOR.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(key.as_str()) {
        return;
    }
    *last = Some(key);
    tracing::warn!(
        ?anchor,
        regions = ?regions
            .iter()
            .map(|r| (r.x, r.y, r.width, r.height, r.mapping_id.clone()))
            .collect::<Vec<_>>(),
        "libei: the session's absolute-coordinate anchor matches no EIS region — falling back to \
         size matching, so the pointer may land on the wrong monitor"
    );
}

/// Plausible output geometry to map normalized coordinates into. gamescope
/// advertises `(0,0,INT32_MAX,INT32_MAX)` meaning "coordinates are raw";
/// normalizing into it explodes a center tap to x≈1e9. 16384 covers real
/// multi-monitor layouts while rejecting that sentinel.
fn sane_region(r: &reis::event::Region) -> bool {
    r.width > 0 && r.height > 0 && r.width <= 16_384 && r.height <= 16_384
}

/// Execute a normalized-scroll plan on an `ei::Scroll` interface: continuous
/// and discrete deltas, stops, and the plan's own frame boundaries. Shared by
/// [`EiState::inject`] and [`EiState::exec_scroll`].
fn exec_scroll_ops(s: &ei::Scroll, dev: &ei::Device, serial: u32, ops: Vec<ScrollOp>) {
    for op in ops {
        match op {
            ScrollOp::Continuous { horizontal, value } => {
                if horizontal {
                    s.scroll(value as f32, 0.0);
                } else {
                    s.scroll(0.0, value as f32);
                }
            }
            ScrollOp::Discrete120 { horizontal, value } => {
                if horizontal {
                    s.scroll_discrete(value, 0);
                } else {
                    s.scroll_discrete(0, value);
                }
            }
            ScrollOp::Stop { horizontal, cancel } => s.scroll_stop(
                u32::from(horizontal),
                u32::from(!horizontal),
                u32::from(cancel),
            ),
            ScrollOp::Frame => dev.frame(serial, crate::monotonic_us()),
            // wlroots-only vocabulary never reaches an ei plan.
            ScrollOp::AxisSource(_) | ScrollOp::DiscreteDetents { .. } => {}
        }
    }
}

fn kind_bit(kind: InputKind) -> u32 {
    let i = match kind {
        InputKind::MouseMove => 0,
        InputKind::MouseMoveAbs => 1,
        InputKind::MouseButtonDown => 2,
        InputKind::MouseButtonUp => 3,
        InputKind::MouseScroll => 4,
        InputKind::KeyDown => 5,
        InputKind::KeyUp => 6,
        InputKind::TouchDown => 7,
        InputKind::TouchMove => 8,
        InputKind::TouchUp => 9,
        InputKind::GamepadButton => 10,
        InputKind::GamepadAxis => 11,
        InputKind::GamepadState => 12,
        InputKind::GamepadRemove => 13,
        InputKind::GamepadArrival => 14,
        InputKind::TextInput => 15,
        InputKind::Scroll => 16,
    };
    1 << i
}

impl EiState {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            last_serial: 0,
            sequence: 0,
            injected: 0,
            seen_kinds: 0,
            held: HeldInput::default(),
            degraded_touch: None,
            output_hint: None,
            gamescope: false,
            scroll: ScrollMapper::new(ScrollBackend::Libei),
            mutter_axis: None,
        }
    }

    /// Run a scroll plan where this backend scrolls: Mutter's session call, or
    /// the EIS scroll device.
    fn exec_scroll(&mut self, ops: Vec<ScrollOp>, ctx: &ei::Context) {
        if ops.is_empty() {
            return;
        }
        if let Some(tx) = &self.mutter_axis {
            for call in mutter_axis_calls(&ops) {
                let _ = tx.send(call);
            }
            return;
        }
        let Some(idx) = self.device_for(DeviceCapability::Scroll) else {
            return;
        };
        let dev = self.devices[idx].device.device().clone();
        self.ensure_emulating(idx, &dev);
        if let Some(s) = self.devices[idx].device.interface::<ei::Scroll>() {
            exec_scroll_ops(&s, &dev, self.last_serial, ops);
            let _ = ctx.flush();
        }
    }

    /// Synthesize wire-level releases through [`EiState::inject`] so the
    /// compositor sees key-up / button-up / touch-up before devices disappear.
    fn release_all(&mut self, ctx: &ei::Context) {
        // A degraded touch's left button is in `held`. Clear the primary-finger latch too, or
        // the next session's first TouchDown reads as a second finger and is ignored.
        self.degraded_touch = None;
        let ups = self.held.release();
        // A scroll gesture still open ends cancelled, or its kinetic tail
        // outlives the session.
        let scroll_ops = self.scroll.cancel_all();
        if ups.is_empty() && scroll_ops.is_empty() {
            return;
        }
        tracing::info!(
            held = ups.len(),
            "libei: releasing input still held at session end"
        );
        for up in &ups {
            self.inject(up, ctx);
        }
        self.exec_scroll(scroll_ops, ctx);
    }

    fn handle_ei(&mut self, ev: EiEvent, ctx: &ei::Context) {
        match ev {
            EiEvent::SeatAdded(e) => {
                e.seat.bind_capabilities(
                    DeviceCapability::Pointer
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Keyboard
                        | DeviceCapability::Scroll
                        | DeviceCapability::Button
                        | DeviceCapability::Touch,
                );
                let _ = ctx.flush();
            }
            EiEvent::DeviceAdded(e) => {
                tracing::info!(device = ?e.device.name(), ty = ?e.device.device_type(), "libei: device added");
                self.devices.push(DeviceSlot {
                    device: e.device,
                    resumed: false,
                    emulating: false,
                });
            }
            EiEvent::DeviceRemoved(e) => {
                self.devices.retain(|d| d.device != e.device);
            }
            EiEvent::DeviceResumed(e) => {
                self.last_serial = e.serial;
                if let Some(d) = self.devices.iter_mut().find(|d| d.device == e.device) {
                    d.resumed = true;
                    d.emulating = false; // must re-issue start_emulating after a resume
                }
                let dev = &e.device;
                tracing::info!(
                    name = ?dev.name(),
                    pointer = dev.has_capability(DeviceCapability::Pointer),
                    pointer_abs = dev.has_capability(DeviceCapability::PointerAbsolute),
                    keyboard = dev.has_capability(DeviceCapability::Keyboard),
                    button = dev.has_capability(DeviceCapability::Button),
                    scroll = dev.has_capability(DeviceCapability::Scroll),
                    // One region per logical monitor; `region_for_mode` picks per event.
                    // Log them so a mis-mapped pointer is diagnosable from the journal.
                    regions = ?dev
                        .regions()
                        .iter()
                        .map(|r| format!("{}x{}+{}+{}", r.width, r.height, r.x, r.y))
                        .collect::<Vec<_>>(),
                    "libei: device RESUMED (now emittable)"
                );
            }
            EiEvent::DevicePaused(e) => {
                if let Some(d) = self.devices.iter_mut().find(|d| d.device == e.device) {
                    d.resumed = false;
                    d.emulating = false;
                }
            }
            // Server reports resulting modifier/group state; we do not set it.
            EiEvent::KeyboardModifiers(e) => self.last_serial = e.serial,
            _ => {}
        }
    }

    fn device_for(&self, cap: DeviceCapability) -> Option<usize> {
        self.devices
            .iter()
            .position(|d| d.resumed && d.device.has_capability(cap))
    }

    fn ensure_emulating(&mut self, idx: usize, dev: &ei::Device) {
        if !self.devices[idx].emulating {
            dev.start_emulating(self.last_serial, self.sequence);
            self.sequence = self.sequence.wrapping_add(1);
            self.devices[idx].emulating = true;
        }
    }

    /// Degrade touch to a single-finger absolute pointer when EIS has no
    /// touchscreen device (gamescope / headless KWin). Down = abs-move + left
    /// press, move = abs-move, up = left release, via [`EiState::inject`] so
    /// region mapping, held-state, and [`EiState::release_all`] apply. Later
    /// fingers are ignored — a pointer has no second contact.
    fn degrade_touch(&mut self, ev: &InputEvent, ctx: &ei::Context) {
        const GS_BUTTON_LEFT: u32 = 1;
        match ev.kind {
            InputKind::TouchDown => {
                if self.degraded_touch.is_some() {
                    return; // secondary finger — single-pointer degradation
                }
                self.degraded_touch = Some(ev.code);
                static NOTED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::info!(
                        "compositor's EIS has no touchscreen device — degrading touch to a \
                         single-finger absolute pointer (tap = left click; multi-touch \
                         gestures unavailable)"
                    );
                }
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseMoveAbs,
                        ..*ev
                    },
                    ctx,
                );
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseButtonDown,
                        code: GS_BUTTON_LEFT,
                        ..*ev
                    },
                    ctx,
                );
            }
            InputKind::TouchMove if self.degraded_touch == Some(ev.code) => {
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseMoveAbs,
                        ..*ev
                    },
                    ctx,
                );
            }
            InputKind::TouchUp if self.degraded_touch == Some(ev.code) => {
                self.degraded_touch = None;
                self.inject(
                    &InputEvent {
                        kind: InputKind::MouseButtonUp,
                        code: GS_BUTTON_LEFT,
                        ..*ev
                    },
                    ctx,
                );
            }
            _ => {}
        }
    }

    fn inject(&mut self, ev: &InputEvent, ctx: &ei::Context) {
        // No ei_touchscreen but an absolute pointer exists → degrade rather than
        // drop. Per event, not latched: a touchscreen appearing later takes over.
        if matches!(
            ev.kind,
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp
        ) && self.device_for(DeviceCapability::Touch).is_none()
            && self.device_for(DeviceCapability::PointerAbsolute).is_some()
        {
            self.degrade_touch(ev, ctx);
            return;
        }
        let Some(cap) = capability_for(ev.kind) else {
            return;
        };
        self.injected += 1;
        let n = self.injected;
        let bit = kind_bit(ev.kind);
        let first = self.seen_kinds & bit == 0;
        self.seen_kinds |= bit;
        let loud = first || n <= 5 || n % 600 == 0;
        if self.mutter_axis.is_some() && cap == DeviceCapability::Scroll {
            let ops = self.scroll.plan(ev);
            self.exec_scroll(ops, ctx);
            return;
        }
        let Some(idx) = self.device_for(cap) else {
            self.drop_without_device(ev, cap, n, loud);
            return;
        };
        let emitted = self.emit(idx, ev);
        if let Err(e) = ctx.flush() {
            // Dead EIS fails flush on every event (mouse-move = 100s/s); same
            // `loud` sampler as the sibling warns.
            if loud {
                tracing::warn!(error = %e, "libei: ctx.flush failed");
            }
        }
        if loud {
            tracing::debug!(n, kind = ?ev.kind, idx, emitted, "libei: emitted");
        }
    }

    /// Report an event no resumed device can take, sampled by `loud`. Touch also warns once:
    /// the portal may grant a touchscreen that EIS never creates.
    fn drop_without_device(&self, ev: &InputEvent, cap: DeviceCapability, n: u64, loud: bool) {
        if loud {
            tracing::warn!(
                n,
                kind = ?ev.kind,
                ?cap,
                devices = self.devices.len(),
                resumed = self.devices.iter().filter(|d| d.resumed).count(),
                "libei: dropped event — no resumed device exposes this capability"
            );
        }
        if matches!(
            ev.kind,
            InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp
        ) {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    "touch received but the compositor's EIS exposed no touchscreen device — \
                     touch is dropped (KWin's libei may not implement ei_touchscreen yet; \
                     gamescope / a newer compositor may)"
                );
            }
        }
    }

    /// [`abs_point`] with this session's anchor, streamed mode and output hint.
    fn map_abs(&self, regions: &[reis::event::Region], ev: &InputEvent) -> Option<(f32, f32)> {
        abs_point(
            regions,
            crate::stream_extent(),
            crate::absolute_anchor().as_ref(),
            self.output_hint,
            ev,
        )
    }

    /// Emit `ev` on device `idx` in its own frame, and note what the compositor now holds.
    /// `false` when nothing was framed: the device lacks the interface, the code has no
    /// mapping, or the scroll plan framed itself.
    fn emit(&mut self, idx: usize, ev: &InputEvent) -> bool {
        let dev = self.devices[idx].device.device().clone();
        self.ensure_emulating(idx, &dev);

        let mut emitted = true;
        let slot = &self.devices[idx].device;
        match ev.kind {
            InputKind::MouseMove => match slot.interface::<ei::Pointer>() {
                Some(p) => p.motion_relative(ev.x as f32, ev.y as f32),
                None => emitted = false,
            },
            InputKind::MouseMoveAbs => match slot.interface::<ei::PointerAbsolute>() {
                Some(p) => match self.map_abs(slot.regions(), ev) {
                    Some((x, y)) => p.motion_absolute(x, y),
                    None => emitted = false,
                },
                None => emitted = false,
            },
            InputKind::MouseButtonDown | InputKind::MouseButtonUp => {
                match (slot.interface::<ei::Button>(), gs_button_to_evdev(ev.code)) {
                    (Some(b), Some(btn)) => {
                        let st = if ev.kind == InputKind::MouseButtonDown {
                            ei::button::ButtonState::Press
                        } else {
                            ei::button::ButtonState::Released
                        };
                        b.button(btn, st);
                    }
                    _ => emitted = false,
                }
            }
            // Legacy and normalized scroll share the mapper. The plan carries
            // ei's framing rules: a Begin-cancel gets its own frame and a stop
            // never shares one with a nonzero delta.
            InputKind::MouseScroll | InputKind::Scroll => match slot.interface::<ei::Scroll>() {
                Some(s) => {
                    exec_scroll_ops(&s, &dev, self.last_serial, self.scroll.plan(ev));
                    emitted = false; // plan Frame ops did the framing
                }
                None => emitted = false,
            },
            InputKind::KeyDown | InputKind::KeyUp => {
                match (slot.interface::<ei::Keyboard>(), vk_to_evdev(ev.code as u8)) {
                    (Some(k), Some(evdev)) => {
                        let st = if ev.kind == InputKind::KeyDown {
                            ei::keyboard::KeyState::Press
                        } else {
                            ei::keyboard::KeyState::Released
                        };
                        k.key(evdev as u32, st);
                    }
                    _ => {
                        emitted = false;
                        tracing::debug!(vk = ev.code, "libei: unmapped VK keycode — dropped");
                    }
                }
            }
            // `code` is the touch id. One event = one frame (ei_touchscreen
            // forbids down/motion/up sharing a frame).
            InputKind::TouchDown | InputKind::TouchMove => {
                match slot.interface::<ei::Touchscreen>() {
                    Some(t) => match self.map_abs(slot.regions(), ev) {
                        Some((x, y)) if ev.kind == InputKind::TouchDown => t.down(ev.code, x, y),
                        Some((x, y)) => t.motion(ev.code, x, y),
                        None => emitted = false,
                    },
                    None => emitted = false,
                }
            }
            InputKind::TouchUp => match slot.interface::<ei::Touchscreen>() {
                Some(t) => t.up(ev.code),
                None => emitted = false,
            },
            InputKind::GamepadState
            | InputKind::GamepadButton
            | InputKind::GamepadAxis
            | InputKind::GamepadRemove
            | InputKind::GamepadArrival
            | InputKind::TextInput => emitted = false,
        }

        if emitted {
            // Hold the injected code, not the raw wire code: `vk_to_evdev` truncates to u8,
            // so 0x41 and 0x141 press the same key and either KeyUp must release it.
            let code = match ev.kind {
                InputKind::KeyDown | InputKind::KeyUp => ev.code & 0xff,
                _ => ev.code,
            };
            self.held.note(&InputEvent { code, ..*ev });
            dev.frame(self.last_serial, crate::monotonic_us());
        }
        emitted
    }
}

/// The EI capability `kind` needs. `None` for gamepads, which take the uinput path, and for
/// committed text: keycodes go against the server's keymap, and `HOST_CAP_TEXT_INPUT` is
/// not advertised on this backend.
fn capability_for(kind: InputKind) -> Option<DeviceCapability> {
    Some(match kind {
        InputKind::MouseMove => DeviceCapability::Pointer,
        InputKind::MouseMoveAbs => DeviceCapability::PointerAbsolute,
        InputKind::MouseButtonDown | InputKind::MouseButtonUp => DeviceCapability::Button,
        InputKind::MouseScroll | InputKind::Scroll => DeviceCapability::Scroll,
        InputKind::KeyDown | InputKind::KeyUp => DeviceCapability::Keyboard,
        InputKind::TouchDown | InputKind::TouchMove | InputKind::TouchUp => DeviceCapability::Touch,
        InputKind::GamepadState
        | InputKind::GamepadButton
        | InputKind::GamepadAxis
        | InputKind::GamepadRemove
        | InputKind::GamepadArrival
        | InputKind::TextInput => return None,
    })
}

/// Where an absolute sample (pointer or touch) lands in compositor space. `x`/`y` are client
/// pixels and `flags` packs the client surface w/h; `None` for a zero-sized surface.
///
/// The normalized position maps into the region [`region_for_mode`] picks. [`sane_region`]
/// rejects gamescope's INT32_MAX "raw" region, where a center tap would become x≈1e9. Without
/// a usable region it scales into the relay-file output `hint`, then passes raw client pixels.
/// An anchor that names no region warns once ([`warn_anchor_miss`]).
fn abs_point(
    regions: &[reis::event::Region],
    extent: Option<(u16, u16)>,
    anchor: Option<&AbsoluteAnchor>,
    hint: Option<(u32, u32)>,
    ev: &InputEvent,
) -> Option<(f32, f32)> {
    let (w, h) = ((ev.flags >> 16) & 0xffff, ev.flags & 0xffff);
    if w == 0 || h == 0 {
        return None;
    }
    let (w, h) = (w as f32, h as f32);
    let nx = (ev.x as f32 / w).clamp(0.0, crate::ABS_EDGE);
    let ny = (ev.y as f32 / h).clamp(0.0, crate::ABS_EDGE);
    if let Some(a) = anchor.filter(|a| anchor_missed(regions, Some(a))) {
        warn_anchor_miss(a, regions);
    }
    Some(
        match region_for_mode(regions, extent, w, h, anchor).filter(|r| sane_region(r)) {
            Some(region) => {
                note_abs_region(region, anchor);
                (
                    region.x as f32 + nx * region.width as f32,
                    region.y as f32 + ny * region.height as f32,
                )
            }
            None => match hint {
                Some((ow, oh)) => (nx * ow as f32, ny * oh as f32),
                None => (ev.x as f32, ev.y as f32),
            },
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(x: u32, y: u32, w: u32, h: u32, mapping_id: Option<&str>) -> reis::event::Region {
        reis::event::Region {
            x,
            y,
            width: w,
            height: h,
            scale: 1.0,
            mapping_id: mapping_id.map(str::to_string),
        }
    }

    /// Two heads at the same size: size matching is a coin flip. Origin picks.
    #[test]
    fn the_origin_disambiguates_two_same_size_monitors() {
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 1920, 1080, None),
        ];
        let anchor = AbsoluteAnchor {
            origin: Some((1920, 0)),
            mapping_id: None,
        };
        let picked = region_for_mode(&regions, None, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!((picked.x, picked.y), (1920, 0));
        // No anchor takes the first same-sized region — required for the
        // client-sized virtual-output path.
        let picked = region_for_mode(&regions, None, 1920.0, 1080.0, None).unwrap();
        assert_eq!((picked.x, picked.y), (0, 0));
    }

    /// A GameStream client sends its own window rect, which can be the operator's monitor size.
    /// The streamed head's mode picks first; the event's size is the fallback.
    #[test]
    fn the_streamed_mode_outranks_the_client_rect() {
        let regions = [
            region(0, 0, 1920, 1080, None),    // the operator's monitor
            region(1920, 0, 3840, 2160, None), // the streamed head
        ];
        let picked = region_for_mode(&regions, Some((3840, 2160)), 1920.0, 1080.0, None).unwrap();
        assert_eq!(picked.x, 1920);
        let picked = region_for_mode(&regions, None, 1920.0, 1080.0, None).unwrap();
        assert_eq!(picked.x, 0);
        // A published mode no region matches falls back to the event's size.
        let picked = region_for_mode(&regions, Some((1280, 720)), 3840.0, 2160.0, None).unwrap();
        assert_eq!(picked.x, 1920);
    }

    /// `mapping_id` outranks origin: a stale/rounded origin must not override
    /// the protocol's stream↔region key.
    #[test]
    fn mapping_id_outranks_the_origin() {
        let regions = [
            region(0, 0, 1920, 1080, Some("head-a")),
            region(1920, 0, 1920, 1080, Some("head-b")),
        ];
        let anchor = AbsoluteAnchor {
            origin: Some((0, 0)),
            mapping_id: Some("head-b".into()),
        };
        let picked = region_for_mode(&regions, None, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!(picked.mapping_id.as_deref(), Some("head-b"));
    }

    /// Display scale shrinks the EI region to logical pixels. Without the scaled
    /// rung the ladder takes `regions.first()`. 1280×800 at 1.5 is 853×533.
    #[test]
    fn a_scaled_output_beats_the_first_region_fallback() {
        let regions = [
            region(0, 0, 1462, 1044, None),    // first() would pick this
            region(1462, 0, 1920, 1080, None), // physical
            region(3382, 0, 853, 533, None),   // 1280×800 at 1.5
        ];
        let picked = region_for_mode(&regions, None, 1280.0, 800.0, None).unwrap();
        assert_eq!((picked.width, picked.height), (853, 533));
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 640, 400, None),
        ];
        let picked = region_for_mode(&regions, None, 1280.0, 800.0, None).unwrap();
        assert_eq!((picked.width, picked.height), (640, 400));
        // Wrong aspect is not a consistent scale — fallback stays `regions.first()`.
        let regions = [
            region(0, 0, 1000, 1000, None),
            region(1000, 0, 640, 200, None),
        ];
        let picked = region_for_mode(&regions, None, 1280.0, 800.0, None).unwrap();
        assert_eq!((picked.width, picked.height), (1000, 1000));
    }

    /// A mirrored monitor's region is not the streamed size; origin is what finds it.
    #[test]
    fn the_anchor_finds_a_monitor_the_streamed_size_does_not_match() {
        let regions = [
            region(0, 0, 1920, 1080, None),
            region(1920, 0, 3840, 2160, None),
        ];
        let anchor = AbsoluteAnchor {
            origin: Some((1920, 0)),
            mapping_id: None,
        };
        let picked = region_for_mode(&regions, None, 1280.0, 720.0, Some(&anchor)).unwrap();
        assert_eq!((picked.width, picked.height), (3840, 2160));
    }

    /// Unmatched anchor falls through the ladder (size, then first) and is reported.
    #[test]
    fn an_unmatched_anchor_falls_back_and_is_reported() {
        let regions = [region(0, 0, 1920, 1080, None)];
        let anchor = AbsoluteAnchor {
            origin: Some((5000, 5000)),
            mapping_id: None,
        };
        assert!(anchor_missed(&regions, Some(&anchor)));
        let picked = region_for_mode(&regions, None, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!((picked.x, picked.y), (0, 0), "fell back to the size match");
        let ok = AbsoluteAnchor {
            origin: Some((0, 0)),
            mapping_id: None,
        };
        assert!(!anchor_missed(&regions, Some(&ok)));
        assert!(!anchor_missed(&regions, None), "no anchor is not a miss");
    }

    /// EI offsets are unsigned; a negative origin must match nothing, not wrap.
    #[test]
    fn a_negative_origin_matches_nothing_rather_than_wrapping() {
        let regions = [region(0, 0, 1920, 1080, None)];
        let anchor = AbsoluteAnchor {
            origin: Some((-1920, 0)),
            mapping_id: None,
        };
        assert!(anchor_missed(&regions, Some(&anchor)));
        let picked = region_for_mode(&regions, None, 1920.0, 1080.0, Some(&anchor)).unwrap();
        assert_eq!((picked.x, picked.y), (0, 0));
    }

    /// Pointer and touch share one ladder: the picked region, else the output hint, else
    /// raw client pixels. A zero-sized surface maps nowhere.
    #[test]
    fn an_absolute_sample_maps_into_the_region_then_the_hint_then_raw_pixels() {
        let ev = InputEvent {
            kind: InputKind::TouchDown,
            _pad: [0; 3],
            code: 0,
            x: 960,
            y: 540,
            flags: (1920 << 16) | 1080,
        };
        let head = [region(1920, 0, 3840, 2160, None)];
        assert_eq!(
            abs_point(&head, None, None, None, &ev),
            Some((3840.0, 1080.0))
        );
        // gamescope's "raw" sentinel is no region to normalize into.
        let raw = [region(0, 0, i32::MAX as u32, i32::MAX as u32, None)];
        assert_eq!(
            abs_point(&raw, None, None, Some((1280, 800)), &ev),
            Some((640.0, 400.0))
        );
        assert_eq!(abs_point(&raw, None, None, None, &ev), Some((960.0, 540.0)));
        let zero = InputEvent { flags: 0, ..ev };
        assert_eq!(abs_point(&head, None, None, None, &zero), None);
    }

    /// An empty anchor is the same as none — callers may build one unconditionally.
    #[test]
    fn an_empty_anchor_is_dropped_by_the_setter() {
        crate::set_absolute_anchor(Some(AbsoluteAnchor::default()));
        assert_eq!(crate::absolute_anchor(), None);
        crate::set_absolute_anchor(Some(AbsoluteAnchor {
            origin: Some((1920, 0)),
            mapping_id: None,
        }));
        assert_eq!(crate::absolute_anchor().unwrap().origin, Some((1920, 0)));
        crate::set_absolute_anchor(None);
        assert_eq!(crate::absolute_anchor(), None);
    }
}
