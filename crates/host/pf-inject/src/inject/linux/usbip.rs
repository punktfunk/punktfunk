//! USB/IP attach on in-tree `vhci_hcd`, shared by every usbip pad.
//!
//! [`usbip_sim`] serves the device on a dedicated server thread, fed over the pad's relay
//! ([`crate::usbip_relay`]), and the local host imports it. Attach is in-process: the host
//! preconnects and accepts only that TCP 4-tuple before handing the socket to `vhci_hcd`. A
//! seat host can't write `attach`; the supervisor's pad server serves its device instead and
//! the seat keeps only the relay. `PUNKTFUNK_USBIP_ATTACH=cli` is an unauthenticated debug
//! override. Callers degrade to UHID on failure.

use crate::usbip_relay::{
    served, Hello, Incoming, Output, Outputs, Served, MAX_IDENTITY, MAX_MESSAGE,
};
use anyhow::{bail, Context, Result};
use rustix::net::SendFlags;
use std::collections::{HashSet, VecDeque};
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use usbip_sim::{UsbEndpoint, UsbInterfaceHandler, UsbIpServer};

/// One device per server, so the usbip default bus id is enough.
const BUS_ID: &str = "0-0-0";
/// usbip CLI default; [`attach_via_cli`] must listen here.
const USBIP_TCP_PORT: u16 = 3240;

/// Every `idVendor:idProduct` a usbip pad here presents: the only devices a seat's vhci port
/// binds drivers for. Each builder's test checks its device is listed.
pub const EMULATED: [(u16, u16); 7] = [
    (
        crate::steam_usbip::STEAM_VENDOR,
        crate::steam_usbip::STEAMDECK_PRODUCT,
    ),
    (
        crate::steam_usbip::STEAM_VENDOR,
        crate::steam_usbip::STEAMCTRL_PRODUCT,
    ),
    (
        crate::triton_usbip::TRITON_VENDOR,
        crate::triton_usbip::TRITON_WIRED_PRODUCT,
    ),
    (
        crate::triton_usbip::TRITON_VENDOR,
        crate::triton_usbip::TRITON_PUCK_PRODUCT,
    ),
    (
        crate::dualsense_proto::DS_VENDOR as u16,
        crate::dualsense_proto::DS_PRODUCT as u16,
    ),
    (
        crate::switch2_proto::VENDOR,
        crate::switch2_proto::Model::Pro.product(),
    ),
    (
        crate::switch2_proto::VENDOR,
        crate::switch2_proto::Model::GameCube.product(),
    ),
];

/// HID class descriptor: one report descriptor of `report_len` bytes.
pub(crate) fn hid_class_descriptor(bcd_hid: u16, country: u8, report_len: usize) -> Vec<u8> {
    let [bcd_lo, bcd_hi] = bcd_hid.to_le_bytes();
    let [len_lo, len_hi] = (report_len as u16).to_le_bytes();
    vec![
        0x09, 0x21, bcd_lo, bcd_hi, country, 0x01, 0x22, len_lo, len_hi,
    ]
}

pub(crate) fn boxed(
    h: impl UsbInterfaceHandler + Send + 'static,
) -> Arc<Mutex<Box<dyn UsbInterfaceHandler + Send>>> {
    Arc::new(Mutex::new(Box::new(h)))
}

pub(crate) fn ep(address: u8, attributes: u8, max_packet_size: u16, interval: u8) -> UsbEndpoint {
    UsbEndpoint {
        address,
        attributes,
        max_packet_size,
        interval,
    }
}

/// Dedicated current-thread runtime. Nested runtimes panic, so this is its own thread.
struct ServerThread {
    stop: Arc<tokio::sync::Notify>,
    join: Option<JoinHandle<()>>,
}

enum ServerEndpoint {
    Listener(std::net::TcpListener),
    Connected(std::net::TcpStream),
}

impl ServerThread {
    /// Serves `served` until the host's connection, or the pad's `relay`, ends.
    fn spawn(
        endpoint: ServerEndpoint,
        served: Served,
        relay: UnixDatagram,
        label: &str,
    ) -> Result<ServerThread> {
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_t = stop.clone();
        let label = label.to_string();
        let join = std::thread::Builder::new()
            .name("pf-deck-usbip".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::error!(error = %e, "usbip server runtime build failed");
                        return;
                    }
                };
                rt.block_on(run_server(endpoint, served, relay, stop_t, label));
            })
            .context("spawn usbip server thread")?;
        Ok(ServerThread {
            stop,
            join: Some(join),
        })
    }

    /// Blocks until the server ends on its own.
    fn wait(mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for ServerThread {
    fn drop(&mut self) {
        self.stop.notify_one();
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// One USB/IP socket. In-process hands a pre-accepted 4-tuple; CLI hands a one-shot listener.
/// Ends when the host's connection or the pad's relay does.
async fn run_server(
    endpoint: ServerEndpoint,
    served: Served,
    relay: UnixDatagram,
    stop: Arc<tokio::sync::Notify>,
    label: String,
) {
    let server = Arc::new(UsbIpServer::new_simulated(vec![served.device]));
    let pad = pump_relay(relay, served.input);
    tokio::pin!(pad);
    let mut sock = match endpoint {
        ServerEndpoint::Connected(sock) => match sock
            .set_nonblocking(true)
            .and_then(|()| tokio::net::TcpStream::from_std(sock))
        {
            Ok(sock) => sock,
            Err(e) => {
                tracing::error!(error = %e, "usbip TcpStream::from_std failed");
                return;
            }
        },
        ServerEndpoint::Listener(listener) => {
            let listener = match tokio::net::TcpListener::from_std(listener) {
                Ok(listener) => listener,
                Err(e) => {
                    tracing::error!(error = %e, "usbip TcpListener::from_std failed");
                    return;
                }
            };
            tokio::select! {
                _ = stop.notified() => return,
                _ = &mut pad => return,
                result = listener.accept() => match result {
                    Ok((sock, _)) => sock,
                    Err(e) => {
                        tracing::warn!(error = %e, "usbip accept error");
                        return;
                    }
                }
            }
        }
    };
    // URB replies interleave with SUBMITs; Nagle + delayed ACK stalls that round trip.
    sock.set_nodelay(true).ok();
    let trace = super::usbip_trace::trace_prefix(&label);
    let conn_label = label.clone();
    let conn = tokio::spawn(async move {
        let label = conn_label;
        let sink = trace.and_then(|prefix| match super::usbip_trace::open_trace(&prefix) {
            Ok(s) => {
                tracing::info!(prefix, "usbip byte trace armed");
                Some(s)
            }
            Err(e) => {
                tracing::warn!(error = %e, "usbip trace files unopenable — running untraced");
                None
            }
        });
        let res = match sink {
            Some(s) => {
                let mut traced = super::usbip_trace::TracedIo::wrap(sock, s);
                usbip_sim::handler(&mut traced, server).await
            }
            None => usbip_sim::handler(&mut sock, server).await,
        };
        match res {
            Ok(()) => tracing::debug!(label, "usbip connection closed by the kernel"),
            Err(e) => tracing::warn!(
                label,
                error = %e,
                "usbip server dropped the connection — the kernel will report this as a \
                 transfer error on whatever URB was in flight"
            ),
        }
    });
    // Runtime lives on this thread; returning here aborts the handler task.
    tokio::select! {
        _ = stop.notified() => {}
        _ = conn => {}
        _ = &mut pad => tracing::debug!(label, "usbip pad hung up"),
    }
}

/// Feeds the pad's messages to its device until the pad hangs up. What does not decode is
/// dropped.
async fn pump_relay(relay: UnixDatagram, mut input: Box<dyn FnMut(Incoming<'_>) + Send>) {
    let Ok(relay) = relay
        .set_nonblocking(true)
        .and_then(|()| tokio::net::UnixDatagram::from_std(relay))
    else {
        return;
    };
    let mut buf = vec![0u8; MAX_IDENTITY + 8];
    loop {
        match relay.recv(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => match Incoming::decode(&buf[..n]) {
                Some(message) => input(message),
                None => tracing::debug!(n, "usbip relay: dropped what the pad sent"),
            },
        }
    }
}

/// Outputs a pad keeps between two drains; past it the oldest go.
const MAX_PENDING_OUTPUTS: usize = 256;
/// How long an in-process import waits for its server.
const IMPORT_WAIT: Duration = Duration::from_secs(1);
/// How long a seat's pad server waits for the supervisor's [`Hello`].
const HELLO_WAIT: Duration = Duration::from_secs(5);

/// A pad's device on its `vhci_hcd` port, and the pad's end of the relay to its server.
/// Drop detaches the port first so the kernel tears the device down before the socket and
/// server go. A seat's port goes with its pad server, which ends when the relay closes.
pub(crate) struct UsbipAttachment {
    vhci_port: u16,
    /// Holds the fd handed to `vhci_hcd`. CLI attach and a seat's pad hold none.
    _client_sock: Option<TcpStream>,
    _server: Option<ServerThread>,
    brokered: bool,
    relay: UnixDatagram,
    pending: Arc<Mutex<VecDeque<Output>>>,
    alive: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

impl UsbipAttachment {
    /// Starts the thread that reads `relay`: outputs queue for [`outputs`](Self::outputs),
    /// audio goes straight to `audio`, so neither waits for the input loop.
    fn linked(
        vhci_port: u16,
        client_sock: Option<TcpStream>,
        server: Option<ServerThread>,
        brokered: bool,
        relay: UnixDatagram,
        audio: Option<SyncSender<Vec<f32>>>,
    ) -> Result<UsbipAttachment> {
        let pending: Arc<Mutex<VecDeque<Output>>> = Arc::default();
        let alive = Arc::new(AtomicBool::new(true));
        let reader = {
            let relay = relay.try_clone().context("clone the pad relay")?;
            let (pending, alive) = (pending.clone(), alive.clone());
            std::thread::Builder::new()
                .name("pf-usbip-pad".into())
                .spawn(move || {
                    read_outputs(&relay, &pending, audio.as_ref());
                    alive.store(false, Ordering::Relaxed);
                })
                .context("spawn the usbip pad reader")?
        };
        Ok(UsbipAttachment {
            vhci_port,
            _client_sock: client_sock,
            _server: server,
            brokered,
            relay,
            pending,
            alive,
            reader: Some(reader),
        })
    }

    /// The input report interface `iface` serves from now on. Never blocks: a full relay
    /// drops it, and the next frame or heartbeat sends the state again.
    pub(crate) fn input(&self, iface: u8, report: &[u8]) {
        self.send(&Incoming::Input { iface, report });
    }

    pub(crate) fn send(&self, message: &Incoming<'_>) {
        let _ = rustix::net::send(
            &self.relay,
            &message.encode(),
            SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
        );
    }

    /// What the host wrote since the last call, oldest first.
    pub(crate) fn outputs(&self) -> Vec<Output> {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.drain(..).collect()
    }

    /// `false` once the server is gone: the kernel dropped the device, or a seat's pad server
    /// ended. The pad manager then makes the pad again.
    pub(crate) fn alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// An attachment that is only a relay, for tests of what a pad sends its server.
    #[cfg(test)]
    pub(crate) fn for_test(relay: UnixDatagram) -> UsbipAttachment {
        UsbipAttachment::linked(0, None, None, true, relay, None).unwrap()
    }
}

/// The pad's side of the relay, until the server hangs up.
fn read_outputs(
    relay: &UnixDatagram,
    pending: &Mutex<VecDeque<Output>>,
    audio: Option<&SyncSender<Vec<f32>>>,
) {
    let mut buf = vec![0u8; MAX_MESSAGE];
    let mut dropped = 0u64;
    loop {
        let n = match relay.recv(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        match Output::decode(&buf[..n]) {
            Some(Output::Audio(pcm)) => {
                let Some(tx) = audio else { continue };
                let samples: Vec<f32> = pcm
                    .chunks_exact(2)
                    .map(|s| f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0)
                    .collect();
                // Drop rather than block: a stalled streamer must not stall the relay.
                if !samples.is_empty() && tx.try_send(samples).is_err() {
                    dropped += 1;
                    if dropped.is_power_of_two() {
                        tracing::debug!(
                            dropped,
                            "pad usb audio queue full — dropping (streamer behind?)"
                        );
                    }
                }
            }
            Some(report) => {
                let mut q = pending.lock().unwrap_or_else(|e| e.into_inner());
                if q.len() >= MAX_PENDING_OUTPUTS {
                    q.pop_front();
                }
                q.push_back(report);
            }
            None => tracing::debug!(n, "usbip relay: dropped what the server sent"),
        }
    }
}

impl Drop for UsbipAttachment {
    fn drop(&mut self) {
        // The kernel frees a port when its connection dies, and another pad can hold it by now:
        // detach only while it still carries our socket. A CLI attach has no fd to compare.
        if !self.brokered {
            let ours = self
                ._client_sock
                .as_ref()
                .is_none_or(|sock| vhci_port_holds(self.vhci_port, sock.as_raw_fd()));
            if !ours {
                tracing::debug!(
                    port = self.vhci_port,
                    "vhci port no longer ours — not detaching"
                );
            } else if let Err(e) = vhci_detach(self.vhci_port) {
                tracing::debug!(port = self.vhci_port, error = %e, "vhci detach failed (device may already be gone)");
            }
        }
        let _ = self.relay.shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

/// Attach `hello`'s device. A seat host has the supervisor serve it; `audio` takes a pad's
/// isochronous OUT. `PUNKTFUNK_USBIP_ATTACH=cli` selects the unauthenticated CLI fallback.
pub(crate) fn attach_device(
    hello: Hello,
    audio: Option<SyncSender<Vec<f32>>>,
    label: &str,
) -> Result<UsbipAttachment> {
    ensure_modules();
    let Some(base) = vhci_base() else {
        bail!("vhci_hcd unavailable (no /sys/devices/platform/vhci_hcd*/status) — is it loaded?");
    };
    if crate::pad_broker::brokered(&base.join("attach")) {
        let (relay, port) =
            crate::pad_broker::serve(&hello).context("serve through the seat pad broker")?;
        tracing::info!(
            label,
            vhci_port = port,
            "attached via usbip (served by the seat supervisor)"
        );
        return UsbipAttachment::linked(port, None, None, true, UnixDatagram::from(relay), audio);
    }
    let mode = std::env::var("PUNKTFUNK_USBIP_ATTACH").ok();
    if mode.as_deref() == Some("cli") {
        tracing::warn!("using the explicitly requested unauthenticated usbip CLI fallback");
        return attach_via_cli(&hello, audio, label);
    }
    attach_in_process(&hello, audio, label)
}

fn accept_expected_client(
    listener: &std::net::TcpListener,
    expected: std::net::SocketAddr,
) -> Result<std::net::TcpStream> {
    loop {
        let (candidate, peer) = listener.accept().context("accept usbip connection")?;
        if peer == expected {
            return Ok(candidate);
        }
        tracing::warn!(%peer, "refusing a local process that raced the usbip importer");
    }
}

/// A connected loopback `(client, server)`; `vhci_hcd` takes the client. Connect first, then
/// accept that 4-tuple: a racer can hit the port, it cannot own our tuple.
pub fn loopback_pair() -> Result<(TcpStream, TcpStream)> {
    // Port 0: do not contend USBIP_TCP_PORT with another pad.
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("bind loopback usbip server")?;
    let port = listener
        .local_addr()
        .context("usbip server local_addr")?
        .port();
    let sock = connect_loopback(port).context("connect to usbip server")?;
    let expected = sock.local_addr().context("usbip client local_addr")?;
    let server = accept_expected_client(&listener, expected)?;
    Ok((sock, server))
}

fn attach_in_process(
    hello: &Hello,
    audio: Option<SyncSender<Vec<f32>>>,
    label: &str,
) -> Result<UsbipAttachment> {
    let (mut sock, server_sock) = loopback_pair()?;
    let (relay, pad) = crate::pad_broker::relay_pair()?;
    let device = served(hello, Outputs::new(&relay)?)?;
    let server = ServerThread::spawn(ServerEndpoint::Connected(server_sock), device, relay, label)?;
    let (devid, speed) =
        import_handshake(&mut sock, IMPORT_WAIT).context("usbip import handshake")?;
    let vhci_port = vhci_find_free_port(speed).context("find a free vhci port")?;
    vhci_attach(vhci_port, sock.as_raw_fd(), devid, speed).context("write vhci_hcd attach")?;
    tracing::info!(
        label,
        vhci_port,
        "attached via usbip (in-process — Steam Input recognizes it)"
    );
    UsbipAttachment::linked(
        vhci_port,
        Some(sock),
        Some(server),
        false,
        UnixDatagram::from(pad),
        audio,
    )
}

/// A seat's pad server: reads the [`Hello`] the supervisor put first on `relay`, then serves
/// that device on `sock` until the host or the pad hangs up.
pub fn serve(sock: TcpStream, relay: UnixDatagram) -> Result<()> {
    relay
        .set_read_timeout(Some(HELLO_WAIT))
        .context("time the pad's hello")?;
    let mut buf = vec![0u8; MAX_IDENTITY + 8];
    let n = relay.recv(&mut buf).context("read the pad's hello")?;
    relay.set_read_timeout(None).context("untime the relay")?;
    let hello = Hello::decode(&buf[..n]).context("not a pad hello")?;
    let label = format!("seat {} {}", hello.kind.label(), hello.index);
    let device = served(&hello, Outputs::new(&relay)?)?;
    ServerThread::spawn(ServerEndpoint::Connected(sock), device, relay, &label)?.wait();
    Ok(())
}

/// CLI attach: listen on [`USBIP_TCP_PORT`], recover the vhci port by diffing sysfs status.
fn attach_via_cli(
    hello: &Hello,
    audio: Option<SyncSender<Vec<f32>>>,
    label: &str,
) -> Result<UsbipAttachment> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", USBIP_TCP_PORT))
        .with_context(|| format!("bind usbip default port {USBIP_TCP_PORT} for CLI attach"))?;
    listener
        .set_nonblocking(true)
        .context("usbip listener set_nonblocking")?;
    let (relay, pad) = crate::pad_broker::relay_pair()?;
    let device = served(hello, Outputs::new(&relay)?)?;
    let server = ServerThread::spawn(ServerEndpoint::Listener(listener), device, relay, label)?;

    let before = vhci_used_ports();
    usbip_attach_cli().context("usbip CLI attach")?;
    let vhci_port =
        wait_for_new_port(&before).context("determine the vhci port the usbip CLI attached to")?;

    tracing::info!(
        label,
        vhci_port,
        "attached via usbip (CLI — Steam Input recognizes it)"
    );
    UsbipAttachment::linked(
        vhci_port,
        None,
        Some(server),
        false,
        UnixDatagram::from(pad),
        audio,
    )
}

// ---- USB/IP import handshake (we are the usbip client until the fd is handed to the kernel) ----

const USBIP_VERSION: u16 = 0x0111;
const OP_REQ_IMPORT: u16 = 0x8003;

/// Retry ~500 ms while the server thread comes up.
fn connect_loopback(port: u16) -> Result<TcpStream> {
    let addr = ("127.0.0.1", port);
    let mut last = None;
    for _ in 0..50 {
        match TcpStream::connect(addr) {
            Ok(s) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    Err(anyhow::anyhow!(
        "connect 127.0.0.1:{port}: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// `OP_REQ_IMPORT` for [`BUS_ID`]. Consume the full 320-byte reply so the kernel's
/// first `USBIP_CMD_SUBMIT` sees a clean socket. Returns `(bus_num<<16 | dev_num, speed)`.
/// `wait` caps each step, so a wedged server cannot head-block the caller.
pub fn import_handshake(sock: &mut TcpStream, wait: Duration) -> Result<(u32, u32)> {
    sock.set_read_timeout(Some(wait)).ok();
    sock.set_write_timeout(Some(wait)).ok();

    let mut req = Vec::with_capacity(40);
    req.extend_from_slice(&USBIP_VERSION.to_be_bytes());
    req.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
    req.extend_from_slice(&0u32.to_be_bytes()); // status
    let mut busid = [0u8; 32];
    let b = BUS_ID.as_bytes();
    busid[..b.len()].copy_from_slice(b);
    req.extend_from_slice(&busid);
    sock.write_all(&req).context("send OP_REQ_IMPORT")?;

    // Header: version(2) code(2) status(4); then 312-byte device record.
    let mut header = [0u8; 8];
    sock.read_exact(&mut header)
        .context("read OP_REP_IMPORT header")?;
    let status = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
    if status != 0 {
        bail!("OP_REP_IMPORT refused (status={status}) — device {BUS_ID} not exported?");
    }
    let mut dev = [0u8; 312];
    sock.read_exact(&mut dev)
        .context("read OP_REP_IMPORT device record")?;
    // Device record layout: path[256], bus_id[32], bus_num(4 BE)@288, dev_num(4 BE)@292, speed(4)@296.
    let be = |o: usize| u32::from_be_bytes([dev[o], dev[o + 1], dev[o + 2], dev[o + 3]]);
    let bus_num = be(288);
    let dev_num = be(292);
    let speed = be(296);
    // Kernel vhci rx/tx honour SO_RCVTIMEO/SO_SNDTIMEO; handshake timeouts would idle-kill the device.
    sock.set_read_timeout(None).ok();
    sock.set_write_timeout(None).ok();
    Ok(((bus_num << 16) | dev_num, speed))
}

// ---- vhci_hcd sysfs plumbing ----

pub fn ensure_modules() {
    let _ = Command::new("modprobe").arg("vhci_hcd").status();
}

/// `usbip attach`; 6 s cap so a hung CLI cannot head-block the input thread.
fn usbip_attach_cli() -> Result<()> {
    let mut child = Command::new("usbip")
        .args(["attach", "-r", "127.0.0.1", "-b", BUS_ID])
        .spawn()
        .context("spawn `usbip attach` (is usbip-utils installed?)")?;
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match child.try_wait().context("wait on `usbip attach`")? {
            Some(st) if st.success() => return Ok(()),
            Some(st) => bail!("`usbip attach` exited with {st}"),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("`usbip attach` timed out (>6s) — killed");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// `vhci_hcd.0` or legacy `vhci_hcd`. Shared with [`crate::vhci_probe`] so the paths cannot drift.
pub fn vhci_base() -> Option<PathBuf> {
    for p in [
        "/sys/devices/platform/vhci_hcd.0",
        "/sys/devices/platform/vhci_hcd",
    ] {
        let base = Path::new(p);
        if base.join("status").exists() {
            return Some(base.to_path_buf());
        }
    }
    None
}

fn read_status() -> Result<String> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    std::fs::read_to_string(base.join("status")).context("read vhci_hcd status")
}

/// Parse one `status` row. Modern `hub port sta …` and legacy `port sta …`; `None` for headers.
fn parse_status_row(line: &str) -> Option<(u16, bool, u32)> {
    let t: Vec<&str> = line.split_whitespace().collect();
    if t.is_empty() {
        return None;
    }
    let (hub_ss, port_str, sta_str) = if t[0] == "hs" || t[0] == "ss" {
        (Some(t[0] == "ss"), *t.get(1)?, *t.get(2)?)
    } else if t[0].chars().all(|c| c.is_ascii_digit()) {
        (None, t[0], *t.get(1)?) // legacy: port sta …
    } else {
        return None; // header ("hub"/"prt"/"port" …)
    };
    let port = port_str.parse::<u16>().ok()?;
    let sta = sta_str.parse::<u32>().ok()?;
    Some((port, hub_ss.unwrap_or(false), sta))
}

/// Kernel `VDEV_ST_NULL`: a free vhci port.
const VDEV_ST_NULL: u32 = 4;

/// Whether `port` is in use on socket `fd`. An unreadable `status` answers yes, the old detach.
pub fn vhci_port_holds(port: u16, fd: RawFd) -> bool {
    read_status().map_or(true, |s| s.lines().any(|l| row_holds(l, port, fd)))
}

/// One `status` row is `port`, in use, on socket `fd` (its `sockfd` column).
fn row_holds(line: &str, port: u16, fd: RawFd) -> bool {
    used_row(line).is_some_and(|(p, sockfd, _)| p == port && sockfd == fd)
}

/// `(port, sockfd, busid)` of every port in use. The sockfd is the attaching process's number.
pub fn vhci_used_rows() -> Vec<(u16, RawFd, String)> {
    read_status()
        .unwrap_or_default()
        .lines()
        .filter_map(used_row)
        .collect()
}

/// One row in use. A port attached but not yet addressed shows sockfd 0 and busid `0-0`.
fn used_row(line: &str) -> Option<(u16, RawFd, String)> {
    let (port, _, sta) = parse_status_row(line)?;
    if sta == VDEV_ST_NULL {
        return None;
    }
    let t: Vec<&str> = line.split_whitespace().collect();
    let at = if matches!(t.first(), Some(&"hs") | Some(&"ss")) {
        5
    } else {
        4
    };
    Some((port, t.get(at)?.parse().ok()?, (*t.get(at + 1)?).to_owned()))
}

/// Free ports in `status` of the speed class a device at `usbip_speed` takes (`>= 5` is
/// SuperSpeed), in order. A legacy single-hub `status` reads as all high speed.
fn free_ports(status: &str, usbip_speed: u32) -> Vec<u16> {
    let want_ss = usbip_speed >= 5;
    status
        .lines()
        .filter_map(parse_status_row)
        .filter(|&(_, is_ss, sta)| sta == VDEV_ST_NULL && is_ss == want_ss)
        .map(|(port, _, _)| port)
        .collect()
}

/// How many free ports a device at `usbip_speed` could take now.
pub fn vhci_free_ports(usbip_speed: u32) -> Result<usize> {
    Ok(free_ports(&read_status()?, usbip_speed).len())
}

/// Free port matching speed (`usbip_speed >= 5` is SuperSpeed).
pub fn vhci_find_free_port(usbip_speed: u32) -> Result<u16> {
    let status = read_status()?;
    if let Some(&port) = free_ports(&status, usbip_speed).first() {
        return Ok(port);
    }
    // Legacy single-hub status has no speed class: take any free port.
    for line in status.lines() {
        if let Some((port, _, sta)) = parse_status_row(line) {
            if sta == VDEV_ST_NULL {
                return Ok(port);
            }
        }
    }
    bail!("no free vhci_hcd port (all ports in use?)")
}

/// Occupied ports; snapshotted around CLI attach to recover its port.
fn vhci_used_ports() -> HashSet<u16> {
    read_status()
        .unwrap_or_default()
        .lines()
        .filter_map(parse_status_row)
        .filter(|&(_, _, sta)| sta != VDEV_ST_NULL)
        .map(|(port, _, _)| port)
        .collect()
}

/// Wait up to 2 s for a port that became used since `before`.
fn wait_for_new_port(before: &HashSet<u16>) -> Result<u16> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(p) = vhci_used_ports().difference(before).copied().min() {
            return Ok(p);
        }
        if Instant::now() >= deadline {
            bail!("no newly-attached vhci port appeared after `usbip attach`");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub fn vhci_attach(port: u16, sockfd: i32, devid: u32, speed: u32) -> Result<()> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    let line = format!("{port} {sockfd} {devid} {speed}");
    std::fs::write(base.join("attach"), line)
        .with_context(|| format!("write vhci_hcd attach (port {port}) — root?"))
}

pub fn vhci_detach(port: u16) -> Result<()> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    std::fs::write(base.join("detach"), format!("{port}")).context("write vhci_hcd detach")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Modern and legacy `status` layouts; a miss here attaches to a busy port.
    #[test]
    fn status_parser_handles_both_layouts() {
        // modern
        assert_eq!(
            parse_status_row("hs  0000 004 000 00000000 000000 0-0"),
            Some((0, false, 4))
        );
        assert_eq!(
            parse_status_row("ss  0008 006 000 00000000 000000 0-0"),
            Some((8, true, 6))
        );
        // legacy (no hub column)
        assert_eq!(
            parse_status_row("0001 004 000 00000000 000000 0-0"),
            Some((1, false, 4))
        );
        // header / blank
        assert_eq!(
            parse_status_row("hub port sta spd dev      sockfd local_busid"),
            None
        );
        assert_eq!(parse_status_row(""), None);
        // Ownership is the sockfd column: a freed port, or one reused on another socket, is not ours.
        assert!(row_holds("hs  0000 006 002 00010002 000017 1-1", 0, 17));
        assert!(!row_holds("hs  0000 006 002 00010002 000018 1-1", 0, 17));
        assert!(!row_holds("hs  0000 004 000 00000000 000000 0-0", 0, 0));
        assert!(row_holds("0001 006 002 00010002 000017 1-1", 1, 17));
        assert_eq!(
            used_row("ss  0008 006 005 000b0002 000009 12-1"),
            Some((8, 9, "12-1".into()))
        );
        assert_eq!(used_row("hs  0001 004 000 00000000 000000 0-0"), None);
    }

    /// A full- or high-speed pad counts and takes only the high-speed hub's ports.
    #[test]
    fn free_port_selection_matches_speed() {
        let status = "hub port sta spd dev      sockfd local_busid\n\
                      hs  0000 006 000 00000000 000000 0-0\n\
                      hs  0001 004 000 00000000 000000 0-0\n\
                      hs  0002 004 000 00000000 000000 0-0\n\
                      ss  0008 004 000 00000000 000000 0-0\n";
        assert_eq!(free_ports(status, 2), [1, 2], "high speed");
        assert_eq!(free_ports(status, 1), [1, 2], "full speed");
        assert_eq!(free_ports(status, 5), [8], "SuperSpeed");
    }

    /// The pad's reader turns a server's S16 into `f32`, and drops rather than blocks when
    /// the streamer stalls.
    #[test]
    fn audio_reaches_the_streamer_as_f32_and_a_stall_drops_it() {
        let (server, pad) = crate::pad_broker::relay_pair().unwrap();
        let pad = UnixDatagram::from(pad);
        let mut raw = Vec::new();
        for s in [i16::MAX, 0, 0, i16::MIN, 0, 0, 0, 0] {
            raw.extend_from_slice(&s.to_le_bytes());
        }
        for _ in 0..8 {
            server.send(&Output::Audio(raw.clone()).encode()).unwrap();
        }
        server
            .send(
                &Output::Report {
                    iface: 3,
                    set: None,
                    data: vec![2],
                }
                .encode(),
            )
            .unwrap();
        drop(server);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<f32>>(1);
        let pending = Mutex::default();
        read_outputs(&pad, &pending, Some(&tx));
        let pcm = rx.try_recv().expect("one decoded chunk");
        assert!(rx.try_recv().is_err(), "the rest were dropped");
        assert_eq!(pcm.len(), 8, "2 frames x 4 channels");
        assert!(
            (pcm[0] - 0.999_97).abs() < 1e-4,
            "ch0 full scale: {}",
            pcm[0]
        );
        assert_eq!(pcm[3], -1.0, "ch3 full scale negative");
        assert_eq!(pending.into_inner().unwrap().len(), 1, "the report queued");
    }

    #[test]
    fn in_process_import_accepts_only_its_own_tcp_tuple() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let _racer = std::net::TcpStream::connect(addr).unwrap();
        let importer = std::net::TcpStream::connect(addr).unwrap();
        let expected = importer.local_addr().unwrap();
        let accepted = accept_expected_client(&listener, expected).unwrap();
        assert_eq!(accepted.peer_addr().unwrap(), expected);
    }

    /// One USB/IP connection, then the loopback port closes. No root / `vhci_hcd`.
    #[test]
    fn usbip_server_serves_one_connection_then_closes_the_port() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        // The pad's end stays open: a relay that hangs up ends the server too.
        let (relay, _pad) = crate::pad_broker::relay_pair().unwrap();
        let served = Served {
            device: usbip_sim::UsbDevice::new(0),
            input: Box::new(|_| {}),
        };
        let server = ServerThread::spawn(
            ServerEndpoint::Listener(listener),
            served,
            relay,
            "test pad",
        )
        .expect("spawn the emulation server");

        // Held open like the kernel's vhci fd.
        let _kernel = connect_loopback(port).expect("the attach connects");

        // Accept races connect (backlog); the port closes only after the server picks it up.
        let mut refused = false;
        for _ in 0..100 {
            match TcpStream::connect(("127.0.0.1", port)) {
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => {
                    refused = true;
                    break;
                }
            }
        }
        assert!(
            refused,
            "a second USB/IP importer must find the port closed"
        );
        drop(server);
    }
}
