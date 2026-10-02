//! Loopback client for `punktfunk-host ctl`: discovery, pinned TLS, and the
//! one place a credential is read.
//!
//! Three files, nothing from argv or the environment:
//!
//! | file | what | absent ⇒ |
//! |------|------|----------|
//! | `mgmt-endpoint` | bound port (`pf_paths::published_mgmt_port`) | 47990, like the tray |
//! | `native-cert.pem` (else `cert.pem`) | the leaf — **the pin** | [`EXIT_UNREACHABLE`] |
//! | `mgmt-token` | operator bearer | [`EXIT_UNREACHABLE`] |
//!
//! Pin before token: [`PinVerify`](punktfunk_core::tls::PinVerify) with the
//! host leaf. rustls validates in the handshake; ureq writes the request only
//! after it completes, so a mismatch never serialises `Authorization`.
//! [`PinVerify::with_observed`] records the seen leaf first: a foreign
//! fingerprint is [`EXIT_PIN`] (squat/rotation); empty is [`EXIT_UNREACHABLE`].
//!
//! No `--token`, no env: `/proc/<pid>/environ` would leak it. A missing
//! `mgmt-token` is a hard error — ctl never mints. Responses are
//! [`serde_json::Value`] so `--json` echoes the server; OpenAPI pins the shape.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Distinct because scripts branch on them. [`EXIT_PIN`] is a security signal,
/// not an ordinary failure.
pub const EXIT_API: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNREACHABLE: i32 = 3;
pub const EXIT_PIN: i32 = 4;

/// JSON envelope version. Additive: fields may be added, never removed or retyped.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug)]
pub struct Failure {
    pub code: i32,
    pub message: String,
}

impl Failure {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Failure {
            code,
            message: message.into(),
        }
    }
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(EXIT_USAGE, message)
    }
    pub fn unreachable(message: impl Into<String>) -> Self {
        Self::new(EXIT_UNREACHABLE, message)
    }
    pub fn api(message: impl Into<String>) -> Self {
        Self::new(EXIT_API, message)
    }
}

pub type Result<T> = std::result::Result<T, Failure>;

/// Loopback: a slow connect means nothing is listening, not a far network.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// One-shot verbs. `watch` passes `None` (long-lived by design).
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Client {
    agent: ureq::Agent,
    base: String,
    /// `Bearer <token>` from the 0600 file. The only place in this crate the
    /// operator credential exists outside `mgmt_token` — grep `bearer` to audit.
    bearer: String,
    /// Leaf the last handshake presented — the [`EXIT_PIN`] discriminator.
    observed: Arc<Mutex<Option<[u8; 32]>>>,
    pin: [u8; 32],
}

impl Client {
    pub fn connect(global_timeout: Option<Duration>) -> Result<Client> {
        Self::connect_in(&pf_paths::config_dir(), global_timeout)
    }

    /// [`Self::connect`] for a one-shot verb: each call ends within [`CALL_TIMEOUT`].
    pub fn one_shot() -> Result<Client> {
        Self::connect(Some(CALL_TIMEOUT))
    }

    /// Explicit config dir so pin-mismatch tests need no `PUNKTFUNK_CONFIG_DIR`
    /// (`unsafe` since edition 2024).
    pub fn connect_in(dir: &Path, global_timeout: Option<Duration>) -> Result<Client> {
        let pin = load_pin(dir)?;
        let token = load_token(dir)?;
        let port = pf_paths::published_mgmt_port_in(dir).unwrap_or(crate::mgmt::DEFAULT_PORT);
        let observed = Arc::new(Mutex::new(None));
        Ok(Client {
            agent: agent(pin, observed.clone(), global_timeout),
            // Always loopback: mgmt honours LOOPBACK peers only, so any other
            // address would be refused anyway.
            base: format!("https://127.0.0.1:{port}"),
            bearer: format!("Bearer {token}"),
            observed,
            pin,
        })
    }

    pub fn get(&self, path: &str) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .get(self.url(path))
            .header("Authorization", &self.bearer)
            .call();
        self.finish(sent, path)
    }

    pub fn delete(&self, path: &str) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .delete(self.url(path))
            .header("Authorization", &self.bearer)
            .call();
        self.finish(sent, path)
    }

    pub fn post(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .post(self.url(path))
            .header("Authorization", &self.bearer)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        self.finish(sent, path)
    }

    /// Whole-object replace. `PUT /display/settings` is the only such route;
    /// `ctl display preset` reads stored policy first or unnamed axes default.
    pub fn put(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .put(self.url(path))
            .header("Authorization", &self.bearer)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        self.finish(sent, path)
    }

    pub fn patch(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
        let sent = self
            .agent
            .patch(self.url(path))
            .header("Authorization", &self.bearer)
            .header("Content-Type", "application/json")
            .send(body.to_string());
        self.finish(sent, path)
    }

    /// Raw GET body, left unread so `watch` can consume SSE frames as they arrive.
    pub fn stream(&self, path: &str) -> Result<Box<dyn std::io::Read + Send>> {
        let resp = self
            .agent
            .get(self.url(path))
            .header("Authorization", &self.bearer)
            .call()
            .map_err(|e| self.transport_failure(e, path))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let mut resp = resp;
            let body = resp.body_mut().read_to_string().unwrap_or_default();
            return Err(Failure::api(http_error(status, &body, path)));
        }
        Ok(Box::new(resp.into_body().into_reader()))
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    fn finish(
        &self,
        sent: std::result::Result<ureq::http::Response<ureq::Body>, ureq::Error>,
        path: &str,
    ) -> Result<serde_json::Value> {
        let mut resp = sent.map_err(|e| self.transport_failure(e, path))?;
        let status = resp.status().as_u16();
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        if !(200..300).contains(&status) {
            return Err(Failure::api(http_error(status, &body, path)));
        }
        if body.trim().is_empty() {
            return Ok(serde_json::Value::Null);
        }
        serde_json::from_str(&body)
            .map_err(|e| Failure::api(format!("{path}: the host sent JSON we can't parse ({e})")))
    }

    /// Observed-fingerprint slot separates squat ([`EXIT_PIN`]) from nobody
    /// listening ([`EXIT_UNREACHABLE`]).
    fn transport_failure(&self, e: ureq::Error, path: &str) -> Failure {
        if let Some(seen) = *self.observed.lock().unwrap() {
            if seen != self.pin {
                return Failure::new(
                    EXIT_PIN,
                    format!(
                        "certificate pin mismatch on {base} — the process answering there presented \
                         {seen}, but this host's identity is {ours}. No token was sent. Either the \
                         host regenerated its certificate (delete the stale pairing state and \
                         re-pair) or another local process is squatting the management port.",
                        base = self.base,
                        seen = hex::encode(seen),
                        ours = hex::encode(self.pin),
                    ),
                );
            }
        }
        Failure::unreachable(format!(
            "can't reach the management API at {}{path}: {e}. Is the host running \
             (`systemctl --user status punktfunk-host`)?",
            self.base
        ))
    }
}

/// Non-2xx as a human line, unwrapping the `ApiError` envelope when present.
fn http_error(status: u16, body: &str, path: &str) -> String {
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_string))
        .unwrap_or_else(|| body.trim().chars().take(200).collect());
    match status {
        401 | 403 => format!(
            "{path}: the host rejected our token ({status}). The persisted `mgmt-token` and the \
             running host disagree — restart the host, or delete the file and let it re-mint."
        ),
        404 => format!("{path}: no such thing here ({status} {detail})"),
        503 => format!("{path}: {detail} ({status})"),
        _ if detail.is_empty() => format!("{path}: the host answered {status}"),
        _ => format!("{path}: {detail} ({status})"),
    }
}

/// Native identity when present, else the legacy GameStream leaf (`mgmt::run`
/// / `identity::load_or_adopt`). Same order as the tray; pinning the other
/// of the pair is [`EXIT_PIN`] on a healthy host.
fn load_pin(dir: &Path) -> Result<[u8; 32]> {
    use rustls::pki_types::pem::PemObject;
    let pem = std::fs::read(dir.join("native-cert.pem"))
        .or_else(|_| std::fs::read(dir.join("cert.pem")))
        .map_err(|_| {
            Failure::unreachable(format!(
                "no host certificate in {} (looked for native-cert.pem, then cert.pem). \
                 Without it there is nothing to pin, and ctl will not send a token unpinned. \
                 Has the host ever run on this machine?",
                dir.display()
            ))
        })?;
    let der = rustls::pki_types::CertificateDer::from_pem_slice(&pem).map_err(|e| {
        Failure::unreachable(format!(
            "the host certificate in {} is not readable as PEM ({e})",
            dir.display()
        ))
    })?;
    Ok(punktfunk_core::tls::cert_fingerprint(der.as_ref()))
}

/// Persisted operator token. Never generates one — see the module docs.
fn load_token(dir: &Path) -> Result<String> {
    crate::mgmt_token::read_persisted(dir).ok_or_else(|| {
        Failure::unreachable(format!(
            "no management token in {} — ctl reads the one the host persists and never mints its \
             own. Start the host once (`systemctl --user start punktfunk-host`) and retry.",
            dir.join("mgmt-token").display()
        ))
    })
}

/// Pinned agent over [`punktfunk_core::tls::pinned_builder`], pin mandatory,
/// observed slot wired so a mismatch is reportable.
fn agent(
    pin: [u8; 32],
    observed: Arc<Mutex<Option<[u8; 32]>>>,
    global_timeout: Option<Duration>,
) -> ureq::Agent {
    let tls = punktfunk_core::tls::pinned_builder(punktfunk_core::tls::PinVerify::with_observed(
        Some(pin),
        observed,
    ))
    .expect("rustls default protocol versions")
    .with_no_client_auth();
    // ureq's `TlsConfig` has no hook for a custom verifier, so the agent takes
    // `ClientConfig` directly through the shared glue.
    punktfunk_core::tls::ureq_agent::agent(
        Arc::new(tls),
        ureq::Agent::config_builder()
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(global_timeout)
            // 4xx/5xx as responses so the `ApiError` body reaches the operator
            // instead of flattening into "status 400".
            .http_status_as_error(false)
            .max_redirects(0)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    /// Port-squat: another local uid answers on the mgmt port with a valid
    /// self-signed cert that is not ours.
    ///
    /// The verb fails with [`EXIT_PIN`] (scripts must not retry into the
    /// squatter). The squatter receives zero application bytes: rustls rejects
    /// during the handshake, so ureq never serialises `Authorization`.
    #[test]
    fn a_squatter_gets_exit_4_and_not_one_byte_of_the_token() {
        punktfunk_core::tls::install_default_provider();
        let dir = std::env::temp_dir().join(format!("pf-ctl-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let ours = crate::identity::ephemeral().unwrap();
        let squatter = crate::identity::ephemeral().unwrap();
        let server = crate::gamestream::tls::server_config_optional_client(
            &squatter.cert_pem,
            &squatter.key_pem,
        )
        .unwrap();

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::fs::write(dir.join("native-cert.pem"), &ours.cert_pem).unwrap();
        std::fs::write(
            dir.join("mgmt-token"),
            "PUNKTFUNK_MGMT_TOKEN=s3kr1t-token\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("mgmt-endpoint"),
            format!("PUNKTFUNK_MGMT_URL=https://127.0.0.1:{port}\n"),
        )
        .unwrap();

        let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
        let recorder = seen.clone();
        let squat = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().expect("squatter accept");
            let mut conn = rustls::ServerConnection::new(server).expect("squatter tls");
            // Client sends a fatal alert instead of finishing; an error here is the pass.
            let _ = conn.complete_io(&mut sock);
            let mut plaintext = Vec::new();
            let _ = conn.reader().read_to_end(&mut plaintext);
            *recorder.lock().unwrap() = plaintext;
        });

        let err = Client::connect_in(&dir, Some(Duration::from_secs(10)))
            .and_then(|c| c.get("/api/v1/status"))
            .expect_err("a mismatched certificate must not produce a successful call");
        assert_eq!(err.code, EXIT_PIN, "wrong exit code: {}", err.message);
        assert!(
            err.message.contains("pin mismatch"),
            "the operator must be told WHICH failure this is: {}",
            err.message
        );

        squat.join().unwrap();
        let bytes = seen.lock().unwrap().clone();
        assert!(
            bytes.is_empty(),
            "the squatter read {} application bytes; the token must never leave the process \
             before the pin matches",
            bytes.len()
        );
        assert!(!String::from_utf8_lossy(&bytes).contains("s3kr1t-token"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No certificate is [`EXIT_UNREACHABLE`], never an unverified connect.
    /// The tray can afford that shape (it holds no token); ctl cannot.
    #[test]
    fn no_certificate_means_no_call_at_all() {
        let dir = std::env::temp_dir().join(format!("pf-ctl-nocert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("mgmt-token"), "PUNKTFUNK_MGMT_TOKEN=deadbeef\n").unwrap();
        // `.err()` not `expect_err`: `Client` has no `Debug` so a derived one
        // cannot print the bearer into a panic or `{:?}`.
        let err = Client::connect_in(&dir, None)
            .err()
            .expect("no cert, no connection");
        assert_eq!(err.code, EXIT_UNREACHABLE);
        assert!(err.message.contains("native-cert.pem"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Certificate without token fails and leaves the directory unchanged.
    #[test]
    fn a_missing_token_is_an_error_never_a_freshly_minted_one() {
        let dir = std::env::temp_dir().join(format!("pf-ctl-notoken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ours = crate::identity::ephemeral().unwrap();
        std::fs::write(dir.join("native-cert.pem"), &ours.cert_pem).unwrap();
        let err = Client::connect_in(&dir, None)
            .err()
            .expect("no token, no connection");
        assert_eq!(err.code, EXIT_UNREACHABLE);
        assert!(!dir.join("mgmt-token").exists(), "ctl minted a token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn http_error_unwraps_the_api_envelope() {
        let msg = http_error(
            400,
            r#"{"error":"grants has reserved bits set"}"#,
            "/api/v1/x",
        );
        assert!(msg.contains("grants has reserved bits set"), "{msg}");
        assert!(msg.contains("400"), "{msg}");
    }

    #[test]
    fn http_error_names_the_token_when_auth_fails() {
        let msg = http_error(401, "", "/api/v1/status");
        assert!(msg.contains("mgmt-token"), "{msg}");
    }

    #[test]
    fn exit_codes_are_distinct() {
        // Scripts branch on these; a collision merges "no host" with "squatter".
        let all = [EXIT_API, EXIT_USAGE, EXIT_UNREACHABLE, EXIT_PIN];
        let mut seen: Vec<i32> = all.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), all.len());
        // None is 0 — a failure that exits 0 is worse than a wrong code.
        assert!(all.iter().all(|c| *c != 0));
    }
}
