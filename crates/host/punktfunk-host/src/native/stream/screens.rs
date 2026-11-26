//! Screens past the first: a display, capture and encoder each, on a video stream of its own.
//!
//! The first screen is [`StreamState`](super::state::StreamState)'s whole loop: recovery,
//! pacing, cursor forwarding, the game. Each further screen is one thread running the session's
//! plan with the pointer drawn into its picture, framed for the client's view of it, and sending
//! whole access units into the session's one send thread under its own stream id, epoch and frame
//! index. It keyframes on the client's ask, rebuilds on a lost capture and encodes at its share
//! of the session's budget. [`ScreenSet`] follows the client's `Screens` and tells it what was
//! built.

use super::pipeline::{build_pipeline_with_retry, BuildRequest, Pipeline};
use super::send::{AuMeta, FrameMsg, SendMsg};
use super::*;
use crate::native::bitrate::{EncDerive, SHARE_ALL};
use crate::native::geometry::Picture;
use crate::native::wiring::{ScreenTell, ScreenWire};
use punktfunk_core::quic::v2::msg::{Screen, StreamConfig, SCREENS_MAX};
use std::sync::atomic::AtomicU16;

/// A keyframe ask inside this long of the last keyframe is the same loss asked again.
const KEY_EVERY: std::time::Duration = std::time::Duration::from_millis(100);
/// Builds of further screens start this far apart and after the first screen's: a compositor
/// that adds two outputs at once can fail both (Mutter's monitor rebuild).
const BUILD_SPACING: std::time::Duration = std::time::Duration::from_millis(1500);
/// Lost captures in a row before a screen gives up and leaves the layout.
const MAX_REBUILDS: u32 = 4;

/// The first screen, as the further screens place themselves against it.
#[derive(Clone, Copy, Default)]
pub(super) struct FirstScreen {
    /// The client's screen 0 record: its monitors sit relative to this one.
    asked: Screen,
    /// What the first screen delivers now, and its display's registry generation.
    mode: punktfunk_core::Mode,
    display_gen: Option<u64>,
}

/// Where the client's screen `ask` sits from its first, scaled from the client's pixels to what
/// the host delivers on the first. `None` when the client named no offset. The client picks
/// `x` and `y`, so the difference is taken in `i64`.
fn offset_of(first: &FirstScreen, ask: &Screen) -> Option<(i32, i32)> {
    if (ask.x, ask.y) == (first.asked.x, first.asked.y) {
        return None;
    }
    let scale = |d: i64, host: u32, client: u32| {
        (d * i64::from(host.max(1)) / i64::from(client.max(1))) as i32
    };
    Some((
        scale(
            i64::from(ask.x) - i64::from(first.asked.x),
            first.mode.width,
            first.asked.mode.width,
        ),
        scale(
            i64::from(ask.y) - i64::from(first.asked.y),
            first.mode.height,
            first.asked.mode.height,
        ),
    ))
}

/// Put the display of the client's screen `ask` beside the first as its monitors sit, and say
/// where it is. The manager holds the place through every later arrangement.
#[cfg(target_os = "windows")]
fn place(
    _vd: &mut dyn crate::vdisplay::VirtualDisplay,
    fp: [u8; 32],
    base: &ScreenBase,
    ask: &Screen,
    _display_gen: Option<u64>,
) -> (i32, i32) {
    let first = *base.first.lock().unwrap_or_else(|e| e.into_inner());
    crate::vdisplay::manager::place_beside(fp, base.params.client_fp, offset_of(&first, ask))
        .unwrap_or((0, 0))
}

/// [`place`] by the backend's own move, in the compositor's desktop units; one that cannot
/// move an output keeps the compositor's.
#[cfg(not(target_os = "windows"))]
fn place(
    vd: &mut dyn crate::vdisplay::VirtualDisplay,
    _fp: [u8; 32],
    base: &ScreenBase,
    ask: &Screen,
    display_gen: Option<u64>,
) -> (i32, i32) {
    let first = *base.first.lock().unwrap_or_else(|e| e.into_inner());
    let own = desk_of(display_gen);
    let at = offset_of(&first, ask)
        .zip(desk_of(first.display_gen))
        .map(|((dx, dy), f)| {
            let reach = |px: u32, host: u32, client: u32| {
                i64::from(px) * i64::from(host.max(1)) / i64::from(client.max(1))
            };
            let (own_size, own_scale) = own.map_or(((ask.mode.width, ask.mode.height), 1.0), |o| {
                (o.size, o.scale)
            });
            (
                f.at.0
                    + desktop_axis(
                        dx,
                        reach(ask.mode.width, first.mode.width, first.asked.mode.width),
                        f.scale,
                        (own_size.0, own_scale),
                    ),
                f.at.1
                    + desktop_axis(
                        dy,
                        reach(ask.mode.height, first.mode.height, first.asked.mode.height),
                        f.scale,
                        (own_size.1, own_scale),
                    ),
            )
        });
    if let Some((x, y)) = at {
        vd.apply_position(x, y);
    }
    at.or_else(|| own.map(|o| o.at)).unwrap_or((0, 0))
}

/// One axis of `d`, a screen's offset from the first in the first's pixels, in desktop units.
/// Right of or below the first, `d` counts the first's pixels at its `scale`. Left of or
/// above, the client sent its far edge less `reach`, its own size in the first's pixels: the
/// edge converts at the first's scale, and the screen reaches back its own extent.
#[cfg(not(target_os = "windows"))]
fn desktop_axis(d: i32, reach: i64, scale: f64, (own_px, own_scale): (u32, f64)) -> i32 {
    use crate::vdisplay::desktop_extent;
    if d >= 0 {
        desktop_extent(d.into(), scale)
    } else {
        desktop_extent(i64::from(d) + reach, scale) - desktop_extent(own_px.into(), own_scale)
    }
}

/// What every screen loop of a session starts from.
#[derive(Clone)]
pub(super) struct ScreenBase {
    /// What the handshake settled: the encode resolution every screen opens on, and the
    /// `PUNKTFUNK_PYROWAVE_MAX_MBPS` cap.
    pub(super) terms: Arc<super::super::handshake::SessionTerms>,
    pub(super) compositor: crate::vdisplay::Compositor,
    /// The first screen's display request; each screen swaps in its own identity.
    pub(super) params: crate::vdisplay::SessionParams,
    pub(super) plan: crate::session_plan::SessionPlan,
    pub(super) bit_depth: u8,
    /// The first screen's refresh rate: a PyroWave frame's head parity is per frame.
    pub(super) hz: u32,
    pub(super) fec_target: Arc<AtomicU8>,
    /// The session's wire budget, kbps: every screen encodes at its share of it.
    pub(super) live_bitrate: Arc<AtomicU32>,
    pub(super) send: Arc<std::sync::mpsc::SyncSender<SendMsg>>,
    pub(super) tell: tokio::sync::mpsc::UnboundedSender<ScreenTell>,
    pub(super) wire: Arc<ScreenWire>,
    pub(super) life: crate::lifecycle::Lifecycle,
    /// Kept current by [`ScreenSet::poll`].
    pub(super) first: Arc<std::sync::Mutex<FirstScreen>>,
}

/// Where a screen slot's stream stands across the loops that run it: the epoch and frame index
/// the next loop starts at. A restarted screen continues them, so a late datagram of the loop
/// before cannot leave the client's lane ahead of the new one.
#[derive(Default)]
struct ScreenSeq {
    epoch: std::sync::atomic::AtomicU8,
    au_seq: std::sync::atomic::AtomicU32,
}

/// One further screen's loop, as the set holds it.
struct ScreenHandle {
    ask: Screen,
    share: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    /// The client's record of a screen it moved, until the loop places the display again.
    moved: Arc<std::sync::Mutex<Option<Screen>>>,
    /// What the loop built; `None` before it has, and after it gave up.
    built: Arc<std::sync::Mutex<Option<Screen>>>,
    /// The picture the loop's last build encodes; `None` before its first.
    encodes: Arc<std::sync::Mutex<Option<Picture>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScreenHandle {
    fn built(&self) -> Option<Screen> {
        *self.built.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The picture the split prices this screen at: what its build encodes, else its ask.
    fn priced(&self, codec: crate::encode::Codec) -> punktfunk_core::Mode {
        let encodes = *self.encodes.lock().unwrap_or_else(|e| e.into_inner());
        encodes
            .unwrap_or_else(|| screen_intent(&self.ask, codec).picture())
            .mode()
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Join the loop if its thread has ended; `true` then.
    fn reap(&mut self) -> bool {
        match self.thread.take_if(|t| t.is_finished()) {
            Some(t) => {
                let _ = t.join();
                true
            }
            None => false,
        }
    }
}

/// What one client list asks of the further screens' loops.
#[derive(Debug, PartialEq)]
struct ScreenPlan {
    /// The client's screen 0 record, when the list names one.
    first: Option<Screen>,
    /// The running loops to stop, by index.
    stop: Vec<u8>,
    /// The screens to start, in the list's order.
    start: Vec<Screen>,
    /// The screens whose index still has a loop ending: each starts once that loop has.
    wait: Vec<Screen>,
}

/// The screens past the first a session runs, following the client's `Screens`.
pub(super) struct ScreenSet {
    /// `None` once [`Self::shutdown`] ran: the send thread must not outlive its last sender.
    base: Option<ScreenBase>,
    rx: std::sync::mpsc::Receiver<Vec<Screen>>,
    loops: Vec<ScreenHandle>,
    /// Stopped loops whose thread has not ended: a build in flight runs for seconds, and the
    /// first screen's tick does not wait for it.
    retiring: Vec<ScreenHandle>,
    /// The client's screens not running: each waits for its index's retiring loop, as two loops
    /// never hold one display identity or stream, or for the session to leave gamescope.
    pending: Vec<Screen>,
    /// The client's screen 0 record, for its position and primary flag.
    first: Screen,
    /// What the first screen's encoder took its picture to at the last poll.
    first_picture: Option<Picture>,
    /// The further screens' pictures the last split priced.
    priced: Vec<punktfunk_core::Mode>,
    /// The layout last told.
    told: Vec<Screen>,
    /// When the next build may start ([`BUILD_SPACING`]).
    next_build: std::time::Instant,
    /// Per screen index.
    seqs: [Arc<ScreenSeq>; SCREENS_MAX],
}

impl ScreenSet {
    /// The set for a session that negotiated screens, building those `asked` names past the first.
    pub(super) fn new(
        base: ScreenBase,
        rx: std::sync::mpsc::Receiver<Vec<Screen>>,
        asked: Vec<Screen>,
    ) -> Self {
        let first = asked
            .iter()
            .find(|s| s.index == 0)
            .copied()
            .unwrap_or_default();
        let mut set = ScreenSet {
            base: Some(base),
            rx,
            loops: Vec::new(),
            retiring: Vec::new(),
            pending: Vec::new(),
            first,
            first_picture: None,
            priced: Vec::new(),
            told: Vec::new(),
            next_build: std::time::Instant::now() + BUILD_SPACING,
            seqs: Default::default(),
        };
        set.apply(asked);
        set
    }

    /// Each turn of the first screen's loop: the client's newest list, ended loops, the starts
    /// they held back, the layout when it moved or the client asked. `first_mode` is what the
    /// first screen delivers now, on the display of registry generation `first_gen`, and
    /// `first_picture` the size its encoder takes that to. Never waits on a loop.
    pub(super) fn poll(
        &mut self,
        first_mode: punktfunk_core::Mode,
        first_picture: Picture,
        first_gen: Option<u64>,
    ) {
        if let Some(base) = self.base.as_ref() {
            let mut f = base.first.lock().unwrap_or_else(|e| e.into_inner());
            (f.asked, f.mode, f.display_gen) = (self.first, first_mode, first_gen);
        }
        // Shares go by the pixels each screen encodes, so a first picture that is new or changed
        // size moves them as a screen joining or leaving does.
        let first_moved = self.first_picture.replace(first_picture) != Some(first_picture);
        let mut newest = None;
        while let Ok(list) = self.rx.try_recv() {
            newest = Some(list);
        }
        let asked = newest.is_some();
        if let Some(list) = newest {
            self.apply(list);
        }
        let before = self.loops.len();
        self.loops.retain_mut(|h| !h.reap());
        let retiring = self.retiring.len();
        self.retiring.retain_mut(|h| !h.reap());
        if self.retiring.len() != retiring && !self.pending.is_empty() {
            self.apply(self.asks());
        }
        if self.loops.len() != before || first_moved || self.pictures() != self.priced {
            self.reshare();
        }
        self.tell_layout(first_mode, asked);
    }

    /// What `want` asks of the loops `running`: a further screen that fits `codec`, the first
    /// of its index in the list, keeps the loop at its index, mode and view or starts one; every
    /// other loop stops. A start waits while its index has a loop `retiring` or stopping.
    fn plan(
        want: &[Screen],
        running: &[Screen],
        retiring: &[u8],
        codec: crate::encode::Codec,
    ) -> ScreenPlan {
        let mut fits: Vec<Screen> = Vec::new();
        for s in want {
            if screen_fits(s, codec) && !fits.iter().any(|w| w.index == s.index) {
                fits.push(*s);
            }
        }
        let same = |a: &Screen, b: &Screen| (a.index, a.mode, a.view) == (b.index, b.mode, b.view);
        let stop: Vec<u8> = running
            .iter()
            .filter(|r| !fits.iter().any(|w| same(w, r)))
            .map(|r| r.index)
            .collect();
        let (wait, start) = fits
            .into_iter()
            .filter(|w| !running.iter().any(|r| same(r, w)))
            .partition(|w| retiring.contains(&w.index) || stop.contains(&w.index));
        ScreenPlan {
            first: want.iter().find(|s| s.index == 0).copied(),
            stop,
            start,
            wait,
        }
    }

    /// Run exactly the screens `list` names past the first, as [`Self::plan`] says, each at the
    /// view its own [`screen_intent`] keeps. A stopped loop retires: [`Self::poll`] reaps it once
    /// its thread ends. A kept loop takes the list's record, and places its display again when
    /// the client moved it against the first. On gamescope, which has one output, every start
    /// waits in `pending` instead; [`Self::rebase`] onto a desktop starts them.
    fn apply(&mut self, list: Vec<Screen>) {
        let Some(base) = self.base.clone() else {
            return;
        };
        let codec = base.plan.codec;
        let list: Vec<Screen> = list
            .into_iter()
            .map(|s| Screen {
                view: screen_intent(&s, codec).view(),
                ..s
            })
            .collect();
        let running: Vec<Screen> = self.loops.iter().map(|h| h.ask).collect();
        let retiring: Vec<u8> = self.retiring.iter().map(|h| h.ask.index).collect();
        let plan = Self::plan(&list, &running, &retiring, base.plan.codec);
        let was = self.first;
        if let Some(first) = plan.first {
            self.first = first;
            base.first.lock().unwrap_or_else(|e| e.into_inner()).asked = first;
        }
        let (stopped, kept) = std::mem::take(&mut self.loops)
            .into_iter()
            .partition::<Vec<_>, _>(|h| plan.stop.contains(&h.ask.index));
        self.loops = kept;
        for h in &stopped {
            // A loop configures its stream under this lock: before this stop, or not at all.
            let _built = h.built.lock().unwrap_or_else(|e| e.into_inner());
            h.stop.store(true, Ordering::SeqCst);
        }
        self.retiring.extend(stopped);
        let from = |s: &Screen, f: &Screen| (s.x.wrapping_sub(f.x), s.y.wrapping_sub(f.y));
        for h in &mut self.loops {
            let same =
                |w: &&Screen| (w.index, w.mode, w.view) == (h.ask.index, h.ask.mode, h.ask.view);
            let Some(&now) = list.iter().find(same) else {
                continue;
            };
            if from(&now, &self.first) != from(&h.ask, &was) {
                *h.moved.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
            }
            h.ask = now;
        }
        self.pending = plan.wait;
        if base.compositor == crate::vdisplay::Compositor::Gamescope {
            self.pending.extend(plan.start);
        } else {
            for w in plan.start {
                self.start(&base, w);
            }
        }
        self.reshare();
    }

    /// The client's list as the set holds it: the first record, then each further screen
    /// running or waiting.
    fn asks(&self) -> Vec<Screen> {
        std::iter::once(self.first)
            .chain(self.loops.iter().map(|h| h.ask))
            .chain(self.pending.iter().copied())
            .collect()
    }

    /// Spawn screen `w`'s loop, its build [`BUILD_SPACING`] after the last one that started.
    fn start(&mut self, base: &ScreenBase, w: Screen) {
        let not_before = self.next_build.max(std::time::Instant::now());
        let handle = ScreenHandle {
            ask: w,
            share: Arc::new(AtomicU16::new(SHARE_ALL / 2)),
            stop: Arc::new(AtomicBool::new(false)),
            moved: Arc::default(),
            built: Arc::default(),
            encodes: Arc::default(),
            thread: None,
        };
        let run = ScreenRun {
            index: w.index,
            stream: punktfunk_core::packet::screen_stream(w.index),
            ask: w,
            mode: w.mode,
            share: handle.share.clone(),
            stop: handle.stop.clone(),
            moved: handle.moved.clone(),
            built: handle.built.clone(),
            encodes: handle.encodes.clone(),
            seq: self.seqs[usize::from(w.index)].clone(),
            not_before,
        };
        let base = base.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("punktfunk-screen-{}", w.index))
            .spawn(move || run_screen(base, run));
        match spawned {
            Ok(t) => {
                self.next_build = not_before + BUILD_SPACING;
                self.loops.push(ScreenHandle {
                    thread: Some(t),
                    ..handle
                });
            }
            Err(e) => {
                tracing::warn!(screen = w.index, error = %e, "screen thread did not spawn")
            }
        }
    }

    /// Each further screen's picture as the split prices it ([`ScreenHandle::priced`]).
    fn pictures(&self) -> Vec<punktfunk_core::Mode> {
        let Some(base) = self.base.as_ref() else {
            return Vec::new();
        };
        self.loops
            .iter()
            .map(|h| h.priced(base.plan.codec))
            .collect()
    }

    /// Split the video by the pixel rate each screen encodes: the first screen's share to the
    /// first loop, each further screen's to its own, at its asked view until it has built.
    fn reshare(&mut self) {
        self.priced = self.pictures();
        let Some(base) = self.base.as_ref() else {
            return;
        };
        let first = self.first_picture.map_or(self.first.mode, |p| p.mode());
        let (first, rest) = shares(first, &self.priced);
        base.wire.primary_share.store(first, Ordering::Relaxed);
        for (h, s) in self.loops.iter().zip(rest) {
            h.share.store(s, Ordering::Relaxed);
        }
    }

    /// Tell the client every screen built, when that changed or the client `asked`. Each
    /// screen's primary flag is its record's, as screen 0's is.
    fn tell_layout(&mut self, first_mode: punktfunk_core::Mode, asked: bool) {
        let Some(base) = self.base.as_ref() else {
            return;
        };
        let mut layout = vec![Screen {
            index: 0,
            stream: punktfunk_core::packet::V2_STREAM_VIDEO,
            mode: first_mode,
            ..self.first
        }];
        layout.extend(self.loops.iter().filter_map(|h| {
            Some(Screen {
                primary: h.ask.primary,
                ..h.built()?
            })
        }));
        if asked || layout != self.told {
            self.told = layout.clone();
            let _ = base.tell.send(ScreenTell::Layout(layout));
        }
    }

    /// The session moved under its screens (a session switch, a codec change): `update` the
    /// base, wait for every loop to end, and start the client's list again on it. Each
    /// restarted screen continues its stream's epoch.
    pub(super) fn rebase(&mut self, update: impl FnOnce(&mut ScreenBase)) {
        let Some(base) = self.base.as_mut() else {
            return;
        };
        update(base);
        let asks = self.asks();
        self.join_all();
        self.apply(asks);
    }

    /// Stop every loop and drop the set's sender, before the send thread is joined.
    pub(super) fn shutdown(&mut self) {
        self.join_all();
        self.base = None;
    }

    /// Stop every loop, retiring ones too, wait for each to end, and drop the waiting starts.
    fn join_all(&mut self) {
        for h in self.loops.iter().chain(&self.retiring) {
            h.stop.store(true, Ordering::SeqCst);
        }
        for mut h in self.loops.drain(..).chain(self.retiring.drain(..)) {
            h.stop_and_join();
        }
        self.pending.clear();
    }
}

impl Drop for ScreenSet {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A further screen's intent: a display of its own at the record's mode and view, framed at
/// Fit. Fit only scales: absolute input in the picture's own extent lands with no frame map.
/// Each list asks afresh, so a record with no view clears the one its loop runs.
fn screen_intent(s: &Screen, codec: crate::encode::Codec) -> crate::native::geometry::Intent {
    let own = crate::native::geometry::Tenure::of(None, false);
    crate::native::geometry::Intent::at_hello(s.mode, s.view, &own, codec)
}

/// The shares of the video, per mille, by pixel rate: the first screen's, then each of `rest`'s.
/// Each is the picture that screen encodes.
fn shares(first: punktfunk_core::Mode, rest: &[punktfunk_core::Mode]) -> (u16, Vec<u16>) {
    let rate = |m: &punktfunk_core::Mode| {
        u64::from(m.width) * u64::from(m.height) * u64::from(m.refresh_hz.max(1))
    };
    let total = rate(&first) + rest.iter().map(rate).sum::<u64>();
    let share = |m| (rate(m) * u64::from(SHARE_ALL) / total.max(1)).max(1) as u16;
    if rest.is_empty() {
        return (SHARE_ALL, Vec::new());
    }
    (share(&first), rest.iter().map(share).collect())
}

/// Screen `index`'s display identity: its own slot, output name and scale memory, apart from the
/// first screen's, the same for this client every session.
fn screen_identity(fp: Option<[u8; 32]>, index: u8) -> [u8; 32] {
    use sha2::Digest as _;
    let mut seed = b"punktfunk screen".to_vec();
    seed.extend_from_slice(&fp.unwrap_or_default());
    seed.push(index);
    sha2::Sha256::digest(&seed).into()
}

/// One loop's own values.
struct ScreenRun {
    index: u8,
    stream: u8,
    /// The client's record of this screen: its size and where it sits.
    ask: Screen,
    mode: punktfunk_core::Mode,
    share: Arc<AtomicU16>,
    stop: Arc<AtomicBool>,
    moved: Arc<std::sync::Mutex<Option<Screen>>>,
    built: Arc<std::sync::Mutex<Option<Screen>>>,
    encodes: Arc<std::sync::Mutex<Option<Picture>>>,
    seq: Arc<ScreenSeq>,
    not_before: std::time::Instant,
}

impl ScreenRun {
    fn leaving(&self, base: &ScreenBase) -> bool {
        self.stop.load(Ordering::SeqCst) || base.life.stopping()
    }

    fn set_built(&self, built: Option<Screen>) {
        *self.built.lock().unwrap_or_else(|e| e.into_inner()) = built;
    }
}

/// How one pipeline's loop ended.
enum Ended {
    Stop,
    Rebuild,
}

/// A further screen from first build to stop: build framed for its view, stream, rebuild on a
/// lost capture. It raises itself as the first screen's stream thread does: a stream thread
/// RealtimeKit raised passes no raise to what it spawns.
fn run_screen(base: ScreenBase, mut run: ScreenRun) {
    #[cfg(target_os = "linux")]
    pf_frame::thread_qos::boost_thread_priority(true);
    let _span = tracing::info_span!("screen", index = run.index).entered();
    let intent = screen_intent(&run.ask, base.plan.codec);
    while std::time::Instant::now() < run.not_before {
        if run.leaving(&base) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let fp = screen_identity(base.params.client_fp, run.index);
    let mut params = base.params.clone();
    params.client_fp = Some(fp);
    // The compositor draws the pointer into this picture; the client's cursor is the first's.
    params.hw_cursor = false;
    params.launch = None;
    params.join_live = false;
    // The hello's size is screen 0's.
    params.display_mm = None;
    let plan = crate::session_plan::SessionPlan {
        cursor_forward: false,
        cursor_blend: false,
        gamescope_cursor: false,
        ..base.plan
    };
    if let Some(why) = over_budget() {
        tracing::warn!(screen = run.index, "{why} — the screen is left out");
        return;
    }
    // A further screen is always one of its own: a session mirrors one monitor at most.
    let own = crate::vdisplay::policy::DisplayTarget::Own;
    let mut vd = match pf_vdisplay::open_for(base.compositor, own) {
        Ok(vd) => vd,
        Err(e) => {
            tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen display backend did not open");
            return;
        }
    };
    params.apply(&mut *vd);
    let quit = base.life.quit_flag().clone();
    let (mut epoch, mut au_seq, mut lost) = (
        run.seq.epoch.load(Ordering::Relaxed),
        run.seq.au_seq.load(Ordering::Relaxed),
        0u32,
    );
    while !run.leaving(&base) && lost <= MAX_REBUILDS {
        let pipe = {
            let _idd_setup = match crate::windows::idd::setup_guard(
                plan.capture,
                Some(fp),
                (run.mode.width, run.mode.height),
                &run.stop,
            ) {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen slot refused");
                    break;
                }
            };
            let req = BuildRequest {
                terms: base.terms.clone(),
                intent,
                bitrate_kbps: base.live_bitrate.load(Ordering::Relaxed),
                bitrate_auto: false,
                bit_depth: base.bit_depth,
                enc_of: enc_of(&base, &run),
                plan,
                wire_seq_base: au_seq,
                screen: run.index,
            };
            build_pipeline_with_retry(&mut vd, &req, &quit, &run.stop, None, 1..=4, None)
        };
        let pipe = match pipe {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen did not build");
                break;
            }
        };
        let delivered = pipe.geometry.delivered();
        let (x, y) = place(&mut *vd, fp, &base, &run.ask, pipe.display_gen);
        let mut cfg = StreamConfig {
            stream: run.stream,
            epoch,
            mode: delivered,
            picture: pipe.geometry.picture().wire(delivered),
            ..Default::default()
        };
        cfg.set_format(super::state::format_of(
            &plan,
            &*pipe.capturer,
            base.bit_depth,
        ));
        // Built and configured under the lock the set stops a loop under: every layout after the
        // config names this screen until it stops, and no config follows one that dropped it.
        {
            let mut built = run.built.lock().unwrap_or_else(|e| e.into_inner());
            if run.leaving(&base) {
                break;
            }
            *built = Some(Screen {
                index: run.index,
                stream: run.stream,
                mode: delivered,
                x,
                y,
                primary: false,
                view: None,
            });
            let encodes = Some(pipe.geometry.picture());
            *run.encodes.lock().unwrap_or_else(|e| e.into_inner()) = encodes;
            let _ = base.tell.send(ScreenTell::Config(cfg));
        }
        tracing::info!(screen = run.index, ?delivered, x, y, "screen built");
        let sent_before = au_seq;
        match stream_screen(&base, &mut run, &mut *vd, fp, pipe, epoch, &mut au_seq) {
            Ended::Stop => break,
            Ended::Rebuild => {
                lost = losses_in_a_row(lost, au_seq != sent_before);
                epoch = epoch.wrapping_add(1);
            }
        }
    }
    run.seq
        .epoch
        .store(epoch.wrapping_add(1), Ordering::Relaxed);
    run.seq.au_seq.store(au_seq, Ordering::Relaxed);
    if lost > MAX_REBUILDS {
        tracing::warn!(
            screen = run.index,
            lost,
            "screen's capture was lost again after each rebuild — the screen is left out"
        );
    }
    run.set_built(None);
}

/// A further screen the client may ask for: a slot past the first, at a mode the handshake
/// and a Reconfigure would accept for the first screen. The size reaches the compositor as a
/// virtual output before any encoder can refuse it.
fn screen_fits(s: &Screen, codec: crate::encode::Codec) -> bool {
    s.index > 0
        && usize::from(s.index) < SCREENS_MAX
        && crate::encode::validate_dimensions(codec, s.mode.width, s.mode.height).is_ok()
        && crate::encode::validate_refresh(s.mode.refresh_hz).is_ok()
}

/// The loss count after a capture loss: a pipeline that `delivered` a frame was a recovery,
/// so this loss starts a new row.
fn losses_in_a_row(lost: u32, delivered: bool) -> u32 {
    if delivered {
        1
    } else {
        lost + 1
    }
}

/// Over the host's `max_displays` with this screen, live and pinned displays counted.
fn over_budget() -> Option<String> {
    let max = crate::vdisplay::policy::prefs()
        .get()
        .effective()
        .max_displays;
    let held = crate::vdisplay::registry::snapshot()
        .displays
        .iter()
        .filter(|d| d.state != "lingering")
        .count() as u32;
    (held >= max)
        .then(|| format!("host display budget exhausted: {held} live, max_displays = {max}"))
}

/// A display on the desktop, as the registry the console reads has it.
#[cfg(not(target_os = "windows"))]
#[derive(Clone, Copy)]
struct Desk {
    at: (i32, i32),
    /// The compositor's scale for it: desktop units are its pixels over this.
    scale: f64,
    /// What it delivers, in pixels.
    size: (u32, u32),
}

/// Display `display_gen` as the registry has it.
#[cfg(not(target_os = "windows"))]
fn desk_of(display_gen: Option<u64>) -> Option<Desk> {
    let generation = display_gen?;
    crate::vdisplay::registry::snapshot()
        .displays
        .iter()
        .find(|d| d.slot == generation)
        .map(|d| Desk {
            at: d.position,
            scale: d.scale,
            size: (d.mode.0, d.mode.1),
        })
}

/// The rate derivation at this screen's share and the FEC in force.
fn enc_of(base: &ScreenBase, run: &ScreenRun) -> EncDerive {
    EncDerive::new(
        &base.terms,
        base.hz,
        base.fec_target.load(Ordering::Relaxed),
        run.share.load(Ordering::Relaxed),
    )
}

/// One pipeline's frames, at its own cadence, until stopped or its capture is lost. A screen
/// the client moved is placed again on `vd`, with no rebuild.
fn stream_screen(
    base: &ScreenBase,
    run: &mut ScreenRun,
    vd: &mut dyn crate::vdisplay::VirtualDisplay,
    fp: [u8; 32],
    pipe: Pipeline,
    epoch: u8,
    au_seq: &mut u32,
) -> Ended {
    let Pipeline {
        mut capturer,
        mut enc,
        mut frame,
        geometry,
        display_gen,
        ..
    } = pipe;
    let interval = geometry.interval();
    // Capture times of the frames handed to the encoder and not yet polled.
    let mut inflight = std::collections::VecDeque::<u64>::new();
    let mut rate = (base.live_bitrate.load(Ordering::Relaxed), enc_of(base, run));
    let mut last_key = std::time::Instant::now();
    let mut next = std::time::Instant::now();
    loop {
        if run.leaving(base) {
            return Ended::Stop;
        }
        let moved = run.moved.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(ask) = moved {
            run.ask = ask;
            let (x, y) = place(vd, fp, base, &ask, display_gen);
            if let Some(b) = run.built.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                (b.x, b.y) = (x, y);
            }
            tracing::info!(screen = run.index, x, y, "screen moved");
        }
        if base.wire.take_keyframe(run.stream) && last_key.elapsed() >= KEY_EVERY {
            enc.request_keyframe();
            last_key = std::time::Instant::now();
        }
        let now_rate = (base.live_bitrate.load(Ordering::Relaxed), enc_of(base, run));
        let key = |r: &(u32, EncDerive)| (r.0, r.1.share_permille(), r.1.fec_target());
        if key(&now_rate) != key(&rate) {
            rate = now_rate;
            enc.reconfigure_bitrate(u64::from(rate.1.enc_kbps(rate.0)) * 1000);
        }
        if let Some(feed) = capturer.feed() {
            feed.wait_arrival(next);
        } else if let Some(wait) = next.checked_duration_since(std::time::Instant::now()) {
            std::thread::sleep(wait);
        }
        let fresh = match capturer.try_latest() {
            Ok(Some(f)) => {
                frame = f;
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen capture lost — rebuilding");
                return Ended::Rebuild;
            }
        };
        let now = now_ns();
        let capture_ns = if fresh && frame.pts_ns > 0 && frame.pts_ns <= now {
            frame.pts_ns
        } else {
            now
        };
        let owed =
            crate::send_pacing::owed_aus(&frame, &mut *enc, std::time::Instant::now() + interval);
        if owed.is_none() {
            let wire_index = au_seq.wrapping_add(inflight.len() as u32);
            if let Err(e) = enc.submit_indexed(&frame, wire_index) {
                tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen encode failed — rebuilding");
                return Ended::Rebuild;
            }
        }
        for _ in 0..owed.unwrap_or(1) {
            inflight.push_back(capture_ns);
        }
        while !inflight.is_empty() {
            let au = match enc.poll() {
                Ok(Some(au)) => au,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(screen = run.index, error = %format!("{e:#}"), "screen encoder failed — rebuilding");
                    return Ended::Rebuild;
                }
            };
            let cap = inflight.pop_front().unwrap_or(now);
            if au.keyframe {
                last_key = std::time::Instant::now();
            }
            let msg = FrameMsg {
                meta: AuMeta {
                    capture_ns: if owed.is_some() && au.pts_ns > 0 {
                        au.pts_ns
                    } else {
                        cap
                    },
                    flags: if au.keyframe {
                        (FLAG_PIC | FLAG_SOF) as u32
                    } else {
                        FLAG_PIC as u32
                    },
                    epoch,
                    stream: run.stream,
                    frame_index: *au_seq,
                    encode_us: 0,
                    queue_us: 0,
                    cap_us: 0,
                    submit_us: 0,
                    wait_us: 0,
                    repeat: !fresh,
                    was_measured: false,
                    driver: None,
                },
                data: au.data,
            };
            if !hand_to_send(base, run, msg) {
                return Ended::Stop;
            }
            *au_seq = au_seq.wrapping_add(1);
        }
        next += interval;
        let now = std::time::Instant::now();
        if next < now {
            next = now;
        }
    }
}

/// Queue `msg` on the send thread, waiting while its queue is full. `false` once the thread is
/// gone or this loop is leaving.
fn hand_to_send(base: &ScreenBase, run: &ScreenRun, msg: FrameMsg) -> bool {
    let mut msg = SendMsg::Frame(msg);
    loop {
        match base.send.try_send(msg) {
            Ok(()) => return true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            Err(std::sync::mpsc::TrySendError::Full(back)) => {
                if run.leaving(base) {
                    return false;
                }
                msg = back;
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A further screen is held to the first screen's mode rules before any display is made.
    #[test]
    fn a_further_screen_asks_a_mode_the_first_would_get() {
        let screen = |index, width, height, refresh_hz| Screen {
            index,
            mode: punktfunk_core::Mode {
                width,
                height,
                refresh_hz,
                refresh_mhz: 0,
            },
            ..Default::default()
        };
        let hevc = crate::encode::Codec::H265;
        assert!(screen_fits(&screen(1, 1920, 1080, 60), hevc));
        assert!(
            !screen_fits(&screen(0, 1920, 1080, 60), hevc),
            "the first screen"
        );
        assert!(!screen_fits(&screen(1, 65_535, 65_535, 60), hevc));
        assert!(!screen_fits(&screen(1, 1921, 1080, 60), hevc), "odd width");
        assert!(!screen_fits(&screen(1, 1920, 0, 60), hevc));
        assert!(!screen_fits(&screen(1, 1920, 1080, 0), hevc));
    }

    /// A screen that streamed between its losses keeps healing; only losses with no frame
    /// between them run out the count.
    #[test]
    fn a_screen_gives_up_only_on_losses_in_a_row() {
        let mut lost = 0;
        for _ in 0..3 * MAX_REBUILDS {
            lost = losses_in_a_row(lost, true);
        }
        assert_eq!(lost, 1, "recovered losses were counted against the screen");
        for _ in 0..MAX_REBUILDS {
            lost = losses_in_a_row(lost, false);
        }
        assert!(lost > MAX_REBUILDS);
    }

    fn mode(width: u32, height: u32, refresh_hz: u32) -> punktfunk_core::Mode {
        punktfunk_core::Mode {
            width,
            height,
            refresh_hz,
            refresh_mhz: 0,
        }
    }

    /// One screen takes the whole video; two take it by pixel rate.
    #[test]
    fn screens_share_the_video_by_pixel_rate() {
        assert_eq!(shares(mode(1920, 1080, 60), &[]), (SHARE_ALL, vec![]));
        let (first, rest) = shares(mode(1920, 1080, 60), &[mode(1920, 1080, 60)]);
        assert_eq!((first, rest), (500, vec![500]));
        let (first, rest) = shares(mode(3840, 2160, 60), &[mode(1920, 1080, 60)]);
        assert_eq!((first, rest), (800, vec![200]));
        let (first, rest) = shares(mode(2560, 1440, 144), &[mode(1920, 1080, 60)]);
        assert!(first + rest[0] <= SHARE_ALL && first > rest[0] * 3);
    }

    /// A KWin session's base for a set under test, with the ends its frames and layouts reach.
    /// Hold the frame end for the test's length.
    fn kwin_base() -> (
        ScreenBase,
        std::sync::mpsc::Receiver<SendMsg>,
        tokio::sync::mpsc::UnboundedReceiver<ScreenTell>,
        Arc<ScreenWire>,
    ) {
        let (send, frames) = std::sync::mpsc::sync_channel(3);
        let (tell, told) = tokio::sync::mpsc::unbounded_channel();
        let wire = Arc::new(ScreenWire::new());
        let mut terms = crate::native::handshake::tests::terms_for(
            crate::native::handshake::tests::hello_at(mode(1920, 1080, 60)),
            crate::encode::resolve(),
        );
        (terms.audio_kbps, terms.shard_payload) = (512, 1408);
        let base = ScreenBase {
            terms: Arc::new(terms),
            compositor: crate::vdisplay::Compositor::Kwin,
            params: crate::vdisplay::SessionParams {
                screen: None,
                topology: None,
                client_fp: Some([3; 32]),
                client_hdr: None,
                display_mm: None,
                hdr: false,
                hw_cursor: true,
                join_live: false,
                quit: Arc::default(),
                launch: Some("steam:570".into()),
                route: None,
                isolation: None,
            },
            plan: crate::session_plan::SessionPlan {
                capture: crate::session_plan::CaptureBackend::Portal,
                topology: crate::session_plan::SessionTopology::SingleProcess,
                encoder: crate::session_plan::EncoderBackend::Software,
                bit_depth: 8,
                hdr: false,
                chroma: crate::encode::ChromaFormat::Yuv420,
                codec: crate::encode::Codec::H265,
                wire_chunk: None,
                pyrowave_bpp: 0.0,
                cursor_blend: false,
                cursor_forward: true,
                gamescope_cursor: false,
                sdr10_native: false,
                slices: 1,
                mirrored: false,
                rgb_full_range: crate::encode::RgbFullRange::BOTH,
                studio_asked: crate::session_plan::StudioAsk::default(),
            },
            bit_depth: 8,
            hz: 60,
            fec_target: Arc::new(AtomicU8::new(10)),
            live_bitrate: Arc::new(AtomicU32::new(40_000)),
            send: Arc::new(send),
            tell,
            wire: wire.clone(),
            life: crate::lifecycle::Lifecycle::new(),
            first: Arc::default(),
        };
        (base, frames, told, wire)
    }

    fn asked(index: u8, mode: punktfunk_core::Mode) -> Screen {
        Screen {
            index,
            mode,
            ..Default::default()
        }
    }

    /// A set asked for a second screen splits the video and tells a layout of what is built,
    /// screen 0 alone until the second builds; dropping the second stops its loop, hands the
    /// whole video back and is answered with the layout, unchanged as it is.
    #[test]
    fn the_set_follows_the_clients_screens() {
        let (base, _frames, mut told, wire) = kwin_base();
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), asked(1, one)]);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        set.poll(one, Picture::of(one), None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("a layout is told at once");
        };
        assert_eq!(layout.len(), 1, "only screen 0 is built yet");
        assert_eq!(
            (layout[0].index, layout[0].stream, layout[0].mode),
            (0, 0, one)
        );
        screens_tx.send(vec![asked(0, one)]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert!(set.loops.is_empty(), "screen 1's loop stopped");
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), SHARE_ALL);
        let Ok(ScreenTell::Layout(again)) = told.try_recv() else {
            panic!("an ask is answered with a layout");
        };
        assert_eq!(again, layout);
        set.poll(one, Picture::of(one), None);
        assert!(
            told.try_recv().is_err(),
            "no ask and no change, nothing told"
        );
        set.shutdown();
        assert!(set.base.is_none());
    }

    /// A client that promised screens but asked for none at Hello: screen 0 is told once and
    /// then nothing, and a screen asked for later starts as one asked at Hello.
    #[test]
    fn a_set_with_no_ask_waits_quietly() {
        let (base, _frames, mut told, wire) = kwin_base();
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, Vec::new());
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), SHARE_ALL);
        set.poll(one, Picture::of(one), None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("screen 0 is told at once");
        };
        assert_eq!((layout.len(), layout[0].index, layout[0].mode), (1, 0, one));
        set.poll(one, Picture::of(one), None);
        assert!(told.try_recv().is_err(), "nothing moved, nothing told");
        screens_tx.send(vec![asked(0, one), asked(1, one)]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert_eq!(set.loops.len(), 1, "screen 1's loop started");
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        set.shutdown();
    }

    /// A screen sits where the client's monitor does beside its first, scaled to what the host
    /// delivers on the first; one the client placed on top of the first is left to the host.
    #[test]
    fn a_screen_sits_where_the_clients_monitor_does() {
        let first = FirstScreen {
            asked: Screen {
                mode: mode(2560, 1440, 60),
                ..Default::default()
            },
            mode: mode(1920, 1080, 60),
            display_gen: Some(1),
        };
        let left = Screen {
            index: 1,
            mode: mode(1920, 1080, 60),
            x: -1920,
            y: 240,
            ..Default::default()
        };
        assert_eq!(offset_of(&first, &left), Some((-1440, 180)));
        let unplaced = Screen { x: 0, y: 0, ..left };
        assert_eq!(offset_of(&first, &unplaced), None);
    }

    /// A compositor positions in desktop units: at 150% the screen right of a 2560-pixel
    /// first starts 1706 on, not 2560. One left of the first reaches back its own extent,
    /// whatever the first's scale.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn a_screen_is_placed_in_desktop_units() {
        assert_eq!(desktop_axis(2560, 2560, 1.5, (2560, 1.5)), 1706);
        assert_eq!(desktop_axis(0, 1440, 1.5, (1440, 1.5)), 0);
        assert_eq!(desktop_axis(-2560, 2560, 1.0, (2560, 1.5)), -1706);
        assert_eq!(desktop_axis(-1920, 1920, 1.5, (1920, 1.0)), -1920);
        assert_eq!(desktop_axis(1920, 1920, 1.0, (2560, 2.0)), 1920);
    }

    /// Each screen of a client has one identity, apart from its other screens' and from
    /// another client's.
    #[test]
    fn a_screen_keeps_its_own_identity() {
        let fp = Some([7u8; 32]);
        assert_eq!(screen_identity(fp, 1), screen_identity(fp, 1));
        assert_ne!(screen_identity(fp, 1), screen_identity(fp, 2));
        assert_ne!(screen_identity(fp, 1), screen_identity(Some([8u8; 32]), 1));
        assert_ne!(screen_identity(fp, 1), [7u8; 32]);
    }

    /// One client list against the further screens running and retiring: the first record the
    /// set keeps (`None`: the old one), the loops it stops, the screens it starts and the ones
    /// that wait for their index's old loop.
    struct PlanRow {
        name: &'static str,
        running: Vec<Screen>,
        retiring: Vec<u8>,
        want: Vec<Screen>,
        first: Option<Screen>,
        stop: Vec<u8>,
        start: Vec<Screen>,
        wait: Vec<Screen>,
    }

    fn plan_rows() -> Vec<PlanRow> {
        let (one, two) = (mode(1920, 1080, 60), mode(2560, 1440, 60));
        let moved = Screen {
            x: 1920,
            ..asked(1, one)
        };
        let viewed = Screen {
            view: Some((1280, 720)),
            ..asked(1, one)
        };
        let row =
            |name, running: &[Screen], want: &[Screen], stop: &[u8], start: &[Screen]| PlanRow {
                name,
                running: running.to_vec(),
                retiring: Vec::new(),
                want: want.to_vec(),
                first: want.iter().find(|s| s.index == 0).copied(),
                stop: stop.to_vec(),
                start: start.to_vec(),
                wait: Vec::new(),
            };
        vec![
            row(
                "a list that adds a screen starts its loop",
                &[],
                &[asked(0, one), asked(1, one)],
                &[],
                &[asked(1, one)],
            ),
            row(
                "a list that drops a screen stops its loop",
                &[asked(1, one)],
                &[asked(0, one)],
                &[1],
                &[],
            ),
            PlanRow {
                wait: vec![asked(1, two)],
                ..row(
                    "S8-6: a resized screen's new loop waits for its old one to end",
                    &[asked(1, one)],
                    &[asked(0, one), asked(1, two)],
                    &[1],
                    &[],
                )
            },
            PlanRow {
                wait: vec![viewed],
                ..row(
                    "a screen shown at another view restarts its loop like a resized one",
                    &[asked(1, one)],
                    &[asked(0, one), viewed],
                    &[1],
                    &[],
                )
            },
            PlanRow {
                retiring: vec![1],
                wait: vec![asked(1, two)],
                ..row(
                    "S8-6: a screen asked while its index retires waits, the others start",
                    &[],
                    &[asked(0, one), asked(1, two), asked(2, one)],
                    &[],
                    &[asked(2, one)],
                )
            },
            PlanRow {
                retiring: vec![1],
                ..row(
                    "a retiring index the list leaves out holds nothing back",
                    &[asked(2, one)],
                    &[asked(0, one), asked(2, one)],
                    &[],
                    &[],
                )
            },
            row(
                "a list that repeats the running screens keeps their loops",
                &[asked(1, one), asked(2, one)],
                &[asked(0, one), asked(1, one), asked(2, one)],
                &[],
                &[],
            ),
            row(
                "a screen moved but not resized keeps its loop",
                &[asked(1, one)],
                &[asked(0, one), moved],
                &[],
                &[],
            ),
            row(
                "an index the list repeats is the first of it",
                &[],
                &[asked(0, one), asked(1, one), asked(1, two)],
                &[],
                &[asked(1, one)],
            ),
            row(
                "index 0 is the first screen, the first of it in the list, never a further one",
                &[],
                &[asked(0, two), asked(0, one)],
                &[],
                &[],
            ),
            row(
                "a list without screen 0 keeps the first record",
                &[asked(1, one)],
                &[asked(1, one)],
                &[],
                &[],
            ),
            row(
                "an empty list stops every further screen",
                &[asked(1, one), asked(2, two)],
                &[],
                &[1, 2],
                &[],
            ),
            row(
                "an index past the last slot stays out",
                &[],
                &[asked(0, one), asked(SCREENS_MAX as u8, one)],
                &[],
                &[],
            ),
            row(
                "S8-4: a screen at a mode the first screen would not get stays out",
                &[],
                &[
                    asked(0, one),
                    asked(1, mode(1921, 1080, 60)),
                    asked(2, mode(1920, 1080, 0)),
                    asked(3, mode(65_535, 65_535, 60)),
                ],
                &[],
                &[],
            ),
            row(
                "S8-4: a running screen resized past the rules stops and stays out",
                &[asked(1, one)],
                &[asked(0, one), asked(1, mode(1920, 0, 60))],
                &[1],
                &[],
            ),
            row(
                "S8-4: a refused screen leaves its index to the next of it",
                &[],
                &[asked(0, one), asked(1, mode(1921, 1080, 60)), asked(1, two)],
                &[],
                &[asked(1, two)],
            ),
        ]
    }

    /// A loop in a build that does not look at its stop until `gate` drops.
    fn building(ask: Screen) -> (ScreenHandle, std::sync::mpsc::Sender<()>) {
        let (gate, held) = std::sync::mpsc::channel::<()>();
        let stop = Arc::new(AtomicBool::new(false));
        let ran = stop.clone();
        let thread = std::thread::spawn(move || {
            let _ran = ran;
            let _ = held.recv_timeout(std::time::Duration::from_secs(10));
        });
        let handle = ScreenHandle {
            ask,
            share: Arc::default(),
            stop,
            moved: Arc::default(),
            built: Arc::default(),
            encodes: Arc::default(),
            thread: Some(thread),
        };
        (handle, gate)
    }

    /// The first record, the stopped loops, the started screens and the waiting ones after a
    /// set running `row.running` past a 1080p60 first screen, with `row.retiring` mid-build,
    /// applies `row.want`.
    fn applied(row: &PlanRow) -> (Screen, Vec<u8>, Vec<Screen>, Vec<Screen>) {
        let (base, _frames, _told, _wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let first = asked(0, mode(1920, 1080, 60));
        let at_hello = std::iter::once(first).chain(row.running.iter().copied());
        let mut set = ScreenSet::new(base, screens_rx, at_hello.collect());
        let running: Vec<Screen> = set.loops.iter().map(|h| h.ask).collect();
        assert_eq!(running, row.running, "{}: the running screens", row.name);
        let mut gates = Vec::new();
        for &index in &row.retiring {
            let (handle, gate) = building(asked(index, first.mode));
            set.retiring.push(handle);
            gates.push(gate);
        }
        let before: Vec<(u8, Arc<AtomicBool>)> = set
            .loops
            .iter()
            .map(|h| (h.ask.index, h.stop.clone()))
            .collect();
        set.apply(row.want.clone());
        let stopped = before
            .iter()
            .filter(|(_, stop)| stop.load(Ordering::SeqCst))
            .map(|(index, _)| *index)
            .collect();
        let started = set
            .loops
            .iter()
            .filter(|h| !before.iter().any(|(_, stop)| Arc::ptr_eq(stop, &h.stop)))
            .map(|h| h.ask)
            .collect();
        let (kept, waiting) = (set.first, set.pending.clone());
        drop(gates);
        set.shutdown();
        (kept, stopped, started, waiting)
    }

    /// Every row, as the plan decides it for an HEVC session.
    #[test]
    fn each_list_plans_as_its_row_says() {
        for row in plan_rows() {
            let plan = ScreenSet::plan(
                &row.want,
                &row.running,
                &row.retiring,
                crate::encode::Codec::H265,
            );
            let expect = ScreenPlan {
                first: row.first,
                stop: row.stop.clone(),
                start: row.start.clone(),
                wait: row.wait.clone(),
            };
            assert_eq!(plan, expect, "{}", row.name);
        }
    }

    /// Every row, as the set applies it.
    #[test]
    fn the_set_applies_each_list_as_its_row_says() {
        for row in plan_rows() {
            let first = row.first.unwrap_or(asked(0, mode(1920, 1080, 60)));
            assert_eq!(
                applied(&row),
                (first, row.stop.clone(), row.start.clone(), row.wait.clone()),
                "{}",
                row.name
            );
        }
    }

    /// S8-6: a removed screen mid-build retires off the first screen's tick; asked again, it
    /// waits without a build slot until its old loop has ended, then starts.
    #[test]
    fn s8_6_a_removed_screen_retires_off_the_tick() {
        let (base, _frames, _told, _wire) = kwin_base();
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one)]);
        let (handle, gate) = building(asked(1, one));
        set.loops.push(handle);
        screens_tx.send(vec![asked(0, one)]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert!(set.loops.is_empty(), "screen 1 still runs");
        assert_eq!(set.retiring.len(), 1, "screen 1 was joined on the tick");
        let next_build = set.next_build;
        screens_tx.send(vec![asked(0, one), asked(1, one)]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert!(set.loops.is_empty(), "screen 1 started beside its old loop");
        assert_eq!(set.pending, vec![asked(1, one)]);
        assert_eq!(
            set.next_build, next_build,
            "a waiting start took a build slot"
        );
        drop(gate);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while set.loops.is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "screen 1 never started"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            set.poll(one, Picture::of(one), None);
        }
        assert!(set.retiring.is_empty() && set.pending.is_empty());
        assert_eq!(set.loops[0].ask, asked(1, one));
        set.shutdown();
    }

    /// A screen the client moves at its mode keeps its loop, which is handed the record to
    /// place again, as when the first moves; a primary flag alone places nothing and reaches
    /// the layout.
    #[test]
    fn a_moved_screen_keeps_its_loop_and_places_again() {
        let (base, _frames, mut told, _wire) = kwin_base();
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one)]);
        let (handle, gate) = building(asked(1, one));
        let (stop, moved) = (handle.stop.clone(), handle.moved.clone());
        *handle.built.lock().unwrap() = Some(Screen {
            stream: 2,
            ..asked(1, one)
        });
        set.loops.push(handle);
        let right = Screen {
            x: 1920,
            ..asked(1, one)
        };
        screens_tx.send(vec![asked(0, one), right]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert!(!stop.load(Ordering::SeqCst), "a move stopped the loop");
        assert_eq!(moved.lock().unwrap().take(), Some(right));
        let primary = Screen {
            primary: true,
            ..right
        };
        screens_tx.send(vec![asked(0, one), primary]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert_eq!(*moved.lock().unwrap(), None, "a primary flag placed it");
        let mut layout = None;
        while let Ok(tell) = told.try_recv() {
            if let ScreenTell::Layout(l) = tell {
                layout = Some(l);
            }
        }
        assert_eq!(layout.map(|l| l[1].primary), Some(true));
        let first_left = Screen {
            x: -1920,
            ..asked(0, one)
        };
        screens_tx.send(vec![first_left, primary]).unwrap();
        set.poll(one, Picture::of(one), None);
        assert_eq!(*moved.lock().unwrap(), Some(primary), "the first moved");
        let first = set.base.as_ref().unwrap().first.lock().unwrap().asked;
        assert_eq!(first, first_left, "the loop places against the old first");
        drop(gate);
        set.shutdown();
    }

    /// Shutdown waits for a retiring loop as for a running one.
    #[test]
    fn shutdown_joins_a_retiring_loop() {
        let (base, _frames, _told, _wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let mut set = ScreenSet::new(base, screens_rx, Vec::new());
        let (handle, gate) = building(asked(1, mode(1920, 1080, 60)));
        let stop = handle.stop.clone();
        set.retiring.push(handle);
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            drop(gate);
        });
        set.shutdown();
        assert_eq!(
            Arc::strong_count(&stop),
            1,
            "a retiring loop outlived shutdown"
        );
        release.join().unwrap();
    }

    /// S8-3: a session that moved under its screens restarts each on the new base with the
    /// client's list unchanged, and drops one the new codec refuses (S8-4).
    #[test]
    fn s8_3_a_rebase_restarts_every_screen_on_the_new_base() {
        let (base, _frames, _told, _wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let (one, wide) = (mode(1920, 1080, 60), mode(5120, 1440, 60));
        let list = vec![asked(0, one), asked(1, one), asked(2, wide)];
        let mut set = ScreenSet::new(base, screens_rx, list);
        let old: Vec<Arc<AtomicBool>> = set.loops.iter().map(|h| h.stop.clone()).collect();
        assert_eq!(old.len(), 2, "both screens fit HEVC");
        set.rebase(|b| b.plan.codec = crate::encode::Codec::H264);
        for stop in &old {
            assert!(stop.load(Ordering::SeqCst), "an old loop was left running");
            assert_eq!(Arc::strong_count(stop), 1, "an old loop had not ended");
        }
        let asks: Vec<Screen> = set.loops.iter().map(|h| h.ask).collect();
        assert_eq!(asks, vec![asked(1, one)], "5120 px is past H.264's limit");
        assert_eq!(
            set.base.as_ref().map(|b| b.plan.codec),
            Some(crate::encode::Codec::H264)
        );
        set.shutdown();
    }

    /// On gamescope every further screen waits: the layout names screen 0 alone, which keeps
    /// the whole video, and a new list only changes what waits. A rebase onto KWin starts the
    /// waiting screens; a rebase back onto gamescope stops them and they wait again.
    #[test]
    fn gamescope_parks_further_screens_until_a_desktop() {
        use crate::vdisplay::Compositor::{Gamescope, Kwin};
        let (mut base, _frames, mut told, wire) = kwin_base();
        base.compositor = Gamescope;
        let (screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let three = vec![asked(0, one), asked(1, one), asked(2, one)];
        let mut set = ScreenSet::new(base, screens_rx, three.clone());
        assert!(set.loops.is_empty(), "a screen started on gamescope");
        assert_eq!(set.pending, three[1..]);
        set.poll(one, Picture::of(one), None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("a layout is told at once");
        };
        assert_eq!(layout.len(), 1, "only screen 0 runs on gamescope");
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), SHARE_ALL);
        screens_tx.send(three[..2].to_vec()).unwrap();
        set.poll(one, Picture::of(one), None);
        assert!(
            set.loops.is_empty(),
            "a new list started a screen on gamescope"
        );
        assert_eq!(set.pending, three[1..2]);
        screens_tx.send(three.clone()).unwrap();
        set.poll(one, Picture::of(one), None);
        set.rebase(|b| b.compositor = Kwin);
        let running: Vec<Screen> = set.loops.iter().map(|h| h.ask).collect();
        assert_eq!(
            running,
            three[1..],
            "the desktop starts the waiting screens"
        );
        assert!(set.pending.is_empty());
        let started: Vec<Arc<AtomicBool>> = set.loops.iter().map(|h| h.stop.clone()).collect();
        set.rebase(|b| b.compositor = Gamescope);
        for stop in &started {
            assert_eq!(Arc::strong_count(stop), 1, "a loop outlived the switch");
        }
        assert!(set.loops.is_empty(), "a screen kept running on gamescope");
        assert_eq!(set.pending, three[1..], "the screens wait again");
        set.shutdown();
    }

    /// A further screen keeps only a view its mode can be scaled to, and weighs in the split at
    /// the picture it encodes.
    #[test]
    fn a_further_screen_is_framed_only_for_a_view_inside_its_mode() {
        let (one, qhd) = (mode(1920, 1080, 60), mode(2560, 1440, 60));
        let set_at = |view| {
            let (base, _frames, _told, wire) = kwin_base();
            let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
            let second = Screen {
                view: Some(view),
                ..asked(1, qhd)
            };
            let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), second]);
            let kept = set.loops[0].ask.view;
            set.shutdown();
            (kept, wire.primary_share.load(Ordering::Relaxed))
        };
        assert_eq!(set_at((1920, 1080)), (Some((1920, 1080)), 500));
        for too_big in [(3840, 2160), (2560, 1440), (1921, 1080)] {
            assert_eq!(set_at(too_big), (None, 360), "{too_big:?}");
        }
    }

    /// One split: the first screen's mode and the picture its encoder takes it to, a further
    /// screen's ask and the picture its build encodes; the first's share after the set polls.
    struct SplitRow {
        name: &'static str,
        first: (punktfunk_core::Mode, punktfunk_core::Mode),
        further: (Screen, Option<punktfunk_core::Mode>),
        first_share: u16,
        verdict: crate::native::tests::Verdict,
    }

    #[rustfmt::skip]
    fn split_rows() -> Vec<SplitRow> {
        use crate::native::tests::Verdict::Wanted;
        let (one, qhd, uhd) = (mode(1920, 1080, 60), mode(2560, 1440, 60), mode(3840, 2160, 60));
        let viewed = |mode, view| Screen { view: Some(view), ..asked(1, mode) };
        let row = |name, first, further, first_share, verdict| SplitRow {
            name, first, further, first_share, verdict,
        };
        vec![
            row("two screens at one size: half each", (one, one), (asked(1, one), None), 500,
                Wanted),
            row("a first screen framed at its view weighs the view", (uhd, one),
                (asked(1, one), None), 500, Wanted),
            row("a first screen whose framing fell back weighs what it encodes", (uhd, uhd),
                (asked(1, one), None), 800, Wanted),
            row("a further screen framed at its view weighs the view", (one, one),
                (viewed(qhd, (1920, 1080)), Some(one)), 500, Wanted),
            row("a further screen whose framing fell back weighs what it encodes",
                (one, one), (viewed(qhd, (1920, 1080)), Some(qhd)), 360, Wanted),
        ]
    }

    /// The split of the video between the first screen and a further one, each at the
    /// picture its encoder takes; a further one not yet built at the view it asked.
    #[test]
    fn geometry_rows() {
        for r in split_rows() {
            let (base, _frames, _told, wire) = kwin_base();
            let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
            let (first_mode, first_picture) = r.first;
            let (further, encodes) = r.further;
            let list = vec![asked(0, first_mode), further];
            let mut set = ScreenSet::new(base, screens_rx, list);
            *set.loops[0].encodes.lock().unwrap() = encodes.map(Picture::of);
            set.poll(first_mode, Picture::of(first_picture), None);
            let share = wire.primary_share.load(Ordering::Relaxed);
            set.shutdown();
            assert_eq!(share, r.first_share, "{} ({:?})", r.name, r.verdict);
        }
    }

    /// U7: a further screen is priced at its asked view until it builds, then at what its
    /// build encodes: one whose framing fell back to 2560×1440 weighs 2560×1440 from the next
    /// poll.
    #[test]
    fn a_further_screen_is_priced_at_what_it_encodes() {
        let verdict = crate::native::tests::Verdict::Wanted;
        let (one, qhd) = (mode(1920, 1080, 60), mode(2560, 1440, 60));
        let (base, _frames, _told, wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let further = Screen {
            view: Some((1920, 1080)),
            ..asked(1, qhd)
        };
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), further]);
        set.poll(one, Picture::of(one), None);
        let asked_share = wire.primary_share.load(Ordering::Relaxed);
        *set.loops[0].encodes.lock().unwrap() = Some(Picture::of(qhd));
        set.poll(one, Picture::of(one), None);
        let share = wire.primary_share.load(Ordering::Relaxed);
        set.shutdown();
        assert_eq!((asked_share, share), (500, 360), "{verdict:?}");
    }

    /// S8-5: the first screen's new mode re-splits the video with no new list.
    #[test]
    fn s8_5_the_first_screens_new_mode_re_splits_the_video() {
        let (base, _frames, _told, wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), asked(1, one)]);
        set.poll(one, Picture::of(one), None);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        let uhd = mode(3840, 2160, 60);
        set.poll(uhd, Picture::of(uhd), None);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 800);
        // Shown at 1080p, the first screen weighs what it encodes.
        set.poll(uhd, Picture::of(one), None);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        set.shutdown();
    }

    /// S8-7: a restarted screen continues its index's stream, so its old loop has ended before
    /// the new one runs: one loop holds the index's sequence.
    #[test]
    fn s8_7_a_restarted_screen_runs_after_its_old_loop_ended() {
        let (base, _frames, _told, _wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let (one, two) = (mode(1920, 1080, 60), mode(2560, 1440, 60));
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), asked(1, one)]);
        let old = set.loops[0].stop.clone();
        set.apply(vec![asked(0, one), asked(1, two)]);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !set.loops.iter().any(|h| h.ask == asked(1, two)) {
            assert!(
                std::time::Instant::now() < deadline,
                "screen 1 never restarted"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            set.poll(one, Picture::of(one), None);
        }
        assert_eq!(
            Arc::strong_count(&old),
            1,
            "the old loop ran beside the new"
        );
        assert_eq!(
            Arc::strong_count(&set.seqs[1]),
            2,
            "two loops held one sequence"
        );
        set.shutdown();
    }
}
