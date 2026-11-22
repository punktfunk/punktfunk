//! The management API over a native connection: one typed management stream per request,
//! framed as the host's tunnel reads it (four bytes of length, a JSON head, the body to FIN;
//! the reply the same). The connection presents this device's certificate, so a paired device
//! reaches its lane on the stream port alone, without TCP 47990. Blocking, for the shells'
//! worker threads; [`request`] keeps one connection per host for the whole process.

use crate::error::{PunktfunkError, Result};
use crate::quic::endpoint;
use crate::quic::v2::{io, registry};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// A reply is a poster at most, and every shell refuses art over 16 MiB.
const MAX_REPLY: usize = 16 << 20;
/// A request is answered within this or not at all, the ceiling pf-client-core's HTTPS agents
/// hold.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// A dial that failed is not retried for this long, so a port that does not answer costs one
/// wait rather than one per request.
const DIAL_BACKOFF: Duration = Duration::from_secs(10);

/// One worker drives every tunnel between requests: keep-alives go out and a host's close
/// lands. Requests block on it from the shells' own threads.
static RT: LazyLock<std::io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("punktfunk-mgmt")
        .enable_all()
        .build()
});

fn runtime() -> Result<&'static tokio::runtime::Runtime> {
    RT.as_ref().map_err(|e| io_err(e))
}

fn io_err(e: &dyn std::fmt::Display) -> PunktfunkError {
    PunktfunkError::Io(std::io::Error::other(e.to_string()))
}

/// One host's management connection.
struct MgmtTunnel {
    _ep: quinn::Endpoint,
    conn: quinn::Connection,
}

/// A reply's status and body.
#[derive(Debug)]
pub struct MgmtReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl MgmtTunnel {
    /// Dial `host:port`, the stream port, presenting `identity` and pinning `pin`. A host that
    /// does not answer within `timeout` is `Timeout`; one that answers the wrong certificate is
    /// `Crypto`.
    fn open(
        host: &str,
        port: u16,
        identity: (&str, &str),
        pin: Option<[u8; 32]>,
        timeout: Duration,
    ) -> Result<MgmtTunnel> {
        runtime()?.block_on(async {
            let addr = super::dial_addr(host, port).await?;
            let (made, observed) =
                endpoint::client_shared(pin, Some(identity), &[registry::ALPN], addr.ip(), false);
            let (ep, _media) = made.map_err(|e| io_err(&e))?;
            let connecting = ep.connect(addr, "punktfunk").map_err(|e| io_err(&e))?;
            match tokio::time::timeout(timeout, connecting).await {
                Err(_) => Err(PunktfunkError::Timeout),
                Ok(Ok(conn)) => Ok(MgmtTunnel { _ep: ep, conn }),
                Ok(Err(e)) => {
                    let wrong_host =
                        pin.is_some() && observed.lock().unwrap().is_some_and(|fp| Some(fp) != pin);
                    Err(match wrong_host {
                        true => PunktfunkError::Crypto,
                        false => io_err(&e),
                    })
                }
            }
        })
    }

    /// `method` `path` with `body`, sent as `content_type` when there is one.
    fn request(&self, req: &Request) -> Result<MgmtReply> {
        let mut head = format!(
            r#"{{"m":{},"p":{}"#,
            json_str(req.method),
            json_str(req.path)
        );
        if let Some(ct) = req.content_type {
            head.push_str(&format!(r#","h":[["content-type",{}]]"#, json_str(ct)));
        }
        head.push('}');
        let mut frame = (head.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(head.as_bytes());
        frame.extend_from_slice(req.body);
        let exchange = async {
            let (mut tx, mut rx) = self.conn.open_bi().await.map_err(|e| io_err(&e))?;
            io::write_stream_type(&mut tx, registry::STREAM_MANAGEMENT).await?;
            tx.write_all(&frame).await.map_err(|e| io_err(&e))?;
            tx.finish().map_err(|e| io_err(&e))?;
            let all = rx.read_to_end(MAX_REPLY).await.map_err(|e| io_err(&e))?;
            parse_reply(&all).ok_or_else(|| io_err(&"a reply that is not one"))
        };
        runtime()?.block_on(async {
            tokio::time::timeout(REQUEST_TIMEOUT, exchange)
                .await
                .map_err(|_| PunktfunkError::Timeout)?
        })
    }
}

/// One management request: `method` (`GET`, `POST` or `DELETE`) `path` (`/api/v1/…` with its
/// query) with `body`, sent as `content_type` when there is one.
pub struct Request<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub content_type: Option<&'a str>,
    pub body: &'a [u8],
}

enum Slot {
    Empty,
    Open(Arc<MgmtTunnel>),
    /// When the dial failed, and whether the host answered with the wrong certificate.
    Failed(Instant, bool),
}

type Key = (String, u16, Option<[u8; 32]>, String);

// A host never asked again keeps its entry: a process talks to a handful of hosts.
static TUNNELS: LazyLock<Mutex<HashMap<Key, Arc<Mutex<Slot>>>>> = LazyLock::new(Default::default);

/// `req` to `host:port`, the stream port, as `identity`, refusing any host but `pin`. One
/// connection per host serves every request; one the host has since closed is dialled again,
/// within `dial`. A failed dial is not retried for [`DIAL_BACKOFF`]. `Crypto`: the host is not
/// the pinned one. Any other failure: the host may still answer on its management port.
pub fn request(
    host: &str,
    port: u16,
    identity: (&str, &str),
    pin: Option<[u8; 32]>,
    dial: Duration,
    req: &Request,
) -> Result<MgmtReply> {
    let key = (host.to_string(), port, pin, identity.0.to_string());
    let slot = TUNNELS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(Slot::Empty)))
        .clone();
    // A kept connection that fails gets one fresh dial: the host may have closed it idle.
    for _ in 0..2 {
        let (tunnel, fresh) = {
            let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
            match &*s {
                Slot::Open(t) if t.conn.close_reason().is_none() => (t.clone(), false),
                Slot::Failed(at, crypto) if at.elapsed() < DIAL_BACKOFF => {
                    return Err(match crypto {
                        true => PunktfunkError::Crypto,
                        false => io_err(&"the stream port did not answer a moment ago"),
                    });
                }
                _ => match MgmtTunnel::open(host, port, identity, pin, dial) {
                    Ok(t) => {
                        let t = Arc::new(t);
                        *s = Slot::Open(t.clone());
                        (t, true)
                    }
                    Err(e) => {
                        *s = Slot::Failed(Instant::now(), matches!(e, PunktfunkError::Crypto));
                        return Err(e);
                    }
                },
            }
        };
        match tunnel.request(req) {
            Ok(reply) => return Ok(reply),
            Err(e) if fresh => return Err(e),
            Err(_) => {
                let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(&*s, Slot::Open(t) if Arc::ptr_eq(t, &tunnel)) {
                    *s = Slot::Empty;
                }
            }
        }
    }
    Err(io_err(&"the stream port dropped the request"))
}

/// `s` as a JSON string.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Four bytes of length, `{"s":status,"h":[…]}`, then the body. Only the status is read.
fn parse_reply(all: &[u8]) -> Option<MgmtReply> {
    let len = u32::from_be_bytes(all.get(..4)?.try_into().ok()?) as usize;
    let head = std::str::from_utf8(all.get(4..4 + len)?).ok()?;
    let at = head.find("\"s\":")? + 4;
    let digits: String = head[at..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    Some(MgmtReply {
        status: digits.parse().ok()?,
        body: all[4 + len..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::parse_reply;

    #[test]
    fn a_reply_parses_and_a_short_one_does_not() {
        let head = br#"{"s":403,"h":[]}"#;
        let mut all = (head.len() as u32).to_be_bytes().to_vec();
        all.extend_from_slice(head);
        all.extend_from_slice(b"no");
        let r = parse_reply(&all).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (403, b"no".as_slice()));
        assert!(parse_reply(&all[..6]).is_none());
        assert!(parse_reply(&[0, 0]).is_none());
    }

    #[test]
    fn a_head_string_is_escaped() {
        assert_eq!(super::json_str("/a?b=\"c\\d\n"), r#""/a?b=\"c\\d\u000a""#);
    }
}
