//! Screens past the first: a display, capture and encoder each, on a video stream of its own.
//!
//! The first screen is [`StreamState`](super::state::StreamState)'s whole loop: recovery,
//! pacing, cursor forwarding, the game. Each further screen is one thread running the session's
//! plan with the pointer drawn into its picture, and sending whole access units into the
//! session's one send thread under its own stream id, epoch and frame index. It keyframes on the
//! client's ask, rebuilds on a lost capture and encodes at its share of the session's budget.
//! [`ScreenSet`] follows the client's `Screens` and tells it what was built.

use super::pipeline::{build_pipeline_with_retry, BuildRequest, Pipeline};
use super::send::{AuMeta, FrameMsg, SendMsg};
use super::*;
use crate::native::bitrate::{EncDerive, SHARE_ALL};
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

/// [`place`] by the backend's own move; one that cannot move an output keeps the compositor's.
#[cfg(not(target_os = "windows"))]
fn place(
    vd: &mut dyn crate::vdisplay::VirtualDisplay,
    _fp: [u8; 32],
    base: &ScreenBase,
    ask: &Screen,
    display_gen: Option<u64>,
) -> (i32, i32) {
    let first = *base.first.lock().unwrap_or_else(|e| e.into_inner());
    let at = offset_of(&first, ask)
        .zip(position_of(first.display_gen))
        .map(|((dx, dy), (x, y))| (x + dx, y + dy));
    if let Some((x, y)) = at {
        vd.apply_position(x, y);
    }
    at.or_else(|| position_of(display_gen)).unwrap_or((0, 0))
}

/// What every screen loop of a session starts from.
#[derive(Clone)]
pub(super) struct ScreenBase {
    pub(super) compositor: crate::vdisplay::Compositor,
    /// The first screen's display request; each screen swaps in its own identity.
    pub(super) params: crate::vdisplay::SessionParams,
    pub(super) plan: crate::session_plan::SessionPlan,
    /// The session's `PUNKTFUNK_PYROWAVE_MAX_MBPS` cap, kbps.
    pub(super) pyrowave_max_kbps: Option<u32>,
    /// The session's encode resolution, which every screen opens on.
    pub(super) encoder: Arc<crate::encode::Resolved>,
    pub(super) bit_depth: u8,
    pub(super) enc_of: EncDerive,
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
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScreenHandle {
    fn built(&self) -> Option<Screen> {
        *self.built.lock().unwrap_or_else(|e| e.into_inner())
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
    /// The client's screens waiting for their index's retiring loop: two loops never hold one
    /// display identity or stream.
    pending: Vec<Screen>,
    /// The client's screen 0 record, for its position and primary flag.
    first: Screen,
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
            told: Vec::new(),
            next_build: std::time::Instant::now() + BUILD_SPACING,
            seqs: Default::default(),
        };
        set.apply(asked);
        set
    }

    /// Each turn of the first screen's loop: the client's newest list, ended loops, the starts
    /// they held back, the layout when it moved or the client asked. `first_mode` is what the
    /// first screen delivers now, on the display of registry generation `first_gen`. Never
    /// waits on a loop.
    pub(super) fn poll(&mut self, first_mode: punktfunk_core::Mode, first_gen: Option<u64>) {
        if let Some(base) = self.base.as_ref() {
            let mut f = base.first.lock().unwrap_or_else(|e| e.into_inner());
            (f.asked, f.mode, f.display_gen) = (self.first, first_mode, first_gen);
        }
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
        // Shares go by pixel rate, so a first screen that changed mode moves them as a screen
        // joining or leaving does.
        let first_moved = self.told.first().is_some_and(|f| f.mode != first_mode);
        if self.loops.len() != before || first_moved {
            self.reshare(first_mode);
        }
        self.tell_layout(first_mode, asked);
    }

    /// What `want` asks of the loops `running`: a further screen that fits `codec`, the first
    /// of its index in the list, keeps the loop at its index and mode or starts one; every
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
        let same = |a: &Screen, b: &Screen| (a.index, a.mode) == (b.index, b.mode);
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

    /// Run exactly the screens `list` names past the first, as [`Self::plan`] says. A stopped
    /// loop retires: [`Self::poll`] reaps it once its thread ends. A kept loop takes the list's
    /// record, and places its display again when the client moved it against the first.
    fn apply(&mut self, list: Vec<Screen>) {
        let Some(base) = self.base.clone() else {
            return;
        };
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
            let same = |w: &&Screen| (w.index, w.mode) == (h.ask.index, h.ask.mode);
            let Some(&now) = list.iter().find(same) else {
                continue;
            };
            if from(&now, &self.first) != from(&h.ask, &was) {
                *h.moved.lock().unwrap_or_else(|e| e.into_inner()) = Some(now);
            }
            h.ask = now;
        }
        for w in plan.start {
            self.start(&base, w);
        }
        self.pending = plan.wait;
        let first_mode = self.told.first().map_or(self.first.mode, |s| s.mode);
        self.reshare(first_mode);
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

    /// Split the video by pixel rate: the first screen's share to the first loop, each further
    /// screen's to its own.
    fn reshare(&self, first_mode: punktfunk_core::Mode) {
        let Some(base) = self.base.as_ref() else {
            return;
        };
        let asks: Vec<punktfunk_core::Mode> = self.loops.iter().map(|h| h.ask.mode).collect();
        let (first, rest) = shares(first_mode, &asks);
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

/// The shares of the video, per mille, by pixel rate: the first screen's, then each of `rest`'s.
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

/// A further screen from first build to stop: build, stream, rebuild on a lost capture.
fn run_screen(base: ScreenBase, mut run: ScreenRun) {
    let _span = tracing::info_span!("screen", index = run.index).entered();
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
    let plan = crate::session_plan::SessionPlan {
        cursor_forward: false,
        cursor_blend: false,
        gamescope_cursor: false,
        reframe_to: None,
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
                mode: run.mode,
                bitrate_kbps: base.live_bitrate.load(Ordering::Relaxed),
                bitrate_auto: false,
                bit_depth: base.bit_depth,
                enc_of: enc_of(&base, &run),
                plan,
                pyrowave_max_kbps: base.pyrowave_max_kbps,
                encoder: base.encoder.clone(),
                client_hdr: base.params.client_hdr,
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
        let delivered = delivered_mode(pipe.frame.width, pipe.frame.height, pipe.interval);
        let (x, y) = place(&mut *vd, fp, &base, &run.ask, pipe.display_gen);
        let mut cfg = StreamConfig {
            stream: run.stream,
            epoch,
            mode: delivered,
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
            });
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

/// A display's desktop origin, from the registry the console reads.
#[cfg(not(target_os = "windows"))]
fn position_of(display_gen: Option<u64>) -> Option<(i32, i32)> {
    let generation = display_gen?;
    crate::vdisplay::registry::snapshot()
        .displays
        .iter()
        .find(|d| d.slot == generation)
        .map(|d| d.position)
}

/// The rate derivation at this screen's share and the FEC in force.
fn enc_of(base: &ScreenBase, run: &ScreenRun) -> EncDerive {
    EncDerive {
        fec_target: base.fec_target.load(Ordering::Relaxed),
        share_permille: run.share.load(Ordering::Relaxed),
        ..base.enc_of
    }
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
        interval,
        display_gen,
        ..
    } = pipe;
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
        if (now_rate.0, now_rate.1.share_permille, now_rate.1.fec_target)
            != (rate.0, rate.1.share_permille, rate.1.fec_target)
        {
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
        let base = ScreenBase {
            compositor: crate::vdisplay::Compositor::Kwin,
            params: crate::vdisplay::SessionParams {
                client_fp: Some([3; 32]),
                client_hdr: None,
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
                reframe_to: None,
                rgb_full_range: true,
                studio_asked: crate::session_plan::StudioAsk::default(),
            },
            pyrowave_max_kbps: None,
            encoder: crate::encode::resolve(),
            bit_depth: 8,
            enc_of: EncDerive {
                audio_kbps: 512,
                shard_payload: 1408,
                fec_target: 10,
                pyrowave_hz: None,
                share_permille: SHARE_ALL,
            },
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
        set.poll(one, None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("a layout is told at once");
        };
        assert_eq!(layout.len(), 1, "only screen 0 is built yet");
        assert_eq!(
            (layout[0].index, layout[0].stream, layout[0].mode),
            (0, 0, one)
        );
        screens_tx.send(vec![asked(0, one)]).unwrap();
        set.poll(one, None);
        assert!(set.loops.is_empty(), "screen 1's loop stopped");
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), SHARE_ALL);
        let Ok(ScreenTell::Layout(again)) = told.try_recv() else {
            panic!("an ask is answered with a layout");
        };
        assert_eq!(again, layout);
        set.poll(one, None);
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
        set.poll(one, None);
        let Ok(ScreenTell::Layout(layout)) = told.try_recv() else {
            panic!("screen 0 is told at once");
        };
        assert_eq!((layout.len(), layout[0].index, layout[0].mode), (1, 0, one));
        set.poll(one, None);
        assert!(told.try_recv().is_err(), "nothing moved, nothing told");
        screens_tx.send(vec![asked(0, one), asked(1, one)]).unwrap();
        set.poll(one, None);
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
        set.poll(one, None);
        assert!(set.loops.is_empty(), "screen 1 still runs");
        assert_eq!(set.retiring.len(), 1, "screen 1 was joined on the tick");
        let next_build = set.next_build;
        screens_tx.send(vec![asked(0, one), asked(1, one)]).unwrap();
        set.poll(one, None);
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
            set.poll(one, None);
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
        set.poll(one, None);
        assert!(!stop.load(Ordering::SeqCst), "a move stopped the loop");
        assert_eq!(moved.lock().unwrap().take(), Some(right));
        let primary = Screen {
            primary: true,
            ..right
        };
        screens_tx.send(vec![asked(0, one), primary]).unwrap();
        set.poll(one, None);
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
        set.poll(one, None);
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

    /// S8-5: the first screen's new mode re-splits the video with no new list.
    #[test]
    fn s8_5_the_first_screens_new_mode_re_splits_the_video() {
        let (base, _frames, _told, wire) = kwin_base();
        let (_screens_tx, screens_rx) = std::sync::mpsc::channel();
        let one = mode(1920, 1080, 60);
        let mut set = ScreenSet::new(base, screens_rx, vec![asked(0, one), asked(1, one)]);
        set.poll(one, None);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 500);
        set.poll(mode(3840, 2160, 60), None);
        assert_eq!(wire.primary_share.load(Ordering::Relaxed), 800);
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
            set.poll(one, None);
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
