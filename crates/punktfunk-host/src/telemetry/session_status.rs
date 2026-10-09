//! Live native-session status for management `GET /status`.
//!
//! GameStream records its session in `AppState.{launch, stream, streaming}`.
//! The native plane never writes those fields — it is handed only the shared
//! stats recorder ([`crate::native::serve`]). This registry is the surface
//! the native video loop ([`crate::native::virtual_stream`]) publishes to,
//! keyed per session so concurrent sessions (up to `max_sessions`) each get
//! an entry.
//!
//! [`register`] on stream start; [`LiveSessionGuard`] removes the entry on
//! any scope exit. `/status` reads [`snapshot`]/[`count`]. The id-less Dashboard
//! stop and IDR reach every native session through [`stop_all_quit`] and
//! [`force_idr_all`]; the per-session routes take one id through [`stop_quit`],
//! [`force_idr`] and [`controls`].
//!
//! The same drop builds a [`crate::events::SessionSummary`] — the session's own
//! numbers plus why it ended — onto `session.ended` and into [`recent`], which is
//! what `GET /api/v1/session/last` serves. [`SessionCounters`] is the shared block
//! the input, audio and encode paths bump so the summary can read totals from
//! threads that outlive it.

use std::collections::VecDeque;
use std::sync::atomic::{
    AtomicBool, AtomicI64, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering,
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use crate::encode::{ChromaFormat, Codec};
use crate::events::{
    AudioEgress, BitrateSpan, GyroCadence, InputCounts, Plane, SessionEndReason, SessionSummary,
};
use punktfunk_core::quic::GrantClass;

mod abr_share;
mod audio_policy;
mod games;
pub use abr_share::{share_for, AbrShare};
pub use audio_policy::{apply_audio_policy, AudioSessions};
#[cfg(feature = "gamestream")]
pub use games::publish_gamestream_game;
pub use games::{games, live_games, waiting_launch, GameSnapshot, WaitingRow};

/// Pack `(w, h, hz)` into one atomic word (16|16|16) — one store, not three racy ones.
pub(crate) fn pack_mode(width: u32, height: u32, refresh_hz: u32) -> u64 {
    ((width as u64 & 0xffff) << 32)
        | ((height as u64 & 0xffff) << 16)
        | (refresh_hz as u64 & 0xffff)
}

/// [`pack_mode`]'s inverse.
pub(crate) fn unpack_mode(packed: u64) -> (u32, u32, u32) {
    (
        ((packed >> 32) & 0xffff) as u32,
        ((packed >> 16) & 0xffff) as u32,
        (packed & 0xffff) as u32,
    )
}

/// One live native session. The Arcs are the video loop's own handles, so a
/// mid-stream mode/bitrate change shows on `/status` with no second write.
struct LiveSession {
    id: u64,
    /// Packed `w:16|h:16|hz:16` ([`pack_mode`]); live on a mode switch.
    mode: Arc<AtomicU64>,
    /// Encoder target, kbps. Same Arc the ABR path writes.
    bitrate_kbps: Arc<AtomicU32>,
    codec: Codec,
    /// Teardown flag ([`stop_all`]).
    stop: Arc<AtomicBool>,
    /// Deliberate-stop flag ([`stop_all_quit`]). Distinct from `stop`: intended
    /// teardown skips display keep-alive linger and trips end-game-on-session-end.
    quit: Arc<AtomicBool>,
    /// One-shot force-keyframe ([`force_idr_all`]). The encode loop drains it
    /// alongside a client decode-recovery request.
    force_idr: Arc<AtomicBool>,
    /// Client label: 12-hex cert-fingerprint prefix, or peer IP if anonymous.
    client: String,
    /// Display name (trust-store, else sanitized Hello). `None` if nameless.
    client_name: Option<String>,
    /// Which plane serves it. Both register here, so a stop or a keyframe reaches either.
    plane: crate::events::Plane,
    hdr: bool,
    /// Bring-up total (hello → first packet), ms. 0 until the first packet left.
    ttff_ms: Arc<AtomicU32>,
    /// Last mid-stream resize (reconfigure → rebuilt), ms. 0 = none yet.
    last_resize_ms: Arc<AtomicU32>,
    /// Launched title's lease, if any — what [`games`] reports for this session.
    game: Option<Arc<crate::gamelease::LeaseShared>>,
    /// The capturer's live health, published by the video loop (WP18). `None` until the
    /// first publish, or on a capturer that does not classify.
    capture_health: Arc<Mutex<Option<pf_capture::CaptureHealth>>>,
    /// Sharing another session's display (`mode_conflict: join`) rather than owning one.
    join: bool,
    /// What the per-session management routes act on.
    controls: SessionControls,
    started: std::time::Instant,
    /// Host wall clock at registration; [`started`](Self::started) cannot give one.
    started_unix: i64,
    /// 8 or 10, and the encoder's chroma. Fixed for the session.
    bit_depth: u8,
    chroma: ChromaFormat,
    /// [`SessionEndReason`] as `u8`, latched by whichever path knows. 0 = the video
    /// loop never reached its clean tail, which is [`SessionEndReason::HostError`].
    end_reason: Arc<AtomicU8>,
    /// What the video loop counted, stored once at its tail ([`record_tally`]).
    /// `None` on a loop that bailed before it.
    tally: Option<SessionTally>,
    /// Totals the input, audio and encode paths bump while the session runs.
    counters: Arc<SessionCounters>,
    /// Client address, so sessions from one NAT or tunnel can be told apart.
    peer: Option<std::net::IpAddr>,
}

/// The video loop's own totals, handed over as it finishes.
#[derive(Clone, Copy)]
pub struct SessionTally {
    /// Access units the send thread put on the wire.
    pub frames_sent: u64,
    /// The capturer's refused frames; `None` where it does not count them.
    pub frames_dropped: Option<u64>,
    /// Path MTU the QUIC stack settled on, bytes.
    pub path_mtu: u16,
}

/// Live handles the management plane acts on for ONE session. Built in
/// `native::serve` and carried here by the video loop, so a per-session route
/// never has to reach across into another session's state.
#[derive(Clone)]
pub struct SessionControls {
    /// LIVE grant mask — the same atomic the input filter reads every event against.
    pub grants: Arc<AtomicU32>,
    /// The pairing's own mask. A live change is `requested & ceiling`: the console
    /// re-points within what this device is paired for, never above it.
    pub ceiling: Arc<AtomicU32>,
    /// Audio egress drops this session's frames while set. Capturer and sink stay up.
    pub muted: Arc<AtomicBool>,
    /// Access deadline, unix seconds; 0 = permanent. The `remaining_secs` an
    /// `AccessUpdate` owes the client.
    pub deadline_unix: Arc<AtomicI64>,
    /// The control task's access lane — the same one the expiry watch writes, so a
    /// live re-point clears the clipboard exactly like a pairing edit does.
    pub access_tx: Option<tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::AccessUpdate>>,
    /// The control task's audio lane. `None` on a session with no control task (tests).
    pub audio_tx: Option<tokio::sync::mpsc::UnboundedSender<punktfunk_core::quic::AudioState>>,
    /// OS pad slots this session holds, one bit each — the player numbers a local
    /// co-op game reads. Published by the input thread.
    pub pad_slots: Arc<AtomicU16>,
    /// Full cert fingerprint, when this session has one. The stable device key —
    /// `client` is only its 12-hex prefix, and an address is not an identity.
    pub fingerprint: Option<String>,
    /// The settings preset the client dialled with, when it named one.
    pub preset: Option<crate::events::PresetRef>,
    /// The profile the session plays as. `None` on GameStream.
    pub profile: Option<crate::events::ProfileRef>,
    /// This device's key in [`crate::inject::pad_pool`]. A reservation and a
    /// reconnect are keyed by it, so both follow the pairing, never the address.
    pub pad_owner: u64,
    /// Player slot the operator picked, 0-based; [`NO_PAD_SLOT`] = lazy claim.
    pub preferred_pad_slot: Arc<AtomicU8>,
    /// Live pad tap this session's input thread publishes to. Idle until a console
    /// opens `GET /session/{id}/pads` ([`crate::pad_feed`]).
    pub pads: Arc<crate::pad_feed::PadFeed>,
}

/// `preferred_pad_slot` for a session the operator has not placed. Not a valid
/// slot: `MAX_PADS` is 16.
pub const NO_PAD_SLOT: u8 = u8::MAX;

/// The compositor head one session streams — where its launch's window stage
/// places the game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamedHead {
    pub compositor: crate::vdisplay::Compositor,
    /// `wl_output.name` of the streamed head.
    pub output: String,
}

impl SessionControls {
    /// Full control, permanent, unmuted, nothing to tell the client. The shape an
    /// anonymous (`--open`) session and the tests start from.
    pub fn open() -> SessionControls {
        SessionControls {
            grants: Arc::new(AtomicU32::new(punktfunk_core::quic::GRANT_ALL)),
            ceiling: Arc::new(AtomicU32::new(punktfunk_core::quic::GRANT_ALL)),
            muted: Arc::new(AtomicBool::new(false)),
            deadline_unix: Arc::new(AtomicI64::new(0)),
            access_tx: None,
            audio_tx: None,
            pad_slots: Arc::new(AtomicU16::new(0)),
            fingerprint: None,
            preset: None,
            profile: None,
            pad_owner: crate::inject::pad_pool::owner_key(None),
            preferred_pad_slot: Arc::new(AtomicU8::new(NO_PAD_SLOT)),
            pads: Arc::new(crate::pad_feed::PadFeed::new()),
        }
    }

    /// The player slot the operator picked for this session, 0-based.
    pub fn player(&self) -> Option<u8> {
        match self.preferred_pad_slot.load(Ordering::Relaxed) {
            NO_PAD_SLOT => None,
            slot => Some(slot),
        }
    }

    /// Place this session's pads on `slot`, or hand it back to the lazy claim with
    /// `None`. The pool reservation is a hint the next pad to plug reads, so a pad
    /// already built keeps the slot it was created under until it re-plugs.
    ///
    /// `false` = another live session asked for that slot first and keeps it.
    pub fn set_player(&self, slot: Option<u8>) -> bool {
        let Some(slot) = slot else {
            self.preferred_pad_slot
                .store(NO_PAD_SLOT, Ordering::Relaxed);
            return true;
        };
        if !crate::inject::pad_pool::global().reserve(slot, self.pad_owner) {
            return false;
        }
        self.preferred_pad_slot.store(slot, Ordering::Relaxed);
        true
    }

    /// OS pad slots this session holds right now, lowest first.
    pub fn pads(&self) -> Vec<u8> {
        let mask = self.pad_slots.load(Ordering::Relaxed);
        (0..punktfunk_core::input::MAX_PADS as u8)
            .filter(|n| mask & (1 << n) != 0)
            .collect()
    }

    /// Re-point the live mask, clamped to the pairing's ceiling, and tell the client
    /// its chip changed. Returns the mask that took effect — never more than `ceiling`.
    pub fn set_grants(&self, requested: u32) -> u32 {
        let applied = requested & self.ceiling.load(Ordering::Relaxed);
        self.grants.store(applied, Ordering::Relaxed);
        let deadline = self.deadline_unix.load(Ordering::Relaxed);
        if let Some(tx) = &self.access_tx {
            let now = crate::clock::unix_secs();
            // Best-effort, like every other `AccessUpdate`: the host enforces either way.
            let _ = tx.send(punktfunk_core::quic::AccessUpdate {
                grants: applied,
                remaining_secs: if deadline == 0 {
                    0
                } else {
                    u32::try_from((deadline - now).max(1)).unwrap_or(u32::MAX)
                },
            });
        }
        applied
    }

    /// Set the audio gate and tell the client, so its overlay can name the silence
    /// instead of leaving the player to wonder what broke.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::SeqCst);
        if let Some(tx) = &self.audio_tx {
            let _ = tx.send(punktfunk_core::quic::AudioState { muted });
        }
    }
}

/// Session totals the loops bump and the summary reads.
///
/// One block rather than a dozen `Arc<Atomic…>`: the input task, the audio thread and the
/// encode loop all outlive [`LiveSessionGuard::drop`], so the summary cannot wait for them
/// to hand a figure over. Relaxed throughout — diagnostics, not synchronisation.
#[derive(Default)]
pub struct SessionCounters {
    /// Datagrams accepted by class, and offers the input queue refused when full.
    pub input_events: AtomicU64,
    pub input_mic: AtomicU64,
    pub input_rich: AtomicU64,
    pub input_dropped: AtomicU64,
    /// `RichInput::Motion` arrivals, and the gaps ≥ 500 ms among them.
    motion_samples: AtomicU64,
    motion_stalls: AtomicU64,
    /// Set when the audio thread starts. Distinguishes a silent plane from no plane.
    audio_ran: AtomicBool,
    audio_sent: AtomicU64,
    audio_infilled: AtomicU64,
    audio_late: AtomicU64,
    audio_max_late_us: AtomicU64,
    audio_reanchors: AtomicU64,
    /// Every encoder target the session ran at. `notes` counts them; the first is the
    /// opening rate, so the rest are the moves.
    bitrate_min_kbps: AtomicU32,
    bitrate_max_kbps: AtomicU32,
    bitrate_sum_kbps: AtomicU64,
    bitrate_notes: AtomicU32,
    /// Last rate noted, so a rebuild at the same number is not a move.
    bitrate_last_kbps: AtomicU32,
    /// Per-minute link health ([`crate::link_health`]). Drained by the control task, not by
    /// the summary: these are window deltas, the rest of this block is session totals.
    pub link: crate::link_health::LinkCounters,
    /// What the shared-path governor reads and writes for this session.
    pub share: AbrShare,
}

impl SessionCounters {
    /// One motion arrival. `stalled` is [`crate::native::motion_cadence`]'s own verdict, so
    /// what counts as a break in the feed is defined in exactly one place.
    /// Zero the per-session tallies. For a plane that keeps one counter block across
    /// sessions (the compat plane's, which its control loop bumps without a session handle).
    pub fn reset(&self) {
        for c in [
            &self.input_events,
            &self.input_mic,
            &self.input_rich,
            &self.input_dropped,
        ] {
            c.store(0, Ordering::Relaxed);
        }
    }

    pub fn note_motion(&self, stalled: bool) {
        self.motion_samples.fetch_add(1, Ordering::Relaxed);
        if stalled {
            self.motion_stalls.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The audio plane is up for this session, however little it goes on to send.
    pub fn note_audio_started(&self) {
        self.audio_ran.store(true, Ordering::Relaxed);
    }

    /// One audio frame on the wire. `late` is the miss against its slot and `was_late` the
    /// window's own verdict on it — same split as [`note_motion`](Self::note_motion).
    pub fn note_audio_frame(&self, late: std::time::Duration, infilled: bool, was_late: bool) {
        self.audio_sent.fetch_add(1, Ordering::Relaxed);
        if infilled {
            self.audio_infilled.fetch_add(1, Ordering::Relaxed);
        }
        if was_late {
            self.audio_late.fetch_add(1, Ordering::Relaxed);
        }
        self.audio_max_late_us
            .fetch_max(late.as_micros() as u64, Ordering::Relaxed);
    }

    pub fn note_audio_reanchor(&self) {
        self.audio_reanchors.fetch_add(1, Ordering::Relaxed);
    }

    /// One encoder target the session ran at. Call it wherever the rate actually changed:
    /// a repeat would read as a move that never happened.
    pub fn note_bitrate(&self, kbps: u32) {
        if kbps == 0 {
            return;
        }
        if self.bitrate_last_kbps.swap(kbps, Ordering::Relaxed) != kbps {
            self.link.note_retarget();
        }
        self.bitrate_min_kbps
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |m| {
                Some(if m == 0 { kbps } else { m.min(kbps) })
            })
            .ok();
        self.bitrate_max_kbps.fetch_max(kbps, Ordering::Relaxed);
        self.bitrate_sum_kbps
            .fetch_add(u64::from(kbps), Ordering::Relaxed);
        self.bitrate_notes.fetch_add(1, Ordering::Relaxed);
    }
}

/// Input one session dropped because its access grants don't cover it, per grant class.
/// One `warn!` on a class's first drop (per-event logging is a log DoS; on GameStream it is
/// the only support signal, as Moonlight has no grants UX), totals at session end.
pub struct GrantDrops {
    plane: Plane,
    counts: [u64; GrantClass::ALL.len()],
    warned: [bool; GrantClass::ALL.len()],
}

impl GrantDrops {
    pub fn new(plane: Plane) -> GrantDrops {
        GrantDrops {
            plane,
            counts: [0; GrantClass::ALL.len()],
            warned: [false; GrantClass::ALL.len()],
        }
    }

    /// `true` when `mask` grants `class`; otherwise counts the drop and returns `false`.
    pub fn permitted(&mut self, mask: u32, class: GrantClass) -> bool {
        if mask & class.bit() != 0 {
            return true;
        }
        self.note(class);
        false
    }

    /// Count one drop; log only the first of each class.
    pub fn note(&mut self, class: GrantClass) {
        let i = class.bit().trailing_zeros() as usize;
        self.counts[i] += 1;
        if !std::mem::replace(&mut self.warned[i], true) {
            tracing::warn!(
                class = ?class,
                plane = self.plane.as_str(),
                "dropping client input this session's access grants don't cover — counted; \
                 further drops of this class are silent until the session-end totals"
            );
        }
    }

    /// `Class=count` pairs in bit order; `None` when nothing was dropped.
    pub fn summary(&self) -> Option<String> {
        let pairs: Vec<String> = GrantClass::ALL
            .iter()
            .zip(self.counts)
            .filter(|(_, n)| *n != 0)
            .map(|(class, n)| format!("{class:?}={n}"))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join(" "))
    }
}

/// Resolved read of one live session for `/status`.
#[derive(Clone)]
pub struct SessionSnapshot {
    /// Same id the per-session routes and `session.started` carry.
    pub id: u64,
    /// Client label: 12-hex cert-fingerprint prefix, or peer IP if anonymous.
    pub client: String,
    pub hdr: bool,
    /// Sharing another session's display rather than owning one.
    pub join: bool,
    pub muted: bool,
    /// Live `GRANT_*` mask, after any per-session re-point.
    pub grants: u32,
    pub uptime_s: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// Display name (trust-store, else sanitized Hello). `None` if nameless.
    pub client_name: Option<String>,
    /// Name of the preset the client dialled with, if any.
    pub preset_name: Option<String>,
    /// The profile it plays as.
    pub profile: Option<crate::events::ProfileRef>,
    /// Which plane serves it.
    pub plane: crate::events::Plane,
    /// The capturer's live health, if it classifies.
    pub capture_health: Option<pf_capture::CaptureHealth>,
    /// Bring-up total (hello → first packet), ms. 0 while still bringing up.
    pub time_to_first_frame_ms: u32,
    /// Last mid-stream resize total, ms. 0 = no resize this session.
    pub last_resize_ms: u32,
    /// OS pad slots this session holds, lowest first. Slot `n` is player `n + 1`.
    pub pads: Vec<u8>,
    /// Player slot the operator picked, 0-based. `None` = lazy claim.
    pub preferred_pad_slot: Option<u8>,
    /// Last closed link-health minute ([`crate::link_health`]). `None` in a session's first
    /// minute, before one has closed.
    pub link: Option<crate::link_health::LinkMinute>,
    /// Other live sessions from the same client address.
    pub shared_path_with: Vec<u64>,
}

/// The live-session table, locked. A panic under the lock leaves every entry whole,
/// so poison is recovered here rather than turned into a panic at the next caller.
fn registry() -> MutexGuard<'static, Vec<LiveSession>> {
    static REG: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// `f` on live session `id`, under the registry lock. `None` = no such session.
fn with_session<T>(id: u64, f: impl FnOnce(&LiveSession) -> T) -> Option<T> {
    registry().iter().find(|s| s.id == id).map(f)
}

fn next_id() -> u64 {
    static ID: AtomicU64 = AtomicU64::new(1);
    ID.fetch_add(1, Ordering::Relaxed)
}

/// [`crate::events::SessionRef`] for this session; mode is read live.
fn session_ref(s: &LiveSession) -> crate::events::SessionRef {
    let (width, height, fps) = unpack_mode(s.mode.load(Ordering::Relaxed));
    crate::events::SessionRef {
        id: s.id,
        client: s.client.clone(),
        // `controls` already carries the device key this session was admitted by.
        fingerprint: s.controls.fingerprint.clone(),
        plane: s.plane,
        mode: crate::events::mode_str(width, height, fps),
        hdr: s.hdr,
        preset: s.controls.preset.clone(),
        profile: s.controls.profile.clone(),
    }
}

/// `None` until a pad sends motion: a keyboard-and-mouse session owes no gyro row.
fn gyro_cadence(c: &SessionCounters) -> Option<GyroCadence> {
    let samples = c.motion_samples.load(Ordering::Relaxed);
    let stalls = c.motion_stalls.load(Ordering::Relaxed);
    (samples > 0 || stalls > 0).then_some(GyroCadence { samples, stalls })
}

/// `None` when no audio thread ran. A thread that ran and sent nothing reports zeros —
/// a silent plane and no plane are different faults.
fn audio_egress(c: &SessionCounters) -> Option<AudioEgress> {
    c.audio_ran.load(Ordering::Relaxed).then(|| AudioEgress {
        sent: c.audio_sent.load(Ordering::Relaxed),
        infilled: c.audio_infilled.load(Ordering::Relaxed),
        late: c.audio_late.load(Ordering::Relaxed),
        max_late_ms: c.audio_max_late_us.load(Ordering::Relaxed) / 1_000,
        reanchors: c.audio_reanchors.load(Ordering::Relaxed),
    })
}

/// `None` until an opening rate is noted. The first note is that rate, so the moves are
/// the notes after it.
fn bitrate_span(c: &SessionCounters) -> Option<BitrateSpan> {
    let notes = c.bitrate_notes.load(Ordering::Relaxed);
    (notes > 0).then(|| BitrateSpan {
        min_kbps: c.bitrate_min_kbps.load(Ordering::Relaxed),
        avg_kbps: (c.bitrate_sum_kbps.load(Ordering::Relaxed) / u64::from(notes)) as u32,
        max_kbps: c.bitrate_max_kbps.load(Ordering::Relaxed),
        adaptive_steps: notes - 1,
    })
}

/// Everything this session ended up being. Built once, in [`LiveSessionGuard::drop`],
/// and shared by `session.ended` and `GET /session/last`.
fn summary(s: &LiveSession) -> SessionSummary {
    let (width, height, fps) = unpack_mode(s.mode.load(Ordering::Relaxed));
    SessionSummary {
        id: s.id,
        client: s.client.clone(),
        client_name: s.client_name.clone(),
        started_unix: s.started_unix,
        duration_s: s.started.elapsed().as_secs(),
        mode: crate::events::mode_str(width, height, fps),
        hdr: s.hdr,
        join: s.join,
        codec: s.codec.label().to_string(),
        bit_depth: s.bit_depth,
        chroma: if s.chroma.is_444() { "4:4:4" } else { "4:2:0" }.to_string(),
        bitrate_kbps: s.bitrate_kbps.load(Ordering::Relaxed),
        bitrate: bitrate_span(&s.counters),
        input: InputCounts {
            events: s.counters.input_events.load(Ordering::Relaxed),
            mic: s.counters.input_mic.load(Ordering::Relaxed),
            rich: s.counters.input_rich.load(Ordering::Relaxed),
            dropped: s.counters.input_dropped.load(Ordering::Relaxed),
        },
        gyro: gyro_cadence(&s.counters),
        audio: audio_egress(&s.counters),
        frames_sent: s.tally.map(|t| t.frames_sent),
        frames_dropped: s.tally.and_then(|t| t.frames_dropped),
        bringup_ms: s.ttff_ms.load(Ordering::Relaxed),
        path_mtu: s.tally.map(|t| t.path_mtu),
        // Nothing latched means the loop never reached its tail, which is a fault.
        ended: SessionEndReason::from_u8(s.end_reason.load(Ordering::SeqCst))
            .unwrap_or(SessionEndReason::HostError),
    }
}

/// Inputs for [`register`]. Named fields: half are same-typed `Arc<Atomic…>`
/// handles, so a transposed pair would compile and report the wrong figure.
pub struct Registration {
    /// Packed `w:16|h:16|hz:16` ([`pack_mode`]); live on a mode switch.
    pub mode: Arc<AtomicU64>,
    /// Encoder target, kbps. Same Arc the ABR path writes.
    pub bitrate_kbps: Arc<AtomicU32>,
    pub codec: Codec,
    /// Teardown flag ([`stop_all`]).
    pub stop: Arc<AtomicBool>,
    /// Deliberate-stop flag ([`stop_all_quit`]). Distinct from `stop`.
    pub quit: Arc<AtomicBool>,
    /// One-shot force-keyframe ([`force_idr_all`]).
    pub force_idr: Arc<AtomicBool>,
    /// Client label: 12-hex cert-fingerprint prefix, or peer IP if anonymous.
    pub client: String,
    /// Display name (trust-store, else sanitized Hello). `None` if nameless.
    pub client_name: Option<String>,
    /// Which plane serves it.
    pub plane: crate::events::Plane,
    pub hdr: bool,
    /// Bring-up total slot (hello → first packet), ms. 0 until first packet.
    pub ttff_ms: Arc<AtomicU32>,
    /// Last mid-stream resize total, ms. 0 = none yet.
    pub last_resize_ms: Arc<AtomicU32>,
    /// Launched title's lease, if this session launched one.
    pub game: Option<Arc<crate::gamelease::LeaseShared>>,
    /// The video loop's capture-health slot; it stores the capturer's report on its own cadence.
    pub capture_health: Arc<Mutex<Option<pf_capture::CaptureHealth>>>,
    /// Sharing another session's display (`mode_conflict: join`) rather than owning one.
    pub join: bool,
    /// Handles the per-session management routes act on.
    pub controls: SessionControls,
    /// 8 or 10, and the encoder's chroma. Both fixed for the session.
    pub bit_depth: u8,
    pub chroma: ChromaFormat,
    /// [`SessionEndReason`] latch, shared with the paths that know why: the connection
    /// watcher, the game-exit close, an operator stop, and the loop's own clean tail.
    pub end_reason: Arc<AtomicU8>,
    /// The session's shared counter block, already being bumped by its side threads.
    pub counters: Arc<SessionCounters>,
    /// Client address. `None` only where there is no connection (tests).
    pub peer: Option<std::net::IpAddr>,
}

/// Publish a live native session. The guard removes it on drop and pairs
/// `session.started` with `session.ended` on every exit path, including panic.
pub fn register(reg: Registration) -> LiveSessionGuard {
    let Registration {
        mode,
        bitrate_kbps,
        codec,
        stop,
        quit,
        force_idr,
        client,
        client_name,
        plane,
        hdr,
        ttff_ms,
        last_resize_ms,
        game,
        capture_health,
        join,
        controls,
        bit_depth,
        chroma,
        end_reason,
        counters,
        peer,
    } = reg;
    let id = next_id();
    let session = LiveSession {
        id,
        mode,
        bitrate_kbps,
        codec,
        stop,
        quit,
        force_idr,
        client,
        client_name,
        plane,
        hdr,
        ttff_ms,
        last_resize_ms,
        game,
        capture_health,
        join,
        controls,
        started: std::time::Instant::now(),
        started_unix: crate::clock::unix_secs(),
        bit_depth,
        chroma,
        end_reason,
        tally: None,
        counters,
        peer: peer.map(|ip| ip.to_canonical()),
    };
    audio_policy::apply_to_new(&session);
    // The opening rate, so the span has a floor before adaptive bitrate moves it. Registration
    // is after the encoder opened, so this is what it actually runs at.
    session
        .counters
        .note_bitrate(session.bitrate_kbps.load(Ordering::Relaxed));
    // The link-health lines carry this id, so a line ties to a `/status` row.
    session.counters.link.set_session_id(id);
    crate::events::emit(crate::events::EventKind::SessionStarted {
        session: session_ref(&session),
    });
    let mut reg = registry();
    let sharing = shared_path(&reg, session.id, session.peer);
    if !sharing.is_empty() {
        // Same address is one NAT or tunnel, not proof of one bottleneck, so the
        // governor divides only what the group is short of ([`share_for`]).
        tracing::info!(
            session = session.id,
            peer = ?session.peer,
            others = ?sharing,
            "sessions share one client address — Automatic ones take equal shares of it"
        );
    }
    reg.push(session);
    drop(reg);
    LiveSessionGuard {
        id,
        _sleep: crate::sleep_inhibit::hold(),
    }
}

/// Drops the registry entry for this session (any video-loop scope exit).
pub struct LiveSessionGuard {
    /// Same id `/status` reports; the video loop's log span names it.
    pub(crate) id: u64,
    /// Sleep inhibit for the session lifetime: a passive viewer must not let
    /// the box auto-suspend ([`crate::sleep_inhibit`]).
    _sleep: crate::sleep_inhibit::StreamHold,
}

impl Drop for LiveSessionGuard {
    /// Retires the entry, then publishes the session's own numbers twice: onto
    /// `session.ended` for a live consumer, and into the ring a bug report reads back.
    fn drop(&mut self) {
        let mut reg = registry();
        if let Some(pos) = reg.iter().position(|s| s.id == self.id) {
            let session = reg.remove(pos);
            drop(reg); // emit outside the registry lock; the bus takes its own
            let summary = summary(&session);
            push_recent(summary.clone());
            crate::events::emit(crate::events::EventKind::SessionEnded {
                session: session_ref(&session),
                summary: Box::new(summary),
            });
        }
    }
}

/// Eight finished sessions: the one that just broke, plus the handful before it a
/// reporter is asked about. A host that streams for weeks must not grow a log here.
const RECENT_SESSIONS: usize = 8;

fn recent_ring() -> &'static Mutex<VecDeque<SessionSummary>> {
    static RING: OnceLock<Mutex<VecDeque<SessionSummary>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RECENT_SESSIONS)))
}

/// Newest at the front, oldest evicted. Split out so the bound is testable without
/// the process-global ring, which every other test in this binary also writes to.
fn push_bounded(ring: &mut VecDeque<SessionSummary>, s: SessionSummary) {
    if ring.len() == RECENT_SESSIONS {
        ring.pop_back();
    }
    ring.push_front(s);
}

fn push_recent(s: SessionSummary) {
    push_bounded(
        &mut recent_ring().lock().unwrap_or_else(|e| e.into_inner()),
        s,
    );
}

/// Finished sessions, newest first. Empty on a host that has not streamed since it
/// started — `GET /session/last` answers with the empty list, not an error.
pub fn recent() -> Vec<SessionSummary> {
    recent_ring()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Hand the video loop's totals to the registry as it finishes, so the summary the
/// guard builds a moment later carries them. Unknown id = the entry is already gone.
pub fn record_tally(id: u64, tally: SessionTally) {
    if let Some(s) = registry().iter_mut().find(|s| s.id == id) {
        s.tally = Some(tally);
    }
}

/// Ids of the other live sessions from `peer`, oldest first.
fn shared_path(reg: &[LiveSession], id: u64, peer: Option<std::net::IpAddr>) -> Vec<u64> {
    let Some(peer) = peer else {
        return Vec::new();
    };
    reg.iter()
        .filter(|s| s.id != id && s.peer == Some(peer))
        .map(|s| s.id)
        .collect()
}

pub fn count() -> usize {
    registry().len()
}

/// Snapshot of every live native session; mode/bitrate read live. Newest last.
pub fn snapshot() -> Vec<SessionSnapshot> {
    let reg = registry();
    reg.iter()
        .map(|s| {
            let (width, height, fps) = unpack_mode(s.mode.load(Ordering::Relaxed));
            SessionSnapshot {
                id: s.id,
                client: s.client.clone(),
                hdr: s.hdr,
                join: s.join,
                muted: s.controls.muted.load(Ordering::SeqCst),
                grants: s.controls.grants.load(Ordering::Relaxed),
                uptime_s: s.started.elapsed().as_secs(),
                width,
                height,
                fps,
                bitrate_kbps: s.bitrate_kbps.load(Ordering::Relaxed),
                codec: s.codec,
                client_name: s.client_name.clone(),
                preset_name: s.controls.preset.as_ref().map(|p| p.name.clone()),
                profile: s.controls.profile.clone(),
                plane: s.plane,
                capture_health: s
                    .capture_health
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone(),
                time_to_first_frame_ms: s.ttff_ms.load(Ordering::Relaxed),
                last_resize_ms: s.last_resize_ms.load(Ordering::Relaxed),
                pads: s.controls.pads(),
                preferred_pad_slot: s.controls.player(),
                link: s.counters.link.last(),
                shared_path_with: shared_path(&reg, s.id, s.peer),
            }
        })
        .collect()
}

/// Tear down every live native session. Best-effort: loops observe the
/// flag and exit; the guard then clears the entry. Not intended teardown —
/// prefer [`stop_all_quit`] for an operator action.
pub fn stop_all() {
    for s in registry().iter() {
        s.stop.store(true, Ordering::SeqCst);
    }
}

/// Tear down live native sessions for `fp_hex` (lowercase hex cert SHA-256)
/// deliberately — unpair must not leave a mid-stream client streaming.
///
/// Match is the registry client label: the fingerprint's 12-hex prefix.
/// An anonymous/TOFU session carries an IP label and never matches.
/// Returns how many sessions were signalled.
pub fn stop_by_fingerprint(fp_hex: &str) -> usize {
    let mut n = 0;
    for s in registry().iter() {
        if s.client.len() == 12 && fp_hex.starts_with(s.client.as_str()) {
            SessionEndReason::StoppedByOperator.latch(&s.end_reason);
            s.quit.store(true, Ordering::SeqCst);
            s.stop.store(true, Ordering::SeqCst);
            n += 1;
        }
    }
    n
}

/// Whether any live native session belongs to a client other than `fp_hex`.
///
/// Busy check for host-power: a granted guest must not power off the host
/// while someone else is streaming. Label match as in [`stop_by_fingerprint`].
/// An anonymous IP-labelled session always counts as another client.
pub fn other_client_live(fp_hex: &str) -> bool {
    registry()
        .iter()
        .any(|s| !(s.client.len() == 12 && fp_hex.starts_with(s.client.as_str())))
}

/// Whether `fp_hex` owns a live session: its own display, not a join onto another's.
///
/// Who may switch the streamed monitor (`display.next`). Label match as in
/// [`stop_by_fingerprint`], so an anonymous IP-labelled session owns nothing.
pub fn owns_live_session(fp_hex: &str) -> bool {
    registry()
        .iter()
        .any(|s| !s.join && s.client.len() == 12 && fp_hex.starts_with(s.client.as_str()))
}

/// Tear down every live native session deliberately (mgmt `DELETE /session`).
///
/// Sets `quit` before `stop` so teardown matches a client's own Stop: the
/// display skips keep-alive linger and end-game-on-session-end sees intent.
/// The summary says `stopped_by_operator`; the client only ever sees `host_ended`.
pub fn stop_all_quit() {
    for s in registry().iter() {
        SessionEndReason::StoppedByOperator.latch(&s.end_reason);
        s.quit.store(true, Ordering::SeqCst);
        s.stop.store(true, Ordering::SeqCst);
    }
}

/// Tear down ONE live native session deliberately (`DELETE /session/{id}`).
/// `false` = no such session. Same `quit`-before-`stop` order and same
/// `stopped_by_operator` summary as [`stop_all_quit`].
pub fn stop_quit(id: u64) -> bool {
    with_session(id, |s| {
        SessionEndReason::StoppedByOperator.latch(&s.end_reason);
        s.quit.store(true, Ordering::SeqCst);
        s.stop.store(true, Ordering::SeqCst);
    })
    .is_some()
}

/// Force a keyframe on ONE live native session (`POST /session/{id}/idr`).
/// `false` = no such session.
pub fn force_idr(id: u64) -> bool {
    with_session(id, |s| s.force_idr.store(true, Ordering::Relaxed)).is_some()
}

/// Whether this session is served by the plane whose per-session mute, access and player
/// lanes exist. `None` = no such session. The compat plane registers (so it has an id, a
/// stop and a keyframe) but carries none of those three.
pub fn has_native_lanes(id: u64) -> Option<bool> {
    with_session(id, |s| s.plane != crate::events::Plane::Gamestream)
}

/// This session's management handles, cloned out so the caller acts without the
/// registry lock. `None` = no such session (the routes' 404).
pub fn controls(id: u64) -> Option<SessionControls> {
    with_session(id, |s| s.controls.clone())
}

/// Force a keyframe on every live native session (`POST /session/idr`).
/// The encode loop drains the flag like a client decode-recovery request.
pub fn force_idr_all() {
    for s in registry().iter() {
        s.force_idr.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Controller-only passes the pad and counts keyboard and pointer drops per class;
    /// the summary lists only classes that dropped, in bit order.
    #[test]
    fn grant_drops_pass_the_granted_class_and_count_the_rest() {
        use punktfunk_core::input::InputKind;
        use punktfunk_core::quic::{classify, GRANT_PRESET_CONTROLLER_ONLY};
        let mut drops = GrantDrops::new(Plane::Gamestream);
        assert_eq!(drops.summary(), None);
        let mask = GRANT_PRESET_CONTROLLER_ONLY;
        assert!(drops.permitted(mask, classify(InputKind::GamepadButton)));
        assert!(!drops.permitted(mask, classify(InputKind::KeyDown)));
        assert!(!drops.permitted(mask, classify(InputKind::TextInput)));
        assert!(!drops.permitted(mask, classify(InputKind::MouseMove)));
        assert!(!drops.permitted(mask, GrantClass::Pointer));
        drops.note(GrantClass::Mic);
        assert_eq!(
            drops.summary().as_deref(),
            Some("Pointer=2 Keyboard=2 Mic=1")
        );
    }

    /// The live-session registry is one table for the whole test binary: a
    /// session one test registers is an active stream to another test's route,
    /// a row in its `/status`, and an entry in the recent ring. Every test that
    /// registers a session or reads the registry's shape holds this.
    ///
    /// A test that also needs `native::tests`' admission lock takes this one
    /// first. One order, so the pair cannot deadlock.
    pub(crate) static REGISTRY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The registry lock for a test that has no runtime of its own.
    /// [`tokio::sync::Mutex::blocking_lock`] panics inside one, so an
    /// `#[tokio::test]` takes `REGISTRY.lock().await` instead.
    pub(crate) fn registry_lock() -> tokio::sync::MutexGuard<'static, ()> {
        REGISTRY.blocking_lock()
    }

    /// A compat-plane session is a registry entry like any other, which is what gives the
    /// console an id to stop — before, a Moonlight session could only be ended host-wide.
    #[test]
    fn a_compat_session_is_stoppable_by_its_own_id() {
        let _registry = registry_lock();
        let stop = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let _guard = register(Registration {
            stop: stop.clone(),
            quit: quit.clone(),
            client_name: Some("Living Room TV".into()),
            plane: crate::events::Plane::Gamestream,
            hdr: true,
            bit_depth: 10,
            ..Registration::fake("9f86d0818840")
        });
        let row = snapshot()
            .into_iter()
            .find(|s| s.plane == crate::events::Plane::Gamestream)
            .expect("the compat session is listed like a native one");
        assert_eq!(row.client_name.as_deref(), Some("Living Room TV"));
        assert!(stop_quit(row.id), "its id addresses it");
        assert!(
            stop.load(Ordering::SeqCst),
            "the stream loop is told to end"
        );
        assert!(
            quit.load(Ordering::SeqCst),
            "and that the end was deliberate"
        );
        assert!(!stop_quit(u64::MAX), "an id nothing holds stops nothing");
    }

    impl Registration {
        /// A native 8-bit H.265 session for `client` at 20 Mbps, every handle fresh and
        /// zeroed, no peer. A test overrides only the fields it reads.
        pub(crate) fn fake(client: &str) -> Registration {
            Registration {
                mode: Arc::new(AtomicU64::new(0)),
                bitrate_kbps: Arc::new(AtomicU32::new(20_000)),
                codec: Codec::H265,
                stop: Arc::new(AtomicBool::new(false)),
                quit: Arc::new(AtomicBool::new(false)),
                force_idr: Arc::new(AtomicBool::new(false)),
                client: client.into(),
                client_name: None,
                plane: crate::events::Plane::Native,
                hdr: false,
                ttff_ms: Arc::new(AtomicU32::new(0)),
                last_resize_ms: Arc::new(AtomicU32::new(0)),
                game: None,
                capture_health: Arc::new(Mutex::new(None)),
                join: false,
                controls: SessionControls::open(),
                bit_depth: 8,
                chroma: ChromaFormat::Yuv420,
                end_reason: Arc::new(AtomicU8::new(0)),
                counters: Arc::new(SessionCounters::default()),
                peer: None,
            }
        }
    }

    fn fake_session(client: &str) -> (LiveSessionGuard, Arc<AtomicBool>, Arc<AtomicBool>) {
        fake_session_with_reason(
            client,
            Arc::new(AtomicU8::new(0)),
            Arc::new(SessionCounters::default()),
        )
    }

    fn fake_session_with_reason(
        client: &str,
        end_reason: Arc<AtomicU8>,
        counters: Arc<SessionCounters>,
    ) -> (LiveSessionGuard, Arc<AtomicBool>, Arc<AtomicBool>) {
        let stop = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let guard = register(Registration {
            stop: stop.clone(),
            quit: quit.clone(),
            end_reason,
            counters,
            ..Registration::fake(client)
        });
        (guard, stop, quit)
    }

    /// A panic under the registry lock costs the next caller nothing: a new session
    /// registers, and the per-id and host-wide stops still reach it.
    #[test]
    fn a_poisoned_registry_keeps_answering() {
        let _registry = registry_lock();
        let _ = std::thread::spawn(|| {
            let _held = registry();
            panic!("poison the registry");
        })
        .join();
        let (guard, stop, quit) = fake_session("aabbccddeeff");
        assert!(count() >= 1);
        assert!(force_idr(guard.id), "the per-id routes find it");
        stop_all_quit();
        assert!(stop.load(Ordering::SeqCst) && quit.load(Ordering::SeqCst));
    }

    pub(super) fn fake_joiner(client: &str, join: bool) -> (LiveSessionGuard, SessionControls) {
        fake_at(client, join, None)
    }

    fn fake_at(
        client: &str,
        join: bool,
        peer: Option<std::net::IpAddr>,
    ) -> (LiveSessionGuard, SessionControls) {
        let controls = SessionControls::open();
        let guard = register(Registration {
            join,
            controls: controls.clone(),
            peer,
            ..Registration::fake(client)
        });
        (guard, controls)
    }

    /// A live session at `peer` with its own counter block, so a test can
    /// publish what the governor reads and move the rate it hands out.
    pub(crate) fn fake_member(
        client: &str,
        peer: std::net::IpAddr,
        kbps: u32,
    ) -> (LiveSessionGuard, Arc<SessionCounters>, Arc<AtomicU32>) {
        let counters = Arc::new(SessionCounters::default());
        let bitrate_kbps = Arc::new(AtomicU32::new(kbps));
        let guard = register(Registration {
            bitrate_kbps: bitrate_kbps.clone(),
            counters: counters.clone(),
            peer: Some(peer),
            ..Registration::fake(client)
        });
        (guard, counters, bitrate_kbps)
    }

    /// One address is one NAT or tunnel: each session names the others there, and an
    /// IPv4-mapped IPv6 peer is the same address.
    #[test]
    fn sessions_from_one_address_name_each_other() {
        let _registry = registry_lock();
        let v4: std::net::IpAddr = "203.0.113.77".parse().unwrap();
        let mapped: std::net::IpAddr = "::ffff:203.0.113.77".parse().unwrap();
        let (a, _) = fake_at("phone", false, Some(v4));
        let (b, _) = fake_at("pc", false, Some(mapped));
        let (c, _) = fake_at("tv", false, Some("203.0.113.78".parse().unwrap()));
        let rows = snapshot();
        let with = |id| {
            rows.iter()
                .find(|s| s.id == id)
                .unwrap()
                .shared_path_with
                .clone()
        };
        assert_eq!(with(a.id), vec![b.id]);
        assert_eq!(with(b.id), vec![a.id]);
        assert!(with(c.id).is_empty());
        drop(b);
        assert!(snapshot()
            .iter()
            .find(|s| s.id == a.id)
            .unwrap()
            .shared_path_with
            .is_empty());
    }

    /// The owner of a live display may switch its monitor; a joiner watching it, a device with
    /// nothing live, and an anonymous IP-labelled session may not.
    #[test]
    fn only_the_owner_of_a_live_display_owns_a_session() {
        let _registry = registry_lock();
        let owner = "cccccccccccc0011223344556677";
        let joiner = "dddddddddddd0011223344556677";
        assert!(!owns_live_session(owner), "nothing live yet");
        let (_o, _) = fake_joiner(&owner[..12], false);
        let (_j, _) = fake_joiner(&joiner[..12], true);
        let (_a, ..) = fake_session("192.168.1.50");
        assert!(owns_live_session(owner));
        assert!(
            !owns_live_session(joiner),
            "a joiner follows the owner's picture"
        );
        assert!(!owns_live_session("eeeeeeeeeeee0011223344556677"));
    }

    /// Unpair revokes a live session by the 12-hex fingerprint prefix
    /// (`quit` + `stop`). Other clients and IP-labelled sessions stay up.
    #[test]
    fn stop_by_fingerprint_revokes_exactly_the_unpaired_client() {
        let _registry = registry_lock();
        let fp = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        let (_g1, stop1, quit1) = fake_session(&fp[..12]);
        let (_g2, stop2, _q2) = fake_session("112233445566"); // a different paired client
        let (_g3, stop3, _q3) = fake_session("192.168.1.50"); // anonymous: IP label, never matches

        assert_eq!(stop_by_fingerprint(fp), 1);
        assert!(stop1.load(Ordering::SeqCst) && quit1.load(Ordering::SeqCst));
        assert!(!stop2.load(Ordering::SeqCst));
        assert!(!stop3.load(Ordering::SeqCst));
    }

    /// Each registered session carries its own pad feed, so a Controllers stream
    /// opened on one id can never draw another session's input.
    #[tokio::test]
    async fn each_session_holds_its_own_pad_feed() {
        let (a, _s, _q) = fake_session("aaaaaaaaaaaa");
        let (b, _s2, _q2) = fake_session("bbbbbbbbbbbb");
        let feed_a = controls(a.id).expect("A is live").pads;
        let feed_b = controls(b.id).expect("B is live").pads;
        assert!(!Arc::ptr_eq(&feed_a, &feed_b));

        let mut rx = feed_a.subscribe();
        // B has no subscriber, so its publish is a no-op; A's must not receive it either way.
        feed_b.publish(|| unreachable!("an unwatched feed must not build a frame"));
        assert!(rx.try_recv().is_err(), "A's stream holds only A's pads");
    }

    fn one_summary(id: u64) -> SessionSummary {
        SessionSummary {
            id,
            client: "192.0.2.7".into(),
            client_name: None,
            started_unix: 0,
            duration_s: 0,
            mode: "1920x1080@60".into(),
            hdr: false,
            join: false,
            codec: "hevc".into(),
            bit_depth: 8,
            chroma: "4:2:0".into(),
            bitrate_kbps: 0,
            bitrate: None,
            frames_sent: None,
            frames_dropped: None,
            input: InputCounts {
                events: 0,
                mic: 0,
                rich: 0,
                dropped: 0,
            },
            gyro: None,
            audio: None,
            bringup_ms: 0,
            path_mtu: None,
            ended: SessionEndReason::HostEnded,
        }
    }

    /// A host that has not streamed answers with nothing, and a host that streams for
    /// weeks still answers with eight — newest first.
    #[test]
    fn the_recent_ring_starts_empty_and_keeps_only_the_newest() {
        let mut ring: VecDeque<SessionSummary> = VecDeque::new();
        assert!(ring.iter().next().is_none(), "no sessions is not an error");

        for id in 1..=(RECENT_SESSIONS as u64 * 3) {
            push_bounded(&mut ring, one_summary(id));
        }
        assert_eq!(ring.len(), RECENT_SESSIONS);
        assert_eq!(ring.front().map(|s| s.id), Some(RECENT_SESSIONS as u64 * 3));
        assert_eq!(
            ring.back().map(|s| s.id),
            Some(RECENT_SESSIONS as u64 * 3 - RECENT_SESSIONS as u64 + 1)
        );
    }

    /// The whole feature end to end: the numbers the video loop hands over, and the
    /// reason an operator stop latched, survive into the ring `GET /session/last` reads.
    #[test]
    fn a_finished_session_lands_in_the_ring_with_its_numbers() {
        let _registry = registry_lock();
        let reason = Arc::new(AtomicU8::new(0));
        let counters = Arc::new(SessionCounters::default());
        let (guard, _stop, _quit) =
            fake_session_with_reason("192.0.2.9", reason.clone(), counters.clone());
        let id = guard.id;
        // What the side threads bump while the session runs, each through its own note.
        counters.input_events.fetch_add(7457, Ordering::Relaxed);
        counters.input_rich.fetch_add(150_983, Ordering::Relaxed);
        for stalled in [false, false, true] {
            counters.note_motion(stalled);
        }
        counters.note_audio_started();
        counters.note_audio_frame(std::time::Duration::from_millis(11), true, true);
        counters.note_audio_frame(std::time::Duration::from_millis(1), false, false);
        counters.note_audio_reanchor();
        // 20 000 was seeded at registration, so these two are the moves.
        counters.note_bitrate(10_000);
        counters.note_bitrate(30_000);
        record_tally(
            id,
            SessionTally {
                frames_sent: 4096,
                frames_dropped: Some(3),
                path_mtu: 1369,
            },
        );
        SessionEndReason::StoppedByOperator.latch(&reason);
        drop(guard);

        // By id: other tests in this binary drop their own sessions into the same ring.
        let mine = recent()
            .into_iter()
            .find(|s| s.id == id)
            .expect("the finished session is in the ring");
        assert_eq!(mine.frames_sent, Some(4096));
        assert_eq!(mine.frames_dropped, Some(3));
        assert_eq!(mine.path_mtu, Some(1369));
        assert_eq!(mine.ended, SessionEndReason::StoppedByOperator);
        assert_eq!(mine.codec, "hevc");
        assert_eq!(mine.chroma, "4:2:0");
        assert_eq!(mine.bitrate_kbps, 20_000);

        let input = &mine.input;
        assert_eq!((input.events, input.rich, input.mic), (7457, 150_983, 0));

        let gyro = mine.gyro.expect("a pad sent motion");
        assert_eq!((gyro.samples, gyro.stalls), (3, 1));

        let audio = mine.audio.expect("the audio plane ran");
        assert_eq!((audio.sent, audio.infilled, audio.late), (2, 1, 1));
        assert_eq!(audio.max_late_ms, 11, "the worst miss, not the last");
        assert_eq!(audio.reanchors, 1);

        // Mean of 20 000, 10 000 and 30 000 — the targets, not their durations.
        let b = mine.bitrate.expect("an encoder opened");
        assert_eq!(
            (b.min_kbps, b.avg_kbps, b.max_kbps),
            (10_000, 20_000, 30_000)
        );
        assert_eq!(b.adaptive_steps, 2, "the opening rate is not a move");
    }

    /// A session whose loop never reached its tail reports the fault and no totals,
    /// rather than a clean end with zeros in it.
    #[test]
    fn a_session_that_never_finished_reports_a_host_error() {
        let _registry = registry_lock();
        let (guard, _stop, _quit) = fake_session("192.0.2.10");
        let id = guard.id;
        drop(guard);

        let mine = recent()
            .into_iter()
            .find(|s| s.id == id)
            .expect("an abandoned session is still summarized");
        assert_eq!(mine.ended, SessionEndReason::HostError);
        assert_eq!(mine.frames_sent, None);
        assert_eq!(mine.path_mtu, None);
        // No pad, no audio thread: absent, not a row of zeros that reads as "all fine".
        assert!(mine.gyro.is_none());
        assert!(mine.audio.is_none());
        // The datagram reader is up before a session registers, so its counts are always a claim.
        assert_eq!(mine.input.events, 0);
    }
}
