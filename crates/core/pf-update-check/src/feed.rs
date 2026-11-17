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
        read_capped(resp, MAX_MANIFEST_BYTES as u64)
            .map_err(|e| FeedError::Failed(format!("read the response body: {e}")))
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

/// The decoded body, at most `cap` bytes. Past it the read is
/// [`ureq::Error::BodyExceedsLimit`], never a truncated body. `cap` bounds both the bytes
/// received and what gzip inflates them to.
pub fn read_capped(
    resp: &mut ureq::http::Response<ureq::Body>,
    cap: u64,
) -> Result<Vec<u8>, ureq::Error> {
    use std::io::Read as _;
    // ureq's limit counts bytes before decoding; `take` counts what the decoder yields.
    let mut buf = Vec::new();
    resp.body_mut()
        .with_config()
        .limit(cap + 1)
        .reader()
        .take(cap + 1)
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > cap {
        return Err(ureq::Error::BodyExceedsLimit(cap));
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
        let over = |e| matches!(e, ureq::Error::BodyExceedsLimit(_));
        assert_eq!(read_capped(&mut response(8), 8).unwrap().len(), 8);
        assert!(over(read_capped(&mut response(9), 8).unwrap_err()));
        assert!(over(read_capped(&mut response(100), 8).unwrap_err()));
    }

    /// 38 bytes of gzip that inflate to 4096 zero bytes.
    const GZIP_4096_ZEROS: [u8; 38] = [
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xed, 0xc1, 0x01, 0x0d, 0x00,
        0x00, 0x00, 0xc2, 0xa0, 0xf7, 0x4f, 0x6d, 0x0f, 0x07, 0x14, 0x00, 0x00, 0x00, 0xf0, 0x6e,
        0x11, 0x00, 0x1c, 0xc7, 0x00, 0x10, 0x00, 0x00,
    ];

    /// A loopback server that answers one GET with `body` as `Content-Encoding: gzip` under a
    /// `Content-Length` of `declared`, then holds the socket until the client closes it.
    fn gzip_response(body: &'static [u8], declared: usize) -> ureq::http::Response<ureq::Body> {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/manifest.json", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            // Drain the request first: on Windows, closing with unread data resets the socket.
            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while let Ok(n) = sock.read(&mut chunk) {
                head.extend_from_slice(&chunk[..n]);
                if n == 0 || head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: {declared}\r\n\r\n"
            );
            let _ = sock.write_all(body);
            let _ = sock.read(&mut chunk);
        });
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .into();
        agent.get(&url).call().unwrap()
    }

    /// The cap counts decoded bytes. A body far under it on the wire that inflates past it is
    /// refused once the cap is passed, without waiting for the rest of the body; one under it
    /// both ways reads whole.
    #[test]
    fn read_capped_counts_decoded_bytes() {
        let small = 1024;
        assert!((GZIP_4096_ZEROS.len() as u64) < small);
        let mut stalled = gzip_response(&GZIP_4096_ZEROS, GZIP_4096_ZEROS.len() + 1);
        let refused = read_capped(&mut stalled, small).unwrap_err();
        assert!(
            matches!(refused, ureq::Error::BodyExceedsLimit(_)),
            "{refused}"
        );
        let mut whole = gzip_response(&GZIP_4096_ZEROS, GZIP_4096_ZEROS.len());
        assert_eq!(read_capped(&mut whole, 4096).unwrap(), vec![0u8; 4096]);
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
