//! The seat pad broker's wire and both ends of its relay.
//!
//! A seat host cannot open `/dev/uinput`: that node is the console user's. It asks the root
//! seat supervisor over [`SOCKET`] for a pad by [`PadKind`]; the supervisor builds the device
//! from this crate's own tables ([`build`]), keeps the kernel fd, and hands the seat one end of
//! a `SOCK_SEQPACKET` pair. [`relay`] moves the seat's `input_event` frames into the device and
//! the device's FF plane back as [`FfNotice`](crate::uinput_abi::FfNotice)s. No kernel fd ever
//! crosses: a passed uinput fd can destroy its pad and create any device.
//!
//! A USB/IP pad stays the seat's own usbip server. The seat runs the import handshake and sends
//! the connected socket with [`attach`]; the supervisor picks the vhci port and writes `attach`.
//! Design: `design/seat-pad-broker.md`.

use crate::gamepad::{build_pad, PadIdentity};
use crate::uhid_abi::{Identity, UhidDevice};
use crate::uinput_abi::{UinputDevice, INPUT_EVENT_LEN};
use anyhow::{anyhow, bail, Context, Result};
use rustix::event::{poll, PollFd, PollFlags};
use rustix::net::{
    recvmsg, sendmsg, socketpair, AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage,
    RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};
use std::io::{ErrorKind, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::time::Duration;

pub use pf_paths::seat::PADS_SOCKET as SOCKET;

/// A pad a seat may ask for. The number is the wire value: append, never renumber. 1–9 are
/// uinput pads, the rest uhid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PadKind {
    Xbox360 = 1,
    XboxOne = 2,
    XboxElite2 = 3,
    DualSense = 10,
    DualSenseEdge = 11,
    DualShock4 = 12,
    SteamDeck = 13,
    SteamController = 14,
    SteamController2 = 15,
    SwitchPro = 16,
    JoyConLeft = 17,
    JoyConRight = 18,
    EightBitDoUltimate2 = 19,
    EightBitDoPro2 = 20,
    EightBitDoPro3 = 21,
    HoripadSteam = 22,
}

impl PadKind {
    pub fn from_wire(value: u8) -> Option<PadKind> {
        Some(match value {
            1 => PadKind::Xbox360,
            2 => PadKind::XboxOne,
            3 => PadKind::XboxElite2,
            10 => PadKind::DualSense,
            11 => PadKind::DualSenseEdge,
            12 => PadKind::DualShock4,
            13 => PadKind::SteamDeck,
            14 => PadKind::SteamController,
            15 => PadKind::SteamController2,
            16 => PadKind::SwitchPro,
            17 => PadKind::JoyConLeft,
            18 => PadKind::JoyConRight,
            19 => PadKind::EightBitDoUltimate2,
            20 => PadKind::EightBitDoPro2,
            21 => PadKind::EightBitDoPro3,
            22 => PadKind::HoripadSteam,
            _ => return None,
        })
    }

    /// The uinput identity of a uinput kind.
    fn uinput(self) -> Option<PadIdentity> {
        PadIdentity::of(self)
    }

    /// The uhid identity of a uhid kind at `index`, as the backend itself builds it.
    fn uhid(self, index: u8) -> Option<Identity> {
        use crate::eightbitdo_proto::Model;
        use crate::steam_proto::SteamModel;
        Some(match self {
            PadKind::DualSense => {
                crate::dualsense::identity(index, &crate::dualsense::DsUhidIdentity::dualsense())
            }
            PadKind::DualSenseEdge => crate::dualsense::identity(
                index,
                &crate::dualsense::DsUhidIdentity::dualsense_edge(),
            ),
            PadKind::DualShock4 => crate::dualshock4::identity(index),
            PadKind::SteamDeck => crate::steam_controller::identity(index, SteamModel::Deck),
            PadKind::SteamController => {
                crate::steam_controller::identity(index, SteamModel::Controller)
            }
            PadKind::SteamController2 => crate::steam_controller2::identity(index),
            PadKind::SwitchPro => crate::switch_pro::identity(index, None),
            PadKind::JoyConLeft => {
                crate::switch_pro::identity(index, Some(crate::switch_proto::Half::Left))
            }
            PadKind::JoyConRight => {
                crate::switch_pro::identity(index, Some(crate::switch_proto::Half::Right))
            }
            PadKind::EightBitDoUltimate2 => crate::eightbitdo::identity(Model::Ultimate2, index),
            PadKind::EightBitDoPro2 => crate::eightbitdo::identity(Model::Pro2, index),
            PadKind::EightBitDoPro3 => crate::eightbitdo::identity(Model::Pro3, index),
            PadKind::HoripadSteam => crate::hori_steam::identity(index),
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            PadKind::Xbox360 | PadKind::XboxOne | PadKind::XboxElite2 => {
                PadIdentity::of(self).map_or("Xbox pad", |id| id.log())
            }
            PadKind::DualSense => "DualSense",
            PadKind::DualSenseEdge => "DualSense Edge",
            PadKind::DualShock4 => "DualShock 4",
            PadKind::SteamDeck => "Steam Deck",
            PadKind::SteamController => "Steam Controller",
            PadKind::SteamController2 => "Steam Controller 2",
            PadKind::SwitchPro => "Switch Pro",
            PadKind::JoyConLeft => "Joy-Con (L)",
            PadKind::JoyConRight => "Joy-Con (R)",
            PadKind::EightBitDoUltimate2 => "8BitDo Ultimate 2",
            PadKind::EightBitDoPro2 => "8BitDo Pro 2",
            PadKind::EightBitDoPro3 => "8BitDo Pro 3",
            PadKind::HoripadSteam => "HORIPAD Steam",
        }
    }
}

/// The marker on a seat's pad: `punktfunk-seat:<account>/<rest>`, what the udev fence reads.
pub fn seat_phys(account: &str, rest: &str) -> String {
    format!("punktfunk-seat:{account}/{rest}")
}

const MAGIC: [u8; 4] = *b"PFPD";
const VERSION: u8 = 2;
const OP_CREATE: u8 = 1;
const OP_ATTACH: u8 = 2;
const OP_DETACH: u8 = 3;
/// A request, whole: magic, version, op, ten bytes of payload.
pub const REQUEST_LEN: usize = 16;
/// An answer, whole: version, status, the port for `Attached`.
const REPLY_LEN: usize = 4;
/// The supervisor answers at once; a seat that waits longer has no supervisor.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What a seat may ask. `Create`'s `index` is the seat's pad slot, part of the pad's marker;
/// `Attach` carries the connected usbip socket beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Create { kind: PadKind, index: u8 },
    Attach { devid: u32, speed: u32 },
    Detach { port: u16 },
}

impl Request {
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let mut out = [0u8; REQUEST_LEN];
        out[..4].copy_from_slice(&MAGIC);
        out[4] = VERSION;
        match self {
            Request::Create { kind, index } => {
                out[5] = OP_CREATE;
                out[6] = kind as u8;
                out[7] = index;
            }
            Request::Attach { devid, speed } => {
                out[5] = OP_ATTACH;
                out[6..10].copy_from_slice(&devid.to_le_bytes());
                out[10..14].copy_from_slice(&speed.to_le_bytes());
            }
            Request::Detach { port } => {
                out[5] = OP_DETACH;
                out[6..8].copy_from_slice(&port.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Option<Request> {
        let b = *bytes.first_chunk::<REQUEST_LEN>()?;
        if bytes.len() != REQUEST_LEN || b[..4] != MAGIC || b[4] != VERSION {
            return None;
        }
        Some(match b[5] {
            OP_CREATE => Request::Create {
                kind: PadKind::from_wire(b[6])?,
                index: b[7],
            },
            OP_ATTACH => Request::Attach {
                devid: u32::from_le_bytes([b[6], b[7], b[8], b[9]]),
                speed: u32::from_le_bytes([b[10], b[11], b[12], b[13]]),
            },
            OP_DETACH => Request::Detach {
                port: u16::from_le_bytes([b[6], b[7]]),
            },
            _ => return None,
        })
    }
}

/// How the supervisor answered. The wire value is stable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// The relay's seat end rides along.
    Created = 0,
    /// Not a running seat's host.
    Refused = 1,
    /// This seat holds every pad it may.
    Capacity = 2,
    UnknownKind = 3,
    /// The device could not be made; the supervisor's log says why.
    Failed = 4,
    /// The port rides along.
    Attached = 5,
    Detached = 6,
}

impl Status {
    fn from_wire(value: u8) -> Option<Status> {
        Some(match value {
            0 => Status::Created,
            1 => Status::Refused,
            2 => Status::Capacity,
            3 => Status::UnknownKind,
            4 => Status::Failed,
            5 => Status::Attached,
            6 => Status::Detached,
            _ => return None,
        })
    }
}

/// One message on the socket, with at most one fd beside it.
fn send_frame(sock: &impl AsFd, bytes: &[u8], pass: Option<BorrowedFd<'_>>) -> Result<()> {
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds: Vec<BorrowedFd<'_>> = pass.into_iter().collect();
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        bail!("no room for the fd beside the message");
    }
    sendmsg(
        sock,
        &[IoSlice::new(bytes)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    Ok(())
}

/// One message into `buf`: how much arrived, and the fd beside it if any.
fn recv_frame(sock: &impl AsFd, buf: &mut [u8]) -> Result<(usize, Option<OwnedFd>)> {
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let got = recvmsg(
        sock,
        &mut [IoSliceMut::new(buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let fd = control.drain().find_map(|message| match message {
        RecvAncillaryMessage::ScmRights(fds) => fds.into_iter().next(),
        _ => None,
    });
    Ok((got.bytes, fd))
}

// ---- the seat's side ----

/// Ask the supervisor for `kind` at `index`: the seat's end of its relay.
pub fn request(kind: PadKind, index: u8) -> Result<OwnedFd> {
    let (status, _, relay) = ask(Request::Create { kind, index }, None)?;
    match status {
        Status::Created => relay.ok_or_else(|| anyhow!("the pad broker answered without a relay")),
        Status::UnknownKind => bail!("the seat pad broker does not know the {} pad", kind.label()),
        Status::Failed => bail!(
            "the seat pad broker could not make the {} pad (its log says why)",
            kind.label()
        ),
        other => bail!("{}", other.refusal()),
    }
}

/// Hand the supervisor `sock`, a usbip connection past its import handshake, to attach as
/// `devid` at `speed`. The vhci port it took.
pub fn attach(sock: BorrowedFd<'_>, devid: u32, speed: u32) -> Result<u16> {
    let (status, port, _) = ask(Request::Attach { devid, speed }, Some(sock))?;
    match status {
        Status::Attached => Ok(port),
        Status::Failed => {
            bail!("the seat pad broker could not attach the device (its log says why)")
        }
        other => bail!("{}", other.refusal()),
    }
}

/// Detach `port`, one this seat attached.
pub fn detach(port: u16) -> Result<()> {
    let (status, _, _) = ask(Request::Detach { port }, None)?;
    match status {
        Status::Detached => Ok(()),
        other => bail!("{}", other.refusal()),
    }
}

impl Status {
    fn refusal(self) -> String {
        match self {
            Status::Refused => "the seat pad broker refused this host: not a running seat".into(),
            Status::Capacity => "this seat holds every pad it may".into(),
            other => format!("the seat pad broker answered {other:?}"),
        }
    }
}

/// One request, one answer: `(status, port, relay)`.
fn ask(request: Request, pass: Option<BorrowedFd<'_>>) -> Result<(Status, u16, Option<OwnedFd>)> {
    let stream = UnixStream::connect(SOCKET).with_context(|| {
        format!("connect to the seat pad broker at {SOCKET} (is punktfunk-seats running?)")
    })?;
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    send_frame(&stream, &request.encode(), pass).context("send the pad request")?;
    recv_reply(&stream).context("read the pad broker's answer")
}

fn recv_reply(stream: &UnixStream) -> Result<(Status, u16, Option<OwnedFd>)> {
    let mut buf = [0u8; REPLY_LEN];
    let (n, relay) = recv_frame(stream, &mut buf)?;
    if n != REPLY_LEN || buf[0] != VERSION {
        bail!("malformed answer ({n} bytes, version {})", buf[0]);
    }
    let status = Status::from_wire(buf[1]).ok_or_else(|| anyhow!("unknown status {}", buf[1]))?;
    Ok((status, u16::from_le_bytes([buf[2], buf[3]]), relay))
}

// ---- the supervisor's side ----

/// A seat's request, read whole off `stream`, with the fd it sent beside it. Anything else is
/// not a request.
pub fn read_request(stream: &UnixStream) -> Result<(Request, Option<OwnedFd>)> {
    let mut buf = [0u8; REQUEST_LEN];
    let (n, fd) = recv_frame(stream, &mut buf).context("read the pad request")?;
    if n != REQUEST_LEN {
        bail!("not a pad request: {n} bytes");
    }
    let request = Request::decode(&buf).ok_or_else(|| anyhow!("not a pad request: {buf:02x?}"))?;
    Ok((request, fd))
}

/// Answer `status` on `stream`: the seat's end of its relay for `Created`, the port for
/// `Attached`.
pub fn reply(
    stream: &UnixStream,
    status: Status,
    port: u16,
    relay: Option<BorrowedFd<'_>>,
) -> Result<()> {
    let [lo, hi] = port.to_le_bytes();
    send_frame(stream, &[VERSION, status as u8, lo, hi], relay)
        .context("send the pad broker's answer")
}

/// A pad the supervisor built for a seat, ready to [`relay`].
pub struct BrokeredPad {
    dev: Brokered,
}

enum Brokered {
    Uinput(UinputDevice),
    Uhid(UhidDevice),
}

/// Build `kind` as the supervisor, stamped as `account`'s pad `index`. The udev fence reads
/// the stamp (`packaging/linux/65-punktfunk-seats.rules`).
pub fn build(kind: PadKind, index: u8, account: &str) -> Result<BrokeredPad> {
    let dev = if let Some(identity) = kind.uinput() {
        Brokered::Uinput(build_pad(
            identity,
            Some(&seat_phys(account, &index.to_string())),
        )?)
    } else {
        let mut id = kind
            .uhid(index)
            .ok_or_else(|| anyhow!("no table for the {} pad", kind.label()))?;
        // The backend's own `punktfunk/<tag>/<index>`, under the seat's name.
        let own = id.phys.trim_start_matches("punktfunk/").to_owned();
        id.phys = seat_phys(account, &own);
        Brokered::Uhid(UhidDevice::open(&id.as_create2())?)
    };
    Ok(BrokeredPad { dev })
}

/// `(supervisor end, seat end)` of a fresh relay.
pub fn relay_pair() -> Result<(UnixDatagram, OwnedFd)> {
    let (ours, theirs) = socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .context("socketpair for the pad relay")?;
    Ok((UnixDatagram::from(ours), theirs))
}

/// Move the seat's events into the pad and the pad's back, until the seat hangs up. Blocks:
/// one thread per relayed pad. A seat that stops reading loses what it missed, never the pad.
pub fn relay(pad: BrokeredPad, seat: UnixDatagram) {
    match pad.dev {
        Brokered::Uinput(dev) => relay_uinput(dev, seat),
        Brokered::Uhid(dev) => dev.relay(seat),
    }
}

/// Frames from `seat` to the kernel whole; the FF plane back as [`FfNotice`]s.
fn relay_uinput(mut dev: UinputDevice, seat: UnixDatagram) {
    let _ = seat.set_nonblocking(true);
    let mut frame = vec![0u8; INPUT_EVENT_LEN * 64];
    loop {
        let mut fds = [
            PollFd::new(&seat, PollFlags::IN),
            PollFd::new(&dev, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => return,
        }
        let (from_seat, from_kernel) = (fds[0].revents(), fds[1].revents());
        if from_seat.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
            match seat.recv(&mut frame) {
                Ok(0) => return,
                Ok(n) => dev.write_batch(&frame[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(_) => return,
            }
        }
        if from_kernel.intersects(PollFlags::IN) {
            while let Some(notice) = dev.next_ff() {
                match seat.send(&notice.encode()) {
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }
            }
        }
        if from_kernel.intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_and_junk_is_refused() {
        let r = Request::Create {
            kind: PadKind::XboxElite2,
            index: 3,
        };
        assert_eq!(Request::decode(&r.encode()), Some(r));
        for r in [
            Request::Attach {
                devid: 0x0001_0002,
                speed: 3,
            },
            Request::Detach { port: 9 },
        ] {
            assert_eq!(Request::decode(&r.encode()), Some(r));
        }
        let mut wrong_kind = r.encode();
        wrong_kind[6] = 200;
        assert_eq!(Request::decode(&wrong_kind), None);
        let mut wrong_version = r.encode();
        wrong_version[4] = 9;
        assert_eq!(Request::decode(&wrong_version), None);
        let mut wrong_op = r.encode();
        wrong_op[5] = 7;
        assert_eq!(Request::decode(&wrong_op), None);
        assert_eq!(Request::decode(&r.encode()[..8]), None, "short");
        let mut magic = r.encode();
        magic[..4].copy_from_slice(b"nope");
        assert_eq!(Request::decode(&magic), None);
    }

    /// An `Attach` crosses with its socket; the answer carries the port.
    #[test]
    fn an_attach_crosses_with_its_socket() {
        let (supervisor, seat) = UnixStream::pair().unwrap();
        let (mine, theirs) = UnixStream::pair().unwrap();
        send_frame(
            &seat,
            &Request::Attach { devid: 5, speed: 2 }.encode(),
            Some(theirs.as_fd()),
        )
        .unwrap();
        let (request, sock) = read_request(&supervisor).unwrap();
        assert_eq!(request, Request::Attach { devid: 5, speed: 2 });
        let sock = UnixStream::from(sock.expect("the socket"));
        use std::io::{Read, Write};
        (&mine).write_all(b"hi").unwrap();
        let mut got = [0u8; 2];
        (&sock).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hi");

        reply(&supervisor, Status::Attached, 11, None).unwrap();
        let (status, port, relay) = recv_reply(&seat).unwrap();
        assert_eq!((status, port), (Status::Attached, 11));
        assert!(relay.is_none());
    }

    /// The answer carries the relay's seat end, and what the supervisor sends on its end
    /// arrives there.
    #[test]
    fn an_answer_hands_the_seat_its_relay_end() {
        let (supervisor, seat) = UnixStream::pair().unwrap();
        let (ours, theirs) = relay_pair().unwrap();
        reply(&supervisor, Status::Created, 0, Some(theirs.as_fd())).unwrap();
        let (status, _, relay) = recv_reply(&seat).unwrap();
        assert_eq!(status, Status::Created);
        let relay = UnixDatagram::from(relay.expect("the relay fd"));
        ours.send(b"frame").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(relay.recv(&mut buf).unwrap(), 5);

        reply(&supervisor, Status::Capacity, 0, None).unwrap();
        let (status, _, relay) = recv_reply(&seat).unwrap();
        assert_eq!(status, Status::Capacity);
        assert!(relay.is_none());
    }
}
