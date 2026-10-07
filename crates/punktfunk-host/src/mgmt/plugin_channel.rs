//! A plugin's settings page without a listener. A Windows plugin in its AppContainer can bind
//! nothing, so it dials its own pipe a second time, asks for an upgrade, and the raw connection
//! is parked here. A request for that page goes down one parked connection as plain HTTP with
//! the plugin's own secret added by the host; the plugin dials again as each one is used, so a
//! slow page never holds the next. A registration with `ui.port` 0 says the plugin works this way.

use crate::mgmt::auth::PluginIdentity;
use crate::mgmt::shared::{api_error, ApiError};
use axum::body::Body;
use axum::extract::{Extension, Path, Request};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use hyper::upgrade::Upgraded;
use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::Notify;

/// Parked connections per plugin, oldest first. Each serves one request.
static PARKED: LazyLock<Mutex<HashMap<String, VecDeque<Upgraded>>>> =
    LazyLock::new(Default::default);
/// Woken when a connection is parked, for a request that found none.
static ARRIVED: Notify = Notify::const_new();
/// The management runtime, for a hold or an install made from a thread of its own.
static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// More parked than this and the oldest goes: a plugin that dials in a loop holds no more.
const MAX_PARKED: usize = 8;
/// How long a request waits for the plugin to park the next connection.
const ATTACH_WAIT: Duration = Duration::from_secs(3);
const UPGRADE_PROTOCOL: &str = "punktfunk-ui";

/// Headers that describe the hop, not the request, plus the two the host sets itself.
const NOT_FORWARDED: [HeaderName; 8] = [
    header::CONNECTION,
    header::HOST,
    header::AUTHORIZATION,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
    header::TE,
    header::TRAILER,
    header::PROXY_AUTHORIZATION,
];

/// Called once the management API serves: the blocking callers need this runtime.
pub(crate) fn remember_runtime() {
    let _ = RUNTIME.set(tokio::runtime::Handle::current());
}

/// Attach a plugin UI channel
///
/// `Upgrade: punktfunk-ui` from the plugin, on its own pipe. The connection is kept for the
/// console's next request to the plugin's page, which the host sends down it as plain HTTP.
#[utoipa::path(
    get,
    path = "/plugins/{id}/ui/attach",
    tag = "plugins",
    operation_id = "attachPluginUi",
    params(("id" = String, Path, description = "The plugin id")),
    responses(
        (status = SWITCHING_PROTOCOLS, description = "The connection is parked for the plugin's page"),
        (status = BAD_REQUEST, description = "Not an `Upgrade: punktfunk-ui` request", body = ApiError),
        (status = UNAUTHORIZED, description = "Missing or invalid bearer token", body = ApiError),
        (status = FORBIDDEN, description = "Not this plugin's own identity", body = ApiError),
    )
)]
pub(crate) async fn attach(
    Path(id): Path<String>,
    who: Option<Extension<PluginIdentity>>,
    req: Request,
) -> Response {
    if who.map(|w| w.0 .0) != Some(id.clone()) {
        return api_error(StatusCode::FORBIDDEN, "a plugin attaches its own channel");
    }
    let wants = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case(UPGRADE_PROTOCOL));
    if !wants {
        return api_error(StatusCode::BAD_REQUEST, "send Upgrade: punktfunk-ui");
    }
    let on_upgrade = hyper::upgrade::on(req);
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => park(&id, upgraded),
            Err(e) => tracing::debug!(plugin = %id, error = %e, "plugin UI channel not upgraded"),
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, UPGRADE_PROTOCOL)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// The console's request to a plugin page, any method, under `/plugins/{id}/ui/`. Admin lane:
/// the host adds the plugin's secret, so the console never holds it for a channel plugin.
pub(crate) async fn proxy(Path((id, rest)): Path<(String, String)>, req: Request) -> Response {
    let Some(cred) = super::plugins::ui_credential(&id).filter(|c| c.port == 0) else {
        return api_error(
            StatusCode::NOT_FOUND,
            "no live plugin UI channel with that id",
        );
    };
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    let (parts, body) = req.into_parts();
    let target = format!("/{rest}{query}");
    match send(
        &id,
        &cred.secret,
        parts.method,
        &target,
        &parts.headers,
        body,
    )
    .await
    {
        Ok(resp) => {
            let (parts, body) = resp.into_parts();
            Response::from_parts(parts, Body::new(body))
        }
        Err(Unreached::NotAttached) => api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "the plugin UI is not attached",
        ),
        Err(Unreached::Io(e)) => api_error(StatusCode::BAD_GATEWAY, &format!("plugin UI: {e}")),
    }
}

/// Why a request never reached the plugin.
#[derive(Debug)]
pub(crate) enum Unreached {
    NotAttached,
    Io(String),
}

impl std::fmt::Display for Unreached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unreached::NotAttached => f.write_str("the plugin UI is not attached"),
            Unreached::Io(e) => f.write_str(e),
        }
    }
}

/// One request down one parked connection, with the plugin's secret as the bearer.
async fn send(
    id: &str,
    secret: &str,
    method: Method,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<hyper::Response<hyper::body::Incoming>, Unreached> {
    let Some(upgraded) = take(id).await else {
        return Err(Unreached::NotAttached);
    };
    // `Upgraded` already speaks hyper's io traits; it needs no tokio adapter.
    let (mut sender, conn) = hyper::client::conn::http1::handshake(upgraded)
        .await
        .map_err(|e| Unreached::Io(e.to_string()))?;
    tokio::spawn(async move {
        let _ = conn.await;
    });
    let mut req = hyper::Request::builder().method(method).uri(path_and_query);
    for (name, value) in headers {
        if !NOT_FORWARDED.contains(name) {
            req = req.header(name, value);
        }
    }
    let req = req
        .header(header::HOST, "punktfunk.plugin")
        .header(header::AUTHORIZATION, format!("Bearer {secret}"))
        .body(body)
        .map_err(|e| Unreached::Io(e.to_string()))?;
    sender
        .send_request(req)
        .await
        .map_err(|e| Unreached::Io(e.to_string()))
}

/// The host's own call to a plugin page — a hold, an install — from a thread of its own:
/// the status and the body as text, or why not.
pub(crate) fn request_blocking(
    id: &str,
    path: &str,
    json: &str,
    timeout: Duration,
) -> Result<(u16, String), Unreached> {
    let Some(cred) = super::plugins::ui_credential(id).filter(|c| c.port == 0) else {
        return Err(Unreached::NotAttached);
    };
    let call = async {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let body = Body::from(json.to_string());
        let resp = send(id, &cred.secret, Method::POST, path, &headers, body).await?;
        let status = resp.status().as_u16();
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| Unreached::Io(e.to_string()))?
            .to_bytes();
        Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
    };
    let timed = async {
        tokio::time::timeout(timeout, call)
            .await
            .unwrap_or_else(|_| Err(Unreached::Io("timed out".into())))
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(timed)),
        Err(_) => RUNTIME
            .get()
            .ok_or_else(|| Unreached::Io("no management runtime".into()))?
            .block_on(timed),
    }
}

fn park(id: &str, upgraded: Upgraded) {
    let mut all = PARKED.lock().unwrap_or_else(|p| p.into_inner());
    let queue = all.entry(id.to_string()).or_default();
    if queue.len() >= MAX_PARKED {
        queue.pop_front();
    }
    queue.push_back(upgraded);
    drop(all);
    ARRIVED.notify_waiters();
}

/// The oldest parked connection for `id`, waiting [`ATTACH_WAIT`] for one to arrive.
async fn take(id: &str) -> Option<Upgraded> {
    let deadline = tokio::time::Instant::now() + ATTACH_WAIT;
    loop {
        let notified = ARRIVED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let taken = PARKED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(id)
            .and_then(|q| q.pop_front());
        if let Some(upgraded) = taken {
            return Some(upgraded);
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            return None;
        }
    }
}
