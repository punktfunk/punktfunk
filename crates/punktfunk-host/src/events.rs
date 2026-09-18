//! Host lifecycle event bus: process-wide broadcast plus a bounded catch-up ring.
//!
//! Fire sites on both planes call [`emit`]. [`EventBus::subscribe`] returns ring catch-up
//! plus a live tail — the shape `GET /api/v1/events` (SSE) and the hook runner consume.
//! Same ring shape as [`crate::log_capture`].
//!
//! Resume with `since = last seen seq` (1-based). A consumer that fell off the ring gets
//! `dropped = true` and resyncs via REST snapshots. Wire shape is additive-only within
//! [`SCHEMA_VERSION`]; the JSON snapshot tests below are the review gate.
//!
//! Emission is fire-and-forget (mutex push + non-blocking broadcast). Slow consumers lag
//! (`RecvError::Lagged`) rather than buffering unboundedly.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use utoipa::ToSchema;

/// Additive-only within a major. Removing or renaming a field is a new major.
pub const SCHEMA_VERSION: u32 = 1;

/// Events are small and low-rate (lifecycle, not per-frame); 1024 spans hours of ordinary host activity.
const RING_CAPACITY: usize = 1024;
/// Per-subscriber live-tail depth before a slow consumer starts lagging.
const BROADCAST_CAPACITY: usize = 256;

/// One lifecycle event as it appears on the wire (`data:` of one SSE frame).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct HostEvent {
    /// 1-based; a consumer resumes with `since = last seen`.
    pub seq: u64,
    /// Unix milliseconds — the [`crate::log_capture::LogEntry`] convention.
    pub ts_ms: u64,
    pub schema: u32,
    /// Flattened as `"kind": "stream.started"` plus payload fields.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Origin plane. Both planes must emit; filtering is the consumer's job.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Plane {
    Native,
    Gamestream,
}

/// `Quit` is the typed close; `Timeout` is transport idle; `Error` is everything else.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DisconnectReason {
    Quit,
    Timeout,
    Error,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct ClientRef {
    /// Display name: the trust-store name (a console rename wins), else the name the client
    /// sent. On the compat plane it is the name the operator gave the device, since Moonlight
    /// sends none. Empty when there is no name at all.
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub plane: Plane,
}

/// Plane-neutral A/V session (distinct from a video [`StreamRef`]).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct SessionRef {
    pub id: u64,
    /// Cert-fingerprint prefix, or peer IP for an anonymous client — not [`ClientRef::name`].
    pub client: String,
    /// The device's full stable id, for a hook filter. Absent for an anonymous client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// `WxH@Hz`, e.g. `"3840x2160@120"`.
    pub mode: String,
    pub hdr: bool,
    /// Which plane serves it, as `stream.*` and `game.*` also report.
    pub plane: Plane,
}

/// Why a session ended, in the client's own words
/// ([`punktfunk_core::client::PunktfunkEndReason`]) so both ends name the same end the
/// same way. The bytes match that ABI where the two overlap.
///
/// [`Self::StoppedByOperator`] is the one a client cannot see: the host closes it as
/// code 0, which every client reads as `host_ended`.
#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum SessionEndReason {
    /// The client hung up — its Stop, or its process went away.
    Local = 1,
    /// The launched game exited and the session followed it (`APP_EXITED`).
    GameExited = 2,
    /// The stream loop finished cleanly.
    HostEnded = 3,
    /// The stream loop bailed. The host log above the summary has the cause.
    HostError = 4,
    /// The link died: idle timeout, reset, network.
    Lost = 5,
    /// `DELETE /session`, `DELETE /session/{id}`, or unpairing a streaming client.
    StoppedByOperator = 6,
}

impl SessionEndReason {
    /// `0` is "nothing latched": the video loop never reached its clean tail.
    pub fn from_u8(v: u8) -> Option<SessionEndReason> {
        Some(match v {
            1 => Self::Local,
            2 => Self::GameExited,
            3 => Self::HostEnded,
            4 => Self::HostError,
            5 => Self::Lost,
            6 => Self::StoppedByOperator,
            _ => return None,
        })
    }

    /// First writer wins. The path that knows why latches before teardown reaches the
    /// generic one, so a game exit is never overwritten by the close that follows it.
    pub fn latch(self, slot: &std::sync::atomic::AtomicU8) {
        use std::sync::atomic::Ordering;
        let _ = slot.compare_exchange(0, self as u8, Ordering::SeqCst, Ordering::Relaxed);
    }
}

/// Client datagrams this session took, by class.
///
/// Counted up to the moment the session was summarized: the reader task ends with the
/// connection, which closes after this, so a last straggler can fall outside.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct InputCounts {
    /// Keyboard, pointer and touch events.
    pub events: u64,
    /// Opus microphone frames.
    pub mic: u64,
    /// Gamepad and stylus batches.
    pub rich: u64,
    /// Offers the input queue refused because it was full. Not a wire loss.
    pub dropped: u64,
}

/// Client gyro arrival counts. Absent when no pad ever sent motion.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct GyroCadence {
    pub samples: u64,
    /// Gaps of 500 ms or more — the feed stopping, not jitter.
    pub stalls: u64,
}

/// Audio egress for the whole session. Absent when the session had no audio plane.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct AudioEgress {
    pub sent: u64,
    /// Frames synthesized over a capture hole. Wire continuity is not captured continuity.
    pub infilled: u64,
    /// Departures a whole frame or more behind their slot.
    pub late: u64,
    pub max_late_ms: u64,
    /// Times the pacer fell far enough behind to forgive its debt and re-anchor.
    pub reanchors: u64,
}

/// What the encoder's target did over the session. Absent when no encoder opened.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct BitrateSpan {
    pub min_kbps: u32,
    /// Mean of the targets the session ran at, NOT weighted by how long each held.
    /// A rate held for a second counts as much as one held for an hour.
    pub avg_kbps: u32,
    pub max_kbps: u32,
    /// Times the target moved after the opening rate: adaptive-bitrate decisions, plus a
    /// rebuild re-resolving what it actually encodes.
    pub adaptive_steps: u32,
}

/// Everything the host knows about one finished session.
///
/// The `session.ended` payload AND the body of `GET /api/v1/session/last`: one struct,
/// so a screenshot of the console card is the API answer a bug report would have carried.
/// A number the host does not accumulate per session is absent rather than zero.
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct SessionSummary {
    /// Same id [`SessionRef`] and the per-session routes carry.
    pub id: u64,
    /// Cert-fingerprint prefix, or peer IP for an anonymous client.
    pub client: String,
    /// Display name (trust store, else the name the client sent). Absent if nameless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// Host wall clock at stream start, unix seconds.
    pub started_unix: i64,
    pub duration_s: u64,
    /// `WxH@Hz` as last delivered; a mid-session mode switch moves it.
    pub mode: String,
    pub hdr: bool,
    /// Shared another session's display at ITS mode instead of owning one.
    pub join: bool,
    /// `h264` | `hevc` | `av1` | `pyrowave`.
    pub codec: String,
    /// 8 or 10. Independent of `hdr` — 10-bit SDR is a mode.
    pub bit_depth: u8,
    /// `4:2:0` or `4:4:4`.
    pub chroma: String,
    /// The encoder's target when the session ended, kbps. `bitrate` has the span.
    pub bitrate_kbps: u32,
    /// What the encoder's target did. Absent on a session that opened no encoder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<BitrateSpan>,
    /// Access units the send thread put on the wire. Absent when the video loop
    /// never reached its tail — the `host_error` case.
    #[schema(value_type = u64, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames_sent: Option<u64>,
    /// Frames the capturer's encode pool refused. Absent on a capturer that does not
    /// count them (everything but the Windows IDD push path).
    #[schema(value_type = u64, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frames_dropped: Option<u64>,
    pub input: InputCounts,
    /// Absent when no pad sent motion — most sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gyro: Option<GyroCadence>,
    /// Absent when the session ran no audio plane (synthetic source, or the thread never started).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioEgress>,
    /// Hello → first video packet, ms. `0` if no packet ever left.
    pub bringup_ms: u32,
    /// Path MTU the QUIC stack settled on, bytes. Absent as `frames_sent` is.
    #[schema(value_type = u16, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_mtu: Option<u16>,
    pub ended: SessionEndReason,
}

/// Live video stream (what the stream marker file reflects).
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct StreamRef {
    /// `WxH@Hz`.
    pub mode: String,
    pub hdr: bool,
    pub client: String,
    /// The device's stable id — what a hook filter should key on, since a display name is
    /// neither unique nor fixed. Absent for an anonymous client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Store-qualified id on the native plane, app title on GameStream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub plane: Plane,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct GameRefPayload {
    /// Store-qualified id (`steam:570`). Absent for an operator-typed GameStream `apps.json` command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub title: String,
    /// `steam`, `heroic`, `custom`, … when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
    pub client: String,
    /// Stable id of the device that launched it; the filter handle a name cannot be.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    pub plane: Plane,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GameEndReason {
    /// The player quit or it crashed — the host did not ask.
    Exited,
    /// The host ended it, per the session⇄game lifetime policy.
    Terminated,
}

#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
pub struct DeviceRef {
    /// Pairing-store copy, already sanitized.
    pub name: String,
    pub fingerprint: String,
    pub plane: Plane,
}

/// Internally tagged `"kind": "<domain>.<verb>"`, flattened into [`HostEvent`].
/// Additive-only within [`SCHEMA_VERSION`].
#[derive(Serialize, Deserialize, ToSchema, Clone, Debug)]
#[serde(tag = "kind")]
pub enum EventKind {
    #[serde(rename = "client.connected")]
    ClientConnected { client: ClientRef },
    #[serde(rename = "client.disconnected")]
    ClientDisconnected {
        client: ClientRef,
        reason: DisconnectReason,
    },
    #[serde(rename = "session.started")]
    SessionStarted { session: SessionRef },
    #[serde(rename = "session.ended")]
    SessionEnded {
        session: SessionRef,
        /// Every number the host has for the finished session, including why it ended.
        // Boxed, not inline: every variant of this enum would otherwise pay its size.
        summary: Box<SessionSummary>,
    },
    #[serde(rename = "stream.started")]
    StreamStarted { stream: StreamRef },
    #[serde(rename = "stream.stopped")]
    StreamStopped { stream: StreamRef },
    /// Fires once the host has seen the game process, not merely spawned its launcher.
    #[serde(rename = "game.running")]
    GameRunning { game: GameRefPayload },
    /// Fires when the game's own window reaches the screen, which is often far
    /// later than its process: a Proton prefix build, a splash on a black
    /// window, an emulator loading a ROM.
    #[serde(rename = "game.window")]
    GameWindow {
        game: GameRefPayload,
        /// Title the compositor or the desktop reports for that window.
        title: String,
        /// Wayland `app_id` or X11 class; `steam_app_<id>` under gamescope; empty on Windows.
        app_id: String,
    },
    #[serde(rename = "game.exited")]
    GameExited {
        game: GameRefPayload,
        reason: GameEndReason,
    },
    #[serde(rename = "pairing.pending")]
    PairingPending { device: DeviceRef },
    #[serde(rename = "pairing.completed")]
    PairingCompleted { device: DeviceRef },
    #[serde(rename = "pairing.denied")]
    PairingDenied { device: DeviceRef },
    /// Explicit operator choice (`add_with_access(Some)`). A pairing with no choice
    /// emits only `pairing.completed` — see `design/per-client-access.md`.
    #[serde(rename = "access.granted")]
    AccessGranted {
        device: DeviceRef,
        /// `GRANT_*` bits; reserved bits already cleared.
        grants: u32,
        /// Host wall-clock unix seconds; absent = permanent.
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_unix: Option<i64>,
    },
    /// Post-pairing edit (console sheet / extend / expire-now), not the original grant.
    #[serde(rename = "access.changed")]
    AccessChanged {
        device: DeviceRef,
        grants: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        expires_unix: Option<i64>,
    },
    /// Deadline fire on a live session. A device with no session expires silently.
    #[serde(rename = "access.expired")]
    AccessExpired { device: DeviceRef },
    #[serde(rename = "display.created")]
    DisplayCreated {
        /// `VirtualDisplay::name` of the backend that minted it.
        backend: String,
        /// `WxH@Hz`.
        mode: String,
    },
    #[serde(rename = "display.released")]
    DisplayReleased {
        /// How many kept displays this release retired.
        count: u32,
    },
    #[serde(rename = "library.changed")]
    LibraryChanged {
        /// `"manual"`, or a provider id.
        source: String,
    },
    /// Once per discovered version; a steady "newer exists" does not re-fire on every refresh.
    #[serde(rename = "update.available")]
    UpdateAvailable {
        version: String,
        /// `stable` or `canary`.
        channel: String,
        /// `apt`, `windows-installer`, … so a hook can hint how to update without a second call.
        install_kind: String,
    },
    /// Boot-time reconciliation by the NEW binary after a successful apply.
    #[serde(rename = "update.applied")]
    UpdateApplied { from: String, to: String },
    #[serde(rename = "plugins.changed")]
    PluginsChanged {
        /// Plugin that registered, restarted, deregistered, or lease-expired. Re-read `GET /api/v1/plugins`.
        id: String,
    },
    #[serde(rename = "store.changed")]
    /// Payload-free: the store is a join. Re-read `GET /api/v1/store/catalog` / `…/installed`.
    StoreChanged,
    /// The operator changed host settings. Re-read `GET /api/v1/host/settings`.
    #[serde(rename = "settings.changed")]
    SettingsChanged {
        /// Setting ids the write named.
        ids: Vec<String>,
    },
    /// Emitted on ACCEPT, and again if the executor later fails. A succeeded power
    /// action ends this process, so "accepted with no later failure" is success.
    #[serde(rename = "action.invoked")]
    ActionInvoked {
        /// `power.sleep`, `power.reboot`, `power.shutdown`, `host.restart`.
        id: String,
        /// Cert-lane invoker; absent for the operator console (admin lane).
        #[serde(skip_serializing_if = "Option::is_none")]
        device: Option<DeviceRef>,
        /// `accepted`, or `failed: <executor error>`.
        outcome: String,
    },
    #[serde(rename = "host.started")]
    HostStarted { version: String, gamestream: bool },
    #[serde(rename = "host.stopping")]
    HostStopping,
}

impl EventKind {
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::ClientConnected { .. } => "client.connected",
            EventKind::ClientDisconnected { .. } => "client.disconnected",
            EventKind::SessionStarted { .. } => "session.started",
            EventKind::SessionEnded { .. } => "session.ended",
            EventKind::StreamStarted { .. } => "stream.started",
            EventKind::StreamStopped { .. } => "stream.stopped",
            EventKind::GameRunning { .. } => "game.running",
            EventKind::GameWindow { .. } => "game.window",
            EventKind::GameExited { .. } => "game.exited",
            EventKind::PairingPending { .. } => "pairing.pending",
            EventKind::PairingCompleted { .. } => "pairing.completed",
            EventKind::PairingDenied { .. } => "pairing.denied",
            EventKind::AccessGranted { .. } => "access.granted",
            EventKind::AccessChanged { .. } => "access.changed",
            EventKind::AccessExpired { .. } => "access.expired",
            EventKind::DisplayCreated { .. } => "display.created",
            EventKind::DisplayReleased { .. } => "display.released",
            EventKind::LibraryChanged { .. } => "library.changed",
            EventKind::UpdateAvailable { .. } => "update.available",
            EventKind::UpdateApplied { .. } => "update.applied",
            EventKind::PluginsChanged { .. } => "plugins.changed",
            EventKind::StoreChanged => "store.changed",
            EventKind::SettingsChanged { .. } => "settings.changed",
            EventKind::ActionInvoked { .. } => "action.invoked",
            EventKind::HostStarted { .. } => "host.started",
            EventKind::HostStopping => "host.stopping",
        }
    }
}

impl EventKind {
    /// `filter.client` axis. For `session.*` this is the short label (fingerprint prefix or
    /// peer IP), not [`ClientRef::name`].
    pub fn client_name(&self) -> Option<&str> {
        match self {
            EventKind::ClientConnected { client }
            | EventKind::ClientDisconnected { client, .. } => Some(&client.name),
            EventKind::SessionStarted { session } | EventKind::SessionEnded { session, .. } => {
                Some(&session.client)
            }
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                Some(&stream.client)
            }
            EventKind::GameRunning { game }
            | EventKind::GameWindow { game, .. }
            | EventKind::GameExited { game, .. } => Some(&game.client),
            EventKind::PairingPending { device }
            | EventKind::PairingCompleted { device }
            | EventKind::PairingDenied { device }
            | EventKind::AccessGranted { device, .. }
            | EventKind::AccessChanged { device, .. }
            | EventKind::AccessExpired { device } => Some(&device.name),
            EventKind::ActionInvoked { device, .. } => device.as_ref().map(|d| d.name.as_str()),
            _ => None,
        }
    }

    pub fn fingerprint(&self) -> Option<&str> {
        match self {
            EventKind::ClientConnected { client }
            | EventKind::ClientDisconnected { client, .. } => client.fingerprint.as_deref(),
            EventKind::PairingPending { device }
            | EventKind::PairingCompleted { device }
            | EventKind::PairingDenied { device }
            | EventKind::AccessGranted { device, .. }
            | EventKind::AccessChanged { device, .. }
            | EventKind::AccessExpired { device } => Some(&device.fingerprint),
            EventKind::ActionInvoked { device, .. } => {
                device.as_ref().map(|d| d.fingerprint.as_str())
            }
            EventKind::SessionStarted { session } | EventKind::SessionEnded { session, .. } => {
                session.fingerprint.as_deref()
            }
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                stream.fingerprint.as_deref()
            }
            EventKind::GameRunning { game }
            | EventKind::GameWindow { game, .. }
            | EventKind::GameExited { game, .. } => game.fingerprint.as_deref(),
            _ => None,
        }
    }

    pub fn plane(&self) -> Option<Plane> {
        match self {
            EventKind::ClientConnected { client }
            | EventKind::ClientDisconnected { client, .. } => Some(client.plane),
            EventKind::SessionStarted { session } | EventKind::SessionEnded { session, .. } => {
                Some(session.plane)
            }
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                Some(stream.plane)
            }
            EventKind::GameRunning { game }
            | EventKind::GameWindow { game, .. }
            | EventKind::GameExited { game, .. } => Some(game.plane),
            EventKind::PairingPending { device }
            | EventKind::PairingCompleted { device }
            | EventKind::PairingDenied { device }
            | EventKind::AccessGranted { device, .. }
            | EventKind::AccessChanged { device, .. }
            | EventKind::AccessExpired { device } => Some(device.plane),
            _ => None,
        }
    }

    pub fn app(&self) -> Option<&str> {
        match self {
            EventKind::StreamStarted { stream } | EventKind::StreamStopped { stream } => {
                stream.app.as_deref()
            }
            // No library id: operator-typed command; title is the only hook-filter handle.
            EventKind::GameRunning { game }
            | EventKind::GameWindow { game, .. }
            | EventKind::GameExited { game, .. } => game.app.as_deref().or(Some(&game.title)),
            _ => None,
        }
    }
}

/// Exact kind (`stream.started`) or `domain.*` on the dot boundary (`stream.*` never
/// matches `streamx.started`). Shared by SSE `?kinds=` and hooks `on:`.
pub fn kind_matches(pattern: &str, kind: &str) -> bool {
    match pattern.strip_suffix(".*") {
        Some(prefix) => kind
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.')),
        None => pattern == kind,
    }
}

pub fn mode_str(width: u32, height: u32, hz: u32) -> String {
    format!("{width}x{height}@{hz}")
}

/// Catch-up plus live tail, taken atomically: no event falls between `catch_up` and
/// the first `rx.recv()`, and none is in both.
pub struct Subscription {
    /// `seq > since`, oldest first.
    pub catch_up: Vec<HostEvent>,
    /// Events between `since` and the first caught-up one were evicted; resync via REST.
    pub dropped: bool,
    /// Live tail. A slow consumer sees `RecvError::Lagged`.
    pub rx: broadcast::Receiver<HostEvent>,
}

pub struct EventBus {
    inner: Mutex<Ring>,
    tx: broadcast::Sender<HostEvent>,
}

struct Ring {
    events: VecDeque<HostEvent>,
    next_seq: u64,
}

impl EventBus {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Mutex::new(Ring {
                events: VecDeque::with_capacity(RING_CAPACITY),
                next_seq: 1,
            }),
            tx,
        }
    }

    /// Fire-and-forget. No receivers is fine — the ring still records for later catch-up.
    pub fn emit(&self, kind: EventKind) {
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let mut ring = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let ev = HostEvent {
            seq: ring.next_seq,
            ts_ms,
            schema: SCHEMA_VERSION,
            kind,
        };
        ring.next_seq += 1;
        if ring.events.len() == RING_CAPACITY {
            ring.events.pop_front();
        }
        ring.events.push_back(ev.clone());
        // Hold the ring lock across `send` so it serializes with `subscribe`: an event
        // lands in catch-up or on the live tail — never both, never neither. `send` is
        // non-blocking, so the hold is trivial.
        let _ = self.tx.send(ev);
    }

    /// Live tail only (no catch-up, no cursor) — for consumers that care from now on.
    pub fn subscribe_live(&self) -> broadcast::Receiver<HostEvent> {
        self.tx.subscribe()
    }

    /// Events with `seq > since` as catch-up; the receiver carries everything after.
    /// `since = 0` means from the ring start.
    pub fn subscribe(&self, since: u64) -> Subscription {
        let ring = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        let first_seq = ring.events.front().map_or(ring.next_seq, |e| e.seq);
        let dropped = since != 0 && since.saturating_add(1) < first_seq;
        let catch_up = ring
            .events
            .iter()
            .filter(|e| e.seq > since)
            .cloned()
            .collect();
        Subscription {
            catch_up,
            dropped,
            rx,
        }
    }
}

/// Process-wide `OnceLock` (the [`crate::log_capture::ring`] shape) so fire sites
/// share it without threading an `Arc`.
pub fn bus() -> &'static EventBus {
    static BUS: OnceLock<EventBus> = OnceLock::new();
    BUS.get_or_init(EventBus::new)
}

/// Non-blocking; safe from any thread, including RAII `Drop` paths.
pub fn emit(kind: EventKind) {
    bus().emit(kind);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(name: &str) -> EventKind {
        EventKind::LibraryChanged {
            source: name.to_string(),
        }
    }

    #[test]
    fn seq_is_monotonic_and_catch_up_resumes() {
        let bus = EventBus::new();
        for i in 0..5 {
            bus.emit(ev(&format!("m{i}")));
        }
        let sub = bus.subscribe(0);
        assert_eq!(
            sub.catch_up.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5]
        );
        assert!(!sub.dropped);
        assert!(sub.catch_up.iter().all(|e| e.schema == SCHEMA_VERSION));

        let sub = bus.subscribe(3);
        assert_eq!(
            sub.catch_up.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![4, 5]
        );
        assert!(!sub.dropped);

        // Cursor at the tip: empty catch-up, not a gap.
        let sub = bus.subscribe(5);
        assert!(sub.catch_up.is_empty());
        assert!(!sub.dropped);
    }

    #[test]
    fn eviction_reports_dropped() {
        let bus = EventBus::new();
        for i in 0..(RING_CAPACITY + 50) {
            bus.emit(ev(&format!("m{i}")));
        }
        let sub = bus.subscribe(10);
        assert!(sub.dropped);
        assert_eq!(sub.catch_up.first().map(|e| e.seq), Some(51));
        // A fresh consumer (since = 0) is a backfill, not a gap.
        let sub = bus.subscribe(0);
        assert!(!sub.dropped);
        assert_eq!(sub.catch_up.len(), RING_CAPACITY);
    }

    #[tokio::test]
    async fn live_tail_continues_exactly_after_catch_up() {
        let bus = EventBus::new();
        bus.emit(ev("before-1"));
        bus.emit(ev("before-2"));
        let mut sub = bus.subscribe(0);
        assert_eq!(sub.catch_up.len(), 2);
        bus.emit(ev("after"));
        let live = sub.rx.recv().await.expect("live event");
        assert_eq!(live.seq, 3);
        assert_eq!(live.kind.name(), "library.changed");
        assert!(sub.rx.try_recv().is_err());
    }

    /// The reporter's requirement, asserted rather than documented: the host names an end
    /// with the byte the client already decodes from the QUIC close.
    #[test]
    fn the_end_reason_bytes_are_the_clients_own() {
        use punktfunk_core::client::PunktfunkEndReason as Client;

        for (host, client) in [
            (SessionEndReason::Local, Client::Local),
            (SessionEndReason::GameExited, Client::GameExited),
            (SessionEndReason::HostEnded, Client::HostEnded),
            (SessionEndReason::HostError, Client::HostError),
            (SessionEndReason::Lost, Client::Lost),
        ] {
            assert_eq!(host as u8, client as u8, "{host:?} must be {client:?}");
            assert_eq!(SessionEndReason::from_u8(host as u8), Some(host));
        }

        // Host-only: a client sees the close code 0 an operator stop rides on as `HostEnded`.
        assert_eq!(SessionEndReason::StoppedByOperator as u8, 6);
        assert_eq!(
            SessionEndReason::from_u8(6),
            Some(SessionEndReason::StoppedByOperator)
        );
        // 0 is "nothing latched" and 7 is a byte no one writes; neither is an end.
        assert_eq!(SessionEndReason::from_u8(0), None);
        assert_eq!(SessionEndReason::from_u8(7), None);
    }

    /// First write wins, which is what keeps a game exit from becoming the `Lost` the
    /// local close that follows it would otherwise latch.
    #[test]
    fn the_first_latched_reason_is_the_one_reported() {
        let slot = std::sync::atomic::AtomicU8::new(0);
        SessionEndReason::GameExited.latch(&slot);
        SessionEndReason::Lost.latch(&slot);
        SessionEndReason::HostEnded.latch(&slot);
        assert_eq!(
            SessionEndReason::from_u8(slot.load(std::sync::atomic::Ordering::SeqCst)),
            Some(SessionEndReason::GameExited)
        );
    }

    /// A paired device's id in the snapshots below.
    const FP_SNAPSHOT: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    /// Additive-only wire contract: a failing snapshot is a schema-version bump, not a test update.
    #[test]
    fn wire_shape_snapshots() {
        let ev = HostEvent {
            seq: 4182,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::StreamStarted {
                stream: StreamRef {
                    mode: mode_str(3840, 2160, 120),
                    hdr: true,
                    client: "Living Room TV".into(),
                    fingerprint: Some(FP_SNAPSHOT.into()),
                    app: Some("steam:570".into()),
                    plane: Plane::Native,
                },
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":4182,"ts_ms":1700000000000,"schema":1,"kind":"stream.started","stream":{"mode":"3840x2160@120","hdr":true,"client":"Living Room TV","fingerprint":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","app":"steam:570","plane":"native"}}"#
        );

        let ev = HostEvent {
            seq: 1,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::ClientDisconnected {
                client: ClientRef {
                    name: "Deck".into(),
                    fingerprint: Some("b1c2".into()),
                    plane: Plane::Gamestream,
                },
                reason: DisconnectReason::Timeout,
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":1,"ts_ms":1700000000000,"schema":1,"kind":"client.disconnected","client":{"name":"Deck","fingerprint":"b1c2","plane":"gamestream"},"reason":"timeout"}"#
        );

        let ev = HostEvent {
            seq: 2,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::HostStopping,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":2,"ts_ms":1700000000000,"schema":1,"kind":"host.stopping"}"#
        );

        let ev = HostEvent {
            seq: 3,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::PluginsChanged {
                id: "rom-manager".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":3,"ts_ms":1700000000000,"schema":1,"kind":"plugins.changed","id":"rom-manager"}"#
        );

        let ev = HostEvent {
            seq: 5,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::GameRunning {
                game: GameRefPayload {
                    app: Some("steam:570".into()),
                    title: "Dota 2".into(),
                    store: Some("steam".into()),
                    client: "Living Room TV".into(),
                    fingerprint: Some(FP_SNAPSHOT.into()),
                    plane: Plane::Native,
                },
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":5,"ts_ms":1700000000000,"schema":1,"kind":"game.running","game":{"app":"steam:570","title":"Dota 2","store":"steam","client":"Living Room TV","fingerprint":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","plane":"native"}}"#
        );

        // Optional ids omitted, not nulled — host-ended game with no library entry.
        let ev = HostEvent {
            seq: 6,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::GameExited {
                game: GameRefPayload {
                    app: None,
                    title: "Big Picture".into(),
                    store: None,
                    client: String::new(),
                    fingerprint: None,
                    plane: Plane::Gamestream,
                },
                reason: GameEndReason::Terminated,
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":6,"ts_ms":1700000000000,"schema":1,"kind":"game.exited","game":{"title":"Big Picture","client":"","plane":"gamestream"},"reason":"terminated"}"#
        );

        // Every field the summary claims, in order. The console card reads this shape, so a
        // renamed or dropped key is a console break as well as a schema bump.
        let ev = HostEvent {
            seq: 7,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::SessionEnded {
                session: SessionRef {
                    id: 3,
                    client: "a1b2c3d4e5f6".into(),
                    fingerprint: Some(FP_SNAPSHOT.into()),
                    mode: mode_str(1920, 1080, 30),
                    hdr: false,
                    plane: Plane::Native,
                },
                summary: Box::new(SessionSummary {
                    id: 3,
                    client: "a1b2c3d4e5f6".into(),
                    client_name: Some("Living Room TV".into()),
                    started_unix: 1_700_000_000,
                    duration_s: 2460,
                    mode: mode_str(1920, 1080, 30),
                    hdr: false,
                    join: false,
                    codec: "hevc".into(),
                    bit_depth: 8,
                    chroma: "4:2:0".into(),
                    bitrate_kbps: 12_400,
                    bitrate: Some(BitrateSpan {
                        min_kbps: 9_000,
                        avg_kbps: 12_100,
                        max_kbps: 15_000,
                        adaptive_steps: 4,
                    }),
                    frames_sent: Some(73_800),
                    frames_dropped: Some(0),
                    input: InputCounts {
                        events: 7457,
                        mic: 0,
                        rich: 150_983,
                        dropped: 0,
                    },
                    gyro: Some(GyroCadence {
                        samples: 150_983,
                        stalls: 0,
                    }),
                    audio: Some(AudioEgress {
                        sent: 492_000,
                        infilled: 12,
                        late: 3,
                        max_late_ms: 11,
                        reanchors: 1,
                    }),
                    bringup_ms: 603,
                    path_mtu: Some(1369),
                    ended: SessionEndReason::GameExited,
                }),
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":7,"ts_ms":1700000000000,"schema":1,"kind":"session.ended","session":{"id":3,"client":"a1b2c3d4e5f6","fingerprint":"9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08","mode":"1920x1080@30","hdr":false,"plane":"native"},"summary":{"id":3,"client":"a1b2c3d4e5f6","client_name":"Living Room TV","started_unix":1700000000,"duration_s":2460,"mode":"1920x1080@30","hdr":false,"join":false,"codec":"hevc","bit_depth":8,"chroma":"4:2:0","bitrate_kbps":12400,"bitrate":{"min_kbps":9000,"avg_kbps":12100,"max_kbps":15000,"adaptive_steps":4},"frames_sent":73800,"frames_dropped":0,"input":{"events":7457,"mic":0,"rich":150983,"dropped":0},"gyro":{"samples":150983,"stalls":0},"audio":{"sent":492000,"infilled":12,"late":3,"max_late_ms":11,"reanchors":1},"bringup_ms":603,"path_mtu":1369,"ended":"game_exited"}}"#
        );

        // A loop that bailed has no totals: every optional one is omitted, never zeroed.
        // `input` stays: the datagram reader runs from before the session registers.
        let ev = HostEvent {
            seq: 8,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::SessionEnded {
                session: SessionRef {
                    id: 4,
                    client: "192.0.2.7".into(),
                    fingerprint: None,
                    mode: mode_str(0, 0, 0),
                    hdr: false,
                    plane: Plane::Native,
                },
                summary: Box::new(SessionSummary {
                    id: 4,
                    client: "192.0.2.7".into(),
                    client_name: None,
                    started_unix: 1_700_000_000,
                    duration_s: 1,
                    mode: mode_str(0, 0, 0),
                    hdr: false,
                    join: false,
                    codec: "av1".into(),
                    bit_depth: 10,
                    chroma: "4:4:4".into(),
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
                    ended: SessionEndReason::HostError,
                }),
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":8,"ts_ms":1700000000000,"schema":1,"kind":"session.ended","session":{"id":4,"client":"192.0.2.7","mode":"0x0@0","hdr":false,"plane":"native"},"summary":{"id":4,"client":"192.0.2.7","started_unix":1700000000,"duration_s":1,"mode":"0x0@0","hdr":false,"join":false,"codec":"av1","bit_depth":10,"chroma":"4:4:4","bitrate_kbps":0,"input":{"events":0,"mic":0,"rich":0,"dropped":0},"bringup_ms":0,"ended":"host_error"}}"#
        );
    }

    #[test]
    fn access_event_wire_shapes_and_filters() {
        let device = DeviceRef {
            name: "Guest Deck".into(),
            fingerprint: "ab12".into(),
            plane: Plane::Native,
        };
        let ev = HostEvent {
            seq: 8,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::AccessGranted {
                device: device.clone(),
                grants: 1, // GRANT_GAMEPAD — controller-only
                expires_unix: Some(1_700_000_400),
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":8,"ts_ms":1700000000000,"schema":1,"kind":"access.granted","device":{"name":"Guest Deck","fingerprint":"ab12","plane":"native"},"grants":1,"expires_unix":1700000400}"#
        );

        // Permanent grant omits expiry (not nulled).
        let ev = HostEvent {
            seq: 9,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::AccessChanged {
                device: device.clone(),
                grants: 63,
                expires_unix: None,
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":9,"ts_ms":1700000000000,"schema":1,"kind":"access.changed","device":{"name":"Guest Deck","fingerprint":"ab12","plane":"native"},"grants":63}"#
        );

        let expired = EventKind::AccessExpired { device };
        assert_eq!(expired.name(), "access.expired");
        assert!(kind_matches("access.*", expired.name()));
        assert!(!kind_matches("pairing.*", expired.name()));
        assert_eq!(expired.client_name(), Some("Guest Deck"));
        assert_eq!(expired.fingerprint(), Some("ab12"));
        assert_eq!(expired.plane(), Some(Plane::Native));
    }

    /// Cert-lane invoke carries the device; the admin/console invoke omits the field, not null.
    #[test]
    fn action_invoked_wire_shapes_and_filters() {
        let ev = HostEvent {
            seq: 10,
            ts_ms: 1_700_000_000_000,
            schema: 1,
            kind: EventKind::ActionInvoked {
                id: "power.sleep".into(),
                device: Some(DeviceRef {
                    name: "Living Room TV".into(),
                    fingerprint: "ab12".into(),
                    plane: Plane::Native,
                }),
                outcome: "accepted".into(),
            },
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"seq":10,"ts_ms":1700000000000,"schema":1,"kind":"action.invoked","id":"power.sleep","device":{"name":"Living Room TV","fingerprint":"ab12","plane":"native"},"outcome":"accepted"}"#
        );
        let admin = EventKind::ActionInvoked {
            id: "power.shutdown".into(),
            device: None,
            outcome: "accepted".into(),
        };
        assert_eq!(
            serde_json::to_string(&admin).unwrap(),
            r#"{"kind":"action.invoked","id":"power.shutdown","outcome":"accepted"}"#
        );
        assert_eq!(admin.name(), "action.invoked");
        assert!(kind_matches("action.*", admin.name()));
        assert_eq!(admin.client_name(), None);
        let cert = HostEvent {
            seq: 11,
            ts_ms: 0,
            schema: 1,
            kind: EventKind::ActionInvoked {
                id: "power.sleep".into(),
                device: Some(DeviceRef {
                    name: "Guest Deck".into(),
                    fingerprint: "ab12".into(),
                    plane: Plane::Native,
                }),
                outcome: "accepted".into(),
            },
        };
        assert_eq!(cert.kind.client_name(), Some("Guest Deck"));
        assert_eq!(cert.kind.fingerprint(), Some("ab12"));
    }

    #[test]
    fn game_events_are_filterable() {
        let running = EventKind::GameRunning {
            game: GameRefPayload {
                app: Some("steam:570".into()),
                title: "Dota 2".into(),
                store: Some("steam".into()),
                client: "Deck".into(),
                fingerprint: Some("ab12".into()),
                plane: Plane::Native,
            },
        };
        assert_eq!(running.name(), "game.running");
        assert!(kind_matches("game.*", running.name()));
        assert!(kind_matches("game.running", running.name()));
        assert!(!kind_matches("gamestream.*", running.name()));
        assert_eq!(running.client_name(), Some("Deck"));
        // The handle a rename cannot move: a game event carries the launching device's id.
        assert_eq!(running.fingerprint(), Some("ab12"));
        assert_eq!(running.plane(), Some(Plane::Native));
        assert_eq!(running.app(), Some("steam:570"));

        // No library id: the title is the filterable handle (operator-typed `apps.json` launch).
        let exited = EventKind::GameExited {
            game: GameRefPayload {
                app: None,
                title: "Big Picture".into(),
                store: None,
                client: String::new(),
                fingerprint: None,
                plane: Plane::Gamestream,
            },
            reason: GameEndReason::Exited,
        };
        assert_eq!(exited.app(), Some("Big Picture"));
        assert_eq!(exited.fingerprint(), None);
    }

    #[test]
    fn wire_shape_roundtrips() {
        let ev = HostEvent {
            seq: 7,
            ts_ms: 3,
            schema: 1,
            kind: EventKind::PairingPending {
                device: DeviceRef {
                    name: "iPad Pro".into(),
                    fingerprint: "ab12".into(),
                    plane: Plane::Native,
                },
            },
        };
        let json = serde_json::to_string(&ev).unwrap();
        let back: HostEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 7);
        assert_eq!(back.kind.name(), "pairing.pending");
        match back.kind {
            EventKind::PairingPending { device } => {
                assert_eq!(device.name, "iPad Pro");
                assert_eq!(device.plane, Plane::Native);
            }
            other => panic!("wrong kind: {other:?}"),
        }
    }
}
