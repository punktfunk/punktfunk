//! The seat pad broker's wire and both ends of its relay.
//!
//! A seat host cannot open `/dev/uinput`: that node is the console user's. It asks the root
//! seat supervisor over [`SOCKET`] for a pad by [`PadKind`]; the supervisor builds the device
//! from this crate's own tables ([`build`]), keeps the kernel fd, and hands the seat one end of
//! a `SOCK_SEQPACKET` pair. [`relay`] moves the seat's `input_event` frames into the device and
//! the device's FF plane back as [`FfNotice`](crate::uinput_abi::FfNotice)s. No kernel fd ever
//! crosses: a passed uinput fd can destroy its pad and create any device.
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
use std::io::{ErrorKind, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
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
const VERSION: u8 = 1;
const OP_CREATE: u8 = 1;
/// A request, whole.
pub const REQUEST_LEN: usize = 8;
const REPLY_LEN: usize = 2;
/// The supervisor answers at once; a seat that waits longer has no supervisor.
const TIMEOUT: Duration = Duration::from_secs(5);

/// `Create`: the one request. `index` is the seat's pad slot, part of the pad's marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub kind: PadKind,
    pub index: u8,
}

impl Request {
    pub fn encode(self) -> [u8; REQUEST_LEN] {
        let [a, b, c, d] = MAGIC;
        [a, b, c, d, VERSION, OP_CREATE, self.kind as u8, self.index]
    }

    pub fn decode(bytes: &[u8]) -> Option<Request> {
        let [a, b, c, d, version, op, kind, index] = *bytes.first_chunk::<REQUEST_LEN>()?;
        if bytes.len() != REQUEST_LEN || [a, b, c, d] != MAGIC || version != VERSION {
            return None;
        }
        if op != OP_CREATE {
            return None;
        }
        Some(Request {
            kind: PadKind::from_wire(kind)?,
            index,
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
}

impl Status {
    fn from_wire(value: u8) -> Option<Status> {
        Some(match value {
            0 => Status::Created,
            1 => Status::Refused,
            2 => Status::Capacity,
            3 => Status::UnknownKind,
            4 => Status::Failed,
            _ => return None,
        })
    }
}

// ---- the seat's side ----

/// Ask the supervisor for `kind` at `index`: the seat's end of its relay.
pub fn request(kind: PadKind, index: u8) -> Result<OwnedFd> {
    let mut stream = UnixStream::connect(SOCKET).with_context(|| {
        format!("connect to the seat pad broker at {SOCKET} (is punktfunk-seats running?)")
    })?;
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    stream
        .write_all(&Request { kind, index }.encode())
        .context("send the pad request")?;
    let (status, relay) = recv_reply(&stream).context("read the pad broker's answer")?;
    match status {
        Status::Created => relay.ok_or_else(|| anyhow!("the pad broker answered without a relay")),
        Status::Refused => bail!("the seat pad broker refused this host: not a running seat"),
        Status::Capacity => bail!("this seat holds every pad it may"),
        Status::UnknownKind => bail!("the seat pad broker does not know the {} pad", kind.label()),
        Status::Failed => bail!(
            "the seat pad broker could not make the {} pad (its log says why)",
            kind.label()
        ),
    }
}

fn recv_reply(stream: &UnixStream) -> Result<(Status, Option<OwnedFd>)> {
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let mut buf = [0u8; REPLY_LEN];
    let got = recvmsg(
        stream,
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    if got.bytes != REPLY_LEN || buf[0] != VERSION {
        bail!("malformed answer ({} bytes, version {})", got.bytes, buf[0]);
    }
    let relay = control.drain().find_map(|message| match message {
        RecvAncillaryMessage::ScmRights(fds) => fds.into_iter().next(),
        _ => None,
    });
    let status = Status::from_wire(buf[1]).ok_or_else(|| anyhow!("unknown status {}", buf[1]))?;
    Ok((status, relay))
}

// ---- the supervisor's side ----

/// A seat's request, read whole off `stream`. Anything else is not a request.
pub fn read_request(stream: &mut UnixStream) -> Result<Request> {
    let mut buf = [0u8; REQUEST_LEN];
    stream
        .read_exact(&mut buf)
        .context("read the pad request")?;
    Request::decode(&buf).ok_or_else(|| anyhow!("not a pad request: {buf:02x?}"))
}

/// Answer `status` on `stream`, with the seat's end of its relay for `Created`.
pub fn reply(stream: &UnixStream, status: Status, relay: Option<BorrowedFd<'_>>) -> Result<()> {
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds: Vec<BorrowedFd<'_>> = relay.into_iter().collect();
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        bail!("no room for the relay fd in the answer");
    }
    let message = [VERSION, status as u8];
    sendmsg(
        stream,
        &[IoSlice::new(&message)],
        &mut control,
        SendFlags::NOSIGNAL,
    )
    .context("send the pad broker's answer")?;
    Ok(())
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
    use std::os::fd::AsFd;

    #[test]
    fn a_request_round_trips_and_junk_is_refused() {
        let r = Request {
            kind: PadKind::XboxElite2,
            index: 3,
        };
        assert_eq!(Request::decode(&r.encode()), Some(r));
        let mut wrong_kind = r.encode();
        wrong_kind[6] = 200;
        assert_eq!(Request::decode(&wrong_kind), None);
        let mut wrong_version = r.encode();
        wrong_version[4] = 9;
        assert_eq!(Request::decode(&wrong_version), None);
        assert_eq!(Request::decode(b"PFPD\x01\x01\x01"), None, "short");
        assert_eq!(Request::decode(b"nope\x01\x01\x01\x00"), None);
    }

    /// Every kind round-trips the wire and has one table, uinput or uhid, never both.
    #[test]
    fn every_kind_has_exactly_one_table() {
        for value in 0..=255u8 {
            let Some(kind) = PadKind::from_wire(value) else {
                continue;
            };
            assert_eq!(kind as u8, value);
            assert_ne!(
                kind.uinput().is_some(),
                kind.uhid(0).is_some(),
                "{}",
                kind.label()
            );
        }
        let ds = PadKind::DualSense.uhid(2).unwrap();
        assert_eq!(ds.phys, "punktfunk/dualsense/2");
        assert_eq!(
            seat_phys("pf-seat-1", ds.phys.trim_start_matches("punktfunk/")),
            "punktfunk-seat:pf-seat-1/dualsense/2"
        );
    }

    /// The answer carries the relay's seat end, and what the supervisor sends on its end
    /// arrives there.
    #[test]
    fn an_answer_hands_the_seat_its_relay_end() {
        let (supervisor, seat) = UnixStream::pair().unwrap();
        let (ours, theirs) = relay_pair().unwrap();
        reply(&supervisor, Status::Created, Some(theirs.as_fd())).unwrap();
        let (status, relay) = recv_reply(&seat).unwrap();
        assert_eq!(status, Status::Created);
        let relay = UnixDatagram::from(relay.expect("the relay fd"));
        ours.send(b"frame").unwrap();
        let mut buf = [0u8; 8];
        assert_eq!(relay.recv(&mut buf).unwrap(), 5);

        reply(&supervisor, Status::Capacity, None).unwrap();
        let (status, relay) = recv_reply(&seat).unwrap();
        assert_eq!(status, Status::Capacity);
        assert!(relay.is_none());
    }
}
