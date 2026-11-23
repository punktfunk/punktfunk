//! GNOME's window list, from punktfunk's own GNOME Shell extension over D-Bus.
//!
//! Mutter offers Wayland clients no toplevel list, and the shell's `Introspect` API answers only
//! GNOME's own portal and screencast services. The extension (`gnome-shell/punktfunk-windows@unom.io`,
//! embedded below) runs inside the shell and serves `ListWindows`: pid, class, focus and fullscreen
//! per window, never titles.
//!
//! The host installs it into the user's extensions directory and enables it the first time it asks.
//! GNOME on Wayland loads a newly installed extension only at the next login; until then there is no
//! list, and a launch hold ends when the game's process starts.

use crate::toplevels::Toplevel;
use ashpd::zbus;
use std::path::PathBuf;
use std::sync::{Mutex, Once};
use std::time::Duration;

const UUID: &str = "punktfunk-windows@unom.io";
const METADATA: &str = include_str!("../../../gnome-shell/punktfunk-windows@unom.io/metadata.json");
const EXTENSION: &str = include_str!("../../../gnome-shell/punktfunk-windows@unom.io/extension.js");

const BUS_NAME: &str = "io.unom.punktfunk.shell";
const OBJECT_PATH: &str = "/io/unom/punktfunk/shell";

/// One read, connect to reply. The lease watcher asks every second.
const READ_BUDGET: Duration = Duration::from_millis(800);

static INSTALL: Once = Once::new();

/// The bus connection every read reuses. Built on the portal runtime, which is never dropped, so
/// its reader outlives each read. A failed read drops it and the next read connects again.
static BUS: Mutex<Option<zbus::Connection>> = Mutex::new(None);

/// Every normal, unminimized window the shell has. `None` only when nobody owns [`BUS_NAME`]
/// (the extension is not installed, not yet loaded, or turned off); empty on any other failure
/// and when the shell did not answer inside [`READ_BUDGET`], so the next read tries again.
pub(crate) fn toplevels() -> Option<Vec<Toplevel>> {
    INSTALL.call_once(install);
    let Ok(rt) = pf_portal::portal_runtime() else {
        return Some(Vec::new());
    };
    // The timeout is built inside the runtime: a timer made on the caller's thread, which has
    // no runtime of its own, panics.
    let read = rt
        .block_on(async { tokio::time::timeout(READ_BUDGET, read()).await })
        .unwrap_or_else(|elapsed| Err(elapsed.into()));
    if read.is_err() {
        *BUS.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
    listed(read)
}

/// [`BUS`], connected first when it is empty.
async fn bus() -> zbus::Result<zbus::Connection> {
    if let Some(conn) = BUS.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(conn);
    }
    let conn = pf_portal::session_bus().await?;
    *BUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn.clone());
    Ok(conn)
}

type Row = (u32, String, bool, bool, String);

/// One read: `None` when nobody owns [`BUS_NAME`], else the extension's rows.
async fn read() -> anyhow::Result<Option<Vec<Row>>> {
    let conn = bus().await?;
    let owned: bool = conn
        .call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &(BUS_NAME,),
        )
        .await?
        .body()
        .deserialize()?;
    if !owned {
        return Ok(None);
    }
    let rows = conn
        .call_method(
            Some(BUS_NAME),
            OBJECT_PATH,
            Some(BUS_NAME),
            "ListWindows",
            &(),
        )
        .await?
        .body()
        .deserialize()?;
    Ok(Some(rows))
}

/// What [`toplevels`] answers for one read: `None` only for an unowned name. An error is an
/// empty list: the launch hold reads again next tick instead of ending.
fn listed(read: anyhow::Result<Option<Vec<Row>>>) -> Option<Vec<Toplevel>> {
    match read {
        Ok(None) => None,
        Ok(Some(rows)) => Some(rows.into_iter().map(toplevel).collect()),
        Err(_) => Some(Vec::new()),
    }
}

/// One `ListWindows` row. GNOME names no head per window, so `output` and `workspace` stay empty.
fn toplevel((pid, class, focused, fullscreen, id): Row) -> Toplevel {
    Toplevel {
        id,
        title: String::new(),
        app_id: class,
        pid: (pid != 0).then_some(pid),
        focused,
        fullscreen,
        workspace: String::new(),
        output: String::new(),
    }
}

/// Write the embedded extension into the user's extensions directory when it is missing or stale,
/// then ask the shell to enable it. Never re-enables an extension the user turned off: only a
/// fresh write asks.
fn install() {
    let Some(dir) = extensions_dir().map(|d| d.join(UUID)) else {
        return;
    };
    let fresh = [("metadata.json", METADATA), ("extension.js", EXTENSION)]
        .iter()
        .any(|(name, body)| std::fs::read_to_string(dir.join(name)).ok().as_deref() != Some(body));
    if !fresh {
        return;
    }
    let written = std::fs::create_dir_all(&dir).and_then(|()| {
        std::fs::write(dir.join("metadata.json"), METADATA)?;
        std::fs::write(dir.join("extension.js"), EXTENSION)
    });
    if let Err(e) = written {
        tracing::warn!(dir = %dir.display(), error = %e, "gnome: window-list extension not installed");
        return;
    }
    let enabled = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .and_then(|rt| rt.block_on(enable()));
    tracing::info!(
        dir = %dir.display(),
        enabled = ?enabled,
        "gnome: installed the host's window-list extension — GNOME loads it at the next login; \
         until then launch holds end when the game's process starts"
    );
}

/// `org.gnome.Shell.Extensions.EnableExtension`, the call `gnome-extensions enable` makes.
async fn enable() -> Option<bool> {
    let call = async {
        let conn = pf_portal::session_bus().await.ok()?;
        let reply = conn
            .call_method(
                Some("org.gnome.Shell.Extensions"),
                "/org/gnome/Shell/Extensions",
                Some("org.gnome.Shell.Extensions"),
                "EnableExtension",
                &(UUID,),
            )
            .await
            .ok()?;
        reply.body().deserialize::<bool>().ok()
    };
    tokio::time::timeout(Duration::from_secs(3), call)
        .await
        .ok()?
}

/// `$XDG_DATA_HOME/gnome-shell/extensions`, else `~/.local/share/gnome-shell/extensions`.
fn extensions_dir() -> Option<PathBuf> {
    let data = crate::with_env_lock(|| {
        std::env::var_os("XDG_DATA_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|v| !v.is_empty())
                    .map(|h| PathBuf::from(h).join(".local/share"))
            })
    })?;
    Some(data.join("gnome-shell/extensions"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_row_becomes_a_toplevel_without_a_title_or_a_zero_pid() {
        let t = toplevel((4242, "steam_app_570".into(), true, false, "17".into()));
        assert_eq!(
            (
                t.pid,
                t.app_id.as_str(),
                t.focused,
                t.fullscreen,
                t.id.as_str()
            ),
            (Some(4242), "steam_app_570", true, false, "17")
        );
        assert!(t.title.is_empty());
        assert_eq!(
            toplevel((0, String::new(), false, false, "1".into())).pid,
            None
        );
    }

    /// Only an unowned extension name lists nothing; a bus error is an empty list.
    #[test]
    fn only_an_unowned_extension_name_lists_no_windows() {
        assert!(listed(Ok(None)).is_none(), "the extension is not running");
        let error = Err(anyhow::anyhow!("session bus restarting"));
        assert_eq!(listed(error).map(|w| w.len()), Some(0));
        let row = (4242, "steam_app_570".into(), true, false, "17".into());
        assert_eq!(listed(Ok(Some(vec![row]))).map(|w| w.len()), Some(1));
    }

    /// The embedded extension names the bus and method this reader calls.
    #[test]
    fn the_embedded_extension_serves_what_the_reader_calls() {
        assert!(METADATA.contains(UUID));
        assert!(EXTENSION.contains(BUS_NAME) && EXTENSION.contains(OBJECT_PATH));
        assert!(EXTENSION.contains("a(usbbs)") && EXTENSION.contains("ListWindows"));
    }
}
