//! The seat pad broker's wire and both ends of its relay.
//!
//! A seat host cannot open `/dev/uinput`: that node is the console user's. It asks the root
//! seat supervisor over [`SOCKET`] for a pad by [`PadKind`]; the supervisor builds the device
//! from this crate's own tables ([`build`]), keeps the kernel fd, and hands the seat one end of
//! a `SOCK_SEQPACKET` pair. [`relay`] moves the seat's `input_event` frames into the device and
//! the device's FF plane back as [`FfNotice`](crate::uinput_abi::FfNotice)s. No kernel fd ever
//! crosses: a passed uinput fd can destroy its pad and create any device.
//!
//! A USB/IP pad is served by the supervisor too ([`serve`]): its pad server runs the device
//! from this crate's tables, and the seat gets only a relay for input reports
//! ([`crate::usbip_relay`]). A seat never answers a control transfer, so it never picks a
//! descriptor. Design: `design/seat-pad-broker.md`.

use crate::gamepad::{build_pad, PadIdentity};
use crate::uhid_abi::{Identity, UhidDevice};
use crate::uinput_abi::{UinputDevice, INPUT_EVENT_LEN};
use crate::usbip_relay::{Hello, UsbKind, MAX_IDENTITY};
use anyhow::{anyhow, bail, Context, Result};
use rustix::event::{poll, PollFd, PollFlags};
use rustix::net::{
    recvmsg, sendmsg, sendmsg_addr, socket_with, socketpair, AddressFamily, RecvAncillaryBuffer,
    RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage, SendFlags,
    SocketAddrUnix, SocketFlags, SocketType,
};
use std::collections::HashMap;
use std::io::{ErrorKind, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

pub use crate::uinput_abi::ReplayUinput;
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

    /// Whether a seat's input report goes to this kind's device. The SC2's captured descriptor
    /// also declares a mouse (`0x40`) and a keyboard (`0x41`); only its controller ids pass.
    pub fn takes(self, report: &[u8]) -> bool {
        match self {
            PadKind::SteamController2 => report
                .first()
                .is_some_and(|&id| pf_driver_proto::triton::input_len(id).is_some()),
            _ => true,
        }
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
/// Ops 2 and 3 are retired; a decoder refuses them.
const OP_SERVE: u8 = 4;
/// A request, whole: magic, version, op, ten bytes of payload.
pub const REQUEST_LEN: usize = 16;
/// An answer, whole: version, status, the port for `Served`.
const REPLY_LEN: usize = 4;
/// The supervisor answers within this; a seat that waits longer has no supervisor. A served
/// pad's answer waits for its pad server to start.
const TIMEOUT: Duration = Duration::from_secs(10);

/// What a seat may ask. `Create`'s `index` is the seat's pad slot, part of the pad's marker.
/// `Serve`'s identity blob follows the request on the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Create {
        kind: PadKind,
        index: u8,
    },
    Serve {
        kind: UsbKind,
        index: u8,
        slot: u8,
        identity_len: u16,
    },
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
            Request::Serve {
                kind,
                index,
                slot,
                identity_len,
            } => {
                out[5] = OP_SERVE;
                out[6] = kind as u8;
                out[7] = index;
                out[8] = slot;
                out[9..11].copy_from_slice(&identity_len.to_le_bytes());
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
            OP_SERVE => {
                let identity_len = u16::from_le_bytes([b[9], b[10]]);
                if usize::from(identity_len) > MAX_IDENTITY || b[8] > 3 {
                    return None;
                }
                Request::Serve {
                    kind: UsbKind::from_wire(b[6])?,
                    index: b[7],
                    slot: b[8],
                    identity_len,
                }
            }
            _ => return None,
        })
    }
}

/// How the supervisor answered. The wire value is stable; 1, 3, 5 and 6 are retired. A peer
/// that is no running seat's host is closed unanswered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// The relay's seat end rides along.
    Created = 0,
    /// This seat holds every pad it may.
    Capacity = 2,
    /// The device could not be made; the supervisor's log says why.
    Failed = 4,
    /// The port and the relay's seat end ride along.
    Served = 7,
}

impl Status {
    fn from_wire(value: u8) -> Option<Status> {
        Some(match value {
            0 => Status::Created,
            2 => Status::Capacity,
            4 => Status::Failed,
            7 => Status::Served,
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

/// Whether this host makes a pad on `node` (`/dev/uinput`, `/dev/uhid`, vhci's `attach`)
/// through the broker: a seat host that can't write it, or finds the supervisor's uinput proxy
/// there, which is the seat's Steam's. The owner's own host at the desk holds seat0's ACL and
/// opens the node itself, as the box's host does.
pub fn brokered(node: &Path) -> bool {
    asks_broker(
        pf_paths::seat::role().is_seat_host(),
        writable(node) && !proxied(node),
    )
}

fn asks_broker(seat_host: bool, writable: bool) -> bool {
    seat_host && !writable
}

/// The misc major, where the kernel's own `/dev/uinput` and `/dev/uhid` live.
const MISC_MAJOR: u32 = 10;

/// Whether `node` is a character device off the misc major: on a seat, the supervisor's uinput
/// proxy bound over `/dev/uinput`.
pub fn proxied(node: &Path) -> bool {
    rustix::fs::stat(node).is_ok_and(|st| {
        rustix::fs::FileType::from_raw_mode(st.st_mode) == rustix::fs::FileType::CharacterDevice
            && rustix::fs::major(st.st_rdev) != MISC_MAJOR
    })
}

/// Whether this process may write `node`, ACL included. Nothing is opened.
pub fn writable(node: &Path) -> bool {
    rustix::fs::access(node, rustix::fs::Access::WRITE_OK).is_ok()
}

/// Ask the supervisor for `kind` at `index`: the seat's end of its relay.
pub fn request(kind: PadKind, index: u8) -> Result<OwnedFd> {
    let (status, _, relay) = ask(&Request::Create { kind, index }.encode())?;
    match status {
        Status::Created => relay.ok_or_else(|| anyhow!("the pad broker answered without a relay")),
        Status::Failed => bail!(
            "the seat pad broker could not make the {} pad (its log says why)",
            kind.label()
        ),
        other => bail!("{}", other.refusal()),
    }
}

/// Ask the supervisor to serve `hello`'s USB device: the seat's end of its relay, and the
/// vhci port the device sits on.
pub fn serve(hello: &Hello) -> Result<(OwnedFd, u16)> {
    let identity_len = u16::try_from(hello.identity.len())
        .ok()
        .filter(|n| usize::from(*n) <= MAX_IDENTITY)
        .ok_or_else(|| anyhow!("identity of {} bytes", hello.identity.len()))?;
    let request = Request::Serve {
        kind: hello.kind,
        index: hello.index,
        slot: hello.slot.min(3),
        identity_len,
    };
    let (status, port, relay) = ask(&[&request.encode()[..], &hello.identity].concat())?;
    let label = hello.kind.label();
    match status {
        Status::Served => Ok((
            relay.ok_or_else(|| anyhow!("the pad broker answered without a relay"))?,
            port,
        )),
        Status::Failed => {
            bail!("the seat pad broker could not serve the {label} (its log says why)")
        }
        other => bail!("{}", other.refusal()),
    }
}

impl Status {
    fn refusal(self) -> String {
        match self {
            Status::Capacity => "this seat holds every pad it may".into(),
            other => format!("the seat pad broker answered {other:?}"),
        }
    }
}

/// One request, one answer: `(status, port, relay)`.
fn ask(request: &[u8]) -> Result<(Status, u16, Option<OwnedFd>)> {
    let stream = UnixStream::connect(SOCKET).with_context(|| {
        format!("connect to the seat pad broker at {SOCKET} (is punktfunk-seats running?)")
    })?;
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));
    send_frame(&stream, request, None).context("send the pad request")?;
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

/// A seat's request, read whole off `stream`; an fd sent beside it is closed. Anything else
/// is not a request.
pub fn read_request(stream: &UnixStream) -> Result<Request> {
    let mut buf = [0u8; REQUEST_LEN];
    let (n, _) = recv_frame(stream, &mut buf).context("read the pad request")?;
    if n != REQUEST_LEN {
        bail!("not a pad request: {n} bytes");
    }
    Request::decode(&buf).ok_or_else(|| anyhow!("not a pad request: {buf:02x?}"))
}

/// The identity blob a `Serve` request said follows it, read whole off `stream`.
pub fn read_identity(stream: &UnixStream, len: u16) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut blob = vec![0u8; usize::from(len)];
    (&*stream)
        .read_exact(&mut blob)
        .context("read the pad's identity")?;
    Ok(blob)
}

/// Answer `status` on `stream`: the seat's end of its relay for `Created` and `Served`, the
/// port for `Served`.
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
    kind: PadKind,
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
    Ok(BrokeredPad { kind, dev })
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

// ---- across a supervisor restart ----

/// systemd's first passed fd.
const LISTEN_FDS_START: RawFd = 3;

impl BrokeredPad {
    fn kernel_fd(&self) -> BorrowedFd<'_> {
        match &self.dev {
            Brokered::Uinput(dev) => dev.as_fd(),
            Brokered::Uhid(dev) => dev.as_fd(),
        }
    }
}

/// A relayed pad's name in systemd's fd store, `pad/<serial>/<kind>/<index>/<account>`: `/k`
/// on the kernel fd, `/r` on the supervisor's relay end. The serial is unique per process.
pub fn store_name(kind: PadKind, index: u8, account: &str) -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    format!("pad/{pid}-{serial}/{}/{index}/{account}", kind as u8)
}

/// What a stored name says: `(serial, kind, index, account)`.
pub fn parse_store_name(name: &str) -> Option<(&str, PadKind, u8, &str)> {
    let mut parts = name.split('/');
    if parts.next()? != "pad" {
        return None;
    }
    let serial = parts.next()?;
    let kind = PadKind::from_wire(parts.next()?.parse().ok()?)?;
    let index = parts.next()?.parse().ok()?;
    let account = parts.next()?;
    parts
        .next()
        .is_none()
        .then_some((serial, kind, index, account))
}

/// Parks `pad` and its relay end in systemd's fd store under `name`, so a supervisor restart
/// keeps both ([`kept`]). Not under systemd, nothing happens.
pub fn keep(name: &str, pad: &BrokeredPad, relay: &UnixDatagram) {
    notify(
        &format!("FDSTORE=1\nFDNAME={name}/k"),
        Some(pad.kernel_fd()),
    );
    notify(&format!("FDSTORE=1\nFDNAME={name}/r"), Some(relay.as_fd()));
}

/// Takes `name`'s fds out of the store: the pad is gone.
pub fn forget(name: &str) {
    notify(&format!("FDSTOREREMOVE=1\nFDNAME={name}/k"), None);
    notify(&format!("FDSTOREREMOVE=1\nFDNAME={name}/r"), None);
}

/// A served USB socket's name in the fd store, `served/<port>/<recorded>`: `recorded` is the
/// number `attach` took, which the port map and vhci's `status` keep after a restart.
fn usb_store_name(port: u16, recorded: RawFd) -> String {
    format!("served/{port}/{recorded}")
}

/// What a stored USB socket's name says: `(port, recorded)`.
fn parse_usb_store_name(name: &str) -> Option<(u16, RawFd)> {
    let (port, recorded) = name.strip_prefix("served/")?.split_once('/')?;
    Some((port.parse().ok()?, recorded.parse().ok()?))
}

/// Parks a served USB socket in systemd's fd store, so a restarted supervisor watches it
/// again ([`kept`]).
pub fn keep_usb(port: u16, recorded: RawFd, sock: BorrowedFd<'_>) {
    let name = usb_store_name(port, recorded);
    notify(&format!("FDSTORE=1\nFDNAME={name}"), Some(sock));
}

/// Takes a USB socket out of the store: its connection ended.
pub fn forget_usb(port: u16, recorded: RawFd) {
    let name = usb_store_name(port, recorded);
    notify(&format!("FDSTOREREMOVE=1\nFDNAME={name}"), None);
}

/// Parks `fd` in systemd's fd store under `name`, which is neither a pad's nor a USB socket's
/// and holds no `:`; [`kept`] hands it back in [`Kept::other`].
pub fn keep_fd(name: &str, fd: BorrowedFd<'_>) {
    notify(&format!("FDSTORE=1\nFDNAME={name}"), Some(fd));
}

/// Takes every fd stored under `name` out of the store.
pub fn forget_fd(name: &str) {
    notify(&format!("FDSTOREREMOVE=1\nFDNAME={name}"), None);
}

/// One message to systemd's notify socket, with `fd` beside it. A path socket only, which is
/// what systemd hands a system service.
fn notify(message: &str, fd: Option<BorrowedFd<'_>>) {
    let Some(path) =
        std::env::var_os("NOTIFY_SOCKET").filter(|p| !p.as_encoded_bytes().starts_with(b"@"))
    else {
        return;
    };
    let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds: Vec<BorrowedFd<'_>> = fd.into_iter().collect();
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return;
    }
    let sent = SocketAddrUnix::new(Path::new(&path)).and_then(|addr| {
        let sock = socket_with(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            None,
        )?;
        sendmsg_addr(
            &sock,
            &addr,
            &[IoSlice::new(message.as_bytes())],
            &mut control,
            SendFlags::NOSIGNAL,
        )
    });
    if let Err(error) = sent {
        tracing::debug!(%error, "systemd fd store not told");
    }
}

/// A pad the previous supervisor relayed, from systemd's fd store.
pub struct KeptPad {
    pub name: String,
    pub kind: PadKind,
    pub index: u8,
    pub account: String,
    pub pad: BrokeredPad,
    pub relay: UnixDatagram,
}

/// What systemd kept across a restart ([`kept`]).
#[derive(Default)]
pub struct Kept {
    pub pads: Vec<KeptPad>,
    /// Each served USB socket as `(port, recorded, socket)`.
    pub usb: Vec<(u16, RawFd, OwnedFd)>,
    /// Every other fd, by the name [`keep_fd`] gave it.
    pub other: Vec<(String, OwnedFd)>,
}

/// What systemd kept across a restart (`LISTEN_FDS`, `LISTEN_FDNAMES`). A kernel fd whose
/// relay end systemd dropped (the seat hung up) is forgotten and closed, which destroys the
/// pad. Every fd is close-on-exec from here. Once per process; a second call finds nothing.
pub fn kept() -> Kept {
    static TAKEN: AtomicBool = AtomicBool::new(false);
    let env = |name: &str| std::env::var(name).unwrap_or_default();
    let ours = env("LISTEN_PID").parse::<u32>().ok() == Some(std::process::id());
    let count = env("LISTEN_FDS").parse::<RawFd>().unwrap_or(0);
    if !ours || count <= 0 || TAKEN.swap(true, Ordering::SeqCst) {
        return Kept::default();
    }
    let names = env("LISTEN_FDNAMES");
    let mut names = names.split(':');
    let (mut kernels, mut relays) = (HashMap::new(), HashMap::new());
    let (mut usb, mut other) = (Vec::new(), Vec::new());
    for raw in LISTEN_FDS_START..LISTEN_FDS_START + count {
        // SAFETY: systemd passes fds 3 through 3 + LISTEN_FDS - 1 to the process LISTEN_PID
        // names, this one. Nothing else here owns them, and `TAKEN` adopts each once.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        let _ = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC);
        let name = names.next().unwrap_or("");
        if let Some((port, recorded)) = parse_usb_store_name(name) {
            usb.push((port, recorded, fd));
            continue;
        }
        match name
            .strip_prefix("pad/")
            .and_then(|_| name.rsplit_once('/'))
        {
            Some((stem, "k")) => kernels.insert(stem.to_owned(), fd),
            Some((stem, "r")) => relays.insert(stem.to_owned(), fd),
            _ => {
                other.push((name.to_owned(), fd));
                None
            }
        };
    }
    let mut out = Vec::new();
    for (name, kernel) in kernels {
        let (Some(relay), Some((_, kind, index, account))) =
            (relays.remove(&name), parse_store_name(&name))
        else {
            forget(&name);
            continue;
        };
        let dev = if kind.uinput().is_some() {
            Brokered::Uinput(UinputDevice::adopt(kernel))
        } else {
            Brokered::Uhid(UhidDevice::adopt(kernel))
        };
        out.push(KeptPad {
            account: account.to_owned(),
            name,
            kind,
            index,
            pad: BrokeredPad { kind, dev },
            relay: UnixDatagram::from(relay),
        });
    }
    relays.into_keys().for_each(|name| forget(&name));
    Kept {
        pads: out,
        usb,
        other,
    }
}

/// Move the seat's events into the pad and the pad's back, until the seat hangs up. Blocks:
/// one thread per relayed pad. A seat that stops reading loses what it missed, never the pad.
pub fn relay(pad: BrokeredPad, seat: UnixDatagram) {
    let kind = pad.kind;
    match pad.dev {
        Brokered::Uinput(dev) => relay_uinput(dev, seat),
        Brokered::Uhid(dev) => dev.relay(seat, |report| kind.takes(report)),
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

    /// Only a seat host that can't write the node asks the broker.
    #[test]
    fn a_host_that_can_write_the_node_opens_it_itself() {
        assert!(asks_broker(true, false));
        assert!(!asks_broker(true, true), "the owner's host at the desk");
        assert!(!asks_broker(false, false), "the box's host");
        let node = std::env::temp_dir().join(format!("pf-pad-node-{}", std::process::id()));
        assert!(!writable(&node), "no node");
        std::fs::write(&node, b"").unwrap();
        assert!(writable(&node));
        let _ = std::fs::remove_file(&node);
    }

    /// The kernel's `/dev/uinput` is misc `10:223`; the supervisor's proxy has a major of its
    /// own, as `/dev/null` (`1:3`) does here.
    #[test]
    fn a_character_device_off_the_misc_major_reads_as_the_proxy() {
        assert!(proxied(Path::new("/dev/null")));
        assert!(!proxied(Path::new("/proc/self/status")), "not a device");
        assert!(!proxied(Path::new("/no/such/node")));
        let uinput = Path::new("/dev/uinput");
        assert!(!uinput.exists() || !proxied(uinput));
    }

    #[test]
    fn a_request_round_trips_and_junk_is_refused() {
        let r = Request::Create {
            kind: PadKind::XboxElite2,
            index: 3,
        };
        assert_eq!(Request::decode(&r.encode()), Some(r));
        let puck = Request::Serve {
            kind: UsbKind::SteamController2Puck,
            index: 4,
            slot: 3,
            identity_len: MAX_IDENTITY as u16,
        };
        assert_eq!(Request::decode(&puck.encode()), Some(puck));
        for retired in [2, 3] {
            let mut op = r.encode();
            op[5] = retired;
            assert_eq!(Request::decode(&op), None, "op {retired}");
        }
        let serve = Request::Serve {
            kind: UsbKind::DualSense,
            index: 0,
            slot: 0,
            identity_len: 0,
        }
        .encode();
        let mut unknown = serve;
        unknown[6] = 99;
        assert_eq!(Request::decode(&unknown), None, "a kind no table serves");
        let mut huge = serve;
        huge[9..11].copy_from_slice(&(MAX_IDENTITY as u16 + 1).to_le_bytes());
        assert_eq!(Request::decode(&huge), None, "an identity past the cap");
        let mut slot = serve;
        slot[8] = 4;
        assert_eq!(Request::decode(&slot), None, "a Puck has four slots");
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

    /// A stored pad's name carries what a restarted supervisor needs to relay it again.
    #[test]
    fn a_store_name_round_trips() {
        let a = store_name(PadKind::DualSense, 2, "pf-seat-1");
        let b = store_name(PadKind::DualSense, 2, "pf-seat-1");
        assert_ne!(a, b, "unique per pad");
        let (serial, kind, index, account) = parse_store_name(&a).unwrap();
        assert!(!serial.contains(['/', ':']));
        assert_eq!((kind, index, account), (PadKind::DualSense, 2, "pf-seat-1"));
        assert!(!a.contains(':'), "systemd's LISTEN_FDNAMES separator");
        assert_eq!(parse_store_name("pad/1-0/200/0/x"), None, "unknown kind");
        assert_eq!(
            parse_store_name("pad/1-0/10/0/x/k"),
            None,
            "a suffix is not a name"
        );
    }

    /// A kept USB socket's name carries the port and the number its map row records.
    #[test]
    fn a_usb_store_name_round_trips() {
        let name = usb_store_name(9, 17);
        assert!(!name.contains(':'), "systemd's LISTEN_FDNAMES separator");
        assert_eq!(parse_usb_store_name(&name), Some((9, 17)));
        assert_eq!(parse_usb_store_name("usb/9/17"), None);
        assert_eq!(parse_store_name(&name), None, "not a pad");
        assert_eq!(parse_usb_store_name("pad/1-0/10/0/x/k"), None);
        assert_eq!(parse_usb_store_name("usb/9/x"), None);
        assert_eq!(parse_usb_store_name("served/9"), None);
    }

    /// No uhid kind takes a report its own table makes a keyboard, consumer control or pointer
    /// of, and no table without report ids declares one.
    #[test]
    fn no_uhid_kind_takes_a_report_its_table_makes_a_keyboard_or_pointer() {
        for kind in (0..=255).filter_map(PadKind::from_wire) {
            let Some(id) = kind.uhid(0) else { continue };
            for risky in crate::usbip_relay::risky_input_reports(id.rdesc) {
                let risky =
                    risky.unwrap_or_else(|| panic!("{} is risky without ids", kind.label()));
                assert!(
                    !kind.takes(&[risky, 0, 0]),
                    "{} takes {risky:#04x}",
                    kind.label()
                );
            }
        }
    }

    /// Only the SC2's controller reports reach its device; its keyboard and mouse never do.
    #[test]
    fn a_seat_sc2_takes_no_keyboard_or_mouse_report() {
        let sc2 = PadKind::SteamController2;
        for id in [0x42, 0x45, 0x43, 0x44, 0x79, 0x7B] {
            assert!(sc2.takes(&[id, 0, 0]), "{id:#04x}");
        }
        for id in [0x40, 0x41, 0x01, 0x80] {
            assert!(!sc2.takes(&[id, 0, 0]), "{id:#04x}");
        }
        assert!(!sc2.takes(&[]), "an empty report");
        assert!(PadKind::DualSense.takes(&[0x01]));
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
