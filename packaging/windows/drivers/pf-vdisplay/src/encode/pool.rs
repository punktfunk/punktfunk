//! The pool: three slots per monitor in the opened backend's input format, filled by the drain
//! worker's fused pass ([`Pool::offer`]) inside the acquire window and drained by the encode
//! thread. The monitor owns it, not the session: between sessions the newest frame keeps
//! landing in it, so a new `SET_ENCODE` on an idle desktop encodes the retained slot as its
//! first IDR and needs no compose. A pool built for another device epoch, size, framing or
//! format is replaced by the next session; nothing rebuilds in place. A surface no pool can
//! take is kept as the monitor's [`Seed`], which the next pool opens on — the compose at
//! swap-chain assign is a monitor's first picture, and it arrives before any `SET_ENCODE`.
//!
//! Which slot, which frame and whether a hold ended is [`Slots`], tested in `pf-driver-proto`;
//! each verb here asks it, then does the GPU pass or the event its answer names. A bypass pool
//! ([`wire::zero_copy`]) hands a session the acquired surface itself ([`DIRECT`]) while no
//! pointer is blended, and the drain worker holds its next acquire until the AU is out.
//!
//! [`Attached`] is the drain worker's cached view of the monitor's pool and session, re-read
//! only when `Monitor::encode_gen` moved, so the steady state takes no lock.

use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pf_driver_proto::encode as wire;
use pf_driver_proto::encode::Framing;
use pf_driver_proto::encode::au::{AuHeader, ENCODER_WEDGED};
use pf_encode_session::drive::{FrameSource, Slot};
use pf_frame::CapturedFrame;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::System::Threading::{ResetEvent, SetEvent, WaitForSingleObject};
use windows62::Win32::Graphics::Direct3D11 as d3d;
use wire::slots::{DIRECT, Place, Slots};

pub use super::cadence::Acquired;
use super::cadence::Cadence;
use super::convert::{Fail, InputKind, Targets, bridge, source_format};
use super::section::EncodeSession;
use super::thread::qpc_now;
use crate::cursor_cell::CursorCell;
use crate::direct_3d_device::Direct3DDevice;
use crate::monitor::Monitor;
use crate::registry::lock;
use crate::worker::OwnedHandle;

/// Three slots: the encoder holds up to two in flight while the drain worker fills one.
pub const SLOTS: usize = 3;

/// How long the bypass drain worker holds its next acquire for the encoder. A wedged encoder
/// must cost the stream, never the head: 100 ms is twelve frame periods at 120 Hz, past any
/// real access unit, and the timeout drops the frame rather than starving the swap-chain.
const BYPASS_HOLD_MS: u32 = 100;

/// What [`Pool::offer`] did with a surface.
pub enum Offer {
    /// In a slot; the frame's source sequence, and the new drop total when the slot was
    /// recycled from under a live consumer.
    Taken(u64, Option<u64>),
    /// The surface is the encoder's input; its source sequence. The caller owes the hold.
    Direct(u64),
    /// Counted; the new drop total.
    Dropped(u64),
    /// Not this pool's surface, nothing counted: what arrived against what the pool was built
    /// for, since a stuck session shows only this line.
    Refused {
        got: (u32, u32, u32),
        want: (u32, u32, u32),
    },
}

struct State {
    slots: Slots,
    targets: Targets,
    /// Bypass only: the acquired surface the encoder reads, set exactly while [`Slots`] holds.
    /// This reference is what keeps DWM's frame alive past the acquire's own.
    held: Option<d3d::ID3D11Texture2D>,
}

impl State {
    /// One GPU pass from `tex` into `slot`, keeping the clean plate when `plate`.
    fn pass(&mut self, tex: &ID3D11Texture2D, slot: usize, plate: bool) -> Result<(), Fail> {
        bridge::<d3d::ID3D11Texture2D>(tex).and_then(|src| self.targets.pass(&src, slot, plate))
    }
}

/// The newest composed frame no pool took: before the monitor's first `SET_ENCODE`, or while
/// its pool no longer fits the surface. The next pool opens on it ([`Pool::first_frame`]), so
/// a session's first IDR needs no compose. One texture, on the drain worker's device.
pub struct Seed(Mutex<Option<Kept>>);

struct Kept {
    epoch: u32,
    size: (u32, u32),
    format: DXGI_FORMAT,
    tex: ID3D11Texture2D,
}

impl Seed {
    pub fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// Copy `tex` over the kept frame; a new device, size or format gets a new texture.
    fn keep(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D) {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `tex` is the live acquired surface; `desc` is a valid local out-param.
        unsafe { tex.GetDesc(&mut desc) };
        let key = (device.epoch(), (desc.Width, desc.Height), desc.Format);
        let mut kept = lock(&self.0);
        if kept
            .as_ref()
            .is_none_or(|k| (k.epoch, k.size, k.format) != key)
        {
            let seed_desc = D3D11_TEXTURE2D_DESC {
                MipLevels: 1,
                ArraySize: 1,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
                ..desc
            };
            let mut t = None;
            // SAFETY: `seed_desc` is a fully-initialized local; `t` a valid out-param.
            let made = unsafe {
                device
                    .device
                    .CreateTexture2D(&seed_desc, None, Some(&mut t))
            };
            *kept = made.ok().and(t).map(|tex| Kept {
                epoch: key.0,
                size: key.1,
                format: key.2,
                tex,
            });
        }
        if let Some(k) = kept.as_ref() {
            // SAFETY: same device, size and format; both textures are alive for the call.
            unsafe { device.device_context.CopyResource(&k.tex, tex) };
        }
    }

    /// The kept frame, handed over, if it was composed on `epoch` at `size` in `format`.
    fn take(&self, epoch: u32, size: (u32, u32), format: DXGI_FORMAT) -> Option<ID3D11Texture2D> {
        let mut kept = lock(&self.0);
        let fits = kept
            .as_ref()
            .is_some_and(|k| (k.epoch, k.size, k.format) == (epoch, size, format));
        fits.then(|| kept.take()).flatten().map(|k| k.tex)
    }
}

impl Drop for Seed {
    /// Free the kept frame with its monitor: the pooled device is idle by then, and D3D11
    /// frees a released texture only at a flush.
    fn drop(&mut self) {
        let Some(kept) = lock(&self.0).take() else {
            return;
        };
        // SAFETY: plain accessors on the live texture's own device.
        let ctx = unsafe { kept.tex.GetDevice().and_then(|d| d.GetImmediateContext()) };
        drop(kept);
        if let Ok(ctx) = ctx {
            // SAFETY: a single call on the pooled device's multithread-protected context.
            unsafe { ctx.Flush() };
        }
    }
}

/// One monitor's pool. See the module docs.
pub struct Pool {
    device_epoch: u32,
    /// The surfaces it takes (`source`) and what its targets encode of them.
    framing: Framing,
    kind: InputKind,
    /// The range the targets' video-engine converter writes.
    full_range: bool,
    source_format: DXGI_FORMAT,
    state: Mutex<State>,
    /// Auto-reset, signalled once per filled slot.
    event: OwnedHandle,
    /// The monitor's source sequence, advanced per frame handed to the pool.
    source_seq: Arc<AtomicU64>,
    /// The monitor's cursor: the blend decision at every pass, the shape at every frame.
    cursor: Arc<CursorCell>,
    bypass: bool,
    /// Bypass only: auto-reset, signalled when the encoder gives the held surface back.
    release_event: Option<OwnedHandle>,
}

impl Pool {
    /// Build the targets for `kind` on `device`: `framing`'s source surfaces in, its encode size
    /// out, converting at `full_range`. `bypass` is [`wire::zero_copy`], which a framing that
    /// scales never is.
    pub fn build(
        device: &Direct3DDevice,
        kind: InputKind,
        framing: Framing,
        full_range: bool,
        source_seq: Arc<AtomicU64>,
        cursor: Arc<CursorCell>,
        bypass: bool,
    ) -> Result<Arc<Self>, Fail> {
        let dev62: d3d::ID3D11Device = bridge(&device.device)?;
        let ctx62: d3d::ID3D11DeviceContext = bridge(&device.device_context)?;
        let targets = Targets::new(kind, &dev62, &ctx62, framing.encode, SLOTS, full_range)?
            .framed(&framing)?;
        let event = OwnedHandle::event(false).ok_or((-2, "event"))?;
        // Without a release event there is nothing to wait on, so the pool copies every frame.
        let release_event = bypass.then(|| OwnedHandle::event(false)).flatten();
        let bypass = release_event.is_some();
        let pool = Arc::new(Self {
            device_epoch: device.epoch(),
            framing,
            kind,
            full_range,
            source_format: DXGI_FORMAT(source_format(kind).0),
            state: Mutex::new(State {
                slots: Slots::new(SLOTS),
                targets,
                held: None,
            }),
            event,
            source_seq,
            cursor,
            bypass,
            release_event,
        });
        // The cursor worker wakes this pool's encode thread when a blended pointer moves over a
        // desktop that composed nothing. `Weak`, so the cell never keeps a retired pool alive.
        pool.cursor.set_waker(Arc::downgrade(&pool));
        Ok(pool)
    }

    /// Whether the encoder reads the acquired surface where it can ([`wire::zero_copy`]).
    pub fn bypass(&self) -> bool {
        self.bypass
    }

    /// Whether a session opening `kind` for `framing` and `full_range` on `device`, with or
    /// without `bypass`, can reuse this pool — and with it the retained slot.
    pub fn matches(
        &self,
        device: &Direct3DDevice,
        kind: InputKind,
        framing: Framing,
        full_range: bool,
        bypass: bool,
    ) -> bool {
        self.device_epoch == device.epoch()
            && self.kind == kind
            && self.framing == framing
            && self.full_range == full_range
            && self.bypass == bypass
    }

    /// Why `tex` on `device` is not a surface this pool was built for, or `None` when it is:
    /// the one test every pass runs first, since a pass over another epoch, size or format is
    /// not valid D3D11 use.
    fn refusal(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D) -> Option<Offer> {
        let (width, height) = self.framing.source;
        if device.epoch() != self.device_epoch {
            return Some(Offer::Refused {
                got: (0, 0, device.epoch()),
                want: (width, height, self.device_epoch),
            });
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `tex` is the live acquired surface; `desc` is a valid local out-param.
        unsafe { tex.GetDesc(&mut desc) };
        let got = (desc.Width, desc.Height, desc.Format.0 as u32);
        let want = (width, height, self.source_format.0 as u32);
        (got != want).then_some(Offer::Refused { got, want })
    }

    /// The monitor's next source sequence number.
    fn next_seq(&self) -> u64 {
        self.source_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The drain worker's pass: the surface goes where [`Slots::offer`] says, then the event.
    /// The state lock is taken blocking: every holder releases it before it calls the encoder,
    /// and the pass serialises on the D3D11 runtime lock anyway. A bypass surface becomes the
    /// encoder's input only with no pointer blended: a blend needs an image of ours.
    pub fn offer(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D, qpc: u64) -> Offer {
        if let Some(refused) = self.refusal(device, tex) {
            return refused;
        }
        let mut st = lock(&self.state);
        // A hidden pointer is no blend: a game under a captured mouse keeps the bypass.
        let may_bypass = || self.bypass && self.cursor.to_blend().is_none();
        let (slot, recycled) = match st.slots.offer(may_bypass) {
            Place::Drop(n) => return Offer::Dropped(n),
            Place::Direct => {
                let Ok(src) = bridge::<d3d::ID3D11Texture2D>(tex) else {
                    return Offer::Dropped(st.slots.drop_one());
                };
                st.held = Some(src);
                // Whatever is signalled now belongs to a surface already given back.
                if let Some(ev) = &self.release_event {
                    // SAFETY: our own auto-reset event, alive as long as `self`.
                    let _ = unsafe { ResetEvent(ev.as_raw()) };
                }
                (DIRECT, None)
            }
            Place::Slot {
                slot,
                recycled_drop,
            } => {
                // Only a blend needs the plate: a flip over a still desktop re-blends the stash
                // `retain` plated, and one over a moving desktop draws on its next compose.
                if st.pass(tex, slot, self.cursor.blends()).is_err() {
                    return Offer::Dropped(st.slots.pass_failed(slot));
                }
                (slot, recycled_drop)
            }
        };
        let seq = self.next_seq();
        st.slots.queue(slot, qpc, seq);
        drop(st);
        self.wake();
        if slot == DIRECT {
            return Offer::Direct(seq);
        }
        Offer::Taken(seq, recycled)
    }

    /// The drain worker's copy of `tex`, the surface the encoder last read in place, once the
    /// desktop went still: the still slot becomes the stash, which is what a keyframe request
    /// or a pointer move re-encodes.
    pub fn retain(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D) {
        if self.refusal(device, tex).is_some() {
            return;
        }
        let mut st = lock(&self.state);
        let Some(slot) = st.slots.still_slot() else {
            return;
        };
        let plate = self.cursor.blends() || self.cursor.armed();
        if st.pass(tex, slot, plate).is_err() {
            return;
        }
        let seq = self.source_seq.load(Ordering::Relaxed);
        st.slots.retained(slot, seq);
        drop(st);
        self.wake();
    }

    /// The frame a new session opens on, as its source sequence: the newest queued one, else
    /// the monitor's seed passed into the still slot. `None` when neither exists.
    pub fn first_frame(&self, seed: &Seed) -> Option<u64> {
        let mut st = lock(&self.state);
        if let Some(seq) = st.slots.newest_seq() {
            return Some(seq);
        }
        let tex = seed.take(self.device_epoch, self.framing.source, self.source_format)?;
        let slot = st.slots.still_slot()?;
        let plate = self.cursor.blends() || self.cursor.armed();
        st.pass(&tex, slot, plate).ok()?;
        let seq = self.next_seq();
        // QPC 0: the drive stamps it with now, not a present that predates the session.
        st.slots.queue(slot, 0, seq);
        Some(seq)
    }

    /// Every slot a departed encoder still held, freed — after a detach, whose encoder may be
    /// mid-read on the GPU; a torn first frame is the price of not waiting for it. A held
    /// surface goes back the same way, so a detach cannot leave the drain worker waiting.
    pub fn reclaim(&self) {
        self.settle(Slots::reclaim);
    }

    /// Run a [`Slots`] verb that may end the hold. If it did, the held surface goes under the
    /// lock, then the drain worker's wait ([`Self::wait_release`]) is released.
    fn settle(&self, verb: impl FnOnce(&mut Slots) -> bool) {
        let mut st = lock(&self.state);
        if !verb(&mut st.slots) {
            return;
        }
        st.held = None;
        drop(st);
        if let Some(ev) = &self.release_event {
            // SAFETY: our own auto-reset event, alive as long as `self`.
            let _ = unsafe { SetEvent(ev.as_raw()) };
        }
    }

    /// The drain worker's hold, after [`Offer::Direct`]: block until the encoder gave the
    /// surface back, so the next acquire never hands DWM one still being read. Past
    /// [`BYPASS_HOLD_MS`] the surface is taken back as a drop (the new total) and the head runs.
    pub fn wait_release(&self) -> Option<u64> {
        let ev = self.release_event.as_ref()?;
        // SAFETY: our own auto-reset event, alive as long as `self`.
        let waited = unsafe { WaitForSingleObject(ev.as_raw(), BYPASS_HOLD_MS) };
        if waited == WAIT_OBJECT_0 {
            return None;
        }
        let mut st = lock(&self.state);
        let dropped = st.slots.hold_timed_out()?;
        st.held = None;
        drop(st);
        dbglog!("[pf-vd] encode: bypass hold timed out ({BYPASS_HOLD_MS} ms) — frame dropped");
        Some(dropped)
    }

    /// Wake the encode thread: a slot filled, or a control op landed in its mailbox.
    pub fn wake(&self) {
        // SAFETY: our own event, alive as long as `self`.
        let _ = unsafe { SetEvent(self.event.as_raw()) };
    }
}

/// The pool as the shared session loop drives it.
impl FrameSource for Pool {
    fn set_live(&self, live: bool) {
        self.settle(|slots| slots.set_live(live));
    }

    fn take_within(&self, budget: usize) -> (Option<Slot>, u64) {
        let mut taken = (None, 0);
        self.settle(|slots| {
            let (frame, shed, ended) = slots.take_within(budget);
            taken = (frame, shed);
            ended
        });
        taken
    }

    /// The last blend is lifted off the stash first: `frame` re-draws a blended pointer, and
    /// one the client took back must not ride the keyframe. Without a blend the slot is clean.
    fn republish(&self) -> Option<Slot> {
        let mut st = lock(&self.state);
        let stash = st.slots.republish()?;
        let restored = st.targets.restore_under(stash.0);
        if self.cursor.blends() {
            restored.ok()?;
        }
        st.slots.hand_out(stash.0);
        Some(stash)
    }

    /// A peek that leaves the mark, so the drive loop can rate-limit to the refresh. A stash
    /// whose AU is still owed is not pending: the loop parks on that AU instead of a timer.
    fn cursor_pending(&self) -> bool {
        self.cursor.is_dirty() && lock(&self.state).slots.republish().is_some()
    }

    /// DWM composes nothing for a cursor move, so the move itself is the frame: the stash's
    /// clean plate is re-blended and the source counter advances, so it reads as progress.
    fn cursor_republish(&self) -> Option<Slot> {
        if !self.cursor.is_dirty() {
            return None;
        }
        let mut st = lock(&self.state);
        let spend_mark = || self.cursor.take_dirty();
        let slot = st.slots.cursor_republish_slot(spend_mark)?;
        // A slot with no clean plate has nothing to re-blend until the next compose, which
        // carries the pointer itself; the mark is spent so the loop does not spin on it.
        st.targets.restore_under(slot).ok()?;
        st.slots.hand_out(slot);
        let seq = self.next_seq();
        // The one per-frame `dbglog!`: an idle desktop under a moving pointer fires this at the
        // cursor poll rate, which would swamp the host's drain ring and `host.log` with it.
        if crate::log::file_log_enabled() {
            dbglog!("[pf-vd] cursor: re-encode on pointer move (no compose) slot={slot} seq={seq}");
        }
        Some((slot, qpc_now(), seq))
    }

    fn cursor_changed(&self) {
        self.cursor.mark_dirty();
    }

    /// The pointer is blended in when the client draws none (the planar pair signals its fence
    /// here). [`DIRECT`] is the held surface itself: no image of ours to draw one on, which is
    /// why [`Pool::offer`] holds only with no pointer to blend.
    fn frame(&self, slot: usize, pts_ns: u64) -> Result<CapturedFrame, Fail> {
        let mut st = lock(&self.state);
        if slot == DIRECT {
            let src = st.held.clone().ok_or((-2, "bypass"))?;
            return Ok(st.targets.direct_frame(&src, pts_ns));
        }
        // A blend draws the pointer where it is now, so it spends the move mark first: a move
        // landing between the two is re-encoded once more, never drawn now and forgotten.
        let _ = self.cursor.take_dirty();
        let cursor = self.cursor.to_blend();
        st.targets.frame(slot, pts_ns, cursor)
    }

    fn release(&self, slot: usize) {
        self.settle(|slots| slots.release(slot));
    }

    fn event(&self) -> std::os::windows::io::RawHandle {
        self.event.as_raw().0
    }

    fn has_full(&self) -> bool {
        lock(&self.state).slots.has_full()
    }

    fn drop_one(&self) -> u64 {
        lock(&self.state).slots.drop_one()
    }
}

/// The drain worker's view of its monitor's pool and session (see the module docs).
pub struct Attached {
    pool: Option<Arc<Pool>>,
    session: Option<Arc<EncodeSession>>,
    /// Where a surface no pool takes waits for the next one.
    seed: Option<Arc<Seed>>,
    seen_gen: u32,
    cadence: Cadence,
    /// One line per worker, not one per seeded surface.
    warned_no_pool: AtomicBool,
}

impl Attached {
    pub fn new() -> Self {
        Self {
            pool: None,
            session: None,
            seed: None,
            seen_gen: u32::MAX,
            cadence: Cadence::new(),
            warned_no_pool: AtomicBool::new(false),
        }
    }

    /// Re-read the monitor's slots when its encode generation moved since the last pass.
    pub fn refresh(&mut self, monitor: &Monitor) {
        let generation = monitor.encode_gen.load(Ordering::Acquire);
        if generation == self.seen_gen {
            return;
        }
        self.seen_gen = generation;
        self.pool = monitor.pool();
        self.session = monitor.encode();
        self.seed = Some(monitor.seed());
        // Which pool this worker now fills, so it can be matched against the one the encode
        // thread drains: a worker filling a pool nobody drains starves the encoder silently.
        let (pool, session) = (self.pool.as_ref().map(Arc::as_ptr), self.session.is_some());
        dbglog!("[pf-vd] pool attach: gen {generation} -> pool {pool:?} session {session}");
    }

    /// The hook, per acquired surface (see [`Pool::offer`]). With no pool, or one that cannot
    /// take the surface, it becomes the seed the next pool opens on; a refusal also marks the
    /// session stale, logged once. `true` means the encoder reads the surface itself, so the
    /// caller owes [`Self::wait_release`] before its next acquire.
    pub fn offer(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D, display_qpc: u64) -> bool {
        let Some(pool) = &self.pool else {
            if let Some(seed) = &self.seed {
                seed.keep(device, tex);
            }
            if !self.warned_no_pool.swap(true, Ordering::AcqRel) {
                dbglog!(
                    "[pf-vd] pool attach: no pool yet - keeping the newest surface as the seed"
                );
            }
            return false;
        };
        match pool.offer(device, tex, display_qpc) {
            Offer::Dropped(n) => {
                self.stamp(offset_of!(AuHeader, dropped_total), Some(n));
                // A pool with no free slot means the encode thread is not releasing them. Rate
                // limited: this fires per frame once the encoder stops draining.
                if n == 1 || n % 512 == 0 {
                    dbglog!(
                        "[pf-vd] pool: no free slot - dropped {n} surfaces (encoder not draining)"
                    );
                }
            }
            Offer::Direct(seq) => {
                self.stamp(offset_of!(AuHeader, source_seq), Some(seq));
                return true;
            }
            Offer::Taken(seq, recycled) => {
                self.stamp(offset_of!(AuHeader, source_seq), Some(seq));
                self.stamp(offset_of!(AuHeader, dropped_total), recycled);
            }
            Offer::Refused {
                got: (gw, gh, gf),
                want: (ww, wh, wf),
            } => {
                if let Some(seed) = &self.seed {
                    seed.keep(device, tex);
                }
                if let Some(s) = &self.session
                    && !s.stale.swap(true, Ordering::AcqRel)
                {
                    // Say so in the header: the host answers a wedge with a fresh SET_ENCODE.
                    s.section
                        .store_u32(offset_of!(AuHeader, encoder_state), ENCODER_WEDGED);
                    let (epoch, pool_epoch) = (device.epoch(), pool.device_epoch);
                    dbglog!(
                        "[pf-vd] encode: pool cannot take the surface - got {gw}x{gh} fmt {gf}, pool wants {ww}x{wh} fmt {wf} (epoch {epoch} vs {pool_epoch}) - session stale until the next SET_ENCODE"
                    );
                }
            }
        }
        false
    }

    /// Store `v` into the session's header at `at`; nothing without a session or a value.
    fn stamp(&self, at: usize, v: Option<u64>) {
        if let (Some(s), Some(v)) = (&self.session, v) {
            s.section.store_u64(at, v);
        }
    }

    /// Wait out the hold ([`Pool::wait_release`]) and time it for the cadence line. Called after
    /// `FinishedProcessingFrame` and before the next acquire, nowhere else.
    pub fn wait_release(&mut self) {
        if let Some(pool) = &self.pool {
            let from = qpc_now();
            let dropped = pool.wait_release();
            self.cadence.note_hold(qpc_now().saturating_sub(from));
            self.stamp(offset_of!(AuHeader, dropped_total), dropped);
        }
    }

    /// Keep `tex` as the pool's still picture ([`Pool::retain`]).
    pub fn retain(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D) {
        if let Some(pool) = &self.pool {
            pool.retain(device, tex);
        }
    }

    /// The drain heartbeat, stamped after `FinishedProcessingFrame` — never inside the window.
    pub fn note_drain(&self) {
        if let Some(s) = &self.session {
            s.section
                .store_u64(offset_of!(AuHeader, drain_heartbeat_qpc), qpc_now());
        }
    }

    /// The heartbeat plus one compose-cadence sample, both after `FinishedProcessingFrame`.
    /// The frame's display stamp is the OS present stamp of the frame just handed back, so the
    /// deltas are DWM's own cadence on this head — the instrument S6 and gate §5-4 are judged
    /// on, stamped identically whichever mode the pool runs.
    pub fn note_frame(&mut self, frame: &Acquired) {
        self.note_drain();
        let fps = self.session.as_ref().map_or(0, |s| s.request.fps);
        let bypass = self.pool.as_ref().is_some_and(|p| p.bypass());
        self.cadence.note(frame, fps, bypass);
    }
}
