//! Shared `/dev/uhid` event ABI (`linux/uhid.h`) and [`UhidDevice`], the one device every UHID
//! pad drives.
//!
//! `struct uhid_event` is `__packed__`: a `u32` type then a union whose
//! largest member is `uhid_create2_req` (name 128 + phys 64 + uniq 64 +
//! rd_size 2 + bus 2 + 4×u32 + rd_data 4096 = 4372). [`UHID_EVENT_SIZE`] is
//! that plus the type tag.
//!
//! [`set_report_data`] and [`output_data`] honour the kernel's `size` field.
//! A fixed window truncates a long report or parses stale bytes past a short
//! one in a reused event buffer.
//!
//! On a seat the kernel fd is the supervisor's ([`crate::pad_broker`]): [`UhidDevice::relayed`]
//! is one end of a `SOCK_SEQPACKET` pair that carries the same events both ways, and
//! [`UhidDevice::relay`] is the supervisor's loop between the other end and `/dev/uhid`. Only
//! what a device answers with crosses from the seat; a `CREATE2` or `DESTROY` of its own is
//! dropped, so the seat can never rebind the driver.

use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixDatagram;

pub const UHID_PATH: &str = "/dev/uhid";

// `enum uhid_event_type`; only the ones backends write.
pub const UHID_DESTROY: u32 = 1;
pub const UHID_OUTPUT: u32 = 6;
pub const UHID_GET_REPORT: u32 = 9;
pub const UHID_GET_REPORT_REPLY: u32 = 10;
pub const UHID_CREATE2: u32 = 11;
pub const UHID_INPUT2: u32 = 12;
pub const UHID_SET_REPORT: u32 = 13;
pub const UHID_SET_REPORT_REPLY: u32 = 14;

/// Cap on a report payload copied out of an event.
pub const HID_MAX_DESCRIPTOR_SIZE: usize = 4096;
/// `u32` type tag plus the create2 union (`sizeof(struct uhid_event)`).
pub const UHID_EVENT_SIZE: usize = 4 + 4372;
/// From `linux/input.h`.
pub const BUS_USB: u16 = 0x03;
pub const BUS_BLUETOOTH: u16 = 0x05;
/// The GET_REPORT reply error for a report this pad does not have.
const EIO: u16 = 5;

/// Shared by GET_REPORT / SET_REPORT request and reply.
const OFF_ID: usize = 4;
/// After `id: u32`, `rnum: u8`, `rtype: u8`.
const OFF_SET_REPORT_SIZE: usize = 10;
/// SET_REPORT payload, and `data` in the reply structs.
const OFF_DATA: usize = 12;
/// After `data[4096]` — unlike SET_REPORT, `size` is trailing.
const OFF_OUTPUT_SIZE: usize = 4 + HID_MAX_DESCRIPTOR_SIZE;

/// Truncation still leaves a NUL: the caller zeros the buffer first.
fn put_cstr(ev: &mut [u8], off: usize, cap: usize, s: &str) {
    let n = s.len().min(cap - 1);
    ev[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
}

/// What the matching GET_REPORT / SET_REPORT reply must echo.
pub fn request_id(ev: &[u8]) -> u32 {
    u32::from_ne_bytes([ev[OFF_ID], ev[OFF_ID + 1], ev[OFF_ID + 2], ev[OFF_ID + 3]])
}

/// Honour the event's own `size`. A fixed window truncates a long report
/// or parses stale bytes past a short one in a reused buffer.
pub fn set_report_data(ev: &[u8]) -> &[u8] {
    let size = u16::from_ne_bytes([ev[OFF_SET_REPORT_SIZE], ev[OFF_SET_REPORT_SIZE + 1]]) as usize;
    let end = (OFF_DATA + size.min(HID_MAX_DESCRIPTOR_SIZE)).min(ev.len());
    &ev[OFF_DATA.min(end)..end]
}

/// `uhid_output_req`: `data[4096]` then trailing `size` (unlike SET_REPORT).
pub fn output_data(ev: &[u8]) -> &[u8] {
    let size = u16::from_ne_bytes([ev[OFF_OUTPUT_SIZE], ev[OFF_OUTPUT_SIZE + 1]]) as usize;
    let end = (4 + size.min(HID_MAX_DESCRIPTOR_SIZE)).min(ev.len());
    &ev[4.min(end)..end]
}

/// What a seat may write to its relay: a device's answers. `CREATE2` and `DESTROY` stay the
/// supervisor's.
const SEAT_MAY_WRITE: [u32; 3] = [UHID_INPUT2, UHID_GET_REPORT_REPLY, UHID_SET_REPORT_REPLY];

/// A [`Create2`] that owns its strings: what a backend builds for its pad, and what the broker
/// builds for a seat's, with the seat's marker in `phys`.
pub struct Identity {
    pub bus: u16,
    pub name: String,
    pub phys: String,
    pub uniq: String,
    pub rdesc: &'static [u8],
    pub vendor: u32,
    pub product: u32,
    pub version: u32,
}

impl Identity {
    pub fn as_create2(&self) -> Create2<'_> {
        Create2 {
            bus: self.bus,
            name: &self.name,
            phys: &self.phys,
            uniq: &self.uniq,
            rdesc: self.rdesc,
            vendor: self.vendor,
            product: self.product,
            version: self.version,
        }
    }
}

/// `UHID_CREATE2` identity: what the kernel driver binds on. Strings truncate to the kernel's
/// fields (name 128, phys and uniq 64).
pub struct Create2<'a> {
    /// [`BUS_USB`] or [`BUS_BLUETOOTH`]; SDL and Steam read the transport from it.
    pub bus: u16,
    pub name: &'a str,
    pub phys: &'a str,
    pub uniq: &'a str,
    pub rdesc: &'a [u8],
    pub vendor: u32,
    pub product: u32,
    /// bcdDevice.
    pub version: u32,
}

/// One kernel request [`UhidDevice::poll`] hands its caller.
pub enum UhidEvent<'a> {
    /// `UHID_OUTPUT`: an output report, as long as the kernel says it is.
    Output(&'a [u8]),
    /// `UHID_GET_REPORT`: answer with [`UhidDevice::reply_get_report`], or the kernel holds the
    /// reader until its timeout.
    GetReport { id: u32, rnum: u8 },
    /// `UHID_SET_REPORT`: a feature write. [`UhidDevice::poll`] acks it after the callback.
    SetReport(&'a [u8]),
}

/// One `/dev/uhid` device. Drop sends `UHID_DESTROY`, which unbinds the kernel driver. A
/// [`relayed`](Self::relayed) device is the seat's end of the supervisor's relay: the same
/// events, and its drop closes the relay, which is its destroy.
pub struct UhidDevice {
    fd: File,
    relayed: bool,
    /// The supervisor hung up: the pad is gone, and the manager makes another.
    dead: bool,
}

impl UhidDevice {
    /// `kind` as a seat gets it (the supervisor builds `id` from its own table, stamped), else
    /// `id` opened here.
    pub fn open_kind(
        kind: crate::pad_broker::PadKind,
        index: u8,
        id: &Identity,
    ) -> Result<UhidDevice> {
        if pf_paths::seat::is_seat_host() {
            return UhidDevice::relayed(crate::pad_broker::request(kind, index)?);
        }
        UhidDevice::open(&id.as_create2())
    }

    /// The seat's end of a relay the supervisor answered.
    pub fn relayed(fd: OwnedFd) -> Result<UhidDevice> {
        let sock = UnixDatagram::from(fd);
        sock.set_nonblocking(true)
            .context("set the pad relay non-blocking")?;
        Ok(UhidDevice {
            fd: File::from(OwnedFd::from(sock)),
            relayed: true,
            dead: false,
        })
    }

    /// `false` once the supervisor dropped this device's relay. A kernel device is always alive.
    pub fn alive(&self) -> bool {
        !self.dead
    }

    /// A write's error, kept: a hung-up relay reads as dead.
    fn wrote(&mut self, result: std::io::Result<()>, what: &str) -> Result<()> {
        if let Err(e) = &result {
            if self.relayed
                && matches!(
                    e.kind(),
                    ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected
                )
            {
                self.dead = true;
            }
        }
        result.context(what.to_owned())
    }

    /// A created device's `/dev/uhid` fd, as systemd's fd store hands it back.
    pub fn adopt(fd: OwnedFd) -> UhidDevice {
        UhidDevice {
            fd: File::from(fd),
            relayed: false,
            dead: false,
        }
    }

    /// Open `/dev/uhid` non-blocking and create the device.
    pub fn open(c: &Create2) -> Result<UhidDevice> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(UHID_PATH)
            .with_context(|| {
                format!("open {UHID_PATH} (is the 60-punktfunk.rules uhid rule installed + are you in 'input'?)")
            })?;
        let mut dev = UhidDevice {
            fd,
            relayed: false,
            dead: false,
        };
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        // uhid_create2_req at 4: name[128] phys[64] uniq[64] rd_size bus vid pid version country rd_data.
        put_cstr(&mut ev, 4, 128, c.name);
        put_cstr(&mut ev, 132, 64, c.phys);
        put_cstr(&mut ev, 196, 64, c.uniq);
        ev[260..262].copy_from_slice(&(c.rdesc.len() as u16).to_ne_bytes());
        ev[262..264].copy_from_slice(&c.bus.to_ne_bytes());
        ev[264..268].copy_from_slice(&c.vendor.to_ne_bytes());
        ev[268..272].copy_from_slice(&c.product.to_ne_bytes());
        ev[272..276].copy_from_slice(&c.version.to_ne_bytes());
        ev[280..280 + c.rdesc.len()].copy_from_slice(c.rdesc);
        dev.fd
            .write_all(&ev)
            .with_context(|| format!("write UHID_CREATE2 for {}", c.name))?;
        Ok(dev)
    }

    /// `UHID_INPUT2`: one input report, as the device's own.
    pub fn write_input(&mut self, data: &[u8]) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_INPUT2.to_ne_bytes());
        // uhid_input2_req: size u16 at 4, data at 6.
        ev[4..6].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[6..6 + data.len()].copy_from_slice(data);
        let wrote = self.fd.write_all(&ev);
        self.wrote(wrote, "write UHID_INPUT2")
    }

    /// Answer a GET_REPORT: `Some(data)`, or `None` (EIO) for a report this pad does not have.
    pub fn reply_get_report(&mut self, id: u32, data: Option<&[u8]>) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_GET_REPORT_REPLY.to_ne_bytes());
        // uhid_get_report_reply_req: id u32 [4..8], err u16 [8..10], size u16 [10..12], data [12..].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        let (err, data) = match data {
            Some(data) => (0u16, data),
            None => (EIO, &[][..]),
        };
        ev[8..10].copy_from_slice(&err.to_ne_bytes());
        ev[10..12].copy_from_slice(&(data.len() as u16).to_ne_bytes());
        ev[OFF_DATA..OFF_DATA + data.len()].copy_from_slice(data);
        let wrote = self.fd.write_all(&ev);
        self.wrote(wrote, "write UHID_GET_REPORT_REPLY")
    }

    fn reply_set_report(&mut self, id: u32) -> Result<()> {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_SET_REPORT_REPLY.to_ne_bytes());
        // uhid_set_report_reply_req: id u32 [4..8], err u16 [8..10].
        ev[4..8].copy_from_slice(&id.to_ne_bytes());
        let wrote = self.fd.write_all(&ev);
        self.wrote(wrote, "write UHID_SET_REPORT_REPLY")
    }

    /// Drain every pending kernel request without blocking, oldest first. A SET_REPORT is
    /// acked with `err = 0` after `on_event` sees it: the kernel holds a feature writer ~5 s
    /// for an unanswered one. Call often — driver init blocks on these answers.
    pub fn poll(&mut self, mut on_event: impl FnMut(&mut UhidDevice, UhidEvent<'_>)) {
        let mut ev = [0u8; UHID_EVENT_SIZE];
        while let Ok(n) = self.fd.read(&mut ev) {
            if n == 0 && self.relayed {
                self.dead = true; // the relay's other end closed
                break;
            }
            if n < UHID_EVENT_SIZE {
                break;
            }
            match u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]) {
                UHID_OUTPUT => on_event(self, UhidEvent::Output(output_data(&ev))),
                UHID_GET_REPORT => {
                    // uhid_get_report_req: id u32 [4..8], rnum u8 [8].
                    let (id, rnum) = (request_id(&ev), ev[8]);
                    on_event(self, UhidEvent::GetReport { id, rnum })
                }
                UHID_SET_REPORT => {
                    on_event(self, UhidEvent::SetReport(set_report_data(&ev)));
                    let _ = self.reply_set_report(request_id(&ev));
                }
                _ => {}
            }
        }
    }

    /// The supervisor's side of a seat's relay: `seat`'s events into the kernel, the kernel's
    /// to `seat`, until the seat hangs up. Blocks; drop destroys the device. Only what a device
    /// answers with crosses from the seat, and only whole events.
    pub fn relay(self, seat: UnixDatagram) {
        use rustix::event::{poll, PollFd, PollFlags};
        let _ = seat.set_nonblocking(true);
        let mut ev = [0u8; UHID_EVENT_SIZE];
        loop {
            let mut fds = [
                PollFd::new(&seat, PollFlags::IN),
                PollFd::new(&self.fd, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(_) => return,
            }
            let (from_seat, from_kernel) = (fds[0].revents(), fds[1].revents());
            if from_seat.intersects(PollFlags::IN | PollFlags::HUP | PollFlags::ERR) {
                match seat.recv(&mut ev) {
                    Ok(0) => return,
                    Ok(n) => {
                        let type_ = u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]);
                        if n == UHID_EVENT_SIZE && SEAT_MAY_WRITE.contains(&type_) {
                            let _ = (&self.fd).write_all(&ev);
                        } else {
                            tracing::debug!(type_, n, "uhid relay: dropped what the seat sent");
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                    Err(_) => return,
                }
            }
            if from_kernel.intersects(PollFlags::IN) {
                while let Ok(n) = (&self.fd).read(&mut ev) {
                    if n < UHID_EVENT_SIZE {
                        break;
                    }
                    match seat.send(&ev) {
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
}

impl AsFd for UhidDevice {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

impl Drop for UhidDevice {
    fn drop(&mut self) {
        // A relayed device's destroy is the socket closing; the supervisor never takes DESTROY.
        if self.relayed {
            return;
        }
        let mut ev = [0u8; UHID_EVENT_SIZE];
        ev[0..4].copy_from_slice(&UHID_DESTROY.to_ne_bytes());
        let _ = self.fd.write_all(&ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixDatagram;

    fn blank() -> Vec<u8> {
        vec![0u8; UHID_EVENT_SIZE]
    }

    /// A device over one end of a datagram pair; the other end plays the kernel.
    fn device() -> (UhidDevice, UnixDatagram) {
        let (dev, kernel) = UnixDatagram::pair().expect("socketpair");
        dev.set_nonblocking(true).expect("nonblocking");
        (
            UhidDevice {
                fd: File::from(OwnedFd::from(dev)),
                relayed: false,
                dead: false,
            },
            kernel,
        )
    }

    /// `SOCK_SEQPACKET`, as the relay's pairs are: a peer's close reads as end-of-file, which
    /// a datagram pair never reports.
    fn seqpacket_pair() -> (UnixDatagram, UnixDatagram) {
        let (a, b) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::SEQPACKET,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .expect("socketpair");
        (UnixDatagram::from(a), UnixDatagram::from(b))
    }

    /// The supervisor's relay passes a device's answers through and nothing else, and a seat
    /// whose relay hung up reads as dead.
    #[test]
    fn the_relay_passes_answers_only_and_a_hangup_is_death() {
        let (kernel_side, kernel) = seqpacket_pair();
        let (sup_end, seat_end) = seqpacket_pair();
        let supervisor = UhidDevice {
            fd: File::from(OwnedFd::from(kernel_side)),
            relayed: false,
            dead: false,
        };
        let relay = std::thread::spawn(move || supervisor.relay(sup_end));
        let mut seat = UhidDevice::relayed(OwnedFd::from(seat_end)).unwrap();
        kernel.set_nonblocking(true).unwrap();

        seat.write_input(&[1, 2, 3]).unwrap();
        let mut got = blank();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(kernel.recv(&mut got).unwrap(), UHID_EVENT_SIZE);
        assert_eq!(event_type(&got), UHID_INPUT2);

        let mut create = blank();
        create[0..4].copy_from_slice(&UHID_CREATE2.to_ne_bytes());
        (&seat.fd).write_all(&create).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            kernel.recv(&mut got).is_err(),
            "a CREATE2 from the seat never reaches the kernel"
        );

        let mut out = blank();
        out[0..4].copy_from_slice(&UHID_OUTPUT.to_ne_bytes());
        kernel.send(&out).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut seen = 0;
        seat.poll(|_, ev| {
            if let UhidEvent::Output(_) = ev {
                seen += 1;
            }
        });
        assert_eq!(seen, 1);
        assert!(seat.alive());

        drop(kernel);
        relay.join().unwrap();
        seat.poll(|_, _| {});
        assert!(!seat.alive(), "the supervisor's relay ended");
    }

    fn event_type(ev: &[u8]) -> u32 {
        u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]])
    }

    /// A pad whose handler ignores SET_REPORT (the Switch Pro's) must still ack it, or a
    /// hidraw `HIDIOCSFEATURE` waits out the kernel's timeout.
    #[test]
    fn a_set_report_the_pad_ignores_is_still_acked() {
        let (mut dev, kernel) = device();
        let mut req = blank();
        req[0..4].copy_from_slice(&UHID_SET_REPORT.to_ne_bytes());
        req[OFF_ID..OFF_ID + 4].copy_from_slice(&0x1234u32.to_ne_bytes());
        kernel.send(&req).unwrap();

        let mut seen = 0;
        dev.poll(|_, ev| {
            if let UhidEvent::SetReport(_) = ev {
                seen += 1;
            }
        });
        assert_eq!(seen, 1);

        let mut reply = blank();
        kernel.set_nonblocking(true).unwrap();
        let n = kernel
            .recv(&mut reply)
            .expect("no SET_REPORT reply was written");
        assert_eq!(n, UHID_EVENT_SIZE);
        assert_eq!(event_type(&reply), UHID_SET_REPORT_REPLY);
        assert_eq!(request_id(&reply), 0x1234);
        assert_eq!(&reply[8..10], &[0, 0], "the ack must report success");
    }

    /// `None` answers EIO with an empty payload; `Some` answers success with the bytes.
    #[test]
    fn get_report_replies_carry_the_error_and_payload() {
        let (mut dev, kernel) = device();
        let mut reply = blank();
        dev.reply_get_report(7, None).unwrap();
        kernel.recv(&mut reply).unwrap();
        assert_eq!(event_type(&reply), UHID_GET_REPORT_REPLY);
        assert_eq!(u16::from_ne_bytes([reply[8], reply[9]]), EIO);
        assert_eq!(&reply[10..12], &[0, 0]);

        dev.reply_get_report(8, Some(&[0x09, 0xAA])).unwrap();
        kernel.recv(&mut reply).unwrap();
        assert_eq!(request_id(&reply), 8);
        assert_eq!(&reply[8..10], &[0, 0]);
        assert_eq!(u16::from_ne_bytes([reply[10], reply[11]]), 2);
        assert_eq!(&reply[OFF_DATA..OFF_DATA + 2], &[0x09, 0xAA]);
    }

    #[test]
    fn dropping_the_device_destroys_it() {
        let (dev, kernel) = device();
        drop(dev);
        let mut ev = blank();
        kernel.recv(&mut ev).unwrap();
        assert_eq!(event_type(&ev), UHID_DESTROY);
    }

    #[test]
    fn set_report_data_honours_the_events_own_size() {
        let mut ev = blank();
        ev[OFF_SET_REPORT_SIZE..OFF_SET_REPORT_SIZE + 2].copy_from_slice(&5u16.to_ne_bytes());
        for (i, b) in [1u8, 2, 3, 4, 5].iter().enumerate() {
            ev[OFF_DATA + i] = *b;
        }
        // Stale bytes past the payload — a fixed-window read would hand these to the parser.
        ev[OFF_DATA + 5] = 0xAA;
        ev[OFF_DATA + 15] = 0xBB;
        assert_eq!(set_report_data(&ev), &[1, 2, 3, 4, 5]);
    }

    #[test]
    fn set_report_data_is_not_truncated_at_sixteen() {
        let mut ev = blank();
        let n = 40usize;
        ev[OFF_SET_REPORT_SIZE..OFF_SET_REPORT_SIZE + 2].copy_from_slice(&(n as u16).to_ne_bytes());
        for i in 0..n {
            ev[OFF_DATA + i] = i as u8;
        }
        let d = set_report_data(&ev);
        assert_eq!(
            d.len(),
            n,
            "a report longer than 16 bytes must survive whole"
        );
        assert_eq!(d[39], 39);
    }

    #[test]
    fn oversized_and_empty_sizes_stay_in_bounds() {
        let mut ev = blank();
        ev[OFF_SET_REPORT_SIZE..OFF_SET_REPORT_SIZE + 2].copy_from_slice(&u16::MAX.to_ne_bytes());
        assert!(set_report_data(&ev).len() <= HID_MAX_DESCRIPTOR_SIZE);
        assert!(OFF_DATA + set_report_data(&ev).len() <= UHID_EVENT_SIZE);

        let ev0 = blank();
        assert!(set_report_data(&ev0).is_empty());
        assert!(output_data(&ev0).is_empty());
    }

    #[test]
    fn output_data_reads_its_trailing_size_field() {
        let mut ev = blank();
        ev[OFF_OUTPUT_SIZE..OFF_OUTPUT_SIZE + 2].copy_from_slice(&3u16.to_ne_bytes());
        ev[4] = 0x02;
        ev[5] = 0x11;
        ev[6] = 0x22;
        ev[7] = 0x33; // past the declared size
        assert_eq!(output_data(&ev), &[0x02, 0x11, 0x22]);
    }

    #[test]
    fn request_id_round_trips() {
        let mut ev = blank();
        ev[OFF_ID..OFF_ID + 4].copy_from_slice(&0xDEAD_BEEFu32.to_ne_bytes());
        assert_eq!(request_id(&ev), 0xDEAD_BEEF);
    }
}
