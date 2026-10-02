//! Host clipboard backends (`design/clipboard-and-file-transfer.md`).
//!
//! Wire protocol and client half live in `punktfunk-core`. This module owns the
//! host selection: offer what a host app copied, paste what the remote offered.
//! [`HostClipboard::open`] picks one backend; [`session`] stays agnostic.
//!
//! Linux prefers [`wayland`] (`ext-data-control-v1`). GNOME has no data-control;
//! [`mutter`] uses `org.gnome.Mutter.RemoteDesktop.Session` directly — the xdg
//! portal needs an interactive grant a headless host cannot answer. A gamescope
//! session uses [`gamescope`], text on its Xwayland.
//! [`windows`] watches `WM_CLIPBOARDUPDATE` and serves via `WM_RENDERFORMAT`.

#[cfg(target_os = "linux")]
mod gamescope;
#[cfg(target_os = "linux")]
mod mutter;
#[cfg(target_os = "linux")]
mod wayland;
#[cfg(target_os = "windows")]
mod windows;
/// Win32 clipboard ↔ wire bytes (CF_HTML offsets, UTF-16, RTF NULs). No Win32
/// crate, so tests run on every host; [`windows`] is the only production caller.
#[cfg(any(target_os = "windows", test))]
mod winfmt;

pub mod session;

#[cfg(target_os = "linux")]
use std::io::Write as _;
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
use std::sync::Arc;

#[cfg(target_os = "linux")]
use punktfunk_core::clipboard::CLIP_FETCH_CAP;

pub enum ClipEvent {
    /// Empty `mimes` means cleared; bytes wait for a client fetch.
    Selection {
        mimes: Vec<String>,
    },
    /// Host app pasting the client's offer; coordinator fetches then `responder`.
    Paste {
        mime: String,
        responder: PasteResponder,
    },
    Closed,
}

/// Bytes sink for a [`ClipEvent::Paste`]. The coordinator never picks the
/// mechanism (pipe, oneshot, or blocking mpsc).
pub enum PasteResponder {
    /// Compositor `send` pipe; write bytes and close (EOF finishes the paste).
    #[cfg(target_os = "linux")]
    Fd(OwnedFd),
    /// Mutter actor owns the `SelectionWrite` fd and the required `SelectionWriteDone`.
    #[cfg(target_os = "linux")]
    Channel(tokio::sync::oneshot::Sender<Vec<u8>>),
    /// `WM_RENDERFORMAT` handler blocks the message-loop thread; `std::sync::mpsc`.
    #[cfg(target_os = "windows")]
    Sync(std::sync::mpsc::Sender<Vec<u8>>),
}

impl PasteResponder {
    /// Empty `bytes` (failed fetch) is an empty paste, never a hang.
    pub async fn respond(self, bytes: Vec<u8>) {
        match self {
            #[cfg(target_os = "linux")]
            PasteResponder::Fd(fd) => {
                let _ = tokio::task::spawn_blocking(move || fulfill_paste(fd, &bytes)).await;
            }
            #[cfg(target_os = "linux")]
            PasteResponder::Channel(tx) => {
                let _ = tx.send(bytes);
            }
            #[cfg(target_os = "windows")]
            PasteResponder::Sync(tx) => {
                let _ = tx.send(bytes);
            }
        }
    }
}

/// Blocking; call off the reactor. Close is EOF, which completes the paste.
#[cfg(target_os = "linux")]
fn fulfill_paste(fd: OwnedFd, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)?;
    Ok(())
}

/// Blocking; reads a host selection transfer to EOF. A selection over [`CLIP_FETCH_CAP`] fails
/// the read: its first 64 MiB is a broken file, not a smaller one.
#[cfg(target_os = "linux")]
fn read_capped(r: impl std::io::Read) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut buf = Vec::new();
    r.take(CLIP_FETCH_CAP as u64 + 1).read_to_end(&mut buf)?;
    anyhow::ensure!(
        buf.len() <= CLIP_FETCH_CAP,
        "clipboard selection exceeds the {CLIP_FETCH_CAP}-byte transfer cap"
    );
    Ok(buf)
}

pub enum HostClipboard {
    #[cfg(target_os = "linux")]
    DataControl(Box<wayland::ClipboardBackend>),
    #[cfg(target_os = "linux")]
    Mutter(mutter::MutterClipboard),
    #[cfg(target_os = "linux")]
    Gamescope(gamescope::GamescopeClipboard),
    #[cfg(target_os = "windows")]
    Windows(windows::WindowsClipboard),
}

impl HostClipboard {
    /// No clipboard the session can reach → error; caller reports `BACKEND_UNAVAILABLE`.
    pub async fn open(
        target: crate::ClipTarget,
    ) -> anyhow::Result<(
        HostClipboard,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        #[cfg(target_os = "linux")]
        {
            if let crate::ClipTarget::Gamescope(xwayland) = target {
                let (g, rx) = gamescope::GamescopeClipboard::open(xwayland)?;
                return Ok((HostClipboard::Gamescope(g), rx));
            }
            // Bind does blocking Wayland roundtrips; keep them off the reactor.
            let dc = tokio::task::spawn_blocking(wayland::ClipboardBackend::open)
                .await
                .map_err(|e| anyhow::anyhow!("data-control open join: {e}"))?;
            match dc {
                Ok((b, rx)) => return Ok((HostClipboard::DataControl(Box::new(b)), rx)),
                Err(e) => tracing::debug!(
                    error = format!("{e:#}"),
                    "no ext-data-control — trying Mutter direct clipboard"
                ),
            }
            let (m, rx) = mutter::MutterClipboard::open().await.map_err(|e| {
                e.context("no clipboard backend (neither ext-data-control nor Mutter)")
            })?;
            Ok((HostClipboard::Mutter(m), rx))
        }
        #[cfg(target_os = "windows")]
        {
            let _ = target;
            let (b, rx) = windows::WindowsClipboard::open().await?;
            Ok((HostClipboard::Windows(b), rx))
        }
    }

    /// Empty means nothing to offer.
    pub fn current_wire_mimes(&self) -> Vec<String> {
        match self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(b) => b.current_wire_mimes(),
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => m.current_wire_mimes(),
            #[cfg(target_os = "linux")]
            HostClipboard::Gamescope(g) => g.current_wire_mimes(),
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => w.current_wire_mimes(),
        }
    }

    pub fn set_offer(&self, wire_mimes: &[String]) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(b) => b.set_offer(wire_mimes),
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => {
                m.set_offer(wire_mimes);
                Ok(())
            }
            #[cfg(target_os = "linux")]
            HostClipboard::Gamescope(g) => g.set_offer(wire_mimes),
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => {
                w.set_offer(wire_mimes);
                Ok(())
            }
        }
    }

    pub fn clear_offer(&self) -> anyhow::Result<()> {
        match self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(b) => b.clear_offer(),
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => {
                m.clear_offer();
                Ok(())
            }
            #[cfg(target_os = "linux")]
            HostClipboard::Gamescope(g) => g.clear_offer(),
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => {
                w.clear_offer();
                Ok(())
            }
        }
    }

    /// Data-control blocks on a pipe (offloaded); Mutter round-trips D-Bus; gamescope
    /// converts on its Xwayland; Windows reads on a blocking thread.
    pub async fn read_current(self: &Arc<Self>, wire_mime: &str) -> anyhow::Result<Vec<u8>> {
        match &**self {
            #[cfg(target_os = "linux")]
            HostClipboard::DataControl(_) => {
                let me = Arc::clone(self);
                let wire = wire_mime.to_string();
                tokio::task::spawn_blocking(move || match &*me {
                    HostClipboard::DataControl(b) => b.read_current(&wire),
                    _ => unreachable!("variant checked above"),
                })
                .await
                .map_err(|e| anyhow::anyhow!("data-control read join: {e}"))?
            }
            #[cfg(target_os = "linux")]
            HostClipboard::Mutter(m) => m.read_current(wire_mime).await,
            #[cfg(target_os = "linux")]
            HostClipboard::Gamescope(g) => g.read_current(wire_mime).await,
            #[cfg(target_os = "windows")]
            HostClipboard::Windows(w) => w.read_current(wire_mime).await,
        }
    }
}

pub const WIRE_TEXT: &str = "text/plain;charset=utf-8";
pub const WIRE_HTML: &str = "text/html";
pub const WIRE_RTF: &str = "text/rtf";
pub const WIRE_PNG: &str = "image/png";
/// JPEG from the source clipboard, verbatim. Do not transcode to PNG: a
/// lossy original re-encoded lossless is pure bloat. PNG is the fallback.
pub const WIRE_JPEG: &str = "image/jpeg";
/// GIF verbatim; transcoding would drop animation.
pub const WIRE_GIF: &str = "image/gif";

/// Canonical wire MIME, or `None` for compositor internals (`TARGETS`,
/// `TIMESTAMP`, `SAVE_TARGETS`). Aliases collapse so the offer list dedups.
#[cfg(target_os = "linux")]
pub fn wayland_to_wire(wl: &str) -> Option<&'static str> {
    // Some apps send `text/plain;charset=...` with odd charsets, or bare `text/plain`.
    let base = wl.split(';').next().unwrap_or(wl).trim();
    match wl {
        "text/html" => Some(WIRE_HTML),
        "text/rtf" | "application/rtf" | "text/richtext" => Some(WIRE_RTF),
        "image/png" => Some(WIRE_PNG),
        "image/jpeg" => Some(WIRE_JPEG),
        "image/gif" => Some(WIRE_GIF),
        _ => match base {
            "text/plain" | "UTF8_STRING" | "STRING" | "TEXT" => Some(WIRE_TEXT),
            _ => None,
        },
    }
}

#[cfg(target_os = "linux")]
pub fn wayland_candidates(wire: &str) -> &'static [&'static str] {
    match wire {
        WIRE_TEXT => &[
            "text/plain;charset=utf-8",
            "text/plain",
            "UTF8_STRING",
            "STRING",
            "TEXT",
        ],
        WIRE_HTML => &["text/html"],
        WIRE_RTF => &["text/rtf", "application/rtf", "text/richtext"],
        WIRE_PNG => &["image/png"],
        WIRE_JPEG => &["image/jpeg"],
        WIRE_GIF => &["image/gif"],
        _ => &[],
    }
}

#[cfg(target_os = "linux")]
pub fn pick_wayland_mime(wire: &str, available: &[String]) -> Option<String> {
    wayland_candidates(wire)
        .iter()
        .find(|c| available.iter().any(|a| a == *c))
        .map(|c| c.to_string())
}

#[cfg(target_os = "linux")]
pub fn offer_wire_mimes(raw: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for m in raw {
        if let Some(wire) = wayland_to_wire(m) {
            if !out.contains(&wire) {
                out.push(wire);
            }
        }
    }
    out
}

/// Client MIME safe to pass as a Wayland string. Printable ASCII, ≤255,
/// `type/subtype`. The generated encoder `unwrap()`s a `CString`; a NUL panics.
#[cfg(target_os = "linux")]
fn valid_passthrough_mime(m: &str) -> bool {
    let Some((ty, rest)) = m.split_once('/') else {
        return false;
    };
    !ty.is_empty()
        && !rest.is_empty()
        && m.len() <= 255
        // Printable ASCII without space: no NUL, no other control, no non-ASCII.
        && m.bytes().all(|b| (0x21..=0x7E).contains(&b))
}

/// A rich-only offer also advertises `text/plain` so plain-text targets can paste
/// (destination-side, one way).
#[cfg(target_os = "linux")]
pub fn wayland_offers_for(wire_mimes: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: &str| {
        if !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    let mut has_plain = false;
    let mut has_rich = false;
    for w in wire_mimes {
        match w.as_str() {
            WIRE_TEXT => {
                has_plain = true;
                push("text/plain;charset=utf-8");
                push("text/plain");
                push("UTF8_STRING");
                push("STRING");
            }
            WIRE_HTML => {
                has_rich = true;
                push("text/html");
            }
            WIRE_RTF => {
                has_rich = true;
                push("text/rtf");
            }
            WIRE_PNG => push("image/png"),
            WIRE_JPEG => push("image/jpeg"),
            WIRE_GIF => push("image/gif"),
            // Uncanonicalized MIME is a client-controlled Wayland string. The generated
            // encoder `unwrap()`s a `CString`; an interior NUL panics. Wire UTF-8 lossy
            // keeps `\0`. Validate here, at the libwayland boundary.
            other if valid_passthrough_mime(other) => push(other),
            other => {
                tracing::debug!(mime = %other.escape_debug(), "clipboard: dropping a malformed client MIME");
            }
        }
    }
    // Rich without plain: advertise plain; the source derives it lazily.
    if has_rich && !has_plain {
        push("text/plain;charset=utf-8");
        push("text/plain");
        push("UTF8_STRING");
        push("STRING");
    }
    out
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn a_selection_over_the_cap_fails_instead_of_truncating() {
        use std::io::Read as _;
        let cap = CLIP_FETCH_CAP as u64;
        assert_eq!(
            read_capped(std::io::repeat(7).take(cap)).unwrap().len(),
            CLIP_FETCH_CAP
        );
        assert!(read_capped(std::io::repeat(7).take(cap + 1)).is_err());
    }

    #[test]
    fn wayland_to_wire_canonicalizes_and_drops_targets() {
        assert_eq!(wayland_to_wire("text/plain"), Some(WIRE_TEXT));
        assert_eq!(wayland_to_wire("UTF8_STRING"), Some(WIRE_TEXT));
        assert_eq!(wayland_to_wire("text/plain;charset=utf-8"), Some(WIRE_TEXT));
        assert_eq!(wayland_to_wire("text/html"), Some(WIRE_HTML));
        assert_eq!(wayland_to_wire("application/rtf"), Some(WIRE_RTF));
        assert_eq!(wayland_to_wire("image/png"), Some(WIRE_PNG));
        assert_eq!(wayland_to_wire("image/jpeg"), Some(WIRE_JPEG));
        assert_eq!(wayland_to_wire("image/gif"), Some(WIRE_GIF));
        assert_eq!(wayland_to_wire("TARGETS"), None);
        assert_eq!(wayland_to_wire("TIMESTAMP"), None);
        assert_eq!(wayland_to_wire("image/webp"), None);
    }

    #[test]
    fn offer_wire_mimes_dedups_aliases() {
        let raw = vec![
            "TARGETS".to_string(),
            "UTF8_STRING".to_string(),
            "text/plain;charset=utf-8".to_string(),
            "text/plain".to_string(),
            "text/html".to_string(),
        ];
        assert_eq!(offer_wire_mimes(&raw), vec![WIRE_TEXT, WIRE_HTML]);
    }

    #[test]
    fn passthrough_mimes_cannot_carry_a_nul_or_control_byte() {
        // Interior NUL survives `String::from_utf8_lossy` on the wire.
        assert!(!valid_passthrough_mime("image/webp\0"));
        assert!(!valid_passthrough_mime("\0"));
        assert!(!valid_passthrough_mime("image/\0webp"));
        assert!(!valid_passthrough_mime("image/web\np"));
        assert!(!valid_passthrough_mime("image/web p"));
        assert!(!valid_passthrough_mime("image/web\tp"));
        assert!(!valid_passthrough_mime(""));
        assert!(!valid_passthrough_mime("noslash"));
        assert!(!valid_passthrough_mime("/nosubtype"));
        assert!(!valid_passthrough_mime("notype/"));
        assert!(!valid_passthrough_mime(&format!(
            "image/{}",
            "x".repeat(300)
        )));
        assert!(valid_passthrough_mime("image/webp"));
        assert!(valid_passthrough_mime("application/x-custom+json"));
        assert!(valid_passthrough_mime("text/plain;charset=utf-8"));

        let offers = wayland_offers_for(&["image/webp\0".to_string(), WIRE_PNG.to_string()]);
        assert_eq!(offers, vec!["image/png".to_string()]);
    }

    #[test]
    fn pick_wayland_mime_prefers_canonical() {
        let avail = vec!["text/plain".to_string(), "UTF8_STRING".to_string()];
        // Charset form missing; next candidate (`text/plain`) wins.
        assert_eq!(
            pick_wayland_mime(WIRE_TEXT, &avail),
            Some("text/plain".to_string())
        );
        let avail2 = vec![
            "text/plain;charset=utf-8".to_string(),
            "text/plain".to_string(),
        ];
        assert_eq!(
            pick_wayland_mime(WIRE_TEXT, &avail2),
            Some("text/plain;charset=utf-8".to_string())
        );
        assert_eq!(pick_wayland_mime(WIRE_PNG, &avail2), None);
    }

    #[test]
    fn wayland_offers_synthesizes_plain_for_rich_only() {
        let offers = wayland_offers_for(&[WIRE_HTML.to_string()]);
        assert!(offers.iter().any(|m| m == "text/html"));
        assert!(
            offers.iter().any(|m| m == "text/plain;charset=utf-8"),
            "rich-only offer must synthesize plain text: {offers:?}"
        );
        let offers2 = wayland_offers_for(&[WIRE_TEXT.to_string()]);
        assert!(offers2.iter().any(|m| m == "UTF8_STRING"));
        assert_eq!(offers2.iter().filter(|m| *m == "text/plain").count(), 1);
    }
}
