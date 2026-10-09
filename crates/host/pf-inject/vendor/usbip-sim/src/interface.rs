use super::*;

/// One **alternate setting** of a [`UsbInterface`] beyond the implicit `bAlternateSetting 0` the
/// interface's own fields describe.
///
/// punktfunk addition. The upstream crate emits exactly one interface descriptor per interface with
/// `bAlternateSetting` hardcoded to 0, which cannot express a USB Audio Class streaming interface:
/// UAC requires alt 0 to be the zero-bandwidth setting (no endpoints) and the *streaming* endpoint
/// to live on alt 1, so the host can release the bus bandwidth when the stream is idle. A virtual
/// DualSense has to present that shape verbatim or `snd-usb-audio` will not create a PCM for it.
///
/// Endpoints declared here are routed to the owning interface's handler exactly like alt-0
/// endpoints ([`UsbDevice::find_ep`](crate::UsbDevice) searches both), because a single handler
/// implements the whole interface across its settings.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct UsbAltSetting {
    /// `bAlternateSetting` — must be non-zero (0 is the interface's own descriptor).
    pub alternate_setting: u8,
    pub interface_class: u8,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    /// Class-specific descriptors emitted between this setting's interface and endpoint descriptors.
    pub class_specific_descriptor: Vec<u8>,
    pub endpoints: Vec<UsbEndpoint>,
    /// Bytes appended **inside** each endpoint descriptor, growing its `bLength` past the standard
    /// 7 — a UAC 1.0 isochronous endpoint carries `bRefresh` + `bSynchAddress` there, making it 9
    /// bytes. Indexed in lockstep with `endpoints`; a missing entry means the plain 7-byte form.
    pub endpoint_extra: Vec<Vec<u8>>,
    /// Whole class-specific descriptors emitted **after** each endpoint descriptor — the UAC 1.0
    /// `AS_ENDPOINT` (`CS_ENDPOINT`/`EP_GENERAL`) descriptor. Indexed like `endpoint_extra`.
    pub endpoint_trailers: Vec<Vec<u8>>,
}

/// Represent a USB interface
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct UsbInterface {
    pub interface_class: u8,
    pub interface_subclass: u8,
    pub interface_protocol: u8,
    pub endpoints: Vec<UsbEndpoint>,
    pub string_interface: u8,
    pub class_specific_descriptor: Vec<u8>,
    /// Alternate settings 1.. (punktfunk addition; empty for a single-setting interface, which is
    /// the upstream behaviour).
    pub alt_settings: Vec<UsbAltSetting>,

    #[cfg_attr(feature = "serde", serde(skip))]
    pub handler: Arc<Mutex<Box<dyn UsbInterfaceHandler + Send>>>,
}

/// One packet of an isochronous URB, as described by the USB/IP `iso_packet_descriptor` array
/// (punktfunk addition — see [`UsbInterfaceHandler::handle_iso_urb`]).
///
/// The kernel submits an ISO URB covering several service intervals at once (a 1 ms-per-packet
/// audio endpoint typically arrives 8 packets at a time), so a handler must see the packet
/// boundaries rather than one flat buffer: each packet is an independent frame whose length may
/// differ, and the reply reports an `actual_length` per packet.
#[derive(Debug, Clone, Copy)]
pub struct IsoPacket<'a> {
    /// This packet's payload, sliced out of the URB transfer buffer. Empty for an IN endpoint.
    pub data: &'a [u8],
    /// The `length` field from the descriptor — how many bytes the host offered (OUT) or expects
    /// at most (IN).
    pub requested_len: usize,
}

/// A handler of a custom usb interface
pub trait UsbInterfaceHandler: std::fmt::Debug {
    /// Return the class specific descriptor which is inserted between interface descriptor and endpoint descriptor
    fn get_class_specific_descriptor(&self) -> Vec<u8>;

    /// Handle a URB(USB Request Block) targeting at this interface
    ///
    /// Can be one of: control transfer to ep0 or other types of transfer to its endpoint.
    /// The resulting data should not exceed `transfer_buffer_length`.
    fn handle_urb(
        &mut self,
        interface: &UsbInterface,
        ep: UsbEndpoint,
        transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> Result<Vec<u8>>;

    /// Handle an **isochronous** URB — one transfer carrying `packets.len()` service-interval
    /// packets (punktfunk addition; ISO is how USB audio moves samples, and the upstream crate
    /// dropped `number_of_packets`/`iso_packet_descriptor` on the floor, which stalls any ISO
    /// endpoint).
    ///
    /// For an OUT endpoint each [`IsoPacket::data`] is one service interval's payload; the reply's
    /// `transfer_buffer` must be empty. For an IN endpoint `data` is empty and the handler returns
    /// one payload per packet. The default implementation **accepts and discards** OUT packets and
    /// returns silence for IN, which keeps a declared-but-unused ISO endpoint streaming rather than
    /// erroring the URB.
    fn handle_iso_urb(
        &mut self,
        _interface: &UsbInterface,
        ep: UsbEndpoint,
        packets: &[IsoPacket<'_>],
    ) -> Result<Vec<Vec<u8>>> {
        Ok(match ep.direction() {
            Direction::In => packets.iter().map(|p| vec![0u8; p.requested_len]).collect(),
            Direction::Out => vec![Vec::new(); packets.len()],
        })
    }

    /// Helper to downcast to actual struct
    ///
    /// Please implement it as:
    /// ```ignore
    /// fn as_any(&mut self) -> &mut dyn Any {
    ///     self
    /// }
    /// ```
    fn as_any(&mut self) -> &mut dyn Any;
}
