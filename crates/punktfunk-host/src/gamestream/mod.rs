//! GameStream control plane: mDNS, nvhttp (serverinfo + pairing), RTSP, and the
//! ENet control stream. `tokio`/`axum` live here; the per-frame path is
//! `stream`/`video`/`audio` on their own threads.
//!
//! Evidence: `design/gamestream-host-plan.md`.

// Moonlight modules and `rusty_enet`/`rsa` exist only with `feature = "gamestream"`.
// Ports and pairing persistence stay in every build; `tls` is the crate's `https`.
#[cfg(feature = "gamestream")]
pub mod apps;
// Non-Linux builds get a stub `start` inside this module.
#[cfg(feature = "gamestream")]
mod audio;
#[cfg(feature = "gamestream")]
pub(crate) mod cert;
#[cfg(feature = "gamestream")]
mod control;
#[cfg(feature = "gamestream")]
mod crypto;
#[cfg(feature = "gamestream")]
pub mod gamepad;
#[cfg(feature = "gamestream")]
mod input;
#[cfg(feature = "gamestream")]
mod mdns;
#[cfg(feature = "gamestream")]
pub(crate) mod nvhttp;
#[cfg(feature = "gamestream")]
pub mod pairing;
/// Moonlight `SS_PEN`/`SS_TOUCH` → native pen / wire touch. See `design/pen-tablet-input.md`.
#[cfg(feature = "gamestream")]
mod pen;
#[cfg(feature = "gamestream")]
mod rtsp;
#[cfg(feature = "gamestream")]
mod serverinfo;
#[cfg(feature = "gamestream")]
pub(crate) mod stream;
// nvhttp and the management tests name it by this path.
#[cfg(any(feature = "gamestream", test))]
pub(crate) use crate::https as tls;
#[cfg(feature = "gamestream")]
mod video;

use crate::host::AppState;
#[cfg(feature = "gamestream")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "gamestream")]
use std::net::{IpAddr, UdpSocket};
use std::sync::Arc;

/// nvhttp ports. Moonlight derives every stream port as an offset from HTTP 47989.
pub const HTTP_PORT: u16 = 47989;
pub const HTTPS_PORT: u16 = 47984;
pub const RTSP_PORT: u16 = 48010;
pub const VIDEO_PORT: u16 = 47998;
pub const CONTROL_PORT: u16 = 47999;
pub const AUDIO_PORT: u16 = 48000;

/// Per-session A/V ping payload. SETUP hex-encodes these 8 bytes as 16 characters the client echoes.
#[cfg(feature = "gamestream")]
pub const AV_PING_LEN: usize = 8;

/// Grace after an unverified owner datagram, before adopting it as the media endpoint.
#[cfg(feature = "gamestream")]
const AV_PING_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Hard wait for any owner datagram on a media port.
#[cfg(feature = "gamestream")]
const AV_PING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// True when `datagram` starts with this session's ping: SETUP ASCII hex or the raw 8 bytes.
/// Trailing bytes are allowed (`SS_PING` appends a sequence). Compared constant-time.
#[cfg(feature = "gamestream")]
fn ping_matches(datagram: &[u8], expect: &[u8; AV_PING_LEN]) -> bool {
    let hex = hex::encode(expect);
    let ascii =
        datagram.len() >= hex.len() && crypto::ct_eq(&datagram[..hex.len()], hex.as_bytes());
    let raw = datagram.len() >= expect.len() && crypto::ct_eq(&datagram[..expect.len()], expect);
    ascii || raw
}

/// Learn a media stream's client UDP endpoint from the first datagram that belongs to this session.
///
/// Source IP is a filter, not a proof: only the launch owner's packets are considered, but a
/// NAT neighbour or spoofed peer can share that address. The per-session ping minted at
/// `/launch` is the proof; a racer who never saw it cannot produce it.
///
/// The payload check prefers rather than requires. The wire reference does not pin the
/// encoding, so a hard gate would black-screen a correct client. An unverified owner
/// datagram is held and adopted only if the grace window expires with nothing better.
#[cfg(feature = "gamestream")]
pub fn learn_client_endpoint(
    sock: &UdpSocket,
    label: &str,
    owner_ip: Option<IpAddr>,
    expect: &[u8; AV_PING_LEN],
) -> Result<std::net::SocketAddr> {
    let start = std::time::Instant::now();
    let deadline = start + AV_PING_TIMEOUT;
    let mut probe = [0u8; 256];
    // First unverified owner datagram, copied because `probe` is overwritten by later reads.
    let mut fallback: Option<(std::net::SocketAddr, Vec<u8>)> = None;
    let mut grace = deadline;
    loop {
        let remaining = grace.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        sock.set_read_timeout(Some(remaining))?;
        // Timeout here is the grace window, not a failure, once a fallback is in hand.
        let Ok((n, src)) = sock.recv_from(&mut probe) else {
            break;
        };
        if owner_ip.is_some_and(|ip| ip != src.ip()) {
            continue;
        }
        if ping_matches(&probe[..n], expect) {
            tracing::info!(%src, "{label}: client endpoint learned (ping payload verified)");
            return Ok(src);
        }
        if fallback.is_none() {
            fallback = Some((src, probe[..n.min(32)].to_vec()));
            grace = (std::time::Instant::now() + AV_PING_GRACE).min(deadline);
        }
    }
    match fallback {
        Some((src, head)) => {
            tracing::warn!(
                %src,
                bytes = %hex::encode(&head),
                "{label}: first datagram did not carry this session's ping payload — adopting it anyway (source-IP-bound only); report these bytes, they pin the wire encoding"
            );
            Ok(src)
        }
        None => anyhow::bail!("{label}: no client ping from the launch owner within 10s"),
    }
}

/// Advertised host version. Major ≥ 7 tells Moonlight to use SHA-256 for pairing.
pub const APP_VERSION: &str = "7.1.431.-1";
pub const GFE_VERSION: &str = "3.23.0.74";
/// `ServerCodecModeSupport` bits from moonlight-common-c `src/Limelight.h`:
/// SCM_H264 0x1, SCM_HEVC 0x100, SCM_HEVC_MAIN10 0x200, SCM_AV1_MAIN8 0x10000, SCM_AV1_MAIN10 0x20000.
pub const SCM_H264: u32 = 0x0000_0001;
pub const SCM_HEVC: u32 = 0x0000_0100;
pub const SCM_HEVC_MAIN10: u32 = 0x0000_0200;
pub const SCM_AV1_MAIN8: u32 = 0x0001_0000;
pub const SCM_AV1_MAIN10: u32 = 0x0002_0000;
/// SDR baseline: H.264 + HEVC Main + AV1 Main 8-bit. HEVC Main10 is layered at runtime by
/// `serverinfo::codec_mode_support` only when [`host_hdr_capable`] is true — a non-HDR host
/// must not advertise a mode it cannot produce. 4:4:4 stays off; stock Moonlight is 4:2:0.
pub const SERVER_CODEC_MODE_SUPPORT: u32 = SCM_H264 | SCM_HEVC | SCM_AV1_MAIN8;

/// Whether this host can deliver an HDR (10-bit BT.2020 PQ) GameStream.
///
/// Gates `IsHdrSupported`, the 10-bit codec bits in serverinfo, and (with the live
/// capture check at RTSP) honoring `dynamicRangeMode`. Behind `PUNKTFUNK_10BIT`
/// (default on; `=0`/`false`/`off`/`no` disables).
///
/// Windows: always true once the policy is on — the IDD capturer can enable PQ on the
/// virtual display. Linux: portal sessions claim yes (HDR is a live monitor fact, rechecked
/// at RTSP via [`pf_capture::gnome_hdr_monitor_active`]); virtual output is HDR only when
/// [`crate::capture::capturer_supports_hdr_for`] says so (gamescope). Both Linux arms also
/// need [`crate::encode::can_encode_10bit`].
pub fn host_hdr_capable() -> bool {
    if !pf_host_config::config().ten_bit {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        true
    }
    #[cfg(target_os = "linux")]
    {
        let source_can_hdr = match pf_host_config::config().video_source.as_deref() {
            Some("portal") => true,
            // Only a gamescope virtual output can be HDR. `detect()` is the same compositor
            // the session will pick, and it is cached downstream.
            _ => crate::vdisplay::detect()
                .ok()
                .is_some_and(|c| crate::capture::capturer_supports_hdr_for(Some(c), None)),
        };
        // Any 10-bit encoder makes the host HDR-capable. Which bits get advertised is
        // `serverinfo::apply_hdr`; whether this session can carry it is the RTSP honor.
        source_can_hdr
            && (crate::encode::can_encode_10bit(crate::encode::Codec::H265)
                || crate::encode::can_encode_10bit(crate::encode::Codec::Av1))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

/// See [`GsState::video_hdr`].
pub type VideoHdr = std::sync::Arc<std::sync::Mutex<Option<pf_frame::HdrMeta>>>;

/// Cumulative client-loss telemetry from the control stream's periodic `0x0201` loss-stats.
/// Control thread adds; video thread's 1 Hz step reads deltas — no lock, no reset.
#[derive(Default)]
pub struct GsLossStats {
    pub lost: std::sync::atomic::AtomicU64,
    /// A report with `lost == 0` is a healthy heartbeat.
    pub reports: std::sync::atomic::AtomicU64,
}

/// Client `/launch` parameters, shared with RTSP and the media stages.
#[derive(Clone, Copy, Debug)]
pub struct LaunchSession {
    /// AES-128 key for RTSP/control/video/audio (`rikey`).
    pub gcm_key: [u8; 16],
    /// Seeds the per-stream GCM IVs (`rikeyid`).
    pub rikeyid: i32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// `/launch?appid=N` — app-catalog entry for this session.
    pub appid: u32,
    /// `localAudioPlayMode=1`: the player wants sound on the host too.
    pub host_audio: bool,
    /// Source IP of the paired HTTPS client that issued `/launch`. Unauthenticated
    /// RTSP/UDP binds to this so an unpaired peer cannot ride the launch. `None` if
    /// the address could not be captured (RTSP then falls back to launch-present only).
    pub peer_ip: Option<std::net::IpAddr>,
    /// SHA-256 cert fingerprint of the paired client that owns this session. Mode-conflict
    /// admission compares it to tell a same-client re-launch (always allowed) from a different
    /// client (subject to `mode_conflict`). `[u8; 32]` keeps [`LaunchSession`] `Copy`; `None`
    /// when the peer cert could not be read.
    pub owner_fp: Option<[u8; 32]>,
}

/// The Moonlight plane's slice of [`AppState`] ([`AppState::gs`](crate::host::AppState::gs)).
#[cfg(feature = "gamestream")]
pub struct GsState {
    /// GameStream RSA-2048 identity. Moonlight pins it; pairing hashes bind its X.509 bytes.
    /// Native planes present `crate::identity` instead.
    pub identity: cert::ServerIdentity,
    pub pairing: pairing::Pairing,
    /// Bound only while `paired` is non-empty. See [`sync_control`].
    pub(crate) control_gate: control::Gate,
    /// This session's A/V ping payload, minted by `/launch` and `/resume`. Not in
    /// [`LaunchSession`]: the client does not supply it, and resume re-mints while that
    /// struct's keys may not. See [`learn_client_endpoint`].
    pub av_ping: std::sync::atomic::AtomicU64,
    /// RTSP ANNOUNCE video config, consumed on PLAY.
    pub stream: std::sync::Mutex<Option<stream::StreamConfig>>,
    /// RTSP ANNOUNCE audio parameters. Defaults to stereo when the client never ANNOUNCEs them.
    pub audio_params: std::sync::Mutex<audio::AudioParams>,
    /// The display's admission stop flag: raised when another client steals it.
    /// Read by [`AppState::end_if_preempted`](crate::host::AppState::end_if_preempted).
    pub preempted: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Mastering metadata of the frames the video thread encodes, `None` while they are SDR.
    /// [`control`] tells the client each change (`0x010e`).
    pub video_hdr: VideoHdr,
    /// Persistent screen capturer, reused across streams. The slot's `bool` is whether it was
    /// opened with the HDR offer; a stream whose negotiated `hdr` differs drops it and opens
    /// a fresh session at the right depth.
    pub video_cap: stream::CapturerSlot,
}

#[cfg(feature = "gamestream")]
impl GsState {
    pub fn new(identity: cert::ServerIdentity) -> GsState {
        GsState {
            identity,
            pairing: pairing::Pairing::new(),
            control_gate: control::Gate::new(),
            av_ping: std::sync::atomic::AtomicU64::new(0),
            stream: std::sync::Mutex::new(None),
            audio_params: std::sync::Mutex::new(audio::AudioParams::default()),
            preempted: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            video_hdr: VideoHdr::default(),
            video_cap: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

#[cfg(feature = "gamestream")]
impl AppState {
    /// End the session if another client stole its display since the last call. A steal is
    /// a drop, not a quit: the display lingers for the stealer, as a native victim's does.
    pub(crate) fn end_if_preempted(&self) -> bool {
        if !self
            .gs
            .preempted
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return false;
        }
        self.end_session("another client took the display");
        true
    }

    /// Mint a fresh A/V ping for `/launch` or `/resume`. Must run before the client's RTSP SETUP.
    pub fn mint_av_ping(&self) -> [u8; AV_PING_LEN] {
        let payload = crypto::random::<AV_PING_LEN>();
        self.gs.av_ping.store(
            u64::from_be_bytes(payload),
            std::sync::atomic::Ordering::SeqCst,
        );
        payload
    }

    /// This session's A/V ping — what SETUP advertises and the media planes expect back.
    pub fn av_ping_payload(&self) -> [u8; AV_PING_LEN] {
        self.gs
            .av_ping
            .load(std::sync::atomic::Ordering::SeqCst)
            .to_be_bytes()
    }
}

/// Callback media threads invoke on a UDP send error: ends the whole session via
/// [`AppState::end_session`], not just the noticing thread. Built by the RTSP PLAY handler.
pub(crate) type OnSessionLost = Arc<dyn Fn() + Send + Sync>;

/// Bind the ENet control port iff at least one pairing exists. Crate-visible so mgmt
/// unpair can reach past the private `control` module. No-op unless `serve` armed the gate.
#[cfg(feature = "gamestream")]
pub(crate) fn sync_control(state: &Arc<AppState>) -> Result<()> {
    control::sync(state)
}

/// Native-only: no ENet port. Callers (mgmt unpair) stay uniform.
#[cfg(not(feature = "gamestream"))]
pub(crate) fn sync_control(_state: &Arc<AppState>) -> Result<()> {
    Ok(())
}

/// Bring the Moonlight listeners up ahead of nvhttp. The `_nvstream` advert is fatal on
/// failure, since Moonlight cannot find the host without it; `mdns = false` (`--no-mdns`)
/// skips it where multicast is dead. Hold the advert for as long as the plane serves.
#[cfg(feature = "gamestream")]
pub(crate) fn start(state: &Arc<AppState>, mdns: bool) -> Result<Option<crate::discovery::Advert>> {
    let advert = if mdns {
        Some(mdns::advertise(&state.host).context("mDNS advertise")?)
    } else {
        tracing::info!("GameStream mDNS advertisement disabled (--no-mdns / PUNKTFUNK_MDNS)");
        None
    };
    rtsp::spawn(state.clone()).context("start RTSP server")?;
    // ENet (`rusty_enet`, transpiled C) binds only while a pairing exists. Pairing is HTTPS
    // on nvhttp and never touches 47999; the port re-syncs when the first client pins,
    // before that client can `/launch`.
    state.gs.control_gate.enable();
    sync_control(state).context("start ENet control server")?;
    Ok(advert)
}

/// Where the paired-client allow-list persists across restarts.
fn paired_path() -> Option<std::path::PathBuf> {
    Some(pf_paths::config_dir().join("paired.json"))
}

/// Load persisted paired-client certificate DERs. Empty on first run, parse failure, or a
/// store a non-admin planted before the first elevated run.
pub(crate) fn load_paired() -> Vec<Vec<u8>> {
    let Some(path) = paired_path() else {
        return Vec::new();
    };
    if crate::planted::quarantine_planted_secret(&path) {
        return Vec::new();
    }
    let Ok(raw) = std::fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<Vec<u8>>>(&raw) {
        Ok(v) => {
            tracing::info!(clients = v.len(), "loaded persisted pairings");
            v
        }
        Err(e) => {
            tracing::warn!(error = %e, "paired.json unreadable — starting unpaired");
            Vec::new()
        }
    }
}

/// Persist the paired-client allow-list after each successful pairing, through
/// [`pf_paths::replace_secret_file`]: a torn `paired.json` would lock out every client.
pub(crate) fn save_paired(paired: &[Vec<u8>]) {
    let Some(path) = paired_path() else { return };
    let written = serde_json::to_vec(paired)
        .map_err(std::io::Error::other)
        .and_then(|bytes| pf_paths::replace_secret_file(&path, &bytes));
    if let Err(e) = written {
        tracing::warn!(error = %e, "pairings not persisted");
    }
}

/// Operator-supplied per-client display labels, keyed by certificate fingerprint.
///
/// Sidecar to [`paired_path`], not a field inside it: `paired.json` is a bare
/// `Vec<Vec<u8>>` of DERs, and a label is not part of the trust decision — a
/// corrupt labels file must never lock anyone out.
///
/// Every moonlight-common-c client self-signs as `CN=NVIDIA GameStream Client`,
/// so the certificate carries no device identity; without a label, five paired
/// devices are five identical rows.
fn labels_path() -> Option<std::path::PathBuf> {
    Some(pf_paths::config_dir().join("client-labels.json"))
}

/// Serializes the read-modify-write in [`set_client_label`]. Two concurrent renames
/// would otherwise race on a whole-file rewrite and drop one of the two names.
static LABELS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Fingerprint → label map. Empty on first run, unreadable file, or parse failure —
/// a label is cosmetic, so every failure degrades to "no names".
pub(crate) fn load_client_labels() -> std::collections::BTreeMap<String, String> {
    let Some(path) = labels_path() else {
        return Default::default();
    };
    let Ok(raw) = std::fs::read(&path) else {
        return Default::default();
    };
    serde_json::from_slice(&raw).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "client-labels.json unreadable — listing clients without names");
        Default::default()
    })
}

/// Set (`Some`) or clear (`None`) one client's label, persisted atomically.
/// Fingerprints are lowercased so a rename and a later lookup agree.
pub(crate) fn set_client_label(fp_hex: &str, label: Option<&str>) -> Option<String> {
    let _guard = LABELS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fp = fp_hex.to_ascii_lowercase();
    let mut labels = load_client_labels();
    let stored = match label {
        Some(l) => {
            let clean = crate::native_pairing::sanitize_device_name(l, &fp);
            labels.insert(fp, clean.clone());
            Some(clean)
        }
        None => {
            labels.remove(&fp);
            None
        }
    };
    save_client_labels(&labels);
    stored
}

/// Drop labels whose fingerprints are no longer paired, so the file cannot grow
/// without bound and a re-pair of the same cert starts unnamed.
pub(crate) fn retain_client_labels(still_paired: &[Vec<u8>]) {
    use sha2::{Digest, Sha256};
    let _guard = LABELS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let live: std::collections::BTreeSet<String> = still_paired
        .iter()
        .map(|der| hex::encode(Sha256::digest(der)))
        .collect();
    let mut labels = load_client_labels();
    let before = labels.len();
    labels.retain(|fp, _| live.contains(fp));
    if labels.len() != before {
        save_client_labels(&labels);
    }
}

/// Persist the label map the way [`save_paired`] persists the allow-list.
fn save_client_labels(labels: &std::collections::BTreeMap<String, String>) {
    let Some(path) = labels_path() else { return };
    let written = serde_json::to_vec(labels)
        .map_err(std::io::Error::other)
        .and_then(|bytes| pf_paths::replace_secret_file(&path, &bytes));
    if let Err(e) = written {
        tracing::warn!(error = %e, "client labels not persisted");
    }
}

#[cfg(all(test, feature = "gamestream"))]
mod av_ping_tests {
    use super::{ping_matches, AV_PING_LEN};

    const P: [u8; AV_PING_LEN] = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77];

    /// The wire reference does not pin ASCII vs raw, and the modern form wraps the
    /// payload with a sequence number. Every shape those unknowns allow must match.
    #[test]
    fn both_encodings_match_with_or_without_a_trailing_sequence() {
        let hex = b"0011223344556677";
        let raw = &P[..];
        assert!(ping_matches(hex, &P), "ASCII hex, exactly");
        assert!(ping_matches(raw, &P), "decoded bytes, exactly");
        assert!(
            ping_matches(&[&hex[..], &[0, 0, 0, 1]].concat(), &P),
            "hex + seq"
        );
        assert!(
            ping_matches(&[raw, &[0, 0, 0, 1]].concat(), &P),
            "raw + seq"
        );
    }

    #[test]
    fn anything_else_does_not_match() {
        assert!(!ping_matches(b"", &P), "empty");
        assert!(!ping_matches(b"PING", &P), "the legacy fixed ping");
        assert!(!ping_matches(b"001122334455667", &P), "one hex char short");
        assert!(!ping_matches(&P[..7], &P), "one raw byte short");
        assert!(
            !ping_matches(b"0011223344556678", &P),
            "last hex char wrong"
        );
        let mut near = P;
        near[7] ^= 1;
        assert!(!ping_matches(&near, &P), "last raw byte wrong");
        // Former fixed ping, now that every session mints its own.
        assert!(!ping_matches(b"0011223344556677", &[0xAB; AV_PING_LEN]));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn secrets_are_written_owner_only() {
        let dir = std::env::temp_dir().join(format!("pf-secret-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        pf_paths::create_private_dir(&dir).expect("create private dir");
        let dmode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dmode, 0o700, "config dir must be owner-only (0700)");

        let key = dir.join("key.pem");
        pf_paths::write_secret_file(&key, b"-----BEGIN PRIVATE KEY-----\n...")
            .expect("write secret");
        let fmode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600, "private key must be owner-only (0600)");

        // Overwrite must keep 0600 (truncate + reopen, not create).
        pf_paths::write_secret_file(&key, b"new contents").expect("rewrite secret");
        let fmode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
        assert_eq!(fmode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
