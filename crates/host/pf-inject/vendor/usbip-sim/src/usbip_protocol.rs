//! USB/IP protocol structs
//!
//! This module contains declarations of all structs used in the USB/IP protocol,
//! as well as functions to serialize and deserialize them to/from byte arrays,
//! and functions to send and receive them over a socket.
//!
//! They are based on the [Linux kernel documentation](https://docs.kernel.org/usb/usbip_protocol.html).

use log::trace;
use std::io::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::UsbDevice;

/// USB/IP protocol version
///
/// This is currently the only supported version of USB/IP
/// for this library.
pub const USBIP_VERSION: u16 = 0x0111;

/// Command code: Retrieve the list of exported USB devices
pub const OP_REQ_DEVLIST: u16 = 0x8005;
/// Command code: import a remote USB device
pub const OP_REQ_IMPORT: u16 = 0x8003;
/// Reply code: The list of exported USB devices
pub const OP_REP_DEVLIST: u16 = 0x0005;
/// Reply code: Reply to import
pub const OP_REP_IMPORT: u16 = 0x0003;

/// Command code: Submit an URB
pub const USBIP_CMD_SUBMIT: u16 = 0x0001;
/// Command code: Unlink an URB
pub const USBIP_CMD_UNLINK: u16 = 0x0002;
/// Reply code: Reply for submitting an URB
pub const USBIP_RET_SUBMIT: u16 = 0x0003;
/// Reply code: Reply for URB unlink
pub const USBIP_RET_UNLINK: u16 = 0x0004;

/// USB/IP direction
///
/// NOTE: Must not be confused with rusb::Direction,
/// which has the opposite enum values. This is only for
/// internal use in the USB/IP protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Out = 0,
    In = 1,
}

/// Common header for all context sensitive packets
///
/// All commands/responses which rely on a device being attached
/// to a client use this header.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UsbIpHeaderBasic {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

impl UsbIpHeaderBasic {
    /// Converts a byte array into a [UsbIpHeaderBasic].
    pub fn from_bytes(bytes: &[u8; 20]) -> Self {
        let result = UsbIpHeaderBasic {
            command: u32::from_be_bytes(bytes[0..4].try_into().unwrap()),
            seqnum: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            devid: u32::from_be_bytes(bytes[8..12].try_into().unwrap()),
            direction: u32::from_be_bytes(bytes[12..16].try_into().unwrap()),
            ep: u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
        };
        // The direction should be 0 or 1
        debug_assert!(result.direction & 1 == result.direction);
        result
    }

    /// Converts the [UsbIpHeaderBasic] into a byte array.
    pub fn to_bytes(&self) -> [u8; 20] {
        let mut result = [0u8; 20];
        result[0..4].copy_from_slice(&self.command.to_be_bytes());
        result[4..8].copy_from_slice(&self.seqnum.to_be_bytes());
        result[8..12].copy_from_slice(&self.devid.to_be_bytes());
        result[12..16].copy_from_slice(&self.direction.to_be_bytes());
        result[16..20].copy_from_slice(&self.ep.to_be_bytes());
        result
    }

    pub(crate) async fn read_from_socket_with_command<T: AsyncReadExt + Unpin>(
        socket: &mut T,
        command: u16,
    ) -> Result<Self> {
        let seqnum = socket.read_u32().await?;
        let devid = socket.read_u32().await?;
        let direction = socket.read_u32().await?;
        // The direction should be 0 or 1
        debug_assert!(direction & 1 == direction);
        let ep = socket.read_u32().await?;

        Ok(UsbIpHeaderBasic {
            command: command.into(),
            seqnum,
            devid,
            direction,
            ep,
        })
    }
}

/// Client side commands from the Virtual Host Controller
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum UsbIpCommand {
    OpReqDevlist {
        status: u32,
    },
    OpReqImport {
        status: u32,
        busid: [u8; 32],
    },
    UsbIpCmdSubmit {
        header: UsbIpHeaderBasic,
        transfer_flags: u32,
        transfer_buffer_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        interval: u32,
        setup: [u8; 8],
        data: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    },
    UsbIpCmdUnlink {
        header: UsbIpHeaderBasic,
        unlink_seqnum: u32,
    },
}

impl UsbIpCommand {
    /// Constructs a [UsbIpCommand] from a socket
    ///
    /// This will consume a variable amount of bytes from the socket.
    /// It might fail if the bytes does not follow the USB/IP protocol properly.
    pub async fn read_from_socket<T: AsyncReadExt + Unpin>(socket: &mut T) -> Result<UsbIpCommand> {
        let version: u16 = socket.read_u16().await?;

        if version != 0 && version != USBIP_VERSION {
            return Err(std::io::Error::other(format!(
                "Unknown version: {version:#04X}"
            )));
        }

        let command: u16 = socket.read_u16().await?;

        trace!(
            "Received command: {:#04X} ({}), parsing...",
            command,
            match command {
                OP_REQ_DEVLIST => "OP_REQ_DEVLIST",
                OP_REQ_IMPORT => "OP_REQ_IMPORT",
                USBIP_CMD_SUBMIT => "USBIP_CMD_SUBMIT",
                USBIP_CMD_UNLINK => "USBIP_CMD_UNLINK",
                _ => "Unknown",
            }
        );

        match command {
            OP_REQ_DEVLIST => {
                let status = socket.read_u32().await?;
                debug_assert!(status == 0);

                Ok(UsbIpCommand::OpReqDevlist { status })
            }
            OP_REQ_IMPORT => {
                let status = socket.read_u32().await?;
                debug_assert!(status == 0);
                let mut busid = [0; 32];
                socket.read_exact(&mut busid).await?;

                Ok(UsbIpCommand::OpReqImport { status, busid })
            }
            USBIP_CMD_SUBMIT => {
                let header =
                    UsbIpHeaderBasic::read_from_socket_with_command(socket, USBIP_CMD_SUBMIT)
                        .await?;
                let transfer_flags = socket.read_u32().await?;
                let transfer_buffer_length = socket.read_u32().await?;
                let start_frame = socket.read_u32().await?;
                let number_of_packets = socket.read_u32().await?;
                let interval = socket.read_u32().await?;

                let mut setup = [0; 8];
                socket.read_exact(&mut setup).await?;

                let data = if header.direction == Direction::In as u32 {
                    vec![]
                } else {
                    let mut data = vec![0; transfer_buffer_length as usize];
                    socket.read_exact(&mut data).await?;
                    data
                };

                // The kernel docs specifies that this should be set to 0xFFFFFFFF for all
                // non-ISO packets, however the actual implementation resorts to 0x00000000
                // https://stackoverflow.com/questions/76899798/usb-ip-what-is-the-size-of-the-iso-packet-descriptor
                let iso_packet_descriptor =
                    if number_of_packets != 0 && number_of_packets != 0xFFFFFFFF {
                        let mut result = vec![0; 16 * number_of_packets as usize];
                        socket.read_exact(&mut result).await?;
                        result
                    } else {
                        vec![]
                    };

                Ok(UsbIpCommand::UsbIpCmdSubmit {
                    header,
                    transfer_flags,
                    transfer_buffer_length,
                    start_frame,
                    number_of_packets,
                    interval,
                    setup,
                    data,
                    iso_packet_descriptor,
                })
            }
            USBIP_CMD_UNLINK => {
                let header =
                    UsbIpHeaderBasic::read_from_socket_with_command(socket, USBIP_CMD_UNLINK)
                        .await?;
                let unlink_seqnum = socket.read_u32().await?;

                let mut _padding = [0; 24];
                socket.read_exact(&mut _padding).await?;

                Ok(UsbIpCommand::UsbIpCmdUnlink {
                    header,
                    unlink_seqnum,
                })
            }
            _ => Err(std::io::Error::other(format!(
                "Unknown command: {command:#04X}"
            ))),
        }
    }

    /// Converts the [UsbIpCommand] into a byte vector
    pub fn to_bytes(&self) -> Vec<u8> {
        match *self {
            UsbIpCommand::OpReqDevlist { status } => {
                let mut result = Vec::with_capacity(8);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REQ_DEVLIST.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result
            }
            UsbIpCommand::OpReqImport { status, busid } => {
                let mut result = Vec::with_capacity(40);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REQ_IMPORT.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&busid);
                result
            }
            UsbIpCommand::UsbIpCmdSubmit {
                ref header,
                transfer_flags,
                transfer_buffer_length,
                start_frame,
                number_of_packets,
                interval,
                setup,
                ref data,
                ref iso_packet_descriptor,
            } => {
                debug_assert!(
                    header.direction != Direction::Out as u32
                        || transfer_buffer_length == data.len() as u32
                );

                let mut result = Vec::with_capacity(48 + data.len() + iso_packet_descriptor.len());
                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&transfer_flags.to_be_bytes());
                result.extend_from_slice(&transfer_buffer_length.to_be_bytes());
                result.extend_from_slice(&start_frame.to_be_bytes());
                result.extend_from_slice(&number_of_packets.to_be_bytes());
                result.extend_from_slice(&interval.to_be_bytes());
                result.extend_from_slice(&setup);
                result.extend_from_slice(data);
                result.extend_from_slice(iso_packet_descriptor);
                result
            }
            UsbIpCommand::UsbIpCmdUnlink {
                ref header,
                unlink_seqnum,
            } => {
                let mut result = Vec::with_capacity(48);
                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&unlink_seqnum.to_be_bytes());
                result.extend_from_slice(&[0; 24]);
                result
            }
        }
    }
}

/// Server side responses from the USB Host
#[derive(Clone)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub enum UsbIpResponse {
    OpRepDevlist {
        status: u32,
        device_count: u32,
        devices: Vec<UsbDevice>,
    },
    OpRepImport {
        status: u32,
        device: Option<UsbDevice>,
    },
    UsbIpRetSubmit {
        header: UsbIpHeaderBasic,
        status: u32,
        actual_length: u32,
        start_frame: u32,
        number_of_packets: u32,
        error_count: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    },
    UsbIpRetUnlink {
        header: UsbIpHeaderBasic,
        status: u32,
    },
}

impl UsbIpResponse {
    /// Converts the [UsbIpResponse] into a byte vector
    pub fn to_bytes(&self) -> Vec<u8> {
        match *self {
            Self::OpRepDevlist {
                status,
                device_count,
                ref devices,
            } => {
                let mut result = Vec::with_capacity(
                    12 + devices.len() * 312
                        + devices
                            .iter()
                            .map(|d| d.interfaces.len() * 4)
                            .sum::<usize>(),
                );
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REP_DEVLIST.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&device_count.to_be_bytes());
                for dev in devices {
                    result.extend_from_slice(&dev.to_bytes_with_interfaces());
                }
                result
            }
            Self::OpRepImport { status, ref device } => {
                let mut result = Vec::with_capacity(320);
                result.extend_from_slice(&USBIP_VERSION.to_be_bytes());
                result.extend_from_slice(&OP_REP_IMPORT.to_be_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                if let Some(device) = device {
                    result.extend_from_slice(&device.to_bytes());
                }
                result
            }
            Self::UsbIpRetSubmit {
                ref header,
                status,
                actual_length,
                start_frame,
                number_of_packets,
                error_count,
                ref transfer_buffer,
                ref iso_packet_descriptor,
            } => {
                let mut result =
                    Vec::with_capacity(48 + transfer_buffer.len() + iso_packet_descriptor.len());

                debug_assert!(header.command == USBIP_RET_SUBMIT.into());
                // `actual_length` is "bytes the device transferred", in BOTH directions and for
                // every transfer type. Only an IN reply carries a buffer to check it against: an
                // OUT reply reports the bytes it *accepted* and sends nothing back, so on OUT the
                // one invariant is an empty buffer. For an isochronous URB the value is the sum
                // of the per-packet actuals, which is what the kernel's `usbip_recv_iso()`
                // recomputes and tears the whole connection down over on a mismatch.
                debug_assert!(if number_of_packets != 0 {
                    actual_length
                        == iso_packet_descriptor
                            .chunks_exact(16)
                            .map(|d| u32::from_be_bytes([d[8], d[9], d[10], d[11]]))
                            .sum::<u32>()
                } else if header.direction == Direction::In as u32 {
                    actual_length == transfer_buffer.len() as u32
                } else {
                    transfer_buffer.is_empty()
                });

                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&actual_length.to_be_bytes());
                result.extend_from_slice(&start_frame.to_be_bytes());
                result.extend_from_slice(&number_of_packets.to_be_bytes());
                result.extend_from_slice(&error_count.to_be_bytes());
                result.extend_from_slice(&[0; 8]);
                result.extend_from_slice(transfer_buffer);
                result.extend_from_slice(iso_packet_descriptor);
                result
            }
            Self::UsbIpRetUnlink { ref header, status } => {
                let mut result = Vec::with_capacity(48);

                debug_assert!(header.command == USBIP_RET_UNLINK.into());

                result.extend_from_slice(&header.to_bytes());
                result.extend_from_slice(&status.to_be_bytes());
                result.extend_from_slice(&[0; 24]);
                result
            }
        }
    }

    pub async fn write_to_socket<T: AsyncWriteExt + Unpin>(&self, socket: &mut T) -> Result<()> {
        socket.write_all(&self.to_bytes()).await
    }

    /// Constructs a OP_REP_DEVLIST response
    pub fn op_rep_devlist(devices: &[UsbDevice]) -> Self {
        Self::OpRepDevlist {
            status: 0,
            device_count: devices.len() as u32,
            devices: devices.to_vec(),
        }
    }

    /// Constructs a successful OP_REP_IMPORT response
    pub fn op_rep_import_success(device: &UsbDevice) -> Self {
        Self::OpRepImport {
            status: 0,
            device: Some(device.clone()),
        }
    }

    /// Constructs a failed OP_REP_IMPORT response
    pub fn op_rep_import_fail() -> Self {
        Self::OpRepImport {
            status: 1,
            device: None,
        }
    }

    /// Constructs a successful `USBIP_RET_SUBMIT` for an **IN** URB: the payload the device
    /// produced, with `actual_length` counting it. Not for OUT — see
    /// [`usbip_ret_submit_out_success`](Self::usbip_ret_submit_out_success).
    pub fn usbip_ret_submit_success(
        header: &UsbIpHeaderBasic,
        start_frame: u32,
        number_of_packets: u32,
        transfer_buffer: Vec<u8>,
        iso_packet_descriptor: Vec<u8>,
    ) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 0,
            actual_length: transfer_buffer.len() as u32,
            start_frame,
            number_of_packets,
            error_count: 0,
            transfer_buffer,
            iso_packet_descriptor,
        }
    }

    /// Constructs a successful `USBIP_RET_SUBMIT` for a non-isochronous **OUT** URB (punktfunk
    /// addition): no payload back, `actual_length` = the bytes the device `accepted`.
    ///
    /// Upstream answered OUT with [`usbip_ret_submit_success`](Self::usbip_ret_submit_success) and
    /// an empty buffer, i.e. `actual_length = 0`. `vhci_hcd` copies that field straight into
    /// `urb->actual_length` (`usbip_pack_pdu(pdu, urb, USBIP_RET_SUBMIT, 0)` in
    /// `vhci_recv_ret_submit()`), and for an OUT URB the kernel has no other source for the count.
    /// So every synchronous writer up the stack was told "0 bytes transferred" on success:
    /// `usbhid_output_report()` returns `actual_length` as the byte count, so a `write()` on the
    /// pad's hidraw returned **0**, and `usb_control_msg()` returns the data-stage length, so
    /// `HIDIOCSFEATURE` returned **0** too. Anything checking `> 0` — winebus's
    /// `hidraw_device_set_output_report`, whose failure branch prints the thread's *stale* errno,
    /// hence "write failed error: 2 No such file or directory" — took the write as failed, and
    /// GE-Proton's `hidraw_enable_dualsense_usb_haptics` never enabled the DualSense's USB haptics
    /// mode. Adaptive triggers, voice-coil haptics and the speaker are all gated behind that one
    /// enable. A real usbip stub reports the real URB's `actual_length`, which on OUT is the
    /// number of bytes sent. Field-diagnosed 2026-08-18.
    pub fn usbip_ret_submit_out_success(header: &UsbIpHeaderBasic, accepted: u32) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 0,
            actual_length: accepted,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            transfer_buffer: Vec::new(),
            iso_packet_descriptor: Vec::new(),
        }
    }

    /// Constructs a successful `USBIP_RET_SUBMIT` for an **isochronous** URB (punktfunk addition).
    ///
    /// `replies` holds one payload per ISO packet (empty for an OUT endpoint) and `requested` the
    /// per-packet `length` the host asked for. The reply's `iso_packet_descriptor` restates the
    /// host's `length` and fills in `offset`, `actual_length` and a per-packet `status` of 0.
    ///
    /// **`actual_length` means "bytes the device transferred", which differs by direction.** On an
    /// IN endpoint that is the payload we produced, concatenated into the transfer buffer at the
    /// offsets stated. On an OUT endpoint there is no payload to return and the value is the number
    /// of bytes we *accepted* — the whole packet. Reporting the empty reply's length there would
    /// tell the kernel the device swallowed nothing, and an audio stream would make no progress
    /// while looking healthy.
    ///
    /// Note this deliberately bypasses [`usbip_ret_submit_success`]'s
    /// `direction == Out ⇒ actual_length == 0` assertion: for ISO the field carries the total of
    /// the per-packet actual lengths in **both** directions.
    pub fn usbip_ret_submit_iso(
        header: &UsbIpHeaderBasic,
        start_frame: u32,
        requested: &[u32],
        replies: &[Vec<u8>],
    ) -> Self {
        let inbound = header.direction == Direction::In as u32;
        let mut transfer_buffer = Vec::new();
        let mut iso_packet_descriptor = Vec::with_capacity(16 * requested.len());
        let mut offset = 0u32;
        // The kernel's own invariant, and the reason this is a running total rather than the
        // buffer's length: `usbip_recv_iso()` sums every packet's `actual_length` and compares
        // the total against the URB's `actual_length`, and on a mismatch raises `ERROR_TCP` and
        // returns `-EPIPE` — which tears the whole connection down (stop threads, release socket,
        // disconnect device), not just the one URB. On an OUT endpoint the transfer buffer is
        // EMPTY while the per-packet actuals are the bytes we accepted, so sizing this from the
        // buffer reported "device swallowed nothing" against nonzero packets and killed the pad
        // ~56 ms after `snd-usb-audio` started the stream.
        let mut total_actual = 0u32;
        for (i, &req_len) in requested.iter().enumerate() {
            let actual = if inbound {
                let payload = replies.get(i).map(Vec::as_slice).unwrap_or(&[]);
                let n = payload.len().min(req_len as usize);
                transfer_buffer.extend_from_slice(&payload[..n]);
                n as u32
            } else {
                req_len
            };
            iso_packet_descriptor.extend_from_slice(&offset.to_be_bytes());
            iso_packet_descriptor.extend_from_slice(&req_len.to_be_bytes());
            iso_packet_descriptor.extend_from_slice(&actual.to_be_bytes());
            iso_packet_descriptor.extend_from_slice(&0u32.to_be_bytes()); // status: success
            offset += actual;
            total_actual += actual;
        }
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 0,
            actual_length: total_actual,
            start_frame,
            number_of_packets: requested.len() as u32,
            error_count: 0,
            transfer_buffer,
            iso_packet_descriptor,
        }
    }

    /// Constructs a failed OP_REP_IMPORT response
    pub fn usbip_ret_submit_fail(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetSubmit {
            header: header.clone(),
            status: 1,
            actual_length: 0,
            start_frame: 0,
            number_of_packets: 0,
            error_count: 0,
            transfer_buffer: vec![],
            iso_packet_descriptor: vec![],
        }
    }

    /// Constructs a successful OP_REP_IMPORT response
    pub fn usbip_ret_unlink_success(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: 0,
        }
    }

    /// RET_UNLINK for a URB cancelled before it completed: `-ECONNRESET`, as `usbip-host` sends.
    pub fn usbip_ret_unlink_reset(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: -104i32 as u32, // -ECONNRESET
        }
    }

    /// Constructs a failed OP_REP_IMPORT response.
    pub fn usbip_ret_unlink_fail(header: &UsbIpHeaderBasic) -> Self {
        Self::UsbIpRetUnlink {
            header: header.clone(),
            status: 1,
        }
    }
}

// (Upstream's in-crate test module removed in the vendored copy — see NOTICE. What follows covers
// only the punktfunk isochronous addition.)
#[cfg(test)]
mod iso_tests {
    use super::*;

    fn header(direction: u32) -> UsbIpHeaderBasic {
        UsbIpHeaderBasic {
            command: USBIP_RET_SUBMIT.into(),
            seqnum: 42,
            devid: 0,
            direction,
            ep: 1,
        }
    }

    fn be(b: &[u8]) -> u32 {
        u32::from_be_bytes([b[0], b[1], b[2], b[3]])
    }

    /// An isochronous OUT reply must restate one 16-byte descriptor per packet with the host's
    /// `length` echoed and `actual_length` reporting that we consumed it all. vhci_hcd reads this
    /// table to complete the URB; a short or mis-sized table desynchronises the kernel's ISO ring
    /// and the audio stream xruns.
    #[test]
    fn iso_out_reply_acknowledges_every_packet() {
        let requested = [192u32, 192, 192];
        let replies = vec![Vec::new(), Vec::new(), Vec::new()];
        let bytes =
            UsbIpResponse::usbip_ret_submit_iso(&header(0), 7, &requested, &replies).to_bytes();

        // 48-byte fixed header: ... status@20, actual_length@24, start_frame@28,
        // number_of_packets@32, error_count@36.
        assert_eq!(be(&bytes[20..24]), 0, "status");
        // NOT zero, even though OUT sends no payload BACK: this field is the bytes the device
        // consumed. `usbip_recv_iso()` sums the table below and compares it against exactly this
        // value, raising ERROR_TCP / -EPIPE on a mismatch — which drops the whole connection, so
        // the pad disappears entirely rather than glitching. Field-observed 2026-08-17.
        assert_eq!(
            be(&bytes[24..28]),
            3 * 192,
            "bytes consumed, not bytes returned"
        );
        assert_eq!(be(&bytes[28..32]), 7, "start_frame echoed");
        assert_eq!(be(&bytes[32..36]), 3, "number_of_packets");
        assert_eq!(be(&bytes[36..40]), 0, "error_count");

        let table = &bytes[48..];
        assert_eq!(
            table.len(),
            3 * 16,
            "one 16-byte descriptor per packet, and no payload buffer on OUT"
        );
        for i in 0..3 {
            let d = &table[i * 16..i * 16 + 16];
            assert_eq!(be(&d[4..8]), 192, "packet {i} length echoed");
            assert_eq!(be(&d[8..12]), 192, "packet {i} fully consumed");
            assert_eq!(be(&d[12..16]), 0, "packet {i} status");
        }

        // The kernel's invariant itself, stated once: sum(table actual_length) == actual_length.
        let summed: u32 = table.chunks_exact(16).map(|d| be(&d[8..12])).sum();
        assert_eq!(
            summed,
            be(&bytes[24..28]),
            "usbip_recv_iso() sums the table against actual_length"
        );
    }

    /// An isochronous IN reply packs the per-packet payloads contiguously and states the offset
    /// each one starts at, so a short packet cannot make the host misread the packets after it.
    #[test]
    fn iso_in_reply_packs_payloads_at_the_offsets_it_states() {
        let requested = [4u32, 4, 4];
        let replies = vec![vec![1u8, 2, 3, 4], vec![9u8, 9], vec![5u8, 6, 7, 8]];
        let bytes =
            UsbIpResponse::usbip_ret_submit_iso(&header(1), 0, &requested, &replies).to_bytes();

        assert_eq!(be(&bytes[24..28]), 10, "4 + 2 + 4 bytes actually returned");
        let (table_at, payload) = (48 + 10, &bytes[48..58]);
        assert_eq!(payload, &[1, 2, 3, 4, 9, 9, 5, 6, 7, 8]);

        let table = &bytes[table_at..];
        let offs: Vec<u32> = (0..3).map(|i| be(&table[i * 16..i * 16 + 4])).collect();
        let acts: Vec<u32> = (0..3)
            .map(|i| be(&table[i * 16 + 8..i * 16 + 12]))
            .collect();
        assert_eq!(offs, vec![0, 4, 6], "offsets follow the short packet");
        assert_eq!(acts, vec![4, 2, 4], "actual lengths, short packet included");
    }

    /// A handler that returns more than the host asked for must be truncated, not allowed to
    /// overrun the host's buffer.
    #[test]
    fn iso_in_reply_truncates_an_overlong_payload() {
        let bytes = UsbIpResponse::usbip_ret_submit_iso(&header(1), 0, &[2], &[vec![1u8, 2, 3, 4]])
            .to_bytes();
        assert_eq!(be(&bytes[24..28]), 2);
        assert_eq!(&bytes[48..50], &[1, 2]);
        assert_eq!(be(&bytes[50 + 8..50 + 12]), 2, "actual_length clamped");
    }

    /// The exact 2026-08-18 field failure: an interrupt-OUT HID output report (a 48-byte
    /// DualSense report `0x02`) must be acknowledged as 48 bytes *accepted*, with no payload
    /// after the 48-byte header. `vhci_hcd` copies `actual_length` into the URB verbatim and
    /// `usbhid_output_report()` hands it back as `write()`'s return value — the old reply said 0,
    /// so every hidraw write on the pad "succeeded" with 0 bytes and winebus took it as failed.
    #[test]
    fn non_iso_out_reply_acknowledges_the_bytes_accepted_and_carries_no_payload() {
        let bytes = UsbIpResponse::usbip_ret_submit_out_success(&header(0), 48).to_bytes();
        assert_eq!(
            bytes.len(),
            48,
            "header only: the kernel reads no payload back on OUT"
        );
        assert_eq!(be(&bytes[20..24]), 0, "status");
        assert_eq!(be(&bytes[24..28]), 48, "bytes accepted, NOT bytes returned");
        assert_eq!(be(&bytes[32..36]), 0, "not isochronous");
    }

    /// The IN constructor keeps counting the payload it returns.
    #[test]
    fn non_iso_in_reply_counts_its_payload() {
        let bytes =
            UsbIpResponse::usbip_ret_submit_success(&header(1), 0, 0, vec![7u8; 41], vec![])
                .to_bytes();
        assert_eq!(bytes.len(), 48 + 41);
        assert_eq!(be(&bytes[24..28]), 41);
    }
}
