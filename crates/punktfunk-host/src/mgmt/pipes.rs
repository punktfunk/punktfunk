//! One named pipe per installed plugin, `\\.\pipe\punktfunk-plugin-<id>`, serving the
//! management router over plain HTTP. A plugin process reaches the host here instead of on
//! the loopback port, so the port can be closed to the runner's account. The DACL admits
//! SYSTEM, Administrators, LocalService and the plugin's own AppContainer package; a client
//! whose token carries that package is the plugin, and the connection says so to the router.
//! Any other client — the runner with the sandbox off — still speaks with a bearer.
//!
//! The set follows `plugin-run/plugin-tokens.json`. A store job rewrites it and calls
//! [`changed`]; a CLI `plugins add` only rewrites it, so the set is re-read on a tick as well.

use crate::gamestream::tls::{serve_conn, PeerAddr, PeerCertFingerprint, PipePlugin};
use crate::windows::app_container::{package_sid, pipe_client_package_sid};
use crate::windows::plugin_pipe::create_plugin_pipe;
use axum::Router;
use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;
use windows::Win32::Foundation::HANDLE;

/// How long a CLI-driven change to the token file waits for its pipe.
const RECONCILE_EVERY: Duration = Duration::from_secs(10);

/// Connections one plugin may hold open at once; the port's per-IP ceiling, per pipe.
const MAX_CONNS_PER_PIPE: usize = 32;

/// What the router sees as the peer: a pipe is local, and the auth gate reads loopback.
const PIPE_PEER: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

static CHANGED: Notify = Notify::const_new();

/// A store job rewrote the token file: reconcile the pipe set now, not at the next tick.
pub(crate) fn changed() {
    CHANGED.notify_one();
}

/// The pipe a plugin with manifest id `id` dials. The SDK derives the same name.
pub(crate) fn pipe_name(id: &str) -> String {
    format!(r"\\.\pipe\punktfunk-plugin-{id}")
}

/// Serve a pipe per installed plugin until the host exits. `tokens` is the live map auth
/// consults; the file beside it carries an install the map has not seen yet.
pub(crate) async fn serve(app: Router, tokens: crate::mgmt::PluginTokens, config_dir: PathBuf) {
    let mut servers: HashMap<String, JoinHandle<()>> = HashMap::new();
    loop {
        let want = installed_ids(&tokens, &config_dir);
        servers.retain(|id, task| {
            if want.contains(id) {
                return true;
            }
            task.abort();
            tracing::info!(plugin = %id, "plugin pipe closed");
            false
        });
        for id in want {
            if servers.contains_key(&id) {
                continue;
            }
            servers.insert(id.clone(), tokio::spawn(serve_one(app.clone(), id)));
        }
        let _ = tokio::time::timeout(RECONCILE_EVERY, CHANGED.notified()).await;
    }
}

/// Ids with a token: the live map and the file, so a CLI install is served before its first
/// request can miss. An id outside the manifest's alphabet names no pipe.
fn installed_ids(tokens: &crate::mgmt::PluginTokens, config_dir: &Path) -> BTreeSet<String> {
    let mut ids: BTreeSet<String> = tokens
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .keys()
        .cloned()
        .collect();
    if let Some(file) = crate::mgmt_token::read_per_plugin(config_dir) {
        ids.extend(file.into_keys());
    }
    ids.retain(|id| {
        !id.is_empty()
            && id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    });
    ids
}

/// The accept loop for one plugin's pipe. Each instance serves one connection; the next
/// instance is created before this one is handed to the router, as the port's accept does.
async fn serve_one(app: Router, id: String) {
    let name = pipe_name(&id);
    // Without a package SID the pipe is LocalService's alone and every client keeps its bearer.
    let package = match package_sid(&id) {
        Ok(sid) => Some(sid),
        Err(e) => {
            tracing::warn!(plugin = %id, error = %format!("{e:#}"), "plugin package SID not derived");
            None
        }
    };
    let conns = Arc::new(Semaphore::new(MAX_CONNS_PER_PIPE));
    let mut first = true;
    let mut announced = false;
    loop {
        let server = match create_plugin_pipe(&name, first, package.as_deref()) {
            Ok(s) => s,
            Err(e) => {
                // The first instance is refused while another process holds the name: a
                // previous host still closing, or a squat. Say so, and keep trying.
                tracing::warn!(plugin = %id, error = %e, "plugin pipe not created — retrying");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };
        if first {
            tracing::info!(plugin = %id, pipe = %name, "plugin pipe listening");
        }
        first = false;
        if let Err(e) = server.connect().await {
            tracing::warn!(plugin = %id, error = %e, "plugin pipe accept failed");
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        // The client's token names its package, or none: only the plugin's own package is
        // the plugin. A runner-account client with no container keeps its bearer lane.
        let client = pipe_client_package_sid(HANDLE(server.as_raw_handle()));
        let stamped = client.is_some() && client == package;
        if !announced {
            tracing::info!(plugin = %id, container = stamped, "plugin reached the host over its pipe");
            announced = true;
        }
        // Over the ceiling: this instance closes unanswered, and the plugin's next dial waits.
        let Ok(permit) = conns.clone().try_acquire_owned() else {
            tracing::warn!(plugin = %id, "plugin pipe connection ceiling reached — dropping");
            continue;
        };
        let app = app.clone();
        let plugin = stamped.then(|| PipePlugin(id.clone()));
        tokio::spawn(async move {
            let _permit = permit;
            serve_conn(
                server,
                app,
                PeerCertFingerprint(None),
                PeerAddr(PIPE_PEER),
                None,
                plugin,
            )
            .await;
        });
    }
}
