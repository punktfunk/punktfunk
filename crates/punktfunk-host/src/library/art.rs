//! Artwork serving: the on-disk cover store, local-file confinement, and the art-proxy rewrite.
//!
//! `GET /library` advertises `/api/v1/library/art/<id>/<kind>` for every cover this host can
//! serve — a local path a plugin resolved, and an `http(s)`/`data:` URL the host can store. The
//! proxy reads a local path from an allowed root; on a store miss it fetches the URL once,
//! sniffs it and keeps the bytes under [`art_store_dir`], so the first client to ask pays the
//! CDN for all of them. No warmer and no scan-time fetch: a request is the only trigger.
//!
//! Confinement is load-bearing: the proxy runs in the host process and the plugin lane supplies
//! the path. `PUNKTFUNK_LIBRARY_ART_ROOTS` replaces the default roots; the store is a root on
//! top of whatever they are, and its files are named `sha256(url)`, so no plugin string chooses
//! a path inside it. A URL the fetch refuses keeps a marker and stays in the catalog verbatim
//! for the client to fetch itself, as before the store existed.

use super::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as plat;
#[cfg(not(windows))]
mod posix;
#[cfg(not(windows))]
use self::posix as plat;

/// 16 MiB. A cover never approaches it; the ceiling is what bounds host memory on a disk read
/// and on a fetch.
const MAX_ART_BYTES: u64 = 16 * 1024 * 1024;

/// A stored cover is revalidated no more often than this, whatever the CDN's `max-age` says:
/// revalidation is a network round trip on a client's request path.
const ART_REVALIDATE_S: u64 = 24 * 60 * 60;

/// What one [`fetch_art`] attempt produced.
enum Fetch {
    /// 200, and the bytes sniffed as an image.
    Fresh {
        bytes: Vec<u8>,
        ctype: &'static str,
        etag: Option<String>,
        max_age_s: u64,
    },
    /// 304, or a CDN this host could not reach: serve what is stored and ask again tomorrow.
    Keep,
    /// Not servable — wrong scheme, a redirect, a non-image type, or over [`MAX_ART_BYTES`].
    Refused,
}

/// Fetch one cover. `data:` decodes inline; `http(s)` streams at most [`MAX_ART_BYTES`] and
/// accepts a declared image on 200 only. Sniffing decides the stored type; `etag` makes it
/// conditional. Redirects are refused so an artwork URL cannot aim the host at an internal
/// endpoint. Logs carry only the origin: userinfo, paths and queries can hold CDN credentials.
/// Blocking (`ureq`) — call off the async runtime.
fn fetch_art(url: &str, etag: Option<&str>) -> Fetch {
    use base64::Engine as _;
    if let Some(rest) = url.strip_prefix("data:") {
        let Some((meta, data)) = rest.split_once(',') else {
            return Fetch::Refused;
        };
        let decoded = if meta.contains(";base64") {
            base64::engine::general_purpose::STANDARD.decode(data).ok()
        } else {
            Some(data.as_bytes().to_vec())
        };
        let Some(bytes) = decoded else {
            return Fetch::Refused;
        };
        return match sniff_image_type(&bytes) {
            Some(ctype) => Fetch::Fresh {
                bytes,
                ctype,
                etag: None,
                max_age_s: 0,
            },
            None => Fetch::Refused,
        };
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Fetch::Refused;
    }
    let log_url = crate::hooks::webhook_origin(url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .max_redirects(0)
        // Judge the status here: 304 is a hit on what is stored, every other non-200 a refusal.
        .http_status_as_error(false)
        .build()
        .into();
    let mut req = agent.get(url);
    if let Some(tag) = etag {
        req = req.header("If-None-Match", tag);
    }
    let Ok(mut resp) = req.call() else {
        tracing::debug!(url = %log_url, "art store: cover fetch did not complete");
        return Fetch::Keep;
    };
    let status = resp.status().as_u16();
    if status == 304 {
        return Fetch::Keep;
    }
    if status != 200 {
        // 5xx and 429 are the CDN having a bad minute, not a bad URL. Refusing would leave a
        // marker behind and keep this cover out of the store until someone clears it by hand.
        if status >= 500 || status == 429 {
            tracing::debug!(url = %log_url, status, "art store: cover fetch deferred");
            return Fetch::Keep;
        }
        tracing::debug!(
            url = %log_url,
            status,
            "art store: refusing a cover the CDN did not serve"
        );
        return Fetch::Refused;
    }
    let (declared, etag, max_age_s) = {
        let h = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let cache = h("Cache-Control").unwrap_or_default();
        (
            h("Content-Type").unwrap_or_default(),
            h("ETag"),
            max_age(&cache),
        )
    };
    if !declared
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("image/")
    {
        tracing::debug!(url = %log_url, ctype = %declared, "art store: refusing a cover that is not an image");
        return Fetch::Refused;
    }
    match resp
        .body_mut()
        .with_config()
        .limit(MAX_ART_BYTES)
        .read_to_vec()
    {
        Ok(bytes) => match sniff_image_type(&bytes) {
            Some(ctype) => Fetch::Fresh {
                bytes,
                ctype,
                etag,
                max_age_s,
            },
            None => Fetch::Refused,
        },
        Err(ureq::Error::BodyExceedsLimit(_)) => {
            tracing::debug!(url = %log_url, "art store: refusing a cover over the size ceiling");
            Fetch::Refused
        }
        // A cut transfer is the network, not the URL: leave the URL usable.
        Err(_) => Fetch::Keep,
    }
}

/// Seconds a `Cache-Control` header allows, 0 when it names no `max-age`. Directives are
/// case-insensitive.
fn max_age(header: &str) -> u64 {
    let lower = header.to_ascii_lowercase();
    lower
        .split(',')
        .filter_map(|d| d.trim().strip_prefix("max-age="))
        .find_map(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// Accepted: `file://…` (the plugin-kit contract, [`file_url_to_path`]), Windows drive-absolute
/// and UNC, and POSIX absolute. Two `/`-leading shapes are excluded because POSIX absolute would
/// otherwise swallow them: the host's own `/api/v1/library/art/…` (must survive a second
/// [`proxy_art`] pass) and a protocol-relative URL (`//cdn/…`).
pub fn is_local_art_path(v: &str) -> bool {
    if v.starts_with("http://") || v.starts_with("https://") || v.starts_with("data:") {
        return false;
    }
    if v.starts_with("file://") {
        return true;
    }
    let b = v.as_bytes();
    if (b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || v.starts_with("\\\\") {
        return true;
    }
    v.starts_with('/') && !v.starts_with("//") && !v.starts_with("/api/")
}

/// `http(s)` and `data:` — the shapes [`fetch_art`] can store. A protocol-relative `//cdn/…` or
/// a relative path is neither local art nor a URL this host resolves, so it passes through.
fn is_remote_art_url(v: &str) -> bool {
    v.starts_with("http://") || v.starts_with("https://") || v.starts_with("data:")
}

/// Decode a `file://` art value to a filesystem path. The kit emits percent-encoded URLs;
/// a raw path with no `%` round-trips either way.
///
/// Empty authority: `file:///home/u/c.jpg` → `/home/u/c.jpg`, and `file:///C:/covers/c.jpg` →
/// `C:/covers/c.jpg` (the drive letter arrives after the extra slash). Non-empty authority is
/// UNC: `file://nas/share/c.jpg` → `\\nas\share\c.jpg`. Anything else is returned untouched.
fn file_url_to_path(v: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    let Some(rest) = v.strip_prefix("file://") else {
        return Cow::Borrowed(v);
    };
    let decoded = percent_decode(rest);
    match decoded.strip_prefix('/') {
        // `file:///…` — the empty-authority form. A Windows drive letter (`/C:/…`) loses the slash;
        // a POSIX path keeps it.
        Some(after) if after.as_bytes().get(1) == Some(&b':') => Cow::Owned(after.to_string()),
        Some(_) => Cow::Owned(decoded),
        // `file://server/share/…` — a UNC path spelled as a URL.
        None => Cow::Owned(format!("\\\\{}", decoded.replace('/', "\\"))),
    }
}

/// Percent-decode `%XX`. Invalid escapes stay verbatim — a bare `%` in a real path is likelier
/// than a malformed kit URL — and a wrong decode then fails the regular-file check, never a
/// wrong read.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// Filesystem roots the art proxy may read from.
///
/// The proxy runs in the host process (LocalSystem on Windows) and the plugin lane supplies the
/// path, so a missing root would make "serve this cover" equal "read any file as SYSTEM". Default
/// is the users base — `%PUBLIC%`'s parent, because SYSTEM's `%USERPROFILE%` is
/// `…\config\systemprofile` and is not where launchers live — plus [`steam_art_roots`] and
/// [`super::launch::playnite_art_roots`] (a portable Playnite keeps covers beside the exe).
/// `PUNKTFUNK_LIBRARY_ART_ROOTS` (`;`-separated) replaces the whole default. [`art_store_dir`]
/// is a root on top of either: its files are named `sha256(url)`, so nothing a plugin publishes
/// picks a path inside it.
fn art_roots() -> Vec<PathBuf> {
    if let Some(configured) = std::env::var_os("PUNKTFUNK_LIBRARY_ART_ROOTS") {
        let mut roots: Vec<PathBuf> = std::env::split_paths(&configured)
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        roots.push(art_store_dir());
        return roots;
    }
    let mut roots = Vec::new();
    // `%PUBLIC%` is `C:\Users\Public` on every supported Windows; its parent is the users base.
    if let Some(public) = std::env::var_os("PUBLIC") {
        if let Some(base) = PathBuf::from(public).parent() {
            roots.push(base.to_path_buf());
        }
    }
    if roots.is_empty() {
        if let Some(drive) = std::env::var_os("SystemDrive") {
            roots.push(PathBuf::from(drive).join("Users"));
        }
    }
    roots.extend(plat::extra_roots());
    roots.push(art_store_dir());
    roots
}

/// Canonicalize first so a junction out of the root is resolved before the containment test.
/// The config-dir exclusion is unconditional even if `PUNKTFUNK_LIBRARY_ART_ROOTS` names it.
fn art_path_is_confined(path: &Path) -> bool {
    // Refuse UNC before touching the filesystem: `canonicalize` would itself coerce the host's
    // machine account into outbound SMB. Any two leading separators — `\\`, `//`, mixed — count;
    // a bare `starts_with(r"\\")` missed the forms Windows accepts equally.
    let lossy = path.to_string_lossy();
    let bytes = lossy.as_bytes();
    let is_sep = |c: u8| c == b'\\' || c == b'/';
    if bytes.len() >= 2 && is_sep(bytes[0]) && is_sep(bytes[1]) {
        return false;
    }
    let Ok(real) = path.canonicalize() else {
        return false;
    };
    resolved_art_path_is_confined(&real)
}

/// Containment half of [`art_path_is_confined`] on an already-resolved path (`canonicalize` or
/// [`final_path_of`]). The read path must judge the object it opened, not the path it was asked.
fn resolved_art_path_is_confined(real: &Path) -> bool {
    if let Ok(config) = pf_paths::config_dir().canonicalize() {
        if real.starts_with(&config) {
            return false;
        }
    }
    art_roots()
        .iter()
        .filter_map(|r| r.canonicalize().ok())
        .any(|root| real.starts_with(&root))
}

/// Serve what the bytes are, not what the extension claims — an extensionless secret or a
/// `key.pem` renamed `cover.png` must not come back as `application/octet-stream`.
fn sniff_image_type(bytes: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| bytes.starts_with(sig);
    if starts(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some("image/gif");
    }
    if starts(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if starts(b"BM") {
        return Some("image/bmp");
    }
    if starts(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("image/x-icon");
    }
    // TGA has no magic number. Validate the fixed header fields instead (colour-map type is 0/1,
    // image type is one of the six defined codes) — enough that no plausible secret passes.
    if bytes.len() >= 18
        && matches!(bytes[1], 0 | 1)
        && matches!(bytes[2], 0 | 1 | 2 | 3 | 9 | 10 | 11)
    {
        return Some("image/x-tga");
    }
    None
}

/// Decode `file://` first, matching [`local_art_bytes`]. `Path::new` on a raw `file:///…` is a
/// relative path whose first component is `file:`, which canonicalizes against the cwd and
/// reads as "outside every root" — write time would then reject what read time would serve.
pub fn art_path_is_servable(value: &str) -> bool {
    // Idempotent for the already-decoded caller: the decoded form no longer carries the prefix,
    // so `local_art_bytes` passing its own output back through here is a no-op, not a second
    // percent-decode of a path that legitimately contains `%`.
    let value = file_url_to_path(value);
    let p = Path::new(&*value);
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .is_some_and(|e| {
            matches!(
                e.as_str(),
                "jpg" | "jpeg" | "png" | "webp" | "gif" | "bmp" | "ico" | "tga"
            )
        });
    ext_ok && art_path_is_confined(p)
}

/// Reject a local-file art value the proxy would refuse to serve, so it never reaches the
/// catalog. URLs and already-proxied paths pass through. `Err` names the field.
pub fn validate_art_paths(art: &Artwork) -> Result<(), String> {
    for (field, value) in [
        ("portrait", &art.portrait),
        ("hero", &art.hero),
        ("logo", &art.logo),
        ("header", &art.header),
    ] {
        let Some(v) = value.as_deref() else { continue };
        if is_local_art_path(v) && !art_path_is_servable(v) {
            return Err(format!(
                "art.{field}: local art must be an image file (jpg/png/webp/gif/bmp/ico/tga) inside \
                 an allowed art root — set PUNKTFUNK_LIBRARY_ART_ROOTS if the library lives \
                 elsewhere, or send an http(s) URL instead"
            ));
        }
    }
    Ok(())
}

/// Drop local-file art the proxy would refuse to serve, returning the `(field, value)` pairs
/// removed. URLs and already-proxied paths stay.
///
/// Provider-reconcile counterpart to [`validate_art_paths`]. A custom-entry PUT can 400 on one
/// path; a plugin reconcile that 400s on one cover would drop the whole store. `None` is the
/// no-art shape every client already renders.
pub fn sanitize_art_paths(art: &mut Artwork) -> Vec<(&'static str, String)> {
    let mut dropped = Vec::new();
    for (field, value) in [
        ("portrait", &mut art.portrait),
        ("hero", &mut art.hero),
        ("logo", &mut art.logo),
        ("header", &mut art.header),
    ] {
        let unservable = value
            .as_deref()
            .is_some_and(|v| is_local_art_path(v) && !art_path_is_servable(v));
        if unservable {
            if let Some(v) = value.take() {
                dropped.push((field, v));
            }
        }
    }
    dropped
}

/// [`MAX_ART_BYTES`] caps the read. Convert `file://`
/// first ([`file_url_to_path`]) so confinement and the read see the same path. Percent-decode
/// before canonicalize, or a `%2e%2e` escape is invisible to the traversal check.
pub fn local_art_bytes(path: &str) -> Option<(Vec<u8>, String)> {
    let path = file_url_to_path(path);
    if !art_path_is_servable(&path) {
        tracing::debug!(
            path = %path,
            "art proxy: refusing a path outside the allowed art roots"
        );
        return None;
    }
    // Re-check confinement and size on the opened handle, then read that handle with a hard cap.
    // A link swap cannot substitute a different file between validation and consumption.
    let p = std::path::Path::new(&*path);
    let mut f = std::fs::File::open(p).ok()?;
    let real = plat::final_path_of(&f)?;
    if !resolved_art_path_is_confined(&real) {
        tracing::debug!(
            path = %path,
            "art proxy: opened file resolves outside the allowed art roots"
        );
        return None;
    }
    let meta = f.metadata().ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_ART_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    use std::io::Read as _;
    (&mut f)
        .take(MAX_ART_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_ART_BYTES {
        return None;
    }
    let ctype = sniff_image_type(&bytes)?;
    Some((bytes, ctype.to_string()))
}

/// Where the covers this host fetched live: `<key>.<ext>` beside `<key>.json` ([`ArtMeta`]),
/// plus a zero-byte `<key>.refused` for a URL a fetch rejected, all keyed by [`art_key`].
///
/// `PUNKTFUNK_LIBRARY_ART_CACHE` moves it; the default is the per-user cache dir. Never the
/// config dir — [`resolved_art_path_is_confined`] refuses that root unconditionally.
fn art_store_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("PUNKTFUNK_LIBRARY_ART_CACHE").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    #[cfg(windows)]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(windows))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.filter(|b| !b.as_os_str().is_empty())
        .unwrap_or_else(std::env::temp_dir)
        .join("punktfunk")
        .join("art")
}

/// Ceiling on the whole store, `PUNKTFUNK_LIBRARY_ART_CACHE_MB` over the 512 MiB default.
fn art_store_cap() -> u64 {
    std::env::var("PUNKTFUNK_LIBRARY_ART_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(512)
        * 1024
        * 1024
}

/// The store's file stem for an art URL. Hashing keeps a plugin's string out of the path, and
/// makes a changed URL a miss on a name nothing writes again rather than a stale hit.
fn art_key(url: &str) -> String {
    hex::encode(Sha256::digest(url.as_bytes()))
}

/// What the store knows about one blob. Rewritten on every revalidation, so a CDN that goes
/// dark costs one timeout a day rather than one per request.
#[derive(Clone, Serialize, Deserialize)]
struct ArtMeta {
    /// The URL these bytes came from. The lookup is the hash of it; this copy is for an
    /// operator reading the store.
    url: String,
    /// The sniffed image type. Also names the blob: `<key>.<art_ext(ctype)>`.
    ctype: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    etag: Option<String>,
    /// The CDN's `max-age`, floored at [`ART_REVALIDATE_S`].
    #[serde(default)]
    max_age_s: u64,
    /// When this blob was last confirmed, in epoch seconds.
    #[serde(default)]
    fetched_at_s: u64,
}

/// Blob extension for a sniffed type. [`sniff_image_type`] is the only producer, so anything
/// else never reached the store.
fn art_ext(ctype: &str) -> Option<&'static str> {
    Some(match ctype {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/bmp" => "bmp",
        "image/x-icon" => "ico",
        "image/x-tga" => "tga",
        _ => return None,
    })
}

fn art_meta_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.json"))
}

fn art_refused_path(dir: &Path, key: &str) -> PathBuf {
    dir.join(format!("{key}.refused"))
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True once a fetch refused this URL. [`proxy_art`] then leaves it in the catalog verbatim, so
/// the client fetches it itself exactly as it did before the store existed.
fn remote_art_refused(url: &str) -> bool {
    art_refused_path(&art_store_dir(), &art_key(url)).exists()
}

/// Fresh until the CDN's `max-age`, and never for less than [`ART_REVALIDATE_S`]. A `data:`
/// blob is its own URL, so it never goes stale.
fn art_is_fresh(meta: &ArtMeta) -> bool {
    meta.url.starts_with("data:")
        || now_s().saturating_sub(meta.fetched_at_s) < meta.max_age_s.max(ART_REVALIDATE_S)
}

fn read_art_meta(dir: &Path, key: &str) -> Option<ArtMeta> {
    let raw = std::fs::read_to_string(art_meta_path(dir, key)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Best-effort: art is not worth failing a request over, and a lost meta is one refetch.
fn write_art_meta(dir: &Path, key: &str, meta: &ArtMeta) {
    let Ok(json) = serde_json::to_string(meta) else {
        return;
    };
    let path = art_meta_path(dir, key);
    let tmp = path.with_extension("json.part");
    if std::fs::write(&tmp, json).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Store one fetched cover: the blob, the meta beside it, then the size cap. The blob lands
/// through a rename, so a half-written cover is never served.
fn write_art(dir: &Path, key: &str, bytes: &[u8], meta: &ArtMeta) {
    let Some(ext) = art_ext(&meta.ctype) else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let blob = dir.join(format!("{key}.{ext}"));
    let tmp = blob.with_extension(format!("{ext}.part"));
    if std::fs::write(&tmp, bytes).is_err() || std::fs::rename(&tmp, &blob).is_err() {
        tracing::debug!(path = %blob.display(), "art store: cover not written");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    write_art_meta(dir, key, meta);
    evict_art_store(dir);
}

/// Hold the store under [`art_store_cap`], oldest file first. Dropping a blob and leaving its
/// meta behind costs one refetch, so the pass needs no per-entry bookkeeping.
fn evict_art_store(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(SystemTime, u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some((meta.modified().ok()?, meta.len(), e.path()))
        })
        .collect();
    let cap = art_store_cap();
    let mut total: u64 = files.iter().map(|(_, len, _)| *len).sum();
    if total <= cap {
        return;
    }
    files.sort_by_key(|(mtime, _, _)| *mtime);
    for (_, len, path) in files {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// Delete every stored cover and every refusal marker; the next request for one fetches it
/// again. Returns how many files went and how many bytes they held.
pub fn clear_art_store() -> Result<(usize, u64)> {
    let dir = art_store_dir();
    let (mut files, mut bytes) = (0usize, 0u64);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok((files, bytes));
    };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        std::fs::remove_file(entry.path())
            .with_context(|| format!("remove {}", entry.path().display()))?;
        files += 1;
        bytes += meta.len();
    }
    Ok((files, bytes))
}

/// Serve a remote cover from the store: the first client to ask pays the CDN, every other one
/// reads the disk. A stale hit revalidates once a day and keeps serving when that fails — an
/// unreachable CDN is what the store is for. Blocking — call off the async runtime.
fn stored_remote_art(url: &str) -> Option<(Vec<u8>, String)> {
    let dir = art_store_dir();
    let key = art_key(url);
    if art_refused_path(&dir, &key).exists() {
        return None;
    }
    // The blob is read back through the proxy's own confinement, not opened directly.
    let stored = read_art_meta(&dir, &key).and_then(|meta| {
        let blob = dir.join(format!("{key}.{}", art_ext(&meta.ctype)?));
        Some((meta, local_art_bytes(blob.to_str()?)?))
    });
    if let Some((meta, hit)) = &stored {
        if art_is_fresh(meta) {
            return Some(hit.clone());
        }
    }
    let etag = stored.as_ref().and_then(|(m, _)| m.etag.clone());
    match fetch_art(url, etag.as_deref()) {
        Fetch::Fresh {
            bytes,
            ctype,
            etag,
            max_age_s,
        } => {
            let meta = ArtMeta {
                url: url.to_string(),
                ctype: ctype.to_string(),
                etag,
                max_age_s,
                fetched_at_s: now_s(),
            };
            write_art(&dir, &key, &bytes, &meta);
            Some((bytes, meta.ctype))
        }
        // Serve what is on disk and hold the next attempt off for a day. With nothing stored,
        // only an outright refusal marks the URL: a CDN we cannot reach is not a bad URL.
        other => match stored {
            Some((meta, hit)) => {
                write_art_meta(
                    &dir,
                    &key,
                    &ArtMeta {
                        fetched_at_s: now_s(),
                        ..meta
                    },
                );
                Some(hit)
            }
            None => {
                if matches!(other, Fetch::Refused) {
                    let _ = std::fs::create_dir_all(&dir);
                    let _ = std::fs::File::create(art_refused_path(&dir, &key));
                }
                None
            }
        },
    }
}

/// Bytes for one art value: a confined local file, or a remote URL the host stores on first
/// use. The two shapes [`proxy_art`] advertises and no other, so what the catalog points at is
/// what this resolves. Blocking — call off the async runtime.
pub(crate) fn resolve_art_bytes(v: &str) -> Option<(Vec<u8>, String)> {
    if is_local_art_path(v) {
        local_art_bytes(v)
    } else if is_remote_art_url(v) {
        stored_remote_art(v)
    } else {
        None
    }
}

/// Advertise the host's art proxy for every cover it can serve on `GET /library`: a local path
/// (a client cannot reach `C:\…`) and a remote URL (the host stores it once, for all of them).
/// A URL a fetch already refused stays verbatim so the client can still try it, and an
/// already-proxied path is left alone.
///
/// Each path carries `?v=`, the head of the source value's own hash. Without it the path stayed
/// the same when the operator pointed an entry at a different cover, and nothing in front of the
/// host — a browser holding `max-age`, a client's disk cache keyed by URL — ever asked again. It
/// costs no I/O and moves exactly when the source does; the same source serving new bytes is
/// still the ETag's job.
pub fn proxy_art(id: &str, art: &mut Artwork) {
    let rw = |field: &mut Option<String>, kind: &str| {
        let proxied = field
            .as_deref()
            .filter(|v| is_local_art_path(v) || (is_remote_art_url(v) && !remote_art_refused(v)))
            .map(|v| format!("/api/v1/library/art/{id}/{kind}?v={}", &art_key(v)[..16]));
        if let Some(path) = proxied {
            *field = Some(path);
        }
    };
    rw(&mut art.portrait, "portrait");
    rw(&mut art.hero, "hero");
    rw(&mut art.logo, "logo");
    rw(&mut art.header, "header");
}

/// Best box-art for a library id, for GameStream `/appasset` (Moonlight fetches covers from the
/// host, not the CDN). Blocking — call off the async runtime.
pub fn fetch_box_art(id: &str) -> Option<(Vec<u8>, String)> {
    let art = merged_art(id)?;
    [
        ArtKind::Portrait,
        ArtKind::Header,
        ArtKind::Hero,
        ArtKind::Logo,
    ]
    .into_iter()
    .filter_map(|kind| art_field(&art, kind))
    .find_map(|v| resolve_art_bytes(&v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_kind_parses_known_names_only() {
        assert_eq!(ArtKind::parse("portrait"), Some(ArtKind::Portrait));
        assert_eq!(ArtKind::parse("hero"), Some(ArtKind::Hero));
        assert_eq!(ArtKind::parse("logo"), Some(ArtKind::Logo));
        assert_eq!(ArtKind::parse("header"), Some(ArtKind::Header));
        assert_eq!(ArtKind::parse("background"), None);
    }

    /// The declared type is not trusted: a `data:` URL that says `image/png` and carries a key
    /// is refused, exactly as a file with the same lie is.
    #[test]
    fn fetch_art_decodes_a_data_url_and_sniffs_it() {
        use base64::Engine as _;
        let png = base64::engine::general_purpose::STANDARD.encode(PNG);
        let Fetch::Fresh { bytes, ctype, .. } =
            fetch_art(&format!("data:image/png;base64,{png}"), None)
        else {
            panic!("a base64 png decodes");
        };
        assert_eq!(bytes, PNG);
        assert_eq!(ctype, "image/png");
        assert!(matches!(
            fetch_art("data:image/png;base64,SGk=", None),
            Fetch::Refused
        ));
        assert!(matches!(
            fetch_art("file:///etc/passwd", None),
            Fetch::Refused
        ));
        assert!(matches!(
            fetch_art("data:image/png;base64,", None),
            Fetch::Refused
        ));
    }

    /// Exclusions are the load-bearing half: two `/`-leading shapes are emitted by the host
    /// itself, so a POSIX rule that swallowed them would break the proxy round-trip.
    #[test]
    fn local_art_path_detection() {
        assert!(is_local_art_path(r"C:\Users\me\cover.jpg"));
        assert!(is_local_art_path("C:/Users/me/cover.png"));
        assert!(is_local_art_path(r"\\nas\share\art.jpg"));
        assert!(is_local_art_path("file:///home/u/covers/x.jpg"));
        assert!(is_local_art_path("file:///C:/covers/x.jpg"));
        assert!(is_local_art_path("/home/u/.cache/lutris/coverart/x.jpg"));
        assert!(is_local_art_path("/var/lib/steam/librarycache/570/h.jpg"));
        assert!(!is_local_art_path("https://cdn/x.jpg"));
        assert!(!is_local_art_path("http://host/x.jpg"));
        assert!(!is_local_art_path("data:image/png;base64,AAAA"));
        // The host's own art-proxy path must survive a second `proxy_art` pass.
        assert!(!is_local_art_path(
            "/api/v1/library/art/custom:abc/portrait"
        ));
        assert!(!is_local_art_path("/api/v1/library/art/steam:570/hero"));
        assert!(!is_local_art_path("//images.gog.com/abc_vertical.jpg"));
        assert!(!is_local_art_path("covers/x.jpg"));
        assert!(!is_local_art_path(""));
    }

    #[test]
    fn file_url_converts_to_a_path_and_percent_decodes() {
        assert_eq!(file_url_to_path("file:///home/u/c.jpg"), "/home/u/c.jpg");
        assert_eq!(
            file_url_to_path("file:///home/u/My%20Games/c%2Bx.jpg"),
            "/home/u/My Games/c+x.jpg"
        );
        assert_eq!(
            file_url_to_path("file:///C:/covers/c.jpg"),
            "C:/covers/c.jpg"
        );
        assert_eq!(
            file_url_to_path("file://nas/share/c.jpg"),
            r"\\nas\share\c.jpg"
        );
        assert_eq!(file_url_to_path("/home/u/c.jpg"), "/home/u/c.jpg");
        assert_eq!(file_url_to_path(r"C:\c.jpg"), r"C:\c.jpg");
        // A lone `%` is a legal path character, not a decode failure.
        assert_eq!(file_url_to_path("file:///home/100%.jpg"), "/home/100%.jpg");
    }

    /// A protocol-relative URL and an already-proxied path are the two shapes the rewrite must
    /// leave alone; everything the host can serve becomes its own proxy path.
    #[test]
    fn proxy_art_advertises_every_servable_cover() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let mut art = Artwork {
            portrait: Some(r"C:\art\p.jpg".into()),
            hero: Some("https://cdn/h.jpg".into()),
            logo: Some("//images.gog.com/abc.jpg".into()),
            header: Some("/api/v1/library/art/custom:x/header".into()),
        };
        proxy_art("custom:abc", &mut art);
        let path = |v: Option<&str>| v.and_then(|s| s.split('?').next()).map(str::to_owned);
        assert_eq!(
            path(art.portrait.as_deref()).as_deref(),
            Some("/api/v1/library/art/custom:abc/portrait")
        );
        assert_eq!(
            path(art.hero.as_deref()).as_deref(),
            Some("/api/v1/library/art/custom:abc/hero"),
            "the host stores a CDN cover once for every client"
        );
        assert_eq!(art.logo.as_deref(), Some("//images.gog.com/abc.jpg"));
        assert_eq!(
            art.header.as_deref(),
            Some("/api/v1/library/art/custom:x/header")
        );
    }

    /// The whole point of the version tag: point the entry at a different cover and every cache
    /// in front of the host is looking at a URL it has never seen. A cover that did not change
    /// keeps its URL, so a warm cache stays warm.
    #[test]
    fn a_replaced_cover_gets_a_url_no_cache_is_holding() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let proxied = |url: &str| {
            let mut art = Artwork {
                portrait: Some(url.into()),
                hero: None,
                logo: None,
                header: None,
            };
            proxy_art("custom:b0f3c03f8a50", &mut art);
            art.portrait.expect("a servable cover is proxied")
        };
        let first = proxied("https://cdn2.steamgriddb.com/thumb/585a84e4.jpg");
        let second = proxied("https://cdn2.steamgriddb.com/thumb/b109fe84.jpg");
        assert!(
            first.starts_with("/api/v1/library/art/custom:b0f3c03f8a50/portrait?v="),
            "{first}"
        );
        assert_ne!(first, second, "a new cover has to be a new URL");
        assert_eq!(
            first,
            proxied("https://cdn2.steamgriddb.com/thumb/585a84e4.jpg"),
            "and an unchanged one must not move, or every shelf refetches on every listing"
        );
    }

    /// Under `ArtRootsEnv`: the store dir is env-driven, and cargo runs these tests in parallel
    /// threads of one process.
    #[test]
    fn posix_local_art_is_classified_and_proxied() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let path = if cfg!(windows) {
            r"C:\covers\cover.jpg".to_string()
        } else {
            "/home/u/.cache/lutris/coverart/cover.jpg".to_string()
        };
        let url = file_url(std::path::Path::new(&path));
        let mut art = Artwork {
            portrait: Some(path.clone()),
            hero: Some(url),
            logo: Some("https://cdn/l.png".into()),
            header: None,
        };
        assert!(is_local_art_path(&path));
        proxy_art("lutris:42", &mut art);
        let route = |v: Option<&str>| v.and_then(|s| s.split('?').next()).map(str::to_owned);
        assert_eq!(
            route(art.portrait.as_deref()).as_deref(),
            Some("/api/v1/library/art/lutris:42/portrait")
        );
        assert_eq!(
            route(art.hero.as_deref()).as_deref(),
            Some("/api/v1/library/art/lutris:42/hero"),
            "a file:// value is local art too"
        );
        assert_eq!(
            route(art.logo.as_deref()).as_deref(),
            Some("/api/v1/library/art/lutris:42/logo")
        );

        // Re-running the rewrite is a no-op: the emitted proxy path must not be mistaken for a file.
        let before = art.portrait.clone();
        proxy_art("lutris:42", &mut art);
        assert_eq!(art.portrait, before);
    }

    pub(super) const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];

    /// Art-root env vars are process-global and cargo runs tests as threads, so mutating tests
    /// must not overlap. Poison is recovered: a panic here must not cascade as `PoisonError`.
    static ART_ROOTS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Holds `ART_ROOTS_LOCK` and the overrides one test needs; restores previous values on
    /// drop, including unwind. The only writer of these env vars in the binary.
    pub(super) struct ArtRootsEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl ArtRootsEnv {
        /// `None` unsets the variable for the test's duration.
        pub(super) fn set(vars: &[(&'static str, Option<&Path>)]) -> Self {
            let _lock = ART_ROOTS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let mut saved = Vec::new();
            for (key, value) in vars {
                saved.push((*key, std::env::var_os(key)));
                // SAFETY: `_lock` is held for this guard's whole lifetime, and this type is the
                // only writer of these variables in the binary — so no other thread is reading
                // them while they change.
                unsafe { write_env(key, value.map(|p| p.as_os_str())) };
            }
            Self { _lock, saved }
        }
    }

    impl Drop for ArtRootsEnv {
        fn drop(&mut self) {
            for (key, value) in &self.saved {
                // SAFETY: still under `_lock`, which outlives this loop — same argument as `set`.
                unsafe { write_env(key, value.as_deref()) };
            }
        }
    }

    /// # Safety
    /// The caller must hold `ART_ROOTS_LOCK`; the process environment is global and unsound to
    /// mutate while another thread reads it.
    unsafe fn write_env(key: &str, value: Option<&std::ffi::OsStr>) {
        match value {
            // SAFETY: the caller holds `ART_ROOTS_LOCK` (this function's documented contract), and
            // `ArtRootsEnv` is the only writer in the binary — so no other thread is reading the
            // environment while it changes.
            Some(v) => unsafe { std::env::set_var(key, v) },
            // SAFETY: as above — the caller's lock is what makes this sound.
            None => unsafe { std::env::remove_var(key) },
        }
    }

    fn confine_art_to(dir: &Path) -> ArtRootsEnv {
        ArtRootsEnv::set(&[("PUNKTFUNK_LIBRARY_ART_ROOTS", Some(dir))])
    }

    /// Confinement, extension, and content sniff are all load-bearing: the proxy reads in the
    /// host process from a path the plugin lane can write.
    #[test]
    fn local_art_bytes_is_confined_and_image_only() {
        let dir = std::env::temp_dir().join(format!("pf-art-test-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let _env = confine_art_to(&dir);

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();
        let (bytes, ctype) = local_art_bytes(cover.to_str().unwrap()).expect("reads a real cover");
        assert_eq!(bytes, PNG);
        assert_eq!(ctype, "image/png");

        let secret = dir.join("mgmt-token");
        std::fs::write(&secret, b"super-secret-admin-token").unwrap();
        assert!(
            local_art_bytes(secret.to_str().unwrap()).is_none(),
            "an extensionless secret must not be served as application/octet-stream"
        );
        let disguised = dir.join("mgmt-token.png");
        std::fs::write(&disguised, b"super-secret-admin-token").unwrap();
        assert!(
            local_art_bytes(disguised.to_str().unwrap()).is_none(),
            "an image extension must not be enough — the bytes must BE an image"
        );

        let elsewhere = outside.join("cover.png");
        std::fs::write(&elsewhere, PNG).unwrap();
        assert!(
            local_art_bytes(elsewhere.to_str().unwrap()).is_none(),
            "a path outside every art root must be refused"
        );
        // Canonicalize first, or `..` out of the root would look contained.
        let traversal = dir
            .join("..")
            .join(outside.file_name().unwrap())
            .join("cover.png");
        assert!(
            local_art_bytes(traversal.to_str().unwrap()).is_none(),
            "`..` out of the root must be refused after canonicalization"
        );

        assert!(local_art_bytes(dir.join("nope.png").to_str().unwrap()).is_none());
        // A directory is not a servable cover — the proxy must never become a directory reader.
        assert!(local_art_bytes(dir.to_str().unwrap()).is_none());

        // Decode `file://` before confinement, or the check inspects a string that is not the path.
        let as_url = file_url(&cover);
        assert_eq!(
            local_art_bytes(&as_url)
                .expect("file:// reads the same cover")
                .0,
            PNG
        );
        assert!(
            local_art_bytes(&file_url(&elsewhere)).is_none(),
            "file:// must not escape the art roots"
        );
        // Percent-decode before canonicalize, or `%2e%2e` hides from the `..` check.
        assert!(
            local_art_bytes(&format!(
                "{}/%2e%2e/{}/cover.png",
                file_url(&dir),
                outside.file_name().unwrap().to_str().unwrap()
            ))
            .is_none(),
            "percent-encoded traversal must be refused"
        );

        // UNC is refused before any filesystem hit (outbound SMB auth coercion).
        assert!(!art_path_is_servable(r"\\attacker\share\a.png"));

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Kit-shaped `file://` for both platforms. POSIX keeps two slashes (`file:///home/…`);
    /// Windows needs three (`file:///C:/…`). `format!("file://{path}")` on Windows is
    /// `file://C:\…`, whose authority is `C:` — a UNC reference, not a local file.
    pub(super) fn file_url(p: &std::path::Path) -> String {
        let posix = p.to_str().unwrap().replace('\\', "/");
        if posix.starts_with('/') {
            format!("file://{posix}")
        } else {
            format!("file:///{posix}")
        }
    }

    #[test]
    fn validate_art_paths_rejects_unservable_local_paths() {
        let ok = Artwork {
            portrait: Some("https://cdn/x.jpg".into()),
            hero: Some("data:image/png;base64,AAAA".into()),
            logo: Some("/api/v1/library/art/custom:x/logo".into()),
            header: None,
        };
        assert!(validate_art_paths(&ok).is_ok(), "URLs pass through");

        let unc = Artwork {
            portrait: Some(r"\\attacker\share\a.png".into()),
            ..Default::default()
        };
        assert!(
            validate_art_paths(&unc).is_err(),
            "UNC is refused at write time"
        );

        let secret = Artwork {
            hero: Some(r"C:\ProgramData\punktfunk\mgmt-token".into()),
            ..Default::default()
        };
        let err = validate_art_paths(&secret).expect_err("a secret path is refused");
        assert!(
            err.starts_with("art.hero"),
            "the error names the field: {err}"
        );
    }

    /// Write gate and read gate must judge the same string. `Path::new` on raw `file:///…` is
    /// a relative `file:` path; asserting servable and readable together is the point — either
    /// alone still passes if only one side decodes.
    #[test]
    fn file_url_art_is_accepted_at_write_time_exactly_as_at_read_time() {
        let dir = std::env::temp_dir().join(format!("pf-art-wr-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("pf-art-wr-out-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let _env = confine_art_to(&dir);

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();

        let url = file_url(&cover);
        assert!(
            is_local_art_path(&url),
            "a file:// value is local art, so the confinement applies to it"
        );
        assert!(
            art_path_is_servable(&url),
            "write time must accept the file:// form of a servable cover"
        );
        assert!(
            validate_art_paths(&Artwork {
                portrait: Some(url.clone()),
                header: Some(url),
                ..Default::default()
            })
            .is_ok(),
            "a real Lutris-shaped payload must reconcile"
        );

        let spaced = dir.join("My Cover.png");
        std::fs::write(&spaced, PNG).unwrap();
        let spaced_url = file_url(&spaced).replace(' ', "%20");
        assert!(
            art_path_is_servable(&spaced_url),
            "percent-encoded names must decode before the containment test: {spaced_url}"
        );
        assert!(local_art_bytes(&spaced_url).is_some(), "read time agrees");

        // Write-gate decode must not skip confinement: out-of-root `file://` is still refused.
        let elsewhere = outside.join("cover.png");
        std::fs::write(&elsewhere, PNG).unwrap();
        assert!(
            !art_path_is_servable(&file_url(&elsewhere)),
            "file:// must not escape the art roots at write time either"
        );
        assert!(
            validate_art_paths(&Artwork {
                portrait: Some(file_url(&elsewhere)),
                ..Default::default()
            })
            .is_err(),
            "an out-of-root file:// cover is still refused"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// Assert the survivors too: a sanitizer that cleared the whole struct would pass a
    /// drop-only test.
    #[test]
    fn sanitize_drops_only_the_unservable_local_art() {
        let dir = std::env::temp_dir().join(format!("pf-art-san-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let _env = confine_art_to(&dir);

        let cover = dir.join("cover.png");
        std::fs::write(&cover, PNG).unwrap();
        let cover_url = file_url(&cover);
        let outside = if cfg!(windows) {
            r"C:\Program Files (x86)\Steam\appcache\librarycache\570\a\library_hero.jpg".to_string()
        } else {
            "/opt/steam/appcache/librarycache/570/a/library_hero.jpg".to_string()
        };

        let mut art = Artwork {
            portrait: Some(cover_url.clone()),
            hero: Some(outside.clone()),
            logo: Some("https://cdn/l.png".into()),
            header: Some("/api/v1/library/art/steam:570/header".into()),
        };
        let dropped = sanitize_art_paths(&mut art);
        assert_eq!(
            dropped,
            vec![("hero", outside)],
            "only the out-of-root local path is dropped, and it is reported"
        );
        assert!(art.hero.is_none(), "the unservable value is gone, not kept");
        assert_eq!(art.portrait.as_deref(), Some(cover_url.as_str()));
        assert_eq!(art.logo.as_deref(), Some("https://cdn/l.png"));
        assert_eq!(
            art.header.as_deref(),
            Some("/api/v1/library/art/steam:570/header")
        );
        assert!(sanitize_art_paths(&mut art).is_empty());

        assert!(validate_art_paths(&art).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sniff_image_type_recognizes_containers_and_rejects_secrets() {
        assert_eq!(sniff_image_type(PNG), Some("image/png"));
        assert_eq!(
            sniff_image_type(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("image/jpeg")
        );
        assert_eq!(sniff_image_type(b"GIF89a...."), Some("image/gif"));
        assert_eq!(
            sniff_image_type(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_type(b"BM\0\0"), Some("image/bmp"));
        assert_eq!(sniff_image_type(b"-----BEGIN PRIVATE KEY-----"), None);
        assert_eq!(sniff_image_type(b"9f8a7b6c5d4e3f2a1b0c"), None);
        assert_eq!(sniff_image_type(b""), None);
    }

    /// Point the store at a scratch dir and the plugin roots at one that does not exist, so a
    /// test proves the store is readable because it is a root of its own.
    fn store_in(dir: &Path) -> ArtRootsEnv {
        let no_plugin_roots = std::env::temp_dir().join("pf-art-not-a-root");
        ArtRootsEnv::set(&[
            ("PUNKTFUNK_LIBRARY_ART_CACHE", Some(dir)),
            ("PUNKTFUNK_LIBRARY_ART_ROOTS", Some(&no_plugin_roots)),
        ])
    }

    /// A loopback HTTP server that answers one connection per entry in `responses` and then
    /// closes, so a test can exercise the CDN being gone. It records the request heads it read.
    fn stub_cdn(
        responses: Vec<Vec<u8>>,
    ) -> (
        String,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
        std::thread::JoinHandle<()>,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let url = format!("http://{}/cover.png", listener.local_addr().expect("addr"));
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let handle = std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for body in responses {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let n = sock.read(&mut buf).unwrap_or(0);
                recorder
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });
        (url, seen, handle)
    }

    /// `Connection: close` keeps a pooled socket from outliving a server the test shuts down.
    fn http_response(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn png_response() -> Vec<u8> {
        http_response("200 OK", "Content-Type: image/png\r\nETag: \"v1\"\r\n", PNG)
    }

    /// The store's name for a cover is the hash of its URL and nothing else: a changed URL is a
    /// miss on a name nothing has written, which is the whole invalidation story.
    #[test]
    fn art_key_is_the_url_hash() {
        assert_eq!(art_key("https://cdn/x.png"), art_key("https://cdn/x.png"));
        assert_ne!(art_key("https://cdn/x.png"), art_key("https://cdn/y.png"));
        assert_eq!(art_key("https://cdn/x.png").len(), 64);
        assert!(
            art_key("../../../etc/shadow")
                .chars()
                .all(|c| c.is_ascii_hexdigit()),
            "a traversal in the URL cannot become one in the store"
        );
    }

    #[test]
    fn a_fetched_cover_is_stored_once_and_served_from_disk() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let (url, _seen, server) = stub_cdn(vec![png_response()]);

        let (bytes, ctype) = stored_remote_art(&url).expect("the first request fetches");
        assert_eq!(bytes, PNG);
        assert_eq!(ctype, "image/png");
        server
            .join()
            .expect("the stub answered exactly one request");

        let key = art_key(&url);
        assert!(store.path().join(format!("{key}.png")).is_file());
        assert!(store.path().join(format!("{key}.json")).is_file());

        // The CDN is gone now, which is what the second read is for.
        assert_eq!(stored_remote_art(&url).expect("served from disk").0, PNG);
        assert!(
            stored_remote_art(&format!("{url}?v=2")).is_none(),
            "a new URL hashes elsewhere and misses instead of serving the old bytes"
        );

        let mut art = Artwork {
            portrait: Some(url),
            ..Default::default()
        };
        proxy_art("custom:abc", &mut art);
        assert!(
            art.portrait
                .as_deref()
                .is_some_and(|v| v.starts_with("/api/v1/library/art/custom:abc/portrait?v=")),
            "{:?}",
            art.portrait
        );
    }

    /// The ceiling holds while the body streams, so 20 MB never lands in host memory whole, and
    /// the URL goes back to the catalog for the client to fetch itself.
    #[test]
    fn an_oversize_cover_is_refused_and_its_url_passes_through() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let mut big = PNG.to_vec();
        big.resize(20 * 1024 * 1024, 0);
        let (url, _seen, _server) = stub_cdn(vec![http_response(
            "200 OK",
            "Content-Type: image/png\r\n",
            &big,
        )]);

        assert!(stored_remote_art(&url).is_none());
        assert!(store
            .path()
            .join(format!("{}.refused", art_key(&url)))
            .is_file());
        let mut art = Artwork {
            portrait: Some(url.clone()),
            ..Default::default()
        };
        proxy_art("custom:abc", &mut art);
        assert_eq!(
            art.portrait.as_deref(),
            Some(url.as_str()),
            "a refused cover stays the client's own fetch"
        );
    }

    #[test]
    fn a_non_image_cover_is_refused() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let (url, _seen, server) = stub_cdn(vec![http_response(
            "200 OK",
            "Content-Type: text/html\r\n",
            b"<html>not a cover</html>",
        )]);

        assert!(stored_remote_art(&url).is_none());
        server.join().expect("the stub answered once");
        assert!(store
            .path()
            .join(format!("{}.refused", art_key(&url)))
            .is_file());
        assert!(
            !store
                .path()
                .join(format!("{}.html", art_key(&url)))
                .exists(),
            "nothing but an image is ever written to the store"
        );
    }

    /// A CDN having a bad minute is not a bad URL: no marker, so the next request tries again.
    #[test]
    fn a_server_error_leaves_no_refusal_marker() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let (url, _seen, server) = stub_cdn(vec![
            http_response("503 Service Unavailable", "", b""),
            http_response("200 OK", "Content-Type: image/png\r\n", PNG),
        ]);

        assert!(stored_remote_art(&url).is_none(), "nothing stored yet");
        assert!(
            !store
                .path()
                .join(format!("{}.refused", art_key(&url)))
                .exists(),
            "a 503 must not be remembered as a refusal"
        );
        // The retry the missing marker allows: the same URL stores on the CDN's next good answer.
        let (bytes, ctype) = stored_remote_art(&url).expect("the retry stores the cover");
        assert_eq!(bytes, PNG);
        assert_eq!(ctype, "image/png");
        server.join().expect("the stub answered twice");
    }

    /// A `3xx` is refused rather than chased: a plugin supplies this URL, and the privileged
    /// host must not be aimed at an internal endpoint.
    #[test]
    fn an_off_origin_redirect_is_refused() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let (url, _seen, server) = stub_cdn(vec![http_response(
            "302 Found",
            "Location: http://169.254.169.254/latest/meta-data\r\n",
            b"",
        )]);

        assert!(stored_remote_art(&url).is_none());
        server.join().expect("the stub answered once");
        assert!(store
            .path()
            .join(format!("{}.refused", art_key(&url)))
            .is_file());
    }

    /// Blocking the CDN must not blank a cover the host already holds: the stale check fires,
    /// the revalidation fails, and the stored bytes still go out.
    #[test]
    fn a_stale_cover_revalidates_and_survives_a_dead_cdn() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let (url, seen, server) = stub_cdn(vec![
            png_response(),
            http_response("304 Not Modified", "", b""),
        ]);
        assert_eq!(stored_remote_art(&url).expect("the first fetch").0, PNG);

        let key = art_key(&url);
        let stale = |store: &Path| {
            let meta = read_art_meta(store, &key).expect("meta beside the blob");
            write_art_meta(
                store,
                &key,
                &ArtMeta {
                    fetched_at_s: 0,
                    ..meta
                },
            );
        };
        stale(store.path());
        assert_eq!(
            stored_remote_art(&url)
                .expect("304 serves the stored copy")
                .0,
            PNG
        );
        server.join().expect("the stub answered both requests");
        let asked = seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .join("\n")
            .to_ascii_lowercase();
        assert!(
            asked.contains("if-none-match: \"v1\""),
            "the revalidation carries the CDN's own tag: {asked}"
        );
        assert!(
            art_is_fresh(&read_art_meta(store.path(), &key).expect("meta rewritten")),
            "a confirmed cover is not revalidated again until tomorrow"
        );

        // Same again with nothing listening at all.
        stale(store.path());
        assert_eq!(
            stored_remote_art(&url)
                .expect("an unreachable CDN still serves the stored copy")
                .0,
            PNG
        );
        assert!(
            !store.path().join(format!("{key}.refused")).exists(),
            "an unreachable CDN is not a bad URL"
        );
    }

    #[test]
    fn clearing_the_store_leaves_it_empty() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = store_in(store.path());
        let (url, _seen, server) = stub_cdn(vec![png_response()]);
        assert!(stored_remote_art(&url).is_some());
        server.join().expect("the stub answered once");

        let (files, bytes) = clear_art_store().expect("clear the store");
        assert_eq!(files, 2, "the blob and its meta");
        assert!(bytes >= PNG.len() as u64);
        assert_eq!(std::fs::read_dir(store.path()).unwrap().count(), 0);
    }

    /// Eviction is the only thing standing between a big library and an unbounded cache dir.
    #[test]
    fn the_store_evicts_the_oldest_file_over_the_cap() {
        let store = tempfile::tempdir().expect("temp store");
        let _env = ArtRootsEnv::set(&[
            ("PUNKTFUNK_LIBRARY_ART_CACHE", Some(store.path())),
            ("PUNKTFUNK_LIBRARY_ART_CACHE_MB", Some(Path::new("1"))),
        ]);
        let old = store.path().join("old.png");
        std::fs::write(&old, vec![0u8; 2 * 1024 * 1024]).expect("write a fat blob");
        // Two distinct mtimes: the order eviction picks is the whole test.
        std::thread::sleep(std::time::Duration::from_millis(10));
        let young = store.path().join("young.png");
        std::fs::write(&young, PNG).expect("write a small blob");

        evict_art_store(store.path());
        assert!(!old.exists(), "the oldest file goes first");
        assert!(young.exists(), "and eviction stops as soon as it is under");
    }
}
