//! A remote-encoder source's edge: how the host reaches the process its encoder runs in.
//!
//! [`Transport`] has the driver's WUDFHost over IOCTLs (`idd/channel.rs`) and a capture
//! worker over pipes (`wgc.rs`); the tests' fake driver is a third. [`HandleTarget`] is that
//! process as the host plants an AU section there, and [`AuMemory`] the host's own mapping of
//! it. Every far-side handle value follows `SET_ENCODE`'s adopt-on-success rule.

use std::time::Instant;

use anyhow::Result;
use pf_driver_proto::encode::{SetEncodeReply, SetEncodeRequest};
use pf_driver_proto::section::SectionView;

use crate::{EncodeCtlSender, EncoderSite};

/// Where a remote encoder runs, as the session's `SET_ENCODE` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RemoteEndpoint {
    pub site: EncoderSite,
    /// The monitor `SET_ENCODE` opens for: the driver's own, or the one a worker captures.
    pub target_id: u32,
}

/// News from the far side that the source answers and goes on. Fatal news is `poll`'s `Err`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportEvent {
    /// The captured monitor changed size (the worker's `SOURCE_CHANGED`). The next delivery
    /// carries it and the loop re-opens the encoder.
    Resized { width: u32, height: u32 },
    /// The driver's WUDFHost exited. The source parks a driver cycle for the loop.
    DriverGone,
}

/// How the host reaches a remote encoder: IOCTLs into the driver's WUDFHost, or pipes into a
/// capture worker.
pub(crate) trait Transport: Send {
    fn endpoint(&self) -> RemoteEndpoint;
    /// What the source captures, as its log lines and errors name it.
    fn name(&self) -> &str;
    /// The process the AU section and its event are planted in.
    fn handles(&self) -> &dyn HandleTarget;
    /// `SET_ENCODE`. The far side adopts the planted values iff this returns `Ok`, or an `Err`
    /// that is `encode::ReplyTooShort`.
    fn set_encode(&self, req: &SetEncodeRequest) -> Result<SetEncodeReply>;
    /// `ENCODE_CTL`, for the life of the proxy it is handed to.
    fn encode_ctl(&self) -> EncodeCtlSender;
    /// The OS shows the secure desktop (UAC, Winlogon) now.
    fn secure_desktop(&self) -> bool;
    /// The far side's news since the last tick. `last_fresh` is the source's last delivery,
    /// which a probe may gate on. `Err`: the source is gone for good.
    fn poll(&mut self, now: Instant, last_fresh: Instant) -> Result<Vec<TransportEvent>>;
    /// Whether the far process still runs, read between ticks. Default: until `poll` says not.
    fn alive(&self) -> bool {
        true
    }
}

/// The process a remote encoder runs in, as the host plants an AU section there.
pub(crate) trait HandleTarget {
    /// A new AU section of `bytes` and its auto-reset event, each duplicated into the process
    /// with only the rights the encoder needs.
    fn plant(&self, bytes: usize) -> Result<Planted>;
    /// Close a planted value the process never adopted.
    fn close_remote(&self, value: u64);
}

/// An AU section the host maps, planted in the encoder's process.
pub(crate) struct Planted {
    pub memory: Box<dyn AuMemory>,
    /// The section's and the event's handle values, valid only in that process.
    pub section: u64,
    pub event: u64,
}

/// The host's own mapping of an AU section, and the event the encoder signals on a publish.
pub(crate) trait AuMemory: Send {
    /// The mapping, for a reader that lives beside it in one owner.
    ///
    /// # Safety
    /// The view, and every reference taken through it, must be gone before `self` drops.
    unsafe fn share(&self) -> SectionView<'static>;
    /// Wait at most `timeout_ms` for a publish signal. The header is the truth.
    fn wait(&self, timeout_ms: u32);
    /// A handle that raises the publish signal from any thread, so [`Self::wait`] returns.
    /// It keeps the signal alive past this mapping.
    fn signal(&self) -> Box<dyn Fn() + Send + Sync>;
}
