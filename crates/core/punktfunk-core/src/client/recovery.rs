//! Client-side loss recovery: [`RecoveryDriver`] folds what the decoder reports into the
//! freeze gate, the RFI asks and one ask window; `RfiRecovery` (the decoder's gaps and the
//! pump's short tails), the recent-RFI count, what the short frames an RFI names still
//! lacked, and the numbering of the feedback datagram that carries asks and reports
//! (`FeedbackOut`).

use crate::quic::v2::dgram::Feedback;
use crate::reanchor::{AnchorEvidence, GateVerdict, LocalRecovery, ReanchorGate};
use std::time::{Duration, Instant};

/// One RFI ask per window so a burst of gaps cannot storm the control stream.
/// The host coalesces further.
const RFI_THROTTLE: Duration = Duration::from_millis(100);

/// Short tails remembered so the decoder's later gap over them asks nothing. The gap
/// arrives within the loss window (~120 ms), a few frames at any rate.
const ASKED_TAILS: usize = 32;

/// Loss asks behind one throttle: the decoder's gaps ([`NativeClient::note_frame_index`])
/// and the pump's short tails ([`Self::tail_short`]). Wrapping `frame_index` arithmetic
/// lives here so embedders do not each re-derive it.
#[derive(Default)]
pub(crate) struct RfiRecovery {
    next_expected: Option<u32>,
    last_req: Option<Instant>,
    /// Lost range the throttle swallowed, widened by later asks; sent by the first
    /// `observe`, `tail_short` or `flush` after the window opens. Otherwise a second gap
    /// inside the window (a lost recovery anchor) asks nothing until the 500 ms backstop.
    pending: Option<(u32, u32)>,
    /// Frames already asked for at their tail, newest last.
    asked: std::collections::VecDeque<u32>,
    /// The newest anchor the decoder took. It references only frames this client holds, so
    /// the gap it ends asks nothing.
    anchored: Option<u32>,
}

/// `a` is ahead of `b` in half-space wrap order.
pub(crate) fn ahead_of(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

/// Where one AU's `frame_index` falls in receive order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameOrder {
    /// The first index seen, or the expected next one.
    InOrder,
    /// A forward jump over this many lost frames.
    Gap(u32),
    /// Behind a frame already seen: a reordered or duplicate AU.
    Straggler,
}

/// Recovery request for a forward gap. Keyframe when the span exceeds
/// [`crate::packet::RFI_MAX_RANGE`]: no encoder still holds that reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryAsk {
    None,
    Rfi(u32, u32),
    Keyframe,
    /// A keyframe for a freeze the client raised with nothing lost: the same ask, counted
    /// apart from recovery's, since the rate is not its cause.
    StarvedKeyframe,
}

impl RfiRecovery {
    /// `order` and `ask` are independent: throttle can yield [`RecoveryAsk::None`]
    /// on a [`FrameOrder::Gap`], so can a gap whose frames were asked for at their tail,
    /// and an in-order frame can carry the ask a throttled gap deferred. Pass the gap
    /// width to [`crate::reanchor::ReanchorGate::arm_expecting_drops`] or the
    /// reassembler's later `frames_dropped` climb is counted as a second loss.
    pub(crate) fn observe(&mut self, frame_index: u32, now: Instant) -> (FrameOrder, RecoveryAsk) {
        let order = match self.next_expected {
            Some(exp) => {
                // Half-space wrap: wrapping_sub < u32::MAX/2 is a forward gap; top half is a straggler.
                let ahead = frame_index.wrapping_sub(exp);
                if ahead == 0 {
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    FrameOrder::InOrder
                } else if ahead < u32::MAX / 2 {
                    // Advance past this frame so the same gap cannot re-fire. The oldest
                    // unsent loss stays `first`: the host invalidates everything since it.
                    self.next_expected = Some(frame_index.wrapping_add(1));
                    let lost = self.unasked(exp, frame_index.wrapping_sub(1));
                    if let Some(lost) = lost.filter(|_| self.anchored != Some(frame_index)) {
                        self.widen(lost);
                    }
                    FrameOrder::Gap(ahead)
                } else {
                    // Leave next_expected: a rewind would false-gap the next in-order frame.
                    FrameOrder::Straggler
                }
            }
            None => {
                self.next_expected = Some(frame_index.wrapping_add(1));
                FrameOrder::InOrder
            }
        };
        (order, self.flush(now))
    }

    /// The pump saw `frame_index`'s last shard arrive short of what its parity can
    /// rebuild. Asks now, a frame interval before the next frame shows the gap, so the
    /// host's next encode is the anchor. A frame the decoder already passed, or one
    /// asked for before, asks nothing new.
    pub(crate) fn tail_short(&mut self, frame_index: u32, now: Instant) -> RecoveryAsk {
        let passed = self
            .next_expected
            .is_some_and(|exp| ahead_of(exp, frame_index));
        if !passed && !self.asked.contains(&frame_index) {
            if self.asked.len() == ASKED_TAILS {
                self.asked.pop_front();
            }
            self.asked.push_back(frame_index);
            self.widen((frame_index, frame_index));
        }
        self.flush(now)
    }

    /// `[first, last]` less the frames asked for at their tail at either end; `None`
    /// when every one was.
    fn unasked(&self, mut first: u32, mut last: u32) -> Option<(u32, u32)> {
        while self.asked.contains(&first) {
            if first == last {
                return None;
            }
            first = first.wrapping_add(1);
        }
        while self.asked.contains(&last) {
            last = last.wrapping_sub(1);
        }
        Some((first, last))
    }

    /// Merge `lost` into the pending range: oldest first, newest last.
    fn widen(&mut self, (first, last): (u32, u32)) {
        self.pending = Some(match self.pending {
            Some((f, l)) => (
                if ahead_of(f, first) { first } else { f },
                if ahead_of(last, l) { last } else { l },
            ),
            None => (first, last),
        });
    }

    /// The decoder took `frame_index`, an anchor: the gap it ends needs no ask.
    pub(crate) fn anchored(&mut self, frame_index: u32) {
        self.anchored = Some(frame_index);
    }

    /// An IDR was asked for: it repairs every frame the pending range names. The RFI
    /// throttle keeps its own window; an IDR ask does not hold back the next RFI.
    pub(crate) fn keyframe_requested(&mut self) {
        self.pending = None;
    }

    /// No ask of either kind went out in the last [`RFI_THROTTLE`].
    fn window_open(&self, now: Instant) -> bool {
        self.last_req
            .is_none_or(|t| now.duration_since(t) >= RFI_THROTTLE)
    }

    /// An IDR went out through the one window: it repairs the pending range and shuts the
    /// window, so an RFI inside it waits instead of replacing the IDR ask's repeats.
    fn keyframe_sent(&mut self, now: Instant) {
        self.pending = None;
        self.last_req = Some(now);
    }

    /// The pending range as an ask once the throttle window is open, else `None`.
    pub(crate) fn flush(&mut self, now: Instant) -> RecoveryAsk {
        let throttled = self
            .last_req
            .is_some_and(|t| now.duration_since(t) < RFI_THROTTLE);
        let Some((first, last)) = self.pending.filter(|_| !throttled) else {
            return RecoveryAsk::None;
        };
        self.pending = None;
        self.last_req = Some(now);
        if last.wrapping_sub(first).wrapping_add(1) > crate::packet::RFI_MAX_RANGE {
            RecoveryAsk::Keyframe
        } else {
            RecoveryAsk::Rfi(first, last)
        }
    }
}

/// What the decoder reports for one picture
/// ([`NativeClient::recovery_on_decoded`](super::NativeClient::recovery_on_decoded)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decoded {
    /// The AU's wire flags.
    pub flags: u32,
    /// The decoder's own IDR bit; `false` where it does not flag keys (`FLAG_SOF` covers it).
    pub keyframe: bool,
    /// Whether this picture predicts from a complete reference chain, on a lane that knows.
    pub evidence: AnchorEvidence,
    /// The lane's recovery-point SEI view; [`LocalRecovery::NONE`] without a parser.
    pub local: LocalRecovery,
    /// This picture's decode order, on a lane that numbers decodes. One at or below the
    /// latest arm's watermark was decoded before the loss, so its SEI cannot lift it.
    pub decode_order: Option<u64>,
}

/// What [`NativeClient::recovery_on_au`](super::NativeClient::recovery_on_au) says of one AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrival {
    /// Feed it. `false`: a straggler a reference decoder rewinds on, or a partial a newer
    /// frame beat.
    pub admit: bool,
    /// Frames lost ahead of this AU, `0` for none: the gap
    /// [`AuAdmission::note`](crate::reanchor::AuAdmission::note) takes.
    pub gap: u32,
}

/// What the embedder does with a decoded picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Shown {
    /// Keep the last good picture on glass instead of this one.
    pub hold: bool,
    /// The freeze lifted on an intra-refresh heal, which a reference-chain ledger cannot
    /// see: forget the chain's damage marks.
    pub forgive_chain: bool,
}

/// One observation for [`RecoveryDriver::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryEvent {
    /// One AU off the wire, before decode. `decode_order` is the decoder's count so far.
    Arrived {
        index: u32,
        complete: bool,
        decode_order: u64,
    },
    Decoded(Decoded),
    /// An AU produced no picture.
    NoOutput,
    /// A loss with no index gap: a parked-AU overflow, a wedge.
    Lost,
    /// The decoder owes output with nothing lost: armed and asked as [`Self::Lost`].
    Starved,
    /// The decoder raised an error it recovers from: its next pictures may be concealment.
    CodecError,
    /// The decoder asks (a rebuild, a demotion, a damaged AU).
    NeedKeyframe,
    /// The pump saw `index`'s tail arrive short of what its parity rebuilds.
    TailShort {
        index: u32,
    },
    /// The pump's pass, with the reassembler's drop count.
    Tick {
        frames_dropped: u64,
    },
}

/// What one [`RecoveryEvent`] asks of the embedder and the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecoveryStep {
    /// `Arrived`: feed the AU. `false` drops a straggler a reference decoder rewinds on, or
    /// a partial a newer frame beat.
    pub(crate) admit: bool,
    /// `Arrived`: frames lost ahead of the AU.
    pub(crate) gap: u32,
    /// The freeze is up after this event.
    pub(crate) hold: bool,
    pub(crate) forgive_chain: bool,
    /// Performed by [`NativeClient`](super::NativeClient), never by an embedder.
    pub(crate) ask: RecoveryAsk,
}

/// One place for the rules every client used to rebuild: the freeze gate, the RFI asks,
/// the one ask window, the straggler and late-partial drops and the arm watermark.
/// Pure: `now` is passed in. [`NativeClient`](super::NativeClient) holds it behind one
/// lock, feeds the decoder's reports and the pump's ticks, and sends what it asks for.
#[derive(Default)]
pub(crate) struct RecoveryDriver {
    /// `None` while no decoder reports: before the first `recovery_on_*`, after
    /// [`Self::reset`], and on a lane that keeps no freeze ([`Self::observe`]). The tick
    /// then polls nothing, so a parked decoder costs no keyframe.
    gate: Option<ReanchorGate>,
    rfi: RfiRecovery,
    /// Newest index handed to the decoder: a partial behind it is the stale past.
    newest: Option<u32>,
    /// Decode-order watermark at the latest arm, and the arm count it was stamped at.
    arm_order: u64,
    arms_seen: u64,
    /// PyroWave: every AU stands alone, so a straggler still decodes.
    all_intra: bool,
    /// The client raised the freeze, not a loss: its asks are counted apart.
    own: bool,
}

impl RecoveryDriver {
    pub(crate) fn set_all_intra(&mut self, all_intra: bool) {
        self.all_intra = all_intra;
    }

    /// The embedder reports through the driver from here on. Seeds the gate with the drop
    /// count so the first tick is not a loss.
    pub(crate) fn engage(&mut self, frames_dropped: u64) {
        if self.gate.is_none() {
            self.gate = Some(ReanchorGate::new(frames_dropped));
        }
    }

    /// The decoder is gone or parked: no gate until the next report starts a fresh one. The
    /// lost range an RFI still owes stays.
    pub(crate) fn reset(&mut self) {
        self.gate = None;
        self.arm_order = 0;
        self.arms_seen = 0;
        self.own = false;
    }

    /// The freeze's arm count; `0` with no gate.
    pub(crate) fn arms(&self) -> u64 {
        self.gate.as_ref().map_or(0, ReanchorGate::arms)
    }

    /// Fold one observation. An ask is recovery's when a loss or a damaged picture raised or
    /// re-armed the freeze it serves: a gap, a drop climb, a lost AU, a damaged chain, a
    /// codec error. A freeze the client raised on an unbroken stream (a starved decoder, a
    /// no-output streak, a decoder asking for a keyframe) asks as
    /// [`RecoveryAsk::StarvedKeyframe`], and its own re-arms leave a loss's freeze the
    /// loss's. With no freeze, an ask is recovery's: a lane that keeps none asks after its
    /// losses.
    pub(crate) fn step(&mut self, ev: RecoveryEvent, now: Instant) -> RecoveryStep {
        let mut admit = true;
        let mut gap = 0;
        let mut forgive_chain = false;
        let mut want_keyframe = false;
        let mut ask = RecoveryAsk::None;
        let arms = self.arms();
        let held = self.gate.as_ref().is_some_and(ReanchorGate::is_holding);
        match ev {
            RecoveryEvent::Arrived {
                index,
                complete,
                decode_order,
            } => (admit, gap, ask) = self.arrived(index, complete, decode_order, now),
            RecoveryEvent::Decoded(d) => (forgive_chain, want_keyframe) = self.decoded(d, now),
            RecoveryEvent::NoOutput => {
                want_keyframe = self.gate.as_mut().is_some_and(|g| g.on_no_output(now));
            }
            // A codec error is held, not asked for: an anchor on its way lifts it, else the
            // backstop asks.
            RecoveryEvent::Lost | RecoveryEvent::Starved | RecoveryEvent::CodecError => {
                if let Some(g) = &mut self.gate {
                    g.arm(now);
                }
                want_keyframe = ev != RecoveryEvent::CodecError;
            }
            RecoveryEvent::NeedKeyframe => match &mut self.gate {
                // The flag fires per damaged AU; a new loss re-arms by gap or climb.
                Some(g) => {
                    if !g.is_holding() {
                        g.arm(now);
                    }
                    want_keyframe = true;
                }
                // No freeze to arm: the ask goes at once, outside the window.
                None => {
                    self.rfi.keyframe_requested();
                    ask = RecoveryAsk::Keyframe;
                }
            },
            RecoveryEvent::TailShort { index } => ask = self.rfi.tail_short(index, now),
            RecoveryEvent::Tick { frames_dropped } => {
                want_keyframe = self
                    .gate
                    .as_mut()
                    .is_some_and(|g| g.poll(frames_dropped, now));
            }
        }
        if self.arms() != arms {
            let own = matches!(
                ev,
                RecoveryEvent::Starved | RecoveryEvent::NoOutput | RecoveryEvent::NeedKeyframe
            );
            self.own = own && (self.own || !held);
        }
        if want_keyframe {
            ask = self.keyframe(now);
        }
        let own = match ev {
            RecoveryEvent::Starved => true,
            RecoveryEvent::NoOutput | RecoveryEvent::NeedKeyframe | RecoveryEvent::Tick { .. } => {
                self.own
            }
            _ => false,
        };
        if own && ask == RecoveryAsk::Keyframe {
            ask = RecoveryAsk::StarvedKeyframe;
        }
        // The pending range goes after the keyframe decision, never before it.
        if ask == RecoveryAsk::None && matches!(ev, RecoveryEvent::Tick { .. }) {
            ask = self.rfi.flush(now);
        }
        RecoveryStep {
            admit,
            gap,
            hold: self.gate.as_ref().is_some_and(ReanchorGate::is_holding),
            forgive_chain,
            ask,
        }
    }

    /// One AU before decode: `(admit, gap, ask)`. A gap asks for the range and arms with
    /// credit; a straggler a reference decoder rewinds on, or a partial behind the newest
    /// decoded frame, is not admitted. An admitted AU stamps the watermark when the gate
    /// re-armed.
    fn arrived(
        &mut self,
        index: u32,
        complete: bool,
        decode_order: u64,
        now: Instant,
    ) -> (bool, u32, RecoveryAsk) {
        let (order, ask) = self.rfi.observe(index, now);
        let mut admit = true;
        let mut lost = 0;
        match order {
            // Credited: the reassembler books these frames ~120 ms from now.
            FrameOrder::Gap(gap) => {
                lost = gap;
                if let Some(g) = &mut self.gate {
                    g.arm_expecting_drops(now, u64::from(gap));
                }
            }
            FrameOrder::Straggler if !self.all_intra => admit = false,
            FrameOrder::InOrder | FrameOrder::Straggler => {}
        }
        if admit
            && !complete
            && self
                .newest
                .is_some_and(|n| n.wrapping_sub(index) <= u32::MAX / 2)
        {
            admit = false;
        }
        if admit {
            self.newest = Some(match self.newest {
                Some(n) if index.wrapping_sub(n) > u32::MAX / 2 => n,
                _ => index,
            });
            // Stamped before this AU decodes: the newest picture at the arm.
            if let Some(g) = &self.gate {
                if g.arms() != self.arms_seen {
                    self.arms_seen = g.arms();
                    self.arm_order = decode_order;
                }
            }
        }
        (admit, lost, ask)
    }

    /// One decoded picture: `(forgive_chain, want_keyframe)`. A recovery SEI on a picture
    /// decoded before the arm cannot lift; a damaged chain that reaches an open gate holds
    /// and asks now, not at the backstop.
    fn decoded(&mut self, d: Decoded, now: Instant) -> (bool, bool) {
        let Some(g) = &mut self.gate else {
            return (false, false);
        };
        let local = if d.decode_order.is_some_and(|o| o <= self.arm_order) {
            LocalRecovery::NONE
        } else {
            d.local
        };
        let mut forgive = g.on_local_recovery(local);
        let was = g.is_holding();
        let present =
            g.on_decoded_corroborated(d.flags, d.keyframe, d.evidence, now) == GateVerdict::Present;
        forgive |= was && present && g.lifted_by_marks();
        let want_keyframe = !present && !was;
        if want_keyframe {
            tracing::debug!("damaged reference chain reached an open gate — holding");
        }
        (forgive, want_keyframe)
    }

    /// A keyframe through the one window. Shut: dropped, and the backstop re-asks.
    fn keyframe(&mut self, now: Instant) -> RecoveryAsk {
        if !self.rfi.window_open(now) {
            return RecoveryAsk::None;
        }
        self.rfi.keyframe_sent(now);
        RecoveryAsk::Keyframe
    }

    /// One AU on a lane that keeps no freeze (an all-intra decoder, a recorder): the RFI
    /// side alone.
    pub(crate) fn observe(&mut self, index: u32, now: Instant) -> (FrameOrder, RecoveryAsk) {
        self.rfi.observe(index, now)
    }

    pub(crate) fn anchored(&mut self, index: u32) {
        self.rfi.anchored(index);
    }
}

/// The frames the decoder took clean, in its order: an IDR, an anchor, a wave's close after an
/// unbroken sweep, or the frame right after a clean one. The newest and the sixteen before it
/// ride [`Feedback::acked`]; the host keeps one of these resident as the next loss's anchor.
#[derive(Default)]
pub(crate) struct AckChain {
    prev: Option<u32>,
    clean: bool,
    /// Every frame since a wave's start followed the one before it.
    sweep: bool,
    last: Option<u32>,
    mask: u16,
}

impl AckChain {
    /// Fold one whole AU as the decoder takes it; `Some` when it decodes clean.
    pub(crate) fn decoded(&mut self, index: u32, flags: u32) -> Option<(u32, u16)> {
        if self.prev.is_some_and(|p| !ahead_of(index, p)) {
            return None;
        }
        let follows = self.prev.is_some_and(|p| p.wrapping_add(1) == index);
        self.prev = Some(index);
        use crate::packet::{USER_FLAG_RECOVERY_CLOSE as CLOSE, USER_FLAG_RECOVERY_POINT as POINT};
        let healed = flags & CLOSE != 0 && follows && self.sweep;
        self.sweep = if flags & POINT != 0 {
            flags & CLOSE == 0
        } else {
            follows && self.sweep
        };
        let fresh = flags
            & (u32::from(crate::packet::FLAG_SOF) | crate::packet::USER_FLAG_RECOVERY_ANCHOR)
            != 0;
        self.clean = fresh || healed || (follows && self.clean);
        if !self.clean {
            return None;
        }
        self.mask = match self.last.map(|l| index.wrapping_sub(l)) {
            Some(d @ 1..=16) => ((u32::from(self.mask) << d) | 1 << (d - 1)) as u16,
            _ => 0,
        };
        self.last = Some(index);
        Some((index, self.mask))
    }
}

/// One minute, the span of the host's `link health … rfi=` line.
const RECENT_SPAN: Duration = Duration::from_secs(60);

/// When each RFI in the last [`RECENT_SPAN`] went out, for the overlay.
#[derive(Default)]
pub(crate) struct RecentRfis(std::collections::VecDeque<Instant>);

impl RecentRfis {
    pub(crate) fn note(&mut self, now: Instant) {
        self.count(now);
        self.0.push_back(now);
    }

    /// RFIs sent in the minute before `now`. Ages out the older ones.
    pub(crate) fn count(&mut self, now: Instant) -> u32 {
        while self
            .0
            .front()
            .is_some_and(|&t| now.duration_since(t) >= RECENT_SPAN)
        {
            self.0.pop_front();
        }
        self.0.len() as u32
    }
}

/// Gaps remembered for the RFI line. More than the frame queue holds before
/// jump-to-live, so a gap is still here when the decoder reaches it.
const SHORT_FRAMES: usize = 16;

/// Frames the pump saw short, each with `(missing, recovery)` from
/// [`crate::session::Session::missing_beyond_parity`]. The pump writes on a short tail
/// or a forward gap; [`NativeClient::request_rfi`] reads the first frame of its range.
/// Both are rare, so a lock is fine.
///
/// [`NativeClient::request_rfi`]: super::NativeClient::request_rfi
#[derive(Default)]
pub(crate) struct ShortFrames(std::collections::VecDeque<(u32, u32, u32)>);

impl ShortFrames {
    pub(crate) fn note(&mut self, frame_index: u32, missing: u32, recovery: u32) {
        if self.0.len() == SHORT_FRAMES {
            self.0.pop_front();
        }
        self.0.push_back((frame_index, missing, recovery));
    }

    /// `(missing, recovery)` for `frame_index`, if the pump saw it short.
    pub(crate) fn get(&self, frame_index: u32) -> Option<(u32, u32)> {
        self.0
            .iter()
            .rev()
            .find(|e| e.0 == frame_index)
            .map(|&(_, missing, recovery)| (missing, recovery))
    }
}

/// Moves `last` to `idx` when `idx` is newer and returns the first index the jump
/// skipped. A repeat (a later part of the same AU) and a straggler leave `last`.
pub(crate) fn first_skipped(last: &mut Option<u32>, idx: u32) -> Option<u32> {
    let prev = last.replace(idx)?;
    let ahead = idx.wrapping_sub(prev);
    if ahead == 0 || ahead >= u32::MAX / 2 {
        *last = Some(prev);
        return None;
    }
    (ahead > 1).then(|| prev.wrapping_add(1))
}

/// How long an unanswered ask repeats. The answer costs a round trip and an encode; past
/// this the decoder's own backstop asks again under a new number.
pub(crate) const ASK_GIVE_UP: Duration = Duration::from_millis(500);

/// The client's feedback datagrams: a window number per report, an ask number per recovery
/// ask. Every datagram carries the levels: the last closed window with its fields, the link
/// rate and the shape, so a lost report reaches the host with the next ack or ask copy. An
/// ask repeats a frame interval apart until the frame that answers it arrives. A host acts
/// on each window and ask number once however many copies arrive.
pub(crate) struct FeedbackOut {
    /// The last closed window: its number (`0` before the first) and fields.
    report: Feedback,
    ask: u16,
    open: Option<OpenAsk>,
    link_kbps: u32,
    /// The shape every datagram asks for (`Feedback::shape`).
    shape: u8,
    /// The newest frame handed on. An answer is a frame after the one the ask saw.
    newest: Option<u32>,
    interval: Duration,
    acks: AckChain,
}

impl Default for FeedbackOut {
    fn default() -> Self {
        FeedbackOut {
            report: Feedback::default(),
            ask: 0,
            open: None,
            link_kbps: 0,
            shape: 0,
            newest: None,
            interval: Duration::from_micros(1_000_000 / 60),
            acks: AckChain::default(),
        }
    }
}

struct OpenAsk {
    invalidate: Option<(u32, u32)>,
    keyframe: bool,
    nack: Option<crate::quic::v2::dgram::Nack>,
    /// The newest frame when the ask went out.
    after: Option<u32>,
    next: Instant,
    until: Instant,
}

impl FeedbackOut {
    /// The stream's refresh: an open ask repeats once per frame.
    pub(crate) fn set_refresh(&mut self, hz: u32) {
        self.interval = Duration::from_micros(1_000_000 / u64::from(hz.clamp(30, 240)));
    }

    /// One report window, numbered: `report` holds only the window's fields. It is the level
    /// every datagram carries until the next window closes.
    pub(crate) fn window(&mut self, report: Feedback) -> Feedback {
        self.report = Feedback {
            window: self.report.window.wrapping_add(1).max(1),
            ..report
        };
        self.stamp()
    }

    /// The link rate, a level every datagram carries. `Some` when it changed: one datagram
    /// tells the host at once.
    pub(crate) fn link(&mut self, kbps: u32) -> Option<Feedback> {
        (self.link_kbps != kbps).then(|| {
            self.link_kbps = kbps;
            self.stamp()
        })
    }

    /// The shape the client asks for, a level the next datagram carries.
    pub(crate) fn shape(&mut self, shape: u8) {
        self.shape = shape;
    }

    /// A new ask under a new number. It replaces the open one.
    pub(crate) fn ask(
        &mut self,
        invalidate: Option<(u32, u32)>,
        keyframe: bool,
        now: Instant,
    ) -> Feedback {
        self.open(invalidate, keyframe, None, now)
    }

    /// Shards of one frame to send again, as a new ask. Only that frame arriving whole
    /// answers it.
    pub(crate) fn nack(&mut self, nack: crate::quic::v2::dgram::Nack, now: Instant) -> Feedback {
        self.open(None, false, Some(nack), now)
    }

    fn open(
        &mut self,
        invalidate: Option<(u32, u32)>,
        keyframe: bool,
        nack: Option<crate::quic::v2::dgram::Nack>,
        now: Instant,
    ) -> Feedback {
        self.ask = self.ask.wrapping_add(1).max(1);
        self.open = Some(OpenAsk {
            invalidate,
            keyframe,
            nack,
            after: self.newest,
            next: now + self.interval,
            until: now + ASK_GIVE_UP,
        });
        self.stamp()
    }

    /// A frame was handed on. A keyframe answers any ask; a recovery anchor or point
    /// answers an RFI. Either only when it came after the ask.
    pub(crate) fn on_frame(&mut self, index: u32, flags: u32) {
        if self.newest.is_none_or(|n| ahead_of(index, n)) {
            self.newest = Some(index);
        }
        let Some(o) = &self.open else { return };
        if let Some(n) = o.nack {
            if index == n.frame {
                self.open = None;
            }
            return;
        }
        let key = flags & u32::from(crate::packet::FLAG_SOF) != 0;
        let anchor = flags
            & (crate::packet::USER_FLAG_RECOVERY_ANCHOR | crate::packet::USER_FLAG_RECOVERY_POINT)
            != 0;
        let after = o.after.is_none_or(|a| ahead_of(index, a));
        if after && (key || (anchor && !o.keyframe)) {
            self.open = None;
        }
    }

    /// The decoder took a whole AU: its acknowledgement when it decodes clean
    /// ([`AckChain`]). Carries the levels, the last window among them, never an ask.
    pub(crate) fn decoded(&mut self, index: u32, flags: u32) -> Option<Feedback> {
        let acked = self.acks.decoded(index, flags)?;
        Some(Feedback {
            acked: Some(acked),
            ..self.levels()
        })
    }

    /// The open ask's next copy, when one is due. An ask nothing answered closes at
    /// [`ASK_GIVE_UP`].
    pub(crate) fn resend(&mut self, now: Instant) -> Option<Feedback> {
        let o = self.open.as_mut()?;
        if now >= o.until {
            self.open = None;
            return None;
        }
        if now < o.next {
            return None;
        }
        o.next = now + self.interval;
        Some(self.stamp())
    }

    /// What the last datagram said, again: the host acts on its numbers once, so a copy only
    /// shows it where the client is.
    pub(crate) fn again(&self) -> Feedback {
        self.stamp()
    }

    /// The last closed window, the link rate and the shape.
    fn levels(&self) -> Feedback {
        Feedback {
            link_kbps: self.link_kbps,
            shape: self.shape,
            ..self.report
        }
    }

    /// The levels and the open ask.
    fn stamp(&self) -> Feedback {
        let mut fb = self.levels();
        if let Some(o) = &self.open {
            fb.ask = self.ask;
            fb.invalidate = o.invalidate;
            fb.keyframe = o.keyframe;
            fb.nack = o.nack;
        }
        fb
    }
}

#[cfg(test)]
mod rfi_recovery_tests {
    use super::{
        first_skipped, FrameOrder::*, RecentRfis, RecoveryAsk, RfiRecovery, ShortFrames,
        RFI_THROTTLE, SHORT_FRAMES,
    };
    use std::time::{Duration, Instant};

    // Offsets from this Instant model the throttle window; do not sleep.
    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn first_frame_arms_without_a_gap() {
        let mut r = RfiRecovery::default();
        assert_eq!(r.observe(100, base()), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(101));
    }

    #[test]
    fn contiguous_frames_never_gap() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(101, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(102, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(103, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(104));
    }

    #[test]
    fn forward_gap_reports_the_exact_lost_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(105, t), (Gap(4), RecoveryAsk::Rfi(101, 104)));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn single_frame_drop_names_a_unit_range() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (Gap(1), RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn throttle_suppresses_bursts_then_re_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(105, t0), (Gap(4), RecoveryAsk::Rfi(101, 104)));
        assert_eq!(
            r.observe(110, t0 + Duration::from_millis(50)),
            (Gap(4), RecoveryAsk::None)
        );
        // The swallowed gap widens the next ask instead of vanishing.
        assert_eq!(
            r.observe(120, t0 + RFI_THROTTLE + Duration::from_millis(1)),
            (Gap(9), RecoveryAsk::Rfi(106, 119))
        );
    }

    #[test]
    fn a_swallowed_gap_is_sent_by_the_next_frame_after_the_window() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        // The recovery anchor itself is lost: a second gap inside the window.
        assert_eq!(
            r.observe(104, t0 + Duration::from_millis(30)),
            (Gap(1), RecoveryAsk::None)
        );
        assert_eq!(
            r.observe(105, t0 + Duration::from_millis(60)),
            (InOrder, RecoveryAsk::None)
        );
        assert_eq!(
            r.observe(106, t0 + RFI_THROTTLE),
            (InOrder, RecoveryAsk::Rfi(103, 103))
        );
        assert_eq!(
            r.observe(107, t0 + RFI_THROTTLE),
            (InOrder, RecoveryAsk::None)
        );
    }

    #[test]
    fn a_swallowed_gap_is_sent_by_a_tick_once_the_window_opens() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        assert_eq!(
            r.observe(104, t0 + Duration::from_millis(30)),
            (Gap(1), RecoveryAsk::None)
        );
        // No frame arrives after the second gap; the pump's tick still sends it.
        assert_eq!(r.flush(t0 + Duration::from_millis(60)), RecoveryAsk::None);
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::Rfi(103, 103));
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::None);
    }

    /// An IDR ask drops the pending range but leaves the RFI window alone: a new
    /// gap right after an IDR ask still asks at once.
    #[test]
    fn a_keyframe_ask_clears_the_pending_range_but_not_the_rfi_window() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        r.keyframe_requested();
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        // Swallowed by the RFI window, then repaired by the IDR.
        assert_eq!(
            r.observe(104, t0 + Duration::from_millis(30)),
            (Gap(1), RecoveryAsk::None)
        );
        r.keyframe_requested();
        assert_eq!(
            r.observe(105, t0 + RFI_THROTTLE),
            (InOrder, RecoveryAsk::None)
        );
    }

    #[test]
    fn a_reordered_pair_is_one_gap_then_a_straggler() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        // The late 101 is a straggler: no gap, expectation untouched.
        assert_eq!(r.observe(101, t), (Straggler, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(103));
        assert_eq!(r.observe(103, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(104));
    }

    #[test]
    fn stragglers_behind_the_delivery_point_are_ignored() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        r.observe(105, t);
        assert_eq!(r.observe(103, t), (Straggler, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(106));
    }

    #[test]
    fn wraparound_is_contiguous_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t);
        assert_eq!(r.observe(u32::MAX, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(0));
        assert_eq!(r.observe(0, t), (InOrder, RecoveryAsk::None));
        assert_eq!(r.next_expected, Some(1));
    }

    #[test]
    fn gap_range_wraps_across_u32_max() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(u32::MAX - 1, t);
        assert_eq!(r.observe(1, t), (Gap(2), RecoveryAsk::Rfi(u32::MAX, 0)));
        assert_eq!(r.next_expected, Some(2));
    }

    /// A gap an anchor ends asks nothing: the anchor references only frames this client
    /// holds. The same gap ended by a plain frame asks as before.
    #[test]
    fn a_gap_an_anchor_ends_asks_nothing() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(10, t);
        r.anchored(12);
        assert_eq!(r.observe(12, t), (Gap(1), RecoveryAsk::None));
        assert_eq!(
            r.observe(15, t + Duration::from_millis(200)),
            (Gap(2), RecoveryAsk::Rfi(13, 14))
        );
    }

    #[test]
    fn huge_gap_resyncs_via_keyframe_not_rfi() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        let jump = 100 + crate::packet::RFI_MAX_RANGE + 2;
        assert_eq!(r.observe(jump, t), (Gap(jump - 101), RecoveryAsk::Keyframe));
        assert_eq!(r.next_expected, Some(jump + 1));
        assert_eq!(r.observe(jump + 1, t), (InOrder, RecoveryAsk::None));
        // Keyframe stamps last_req too; an immediate follow-up gap stays quiet.
        assert_eq!(
            r.observe(jump + 10, t + Duration::from_millis(1)),
            (Gap(8), RecoveryAsk::None)
        );
    }

    #[test]
    fn a_short_tail_asks_before_the_gap_and_the_gap_asks_nothing() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.tail_short(101, t), RecoveryAsk::Rfi(101, 101));
        let later = t + RFI_THROTTLE;
        assert_eq!(r.observe(102, later), (Gap(1), RecoveryAsk::None));
        assert_eq!(r.flush(later), RecoveryAsk::None);
    }

    #[test]
    fn a_gap_wider_than_the_short_tails_asks_for_the_rest() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.tail_short(101, t), RecoveryAsk::Rfi(101, 101));
        let later = t + RFI_THROTTLE;
        assert_eq!(r.observe(103, later), (Gap(2), RecoveryAsk::Rfi(102, 102)));
        let mut r = RfiRecovery::default();
        r.observe(100, t);
        assert_eq!(r.tail_short(102, t), RecoveryAsk::Rfi(102, 102));
        assert_eq!(r.observe(103, later), (Gap(2), RecoveryAsk::Rfi(101, 101)));
    }

    #[test]
    fn a_throttled_short_tail_goes_out_once() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(100, t0);
        assert_eq!(r.observe(102, t0), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        let t1 = t0 + Duration::from_millis(30);
        assert_eq!(r.tail_short(105, t1), RecoveryAsk::None);
        assert_eq!(r.observe(103, t1), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(104, t1), (InOrder, RecoveryAsk::None));
        assert_eq!(r.observe(106, t1), (Gap(1), RecoveryAsk::None));
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::Rfi(105, 105));
        assert_eq!(r.flush(t0 + RFI_THROTTLE * 2), RecoveryAsk::None);
    }

    #[test]
    fn a_short_tail_the_decoder_passed_or_already_asked_asks_nothing() {
        let mut r = RfiRecovery::default();
        let t = base();
        r.observe(100, t);
        assert_eq!(r.observe(102, t), (Gap(1), RecoveryAsk::Rfi(101, 101)));
        let later = t + RFI_THROTTLE;
        assert_eq!(r.tail_short(101, later), RecoveryAsk::None, "passed");
        assert_eq!(r.tail_short(103, later), RecoveryAsk::Rfi(103, 103));
        let much_later = later + RFI_THROTTLE;
        assert_eq!(r.tail_short(103, much_later), RecoveryAsk::None, "asked");
    }

    #[test]
    fn short_tails_merge_across_the_wrap() {
        let mut r = RfiRecovery::default();
        let t0 = base();
        r.observe(u32::MAX - 3, t0);
        assert_eq!(
            r.tail_short(u32::MAX - 2, t0),
            RecoveryAsk::Rfi(u32::MAX - 2, u32::MAX - 2)
        );
        let t1 = t0 + Duration::from_millis(30);
        assert_eq!(r.tail_short(0, t1), RecoveryAsk::None);
        assert_eq!(r.tail_short(u32::MAX, t1), RecoveryAsk::None);
        assert_eq!(r.flush(t0 + RFI_THROTTLE), RecoveryAsk::Rfi(u32::MAX, 0));
    }

    #[test]
    fn a_jump_names_its_first_skipped_frame_and_parts_or_stragglers_do_not() {
        let mut last = None;
        assert_eq!(first_skipped(&mut last, 100), None);
        assert_eq!(first_skipped(&mut last, 101), None);
        assert_eq!(first_skipped(&mut last, 101), None, "a later part");
        assert_eq!(first_skipped(&mut last, 104), Some(102));
        assert_eq!(first_skipped(&mut last, 102), None, "a straggler");
        assert_eq!(last, Some(104));
        let mut last = Some(u32::MAX);
        assert_eq!(first_skipped(&mut last, 1), Some(0));
    }

    #[test]
    fn short_frames_keep_the_newest_and_forget_the_oldest() {
        let mut s = ShortFrames::default();
        for i in 0..=SHORT_FRAMES as u32 {
            s.note(i, i + 1, 2);
        }
        assert_eq!(s.get(0), None, "evicted");
        assert_eq!(s.get(1), Some((2, 2)));
        assert_eq!(s.get(SHORT_FRAMES as u32 + 1), None, "never short");
    }

    #[test]
    fn recent_rfis_age_out_after_a_minute() {
        let t0 = base();
        let at = |s| t0 + Duration::from_secs(s);
        let mut r = RecentRfis::default();
        assert_eq!(r.count(t0), 0);
        r.note(t0);
        r.note(at(30));
        assert_eq!(r.count(at(59)), 2);
        assert_eq!(r.count(at(60)), 1);
        r.note(at(61));
        assert_eq!(r.count(at(89)), 2);
        assert_eq!(r.count(at(121)), 0);
    }
}

#[cfg(test)]
mod feedback_out_tests {
    use super::*;
    use crate::packet::{FLAG_PIC, FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR};

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    /// An RFI repeats once a frame interval under one number while frames go by unanswered,
    /// and falls silent when the anchor after it arrives. A window carries the open ask and
    /// the link rate, and no ask once it closed.
    #[test]
    fn an_ask_repeats_each_frame_until_its_anchor_arrives() {
        let t0 = Instant::now();
        let mut out = FeedbackOut::default();
        out.set_refresh(60);
        out.link(940_000).expect("a new level goes out");
        assert_eq!(out.link(940_000), None, "an unchanged one does not");
        out.on_frame(10, FLAG_PIC.into());
        let first = out.ask(Some((7, 9)), false, t0);
        assert_eq!(
            (first.ask, first.invalidate, first.window),
            (1, Some((7, 9)), 0)
        );
        assert_eq!(out.resend(ms(t0, 10)), None, "not inside the interval");
        let mut copies = 0;
        for f in 11..15u32 {
            out.on_frame(f, FLAG_PIC.into());
            let c = out
                .resend(ms(t0, 17 * u64::from(f - 10)))
                .expect("a copy per frame");
            assert_eq!(
                (c.ask, c.invalidate, c.link_kbps),
                (1, Some((7, 9)), 940_000)
            );
            copies += 1;
        }
        assert_eq!(copies, 4);
        out.on_frame(15, FLAG_PIC as u32 | USER_FLAG_RECOVERY_ANCHOR);
        assert_eq!(
            out.resend(ms(t0, 120)),
            None,
            "silent once the anchor arrived"
        );
        let w = out.window(Feedback {
            loss_ppm: 300,
            packets_received: 9,
            ..Default::default()
        });
        assert_eq!(
            (w.window, w.ask, w.loss_ppm, w.link_kbps),
            (1, 0, 300, 940_000)
        );
    }

    /// A NACK repeats like any ask until its own frame arrives; frames after it answer
    /// nothing. The RFI after it replaces it whole.
    /// The decoder's clean frames are acknowledged with the sixteen before them; a gap
    /// stops them until an anchor or an IDR, and a straggler never counts.
    #[test]
    fn a_clean_chain_is_acknowledged_and_a_gap_breaks_it_until_an_anchor() {
        use crate::packet::FLAG_SOF;
        let mut out = FeedbackOut::default();
        let mut acked = |i, f| out.decoded(i, f).and_then(|fb| fb.acked);
        assert_eq!(acked(5, 0), None, "nothing clean before an IDR");
        assert_eq!(acked(6, FLAG_SOF as u32), Some((6, 0)));
        assert_eq!(acked(7, 0), Some((7, 0b1)));
        assert_eq!(acked(8, 0), Some((8, 0b11)));
        assert_eq!(acked(10, 0), None, "9 never came");
        assert_eq!(acked(9, 0), None, "a straggler");
        assert_eq!(acked(11, 0), None, "still on the broken chain");
        assert_eq!(acked(13, USER_FLAG_RECOVERY_ANCHOR), Some((13, 0b111_0000)));
        assert_eq!(acked(14, 0), Some((14, 0b1110_0001)));
        assert_eq!(acked(40, USER_FLAG_RECOVERY_ANCHOR), Some((40, 0)));
    }

    /// A wave's close restarts the chain once the client saw its start and every frame after
    /// it; a gap inside the sweep leaves the close unconfirmed.
    #[test]
    fn a_whole_wave_restarts_the_chain() {
        use crate::packet::{USER_FLAG_RECOVERY_CLOSE as CLOSE, USER_FLAG_RECOVERY_POINT as POINT};
        let mut out = FeedbackOut::default();
        let mut acked = |i, f| out.decoded(i, f).and_then(|fb| fb.acked);
        assert_eq!(acked(5, 0), None, "a broken chain");
        assert_eq!(acked(6, POINT), None, "the start leans on the damage");
        assert_eq!(acked(7, 0), None);
        assert_eq!(acked(8, POINT | CLOSE), Some((8, 0)));
        assert_eq!(acked(9, 0), Some((9, 0b1)));
        assert_eq!(acked(20, 0), None, "a gap");
        assert_eq!(acked(21, POINT), None);
        assert_eq!(acked(23, 0), None, "22 never came");
        assert_eq!(acked(24, POINT | CLOSE), None, "the sweep had a gap");
    }

    #[test]
    fn a_nack_repeats_until_its_frame_arrives() {
        let t0 = Instant::now();
        let mut out = FeedbackOut::default();
        let n = crate::quic::v2::dgram::Nack::new(40, &[5, 6]).unwrap();
        let first = out.nack(n, t0);
        assert_eq!(
            (first.ask, first.nack, first.invalidate),
            (1, Some(n), None)
        );
        out.on_frame(41, u32::from(FLAG_SOF));
        let copy = out
            .resend(ms(t0, 17))
            .expect("a later keyframe answers nothing");
        assert_eq!(copy.nack, Some(n));
        out.on_frame(40, FLAG_PIC.into());
        assert_eq!(out.resend(ms(t0, 34)), None, "its frame arrived");
        let rfi = out.ask(Some((40, 40)), false, ms(t0, 40));
        assert_eq!(
            (rfi.ask, rfi.nack, rfi.invalidate),
            (2, None, Some((40, 40)))
        );
    }

    /// Every other datagram lost: the copy one interval later still carries the ask. A
    /// keyframe ask ignores an anchor and closes on the keyframe; one nothing answers stops
    /// at the bound. A newer ask takes over under the next number.
    #[test]
    fn a_lost_copy_is_covered_within_an_interval_and_the_cadence_stops() {
        let t0 = Instant::now();
        let mut out = FeedbackOut::default();
        out.set_refresh(120);
        let _lost = out.ask(None, true, t0);
        let next = out
            .resend(ms(t0, 9))
            .expect("a second copy one interval on");
        assert!(next.keyframe && next.ask == 1);
        out.on_frame(1, USER_FLAG_RECOVERY_ANCHOR);
        assert!(
            out.resend(ms(t0, 18)).is_some(),
            "an anchor does not answer a keyframe ask"
        );
        out.on_frame(2, u32::from(FLAG_SOF));
        assert_eq!(out.resend(ms(t0, 27)), None);

        let rfi = out.ask(Some((40, 40)), false, ms(t0, 30));
        assert_eq!(
            (rfi.ask, rfi.keyframe, rfi.invalidate),
            (2, false, Some((40, 40)))
        );
        out.on_frame(1, u32::from(FLAG_SOF));
        assert!(
            out.resend(ms(t0, 40)).is_some(),
            "a frame from before the ask answers nothing"
        );
        assert_eq!(
            out.resend(ms(t0, 30) + ASK_GIVE_UP),
            None,
            "the cadence stops"
        );
        assert_eq!(out.window(Feedback::default()).ask, 0);
    }

    /// The last closed window rides every datagram until the next one closes: acks, ask
    /// copies and link changes repeat it, so a lost report costs one datagram.
    #[test]
    fn every_datagram_carries_the_last_window() {
        let t0 = Instant::now();
        let mut out = FeedbackOut::default();
        let fields = |f: Feedback| {
            let positions = (f.head, f.mid, f.tail, f.sock_drops);
            (f.window, f.loss_ppm, f.packets_received, positions)
        };
        let first = out.decoded(1, u32::from(FLAG_SOF)).expect("an IDR");
        assert_eq!(fields(first), fields(Feedback::default()), "no window yet");
        for n in 1..=2u32 {
            let report = Feedback {
                loss_ppm: 300 * n,
                packets_received: 9 * u64::from(n),
                head: n,
                mid: n + 1,
                tail: n + 2,
                sock_drops: n + 3,
                ..Default::default()
            };
            let level = fields(Feedback {
                window: n,
                ..report
            });
            assert_eq!(fields(out.window(report)), level);
            let ack = out.decoded(1 + n, 0).expect("the next clean frame");
            assert_eq!(fields(ack), level, "an ack");
            let t = ms(t0, 100 * u64::from(n));
            out.ask(Some((5, 5)), false, t);
            let copy = out.resend(ms(t, 20)).expect("a copy");
            assert_eq!((fields(copy), copy.ask), (level, n as u16), "an ask copy");
            let link = out.link(1_000 * n).expect("a new rate");
            assert_eq!(fields(link), level, "a link change");
        }
    }
}

#[cfg(test)]
mod driver_tests {
    use super::{
        Decoded, FrameOrder, RecoveryAsk, RecoveryDriver, RecoveryEvent as E, RecoveryStep,
    };
    use crate::packet::{FLAG_SOF, USER_FLAG_RECOVERY_ANCHOR, USER_FLAG_RECOVERY_POINT};
    use crate::reanchor::{AnchorEvidence, LocalRecovery};
    use serde_json::Value;
    use std::time::{Duration, Instant};

    fn num(v: &Value, key: &str, default: u64) -> u64 {
        v.get(key).map_or(default, |x| x.as_u64().expect(key))
    }

    fn flag(v: &Value, key: &str, default: bool) -> bool {
        v.get(key).map_or(default, |x| x.as_bool().expect(key))
    }

    fn decoded(s: &Value) -> Decoded {
        let names = s.get("flags").and_then(Value::as_array);
        let flags = names.into_iter().flatten().fold(0, |acc, name| {
            acc | match name.as_str() {
                Some("sof") => u32::from(FLAG_SOF),
                Some("anchor") => USER_FLAG_RECOVERY_ANCHOR,
                Some("point") => USER_FLAG_RECOVERY_POINT,
                other => panic!("flag {other:?}"),
            }
        });
        Decoded {
            flags,
            keyframe: flag(s, "keyframe", false),
            evidence: match s.get("evidence").and_then(Value::as_str) {
                None | Some("unavailable") => AnchorEvidence::Unavailable,
                Some("clean") => AnchorEvidence::ReferencesClean,
                Some("damaged") => AnchorEvidence::ReferencesDamaged,
                other => panic!("evidence {other:?}"),
            },
            local: LocalRecovery {
                sei_here: flag(s, "sei", false),
                is_recovery_point: flag(s, "recovery_point", false),
            },
            decode_order: s.get("decode_order").map(|x| x.as_u64().expect("order")),
        }
    }

    fn event(s: &Value) -> E {
        let index = || num(s, "index", u64::MAX) as u32;
        match s["ev"].as_str().expect("ev") {
            "arrived" => E::Arrived {
                index: index(),
                complete: flag(s, "complete", true),
                decode_order: num(s, "decode_order", 0),
            },
            "decoded" => E::Decoded(decoded(s)),
            "no_output" => E::NoOutput,
            "lost" => E::Lost,
            "codec_error" => E::CodecError,
            "need_keyframe" => E::NeedKeyframe,
            "tail_short" => E::TailShort { index: index() },
            "tick" => E::Tick {
                frames_dropped: num(s, "frames_dropped", 0),
            },
            other => panic!("event {other}"),
        }
    }

    fn want(s: &Value) -> RecoveryStep {
        let w = &s["want"];
        RecoveryStep {
            admit: flag(w, "admit", true),
            gap: num(w, "gap", 0) as u32,
            hold: flag(w, "hold", false),
            forgive_chain: flag(w, "forgive_chain", false),
            ask: match w.get("ask") {
                None => RecoveryAsk::None,
                Some(Value::String(kind)) if kind == "keyframe" => RecoveryAsk::Keyframe,
                Some(Value::String(kind)) if kind == "starved_keyframe" => {
                    RecoveryAsk::StarvedKeyframe
                }
                Some(Value::Array(r)) => RecoveryAsk::Rfi(
                    r[0].as_u64().expect("first") as u32,
                    r[1].as_u64().expect("last") as u32,
                ),
                other => panic!("ask {other:?}"),
            },
        }
    }

    /// A starved decoder's ask and the backstop's re-ask of its freeze are counted apart; a
    /// loss that re-arms the freeze makes the next ones recovery's again.
    #[test]
    fn a_starved_freeze_asks_apart_until_a_loss_re_arms_it() {
        let t0 = Instant::now();
        let at = |n| t0 + Duration::from_millis(n);
        let tick = E::Tick { frames_dropped: 0 };
        let mut d = RecoveryDriver::default();
        d.engage(0);
        let first = d.step(E::Starved, t0);
        assert_eq!(
            (first.ask, first.hold),
            (RecoveryAsk::StarvedKeyframe, true)
        );
        assert_eq!(d.step(tick, at(500)).ask, RecoveryAsk::StarvedKeyframe);
        assert_eq!(d.step(E::Lost, at(700)).ask, RecoveryAsk::Keyframe);
        assert_eq!(d.step(tick, at(1_200)).ask, RecoveryAsk::Keyframe);
    }

    /// A freeze the decoder raised on an unbroken stream asks apart, its re-asks and a
    /// no-output streak inside it too, until a gap re-arms it. A loss's freeze stays the
    /// loss's through a no-output streak, and with no freeze an ask is recovery's.
    #[test]
    fn a_freeze_the_decoder_raised_asks_apart_until_a_loss_re_arms_it() {
        let t0 = Instant::now();
        let at = |n| t0 + Duration::from_millis(n);
        let tick = E::Tick { frames_dropped: 0 };
        let arrived = |index| E::Arrived {
            index,
            complete: true,
            decode_order: 0,
        };
        let mut d = RecoveryDriver::default();
        d.engage(0);
        d.step(arrived(100), t0);
        let first = d.step(E::NeedKeyframe, t0);
        assert_eq!(
            (first.ask, first.hold),
            (RecoveryAsk::StarvedKeyframe, true)
        );
        assert_eq!(
            d.step(E::NeedKeyframe, at(100)).ask,
            RecoveryAsk::StarvedKeyframe
        );
        for n in [110, 120, 130] {
            d.step(E::NoOutput, at(n));
        }
        assert_eq!(d.step(tick, at(700)).ask, RecoveryAsk::StarvedKeyframe);
        assert_eq!(
            d.step(arrived(103), at(800)).ask,
            RecoveryAsk::Rfi(101, 102)
        );
        assert_eq!(d.step(E::NeedKeyframe, at(900)).ask, RecoveryAsk::Keyframe);
        for n in [910, 920, 930] {
            d.step(E::NoOutput, at(n));
        }
        assert_eq!(d.step(tick, at(1_500)).ask, RecoveryAsk::Keyframe);
        d.reset();
        assert_eq!(
            d.step(E::NeedKeyframe, at(2_000)).ask,
            RecoveryAsk::Keyframe
        );
    }

    /// `clients/shared/recovery-driver-vectors.json`, every case on a fresh driver. An
    /// embedder's report engages the gate at the script's drop count, as `NativeClient` does.
    #[test]
    fn shared_vectors() {
        let raw = include_str!("../../../../../clients/shared/recovery-driver-vectors.json");
        let file: Value = serde_json::from_str(raw).expect("vector file parses");
        let t0 = Instant::now();
        let cases = file["cases"].as_array().expect("cases");
        assert!(cases.iter().any(|c| flag(c, "abi", false)));
        for case in cases {
            let name = case["name"].as_str().expect("name");
            let mut d = RecoveryDriver::default();
            d.set_all_intra(flag(case, "all_intra", false));
            let mut dropped = 0;
            for (i, s) in case["steps"].as_array().expect("steps").iter().enumerate() {
                let now = t0 + Duration::from_millis(num(s, "at_ms", 0));
                let got = match s["ev"].as_str().expect("ev") {
                    "reset" => {
                        d.reset();
                        continue;
                    }
                    "noted" => {
                        let (order, ask) = d.observe(num(s, "index", u64::MAX) as u32, now);
                        RecoveryStep {
                            admit: true,
                            gap: match order {
                                FrameOrder::Gap(gap) => gap,
                                FrameOrder::InOrder | FrameOrder::Straggler => 0,
                            },
                            hold: false,
                            forgive_chain: false,
                            ask,
                        }
                    }
                    _ => {
                        let ev = event(s);
                        match ev {
                            E::Tick { frames_dropped } => dropped = frames_dropped,
                            E::NeedKeyframe | E::TailShort { .. } => {}
                            _ => d.engage(dropped),
                        }
                        d.step(ev, now)
                    }
                };
                assert_eq!(got, want(s), "{name}: step {i}");
            }
        }
    }
}
