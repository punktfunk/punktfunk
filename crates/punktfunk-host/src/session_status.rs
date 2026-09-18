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
use std::sync::{Arc, Mutex, OnceLock};

use crate::encode::{ChromaFormat, Codec};
use crate::events::{
    AudioEgress, BitrateSpan, GyroCadence, InputCounts, SessionEndReason, SessionSummary,
};

/// One live native session. The Arcs are the video loop's own handles, so a
/// mid-stream mode/bitrate change shows on `/status` with no second write.
struct LiveSession {
    id: u64,
    /// Packed `w:16|h:16|hz:16` ([`crate::native::pack_mode`]); live on a mode switch.
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
    /// Head the window routes read and act on ([`SessionControls::set_head`]).
    /// Latched per session: the injector's slot is one per process, and a second
    /// session's bring-up would otherwise re-point this one at its head.
    pub head: Arc<Mutex<Option<StreamedHead>>>,
    /// OS pad slots this session holds, one bit each — the player numbers a local
    /// co-op game reads. Published by the input thread.
    pub pad_slots: Arc<AtomicU16>,
    /// Full cert fingerprint, when this session has one. The stable device key —
    /// `client` is only its 12-hex prefix, and an address is not an identity.
    pub fingerprint: Option<String>,
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

/// The compositor head one session streams — what its window list names.
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
            head: Arc::new(Mutex::new(None)),
            pad_slots: Arc::new(AtomicU16::new(0)),
            fingerprint: None,
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

    /// Latch the head this session streams, once the capture pipeline names it.
    /// A backend that names no output leaves it `None` and lists nothing.
    pub fn set_head(&self, head: Option<StreamedHead>) {
        *self.head.lock().unwrap_or_else(|e| e.into_inner()) = head;
    }

    /// This session's head, or `None` before capture is up.
    pub fn head(&self) -> Option<StreamedHead> {
        self.head.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Re-point the live mask, clamped to the pairing's ceiling, and tell the client
    /// its chip changed. Returns the mask that took effect — never more than `ceiling`.
    pub fn set_grants(&self, requested: u32) -> u32 {
        let applied = requested & self.ceiling.load(Ordering::Relaxed);
        self.grants.store(applied, Ordering::Relaxed);
        let deadline = self.deadline_unix.load(Ordering::Relaxed);
        if let Some(tx) = &self.access_tx {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
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

/// Which sessions hear a title's audio (`audio.sessions` on a custom entry).
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    serde::Serialize,
    serde::Deserialize,
    utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AudioSessions {
    #[default]
    All,
    /// The session that owns the display; joiners are silent.
    Owner,
    /// The sessions that joined the display; the owner is silent.
    Joined,
    /// Only the session that launched the title.
    Launcher,
}

/// Whether `policy` silences a session. `launcher` is the launching session's
/// client label, matched the way [`stop_by_fingerprint`] matches.
fn policy_mutes(policy: AudioSessions, launcher: &str, client: &str, join: bool) -> bool {
    match policy {
        AudioSessions::All => false,
        AudioSessions::Owner => join,
        AudioSessions::Joined => !join,
        AudioSessions::Launcher => client != launcher,
    }
}

struct AudioPolicy {
    sessions: AudioSessions,
    launcher: String,
    /// Sessions this policy muted, so its end unmutes exactly those.
    muted: Vec<u64>,
}

/// The live title policy, applied to sessions that register while it stands.
/// One at a time: a second lease replaces it, and the first to end lifts both.
static AUDIO_POLICY: Mutex<Option<AudioPolicy>> = Mutex::new(None);

/// Apply a title's `audio.sessions` through the same mute the operator uses, so
/// the client is told and the operator can unmute live. Drop unmutes what it muted.
pub fn apply_audio_policy(sessions: AudioSessions, launcher: &str) -> AudioPolicyGuard {
    let mut muted = Vec::new();
    for s in registry().lock().unwrap().iter() {
        // Compat sessions have no per-session mute, and marking one muted without muting it
        // would put a Muted badge on a session the operator can still hear.
        if s.plane == crate::events::Plane::Native
            && policy_mutes(sessions, launcher, &s.client, s.join)
        {
            s.controls.set_muted(true);
            muted.push(s.id);
        }
    }
    *AUDIO_POLICY.lock().unwrap() = Some(AudioPolicy {
        sessions,
        launcher: launcher.to_owned(),
        muted,
    });
    tracing::info!(policy = ?sessions, launcher, "title audio policy applied");
    AudioPolicyGuard(())
}

pub struct AudioPolicyGuard(());

impl Drop for AudioPolicyGuard {
    fn drop(&mut self) {
        let Some(policy) = AUDIO_POLICY.lock().unwrap().take() else {
            return;
        };
        for s in registry().lock().unwrap().iter() {
            if policy.muted.contains(&s.id) {
                s.controls.set_muted(false);
            }
        }
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

fn registry() -> &'static Mutex<Vec<LiveSession>> {
    static REG: OnceLock<Mutex<Vec<LiveSession>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(Vec::new()))
}

fn next_id() -> u64 {
    static ID: AtomicU64 = AtomicU64::new(1);
    ID.fetch_add(1, Ordering::Relaxed)
}

/// [`crate::events::SessionRef`] for this session; mode is read live.
fn session_ref(s: &LiveSession) -> crate::events::SessionRef {
    let (width, height, fps) = crate::native::unpack_mode(s.mode.load(Ordering::Relaxed));
    crate::events::SessionRef {
        id: s.id,
        client: s.client.clone(),
        // `controls` already carries the device key this session was admitted by.
        fingerprint: s.controls.fingerprint.clone(),
        plane: s.plane,
        mode: crate::events::mode_str(width, height, fps),
        hdr: s.hdr,
    }
}

/// Host wall clock, unix seconds — the same clock `mgmt::auth` stamps deadlines in.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
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
    let (width, height, fps) = crate::native::unpack_mode(s.mode.load(Ordering::Relaxed));
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
    /// Packed `w:16|h:16|hz:16` ([`crate::native::pack_mode`]); live on a mode switch.
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
    // A standing title policy reaches a session that arrives under it.
    if let Some(p) = AUDIO_POLICY.lock().unwrap().as_mut() {
        if plane == crate::events::Plane::Native
            && policy_mutes(p.sessions, &p.launcher, &client, join)
        {
            controls.set_muted(true);
            p.muted.push(id);
        }
    }
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
        started_unix: unix_now(),
        bit_depth,
        chroma,
        end_reason,
        tally: None,
        counters,
        peer: peer.map(|ip| ip.to_canonical()),
    };
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
    let mut reg = registry().lock().unwrap();
    let sharing = shared_path(&reg, session.id, session.peer);
    if !sharing.is_empty() {
        // Same address is one NAT or tunnel, not proof of one bottleneck — so an observation.
        tracing::warn!(
            session = session.id,
            peer = ?session.peer,
            others = ?sharing,
            "sessions share one client address — each adapts its bitrate on its own"
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
        let mut reg = registry().lock().unwrap_or_else(|e| e.into_inner());
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
    if let Some(s) = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter_mut()
        .find(|s| s.id == id)
    {
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
    registry().lock().unwrap().len()
}

/// Snapshot of every live native session; mode/bitrate read live. Newest last.
pub fn snapshot() -> Vec<SessionSnapshot> {
    let reg = registry().lock().unwrap();
    reg.iter()
        .map(|s| {
            let (width, height, fps) = crate::native::unpack_mode(s.mode.load(Ordering::Relaxed));
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

/// One launched game as `/status` reports it.
pub struct GameSnapshot {
    /// Streaming session, or `None` if the session is gone and the game is in
    /// its reconnect window.
    pub session_id: Option<u64>,
    pub client: String,
    pub app_id: Option<String>,
    pub title: String,
    pub store: Option<String>,
    pub plane: crate::events::Plane,
    /// `launching` / `running` / `window` / `exited` / `untracked`, or `grace`
    /// on the reconnect window.
    pub state: &'static str,
    /// `running`, and `window` will follow once the game's window is up.
    pub awaiting_window: bool,
    /// Seconds left before the game is ended. Set only on a `grace` row.
    pub grace_remaining_s: Option<u64>,
}

/// Compat plane's launched game, while it has one.
///
/// GameStream is not in the native registry: that holds the loop's `Arc`
/// handles, which the compat plane does not have. One `AppState.launch`
/// means one slot; this is what keeps a Moonlight game on `/status`
/// alongside a native session.
fn gs_game() -> &'static Mutex<Option<Arc<crate::gamelease::LeaseShared>>> {
    static SLOT: OnceLock<Mutex<Option<Arc<crate::gamelease::LeaseShared>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Publish the compat plane's game. The guard retracts it on any stream-loop
/// exit ([`LiveSessionGuard`]'s counterpart).
pub fn publish_gamestream_game(shared: Arc<crate::gamelease::LeaseShared>) -> GamestreamGameGuard {
    *gs_game().lock().unwrap_or_else(|e| e.into_inner()) = Some(shared);
    GamestreamGameGuard
}

/// Retracts the compat plane's published game on drop.
pub struct GamestreamGameGuard;

impl Drop for GamestreamGameGuard {
    fn drop(&mut self) {
        *gs_game().lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// Every launched game the host currently knows: live sessions first, then
/// the compat plane, then games waiting out a reconnect window.
///
/// Sources stay separate — a grace-pending game has no session to hang
/// off, and omitting it would hide "the host is about to close this game".
pub fn games() -> Vec<GameSnapshot> {
    let mut out: Vec<GameSnapshot> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|s| {
            let g = s.game.as_ref()?;
            Some(GameSnapshot {
                session_id: Some(s.id),
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: g.state().as_str(),
                awaiting_window: g.awaits_window(),
                grace_remaining_s: None,
            })
        })
        .collect();
    out.extend(
        gs_game()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|g| GameSnapshot {
                // Compat plane has no session id. State is never `grace` while
                // streaming, so the console tells this from a grace row by state.
                session_id: None,
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: g.state().as_str(),
                awaiting_window: g.awaits_window(),
                grace_remaining_s: None,
            }),
    );
    out.extend(
        crate::gamelease::pending_snapshot()
            .into_iter()
            .map(|(g, remaining)| GameSnapshot {
                session_id: None,
                client: g.client.clone(),
                app_id: g.game.id.clone(),
                title: g.game.title.clone(),
                store: g.game.store.clone(),
                plane: g.plane,
                state: "grace",
                awaiting_window: false,
                grace_remaining_s: Some(remaining),
            }),
    );
    out
}

/// Leases on games that are still on a streaming session, filtered by `app_id`
/// (`None` = all of them). What `POST /game/end` reaches when a title is up
/// rather than waiting out a reconnect window.
pub fn live_games(app_id: Option<&str>) -> Vec<Arc<crate::gamelease::LeaseShared>> {
    let mine =
        |g: &Arc<crate::gamelease::LeaseShared>| app_id.is_none() || g.game.id.as_deref() == app_id;
    let mut out: Vec<Arc<crate::gamelease::LeaseShared>> = registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter_map(|s| s.game.clone())
        .filter(&mine)
        .collect();
    out.extend(
        gs_game()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|g| mine(g))
            .cloned(),
    );
    out
}

/// Tear down every live native session. Best-effort: loops observe the
/// flag and exit; the guard then clears the entry. Not intended teardown —
/// prefer [`stop_all_quit`] for an operator action.
pub fn stop_all() {
    for s in registry().lock().unwrap().iter() {
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
    for s in registry().lock().unwrap().iter() {
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
        .lock()
        .unwrap()
        .iter()
        .any(|s| !(s.client.len() == 12 && fp_hex.starts_with(s.client.as_str())))
}

/// Tear down every live native session deliberately (mgmt `DELETE /session`).
///
/// Sets `quit` before `stop` so teardown matches a client's own Stop: the
/// display skips keep-alive linger and end-game-on-session-end sees intent.
/// The summary says `stopped_by_operator`; the client only ever sees `host_ended`.
pub fn stop_all_quit() {
    for s in registry().lock().unwrap().iter() {
        SessionEndReason::StoppedByOperator.latch(&s.end_reason);
        s.quit.store(true, Ordering::SeqCst);
        s.stop.store(true, Ordering::SeqCst);
    }
}

/// Tear down ONE live native session deliberately (`DELETE /session/{id}`).
/// `false` = no such session. Same `quit`-before-`stop` order and same
/// `stopped_by_operator` summary as [`stop_all_quit`].
pub fn stop_quit(id: u64) -> bool {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|s| s.id == id)
        .is_some_and(|s| {
            SessionEndReason::StoppedByOperator.latch(&s.end_reason);
            s.quit.store(true, Ordering::SeqCst);
            s.stop.store(true, Ordering::SeqCst);
            true
        })
}

/// Force a keyframe on ONE live native session (`POST /session/{id}/idr`).
/// `false` = no such session.
pub fn force_idr(id: u64) -> bool {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|s| s.id == id)
        .is_some_and(|s| {
            s.force_idr.store(true, Ordering::Relaxed);
            true
        })
}

/// Whether this session is served by the plane whose per-session mute, access and player
/// lanes exist. `None` = no such session. The compat plane registers (so it has an id, a
/// stop and a keyframe) but carries none of those three.
pub fn has_native_lanes(id: u64) -> Option<bool> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.plane == crate::events::Plane::Native)
}

/// This session's management handles, cloned out so the caller acts without the
/// registry lock. `None` = no such session (the routes' 404).
pub fn controls(id: u64) -> Option<SessionControls> {
    registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.controls.clone())
}

/// Force a keyframe on every live native session (`POST /session/idr`).
/// The encode loop drains the flag like a client decode-recovery request.
pub fn force_idr_all() {
    for s in registry().lock().unwrap().iter() {
        s.force_idr.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A compat-plane session is a registry entry like any other, which is what gives the
    /// console an id to stop — before, a Moonlight session could only be ended host-wide.
    #[test]
    fn a_compat_session_is_stoppable_by_its_own_id() {
        // No serializing lock: the registry is shared, but this is its only compat session.
        let stop = Arc::new(AtomicBool::new(false));
        let quit = Arc::new(AtomicBool::new(false));
        let _guard = register(Registration {
            mode: Arc::new(AtomicU64::new(0)),
            bitrate_kbps: Arc::new(AtomicU32::new(20_000)),
            codec: Codec::H265,
            stop: stop.clone(),
            quit: quit.clone(),
            force_idr: Arc::new(AtomicBool::new(false)),
            client: "9f86d0818840".into(),
            client_name: Some("Living Room TV".into()),
            plane: crate::events::Plane::Gamestream,
            hdr: true,
            ttff_ms: Arc::new(AtomicU32::new(0)),
            last_resize_ms: Arc::new(AtomicU32::new(0)),
            game: None,
            capture_health: Arc::new(Mutex::new(None)),
            join: false,
            controls: SessionControls::open(),
            bit_depth: 10,
            chroma: ChromaFormat::Yuv420,
            end_reason: Arc::new(AtomicU8::new(0)),
            counters: Arc::new(SessionCounters::default()),
            peer: None,
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
            mode: Arc::new(AtomicU64::new(0)),
            bitrate_kbps: Arc::new(AtomicU32::new(20_000)),
            codec: Codec::H265,
            stop: stop.clone(),
            quit: quit.clone(),
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
            end_reason,
            counters,
            peer: None,
        });
        (guard, stop, quit)
    }

    fn fake_joiner(client: &str, join: bool) -> (LiveSessionGuard, SessionControls) {
        fake_at(client, join, None)
    }

    fn fake_at(
        client: &str,
        join: bool,
        peer: Option<std::net::IpAddr>,
    ) -> (LiveSessionGuard, SessionControls) {
        let controls = SessionControls::open();
        let guard = register(Registration {
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
            join,
            controls: controls.clone(),
            bit_depth: 8,
            chroma: ChromaFormat::Yuv420,
            end_reason: Arc::new(AtomicU8::new(0)),
            counters: Arc::new(SessionCounters::default()),
            peer,
        });
        (guard, controls)
    }

    /// One address is one NAT or tunnel: each session names the others there, and an
    /// IPv4-mapped IPv6 peer is the same address.
    #[test]
    fn sessions_from_one_address_name_each_other() {
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

    #[test]
    fn policy_picks_the_sessions_it_names() {
        use AudioSessions::*;
        // (policy, client, join) → muted
        for (policy, client, join, want) in [
            (All, "aaaaaaaaaaaa", true, false),
            (Owner, "aaaaaaaaaaaa", false, false),
            (Owner, "bbbbbbbbbbbb", true, true),
            (Joined, "aaaaaaaaaaaa", false, true),
            (Joined, "bbbbbbbbbbbb", true, false),
            (Launcher, "aaaaaaaaaaaa", false, false),
            (Launcher, "bbbbbbbbbbbb", true, true),
        ] {
            assert_eq!(
                policy_mutes(policy, "aaaaaaaaaaaa", client, join),
                want,
                "{policy:?} {client} join={join}"
            );
        }
    }

    /// A policy mutes by role, reaches a joiner that registers later, and its end
    /// unmutes only what it muted: an operator's own mute stays.
    #[test]
    fn policy_mute_reaches_late_joiners_and_lifts_at_the_end() {
        let (_owner, owner) = fake_joiner("cccccccccccc", false);
        let guard = apply_audio_policy(AudioSessions::Owner, "cccccccccccc");
        let (_joiner, joiner) = fake_joiner("dddddddddddd", true);
        assert!(!owner.muted.load(Ordering::SeqCst));
        assert!(
            joiner.muted.load(Ordering::SeqCst),
            "a joiner registered under the policy is muted"
        );
        owner.set_muted(true);
        drop(guard);
        assert!(
            !joiner.muted.load(Ordering::SeqCst),
            "the lease ending lifts the policy mute"
        );
        assert!(
            owner.muted.load(Ordering::SeqCst),
            "the operator's mute outlives the policy"
        );
    }

    /// Unpair revokes a live session by the 12-hex fingerprint prefix
    /// (`quit` + `stop`). Other clients and IP-labelled sessions stay up.
    #[test]
    fn stop_by_fingerprint_revokes_exactly_the_unpaired_client() {
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

    /// A Moonlight game has no live-session entry, so it is only on `/status`
    /// while [`publish_gamestream_game`]'s guard is alive.
    #[test]
    fn a_gamestream_game_is_visible_only_while_its_stream_runs() {
        let id = "steam:1701";
        let mine = || {
            games()
                .into_iter()
                .find(|g| g.app_id.as_deref() == Some(id))
        };

        let lease = crate::gamelease::open(
            crate::gamelease::LeaseRequest {
                game: crate::gamelease::GameRef {
                    id: Some(id.to_string()),
                    store: Some("steam".into()),
                    title: "Test Title".into(),
                },
                client: "192.0.2.7".into(),
                fingerprint: None,
                plane: crate::events::Plane::Gamestream,
                // No signals: inert lease, so no watcher thread races the assertions.
                spec: crate::library::DetectSpec::default(),
                nested: false,
                launcher: false,
                child: None,
                spawned: None,
                launch_stamp: None,
                procs: None,
                #[cfg(target_os = "linux")]
                workspace: None,
                window: None,
                outcome: None,
            },
            Box::new(|| {}),
        );
        assert!(mine().is_none(), "not published yet");

        {
            let _pub = publish_gamestream_game(lease.shared());
            let row = mine().expect("the compat plane's game is reported");
            assert_eq!(
                row.session_id, None,
                "no live-session entry to attribute it to"
            );
            assert_eq!(row.plane, crate::events::Plane::Gamestream);
            assert_eq!(row.client, "192.0.2.7");
            assert_eq!(row.title, "Test Title");
            // Not `grace` while the stream is up: the console keys
            // countdown / End now off that state.
            assert_ne!(row.state, "grace");
            // The same row is what `POST /game/end` reaches with `streaming`,
            // and only ever under its own id.
            assert_eq!(live_games(Some(id)).len(), 1);
            assert!(live_games(Some("steam:9999")).is_empty());
        }
        assert!(mine().is_none(), "the row goes with the stream");
        assert!(
            live_games(Some(id)).is_empty(),
            "a game with no stream left is the grace registry's, not this list's"
        );
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
