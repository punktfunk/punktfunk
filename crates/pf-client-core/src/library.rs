//! Game-library client for the host management REST API:
//! `GET https://<host>:<mgmt>/api/v1/library` plus the per-title art proxy.
//!
//! Auth is mTLS: the client presents the persistent identity paired over QUIC;
//! paired certs may read the library routes (no bearer token). The host cert is
//! checked against the pinned SHA-256 fingerprint (`KnownHost::fp_hex`), not a CA.
//!
//! Types (`GameEntry`, `Artwork`, `RunningGame`, `LibraryError`, `base_url`) are
//! portable. The ureq/rustls fetch path is desktop-gated (`linux` / `windows`).

use serde::{Deserialize, Serialize};
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
use std::collections::VecDeque;
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
use std::sync::{Arc, Mutex};
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
use std::time::Duration;

/// Matches host `mgmt::DEFAULT_PORT`. Discovered hosts override via mDNS `mgmt`
/// TXT (`DiscoveredHost::mgmt_port`); a saved host that is not advertising falls
/// back here.
pub const DEFAULT_MGMT_PORT: u16 = 47990;

/// Cover URLs as the host sends them: CDN for custom entries, host-relative
/// `/api/v1/library/art/...` for Steam. Wire also has `logo`; it is not a poster
/// kind, so it is not a field here.
///
/// `Serialize` so [`crate::library_cache`] can write a catalog back verbatim.
/// `skip_serializing_if` keeps omitted host fields omitted, not `null`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Artwork {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub portrait: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hero: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
}

impl Artwork {
    pub fn poster_candidates(&self, base: &str) -> Vec<String> {
        [&self.portrait, &self.header, &self.hero]
            .into_iter()
            .flatten()
            .map(|u| {
                if u.starts_with('/') {
                    format!("{base}{u}")
                } else {
                    u.clone()
                }
            })
            .collect()
    }

    /// Separate from `poster_candidates` so the caller need not invent a `base`.
    pub fn is_empty(&self) -> bool {
        self.portrait.is_none() && self.header.is_none() && self.hero.is_none()
    }
}

/// One title. `id` is store-qualified (`steam:<appid>`, `custom:<id>`) and is the
/// launch handle Hello carries. Host `launch` spec is not a field: launch is by
/// id. `Serialize` for [`crate::library_cache`] — see [`Artwork`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GameEntry {
    pub id: String,
    /// Store badge on the poster (`"steam"`, `"custom"`, …).
    pub store: String,
    pub title: String,
    #[serde(default)]
    pub art: Artwork,
    /// Free-form display string from the host's flattened `GameMeta`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// The rest of that `GameMeta` the launch hold has room to show. The host has sent these
    /// since the library API existed and nothing read them until a screen wanted more than a
    /// title. Every one is optional on the wire, so an older host simply says nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub developer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_year: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// `"game"` (default; older hosts omit) or `"launcher"`. A plain string: the
    /// host owns the vocabulary; an unknown value must not fail the catalog decode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Brand-mark slug (`"steam"`, `"heroic"`, `"playnite"`), not image bytes.
    /// Resolve through [`GameEntry::icon_token`] — never interpolate `icon` raw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Host play stats. `None` until the host has launched the title once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<GameStats>,
}

/// One title's play numbers as the host keeps them: the last launch (unix ms), total and
/// last-run play time (ms), and the launch count. Every field defaults, so a host that adds
/// or drops one never fails the catalog.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct GameStats {
    pub last_played_unix_ms: u64,
    pub play_time_ms: u64,
    pub last_run_ms: u64,
    pub launch_count: u32,
}

/// The console's desktop-tile id. `\0` prefix as Home's Add and Rescan tiles use: a host
/// title id is a store reference, and none of them can start with a NUL. Lives here, not in
/// the console, because [`crate::collate`] has to keep the tile out of every group and the
/// shells that have no such tile simply never match it.
pub const DESKTOP_ID: &str = "\0desktop";

/// The mark that tile draws, as an [`icon`](GameEntry::icon) token. Not a brand mark: each
/// shell resolves it through its own icon set — Lucide `monitor` on the three that carry
/// [`crate::lucide`], the nearest system symbol on Apple and Android.
pub const DESKTOP_ICON: &str = "monitor";

/// Store id → display label. One table: the console, the GTK dialog and the WinUI dialog all
/// drew this from a copy of their own, and a store added to one never reached the others.
pub fn store_label(store: &str) -> &'static str {
    match store {
        "steam" => "Steam",
        "custom" => "Custom",
        "heroic" => "Heroic",
        "lutris" => "Lutris",
        "epic" => "Epic",
        "gog" => "GOG",
        "xbox" => "Xbox",
        _ => "Game",
    }
}

impl GameEntry {
    pub fn is_launcher(&self) -> bool {
        self.role.as_deref() == Some("launcher")
    }

    /// Re-check before interpolating into a resource name or path. Callers
    /// concatenate this into `pf-launcher-{t}-symbolic` and asset lookups.
    pub fn icon_token(&self) -> Option<&str> {
        let t = self.icon.as_deref()?;
        let ok = !t.is_empty()
            && t.len() <= 32
            && t.starts_with(|c: char| c.is_ascii_lowercase())
            && t.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        ok.then_some(t)
    }
}

/// Classified so the UI can tell "not paired" from "wrong pin" from "down".
#[derive(Debug)]
pub enum LibraryError {
    /// Host rejected the client cert — this device is not on the paired list.
    NotPaired,
    /// Host cert did not hash to the pinned fingerprint (impostor or rotated).
    PinMismatch,
    Http(u16),
    Unreachable(String),
}

impl std::fmt::Display for LibraryError {
    /// A phrase, never a sentence. Every caller supplies the frame — a
    /// "Couldn't load the library" title on the three library screens, a
    /// "{label} failed — " lead on a host action, "Couldn't send logs — " on
    /// an upload. A sentence here reads as a second headline under the first.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LibraryError::NotPaired => {
                f.write_str("the host doesn't recognize this device — pair with it first")
            }
            LibraryError::PinMismatch => {
                f.write_str("the host's certificate isn't the one you paired with — pair again")
            }
            LibraryError::Http(code) => write!(f, "the host refused it ({code})"),
            LibraryError::Unreachable(why) => write!(f, "couldn't reach the host — {why}"),
        }
    }
}

pub fn base_url(addr: &str, mgmt_port: u16) -> String {
    if addr.contains(':') {
        format!("https://[{addr}]:{mgmt_port}")
    } else {
        format!("https://{addr}:{mgmt_port}")
    }
}

/// mTLS agent: client cert from `identity`, server checked by `pin`.
/// `pin = None` is TOFU (accept any cert), same as the QUIC connect.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn agent(
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<ureq::Agent, LibraryError> {
    use rustls::pki_types::pem::PemObject;
    let bad =
        |what: &str, e: &dyn std::fmt::Display| LibraryError::Unreachable(format!("{what}: {e}"));
    // Same aws-lc-rs provider the QUIC endpoints install — mixing rustls
    // providers panics.
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| bad("tls config", &e))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(punktfunk_core::tls::PinVerify::new(pin)));
    let cert = rustls::pki_types::CertificateDer::from_pem_slice(identity.0.as_bytes())
        .map_err(|e| bad("client cert pem", &e))?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(identity.1.as_bytes())
        .map_err(|e| bad("client key pem", &e))?;
    let cfg = builder
        .with_client_auth_cert(vec![cert], key)
        .map_err(|e| bad("client auth", &e))?;
    // ureq's `TlsConfig` has no custom-verifier hook; wrap this `ClientConfig`
    // via `tls::ureq_agent`.
    Ok(punktfunk_core::tls::ureq_agent::agent(
        Arc::new(cfg),
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_global(Some(Duration::from_secs(10)))
            .build(),
    ))
}

/// `GET /api/v1/library`. 401/403 → [`LibraryError::NotPaired`]; pin failure →
/// [`LibraryError::PinMismatch`].
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn fetch_games(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Vec<GameEntry>, LibraryError> {
    let agent = agent(identity, pin)?;
    let url = format!("{}/api/v1/library", base_url(addr, mgmt_port));
    let body = match agent.get(&url).call() {
        Ok(mut resp) => resp
            .body_mut()
            .read_to_string()
            .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))?,
        Err(e) => return Err(classify(e)),
    };
    serde_json::from_str(&body).map_err(|e| LibraryError::Unreachable(format!("bad JSON: {e}")))
}

/// One title currently launched, from `GET /api/v1/status`. Partial
/// `ActiveGame`: session/plane/grace stay undecoded so a shelf does not
/// break when the operator payload grows.
#[derive(Clone, Debug, Deserialize)]
pub struct RunningGame {
    /// Store-qualified id (`steam:570`); join key onto [`GameEntry`].
    /// Absent for an operator-typed GameStream command (no catalog row).
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub title: String,
    /// `launching` | `running` | `window` | `exited` | `untracked` | `grace`. A
    /// String so an unknown host value cannot fail the whole list decode.
    #[serde(default)]
    pub state: String,
    /// `running`, and the host will report `window` once the game's window is up.
    /// False from a host that cannot see windows, or predates them.
    #[serde(default)]
    pub awaiting_window: bool,
}

impl RunningGame {
    /// True unless `state == "exited"`. `untracked` (host cannot follow the
    /// process) and `grace` (session gone, process still up) both count as up.
    pub fn is_up(&self) -> bool {
        self.state != "exited"
    }
}

/// `/status` slice the shelf needs. Other operator fields stay undecoded so a
/// schema change there cannot break the library screen.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
#[derive(Deserialize)]
struct HostStatus {
    #[serde(default)]
    games: Vec<RunningGame>,
}

/// `GET /api/v1/status` `games[]`. Best-effort: older host, unreachable, or
/// unknown shape → empty list, never an error. A missing Resume badge is
/// cheaper than failing the library screen.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn fetch_running(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Vec<RunningGame> {
    let Ok(agent) = agent(identity, pin) else {
        return Vec::new();
    };
    let url = format!("{}/api/v1/status", base_url(addr, mgmt_port));
    let Ok(mut resp) = agent.get(&url).call() else {
        return Vec::new();
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return Vec::new();
    };
    serde_json::from_str::<HostStatus>(&body)
        .map(|s| s.games)
        .unwrap_or_default()
}

/// 20 s. What a host has up changes minute to minute, unlike the grants
/// [`crate::host_actions::TTL`] governs — but a home carousel ticks far faster
/// than that, so this is the rate limit, not the display cadence.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub const RUNNING_TTL: std::time::Duration = std::time::Duration::from_secs(20);

/// Process-wide, keyed by host fingerprint — the [`crate::host_actions`] cache
/// with a shorter fuse. One cache so a host tile, its shelf and the menu on it
/// cannot each answer "what is up" differently.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
type RunningCache =
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Vec<RunningGame>)>>;

#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
fn running_cache() -> &'static RunningCache {
    static C: std::sync::OnceLock<RunningCache> = std::sync::OnceLock::new();
    C.get_or_init(Default::default)
}

/// The title to name on a host tile: what this host last said it has up.
///
/// Empty until [`refresh_running`] answers, for a host running nothing, and for
/// one whose entry carries no title — a shell renders the empty string as "no
/// line", which is also the right answer for a host too old to be asked.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn now_playing(fp_hex: &str) -> String {
    running_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(fp_hex)
        .and_then(|(_, games)| games.iter().find(|g| !g.title.is_empty()))
        .map(|g| g.title.clone())
        .unwrap_or_default()
}

/// Spawn a worker unless the cache is still inside [`RUNNING_TTL`]. Idempotent;
/// call it on whatever tick a shell already refreshes host rows on.
///
/// Stamped before the request, so a hung host cannot spawn a worker per tick —
/// [`crate::host_actions::refresh`]'s rule, and for the same reason.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn refresh_running(addr: &str, mgmt_port: u16, fp_hex: &str) {
    if fp_hex.is_empty() {
        return; // empty fingerprint cannot authenticate or key the cache
    }
    {
        let mut c = running_cache().lock().unwrap_or_else(|e| e.into_inner());
        match c.get_mut(fp_hex) {
            Some(entry) if entry.0.elapsed() < RUNNING_TTL => return,
            Some(entry) => entry.0 = std::time::Instant::now(),
            None => {
                c.insert(fp_hex.to_string(), (std::time::Instant::now(), Vec::new()));
            }
        }
    }
    let (addr, fp_hex) = (addr.to_string(), fp_hex.to_string());
    std::thread::Builder::new()
        .name("punktfunk-nowplaying".into())
        .spawn(move || {
            let Ok(identity) = crate::trust::load_or_create_identity() else {
                return;
            };
            let pin = crate::trust::parse_hex32(&fp_hex);
            let up: Vec<RunningGame> = fetch_running(&addr, mgmt_port, &identity, pin)
                .into_iter()
                .filter(RunningGame::is_up)
                .collect();
            running_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(fp_hex, (std::time::Instant::now(), up));
        })
        .ok();
}

/// Drop what this host said — the caller just ended a session on it, so the
/// answer is about to change and the next tick must ask rather than wait out
/// [`RUNNING_TTL`].
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn invalidate_running(fp_hex: &str) {
    running_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(fp_hex);
}

/// 16 MiB. Steam heroes are a few MB; larger is not an image for the decoder.
/// [`crate::art_cache`] holds the same ceiling, so disk never serves what the
/// network would have refused.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub(crate) const ART_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Host-origin URLs (`base` prefix) use the pinned mTLS agent; the art proxy
/// requires the paired cert. Any other origin (custom-entry CDN) uses ureq's
/// default agent: webpki trust, no client cert.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn fetch_art(pinned: &ureq::Agent, base: &str, url: &str) -> Result<Vec<u8>, LibraryError> {
    let mut resp = if url.starts_with(base) {
        pinned.get(url).call()
    } else {
        // Default ureq agent uses the process rustls provider. Install it here;
        // several binaries link this crate and may not have done so.
        punktfunk_core::tls::install_default_provider();
        ureq::get(url)
            .config()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .call()
    }
    .map_err(classify)?;
    // ureq 3's default body cap is below a legitimate Steam hero; raise it.
    resp.body_mut()
        .with_config()
        .limit(ART_MAX_BYTES)
        .read_to_vec()
        .map_err(|e| LibraryError::Unreachable(format!("read image: {e}")))
}

/// Three workers: enough for a LAN art proxy without a connection burst.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
const ART_WORKERS: usize = 3;

/// Walk each job's candidate URLs until one loads — [`crate::art_cache`] first,
/// then the network, which writes what it fetched back. Results arrive on the
/// returned channel; drop the receiver to stop the workers (page popped).
/// Consumer decodes textures on the main loop, cached bytes included.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn spawn_art_fetch(
    base: String,
    identity: (String, String),
    pin: Option<[u8; 32]>,
    jobs: VecDeque<(String, Vec<String>)>,
) -> async_channel::Receiver<(String, Vec<u8>)> {
    let queue = Arc::new(Mutex::new(jobs));
    let (tx, rx) = async_channel::unbounded::<(String, Vec<u8>)>();
    for _ in 0..ART_WORKERS {
        let queue = queue.clone();
        let tx = tx.clone();
        let base = base.clone();
        let identity = identity.clone();
        std::thread::Builder::new()
            .name("punktfunk-lib-art".into())
            .spawn(move || {
                let Ok(agent) = agent(&identity, pin) else {
                    return;
                };
                loop {
                    // Asked before every request, not only on a hit: a title whose posters all
                    // miss never reaches `send_blocking`, so a run of misses used to grind
                    // through the whole queue against a page that had already been closed —
                    // or a host that had gone away.
                    if tx.is_closed() {
                        return;
                    }
                    let job = queue.lock().unwrap().pop_front();
                    let Some((id, candidates)) = job else { break };
                    // Every candidate is asked of disk before any of them is asked of the
                    // network: last launch's winner is often the second URL, and walking in
                    // order would spend a round trip on the first one's known miss.
                    if let Some(bytes) = candidates.iter().find_map(|u| crate::art_cache::load(u)) {
                        if tx.send_blocking((id, bytes)).is_err() {
                            return;
                        }
                        continue;
                    }
                    for url in &candidates {
                        if tx.is_closed() {
                            return;
                        }
                        match fetch_art(&agent, &base, url) {
                            Ok(bytes) => {
                                crate::art_cache::store(url, &bytes);
                                // Receiver dropped (page popped) — stop fetching.
                                if tx.send_blocking((id, bytes)).is_err() {
                                    return;
                                }
                                break;
                            }
                            // Miss (often 404 on a guessed CDN path) — try the next URL.
                            Err(e) => tracing::debug!(%id, url, error = %e, "poster miss"),
                        }
                    }
                }
            })
            .expect("spawn art thread");
    }
    rx
}

#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub(crate) fn classify(e: ureq::Error) -> LibraryError {
    match e {
        ureq::Error::StatusCode(401 | 403) => LibraryError::NotPaired,
        ureq::Error::StatusCode(code) => LibraryError::Http(code),
        // `PinVerify`'s fingerprint-mismatch error. Match this variant only —
        // a broader cert-error arm would also fire on unrelated TLS failures.
        ureq::Error::Rustls(rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        )) => LibraryError::PinMismatch,
        other => LibraryError::Unreachable(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_candidates_order_and_resolution() {
        let art = Artwork {
            portrait: Some("/api/v1/library/art/steam:570/portrait".into()),
            hero: Some("https://cdn.example/hero.jpg".into()),
            header: Some("/api/v1/library/art/steam:570/header".into()),
        };
        assert_eq!(
            art.poster_candidates("https://192.168.1.42:47990"),
            vec![
                "https://192.168.1.42:47990/api/v1/library/art/steam:570/portrait",
                "https://192.168.1.42:47990/api/v1/library/art/steam:570/header",
                "https://cdn.example/hero.jpg",
            ]
        );
        assert!(Artwork::default()
            .poster_candidates("https://h:47990")
            .is_empty());
    }

    #[test]
    fn game_entry_decodes_the_wire_shape() {
        // Wire shape from mgmt: optional art omitted, `launch` present but ignored.
        let json = r#"[
            {"id":"steam:570","store":"steam","title":"Dota 2","platform":"PC",
             "art":{"portrait":"/api/v1/library/art/steam:570/portrait"},
             "launch":{"kind":"steam_appid","value":"570"}},
            {"id":"custom:abc","store":"custom","title":"My Emu","art":{}}
        ]"#;
        let games: Vec<GameEntry> = serde_json::from_str(json).unwrap();
        assert_eq!(games.len(), 2);
        assert_eq!(games[0].id, "steam:570");
        assert_eq!(games[0].platform.as_deref(), Some("PC"));
        assert!(games[1].art.portrait.is_none());
        assert!(
            games[1].platform.is_none(),
            "pre-metadata hosts still parse"
        );
    }

    #[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
    #[test]
    fn running_games_decode_and_untracked_counts_as_up() {
        // Host `/status` shape: extra operator fields present; typed command omits `app_id`.
        let json = r#"{"games":[
            {"app_id":"steam:570","title":"Dota 2","state":"running","plane":"native",
             "client":"iPad","session_id":7},
            {"app_id":"steam:1091500","title":"Cyberpunk","state":"untracked","plane":"native",
             "client":"Deck"},
            {"app_id":"custom:x","title":"Waiting","state":"grace","plane":"native",
             "client":"TV","grace_remaining_s":252},
            {"app_id":"steam:4","title":"Gone","state":"exited","plane":"native","client":"TV"},
            {"title":"A typed command","state":"running","plane":"gamestream","client":"TV"}
        ],"video_streaming":false}"#;
        let status: HostStatus = serde_json::from_str(json).expect("the /status slice decodes");
        assert_eq!(status.games.len(), 5);
        assert!(status.games[1].is_up(), "untracked is up");
        assert!(
            status.games[2].is_up(),
            "grace is up — resuming matters most here"
        );
        assert!(!status.games[3].is_up(), "only a confirmed exit is down");
        assert!(
            status.games[4].app_id.is_none(),
            "a typed command has no catalog id"
        );
    }

    #[test]
    fn an_unknown_state_from_a_newer_host_still_decodes_and_reads_as_up() {
        let one: RunningGame =
            serde_json::from_str(r#"{"app_id":"steam:1","title":"T","state":"hibernating"}"#)
                .expect("an unknown state is not a decode failure");
        assert!(one.is_up());
    }

    #[test]
    fn a_catalog_round_trips_through_the_cache_encoding() {
        // Omitted host fields must stay omitted on the way back, not become `null`.
        let json = r#"[{"id":"steam:570","store":"steam","title":"Dota 2","platform":"PC",
             "art":{"portrait":"/api/v1/library/art/steam:570/portrait"},"role":"launcher",
             "icon":"steam"},
            {"id":"custom:abc","store":"custom","title":"My Emu","art":{}}]"#;
        let games: Vec<GameEntry> = serde_json::from_str(json).unwrap();
        let back: Vec<GameEntry> =
            serde_json::from_str(&serde_json::to_string(&games).unwrap()).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back[0].is_launcher());
        assert_eq!(back[0].icon_token(), Some("steam"));
        assert_eq!(back[0].platform.as_deref(), Some("PC"));
        assert_eq!(
            back[0].art.poster_candidates("https://h:47990"),
            vec!["https://h:47990/api/v1/library/art/steam:570/portrait"]
        );
        assert!(back[1].platform.is_none() && back[1].role.is_none());
    }

    #[test]
    fn ipv6_base_url_is_bracketed() {
        assert_eq!(base_url("fe80::1", 47990), "https://[fe80::1]:47990");
        assert_eq!(base_url("192.168.1.42", 1234), "https://192.168.1.42:1234");
    }
}
