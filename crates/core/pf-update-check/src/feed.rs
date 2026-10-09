//! Fetch the per-channel manifest and its detached signature.
//!
//! Blocking (`ureq`): both consumers call this off a background thread. Bundled
//! webpki roots avoid a system cert store.
//!
//! The signature is verified over the final response bytes, after redirects.
//! The registry 303s a file GET to object storage; verifying the pre-redirect
//! body would sign a redirect stub. See `publish-sysext-feed.sh`.

use crate::manifest::{self, Manifest, MAX_MANIFEST_BYTES};
use crate::sig::PublicKey;
use std::time::Duration;

/// Feed base — `<base>/<channel>/manifest.json` + `.sig`.
pub const DEFAULT_FEED_BASE: &str =
    "https://git.unom.io/api/packages/unom/generic/punktfunk-update";

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a fetch did not produce a manifest.
///
/// A 404 on `manifest.json` is an empty channel, not a broken box. Every other
/// failure stays a real error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// Empty channel: 404 on `manifest.json` only. A 404 on the signature is a
    /// half-published pair and must stay fail-closed.
    NotPublished,
    Failed(String),
}

impl FeedError {
    pub fn is_not_published(&self) -> bool {
        matches!(self, Self::NotPublished)
    }
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotPublished => f.write_str("no release has been published on this channel yet"),
            Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for FeedError {}

/// `PUNKTFUNK_UPDATE_FEED` override, else [`DEFAULT_FEED_BASE`]. Operator config, not
/// request-time input. Only `https://` or `http://127.0.0.1` — anything else would
/// silently drop TLS.
pub fn feed_base() -> String {
    std::env::var("PUNKTFUNK_UPDATE_FEED")
        .ok()
        .filter(|s| s.starts_with("https://") || s.starts_with("http://127.0.0.1"))
        .unwrap_or_else(|| DEFAULT_FEED_BASE.to_string())
}

/// Fetch and verify the channel manifest. An empty `keys` list is refused, not treated as skip-verify.
pub fn fetch_manifest_blocking(
    base: &str,
    channel: &str,
    keys: &[PublicKey],
    user_agent: &str,
) -> Result<Manifest, FeedError> {
    if keys.is_empty() {
        return Err(FeedError::Failed(
            "no update key is pinned in this build".into(),
        ));
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .max_redirects(3)
        .user_agent(user_agent.to_string())
        .build()
        .into();
    let url = format!("{base}/{channel}/manifest.json");
    let sig_url = format!("{url}.sig");

    // Only the manifest GET may become [`FeedError::NotPublished`].
    let capped = |resp: &mut ureq::http::Response<ureq::Body>| {
        read_capped(resp, MAX_MANIFEST_BYTES).map_err(FeedError::Failed)
    };
    let body = capped(&mut agent.get(&url).call().map_err(manifest_err)?)?;
    let sig = capped(&mut agent.get(&sig_url).call().map_err(fetch_err)?)?;
    let sig_text = String::from_utf8(sig)
        .map_err(|_| FeedError::Failed("signature file is not text".into()))?;

    manifest::verify_and_parse(&body, &sig_text, keys, channel)
        .map_err(|e| FeedError::Failed(format!("{e:#}")))
}

/// 404 on this GET is an empty channel, not a broken feed.
fn manifest_err(e: ureq::Error) -> FeedError {
    match e {
        ureq::Error::StatusCode(404) => FeedError::NotPublished,
        other => fetch_err(other),
    }
}

fn fetch_err(e: ureq::Error) -> FeedError {
    FeedError::Failed(match e {
        ureq::Error::StatusCode(code) => format!("feed returned HTTP {code}"),
        other => format!("feed fetch failed: {other}"),
    })
}

/// The body of a signed document, at most `cap` bytes. Oversize is an error, never a truncated
/// body that then fails its signature for the wrong reason. The plugin store reads through this.
pub fn read_capped(
    resp: &mut ureq::http::Response<ureq::Body>,
    cap: usize,
) -> Result<Vec<u8>, String> {
    // ureq refuses a body that reaches its cap+1 limit; the length check refuses anything past
    // `cap` that still gets through.
    let buf = resp
        .body_mut()
        .with_config()
        .limit(cap as u64 + 1)
        .read_to_vec()
        .map_err(|e| format!("read the response body: {e}"))?;
    if buf.len() > cap {
        return Err(format!("response exceeds the {cap}-byte cap"));
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_keys_never_fetches() {
        let err =
            fetch_manifest_blocking("https://127.0.0.1:1", "stable", &[], "test").unwrap_err();
        assert!(err.to_string().contains("no update key"), "{err}");
        assert!(!err.is_not_published());
    }

    fn status(code: u16) -> ureq::Error {
        ureq::Error::StatusCode(code)
    }

    #[test]
    fn manifest_404_is_not_published_but_other_statuses_are_failures() {
        assert_eq!(manifest_err(status(404)), FeedError::NotPublished);
        for code in [403, 500, 502] {
            let e = manifest_err(status(code));
            assert!(!e.is_not_published(), "HTTP {code} must stay a failure");
            assert!(e.to_string().contains(&code.to_string()), "{e}");
        }
    }

    fn response(len: usize) -> ureq::http::Response<ureq::Body> {
        ureq::http::Response::builder()
            .status(200)
            .body(ureq::Body::builder().data(vec![b'x'; len]))
            .unwrap()
    }

    /// Oversize is refused whole, never truncated to the cap.
    #[test]
    fn read_capped_refuses_oversize() {
        assert_eq!(read_capped(&mut response(8), 8).unwrap().len(), 8);
        assert!(read_capped(&mut response(9), 8).is_err());
        assert!(read_capped(&mut response(100), 8).is_err());
    }

    #[test]
    fn signature_404_stays_a_failure() {
        let e = fetch_err(status(404));
        assert!(!e.is_not_published());
        assert_eq!(e.to_string(), "feed returned HTTP 404");
    }

    #[test]
    fn not_published_reads_as_plain_english() {
        assert_eq!(
            FeedError::NotPublished.to_string(),
            "no release has been published on this channel yet"
        );
    }

    #[test]
    fn feed_override_must_be_https_or_loopback() {
        // Env is process-global and tests run in parallel; this is the override predicate only.
        let ok = |s: &str| s.starts_with("https://") || s.starts_with("http://127.0.0.1");
        assert!(ok("https://example.test/feed"));
        assert!(ok("http://127.0.0.1:8080/feed"));
        assert!(!ok("http://example.test/feed"));
        assert!(!ok("file:///tmp/feed"));
    }
}
