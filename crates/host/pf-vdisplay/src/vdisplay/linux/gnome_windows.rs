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
use std::sync::Once;
use std::time::Duration;

const UUID: &str = "punktfunk-windows@unom.io";
const METADATA: &str = include_str!("../../../gnome-shell/punktfunk-windows@unom.io/metadata.json");
const EXTENSION: &str = include_str!("../../../gnome-shell/punktfunk-windows@unom.io/extension.js");

const BUS_NAME: &str = "io.unom.Punktfunk.Shell";
const OBJECT_PATH: &str = "/io/unom/Punktfunk/Shell";

/// One read, connect to reply. The lease watcher asks every second.
const READ_BUDGET: Duration = Duration::from_millis(800);

static INSTALL: Once = Once::new();

/// Every normal, unminimized window the shell has. `None` when the extension is not running (not
/// installed, not yet loaded, or turned off); empty when it did not answer inside [`READ_BUDGET`].
pub(crate) fn toplevels() -> Option<Vec<Toplevel>> {
    INSTALL.call_once(install);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        let read = async {
            let conn = zbus::Connection::session().await.ok()?;
            let running = conn
                .call_method(
                    Some("org.freedesktop.DBus"),
                    "/org/freedesktop/DBus",
                    Some("org.freedesktop.DBus"),
                    "NameHasOwner",
                    &(BUS_NAME,),
                )
                .await
                .ok()?
                .body()
                .deserialize::<bool>()
                .ok()?;
            if !running {
                return None;
            }
            let rows = conn
                .call_method(
                    Some(BUS_NAME),
                    OBJECT_PATH,
                    Some(BUS_NAME),
                    "ListWindows",
                    &(),
                )
                .await
                .ok()
                .and_then(|reply| reply.body().deserialize::<Vec<Row>>().ok())
                .unwrap_or_default();
            Some(rows.into_iter().map(toplevel).collect())
        };
        tokio::time::timeout(READ_BUDGET, read)
            .await
            .unwrap_or_else(|_| Some(Vec::new()))
    })
}

type Row = (u32, String, bool, bool, String);

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
        let conn = zbus::Connection::session().await.ok()?;
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

    /// The embedded extension names the bus and method this reader calls.
    #[test]
    fn the_embedded_extension_serves_what_the_reader_calls() {
        assert!(METADATA.contains(UUID));
        assert!(EXTENSION.contains(BUS_NAME) && EXTENSION.contains(OBJECT_PATH));
        assert!(EXTENSION.contains("a(usbbs)") && EXTENSION.contains("ListWindows"));
    }
}
