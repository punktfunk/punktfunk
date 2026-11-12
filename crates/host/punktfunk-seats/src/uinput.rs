//! A seat's `/dev/uinput`. The supervisor serves each running seat a CUSE device,
//! `/dev/punktfunk/uinput-<account>`, which the seat's unit binds over `/dev/uinput`, and replays
//! each open onto a real `/dev/uinput` of its own. A seat never holds the real node: a device it
//! made there would sit on seat0, and a keyboard or mouse reaches the VT's SysRq and
//! `/dev/input/mice` whoever owns its node. So only gamepad capabilities pass ([`allowed`]), the
//! supervisor stamps `phys` itself and records each device it makes for the udev fence
//! ([`vouch_made`]), and a seat holds at most [`MAX_OPENS`].
//!
//! Steam is a 32-bit client: the kernel marks its ioctls, and its `input_event` and FF upload
//! layouts are translated. The CUSE fd and each open's uinput fd sit in systemd's fd store, so a
//! supervisor restart keeps the node the seat has bound and the devices on it.

use crate::cuse::{self, Request};
use pf_inject::pad_broker::{self, ReplayUinput};
use pf_seats::linux::fence::vouch::{self, Vouch};
use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::io::Errno;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{self, ErrorKind, Read as _, Write as _};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

/// Devices one seat may hold at once: a pad and its motion sensors for each player, and a swap.
pub const MAX_OPENS: usize = 16;
/// Blocking reads one open may have waiting.
const MAX_WAITING_READS: usize = 4;
/// Events one read moves at most.
const READ_EVENTS: usize = 64;
/// Bytes one kernel read or write request carries; the kernel never takes less than 4096.
const MAX_IO: u32 = 4096;
/// One request, whole: the kernel refuses a `/dev/cuse` read buffer under 8192 bytes.
const CHANNEL_BUF: usize = 16 * 1024;
/// `O_NONBLOCK` in a read's flags, asm-generic and x86 alike.
const O_NONBLOCK: u32 = 0o4000;

const EPERM: i32 = Errno::PERM.raw_os_error();
const EINTR: i32 = Errno::INTR.raw_os_error();
const EIO: i32 = Errno::IO.raw_os_error();
const EBADF: i32 = Errno::BADF.raw_os_error();
const EAGAIN: i32 = Errno::AGAIN.raw_os_error();
const EINVAL: i32 = Errno::INVAL.raw_os_error();
const E2BIG: i32 = Errno::TOOBIG.raw_os_error();
const ENOENT: i32 = Errno::NOENT.raw_os_error();
const ENOSPC: i32 = Errno::NOSPC.raw_os_error();
const ENOSYS: i32 = Errno::NOSYS.raw_os_error();

// `include/uapi/linux/uinput.h`, and the compat numbers `drivers/input/misc/uinput.c` maps.
const UI_DEV_CREATE: u32 = 0x5501;
const UI_DEV_DESTROY: u32 = 0x5502;
const UI_DEV_SETUP: u32 = 0x405c_5503;
const UI_ABS_SETUP: u32 = 0x401c_5504;
/// `_IOW('U', nr, int)`: `UI_SET_EVBIT` (100) through `UI_SET_PROPBIT` (110).
const UI_SET_BIT: u32 = 0x4004_5500;
const NR_ABS: u32 = 103;
const NR_PHYS: u32 = 108;
const UI_SET_PHYS: u32 = 0x4008_556c;
const UI_SET_PHYS_COMPAT: u32 = 0x4004_556c;
const UI_BEGIN_FF_UPLOAD: u32 = 0xc068_55c8;
const UI_END_FF_UPLOAD: u32 = 0x4068_55c9;
const UI_BEGIN_FF_UPLOAD_COMPAT: u32 = 0xc060_55c8;
const UI_END_FF_UPLOAD_COMPAT: u32 = 0x4060_55c9;
const UI_BEGIN_FF_ERASE: u32 = 0xc00c_55ca;
const UI_END_FF_ERASE: u32 = 0x400c_55cb;
const UI_GET_VERSION: u32 = 0x8004_552d;
/// `UI_GET_SYSNAME(len)` with `len` zero.
const UI_GET_SYSNAME_0: u32 = 0x8000_552c;
const IOCSIZE_MASK: u32 = 0x3fff << 16;

/// `struct input_event`: 64-bit seconds and microseconds, type, code, value.
const EVENT: usize = 24;
/// `struct input_event_compat`: the same with 32-bit seconds and microseconds.
const EVENT_COMPAT: usize = 16;
/// `struct uinput_user_dev`, the legacy setup write. Neither event size divides it.
const USER_DEV: usize = 1116;
const _: () = assert!(!USER_DEV.is_multiple_of(EVENT) && !USER_DEV.is_multiple_of(EVENT_COMPAT));
const DEV_SETUP: usize = 92;
const ABS_SETUP: usize = 28;
const FF_ERASE: usize = 12;
/// `struct uinput_ff_upload`: request id, retval, then `effect` and `old`.
const FF_UPLOAD: usize = 104;
const FF_EFFECT: usize = 48;
/// `struct ff_effect_compat` is the native layout cut 4 bytes short, inside the custom-waveform
/// pointer that ends both; uinput copies the first 44 bytes each way.
const FF_EFFECT_COMPAT: usize = 44;
const FF_UPLOAD_COMPAT: usize = 96;

const EV_SYN: u32 = 0x00;
const EV_KEY: u32 = 0x01;
const EV_ABS: u32 = 0x03;
const EV_MSC: u32 = 0x04;
const EV_FF: u32 = 0x15;
const INPUT_PROP_ACCELEROMETER: u32 = 0x06;

/// Whether a seat may set `code` with `UI_SET_*BIT` number `nr` (`UI_ABS_SETUP` counts as
/// `UI_SET_ABSBIT`). Only what a gamepad or its motion sensors carry: never a key below
/// `BTN_MISC` or a braille key, which make a VT keyboard, never a mouse button, touch or tool,
/// relative axis, multitouch axis, switch, LED, sound or autorepeat.
fn allowed(nr: u32, code: u32) -> bool {
    match nr {
        100 => matches!(code, EV_SYN | EV_KEY | EV_ABS | EV_MSC | EV_FF),
        // BTN_JOYSTICK..BTN_THUMBR, BTN_DPAD_*, BTN_TRIGGER_HAPPY*.
        101 => matches!(code, 0x120..=0x13e | 0x220..=0x223 | 0x2c0..=0x2e7),
        // Sticks, triggers, wheels and hats; ABS_VOLUME..ABS_MISC. Not pressure, distance,
        // tilt or tool width (0x18..=0x1c), nor ABS_MT_* (0x2f up).
        NR_ABS => matches!(code, 0x00..=0x17 | 0x20..=0x28),
        // MSC and FF codes: the kernel bounds them.
        104 | 107 => true,
        110 => code == INPUT_PROP_ACCELEROMETER,
        _ => false,
    }
}

/// The capability a `UI_SET_*BIT` number sets, for the log.
fn bit_kind(nr: u32) -> &'static str {
    match nr {
        100 => "EV",
        101 => "KEY",
        102 => "REL",
        103 => "ABS",
        104 => "MSC",
        105 => "LED",
        106 => "SND",
        107 => "FF",
        109 => "SW",
        110 => "PROP",
        _ => "?",
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut word = [0u8; 8];
    word.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(word)
}

/// A 32-bit caller's events as the kernel takes them; a trailing partial event is dropped.
fn events_from_compat(compat: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(compat.len() / EVENT_COMPAT * EVENT);
    for e in compat.chunks_exact(EVENT_COMPAT) {
        out.extend_from_slice(&u64::from(u32_at(e, 0)).to_ne_bytes());
        out.extend_from_slice(&u64::from(u32_at(e, 4)).to_ne_bytes());
        out.extend_from_slice(&e[8..16]);
    }
    out
}

/// Events the kernel queued, as a 32-bit caller reads them: the time cut to 32 bits.
fn events_to_compat(native: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(native.len() / EVENT * EVENT_COMPAT);
    for e in native.chunks_exact(EVENT) {
        out.extend_from_slice(&(u64_at(e, 0) as u32).to_ne_bytes());
        out.extend_from_slice(&(u64_at(e, 8) as u32).to_ne_bytes());
        out.extend_from_slice(&e[16..24]);
    }
    out
}

fn ff_upload_from_compat(compat: &[u8; FF_UPLOAD_COMPAT]) -> [u8; FF_UPLOAD] {
    let mut native = [0u8; FF_UPLOAD];
    native[..8 + FF_EFFECT_COMPAT].copy_from_slice(&compat[..8 + FF_EFFECT_COMPAT]);
    native[8 + FF_EFFECT..8 + FF_EFFECT + FF_EFFECT_COMPAT]
        .copy_from_slice(&compat[8 + FF_EFFECT_COMPAT..]);
    native
}

fn ff_upload_to_compat(native: &[u8; FF_UPLOAD]) -> [u8; FF_UPLOAD_COMPAT] {
    let mut compat = [0u8; FF_UPLOAD_COMPAT];
    compat[..8 + FF_EFFECT_COMPAT].copy_from_slice(&native[..8 + FF_EFFECT_COMPAT]);
    compat[8 + FF_EFFECT_COMPAT..]
        .copy_from_slice(&native[8 + FF_EFFECT..8 + FF_EFFECT + FF_EFFECT_COMPAT]);
    compat
}

fn errno(error: &io::Error) -> i32 {
    error.raw_os_error().unwrap_or(EIO)
}

/// What an open replays onto: the real `/dev/uinput`, or a fake in tests.
pub trait Uinput: AsFd {
    fn ioctl_value(&self, req: u32, value: u32) -> io::Result<()>;
    fn ioctl_buffer<const N: usize>(&self, req: u32, buf: &mut [u8; N]) -> io::Result<i32>;
    fn set_phys(&self, phys: &str) -> io::Result<()>;
    fn read(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn write(&self, buf: &[u8]) -> io::Result<usize>;
}

impl Uinput for ReplayUinput {
    fn ioctl_value(&self, req: u32, value: u32) -> io::Result<()> {
        ReplayUinput::ioctl_value(self, req, value)
    }
    fn ioctl_buffer<const N: usize>(&self, req: u32, buf: &mut [u8; N]) -> io::Result<i32> {
        ReplayUinput::ioctl_buffer(self, req, buf)
    }
    fn set_phys(&self, phys: &str) -> io::Result<()> {
        ReplayUinput::set_phys(self, phys)
    }
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        ReplayUinput::read(self, buf)
    }
    fn write(&self, buf: &[u8]) -> io::Result<usize> {
        ReplayUinput::write(self, buf)
    }
}

/// One open of a seat's node: one real uinput fd, one device.
struct Open<U> {
    dev: U,
    /// The caller's ABI, from its latest ioctl: `Some(true)` for 32-bit.
    compat: Option<bool>,
    /// The kernel's handle for the file, from its first poll.
    kh: Option<u64>,
    /// A poller waits for the device to turn readable.
    notify: bool,
    /// Blocking reads waiting, `(unique, size)`.
    reads: VecDeque<(u64, u32)>,
    /// Its name in systemd's fd store.
    stored: Option<String>,
    /// The record that vouches for its device, dropped after the device.
    vouch: Option<Vouch>,
}

impl<U> Open<U> {
    fn new(dev: U) -> Self {
        Open {
            dev,
            compat: None,
            kh: None,
            notify: false,
            reads: VecDeque::new(),
            stored: None,
            vouch: None,
        }
    }
}

fn chan_name(account: &str) -> String {
    format!("uinput/{account}")
}

/// An open's fd-store name, `uinput/<account>/<fh>/<abi>/<kh>`: what a restarted supervisor
/// needs to serve it again.
fn open_name(account: &str, fh: u64, compat: Option<bool>, kh: Option<u64>) -> String {
    let abi = match compat {
        None => "n",
        Some(true) => "32",
        Some(false) => "64",
    };
    let kh = kh.map_or("-".to_owned(), |kh| kh.to_string());
    format!("uinput/{account}/{fh}/{abi}/{kh}")
}

#[derive(Debug, PartialEq, Eq)]
enum Stored {
    Chan(String),
    Open {
        account: String,
        fh: u64,
        compat: Option<bool>,
        kh: Option<u64>,
    },
}

fn parse_name(name: &str) -> Option<Stored> {
    let mut parts = name.strip_prefix("uinput/")?.split('/');
    let account = parts.next().filter(|a| !a.is_empty())?.to_owned();
    let Some(fh) = parts.next() else {
        return Some(Stored::Chan(account));
    };
    let compat = match parts.next()? {
        "n" => None,
        "32" => Some(true),
        "64" => Some(false),
        _ => return None,
    };
    let kh = match parts.next()? {
        "-" => None,
        kh => Some(kh.parse().ok()?),
    };
    parts.next().is_none().then_some(Stored::Open {
        account,
        fh: fh.parse().ok()?,
        compat,
        kh,
    })
}

/// One `FUSE_IOCTL`.
struct Ioctl<'a> {
    cmd: u32,
    arg: u64,
    in_size: u32,
    out_size: u32,
    data: &'a [u8],
}

enum Answer {
    Done(i32, Vec<u8>),
    /// Call again with this many bytes at the argument copied in, and room for this many out.
    Retry(usize, usize),
    Error(i32),
    /// A capability the allowlist refuses: `(nr, code)`.
    Refused(u32, u32),
}

impl Ioctl<'_> {
    /// `Some(retry)` until the kernel copied `copy_in` bytes in and made room for `copy_out`.
    fn needs(&self, copy_in: usize, copy_out: usize) -> Option<Answer> {
        let short = (self.in_size as usize) < copy_in || (self.out_size as usize) < copy_out;
        short.then_some(Answer::Retry(copy_in, copy_out))
    }

    fn size(&self) -> usize {
        ((self.cmd & IOCSIZE_MASK) >> 16) as usize
    }
}

fn done(result: io::Result<()>) -> Answer {
    match result {
        Ok(()) => Answer::Done(0, Vec::new()),
        Err(error) => Answer::Error(errno(&error)),
    }
}

/// A request that copies `N` bytes in, and back out when `out`.
fn fixed<U: Uinput, const N: usize>(dev: &U, call: &Ioctl<'_>, req: u32, out: bool) -> Answer {
    if let Some(retry) = call.needs(N, if out { N } else { 0 }) {
        return retry;
    }
    let Some(data) = call.data.first_chunk::<N>() else {
        return Answer::Error(EINVAL);
    };
    let mut buf = *data;
    match dev.ioctl_buffer(req, &mut buf) {
        Ok(result) => Answer::Done(result, if out { buf.to_vec() } else { Vec::new() }),
        Err(error) => Answer::Error(errno(&error)),
    }
}

fn abs_setup<U: Uinput>(dev: &U, call: &Ioctl<'_>) -> Answer {
    let size = call.size();
    if size > ABS_SETUP {
        return Answer::Error(E2BIG);
    }
    if let Some(retry) = call.needs(size, 0) {
        return retry;
    }
    let mut buf = [0u8; ABS_SETUP];
    buf[..size].copy_from_slice(&call.data[..size]);
    let code = u32::from(u16::from_ne_bytes([buf[0], buf[1]]));
    if !allowed(NR_ABS, code) {
        return Answer::Refused(NR_ABS, code);
    }
    match dev.ioctl_buffer(UI_ABS_SETUP, &mut buf) {
        Ok(result) => Answer::Done(result, Vec::new()),
        Err(error) => Answer::Error(errno(&error)),
    }
}

fn ff_upload<U: Uinput>(dev: &U, call: &Ioctl<'_>) -> Answer {
    let begin = matches!(call.cmd, UI_BEGIN_FF_UPLOAD | UI_BEGIN_FF_UPLOAD_COMPAT);
    let compat = matches!(
        call.cmd,
        UI_BEGIN_FF_UPLOAD_COMPAT | UI_END_FF_UPLOAD_COMPAT
    );
    let theirs = if compat { FF_UPLOAD_COMPAT } else { FF_UPLOAD };
    if let Some(retry) = call.needs(theirs, if begin { theirs } else { 0 }) {
        return retry;
    }
    let native = if compat {
        call.data
            .first_chunk::<FF_UPLOAD_COMPAT>()
            .map(ff_upload_from_compat)
    } else {
        call.data.first_chunk::<FF_UPLOAD>().copied()
    };
    let Some(mut native) = native else {
        return Answer::Error(EINVAL);
    };
    let req = if begin {
        UI_BEGIN_FF_UPLOAD
    } else {
        UI_END_FF_UPLOAD
    };
    match dev.ioctl_buffer(req, &mut native) {
        Ok(result) if !begin => Answer::Done(result, Vec::new()),
        Ok(result) if compat => Answer::Done(result, ff_upload_to_compat(&native).to_vec()),
        Ok(result) => Answer::Done(result, native.to_vec()),
        Err(error) => Answer::Error(errno(&error)),
    }
}

/// `UI_GET_SYSNAME(len)`: the created device's sysfs name, cut to `len` as uinput cuts it.
fn sysname<U: Uinput>(dev: &U, call: &Ioctl<'_>) -> Answer {
    let len = call.size();
    if let Some(retry) = call.needs(0, len) {
        return retry;
    }
    let mut buf = [0u8; 64];
    match dev.ioctl_buffer(UI_GET_SYSNAME_0 | (64 << 16), &mut buf) {
        Ok(_) => {
            let whole = buf
                .iter()
                .position(|&b| b == 0)
                .map_or(buf.len(), |n| n + 1);
            let n = whole.min(len);
            Answer::Done(n as i32, buf[..n].to_vec())
        }
        Err(error) => Answer::Error(errno(&error)),
    }
}

/// Replays one ioctl onto `dev`. `phys` is the stamp `UI_DEV_CREATE` puts on the device.
fn replay<U: Uinput>(dev: &U, call: &Ioctl<'_>, phys: &str) -> Answer {
    let nr = call.cmd & 0xff;
    match call.cmd {
        // The supervisor's stamp, not the seat's, is what the udev fence reads.
        UI_SET_PHYS | UI_SET_PHYS_COMPAT => Answer::Done(0, Vec::new()),
        UI_DEV_CREATE => done(
            dev.set_phys(phys)
                .and_then(|()| dev.ioctl_value(UI_DEV_CREATE, 0)),
        ),
        UI_DEV_DESTROY => done(dev.ioctl_value(UI_DEV_DESTROY, 0)),
        cmd if cmd & !0xff == UI_SET_BIT && (100..=110).contains(&nr) && nr != NR_PHYS => {
            let Ok(code) = u32::try_from(call.arg) else {
                return Answer::Error(EINVAL);
            };
            if !allowed(nr, code) {
                return Answer::Refused(nr, code);
            }
            done(dev.ioctl_value(cmd, code))
        }
        UI_DEV_SETUP => fixed::<U, DEV_SETUP>(dev, call, UI_DEV_SETUP, false),
        cmd if cmd & !IOCSIZE_MASK == UI_ABS_SETUP & !IOCSIZE_MASK => abs_setup(dev, call),
        UI_BEGIN_FF_UPLOAD
        | UI_END_FF_UPLOAD
        | UI_BEGIN_FF_UPLOAD_COMPAT
        | UI_END_FF_UPLOAD_COMPAT => ff_upload(dev, call),
        UI_BEGIN_FF_ERASE => fixed::<U, FF_ERASE>(dev, call, UI_BEGIN_FF_ERASE, true),
        UI_END_FF_ERASE => fixed::<U, FF_ERASE>(dev, call, UI_END_FF_ERASE, false),
        UI_GET_VERSION => fixed::<U, 4>(dev, call, UI_GET_VERSION, true),
        cmd if cmd & !IOCSIZE_MASK == UI_GET_SYSNAME_0 => sysname(dev, call),
        // What uinput answers a request it doesn't know.
        _ => Answer::Error(EINVAL),
    }
}

/// The record for the device `dev` made, by the name the kernel gave it: what the fence hands
/// to the seat. Anything else carrying the seat's stamp, a copy included, goes to nobody.
fn vouch_made<U: Uinput>(dev: &U, dir: &Path, account: &str) -> io::Result<Vouch> {
    let mut name = [0u8; 64];
    dev.ioctl_buffer(UI_GET_SYSNAME_0 | (64 << 16), &mut name)?;
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    let mut vouch = Vouch::new(dir, account, false)?;
    vouch.made(&String::from_utf8_lossy(&name[..len]))?;
    Ok(vouch)
}

/// Whether `fd` has something to read now.
fn readable(fd: impl AsFd) -> bool {
    let mut fds = [PollFd::new(&fd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    poll(&mut fds, Some(&now)).is_ok_and(|n| n > 0)
}

/// Up to `size` bytes of queued events in the caller's layout; `None` while none are queued.
fn take_events<U: Uinput>(open: &Open<U>, size: u32) -> io::Result<Option<Vec<u8>>> {
    let compat = open.compat == Some(true);
    let theirs = if compat { EVENT_COMPAT } else { EVENT };
    let count = (size as usize / theirs).min(READ_EVENTS);
    if count == 0 {
        return match size {
            0 => Ok(Some(Vec::new())),
            _ => Err(io::Error::from_raw_os_error(EINVAL)),
        };
    }
    let mut buf = vec![0u8; count * EVENT];
    match open.dev.read(&mut buf) {
        Ok(n) => {
            buf.truncate(n - n % EVENT);
            Ok(Some(if compat { events_to_compat(&buf) } else { buf }))
        }
        Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error),
    }
}

/// One seat's CUSE device and the devices made through it.
pub struct Proxy<U> {
    chan: File,
    account: String,
    opener: Box<dyn Fn() -> io::Result<U> + Send>,
    opens: HashMap<u64, Open<U>>,
    next_fh: u64,
    /// `(nr, code)` already logged as refused.
    refused: HashSet<(u32, u32)>,
    /// Where the records that vouch for its devices go ([`vouch::DIR`]).
    vouched: PathBuf,
}

impl<U: Uinput> Proxy<U> {
    fn new(
        chan: File,
        account: &str,
        opener: Box<dyn Fn() -> io::Result<U> + Send>,
        kept: Vec<(u64, Open<U>)>,
    ) -> Self {
        let next_fh = kept.iter().map(|(fh, _)| fh + 1).max().unwrap_or(1);
        Proxy {
            chan,
            account: account.to_owned(),
            opener,
            opens: kept.into_iter().collect(),
            next_fh,
            refused: HashSet::new(),
            vouched: PathBuf::from(vouch::DIR),
        }
    }

    fn send(&mut self, message: &[u8]) {
        match (&self.chan).write(message) {
            Ok(_) => {}
            // The request was interrupted and is gone.
            Err(error) if error.raw_os_error() == Some(ENOENT) => {}
            Err(error) => tracing::debug!(%error, account = self.account, "seat uinput reply"),
        }
    }

    fn handle(&mut self, buf: &[u8]) {
        let Some((unique, request)) = cuse::parse(buf) else {
            tracing::debug!(
                len = buf.len(),
                account = self.account,
                "seat uinput request unread"
            );
            return;
        };
        let answer = match request {
            Request::Open { .. } => Some(self.open(unique)),
            Request::Release { fh } => {
                self.release(fh);
                Some(cuse::reply(unique, &[]))
            }
            Request::Flush => Some(cuse::reply(unique, &[])),
            Request::Read { fh, size, flags } => {
                self.read(unique, fh, size, flags & O_NONBLOCK != 0)
            }
            Request::Write { fh, data } => Some(self.write(unique, fh, data)),
            Request::Ioctl {
                fh,
                flags,
                cmd,
                arg,
                in_size,
                out_size,
                data,
            } => {
                let call = Ioctl {
                    cmd,
                    arg,
                    in_size,
                    out_size,
                    data,
                };
                Some(self.ioctl(unique, fh, flags & cuse::FUSE_IOCTL_32BIT != 0, &call))
            }
            Request::Poll { fh, kh, flags } => Some(self.poll(unique, fh, kh, flags)),
            Request::Interrupt { unique } => self.interrupt(unique),
            Request::Init { .. } | Request::Other(_) => Some(cuse::error(unique, ENOSYS)),
        };
        if let Some(answer) = answer {
            self.send(&answer);
        }
    }

    fn open(&mut self, unique: u64) -> Vec<u8> {
        if self.opens.len() >= MAX_OPENS {
            tracing::warn!(
                account = self.account,
                cap = MAX_OPENS,
                "seat asked for one uinput device too many"
            );
            return cuse::error(unique, ENOSPC);
        }
        let dev = match (self.opener)() {
            Ok(dev) => dev,
            Err(error) => {
                tracing::warn!(%error, account = self.account, "seat uinput device not opened");
                return cuse::error(unique, errno(&error));
            }
        };
        let fh = self.next_fh;
        self.next_fh += 1;
        self.opens.insert(fh, Open::new(dev));
        self.store(fh);
        cuse::open_reply(unique, fh)
    }

    /// Closes the open's uinput fd, which destroys its device.
    fn release(&mut self, fh: u64) {
        if let Some(name) = self.opens.remove(&fh).and_then(|open| open.stored) {
            pad_broker::forget_fd(&name);
        }
    }

    fn read(&mut self, unique: u64, fh: u64, size: u32, nonblock: bool) -> Option<Vec<u8>> {
        let Some(open) = self.opens.get_mut(&fh) else {
            return Some(cuse::error(unique, EBADF));
        };
        match take_events(open, size) {
            Ok(Some(data)) => Some(cuse::reply(unique, &[&data])),
            Ok(None) if nonblock || open.reads.len() >= MAX_WAITING_READS => {
                Some(cuse::error(unique, EAGAIN))
            }
            Ok(None) => {
                open.reads.push_back((unique, size));
                None
            }
            Err(error) => Some(cuse::error(unique, errno(&error))),
        }
    }

    fn write(&mut self, unique: u64, fh: u64, data: &[u8]) -> Vec<u8> {
        let Some(open) = self.opens.get(&fh) else {
            return cuse::error(unique, EBADF);
        };
        let written = if open.compat == Some(true) && data.len() != USER_DEV {
            if !data.len().is_multiple_of(EVENT_COMPAT) {
                return cuse::error(unique, EINVAL);
            }
            let native = events_from_compat(data);
            open.dev.write(&native).map(|n| n / EVENT * EVENT_COMPAT)
        } else {
            open.dev.write(data)
        };
        match written {
            Ok(n) => cuse::write_reply(unique, n as u32),
            Err(error) => cuse::error(unique, errno(&error)),
        }
    }

    /// Replays `call` on `fh`'s device. A device made is recorded for the fence; one destroyed
    /// loses its record.
    fn ioctl(&mut self, unique: u64, fh: u64, compat: bool, call: &Ioctl<'_>) -> Vec<u8> {
        let phys = pad_broker::seat_phys(&self.account, &format!("steam/{fh}"));
        let Some(open) = self.opens.get_mut(&fh) else {
            return cuse::error(unique, EBADF);
        };
        // Stored once, with the first ABI: a caller flipping ABIs must not re-store each call.
        let learned = open.compat.is_none();
        open.compat = Some(compat);
        let answer = replay(&open.dev, call, &phys);
        match (call.cmd, &answer) {
            (UI_DEV_CREATE, Answer::Done(..)) => {
                open.vouch = vouch_made(&open.dev, &self.vouched, &self.account)
                    .inspect_err(|error| {
                        tracing::warn!(%error, account = self.account,
                            "seat uinput device not recorded: the fence refuses it");
                    })
                    .ok();
            }
            (UI_DEV_DESTROY, _) => open.vouch = None,
            _ => {}
        }
        if learned {
            self.store(fh);
        }
        match answer {
            Answer::Done(result, out) => cuse::ioctl_reply(unique, result, &out),
            Answer::Retry(copy_in, copy_out) => {
                let iov = |len: usize| (len > 0).then_some((call.arg, len as u64));
                let (ins, outs) = (iov(copy_in), iov(copy_out));
                cuse::ioctl_retry(unique, ins.as_slice(), outs.as_slice())
            }
            Answer::Error(errno) => cuse::error(unique, errno),
            Answer::Refused(nr, code) => {
                // Bounded: a seat can't grow the set past the code space it may name.
                if self.refused.len() < 1024 && self.refused.insert((nr, code)) {
                    tracing::warn!(
                        account = self.account,
                        kind = bit_kind(nr),
                        code = %format!("{code:#x}"),
                        "seat uinput capability refused"
                    );
                }
                cuse::error(unique, EPERM)
            }
        }
    }

    fn poll(&mut self, unique: u64, fh: u64, kh: u64, flags: u32) -> Vec<u8> {
        let Some(open) = self.opens.get_mut(&fh) else {
            return cuse::error(unique, EBADF);
        };
        let mut revents = PollFlags::OUT | PollFlags::WRNORM;
        if readable(&open.dev) {
            revents |= PollFlags::IN | PollFlags::RDNORM;
        } else if flags & cuse::FUSE_POLL_SCHEDULE_NOTIFY != 0 {
            open.notify = true;
        }
        let learned = open.kh != Some(kh);
        open.kh = Some(kh);
        if learned {
            self.store(fh);
        }
        cuse::poll_reply(unique, u32::from(revents.bits()))
    }

    /// The kernel gave up on `unique`: a blocking read still waiting ends with `EINTR`.
    fn interrupt(&mut self, unique: u64) -> Option<Vec<u8>> {
        self.opens.values_mut().find_map(|open| {
            let at = open.reads.iter().position(|(u, _)| *u == unique)?;
            open.reads.remove(at);
            Some(cuse::error(unique, EINTR))
        })
    }

    /// A restart is coming: ends every blocking read still waiting with `EINTR`. The kernel
    /// keeps a read this process took in processing until it is answered, and the next supervisor
    /// can't name it; its caller reads again, and the new request waits for the next supervisor.
    fn drain(&mut self) {
        let waiting: Vec<u64> = self
            .opens
            .values_mut()
            .flat_map(|open| open.reads.drain(..))
            .map(|(unique, _)| unique)
            .collect();
        for unique in waiting {
            self.send(&cuse::error(unique, EINTR));
        }
    }

    /// `fh`'s device turned readable: answers the reads waiting on it, then wakes its poller.
    fn wake(&mut self, fh: u64) {
        let Some(open) = self.opens.get_mut(&fh) else {
            return;
        };
        let mut out = Vec::new();
        while let Some(&(unique, size)) = open.reads.front() {
            match take_events(open, size) {
                Ok(None) => break,
                Ok(Some(data)) => out.push(cuse::reply(unique, &[&data])),
                Err(error) => out.push(cuse::error(unique, errno(&error))),
            }
            open.reads.pop_front();
        }
        if open.notify && readable(&open.dev) {
            open.notify = false;
            out.extend(open.kh.map(cuse::notify_poll));
        }
        out.iter().for_each(|message| self.send(message));
    }

    /// Parks `fh`'s uinput fd in systemd's fd store under its current name. systemd keeps one
    /// copy of an open file, so the old name goes first.
    fn store(&mut self, fh: u64) {
        let Some(open) = self.opens.get_mut(&fh) else {
            return;
        };
        let name = open_name(&self.account, fh, open.compat, open.kh);
        if open.stored.as_deref() == Some(name.as_str()) {
            return;
        }
        if let Some(old) = open.stored.take() {
            pad_broker::forget_fd(&old);
        }
        pad_broker::keep_fd(&name, open.dev.as_fd());
        open.stored = Some(name);
    }

    /// Every fd-store name this proxy holds.
    fn names(&self) -> Vec<String> {
        let opens = self.opens.values().filter_map(|open| open.stored.clone());
        std::iter::once(chan_name(&self.account))
            .chain(opens)
            .collect()
    }
}

/// Serves `proxy` until `stop`'s peer hangs up or the kernel ends the device, then takes its
/// fds out of systemd's store; dropping them removes the node and destroys every device. A byte
/// on `stop` is [`Proxy::drain`]: the proxy answers its waiting reads and reads no more requests,
/// and leaves its fds in the store for the supervisor that follows.
fn serve<U: Uinput>(mut proxy: Proxy<U>, stop: UnixStream) {
    let mut buf = vec![0u8; CHANNEL_BUF];
    loop {
        let waiting: Vec<u64> = proxy
            .opens
            .iter()
            .filter(|(_, open)| open.notify || !open.reads.is_empty())
            .map(|(fh, _)| *fh)
            .collect();
        let (requests, stopped, ready) = {
            let mut fds = vec![
                PollFd::new(&proxy.chan, PollFlags::IN),
                PollFd::new(&stop, PollFlags::IN),
            ];
            fds.extend(
                waiting
                    .iter()
                    .map(|fh| PollFd::new(&proxy.opens[fh].dev, PollFlags::IN)),
            );
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(Errno::INTR) => continue,
                Err(error) => {
                    tracing::warn!(%error, account = proxy.account, "seat uinput poll");
                    break;
                }
            }
            let ready: Vec<u64> = waiting
                .iter()
                .zip(&fds[2..])
                .filter(|(_, fd)| !fd.revents().is_empty())
                .map(|(fh, _)| *fh)
                .collect();
            (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
                ready,
            )
        };
        if stopped {
            let mut byte = [0u8; 1];
            if (&stop).read(&mut byte).is_ok_and(|n| n == 1) {
                proxy.drain();
                return;
            }
            break;
        }
        if requests {
            match (&proxy.chan).read(&mut buf) {
                Ok(n) => proxy.handle(&buf[..n]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
                        || error.raw_os_error() == Some(ENOENT) => {}
                Err(error) => {
                    tracing::info!(%error, account = proxy.account, "seat uinput device ended");
                    break;
                }
            }
        }
        ready.into_iter().for_each(|fh| proxy.wake(fh));
    }
    proxy
        .names()
        .iter()
        .for_each(|name| pad_broker::forget_fd(name));
}

/// A running proxy: dropping `stop` ends its thread.
struct Handle {
    stop: UnixStream,
    thread: JoinHandle<()>,
}

impl Handle {
    fn end(self) {
        drop(self.stop);
        let _ = self.thread.join();
    }

    /// Has the proxy answer its waiting reads and stop, and waits for it.
    fn drain(self) {
        let _ = (&self.stop).write_all(b"d");
        let _ = self.thread.join();
    }
}

/// Where `punktfunk-seats drain` reaches the running supervisor: root's alone.
const DRAIN_SOCKET: &str = "/run/punktfunk/uinput-drain.sock";
/// How long the unit's `ExecStop=` waits for the supervisor to answer.
const DRAIN_WAIT: Duration = Duration::from_secs(5);

/// Listens on [`DRAIN_SOCKET`] for the unit's `ExecStop=`: each root connection drains every
/// proxy before the stop signal ends the process. The proxies hold reads the kernel counts as in
/// processing, and a signal handler would need `unsafe`.
pub fn listen_for_drain() {
    let bound = pf_seats::linux::socket::bind_fresh(Path::new(DRAIN_SOCKET)).and_then(|listener| {
        std::fs::set_permissions(DRAIN_SOCKET, std::fs::Permissions::from_mode(0o600))?;
        Ok(listener)
    });
    let listener = match bound {
        Ok(listener) => listener,
        Err(error) => {
            tracing::warn!(%error, "seat uinput drain socket not bound");
            return;
        }
    };
    let spawned = std::thread::Builder::new()
        .name("seat-drain".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let root = pf_seats::linux::socket::peer_uid(&stream).is_ok_and(|uid| uid == 0);
                if root {
                    drain_all();
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "seat uinput drain thread did not start");
    }
}

fn drain_all() {
    let all = std::mem::take(&mut *proxies());
    for (account, handle) in all {
        handle.drain();
        tracing::info!(account, "seat uinput proxy drained");
    }
}

/// `punktfunk-seats drain`: returns once the running supervisor has answered the reads its
/// proxies hold. Fails when none is listening.
pub fn ask_to_drain() -> io::Result<()> {
    let mut stream = UnixStream::connect(DRAIN_SOCKET)?;
    stream.set_read_timeout(Some(DRAIN_WAIT))?;
    stream.read_to_end(&mut Vec::new()).map(drop)
}

static PROXIES: LazyLock<Mutex<HashMap<String, Handle>>> = LazyLock::new(Mutex::default);

fn proxies() -> MutexGuard<'static, HashMap<String, Handle>> {
    PROXIES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The backend's [`pf_seats::linux::UnitHook`]: `account`'s proxy up before its unit starts, as
/// `uid`'s, and down once it stopped.
pub fn hook(account: &str, uid: Option<u32>) {
    match uid {
        Some(uid) => up(account, uid),
        None => {
            if let Some(handle) = proxies().remove(account) {
                handle.end();
                tracing::info!(account, "seat uinput proxy down");
            }
        }
    }
}

fn up(account: &str, uid: u32) {
    let mut all = proxies();
    if all.get(account).is_some_and(|h| !h.thread.is_finished()) {
        return;
    }
    if let Some(ended) = all.remove(account) {
        ended.end();
    }
    let chan = match make(account, uid) {
        Ok(chan) => chan,
        Err(error) => {
            tracing::warn!(%error, account, "seat uinput proxy not made");
            return;
        }
    };
    pad_broker::keep_fd(&chan_name(account), chan.as_fd());
    if let Some(handle) = spawn(account, chan, Vec::new()) {
        all.insert(account.to_owned(), handle);
        tracing::info!(account, "seat uinput proxy up");
    }
}

const CUSE: &str = "/dev/cuse";

/// Whether a `/dev/cuse` with this owner and mode opens for root alone.
fn root_only(uid: u32, mode: u32) -> bool {
    uid == 0 && mode & 0o077 == 0
}

/// Makes `/dev/cuse` root's alone. Whoever else can open it can register a node under the name a
/// seat's unit binds, and the fence then hands that node to the seat's user.
fn close_cuse(node: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(node)?;
    if root_only(meta.uid(), meta.mode()) {
        return Ok(());
    }
    tracing::warn!(node = %node.display(), "cuse node opens to other users; made root's alone");
    std::os::unix::fs::chown(node, Some(0), Some(0))?;
    std::fs::set_permissions(node, std::fs::Permissions::from_mode(0o600))
}

/// `account`'s CUSE device, its node handed to `uid`, `0600`. An earlier device of that name
/// still there after two seconds fails it: the kernel refuses a twin name, and the `chown` would
/// land on the old node.
fn make(account: &str, uid: u32) -> io::Result<File> {
    close_cuse(Path::new(CUSE))?;
    let devname = pf_paths::seat::uinput_devname(account);
    // One that just ended lingers until systemd drops its stored copy.
    let sysfs = Path::new("/sys/class/cuse").join(devname.replace('/', "!"));
    for _ in 0..20 {
        if !sysfs.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if sysfs.exists() {
        let still = format!("an earlier /dev/{devname} is still there");
        return Err(io::Error::new(ErrorKind::AlreadyExists, still));
    }
    let chan = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(CUSE)?;
    handshake(&chan, &devname)?;
    let node = Path::new("/dev").join(&devname);
    std::os::unix::fs::chown(&node, Some(uid), Some(0))?;
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o600))?;
    Ok(chan)
}

/// Answers the `CUSE_INIT` the kernel queued at open. The device exists once the reply is in.
fn handshake(chan: &File, devname: &str) -> io::Result<()> {
    let mut buf = vec![0u8; CHANNEL_BUF];
    let n = (&*chan).read(&mut buf)?;
    let Some((unique, Request::Init { major, minor })) = cuse::parse(&buf[..n]) else {
        return Err(io::Error::other("no CUSE_INIT"));
    };
    if major != 7 || minor < cuse::MIN_MINOR {
        return Err(io::Error::other(format!("CUSE protocol {major}.{minor}")));
    }
    (&*chan).write_all(&cuse::init_reply(unique, minor, MAX_IO, devname))
}

/// Starts serving `chan` for `account`, with `kept` the opens a previous supervisor made.
fn spawn(account: &str, chan: File, kept: Vec<(u64, Open<ReplayUinput>)>) -> Option<Handle> {
    let names: Vec<String> = std::iter::once(chan_name(account))
        .chain(kept.iter().filter_map(|(_, open)| open.stored.clone()))
        .collect();
    let started = UnixStream::pair().and_then(|(stop, theirs)| {
        rustix::io::ioctl_fionbio(&chan, true)?;
        let opener = Box::new(ReplayUinput::open);
        let proxy = Proxy::new(chan, account, opener, kept);
        let thread = std::thread::Builder::new()
            .name("seat-uinput".into())
            .spawn(move || serve(proxy, theirs))?;
        Ok(Handle { stop, thread })
    });
    match started {
        Ok(handle) => Some(handle),
        Err(error) => {
            tracing::warn!(%error, account, "seat uinput thread did not start");
            names.iter().for_each(|name| pad_broker::forget_fd(name));
            None
        }
    }
}

/// Serves again every proxy systemd kept across a supervisor restart, with its devices, each
/// device recorded for the fence again: the supervisor that made it may have written no record.
/// A kept fd no proxy claims leaves the store.
pub fn resume(kept: Vec<(String, OwnedFd)>) {
    let mut chans = HashMap::new();
    let mut opens: HashMap<String, Vec<(u64, Open<ReplayUinput>)>> = HashMap::new();
    for (name, fd) in kept {
        match parse_name(&name) {
            Some(Stored::Chan(account)) => {
                chans.insert(account, File::from(fd));
            }
            Some(Stored::Open {
                account,
                fh,
                compat,
                kh,
            }) => {
                let mut open = Open::new(ReplayUinput::adopt(fd));
                open.compat = compat;
                open.kh = kh;
                // A poller may have waited through the restart; a spare wake is harmless.
                open.notify = kh.is_some();
                open.stored = Some(name);
                // An open whose device isn't made answers ENOENT.
                open.vouch = match vouch_made(&open.dev, Path::new(vouch::DIR), &account) {
                    Ok(vouch) => Some(vouch),
                    Err(error) if error.raw_os_error() == Some(ENOENT) => None,
                    Err(error) => {
                        tracing::warn!(%error, account,
                            "kept seat uinput device not recorded: the fence refuses it");
                        None
                    }
                };
                opens.entry(account).or_default().push((fh, open));
            }
            None => pad_broker::forget_fd(&name),
        }
    }
    let mut all = proxies();
    for (account, chan) in chans {
        let kept = opens.remove(&account).unwrap_or_default();
        tracing::info!(
            account,
            devices = kept.len(),
            "seat uinput proxy kept across a restart"
        );
        if let Some(handle) = spawn(&account, chan, kept) {
            all.insert(account, handle);
        }
    }
    for (_, open) in opens.into_values().flatten() {
        open.stored
            .iter()
            .for_each(|name| pad_broker::forget_fd(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::BorrowedFd;
    use std::os::unix::net::UnixDatagram;
    use std::sync::mpsc;
    use std::sync::Arc;

    /// `_IOC(dir, 'U', nr, size)`.
    fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
        (dir << 30) | ((size as u32) << 16) | (u32::from(b'U') << 8) | nr
    }

    #[test]
    fn the_request_numbers_encode_the_header_struct_sizes() {
        let (w, r, rw) = (1, 2, 3);
        assert_eq!(UI_DEV_SETUP, ioc(w, 3, DEV_SETUP));
        assert_eq!(UI_ABS_SETUP, ioc(w, 4, ABS_SETUP));
        assert_eq!(UI_SET_BIT | 101, ioc(w, 101, 4), "UI_SET_KEYBIT");
        assert_eq!(UI_SET_PHYS, ioc(w, 108, 8), "char *");
        assert_eq!(UI_SET_PHYS_COMPAT, ioc(w, 108, 4), "compat_uptr_t");
        assert_eq!(UI_BEGIN_FF_UPLOAD, ioc(rw, 200, FF_UPLOAD));
        assert_eq!(UI_END_FF_UPLOAD, ioc(w, 201, FF_UPLOAD));
        assert_eq!(UI_BEGIN_FF_UPLOAD_COMPAT, ioc(rw, 200, FF_UPLOAD_COMPAT));
        assert_eq!(UI_END_FF_UPLOAD_COMPAT, ioc(w, 201, FF_UPLOAD_COMPAT));
        assert_eq!(UI_BEGIN_FF_ERASE, ioc(rw, 202, FF_ERASE));
        assert_eq!(UI_END_FF_ERASE, ioc(w, 203, FF_ERASE));
        assert_eq!(UI_GET_VERSION, ioc(r, 45, 4));
        assert_eq!(UI_GET_SYSNAME_0, ioc(r, 44, 0));
        assert_eq!(FF_UPLOAD, 8 + 2 * FF_EFFECT);
        assert_eq!(FF_UPLOAD_COMPAT, 8 + 2 * FF_EFFECT_COMPAT);
        // ff_effect_compat: a 14-byte head padded to 16, then a 28-byte union whose largest
        // member is ff_periodic_effect_compat (18 bytes, padded to 20, custom_len, a u32 pointer).
        assert_eq!(FF_EFFECT_COMPAT, 16 + 28);
        assert_eq!(USER_DEV, 80 + 8 + 4 + 4 * 64 * 4);
    }

    #[test]
    fn only_gamepad_capabilities_pass() {
        let ev = |code| allowed(100, code);
        let key = |code| allowed(101, code);
        let abs = |code| allowed(NR_ABS, code);
        for code in [EV_SYN, EV_KEY, EV_ABS, EV_MSC, EV_FF] {
            assert!(ev(code), "EV {code:#x}");
        }
        for (code, what) in [
            (0x02, "EV_REL"),
            (0x05, "EV_SW"),
            (0x11, "EV_LED"),
            (0x12, "EV_SND"),
            (0x14, "EV_REP"),
            (0x16, "EV_PWR"),
        ] {
            assert!(!ev(code), "{what}");
        }
        for code in [0x120, 0x130, 0x13c, 0x13e, 0x220, 0x223, 0x2c0, 0x2e7] {
            assert!(key(code), "gamepad {code:#x}");
        }
        for (code, what) in [
            (0x01, "KEY_ESC"),
            (0x1e, "KEY_A"),
            (0x38, "KEY_LEFTALT, SysRq"),
            (0x63, "KEY_SYSRQ"),
            (0xff, "below BTN_MISC"),
            (0x100, "BTN_0"),
            (0x110, "BTN_LEFT"),
            (0x117, "BTN_TASK"),
            (0x140, "BTN_TOOL_PEN"),
            (0x145, "BTN_TOOL_FINGER"),
            (0x14a, "BTN_TOUCH"),
            (0x14f, "BTN_TOOL_QUINTTAP"),
            (0x1f1, "KEY_BRL_DOT1"),
            (0x2e8, "past BTN_TRIGGER_HAPPY40"),
        ] {
            assert!(!key(code), "{what}");
        }
        for code in [0x00, 0x05, 0x10, 0x17, 0x28] {
            assert!(abs(code), "ABS {code:#x}");
        }
        for (code, what) in [
            (0x18, "ABS_PRESSURE"),
            (0x1c, "ABS_TOOL_WIDTH"),
            (0x2f, "ABS_MT_SLOT"),
            (0x35, "ABS_MT_POSITION_X"),
            (0x3f, "ABS_MAX"),
        ] {
            assert!(!abs(code), "{what}");
        }
        for nr in [102, 105, 106, 109] {
            assert!(!allowed(nr, 0), "{}", bit_kind(nr));
        }
        assert!(allowed(110, INPUT_PROP_ACCELEROMETER));
        assert!(!allowed(110, 0), "INPUT_PROP_POINTER");
        assert!(!allowed(110, 1), "INPUT_PROP_DIRECT");
    }

    fn compat_event(sec: u32, usec: u32, kind: u16, code: u16, value: i32) -> Vec<u8> {
        [
            &sec.to_ne_bytes()[..],
            &usec.to_ne_bytes(),
            &kind.to_ne_bytes(),
            &code.to_ne_bytes(),
            &value.to_ne_bytes(),
        ]
        .concat()
    }

    fn native_event(sec: u64, usec: u64, kind: u16, code: u16, value: i32) -> Vec<u8> {
        [
            &sec.to_ne_bytes()[..],
            &usec.to_ne_bytes(),
            &kind.to_ne_bytes(),
            &code.to_ne_bytes(),
            &value.to_ne_bytes(),
        ]
        .concat()
    }

    #[test]
    fn events_cross_between_the_32_and_64_bit_layouts() {
        let compat = [
            compat_event(7, 500, 1, 0x130, 1),
            compat_event(u32::MAX, 0, 3, 0, -32768),
        ]
        .concat();
        let native = [
            native_event(7, 500, 1, 0x130, 1),
            native_event(u64::from(u32::MAX), 0, 3, 0, -32768),
        ]
        .concat();
        assert_eq!(events_from_compat(&compat), native);
        assert_eq!(events_to_compat(&native), compat);
        assert_eq!(
            events_from_compat(&compat[..20]).len(),
            EVENT,
            "a partial event"
        );
    }

    #[test]
    fn an_ff_upload_crosses_between_the_32_and_64_bit_layouts() {
        let mut native = [0u8; FF_UPLOAD];
        native[..4].copy_from_slice(&3u32.to_ne_bytes());
        native[4..8].copy_from_slice(&(-5i32).to_ne_bytes());
        for (i, b) in native[8..8 + FF_EFFECT_COMPAT].iter_mut().enumerate() {
            *b = i as u8 + 1;
        }
        for (i, b) in native[8 + FF_EFFECT..8 + FF_EFFECT + FF_EFFECT_COMPAT]
            .iter_mut()
            .enumerate()
        {
            *b = i as u8 + 101;
        }
        let compat = ff_upload_to_compat(&native);
        assert_eq!(&compat[..8], &native[..8]);
        assert_eq!(&compat[8..52], &native[8..52], "effect");
        assert_eq!(&compat[52..96], &native[56..100], "old");
        assert_eq!(ff_upload_from_compat(&compat), native);
    }

    #[test]
    fn a_store_name_round_trips() {
        assert_eq!(
            parse_name(&chan_name("pf-seat-1")),
            Some(Stored::Chan("pf-seat-1".into()))
        );
        for (compat, kh) in [(None, None), (Some(true), Some(9)), (Some(false), None)] {
            let name = open_name("pf-seat-1", 4, compat, kh);
            assert!(!name.contains(':'), "systemd's LISTEN_FDNAMES separator");
            assert_eq!(
                parse_name(&name),
                Some(Stored::Open {
                    account: "pf-seat-1".into(),
                    fh: 4,
                    compat,
                    kh
                })
            );
        }
        assert_eq!(parse_name("pad/1-0/10/0/x/k"), None);
        assert_eq!(parse_name("uinput/"), None);
        assert_eq!(parse_name("uinput/a/4/16/-"), None);
        assert_eq!(parse_name("uinput/a/4/32/-/x"), None);
    }

    /// Stands in for `/dev/uinput`: logs each request, and reads what the test queues.
    struct Fake {
        queue: UnixDatagram,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Fake {
        fn note(&self, line: String) {
            self.log.lock().unwrap().push(line);
        }
    }

    impl AsFd for Fake {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.queue.as_fd()
        }
    }

    impl Uinput for Fake {
        fn ioctl_value(&self, req: u32, value: u32) -> io::Result<()> {
            self.note(format!("value {req:#x} {value:#x}"));
            Ok(())
        }
        fn ioctl_buffer<const N: usize>(&self, req: u32, buf: &mut [u8; N]) -> io::Result<i32> {
            self.note(format!("buffer {req:#x} {N}"));
            if req == UI_BEGIN_FF_UPLOAD {
                // effect.type = FF_RUMBLE, effect.u.rumble.strong_magnitude = 0xc000.
                buf[8..10].copy_from_slice(&0x50u16.to_ne_bytes());
                buf[24..26].copy_from_slice(&0xc000u16.to_ne_bytes());
            }
            if req & !IOCSIZE_MASK == UI_GET_SYSNAME_0 {
                // A number no test box has: the record's write refreshes the real device.
                buf[..12].copy_from_slice(b"input987650\0");
            }
            Ok(0)
        }
        fn set_phys(&self, phys: &str) -> io::Result<()> {
            self.note(format!("phys {phys}"));
            Ok(())
        }
        fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.queue.recv(buf)
        }
        fn write(&self, buf: &[u8]) -> io::Result<usize> {
            self.note(format!("write {}", buf.len()));
            self.log.lock().unwrap().push(hex(buf));
            Ok(buf.len())
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The kernel's side of `/dev/cuse`.
    struct Kernel {
        sock: UnixDatagram,
        unique: u64,
    }

    impl Kernel {
        fn send(&mut self, opcode: u32, body: &[u8]) -> u64 {
            self.unique += 2;
            let request = cuse::request(opcode, self.unique, body);
            self.sock.send(&request).unwrap();
            self.unique
        }

        fn recv(&self) -> Vec<u8> {
            let mut buf = vec![0u8; 4096];
            let n = self.sock.recv(&mut buf).expect("an answer");
            buf.truncate(n);
            buf
        }

        /// The answer to `unique`: its error and payload.
        fn answer(&self, unique: u64) -> (i32, Vec<u8>) {
            let reply = self.recv();
            assert_eq!(u64_at(&reply, 8), unique, "answers in order");
            assert_eq!(u32_at(&reply, 0) as usize, reply.len());
            let error = i32::from_ne_bytes(reply[4..8].try_into().unwrap());
            (error, reply[cuse::OUT_HEADER_LEN..].to_vec())
        }

        fn ask(&mut self, opcode: u32, body: &[u8]) -> (i32, Vec<u8>) {
            let unique = self.send(opcode, body);
            self.answer(unique)
        }

        fn ioctl(&mut self, cmd: u32, arg: u64, data: &[u8], out_size: u32) -> (i32, Vec<u8>) {
            let body = [
                &1u64.to_ne_bytes()[..],
                &cuse::FUSE_IOCTL_32BIT.to_ne_bytes(),
                &cmd.to_ne_bytes(),
                &arg.to_ne_bytes(),
                &(data.len() as u32).to_ne_bytes(),
                &out_size.to_ne_bytes(),
                data,
            ]
            .concat();
            self.ask(cuse::FUSE_IOCTL, &body)
        }

        fn read(&mut self, size: u32, flags: u32) -> u64 {
            let body = [
                &1u64.to_ne_bytes()[..],
                &0u64.to_ne_bytes(),
                &size.to_ne_bytes(),
                &0u32.to_ne_bytes(),
                &0u64.to_ne_bytes(),
                &flags.to_ne_bytes(),
                &0u32.to_ne_bytes(),
            ]
            .concat();
            self.send(cuse::FUSE_READ, &body)
        }
    }

    /// An open's fd-store name keeps the ABI of its first ioctl: a caller that flips ABIs on
    /// every call stores nothing again.
    #[test]
    fn an_opens_abi_is_stored_once() {
        let (_kernel, ours) = pad_broker::relay_pair().unwrap();
        let opener = Box::new(|| {
            let (queue, _) = UnixDatagram::pair()?;
            Ok(Fake {
                queue,
                log: Arc::default(),
            })
        });
        let mut proxy = Proxy::new(File::from(ours), "pf-seat-1", opener, Vec::new());
        proxy.handle(&cuse::request(cuse::FUSE_OPEN, 2, &[0; 8]));
        let abis = [cuse::FUSE_IOCTL_32BIT, 0, cuse::FUSE_IOCTL_32BIT, 0];
        for (n, flags) in abis.into_iter().enumerate() {
            let body = [
                &1u64.to_ne_bytes()[..],
                &flags.to_ne_bytes(),
                &(UI_SET_BIT | 100).to_ne_bytes(),
                &1u64.to_ne_bytes(),
                &0u32.to_ne_bytes(),
                &0u32.to_ne_bytes(),
            ]
            .concat();
            proxy.handle(&cuse::request(cuse::FUSE_IOCTL, 4 + 2 * n as u64, &body));
            let stored = proxy.opens[&1].stored.as_deref();
            assert_eq!(stored, Some("uinput/pf-seat-1/1/32/-"), "call {n}");
        }
    }

    /// A 32-bit Steam's virtual pad through the proxy, over a socket standing in for
    /// `/dev/cuse` and a fake uinput.
    #[test]
    fn a_32_bit_pad_is_made_fed_and_rumbled_through_the_proxy() {
        let (theirs, ours) = pad_broker::relay_pair().unwrap();
        let mut kernel = Kernel {
            sock: theirs,
            unique: 0,
        };
        kernel
            .sock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (queues, queued) = mpsc::channel::<UnixDatagram>();
        let opener_log = Arc::clone(&log);
        let opener = Box::new(move || {
            let (queue, push) = UnixDatagram::pair()?;
            queue.set_nonblocking(true)?;
            queues.send(push).unwrap();
            Ok(Fake {
                queue,
                log: Arc::clone(&opener_log),
            })
        });
        let chan = File::from(ours);
        let mut proxy = Proxy::new(chan, "pf-seat-1", opener, Vec::new());
        let records = std::env::temp_dir().join(format!("pf-vouched-{}", std::process::id()));
        proxy.vouched = records.clone();
        let record = records.join("@input987650");
        let (stop, theirs) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || serve(proxy, theirs));
        let taken = |log: &Arc<Mutex<Vec<String>>>| std::mem::take(&mut *log.lock().unwrap());

        let (error, open) = kernel.ask(cuse::FUSE_OPEN, &[0; 8]);
        assert_eq!((error, u64_at(&open, 0)), (0, 1), "fh 1");
        let push = queued.recv().unwrap();

        assert_eq!(kernel.ioctl(UI_SET_BIT | 100, 1, &[], 0).0, 0, "EV_KEY");
        assert_eq!(
            kernel.ioctl(UI_SET_BIT | 101, 0x130, &[], 0).0,
            0,
            "BTN_SOUTH"
        );
        assert_eq!(
            kernel.ioctl(UI_SET_BIT | 101, 0x1e, &[], 0).0,
            -EPERM,
            "KEY_A"
        );
        assert_eq!(
            kernel.ioctl(UI_SET_BIT | 100, 2, &[], 0).0,
            -EPERM,
            "EV_REL"
        );
        assert_eq!(kernel.ioctl(UI_SET_PHYS_COMPAT, 0x1000, &[], 0).0, 0);
        assert_eq!(
            taken(&log),
            ["value 0x40045564 0x1", "value 0x40045565 0x130"],
            "refused bits never reach uinput"
        );

        // UI_DEV_SETUP: a retry for its 92 bytes, then the call with them.
        let (error, retry) = kernel.ioctl(UI_DEV_SETUP, 0x2000, &[], 0);
        assert_eq!(error, 0);
        assert_eq!(
            &retry[..16],
            &cuse::ioctl_retry(0, &[(0x2000, 92)], &[])[16..32]
        );
        assert_eq!(u64_at(&retry, 16), 0x2000);
        assert_eq!(u64_at(&retry, 24), 92);
        assert_eq!(kernel.ioctl(UI_DEV_SETUP, 0x2000, &[0; 92], 0).0, 0);
        let mut abs = [0u8; 28];
        abs[..2].copy_from_slice(&0x35u16.to_ne_bytes());
        assert_eq!(
            kernel.ioctl(UI_ABS_SETUP, 0x3000, &abs, 0).0,
            -EPERM,
            "ABS_MT_POSITION_X"
        );
        assert!(!record.exists(), "no record before the device exists");
        assert_eq!(kernel.ioctl(UI_DEV_CREATE, 0, &[], 0).0, 0);
        assert_eq!(
            taken(&log),
            [
                "buffer 0x405c5503 92",
                "phys punktfunk-seat:pf-seat-1/steam/1",
                "value 0x5501 0x0",
                "buffer 0x8040552c 64"
            ]
        );
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            "pf-seat-1\n",
            "recorded by the name the kernel gave it"
        );

        // Two 16-byte events in, two 24-byte events to uinput.
        let events = [compat_event(1, 2, 1, 0x130, 1), compat_event(1, 2, 0, 0, 0)].concat();
        let write = [
            &1u64.to_ne_bytes()[..],
            &[0; 8],
            &32u32.to_ne_bytes(),
            &[0; 20],
        ]
        .concat();
        let (error, written) = kernel.ask(cuse::FUSE_WRITE, &[write, events].concat());
        assert_eq!((error, u32_at(&written, 0)), (0, 32));
        let native = [native_event(1, 2, 1, 0x130, 1), native_event(1, 2, 0, 0, 0)].concat();
        assert_eq!(taken(&log), ["write 48".to_owned(), hex(&native)]);

        let unique = kernel.read(64, O_NONBLOCK);
        assert_eq!(kernel.answer(unique).0, -EAGAIN);
        let poll = [
            &1u64.to_ne_bytes()[..],
            &7u64.to_ne_bytes(),
            &1u32.to_ne_bytes(),
            &1u32.to_ne_bytes(),
        ]
        .concat();
        let (error, revents) = kernel.ask(cuse::FUSE_POLL, &poll);
        assert_eq!(
            (error, u32_at(&revents, 0)),
            (0, 0x104),
            "writable, not readable"
        );

        // uinput queues an FF upload request; the poller is woken, the read gets it as 16 bytes.
        push.send(&native_event(3, 4, 0x101, 1, 9)).unwrap();
        assert_eq!(kernel.recv(), cuse::notify_poll(7));
        let unique = kernel.read(32, O_NONBLOCK);
        let (error, event) = kernel.answer(unique);
        assert_eq!((error, event), (0, compat_event(3, 4, 0x101, 1, 9)));

        // UI_BEGIN_FF_UPLOAD_COMPAT: 96 bytes in and out, replayed as the 104-byte call.
        let (_, retry) = kernel.ioctl(UI_BEGIN_FF_UPLOAD_COMPAT, 0x4000, &[], 0);
        assert_eq!(u32_at(&retry, 8), 1, "one in iovec");
        assert_eq!(u32_at(&retry, 12), 1, "one out iovec");
        let mut upload = [0u8; FF_UPLOAD_COMPAT];
        upload[..4].copy_from_slice(&9u32.to_ne_bytes());
        let (error, out) = kernel.ioctl(UI_BEGIN_FF_UPLOAD_COMPAT, 0x4000, &upload, 96);
        assert_eq!((error, out.len()), (0, 16 + FF_UPLOAD_COMPAT));
        let out = &out[16..];
        assert_eq!(u32_at(out, 0), 9, "request id");
        assert_eq!(&out[8..10], &0x50u16.to_ne_bytes(), "FF_RUMBLE");
        assert_eq!(&out[24..26], &0xc000u16.to_ne_bytes(), "strong magnitude");
        assert_eq!(taken(&log), ["buffer 0xc06855c8 104"]);

        // A blocking read waits; the kernel's interrupt ends it with EINTR.
        let waiting = kernel.read(32, 0);
        kernel.send(cuse::FUSE_INTERRUPT, &waiting.to_ne_bytes());
        assert_eq!(kernel.answer(waiting).0, -EINTR);

        // The cap: the open already made, then one short of it, then a refusal.
        for _ in 1..MAX_OPENS {
            assert_eq!(kernel.ask(cuse::FUSE_OPEN, &[0; 8]).0, 0);
        }
        assert_eq!(kernel.ask(cuse::FUSE_OPEN, &[0; 8]).0, -ENOSPC);
        let release = [&1u64.to_ne_bytes()[..], &[0; 16]].concat();
        assert_eq!(kernel.ask(cuse::FUSE_RELEASE, &release), (0, Vec::new()));
        assert!(!record.exists(), "a released device's record goes");
        assert_eq!(kernel.ask(cuse::FUSE_OPEN, &[0; 8]).0, 0, "a slot back");
        assert_eq!(kernel.ask(46, &[0; 24]).0, -ENOSYS, "FUSE_LSEEK");

        drop(stop);
        thread.join().unwrap();
        let _ = std::fs::remove_dir_all(&records);
    }

    /// A drain answers the reads a proxy holds with `EINTR` and reads nothing more: what the
    /// kernel queues afterwards waits for the next supervisor.
    #[test]
    fn a_drained_proxy_ends_its_waiting_reads_and_takes_no_more() {
        let (theirs, ours) = pad_broker::relay_pair().unwrap();
        let mut kernel = Kernel {
            sock: theirs,
            unique: 0,
        };
        kernel
            .sock
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (queues, queued) = mpsc::channel::<UnixDatagram>();
        let opener = Box::new(move || {
            let (queue, push) = UnixDatagram::pair()?;
            queue.set_nonblocking(true)?;
            queues.send(push).unwrap();
            Ok(Fake {
                queue,
                log: Arc::default(),
            })
        });
        // The fd store's copy: the connection outlives the proxy.
        let _stored = ours.try_clone().unwrap();
        let proxy = Proxy::new(File::from(ours), "pf-seat-1", opener, Vec::new());
        let (stop, theirs) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(move || serve(proxy, theirs));

        assert_eq!(kernel.ask(cuse::FUSE_OPEN, &[0; 8]).0, 0);
        let _push = queued.recv().unwrap();
        let first = kernel.read(32, 0);
        let second = kernel.read(32, 0);
        assert_eq!(kernel.ask(46, &[0; 24]).0, -ENOSYS, "both reads were taken");

        (&stop).write_all(b"d").unwrap();
        let ended = [kernel.answer(first).0, kernel.answer(second).0];
        assert_eq!(ended, [-EINTR, -EINTR]);
        thread.join().unwrap();

        kernel.send(46, &[0; 24]);
        kernel
            .sock
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        assert!(kernel.sock.recv(&mut [0u8; 64]).is_err(), "nobody reads it");
    }

    #[test]
    fn only_a_root_owned_cuse_node_without_group_or_other_access_is_closed() {
        assert!(root_only(0, 0o600));
        assert!(root_only(0, 0o020_600));
        assert!(!root_only(0, 0o660));
        assert!(!root_only(0, 0o606));
        assert!(!root_only(1000, 0o600));
    }
}
