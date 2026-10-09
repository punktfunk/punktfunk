//! USB/IP attach on in-tree `vhci_hcd`, shared by every usbip pad.
//!
//! [`usbip_sim`] emulates the device on a dedicated server thread and the local host
//! imports it. Attach is in-process: the host preconnects and accepts only that TCP
//! 4-tuple before handing the socket to `vhci_hcd`. `PUNKTFUNK_USBIP_ATTACH=cli` is an
//! unauthenticated debug override. Callers degrade to UHID on failure.

use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use usbip_sim::{UsbDevice, UsbEndpoint, UsbInterfaceHandler, UsbIpServer};

/// One device per server, so the usbip default bus id is enough.
const BUS_ID: &str = "0-0-0";
/// usbip CLI default; [`attach_via_cli`] must listen here.
const USBIP_TCP_PORT: u16 = 3240;

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
    fn spawn(endpoint: ServerEndpoint, dev: UsbDevice, label: &str) -> Result<ServerThread> {
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
                rt.block_on(run_server(
                    endpoint,
                    Arc::new(UsbIpServer::new_simulated(vec![dev])),
                    stop_t,
                    label,
                ));
            })
            .context("spawn usbip server thread")?;
        Ok(ServerThread {
            stop,
            join: Some(join),
        })
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
async fn run_server(
    endpoint: ServerEndpoint,
    server: Arc<UsbIpServer>,
    stop: Arc<tokio::sync::Notify>,
    label: String,
) {
    let mut sock = match endpoint {
        ServerEndpoint::Connected(sock) => match tokio::net::TcpStream::from_std(sock) {
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
    let conn = tokio::spawn(async move {
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
    }
}

/// Drop detaches the `vhci_hcd` port first so the kernel tears the device down
/// before the socket and server go.
pub(crate) struct UsbipAttachment {
    vhci_port: u16,
    /// Holds the fd handed to `vhci_hcd`. CLI attach is `None` — the CLI already passed its fd.
    _client_sock: Option<TcpStream>,
    _server: ServerThread,
}

impl Drop for UsbipAttachment {
    fn drop(&mut self) {
        // The kernel frees a port when its connection dies, and another pad can hold it by now:
        // detach only while it still carries our socket. A CLI attach has no fd to compare.
        if let Some(sock) = &self._client_sock {
            if !vhci_port_holds(self.vhci_port, sock.as_raw_fd()) {
                tracing::debug!(
                    port = self.vhci_port,
                    "vhci port no longer ours — not detaching"
                );
                return;
            }
        }
        if let Err(e) = vhci_detach(self.vhci_port) {
            tracing::debug!(port = self.vhci_port, error = %e, "vhci detach failed (device may already be gone)");
        }
    }
}

/// In-process attach. `PUNKTFUNK_USBIP_ATTACH=cli` selects the unauthenticated CLI fallback.
pub(crate) fn attach_device(build: impl Fn() -> UsbDevice, label: &str) -> Result<UsbipAttachment> {
    ensure_modules();
    if vhci_base().is_none() {
        bail!("vhci_hcd unavailable (no /sys/devices/platform/vhci_hcd*/status) — is it loaded?");
    }
    let mode = std::env::var("PUNKTFUNK_USBIP_ATTACH").ok();
    if mode.as_deref() == Some("cli") {
        tracing::warn!("using the explicitly requested unauthenticated usbip CLI fallback");
        return attach_via_cli(build(), label);
    }
    attach_in_process(build(), label)
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

fn attach_in_process(dev: UsbDevice, label: &str) -> Result<UsbipAttachment> {
    // Port 0: do not contend USBIP_TCP_PORT with another pad.
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("bind loopback usbip server")?;
    let port = listener
        .local_addr()
        .context("usbip server local_addr")?
        .port();
    // Connect first, then accept that 4-tuple. A racer can hit the port; it cannot own our tuple.
    let mut sock = connect_loopback(port).context("connect to usbip server")?;
    let expected = sock.local_addr().context("usbip client local_addr")?;
    let server_sock = accept_expected_client(&listener, expected)?;
    server_sock
        .set_nonblocking(true)
        .context("usbip server socket set_nonblocking")?;
    let server = ServerThread::spawn(ServerEndpoint::Connected(server_sock), dev, label)?;
    let (devid, speed) = import_handshake(&mut sock).context("usbip import handshake")?;

    // Kernel vhci rx/tx honour SO_RCVTIMEO/SO_SNDTIMEO; handshake timeouts would idle-kill the device.
    let vhci_port = vhci_find_free_port(speed).context("find a free vhci port")?;
    sock.set_read_timeout(None).ok();
    sock.set_write_timeout(None).ok();
    vhci_attach(vhci_port, sock.as_raw_fd(), devid, speed).context("write vhci_hcd attach")?;

    tracing::info!(
        label,
        vhci_port,
        "attached via usbip (in-process — Steam Input recognizes it)"
    );
    Ok(UsbipAttachment {
        vhci_port,
        _client_sock: Some(sock),
        _server: server,
    })
}

/// CLI attach: listen on [`USBIP_TCP_PORT`], recover the vhci port by diffing sysfs status.
fn attach_via_cli(dev: UsbDevice, label: &str) -> Result<UsbipAttachment> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", USBIP_TCP_PORT))
        .with_context(|| format!("bind usbip default port {USBIP_TCP_PORT} for CLI attach"))?;
    listener
        .set_nonblocking(true)
        .context("usbip listener set_nonblocking")?;
    let server = ServerThread::spawn(ServerEndpoint::Listener(listener), dev, label)?;

    let before = vhci_used_ports();
    usbip_attach_cli().context("usbip CLI attach")?;
    let vhci_port =
        wait_for_new_port(&before).context("determine the vhci port the usbip CLI attached to")?;

    tracing::info!(
        label,
        vhci_port,
        "attached via usbip (CLI — Steam Input recognizes it)"
    );
    Ok(UsbipAttachment {
        vhci_port,
        _client_sock: None,
        _server: server,
    })
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
fn import_handshake(sock: &mut TcpStream) -> Result<(u32, u32)> {
    // 1 s cap so a wedged handshake cannot head-block the input thread.
    sock.set_read_timeout(Some(Duration::from_secs(1))).ok();
    sock.set_write_timeout(Some(Duration::from_secs(1))).ok();

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
pub(crate) fn vhci_base() -> Option<PathBuf> {
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
fn vhci_port_holds(port: u16, fd: std::os::fd::RawFd) -> bool {
    read_status().map_or(true, |s| s.lines().any(|l| row_holds(l, port, fd)))
}

/// One `status` row is `port`, in use, on socket `fd` (its `sockfd` column).
fn row_holds(line: &str, port: u16, fd: std::os::fd::RawFd) -> bool {
    let t: Vec<&str> = line.split_whitespace().collect();
    let sockfd = match t.first() {
        Some(&"hs") | Some(&"ss") => t.get(5),
        _ => t.get(4),
    };
    parse_status_row(line).is_some_and(|(p, _, sta)| p == port && sta != VDEV_ST_NULL)
        && sockfd.and_then(|s| s.parse::<std::os::fd::RawFd>().ok()) == Some(fd)
}

/// Free port matching speed (`usbip_speed >= 5` is SuperSpeed).
fn vhci_find_free_port(usbip_speed: u32) -> Result<u16> {
    let want_ss = usbip_speed >= 5;
    let status = read_status()?;
    for line in status.lines() {
        if let Some((port, is_ss, sta)) = parse_status_row(line) {
            if sta == VDEV_ST_NULL && is_ss == want_ss {
                return Ok(port);
            }
        }
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

fn vhci_attach(port: u16, sockfd: i32, devid: u32, speed: u32) -> Result<()> {
    let base = vhci_base().context("vhci_hcd sysfs not present")?;
    let line = format!("{port} {sockfd} {devid} {speed}");
    std::fs::write(base.join("attach"), line)
        .with_context(|| format!("write vhci_hcd attach (port {port}) — root?"))
}

fn vhci_detach(port: u16) -> Result<()> {
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
    }

    #[test]
    fn free_port_selection_matches_speed() {
        let status = "hub port sta spd dev      sockfd local_busid\n\
                      hs  0000 006 000 00000000 000000 0-0\n\
                      hs  0001 004 000 00000000 000000 0-0\n\
                      ss  0008 004 000 00000000 000000 0-0\n";
        // `vhci_find_free_port` reads sysfs; test the selection against a fixture.
        let hs = status
            .lines()
            .filter_map(parse_status_row)
            .find(|&(_, is_ss, sta)| sta == VDEV_ST_NULL && !is_ss)
            .map(|(p, _, _)| p);
        let ss = status
            .lines()
            .filter_map(parse_status_row)
            .find(|&(_, is_ss, sta)| sta == VDEV_ST_NULL && is_ss)
            .map(|(p, _, _)| p);
        assert_eq!(hs, Some(1));
        assert_eq!(ss, Some(8));
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
        let server = ServerThread::spawn(
            ServerEndpoint::Listener(listener),
            UsbDevice::new(0),
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
