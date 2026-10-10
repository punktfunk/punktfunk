//! `/dev/uinput` ABI (`linux/uinput.h`) shared by the uinput devices: the ioctl numbers, the
//! `#[repr(C)]` setup structs, the `input_event` encoding, the FF upload protocol and
//! [`UinputDevice`], which owns the fd and destroys the device on drop. Capabilities (keys,
//! axes, FF, props) are each device's own data. Typed `ioctl` and `open` come from
//! [`crate::uapi`].
//!
//! On a seat the kernel fd is the supervisor's ([`crate::pad_broker`]): the device here is one
//! end of a relay socket. It sends the same `input_event`s, one datagram per `SYN_REPORT`
//! frame, and reads the FF plane as [`FfNotice`]s the supervisor already answered the kernel
//! for. A relay the supervisor dropped reads as dead ([`UinputDevice::alive`]), and the pad is
//! made again.
//!
//! The numbers are the generic Linux ioctl encoding, the same on x86_64 and arm64; the
//! asserts pin each struct to the size its request encodes. `/dev/uinput` needs the udev rule
//! and the `input` group (`scripts/60-punktfunk.rules`).

use crate::uapi::{self, Pod};
use anyhow::{anyhow, bail, Context, Result};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::mem::size_of;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixDatagram;

pub(crate) const UI_DEV_CREATE: libc::c_ulong = 0x5501;
pub(crate) const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
pub(crate) const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
pub(crate) const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
pub(crate) const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
pub(crate) const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
pub(crate) const UI_SET_FFBIT: libc::c_ulong = 0x4004_556b;
/// `_IOW('U', 108, char*)`: the device's `phys`, read as a C string.
const UI_SET_PHYS: libc::c_ulong = 0x4008_556c;
pub(crate) const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;
const UI_BEGIN_FF_UPLOAD: libc::c_ulong = 0xc068_55c8;
const UI_END_FF_UPLOAD: libc::c_ulong = 0x4068_55c9;
const UI_BEGIN_FF_ERASE: libc::c_ulong = 0xc00c_55ca;
const UI_END_FF_ERASE: libc::c_ulong = 0x400c_55cb;

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const EV_FF: u16 = 0x15;
const EV_UINPUT: u16 = 0x0101;
pub(crate) const SYN_REPORT: u16 = 0;
const UI_FF_UPLOAD: u16 = 1;
const UI_FF_ERASE: u16 = 2;
pub(crate) const FF_RUMBLE: u16 = 0x50;
pub(crate) const FF_GAIN: u16 = 0x60;

#[repr(C)]
pub(crate) struct InputId {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[repr(C)]
struct UinputSetup {
    id: InputId,
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub(crate) struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    _pad: u16,
    absinfo: AbsInfo,
}

/// `struct ff_effect` (48 bytes; the union starts at offset 16).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FfEffect {
    type_: u16,
    id: i16,
    direction: u16,
    trigger_button: u16,
    trigger_interval: u16,
    replay_length: u16,
    replay_delay: u16,
    _pad: u16,
    /// Union; for `FF_RUMBLE`: `u16 strong_magnitude` at [0..2], `u16 weak_magnitude` at [2..4].
    u: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UinputFfUpload {
    request_id: u32,
    retval: i32,
    effect: FfEffect,
    old: FfEffect,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UinputFfErase {
    request_id: u32,
    retval: i32,
    effect_id: u32,
}

/// `struct input_event`: a 16-byte `timeval` the kernel stamps, then type, code, value.
pub(crate) const INPUT_EVENT_LEN: usize = 24;

// `<linux/uinput.h>` sizes, the ones the ioctl numbers above encode.
const _: () = {
    assert!(size_of::<UinputSetup>() == 92);
    assert!(size_of::<UinputAbsSetup>() == 28);
    assert!(size_of::<FfEffect>() == 48);
    assert!(size_of::<UinputFfUpload>() == 104);
    assert!(size_of::<UinputFfErase>() == 12);
    assert!(uapi::arg_size(UI_DEV_SETUP) == size_of::<UinputSetup>());
    assert!(uapi::arg_size(UI_ABS_SETUP) == size_of::<UinputAbsSetup>());
    assert!(uapi::arg_size(UI_BEGIN_FF_UPLOAD) == size_of::<UinputFfUpload>());
    assert!(uapi::arg_size(UI_BEGIN_FF_ERASE) == size_of::<UinputFfErase>());
    assert!(size_of::<libc::input_event>() == INPUT_EVENT_LEN);
};

// SAFETY: `#[repr(C)]` integers and a byte array; the sizes above are the field sums, so
// neither struct has padding.
unsafe impl Pod for UinputSetup {}
// SAFETY: as `UinputSetup`.
unsafe impl Pod for UinputAbsSetup {}
// SAFETY: as `UinputSetup`.
unsafe impl Pod for UinputFfUpload {}
// SAFETY: as `UinputSetup`.
unsafe impl Pod for UinputFfErase {}

pub(crate) fn input_event(type_: u16, code: u16, value: i32) -> [u8; INPUT_EVENT_LEN] {
    let mut ev = [0u8; INPUT_EVENT_LEN];
    ev[16..18].copy_from_slice(&type_.to_ne_bytes());
    ev[18..20].copy_from_slice(&code.to_ne_bytes());
    ev[20..24].copy_from_slice(&value.to_ne_bytes());
    ev
}

/// `(type, code, value)` of an event read back from the node.
fn parse_input_event(ev: &[u8; INPUT_EVENT_LEN]) -> (u16, u16, i32) {
    (
        u16::from_ne_bytes([ev[16], ev[17]]),
        u16::from_ne_bytes([ev[18], ev[19]]),
        i32::from_ne_bytes([ev[20], ev[21], ev[22], ev[23]]),
    )
}

/// One thing the game did on a pad's FF plane, with the upload and erase ioctl halves already
/// answered: what the mixer takes, on either side of a relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FfNotice {
    /// An `FF_RUMBLE` effect landed in kernel slot `id`.
    Upload {
        id: i16,
        strong: u16,
        weak: u16,
        replay_ms: u16,
        delay_ms: u16,
    },
    Erase {
        id: i16,
    },
    Gain(u32),
    Play {
        id: i16,
        on: bool,
    },
}

/// One notice per relay datagram: tag, id, four `u16`s, one `u32`.
pub(crate) const FF_NOTICE_LEN: usize = 16;

impl FfNotice {
    pub(crate) fn encode(self) -> [u8; FF_NOTICE_LEN] {
        let mut b = [0u8; FF_NOTICE_LEN];
        let (tag, id, a, c, d, e, g) = match self {
            FfNotice::Upload {
                id,
                strong,
                weak,
                replay_ms,
                delay_ms,
            } => (1, id, strong, weak, replay_ms, delay_ms, 0),
            FfNotice::Erase { id } => (2, id, 0, 0, 0, 0, 0),
            FfNotice::Gain(gain) => (3, 0, 0, 0, 0, 0, gain),
            FfNotice::Play { id, on } => (4, id, on as u16, 0, 0, 0, 0),
        };
        b[0] = tag;
        b[2..4].copy_from_slice(&id.to_ne_bytes());
        b[4..6].copy_from_slice(&a.to_ne_bytes());
        b[6..8].copy_from_slice(&c.to_ne_bytes());
        b[8..10].copy_from_slice(&d.to_ne_bytes());
        b[10..12].copy_from_slice(&e.to_ne_bytes());
        b[12..16].copy_from_slice(&g.to_ne_bytes());
        b
    }

    pub(crate) fn decode(b: &[u8]) -> Option<FfNotice> {
        if b.len() != FF_NOTICE_LEN {
            return None;
        }
        let u16_at = |i: usize| u16::from_ne_bytes([b[i], b[i + 1]]);
        let id = i16::from_ne_bytes([b[2], b[3]]);
        Some(match b[0] {
            1 => FfNotice::Upload {
                id,
                strong: u16_at(4),
                weak: u16_at(6),
                replay_ms: u16_at(8),
                delay_ms: u16_at(10),
            },
            2 => FfNotice::Erase { id },
            3 => FfNotice::Gain(u32::from_ne_bytes([b[12], b[13], b[14], b[15]])),
            4 => FfNotice::Play {
                id,
                on: u16_at(4) != 0,
            },
            _ => return None,
        })
    }
}

/// Events of one frame, at most, before a relayed device sends them unasked.
const RELAY_BATCH_MAX: usize = 64 * INPUT_EVENT_LEN;

enum Inner {
    /// `/dev/uinput` itself.
    Kernel(File),
    /// The seat's end of the supervisor's relay. `dead` once the supervisor hung up.
    Relayed {
        sock: UnixDatagram,
        batch: Vec<u8>,
        dead: bool,
    },
}

/// One `/dev/uinput` device. Set its capabilities, then [`create`](Self::create); drop sends
/// `UI_DEV_DESTROY` before the fd closes. A [`relayed`](Self::relayed) device is built by the
/// supervisor and only emits and reads; its drop closes the relay, which is its destroy.
pub(crate) struct UinputDevice {
    inner: Inner,
}

impl UinputDevice {
    /// Open `/dev/uinput` non-blocking, so a read drains the FF queue without waiting.
    pub(crate) fn open() -> Result<UinputDevice> {
        let fd = uapi::open_nonblock("/dev/uinput").map_err(|e| {
            anyhow!(
                "open /dev/uinput: {e} (install the udev rule granting the 'input' group access \
                 — see scripts/60-punktfunk.rules — and add the user to the 'input' group)"
            )
        })?;
        Ok(UinputDevice {
            inner: Inner::Kernel(fd),
        })
    }

    /// The seat's end of a relay the supervisor answered ([`crate::pad_broker::request`]).
    pub(crate) fn relayed(fd: OwnedFd) -> Result<UinputDevice> {
        let sock = UnixDatagram::from(fd);
        sock.set_nonblocking(true)
            .context("set the pad relay non-blocking")?;
        Ok(UinputDevice {
            inner: Inner::Relayed {
                sock,
                batch: Vec::with_capacity(RELAY_BATCH_MAX),
                dead: false,
            },
        })
    }

    fn kernel(&self, what: &str) -> Result<&File> {
        match &self.inner {
            Inner::Kernel(fd) => Ok(fd),
            Inner::Relayed { .. } => bail!("{what}: a relayed pad is built by the seat supervisor"),
        }
    }

    /// Enable each of `codes` with one `UI_SET_*BIT` request.
    pub(crate) fn set_bits(&self, req: libc::c_ulong, what: &str, codes: &[u16]) -> Result<()> {
        let fd = self.kernel(what)?;
        for &code in codes {
            uapi::ioctl_value(fd.as_fd(), req, code.into())
                .with_context(|| format!("{what}({code:#x})"))?;
        }
        Ok(())
    }

    pub(crate) fn abs(&self, code: u16, absinfo: AbsInfo) -> Result<()> {
        let fd = self.kernel("UI_ABS_SETUP")?;
        let mut a = UinputAbsSetup {
            code,
            _pad: 0,
            absinfo,
        };
        uapi::ioctl_with(fd.as_fd(), UI_ABS_SETUP, &mut a).context("UI_ABS_SETUP")?;
        Ok(())
    }

    /// `UI_SET_PHYS`, before [`create`](Self::create): the marker a seat's pad carries.
    pub(crate) fn set_phys(&self, phys: &str) -> Result<()> {
        let fd = self.kernel("UI_SET_PHYS")?;
        let phys = std::ffi::CString::new(phys).context("UI_SET_PHYS: phys holds a NUL")?;
        uapi::ioctl_cstr(fd.as_fd(), UI_SET_PHYS, &phys).context("UI_SET_PHYS")?;
        Ok(())
    }

    /// `UI_DEV_SETUP` then `UI_DEV_CREATE`. `name` is truncated to the 79 bytes the setup holds.
    pub(crate) fn create(&self, id: InputId, name: &[u8], ff_effects_max: u32) -> Result<()> {
        let fd = self.kernel("UI_DEV_SETUP")?;
        let mut setup = UinputSetup {
            id,
            name: [0; 80],
            ff_effects_max,
        };
        let n = name.len().min(setup.name.len() - 1);
        setup.name[..n].copy_from_slice(&name[..n]);
        uapi::ioctl_with(fd.as_fd(), UI_DEV_SETUP, &mut setup).context("UI_DEV_SETUP")?;
        uapi::ioctl_value(fd.as_fd(), UI_DEV_CREATE, 0).context("UI_DEV_CREATE")?;
        Ok(())
    }

    /// Best-effort: a full kernel queue drops the event, and the next frame re-syncs state. A
    /// relayed device sends the frame at its `SYN_REPORT`.
    pub(crate) fn emit(&mut self, type_: u16, code: u16, value: i32) {
        match &mut self.inner {
            Inner::Kernel(fd) => {
                let _ = (&*fd).write(&input_event(type_, code, value));
            }
            Inner::Relayed { sock, batch, dead } => {
                batch.extend_from_slice(&input_event(type_, code, value));
                if type_ != EV_SYN && batch.len() < RELAY_BATCH_MAX {
                    return;
                }
                let sent = sock.send(batch);
                batch.clear();
                match sent {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => *dead = true,
                }
            }
        }
    }

    /// The supervisor's side of the relay: a seat's frame, written to the kernel whole.
    pub(crate) fn write_batch(&self, events: &[u8]) {
        if let Inner::Kernel(fd) = &self.inner {
            let _ = (&*fd).write(events);
        }
    }

    /// `false` once the supervisor dropped this device's relay. A kernel device is always alive.
    pub(crate) fn alive(&self) -> bool {
        !matches!(self.inner, Inner::Relayed { dead: true, .. })
    }

    /// The next queued `(type, code, value)`, or `None` once EAGAIN or a short read says the
    /// queue is drained.
    fn read_event(fd: &File) -> Option<(u16, u16, i32)> {
        let mut buf = [0u8; INPUT_EVENT_LEN];
        matches!((&*fd).read(&mut buf), Ok(n) if n == buf.len()).then(|| parse_input_event(&buf))
    }

    /// The next thing the game did on the FF plane, or `None` once the queue is drained. On the
    /// kernel device this answers `UI_BEGIN/END_FF_*` at once: a game's `EVIOCSFF` blocks until
    /// it is answered. Call often.
    pub(crate) fn next_ff(&mut self) -> Option<FfNotice> {
        match &mut self.inner {
            Inner::Kernel(fd) => loop {
                let (type_, code, value) = Self::read_event(fd)?;
                let notice = match (type_, code) {
                    (EV_UINPUT, UI_FF_UPLOAD) => Self::answer_upload(fd, value as u32),
                    (EV_UINPUT, UI_FF_ERASE) => Self::answer_erase(fd, value as u32),
                    (EV_FF, FF_GAIN) => Some(FfNotice::Gain((value as u32).min(0xFFFF))),
                    (EV_FF, id) => Some(FfNotice::Play {
                        id: id as i16,
                        on: value != 0,
                    }),
                    _ => None,
                };
                if notice.is_some() {
                    return notice;
                }
            },
            Inner::Relayed { sock, dead, .. } => {
                let mut buf = [0u8; FF_NOTICE_LEN];
                loop {
                    match sock.recv(&mut buf) {
                        Ok(0) => {
                            *dead = true;
                            return None;
                        }
                        Ok(n) => {
                            if let Some(notice) = FfNotice::decode(&buf[..n]) {
                                return Some(notice);
                            }
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => return None,
                        Err(_) => {
                            *dead = true;
                            return None;
                        }
                    }
                }
            }
        }
    }

    /// `UI_BEGIN_FF_UPLOAD` … `UI_END_FF_UPLOAD`. ff-core assigned the slot before uinput saw
    /// the request, so the kernel's id is handed straight back. A type other than rumble is
    /// answered and dropped.
    fn answer_upload(fd: &File, request_id: u32) -> Option<FfNotice> {
        let mut up = UinputFfUpload {
            request_id,
            ..Default::default()
        };
        uapi::ioctl_with(fd.as_fd(), UI_BEGIN_FF_UPLOAD, &mut up).ok()?;
        let e = up.effect;
        debug_assert!(e.id >= 0, "uinput handed us an unassigned FF effect id");
        let notice = (e.type_ == FF_RUMBLE).then(|| FfNotice::Upload {
            id: e.id,
            strong: u16::from_ne_bytes([e.u[0], e.u[1]]),
            weak: u16::from_ne_bytes([e.u[2], e.u[3]]),
            replay_ms: e.replay_length,
            delay_ms: e.replay_delay,
        });
        up.effect.id = e.id;
        up.retval = 0;
        let _ = uapi::ioctl_with(fd.as_fd(), UI_END_FF_UPLOAD, &mut up);
        notice
    }

    fn answer_erase(fd: &File, request_id: u32) -> Option<FfNotice> {
        let mut er = UinputFfErase {
            request_id,
            ..Default::default()
        };
        uapi::ioctl_with(fd.as_fd(), UI_BEGIN_FF_ERASE, &mut er).ok()?;
        let id = er.effect_id as i16;
        er.retval = 0;
        let _ = uapi::ioctl_with(fd.as_fd(), UI_END_FF_ERASE, &mut er);
        Some(FfNotice::Erase { id })
    }
}

impl AsFd for UinputDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match &self.inner {
            Inner::Kernel(fd) => fd.as_fd(),
            Inner::Relayed { sock, .. } => sock.as_fd(),
        }
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        // The fd closes only after this body. Errors are moot on teardown.
        if let Inner::Kernel(fd) = &self.inner {
            let _ = uapi::ioctl_value(fd.as_fd(), UI_DEV_DESTROY, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_round_trips() {
        assert_eq!(
            parse_input_event(&input_event(0x15, 0x50, -7)),
            (0x15, 0x50, -7)
        );
    }

    #[test]
    fn ff_notices_round_trip_and_junk_is_refused() {
        let all = [
            FfNotice::Upload {
                id: 3,
                strong: 0xC000,
                weak: 0x4000,
                replay_ms: 5000,
                delay_ms: 20,
            },
            FfNotice::Erase { id: -1 },
            FfNotice::Gain(0xFFFF),
            FfNotice::Play { id: 7, on: true },
            FfNotice::Play { id: 7, on: false },
        ];
        for n in all {
            assert_eq!(FfNotice::decode(&n.encode()), Some(n));
        }
        assert_eq!(FfNotice::decode(&[9; FF_NOTICE_LEN]), None);
        assert_eq!(FfNotice::decode(&[1; 15]), None);
    }

    /// A relayed device sends one datagram per frame, and reads a notice the other end sent.
    #[test]
    fn a_relayed_device_batches_a_frame_and_reads_notices() {
        let (seat, supervisor) = UnixDatagram::pair().unwrap();
        let mut dev = UinputDevice::relayed(OwnedFd::from(seat)).unwrap();
        supervisor.set_nonblocking(true).unwrap();
        dev.emit(EV_KEY, 0x130, 1);
        dev.emit(EV_ABS, 0, -500);
        let mut buf = [0u8; 1024];
        assert!(
            supervisor.recv(&mut buf).is_err(),
            "nothing before SYN_REPORT"
        );
        dev.emit(EV_SYN, SYN_REPORT, 0);
        let n = supervisor.recv(&mut buf).unwrap();
        assert_eq!(n, 3 * INPUT_EVENT_LEN);
        assert_eq!(&buf[..INPUT_EVENT_LEN], &input_event(EV_KEY, 0x130, 1));

        assert_eq!(dev.next_ff(), None);
        supervisor
            .send(&FfNotice::Play { id: 2, on: true }.encode())
            .unwrap();
        assert_eq!(dev.next_ff(), Some(FfNotice::Play { id: 2, on: true }));
        assert!(dev.alive());

        drop(supervisor);
        dev.emit(EV_SYN, SYN_REPORT, 0);
        assert!(!dev.alive(), "a hung-up relay reads as dead");
    }
}
