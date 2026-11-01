//! Client→host input thread and the per-pad virtual-gamepad router.
//!
//! `serve_session` spawns [`input_thread`] and feeds it [`ClientInput`]. Pointer and
//! keyboard go through [`InputRoute`]; gamepad frames go through [`Pads`], which fans
//! mixed controller kinds to uinput/UHID (Linux) or XUSB/UMDF (Windows). Rumble and
//! HID-output pump on the same thread.
//!
//! Pin `PUNKTFUNK_RUMBLE_TTL_MS`, `PUNKTFUNK_XBOX_BACKEND=xusb`.
//! Evidence: `design/gamescope-multiuser.md`, `design/rumble-envelope-plan.md`,
//! `design/trigger-rumble-plane.md`, `design/pen-tablet-input.md`.

use super::*;

/// Pointer/keyboard injector target, re-pointable without restarting [`input_thread`].
/// Isolated gamescope sessions pin their own [`crate::inject::InjectorService`]; everyone
/// else shares the host-lifetime one (`design/gamescope-multiuser.md`). A `Mutex` per event,
/// not a second channel: input is a few kHz and the clone is cheap.
#[derive(Clone)]
pub(crate) struct InputRoute(std::sync::Arc<std::sync::Mutex<RouteTargets>>);

/// The injector input goes to, and the one a switch moved it off until the input thread has
/// let go of what was pressed there.
struct RouteTargets {
    live: std::sync::mpsc::Sender<InputEvent>,
    retired: Option<std::sync::mpsc::Sender<InputEvent>>,
}

impl InputRoute {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<InputEvent>) -> InputRoute {
        InputRoute(std::sync::Arc::new(std::sync::Mutex::new(RouteTargets {
            live: tx,
            retired: None,
        })))
    }

    /// Send error means the injector is gone. Input is lossy — drop it.
    pub(crate) fn send(
        &self,
        ev: InputEvent,
    ) -> Result<(), std::sync::mpsc::SendError<InputEvent>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .live
            .send(ev)
    }

    /// Mid-stream compositor switch. Does not restart [`input_thread`]; the old target is kept
    /// for [`Self::take_retired`], so what is held there is released there.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set(&self, tx: std::sync::mpsc::Sender<InputEvent>) {
        let mut t = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::mem::replace(&mut t.live, tx);
        t.retired = Some(old);
    }

    /// The target a switch moved input off, once.
    fn take_retired(&self) -> Option<std::sync::mpsc::Sender<InputEvent>> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retired
            .take()
    }
}

/// What this session holds down on the injector, and the route its ups go out on. Dropping it
/// sends them, so a thread that unwinds leaves no key or button down on the host.
struct Held {
    held: crate::inject::held::HeldInput,
    route: InputRoute,
}

impl Held {
    /// A compositor switch moved the route: let go, on the old target, of what was pressed
    /// there. Its matching ups would otherwise reach the new one and leave the old one latched.
    fn follow_route(&mut self) {
        if let Some(old) = self.route.take_retired() {
            for ev in self.held.release() {
                let _ = old.send(ev);
            }
        }
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        for ev in self.held.release() {
            let _ = self.route.send(ev);
        }
    }
}

/// Highest wire pad index (`flags` / snapshot `pad`). The uinput manager caps creation separately.
const MAX_WIRE_PADS: usize = punktfunk_core::input::MAX_PADS;

mod backends;
use backends::PadBackends;

/// How long a declared Steam Controller waits for its identity ([`Pads::waits_for_identity`]).
const IDENTITY_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// How long an undeclared pad waits for its kind ([`Pads::waits_for_arrival`]). A client
/// re-sends a lost arrival twice, 100 ms apart.
const ARRIVAL_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

/// When a pad's identity wait started, once it declared `resolved` over `previous`. The first
/// declaration of an identity kind starts it, even when the session default already is one; a
/// re-declare keeps the clock it started; any other kind has no wait.
fn identity_wait_after(
    previous: GamepadPref,
    resolved: GamepadPref,
    first_declaration: bool,
    since: Option<std::time::Instant>,
    now: std::time::Instant,
) -> Option<std::time::Instant> {
    match (takes_identity(previous), takes_identity(resolved)) {
        (false, true) => Some(now),
        (true, true) if first_declaration => Some(now),
        (true, true) => since,
        (_, false) => None,
    }
}

/// The kinds whose virtual pad answers as the physical one a client captured: both Steam
/// Controllers.
fn takes_identity(kind: GamepadPref) -> bool {
    matches!(
        kind,
        GamepadPref::SteamController
            | GamepadPref::SteamController2
            | GamepadPref::SteamController2Puck
    )
}

/// Per-pad virtual-gamepad router. Each index uses the kind declared in
/// [`InputKind::GamepadArrival`]; undeclared pads keep the Hello session default.
///
/// Managers are created lazily and own only the indices routed to them. An index
/// another manager owns is `None` here, so an `active_mask` unplug sweep never
/// tears down another kind's device.
///
/// [`resolve_pad_kind`] folds any kind the build cannot construct into one it can.
struct Pads {
    /// Wire index → host-wide OS slot ([`crate::inject::pad_pool`]).
    /// Every client numbers its first pad 0; using the wire index as the OS
    /// identity (mailbox, instance id, pairing MAC) would collide two sessions.
    /// Claimed on first present frame, released on unplug.
    slots: crate::inject::pad_pool::PadSlotMap<'static>,
    /// Slot mask as last reported ([`Pads::take_slot_change`]). The OS slot is the
    /// player number, so the console and the client are told when it moves — once
    /// per change, not once per frame.
    published: u16,
    /// One warn per session when OS slots are exhausted — not one per frame.
    slots_exhausted_warned: bool,
    /// Wire pads whose device is on its unplug grace. The pool slot follows the device out
    /// (`reap_pending`), not the removal frame: released early, a second claimant lands on the
    /// lingering mailbox and blocks 200 ms on a false "owned elsewhere".
    pending_release: [Option<std::time::Instant>; MAX_WIRE_PADS],
    /// Resolved kind per pad; session default until a `GamepadArrival`.
    kinds: [GamepadPref; MAX_WIRE_PADS],
    /// What the client asked for, before [`resolve_pad_kind`] folded it. `None` until
    /// this pad declares. Reported by the Controllers feed, never used for routing.
    declared: [Option<GamepadPref>; MAX_WIRE_PADS],
    /// What a captured Steam Controller told us it is, by wire pad. Kept across a re-plug: a
    /// new pad on the index sends its own before it arrives.
    identities: [Option<std::sync::Arc<crate::inject::triton_proto::Sc2Identity>>; MAX_WIRE_PADS],
    /// When each pad declared a kind that [`takes_identity`] ([`Pads::waits_for_identity`]).
    identity_since: [Option<std::time::Instant>; MAX_WIRE_PADS],
    /// When each pad's first frame came with no kind declared ([`Pads::waits_for_arrival`]).
    undeclared_since: [Option<std::time::Instant>; MAX_WIRE_PADS],
    /// Pads whose raw reports may build them ([`Pads::raw_builds`]): set by a declaration,
    /// cleared by a removal, so a report that lands after the remove builds nothing.
    raw_armed: [bool; MAX_WIRE_PADS],
    /// Manager that holds a built device at this index (`None` = none). Stays put
    /// if `kinds[idx]` later changes (arrival-after-first-frame), so a pad is
    /// never duplicated and removal always hits the manager that owns it.
    owner: [Option<GamepadPref>; MAX_WIRE_PADS],
    xbox360: Option<crate::inject::gamepad::GamepadManager>,
    /// Every other identity, per OS.
    backends: PadBackends,
    /// The session's `PUNKTFUNK_STEAM_FORCE`, for every kind [`resolve_pad_kind`] folds.
    steam_force: bool,
}

impl Pads {
    /// Every pad starts on the session kind ([`resolve_gamepad`]) until it declares otherwise.
    /// `id` names the device and the player slot it asked for, so its pads land on the
    /// same OS slots they had last connect ([`crate::inject::pad_pool::PadIdentity`]).
    /// `steam_force` is the session's `PUNKTFUNK_STEAM_FORCE`.
    fn new(
        default: GamepadPref,
        id: crate::inject::pad_pool::PadIdentity,
        steam_force: bool,
    ) -> Pads {
        let default = resolve_pad_kind(default, steam_force);
        tracing::info!(
            default = default.as_str(),
            preferred_slot = ?id.preferred,
            "gamepad backends: per-pad router (session default)"
        );
        Pads {
            slots: crate::inject::pad_pool::PadSlotMap::new(id),
            published: 0,
            slots_exhausted_warned: false,
            pending_release: [None; MAX_WIRE_PADS],
            kinds: [default; MAX_WIRE_PADS],
            declared: [None; MAX_WIRE_PADS],
            identities: Default::default(),
            identity_since: [None; MAX_WIRE_PADS],
            undeclared_since: [None; MAX_WIRE_PADS],
            raw_armed: [false; MAX_WIRE_PADS],
            owner: [None; MAX_WIRE_PADS],
            xbox360: None,
            backends: PadBackends::default(),
            steam_force,
        }
    }

    /// Record a declared kind (resolved to a buildable backend). Takes effect on
    /// the next frame. A device already built keeps its owner until re-plug —
    /// no live swap, even if arrival lands after the first frame. The unresolved
    /// kind is kept beside it: the Controllers page names both.
    fn set_kind(&mut self, idx: usize, kind: GamepadPref) {
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let resolved = resolve_pad_kind(kind, self.steam_force);
        if self.kinds[idx] != resolved {
            tracing::info!(
                pad = idx,
                kind = resolved.as_str(),
                "gamepad kind declared (per-pad)"
            );
        }
        self.identity_since[idx] = identity_wait_after(
            self.kinds[idx],
            resolved,
            self.declared[idx].is_none(),
            self.identity_since[idx],
            std::time::Instant::now(),
        );
        self.kinds[idx] = resolved;
        self.declared[idx] = Some(kind);
        self.raw_armed[idx] = true;
    }

    /// Who the Steam Controller on wire pad `id.pad` is, for the virtual pad built next.
    fn set_identity(&mut self, id: &punktfunk_core::quic::PadIdentity) {
        let idx = usize::from(id.pad);
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let identity = crate::inject::triton_proto::Sc2Identity::from_wire(id);
        tracing::info!(
            pad = idx,
            serial = identity
                .as_ref()
                .and_then(|i| i.serial.as_deref())
                .unwrap_or("-"),
            replies = identity.as_ref().map_or(0, |i| i.replies.len()),
            "pad identity received"
        );
        self.identities[idx] = identity.map(std::sync::Arc::new);
    }

    /// A client sends a pad's identity on the control stream and its arrival as a datagram, so
    /// the arrival can land first. A Steam Controller waits up to [`IDENTITY_WAIT`] for it: the
    /// virtual pad is built once, as itself. A client that sends none gets the canned identity.
    fn waits_for_identity(&self, idx: usize) -> bool {
        cfg!(any(target_os = "linux", windows))
            && self.owner[idx].is_none()
            && takes_identity(self.kinds[idx])
            && self.identities[idx].is_none()
            && self.identity_since[idx].is_some_and(|t| t.elapsed() < IDENTITY_WAIT)
    }

    /// Whether a raw report from wire pad `idx` stands for its first frame: a Steam Controller
    /// declared since its last removal, with no device. Its raw reports are its state, and a
    /// pad at rest, or driven only by trackpads and gyro, sends no typed frame to build it.
    fn raw_builds(&self, idx: usize) -> bool {
        idx < MAX_WIRE_PADS
            && self.owner[idx].is_none()
            && self.raw_armed[idx]
            && takes_identity(self.kinds[idx])
    }

    /// A built pad never changes kind, so a frame that beats a lost arrival would pin the
    /// session default for good. An undeclared pad waits up to [`ARRIVAL_WAIT`] from its first
    /// frame for the re-sent arrival; a client that declares nothing gets the default after.
    fn waits_for_arrival(&mut self, idx: usize) -> bool {
        if self.owner[idx].is_some() || self.declared[idx].is_some() {
            return false;
        }
        let since = *self.undeclared_since[idx].get_or_insert_with(std::time::Instant::now);
        since.elapsed() < ARRIVAL_WAIT
    }

    /// The device pad `idx` is: the kind it was built as, else the one it will be built as
    /// (the declared kind as this host resolves it). A DualSense the host folds to Xbox 360
    /// has no speaker to stream to.
    fn device_kind(&self, idx: usize) -> Option<GamepadPref> {
        self.owner
            .get(idx)
            .copied()
            .flatten()
            .or_else(|| self.kinds.get(idx).copied())
    }

    /// This pad as the Controllers feed reports it: the device the host built, the
    /// kind the client asked for, and the state this thread just applied.
    fn feed_frame(&self, idx: usize, wire: &WirePads) -> crate::pad_feed::PadFrame {
        let (state, mask) = (&wire.state[idx], wire.mask);
        crate::pad_feed::PadFrame {
            pad: idx as u8,
            ts_ms: crate::clock::unix_ms(),
            device: self
                .device_kind(idx)
                .unwrap_or(GamepadPref::Xbox360)
                .as_str()
                .to_string(),
            declared: self.declared[idx].map(|k| k.as_str().to_string()),
            slot: self.slots.slot_of(idx),
            present: mask & (1 << idx) != 0,
            buttons: state.buttons,
            left_trigger: state.left_trigger,
            right_trigger: state.right_trigger,
            ls_x: state.ls_x,
            ls_y: state.ls_y,
            rs_x: state.rs_x,
            rs_y: state.rs_y,
        }
    }

    /// Apply wire pad `idx`'s frame, and show it on the Controllers feed.
    fn apply_wire(&mut self, wire: &WirePads, idx: usize, feed: &crate::pad_feed::PadFeed) {
        self.handle(&punktfunk_core::input::GamepadEvent::State(wire.frame(idx)));
        feed.publish(|| self.feed_frame(idx, wire));
    }

    fn handle(&mut self, ev: &punktfunk_core::input::GamepadEvent) {
        use punktfunk_core::input::GamepadEvent;
        // Present = mask bit set (create/update). Cleared bit is the `GamepadRemove` frame.
        let (idx, present) = match ev {
            GamepadEvent::State(f) => {
                let idx = f.index as usize;
                (idx, f.active_mask & (1 << idx) != 0)
            }
            GamepadEvent::Arrival { index, .. } => (*index as usize, true),
        };
        if idx >= MAX_WIRE_PADS
            || present && (self.waits_for_arrival(idx) || self.waits_for_identity(idx))
        {
            return;
        }
        // The only wire→OS slot translation. Claim on present; a removal must not
        // mint a slot for a pad that is going away.
        let slot = if present {
            self.slots.claim_for(idx)
        } else {
            self.slots.slot_of(idx)
        };
        let Some(slot) = slot else {
            if present && !self.slots_exhausted_warned {
                self.slots_exhausted_warned = true;
                tracing::warn!(
                    pad = idx,
                    max = MAX_WIRE_PADS,
                    "no host pad slot left — every OS slot is held by a live session, so this pad \
                     gets no device. It appears when a slot frees (another session ending, or one \
                     of its pads unplugging)."
                );
            }
            return;
        };
        let had_device = self.owner[idx].is_some();
        if present && !had_device && takes_identity(self.kinds[idx]) {
            crate::inject::triton_proto::stage_identity(slot, self.identities[idx].clone());
        }
        let (kind, new_owner) = route_decision(self.owner[idx], self.kinds[idx], present);
        self.owner[idx] = new_owner;
        self.route_handle(kind, &self.re_index(ev, slot));
        if present {
            self.pending_release[idx] = None;
        } else {
            self.note_removed(idx, had_device, std::time::Instant::now());
        }
    }

    /// `pad_slots::SWEEP_GRACE` plus one backend pump: the device, and its mailbox, are gone
    /// before the slot is offered again.
    const RELEASE_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

    /// A removal frame. A built device lingers its grace, so its slot lingers too: a re-plug
    /// inside it re-mints the same slot ([`PadSlotMap::claim_for`] memoizes) and the sweep hands
    /// back the live pad. A pad with no device has nothing to wait for.
    fn note_removed(&mut self, idx: usize, had_device: bool, now: std::time::Instant) {
        if had_device {
            self.pending_release[idx] = Some(now);
        } else {
            self.pending_release[idx] = None;
            self.slots.release(idx);
        }
    }

    fn reap_pending(&mut self) {
        self.reap_pending_at(std::time::Instant::now());
    }

    fn reap_pending_at(&mut self, now: std::time::Instant) {
        for idx in 0..MAX_WIRE_PADS {
            if self.pending_release[idx]
                .is_some_and(|t| now.duration_since(t) >= Self::RELEASE_GRACE)
            {
                self.pending_release[idx] = None;
                self.slots.release(idx);
            }
        }
    }

    /// A wire pad that declared itself, and so reserved a slot, but never sent a frame has no
    /// device to linger: its removal releases the slot at once. A pad in its unplug grace is
    /// not that pad: its device still lingers, and a re-sent removal must not free the slot.
    fn release_unbuilt(&mut self, idx: usize) {
        if idx < MAX_WIRE_PADS && self.owner[idx].is_none() && self.pending_release[idx].is_none() {
            self.pending_release[idx] = None;
            self.slots.release(idx);
        }
    }

    /// Rewrites wire numbering into the host-wide slot the backends create under.
    /// `active_mask` is translated too: a wire-space mask would unplug another
    /// session's pad, or spare one this session had dropped.
    fn re_index(
        &self,
        ev: &punktfunk_core::input::GamepadEvent,
        slot: u8,
    ) -> punktfunk_core::input::GamepadEvent {
        use punktfunk_core::input::GamepadEvent;
        match ev {
            GamepadEvent::State(f) => {
                let mut f = *f;
                f.index = i16::from(slot);
                f.active_mask = self.slots.os_mask(f.active_mask);
                GamepadEvent::State(f)
            }
            GamepadEvent::Arrival {
                kind,
                capabilities,
                audio_caps,
                ..
            } => GamepadEvent::Arrival {
                index: slot,
                kind: *kind,
                capabilities: *capabilities,
                audio_caps: *audio_caps,
            },
        }
    }

    /// Reserve this wire pad's OS slot without building a device — the identity every
    /// per-pad host resource is named by ([`crate::inject::pad_pool`]), which a pad-audio
    /// streamer needs before the pad's first frame. Idempotent: the frame that follows
    /// claims the same slot. `None` once every slot on the host is held.
    fn claim_os_slot(&mut self, wire: usize) -> Option<u8> {
        self.slots.claim_for(wire)
    }

    /// The OS slots this session holds, when they have moved since the last call.
    /// Bit `n` = slot `n` = player `n + 1`.
    fn take_slot_change(&mut self) -> Option<u16> {
        let now = self.slots.held_mask();
        (now != self.published).then(|| {
            self.published = now;
            now
        })
    }

    /// [`Self::re_index`] for the rich plane (touchpad, motion, raw HID reports).
    /// `None` when this wire pad holds no slot: rich never creates a device.
    ///
    /// A client numbers its first pad 0 and the slot is claimed on that pad's first
    /// frame, so the pad that moves first takes slot 0 whatever its wire index. Skip
    /// this and two pads swap devices — every SC2 raw report, and the rumble Steam
    /// answers it with, lands on the other player's controller.
    fn rich_in_slot_space(
        &self,
        mut rich: punktfunk_core::quic::RichInput,
    ) -> Option<punktfunk_core::quic::RichInput> {
        rich.set_pad(self.slots.slot_of(rich.pad() as usize)?);
        Some(rich)
    }

    fn route_handle(&mut self, kind: GamepadPref, ev: &punktfunk_core::input::GamepadEvent) {
        if !self.backends.route_handle(kind, ev) {
            self.xbox360
                .get_or_insert_with(crate::inject::gamepad::GamepadManager::new)
                .handle(ev);
        }
    }

    /// Touchpad / motion for the pad's manager. No device yet → no-op. Xbox has no rich plane.
    fn apply_rich(&mut self, rich: punktfunk_core::quic::RichInput) {
        let idx = rich.pad() as usize;
        // Same wire→OS-slot rewrite `re_index` does for events: the managers index their
        // slot table in OS space. No slot = no device for this pad, and rich never
        // creates one.
        let Some(rich) = self.rich_in_slot_space(rich) else {
            return;
        };
        // Owner, else declared kind (pre-first-frame). After a kind change, rich must not
        // land on the wrong backend.
        let kind = self
            .owner
            .get(idx)
            .copied()
            .flatten()
            .or_else(|| self.kinds.get(idx).copied())
            .unwrap_or(GamepadPref::Xbox360);
        self.backends.apply_rich(kind, rich);
    }

    /// How long the input thread may wait for the next event. A built Steam Controller polls
    /// at 1 ms: Steam plays its trackpad haptics as single pulses, up to 1 kHz on a Triton, and
    /// they must not sit 4 ms then burst. Any other device, or one in its unplug grace, pumps
    /// feedback every 4 ms. With none, nothing is owed between events: 25 ms bounds how late
    /// `stop` and a withdrawn grant are seen.
    fn feedback_poll_interval(&self) -> std::time::Duration {
        if self.owner.iter().flatten().any(|k| takes_identity(*k)) {
            return std::time::Duration::from_millis(1);
        }
        if self.owner.iter().any(Option::is_some)
            || self.pending_release.iter().any(Option::is_some)
        {
            return std::time::Duration::from_millis(4);
        }
        std::time::Duration::from_millis(25)
    }

    /// Pump every live backend. `rumble` is `(pad, low, high, lt, rt)` on 0xCA;
    /// `hidout` is lightbar / LEDs / adaptive triggers on UHID/UMDF. Closures
    /// re-borrow to satisfy `FnMut`.
    ///
    /// Only the Windows HID Xbox managers ever report non-zero trigger levels;
    /// every other backend's source packet has no field for them, so v3 goes
    /// out as v2 plus a zero tail (`PadFeedback::rumble`).
    fn pump(
        &mut self,
        mut rumble: impl FnMut(u16, u16, u16, u16, u16),
        mut hidout: impl FnMut(punktfunk_core::quic::HidOutput),
    ) {
        self.reap_pending();
        // Reverse of `re_index`: backends tag OS slots; the client only knows its wire
        // index. Miss this and rumble lands on another session's pad.
        // Snapshot first: the callbacks borrow `&mut self.<manager>`, so they
        // cannot also borrow `self.slots`.
        let mut wire_of = [None; MAX_WIRE_PADS];
        for (slot, wire) in wire_of.iter_mut().enumerate() {
            *wire = self.slots.wire_of(slot as u8);
        }
        // No wire index = not this session's pad; drop the feedback.
        let mut rumble = |pad: u16, low, high, lt, rt| {
            if let Some(wire) = wire_of.get(pad as usize).copied().flatten() {
                rumble(wire as u16, low, high, lt, rt);
            }
        };
        let mut hidout = |h: punktfunk_core::quic::HidOutput| {
            if let Some(wire) = wire_of.get(h.pad() as usize).copied().flatten() {
                hidout(h.with_pad(wire as u16));
            }
        };
        if let Some(m) = &mut self.xbox360 {
            m.pump_rumble(&mut rumble); // Xbox has no rich-feedback plane
        }
        self.backends.pump(&mut rumble, &mut hidout);
    }

    /// Re-emit HID reports so kernel/SDL do not drop a held-steady UHID/UMDF pad.
    /// Xbox evdev holds last-known state — no heartbeat. Cadence is each manager's
    /// gap timer, not this per-tick call.
    fn heartbeat(&mut self) {
        self.backends.heartbeat();
    }
}

/// Per-pad 0xD1 streamers (`super::pad_audio`). `spawn` refuses pads past 0..4.
/// Opened when a DualSense-family arrival declares renderer bits; reaped on
/// remove / re-declare / teardown.
struct PadAudioSlots {
    /// `(kinds, handle)` per running pad. `kinds` makes an identical re-arrival
    /// (resent against datagram loss) a no-op.
    slots: [Option<(u8, pad_audio::PadAudioHandle)>; MAX_WIRE_PADS],
    /// Streamer starts inside the current window, first one included. Arrivals are client-
    /// triggered; without a ceiling the client decides how many WASAPI captures open.
    starts: [u8; MAX_WIRE_PADS],
    /// When the window's first start happened; `None` before any.
    window_start: [Option<std::time::Instant>; MAX_WIRE_PADS],
}

/// Captures one pad may open inside [`START_WINDOW`]. A real pad declares once and identical
/// re-arrivals no-op; a flaky link re-plugs a few times an hour, a client cycling kinds or
/// declare/stop would open WASAPI captures forever.
const MAX_PAD_AUDIO_STARTS: u8 = 8;
const START_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

impl PadAudioSlots {
    fn new() -> PadAudioSlots {
        PadAudioSlots {
            slots: std::array::from_fn(|_| None),
            starts: [0; MAX_WIRE_PADS],
            window_start: [None; MAX_WIRE_PADS],
        }
    }

    /// Same kinds → keep; changed → restart; idle → spawn. A slot with no endpoint
    /// stays empty (arrivals retry a few times). Only an actual open spends
    /// [`MAX_PAD_AUDIO_STARTS`]. `edge` selects DualSense Edge on Linux; Windows
    /// endpoints are pre-stamped so it is ignored there.
    ///
    /// Two index spaces: this table and the client's datagrams use `pad`, while the
    /// endpoint / card / sink a streamer captures is named by the pad's OS `slot`.
    fn ensure(
        &mut self,
        conn: &super::link::SessionLink,
        pad: u8,
        slot: u8,
        kinds: u8,
        edge: bool,
    ) {
        let idx = pad as usize;
        if idx >= MAX_WIRE_PADS {
            return;
        }
        let running = self.slots[idx].as_ref().map(|(have, _)| *have);
        if running == Some(kinds) {
            return;
        }
        // Client-driven: cycling kinds or declare/stop would spawn WASAPI captures
        // forever. Gate before `stop` so a pad at the ceiling keeps the streamer
        // it has instead of losing it to the last request.
        if self.window_start[idx].is_some_and(|t| t.elapsed() >= START_WINDOW) {
            self.starts[idx] = 0;
            self.window_start[idx] = None;
        }
        if self.starts[idx] >= MAX_PAD_AUDIO_STARTS {
            tracing::warn!(
                pad = idx,
                "pad-audio streamer already started {MAX_PAD_AUDIO_STARTS} times — ignoring; the \
                 pad keeps whatever streamer it has for this session"
            );
            return;
        }
        if running.is_some() {
            tracing::info!(
                pad = idx,
                starts = self.starts[idx],
                "pad-audio kinds changed — restarting the streamer"
            );
            self.stop(idx);
        }
        let stop = Arc::new(AtomicBool::new(false));
        if let Some(h) = pad_audio::spawn(conn.clone(), pad, slot, kinds, edge, stop) {
            // Charge only an open that happened. A slot with no endpoint must not
            // spend the ceiling on arrival re-sends.
            self.starts[idx] += 1;
            self.window_start[idx].get_or_insert_with(std::time::Instant::now);
            self.slots[idx] = Some((kinds, h));
        }
    }

    /// Signal and reap on a detached thread. A quiet capturer can sit ~5 s in recv;
    /// this thread must keep ≤4 ms (games block on GET_REPORT). Failed spawn
    /// falls back to the handle's drop (signal + join).
    fn stop(&mut self, idx: usize) {
        if let Some((_, h)) = self.slots.get_mut(idx).and_then(|s| s.take()) {
            h.signal();
            let _ = std::thread::Builder::new()
                .name("punktfunk1-padreap".into())
                .spawn(move || h.stop());
        }
    }

    /// Signal every streamer first so they wind down concurrently, then join.
    /// Worst case is one ~5 s quiet-endpoint timeout, inside the session's 10 s
    /// side-thread grace — not one timeout per pad.
    fn stop_all(&mut self) {
        for s in self.slots.iter().flatten() {
            s.1.signal();
        }
        for s in &mut self.slots {
            if let Some((_, h)) = s.take() {
                h.stop();
            }
        }
    }
}

/// Both input planes on one channel so the thread wakes on either. A second
/// rich channel drained after the 4 ms recv timeout quantized every gyro sample.
pub(super) enum ClientInput {
    /// 0xC8: pointer / keyboard / gamepad button+axis.
    Event(InputEvent),
    /// 0xCC: touchpad contacts + motion samples.
    Rich(punktfunk_core::quic::RichInput),
    /// 0xCC/0x05 stylus batches, diffed into a per-session virtual tablet
    /// (`design/pen-tablet-input.md`).
    Pen(punktfunk_core::quic::PenBatch),
    /// A captured Steam Controller's serial and feature replies, off the control stream.
    PadIdentity(punktfunk_core::quic::PadIdentity),
}

/// Whether `input` is a person acting. A pad's 10 Hz snapshot beacon, its gyro and its raw HID
/// reports stream while nobody touches anything, so they never hold off the suspend veto; a
/// snapshot counts where it changes the pad ([`WirePads::changes`]).
fn is_activity(input: &ClientInput) -> bool {
    use punktfunk_core::quic::RichInput;
    match input {
        ClientInput::Event(ev) => ev.kind != InputKind::GamepadState,
        ClientInput::Rich(RichInput::Touchpad { .. } | RichInput::TouchpadEx { .. }) => true,
        ClientInput::Rich(_) | ClientInput::PadIdentity(_) => false,
        ClientInput::Pen(_) => true,
    }
}

/// Per-session stylus ([`crate::pen_sink::PenSink`]) plus the stroke timeout this
/// lossy datagram plane needs.
struct PenSession {
    sink: crate::pen_sink::PenSink,
    last_rx: std::time::Instant,
}

impl PenSession {
    fn new() -> PenSession {
        PenSession {
            sink: Default::default(),
            last_rx: std::time::Instant::now(),
        }
    }

    fn apply(&mut self, batch: &punktfunk_core::quic::PenBatch) {
        self.last_rx = std::time::Instant::now();
        self.sink.apply(batch);
    }

    /// Dead-client failsafe ([`PEN_TOUCH_TIMEOUT_MS`](punktfunk_core::quic::PEN_TOUCH_TIMEOUT_MS)).
    /// Clients repeat the last sample (≤100 ms) while in range, including a
    /// stationary touch, so silence means gone — do not leave the stroke down.
    /// The input loop caps recv at 100 ms while the pen is active so this runs.
    fn check_timeout(&mut self) {
        if self.sink.active()
            && self.last_rx.elapsed().as_millis()
                >= punktfunk_core::quic::PEN_TOUCH_TIMEOUT_MS as u128
        {
            tracing::debug!("pen: sample stream went silent — force-releasing the stroke");
            self.release_all();
        }
    }

    /// Session end and the timeout.
    fn release_all(&mut self) {
        self.sink.force_release();
    }

    fn active(&self) -> bool {
        self.sink.active()
    }
}

/// Default 0xCA envelope TTL: the client silences unless renewed. Covers 2–3
/// lost renewals and caps an abandoned rumble on every client. Override with
/// `PUNKTFUNK_RUMBLE_TTL_MS`, floored at [`RUMBLE_TTL_FLOOR_MS`].
const RUMBLE_TTL_MS: u16 = 400;
/// `PUNKTFUNK_RUMBLE_TTL_MS` floor. Below this, ~50 ms client ticks make expiry audible
/// (`design/rumble-envelope-plan.md`).
const RUMBLE_TTL_FLOOR_MS: u16 = 150;
/// `PUNKTFUNK_RUMBLE_TTL_MS` ceiling. A multi-second lease is no longer prompt,
/// and staying well under `u16::MAX` avoids a client treating TTL as a sentinel.
const RUMBLE_TTL_CEIL_MS: u16 = 5_000;
/// Floor for renew = ttl × 3/10, so an aggressive TTL hatch cannot spin faster.
const RUMBLE_RENEW_FLOOR_MS: u64 = 60;
/// Stop re-sends on later renewal ticks after the immediate zero datagram.
/// Covers stop loss for legacy clients; a v2 client also self-silences at TTL.
/// Immediate send + this many = 3 zeros total.
const RUMBLE_STOP_BURST: u8 = 2;

/// The session encoder's framing of the captured picture. Default maps nothing.
pub(super) type FrameMap = Arc<std::sync::Mutex<punktfunk_core::video_fit::Reframe>>;

/// Moves absolute pointer and touch input from a cropped picture back to the source. A scale
/// alone needs nothing: injectors already normalise by the event's own extent.
fn reframe_input(ev: &mut InputEvent, map: &punktfunk_core::video_fit::Reframe) {
    let absolute = matches!(
        ev.kind,
        InputKind::MouseMoveAbs | InputKind::TouchDown | InputKind::TouchMove
    );
    let extent = (f64::from(ev.flags >> 16), f64::from(ev.flags & 0xffff));
    if !absolute || !map.is_cropped() || extent.0 == 0.0 || extent.1 == 0.0 {
        return;
    }
    let (x, y) = map.to_source(f64::from(ev.x), f64::from(ev.y), extent);
    ev.x = x.round() as i32;
    ev.y = y.round() as i32;
    ev.flags = (map.source.0.min(0xffff) << 16) | map.source.1.min(0xffff);
}

/// [`reframe_input`] for a stylus batch, whose samples are `0..=1` of the picture.
fn reframe_pen(
    batch: &punktfunk_core::quic::PenBatch,
    map: &punktfunk_core::video_fit::Reframe,
) -> punktfunk_core::quic::PenBatch {
    if !map.is_cropped() {
        return *batch;
    }
    let (sw, sh) = (f64::from(map.source.0), f64::from(map.source.1));
    let mut samples = batch.samples().to_vec();
    for s in &mut samples {
        let (x, y) = map.to_source(f64::from(s.x), f64::from(s.y), (1.0, 1.0));
        (s.x, s.y) = ((x / sw) as f32, (y / sh) as f32);
    }
    punktfunk_core::quic::PenBatch::new(batch.seq, &samples)
}

/// `(low, high, left_trigger, right_trigger)`, `0..=0xFFFF`, 0xCA order. One
/// value because they share one `seq` and one TTL on the wire.
type RumbleLevels = (u16, u16, u16, u16);

/// All four motors zero. A `(low, high)`-only test stamps trigger-only rumble
/// (racing titles drive triggers with silent handles) as `ttl = 0`; the client
/// silences on arrival with no error ([`tests::a_trigger_only_rumble_gets_a_live_ttl`]).
fn rumble_silent(lv: RumbleLevels) -> bool {
    lv == (0, 0, 0, 0)
}

/// One 0xCA rumble datagram, always v3. Best-effort, like every side-plane datagram.
///
/// v3 is unconditional — not "only if a trigger is non-zero". A history-dependent wire
/// form is a sequence bug; pre-v3 clients read the 10-byte prefix and ignore the tail.
fn rumble_datagram(pad: u16, lv: RumbleLevels, seq: u8, ttl_ms: u16) -> Vec<u8> {
    let (low, high, lt, rt) = lv;
    punktfunk_core::quic::encode_rumble_datagram_v3(pad, low, high, seq, ttl_ms, lt, rt).to_vec()
}

/// Per-pad 0xCA rumble leases. Rumble is v3 (`[level][seq][ttl_ms][trigger levels]`):
/// an active level renews every `ttl × 3/10` and an abandoned one expires client-side
/// (`design/rumble-envelope-plan.md`, `design/trigger-rumble-plane.md`). Four motors
/// share one `seq` and one TTL.
struct RumbleLeases {
    lv: [RumbleLevels; MAX_WIRE_PADS],
    seen: [bool; MAX_WIRE_PADS],
    /// Wraps per pad and is bumped on every change and renewal. Never reset, not even
    /// by [`Self::clear`]: the client's gate has no reset (`client/pump/datagram_task.rs`).
    seq: [u8; MAX_WIRE_PADS],
    stop_burst: [u8; MAX_WIRE_PADS],
    ttl_ms: u16,
    every: std::time::Duration,
    last: std::time::Instant,
}

impl RumbleLeases {
    /// The session's `PUNKTFUNK_RUMBLE_TTL_MS`, held inside the floor and ceiling; unset is
    /// [`RUMBLE_TTL_MS`].
    fn with_ttl(ttl_ms: Option<u16>) -> RumbleLeases {
        let ttl_ms = ttl_ms
            .map(|v| v.clamp(RUMBLE_TTL_FLOOR_MS, RUMBLE_TTL_CEIL_MS))
            .unwrap_or(RUMBLE_TTL_MS);
        RumbleLeases::new(ttl_ms, std::time::Instant::now())
    }

    /// Renew at 30 % of TTL (≈120 ms at 400) so 2–3 renewals cover the lease.
    fn new(ttl_ms: u16, now: std::time::Instant) -> RumbleLeases {
        let every =
            std::time::Duration::from_millis((ttl_ms as u64 * 3 / 10).max(RUMBLE_RENEW_FLOOR_MS));
        RumbleLeases {
            lv: [(0, 0, 0, 0); MAX_WIRE_PADS],
            seen: [false; MAX_WIRE_PADS],
            seq: [0; MAX_WIRE_PADS],
            stop_burst: [0; MAX_WIRE_PADS],
            ttl_ms,
            every,
            last: now,
        }
    }

    /// A backend's new level for `pad`, as the datagram to send. Every change bumps
    /// `seq`; a fall to zero arms the stop burst, and a re-assert clears it.
    fn on_level(&mut self, pad: u16, lv: RumbleLevels) -> Vec<u8> {
        let idx = pad as usize;
        if idx >= MAX_WIRE_PADS {
            // Out of range (backends never emit this): forwarded ungated.
            return rumble_datagram(pad, lv, 0, self.ttl_ms);
        }
        let (silent, prev) = (rumble_silent(lv), self.lv[idx]);
        // Silent→active once per buzz, with the triggers, so "host never saw trigger
        // rumble" is separable from "client never rendered it".
        if rumble_silent(prev) && !silent {
            let (low, high, lt, rt) = lv;
            tracing::debug!(
                pad,
                low,
                high,
                lt,
                rt,
                "rumble: forwarding to client (0xCA)"
            );
        }
        self.lv[idx] = lv;
        self.seen[idx] = true;
        self.seq[idx] = self.seq[idx].wrapping_add(1);
        self.stop_burst[idx] = if silent && !rumble_silent(prev) {
            RUMBLE_STOP_BURST
        } else {
            0
        };
        // Any of the four motors → live TTL. See `rumble_silent`.
        let ttl = if silent { 0 } else { self.ttl_ms };
        rumble_datagram(pad, lv, self.seq[idx], ttl)
    }

    /// Once per interval: renew each active lease (bump `seq`, fresh TTL), drain a stop
    /// burst, then go quiet.
    fn tick(&mut self, now: std::time::Instant, mut send: impl FnMut(Vec<u8>)) {
        if now.saturating_duration_since(self.last) < self.every {
            return;
        }
        self.last = now;
        for i in (0..MAX_WIRE_PADS).filter(|&i| self.seen[i]) {
            let lv = self.lv[i];
            let silent = rumble_silent(lv);
            if silent {
                if self.stop_burst[i] == 0 {
                    continue;
                }
                self.stop_burst[i] -= 1;
            }
            self.seq[i] = self.seq[i].wrapping_add(1);
            let ttl = if silent { 0 } else { self.ttl_ms };
            send(rumble_datagram(i as u16, lv, self.seq[i], ttl));
        }
    }

    /// Drop a removed pad's lease so a re-plug on the same index can't buzz the new
    /// device. `seq` stays ([`tests::rumble_seq_survives_a_removal_so_the_client_gate_accepts`]).
    fn clear(&mut self, idx: usize) {
        self.lv[idx] = (0, 0, 0, 0);
        self.seen[idx] = false;
        self.stop_burst[idx] = 0;
    }
}

/// The session's wire pads: each one's state, the attached mask and the seq gate that
/// snapshots and removals share. An accepted event returns the pad whose
/// [`Self::frame`] must reach the backends.
#[derive(Default)]
struct WirePads {
    /// Incremental events fold in; a snapshot replaces one. `pad`/`seq` stay zero so an
    /// unchanged snapshot refresh compares equal.
    state: [punktfunk_core::input::GamepadSnapshot; MAX_WIRE_PADS],
    mask: u16,
    /// Last applied snapshot or removal seq, `None` until the first. An older one must
    /// not roll held state back.
    seq: [Option<u8>; MAX_WIRE_PADS],
}

impl WirePads {
    fn frame(&self, idx: usize) -> punktfunk_core::input::GamepadFrame {
        self.state[idx].to_frame(idx as u8, self.mask)
    }

    fn attached(&self) -> impl Iterator<Item = usize> + '_ {
        (0..MAX_WIRE_PADS).filter(|i| self.mask & (1 << i) != 0)
    }

    /// Attach pad `idx` at rest, leaving the seq gate alone: a pad whose first state is a raw
    /// report. Its removal then clears it like any attached pad.
    fn attach_at_rest(&mut self, idx: usize) {
        if idx < MAX_WIRE_PADS && self.mask & (1 << idx) == 0 {
            self.mask |= 1 << idx;
            self.state[idx] = Default::default();
        }
    }

    /// One button or axis event. `None` = bad index or unknown axis.
    fn fold(&mut self, ev: &InputEvent) -> Option<usize> {
        let idx = ev.flags as usize;
        if idx >= MAX_WIRE_PADS || !self.state[idx].fold(ev) {
            return None;
        }
        self.mask |= 1 << idx;
        Some(idx)
    }

    /// Open every pad's seq gate. A client stamps a seq on each refresh even while its sends
    /// are filtered, so after a gamepad grant comes back its seq can sit anywhere ahead.
    fn forget_seqs(&mut self) {
        self.seq = [None; MAX_WIRE_PADS];
    }

    /// Whether `snap` moves the pad: newer than the gate, and its first state or another one.
    fn changes(&self, snap: &punktfunk_core::input::GamepadSnapshot) -> bool {
        let idx = snap.pad as usize;
        idx < MAX_WIRE_PADS
            && punktfunk_core::input::GamepadSnapshot::seq_newer(snap.seq, self.seq[idx])
            && (self.mask & (1 << idx) == 0
                || self.state[idx]
                    != punktfunk_core::input::GamepadSnapshot {
                        pad: 0,
                        seq: 0,
                        ..*snap
                    })
    }

    /// A newer snapshot replaces the pad. An unchanged refresh (~100 ms) of a `built` pad
    /// advances the gate but returns `None`: re-emitting it churns the XInput packet number.
    /// A pad with no device yet (its arrival, identity or slot still owed) gets every refresh,
    /// so its build is retried without waiting for the player to move it.
    fn snapshot(
        &mut self,
        snap: punktfunk_core::input::GamepadSnapshot,
        built: bool,
    ) -> Option<usize> {
        let idx = snap.pad as usize;
        if idx >= MAX_WIRE_PADS
            || !punktfunk_core::input::GamepadSnapshot::seq_newer(snap.seq, self.seq[idx])
        {
            return None;
        }
        self.seq[idx] = Some(snap.seq);
        let state = punktfunk_core::input::GamepadSnapshot {
            pad: 0,
            seq: 0,
            ..snap
        };
        if built && self.mask & (1 << idx) != 0 && self.state[idx] == state {
            return None;
        }
        self.state[idx] = state;
        self.mask |= 1 << idx;
        Some(idx)
    }

    /// Every attached pad back to rest, still attached. A withdrawn gamepad grant filters out
    /// the releases that would have got it there. The pads whose state changed.
    fn rest_all(&mut self) -> Vec<usize> {
        let moved: Vec<usize> = self
            .attached()
            .filter(|&i| self.state[i] != Default::default())
            .collect();
        for &i in &moved {
            self.state[i] = Default::default();
        }
        moved
    }

    /// A hot-unplug. `None` = stale or out of range; `Some(true)` = the pad was attached
    /// and its cleared [`Self::frame`] fires each backend's unplug sweep.
    fn remove(&mut self, pad: u8, seq: u8) -> Option<bool> {
        let idx = pad as usize;
        if idx >= MAX_WIRE_PADS
            || !punktfunk_core::input::GamepadSnapshot::seq_newer(seq, self.seq[idx])
        {
            return None;
        }
        self.seq[idx] = Some(seq);
        let attached = self.mask & (1 << idx) != 0;
        if attached {
            self.mask &= !(1 << idx);
            self.state[idx] = Default::default();
        }
        Some(attached)
    }
}

/// A `GamepadArrival`: `code` is the [`GamepadPref`], the low byte of `flags` the pad and
/// bits 8/9 its audio-render caps (always [`decode_gamepad_arrival`], never the whole
/// word). Starts or stops the pad's 0xD1 streamer when pad audio was negotiated.
///
/// [`decode_gamepad_arrival`]: punktfunk_core::input::decode_gamepad_arrival
fn declare_pad(
    pads: &mut Pads,
    streams: &mut PadAudioSlots,
    conn: &super::link::SessionLink,
    ev: &InputEvent,
    pad_audio_on: bool,
) {
    let (pad, audio_caps) = punktfunk_core::input::decode_gamepad_arrival(ev.flags);
    let idx = pad as usize;
    let kind = GamepadPref::from_u8(ev.code as u8);
    if audio_caps != 0 {
        tracing::debug!(
            pad = idx,
            haptics = audio_caps & 0x01 != 0,
            speaker = audio_caps & 0x02 != 0,
            "pad-audio render caps declared (arrival flags bits 8/9)"
        );
    }
    pads.set_kind(idx, kind);
    if !pad_audio_on {
        return;
    }
    // A DualSense-family device with renderer bits. A re-declare without bits, or a device
    // with no pad audio, stops the streamer.
    let device = pads.device_kind(idx);
    let dualsense = matches!(
        device,
        Some(GamepadPref::DualSense | GamepadPref::DualSenseEdge)
    );
    let want = if dualsense { audio_caps } else { 0 };
    // The streamer captures what the pad's OS slot names, and the first frame that would
    // claim the slot may be seconds away, so the declaration reserves it. No slot = no device.
    let slot = if want != 0 {
        pads.claim_os_slot(idx)
    } else {
        None
    };
    match slot {
        Some(slot) => streams.ensure(
            conn,
            pad,
            slot,
            want,
            device == Some(GamepadPref::DualSenseEdge),
        ),
        None => streams.stop(idx),
    }
}

/// The session's one `read_datagram` loop (two would race): 0xCB mic, 0xCC rich and pen, 0xC8
/// input, magics disjoint. Each is tested against the live grant mask before it is offered, and a
/// full queue drops rather than block the mic and this reader. The client's feedback goes to the
/// control task. Ends with the connection.
pub(super) fn spawn_datagram_reader(
    conn: super::link::SessionLink,
    grants: Arc<AtomicU32>,
    counters: Arc<crate::session_status::SessionCounters>,
    mic_tx: std::sync::mpsc::SyncSender<crate::audio::MicFrame>,
    input_tx: std::sync::mpsc::SyncSender<ClientInput>,
    feedback_tx: tokio::sync::mpsc::Sender<punktfunk_core::quic::v2::dgram::Feedback>,
) {
    tokio::spawn(async move {
        // Shared, not local: this task ends with the connection, which closes after the session
        // summary is built, so a local total would never reach it.
        let n = &*counters;
        let mut denied = crate::session_status::GrantDrops::new(conn.plane());
        let mic_source = crate::audio::mic_source_id();
        // Full queue: drop, never block (would stall mic + this reader). Disconnected ends the loop.
        let offer = |tx: &std::sync::mpsc::SyncSender<ClientInput>, item: ClientInput| match tx
            .try_send(item)
        {
            Ok(()) => true,
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                n.input_dropped.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => false,
        };
        while let Ok(incoming) = conn.read_datagram().await {
            let d = match incoming {
                super::link::Incoming::Plain(d) => d,
                super::link::Incoming::Feedback(fb) => {
                    let _ = feedback_tx.try_send(fb);
                    continue;
                }
            };
            // One relaxed load per datagram; test before offering. Mic/rich/pen by plane tag;
            // 0xC8 through `classify`.
            let mask = grants.load(Ordering::Relaxed);
            if let Some((seq, pts, opus)) = punktfunk_core::quic::decode_mic_datagram(&d) {
                // Dropping here is the setup gate: forwarding is the only attach this plane has.
                if !denied.permitted(mask, GrantClass::Mic) {
                    continue;
                }
                n.input_mic.fetch_add(1, Ordering::Relaxed);
                // Bounded `try_send`: never block this loop. seq + pts ride for de-jitter.
                let _ = mic_tx.try_send(crate::audio::MicFrame {
                    source: mic_source,
                    seq,
                    pts_ns: pts,
                    opus: opus.to_vec(),
                });
            } else if let Some(rich) = punktfunk_core::quic::RichInput::decode(&d) {
                if !denied.permitted(mask, GrantClass::Gamepad) {
                    continue;
                }
                n.input_rich.fetch_add(1, Ordering::Relaxed);
                if !offer(&input_tx, ClientInput::Rich(rich)) {
                    break;
                }
            } else if let Some(pen) = punktfunk_core::quic::PenBatch::decode(&d) {
                // 0xCC kind 0x05 stylus (`RichInput::decode` returns None). Same input thread.
                if !denied.permitted(mask, GrantClass::Pointer) {
                    continue;
                }
                n.input_rich.fetch_add(1, Ordering::Relaxed);
                if !offer(&input_tx, ClientInput::Pen(pen)) {
                    break;
                }
            } else if let Some(mut ev) = InputEvent::decode(&d) {
                if !denied.permitted(mask, classify(ev.kind)) {
                    continue;
                }
                n.input_events.fetch_add(1, Ordering::Relaxed);
                if matches!(ev.kind, InputKind::MouseMove | InputKind::MouseMoveAbs) {
                    n.pointer_moves.fetch_add(1, Ordering::Relaxed);
                }
                // KEY_FLAG_SEMANTIC_VK is in-process (GameStream ingest). Strip it from the wire.
                if matches!(
                    ev.kind,
                    punktfunk_core::input::InputKind::KeyDown
                        | punktfunk_core::input::InputKind::KeyUp
                ) {
                    ev.flags &= !crate::inject::KEY_FLAG_SEMANTIC_VK;
                }
                if !offer(&input_tx, ClientInput::Event(ev)) {
                    break;
                }
            }
        }
        tracing::info!(
            input = n.input_events.load(Ordering::Relaxed),
            mic = n.input_mic.load(Ordering::Relaxed),
            rich = n.input_rich.load(Ordering::Relaxed),
            dropped = n.input_dropped.load(Ordering::Relaxed),
            denied = denied.summary().as_deref().unwrap_or("none"),
            "client datagram stream ended"
        );
    });
}

/// Input thread → control task: what the client hears about its pads on the reliable stream.
pub(super) enum PadToClient {
    Slots(punktfunk_core::quic::PadSlots),
    /// A feature report for the physical pad. Only toward a client with `FEATURE_PAD_WRITES`.
    Feature(punktfunk_core::quic::PadFeature),
}

/// What [`input_thread`] holds between wakes, with one method per concern.
struct InputLoop {
    conn: super::link::SessionLink,
    pad_audio_on: bool,
    pad_slots: Arc<std::sync::atomic::AtomicU16>,
    pad_tx: Option<tokio::sync::mpsc::UnboundedSender<PadToClient>>,
    pad_writes: bool,
    grants: Arc<AtomicU32>,
    /// The grant mask as of the last [`Self::on_grants`].
    granted: u32,
    frame_map: FrameMap,
    pad_feed: Arc<crate::pad_feed::PadFeed>,
    counters: Arc<crate::session_status::SessionCounters>,
    pads: Pads,
    /// 0xD1 streamers; `pad_audio_on` is the negotiated Welcome cap.
    pad_streams: PadAudioSlots,
    /// Per-pad motion cadence, always on. Summarized at `info` on session end.
    motion_cadence: super::motion_cadence::MotionCadence,
    pad_uplink: super::pad_uplink::PadUplink,
    wire: WirePads,
    rumble: RumbleLeases,
    /// Its route is the injector every pointer and key goes to.
    held: Held,
    pen: PenSession,
}

impl InputLoop {
    /// A grant withdrawn mid-session filters that class's releases from here on: let go of
    /// what it holds now, or a key, button or stick stays down for the session. A regained
    /// gamepad grant opens the seq gate. The held set first follows a moved route.
    fn on_grants(&mut self) {
        use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_KEYBOARD, GRANT_POINTER};
        let now_granted = self.grants.load(Ordering::Relaxed);
        let lost = self.granted & !now_granted;
        let gained = now_granted & !self.granted;
        self.granted = now_granted;
        if gained & GRANT_GAMEPAD != 0 {
            self.wire.forget_seqs();
        }
        self.held.follow_route();
        if lost == 0 {
            return;
        }
        let ups = self
            .held
            .held
            .release_classes(lost & GRANT_KEYBOARD != 0, lost & GRANT_POINTER != 0);
        for ev in ups {
            let _ = self.held.route.send(ev);
        }
        if lost & GRANT_GAMEPAD != 0 {
            for idx in self.wire.rest_all() {
                self.pads.apply_wire(&self.wire, idx, &self.pad_feed);
            }
        }
    }

    /// A console just opened the Controllers page. A held button sends no further frames, so
    /// every live pad is re-published, or the page draws nothing until the next press. One
    /// relaxed load per wake when the page is closed.
    fn resync_feed(&self) {
        if self.pad_feed.take_resync() {
            for idx in self.wire.attached() {
                self.pad_feed
                    .publish(|| self.pads.feed_frame(idx, &self.wire));
            }
        }
    }

    /// [`Pads::feedback_poll_interval`], at most 100 ms while the pen is in range so
    /// [`PenSession::check_timeout`] meets its 200 ms deadline.
    fn poll_interval(&self) -> std::time::Duration {
        let poll = self.pads.feedback_poll_interval();
        if self.pen.active() {
            poll.min(std::time::Duration::from_millis(100))
        } else {
            poll
        }
    }

    /// The uplink's window line, every wake and before the wake's input is counted.
    fn log_uplink_window(&mut self) {
        for w in self.pad_uplink.take_window(std::time::Instant::now()) {
            w.log();
        }
    }

    /// One item off the datagram reader. A person acting drops a standing suspend veto before
    /// the grant tests: a denied event still means someone is here. The guards repeat
    /// dispatch's grant test: an arm that creates a virtual pad, tablet or pad-audio streamer
    /// stays unreachable if an upstream filter regresses. A missed grant drops the item.
    fn on_input(&mut self, input: ClientInput) {
        use punktfunk_core::quic::{GRANT_GAMEPAD, GRANT_POINTER};
        if is_activity(&input) {
            crate::sleep_inhibit::note_input();
        }
        let mask = self.grants.load(Ordering::Relaxed);
        match input {
            ClientInput::Rich(rich) if mask & GRANT_GAMEPAD != 0 => self.on_rich(rich),
            ClientInput::PadIdentity(id) if mask & GRANT_GAMEPAD != 0 => {
                self.pads.set_identity(&id)
            }
            ClientInput::Pen(batch) if mask & GRANT_POINTER != 0 => self.pen.apply(&reframe_pen(
                &batch,
                &self.frame_map.lock().unwrap_or_else(|e| e.into_inner()),
            )),
            ClientInput::Event(ev) if mask & punktfunk_core::quic::classify(ev.kind).bit() != 0 => {
                self.on_event(ev)
            }
            _ => {}
        }
    }

    /// A raw report builds its Steam Controller ([`Pads::raw_builds`]) before it lands; any
    /// other rich input only reaches a device that exists.
    fn on_rich(&mut self, rich: punktfunk_core::quic::RichInput) {
        use punktfunk_core::quic::RichInput;
        match rich {
            RichInput::Motion { pad, .. } => {
                let now = std::time::Instant::now();
                self.counters
                    .note_motion(self.motion_cadence.record(pad, now));
            }
            RichInput::HidReport { pad, .. } if self.pads.raw_builds(usize::from(pad)) => {
                let idx = usize::from(pad);
                self.wire.attach_at_rest(idx);
                self.pads.apply_wire(&self.wire, idx, &self.pad_feed);
            }
            _ => {}
        }
        self.pads.apply_rich(rich);
    }

    fn on_event(&mut self, ev: InputEvent) {
        match ev.kind {
            InputKind::GamepadButton | InputKind::GamepadAxis => {
                // Bad index / unknown axis: nothing to apply, and the tick still runs: the
                // DualSense GET_REPORT handshake is answered there.
                if let Some(idx) = self.wire.fold(&ev) {
                    self.pads.apply_wire(&self.wire, idx, &self.pad_feed);
                }
            }
            InputKind::GamepadState => self.on_snapshot(&ev),
            InputKind::GamepadRemove => self.on_remove(&ev),
            InputKind::GamepadArrival => declare_pad(
                &mut self.pads,
                &mut self.pad_streams,
                &self.conn,
                &ev,
                self.pad_audio_on,
            ),
            _ => self.inject(ev),
        }
    }

    /// A pad's whole state. The uplink counts it as it arrived; it holds off the suspend veto
    /// only when it moves the pad ([`WirePads::changes`]).
    fn on_snapshot(&mut self, ev: &InputEvent) {
        let Some(snap) = punktfunk_core::input::GamepadSnapshot::from_event(ev) else {
            return;
        };
        let now = std::time::Instant::now();
        if let Some(g) = self.pad_uplink.note(snap.pad as usize, snap.seq, now) {
            tracing::warn!(
                pad = g.pad,
                silence_ms = g.silence_ms,
                lost = g.lost,
                "controller updates stopped reaching the host — lost: updates the client sent \
                 that never arrived. lost near silence_ms / 100 = the path dropped them; 0 = the \
                 client sent none"
            );
        }
        if self.wire.changes(&snap) {
            crate::sleep_inhibit::note_input();
        }
        let built = self
            .pads
            .owner
            .get(snap.pad as usize)
            .is_some_and(Option::is_some);
        if let Some(idx) = self.wire.snapshot(snap, built) {
            self.pad_uplink.note_change(idx);
            self.pads.apply_wire(&self.wire, idx, &self.pad_feed);
        }
    }

    /// A hot-unplug. Its device, rumble lease and streamer go with it; the seq gate keeps a
    /// stale removal off a re-plugged pad.
    fn on_remove(&mut self, ev: &InputEvent) {
        let (pad, seq) = punktfunk_core::input::decode_gamepad_remove(ev.flags);
        let idx = pad as usize;
        let Some(attached) = self.wire.remove(pad, seq) else {
            return;
        };
        self.pads.raw_armed[idx] = false;
        self.pad_uplink.forget(idx);
        if attached {
            self.pads.apply_wire(&self.wire, idx, &self.pad_feed);
            tracing::info!(pad = idx, "gamepad unplugged (native detach)");
        } else {
            self.pads.release_unbuilt(idx);
        }
        self.rumble.clear(idx);
        self.pad_streams.stop(idx);
    }

    /// Pointer and keyboard, noted in the held set first so a mid-press disconnect is undone.
    /// The framing is the first screen's encoder's; the others are not cropped. A send error
    /// means the injector is gone, and input is lossy.
    fn inject(&mut self, mut ev: InputEvent) {
        self.held.held.note(&ev);
        if ev.screen() == 0 {
            reframe_input(
                &mut ev,
                &self.frame_map.lock().unwrap_or_else(|e| e.into_inner()),
            );
        }
        let _ = self.held.route.send(ev);
    }

    /// Every wake, after its input: the pen's timeout, the feedback pump, the heartbeat, the
    /// slot report and the rumble leases.
    fn on_tick(&mut self) {
        self.pen.check_timeout();
        self.pump_feedback();
        // Held-steady UHID pads send no wire events; heartbeat re-emits. Xbox is a no-op.
        self.pads.heartbeat();
        self.report_slots();
        let conn = &self.conn;
        self.rumble.tick(std::time::Instant::now(), |d| {
            conn.send_datagram(d);
        });
    }

    /// Every wake (≤1 ms Triton, ≤4 ms else): games block on EVIOCSFF and HID GET_REPORT.
    /// Rumble is 0xCA; rich HID-out is 0xCD, and a feature report goes to the control task
    /// for a client that reads them there.
    fn pump_feedback(&mut self) {
        let (conn, rumble) = (&self.conn, &mut self.rumble);
        let (pad_tx, pad_writes) = (&self.pad_tx, self.pad_writes);
        self.pads.pump(
            |pad, low, high, lt, rt| {
                conn.send_datagram(rumble.on_level(pad, (low, high, lt, rt)));
            },
            |h| match (h, pad_tx) {
                (punktfunk_core::quic::HidOutput::HidRaw { pad, kind, data }, Some(tx))
                    if pad_writes && kind == punktfunk_core::quic::HID_RAW_FEATURE =>
                {
                    let _ = tx.send(PadToClient::Feature(punktfunk_core::quic::PadFeature {
                        pad,
                        data,
                    }));
                }
                (h, _) => {
                    conn.send_datagram(h.encode());
                }
            },
        );
    }

    /// After the pump reaped the unplug grace, so a re-plug inside it is not reported as a pad
    /// leaving and coming back. Silent while the slots stand.
    fn report_slots(&mut self) {
        if let Some(mask) = self.pads.take_slot_change() {
            self.pad_slots.store(mask, Ordering::Relaxed);
            if let Some(tx) = &self.pad_tx {
                let slots = punktfunk_core::quic::PadSlots { slots: mask };
                let _ = tx.send(PadToClient::Slots(slots));
            }
        }
    }

    /// Session end. Ink lifts (buttons → tip → proximity) and the held set lets go on the
    /// injector, which outlives the session: a client that vanishes mid-press is undone here.
    /// The pads and their OS slots go before the streamer join, which can sit ~5 s on a quiet
    /// endpoint, so a session that preempted this one claims them inside its 1.5 s grace.
    fn finish(self) {
        let InputLoop {
            mut pen,
            mut held,
            pads,
            mut pad_streams,
            motion_cadence,
            mut pad_uplink,
            ..
        } = self;
        pen.release_all();
        let ups = held.held.release();
        if !ups.is_empty() {
            tracing::debug!(
                count = ups.len(),
                "input: releasing held buttons/keys/contacts at session end"
            );
        }
        for ev in ups {
            let _ = held.route.send(ev);
        }
        drop(pads);
        pad_streams.stop_all();
        // One line per motion pad, at `info`: the question is asked from a log after the fact.
        motion_cadence.log_summary();
        // A player stops a session soon after the pad dies: its last partial window is the
        // evidence.
        for w in pad_uplink.drain(std::time::Instant::now()) {
            w.log();
        }
    }
}

/// Per-session input thread. Pointer/keyboard go through [`InputRoute`]; gamepad through
/// [`Pads`] (Hello kind is the per-pad default), and every pad state that reaches `Pads` also
/// reaches `pad_feed`, which the console's Controllers page draws. Rich input applies on
/// arrival; rumble and HID-output pump between events. Gamepads die with the session; the
/// pointer/keyboard injector (and its portal grant) outlives it.
///
/// Ends on `stop` or when `rx` disconnects ([`InputLoop::finish`]). `pad_id` names the device
/// its pads belong to and the player slot the operator picked for it; `pad_slots` and `pad_tx`
/// publish the slots it ends up with, to `/status` and to the client's overlay.
#[allow(clippy::too_many_arguments)]
pub(super) fn input_thread(
    rx: std::sync::mpsc::Receiver<ClientInput>,
    conn: super::link::SessionLink,
    inj_tx: InputRoute,
    gamepad: GamepadPref,
    pad_audio_on: bool,
    pad_id: crate::inject::pad_pool::PadIdentity,
    pad_slots: Arc<std::sync::atomic::AtomicU16>,
    pad_tx: Option<tokio::sync::mpsc::UnboundedSender<PadToClient>>,
    // The client reads feature reports off the control stream (`FEATURE_PAD_WRITES`).
    pad_writes: bool,
    // Live grant mask. Dispatch already drops non-granted traffic; `InputLoop::on_input`
    // tests it again, deny-at-setup: without `GRANT_GAMEPAD` no arm that could create a
    // virtual pad or pad-audio streamer runs. One relaxed load per item.
    grants: Arc<AtomicU32>,
    frame_map: FrameMap,
    // This session's Controllers-page tap. Every accepted pad state goes here as well
    // as to the backends, so the page shows what was injected. Idle with nobody watching.
    pad_feed: Arc<crate::pad_feed::PadFeed>,
    stop: Arc<AtomicBool>,
    // Session gyro totals. This thread is joined after the summary is built, so the
    // per-pad `MotionCadence` histogram cannot be what the summary reads.
    counters: Arc<crate::session_status::SessionCounters>,
    knobs: super::SessionKnobs,
) {
    let mut lp = InputLoop {
        pads: Pads::new(gamepad, pad_id, knobs.steam_force),
        pad_streams: PadAudioSlots::new(),
        motion_cadence: super::motion_cadence::MotionCadence::new(),
        pad_uplink: super::pad_uplink::PadUplink::new(std::time::Instant::now()),
        wire: WirePads::default(),
        rumble: RumbleLeases::with_ttl(knobs.rumble_ttl_ms),
        held: Held {
            held: crate::inject::held::HeldInput::default(),
            route: inj_tx,
        },
        pen: PenSession::new(),
        granted: grants.load(Ordering::Relaxed),
        conn,
        pad_audio_on,
        pad_slots,
        pad_tx,
        pad_writes,
        grants,
        frame_map,
        pad_feed,
        counters,
    };
    // A reconnect or steal sets `stop` while this connection is still open and
    // claims this session's OS slots 1.5 s later; the channel outlives that.
    while !stop.load(Ordering::SeqCst) {
        lp.on_grants();
        lp.resync_feed();
        let arrived = rx.recv_timeout(lp.poll_interval());
        lp.log_uplink_window();
        match arrived {
            Ok(input) => lp.on_input(input),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        lp.on_tick();
    }
    lp.finish();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::pad_pool::PadIdentity;
    use punktfunk_core::input::{InputEvent, InputKind};

    #[test]
    fn a_cropped_joiner_points_into_the_owner_picture() {
        use punktfunk_core::video_fit::{Reframe, VideoFit};
        let map = Reframe::plan(VideoFit::Crop, (3840, 2160), (2400, 1080));
        let mut ev = InputEvent {
            kind: InputKind::TouchDown,
            _pad: [0; 3],
            code: 1,
            x: 2400,
            y: 1080,
            flags: (2400 << 16) | 1080,
        };
        reframe_input(&mut ev, &map);
        assert_eq!((ev.x, ev.y, ev.flags), (3840, 1944, (3840 << 16) | 2160));
        // Keys and an unframed session pass through.
        let key = InputEvent {
            kind: InputKind::KeyDown,
            ..ev
        };
        let mut k = key;
        reframe_input(&mut k, &map);
        assert_eq!(k, key);
        let mut e = ev;
        reframe_input(&mut e, &Reframe::default());
        assert_eq!(e, ev);

        let pen = punktfunk_core::quic::PenSample {
            x: 0.5,
            y: 0.0,
            ..Default::default()
        };
        let out = reframe_pen(&punktfunk_core::quic::PenBatch::new(7, &[pen]), &map);
        assert_eq!(
            (out.seq, out.samples()[0].x, out.samples()[0].y),
            (7, 0.5, 0.1)
        );
    }

    /// A key held across a compositor switch is let go on the injector it went down on, and a
    /// thread that leaves, however it leaves, lets go of the rest.
    #[test]
    fn held_input_is_released_where_it_was_pressed() {
        let key = |kind| InputEvent {
            kind,
            _pad: [0; 3],
            code: 30,
            x: 0,
            y: 0,
            flags: 0,
        };
        let (old_tx, old_rx) = std::sync::mpsc::channel::<InputEvent>();
        let (new_tx, new_rx) = std::sync::mpsc::channel::<InputEvent>();
        let route = InputRoute::new(old_tx);
        let mut held = Held {
            held: crate::inject::held::HeldInput::default(),
            route: route.clone(),
        };
        held.held.note(&key(InputKind::KeyDown));
        route.set(new_tx);
        held.follow_route();
        assert_eq!(old_rx.try_recv().map(|e| e.kind), Ok(InputKind::KeyUp));
        held.held.note(&key(InputKind::KeyDown));
        drop(held);
        assert_eq!(
            new_rx.try_recv().map(|e| e.kind),
            Ok(InputKind::KeyUp),
            "released on drop"
        );
    }

    /// Mid-stream compositor switch: later events land on the new target.
    #[test]
    fn input_route_swaps_targets_mid_flight() {
        let ev = InputEvent {
            kind: InputKind::MouseMove,
            _pad: [0; 3],
            code: 0,
            x: 1,
            y: 2,
            flags: 0,
        };
        let (shared_tx, shared_rx) = std::sync::mpsc::channel::<InputEvent>();
        let (pinned_tx, pinned_rx) = std::sync::mpsc::channel::<InputEvent>();
        let route = InputRoute::new(shared_tx);
        route.send(ev).unwrap();
        assert_eq!(shared_rx.try_recv().unwrap().x, 1);
        route.set(pinned_tx);
        route.send(ev).unwrap();
        assert!(shared_rx.try_recv().is_err(), "old target no longer fed");
        assert_eq!(pinned_rx.try_recv().unwrap().y, 2);
    }

    /// Two pads whose OS slots were claimed out of wire order — the pad that moves
    /// first takes the lower slot whatever the client numbered it. Untranslated, one
    /// pad's raw reports drive the other's device and Steam answers the touch with
    /// rumble on the wrong controller.
    #[test]
    fn rich_input_is_re_addressed_into_slot_space() {
        use punktfunk_core::quic::{RichInput, HID_REPORT_MAX};
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        let slot1 = pads.slots.claim_for(1).expect("a free OS slot");
        let slot0 = pads.slots.claim_for(0).expect("a free OS slot");
        assert_ne!(slot0, slot1, "two wire pads share an OS slot");

        let report = |pad| RichInput::HidReport {
            pad,
            len: 1,
            data: [0x42; HID_REPORT_MAX],
        };
        assert_eq!(
            pads.rich_in_slot_space(report(1)).map(|r| r.pad()),
            Some(slot1)
        );
        assert_eq!(
            pads.rich_in_slot_space(report(0)).map(|r| r.pad()),
            Some(slot0)
        );
        // No slot = no device. Dropped, never folded onto slot 0.
        assert!(pads.rich_in_slot_space(report(2)).is_none());
    }

    /// A Steam Controller of either generation is built once, as itself: it waits for the
    /// identity its client sends, and nothing else waits.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_steam_controller_waits_for_its_identity() {
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        // Set the resolved kind directly: a box without `/dev/uhid` folds an SC2 to Xbox 360.
        pads.kinds[1] = GamepadPref::SteamController2;
        pads.kinds[4] = GamepadPref::SteamController;
        for idx in [1, 2, 4] {
            pads.identity_since[idx] = Some(std::time::Instant::now());
        }
        assert!(pads.waits_for_identity(1));
        assert!(pads.waits_for_identity(4));
        assert!(!pads.waits_for_identity(2), "an Xbox pad never waits");
        pads.set_identity(&punktfunk_core::quic::PadIdentity {
            pad: 1,
            serial: "FXA0000000001".into(),
            replies: Vec::new(),
            slot: 0,
        });
        assert!(!pads.waits_for_identity(1));
        pads.kinds[3] = GamepadPref::SteamController2;
        pads.identity_since[3] = Some(std::time::Instant::now() - IDENTITY_WAIT);
        assert!(!pads.waits_for_identity(3), "the wait ends");
    }

    /// A declared Steam Controller with no device is built by its raw reports, and a pad
    /// attached that way unplugs like any other. Nothing else is built from rich input.
    #[test]
    fn a_raw_report_builds_a_resting_steam_controller() {
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        // Set the resolved kinds directly: a box without the backend folds them to Xbox 360.
        pads.kinds = [GamepadPref::SteamController2; MAX_WIRE_PADS];
        pads.raw_armed[0] = true;
        pads.kinds[2] = GamepadPref::DualSense;
        pads.raw_armed[2] = true;
        pads.kinds[3] = GamepadPref::SteamController;
        pads.raw_armed[3] = true;
        pads.owner[3] = Some(GamepadPref::SteamController);
        pads.set_kind(4, GamepadPref::Xbox360);
        assert!(pads.raw_armed[4], "a declaration arms the pad");
        assert!(pads.raw_builds(0));
        assert!(!pads.raw_builds(1), "undeclared, or removed since");
        assert!(!pads.raw_builds(2), "not a raw pad");
        assert!(!pads.raw_builds(3), "already built");
        assert!(!pads.raw_builds(MAX_WIRE_PADS));

        let mut wire = WirePads::default();
        wire.attach_at_rest(0);
        assert!(wire.frame(0).active_mask & 1 != 0);
        assert_eq!(wire.remove(0, 1), Some(true), "its removal unplugs it");
        assert!(wire.frame(0).active_mask & 1 == 0);
    }

    /// Pad audio and the Controllers page follow the device: a pad already built as Xbox 360
    /// that later declares a DualSense stays an Xbox 360 pad, with no speaker to stream to.
    #[test]
    fn a_pad_is_the_device_it_was_built_as() {
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        pads.owner[0] = Some(GamepadPref::Xbox360);
        pads.set_kind(0, GamepadPref::DualSense);
        assert_eq!(pads.device_kind(0), Some(GamepadPref::Xbox360));
        pads.set_kind(1, GamepadPref::Xbox360);
        assert_eq!(
            pads.device_kind(1),
            Some(GamepadPref::Xbox360),
            "unbuilt: as resolved"
        );
        assert_eq!(pads.device_kind(MAX_WIRE_PADS), None);
    }

    /// The thread polls as fast as its live devices need, not as its managers ever did: a
    /// Steam Controller that left takes its 1 kHz with it, and no device means no feedback.
    #[test]
    fn the_input_thread_polls_for_the_pads_it_has() {
        use std::time::Duration;
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        assert_eq!(pads.feedback_poll_interval(), Duration::from_millis(25));
        pads.owner[0] = Some(GamepadPref::SteamController2);
        assert_eq!(pads.feedback_poll_interval(), Duration::from_millis(1));
        pads.owner[0] = None;
        pads.pending_release[0] = Some(std::time::Instant::now());
        assert_eq!(
            pads.feedback_poll_interval(),
            Duration::from_millis(4),
            "the grace"
        );
        pads.pending_release[0] = None;
        pads.owner[1] = Some(GamepadPref::Xbox360);
        assert_eq!(pads.feedback_poll_interval(), Duration::from_millis(4));
    }

    /// A session whose default is a Steam Controller still waits for each pad's identity:
    /// the first declaration starts the clock, a re-declare keeps it.
    #[test]
    fn a_steam_controller_default_still_waits_for_its_identity() {
        use GamepadPref::{SteamController2 as Sc2, Xbox360};
        let (t0, t1) = (std::time::Instant::now(), std::time::Instant::now());
        assert_eq!(identity_wait_after(Sc2, Sc2, true, None, t1), Some(t1));
        assert_eq!(
            identity_wait_after(Sc2, Sc2, false, Some(t0), t1),
            Some(t0),
            "re-declare"
        );
        assert_eq!(identity_wait_after(Xbox360, Sc2, false, None, t1), Some(t1));
        assert_eq!(identity_wait_after(Sc2, Xbox360, true, Some(t0), t1), None);
    }

    /// A frame that beats its pad's arrival does not build the session default: the pad waits
    /// for its kind, and one that never declares is built once the wait ends.
    #[test]
    fn an_undeclared_pad_waits_for_its_arrival() {
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        assert!(pads.waits_for_arrival(0));
        pads.set_kind(0, GamepadPref::SteamController2Puck);
        assert!(!pads.waits_for_arrival(0), "the kind landed");
        pads.undeclared_since[1] = Some(std::time::Instant::now() - ARRIVAL_WAIT);
        assert!(!pads.waits_for_arrival(1), "the wait ends");
        pads.owner[2] = Some(GamepadPref::Xbox360);
        assert!(!pads.waits_for_arrival(2), "a built pad never waits");
    }

    /// The console and the client are told which players this session is, once per
    /// change. Resent every frame it would be a control message per pad packet.
    #[test]
    fn the_slot_map_is_reported_only_when_it_moves() {
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        assert_eq!(pads.take_slot_change(), None, "no pad, nothing to say");
        let slot = pads.claim_os_slot(0).expect("a free OS slot");
        assert_eq!(pads.take_slot_change(), Some(1 << slot));
        assert_eq!(pads.take_slot_change(), None, "the same slots, again");
        pads.slots.release(0);
        assert_eq!(pads.take_slot_change(), Some(0), "the pad left");
    }

    /// A pad-audio streamer starts at the arrival and captures the endpoint / card / sink
    /// named by the pad's OS slot, so the arrival must reserve the same slot the pad's
    /// first frame would have taken — never a second name for one pad.
    #[test]
    fn an_arrival_reserves_the_slot_the_first_frame_would_claim() {
        let mut pads = Pads::new(GamepadPref::DualSense, PadIdentity::anonymous(), false);
        let slot = pads.claim_os_slot(1).expect("a free OS slot");
        assert_eq!(pads.slots.claim_for(1), Some(slot), "first frame re-mints");
        assert_eq!(pads.claim_os_slot(1), Some(slot), "re-declare re-mints");
    }

    /// A reconnect preempts its zombie by setting `stop` and claims the same OS slots
    /// 1.5 s later, while the zombie's connection (and so its input channel) is still
    /// open. The thread must end on the flag alone; `PadSlotMap`'s drop frees the slots.
    #[tokio::test]
    async fn a_stopped_session_ends_its_input_thread_while_its_link_is_open() {
        use punktfunk_core::quic::endpoint;
        let server = endpoint::server("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            let conn = server
                .accept()
                .await
                .expect("incoming")
                .await
                .expect("host side connects");
            (server, conn)
        });
        let client = endpoint::client_insecure().unwrap();
        let client_conn = client.connect(addr, "punktfunk").unwrap().await.unwrap();
        let (_server, host_conn) = accept.await.unwrap();

        // Held open for the whole test: `Disconnected` must not be what ends the thread.
        let (input_tx, input_rx) = std::sync::mpsc::sync_channel::<ClientInput>(8);
        let (inj_tx, _inj_rx) = std::sync::mpsc::channel::<InputEvent>();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                input_thread(
                    input_rx,
                    super::super::link::SessionLink::QuicV2(
                        host_conn.clone(),
                        Arc::new(super::super::link::V2Link::new(
                            host_conn,
                            Arc::new(std::net::UdpSocket::bind("127.0.0.1:0").unwrap()),
                        )),
                    ),
                    InputRoute::new(inj_tx),
                    GamepadPref::Xbox360,
                    false,
                    PadIdentity::anonymous(),
                    Arc::new(std::sync::atomic::AtomicU16::new(0)),
                    None,
                    false,
                    Arc::new(AtomicU32::new(0)),
                    Arc::new(std::sync::Mutex::new(
                        punktfunk_core::video_fit::Reframe::default(),
                    )),
                    Arc::new(crate::pad_feed::PadFeed::new()),
                    stop,
                    Arc::new(crate::session_status::SessionCounters::default()),
                    crate::native::SessionKnobs::read(),
                )
            })
        };
        stop.store(true, Ordering::SeqCst);
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::task::spawn_blocking(move || thread.join()),
        )
        .await;
        assert!(
            joined.is_ok(),
            "input thread still running 2 s after stop with its channel open"
        );
        drop(input_tx);
        drop(client_conn);
    }

    /// The device lingers 300 ms after its removal frame; the pool slot lingers with it, so a
    /// re-plug inside the grace lands on the same slot and no other claimant takes it.
    #[test]
    fn a_removed_pad_keeps_its_slot_through_the_unplug_grace() {
        use std::time::{Duration, Instant};
        let t = Instant::now();
        let mut pads = Pads::new(GamepadPref::Xbox360, PadIdentity::anonymous(), false);
        let slot = pads.slots.claim_for(0).expect("a free OS slot");
        pads.owner[0] = Some(GamepadPref::Xbox360);
        pads.note_removed(0, true, t);
        pads.reap_pending_at(t + Pads::RELEASE_GRACE - Duration::from_millis(1));
        assert_eq!(
            pads.slots.slot_of(0),
            Some(slot),
            "released inside the grace"
        );
        // A re-plug inside the grace: the frame re-mints the memoized slot and clears the clock.
        assert_eq!(pads.slots.claim_for(0), Some(slot));
        pads.pending_release[0] = None;
        pads.reap_pending_at(t + Duration::from_secs(5));
        assert_eq!(
            pads.slots.slot_of(0),
            Some(slot),
            "a re-plugged pad lost its slot"
        );
        // No re-plug: the slot follows the device out once the grace has run.
        pads.note_removed(0, true, t);
        pads.reap_pending_at(t + Pads::RELEASE_GRACE);
        assert_eq!(pads.slots.slot_of(0), None, "held past the grace");
        // A pad that never built a device has nothing to wait for.
        pads.slots.claim_for(1).expect("a free OS slot");
        pads.release_unbuilt(1);
        assert_eq!(pads.slots.slot_of(1), None);
        // The client re-sends a removal: the second one finds no device attached, but the
        // first one's device still lingers in its grace.
        let slot = pads.slots.claim_for(2).expect("a free OS slot");
        pads.owner[2] = None;
        pads.note_removed(2, true, t);
        pads.release_unbuilt(2);
        assert_eq!(
            pads.slots.slot_of(2),
            Some(slot),
            "a re-sent removal freed the grace"
        );
    }

    /// Incremental events fold in, a newer snapshot replaces the pad, and a removal
    /// shares the snapshot seq gate so a reordered packet can't roll state back.
    #[test]
    fn wire_pads_fold_replace_and_seq_gate() {
        use punktfunk_core::input::{gamepad, GamepadSnapshot};
        let mut wire = WirePads::default();
        let axis = InputEvent {
            kind: InputKind::GamepadAxis,
            _pad: [0; 3],
            code: gamepad::AXIS_LT,
            x: 200,
            y: 0,
            flags: 1,
        };
        assert_eq!(wire.fold(&axis), Some(1));
        assert_eq!((wire.frame(1).left_trigger, wire.mask), (200, 0b10));
        let unknown = InputEvent { code: 42, ..axis };
        assert_eq!(wire.fold(&unknown), None, "unknown axis");
        assert_eq!(
            wire.fold(&InputEvent { flags: 16, ..axis }),
            None,
            "bad index"
        );

        let snap = GamepadSnapshot {
            pad: 1,
            seq: 1,
            buttons: gamepad::BTN_A,
            left_trigger: 255,
            ls_x: 100,
            ls_y: -100,
            ..Default::default()
        };
        assert_eq!(wire.snapshot(snap, true), Some(1));
        let f = wire.frame(1);
        assert_eq!(
            (f.index, f.buttons, f.left_trigger),
            (1, gamepad::BTN_A, 255)
        );
        assert_eq!((f.ls_x, f.ls_y), (100, -100));

        let stale = GamepadSnapshot {
            seq: 0,
            left_trigger: 10,
            ..snap
        };
        assert_eq!(
            wire.snapshot(stale, true),
            None,
            "a reorder rolled state back"
        );
        assert_eq!(wire.frame(1).left_trigger, 255);
        // An unchanged refresh advances the gate but emits nothing.
        assert_eq!(
            wire.snapshot(GamepadSnapshot { seq: 2, ..snap }, true),
            None
        );
        assert_eq!(
            wire.snapshot(GamepadSnapshot { seq: 2, ..stale }, true),
            None
        );

        assert_eq!(wire.remove(1, 2), None, "a removal older than the gate");
        assert_eq!(wire.remove(1, 3), Some(true));
        assert_eq!((wire.mask, wire.frame(1).buttons), (0, 0));
        assert_eq!(wire.remove(1, 4), Some(false), "nothing attached");
        // The first snapshot after a re-plug emits even when it matches the cleared state.
        let replug = GamepadSnapshot {
            pad: 1,
            seq: 5,
            ..Default::default()
        };
        assert_eq!(wire.snapshot(replug, true), Some(1));
        assert_eq!(wire.attached().collect::<Vec<_>>(), [1]);
    }

    /// The client's seq moved on while the gamepad grant was off; once it is back, the
    /// next snapshot lands wherever that seq now is.
    #[test]
    fn a_regained_grant_opens_the_seq_gate() {
        use punktfunk_core::input::GamepadSnapshot;
        let mut wire = WirePads::default();
        let beacon = |seq| GamepadSnapshot {
            pad: 0,
            seq,
            ..Default::default()
        };
        assert_eq!(wire.snapshot(beacon(1), true), Some(0));
        // 20 s of 10 Hz refreshes the host never saw.
        let later = GamepadSnapshot {
            buttons: 1,
            ..beacon(1u8.wrapping_add(200))
        };
        assert_eq!(
            wire.snapshot(later, true),
            None,
            "read as older than the gate"
        );
        wire.forget_seqs();
        assert_eq!(wire.snapshot(later, true), Some(0));
    }

    /// A parked controller's beacon and gyro are not a person: only input that moves
    /// something holds off the suspend veto.
    #[test]
    fn only_a_person_acting_holds_off_the_suspend_veto() {
        use punktfunk_core::input::GamepadSnapshot;
        let ev = |kind| {
            ClientInput::Event(InputEvent {
                kind,
                _pad: [0; 3],
                code: 0,
                x: 0,
                y: 0,
                flags: 0,
            })
        };
        assert!(is_activity(&ev(InputKind::KeyDown)));
        assert!(is_activity(&ev(InputKind::MouseMove)));
        assert!(!is_activity(&ev(InputKind::GamepadState)), "the beacon");
        let motion = punktfunk_core::quic::RichInput::Motion {
            pad: 0,
            gyro: [0; 3],
            accel: [0; 3],
        };
        assert!(!is_activity(&ClientInput::Rich(motion)), "gyro on a table");

        let mut wire = WirePads::default();
        let beacon = |seq| GamepadSnapshot {
            pad: 0,
            seq,
            ..Default::default()
        };
        assert!(wire.changes(&beacon(1)), "a pad's first state");
        wire.snapshot(beacon(1), true);
        assert!(!wire.changes(&beacon(2)), "the same state again");
        let pressed = GamepadSnapshot {
            buttons: 1,
            ..beacon(3)
        };
        assert!(wire.changes(&pressed));
    }

    /// A pad whose build was deferred (its arrival or identity late, no slot free) is handed
    /// to the backends again on the next unchanged beacon; a built one is not.
    #[test]
    fn an_unbuilt_pad_is_retried_on_an_unchanged_beacon() {
        use punktfunk_core::input::GamepadSnapshot;
        let mut wire = WirePads::default();
        let beacon = |seq| GamepadSnapshot {
            pad: 0,
            seq,
            ..Default::default()
        };
        assert_eq!(wire.snapshot(beacon(1), false), Some(0));
        assert_eq!(
            wire.snapshot(beacon(2), false),
            Some(0),
            "the deferred pad is retried"
        );
        assert_eq!(
            wire.snapshot(beacon(3), true),
            None,
            "a built pad's refresh churns nothing"
        );
    }

    /// The client's gate: accepts a seq newer than the last it applied.
    fn deliver(d: &[u8], gate: &mut Option<u8>) -> bool {
        let env = punktfunk_core::quic::decode_rumble_envelope(d)
            .expect("rumble decodes")
            .envelope
            .expect("envelope tail present");
        let fresh = punktfunk_core::input::GamepadSnapshot::seq_newer(env.seq, *gate);
        if fresh {
            *gate = Some(env.seq);
        }
        fresh
    }

    /// A pad re-plug must not reset the rumble `seq`.
    ///
    /// The client's `rumble_last_seq` lives for the whole QUIC connection and has
    /// no reset (`client/pump/datagram_task.rs`). Resetting the host counter on
    /// `GamepadRemove` strands every later envelope until it climbs past the
    /// stored value (up to 128 sends).
    #[test]
    fn rumble_seq_survives_a_removal_so_the_client_gate_accepts() {
        let mut leases = RumbleLeases::new(RUMBLE_TTL_MS, std::time::Instant::now());
        let mut gate: Option<u8> = None;
        for i in 0..100u16 {
            assert!(deliver(
                &leases.on_level(0, (0x4000 + i, 0x8000, 0, 0)),
                &mut gate
            ));
        }
        assert_eq!(gate, Some(100));

        // Unplug mid-buzz: the lease is cleared, the counter is not.
        leases.clear(0);
        assert_eq!(
            (leases.lv[0], leases.seen[0], leases.stop_burst[0]),
            ((0, 0, 0, 0), false, 0)
        );
        assert!(
            deliver(&leases.on_level(0, (0x1234, 0, 0, 0)), &mut gate),
            "first envelope after a re-plug was dropped by the client's reorder gate"
        );

        // Non-vacuity: a counter restarted at 0 is rejected for the whole forward window.
        let mut restarted = RumbleLeases::new(RUMBLE_TTL_MS, std::time::Instant::now());
        let mut stranded = Some(100u8);
        assert!(
            (0..100u16).all(|i| !deliver(&restarted.on_level(0, (i + 1, 0, 0, 0)), &mut stranded)),
            "test is vacuous — a restarted counter should have been gated out"
        );
    }

    /// An active lease renews once per interval with a fresh TTL. A fall to zero sends
    /// its zero at once, then [`RUMBLE_STOP_BURST`] more on the next ticks, then goes quiet.
    #[test]
    fn a_stopped_rumble_sends_its_burst_then_goes_quiet() {
        use punktfunk_core::quic::decode_rumble_envelope;
        use std::time::Duration;
        let t0 = std::time::Instant::now();
        let mut leases = RumbleLeases::new(400, t0);
        assert_eq!(leases.every, Duration::from_millis(120));
        let tick = |leases: &mut RumbleLeases, at: Duration| {
            let mut out = Vec::new();
            leases.tick(t0 + at, |d| out.push(decode_rumble_envelope(&d).unwrap()));
            out
        };
        leases.on_level(3, (0x4000, 0, 0, 0));
        assert!(
            tick(&mut leases, Duration::from_millis(119)).is_empty(),
            "renewed early"
        );
        let renew = tick(&mut leases, Duration::from_millis(120));
        assert_eq!(renew.len(), 1);
        let env = renew[0].envelope.unwrap();
        assert_eq!(
            (renew[0].pad, renew[0].low, env.seq, env.ttl_ms),
            (3, 0x4000, 2, 400)
        );

        let stop = decode_rumble_envelope(&leases.on_level(3, (0, 0, 0, 0))).unwrap();
        assert_eq!(stop.envelope.unwrap().ttl_ms, 0);
        let mut zeros = 0;
        for n in 2..10u64 {
            for u in tick(&mut leases, Duration::from_millis(120 * n)) {
                assert_eq!((u.low, u.envelope.unwrap().ttl_ms), (0, 0));
                zeros += 1;
            }
        }
        assert_eq!(zeros, RUMBLE_STOP_BURST, "stop burst");
    }

    /// A rumble that drives only the impulse triggers must still get a live TTL
    /// (`design/trigger-rumble-plane.md`).
    ///
    /// `(low, high) == (0, 0)` as silence stamps trigger-only rumble `ttl = 0`;
    /// the client silences on arrival with no error.
    #[test]
    fn a_trigger_only_rumble_gets_a_live_ttl() {
        let mut leases = RumbleLeases::new(RUMBLE_TTL_MS, std::time::Instant::now());
        let d = leases.on_level(0, (0, 0, 0x8000, 0));
        let u = punktfunk_core::quic::decode_rumble_envelope(&d).expect("v3 envelope decodes");
        assert_eq!(
            u.envelope.expect("v3 carries the v2 tail").ttl_ms,
            RUMBLE_TTL_MS,
            "trigger-only rumble was stamped with a dead lease"
        );
        assert_eq!((u.left_trigger, u.right_trigger), (0x8000, 0));
        assert_eq!((u.low, u.high), (0, 0), "handles stay at rest");

        // All-zero is the only stop, and the only thing that gets ttl = 0.
        assert!(rumble_silent((0, 0, 0, 0)));
        for lv in [
            (1, 0, 0, 0),
            (0, 1, 0, 0),
            (0, 0, 1, 0),
            (0, 0, 0, 1),
            (0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF),
        ] {
            assert!(!rumble_silent(lv), "{lv:?} must not read as a stop");
        }
    }
}
