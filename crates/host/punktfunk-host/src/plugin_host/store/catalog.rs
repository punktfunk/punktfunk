//! Catalog fetch and disk cache for a source's signed index (`plugin-store.md`).
//!
//! Fetch is HTTPS-only, size/timeout/redirect bounded, and never attaches credentials.
//! A pinned-key document that does not verify is an error: keep the last good copy and
//! report it; never fall back to unsigned. A host that cannot reach the network still
//! browses and installs from cache, because the pin travelled with the entry.
//!
//! Pin: `ureq_returns_304_as_ok` (304 is `Ok`, not an error).

use super::index::{Index, MAX_INDEX_BYTES};
use super::sources::Source;
use pf_update_check::feed::read_capped;
use std::path::{Path, PathBuf};
use std::time::Duration;

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) enum Fetched {
    Fresh {
        index: Box<Index>,
        etag: Option<String>,
    },
    /// HTTP 304: the cached copy is still current.
    NotModified,
    /// Caller keeps the last good copy and marks the source stale.
    Failed(String),
}

/// Blocking (`ureq`). Callers run this on a blocking thread, never on the async runtime.
pub(crate) fn fetch(source: &Source, etag: Option<&str>) -> Fetched {
    if !source.url.starts_with("https://") {
        return Fetched::Failed("source url must be https".into());
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        // Three hops is enough for a signed document; more is a stall.
        .max_redirects(3)
        .user_agent(format!("punktfunk-host/{}", super::index::host_version()))
        .build()
        .into();

    let mut req = agent.get(&source.url);
    if let Some(tag) = etag {
        req = req.header("If-None-Match", tag);
    }
    let mut resp = match req.call() {
        // ureq reports only status >= 400 as Err. A 304 is Ok with an empty body; treating it as
        // an error verifies a signature over zero bytes. Pin: `ureq_returns_304_as_ok`.
        Ok(r) if r.status() == 304 => return Fetched::NotModified,
        Ok(r) => r,
        Err(ureq::Error::StatusCode(code)) => {
            return Fetched::Failed(format!("index fetch returned HTTP {code}"))
        }
        Err(e) => return Fetched::Failed(format!("index fetch failed: {e}")),
    };
    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match read_capped(&mut resp, MAX_INDEX_BYTES) {
        Ok(b) => b,
        Err(e) => return Fetched::Failed(e),
    };

    // Verify before parse. Nothing below may look at a field until this passes.
    let keys = source.keys();
    if !keys.is_empty() {
        let sig = match agent.get(&source.sig_url()).call() {
            Ok(mut r) => match read_capped(&mut r, MAX_INDEX_BYTES) {
                Ok(b) => b,
                Err(e) => return Fetched::Failed(format!("signature: {e}")),
            },
            Err(e) => {
                return Fetched::Failed(format!(
                    "this source is signed but its signature could not be fetched: {e}"
                ))
            }
        };
        let sig_text = match String::from_utf8(sig) {
            Ok(s) => s,
            Err(_) => return Fetched::Failed("signature file is not text".into()),
        };
        if let Err(e) = super::index::verify_signature(&body, &sig_text, &keys) {
            return Fetched::Failed(format!("signature check failed: {e}"));
        }
    }

    match Index::parse(&body) {
        Ok(index) => Fetched::Fresh {
            index: Box::new(index),
            etag: new_etag,
        },
        Err(e) => Fetched::Failed(format!("{e:#}")),
    }
}

/// `<config_dir>/store-cache`. Last good copy of each source, so a host can boot without a network.
pub(crate) fn cache_dir() -> PathBuf {
    pf_paths::config_dir().join("store-cache")
}

fn body_path(dir: &Path, source: &str) -> PathBuf {
    dir.join(format!("{source}.json"))
}

fn meta_path(dir: &Path, source: &str) -> PathBuf {
    dir.join(format!("{source}.meta.json"))
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
pub(crate) struct CacheMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// Unix seconds of the fetch that produced the cached body.
    #[serde(default)]
    pub fetched_at: u64,
}

/// Signature is not re-checked: it was checked when the bytes were accepted, and the cache
/// lives in the host's private config tree.
pub(crate) fn read_cache(dir: &Path, source: &str) -> Option<(Index, CacheMeta)> {
    let body = std::fs::read(body_path(dir, source)).ok()?;
    let index = Index::parse(&body).ok()?;
    let meta = std::fs::read(meta_path(dir, source))
        .ok()
        .and_then(|b| serde_json::from_slice::<CacheMeta>(&b).ok())
        .unwrap_or_default();
    Some((index, meta))
}

pub(crate) fn write_cache(dir: &Path, source: &str, index: &Index, meta: &CacheMeta) {
    if let Err(e) = pf_paths::create_private_dir(dir) {
        tracing::warn!("store cache dir not created: {e}");
        return;
    }
    match serde_json::to_vec_pretty(index) {
        Ok(b) => {
            let _ = pf_paths::replace_file(&body_path(dir, source), &b);
        }
        Err(e) => tracing::warn!("catalog cache not serialized: {e}"),
    }
    if let Ok(b) = serde_json::to_vec_pretty(meta) {
        let _ = pf_paths::replace_file(&meta_path(dir, source), &b);
    }
}

/// Drop a removed source's files so a later re-add is not served a stale shelf.
pub(crate) fn drop_cache(dir: &Path, source: &str) {
    let _ = std::fs::remove_file(body_path(dir, source));
    let _ = std::fs::remove_file(meta_path(dir, source));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_index() -> Index {
        Index::parse(
            br#"{"schema":1,"name":"t","plugins":[{"id":"a","pkg":"@p/plugin-a",
                "registry":"https://r.example/","title":"A","version":"1.0.0",
                "integrity":"sha512-AAAA"}]}"#,
        )
        .unwrap()
    }

    #[test]
    fn cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let idx = sample_index();
        let meta = CacheMeta {
            etag: Some("\"abc\"".into()),
            fetched_at: 1_700_000_000,
        };
        write_cache(dir.path(), "unom", &idx, &meta);

        let (back, back_meta) = read_cache(dir.path(), "unom").expect("cache should be readable");
        assert_eq!(back.plugins.len(), 1);
        assert_eq!(back.plugins[0].id, "a");
        assert_eq!(back_meta.etag.as_deref(), Some("\"abc\""));
        assert_eq!(back_meta.fetched_at, 1_700_000_000);
    }

    #[test]
    fn missing_or_corrupt_cache_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_cache(dir.path(), "nope").is_none());
        std::fs::write(dir.path().join("bad.json"), b"{ not json").unwrap();
        assert!(read_cache(dir.path(), "bad").is_none());
    }

    #[test]
    fn cache_is_revalidated_on_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.json"), br#"{"schema":99,"plugins":[]}"#).unwrap();
        assert!(read_cache(dir.path(), "x").is_none());
    }

    #[test]
    fn drop_cache_removes_both_files() {
        let dir = tempfile::tempdir().unwrap();
        write_cache(dir.path(), "s", &sample_index(), &CacheMeta::default());
        assert!(read_cache(dir.path(), "s").is_some());
        drop_cache(dir.path(), "s");
        assert!(read_cache(dir.path(), "s").is_none());
        assert!(!meta_path(dir.path(), "s").exists());
    }

    /// ureq reports only status >= 400 as Err. A 304 (If-None-Match hit) is Ok with an empty body.
    /// If an upgrade changes that, fail here so [`fetch`] can be updated.
    #[test]
    fn ureq_returns_304_as_ok() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                // Drain the request before answering. On Windows, closing with unread data sends
                // RST instead of FIN and the client never sees the 304. A GET has no body, so the
                // header terminator is the whole request.
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while let Ok(n) = sock.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let _ = sock.write_all(b"HTTP/1.1 304 Not Modified\r\nETag: \"x\"\r\n\r\n");
                let _ = sock.flush();
            }
        });

        let resp = ureq::get(&format!("http://{addr}/index.json"))
            .header("If-None-Match", "\"x\"")
            .call();
        let _ = server.join();

        match resp {
            Ok(r) => assert_eq!(r.status(), 304, "304 must arrive as Ok, and be checked for"),
            Err(e) => panic!("ureq now reports 304 as an error ({e}) — fetch() must be updated"),
        }
    }

    #[test]
    fn non_https_source_never_reaches_the_network() {
        let s = Source {
            name: "x".into(),
            url: "http://example.org/i.json".into(),
            public_key: None,
        };
        assert!(matches!(fetch(&s, None), Fetched::Failed(_)));
    }
}
