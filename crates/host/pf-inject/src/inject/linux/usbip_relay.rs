//! The relay between a USB/IP pad and its server, and the server's devices.
//!
//! A USB/IP pad is two halves. The server is the device: every descriptor, every control
//! transfer and every poll comes from this crate's tables ([`served`]). The pad half keeps the
//! state encoder and the feedback parsers. They talk over one `SOCK_SEQPACKET` relay: the pad
//! sends input reports by interface ([`Incoming`]), the server sends back what the host wrote
//! ([`Output`]), and each half ends when the other hangs up. The box's host runs the server on a
//! thread of its own; a seat's runs in the supervisor's pad server, so nothing a seat sends
//! reaches a descriptor. A kind takes an input report only on the interfaces and report ids its
//! table lists, which keeps a table's keyboard and mouse parts silent.

use crate::triton_proto::Sc2Identity;
use anyhow::{anyhow, bail, Context, Result};
use rustix::net::SendFlags;
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use usbip_sim::UsbDevice;

const HELLO: u8 = 1;
const INPUT: u8 = 2;
const SEAT: u8 = 3;
const VACATE: u8 = 4;
const OUTPUT: u8 = 5;
const AUDIO: u8 = 6;

/// The largest message: one isochronous URB of 48 DualSense packets, with room.
pub const MAX_MESSAGE: usize = 32 * 1024;
/// The largest identity blob a pad may send.
pub const MAX_IDENTITY: usize = 4096;
/// The longest identity serial: a real pad's is 13 characters.
const MAX_SERIAL: usize = 16;

/// A USB/IP pad this crate serves. The number is the wire value: append, never renumber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UsbKind {
    SteamDeck = 1,
    SteamController = 2,
    SteamController2 = 3,
    SteamController2Puck = 4,
    DualSense = 5,
    Switch2Pro = 6,
    Switch2GameCube = 7,
}

impl UsbKind {
    pub const ALL: [UsbKind; 7] = [
        UsbKind::SteamDeck,
        UsbKind::SteamController,
        UsbKind::SteamController2,
        UsbKind::SteamController2Puck,
        UsbKind::DualSense,
        UsbKind::Switch2Pro,
        UsbKind::Switch2GameCube,
    ];

    pub fn from_wire(value: u8) -> Option<UsbKind> {
        UsbKind::ALL.into_iter().find(|kind| *kind as u8 == value)
    }

    pub fn label(self) -> &'static str {
        match self {
            UsbKind::SteamDeck => "Steam Deck",
            UsbKind::SteamController => "Steam Controller",
            UsbKind::SteamController2 => "Steam Controller 2",
            UsbKind::SteamController2Puck => "Steam Controller 2 Puck",
            UsbKind::DualSense => "DualSense",
            UsbKind::Switch2Pro => "Switch 2 Pro Controller",
            UsbKind::Switch2GameCube => "Switch 2 GameCube controller",
        }
    }
}

/// What a server is built from: the relay's first message on a seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub kind: UsbKind,
    /// The pad's slot on its host; varies the canned serial.
    pub index: u8,
    /// A Puck pad's slot, 0–3.
    pub slot: u8,
    /// The seat a DualSense's MAC carries ([`crate::pad_broker::ps_seat`]), 0 for the box's own.
    /// On a seat the supervisor sets it, never the seat.
    pub seat: u16,
    /// An identity blob (`pf_driver_proto::triton::identity_blob`), or empty.
    pub identity: Vec<u8>,
}

impl Hello {
    pub fn encode(&self) -> Vec<u8> {
        let [seat_lo, seat_hi] = self.seat.to_le_bytes();
        let mut m = vec![
            HELLO,
            self.kind as u8,
            self.index,
            self.slot,
            seat_lo,
            seat_hi,
        ];
        m.extend_from_slice(&self.identity);
        m
    }

    pub fn decode(m: &[u8]) -> Option<Hello> {
        let (&[HELLO, kind, index, slot, seat_lo, seat_hi], identity) =
            m.split_first_chunk::<6>()?
        else {
            return None;
        };
        if identity.len() > MAX_IDENTITY {
            return None;
        }
        Some(Hello {
            kind: UsbKind::from_wire(kind)?,
            index,
            slot: slot.min(3),
            seat: u16::from_le_bytes([seat_lo, seat_hi]),
            identity: identity.to_vec(),
        })
    }

    /// The client's identity, checked; `None` when it sent none.
    pub fn identity(&self) -> Result<Option<Arc<Sc2Identity>>> {
        checked_identity(&self.identity, self.slot).map(|id| id.map(Arc::new))
    }
}

/// An identity blob, read through `pf_driver_proto`'s checked view. Its serial becomes a USB
/// string, so only 1–16 ASCII letters and digits pass.
pub fn checked_identity(blob: &[u8], slot: u8) -> Result<Option<Sc2Identity>> {
    if blob.is_empty() {
        return Ok(None);
    }
    if blob.len() > MAX_IDENTITY {
        bail!("identity of {} bytes", blob.len());
    }
    let id = pf_driver_proto::triton::Identity::parse(blob)
        .ok_or_else(|| anyhow!("identity blob does not parse"))?;
    let serial = id.serial;
    let plain = serial.len() <= MAX_SERIAL && serial.bytes().all(|b| b.is_ascii_alphanumeric());
    if !plain {
        bail!("identity serial {serial:?} is not 1–16 letters and digits");
    }
    let replies = id
        .pairs()
        .map(|(request, reply)| {
            let mut full = [0u8; 64];
            full[..reply.len()].copy_from_slice(reply);
            (request.to_vec(), full)
        })
        .collect();
    Ok(Some(Sc2Identity {
        serial: (!serial.is_empty()).then(|| serial.to_owned()),
        replies,
        slot: slot.min(3),
    }))
}

/// What a pad sends its server after [`Hello`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Incoming<'a> {
    /// The input report interface `iface` serves from now on.
    Input { iface: u8, report: &'a [u8] },
    /// A Puck: a pad sits down at `slot` as `identity` (a blob, or empty).
    Seat {
        slot: u8,
        index: u8,
        identity: &'a [u8],
    },
    /// A Puck: the pad at `slot` left.
    Vacate { slot: u8 },
}

impl Incoming<'_> {
    pub fn encode(&self) -> Vec<u8> {
        match *self {
            Incoming::Input { iface, report } => [&[INPUT, iface][..], report].concat(),
            Incoming::Seat {
                slot,
                index,
                identity,
            } => [&[SEAT, slot, index][..], identity].concat(),
            Incoming::Vacate { slot } => vec![VACATE, slot],
        }
    }

    pub fn decode(m: &[u8]) -> Option<Incoming<'_>> {
        Some(match m {
            [INPUT, iface, report @ ..] if report.len() <= 64 => Incoming::Input {
                iface: *iface,
                report,
            },
            [SEAT, slot, index, identity @ ..] if identity.len() <= MAX_IDENTITY => {
                Incoming::Seat {
                    slot: *slot,
                    index: *index,
                    identity,
                }
            }
            [VACATE, slot] => Incoming::Vacate { slot: *slot },
            _ => return None,
        })
    }
}

/// What the host wrote to the device, as the server got it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Output {
    /// An OUT transfer on `iface`'s endpoint, or with `set` (its `wValue`) a SET_REPORT on EP0.
    Report {
        iface: u8,
        set: Option<u16>,
        data: Vec<u8>,
    },
    /// Isochronous OUT payloads, S16LE, packets back to back.
    Audio(Vec<u8>),
}

impl Output {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Output::Report { iface, set, data } => {
                let [lo, hi] = set.unwrap_or(0).to_le_bytes();
                [&[OUTPUT, *iface, u8::from(set.is_some()), lo, hi][..], data].concat()
            }
            Output::Audio(pcm) => [&[AUDIO][..], pcm].concat(),
        }
    }

    pub fn decode(m: &[u8]) -> Option<Output> {
        Some(match m {
            [OUTPUT, iface, has_set @ (0 | 1), lo, hi, data @ ..] => Output::Report {
                iface: *iface,
                set: (*has_set == 1).then(|| u16::from_le_bytes([*lo, *hi])),
                data: data.to_vec(),
            },
            [AUDIO, pcm @ ..] => Output::Audio(pcm.to_vec()),
            _ => return None,
        })
    }
}

/// The server's way back to its pad. It never blocks: a pad that stops reading loses outputs,
/// never the device.
#[derive(Clone, Debug)]
pub struct Outputs(Arc<UnixDatagram>);

impl Outputs {
    pub fn new(relay: &UnixDatagram) -> Result<Outputs> {
        Ok(Outputs(Arc::new(
            relay.try_clone().context("clone the pad relay")?,
        )))
    }

    pub(crate) fn report(&self, iface: u8, set: Option<u16>, data: &[u8]) {
        if !data.is_empty() {
            self.send(&Output::Report {
                iface,
                set,
                data: data.to_vec(),
            });
        }
    }

    pub(crate) fn audio(&self, pcm: Vec<u8>) {
        if !pcm.is_empty() {
            self.send(&Output::Audio(pcm));
        }
    }

    fn send(&self, output: &Output) {
        let _ = rustix::net::send(
            &*self.0,
            &output.encode(),
            SendFlags::DONTWAIT | SendFlags::NOSIGNAL,
        );
    }
}

/// A kind's device and what feeds it: each message from the pad goes to `input`.
pub struct Served {
    pub device: UsbDevice,
    pub input: Box<dyn FnMut(Incoming<'_>) + Send>,
}

/// The feed of a device that serves one `N`-byte report on interface `iface`: an input for
/// it, `N` bytes long and led by `id` when the table numbers its reports, replaces `slot`.
/// Anything else is dropped.
pub(crate) fn one_report<const N: usize>(
    slot: Arc<std::sync::Mutex<[u8; N]>>,
    iface: u8,
    id: Option<u8>,
) -> Box<dyn FnMut(Incoming<'_>) + Send> {
    Box::new(move |message| {
        let Incoming::Input { iface: to, report } = message else {
            return;
        };
        let Ok(report) = <[u8; N]>::try_from(report) else {
            return;
        };
        if to == iface && id.is_none_or(|id| report[0] == id) {
            *slot.lock().unwrap_or_else(|e| e.into_inner()) = report;
        }
    })
}

/// Builds `hello`'s device from this crate's tables, sending what the host writes to `out`.
pub fn served(hello: &Hello, out: Outputs) -> Result<Served> {
    use crate::steam_proto::SteamModel;
    use crate::switch2_proto::Model;
    let identity = hello.identity()?;
    Ok(match hello.kind {
        UsbKind::SteamDeck => crate::steam_usbip::served(hello.index, SteamModel::Deck, None, out),
        UsbKind::SteamController => {
            crate::steam_usbip::served(hello.index, SteamModel::Controller, identity, out)
        }
        UsbKind::SteamController2 => crate::triton_usbip::served_wired(hello.index, identity, out),
        UsbKind::SteamController2Puck => {
            crate::triton_usbip::served_puck(hello.index, hello.slot, identity, out)
        }
        UsbKind::DualSense => crate::dualsense_usbip::served(hello.index, hello.seat, out),
        UsbKind::Switch2Pro => crate::switch2_usbip::served(Model::Pro, hello.index, out),
        UsbKind::Switch2GameCube => crate::switch2_usbip::served(Model::GameCube, hello.index, out),
    })
}

/// [`Outputs`] into a fresh relay, and the relay's pad end to read them from.
#[cfg(test)]
pub(crate) fn test_relay() -> (Outputs, UnixDatagram) {
    let (ours, pad) = crate::pad_broker::relay_pair().unwrap();
    let pad = UnixDatagram::from(pad);
    pad.set_nonblocking(true).unwrap();
    (Outputs::new(&ours).unwrap(), pad)
}

/// Every output waiting on a [`test_relay`]'s pad end.
#[cfg(test)]
pub(crate) fn drain(pad: &UnixDatagram) -> Vec<Output> {
    let mut buf = vec![0u8; MAX_MESSAGE];
    std::iter::from_fn(|| {
        let n = pad.recv(&mut buf).ok()?;
        Some(Output::decode(&buf[..n]))
    })
    .flatten()
    .collect()
}

/// The input reports `rdesc` declares that the kernel turns into a key below `BTN_MISC` or a
/// pointer: a keyboard or consumer usage, a keyboard, keypad, mouse or system-control
/// collection, or a relative axis. `None` stands for a descriptor without report ids.
#[cfg(test)]
pub(crate) fn risky_input_reports(rdesc: &[u8]) -> Vec<Option<u8>> {
    use hidreport::{Field, FieldAttributes, Report, ReportDescriptor, Usage};
    let page = |u: &Usage| u16::from(u.usage_page);
    let risky_usage = |u: &Usage| matches!(page(u), 0x07 | 0x0C);
    let risky_collection = |u: &Usage| {
        page(u) == 0x01 && matches!(u16::from(u.usage_id), 0x02 | 0x06 | 0x07 | 0x80..=0x83)
    };
    let rd = ReportDescriptor::try_from(rdesc).expect("a table's descriptor parses");
    rd.input_reports()
        .iter()
        .filter(|report| {
            report.fields().iter().any(|field| {
                let in_risky = field
                    .collections()
                    .iter()
                    .flat_map(|c| c.usages())
                    .any(risky_collection);
                in_risky
                    || match field {
                        Field::Variable(v) => risky_usage(&v.usage) || v.is_relative(),
                        Field::Array(a) => a.usages().iter().any(risky_usage),
                        Field::Constant(_) => false,
                    }
            })
        })
        .map(|report| report.report_id().as_ref().map(u8::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use usbip_sim::usbip_protocol::{UsbIpCommand, UsbIpHeaderBasic, USBIP_CMD_SUBMIT};
    use usbip_sim::{Direction, SetupPacket, UsbEndpoint, UsbIpServer};

    /// Every kind's device, built as a seat's pad server builds it.
    fn every_device() -> impl Iterator<Item = (UsbKind, Served)> {
        UsbKind::ALL.into_iter().map(|kind| {
            let hello = Hello {
                kind,
                index: 1,
                slot: 0,
                seat: 0,
                identity: Vec::new(),
            };
            (kind, served(&hello, test_relay().0).unwrap())
        })
    }

    const EP0: UsbEndpoint = UsbEndpoint {
        address: 0x80,
        attributes: 0,
        max_packet_size: 64,
        interval: 0,
    };

    /// One poll of interface `iface`'s interrupt IN; `None` without one, or while it waits.
    fn poll_in(device: &UsbDevice, iface: usize) -> Option<Vec<u8>> {
        let intf = &device.interfaces[iface];
        let ep = *intf
            .endpoints
            .iter()
            .find(|e| e.attributes & 3 == 3 && matches!(e.direction(), Direction::In))?;
        let mut handler = intf.handler.lock().unwrap();
        handler
            .handle_urb(intf, ep, 64, SetupPacket::default(), &[])
            .ok()
    }

    fn report_descriptor(device: &UsbDevice, iface: usize) -> Vec<u8> {
        let intf = &device.interfaces[iface];
        let setup = SetupPacket {
            request_type: 0x81,
            request: 0x06,
            value: 0x2200,
            index: iface as u16,
            length: 0xFFFF,
        };
        let mut handler = intf.handler.lock().unwrap();
        handler
            .handle_urb(intf, EP0, 0xFFFF, setup, &[])
            .unwrap_or_default()
    }

    /// No kind takes an input report its own table makes a keyboard, consumer control or
    /// pointer of: the Deck's mouse and keyboard take nothing, the SC2's `0x40` and `0x41`
    /// never pass. Every report id is tried on every interface, as a seat could.
    #[test]
    fn no_kind_takes_a_report_its_table_makes_a_keyboard_or_pointer() {
        for (kind, mut probe) in every_device() {
            let mut took_any = false;
            for iface in 0..probe.device.interfaces.len() {
                if poll_in(&probe.device, iface).is_none()
                    && !probe.device.interfaces[iface]
                        .endpoints
                        .iter()
                        .any(|e| e.attributes & 3 == 3)
                {
                    continue;
                }
                let rdesc = report_descriptor(&probe.device, iface);
                let risky = if rdesc.is_empty() {
                    Vec::new()
                } else {
                    risky_input_reports(&rdesc)
                };
                for _ in 0..8 {
                    poll_in(&probe.device, iface);
                }
                for id in 0..=255u8 {
                    let mut report = [0xA5u8; 64];
                    report[0] = id;
                    (probe.input)(Incoming::Input {
                        iface: iface as u8,
                        report: &report,
                    });
                    let taken = poll_in(&probe.device, iface)
                        .is_some_and(|r| r.len() > 2 && r[0] == id && r[2] == 0xA5);
                    if taken {
                        took_any = true;
                        assert!(
                            !risky.contains(&Some(id)) && !risky.contains(&None),
                            "{kind:?} interface {iface} took report {id:#04x}"
                        );
                    }
                }
            }
            assert!(took_any, "{kind:?} took no report at all");
        }
    }

    /// The SC2's own descriptor is why the list exists: its mouse and keyboard reports.
    #[test]
    fn the_sc2_table_declares_a_mouse_and_a_keyboard() {
        assert_eq!(
            risky_input_reports(crate::triton_proto::TRITON_RDESC),
            [Some(0x40), Some(0x41)]
        );
        assert_eq!(
            risky_input_reports(crate::steam_proto::RDESC_DECK_KBD),
            [None]
        );
    }

    /// A ep0 IN SUBMIT carrying `setup`.
    fn submit(seqnum: u32, setup: [u8; 8]) -> Vec<u8> {
        UsbIpCommand::UsbIpCmdSubmit {
            header: UsbIpHeaderBasic {
                command: USBIP_CMD_SUBMIT.into(),
                seqnum,
                devid: 0,
                direction: 1,
                ep: 0,
            },
            transfer_flags: 0,
            transfer_buffer_length: u16::from_le_bytes([setup[6], setup[7]]).into(),
            start_frame: 0,
            number_of_packets: 0,
            interval: 0,
            setup,
            data: Vec::new(),
            iso_packet_descriptor: Vec::new(),
        }
        .to_bytes()
    }

    /// Every descriptor a host reads: device, configuration, the first strings, and each
    /// interface's report descriptor.
    fn descriptor_reads(device: &UsbDevice) -> Vec<[u8; 8]> {
        let mut reads = vec![
            [0x80, 0x06, 0x00, 0x01, 0, 0, 0x12, 0],
            [0x80, 0x06, 0x00, 0x02, 0, 0, 0xFF, 0x0F],
        ];
        reads.extend((1..=3u8).map(|i| [0x80, 0x06, i, 0x03, 0x09, 0x04, 0xFF, 0]));
        reads.extend(
            (0..device.interfaces.len() as u8).map(|i| [0x81, 0x06, 0, 0x22, i, 0, 0xFF, 0x0F]),
        );
        reads
    }

    /// `(status, payload)` of each read, in order, over the USB/IP wire.
    async fn read_all(
        host: &mut tokio::io::DuplexStream,
        reads: &[[u8; 8]],
    ) -> Vec<(i32, Vec<u8>)> {
        let mut out = Vec::new();
        for (seqnum, setup) in (1..).zip(reads) {
            host.write_all(&submit(seqnum, *setup)).await.unwrap();
            let mut head = [0u8; 48];
            host.read_exact(&mut head).await.unwrap();
            let be = |i: usize| u32::from_be_bytes(head[i..i + 4].try_into().unwrap());
            let mut payload = vec![0u8; be(24) as usize];
            host.read_exact(&mut payload).await.unwrap();
            out.push((be(20) as i32, payload));
        }
        out
    }

    /// A seat cannot change a descriptor after attach: what the host reads over USB/IP is the
    /// same before and after the pad sent every kind of message, descriptor-shaped junk too.
    #[test]
    fn a_pad_cannot_change_a_descriptor_after_attach() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fake_keyboard = crate::steam_proto::RDESC_DECK_KBD;
        for (kind, Served { device, mut input }) in every_device() {
            let reads = descriptor_reads(&device);
            let interfaces = device.interfaces.len() as u8;
            let server = Arc::new(UsbIpServer::new_simulated(vec![device]));
            rt.block_on(async {
                let (mut host, mut sim) = tokio::io::duplex(1 << 16);
                tokio::spawn(async move { usbip_sim::handler(&mut sim, server).await });
                let mut busid = [0u8; 32];
                busid[..5].copy_from_slice(b"0-0-0");
                let import = UsbIpCommand::OpReqImport { status: 0, busid };
                host.write_all(&import.to_bytes()).await.unwrap();
                host.read_exact(&mut [0u8; 320]).await.unwrap();

                let before = read_all(&mut host, &reads).await;
                for iface in 0..interfaces + 2 {
                    for report in [&[0x41, 0, 4][..], fake_keyboard, &[0x12, 0x01, 0, 2]] {
                        input(Incoming::Input { iface, report });
                    }
                }
                for slot in 0..6 {
                    input(Incoming::Seat {
                        slot,
                        index: 9,
                        identity: fake_keyboard,
                    });
                    input(Incoming::Vacate { slot });
                }
                let after = read_all(&mut host, &reads).await;
                assert_eq!(before, after, "{kind:?}");
                assert!(
                    !before[0].1.is_empty(),
                    "{kind:?} answered its device descriptor"
                );
            });
        }
    }

    #[test]
    fn messages_round_trip_and_junk_is_refused() {
        let hello = Hello {
            kind: UsbKind::SteamController2Puck,
            index: 3,
            slot: 2,
            seat: 0xBEEF,
            identity: vec![1, b'A'],
        };
        assert_eq!(Hello::decode(&hello.encode()), Some(hello.clone()));
        let mut unknown = hello.encode();
        unknown[1] = 200;
        assert_eq!(Hello::decode(&unknown), None, "unknown kind");
        assert_eq!(Hello::decode(&[HELLO, 1, 0, 0, 0]), None, "short");
        let huge = Hello {
            identity: vec![0; MAX_IDENTITY + 1],
            ..hello
        };
        assert_eq!(Hello::decode(&huge.encode()), None, "oversize identity");

        for m in [
            Incoming::Input {
                iface: 2,
                report: &[0x42, 1, 2],
            },
            Incoming::Seat {
                slot: 1,
                index: 4,
                identity: &[0],
            },
            Incoming::Vacate { slot: 3 },
        ] {
            assert_eq!(Incoming::decode(&m.encode()), Some(m));
        }
        assert_eq!(Incoming::decode(&[INPUT, 0]).unwrap().encode(), [INPUT, 0]);
        assert_eq!(Incoming::decode(&[INPUT, 0, 0].repeat(40)), None);
        assert_eq!(
            Incoming::decode(&[OUTPUT, 0, 0, 0, 0]),
            None,
            "not the pad's"
        );

        for o in [
            Output::Report {
                iface: 3,
                set: Some(0x0302),
                data: vec![2, 9],
            },
            Output::Report {
                iface: 1,
                set: None,
                data: vec![7],
            },
            Output::Audio(vec![1, 2, 3, 4]),
        ] {
            assert_eq!(Output::decode(&o.encode()), Some(o));
        }
        assert_eq!(Output::decode(&[OUTPUT, 0, 2, 0, 0]), None);
    }

    #[test]
    fn an_identity_passes_only_whole_and_with_a_plain_serial() {
        use pf_driver_proto::triton::identity_blob;
        let pairs = [(&[0x01, 0x83][..], &[0x01, 0x83, 0x19][..])];
        let id = checked_identity(&identity_blob("FXA0000000001", pairs), 1)
            .unwrap()
            .unwrap();
        assert_eq!(id.serial.as_deref(), Some("FXA0000000001"));
        assert_eq!(id.slot, 1);
        assert_eq!(id.reply(&[0x01, 0x83]).unwrap()[2], 0x19);
        assert_eq!(checked_identity(&[], 0).unwrap(), None);
        let no_serial = checked_identity(&identity_blob("", pairs), 0)
            .unwrap()
            .unwrap();
        assert_eq!(no_serial.serial, None);
        for bad in ["FXA 1", "FXA-1", "ABCDEFGHIJKLMNOPQ", "Ünicode"] {
            assert!(
                checked_identity(&identity_blob(bad, pairs), 0).is_err(),
                "{bad:?}"
            );
        }
        assert!(checked_identity(&[5, b'A'], 0).is_err(), "torn");
    }
}
