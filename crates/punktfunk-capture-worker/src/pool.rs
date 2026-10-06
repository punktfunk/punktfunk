//! The worker's frame pool: three slots in the opened backend's input format, filled by the
//! capture's arrival handler ([`Pool::offer`]) and drained by the session loop through
//! [`FrameSource`]. One pool per encoder open.
//!
//! It is the driver's pool without what only a swap chain needs. Windows Graphics Capture draws
//! the pointer into the frame, so nothing is blended here and no pointer move is a frame of its
//! own; a capture frame is copied before it goes back, so the encoder never reads one in place.

use std::collections::VecDeque;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use pf_driver_proto::encode as wire;
use pf_encode_session::drive::{FrameSource, Slot};
use pf_encode_session::targets::{source_format, InputKind, Targets};
use pf_encode_session::{lock, Fail};
use pf_frame::CapturedFrame;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_TEXTURE2D_DESC,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::System::Threading::{CreateEventW, SetEvent};

/// Three slots: the encoder holds up to two in flight while the arrival handler fills one.
const SLOTS: usize = 3;

struct State {
    targets: Targets,
    free: Vec<usize>,
    /// Queued frames in arrival order.
    full: VecDeque<Slot>,
    /// Handed to the encoder, access unit still owed.
    encoding: Vec<usize>,
    /// The newest frame the encoder took. Its pixels stay in the slot until a later pass
    /// reuses it, which is what a keyframe request re-encodes over a still desktop.
    stash: Option<Slot>,
    /// The session loop is consuming: only then does a recycled slot cost a frame.
    live: bool,
}

/// What [`Pool::offer`] did with a frame.
pub enum Offer {
    /// In a slot: its source sequence, and the new drop total when the slot was recycled from
    /// under the loop.
    Taken(u64, Option<u64>),
    /// Counted; the new drop total.
    Dropped(u64),
    /// Not this pool's size or format; nothing counted.
    Refused,
}

pub struct Pool {
    /// The surface this pool takes: width, height, format.
    want: (u32, u32, DXGI_FORMAT),
    state: Mutex<State>,
    /// Auto-reset, signalled once per queued frame and once per control wake.
    event: OwnedHandle,
    source_seq: AtomicU64,
    dropped: AtomicU64,
}

impl Pool {
    /// Build the targets for `kind` at `size` on the capture's device.
    pub fn build(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        kind: InputKind,
        size: (u32, u32),
    ) -> Result<Self, Fail> {
        let targets = Targets::new(kind, device, context, size, SLOTS)?;
        // SAFETY: plain unnamed auto-reset event creation; the result is checked.
        let event = unsafe { CreateEventW(None, false, false, PCWSTR::null()) }
            .map_err(|_| (-2, "event"))?;
        Ok(Self {
            want: (size.0, size.1, source_format(kind)),
            state: Mutex::new(State {
                targets,
                free: (0..SLOTS).collect(),
                full: VecDeque::new(),
                encoding: Vec::new(),
                stash: None,
                live: false,
            }),
            // SAFETY: the event was just created here and nothing else can close it.
            event: unsafe { OwnedHandle::from_raw_handle(event.0) },
            source_seq: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        })
    }

    fn drop_one(&self) -> u64 {
        self.dropped.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// One GPU pass from a captured frame into a slot, then the event. `qpc` is the frame's
    /// present stamp, `0` for a picture kept from before this pool. With no free slot the
    /// oldest queued frame is recycled ([`wire::offer_slot`]), so the loop reads the newest.
    pub fn offer(&self, tex: &ID3D11Texture2D, qpc: u64) -> Offer {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `tex` is the live captured surface; `desc` is a valid local out-param.
        unsafe { tex.GetDesc(&mut desc) };
        if (desc.Width, desc.Height, desc.Format) != self.want {
            return Offer::Refused;
        }
        let mut st = lock(&self.state);
        let mut recycled = None;
        let (free_top, oldest) = (st.free.last().copied(), st.full.front().map(|f| f.0));
        let i = match wire::offer_slot(free_top, oldest, st.live) {
            Some(wire::OfferSlot::Free(i)) => {
                st.free.pop();
                i
            }
            Some(wire::OfferSlot::Recycle { slot, lost }) => {
                st.full.pop_front();
                if lost {
                    recycled = Some(self.drop_one());
                }
                slot
            }
            None => return Offer::Dropped(self.drop_one()),
        };
        if st.targets.pass(tex, i, false).is_err() {
            st.free.push(i);
            return Offer::Dropped(self.drop_one());
        }
        let seq = self.source_seq.fetch_add(1, Ordering::Relaxed) + 1;
        st.full.push_back((i, qpc, seq));
        drop(st);
        self.wake();
        Offer::Taken(seq, recycled)
    }

    /// Wake the session loop without a frame: a control op is in its mailbox.
    pub fn wake(&self) {
        // SAFETY: our own event, alive as long as `self`.
        unsafe {
            let _ = SetEvent(HANDLE(self.event.as_raw_handle()));
        }
    }
}

impl FrameSource for Pool {
    /// Starting keeps only the newest queued frame, which becomes the first picture.
    fn set_live(&self, live: bool) {
        let mut st = lock(&self.state);
        st.live = live;
        while live && st.full.len() > 1 {
            let (i, ..) = st.full.pop_front().expect("len > 1");
            st.free.push(i);
        }
    }

    fn take_within(&self, budget: usize) -> (Option<Slot>, u64) {
        let mut st = lock(&self.state);
        let mut shed = 0;
        while budget > 0 && st.full.len() > budget {
            let (i, ..) = st.full.pop_front().expect("len > budget");
            st.free.push(i);
            shed += 1;
        }
        let f = (budget > 0).then(|| st.full.pop_front()).flatten();
        if let Some(f) = f {
            st.encoding.push(f.0);
            st.stash = Some(f);
        }
        (f, shed)
    }

    /// The stash again, unless its slot is busy or a newer frame is queued
    /// ([`wire::republish_slot`]). Stamp `0`: the loop stamps the re-encode with now.
    fn republish(&self) -> Option<Slot> {
        let mut st = lock(&self.state);
        let (slot, _, seq) = st.stash?;
        wire::republish_slot(Some(slot), st.full.len(), &st.free)?;
        st.free.retain(|&s| s != slot);
        st.encoding.push(slot);
        Some((slot, 0, seq))
    }

    fn cursor_pending(&self) -> bool {
        false
    }

    fn cursor_republish(&self) -> Option<Slot> {
        None
    }

    fn cursor_changed(&self) {}

    fn frame(&self, slot: usize, pts_ns: u64) -> Result<CapturedFrame, Fail> {
        lock(&self.state).targets.frame(slot, pts_ns, None)
    }

    fn release(&self, slot: usize) {
        let mut st = lock(&self.state);
        st.encoding.retain(|&s| s != slot);
        if !st.free.contains(&slot) {
            st.free.push(slot);
        }
    }

    fn event(&self) -> RawHandle {
        self.event.as_raw_handle()
    }

    fn has_full(&self) -> bool {
        !lock(&self.state).full.is_empty()
    }

    fn drop_one(&self) -> u64 {
        Pool::drop_one(self)
    }
}
