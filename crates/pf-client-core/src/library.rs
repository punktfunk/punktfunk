//! Game-library client for the host management REST API:
//! `GET https://<host>:<mgmt>/api/v1/library/page`, walked to the end, plus the
//! per-title art proxy.
//!
//! Auth is mTLS: the client presents the persistent identity paired over QUIC;
//! paired certs may read the library routes (no bearer token). The host cert is
//! checked against the pinned SHA-256 fingerprint (`KnownHost::fp_hex`), not a CA.
//!
//! Types (`GameEntry`, `Artwork`, `RunningGame`, `LibraryError`, `base_url`) are
//! portable. The ureq/rustls fetch path is desktop-gated (`linux` / `windows`).

use serde::{Deserialize, Serialize};
#[cfg(desktop)]
use std::collections::VecDeque;
#[cfg(desktop)]
use std::sync::{Arc, Mutex};
#[cfg(desktop)]
use std::time::{Duration, Instant};

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

/// Monogram for a poster without art: the first letters of the first two words. Every
/// Rust shell draws its placeholder tiles with it.
pub fn initials(title: &str) -> String {
    title
        .split_whitespace()
        .take(2)
        .filter_map(|w| w.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

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
#[cfg(desktop)]
pub fn agent(
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<ureq::Agent, LibraryError> {
    use rustls::pki_types::pem::PemObject;
    let bad =
        |what: &str, e: &dyn std::fmt::Display| LibraryError::Unreachable(format!("{what}: {e}"));
    let builder = punktfunk_core::tls::pinned_builder(punktfunk_core::tls::PinVerify::new(pin))
        .map_err(|e| bad("tls config", &e))?;
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

/// One answer of `GET /api/v1/library/page`. `total` and `platforms` stay undecoded:
/// every shell collates the whole catalog itself.
#[derive(Deserialize)]
struct LibraryPage {
    items: Vec<GameEntry>,
    #[serde(default)]
    next_cursor: Option<String>,
}

/// Titles a request: the host's ceiling for one page.
pub const PAGE_LIMIT: u32 = 200;

/// 500 pages of 200 is 100 000 titles. A host whose cursor never runs out stops here.
const MAX_PAGES: usize = 500;

/// The whole catalog, a page at a time, so no answer grows with the library. `get` takes
/// the cursor of the page before and answers one page's body; `bad_reply` words a body
/// that does not decode. Any page failing fails the walk: half a catalog is not one.
pub fn walk_pages<E>(
    mut get: impl FnMut(Option<&str>) -> Result<String, E>,
    bad_reply: impl Fn(String) -> E,
) -> Result<Vec<GameEntry>, E> {
    let mut games = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let body = get(cursor.as_deref())?;
        let page: LibraryPage =
            serde_json::from_str(&body).map_err(|e| bad_reply(format!("bad JSON: {e}")))?;
        games.extend(page.items);
        match page.next_cursor {
            // A cursor that does not move would ask for the same page forever.
            Some(next) if !next.is_empty() && cursor.as_deref() != Some(next.as_str()) => {
                cursor = Some(next);
            }
            _ => break,
        }
    }
    Ok(games)
}

#[cfg(desktop)]
fn body_of(
    answer: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
) -> Result<String, LibraryError> {
    answer
        .map_err(classify)?
        .body_mut()
        .read_to_string()
        .map_err(|e| LibraryError::Unreachable(format!("read body: {e}")))
}

/// The host's catalog, walked by `GET /api/v1/library/page`. A host older than that route
/// refuses it on this lane, so `GET /api/v1/library` answers whole instead. 401/403 from
/// both → [`LibraryError::NotPaired`]; pin failure → [`LibraryError::PinMismatch`].
#[cfg(desktop)]
pub fn fetch_games(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Vec<GameEntry>, LibraryError> {
    let agent = agent(identity, pin)?;
    let base = base_url(addr, mgmt_port);
    let bad_reply = LibraryError::Unreachable;
    let paged = walk_pages(
        |cursor| {
            let page = agent
                .get(format!("{base}/api/v1/library/page"))
                .query("limit", PAGE_LIMIT.to_string());
            body_of(match cursor {
                Some(c) => page.query("cursor", c).call(),
                None => page.call(),
            })
        },
        bad_reply,
    );
    match paged {
        Err(LibraryError::NotPaired | LibraryError::Http(404)) => {
            let body = body_of(agent.get(format!("{base}/api/v1/library")).call())?;
            serde_json::from_str(&body).map_err(|e| bad_reply(format!("bad JSON: {e}")))
        }
        walked => walked,
    }
}

/// One title currently launched, from `GET /api/v1/status`. Partial
/// `ActiveGame`: plane and grace stay undecoded so a shelf does not break when
/// the operator payload grows.
#[derive(Clone, Debug, Deserialize)]
pub struct RunningGame {
    /// Store-qualified id (`steam:570`); join key onto [`GameEntry`].
    /// Absent for an operator-typed GameStream command (no catalog row).
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub title: String,
    /// `launching` | `running` | `window` | `exited` | `untracked` | `grace` |
    /// `detached`. A String so an unknown host value cannot fail the whole list decode.
    #[serde(default)]
    pub state: String,
    /// `running`, and the host will report `window` once the game's window is up.
    /// False from a host that cannot see windows, or predates them.
    #[serde(default)]
    pub awaiting_window: bool,
    /// The live session streaming it; `None` for a game nobody streams.
    #[serde(default)]
    pub session_id: Option<u64>,
    /// This device may end it ([`end_game`]): a game it launched. False from a
    /// host that predates the field.
    #[serde(default)]
    pub endable: bool,
}

impl RunningGame {
    /// True unless `state == "exited"`. `untracked` (host cannot follow the
    /// process), `grace` and `detached` (session gone, process still up) count as up.
    pub fn is_up(&self) -> bool {
        self.state != "exited"
    }

    /// A game this device launched that a live session streams: what an in-stream
    /// End game ends.
    pub fn streamed_here(&self) -> bool {
        self.endable && self.session_id.is_some() && self.app_id.is_some()
    }
}

/// What asking the host to end a game came to (`POST /api/v1/game/end`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameEnd {
    Ended,
    /// `409`: the host had nothing of this title left to end.
    NotRunning,
    /// `401`/`404`: a host that predates ending games from a device.
    Unsupported,
    /// `403`: this device's access to the host expired.
    Expired,
    Failed(String),
}

impl GameEnd {
    pub fn from_status(code: u16) -> GameEnd {
        match code {
            200..=299 => GameEnd::Ended,
            409 => GameEnd::NotRunning,
            401 | 404 => GameEnd::Unsupported,
            403 => GameEnd::Expired,
            code => GameEnd::Failed(format!("the host refused it ({code})")),
        }
    }

    /// The player-facing line. The Swift, Kotlin and web clients use the same words.
    pub fn notice(&self, title: &str) -> String {
        match self {
            GameEnd::Ended => format!("Ended {title}."),
            GameEnd::NotRunning => format!("{title} isn't running any more."),
            GameEnd::Unsupported => "This host needs an update to end games from here.".into(),
            GameEnd::Expired => "This device's access to the host has expired.".into(),
            GameEnd::Failed(why) => format!("Couldn't end {title} \u{2014} {why}"),
        }
    }
}

/// `POST /api/v1/game/end` for one title, live session included. The host ends
/// it only if this device launched it. Blocking.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn end_game(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    app_id: &str,
) -> GameEnd {
    let agent = match agent(identity, pin) {
        Ok(a) => a,
        Err(e) => return GameEnd::Failed(e.to_string()),
    };
    let url = format!("{}/api/v1/game/end", base_url(addr, mgmt_port));
    let body = serde_json::json!({ "app_id": app_id, "streaming": true }).to_string();
    match agent
        .post(&url)
        .header("Content-Type", "application/json")
        .send(body)
    {
        Ok(_) => GameEnd::Ended,
        Err(ureq::Error::StatusCode(code)) => GameEnd::from_status(code),
        Err(e) => GameEnd::Failed(classify(e).to_string()),
    }
}

/// `/status` slice the shelf needs. Other operator fields stay undecoded so a
/// schema change there cannot break the library screen.
#[cfg(desktop)]
#[derive(Deserialize, Default)]
struct HostStatus {
    #[serde(default)]
    games: Vec<RunningGame>,
}

/// `GET {path}` on the host's mgmt API, decoded. Any miss (unreachable, an older host
/// without the route, an unknown shape) is `T::default()`, never an error.
#[cfg(desktop)]
pub(crate) fn get_json<T: serde::de::DeserializeOwned + Default>(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    path: &str,
) -> T {
    let Ok(agent) = agent(identity, pin) else {
        return T::default();
    };
    let url = format!("{}{path}", base_url(addr, mgmt_port));
    let Ok(mut resp) = agent.get(&url).call() else {
        return T::default();
    };
    let Ok(body) = resp.body_mut().read_to_string() else {
        return T::default();
    };
    serde_json::from_str(&body).unwrap_or_default()
}

/// `GET /api/v1/status` `games[]`. Best-effort: older host, unreachable, or
/// unknown shape → empty list, never an error. A missing Resume badge is
/// cheaper than failing the library screen.
#[cfg(desktop)]
pub fn fetch_running(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Vec<RunningGame> {
    get_json::<HostStatus>(addr, mgmt_port, identity, pin, "/api/v1/status").games
}

/// A process-wide list per host fingerprint, so every tile, shelf and menu reading it
/// agrees. [`FpCache::refresh`] fetches on a worker at most once a `ttl`, stamping the
/// entry before the request so a hung host cannot spawn a worker per tick.
#[cfg(desktop)]
pub(crate) struct FpCache<T> {
    map: std::sync::OnceLock<Mutex<FpEntries<T>>>,
    ttl: Duration,
    thread: &'static str,
}

#[cfg(desktop)]
type FpEntries<T> = std::collections::HashMap<String, (Instant, Vec<T>)>;

/// A best-effort mgmt GET: address, mgmt port, identity, pin.
#[cfg(desktop)]
type FpFetch<T> = fn(&str, u16, &(String, String), Option<[u8; 32]>) -> Vec<T>;

#[cfg(desktop)]
impl<T: Clone + Send + 'static> FpCache<T> {
    pub(crate) const fn new(ttl: Duration, thread: &'static str) -> Self {
        FpCache {
            map: std::sync::OnceLock::new(),
            ttl,
            thread,
        }
    }

    fn map(&self) -> std::sync::MutexGuard<'_, FpEntries<T>> {
        self.map
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// What this host last answered. Empty until [`FpCache::refresh`] lands.
    pub(crate) fn get(&self, fp_hex: &str) -> Vec<T> {
        self.map()
            .get(fp_hex)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }

    /// Spawn `fetch` unless the entry is still inside the TTL, keeping the rows `keep`
    /// passes. The identity is loaded on the worker so callers need not thread it through.
    pub(crate) fn refresh(
        &'static self,
        addr: &str,
        mgmt_port: u16,
        fp_hex: &str,
        fetch: FpFetch<T>,
        keep: fn(&T) -> bool,
    ) {
        if fp_hex.is_empty() {
            return; // empty fingerprint cannot authenticate or key the cache
        }
        {
            let mut c = self.map();
            match c.get_mut(fp_hex) {
                Some(entry) if entry.0.elapsed() < self.ttl => return,
                Some(entry) => entry.0 = Instant::now(),
                None => {
                    c.insert(fp_hex.to_string(), (Instant::now(), Vec::new()));
                }
            }
        }
        let (addr, fp_hex) = (addr.to_string(), fp_hex.to_string());
        std::thread::Builder::new()
            .name(self.thread.into())
            .spawn(move || {
                let Ok(identity) = crate::trust::load_or_create_identity() else {
                    return;
                };
                let pin = crate::trust::parse_hex32(&fp_hex);
                let found: Vec<T> = fetch(&addr, mgmt_port, &identity, pin)
                    .into_iter()
                    .filter(keep)
                    .collect();
                self.map().insert(fp_hex, (Instant::now(), found));
            })
            .ok();
    }

    pub(crate) fn invalidate(&self, fp_hex: &str) {
        self.map().remove(fp_hex);
    }
}

/// 20 s. What a host has up changes minute to minute, unlike the grants
/// [`crate::host_actions::TTL`] governs — but a home carousel ticks far faster
/// than that, so this is the rate limit, not the display cadence.
#[cfg(desktop)]
pub const RUNNING_TTL: Duration = Duration::from_secs(20);

#[cfg(desktop)]
static RUNNING: FpCache<RunningGame> = FpCache::new(RUNNING_TTL, "punktfunk-nowplaying");

/// The title to name on a host tile: what this host last said it has up.
///
/// Empty until [`refresh_running`] answers, for a host running nothing, and for
/// one whose entry carries no title — a shell renders the empty string as "no
/// line", which is also the right answer for a host too old to be asked.
#[cfg(desktop)]
pub fn now_playing(fp_hex: &str) -> String {
    RUNNING
        .get(fp_hex)
        .into_iter()
        .find(|g| !g.title.is_empty())
        .map(|g| g.title)
        .unwrap_or_default()
}

/// Ask the host what it has up unless [`RUNNING_TTL`] says the last answer still
/// stands. Idempotent; call it on whatever tick a shell already refreshes host rows on.
#[cfg(desktop)]
pub fn refresh_running(addr: &str, mgmt_port: u16, fp_hex: &str) {
    RUNNING.refresh(addr, mgmt_port, fp_hex, fetch_running, RunningGame::is_up);
}

/// What this host last said it has up, from the [`refresh_running`] cache.
#[cfg(all(feature = "desktop", any(target_os = "linux", windows)))]
pub fn running(fp_hex: &str) -> Vec<RunningGame> {
    RUNNING.get(fp_hex)
}

/// Drop what this host said — the caller just ended a session on it, so the
/// answer is about to change and the next tick must ask rather than wait out
/// [`RUNNING_TTL`].
#[cfg(desktop)]
pub fn invalidate_running(fp_hex: &str) {
    RUNNING.invalidate(fp_hex);
}

/// 16 MiB. Steam heroes are a few MB; larger is not an image for the decoder.
/// [`crate::art_cache`] holds the same ceiling, so disk never serves what the
/// network would have refused.
#[cfg(desktop)]
pub(crate) const ART_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// Host-origin URLs (`base` prefix) use the pinned mTLS agent; the art proxy
/// requires the paired cert. Any other origin (custom-entry CDN) uses ureq's
/// default agent: webpki trust, no client cert.
#[cfg(desktop)]
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
#[cfg(desktop)]
const ART_WORKERS: usize = 3;

/// Walk each job's candidate URLs until one loads — [`crate::art_cache`] first,
/// then the network, which writes what it fetched back. Results arrive on the
/// returned channel; drop the receiver to stop the workers (page popped).
/// Consumer decodes textures on the main loop, cached bytes included.
#[cfg(desktop)]
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
                    let bytes = resolve_art(&agent, &base, &id, &candidates, || tx.is_closed());
                    // Receiver dropped (page popped): stop fetching.
                    if let Some(bytes) = bytes {
                        if tx.send_blocking((id, bytes)).is_err() {
                            return;
                        }
                    }
                }
            })
            .expect("spawn art thread");
    }
    rx
}

/// One poster's bytes: every candidate is asked of disk before any of the network, since last
/// launch's winner is often the second URL. A network hit is written back to disk. `gone` is
/// asked between requests, so a closed page stops the walk.
#[cfg(desktop)]
fn resolve_art(
    agent: &ureq::Agent,
    base: &str,
    id: &str,
    candidates: &[String],
    gone: impl Fn() -> bool,
) -> Option<Vec<u8>> {
    if let Some(bytes) = candidates.iter().find_map(|u| crate::art_cache::load(u)) {
        return Some(bytes);
    }
    for url in candidates {
        if gone() {
            return None;
        }
        match fetch_art(agent, base, url) {
            Ok(bytes) => {
                crate::art_cache::store(url, &bytes);
                return Some(bytes);
            }
            // Miss (often 404 on a guessed CDN path): try the next URL.
            Err(e) => tracing::debug!(%id, url, error = %e, "poster miss"),
        }
    }
    None
}

/// Posters most shown right now: newer requests are served first, and past this many waiting
/// the oldest are dropped, since a fast scroll asks for thousands it has already passed.
#[cfg(desktop)]
const ART_QUEUE_MAX: usize = 256;

/// One poster to find: its title's id, the URLs to try, and a hint the decoder reads.
#[cfg(desktop)]
pub struct ArtJob<H> {
    pub id: String,
    pub candidates: Vec<String>,
    pub hint: H,
}

/// The waiting jobs, newest last.
#[cfg(desktop)]
struct ArtQueue<H> {
    jobs: VecDeque<ArtJob<H>>,
    closed: bool,
}

#[cfg(desktop)]
impl<H> ArtQueue<H> {
    /// Queue `job`; returns the ids that fell off the old end.
    fn push(&mut self, job: ArtJob<H>) -> Vec<String> {
        self.jobs.push_back(job);
        let over = self.jobs.len().saturating_sub(ART_QUEUE_MAX);
        self.jobs.drain(..over).map(|j| j.id).collect()
    }

    fn pop(&mut self) -> Option<ArtJob<H>> {
        self.jobs.pop_back()
    }
}

/// Posters on demand for one host: jobs arrive as posters are drawn, and each worker keeps its
/// one connection for the pool's life. `decode` runs on the worker, so the thread that draws
/// only wraps finished pixels. Dropping the pool stops the workers.
#[cfg(desktop)]
pub struct ArtPool<H> {
    shared: Arc<(Mutex<ArtQueue<H>>, std::sync::Condvar)>,
}

#[cfg(desktop)]
impl<H: Send + 'static> ArtPool<H> {
    /// Start the workers. Each result is the job's id and its decoded poster, `None` when no
    /// candidate loaded or decoded.
    pub fn start<T: Send + 'static>(
        base: String,
        identity: (String, String),
        pin: Option<[u8; 32]>,
        decode: impl Fn(Vec<u8>, &H) -> Option<T> + Send + Sync + 'static,
    ) -> (ArtPool<H>, async_channel::Receiver<(String, Option<T>)>) {
        let shared = Arc::new((
            Mutex::new(ArtQueue {
                jobs: VecDeque::new(),
                closed: false,
            }),
            std::sync::Condvar::new(),
        ));
        let (tx, rx) = async_channel::unbounded();
        let decode = Arc::new(decode);
        for _ in 0..ART_WORKERS {
            let (shared, tx, base, identity, decode) = (
                shared.clone(),
                tx.clone(),
                base.clone(),
                identity.clone(),
                decode.clone(),
            );
            let spawned = std::thread::Builder::new()
                .name("punktfunk-lib-art".into())
                .spawn(move || {
                    let Ok(agent) = agent(&identity, pin) else {
                        return;
                    };
                    let gone = || tx.is_closed() || shared.0.lock().unwrap().closed;
                    loop {
                        let job = {
                            let mut queue = shared.0.lock().unwrap();
                            loop {
                                if queue.closed {
                                    return;
                                }
                                if let Some(job) = queue.pop() {
                                    break job;
                                }
                                queue = shared.1.wait(queue).unwrap();
                            }
                        };
                        let bytes = resolve_art(&agent, &base, &job.id, &job.candidates, gone);
                        let decoded = bytes.and_then(|b| decode(b, &job.hint));
                        if tx.send_blocking((job.id, decoded)).is_err() {
                            return;
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "poster worker did not start");
            }
        }
        (ArtPool { shared }, rx)
    }

    /// Ask for a poster ahead of everything already waiting. Returns the ids dropped from the
    /// old end, which were never fetched.
    pub fn push(&self, job: ArtJob<H>) -> Vec<String> {
        let dropped = self.shared.0.lock().unwrap().push(job);
        self.shared.1.notify_one();
        dropped
    }
}

#[cfg(desktop)]
impl<H> Drop for ArtPool<H> {
    fn drop(&mut self) {
        self.shared.0.lock().unwrap().closed = true;
        self.shared.1.notify_all();
    }
}

#[cfg(desktop)]
pub(crate) fn classify(e: ureq::Error) -> LibraryError {
    // `PinVerify`'s fingerprint-mismatch error, and only that: a broader cert arm would also
    // catch unrelated TLS failures. The pinned connector hands it back inside an io error.
    let pin_failure = |r: &rustls::Error| {
        matches!(
            r,
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure
            )
        )
    };
    match e {
        ureq::Error::StatusCode(401 | 403) => LibraryError::NotPaired,
        ureq::Error::StatusCode(code) => LibraryError::Http(code),
        ureq::Error::Rustls(ref r) if pin_failure(r) => LibraryError::PinMismatch,
        ureq::Error::Io(ref io)
            if io
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .is_some_and(pin_failure) =>
        {
            LibraryError::PinMismatch
        }
        other => LibraryError::Unreachable(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The newest request is served first, and a full queue drops its oldest.
    #[cfg(desktop)]
    #[test]
    fn the_art_queue_serves_the_newest_and_drops_the_oldest() {
        let job = |i: usize| ArtJob {
            id: format!("t{i}"),
            candidates: Vec::new(),
            hint: (),
        };
        let mut queue = ArtQueue {
            jobs: VecDeque::new(),
            closed: false,
        };
        for i in 0..ART_QUEUE_MAX {
            assert!(queue.push(job(i)).is_empty());
        }
        assert_eq!(queue.push(job(ART_QUEUE_MAX)), ["t0"]);
        assert_eq!(queue.pop().map(|j| j.id), Some(format!("t{ART_QUEUE_MAX}")));
        assert_eq!(
            queue.pop().map(|j| j.id),
            Some(format!("t{}", ART_QUEUE_MAX - 1))
        );
    }

    /// A changed host certificate is a pin mismatch however ureq wraps it, never a host that
    /// could not be reached.
    #[cfg(desktop)]
    #[test]
    fn a_failed_pin_is_a_pin_mismatch() {
        let pin = || {
            rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            )
        };
        let wrapped = std::io::Error::new(std::io::ErrorKind::InvalidData, pin());
        assert!(matches!(
            classify(ureq::Error::Io(wrapped)),
            LibraryError::PinMismatch
        ));
        assert!(matches!(
            classify(ureq::Error::Rustls(pin())),
            LibraryError::PinMismatch
        ));
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(matches!(
            classify(ureq::Error::Io(refused)),
            LibraryError::Unreachable(_)
        ));
        let expired = rustls::Error::InvalidCertificate(rustls::CertificateError::Expired);
        let wrapped = std::io::Error::new(std::io::ErrorKind::InvalidData, expired);
        assert!(matches!(
            classify(ureq::Error::Io(wrapped)),
            LibraryError::Unreachable(_)
        ));
    }

    fn row(json: &str) -> RunningGame {
        serde_json::from_str(json).unwrap()
    }

    /// Only a game this device launched and still streams is the in-stream End game's.
    #[test]
    fn streamed_here_is_this_devices_live_launch() {
        let live = r#"{"app_id":"steam:1","state":"running","session_id":3,"endable":true}"#;
        assert!(row(live).streamed_here());
        let other = r#"{"app_id":"steam:1","state":"running","session_id":3}"#;
        assert!(!row(other).streamed_here(), "another device's launch");
        let left = r#"{"app_id":"steam:1","state":"detached","endable":true}"#;
        assert!(!row(left).streamed_here(), "nobody streams it");
        assert!(row(left).is_up());
    }

    #[test]
    fn a_game_end_status_maps_to_what_the_player_is_told() {
        assert_eq!(GameEnd::from_status(200), GameEnd::Ended);
        assert_eq!(GameEnd::from_status(409), GameEnd::NotRunning);
        assert_eq!(GameEnd::from_status(401), GameEnd::Unsupported);
        assert_eq!(GameEnd::from_status(404), GameEnd::Unsupported);
        assert_eq!(GameEnd::from_status(403), GameEnd::Expired);
        assert_eq!(
            GameEnd::NotRunning.notice("Hades"),
            "Hades isn't running any more."
        );
    }

    #[test]
    fn initials_take_two_words() {
        assert_eq!(initials("Dota 2"), "D2");
        assert_eq!(initials("half-life"), "H");
        assert_eq!(initials("The Witness III"), "TW");
        assert_eq!(initials(""), "");
    }

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

    #[cfg(desktop)]
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

    fn page(ids: &[&str], next: Option<&str>) -> String {
        let items: Vec<_> = ids
            .iter()
            .map(
                |id| serde_json::json!({"id": id, "store": "custom", "title": id, "hidden": false}),
            )
            .collect();
        serde_json::json!({"items": items, "next_cursor": next, "total": 4, "platforms": []})
            .to_string()
    }

    fn ids(games: &[GameEntry]) -> Vec<&str> {
        games.iter().map(|g| g.id.as_str()).collect()
    }

    #[test]
    fn a_walk_follows_the_cursor_to_the_last_page() {
        let mut asked = Vec::new();
        let games = walk_pages(
            |cursor| {
                asked.push(cursor.map(str::to_string));
                Ok::<_, String>(match cursor {
                    None => page(&["a", "b"], Some("c1")),
                    Some("c1") => page(&["c"], Some("c2")),
                    _ => page(&["d"], None),
                })
            },
            |why| why,
        )
        .unwrap();
        assert_eq!(ids(&games), ["a", "b", "c", "d"]);
        assert_eq!(asked, [None, Some("c1".into()), Some("c2".into())]);
    }

    #[test]
    fn a_walk_ends_on_a_cursor_that_does_not_move() {
        let mut calls = 0;
        let games = walk_pages(
            |_| {
                calls += 1;
                Ok::<_, String>(page(&["a"], Some("stuck")))
            },
            |why| why,
        )
        .unwrap();
        assert_eq!(
            calls, 2,
            "the second answer repeats the cursor it was asked with"
        );
        assert_eq!(games.len(), 2);
    }

    #[test]
    fn a_failed_page_fails_the_walk() {
        let walked = walk_pages(
            |cursor| match cursor {
                None => Ok(page(&["a"], Some("c1"))),
                Some(_) => Err("the host went away".to_string()),
            },
            |why| why,
        );
        assert_eq!(walked.unwrap_err(), "the host went away");
        let undecodable = walk_pages(|_| Ok::<_, String>("[]".into()), |why| why);
        assert!(undecodable.unwrap_err().starts_with("bad JSON"));
    }

    #[test]
    fn ipv6_base_url_is_bracketed() {
        assert_eq!(base_url("fe80::1", 47990), "https://[fe80::1]:47990");
        assert_eq!(base_url("192.168.1.42", 1234), "https://192.168.1.42:1234");
    }
}
