//! Screens past the first: one lane each, from the reassembler to the embedder.
//!
//! The first screen keeps the whole pump: ABR, NACK, partial delivery, the recovery driver. A
//! further screen's stream carries whole frames only. Its lane holds a frame until the config of
//! its epoch is known, drops from a gap to the next keyframe and asks the host for one, and
//! queues the rest for [`NativeClient::next_screen_frame`](super::NativeClient::next_screen_frame).

use super::frame_channel::FrameChannel;
use super::pump::anchor::EpochAnchor;
use crate::quic::v2::dgram::Feedback;
use crate::quic::v2::msg::{Screen, StreamConfig, SCREENS_MAX};
use crate::quic::VideoFormat;
use crate::session::Frame;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a frame waits for its epoch's config: the host sends it first, so a wait is the
/// skew between the control stream and the datagrams.
const HOLD_FOR: Duration = Duration::from_millis(250);
/// Frames held at most while their config is out.
const HOLD_MAX: usize = 16;
/// While a lane waits for a keyframe it asks again this often: the ask is a datagram.
const ASK_EVERY: Duration = Duration::from_millis(200);
/// Queued frames past which the embedder is behind: the lane drops them and asks for a keyframe.
const QUEUE_MAX: usize = 12;

/// One screen's frames between the pump and the embedder.
pub(crate) struct Lane {
    pub(crate) frames: FrameChannel,
    state: Mutex<LaneState>,
}

struct LaneState {
    stream: u8,
    anchor: EpochAnchor,
    /// The format of the newest frame handed out; a change marks the next one's `new_format`.
    live: Option<VideoFormat>,
    newest_epoch: Option<u8>,
    held: VecDeque<Frame>,
    held_since: Option<Instant>,
    last_index: Option<u32>,
    /// From the start, a gap or a shed queue until a keyframe: the frames before it reference
    /// pictures this decoder never had.
    awaiting_key: bool,
    last_ask: Option<Instant>,
    asks: u16,
}

impl LaneState {
    fn new(stream: u8) -> Self {
        let mut anchor = EpochAnchor::default();
        (anchor.on, anchor.formats) = (true, true);
        LaneState {
            stream,
            anchor,
            live: None,
            newest_epoch: None,
            held: VecDeque::new(),
            held_since: None,
            last_index: None,
            awaiting_key: true,
            last_ask: None,
            asks: 0,
        }
    }

    /// Whether `epoch` predates the newest seen, wrapping (1–128 behind): its decoder is gone.
    fn behind_epoch(&mut self, epoch: u8) -> bool {
        let newest = *self.newest_epoch.get_or_insert(epoch);
        if epoch.wrapping_sub(newest) >= 128 {
            return true;
        }
        self.newest_epoch = Some(epoch);
        false
    }

    /// A frame off the wire, in arrival order.
    fn admit(&mut self, frame: Frame, now: Instant, out: &FrameChannel) {
        if self.behind_epoch(frame.epoch) {
            return;
        }
        if let Some(last) = self.last_index {
            match frame.frame_index.wrapping_sub(last) as i32 {
                // Late or repeated: the decoder already moved past it.
                d if d <= 0 => return,
                1 => {}
                _ => self.awaiting_key = true,
            }
        }
        self.last_index = Some(frame.frame_index);
        if !self.held.is_empty() || !self.anchor.ready(frame.epoch) {
            if self.held.len() == HOLD_MAX {
                self.held.pop_front();
                self.awaiting_key = true;
            }
            self.held_since.get_or_insert(now);
            self.held.push_back(frame);
            return;
        }
        self.hand_on(frame, out);
    }

    /// Frames whose config came, in order, and past [`HOLD_FOR`] the rest under the newest
    /// format booked.
    fn release(&mut self, now: Instant, out: &FrameChannel) {
        while let Some(front) = self.held.front() {
            if !self.anchor.ready(front.epoch) {
                if self.held_since.is_some_and(|t| now < t + HOLD_FOR) {
                    return;
                }
                self.anchor.assume(front.epoch);
                if !self.anchor.ready(front.epoch) {
                    // No config ever came for this stream: nothing can decode it.
                    self.held.clear();
                    break;
                }
            }
            let frame = self.held.pop_front().expect("front exists");
            self.hand_on(frame, out);
        }
        self.held_since = None;
    }

    fn hand_on(&mut self, mut frame: Frame, out: &FrameChannel) {
        let key = is_key(&frame);
        if self.awaiting_key && !key {
            return;
        }
        if key {
            self.awaiting_key = false;
        }
        let format = self.anchor.format(frame.epoch);
        if format != self.live {
            self.live = format;
            frame.new_format = format;
        }
        if !out.consumer_seen() {
            if out.preroll(frame) > 0 {
                self.awaiting_key = true;
            }
        } else {
            out.push(frame);
            if out.depth() > QUEUE_MAX {
                out.clear();
                self.awaiting_key = true;
            }
        }
    }

    /// A keyframe ask when one is owed, none is already held for its config, and none went out
    /// in the last [`ASK_EVERY`].
    fn ask(&mut self, now: Instant) -> Option<Feedback> {
        if !self.awaiting_key
            || self.held.iter().any(is_key)
            || self.last_ask.is_some_and(|t| now < t + ASK_EVERY)
        {
            return None;
        }
        self.last_ask = Some(now);
        self.asks = self.asks.wrapping_add(1).max(1);
        Some(Feedback {
            stream: self.stream,
            ask: self.asks,
            keyframe: true,
            ..Default::default()
        })
    }
}

/// The host stamps a keyframe `FLAG_PIC | FLAG_SOF`.
fn is_key(frame: &Frame) -> bool {
    frame.flags & u32::from(crate::packet::FLAG_SOF) != 0
}

/// Every lane of the session, the layout the host built, and whether screens are in force.
#[derive(Default)]
pub(crate) struct Screens {
    on: AtomicBool,
    lanes: Mutex<Vec<(u8, Arc<Lane>)>>,
    layout: Mutex<Vec<Screen>>,
    /// Moves with each layout, so an embedder polls a number instead of comparing lists.
    layout_gen: AtomicU32,
}

impl Screens {
    /// The host took [`FEATURE_SCREENS`](crate::quic::v2::registry::FEATURE_SCREENS).
    pub(crate) fn set_on(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }

    pub(crate) fn is_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub(crate) fn lane(&self, stream: u8) -> Option<Arc<Lane>> {
        let lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes
            .iter()
            .find(|(s, _)| *s == stream)
            .map(|(_, l)| l.clone())
    }

    /// The lane for `stream`, opened on first use; `None` with screens off or the lanes full.
    /// Once a layout is in, a stream it does not name opens one only for a `config`: the host
    /// configures a new screen before its layout names it, and a retired screen's late frames
    /// open nothing.
    fn open(&self, stream: u8, config: bool) -> Option<Arc<Lane>> {
        if !self.on.load(Ordering::Relaxed) {
            return None;
        }
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, l)) = lanes.iter().find(|(s, _)| *s == stream) {
            return Some(l.clone());
        }
        if lanes.len() + 1 >= SCREENS_MAX {
            return None;
        }
        let named = || {
            let layout = self.layout.lock().unwrap_or_else(|e| e.into_inner());
            layout.iter().any(|s| s.stream == stream)
        };
        if !config && self.layout_gen.load(Ordering::Relaxed) > 0 && !named() {
            return None;
        }
        let lane = Arc::new(Lane {
            frames: FrameChannel::new(),
            state: Mutex::new(LaneState::new(stream)),
        });
        lanes.push((stream, lane.clone()));
        Some(lane)
    }

    /// The config of one epoch of a further screen's stream; it opens the lane.
    pub(crate) fn config(&self, cfg: StreamConfig) {
        if let Some(lane) = self.open(cfg.stream, true) {
            let mut st = lane.state.lock().unwrap_or_else(|e| e.into_inner());
            st.anchor.config(cfg);
            st.release(Instant::now(), &lane.frames);
        }
    }

    /// The host's layout. A lane whose stream it no longer names is closed: its screen is gone.
    /// Set under the lanes lock, so [`Self::open`] never opens one against the layout before.
    pub(crate) fn set_layout(&self, screens: Vec<Screen>) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        lanes.retain(|(stream, lane)| {
            let kept = screens.iter().any(|s| s.stream == *stream);
            if !kept {
                lane.frames.close();
            }
            kept
        });
        *self.layout.lock().unwrap_or_else(|e| e.into_inner()) = screens;
        self.layout_gen.fetch_add(1, Ordering::Relaxed);
    }

    /// The layout and its generation; generation `0` before the host sent one.
    pub(crate) fn layout(&self) -> (u32, Vec<Screen>) {
        let layout = self
            .layout
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        (self.layout_gen.load(Ordering::Relaxed), layout)
    }

    /// A frame of a further screen's stream. The keyframe ask its lane now owes, if any; none
    /// for a stream with no lane that the layout does not name.
    pub(crate) fn on_frame(&self, frame: Frame, now: Instant) -> Option<Feedback> {
        let lane = self.open(frame.stream, false)?;
        let mut st = lane.state.lock().unwrap_or_else(|e| e.into_inner());
        st.admit(frame, now, &lane.frames);
        st.ask(now)
    }

    /// The embedder's decoder for `stream` lost its picture, or is new: drop to the next
    /// keyframe, ask for it, and name its format again.
    pub(crate) fn want_key(&self, stream: u8) {
        if let Some(lane) = self.lane(stream) {
            let mut st = lane.state.lock().unwrap_or_else(|e| e.into_inner());
            st.awaiting_key = true;
            st.live = None;
        }
    }

    /// Each pump turn: held frames whose wait ran out, and the asks still owed.
    pub(crate) fn tick(&self, now: Instant) -> Vec<Feedback> {
        let lanes: Vec<Arc<Lane>> = (self.lanes.lock().unwrap_or_else(|e| e.into_inner()))
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        lanes
            .iter()
            .filter_map(|lane| {
                let mut st = lane.state.lock().unwrap_or_else(|e| e.into_inner());
                st.release(now, &lane.frames);
                st.ask(now)
            })
            .collect()
    }

    /// The session ended: wake every consumer with `Closed`.
    pub(crate) fn close(&self) {
        for (_, lane) in self.lanes.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            lane.frames.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Mode;
    use crate::packet::{FLAG_PIC, FLAG_SOF};
    use crate::quic::CODEC_HEVC;

    fn frame(stream: u8, epoch: u8, index: u32, key: bool) -> Frame {
        Frame {
            data: Vec::new().into(),
            frame_index: index,
            pts_ns: 0,
            flags: u32::from(if key { FLAG_PIC | FLAG_SOF } else { FLAG_PIC }),
            epoch,
            stream,
            complete: true,
            part: None,
            received_ns: 0,
            new_format: None,
        }
    }

    fn cfg(stream: u8, epoch: u8) -> StreamConfig {
        StreamConfig {
            stream,
            epoch,
            mode: Mode {
                width: 1920,
                height: 1080,
                refresh_hz: 60,
                refresh_mhz: 0,
            },
            codec: CODEC_HEVC,
            ..Default::default()
        }
    }

    fn on() -> Screens {
        let s = Screens::default();
        s.set_on(true);
        s
    }

    fn popped(s: &Screens, stream: u8) -> Vec<u32> {
        let lane = s.lane(stream).unwrap();
        let mut got = Vec::new();
        while let super::super::frame_channel::FramePop::Frame(f) =
            lane.frames.pop(Duration::from_millis(1))
        {
            got.push(f.frame_index);
        }
        got
    }

    /// Frames wait for their epoch's config, then go in order; the first carries the format.
    #[test]
    fn frames_wait_for_their_config_and_open_with_its_format() {
        let s = on();
        let now = Instant::now();
        assert!(s.on_frame(frame(2, 0, 0, true), now).is_none());
        s.on_frame(frame(2, 0, 1, false), now);
        assert!(popped(&s, 2).is_empty(), "no config yet");
        s.config(cfg(2, 0));
        let lane = s.lane(2).unwrap();
        let super::super::frame_channel::FramePop::Frame(first) =
            lane.frames.pop(Duration::from_millis(1))
        else {
            panic!("the keyframe went on");
        };
        assert_eq!(first.new_format.map(|f| f.codec), Some(CODEC_HEVC));
        assert_eq!(popped(&s, 2), [1]);
    }

    /// A gap drops frames to the next keyframe and asks for one, again only after a while.
    #[test]
    fn a_gap_waits_for_a_keyframe_and_asks_for_it() {
        let s = on();
        let now = Instant::now();
        s.config(cfg(2, 0));
        s.on_frame(frame(2, 0, 0, true), now);
        s.on_frame(frame(2, 0, 1, false), now);
        let ask = s.on_frame(frame(2, 0, 3, false), now).expect("a gap asks");
        assert_eq!((ask.stream, ask.keyframe), (2, true));
        assert!(
            s.on_frame(frame(2, 0, 4, false), now).is_none(),
            "asked just now"
        );
        assert_eq!(s.tick(now + ASK_EVERY).len(), 1, "asks again");
        s.on_frame(frame(2, 0, 5, true), now + ASK_EVERY);
        s.on_frame(frame(2, 0, 6, false), now + ASK_EVERY);
        assert_eq!(popped(&s, 2), [0, 1, 5, 6]);
        assert!(
            s.tick(now + ASK_EVERY * 3).is_empty(),
            "the keyframe ended the ask"
        );
    }

    /// A new decoder's ask drops to the next keyframe, and that frame names the format again.
    #[test]
    fn a_wanted_key_names_the_format_again() {
        let s = on();
        let now = Instant::now();
        s.config(cfg(2, 0));
        s.on_frame(frame(2, 0, 0, true), now);
        assert_eq!(popped(&s, 2), [0]);
        s.want_key(2);
        assert_eq!(s.tick(now).len(), 1, "asks");
        s.on_frame(frame(2, 0, 1, false), now);
        s.on_frame(frame(2, 0, 2, true), now);
        let lane = s.lane(2).unwrap();
        let super::super::frame_channel::FramePop::Frame(key) =
            lane.frames.pop(Duration::from_millis(1))
        else {
            panic!("the keyframe went on");
        };
        assert_eq!(key.frame_index, 2);
        assert!(key.new_format.is_some());
    }

    /// Screens off: another stream's frames go nowhere.
    #[test]
    fn screens_off_take_nothing() {
        let s = Screens::default();
        s.on_frame(frame(2, 0, 0, true), Instant::now());
        assert!(s.lane(2).is_none());
    }

    /// A layout that drops a screen closes its lane.
    #[test]
    fn a_dropped_screen_closes_its_lane() {
        let s = on();
        s.config(cfg(2, 0));
        let lane = s.lane(2).unwrap();
        s.set_layout(vec![Screen::default()]);
        assert!(s.lane(2).is_none());
        assert!(matches!(
            lane.frames.pop(Duration::from_millis(1)),
            super::super::frame_channel::FramePop::Closed
        ));
        assert_eq!(s.layout().0, 1);
    }

    /// Once a layout is in, a frame of a stream it does not name opens no lane and asks for
    /// nothing; a config still opens one, as it comes before a new screen's layout.
    #[test]
    fn a_frame_the_layout_does_not_name_opens_no_lane() {
        let s = on();
        let now = Instant::now();
        let second = Screen {
            index: 1,
            stream: 2,
            ..Default::default()
        };
        s.set_layout(vec![Screen::default(), second]);
        assert!(s.on_frame(frame(3, 0, 0, false), now).is_none());
        assert!(s.lane(3).is_none());
        assert!(s.tick(now + ASK_EVERY).is_empty());
        assert!(s.on_frame(frame(2, 0, 0, false), now).is_some(), "named");
        s.config(cfg(3, 0));
        assert!(s.lane(3).is_some());
    }
}
