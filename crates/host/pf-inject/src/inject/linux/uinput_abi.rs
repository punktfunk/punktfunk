//! `/dev/uinput` ABI (`linux/uinput.h`) shared by the uinput devices: the ioctl numbers, the
//! `#[repr(C)]` setup structs, the `input_event` encoding and [`UinputDevice`], which owns the
//! fd and destroys the device on drop. Capabilities (keys, axes, FF, props) are each device's
//! own data. Typed `ioctl` and `open` come from [`crate::uapi`].
//!
//! The numbers are the generic Linux ioctl encoding, the same on x86_64 and arm64; the
//! asserts pin each struct to the size its request encodes. `/dev/uinput` needs the udev rule
//! and the `input` group (`scripts/60-punktfunk.rules`).

use crate::uapi::{self, Pod};
use anyhow::{anyhow, Context, Result};
use std::fs::File;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsFd, BorrowedFd};

pub(crate) const UI_DEV_CREATE: libc::c_ulong = 0x5501;
pub(crate) const UI_DEV_DESTROY: libc::c_ulong = 0x5502;
pub(crate) const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
pub(crate) const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
pub(crate) const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
pub(crate) const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
pub(crate) const UI_SET_FFBIT: libc::c_ulong = 0x4004_556b;
pub(crate) const UI_SET_PROPBIT: libc::c_ulong = 0x4004_556e;

pub(crate) const EV_SYN: u16 = 0x00;
pub(crate) const EV_KEY: u16 = 0x01;
pub(crate) const EV_ABS: u16 = 0x03;
pub(crate) const SYN_REPORT: u16 = 0;

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

/// `struct input_event`: a 16-byte `timeval` the kernel stamps, then type, code, value.
pub(crate) const INPUT_EVENT_LEN: usize = 24;

// `<linux/uinput.h>` sizes, the ones the ioctl numbers above encode.
const _: () = {
    assert!(size_of::<UinputSetup>() == 92);
    assert!(size_of::<UinputAbsSetup>() == 28);
    assert!(uapi::arg_size(UI_DEV_SETUP) == size_of::<UinputSetup>());
    assert!(uapi::arg_size(UI_ABS_SETUP) == size_of::<UinputAbsSetup>());
    assert!(size_of::<libc::input_event>() == INPUT_EVENT_LEN);
};

// SAFETY: `#[repr(C)]` integers and a byte array; the sizes above are the field sums, so
// neither struct has padding.
unsafe impl Pod for UinputSetup {}
// SAFETY: as `UinputSetup`.
unsafe impl Pod for UinputAbsSetup {}

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

/// One `/dev/uinput` device. Set its capabilities, then [`create`](Self::create); drop sends
/// `UI_DEV_DESTROY` before the fd closes.
pub(crate) struct UinputDevice {
    fd: File,
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
        Ok(UinputDevice { fd })
    }

    /// Enable each of `codes` with one `UI_SET_*BIT` request.
    pub(crate) fn set_bits(&self, req: libc::c_ulong, what: &str, codes: &[u16]) -> Result<()> {
        for &code in codes {
            uapi::ioctl_value(self.fd.as_fd(), req, code.into())
                .with_context(|| format!("{what}({code:#x})"))?;
        }
        Ok(())
    }

    pub(crate) fn abs(&self, code: u16, absinfo: AbsInfo) -> Result<()> {
        let mut a = UinputAbsSetup {
            code,
            _pad: 0,
            absinfo,
        };
        uapi::ioctl_with(self.fd.as_fd(), UI_ABS_SETUP, &mut a).context("UI_ABS_SETUP")?;
        Ok(())
    }

    /// `UI_DEV_SETUP` then `UI_DEV_CREATE`. `name` is truncated to the 79 bytes the setup holds.
    pub(crate) fn create(&self, id: InputId, name: &[u8], ff_effects_max: u32) -> Result<()> {
        let mut setup = UinputSetup {
            id,
            name: [0; 80],
            ff_effects_max,
        };
        let n = name.len().min(setup.name.len() - 1);
        setup.name[..n].copy_from_slice(&name[..n]);
        uapi::ioctl_with(self.fd.as_fd(), UI_DEV_SETUP, &mut setup).context("UI_DEV_SETUP")?;
        uapi::ioctl_value(self.fd.as_fd(), UI_DEV_CREATE, 0).context("UI_DEV_CREATE")?;
        Ok(())
    }

    /// Best-effort: a full kernel queue drops the event, and the next frame re-syncs state.
    pub(crate) fn emit(&self, type_: u16, code: u16, value: i32) {
        let _ = (&self.fd).write(&input_event(type_, code, value));
    }

    /// The next queued `(type, code, value)`, or `None` once EAGAIN or a short read says the
    /// queue is drained.
    pub(crate) fn read_event(&self) -> Option<(u16, u16, i32)> {
        let mut buf = [0u8; INPUT_EVENT_LEN];
        matches!((&self.fd).read(&mut buf), Ok(n) if n == buf.len())
            .then(|| parse_input_event(&buf))
    }
}

impl AsFd for UinputDevice {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for UinputDevice {
    fn drop(&mut self) {
        // The fd closes only after this body. Errors are moot on teardown.
        let _ = uapi::ioctl_value(self.fd.as_fd(), UI_DEV_DESTROY, 0);
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
}
