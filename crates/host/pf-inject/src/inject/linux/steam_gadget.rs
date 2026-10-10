//! Virtual Steam Deck over the USB gadget subsystem (`raw_gadget` + `dummy_hcd`).
//!
//! Steam Input promotes a Deck only on USB interface 2. UHID has no interface
//! number (`Interface: -1`), so it enumerates but never promotes. This backend
//! presents a 3-interface Deck (mouse 0, keyboard 1, controller 2) on a
//! `dummy_hcd` loopback UDC and answers every control transfer from userspace
//! via `/dev/raw-gadget`, including HID feature reports `f_hid` cannot.
//! Descriptors are captured from a physical Deck; see
//! `packaging/linux/steam-deck-gadget/` for the PoC and USB-stack traps.
//!
//! SteamOS-host only: needs `dummy_hcd` + `raw_gadget`. Report bytes come from
//! [`super::steam_proto`]. The Secure-Boot-clean alternative is
//! [`super::steam_usbip`].

use crate::uapi;
use anyhow::{Context, Result};
use std::fs::File;
use std::mem::size_of;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

// linux/usb/raw_gadget.h, inlined; this crate has no kernel headers.
const UDC_NAME_MAX: usize = 128;

#[repr(C)]
struct UsbRawInit {
    driver_name: [u8; UDC_NAME_MAX],
    device_name: [u8; UDC_NAME_MAX],
    speed: u8,
}

const EVENT_HDR: usize = 8; // usb_raw_event: type + length; data[] follows
const EVENT_BUF: usize = EVENT_HDR + 64; // 64 holds the 8-byte setup packet

const EPIO_HDR: usize = 8; // usb_raw_ep_io: ep, flags, length; data[] follows

// Kernel EP_ENABLE wants the 9-byte audio form (bRefresh/bSynchAddress), not USB's 7-byte EP desc.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct UsbEndpointDescriptor {
    b_length: u8,
    b_descriptor_type: u8,
    b_endpoint_address: u8,
    bm_attributes: u8,
    w_max_packet_size: u16,
    b_interval: u8,
    b_refresh: u8,
    b_synch_address: u8,
}

// SAFETY: byte arrays and a byte; no padding.
unsafe impl uapi::Pod for UsbRawInit {}
// SAFETY: `packed` integers, so no padding.
unsafe impl uapi::Pod for UsbEndpointDescriptor {}

const fn ioc(dir: u64, nr: u64, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u64) << 16) | ((b'U' as u64) << 8) | nr) as libc::c_ulong
}
const IOCTL_INIT: libc::c_ulong = ioc(1, 0, size_of::<UsbRawInit>());
const IOCTL_RUN: libc::c_ulong = ioc(0, 1, 0);
const IOCTL_EVENT_FETCH: libc::c_ulong = ioc(2, 2, EVENT_HDR); // size is the header; kernel copies more
const IOCTL_EP0_WRITE: libc::c_ulong = ioc(1, 3, EPIO_HDR);
const IOCTL_EP0_READ: libc::c_ulong = ioc(2 | 1, 4, EPIO_HDR); // _IOWR: kernel writes the payload into our buffer
const IOCTL_EP_ENABLE: libc::c_ulong = ioc(1, 5, size_of::<UsbEndpointDescriptor>());
const IOCTL_EP_WRITE: libc::c_ulong = ioc(1, 7, EPIO_HDR);
const IOCTL_CONFIGURE: libc::c_ulong = ioc(0, 9, 0);
const IOCTL_VBUS_DRAW: libc::c_ulong = ioc(1, 10, 4);
const IOCTL_EP0_STALL: libc::c_ulong = ioc(0, 12, 0);

const USB_RAW_EVENT_CONNECT: u32 = 1;
const USB_RAW_EVENT_CONTROL: u32 = 2;
const USB_SPEED_HIGH: u8 = 3;

use super::steam_proto::{
    deck_serial, feature_reply, neutral_deck_report, RDESC_DECK_CTRL as RDESC_CTRL,
    RDESC_DECK_KBD as RDESC_KBD, RDESC_DECK_MOUSE as RDESC_MOUSE,
};

// USB device descriptor: Valve 28DE:1205, bcdUSB 2.00, bcdDevice 3.00.
const DEV_DESC: [u8; 18] = [
    18, 1, 0x00, 0x02, 0, 0, 0, 64, 0xDE, 0x28, 0x05, 0x12, 0x00, 0x03, 1, 2, 3, 1,
];

const HID_DT: u8 = 0x21;
const HID_RPT_DT: u8 = 0x22;

fn build_config() -> Vec<u8> {
    let mut c = Vec::with_capacity(84);
    c.extend_from_slice(&[9, 2, 84, 0, 3, 1, 0, 0x80, 250]);
    let iface = |n: u8, sub: u8, proto: u8| [9u8, 4, n, 0, 1, 3, sub, proto, 0];
    let hid = |rlen: u16, country: u8| {
        [
            9u8,
            HID_DT,
            0x10,
            0x01,
            country,
            1,
            HID_RPT_DT,
            (rlen & 0xff) as u8,
            (rlen >> 8) as u8,
        ]
    };
    let ep = |addr: u8, mps: u16| [7u8, 5, addr, 0x03, (mps & 0xff) as u8, (mps >> 8) as u8, 4];
    // 3-interface Deck: mouse 0x81, boot kbd 0x82, controller 0x83.
    // Steam Input filters on iface 2. Country 33 matches a physical Deck.
    c.extend_from_slice(&iface(0, 0, 2));
    c.extend_from_slice(&hid(RDESC_MOUSE.len() as u16, 0));
    c.extend_from_slice(&ep(0x81, 8));
    c.extend_from_slice(&iface(1, 1, 1));
    c.extend_from_slice(&hid(RDESC_KBD.len() as u16, 0));
    c.extend_from_slice(&ep(0x82, 8));
    c.extend_from_slice(&iface(2, 0, 0));
    c.extend_from_slice(&hid(RDESC_CTRL.len() as u16, 33));
    c.extend_from_slice(&ep(0x83, 64));
    debug_assert_eq!(c.len(), 84);
    c
}

fn string_desc(idx: u8, serial: &str) -> Vec<u8> {
    if idx == 0 {
        return vec![4, 3, 0x09, 0x04]; // LANGID 0x0409 en-US
    }
    let s: &str = match idx {
        1 => "Valve Software",
        2 => "Steam Deck Controller",
        3 => serial,
        _ => "",
    };
    let mut v = vec![(2 + s.len() * 2) as u8, 3];
    for ch in s.encode_utf16() {
        v.push((ch & 0xff) as u8);
        v.push((ch >> 8) as u8);
    }
    v
}

/// One raw_gadget transfer or event fetch. `buf` is the 8-byte header (`usb_raw_ep_io` or
/// `usb_raw_event`, payload length at bytes 4..8) followed by the payload the kernel copies
/// in or out. Panics unless `buf` holds that payload. Negative on error, as `ioctl`.
fn transfer(fd: BorrowedFd<'_>, req: libc::c_ulong, buf: &mut [u8]) -> i32 {
    let len = u32::from_ne_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    assert!(
        buf.len() >= EPIO_HDR + len,
        "raw_gadget payload overruns its buffer"
    );
    // SAFETY: `req` reads the header, then copies at most its `len` payload bytes to or from
    // the bytes right after it, all inside `buf`, which outlives the synchronous call.
    unsafe { libc::ioctl(fd.as_raw_fd(), req as _, buf.as_mut_ptr()) }
}

fn ep0_write(fd: BorrowedFd<'_>, data: &[u8]) -> i32 {
    let mut buf = vec![0u8; EPIO_HDR + data.len()];
    buf[0..2].copy_from_slice(&0u16.to_ne_bytes());
    buf[4..8].copy_from_slice(&(data.len() as u32).to_ne_bytes());
    buf[EPIO_HDR..].copy_from_slice(data);
    transfer(fd, IOCTL_EP0_WRITE, &mut buf)
}
fn ep0_read(fd: BorrowedFd<'_>, len: usize) -> (i32, Vec<u8>) {
    let mut buf = vec![0u8; EPIO_HDR + len.max(1)];
    buf[4..8].copy_from_slice(&(len as u32).to_ne_bytes());
    let r = transfer(fd, IOCTL_EP0_READ, &mut buf);
    let n = if r > 0 { r as usize } else { 0 };
    (r, buf[EPIO_HDR..EPIO_HDR + n.min(len.max(1))].to_vec())
}
/// Status stage of a no-data OUT is an IN; a zero-length `EP0_READ` completes it.
fn ep0_ack(fd: BorrowedFd<'_>) {
    ep0_read(fd, 0);
}
/// raw_gadget fails a no-argument request whose value is not 0.
fn ep0_stall(fd: BorrowedFd<'_>) {
    let _ = uapi::ioctl_value(fd, IOCTL_EP0_STALL, 0);
}

/// Wakes a worker blocked in `EVENT_FETCH`/`EP_WRITE` at teardown.
///
/// Those ioctls are `wait_event_interruptible` with no timeout and no `O_NONBLOCK`.
/// Close cannot wake: the in-flight syscall holds a `struct file` ref. A no-op
/// non-`SA_RESTART` `SIGUSR1` returns `EINTR`; the loop then sees `running`.
/// `SIGUSR1` is unused in this process, so a stray delivery is harmless.
const WAKE_SIGNAL: libc::c_int = libc::SIGUSR1;

/// No-op `WAKE_SIGNAL` handler, `sa_flags = 0` (no `SA_RESTART`) so the ioctl returns `EINTR`.
fn install_wake_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        extern "C" fn noop(_: libc::c_int) {}
        // SAFETY: well-formed `sigaction`, empty mask, valid no-op handler.
        // Touches only this process's `WAKE_SIGNAL` disposition.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            // `sa_sigaction` is a `usize` slot. Function-item → integer trips
            // `clippy::function_casts_as_integer`; hop via `*const ()`.
            sa.sa_sigaction = noop as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(WAKE_SIGNAL, &sa, std::ptr::null_mut());
        }
    });
}

/// `Drop` signals a worker parked in a blocking ioctl.
/// `tid` is `pthread_self()` (0 until start); `done` is set just before return.
struct Waker {
    tid: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
}

/// Drop stops the workers and closes the fd; the kernel then tears the device down.
pub struct SteamDeckGadget {
    report: Arc<Mutex<[u8; 64]>>,
    feedback: Arc<Mutex<super::steam_proto::SteamFeedback>>,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    wakers: Vec<Waker>,
    /// Closing `/dev/raw-gadget` tears the gadget down.
    _fd: Arc<File>,
    enc: super::steam_proto::DeckEncoder,
}

impl SteamDeckGadget {
    /// Bind a Deck on `dummy_udc.0`. `index` only changes the serial.
    /// Needs `dummy_hcd` + `raw_gadget` and write access to `/dev/raw-gadget`.
    pub fn open(index: u8) -> Result<SteamDeckGadget> {
        // std opens CLOEXEC: a spawned game must not inherit the gadget and keep it bound
        // past our drop.
        let fd = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/raw-gadget")
            .context("open /dev/raw-gadget (is raw_gadget+dummy_hcd loaded, are we root?)")?;
        let fd = Arc::new(fd);

        let mut init = UsbRawInit {
            driver_name: [0; UDC_NAME_MAX],
            device_name: [0; UDC_NAME_MAX],
            speed: USB_SPEED_HIGH,
        };
        copy_cstr(&mut init.driver_name, "dummy_udc");
        copy_cstr(&mut init.device_name, "dummy_udc.0");
        uapi::ioctl_with(fd.as_fd(), IOCTL_INIT, &mut init).context("raw_gadget INIT")?;
        uapi::ioctl_value(fd.as_fd(), IOCTL_RUN, 0).context("raw_gadget RUN")?;

        let serial = deck_serial(index);
        let report = Arc::new(Mutex::new(neutral_deck_report()));
        let feedback = Arc::new(Mutex::new(Default::default()));
        let running = Arc::new(AtomicBool::new(true));
        let ctrl_ep = Arc::new(std::sync::atomic::AtomicI32::new(-1));
        let configured = Arc::new(AtomicBool::new(false));

        // Handler must exist before a worker can park in a blocking ioctl.
        install_wake_handler();
        let ctrl_waker = Waker {
            tid: Arc::new(AtomicU64::new(0)),
            done: Arc::new(AtomicBool::new(false)),
        };
        let stream_waker = Waker {
            tid: Arc::new(AtomicU64::new(0)),
            done: Arc::new(AtomicBool::new(false)),
        };

        let control = {
            let fd = fd.clone();
            let running = running.clone();
            let ctrl_ep = ctrl_ep.clone();
            let configured = configured.clone();
            let feedback = feedback.clone();
            let tid = ctrl_waker.tid.clone();
            let done = ctrl_waker.done.clone();
            std::thread::Builder::new()
                .name("pf-deck-gadget-ctrl".into())
                .spawn(move || {
                    // SAFETY: `pthread_self` is always valid on the calling thread.
                    tid.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
                    control_loop(fd, running, ctrl_ep, configured, feedback, serial);
                    done.store(true, Ordering::SeqCst);
                })
                .context("spawn gadget control thread")?
        };
        // Built before the second spawn: if that fails, `Drop` stops and joins the first.
        let mut gadget = SteamDeckGadget {
            report,
            feedback,
            running,
            threads: vec![control],
            wakers: vec![ctrl_waker],
            _fd: fd,
            enc: Default::default(),
        };
        let stream = {
            let fd = gadget._fd.clone();
            let running = gadget.running.clone();
            let report = gadget.report.clone();
            let tid = stream_waker.tid.clone();
            let done = stream_waker.done.clone();
            std::thread::Builder::new()
                .name("pf-deck-gadget-stream".into())
                .spawn(move || {
                    // SAFETY: `pthread_self` is always valid on the calling thread.
                    tid.store(unsafe { libc::pthread_self() } as u64, Ordering::SeqCst);
                    stream_loop(fd, running, ctrl_ep, configured, report);
                    done.store(true, Ordering::SeqCst);
                })
                .context("spawn gadget stream thread")?
        };
        gadget.threads.push(stream);
        gadget.wakers.push(stream_waker);
        Ok(gadget)
    }

    pub fn write_state(&mut self, st: &super::steam_proto::SteamState) {
        let r = self.enc.encode(st);
        if let Ok(mut g) = self.report.lock() {
            *g = r;
        }
    }

    /// Take rumble (and other) feedback the host wrote since the last call.
    pub fn service(&mut self) -> super::steam_proto::SteamFeedback {
        self.feedback
            .lock()
            .map(|mut f| std::mem::take(&mut *f))
            .unwrap_or_default()
    }
}

impl Drop for SteamDeckGadget {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        // `EVENT_FETCH` only sees `running` between ioctls; signal it out (see `WAKE_SIGNAL`).
        // Retry: the signal can land just before re-entry. 200 × 5 ms ≈ 1 s caps a stuck join.
        for _ in 0..200 {
            let mut all_done = true;
            for w in &self.wakers {
                if w.done.load(Ordering::SeqCst) {
                    continue;
                }
                all_done = false;
                let tid = w.tid.load(Ordering::SeqCst);
                if tid != 0 {
                    // SAFETY: join runs after this loop, so `tid` is a live or finished-but-unjoined
                    // pthread. `pthread_kill` on the latter returns ESRCH; it is not UB.
                    unsafe { libc::pthread_kill(tid as libc::pthread_t, WAKE_SIGNAL) };
                }
            }
            if all_done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn copy_cstr(dst: &mut [u8], s: &str) {
    let n = s.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

fn control_loop(
    file: Arc<File>,
    running: Arc<AtomicBool>,
    ctrl_ep: Arc<std::sync::atomic::AtomicI32>,
    configured: Arc<AtomicBool>,
    feedback: Arc<Mutex<super::steam_proto::SteamFeedback>>,
    serial: String,
) {
    let fd = file.as_fd();
    let cfg = build_config();
    let mut last_set: Vec<u8> = Vec::new();
    let mut evbuf = [0u8; EVENT_BUF];
    while running.load(Ordering::SeqCst) {
        evbuf[4..8].copy_from_slice(&(8u32).to_ne_bytes()); // setup packet is 8 bytes
        let r = transfer(fd, IOCTL_EVENT_FETCH, &mut evbuf);
        if r < 0 {
            if running.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            continue;
        }
        let etype = u32::from_ne_bytes([evbuf[0], evbuf[1], evbuf[2], evbuf[3]]);
        match etype {
            USB_RAW_EVENT_CONNECT => {}
            USB_RAW_EVENT_CONTROL => {
                let s = &evbuf[EVENT_HDR..EVENT_HDR + 8];
                let ctrl = Setup {
                    bm_request_type: s[0],
                    b_request: s[1],
                    w_value: u16::from_le_bytes([s[2], s[3]]),
                    w_index: u16::from_le_bytes([s[4], s[5]]),
                    w_length: u16::from_le_bytes([s[6], s[7]]),
                };
                handle_control(
                    fd,
                    &ctrl,
                    &cfg,
                    &serial,
                    &ctrl_ep,
                    &configured,
                    &mut last_set,
                    &feedback,
                );
            }
            _ => {}
        }
    }
}

struct Setup {
    bm_request_type: u8,
    b_request: u8,
    w_value: u16,
    w_index: u16,
    w_length: u16,
}

#[allow(clippy::too_many_arguments)]
fn handle_control(
    fd: BorrowedFd<'_>,
    ctrl: &Setup,
    cfg: &[u8],
    serial: &str,
    ctrl_ep: &std::sync::atomic::AtomicI32,
    configured: &AtomicBool,
    last_set: &mut Vec<u8>,
    feedback: &Mutex<super::steam_proto::SteamFeedback>,
) {
    let idx = (ctrl.w_index & 0xff) as u8;
    let type_class = ctrl.bm_request_type & 0x60;
    let wl = ctrl.w_length as usize;
    if type_class == 0x00 {
        // USB standard
        match ctrl.b_request {
            0x06 => {
                // GET_DESCRIPTOR
                let dt = (ctrl.w_value >> 8) as u8;
                let di = (ctrl.w_value & 0xff) as u8;
                let resp: Vec<u8> = match dt {
                    1 => DEV_DESC.to_vec(),
                    2 => cfg.to_vec(),
                    3 => string_desc(di, serial),
                    HID_RPT_DT => match idx {
                        0 => RDESC_MOUSE.to_vec(),
                        1 => RDESC_KBD.to_vec(),
                        _ => RDESC_CTRL.to_vec(),
                    },
                    HID_DT => hid_desc_for(cfg, idx),
                    _ => {
                        ep0_stall(fd);
                        return;
                    }
                };
                let n = resp.len().min(wl);
                ep0_write(fd, &resp[..n]);
            }
            0x09 => {
                // SET_CONFIGURATION
                let _ = uapi::ioctl_value(fd, IOCTL_VBUS_DRAW, 0x32);
                let _ = uapi::ioctl_value(fd, IOCTL_CONFIGURE, 0);
                enable_endpoints(fd, ctrl_ep);
                ep0_ack(fd);
                configured.store(true, Ordering::SeqCst);
            }
            0x0b => ep0_ack(fd), // SET_INTERFACE
            0x00 => {
                let st = 0u16;
                ep0_write(fd, &st.to_le_bytes());
            }
            _ => ep0_stall(fd),
        }
    } else if type_class == 0x20 {
        // HID class
        match ctrl.b_request {
            0x01 => {
                // GET_REPORT — feature reply for the last SET_REPORT
                let resp = feature_reply(last_set, serial);
                let n = resp.len().min(wl);
                ep0_write(fd, &resp[..n]);
            }
            0x09 => {
                // SET_REPORT
                let (r, data) = ep0_read(fd, wl);
                if r > 0 {
                    *last_set = data.clone();
                    // parse_steam_output expects [report-id(0), cmd, …]; EP0 OUT data is [cmd, …].
                    let mut framed = Vec::with_capacity(data.len() + 1);
                    framed.push(0);
                    framed.extend_from_slice(&data);
                    let fb = super::steam_proto::parse_steam_output(&framed);
                    if fb.rumble.is_some() {
                        if let Ok(mut g) = feedback.lock() {
                            *g = fb;
                        }
                    }
                }
            }
            0x0a | 0x0b => ep0_ack(fd), // SET_IDLE / SET_PROTOCOL
            0x03 => {
                ep0_write(fd, &[0u8]);
            } // GET_PROTOCOL
            _ => ep0_stall(fd),
        }
    } else {
        ep0_stall(fd);
    }
}

fn hid_desc_for(cfg: &[u8], idx: u8) -> Vec<u8> {
    // HID desc sits after each iface in the config blob.
    // Layout: cfg(9) | 3×(iface 9 + HID 9 + EP 7).
    let off = match idx {
        0 => 9 + 9,
        1 => 9 + 25 + 9,
        _ => 9 + 50 + 9,
    };
    cfg.get(off..off + 9)
        .map(|s| s.to_vec())
        .unwrap_or_default()
}

/// Enable the three interrupt-IN endpoints; the controller's handle, or -1, lands in `ctrl_ep`.
fn enable_endpoints(fd: BorrowedFd<'_>, ctrl_ep: &std::sync::atomic::AtomicI32) {
    let enable = |addr: u8, mps: u16| {
        let mut ep = UsbEndpointDescriptor {
            b_length: 7,
            b_descriptor_type: 5,
            b_endpoint_address: addr,
            bm_attributes: 0x03,
            w_max_packet_size: mps,
            b_interval: 4,
            ..Default::default()
        };
        uapi::ioctl_with(fd, IOCTL_EP_ENABLE, &mut ep).unwrap_or(-1)
    };
    enable(0x81, 8);
    enable(0x82, 8);
    ctrl_ep.store(enable(0x83, 64), Ordering::SeqCst);
}

fn stream_loop(
    file: Arc<File>,
    running: Arc<AtomicBool>,
    ctrl_ep: Arc<std::sync::atomic::AtomicI32>,
    configured: Arc<AtomicBool>,
    report: Arc<Mutex<[u8; 64]>>,
) {
    let fd = file.as_fd();
    while running.load(Ordering::SeqCst) {
        let ep = ctrl_ep.load(Ordering::SeqCst);
        if configured.load(Ordering::SeqCst) && ep >= 0 {
            let r = report
                .lock()
                .map(|g| *g)
                .unwrap_or_else(|_| neutral_deck_report());
            let mut buf = [0u8; EPIO_HDR + 64];
            buf[0..2].copy_from_slice(&(ep as u16).to_ne_bytes());
            buf[4..8].copy_from_slice(&(64u32).to_ne_bytes());
            buf[EPIO_HDR..].copy_from_slice(&r);
            // EP_WRITE blocks until the host polls interrupt-IN; this loop has its own thread.
            transfer(fd, IOCTL_EP_WRITE, &mut buf);
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

/// Ignore `modprobe` failure; the caller falls back if `/dev/raw-gadget` is still missing.
pub fn ensure_modules() {
    for m in ["dummy_hcd", "raw_gadget"] {
        let _ = std::process::Command::new("modprobe").arg(m).status();
    }
}

/// Auto is on for SteamOS (ships the modules and runs Steam Input) and off elsewhere;
/// `PUNKTFUNK_STEAM_GADGET` on/off forces it. A host that *is* a Deck never reaches
/// here: `resolve_gamepad` degrades `SteamDeck` → DualSense before the manager is built.
pub fn gadget_preferred() -> bool {
    pf_host_config::row_tri("PUNKTFUNK_STEAM_GADGET")
        .unwrap_or_else(|| pf_host_config::os_release::os_release().is("steamos"))
}
