//! Committed text on KWin through its libei text device (`ei_text`, Plasma 6.8 on libei 1.6).
//!
//! KWin commits what arrives there as an input method would: into the focused text field,
//! else as keys it sends the application itself. Neither passes an input method's keyboard
//! grab, so a character the session layout lacks arrives where the fake-input keysym
//! request loses it. The device rides KWin's own EIS (`connectToEIS`, no portal and no
//! grant), which hands out text with the keyboard.
//!
//! It is not the way for all text. An application applies a text commit ahead of a key
//! that reached it just before, so a Backspace followed at once by text deletes the new
//! character. Keys keep their order only among themselves; the caller sends here what a
//! key can't type, and gives the application time between a key and the text after it.
//!
//! This is also a second connection beside the fake-input one, and KWin reads the two in
//! no fixed order. [`TextPipe::settle`] waits until KWin has taken the text, so the key
//! that follows a character lands after it.

use anyhow::{anyhow, bail, Context as _, Result};
use reis::ei;
use reis::event::{Device, DeviceCapability, EiEvent, EiEventConverter};
use reis::{Interface as _, PendingRequestResult};
use std::cell::Cell;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// How long KWin gets to hand out the text device, and to answer a sync.
const ANSWER: Duration = Duration::from_millis(500);

/// The portal's keyboard device bit, which KWin's `connectToEIS` reads.
const KEYBOARD: i32 = 1;

pub(super) struct TextPipe {
    context: ei::Context,
    events: EiEventConverter,
    device: Device,
    text: ei::Text,
    /// The serial of the device's last resume; `None` while KWin has it paused.
    resumed: Option<u32>,
    /// `start_emulating` went out since that resume.
    emulating: bool,
    sequence: u32,
}

impl TextPipe {
    /// Open KWin's EIS over `bus` and wait for its text device. KWin keeps the EIS context
    /// while `bus` holds its name, so the caller keeps `bus` for as long as the pipe.
    /// An error for a KWin with no EIS, or one whose seat offers no text: Plasma before 6.8,
    /// or a KWin built on an older libei. That answer comes with the seat, without a wait.
    pub fn open(bus: &zbus::blocking::Connection) -> Result<Self> {
        let reply = bus
            .call_method(
                Some("org.kde.KWin"),
                "/org/kde/KWin/EIS/RemoteDesktop",
                Some("org.kde.KWin.EIS.RemoteDesktop"),
                "connectToEIS",
                &(KEYBOARD,),
            )
            .context("connect to KWin's EIS")?;
        let (fd, _cookie): (zbus::zvariant::OwnedFd, i32) = reply.body().deserialize()?;
        let stream = UnixStream::from(std::os::fd::OwnedFd::from(fd));
        let context = ei::Context::new(stream).context("open the EIS context")?;
        let hello = reis::handshake::ei_handshake_blocking(
            &context,
            "punktfunk-host-text",
            ei::handshake::ContextType::Sender,
        )
        .map_err(|e| anyhow!("EIS handshake: {e}"))?;
        let mut events = EiEventConverter::new(&context, hello);
        context.flush().context("flush EIS")?;

        let deadline = Instant::now() + ANSWER;
        let mut seat_types = false;
        let mut found: Option<(Device, ei::Text)> = None;
        let mut resumed = None;
        while resumed.is_none() {
            let peek = |event: &ei::Event| {
                if let ei::Event::Seat(_, ei::seat::Event::Capability { interface, .. }) = event {
                    seat_types |= interface == ei::Text::NAME;
                }
            };
            pump(&context, &mut events, deadline, peek).context("wait for KWin's text device")?;
            while let Some(event) = events.next_event() {
                match event {
                    EiEvent::SeatAdded(e) => {
                        if !seat_types {
                            bail!("KWin's seat offers no text device");
                        }
                        e.seat.bind_capabilities(DeviceCapability::Text.into());
                        context.flush().context("flush EIS")?;
                    }
                    EiEvent::DeviceAdded(e) => {
                        // `ei_device` v3 asks a sender for this once the device is described.
                        if e.device.device().version() >= 3 {
                            e.device.device().ready();
                            context.flush().context("flush EIS")?;
                        }
                        if let Some(text) = e.device.interface::<ei::Text>() {
                            found = Some((e.device, text));
                        }
                    }
                    EiEvent::DeviceResumed(e) => resumed = Some(e.serial),
                    _ => {}
                }
            }
        }
        let (device, text) = found.context("KWin resumed a device with no text interface")?;
        Ok(Self {
            context,
            events,
            device,
            text,
            resumed,
            emulating: false,
            sequence: 0,
        })
    }

    /// Type `c`. An error leaves nothing typed: the caller types it another way.
    pub fn send(&mut self, c: char) -> Result<()> {
        self.drain()?;
        let Some(serial) = self.resumed else {
            bail!("KWin paused the text device");
        };
        let device = self.device.device();
        if !self.emulating {
            self.sequence += 1;
            device.start_emulating(serial, self.sequence);
            self.emulating = true;
        }
        self.text.utf8(c.encode_utf8(&mut [0; 4]));
        device.frame(serial, crate::monotonic_us());
        self.context.flush().context("flush EIS")
    }

    /// Wait until KWin has read everything sent. It commits the text in the same pass, before
    /// it reads another connection.
    pub fn settle(&mut self) -> Result<()> {
        let done = Rc::new(Cell::new(false));
        let callback = self.events.connection().connection().sync(1);
        let seen = Rc::clone(&done);
        self.events
            .add_callback_handler(callback, move |_| seen.set(true));
        self.context.flush().context("flush EIS")?;
        let deadline = Instant::now() + ANSWER;
        while !done.get() {
            pump(&self.context, &mut self.events, deadline, |_| {})
                .context("wait for KWin's sync answer")?;
            self.note_events();
        }
        Ok(())
    }

    /// Take what KWin sent without waiting: its pings, and a pause or resume of the device.
    fn drain(&mut self) -> Result<()> {
        pump(&self.context, &mut self.events, Instant::now(), |_| {})?;
        self.note_events();
        Ok(())
    }

    fn note_events(&mut self) {
        while let Some(event) = self.events.next_event() {
            match event {
                EiEvent::DeviceResumed(e) => {
                    self.resumed = Some(e.serial);
                    self.emulating = false;
                }
                EiEvent::DevicePaused(_) | EiEvent::DeviceRemoved(_) => self.resumed = None,
                _ => {}
            }
        }
    }
}

/// Hand `events` what KWin has sent, showing `peek` each raw event first: what the context
/// already holds (the handshake's read takes in whatever followed it), else what arrives
/// until `deadline`. An error when that wait runs out, unless `deadline` is already past:
/// then nothing to read is the answer.
fn pump(
    context: &ei::Context,
    events: &mut EiEventConverter,
    deadline: Instant,
    mut peek: impl FnMut(&ei::Event),
) -> Result<()> {
    use rustix::event::{poll, PollFd, PollFlags, Timespec};
    let mut take = || -> Result<bool> {
        let mut any = false;
        while let Some(pending) = context.pending_event() {
            any = true;
            match pending {
                PendingRequestResult::Request(event) => {
                    peek(&event);
                    events
                        .handle_event(event)
                        .map_err(|e| anyhow!("handle an EIS event: {e:?}"))?;
                }
                PendingRequestResult::ParseError(e) => bail!("parse an EIS event: {e:?}"),
                PendingRequestResult::InvalidObject(_) => {}
            }
        }
        Ok(any)
    };
    if take()? {
        return Ok(());
    }
    let wait = deadline.saturating_duration_since(Instant::now());
    let timeout = Timespec::try_from(wait).ok();
    let mut fds = [PollFd::new(context, PollFlags::IN)];
    if poll(&mut fds, timeout.as_ref()).context("poll EIS")? == 0 {
        if wait.is_zero() {
            return Ok(());
        }
        bail!("no answer in time");
    }
    context.read().context("read EIS")?;
    take()?;
    Ok(())
}
