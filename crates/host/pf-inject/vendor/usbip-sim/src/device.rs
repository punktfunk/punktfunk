use super::*;

#[derive(Clone, Default, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Version {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

// (Upstream's `From<rusb::Version>` conversions removed — this crate has no libusb dependency.)

/// bcdDevice
impl From<u16> for Version {
    fn from(value: u16) -> Self {
        Self {
            major: (value >> 8) as u8,
            minor: ((value >> 4) & 0xF) as u8,
            patch: (value & 0xF) as u8,
        }
    }
}

/// Extra descriptors emitted between the configuration descriptor and interface 0 (for example,
/// an Interface Association Descriptor for a CDC function).
pub type ConfigurationDescriptorPrefix = Vec<u8>;

/// Represent a USB device
#[derive(Clone, Default, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct UsbDevice {
    pub path: String,
    pub bus_id: String,
    pub bus_num: u32,
    pub dev_num: u32,
    pub speed: u32,
    pub vendor_id: u16,
    pub product_id: u16,
    pub device_bcd: Version,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub configuration_attributes: u8,
    pub configuration_max_power: u8,
    pub configuration_descriptor_prefix: ConfigurationDescriptorPrefix,
    /// Optional complete BOS descriptor. `None` uses the simulator's minimal empty BOS.
    pub bos_descriptor: Option<Vec<u8>>,
    pub num_configurations: u8,
    pub interfaces: Vec<UsbInterface>,

    #[cfg_attr(feature = "serde", serde(skip))]
    pub device_handler: Option<Arc<Mutex<Box<dyn UsbDeviceHandler + Send>>>>,

    /// Per-endpoint completion deadlines (punktfunk addition) — the absolute-time ledger
    /// [`next_deadline`](Self::next_deadline) keeps for isochronous URBs and paced interrupt IN.
    /// Keyed by endpoint address. Shared across clones because the clones all present the same
    /// device: whoever services the endpoint advances the one clock.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub(crate) pace_deadlines: Arc<Mutex<HashMap<u8, tokio::time::Instant>>>,

    /// Pace interrupt-IN polls on that ledger too (punktfunk addition). Off, a poll sleeps
    /// `bInterval` from its submission and timer slop stretches the period: 4 ms serves near 5.
    pub absolute_interrupt_pacing: bool,

    pub usb_version: Version,

    pub(crate) ep0_in: UsbEndpoint,
    pub(crate) ep0_out: UsbEndpoint,
    // strings
    pub(crate) string_pool: HashMap<u8, String>,
    pub(crate) string_configuration: u8,
    pub(crate) string_manufacturer: u8,
    pub(crate) string_product: u8,
    pub(crate) string_serial: u8,
}

impl UsbDevice {
    pub fn new(index: u32) -> Self {
        let mut res = Self {
            path: "/sys/bus/0/0/0".to_string(),
            bus_id: "0-0-0".to_string(),
            dev_num: index,
            speed: UsbSpeed::High as u32,
            ep0_in: UsbEndpoint {
                address: 0x80,
                attributes: EndpointAttributes::Control as u8,
                max_packet_size: EP0_MAX_PACKET_SIZE,
                interval: 0,
            },
            ep0_out: UsbEndpoint {
                address: 0x00,
                attributes: EndpointAttributes::Control as u8,
                max_packet_size: EP0_MAX_PACKET_SIZE,
                interval: 0,
            },
            // configured, bus-powered at 100 mA by default
            configuration_value: 1,
            configuration_attributes: 0x80,
            configuration_max_power: 0x32,
            num_configurations: 1,
            ..Self::default()
        };
        res.string_configuration = res.new_string("Default Configuration");
        res.string_manufacturer = res.new_string("Manufacturer");
        res.string_product = res.new_string("Product");
        res.string_serial = res.new_string("Serial");
        res
    }

    /// Returns the old value, if present.
    pub fn set_configuration_name(&mut self, name: &str) -> Option<String> {
        let old = (self.string_configuration != 0)
            .then(|| self.string_pool.remove(&self.string_configuration))
            .flatten();
        self.string_configuration = self.new_string(name);
        old
    }

    /// Unset configuration name and returns the old value, if present.
    pub fn unset_configuration_name(&mut self) -> Option<String> {
        let old = (self.string_configuration != 0)
            .then(|| self.string_pool.remove(&self.string_configuration))
            .flatten();
        self.string_configuration = 0;
        old
    }

    /// Returns the old value, if present.
    pub fn set_serial_number(&mut self, name: &str) -> Option<String> {
        let old = (self.string_serial != 0)
            .then(|| self.string_pool.remove(&self.string_serial))
            .flatten();
        self.string_serial = self.new_string(name);
        old
    }

    /// Unset serial number and returns the old value, if present.
    pub fn unset_serial_number(&mut self) -> Option<String> {
        let old = (self.string_serial != 0)
            .then(|| self.string_pool.remove(&self.string_serial))
            .flatten();
        self.string_serial = 0;
        old
    }

    /// Returns the old value, if present.
    pub fn set_product_name(&mut self, name: &str) -> Option<String> {
        let old = (self.string_product != 0)
            .then(|| self.string_pool.remove(&self.string_product))
            .flatten();
        self.string_product = self.new_string(name);
        old
    }

    /// Unset product name and returns the old value, if present.
    pub fn unset_product_name(&mut self) -> Option<String> {
        let old = (self.string_product != 0)
            .then(|| self.string_pool.remove(&self.string_product))
            .flatten();
        self.string_product = 0;
        old
    }

    /// Returns the old value, if present.
    pub fn set_manufacturer_name(&mut self, name: &str) -> Option<String> {
        let old = (self.string_manufacturer != 0)
            .then(|| self.string_pool.remove(&self.string_manufacturer))
            .flatten();
        self.string_manufacturer = self.new_string(name);
        old
    }

    /// Unset manufacturer name and returns the old value, if present.
    pub fn unset_manufacturer_name(&mut self) -> Option<String> {
        let old = (self.string_manufacturer != 0)
            .then(|| self.string_pool.remove(&self.string_manufacturer))
            .flatten();
        self.string_manufacturer = 0;
        old
    }

    pub fn with_interface(
        mut self,
        interface_class: u8,
        interface_subclass: u8,
        interface_protocol: u8,
        name: Option<&str>,
        endpoints: Vec<UsbEndpoint>,
        handler: Arc<Mutex<Box<dyn UsbInterfaceHandler + Send>>>,
    ) -> Self {
        let string_interface = name.map(|name| self.new_string(name)).unwrap_or(0);
        let class_specific_descriptor = handler.lock().unwrap().get_class_specific_descriptor();
        self.interfaces.push(UsbInterface {
            interface_class,
            interface_subclass,
            interface_protocol,
            endpoints,
            string_interface,
            class_specific_descriptor,
            alt_settings: Vec::new(),
            handler,
        });
        self
    }

    /// Attach alternate settings to the interface added most recently by [`with_interface`]
    /// (punktfunk addition — see [`UsbAltSetting`]). Chained directly after the `with_interface`
    /// that created the alt-0 setting.
    ///
    /// # Panics
    /// If no interface has been added yet, or if any setting uses `alternate_setting == 0` (that
    /// number belongs to the interface's own descriptor).
    pub fn with_alt_settings(mut self, alts: Vec<UsbAltSetting>) -> Self {
        assert!(
            alts.iter().all(|a| a.alternate_setting != 0),
            "alternate_setting 0 is the interface's own descriptor"
        );
        self.interfaces
            .last_mut()
            .expect("with_alt_settings called before with_interface")
            .alt_settings = alts;
        self
    }

    pub fn with_device_handler(
        mut self,
        handler: Arc<Mutex<Box<dyn UsbDeviceHandler + Send>>>,
    ) -> Self {
        self.device_handler = Some(handler);
        self
    }

    pub(crate) fn new_string(&mut self, s: &str) -> u8 {
        for i in 1.. {
            if let std::collections::hash_map::Entry::Vacant(e) = self.string_pool.entry(i) {
                e.insert(s.to_string());
                return i;
            }
        }
        panic!("string poll exhausted")
    }

    pub(crate) fn find_ep(&self, ep: u8) -> Option<(UsbEndpoint, Option<&UsbInterface>)> {
        if ep == self.ep0_in.address {
            Some((self.ep0_in, None))
        } else if ep == self.ep0_out.address {
            Some((self.ep0_out, None))
        } else {
            for intf in &self.interfaces {
                // Alt-setting endpoints route to the same handler as alt 0 (punktfunk addition):
                // one handler implements the whole interface across its settings, and the kernel
                // only ever drives the endpoints of the setting it selected.
                let alt_eps = intf.alt_settings.iter().flat_map(|a| a.endpoints.iter());
                for endpoint in intf.endpoints.iter().chain(alt_eps) {
                    if endpoint.address == ep {
                        return Some((*endpoint, Some(intf)));
                    }
                }
            }
            None
        }
    }

    pub(crate) fn to_bytes(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(312);

        let mut path = self.path.as_bytes().to_vec();
        debug_assert!(path.len() <= 256);
        path.resize(256, 0);
        result.extend_from_slice(path.as_slice());

        let mut bus_id = self.bus_id.as_bytes().to_vec();
        debug_assert!(bus_id.len() <= 32);
        bus_id.resize(32, 0);
        result.extend_from_slice(bus_id.as_slice());

        result.extend_from_slice(&self.bus_num.to_be_bytes());
        result.extend_from_slice(&self.dev_num.to_be_bytes());
        result.extend_from_slice(&self.speed.to_be_bytes());
        result.extend_from_slice(&self.vendor_id.to_be_bytes());
        result.extend_from_slice(&self.product_id.to_be_bytes());
        result.push(self.device_bcd.major);
        result.push(self.device_bcd.minor);
        result.push(self.device_class);
        result.push(self.device_subclass);
        result.push(self.device_protocol);
        result.push(self.configuration_value);
        result.push(self.num_configurations);
        result.push(self.interfaces.len() as u8);

        result
    }

    pub(crate) fn to_bytes_with_interfaces(&self) -> Vec<u8> {
        let mut result = self.to_bytes();
        result.reserve(4 * self.interfaces.len());

        for intf in &self.interfaces {
            result.push(intf.interface_class);
            result.push(intf.interface_subclass);
            result.push(intf.interface_protocol);
            result.push(0); // padding
        }

        result
    }

    /// The service interval of `ep`: one packet's worth of time, and the interrupt-IN poll period.
    /// High/Super speed express `bInterval` as `2^(n-1)` 125 µs microframes; full/low speed as
    /// whole milliseconds.
    pub(crate) fn service_interval(&self, ep: UsbEndpoint) -> std::time::Duration {
        if self.speed == UsbSpeed::High as u32
            || self.speed == UsbSpeed::Super as u32
            || self.speed == UsbSpeed::SuperPlus as u32
        {
            let n = ep.interval.clamp(1, 16) as u32;
            std::time::Duration::from_micros((1u64 << (n - 1)) * 125)
        } else {
            std::time::Duration::from_millis(ep.interval.max(1) as u64)
        }
    }

    /// Dispatch an **isochronous** URB to the owning interface's handler (punktfunk addition).
    ///
    /// Returns one payload per packet: empty vectors for an OUT endpoint (the host wants only the
    /// per-packet `actual_length` back), the sampled data for an IN endpoint.
    ///
    /// **Paced by `bInterval` × the packet count, and that pacing is the device's audio clock.**
    /// Isochronous endpoints move exactly one packet per service interval on real hardware, and
    /// `snd-usb-audio` advances its PCM pointer from URB *completions* — it has no other time
    /// reference. `vhci_hcd` does not throttle the server side (the same reason `handler` paces
    /// interrupt IN), so completing instantly would both spin the loopback link and tell the
    /// kernel the device consumed a whole URB's worth of samples in no time, running the stream's
    /// clock away and xrunning it continuously.
    ///
    /// **Paced against an absolute per-endpoint deadline, not relative sleeps.** A plain
    /// `sleep(interval × packets)` per URB adds every source of slop — tokio timer granularity,
    /// socket I/O, handler lock waits — ON TOP of the nominal period, so the device's clock runs
    /// systematically slow (measured ~26 % slow on a busy graph, 2026-08-18: `hw_ptr` advanced
    /// ~35.7 k frames/s against a 48 kHz stream — the PCM backs up, latency grows into xruns,
    /// and anything clocked off this device drags). The ledger makes late completions *catch up*:
    /// each URB advances the endpoint's deadline by exactly its nominal duration and sleeps until
    /// that absolute instant, so overhead eats into the next sleep instead of accumulating. If
    /// the stream stalls long enough that the ledger is far behind (stop/start, unlink storm),
    /// it re-anchors to now rather than fast-forwarding a burst of instant completions.
    pub(crate) async fn handle_iso_urb(
        &self,
        ep: UsbEndpoint,
        intf: Option<&UsbInterface>,
        packets: &[IsoPacket<'_>],
    ) -> Result<Vec<Vec<u8>>> {
        let Some(intf) = intf else {
            // ISO on ep0 is not a thing; treat it as an unsupported transfer rather than panicking.
            return Err(std::io::Error::other("isochronous transfer to ep0"));
        };
        let step = self.service_interval(ep) * packets.len() as u32;
        tokio::time::sleep_until(self.next_deadline(ep, step)).await;
        let mut handler = intf.handler.lock().unwrap();
        handler.handle_iso_urb(intf, ep, packets)
    }

    /// When `ep`'s next transfer of `step` completes: its ledger entry advanced by exactly `step`.
    /// Overhead eats into the next sleep instead of accumulating. A ledger further behind than
    /// two frames of slack means a stalled stream, which re-anchors to now rather than bursting.
    pub(crate) fn next_deadline(
        &self,
        ep: UsbEndpoint,
        step: std::time::Duration,
    ) -> tokio::time::Instant {
        const RESYNC_SLACK: std::time::Duration = std::time::Duration::from_millis(20);
        let mut ledger = self.pace_deadlines.lock().unwrap();
        let now = tokio::time::Instant::now();
        let due = ledger.entry(ep.address).or_insert(now);
        if *due + RESYNC_SLACK < now {
            *due = now;
        }
        *due += step;
        *due
    }

    pub(crate) async fn handle_urb(
        &self,
        ep: UsbEndpoint,
        intf: Option<&UsbInterface>,
        transfer_buffer_length: u32,
        setup_packet: SetupPacket,
        out_data: &[u8],
    ) -> Result<Vec<u8>> {
        use DescriptorType::*;
        use Direction::*;
        use EndpointAttributes::*;
        use StandardRequest::*;

        // Only bits 1..0 of bmAttributes are the transfer type — see `UsbEndpoint::transfer_type`.
        match (ep.transfer_type(), ep.direction()) {
            (Some(Control), In) => {
                // control in
                debug!("Control IN setup={setup_packet:x?}");
                match (
                    setup_packet.request_type,
                    FromPrimitive::from_u8(setup_packet.request),
                ) {
                    (0b10000000, Some(GetDescriptor)) => {
                        // high byte: type
                        match FromPrimitive::from_u16(setup_packet.value >> 8) {
                            Some(Device) => {
                                debug!("Get device descriptor");
                                // Standard Device Descriptor
                                let mut desc = vec![
                                    0x12,         // bLength
                                    Device as u8, // bDescriptorType: Device
                                    (self.usb_version.minor << 4) | self.usb_version.patch,
                                    self.usb_version.major, // bcdUSB
                                    self.device_class,      // bDeviceClass
                                    self.device_subclass,   // bDeviceSubClass
                                    self.device_protocol,   // bDeviceProtocol
                                    self.ep0_in.max_packet_size as u8, // bMaxPacketSize0
                                    self.vendor_id as u8,   // idVendor
                                    (self.vendor_id >> 8) as u8,
                                    self.product_id as u8, // idProduct
                                    (self.product_id >> 8) as u8,
                                    (self.device_bcd.minor << 4) | self.device_bcd.patch,
                                    self.device_bcd.major,    // bcdDevice
                                    self.string_manufacturer, // iManufacturer
                                    self.string_product,      // iProduct
                                    self.string_serial,       // iSerial
                                    self.num_configurations,  // bNumConfigurations
                                ];

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            Some(BOS) => {
                                debug!("Get BOS descriptor");
                                let mut desc = self.bos_descriptor.clone().unwrap_or_else(|| {
                                    vec![
                                        0x05,      // bLength
                                        BOS as u8, // bDescriptorType: BOS
                                        0x05, 0x00, // wTotalLength
                                        0x00, // bNumCapabilities
                                    ]
                                });

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            Some(Configuration) => {
                                debug!("Get configuration descriptor");
                                // Standard Configuration Descriptor
                                let mut desc = vec![
                                    0x09,                // bLength
                                    Configuration as u8, // bDescriptorType: Configuration
                                    0x00,
                                    0x00, // wTotalLength: to be filled below
                                    self.interfaces.len() as u8, // bNumInterfaces
                                    self.configuration_value, // bConfigurationValue
                                    self.string_configuration, // iConfiguration
                                    self.configuration_attributes, // bmAttributes
                                    self.configuration_max_power, // bMaxPower (2 mA units)
                                ];
                                desc.extend_from_slice(&self.configuration_descriptor_prefix);
                                for (i, intf) in self.interfaces.iter().enumerate() {
                                    let mut intf_desc = vec![
                                        0x09,                       // bLength
                                        Interface as u8,            // bDescriptorType: Interface
                                        i as u8,                    // bInterfaceNum
                                        0x00,                       // bAlternateSettings
                                        intf.endpoints.len() as u8, // bNumEndpoints
                                        intf.interface_class,       // bInterfaceClass
                                        intf.interface_subclass,    // bInterfaceSubClass
                                        intf.interface_protocol,    // bInterfaceProtocol
                                        intf.string_interface,      //iInterface
                                    ];
                                    // class specific endpoint
                                    let mut specific = intf.class_specific_descriptor.clone();
                                    intf_desc.append(&mut specific);
                                    // endpoint descriptors
                                    for endpoint in &intf.endpoints {
                                        intf_desc.append(&mut endpoint_descriptor(endpoint, &[]));
                                    }
                                    desc.append(&mut intf_desc);

                                    // Alternate settings 1.. (punktfunk addition): another full
                                    // interface descriptor per setting, same bInterfaceNumber.
                                    for alt in &intf.alt_settings {
                                        let mut alt_desc = vec![
                                            0x09,                      // bLength
                                            Interface as u8,           // bDescriptorType
                                            i as u8,                   // bInterfaceNumber
                                            alt.alternate_setting,     // bAlternateSetting
                                            alt.endpoints.len() as u8, // bNumEndpoints
                                            alt.interface_class,
                                            alt.interface_subclass,
                                            alt.interface_protocol,
                                            intf.string_interface, // iInterface
                                        ];
                                        alt_desc.extend_from_slice(&alt.class_specific_descriptor);
                                        for (n, endpoint) in alt.endpoints.iter().enumerate() {
                                            let extra = alt
                                                .endpoint_extra
                                                .get(n)
                                                .map(Vec::as_slice)
                                                .unwrap_or(&[]);
                                            alt_desc
                                                .append(&mut endpoint_descriptor(endpoint, extra));
                                            if let Some(t) = alt.endpoint_trailers.get(n) {
                                                alt_desc.extend_from_slice(t);
                                            }
                                        }
                                        desc.append(&mut alt_desc);
                                    }
                                }
                                // length
                                let len = desc.len() as u16;
                                desc[2] = len as u8;
                                desc[3] = (len >> 8) as u8;

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            Some(String) => {
                                debug!("Get string descriptor");
                                let index = setup_packet.value as u8;
                                if index == 0 {
                                    // String Descriptor Zero, Specifying Languages Supported by the Device
                                    // language ids
                                    let mut desc = vec![
                                        4,                            // bLength
                                        DescriptorType::String as u8, // bDescriptorType
                                        0x09,
                                        0x04, // wLANGID[0], en-US
                                    ];
                                    // requested len too short: wLength < real length
                                    if setup_packet.length < desc.len() as u16 {
                                        desc.resize(setup_packet.length as usize, 0);
                                    }
                                    Ok(desc)
                                } else if let Some(s) = &self.string_pool.get(&index) {
                                    // UNICODE String Descriptor
                                    let bytes: Vec<u16> = s.encode_utf16().collect();
                                    let mut desc = vec![
                                        2 + bytes.len() as u8 * 2,    // bLength
                                        DescriptorType::String as u8, // bDescriptorType
                                    ];
                                    for byte in bytes {
                                        desc.push(byte as u8);
                                        desc.push((byte >> 8) as u8);
                                    }

                                    // requested len too short: wLength < real length
                                    if setup_packet.length < desc.len() as u16 {
                                        desc.resize(setup_packet.length as usize, 0);
                                    }
                                    Ok(desc)
                                } else {
                                    Err(std::io::Error::new(
                                        std::io::ErrorKind::InvalidInput,
                                        format!("Invalid string index: {index}"),
                                    ))
                                }
                            }
                            Some(DeviceQualifier) => {
                                debug!("Get device qualifier descriptor");
                                // Device_Qualifier Descriptor
                                let mut desc = vec![
                                    0x0A,                  // bLength
                                    DeviceQualifier as u8, // bDescriptorType: Device Qualifier
                                    self.usb_version.minor,
                                    self.usb_version.major, // bcdUSB
                                    self.device_class,      // bDeviceClass
                                    self.device_subclass,   // bDeviceSUbClass
                                    self.device_protocol,   // bDeviceProtocol
                                    self.ep0_in.max_packet_size as u8, // bMaxPacketSize0
                                    self.num_configurations, // bNumConfigurations
                                    0x00,                   // bReserved
                                ];

                                // requested len too short: wLength < real length
                                if setup_packet.length < desc.len() as u16 {
                                    desc.resize(setup_packet.length as usize, 0);
                                }
                                Ok(desc)
                            }
                            _ => {
                                warn!("unknown desc type: {setup_packet:x?}");
                                Ok(vec![])
                            }
                        }
                    }
                    _ if setup_packet.request_type & 0xF == 1 => {
                        // to interface
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        // only low 8 bits are valid
                        let intf = &self.interfaces[setup_packet.index as usize & 0xFF];
                        let mut handler = intf.handler.lock().unwrap();
                        handler.handle_urb(intf, ep, transfer_buffer_length, setup_packet, out_data)
                    }
                    _ if setup_packet.request_type & 0xF == 0 && self.device_handler.is_some() => {
                        // to device
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        let lock = self.device_handler.as_ref().unwrap();
                        let mut handler = lock.lock().unwrap();
                        handler.handle_urb(transfer_buffer_length, setup_packet, out_data)
                    }
                    _ => unimplemented!("control in"),
                }
            }
            (Some(Control), Out) => {
                // control out
                debug!("Control OUT setup={setup_packet:x?}");
                match (
                    setup_packet.request_type,
                    FromPrimitive::from_u8(setup_packet.request),
                ) {
                    (0b00000000, Some(SetConfiguration)) => {
                        let mut desc = vec![
                            self.configuration_value, // bConfigurationValue
                        ];

                        // requested len too short: wLength < real length
                        if setup_packet.length < desc.len() as u16 {
                            desc.resize(setup_packet.length as usize, 0);
                        }
                        Ok(desc)
                    }
                    _ if setup_packet.request_type & 0xF == 1 => {
                        // to interface
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        // only low 8 bits are valid
                        let intf = &self.interfaces[setup_packet.index as usize & 0xFF];
                        let mut handler = intf.handler.lock().unwrap();
                        handler.handle_urb(intf, ep, transfer_buffer_length, setup_packet, out_data)
                    }
                    _ if setup_packet.request_type & 0xF == 0 && self.device_handler.is_some() => {
                        // to device
                        // see https://www.beyondlogic.org/usbnutshell/usb6.shtml
                        let lock = self.device_handler.as_ref().unwrap();
                        let mut handler = lock.lock().unwrap();
                        handler.handle_urb(transfer_buffer_length, setup_packet, out_data)
                    }
                    _ => unimplemented!("control out"),
                }
            }
            (Some(_), _) => {
                // Interrupt / bulk OUT. `handler` paces and dispatches IN itself (punktfunk
                // modification), so a poll never holds up the connection.
                let intf = intf.unwrap();
                let mut handler = intf.handler.lock().unwrap();
                handler.handle_urb(intf, ep, transfer_buffer_length, setup_packet, out_data)
            }
            _ => unimplemented!("transfer to {:?}", ep),
        }
    }
}

/// Serialize one standard endpoint descriptor. `extra` is appended inside the descriptor and grows
/// `bLength` past 7 — UAC 1.0 isochronous endpoints carry `bRefresh` + `bSynchAddress` that way
/// (punktfunk addition; upstream only ever emitted the 7-byte form).
fn endpoint_descriptor(endpoint: &UsbEndpoint, extra: &[u8]) -> Vec<u8> {
    let mut d = vec![
        (7 + extra.len()) as u8,               // bLength
        DescriptorType::Endpoint as u8,        // bDescriptorType
        endpoint.address,                      // bEndpointAddress
        endpoint.attributes,                   // bmAttributes
        endpoint.max_packet_size as u8,        // wMaxPacketSize (lo)
        (endpoint.max_packet_size >> 8) as u8, // wMaxPacketSize (hi)
        endpoint.interval,                     // bInterval
    ];
    d.extend_from_slice(extra);
    d
}

/// A handler for URB targeting the device
pub trait UsbDeviceHandler: std::fmt::Debug {
    /// Handle a URB(USB Request Block) targeting at this device
    ///
    /// When the lower 4 bits of `bmRequestType` is zero and the URB is not handled by the library, this function is called.
    /// The resulting data should not exceed `transfer_buffer_length`
    fn handle_urb(
        &mut self,
        transfer_buffer_length: u32,
        setup: SetupPacket,
        req: &[u8],
    ) -> Result<Vec<u8>>;

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

// (In-crate test module removed in the vendored copy — see NOTICE.)

#[cfg(test)]
mod pacing_tests {
    use super::*;

    fn dev(speed: UsbSpeed) -> UsbDevice {
        let mut d = UsbDevice::new(0);
        d.speed = speed as u32;
        d
    }

    fn iso_ep(interval: u8) -> UsbEndpoint {
        UsbEndpoint {
            address: 0x01,
            attributes: EndpointAttributes::Isochronous as u8,
            max_packet_size: 392,
            interval,
        }
    }

    /// At high speed `bInterval` counts 125 µs microframes as `2^(n-1)`, so the audio endpoints'
    /// `bInterval 4` must come out as exactly one millisecond — the rate that makes a 48 kHz
    /// stream advance 48 frames per packet. Getting this wrong retunes the device's sample clock.
    #[test]
    fn high_speed_interval_4_is_one_millisecond() {
        assert_eq!(
            dev(UsbSpeed::High).service_interval(iso_ep(4)),
            std::time::Duration::from_millis(1)
        );
        assert_eq!(
            dev(UsbSpeed::High).service_interval(iso_ep(1)),
            std::time::Duration::from_micros(125)
        );
        assert_eq!(
            dev(UsbSpeed::High).service_interval(iso_ep(6)),
            std::time::Duration::from_millis(4)
        );
    }

    /// A no-op ISO handler so the pacing tests can drive `handle_iso_urb` without a device model.
    #[derive(Debug)]
    struct NullIso;
    impl crate::UsbInterfaceHandler for NullIso {
        fn handle_urb(
            &mut self,
            _interface: &UsbInterface,
            _ep: UsbEndpoint,
            _transfer_buffer_length: u32,
            _setup: crate::SetupPacket,
            _req: &[u8],
        ) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn handle_iso_urb(
            &mut self,
            _interface: &UsbInterface,
            _ep: UsbEndpoint,
            packets: &[IsoPacket<'_>],
        ) -> Result<Vec<Vec<u8>>> {
            Ok(vec![Vec::new(); packets.len()])
        }
        fn get_class_specific_descriptor(&self) -> Vec<u8> {
            Vec::new()
        }
        fn as_any(&mut self) -> &mut dyn std::any::Any {
            self
        }
    }

    fn null_intf() -> UsbInterface {
        UsbInterface {
            interface_class: 1,
            interface_subclass: 2,
            interface_protocol: 0,
            endpoints: vec![iso_ep(4)],
            string_interface: 0,
            class_specific_descriptor: Vec::new(),
            alt_settings: Vec::new(),
            handler: Arc::new(Mutex::new(
                Box::new(NullIso) as Box<dyn crate::UsbInterfaceHandler + Send>
            )),
        }
    }

    /// The completion pace must hold the NOMINAL rate over many URBs — a relative
    /// `sleep(interval × packets)` per URB adds scheduling overhead on top of every period and
    /// the device's audio clock runs measurably slow (~26 % on a busy graph, field 2026-08-18).
    /// Under tokio's paused clock the ledger's `sleep_until` deadlines auto-advance with zero
    /// slop, so 50 URBs × 8 packets × 1 ms must take exactly 400 ms of virtual time — and the
    /// deadline arithmetic (not per-call `now()`) is what guarantees the same under real slop.
    #[tokio::test(start_paused = true)]
    async fn iso_pacing_holds_the_nominal_rate_across_urbs() {
        let d = dev(UsbSpeed::High);
        let intf = null_intf();
        let buf = [0u8; 392];
        let start = tokio::time::Instant::now();
        for _ in 0..50 {
            let packets: Vec<IsoPacket<'_>> = (0..8)
                .map(|_| IsoPacket {
                    data: &buf,
                    requested_len: 392,
                })
                .collect();
            d.handle_iso_urb(iso_ep(4), Some(&intf), &packets)
                .await
                .expect("iso urb");
        }
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_millis(400),
            "50 URBs × 8 packets × 1 ms must complete in exactly their nominal duration"
        );
    }

    /// After a stall longer than the resync slack, the ledger re-anchors to now instead of
    /// fast-forwarding a burst of instant completions through the stale backlog.
    #[tokio::test(start_paused = true)]
    async fn iso_pacing_reanchors_after_a_stall() {
        let d = dev(UsbSpeed::High);
        let intf = null_intf();
        let buf = [0u8; 392];
        let one = |d: &UsbDevice, intf: &UsbInterface| {
            let packets = vec![IsoPacket {
                data: &buf,
                requested_len: 392,
            }];
            let d = d.clone();
            let intf = intf.clone();
            async move { d.handle_iso_urb(iso_ep(4), Some(&intf), &packets).await }
        };
        one(&d, &intf).await.expect("prime the ledger");
        // Stall well past the slack, then resume: the next URB must take ~its nominal 1 ms from
        // NOW, not complete instantly against the stale deadline.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let start = tokio::time::Instant::now();
        one(&d, &intf).await.expect("resumed urb");
        assert_eq!(start.elapsed(), std::time::Duration::from_millis(1));
    }

    /// `bmAttributes` carries the synchronisation and usage type above the transfer type, so a real
    /// UAC endpoint is `0x05`/`0x09` rather than a bare `0x01`. Decoding the whole byte returns
    /// `None` for those and used to reach `unimplemented!()`; only bits 1..0 may be decoded.
    #[test]
    fn transfer_type_ignores_the_sync_and_usage_bits() {
        let iso = |attrs| {
            UsbEndpoint {
                address: 0x01,
                attributes: attrs,
                max_packet_size: 392,
                interval: 4,
            }
            .transfer_type()
        };
        // 0x09 = isochronous + adaptive data (the DualSense's speaker/haptic endpoint),
        // 0x05 = isochronous + asynchronous data (its microphone endpoint).
        assert!(matches!(iso(0x09), Some(EndpointAttributes::Isochronous)));
        assert!(matches!(iso(0x05), Some(EndpointAttributes::Isochronous)));
        assert!(matches!(iso(0x01), Some(EndpointAttributes::Isochronous)));
        // The plain forms every pre-existing device used must keep decoding as before.
        assert!(matches!(iso(0x03), Some(EndpointAttributes::Interrupt)));
        assert!(matches!(iso(0x02), Some(EndpointAttributes::Bulk)));
        assert!(matches!(iso(0x00), Some(EndpointAttributes::Control)));
    }

    /// Full speed states `bInterval` in whole milliseconds instead, and 0 must not mean "no wait"
    /// (that would free-run the link).
    /// A paced interrupt poll shares that ledger: a late wake-up shortens the next sleep instead
    /// of stretching the period.
    #[tokio::test(start_paused = true)]
    async fn interrupt_pacing_holds_the_nominal_period() {
        let d = dev(UsbSpeed::Full);
        let ep = UsbEndpoint {
            address: 0x81,
            attributes: EndpointAttributes::Interrupt as u8,
            max_packet_size: 64,
            interval: 4,
        };
        let period = d.service_interval(ep);
        let first = d.next_deadline(ep, period);
        tokio::time::sleep_until(first + std::time::Duration::from_millis(1)).await;
        assert_eq!(d.next_deadline(ep, period) - first, period);
    }

    #[test]
    fn full_speed_interval_is_milliseconds_and_never_zero() {
        assert_eq!(
            dev(UsbSpeed::Full).service_interval(iso_ep(1)),
            std::time::Duration::from_millis(1)
        );
        assert_eq!(
            dev(UsbSpeed::Full).service_interval(iso_ep(0)),
            std::time::Duration::from_millis(1)
        );
    }
}
