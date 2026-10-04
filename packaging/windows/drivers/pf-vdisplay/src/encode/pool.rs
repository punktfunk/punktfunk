//! The pool: three slots per monitor in the opened backend's input format, filled by the drain
//! worker's fused pass ([`Pool::offer`]) inside the acquire window and drained by the encode
//! thread. The monitor owns it, not the session: between sessions the newest frame keeps
//! landing in it, so a new `SET_ENCODE` on an idle desktop encodes the retained slot as its
//! first IDR and needs no compose. A pool built for another device epoch, size or format is
//! replaced by the next session; nothing rebuilds in place. A surface no pool can take is kept
//! as the monitor's [`Seed`], which the next pool opens on — the compose at swap-chain assign
//! is a monitor's first picture, and it arrives before any `SET_ENCODE`.
//!
//! [`Attached`] is the drain worker's cached view of the monitor's pool and session,
//! re-read only when `Monitor::encode_gen` moved, so the steady state takes no lock. It also
//! carries [`Cadence`], the compose-cadence histogram both modes stamp after `Finished`.
//!
//! A bypass pool ([`wire::zero_copy`]) issues no pass while a session reads and there is no
//! pointer to blend: the encoder reads the acquired surface ([`DIRECT`]) and the drain worker
//! holds its next acquire until the AU is out. A pointer to blend or no session takes a slot.

use std::collections::VecDeque;
use std::mem::offset_of;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pf_driver_proto::encode as wire;
use pf_driver_proto::encode::au::AuHeader;
use pf_frame::CapturedFrame;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::System::Threading::{ResetEvent, SetEvent, WaitForSingleObject};
use windows62::Win32::Graphics::Direct3D11 as d3d;

use super::convert::{Fail, InputKind, Targets, bridge, source_format};
use super::section::EncodeSession;
use super::thread::{qpc_frequency, qpc_now};
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

/// The slot number of a frame that is the acquired surface itself, in no slot.
pub const DIRECT: usize = usize::MAX;

/// What [`Pool::offer`] did with a surface.
pub enum Offer {
    /// In a slot; the frame's source sequence, and the new drop total when the slot was
    /// recycled from under a live consumer.
    Taken(u64, Option<u64>),
    /// The surface is the encoder's input; its source sequence. The caller owes the hold.
    Direct(u64),
    /// Counted; the new drop total.
    Dropped(u64),
    /// Not this pool's surface — nothing counted. Carries what arrived against what the pool
    /// was built for, because the three reasons are indistinguishable from the outside and a
    /// stuck session shows only this line.
    Refused {
        got: (u32, u32, u32),
        want: (u32, u32, u32),
    },
}

struct State {
    targets: Targets,
    free: Vec<usize>,
    /// `(slot, PresentDisplayQPCTime, source_seq)` in acquire order.
    full: VecDeque<(usize, u64, u64)>,
    /// Handed to the encoder, AU still owed.
    encoding: Vec<usize>,
    /// `(slot, qpc, seq)` of the newest frame the encoder took. Its pixels survive in the slot
    /// until a later pass reuses it, which is what lets a keyframe request re-encode the last
    /// picture when the desktop composed nothing since.
    stash: Option<(usize, u64, u64)>,
    /// An encode thread is consuming. A recycle costs a frame only while this is set; without
    /// a consumer the retained image just becomes the current desktop.
    live: bool,
    /// Bypass only: the acquired surface the encoder reads. One at a time — the drain worker
    /// waits for its release before it acquires again — and the reference here is what keeps
    /// DWM's frame alive past the acquire's own.
    held: Option<d3d::ID3D11Texture2D>,
}

/// Give the held surface up, with the queue entry that named it: a [`DIRECT`] entry is only
/// ever queued while its surface is held, so no slot pass can meet one.
fn unhold(st: &mut State) -> bool {
    st.full.retain(|f| f.0 != DIRECT);
    st.held.take().is_some()
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
    width: u32,
    height: u32,
    kind: InputKind,
    source_format: DXGI_FORMAT,
    state: Mutex<State>,
    /// Auto-reset, signalled once per filled slot.
    event: OwnedHandle,
    /// The monitor's source sequence, advanced per frame handed to the pool.
    source_seq: Arc<AtomicU64>,
    /// Frames dropped at the pool or skipped by the encode thread for a full slot table.
    dropped: AtomicU64,
    /// The monitor's cursor: read at every pass for the blend decision, at every frame for
    /// the shape.
    cursor: Arc<CursorCell>,
    /// The encoder reads the acquired surface where it can (see the module docs).
    bypass: bool,
    /// Bypass only: auto-reset, signalled when the encoder gives the held surface back.
    release_event: Option<OwnedHandle>,
}

impl Pool {
    /// Build the targets for `kind` at `size` on `device`; `bypass` is [`wire::zero_copy`].
    pub fn build(
        device: &Direct3DDevice,
        kind: InputKind,
        size: (u32, u32),
        source_seq: Arc<AtomicU64>,
        cursor: Arc<CursorCell>,
        bypass: bool,
    ) -> Result<Arc<Self>, Fail> {
        let dev62: d3d::ID3D11Device = bridge(&device.device)?;
        let ctx62: d3d::ID3D11DeviceContext = bridge(&device.device_context)?;
        let targets = Targets::new(kind, &dev62, &ctx62, size, SLOTS)?;
        let event = OwnedHandle::event(false).ok_or((-2, "event"))?;
        // Without a release event there is nothing to wait on, so the pool copies every frame.
        let release_event = bypass.then(|| OwnedHandle::event(false)).flatten();
        let bypass = release_event.is_some();
        let pool = Arc::new(Self {
            device_epoch: device.epoch(),
            width: size.0,
            height: size.1,
            kind,
            source_format: DXGI_FORMAT(source_format(kind).0),
            state: Mutex::new(State {
                targets,
                free: (0..SLOTS).collect(),
                full: VecDeque::new(),
                encoding: Vec::new(),
                stash: None,
                live: false,
                held: None,
            }),
            event,
            source_seq,
            dropped: AtomicU64::new(0),
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

    /// Whether a session opening `kind` at `size` on `device`, with or without `bypass`, can
    /// reuse this pool — and with it the retained slot.
    pub fn matches(
        &self,
        device: &Direct3DDevice,
        kind: InputKind,
        size: (u32, u32),
        bypass: bool,
    ) -> bool {
        self.device_epoch == device.epoch()
            && self.kind == kind
            && (self.width, self.height) == size
            && self.bypass == bypass
    }

    /// The filled-slot event, for the encode thread's wait.
    pub fn event(&self) -> HANDLE {
        self.event.as_raw()
    }

    /// The drain worker's pass: one GPU pass from the acquired surface into a slot, then the
    /// event. The state lock is taken blocking — every holder releases it before it calls the
    /// encoder, so the wait is one pass on the shared immediate context, which this pass would
    /// serialise on through the D3D11 runtime lock anyway. With no free slot the oldest queued
    /// frame is recycled ([`wire::offer_slot`]), so the encoder always reads the freshest
    /// composed picture; that recycle is the only counted drop. While the cursor is armed the
    /// converter kinds keep the RGB copy for a later blend. A bypass pool issues no pass
    /// while a session reads and there is no pointer to blend: the surface itself becomes
    /// the encoder's input and only one may be out at a time.
    pub fn offer(&self, device: &Direct3DDevice, tex: &ID3D11Texture2D, qpc: u64) -> Offer {
        let want = (self.width, self.height, self.source_format.0 as u32);
        if device.epoch() != self.device_epoch {
            return Offer::Refused {
                got: (0, 0, device.epoch()),
                want: (self.width, self.height, self.device_epoch),
            };
        }
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `tex` is the live acquired surface; `desc` is a valid local out-param.
        unsafe { tex.GetDesc(&mut desc) };
        let got = (desc.Width, desc.Height, desc.Format.0 as u32);
        if got != want {
            return Offer::Refused { got, want };
        }
        let mut st = lock(&self.state);
        let mut recycled = None;
        // A surface still held means its access unit is not out; this frame is dropped
        // rather than queued behind it, so the hold is never nested.
        if st.held.is_some() {
            return self.drop_one();
        }
        // Between sessions nobody gives a surface back, and a blend needs an image of ours.
        // A hidden pointer is no blend: a game under a captured mouse stays on this path.
        let i = if self.bypass && st.live && self.cursor.to_blend().is_none() {
            match bridge::<d3d::ID3D11Texture2D>(tex) {
                Ok(src) => st.held = Some(src),
                Err(_) => return self.drop_one(),
            }
            // Whatever is signalled now belongs to a surface already given back.
            if let Some(ev) = &self.release_event {
                // SAFETY: our own auto-reset event, alive as long as `self`.
                unsafe {
                    let _ = ResetEvent(ev.as_raw());
                }
            }
            DIRECT
        } else {
            // Take the slot out of exactly one list: a slot popped and then not used is in
            // none of the three, and nothing would put it back.
            let free_top = st.free.last().copied();
            let oldest = st.full.front().map(|f| f.0);
            let i = match wire::offer_slot(free_top, oldest, st.live) {
                Some(wire::OfferSlot::Free(i)) => {
                    st.free.pop();
                    i
                }
                Some(wire::OfferSlot::Recycle { slot, lost }) => {
                    st.full.pop_front();
                    if lost {
                        recycled = Some(self.dropped.fetch_add(1, Ordering::Relaxed) + 1);
                    }
                    slot
                }
                None => return self.drop_one(),
            };
            // Keep a clean plate whenever the blend could turn on: a flip over a still desktop
            // has no other picture to draw the pointer onto.
            let plate = self.cursor.blends() || self.cursor.armed();
            let passed =
                bridge::<d3d::ID3D11Texture2D>(tex).and_then(|src| st.targets.pass(&src, i, plate));
            if passed.is_err() {
                st.free.push(i);
                return self.drop_one();
            }
            i
        };
        let seq = self.source_seq.fetch_add(1, Ordering::Relaxed) + 1;
        st.full.push_back((i, qpc, seq));
        drop(st);
        // SAFETY: our own event, alive as long as `self`.
        unsafe {
            let _ = SetEvent(self.event.as_raw());
        }
        if i == DIRECT {
            return Offer::Direct(seq);
        }
        Offer::Taken(seq, recycled)
    }

    /// The drain worker's copy of `tex`, the surface the encoder last read in place, once the
    /// desktop went still: the stash becomes a slot, which is what a keyframe request or a
    /// pointer move re-encodes. Skipped while a composed frame is queued — that one is newer.
    pub fn retain(&self, tex: &ID3D11Texture2D) {
        let mut st = lock(&self.state);
        let (true, Some(&i)) = (st.full.is_empty(), st.free.last()) else {
            return;
        };
        let plate = self.cursor.blends() || self.cursor.armed();
        let passed =
            bridge::<d3d::ID3D11Texture2D>(tex).and_then(|src| st.targets.pass(&src, i, plate));
        if passed.is_err() {
            return;
        }
        st.stash = Some((i, 0, self.source_seq.load(Ordering::Relaxed)));
        drop(st);
        self.wake();
    }

    /// The frame a new session opens on, as its source sequence: the newest queued one, else
    /// the monitor's seed passed into a free slot. `None` when neither exists.
    pub fn first_frame(&self, seed: &Seed) -> Option<u64> {
        let mut st = lock(&self.state);
        if let Some(&(.., seq)) = st.full.back() {
            return Some(seq);
        }
        let tex = seed.take(
            self.device_epoch,
            (self.width, self.height),
            self.source_format,
        )?;
        let i = *st.free.last()?;
        let plate = self.cursor.blends() || self.cursor.armed();
        bridge::<d3d::ID3D11Texture2D>(&tex)
            .and_then(|src| st.targets.pass(&src, i, plate))
            .ok()?;
        st.free.pop();
        let seq = self.source_seq.fetch_add(1, Ordering::Relaxed) + 1;
        // QPC 0: the drive stamps it with now, not a present that predates the session.
        st.full.push_back((i, 0, seq));
        Some(seq)
    }

    /// One more frame dropped; the new total, for the header.
    pub fn drop_one(&self) -> Offer {
        Offer::Dropped(self.dropped.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// The encode thread starts (`true`: only the newest full slot is kept, the stash) or
    /// stops (`false`: the pool goes back to recycling, and a surface nobody took is given up).
    pub fn set_live(&self, live: bool) {
        let mut st = lock(&self.state);
        st.live = live;
        if live {
            while st.full.len() > 1 {
                let (i, ..) = st.full.pop_front().expect("len > 1");
                st.free.push(i);
            }
            return;
        }
        let handed_back = unhold(&mut st);
        drop(st);
        if handed_back {
            self.signal_release();
        }
    }

    /// Whether a composed frame is queued.
    pub fn has_full(&self) -> bool {
        !lock(&self.state).full.is_empty()
    }

    /// The oldest full slot within `budget` frames, now the encoder's, remembered as the stash for
    /// [`Self::republish`], plus how many queued frames were shed. Frames past the budget go
    /// oldest first, so the encoder reads the newest. With no budget the slots wait for credit,
    /// but a held bypass surface is shed: the drain worker must not wait on credit.
    pub fn take_within(&self, budget: usize) -> (Option<(usize, u64, u64)>, u64) {
        let mut st = lock(&self.state);
        let mut shed = 0;
        let mut handed_back = false;
        if budget == 0 {
            if st.full.iter().any(|f| f.0 == DIRECT) {
                handed_back = unhold(&mut st);
                shed = 1;
            }
        } else {
            while st.full.len() > budget {
                let (i, ..) = st.full.pop_front().expect("len > budget");
                if i == DIRECT {
                    handed_back |= st.held.take().is_some();
                } else {
                    st.free.push(i);
                }
                shed += 1;
            }
        }
        let f = (budget > 0).then(|| st.full.pop_front()).flatten();
        if let Some(f) = f {
            st.encoding.push(f.0);
            st.stash = Some(f);
        }
        drop(st);
        if handed_back {
            self.signal_release();
        }
        (f, shed)
    }

    /// The stash again, for a client that asked for a keyframe while the desktop composed
    /// nothing. DWM presents only what something dirties, so a session whose client draws its
    /// own pointer can go quiet with the client holding no picture at all; its keyframe request
    /// is then the only signal that anyone needs one, and there is no frame for the ordinary
    /// path to mark as an IDR.
    ///
    /// Yields nothing unless the slot is idle and no composed frame is queued
    /// ([`wire::republish_slot`]), and moves it out of `free` so no drain pass can overwrite
    /// the pixels the encoder is about to read. The last blend is lifted off the slot first:
    /// `frame` re-draws a blended pointer, and one the client took back must not ride the
    /// keyframe. Only a blend needs the plate; without one a slot with none is clean already.
    /// QPC 0: the drive stamps the re-encode with now, not the stale present time.
    pub fn republish(&self) -> Option<(usize, u64, u64)> {
        let mut st = lock(&self.state);
        let (slot, _, seq) = st.stash?;
        let queued = st.full.len();
        wire::republish_slot(Some(slot), queued, &st.free)?;
        let restored = st.targets.restore_under(slot);
        if self.cursor.blends() {
            restored.ok()?;
        }
        st.free.retain(|&s| s != slot);
        st.encoding.push(slot);
        Some((slot, 0, seq))
    }

    /// The pointer or its render model changed since the encode thread last looked AND the
    /// stash can be re-encoded now — a peek that leaves the mark, so the drive loop can rate-limit to the
    /// refresh. A stash whose access unit is still owed is not pending: the loop parks on that
    /// AU instead of spinning on a timer, and the move is picked up once the slot comes back.
    pub fn cursor_pending(&self) -> bool {
        if !self.cursor.is_dirty() {
            return false;
        }
        let st = lock(&self.state);
        st.stash.is_some_and(|(slot, ..)| {
            wire::republish_slot(Some(slot), st.full.len(), &st.free).is_some()
        })
    }

    /// Re-encode the stash with the pointer where it is NOW, or without it once the client draws
    /// it. DWM excludes the hardware cursor and composes only on damage, so a cursor move over a
    /// still desktop yields no frame; this makes the move itself the frame. The clean plate is
    /// re-blended (never the last blend again), the
    /// slot is taken like [`Self::republish`], and the source counter advances so the move reads
    /// as real progress. `None` unless a move is pending, a plate exists, the slot is idle and no
    /// composed frame is queued — a queued frame carries the current pointer itself.
    ///
    /// The mark is consumed only once the slot is taken: consumed on a busy slot, the last move
    /// of a gesture was lost and the client's pointer rested one step behind.
    pub fn cursor_republish(&self) -> Option<(usize, u64, u64)> {
        if !self.cursor.is_dirty() {
            return None;
        }
        let mut st = lock(&self.state);
        let (slot, ..) = st.stash?;
        wire::republish_slot(Some(slot), st.full.len(), &st.free)?;
        if !self.cursor.take_dirty() {
            return None;
        }
        // A slot with no clean plate has nothing to re-blend until the next compose, which
        // carries the pointer itself; the mark is spent so the loop does not spin on it.
        st.targets.restore_under(slot).ok()?;
        st.free.retain(|&s| s != slot);
        st.encoding.push(slot);
        let seq = self.source_seq.fetch_add(1, Ordering::Relaxed) + 1;
        // The one per-frame `dbglog!`: an idle desktop under a moving pointer fires this at the
        // cursor poll rate, which would swamp the host's drain ring and `host.log` with it.
        if crate::log::file_log_enabled() {
            dbglog!("[pf-vd] cursor: re-encode on pointer move (no compose) slot={slot} seq={seq}");
        }
        Some((slot, qpc_now(), seq))
    }

    /// A cursor-only re-encode was dropped: mark the pointer changed so it is tried again.
    pub fn cursor_changed(&self) {
        self.cursor.mark_dirty();
    }

    /// Hand a slot back, whether its AU was published or it was skipped. For [`DIRECT`] this
    /// gives the acquired surface up, which releases the drain worker's hold — a surface the
    /// drain worker already took back on timeout signals nothing.
    pub fn release(&self, slot: usize) {
        let mut st = lock(&self.state);
        st.encoding.retain(|&s| s != slot);
        let handed_back = slot == DIRECT && unhold(&mut st);
        if slot != DIRECT && !st.free.contains(&slot) {
            st.free.push(slot);
        }
        drop(st);
        if handed_back {
            self.signal_release();
        }
    }

    /// Every slot a departed encoder still held, freed — after a detach, whose encoder may be
    /// mid-read on the GPU; a torn first frame is the price of not waiting for it. The bypass
    /// surface goes back the same way, so a detach cannot leave the drain worker waiting.
    pub fn reclaim(&self) {
        let mut st = lock(&self.state);
        let slots = core::mem::take(&mut st.encoding);
        for slot in slots {
            if slot != DIRECT && !st.free.contains(&slot) {
                st.free.push(slot);
            }
        }
        let handed_back = unhold(&mut st);
        drop(st);
        if handed_back {
            self.signal_release();
        }
    }

    /// Release the drain worker's hold (see [`Self::wait_release`]).
    fn signal_release(&self) {
        if let Some(ev) = &self.release_event {
            // SAFETY: our own auto-reset event, alive as long as `self`.
            unsafe {
                let _ = SetEvent(ev.as_raw());
            }
        }
    }

    /// The drain worker's hold, after [`Offer::Direct`]: block until the encoder gave the
    /// acquired surface back, so the next acquire never hands DWM a surface still being read.
    /// Bounded by [`BYPASS_HOLD_MS`]; a timeout takes the surface back, counts a drop and lets
    /// the head run on — the encoder then finds nothing to wrap and skips that frame.
    pub fn wait_release(&self) {
        let Some(ev) = &self.release_event else {
            return;
        };
        // SAFETY: our own auto-reset event, alive as long as `self`.
        let waited = unsafe { WaitForSingleObject(ev.as_raw(), BYPASS_HOLD_MS) };
        if waited == WAIT_OBJECT_0 {
            return;
        }
        let taken = unhold(&mut lock(&self.state));
        if taken {
            self.drop_one();
            dbglog!("[pf-vd] encode: bypass hold timed out ({BYPASS_HOLD_MS} ms) — frame dropped");
        }
    }

    /// Wake the encode thread without a frame — a control op landed in its mailbox.
    pub fn wake(&self) {
        // SAFETY: our own event, alive as long as `self`.
        unsafe {
            let _ = SetEvent(self.event.as_raw());
        }
    }

    /// Wrap slot `slot` as the frame `submit` takes, the pointer blended in when the client
    /// draws none (the planar pair signals its fence here). [`DIRECT`] is the held surface
    /// itself and carries no pointer: there is no driver-owned image to draw one on, so
    /// [`Self::offer`] takes that path only with no pointer to blend.
    ///
    /// A blend draws the pointer where it is now, so it spends the move mark: left set, the
    /// loop re-encoded the same picture once more right after a composed frame.
    pub fn frame(&self, slot: usize, pts_ns: u64) -> Result<CapturedFrame, Fail> {
        let mut st = lock(&self.state);
        if slot == DIRECT {
            let src = st.held.clone().ok_or((-2, "bypass"))?;
            return Ok(st.targets.direct_frame(&src, pts_ns));
        }
        // Taken before the image is read: a move landing between the two is drawn now and
        // re-encoded once more, never drawn now and forgotten.
        let _ = self.cursor.take_dirty();
        let cursor = self.cursor.to_blend();
        st.targets.frame(slot, pts_ns, cursor)
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
        dbglog!(
            "[pf-vd] pool attach: gen {} -> pool {:?} session {}",
            generation,
            self.pool.as_ref().map(Arc::as_ptr),
            self.session.is_some()
        );
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
        let session = self.session.as_deref();
        let mut held = false;
        match pool.offer(device, tex, display_qpc) {
            Offer::Dropped(n) => {
                if let Some(s) = session {
                    s.section.store_u64(offset_of!(AuHeader, dropped_total), n);
                }
                // A pool with no free slot means the encode thread is not releasing them. Rate
                // limited: this fires per frame once the encoder stops draining.
                if n == 1 || n % 512 == 0 {
                    dbglog!(
                        "[pf-vd] pool: no free slot - dropped {n} surfaces (encoder not draining)"
                    );
                }
            }
            Offer::Direct(seq) => {
                held = true;
                if let Some(s) = session {
                    s.section.store_u64(offset_of!(AuHeader, source_seq), seq);
                }
            }
            Offer::Taken(seq, recycled) => {
                if let Some(s) = session {
                    s.section.store_u64(offset_of!(AuHeader, source_seq), seq);
                    if let Some(n) = recycled {
                        s.section.store_u64(offset_of!(AuHeader, dropped_total), n);
                    }
                }
            }
            Offer::Refused { got, want } => {
                if let Some(seed) = &self.seed {
                    seed.keep(device, tex);
                }
                if let Some(s) = session
                    && !s.stale.swap(true, Ordering::AcqRel)
                {
                    // Say so in the header too. Only a fresh SET_ENCODE rebuilds the pool, and
                    // the host cannot ask for one it never learns it needs: every surface is
                    // dropped here while its telemetry still reads a healthy open encoder, so the
                    // stream goes black until an unrelated timeout happens to rebuild it.
                    s.section.store_u32(
                        offset_of!(AuHeader, encoder_state),
                        pf_driver_proto::encode::au::ENCODER_WEDGED,
                    );
                    dbglog!(
                        "[pf-vd] encode: pool cannot take the surface - got {}x{} fmt {}, pool wants {}x{} fmt {} (epoch {} vs {}) - session stale until the next SET_ENCODE",
                        got.0,
                        got.1,
                        got.2,
                        want.0,
                        want.1,
                        want.2,
                        device.epoch(),
                        pool.device_epoch
                    );
                }
            }
        }
        held
    }

    /// Wait out the hold ([`Pool::wait_release`]) and time it for the cadence line. Called after
    /// `FinishedProcessingFrame` and before the next acquire, nowhere else.
    pub fn wait_release(&mut self) {
        if let Some(pool) = &self.pool {
            let from = qpc_now();
            pool.wait_release();
            self.cadence.note_hold(qpc_now().saturating_sub(from));
        }
    }

    /// Keep `tex` as the pool's still picture ([`Pool::retain`]).
    pub fn retain(&self, tex: &ID3D11Texture2D) {
        if let Some(pool) = &self.pool {
            pool.retain(tex);
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

/// One acquired frame, as the drain worker saw it arrive.
pub struct Acquired {
    /// `PresentDisplayQPCTime`: the OS's display time for the frame.
    pub display_qpc: u64,
    /// When the acquire returned it.
    pub at_qpc: u64,
    /// `PresentationFrameNumber`.
    pub number: u32,
    /// It was already waiting: no empty acquire came before it.
    pub queued: bool,
}

/// `PresentDisplayQPCTime` deltas in eighth frame periods: 24 buckets, the last one everything
/// at or over 2.875 periods, plus the run's worst gap. Reported every [`Cadence::REPORT_MS`] to
/// the driver log. This is the only instrument for gate §5-4 ("deltas never exceed 2 frame
/// periods"), so it is unconditional — a spike feature must not be what a shipping gate reads.
/// Bucket 8 opens at exactly one period and bucket 16 at two, so both bars are counts, not
/// interpolations.
///
/// After the histogram come the acquire's own counts, which tell a late driver from a head that
/// composed nothing: `missed` frame numbers never seen, `same` frames whose number did not
/// move, `queued` frames already waiting when the acquire came back, `lead_us` how far ahead of
/// its display time a frame arrived (negative is after it), `hold_us` the bypass wait.
struct Cadence {
    hz: u64,
    last: u64,
    last_number: Option<u32>,
    since: u64,
    n: u64,
    max_us: u64,
    buckets: [u32; Self::BUCKETS],
    missed: u64,
    same: u32,
    queued: u32,
    leads: u32,
    lead_sum_us: i64,
    lead_min_us: i64,
    holds: u32,
    hold_sum_us: u64,
    hold_max_us: u64,
}

impl Cadence {
    const REPORT_MS: u64 = 10_000;
    const BUCKETS: usize = 24;

    fn new() -> Self {
        Self {
            hz: qpc_frequency(),
            last: 0,
            last_number: None,
            since: 0,
            n: 0,
            max_us: 0,
            buckets: [0; Self::BUCKETS],
            missed: 0,
            same: 0,
            queued: 0,
            leads: 0,
            lead_sum_us: 0,
            lead_min_us: i64::MAX,
            holds: 0,
            hold_sum_us: 0,
            hold_max_us: 0,
        }
    }

    /// One bypass hold, in QPC ticks.
    fn note_hold(&mut self, ticks: u64) {
        let us = ticks * 1_000_000 / self.hz;
        self.holds += 1;
        self.hold_sum_us += us;
        self.hold_max_us = self.hold_max_us.max(us);
    }

    /// The acquire's own counts. Every frame counts, whatever its stamp.
    fn note_acquire(&mut self, f: &Acquired) {
        if let Some(last) = self.last_number.replace(f.number) {
            match f.number.wrapping_sub(last) {
                0 => self.same += 1,
                1 => {}
                // A step backwards is a counter that started over, not a gap.
                gap if gap < u32::MAX / 2 => self.missed += u64::from(gap - 1),
                _ => {}
            }
        }
        self.queued += u32::from(f.queued);
        if f.display_qpc != 0 {
            let lead_us = (f.display_qpc as i64 - f.at_qpc as i64) * 1_000_000 / self.hz as i64;
            self.leads += 1;
            self.lead_sum_us += lead_us;
            self.lead_min_us = self.lead_min_us.min(lead_us);
        }
    }

    /// One acquired frame. A zero present stamp, a backwards one or an unknown refresh only
    /// re-anchors: the run's buckets stay in one unit.
    fn note(&mut self, f: &Acquired, fps: u32, bypass: bool) {
        self.note_acquire(f);
        let qpc = f.display_qpc;
        let period_us = 1_000_000 / u64::from(fps.max(1));
        let (last, since) = (self.last, self.since);
        self.last = qpc;
        if qpc == 0 || last == 0 || qpc <= last {
            self.since = qpc;
            return;
        }
        if since == 0 {
            self.since = last;
        }
        let delta_us = (qpc - last) * 1_000_000 / self.hz;
        self.n += 1;
        self.max_us = self.max_us.max(delta_us);
        let bucket = (delta_us * 8 / period_us.max(1)).min(Self::BUCKETS as u64 - 1) as usize;
        self.buckets[bucket] += 1;
        let window_ms = (qpc - self.since) * 1_000 / self.hz;
        if window_ms < Self::REPORT_MS {
            return;
        }
        let h = self.buckets.map(|b| b.to_string()).join("/");
        dbglog!(
            "[pf-vd] cadence: mode={} fps={fps} win_ms={window_ms} n={} max_us={} h={h} missed={} same={} queued={} lead_us mean={} min={} hold_us mean={} max={}",
            if bypass { "bypass" } else { "pool" },
            self.n,
            self.max_us,
            self.missed,
            self.same,
            self.queued,
            self.lead_sum_us / i64::from(self.leads.max(1)),
            if self.leads == 0 { 0 } else { self.lead_min_us },
            self.hold_sum_us / u64::from(self.holds.max(1)),
            self.hold_max_us
        );
        // The window's counts start over; the two anchors carry across it.
        *self = Self {
            last: self.last,
            last_number: self.last_number,
            since: qpc,
            ..Self::new()
        };
    }
}
