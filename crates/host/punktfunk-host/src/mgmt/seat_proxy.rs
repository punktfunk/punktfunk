//! `ANY /profiles/{id}/proxy/{*rest}`: the console reaching a seat profile's own host, and the
//! path and answer rules that keep the box's own routes and origin out of a seat's reach.

use super::profiles::{needs_start, refused, seat_of, seats_now, start_seat};
use super::scope::{scope_of, Scope};
use super::shared::*;
use axum::http::{header, Method};
use punktfunk_core::mgmt_routes::path_matches;

/// Whether the seat proxy refuses `method` on `rest`, the decoded path (the query travels apart):
/// a route with no scope or the box's, and a plugin UI's credential, which stays on its host.
fn proxy_refuses(method: &Method, rest: &str) -> bool {
    let path = format!("/api/v1/{rest}");
    path_matches("/api/v1/plugins/{}/ui-credential", &path)
        || !matches!(scope_of(method, &path), Some(scope) if scope != Scope::Box)
}

/// `rest` and `query` as the seat's host reads them. The router hands `rest` over decoded, so each
/// segment is escaped again: an encoded `?` stays part of its segment and starts no query.
fn seat_target(rest: &str, query: Option<&str>) -> String {
    let path = rest
        .split('/')
        .map(super::library::path_segment)
        .collect::<Vec<_>>()
        .join("/");
    match query {
        Some(q) => format!("{path}?{q}"),
        None => path,
    }
}

/// `ANY /api/v1/profiles/{id}/proxy/{*rest}`: the console reaching a seat profile's own host for
/// what is per seat (library, game sources, plugins). Admin lane only: neither the plugin nor
/// the device allowlist names it. Undocumented in the spec, because it forwards any method.
pub(crate) async fn proxy_profile_seat(
    State(st): State<Arc<MgmtState>>,
    Path((id, rest)): Path<(String, String)>,
    method: Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if proxy_refuses(&method, &rest) {
        return api_error_coded(
            StatusCode::FORBIDDEN,
            "seat_part_is_box",
            "That part of a seat is the box's. Change it on this page, not through a seat.",
        );
    }
    let seats = seats_now(&st).await;
    let (_, seat) = match seat_of(&st, &id, &seats) {
        Ok(found) => found,
        Err(refusal) => return refused(refusal),
    };
    let Some(row) = seats.seat(&seat).map(|(s, _)| s.clone()) else {
        return api_error_coded(
            StatusCode::NOT_FOUND,
            "seat_gone",
            "This profile's seat is gone.",
        );
    };
    // The owner's session starts on demand: the console asked for its library, and nobody has
    // logged in to make it.
    if row.owner && needs_start(&row.runtime.state) {
        start_seat(&seat);
        return api_error_coded(
            StatusCode::SERVICE_UNAVAILABLE,
            "owner_session_starting",
            "The owner's session is starting. Try again in a moment.",
        );
    }
    let call = super::owner::Forwarded {
        method: method.as_str().to_string(),
        target: seat_target(&rest, uri.query()),
        content_type: headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string),
        body: body.to_vec(),
        device: None,
        person: None,
        stream: super::owner::tail(&rest),
        start: false,
    };
    let forwarded = blocking("seat", move || crate::seats::forward(&row, call));
    let answer = match forwarded.await {
        Ok(Some(answer)) => answer,
        Ok(None) => {
            return api_error_coded(
                StatusCode::BAD_GATEWAY,
                "seat_no_answer_start",
                "The seat didn't answer. Start it, then try again.",
            )
        }
        Err(resp) => return resp,
    };
    match answer {
        super::owner::Answer::Reply(status, content_type, bytes) => seat_answer(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            content_type.as_deref(),
            bytes,
        ),
        // A tail is the seat host's own event frames: text, never a page.
        super::owner::Answer::Stream(status, _, body) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut resp = (status, body).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/event-stream"),
            );
            resp
        }
        other => other.into_response(),
    }
}

/// A seat host's answer, relayed on the console's origin. Only JSON and images keep their
/// type, SVG excepted because it runs script; the rest is served as inert bytes, so a seat
/// cannot run a page here.
fn seat_answer(status: StatusCode, content_type: Option<&str>, bytes: Vec<u8>) -> Response {
    let passes = |c: &&str| {
        let media = c
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        media == "application/json" || (media.starts_with("image/") && !media.contains("svg"))
    };
    let content_type = content_type
        .filter(passes)
        .and_then(|c| header::HeaderValue::from_str(c).ok())
        .unwrap_or(header::HeaderValue::from_static("application/octet-stream"));
    let mut resp = (status, bytes).into_response();
    let headers = resp.headers_mut();
    headers.insert(header::CONTENT_TYPE, content_type);
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        header::HeaderValue::from_static("sandbox"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seat_proxy_keeps_the_box_s_own_routes_to_itself() {
        for (method, passed) in [
            (Method::GET, "library"),
            (Method::HEAD, "library/page"),
            (Method::PUT, "plugins/rom-manager"),
            (Method::GET, "display/state"),
            (Method::PATCH, "host/settings"),
        ] {
            assert!(!proxy_refuses(&method, passed), "{method} {passed}");
        }
        for (method, refused) in [
            (Method::POST, "native/pair/arm"),
            (Method::GET, "profiles"),
            (Method::POST, "pair/pin"),
            (Method::GET, "update/status"),
            (Method::POST, "actions/power.sleep"),
            (Method::GET, "clients"),
            (Method::PUT, "display/settings"),
            (Method::GET, "library/../native/clients"),
            (Method::GET, "./native/clients"),
            (Method::GET, "plugins/steam/ui-credential"),
            (Method::GET, "Plugins/steam/UI-credential/"),
            (Method::GET, "plugins/steam/ui"),
            (Method::GET, "library/page?as=kid"),
            (Method::POST, "library"),
        ] {
            assert!(proxy_refuses(&method, refused), "{method} {refused}");
        }
    }

    /// The proxy's rule before the scope table: a first segment the box keeps, a dot segment,
    /// and a plugin UI's credential.
    fn refused_by_name(rest: &str) -> bool {
        let first = rest
            .trim_start_matches('/')
            .split(['/', '?'])
            .next()
            .unwrap_or("");
        let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
        segments.iter().any(|s| *s == "." || *s == "..")
            || matches!(
                segments[..],
                [plugins, _, credential]
                    if plugins.eq_ignore_ascii_case("plugins")
                        && credential.eq_ignore_ascii_case("ui-credential")
            )
            || matches!(
                first,
                "native" | "pair" | "profiles" | "update" | "actions" | "clients"
            )
    }

    /// The table refuses every operation the named list did, and the box's own that the list
    /// let through, each listed here. No other answer changed.
    #[test]
    fn the_table_refuses_what_the_named_list_did_and_the_box_s_rest() {
        const NEWLY_REFUSED: &[(&str, &str)] = &[
            ("DELETE", "/api/v1/client-logs/{id}"),
            ("DELETE", "/api/v1/display/presets/{id}"),
            ("GET", "/api/v1/client-logs"),
            ("GET", "/api/v1/client-logs/{id}"),
            ("GET", "/api/v1/display/monitors"),
            ("GET", "/api/v1/display/presets"),
            ("GET", "/api/v1/display/settings"),
            ("GET", "/api/v1/webtransport"),
            ("POST", "/api/v1/auth/device/challenge"),
            ("POST", "/api/v1/auth/device/token"),
            ("POST", "/api/v1/client-logs"),
            ("POST", "/api/v1/display/presets"),
            ("POST", "/api/v1/display/release"),
            ("PUT", "/api/v1/display/layout"),
            ("PUT", "/api/v1/display/presets/{id}"),
            ("PUT", "/api/v1/display/settings"),
            ("PUT", "/api/v1/gpus/preference"),
            ("PUT", "/api/v1/library/own"),
        ];
        let doc: serde_json::Value = serde_json::from_str(&crate::mgmt::openapi_json()).unwrap();
        let mut changed = Vec::new();
        for (path, ops) in doc["paths"].as_object().unwrap() {
            for method in ops.as_object().unwrap().keys() {
                let method = Method::from_bytes(method.to_uppercase().as_bytes()).unwrap();
                let rest = path.strip_prefix("/api/v1/").unwrap();
                let now = proxy_refuses(&method, rest);
                if now != refused_by_name(rest) {
                    assert!(
                        now,
                        "{method} {path}: the table passes what the list refused"
                    );
                    changed.push((method.to_string(), path.clone()));
                }
            }
        }
        changed.sort();
        let want: Vec<(String, String)> = NEWLY_REFUSED
            .iter()
            .map(|(m, p)| (m.to_string(), p.to_string()))
            .collect();
        assert_eq!(changed, want);
    }

    /// The router hands `rest` over decoded. An encoded `?` or `%` must reach the seat as the
    /// character it is, not as a query or an escape the seat decodes a second time.
    #[tokio::test]
    async fn the_router_decodes_the_path_the_seat_proxy_re_escapes() {
        use tower::ServiceExt;
        let app = axum::Router::new().route(
            "/profiles/{id}/proxy/{*rest}",
            axum::routing::any(|Path((_, rest)): Path<(String, String)>| async move { rest }),
        );
        let req = axum::http::Request::builder()
            .uri("/profiles/a/proxy/store/install%3F/%2e%2e/a%252Fb")
            .body(axum::body::Body::empty())
            .unwrap();
        let body = axum::body::to_bytes(app.oneshot(req).await.unwrap().into_body(), 1024)
            .await
            .unwrap();
        let rest = String::from_utf8(body.to_vec()).unwrap();
        assert_eq!(rest, "store/install?/../a%2Fb");
        assert!(proxy_refuses(&Method::POST, &rest));
    }

    #[test]
    fn a_decoded_path_reaches_the_seat_as_the_segments_it_had() {
        assert_eq!(seat_target("library/steam:570", None), "library/steam:570");
        assert_eq!(
            seat_target("store/sources/my src", Some("as=kid&x=1")),
            "store/sources/my%20src?as=kid&x=1"
        );
        assert_eq!(seat_target("store/install?", None), "store/install%3F");
        assert_eq!(seat_target("a%2Fb", None), "a%252Fb");
        assert_eq!(seat_target("rom/é#1", None), "rom/%C3%A9%231");
    }

    #[test]
    fn a_seat_cannot_serve_a_page_from_the_console_s_origin() {
        let served = |ct: Option<&str>| {
            let resp = seat_answer(StatusCode::OK, ct, b"<script>1</script>".to_vec());
            let get = |h| resp.headers()[h].to_str().unwrap().to_string();
            (
                get(header::CONTENT_TYPE),
                get(header::CONTENT_SECURITY_POLICY),
                get(header::X_CONTENT_TYPE_OPTIONS),
            )
        };
        for kept in [
            "application/json",
            "Application/JSON; charset=utf-8",
            "image/webp",
        ] {
            let (ct, csp, sniff) = served(Some(kept));
            assert_eq!(
                (ct.as_str(), csp.as_str(), sniff.as_str()),
                (kept, "sandbox", "nosniff")
            );
        }
        for page in [
            "text/html",
            "text/html; charset=utf-8",
            "application/xhtml+xml",
            "image/svg+xml",
            "IMAGE/SVG+XML; charset=utf-8",
            "image",
            "",
        ] {
            assert_eq!(served(Some(page)).0, "application/octet-stream", "{page}");
        }
        let (ct, csp, sniff) = served(None);
        assert_eq!(
            (ct.as_str(), csp.as_str(), sniff.as_str()),
            ("application/octet-stream", "sandbox", "nosniff")
        );
    }
}
