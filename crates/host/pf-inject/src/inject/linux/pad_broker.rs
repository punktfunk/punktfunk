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

/// A pad a seat may ask for. The number is the wire value: append, never renumber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PadKind {
    Xbox360 = 1,
    XboxOne = 2,
    XboxElite2 = 3,
}

impl PadKind {
    pub fn from_wire(value: u8) -> Option<PadKind> {
        Some(match value {
            1 => PadKind::Xbox360,
            2 => PadKind::XboxOne,
            3 => PadKind::XboxElite2,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        PadIdentity::of(self).log()
    }
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
    dev: UinputDevice,
}

/// Build `kind` as the supervisor, stamped as `account`'s pad `index`. The udev fence reads
/// the stamp (`packaging/linux/65-punktfunk-seats.rules`).
pub fn build(kind: PadKind, index: u8, account: &str) -> Result<BrokeredPad> {
    let phys = format!("punktfunk-seat:{account}/{index}");
    let dev = build_pad(PadIdentity::of(kind), Some(&phys))?;
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

/// Move frames from `seat` into the pad and its FF plane back, until the seat hangs up. Blocks:
/// one thread per relayed pad. A seat that stops reading its notices loses them, never the pad.
pub fn relay(pad: BrokeredPad, seat: UnixDatagram) {
    let mut dev = pad.dev;
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
