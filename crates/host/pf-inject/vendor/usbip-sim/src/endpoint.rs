use super::*;

/// Represent a USB endpoint
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct UsbEndpoint {
    /// bEndpointAddress
    pub address: u8,
    /// bmAttributes
    pub attributes: u8,
    /// wMaxPacketSize
    pub max_packet_size: u16,
    /// bInterval
    pub interval: u8,
}

impl UsbEndpoint {
    /// Get direction from MSB of address
    pub fn direction(&self) -> Direction {
        if self.address & 0x80 != 0 {
            Direction::In
        } else {
            Direction::Out
        }
    }

    /// Whether this is endpoint zero
    pub fn is_ep0(&self) -> bool {
        self.address & 0x7F == 0
    }

    /// The endpoint's **transfer type**, which is only bits 1..0 of `bmAttributes`.
    ///
    /// punktfunk fix: the rest of the byte is meaningful — for an isochronous endpoint bits 3..2 are
    /// the synchronisation type and bits 5..4 the usage type (USB 2.0 §9.6.6). A real UAC endpoint
    /// therefore reads `0x05` (async data) or `0x09` (adaptive data), and matching the *whole* byte
    /// against [`EndpointAttributes`] yields `None` for both — which used to fall through to
    /// `unimplemented!()` and panic the connection task. Every endpoint the crate shipped with was
    /// a plain `0x03` interrupt, so nothing noticed until an audio endpoint arrived.
    pub fn transfer_type(&self) -> Option<EndpointAttributes> {
        num_traits::FromPrimitive::from_u8(self.attributes & 0x03)
    }
}
