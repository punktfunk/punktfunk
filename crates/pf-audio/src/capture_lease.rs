//! A session's desktop capturer from open to park, shared by both audio planes. The planes keep
//! their own wire; only the capturer's lifecycle lives here.

use crate::{
    open_audio_capture_named, park_audio_capture, take_parked_capture, AudioCapSlot, AudioCapturer,
};
use anyhow::Result;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Wait after a failed open or a capture death before the next open.
const REOPEN_BACKOFF: Duration = Duration::from_secs(2);

/// This session's capturer: opened, reopened after a death under [`REOPEN_BACKOFF`], its sink
/// name published for a later joiner, and parked at the end. Empty chunks from a quiet sink
/// are not a death.
pub struct CaptureLease {
    cap: Option<Box<dyn AudioCapturer>>,
    /// The sink this session opens by name. `None` is the shared path, the only one that parks.
    target: Option<String>,
    last_failed: Option<Instant>,
    channels: u32,
    rate_hz: u32,
    route: CaptureRoute,
}

/// Which sink a lease captures and where it publishes the name. `Default` is the shared path.
#[derive(Default)]
pub struct CaptureRoute {
    /// Isolated session sink (`design/gamescope-multiuser.md`). Linux-only.
    pub sink: Option<String>,
    /// A `join` session taps the owner's sink instead of minting a second one of that name.
    pub tap: bool,
    /// The owner's live-display slot, for a joiner on the shared path.
    pub tap_from: Option<Arc<Mutex<Option<String>>>>,
    /// This session's live-display record: the sink name goes here on every open.
    pub published: Arc<Mutex<Option<String>>>,
}

/// What [`CaptureLease::ready`] found.
pub enum Ready {
    Live,
    /// A new capturer after a gap: drop whatever straddles it.
    Reopened,
    /// Still down; try again next pass.
    Down,
}

impl CaptureLease {
    /// How long a joiner waits for the owner's sink name before minting its own. The owner's
    /// capturer opens within its first second; past this, the owner has no audio to share.
    const JOIN_SINK_WAIT: Duration = Duration::from_secs(2);

    /// A lease with no capturer yet; [`Self::open`] opens it.
    pub fn new(channels: u32, rate_hz: u32, route: CaptureRoute) -> CaptureLease {
        CaptureLease {
            cap: None,
            target: None,
            last_failed: None,
            channels,
            rate_hz,
            route,
        }
    }

    /// The sink to open: the isolated one, else the owner's once published. A joiner spawned
    /// inside the owner's first second waits for it — minting its own sink here would claim
    /// the default from under the owner. `stopped` ends the wait.
    fn resolve(&self, stopped: impl Fn() -> bool) -> Option<String> {
        if self.route.sink.is_some() {
            return self.route.sink.clone();
        }
        let slot = self.route.tap_from.as_ref()?;
        let deadline = Instant::now() + Self::JOIN_SINK_WAIT;
        loop {
            if let Some(name) = slot.lock().unwrap().clone() {
                return Some(name);
            }
            if Instant::now() >= deadline || stopped() {
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// First open. Isolated sessions never adopt the parked shared capturer, and the audio
    /// settings must match too: a keep-host session must not inherit a sink claim. A failed
    /// open is retried by [`Self::ready`] like a mid-session death.
    pub fn open(&mut self, parked: &AudioCapSlot, stopped: impl Fn() -> bool) {
        self.target = self.resolve(stopped);
        let reuse = if self.target.is_none() {
            take_parked_capture(parked, self.channels, self.rate_hz)
        } else {
            None
        };
        self.cap = reuse.or_else(|| match self.open_named() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture did not open — retrying in the background until it comes up");
                None
            }
        });
        self.last_failed = self.cap.is_none().then(Instant::now);
        self.publish();
    }

    fn open_named(&self) -> Result<Box<dyn AudioCapturer>> {
        open_audio_capture_named(
            self.channels,
            self.rate_hz,
            self.target.as_deref(),
            self.route.tap,
        )
    }

    fn publish(&self) {
        if let Some(c) = &self.cap {
            *self.route.published.lock().unwrap() = c.sink_name().map(str::to_owned);
        }
    }

    /// A live capturer, reopening a dead one once the backoff allows. Sleeps 200 ms when it
    /// stays down.
    pub fn ready(&mut self, stopped: impl Fn() -> bool) -> Ready {
        if self.cap.is_some() {
            return Ready::Live;
        }
        if self
            .last_failed
            .is_some_and(|t| t.elapsed() < REOPEN_BACKOFF)
        {
            std::thread::sleep(Duration::from_millis(200));
            return Ready::Down;
        }
        // The owner may have reopened on a new name meanwhile.
        self.target = self.resolve(stopped);
        match self.open_named() {
            Ok(c) => {
                tracing::info!("audio capture reopened");
                self.cap = Some(c);
                self.last_failed = None;
                self.publish();
                Ready::Reopened
            }
            Err(e) => {
                tracing::debug!(error = %format!("{e:#}"), "audio capture did not reopen — retrying");
                self.last_failed = Some(Instant::now());
                std::thread::sleep(Duration::from_millis(200));
                Ready::Down
            }
        }
    }

    /// Whether a capturer is open.
    pub fn is_live(&self) -> bool {
        self.cap.is_some()
    }

    /// The live capturer. Only after [`Self::ready`] said so.
    pub fn live(&mut self) -> &mut Box<dyn AudioCapturer> {
        self.cap.as_mut().expect("capturer is live")
    }

    /// The live capturer's next chunk, waiting at most `budget` (`None`: until one comes).
    /// `None` back means the capture thread died; [`Self::ready`] reopens it.
    pub fn next_chunk(&mut self, budget: Option<Duration>) -> Option<Vec<f32>> {
        let cap = self.live();
        let got = match budget {
            None => cap.next_chunk(),
            Some(budget) => cap.next_chunk_within(budget),
        };
        match got {
            Ok(chunk) => Some(chunk),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "audio capture lost — reopening");
                self.lost();
                None
            }
        }
    }

    /// The capture thread died: reopen after the backoff.
    fn lost(&mut self) {
        self.cap = None;
        self.last_failed = Some(Instant::now());
    }

    /// Park a live shared capturer (releases the routing claim). An isolated capturer is
    /// dropped: its sink name is this session's, and a later shared session would capture a
    /// sink nothing routes to.
    pub fn park(self, parked: &AudioCapSlot) {
        if let Some(mut c) = self.cap {
            c.idle();
            if self.target.is_none() {
                park_audio_capture(parked, c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Idle;
    impl AudioCapturer for Idle {
        fn next_chunk(&mut self) -> Result<Vec<f32>> {
            Ok(Vec::new())
        }
    }

    /// Only a shared capturer parks. An isolated one carries this session's sink name, and a
    /// later shared session adopting it would capture a sink nothing routes to.
    #[test]
    fn only_a_shared_capturer_is_parked() {
        let lease = |target: Option<&str>| CaptureLease {
            cap: Some(Box::new(Idle)),
            target: target.map(str::to_owned),
            ..CaptureLease::new(2, 48_000, CaptureRoute::default())
        };
        let slot: AudioCapSlot = Default::default();
        lease(Some("punktfunk-session-1")).park(&slot);
        assert!(
            slot.lock().unwrap().is_none(),
            "an isolated capturer was parked"
        );
        lease(None).park(&slot);
        // Windows drops every capturer at park (`park_audio_capture`).
        assert_eq!(slot.lock().unwrap().is_some(), !cfg!(windows));
    }

    struct Dead;
    impl AudioCapturer for Dead {
        fn next_chunk(&mut self) -> Result<Vec<f32>> {
            anyhow::bail!("capture thread ended")
        }
    }

    /// A dead capture thread drops the capturer, and the next open waits out the backoff.
    #[test]
    fn a_dead_capturer_stays_down_through_the_backoff() {
        let mut lease = CaptureLease::new(2, 48_000, CaptureRoute::default());
        lease.cap = Some(Box::new(Dead));
        assert!(matches!(lease.ready(|| false), Ready::Live));
        assert!(lease.next_chunk(None).is_none());
        assert!(!lease.is_live());
        assert!(matches!(lease.ready(|| false), Ready::Down));
    }
}
