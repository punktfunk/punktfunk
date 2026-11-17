//! The glass clock: the four instruments that stamp when a present reached the screen, and
//! their lifecycle per swapchain. The present-wait waiter, the `VK_EXT_present_timing`
//! engine, the DXGI vblank waiter (Windows, estimates) and the compositor's
//! `wp_presentation` feedback (Linux) are owned by [`GlassClock`] alone.
//!
//! The presenter sends an [`Event`] as a swapchain is made, retires, drains or overflows the
//! engine's result queue; [`lifecycle_step`] answers the [`Act`] the clock performs: arm the
//! engine on the new swapchain, drain the waiter, disarm the engine. The engine's count of
//! stamps asked and not yet reported moves on two threads (the presenter asks, the waiter's
//! poll reads the reports), so it lives in the engine; [`asks_after`] is its rule. The
//! waiter polls only while that count is above zero. [`Clocks::rank`] names the instrument
//! whose stamp the ledger reads.
//!
//! The model is pure, built and tested on every platform; the clock is Linux and Windows.
//! Pinned by the table tests below.

/// Where the engine stands with the live swapchain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Lifecycle {
    /// No swapchain armed. `stamps`: the next one is made for engine stamps and armed.
    Unarmed { stamps: bool },
    /// The engine stamps the presents on `swapchain` (its raw handle).
    Armed { swapchain: u64 },
    /// The swapchain retires; nothing is armed until the waiter has drained it.
    Draining { stamps: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    /// A swapchain was made (its raw handle).
    Created(u64),
    /// The live swapchain goes: a rebuild retires it, or the native lane takes the window.
    Retiring,
    /// No wait names the retired swapchain any more.
    Drained,
    /// A present asked into a full result queue: no stamps from here on.
    QueueFull,
    /// A present asked the engine for a stamp.
    Asked,
    /// The engine reported on a present that asked.
    Reported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Act {
    /// Size the swapchain's result queue and pick its clock.
    Arm(u64),
    /// Forget the stamps asked for.
    Disarm,
    /// Wait out the waiter and drop the unclaimed present.
    Drain,
    Keep,
}

impl Lifecycle {
    /// A swapchain made now asks for engine stamps.
    pub(crate) fn stamps(self) -> bool {
        match self {
            Lifecycle::Unarmed { stamps } | Lifecycle::Draining { stamps } => stamps,
            Lifecycle::Armed { .. } => true,
        }
    }

    pub(crate) fn armed(self) -> bool {
        matches!(self, Lifecycle::Armed { .. })
    }
}

pub(crate) fn lifecycle_step(s: Lifecycle, e: Event) -> (Lifecycle, Act) {
    use Lifecycle::*;
    match e {
        Event::Created(_) if matches!(s, Draining { .. }) => (s, Act::Keep),
        Event::Created(swapchain) if s.stamps() => (Armed { swapchain }, Act::Arm(swapchain)),
        Event::Created(_) => (s, Act::Keep),
        Event::Retiring => (Draining { stamps: s.stamps() }, Act::Drain),
        Event::Drained => (Unarmed { stamps: s.stamps() }, Act::Disarm),
        Event::QueueFull => match s {
            Draining { .. } => (Draining { stamps: false }, Act::Disarm),
            _ => (Unarmed { stamps: false }, Act::Disarm),
        },
        Event::Asked | Event::Reported => (s, Act::Keep),
    }
}

/// The stamps asked and not yet reported, after `e`: an ask counts one, a report takes one
/// back and never goes below zero; a new, retired or overflowed swapchain forgets them.
pub(crate) fn asks_after(asked: u32, e: Event) -> u32 {
    match e {
        Event::Asked => asked.saturating_add(1),
        Event::Reported => asked.saturating_sub(1),
        Event::Created(_) | Event::Retiring | Event::Drained | Event::QueueFull => 0,
    }
}

/// The glass clocks that follow the swapchain's presents.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Clocks {
    /// Present-wait, either generation.
    pub(crate) waiter: bool,
    /// The vblank waiter, whose stamps are estimates.
    pub(crate) vblank: bool,
    /// The last frame went through the native lane, whose feedback stamps it.
    pub(crate) lane: bool,
    /// The engine is armed on the live swapchain.
    pub(crate) engine: bool,
}

impl Clocks {
    /// Where the display stamp comes from: `feedback` (the lane), `timing` (the engine's
    /// own stamps), `wait` (present-wait), `est` (the vblank waiter), `none`.
    pub(crate) fn rank(self) -> &'static str {
        if self.lane {
            "feedback"
        } else if self.engine {
            "timing"
        } else if self.waiter {
            "wait"
        } else if self.vblank {
            "est"
        } else {
            "none"
        }
    }

    /// The display stamps are the vblank waiter's estimate.
    pub(crate) fn estimated(self) -> bool {
        !self.waiter && self.vblank && !self.lane
    }

    /// Present-wait, the vblank waiter or the lane's feedback drives the display stamp.
    pub(crate) fn active(self) -> bool {
        self.waiter || self.vblank || self.lane
    }
}

#[cfg(any(target_os = "linux", windows))]
pub(crate) use clock::GlassClock;

#[cfg(any(target_os = "linux", windows))]
mod clock {
    use super::{lifecycle_step, Act, Clocks, Event, Lifecycle};
    use crate::vk::present_timing::{Job, PresentTimer, PresentedSample};
    use crate::vk::timing_ext::{Engine, TimingInfo};
    use ash::vk::{self, Handle as _};
    use std::sync::{Arc, MutexGuard};

    /// The four instruments, their lifecycle with the live swapchain, and the present ids.
    pub(crate) struct GlassClock {
        /// `VK_EXT_present_timing`: the engine stamps each present itself, and the waiter
        /// reads the stamp in place of its own wake time.
        engine: Option<Arc<Engine>>,
        /// On-glass timing from present-wait, either generation. `None` without it; the run
        /// loop then keeps its submit-time display stamp (or the vblank waiter's estimate).
        waiter: Option<PresentTimer>,
        /// The waiter runs on `VK_KHR_present_wait2`: presents chain `VkPresentId2KHR` and
        /// the swapchain is created with the present-id2/present-wait2 flags.
        id2: bool,
        /// The output's vblank as the glass clock where present-wait is missing (AMD on
        /// Windows). Estimated stamps, `glass=est`.
        #[cfg(windows)]
        vblank: Option<crate::vk::vblank_timing::VblankTimer>,
        /// The compositor's own stamp per swapchain present, beside the driver's.
        #[cfg(target_os = "linux")]
        feedback: Option<crate::wl_feedback::SurfaceFeedback>,
        /// (flipped without a copy, shown) since the last take, from the compositor's stamps.
        #[cfg(target_os = "linux")]
        scanout: (u32, u32),
        /// The compositor has flagged a zero-copy flip at least once. KWin before 6.8 never
        /// sets the bit, so until then a zero means nothing.
        #[cfg(target_os = "linux")]
        scanout_reported: bool,
        /// Compositor stamps by present id, waiting for the driver's sample of that present.
        #[cfg(target_os = "linux")]
        compositor_stamps: std::collections::HashMap<u64, u64>,
        /// Driver samples without an exact stamp, held one pass for their compositor stamp.
        #[cfg(target_os = "linux")]
        held: Vec<PresentedSample>,
        state: Lifecycle,
        /// The last present asked the engine for a stamp.
        asked: bool,
        /// Strictly increasing present id (spec: per swapchain). 0 = none presented with one.
        next_present_id: u64,
        /// Last successful id-carrying present, awaiting [`GlassClock::note_presented`].
        last_presented: Option<(vk::SwapchainKHR, u64)>,
    }

    impl GlassClock {
        pub(crate) fn new(
            engine: Option<Arc<Engine>>,
            waiter: Option<PresentTimer>,
            id2: bool,
        ) -> GlassClock {
            GlassClock {
                state: Lifecycle::Unarmed {
                    stamps: engine.is_some(),
                },
                engine,
                waiter,
                id2,
                #[cfg(windows)]
                vblank: None,
                #[cfg(target_os = "linux")]
                feedback: None,
                #[cfg(target_os = "linux")]
                scanout: (0, 0),
                #[cfg(target_os = "linux")]
                scanout_reported: false,
                #[cfg(target_os = "linux")]
                compositor_stamps: std::collections::HashMap::new(),
                #[cfg(target_os = "linux")]
                held: Vec::new(),
                asked: false,
                next_present_id: 0,
                last_presented: None,
            }
        }

        #[cfg(windows)]
        pub(crate) fn with_vblank(
            self,
            vblank: Option<crate::vk::vblank_timing::VblankTimer>,
        ) -> GlassClock {
            GlassClock { vblank, ..self }
        }

        #[cfg(target_os = "linux")]
        pub(crate) fn with_feedback(
            self,
            feedback: Option<crate::wl_feedback::SurfaceFeedback>,
        ) -> GlassClock {
            GlassClock { feedback, ..self }
        }

        /// Step the lifecycle and perform its act; an `Arm` is [`GlassClock::created`]'s.
        fn step(&mut self, e: Event) -> Act {
            let (next, act) = lifecycle_step(self.state, e);
            self.state = next;
            match act {
                Act::Arm(_) | Act::Keep => {}
                Act::Disarm => {
                    if let Some(t) = &self.engine {
                        t.disarm();
                    }
                }
                Act::Drain => {
                    // `oldSwapchain` is externally synchronised: the driver may retire it under
                    // a parked `vkWaitForPresentKHR`. Bounded by the waiter's 250 ms cap.
                    if let Some(t) = &self.waiter {
                        t.drain();
                    }
                    // `note_presented` is the only producer and shares this thread, so nothing
                    // enqueues for the dying swapchain before its destroy.
                    self.last_presented = None;
                }
            }
            act
        }

        /// A swapchain was made: the engine arms on it. One the engine refuses stays
        /// unarmed; the next swapchain asks again.
        ///
        /// # Safety
        /// `swapchain` is live, and [`GlassClock::retire`] ran since the last one was made.
        pub(crate) unsafe fn created(&mut self, swapchain: vk::SwapchainKHR) {
            let Act::Arm(_) = self.step(Event::Created(swapchain.as_raw())) else {
                return;
            };
            // SAFETY: the caller's contract: the swapchain was just made and the waiter is
            // drained, so this thread is its only user.
            let armed = (self.engine.as_ref()).is_some_and(|t| unsafe { t.arm(swapchain) });
            if !armed {
                self.state = Lifecycle::Unarmed { stamps: true };
            }
        }

        /// The live swapchain goes: wait out the waiter, then forget the stamps asked of it.
        /// The present not yet claimed was the swapchain's.
        pub(crate) fn retire(&mut self) {
            self.step(Event::Retiring);
            self.step(Event::Drained);
        }

        /// A present asked into a full result queue: no stamps from here on.
        pub(crate) fn queue_full(&mut self) {
            self.step(Event::QueueFull);
        }

        /// A swapchain made now asks for engine stamps.
        pub(crate) fn stamps(&self) -> bool {
            self.state.stamps()
        }

        /// Presents chain `VkPresentId2KHR`; swapchains take the present-id2/wait2 flags.
        pub(crate) fn id2(&self) -> bool {
            self.id2
        }

        pub(crate) fn has_waiter(&self) -> bool {
            self.waiter.is_some()
        }

        /// A glass clock follows the swapchain's presents: present-wait, or the vblank waiter.
        pub(crate) fn follows(&self) -> bool {
            let c = self.clocks(false);
            c.waiter || c.vblank
        }

        /// The swapchain's host sync against the waiter, for one acquire or present call.
        pub(crate) fn swapchain_guard(&self) -> Option<MutexGuard<'_, ()>> {
            self.waiter.as_ref().map(|t| t.swapchain_guard())
        }

        /// The id the next present carries.
        pub(crate) fn next_id(&self) -> u64 {
            self.next_present_id + 1
        }

        /// The stamp request for the next present, while the engine is armed and the
        /// swapchain's result queue has room: asking into a full one fails the present.
        pub(crate) fn ask(&self) -> Option<TimingInfo> {
            let engine = self.engine.as_ref().filter(|_| self.state.armed())?;
            engine.may_ask().then(|| engine.request())
        }

        /// A present goes out under [`GlassClock::next_id`]: a following clock takes the id,
        /// and the compositor is asked to stamp the commit. Only a waiter's sample can take
        /// its answer, so without one none is asked.
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
        pub(crate) fn take_id(&mut self, redraw: bool) {
            let follows = self.follows();
            if follows {
                self.next_present_id += 1;
            }
            #[cfg(target_os = "linux")]
            if let Some(fb) = self.feedback.as_mut().filter(|_| follows && !redraw) {
                fb.request(self.next_present_id);
            }
        }

        /// The present was queued. Only then may its id be claimed: a failed present's id
        /// may never signal.
        pub(crate) fn sent(&mut self, swapchain: vk::SwapchainKHR, asked: bool) {
            if self.follows() {
                self.last_presented = Some((swapchain, self.next_present_id));
            }
            self.asked = asked;
            if let Some(t) = self.engine.as_ref().filter(|_| asked) {
                t.note_asked();
            }
        }

        /// Claim the just-queued present for on-glass timing, with its frame's capture and
        /// decode stamps; the submit signals `done_sem` with the present's id when its GPU
        /// work is done. `off_grid`: the present mode does not land on the vblank grid.
        #[cfg_attr(not(windows), allow(unused_variables))]
        pub(crate) fn note_presented(
            &mut self,
            done_sem: vk::Semaphore,
            pts_ns: u64,
            decoded_ns: u64,
            off_grid: bool,
        ) {
            let Some((swapchain, id)) = self.last_presented.take() else {
                return;
            };
            let done = (done_sem != vk::Semaphore::null()).then_some((done_sem, id));
            // The present call has returned, so "now" is its tail.
            let now_ns = pf_client_core::session::now_ns();
            if let Some(t) = &self.waiter {
                t.enqueue(Job {
                    swapchain,
                    present_id: id,
                    done,
                    stamped: std::mem::take(&mut self.asked),
                    pts_ns,
                    decoded_ns,
                    submitted_ns: now_ns,
                });
            }
            #[cfg(windows)]
            if let Some(t) = &self.vblank {
                t.enqueue(done, pts_ns, decoded_ns, now_ns, off_grid);
            }
        }

        /// Undisplayed id-carrying presents in flight (0 without a waiter).
        pub(crate) fn outstanding(&self) -> usize {
            let waited = self.waiter.as_ref().map_or(0, |t| t.outstanding());
            #[cfg(windows)]
            let waited = waited + self.vblank.as_ref().map_or(0, |t| t.outstanding());
            waited
        }

        /// Run-loop wake for present completions: the present-wait waiter's, else the
        /// vblank waiter's.
        pub(crate) fn set_wake(&self, cb: Box<dyn Fn() + Send>) {
            #[cfg(windows)]
            if self.waiter.is_none() {
                if let Some(t) = &self.vblank {
                    t.set_wake(cb);
                }
                return;
            }
            if let Some(t) = &self.waiter {
                t.set_wake(cb);
            }
        }

        /// Presents the engine reported as never shown since the last call.
        pub(crate) fn take_unshown(&self) -> u32 {
            self.waiter.as_ref().map_or(0, |t| t.take_unshown())
        }

        /// The output's own vblank spacing, where a vblank waiter measures it.
        pub(crate) fn measured_refresh_ns(&self) -> Option<u64> {
            #[cfg(windows)]
            {
                let t = self.vblank.as_ref()?;
                Some(t.refresh_ns()).filter(|&p| p > 0)
            }
            #[cfg(not(windows))]
            {
                None
            }
        }

        /// The armed engine's refresh as `(cycle, interval)` in ns, once it has reported one.
        pub(crate) fn engine_refresh(&self) -> Option<(u64, u64)> {
            use std::sync::atomic::Ordering;
            let r = &self.engine.as_ref().filter(|_| self.state.armed())?.refresh;
            let cycle = r.duration_ns.load(Ordering::Relaxed);
            (cycle > 0).then(|| (cycle, r.interval_ns.load(Ordering::Relaxed)))
        }

        /// The vblank waiter follows the window to `monitor`.
        #[cfg(windows)]
        pub(crate) fn retarget(&self, monitor: isize) {
            if let Some(t) = &self.vblank {
                t.retarget(monitor);
            }
        }

        /// The instruments behind the display stamp. `lane`: the last frame went through
        /// the native lane.
        pub(crate) fn clocks(&self, lane: bool) -> Clocks {
            #[cfg(windows)]
            let vblank = self.vblank.is_some();
            #[cfg(not(windows))]
            let vblank = false;
            Clocks {
                waiter: self.waiter.is_some(),
                vblank,
                lane,
                engine: self.state.armed(),
            }
        }

        /// (flipped without a copy, shown) since the last call, by the compositor's own
        /// word. `None` until the compositor has ever flagged a flip: a zero from one that
        /// never says so is not "composited".
        #[cfg(target_os = "linux")]
        pub(crate) fn take_scanout(&mut self) -> Option<(u32, u32)> {
            let out = std::mem::take(&mut self.scanout);
            self.scanout_reported.then_some(out)
        }

        /// The waiters' samples, then `lane`'s, each driver sample joined with the
        /// compositor's stamp for the same present id.
        pub(crate) fn take_samples(
            &mut self,
            mut lane: Vec<PresentedSample>,
        ) -> Vec<PresentedSample> {
            let mut out = self
                .waiter
                .as_ref()
                .map(|t| t.take_samples())
                .unwrap_or_default();
            #[cfg(windows)]
            if let Some(t) = &self.vblank {
                out.extend(t.take_samples());
            }
            #[cfg(target_os = "linux")]
            self.take_compositor_stamps();
            // The driver's half of the join, on every OS: a Windows build without it has no
            // reader of the id at all.
            for s in &out {
                tracing::trace!(
                    target: "pf_glass",
                    id = s.present_id,
                    displayed_ns = s.displayed_ns,
                    submitted_ns = s.submitted_ns,
                    exact = s.exact,
                    "driver stamp"
                );
            }
            out.append(&mut lane);
            #[cfg(target_os = "linux")]
            {
                out = self.join_compositor_stamps(out);
            }
            out
        }

        /// The compositor's answers so far, by present id, and the scanout counts.
        #[cfg(target_os = "linux")]
        fn take_compositor_stamps(&mut self) {
            let Some(fb) = self.feedback.as_mut() else {
                return;
            };
            for s in fb.take() {
                if let Some(t) = s.displayed_ns {
                    self.scanout.1 += 1;
                    if s.zero_copy {
                        self.scanout.0 += 1;
                        self.scanout_reported = true;
                    }
                    self.compositor_stamps.insert(s.present_id, t);
                }
                tracing::trace!(
                    target: "pf_glass",
                    id = s.present_id,
                    displayed_ns = s.displayed_ns.unwrap_or(0),
                    zero_copy = s.zero_copy,
                    refresh_ns = s.refresh_ns,
                    "compositor stamp"
                );
            }
        }

        /// A driver sample without the engine's stamp takes the compositor's for the same
        /// present. One with neither yet waits a pass, and everything behind it waits too so
        /// stamps leave in order; after that it goes out on its wake time. The map drops ids
        /// more than 64 older than the last sample out.
        #[cfg(target_os = "linux")]
        fn join_compositor_stamps(&mut self, fresh: Vec<PresentedSample>) -> Vec<PresentedSample> {
            let mut ready = Vec::with_capacity(self.held.len() + fresh.len());
            for mut s in self.held.drain(..) {
                take_stamp(&mut self.compositor_stamps, &mut s);
                ready.push(s);
            }
            let mut holding = false;
            for mut s in fresh {
                let joined = take_stamp(&mut self.compositor_stamps, &mut s);
                holding |= !joined && s.present_id != 0;
                if holding {
                    self.held.push(s);
                } else {
                    ready.push(s);
                }
            }
            if let Some(last) = ready.last() {
                let floor = last.present_id.saturating_sub(64);
                self.compositor_stamps.retain(|&id, _| id >= floor);
            }
            ready
        }

        /// Join the waiters before the swapchain and `done_sem` they read are destroyed:
        /// the present-wait waiter's in-flight waits are capped at 250 ms.
        pub(crate) fn stop(&mut self) {
            self.waiter.take();
            #[cfg(windows)]
            self.vblank.take();
        }
    }

    /// `true` once the sample carries an exact display time, its own or the compositor's.
    #[cfg(target_os = "linux")]
    fn take_stamp(
        stamps: &mut std::collections::HashMap<u64, u64>,
        s: &mut PresentedSample,
    ) -> bool {
        let stamp = stamps.remove(&s.present_id);
        if s.exact {
            return true;
        }
        match stamp {
            Some(t) => {
                s.displayed_ns = t;
                s.exact = true;
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Act::*;
    use Event::*;
    use Lifecycle::*;

    const OLD: u64 = 0x10;
    const NEW: u64 = 0x20;

    #[test]
    fn the_engine_follows_each_swapchain() {
        let on = Unarmed { stamps: true };
        let off = Unarmed { stamps: false };
        let armed = Armed { swapchain: OLD };
        #[rustfmt::skip]
        let rows: &[(&str, Lifecycle, Event, Lifecycle, Act)] = &[
            ("a created swapchain arms", on, Created(NEW), Armed { swapchain: NEW }, Arm(NEW)),
            ("without the engine a swapchain arms nothing", off, Created(NEW), off, Keep),
            ("retiring drains the waiter", armed, Retiring, Draining { stamps: true }, Drain),
            ("an unarmed swapchain drains too", off, Retiring, Draining { stamps: false }, Drain),
            ("drained disarms", Draining { stamps: true }, Drained, on, Disarm),
            ("a created swapchain waits for the drain", Draining { stamps: true }, Created(NEW), Draining { stamps: true }, Keep),
            ("R2-3: a full queue disarms", armed, QueueFull, off, Disarm),
            ("R2-3: an unarmed swapchain takes no stamps", off, Created(NEW), off, Keep),
            ("R2-3: a full queue mid-drain keeps draining", Draining { stamps: true }, QueueFull, Draining { stamps: false }, Disarm),
            ("an ask keeps the swapchain armed", armed, Asked, armed, Keep),
            ("a report keeps the swapchain armed", armed, Reported, armed, Keep),
            ("a report for an unarmed swapchain is no word", on, Reported, on, Keep),
        ];
        for &(name, from, event, to, act) in rows {
            assert_eq!(lifecycle_step(from, event), (to, act), "{name}");
        }
    }

    /// (events, acts performed, end state) from an armed swapchain.
    type Run = (&'static str, &'static [Event], &'static [Act], Lifecycle);

    #[test]
    fn a_rebuild_drains_then_arms() {
        #[rustfmt::skip]
        let runs: &[Run] = &[
            ("a rebuild drains, then arms the new swapchain",
                &[Retiring, Drained, Created(NEW)], &[Drain, Disarm, Arm(NEW)], Armed { swapchain: NEW }),
            ("the lane takes the window: drained, nothing armed",
                &[Retiring, Drained], &[Drain, Disarm], Unarmed { stamps: true }),
            ("R2-3: after a full queue the rebuilt swapchain asks for no stamps",
                &[QueueFull, Retiring, Drained, Created(NEW)], &[Disarm, Drain, Disarm, Keep], Unarmed { stamps: false }),
        ];
        for &(name, events, acts, end) in runs {
            let mut s = Armed { swapchain: OLD };
            let mut done = Vec::new();
            for &e in events {
                let (next, act) = lifecycle_step(s, e);
                s = next;
                done.push(act);
            }
            assert_eq!((done.as_slice(), s), (acts, end), "{name}");
        }
    }

    #[test]
    fn asks_count_until_reported() {
        #[rustfmt::skip]
        let rows: &[(&str, u32, Event, u32)] = &[
            ("an ask counts", 2, Asked, 3),
            ("a report takes one back", 3, Reported, 2),
            ("R2-3: never below zero", 0, Reported, 0),
            ("a new swapchain starts at zero", 5, Created(NEW), 0),
            ("a full queue forgets the asks", 5, QueueFull, 0),
            ("a drained swapchain forgets the asks", 5, Drained, 0),
        ];
        for &(name, asked, event, want) in rows {
            assert_eq!(asks_after(asked, event), want, "{name}");
        }
    }

    /// (lane shown, engine armed, waiter, vblank) → (source, estimated, timing active)
    type GlassRow = ((bool, bool, bool, bool), (&'static str, bool, bool));

    #[test]
    fn the_display_stamp_source_ranks_lane_engine_wait_vblank() {
        let table: &[GlassRow] = &[
            ((true, true, true, true), ("feedback", false, true)),
            ((true, false, false, true), ("feedback", false, true)),
            ((false, true, true, false), ("timing", false, true)),
            ((false, false, true, true), ("wait", false, true)),
            ((false, false, false, true), ("est", true, true)),
            ((false, false, false, false), ("none", false, false)),
        ];
        for &(row, (source, estimated, active)) in table {
            let (lane, engine, waiter, vblank) = row;
            let c = Clocks {
                waiter,
                vblank,
                lane,
                engine,
            };
            assert_eq!(c.rank(), source, "{row:?}");
            assert_eq!(c.estimated(), estimated, "{row:?}");
            assert_eq!(c.active(), active, "{row:?}");
        }
    }

    /// The waiter polls the engine only while asks are outstanding: after a disarm, a
    /// report the swapchain still owes leaves nothing to poll for.
    #[test]
    fn a_disarmed_engine_is_not_polled() {
        let asked = asks_after(asks_after(4, QueueFull), Reported);
        assert_eq!(asked, 0);
    }
}
