//! What a browser must know before it can dial the WebTransport plane.
//!
//! Unauthenticated on purpose, and `require_auth` exempts the path: a browser that has never
//! paired holds no client certificate, so there is no credential it could present here. The
//! response carries nothing secret — a certificate hash is what any peer learns by connecting —
//! and it authorises nothing. An on-path attacker who substitutes both the hash and the
//! certificate still cannot complete PAKE pairing, which is where the peer is actually proven
//! (`design/web-client.md` §4).
//!
//! A browser that HAS paired gets one thing more: the host's long-lived certificate and its
//! signature over the hash above. That chains this plane's throwaway certificate to the
//! fingerprint the browser pinned at pairing, which is what a browser cannot do with
//! `serverCertificateHashes` alone.
//!
//! `404` when the plane is off, so a page can tell "this host does not offer it" from "this host
//! is unreachable".

use crate::mgmt::shared::ApiError;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;

/// Everything `new WebTransport(url, { serverCertificateHashes })` needs.
#[derive(Serialize, ToSchema)]
pub(crate) struct WebTransportInfo {
    /// UDP port the plane listens on. Not the management port, and not the native plane's.
    port: u16,
    /// Lowercase hex SHA-256 of the leaf certificate DER — the bytes that go in
    /// `serverCertificateHashes[0].value`.
    cert_hash_sha256: String,
    /// Unix seconds. Past this the certificate has rotated and the hash above is stale; fetch
    /// again rather than cache. Always under two weeks out — the spec refuses anything longer.
    expires_at: u64,
    /// `allowPooling: true` is a `TypeError` when combined with `serverCertificateHashes`, so a
    /// client must pass this. Stated here because a browser that ignores it fails at Web PKI
    /// validation with no useful error.
    allow_pooling: bool,
    /// Hex ECDSA-P256-SHA256 signature (ASN.1 DER) by the host's long-lived native identity over
    /// `"punktfunk-wt-cert-v1:" + cert_hash_sha256`. Absent when that identity is the legacy RSA
    /// pair. A browser that has paired MUST check this; one that has not cannot, and does not.
    #[serde(skip_serializing_if = "Option::is_none")]
    cert_hash_sig: Option<String>,
    /// Base64 DER of the native identity's leaf certificate — the key that verifies
    /// `cert_hash_sig`. A browser hashes it and compares with the fingerprint it stored at
    /// pairing; trusting it without that check would defeat the whole exercise.
    #[serde(skip_serializing_if = "Option::is_none")]
    host_cert_der: Option<String>,
}

/// Where to reach the browser plane
#[utoipa::path(
    get,
    path = "/webtransport",
    tag = "host",
    operation_id = "getWebTransport",
    // Override the document-global bearerAuth: a browser has no credential before pairing.
    security(()),
    responses(
        (status = OK, description = "The live certificate hash and port", body = WebTransportInfo),
        (status = NOT_FOUND, description = "The browser plane is not enabled on this host", body = ApiError),
    )
)]
pub(crate) async fn get_webtransport() -> Response {
    match crate::webtransport::published() {
        Some(p) => {
            let (cert_hash_sig, host_cert_der) = match p.attestation {
                Some(a) => (Some(a.cert_hash_sig), Some(a.host_cert_der)),
                None => (None, None),
            };
            Json(WebTransportInfo {
                port: p.port,
                cert_hash_sha256: p.cert_hash,
                expires_at: p.expires_at,
                allow_pooling: false,
                cert_hash_sig,
                host_cert_der,
            })
            .into_response()
        }
        None => crate::mgmt::shared::api_error(
            StatusCode::NOT_FOUND,
            "the WebTransport plane is not enabled (--webtransport / PUNKTFUNK_WEBTRANSPORT)",
        ),
    }
}
