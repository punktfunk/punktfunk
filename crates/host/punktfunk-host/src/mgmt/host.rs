//! Host-tagged `/api/v1` routes: identity, liveness, compositor list, live status,
//! and the loopback tray summary. Split out of the `mgmt` facade.

use super::auth::{AuthLane, PairedDevice};
use super::shared::*;
use crate::encode::Codec;
use crate::gamestream::APP_VERSION;
use crate::gamestream::AUDIO_PORT;
use crate::gamestream::CONTROL_PORT;
use crate::gamestream::GFE_VERSION;
use crate::gamestream::RTSP_PORT;
use crate::gamestream::VIDEO_PORT;
use axum::Extension;
use std::sync::atomic::Ordering;

#[derive(Serialize, ToSchema)]
pub(crate) struct Health {
    /// Always `"ok"` when the host responds.
    #[schema(example = "ok")]
    status: String,
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
    abi_version: u32,
}

/// Host identity and capabilities. Static for the process except `local_ip`.
#[derive(Serialize, ToSchema)]
pub(crate) struct HostInfo {
    hostname: String,
    /// Persisted host id; pairing matches on this.
    uniqueid: String,
    /// Fresh LAN IP each request — do not cache. Cold-boot and network-move report
    /// `127.0.0.1` until a real address exists.
    local_ip: String,
    /// `punktfunk-host` crate version.
    version: String,
    /// `punktfunk-core` C ABI version.
    abi_version: u32,
    /// GameStream host version advertised to Moonlight clients.
    app_version: String,
    /// GFE version advertised to Moonlight clients.
    gfe_version: String,
    /// OS chain, generic → specific, slash-separated (`windows` | `macos` |
    /// `linux[/<family>][/<id>]`). Walk most-specific-first; an unknown distro still matches its family.
    #[schema(example = "linux/fedora/bazzite")]
    os: String,
    /// Human-readable OS name (os-release `PRETTY_NAME`; `"Windows"`/`"macOS"` elsewhere).
    #[schema(example = "Bazzite 42 (Kinoite)")]
    os_name: String,
    /// Codecs this host can encode (`host_wire_caps`, not the compile-time list).
    codecs: Vec<ApiCodec>,
    /// GameStream/Moonlight-compat planes are running (`--gamestream`). `false` is the default (native only).
    gamestream: bool,
    /// This host is the door (Linux, **Reachable without logging in**): a system service that
    /// places every connect on a seat and streams nothing itself.
    door: bool,
    /// Why turning the door on would be refused here, as the operator's next move: the files a
    /// channel installs it with are missing, or the account isn't in `punktfunk-update`. `null`
    /// when it would start, on the door itself and on a seat host, and for every lane but the
    /// admin one. The answer is up to 30 s old; a switch request drops it.
    door_unavailable: Option<String>,
    /// The id of `door_unavailable`: the host message `apierr_<code>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    door_unavailable_code: Option<String>,
    /// The values of that message's placeholders, by name.
    #[serde(skip_serializing_if = "Option::is_none")]
    door_unavailable_params: Option<std::collections::BTreeMap<String, String>>,
    /// The switch this host last started ran and failed, so the console stops waiting for it.
    /// Admin lane only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    door_switch_failed: bool,
    /// Whether hooks and prep steps marked `elevated` run as administrator: `true` once an
    /// administrator ran `punktfunk-host hooks elevated on` here. `null` where commands can't
    /// elevate (every OS but Windows), and for every lane but the admin one.
    elevated_commands: Option<bool>,
    /// Hex SHA-256 of this host's leaf certificate — what a client pins. Public by
    /// construction: every client reads it off the handshake. Carried here so a connect link
    /// can name it, and a first connect over an untrusted path is verified rather than blind.
    /// `null` only if the identity could not be parsed.
    #[schema(example = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")]
    fingerprint: Option<String>,
    ports: PortMap,
}

/// Ports a client needs. Moonlight derives stream ports from HTTP; a control pane should not.
/// A port the host does not serve right now is left out.
#[derive(Serialize, ToSchema)]
pub(crate) struct PortMap {
    mgmt: u16,
    /// UDP: the native stream port. Video, audio, control and the paired-device library.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    native: Option<u16>,
    /// UDP: the browser client's plane, while it is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    browser: Option<u16>,
    /// UDP: each seat's stream port, while seats are on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    seats: Vec<u16>,
    /// nvhttp plain HTTP (serverinfo, pairing). This and the five below: GameStream only.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    http: Option<u16>,
    /// nvhttp mutual-TLS HTTPS (post-pairing).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    https: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    rtsp: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    video: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    control: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = u16, required = false)]
    audio: Option<u16>,
}

/// Wire token is the stack's canonical codec name (`Codec::label`). `H265` serializes as `"hevc"`, not `"h265"`.
#[derive(Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ApiCodec {
    H264,
    #[serde(rename = "hevc")]
    H265,
    Av1,
    /// Opt-in wired-LAN intra-only wavelet codec.
    PyroWave,
}

impl From<Codec> for ApiCodec {
    fn from(c: Codec) -> Self {
        match c {
            Codec::H264 => ApiCodec::H264,
            Codec::H265 => ApiCodec::H265,
            Codec::Av1 => ApiCodec::Av1,
            Codec::PyroWave => ApiCodec::PyroWave,
        }
    }
}

/// Live status; changes as sessions start and end.
#[derive(Serialize, ToSchema)]
pub(crate) struct RuntimeStatus {
    video_streaming: bool,
    audio_streaming: bool,
    /// Pairing handshake is waiting for a PIN (`POST /api/v1/pair/pin`).
    pin_pending: bool,
    /// GameStream paired-cert count. Native devices are `native_paired_clients`; sum both for the total.
    paired_clients: u32,
    /// Native-plane pairings (separate store).
    native_paired_clients: u32,
    /// Live sessions on both planes. Native admits concurrent sessions so this can exceed 1;
    /// `session`/`stream` are one representative and `sessions` is the list.
    active_sessions: u32,
    /// Every live session, one row each — what the per-session routes take an id from.
    sessions: Vec<SessionRow>,
    /// GameStream launch if present, else the first live native session. `null` when idle.
    /// `session_id` says which row of `sessions` this is.
    session: Option<SessionInfo>,
    /// Active stream parameters of that same session. `null` when idle.
    stream: Option<StreamInfo>,
    /// Which `sessions` row `session`/`stream` describe. `null` when idle, or when the
    /// representative is the GameStream stream (the compat plane has no id).
    // `value_type`: an `Option<u64>` alone generates as `never` in the SDK.
    #[schema(value_type = u64, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    /// Launched titles: live sessions plus `state: "grace"` reconnect-window rows. Empty for a desktop-only stream.
    games: Vec<ActiveGame>,
    /// Titles downloading, queued, paused, or finished in the last ten minutes. A launch hold
    /// joins its title's row by `app_id`. Absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    downloads: Vec<crate::library::downloads::Download>,
    /// The calling device's live access grants (`GRANT_*` bits); 0 once expired. Absent for
    /// the operator, who may do everything.
    #[schema(value_type = u32, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    grants: Option<u32>,
    /// Windows audio-wiring verdict; absent off-Windows and before the first pass. Present while idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    audio: Option<AudioWiring>,
    /// Windows display state: topology transactions and outstanding device leases. Absent off-Windows.
    #[serde(skip_serializing_if = "Option::is_none")]
    display: Option<DisplayHealth>,
}

/// Host-wide Windows display health: the topology transaction log and the leases the stream holds.
#[derive(Serialize, ToSchema)]
pub(crate) struct DisplayHealth {
    /// Count of topology transactions that observed a change since the host started.
    topology_generation: u64,
    /// The most recent topology transaction, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_transaction: Option<TopologyTransaction>,
    /// Monitor devnodes disabled for a stream and not yet re-enabled (a leftover here after a
    /// crash is what the next host start replays).
    pnp_leases: u32,
    /// Protocol the installed pf-vdisplay driver answered at the last handshake; absent until a session ran one.
    #[serde(skip_serializing_if = "Option::is_none")]
    driver_protocol: Option<u32>,
    /// Protocol this host drives; a driver answering less fails every session.
    host_protocol: u32,
}

/// One topology transaction as the display actor recorded it.
#[derive(Serialize, ToSchema)]
pub(crate) struct TopologyTransaction {
    /// `acquire-isolate` / `exclusive-reassert` / …
    reason: String,
    /// `changed` / `unchanged` / `unknown`.
    outcome: String,
    took_ms: u64,
}

/// Per-session capture health (Windows IDD-push): the live classifier verdict, the driver
/// encoder's self-report and the last staged-recovery episode.
#[derive(Serialize, ToSchema)]
pub(crate) struct CaptureHealth {
    /// `healthy` / `idle` / `suspect` / `stalled` / `recovering` / `rebuilding` / `secure_desktop`.
    #[schema(example = "healthy")]
    class: String,
    /// When `class` is `stalled`: `worker` / `encoder` / `presentation` / `driver`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stall_class: Option<String>,
    /// Time since the last real source frame.
    source_gap_ms: u64,
    /// Activity evidence behind the verdict: `input` / `canary`.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence: Option<String>,
    /// The newest access unit's OS present stamp against the moment the host took it.
    #[serde(skip_serializing_if = "Option::is_none")]
    present_to_arrival_ms: Option<u64>,
    /// `present_to_arrival_ms` is past the classifier's bound: frames come late rather than not
    /// at all. Reported only — no recovery rung fires on it.
    late_frames: bool,
    /// The driver encoder's own state word: `closed` / `open` / `encoding` / `wedged`.
    /// Absent until the session's first `SET_ENCODE`.
    #[serde(skip_serializing_if = "Option::is_none")]
    encoder_state: Option<String>,
    /// The backend the driver opened: `nvenc` / `amf` / `qsv` / `pyrowave`. Absent as above.
    #[serde(skip_serializing_if = "Option::is_none")]
    backend_opened: Option<String>,
    /// Encode threads the driver abandoned after a wedge; two opens the driver cycle.
    detached: u32,
    /// Access units the driver published, and frames it dropped at its encode pool.
    published_total: u64,
    dropped_total: u64,
    /// The recovery stage running now, while an episode is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    current_stage: Option<String>,
    /// The last closed recovery episode.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_episode: Option<CaptureEpisode>,
    /// Stalled verdicts refused for budget or cooldown since the last episode.
    episodes_suppressed: u32,
    /// Time left in the post-failure cooldown, while one is in force.
    #[serde(skip_serializing_if = "Option::is_none")]
    cooldown_remaining_ms: Option<u64>,
}

/// One closed staged-recovery episode.
#[derive(Serialize, ToSchema)]
pub(crate) struct CaptureEpisode {
    stall_class: String,
    recovered: bool,
    took_ms: u64,
    /// The rungs run, in ladder order.
    stages: Vec<CaptureStage>,
    consecutive_failures: u32,
    cooldown_ms: u64,
}

/// One recovery rung of an episode.
#[derive(Serialize, ToSchema)]
pub(crate) struct CaptureStage {
    /// `encoder_reset` / `swap_chain_reset` / `presentation_reset` / `driver_cycle`.
    stage: String,
    /// `applied` / `failed` / `unsupported` / `timed_out`.
    outcome: String,
    took_ms: u64,
}

fn api_capture_health(h: &pf_capture::CaptureHealth) -> CaptureHealth {
    let ms = |d: std::time::Duration| d.as_millis().min(u64::MAX as u128) as u64;
    CaptureHealth {
        class: h.class.into(),
        stall_class: h.stall_class.map(Into::into),
        source_gap_ms: ms(h.source_gap),
        evidence: h.evidence.map(Into::into),
        present_to_arrival_ms: h.present_to_arrival.map(ms),
        late_frames: h.late_frames,
        encoder_state: h.encoder_state.map(Into::into),
        backend_opened: h.backend_opened.map(Into::into),
        detached: h.detached,
        published_total: h.published_total,
        dropped_total: h.dropped_total,
        current_stage: h.current_stage.map(Into::into),
        last_episode: h.last_episode.as_ref().map(|e| CaptureEpisode {
            stall_class: e.stall_class.into(),
            recovered: e.recovered,
            took_ms: ms(e.took),
            stages: e
                .stages
                .iter()
                .map(|&(stage, outcome, took)| CaptureStage {
                    stage: stage.into(),
                    outcome: outcome.into(),
                    took_ms: ms(took),
                })
                .collect(),
            consecutive_failures: e.consecutive_failures,
            cooldown_ms: ms(e.cooldown),
        }),
        episodes_suppressed: h.episodes_suppressed,
        cooldown_remaining_ms: h.cooldown_remaining.map(ms),
    }
}

#[cfg(target_os = "windows")]
fn display_health() -> Option<DisplayHealth> {
    use pf_win_display::topology_churn;
    Some(DisplayHealth {
        topology_generation: topology_churn::generation(),
        last_transaction: topology_churn::last().map(|t| TopologyTransaction {
            reason: t.reason.into(),
            outcome: match t.outcome {
                topology_churn::Outcome::Changed => "changed",
                topology_churn::Outcome::Unchanged => "unchanged",
                topology_churn::Outcome::Unknown => "unknown",
            }
            .into(),
            took_ms: t.took.as_millis().min(u64::MAX as u128) as u64,
        }),
        pnp_leases: pf_win_display::monitor_devnode::leases().len() as u32,
        driver_protocol: crate::vdisplay::manager::driver_protocol(),
        host_protocol: pf_driver_proto::PROTOCOL_VERSION,
    })
}

#[cfg(not(target_os = "windows"))]
fn display_health() -> Option<DisplayHealth> {
    None
}

/// Windows audio wiring: which endpoint carries each role. Names are the Sound-settings friendly names.
#[derive(Serialize, ToSchema)]
pub(crate) struct AudioWiring {
    /// `full` | `audio_only` | `mic_only` | `none`.
    #[schema(example = "full")]
    readiness: String,
    /// Desktop-audio loopback friendly name; absent = unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    loopback: Option<String>,
    /// Virtual-mic write-target friendly name; absent = unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    mic: Option<String>,
    /// Mic withheld so game audio keeps the only working sink.
    mic_withheld: bool,
    /// Loopback is the degraded last resort; desktop audio may be silent until endpoints change.
    last_resort: bool,
    /// Why the chosen loopback endpoint NARROWS the desktop mix (rate/channels), when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    narrowing: Option<String>,
}

/// `None` off-Windows or before the first wiring pass.
fn audio_wiring() -> Option<AudioWiring> {
    use crate::audio::wiring_plan as wp;
    crate::audio::wiring_snapshot().map(|w| AudioWiring {
        readiness: match wp::readiness(&w) {
            wp::AudioReadiness::Full => "full",
            wp::AudioReadiness::AudioOnly => "audio_only",
            wp::AudioReadiness::MicOnly => "mic_only",
            wp::AudioReadiness::Nothing => "none",
        }
        .into(),
        loopback: w.loopback_render.map(|(n, _)| n),
        mic: w.mic_render.map(|(n, _)| n),
        mic_withheld: w.mic_withheld,
        last_resort: w.loopback_last_resort,
        narrowing: w.loopback_narrowing,
    })
}

#[derive(Serialize, ToSchema)]
pub(crate) struct ActiveGame {
    /// Streaming session; `null` while waiting out the reconnect window. Pass it to
    /// `DELETE /session/{id}` to stop that one session.
    // `value_type`: an `Option<u64>` alone generates as `never` in the SDK.
    #[schema(value_type = u64, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    session_id: Option<u64>,
    /// Client-supplied device name of the session that launched it; may be empty.
    client: String,
    /// Store-qualified library id (`steam:570`); matches `GET /library`. Absent for a typed GameStream command.
    #[serde(skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    title: String,
    /// Which store surfaced it (`steam`, `heroic`, `custom`, …), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<String>,
    /// `native`, `gamestream` or `web`.
    plane: crate::events::Plane,
    /// `launching` | `running` | `window` (its window is on the streamed screen) | `exited` |
    /// `untracked` (exit will never be seen) | `grace` (reconnect window) | `detached` (still
    /// running, no session holds it).
    #[schema(example = "running")]
    state: String,
    /// Present and true while `running` on a host that will report `window` next. A launch hold
    /// waits for that instead of revealing a game that is still loading.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    awaiting_window: bool,
    /// Seconds until this game is ended — only present on a `grace` row.
    #[serde(skip_serializing_if = "Option::is_none")]
    grace_remaining_s: Option<u64>,
    /// Present and true when this caller may end it with `POST /game/end`: the operator any
    /// launched game, a paired device only a game it launched.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    #[schema(required = false)]
    endable: bool,
    /// On a door, to the operator: the profile whose seat host runs it, whose proxy reaches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    seat: Option<String>,
}

/// One live session as the Dashboard lists it: who, where, since when, and the state
/// the per-session routes change.
#[derive(Serialize, ToSchema)]
pub(crate) struct SessionRow {
    /// Pass to `DELETE /session/{id}` and friends. `null` on the compat plane, which
    /// has no per-session handle — stop it with the host-wide `DELETE /session`.
    // `value_type`: an `Option<u64>` alone generates as `never` in the SDK.
    #[schema(value_type = u64, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    plane: crate::events::Plane,
    /// Where the session is. `streaming` for most of its life; `ending` once a stop, a
    /// game exit or the client's close has been seen and the row is about to go.
    phase: crate::lifecycle::Phase,
    /// The punktfunk protocol the session speaks. Absent on the compat plane.
    // `value_type`: an `Option<u8>` alone generates as `never` in the SDK.
    #[schema(value_type = u32, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    wire: Option<u8>,
    /// Fingerprint prefix, or peer IP for an anonymous client.
    client: String,
    /// Display name (trust store, else the name the client sent). `null` if nameless.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    /// Name of the settings preset the client dialled with. Absent for plain settings.
    #[serde(skip_serializing_if = "Option::is_none")]
    preset_name: Option<String>,
    /// The profile the session plays as. Blank, like `client_name`, for another device's
    /// session on the cert lane.
    #[serde(skip_serializing_if = "Option::is_none")]
    profile: Option<crate::events::ProfileRef>,
    /// On a door, to the operator: the profile whose seat host runs it, whose proxy reaches it.
    #[serde(skip_serializing_if = "Option::is_none")]
    seat: Option<String>,
    /// `WxH@Hz`.
    #[schema(example = "3840x2160@120")]
    mode: String,
    hdr: bool,
    /// Sharing another session's display rather than owning one. Which session it joined
    /// is not reported yet — the two registries do not share ids (issue #1095).
    join: bool,
    /// Audio is held back for this session alone (`PUT /session/{id}/audio`).
    muted: bool,
    /// `full` | `controller` | `view` | `custom`, live — not the pairing's stored level.
    /// `null` on the compat plane, which is ungoverned.
    #[serde(skip_serializing_if = "Option::is_none")]
    access_level: Option<String>,
    /// OS pad slots this session holds, lowest first. Slot `n` is player `n + 1` to a
    /// local co-op game. Empty while the session has no controller.
    pads: Vec<u8>,
    /// Player slot the operator picked for this session, 0-based (`PUT
    /// /session/{id}/player`). `null` = the slot is whichever comes free.
    // `value_type`: an `Option<u8>` alone generates as `never` in the SDK.
    #[schema(value_type = u32, required = false)]
    #[serde(skip_serializing_if = "Option::is_none")]
    preferred_pad_slot: Option<u8>,
    /// Seconds since the stream started.
    uptime_s: u64,
    /// The last closed minute of link health: loss, the recovery frames it cost, and the FEC
    /// and bitrate bands. `null` in a session's first minute, and on the compat plane.
    #[serde(skip_serializing_if = "Option::is_none")]
    link: Option<crate::link_health::LinkMinute>,
    /// Other live sessions from this client's address — one NAT or tunnel, so most likely one
    /// network path. Their bitrates adapt independently. Absent when there are none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[schema(required = false)]
    shared_path_with: Vec<u64>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct SessionInfo {
    width: u32,
    height: u32,
    fps: u32,
    /// Live capture health (Windows IDD-push, native plane). Absent on GameStream, on Linux,
    /// and until the video loop's first publish.
    #[serde(skip_serializing_if = "Option::is_none")]
    capture: Option<CaptureHealth>,
}

/// Negotiated stream parameters (RTSP on GameStream; live mode on native).
#[derive(Serialize, ToSchema)]
pub(crate) struct StreamInfo {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// Video payload size per packet (bytes).
    packet_size: u32,
    /// Client's parity floor per FEC block (`minRequiredFecPackets`).
    min_fec: u8,
    codec: ApiCodec,
    /// Hello → first video packet, ms. Native only; `null` on GameStream or while still bringing up.
    time_to_first_frame_ms: Option<u32>,
    /// Last mid-stream resize, reconfigure → rebuilt, ms. Native only; `null` if none / GameStream.
    last_resize_ms: Option<u32>,
}

/// Tray snapshot for loopback: counts, booleans, and `client_name`.
/// `require_auth` admits the tray token from loopback; nothing here is a secret.
#[derive(Serialize, ToSchema)]
pub(crate) struct LocalSummary {
    /// Host version (mirrors `/health`).
    version: String,
    /// Video streaming on either plane. The GameStream flag alone misses native sessions.
    video_streaming: bool,
    /// True while audio is streaming on either plane (same rule as `video_streaming`).
    audio_streaming: bool,
    /// GameStream launch if present, else the first live native session. `null` when idle.
    session: Option<SessionInfo>,
    /// First native session's display name (trust-store, else connect-time). `null` when idle, nameless, or GameStream.
    #[serde(skip_serializing_if = "Option::is_none")]
    client_name: Option<String>,
    /// GameStream paired-cert count.
    paired_clients: u32,
    /// Native-plane pairing count.
    native_paired_clients: u32,
    /// GameStream pairing is waiting for a PIN.
    pin_pending: bool,
    /// Native pairing knocks awaiting the operator's approval (count only).
    pending_approvals: u32,
    /// Lingering or pinned virtual displays with no live session. Active (in-use) displays are not counted.
    kept_displays: u32,
    /// Other GameStream hosts on this machine, detected at startup. Running one alongside is unsupported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
    /// Compact labels (`Hades`, `Hades (closing in 4:12)`). Countdown means the client is gone and the host will end the game when the window closes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    games: Vec<String>,
}

/// Liveness probe
///
/// Unauthenticated: `require_auth` exempts it.
#[utoipa::path(
    get,
    path = "/health",
    tag = "host",
    operation_id = "getHealth",
    // Override the document-global bearerAuth: this route is exempt in `require_auth`.
    security(()),
    responses((status = OK, description = "Host is up", body = Health))
)]
pub(crate) async fn get_health() -> Json<Health> {
    Json(Health {
        status: "ok".into(),
        version: crate::version::get().into(),
        abi_version: punktfunk_core::ABI_VERSION,
    })
}

/// How long a probe of the door's blockers answers: it forks `id`.
const DOOR_PROBE_FRESH: std::time::Duration = std::time::Duration::from_secs(30);

type DoorProbe<T = ApiError> = std::sync::Mutex<Option<(std::time::Instant, Option<T>)>>;

static DOOR_PROBE: DoorProbe = std::sync::Mutex::new(None);

/// The door's blockers, read only for the console: a paired device is not told which account
/// the host runs as, and a seat host has no door to turn on.
async fn door_unavailable(lane: AuthLane) -> Option<ApiError> {
    if lane != AuthLane::Admin || pf_paths::seat::role().is_seat_host() {
        return None;
    }
    tokio::task::spawn_blocking(|| probe_door(&DOOR_PROBE, crate::door::unavailable))
        .await
        .unwrap_or_default()
}

/// Whether the owner's host behind this door serves Moonlight, for the console's pairing card:
/// the door never does, and the owner's row may. Never starts the owner's session.
async fn owner_gamestream(st: &MgmtState, lane: AuthLane) -> bool {
    if lane != AuthLane::Admin {
        return false;
    }
    st.owner_call("GET", "host", None, false)
        .await
        .ok_body()
        .and_then(|body| serde_json::from_slice::<serde_json::Value>(&body).ok())
        .and_then(|v| v.get("gamestream")?.as_bool())
        .unwrap_or(false)
}

/// Drops the remembered blockers, so the next read asks again. A door switch request changes what
/// they are: the operator fixes one, or the switch starts.
pub(super) fn forget_door_probe() {
    forget_probe(&DOOR_PROBE);
}

fn forget_probe<T>(last: &DoorProbe<T>) {
    *last.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `probe`'s answer, or the last one if it is under [`DOOR_PROBE_FRESH`] old.
fn probe_door<T: Clone>(last: &DoorProbe<T>, probe: impl FnOnce() -> Option<T>) -> Option<T> {
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, answer)) = last.as_ref() {
        if at.elapsed() < DOOR_PROBE_FRESH {
            return answer.clone();
        }
    }
    let answer = probe();
    *last = Some((std::time::Instant::now(), answer.clone()));
    answer
}

/// Host identity and capabilities
#[utoipa::path(
    get,
    path = "/host",
    tag = "host",
    operation_id = "getHostInfo",
    responses(
        (status = OK, description = "Host identity, versions, codecs, and port map", body = HostInfo),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_host_info(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
) -> Json<HostInfo> {
    let h = &st.app.host;
    let door_unavailable = door_unavailable(lane).await;
    let door_switch_failed = lane == AuthLane::Admin
        && tokio::task::spawn_blocking(crate::door::switch_failed)
            .await
            .unwrap_or(false);
    // The ledger asks the seat supervisor: off the async workers.
    let seats = tokio::task::spawn_blocking(|| {
        let ledger = crate::seats::ledger();
        match ledger.on {
            true => ledger.seats.iter().map(|s| s.native_port).collect(),
            false => Vec::new(),
        }
    })
    .await
    .unwrap_or_default();
    let gs = st.gamestream_enabled || owner_gamestream(&st, lane).await;
    Json(HostInfo {
        hostname: h.hostname.clone(),
        uniqueid: h.uniqueid.clone(),
        local_ip: h.local_ip().to_string(),
        version: crate::version::get().into(),
        abi_version: punktfunk_core::ABI_VERSION,
        app_version: APP_VERSION.into(),
        gfe_version: GFE_VERSION.into(),
        os: h.os_chain.clone(),
        os_name: h.os_name.clone(),
        // Same mask as GameStream/QUIC negotiation (`wire_caps`), not the compile-time list.
        // A door encodes nothing, so it names none and never probes the GPU.
        codecs: {
            let caps = if pf_paths::seat::role().is_door() {
                0
            } else {
                crate::encode::resolve().wire_caps()
            };
            use punktfunk_core::quic::{CODEC_AV1, CODEC_H264, CODEC_HEVC, CODEC_PYROWAVE};
            [
                (CODEC_H264, ApiCodec::H264),
                (CODEC_HEVC, ApiCodec::H265),
                (CODEC_AV1, ApiCodec::Av1),
                (CODEC_PYROWAVE, ApiCodec::PyroWave),
            ]
            .into_iter()
            .filter(|(bit, _)| caps & bit != 0)
            .map(|(_, codec)| codec)
            .collect()
        },
        gamestream: gs,
        door: pf_paths::seat::role().is_door(),
        door_unavailable_code: door_unavailable.as_ref().and_then(|e| e.code.clone()),
        door_unavailable_params: door_unavailable.as_ref().and_then(|e| e.params.clone()),
        door_unavailable: door_unavailable.map(|e| e.error),
        door_switch_failed,
        elevated_commands: (cfg!(windows) && lane == AuthLane::Admin)
            .then(crate::hooks::elevated_on),
        fingerprint: st.identity_fingerprint.map(hex::encode),
        ports: PortMap {
            mgmt: st.port,
            native: st.app.native_port.get().copied(),
            browser: crate::webtransport::published().map(|p| p.port),
            seats,
            http: gs.then_some(h.http_port),
            https: gs.then_some(h.https_port),
            rtsp: gs.then_some(RTSP_PORT),
            video: gs.then_some(VIDEO_PORT),
            control: gs.then_some(CONTROL_PORT),
            audio: gs.then_some(AUDIO_PORT),
        },
    })
}

/// `own` as this host answers, or on a door with every seat's sessions, games and downloads
/// in it: the door streams nothing, so without them a client sees no game to resume and the
/// console no session. `caller` is the device asking on the cert lane, which sees the others'
/// rows without their identity, as on one host.
async fn with_seats(st: &MgmtState, own: RuntimeStatus, caller: Option<String>) -> Response {
    if st.door.is_none() {
        return Json(own).into_response();
    }
    let snapshot = st.seats_snapshot().await;
    let profiles = st.app.profiles.get().map(|p| p.list()).unwrap_or_default();
    let seats: Vec<(Option<String>, &serde_json::Value)> = snapshot
        .seats
        .iter()
        .filter_map(|seat| {
            let status = snapshot.statuses.get(seat.id.as_str())?;
            Some((crate::seats::profile_on(seat, &profiles), status))
        })
        .collect();
    let Ok(own) = serde_json::to_value(own) else {
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "status did not serialize",
        );
    };
    Json(merged_status(own, &seats, caller.as_deref())).into_response()
}

/// [`with_seats`] on the wire shapes: `seats` in ledger order with the profile each plays as,
/// `caller` the device's fingerprint. Another device's row loses what `get_status` withholds
/// from it: who plays, and whether this caller may end it. Only the operator learns the seat.
fn merged_status(
    mut own: serde_json::Value,
    seats: &[(Option<String>, &serde_json::Value)],
    caller: Option<&str>,
) -> serde_json::Value {
    use serde_json::Value;
    let mine = |row: &Value| {
        caller.is_none_or(|fp| {
            row.get("client")
                .and_then(Value::as_str)
                .is_some_and(|c| !c.is_empty() && fp.starts_with(c))
        })
    };
    for (profile, seat) in seats {
        for key in ["sessions", "games"] {
            let rows = seat
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if rows.is_empty() {
                continue;
            }
            // A list the door left out as empty is empty, not absent.
            if !own[key].is_array() {
                own[key] = Value::Array(Vec::new());
            }
            let Some(into) = own[key].as_array_mut() else {
                continue;
            };
            for mut row in rows {
                let theirs = !mine(&row);
                if let Some(o) = row.as_object_mut() {
                    if theirs {
                        o.insert("client".into(), Value::String(String::new()));
                        o.remove("client_name");
                        o.remove("profile");
                        o.remove("endable");
                    }
                    if let (None, Some(profile)) = (caller, profile) {
                        o.insert("seat".into(), Value::String(profile.clone()));
                    }
                }
                into.push(row);
            }
        }
        if let Some(rows) = seat.get("downloads").and_then(Value::as_array) {
            if !rows.is_empty() && !own["downloads"].is_array() {
                own["downloads"] = Value::Array(Vec::new());
            }
            if let Some(into) = own["downloads"].as_array_mut() {
                into.extend(rows.iter().cloned());
            }
        }
        for flag in ["video_streaming", "audio_streaming"] {
            if seat.get(flag).and_then(Value::as_bool) == Some(true) {
                own[flag] = Value::Bool(true);
            }
        }
        // One representative, as on one host: the door's own first, else the first seat's.
        if own["session"].is_null() && !seat["session"].is_null() {
            for key in ["session", "stream", "session_id"] {
                if let Some(v) = seat.get(key) {
                    own[key] = v.clone();
                }
            }
        }
    }
    let count = own["sessions"].as_array().map_or(0, Vec::len);
    own["active_sessions"] = Value::from(count as u64);
    own
}

/// A compositor backend and whether it is usable now.
#[derive(Serialize, ToSchema)]
pub(crate) struct AvailableCompositor {
    /// Stable id (`kwin` | `wlroots` | `mutter` | `gamescope`); pass to `--compositor`.
    id: String,
    label: String,
    /// Usable now: the live session's compositor, or gamescope if its binary is installed.
    available: bool,
    /// True for the backend an `Auto` (unspecified) request resolves to right now.
    default: bool,
}

/// List compositor backends
///
/// Each row carries availability and whether `Auto` resolves to it. Clients pass
/// `id` to `--compositor` or `PUNKTFUNK_COMPOSITOR_*`.
#[utoipa::path(
    get,
    path = "/compositors",
    tag = "host",
    operation_id = "listCompositors",
    responses(
        (status = OK, description = "Compositor backends with availability + the auto-detected default", body = [AvailableCompositor]),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn list_compositors() -> Json<Vec<AvailableCompositor>> {
    // Empty off Linux: `vdisplay::open` ignores compositor there.
    // Listing unavailable Linux backends looks like a detection bug.
    #[cfg(not(target_os = "linux"))]
    let list = Vec::new();
    #[cfg(target_os = "linux")]
    // One `/proc` scan for both columns (`vdisplay::available`); default cannot be unavailable.
    let list = {
        let available = crate::vdisplay::available();
        let default = crate::vdisplay::detect().ok();
        crate::vdisplay::Compositor::all()
            .into_iter()
            .map(|c| AvailableCompositor {
                id: c.id().into(),
                label: c.label().into(),
                available: available.contains(&c),
                default: default == Some(c),
            })
            .collect()
    };
    Json(list)
}

/// Live host status
#[utoipa::path(
    get,
    path = "/status",
    tag = "host",
    operation_id = "getStatus",
    responses(
        (status = OK, description = "Streaming/pairing state and the active session, if any", body = RuntimeStatus),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
    )
)]
pub(crate) async fn get_status(
    State(st): State<Arc<MgmtState>>,
    Extension(lane): Extension<AuthLane>,
    device: Option<Extension<PairedDevice>>,
) -> Response {
    let ender = super::session::GameEnder::of(&st, lane, device.as_ref().map(|d| d.0 .0.as_str()));
    let gs_launch = *st.app.launch.lock().unwrap_or_else(|e| e.into_inner());
    // Stream slot is GameStream-featured only; a native-only build has no compat-plane stream.
    #[cfg(feature = "gamestream")]
    let gs_stream = st
        .app
        .gs
        .stream
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let gs_video = st.app.streaming.load(Ordering::SeqCst);
    let gs_audio = st.app.audio_streaming.load(Ordering::SeqCst);
    // Native plane, published by the video loop; lives outside `AppState` (see `session_status`).
    let native = crate::session_status::snapshot();

    // A paired device reads every row but only its own identity: the others' names and
    // fingerprints stay off its lane, as the rosters do. `client` is a fingerprint prefix.
    let caller = match (lane, &device) {
        (AuthLane::Cert, Some(d)) => Some(d.0 .0.as_str()),
        (AuthLane::Cert, None) => Some(""),
        _ => None,
    };
    let shown = |client: &str| caller.is_none_or(|fp| !client.is_empty() && fp.starts_with(client));
    // One row per live session, for the Dashboard's list and the per-session routes. Both
    // planes register, so a compat session carries an id like any other and its stop and
    // keyframe reach it. The lanes it does not have stay absent rather than reading as off.
    let sessions: Vec<SessionRow> = native
        .iter()
        .map(|s| {
            let native_plane = s.plane != crate::events::Plane::Gamestream;
            let own = shown(&s.client);
            SessionRow {
                id: Some(s.id),
                plane: s.plane,
                phase: s.phase,
                wire: native_plane.then_some(2),
                client: if own { s.client.clone() } else { String::new() },
                client_name: s.client_name.clone().filter(|_| own),
                preset_name: s.preset_name.clone(),
                profile: s.profile.clone().filter(|_| own),
                mode: crate::events::mode_str(s.width, s.height, s.fps),
                hdr: s.hdr,
                join: s.join,
                muted: s.muted,
                // Per-session access is a native lane: the compat plane checks its grants per
                // nvhttp request and has no channel to tell a client they changed.
                access_level: native_plane
                    .then(|| super::native::access_level(Some(s.grants)).to_string()),
                pads: s.pads.clone(),
                preferred_pad_slot: s.preferred_pad_slot,
                uptime_s: s.uptime_s,
                link: s.link.clone(),
                shared_path_with: s.shared_path_with.clone(),
                seat: None,
            }
        })
        .collect();
    // Detail card is singular: GameStream if live, else the first native session. `active_sessions` is the true count.
    let session = gs_launch
        .map(|l| SessionInfo {
            width: l.width,
            height: l.height,
            fps: l.fps,
            capture: None,
        })
        .or_else(|| {
            native.first().map(|s| SessionInfo {
                width: s.width,
                height: s.height,
                fps: s.fps,
                capture: s.capture_health.as_ref().map(api_capture_health),
            })
        });
    #[cfg(feature = "gamestream")]
    let gs_stream_info = gs_stream.map(|c| StreamInfo {
        width: c.width,
        height: c.height,
        fps: c.fps,
        bitrate_kbps: c.bitrate_kbps,
        packet_size: c.packet_size as u32,
        min_fec: c.min_fec,
        codec: c.codec.into(),
        // Transition latencies are native-plane only.
        time_to_first_frame_ms: None,
        last_resize_ms: None,
    });
    #[cfg(not(feature = "gamestream"))]
    let gs_stream_info: Option<StreamInfo> = None;
    let stream = gs_stream_info.or_else(|| {
        native.first().map(|s| StreamInfo {
            width: s.width,
            height: s.height,
            fps: s.fps,
            bitrate_kbps: s.bitrate_kbps,
            // FEC/packetization are RTSP (GameStream); native QUIC shards differently, so 0 = not applicable.
            packet_size: 0,
            min_fec: 0,
            codec: s.codec.into(),
            time_to_first_frame_ms: (s.time_to_first_frame_ms > 0)
                .then_some(s.time_to_first_frame_ms),
            last_resize_ms: (s.last_resize_ms > 0).then_some(s.last_resize_ms),
        })
    });
    let status = RuntimeStatus {
        video_streaming: gs_video || !native.is_empty(),
        audio_streaming: gs_audio || !native.is_empty(),
        pin_pending: gs_pin_pending(&st),
        paired_clients: st
            .app
            .paired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u32,
        native_paired_clients: st.native.as_ref().map_or(0, |n| n.status().paired_clients),
        // Both planes register, so the rows are the count.
        active_sessions: sessions.len() as u32,
        // The singular slot is the compat session while one streams — it has an id now. A
        // launch that has not reached PLAY has no session yet, and naming a native id there
        // would disagree with the card above.
        session_id: native
            .iter()
            .find(|s| s.plane == crate::events::Plane::Gamestream)
            .map(|s| s.id)
            .or_else(|| {
                gs_launch
                    .is_none()
                    .then(|| native.first().map(|s| s.id))
                    .flatten()
            }),
        sessions,
        session,
        stream,
        display: display_health(),
        games: crate::session_status::games()
            .into_iter()
            .map(|g| ActiveGame {
                session_id: g.session_id,
                client: if shown(&g.client) {
                    g.client
                } else {
                    String::new()
                },
                app_id: g.app_id,
                title: g.title,
                store: g.store,
                plane: g.plane,
                state: g.state.to_string(),
                awaiting_window: g.awaiting_window,
                grace_remaining_s: g.grace_remaining_s,
                endable: ender.may_end(g.state, g.launched_by.as_deref()),
                seat: None,
            })
            .collect(),
        downloads: crate::library::downloads::snapshot(),
        grants: caller.map(|fp| {
            st.native
                .as_ref()
                .and_then(|n| n.effective(fp, crate::clock::unix_secs()))
                .unwrap_or(0)
        }),
        audio: audio_wiring(),
    };
    with_seats(&st, status, caller.map(str::to_string)).await
}

/// Loopback tray summary
///
/// Bearer: `<config>/tray-token`, which every local account but a seat's may read; loopback only.
#[utoipa::path(
    get,
    path = "/local/summary",
    tag = "host",
    operation_id = "getLocalSummary",
    // Override the document-global bearerAuth: the tray token is its own credential.
    security(()),
    responses(
        (status = OK, description = "Non-sensitive local host status (loopback peers only)", body = LocalSummary),
        (status = UNAUTHORIZED, description = "Non-loopback peer, or not the tray token", body = ApiError),
        (status = NOT_FOUND, description = "This host minted no tray token (a seat host)", body = ApiError),
    )
)]
pub(crate) async fn get_local_summary(State(st): State<Arc<MgmtState>>) -> Json<LocalSummary> {
    // Snapshot once; reused for the session card and the streaming flags below.
    let native = crate::session_status::snapshot();
    // GameStream launch, else the first live native session (same order as `/status`).
    let session = st
        .app
        .launch
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .map(|l| SessionInfo {
            width: l.width,
            height: l.height,
            fps: l.fps,
            capture: None,
        })
        .or_else(|| {
            native.first().map(|s| SessionInfo {
                width: s.width,
                height: s.height,
                fps: s.fps,
                capture: s.capture_health.as_ref().map(api_capture_health),
            })
        });
    let (native_paired_clients, pending_approvals) = st
        .native
        .as_ref()
        .map(|n| (n.status().paired_clients, n.pending().len() as u32))
        .unwrap_or((0, 0));
    Json(LocalSummary {
        version: crate::version::get().into(),
        // Either plane, like `/status`; GameStream flags alone miss a native session.
        video_streaming: st.app.streaming.load(Ordering::SeqCst) || !native.is_empty(),
        audio_streaming: st.app.audio_streaming.load(Ordering::SeqCst) || !native.is_empty(),
        session,
        // First native session's name. GameStream launches have no device name, so this stays absent there.
        client_name: native.first().and_then(|s| s.client_name.clone()),
        paired_clients: st
            .app
            .paired
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len() as u32,
        native_paired_clients,
        pin_pending: gs_pin_pending(&st),
        pending_approvals,
        kept_displays: crate::vdisplay::registry::snapshot()
            .displays
            .iter()
            .filter(|d| d.state == "lingering" || d.state == "pinned")
            .count() as u32,
        // Startup cache (empty if nothing detected / never scanned); not a per-poll process scan.
        conflicts: crate::detect::summary_labels(crate::detect::snapshot()),
        games: crate::session_status::games()
            .into_iter()
            .map(|g| match (g.grace_remaining_s, g.state) {
                (Some(left), _) => {
                    format!("{} (closing in {}:{:02})", g.title, left / 60, left % 60)
                }
                // Untracked: the host cannot follow this process, so say so rather than a bare title.
                (None, "untracked") => format!("{} (not tracked)", g.title),
                (None, _) => g.title,
            })
            .collect(),
    })
}

/// GameStream PIN wait. `false` in a native-only build (pairing does not exist); the field stays so the schema matches across flavors.
#[cfg(feature = "gamestream")]
fn gs_pin_pending(st: &Arc<MgmtState>) -> bool {
    st.app.gs.pairing.pin.awaiting_pin()
}
#[cfg(not(feature = "gamestream"))]
fn gs_pin_pending(_st: &Arc<MgmtState>) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_door_probe_runs_once_a_while() {
        let last: DoorProbe<String> = std::sync::Mutex::new(None);
        let mut runs = 0;
        let mut ask = |answer: Option<&str>| {
            probe_door(&last, || {
                runs += 1;
                answer.map(str::to_string)
            })
        };
        assert_eq!(ask(Some("first")).as_deref(), Some("first"));
        assert_eq!(ask(None).as_deref(), Some("first"));
        assert_eq!(runs, 1);
        *last.lock().unwrap() = Some((
            std::time::Instant::now() - DOOR_PROBE_FRESH,
            Some("stale".into()),
        ));
        assert_eq!(probe_door(&last, || None), None);
    }

    /// The operator learns which seat runs each row; a device sees another's row without who
    /// plays it or the seat's own answer to whether it may end it.
    #[test]
    fn merged_rows_name_their_seat_to_the_operator_alone() {
        let seat = serde_json::json!({
            "sessions": [{"id": 3, "client": "aa11bb22cc33", "client_name": "Kid's Deck"}],
            "games": [{"session_id": 3, "client": "aa11bb22cc33", "title": "Hades",
                       "endable": true}],
        });
        let seats = [(Some("kid".to_string()), &seat)];
        let admin = merged_status(serde_json::json!({}), &seats, None);
        assert_eq!(admin["sessions"][0]["seat"], "kid");
        assert_eq!(admin["games"][0]["seat"], "kid");
        assert_eq!(admin["games"][0]["endable"], true);
        let other = merged_status(serde_json::json!({}), &seats, Some("ffffffffffff"));
        for key in ["sessions", "games"] {
            let row = &other[key][0];
            assert_eq!(row["client"], "", "{key}");
            for gone in ["seat", "client_name", "endable"] {
                assert!(row.get(gone).is_none(), "{key} {gone}");
            }
        }
        let own = merged_status(serde_json::json!({}), &seats, Some("aa11bb22cc33dd"));
        assert_eq!(own["games"][0]["endable"], true, "its own game");
        assert!(own["games"][0].get("seat").is_none());
    }

    /// A switch request drops the answer a fresh probe would have kept.
    #[test]
    fn a_forgotten_door_probe_asks_again() {
        let last: DoorProbe<String> = std::sync::Mutex::new(None);
        assert_eq!(
            probe_door(&last, || Some("blocked".into())).as_deref(),
            Some("blocked")
        );
        forget_probe(&last);
        assert_eq!(probe_door(&last, || None), None);
    }
}
