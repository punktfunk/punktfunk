//! macOS shared-clipboard backend over `NSPasteboard` (`design/clipboard-and-file-transfer.md` §4).
//!
//! macOS has no clipboard-change notification, so one thread owns the pasteboard handle and
//! polls `changeCount`. A count that is not the one our last write returned is another app's
//! copy and becomes [`ClipEvent::Selection`]; our own writes never echo.
//!
//! A client offer is written eagerly. A lazy promise (`NSPasteboardTypeOwner`) is answered only
//! on the main thread's run loop, which a tokio host does not run, so each offered kind is
//! fetched through [`ClipEvent::Paste`] and written together. A host copy that lands while the
//! fetch runs is the newer one and wins.
//!
//! Carried kinds: text, HTML, RTF, PNG, JPEG, GIF. Files and other types are not offered.

use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::{msg_send, sel};
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSPasteboard};
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};

use punktfunk_core::clipboard::CLIP_FETCH_CAP;

use super::macfmt::{self, Ledger};
use super::{ClipEvent, PasteResponder, WIRE_PNG};

/// 250 ms: a host copy reaches the client at most this late.
const POLL: Duration = Duration::from_millis(250);
/// Wake rate while a client offer is being fetched, so its bytes land without a poll's delay.
const FETCH_TICK: Duration = Duration::from_millis(20);
/// The coordinator answers every paste within its 60 s fetch timeout; this bounds one that
/// never does.
const OFFER_TIMEOUT: Duration = Duration::from_secs(65);

type ClipTx = tokio::sync::mpsc::UnboundedSender<ClipEvent>;

/// The pasteboard the backend serves.
pub enum Board {
    /// The user's clipboard.
    General,
    /// A private pasteboard by name, released when the backend drops.
    #[cfg_attr(not(test), allow(dead_code))]
    Named(String),
}

enum Cmd {
    SetOffer(Vec<String>),
    Clear,
    Read {
        wire: String,
        resp: tokio::sync::oneshot::Sender<anyhow::Result<Vec<u8>>>,
    },
    Shutdown,
}

/// A client offer being fetched: one paste per carried kind, written once all have answered.
struct Pending {
    deadline: Instant,
    /// Pasteboard type and the channel its bytes arrive on.
    waiting: Vec<(&'static str, mpsc::Receiver<Vec<u8>>)>,
    done: Vec<(&'static str, Vec<u8>)>,
}

/// Everything that touches the pasteboard. `NSPasteboard` is not `Send`, so this is built and
/// used on the backend thread only.
struct Worker {
    pb: Retained<NSPasteboard>,
    named: bool,
    ledger: Ledger,
    clip_tx: ClipTx,
    current_wire: Arc<Mutex<Vec<String>>>,
    pending: Option<Pending>,
}

impl Worker {
    fn declared_wires(&self) -> Vec<String> {
        let types: Vec<String> = self
            .pb
            .types()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default();
        macfmt::wires_for(&types)
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Announces a copy another app made since the last look.
    fn poll(&mut self) {
        if !self.ledger.moved(self.pb.changeCount()) {
            return;
        }
        // A host copy made while the client's offer was being fetched is the newer one.
        self.pending = None;
        let mimes = self.declared_wires();
        *self.current_wire.lock().unwrap() = mimes.clone();
        let _ = self.clip_tx.send(ClipEvent::Selection { mimes });
    }

    /// Starts fetching the carried kinds of `wires`. An offer with none of them clears ours.
    fn set_offer(&mut self, wires: &[String]) {
        self.pending = None;
        let kinds: Vec<_> = macfmt::KINDS
            .iter()
            .filter(|(wire, _)| wires.iter().any(|w| w == wire))
            .collect();
        if kinds.is_empty() {
            self.clear();
            return;
        }
        // A host copy that came just before this offer is announced first; the offer replaces it.
        self.poll();
        let mut waiting = Vec::new();
        for (wire, uti) in kinds {
            let (tx, rx) = mpsc::channel();
            let paste = ClipEvent::Paste {
                mime: wire.to_string(),
                responder: PasteResponder::Sync(tx),
            };
            if self.clip_tx.send(paste).is_ok() {
                waiting.push((*uti, rx));
            }
        }
        self.pending = Some(Pending {
            deadline: Instant::now() + OFFER_TIMEOUT,
            waiting,
            done: Vec::new(),
        });
    }

    /// Collects the fetched bytes and writes them once every kind has answered or the deadline
    /// has passed.
    fn advance(&mut self) {
        let Some(p) = self.pending.as_mut() else {
            return;
        };
        p.waiting.retain(|(uti, rx)| match rx.try_recv() {
            // An empty answer is a failed fetch.
            Ok(bytes) => {
                if !bytes.is_empty() {
                    p.done.push((*uti, bytes));
                }
                false
            }
            Err(TryRecvError::Empty) => true,
            Err(TryRecvError::Disconnected) => false,
        });
        if !p.waiting.is_empty() && Instant::now() < p.deadline {
            return;
        }
        let done = std::mem::take(&mut p.done);
        self.pending = None;
        if done.is_empty() {
            tracing::debug!("clipboard offer fetched nothing");
        } else if self.ledger.unchanged(self.pb.changeCount()) {
            self.write(&done);
        }
    }

    /// Replaces the pasteboard's contents with `items`, one type each.
    fn write(&mut self, items: &[(&'static str, Vec<u8>)]) {
        let types: Vec<Retained<NSString>> = items
            .iter()
            .map(|(uti, _)| NSString::from_str(uti))
            .collect();
        // SAFETY: no owner object is passed, so every declared type gets its data below.
        let count = unsafe {
            self.pb
                .declareTypes_owner(&NSArray::from_retained_slice(&types), None)
        };
        for ((_, bytes), ty) in items.iter().zip(&types) {
            if !self
                .pb
                .setData_forType(Some(&NSData::with_bytes(bytes)), ty)
            {
                tracing::debug!(uti = %ty, "clipboard type refused by the pasteboard");
            }
        }
        self.ledger.wrote(count);
        // Our own offer is not a host selection to announce back.
        self.current_wire.lock().unwrap().clear();
    }

    /// Empties the pasteboard only while it holds our offer, never another app's copy.
    fn clear(&mut self) {
        self.pending = None;
        if self.ledger.is_ours(self.pb.changeCount()) {
            self.ledger.cleared(self.pb.clearContents());
            self.current_wire.lock().unwrap().clear();
        }
    }

    fn read(&self, wire: &str) -> anyhow::Result<Vec<u8>> {
        let uti = macfmt::type_for(wire).context("unsupported wire MIME")?;
        anyhow::ensure!(
            !self.ledger.is_ours(self.pb.changeCount()),
            "clipboard currently held by our own offer"
        );
        let mut data = self.pb.dataForType(&NSString::from_str(uti));
        if data.is_none() && wire == WIRE_PNG {
            data = self
                .pb
                .dataForType(&NSString::from_str(macfmt::TIFF))
                .and_then(|tiff| tiff_to_png(&tiff));
        }
        let bytes = data
            .context("the pasteboard has no data for the type")?
            .to_vec();
        within_cap(bytes.len())?;
        Ok(bytes)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.named && self.pb.respondsToSelector(sel!(releaseGlobally)) {
            // SAFETY: `releaseGlobally` takes no arguments and returns nothing; the handle is
            // not used after this.
            let _: () = unsafe { msg_send![&*self.pb, releaseGlobally] };
        }
    }
}

/// Async-side handle. All pasteboard work runs on the backend thread.
pub struct MacClipboard {
    cmd_tx: mpsc::Sender<Cmd>,
    current_wire: Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MacClipboard {
    pub async fn open(
        board: Board,
    ) -> anyhow::Result<(
        MacClipboard,
        tokio::sync::mpsc::UnboundedReceiver<ClipEvent>,
    )> {
        let (clip_tx, clip_rx) = tokio::sync::mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let current_wire = Arc::new(Mutex::new(Vec::new()));
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let cw = Arc::clone(&current_wire);
        let join = std::thread::Builder::new()
            .name("punktfunk-clipboard-mac".into())
            .spawn(move || run(board, cmd_rx, clip_tx, cw, ready_tx))
            .context("spawn macOS clipboard thread")?;
        match tokio::time::timeout(Duration::from_secs(3), ready_rx).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => anyhow::bail!("macOS clipboard thread exited during bring-up"),
            Err(_) => anyhow::bail!("macOS clipboard bring-up timed out"),
        }
        Ok((
            MacClipboard {
                cmd_tx,
                current_wire,
                join: Some(join),
            },
            clip_rx,
        ))
    }

    pub fn current_wire_mimes(&self) -> Vec<String> {
        self.current_wire.lock().unwrap().clone()
    }

    /// Bytes are fetched from the client through [`ClipEvent::Paste`] before they are written.
    pub fn set_offer(&self, wire_mimes: &[String]) {
        let _ = self.cmd_tx.send(Cmd::SetOffer(wire_mimes.to_vec()));
    }

    pub fn clear_offer(&self) {
        let _ = self.cmd_tx.send(Cmd::Clear);
    }

    pub async fn read_current(&self, wire_mime: &str) -> anyhow::Result<Vec<u8>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.cmd_tx
            .send(Cmd::Read {
                wire: wire_mime.to_string(),
                resp: tx,
            })
            .map_err(|_| anyhow::anyhow!("clipboard thread gone"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("clipboard read dropped"))?
    }
}

impl Drop for MacClipboard {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Cmd::Shutdown);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

fn run(
    board: Board,
    cmd_rx: mpsc::Receiver<Cmd>,
    clip_tx: ClipTx,
    current_wire: Arc<Mutex<Vec<String>>>,
    ready_tx: tokio::sync::oneshot::Sender<()>,
) {
    let (pb, named) = match &board {
        Board::General => (NSPasteboard::generalPasteboard(), false),
        Board::Named(name) => (
            NSPasteboard::pasteboardWithName(&NSString::from_str(name)),
            true,
        ),
    };
    let mut w = Worker {
        ledger: Ledger::new(pb.changeCount()),
        pb,
        named,
        clip_tx,
        current_wire,
        pending: None,
    };
    // Only later copies are announced; the coordinator asks for the current one on enable.
    *w.current_wire.lock().unwrap() = w.declared_wires();
    let _ = ready_tx.send(());

    loop {
        let wait = if w.pending.is_some() {
            FETCH_TICK
        } else {
            POLL
        };
        match cmd_rx.recv_timeout(wait) {
            Ok(Cmd::SetOffer(wires)) => w.set_offer(&wires),
            Ok(Cmd::Clear) => w.clear(),
            Ok(Cmd::Read { wire, resp }) => {
                let _ = resp.send(w.read(&wire));
            }
            Ok(Cmd::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        w.poll();
        w.advance();
    }
    // Do not leave our offer on the user's clipboard after the session ends.
    w.clear();
    let _ = w.clip_tx.send(ClipEvent::Closed);
}

/// AppKit's own `NSImage` writes TIFF alone; a conversion offers it as the PNG the wire carries.
fn tiff_to_png(tiff: &NSData) -> Option<Retained<NSData>> {
    let rep = NSBitmapImageRep::imageRepWithData(tiff)?;
    // SAFETY: the properties dictionary is empty, so no value has a type to get wrong.
    unsafe {
        rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
    }
}

/// A selection over [`CLIP_FETCH_CAP`] is an error, as on the other backends, never a truncation.
fn within_cap(len: usize) -> anyhow::Result<()> {
    anyhow::ensure!(
        len <= CLIP_FETCH_CAP,
        "clipboard selection exceeds the {CLIP_FETCH_CAP}-byte transfer cap"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use tokio::sync::mpsc::error::TryRecvError as RxEmpty;

    use super::*;
    use crate::host::{WIRE_GIF, WIRE_HTML, WIRE_JPEG, WIRE_RTF, WIRE_TEXT};

    type Rx = tokio::sync::mpsc::UnboundedReceiver<ClipEvent>;

    const TEXT: &str = "public.utf8-plain-text";

    /// A name no other test or process uses. Tests never touch the general pasteboard.
    fn private_name() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "io.unom.punktfunk.clipboard-test.{}.{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// The backend on a private pasteboard, which it releases when it drops, and a second
    /// handle on that board: a test's "other app".
    async fn open_private() -> (MacClipboard, Rx, Retained<NSPasteboard>) {
        let name = private_name();
        let app = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
        let (clip, rx) = MacClipboard::open(Board::Named(name)).await.unwrap();
        (clip, rx, app)
    }

    /// What another app does on a copy: declare the types, then hand over their bytes.
    fn copy(app: &NSPasteboard, items: &[(&str, &[u8])]) {
        let types: Vec<_> = items.iter().map(|(t, _)| NSString::from_str(t)).collect();
        // SAFETY: no owner object is passed; each type gets its data right below.
        unsafe { app.declareTypes_owner(&NSArray::from_retained_slice(&types), None) };
        for ((_, bytes), ty) in items.iter().zip(&types) {
            assert!(app.setData_forType(Some(&NSData::with_bytes(bytes)), ty));
        }
    }

    fn held(app: &NSPasteboard, uti: &str) -> Option<Vec<u8>> {
        app.dataForType(&NSString::from_str(uti))
            .map(|d| d.to_vec())
    }

    async fn until(what: &str, mut ok: impl FnMut() -> bool) {
        for _ in 0..250 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn next(rx: &mut Rx) -> ClipEvent {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("an event within 5 s")
            .expect("the backend is alive")
    }

    async fn selection(rx: &mut Rx) -> Vec<String> {
        match next(rx).await {
            ClipEvent::Selection { mimes } => mimes,
            _ => panic!("expected a Selection"),
        }
    }

    async fn paste(rx: &mut Rx) -> (String, PasteResponder) {
        match next(rx).await {
            ClipEvent::Paste { mime, responder } => (mime, responder),
            _ => panic!("expected a Paste"),
        }
    }

    /// Long enough for several polls, so an echo or an overwrite would have happened.
    async fn settle() {
        tokio::time::sleep(POLL * 3).await;
    }

    /// A 2x2 PNG, a real image for the conversion path.
    fn png() -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::new_rgb8(2, 2)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[tokio::test]
    async fn every_kind_a_host_app_copies_reads_back_verbatim() {
        let (clip, mut rx, app) = open_private().await;
        let png = png();
        let kinds: [(&str, &str, &[u8]); 6] = [
            (WIRE_TEXT, TEXT, "héllo ✓".as_bytes()),
            (WIRE_HTML, "public.html", b"<b>hi</b>"),
            (WIRE_RTF, "public.rtf", br"{\rtf1 hi}"),
            (WIRE_PNG, "public.png", &png),
            (WIRE_JPEG, "public.jpeg", b"\xff\xd8jpeg-ish"),
            (WIRE_GIF, "com.compuserve.gif", b"GIF89a-ish"),
        ];
        for (wire, uti, bytes) in kinds {
            copy(&app, &[(uti, bytes)]);
            let mimes = selection(&mut rx).await;
            assert!(mimes.iter().any(|m| m == wire), "{wire} not in {mimes:?}");
            assert_eq!(clip.current_wire_mimes(), mimes);
            assert_eq!(clip.read_current(wire).await.unwrap(), bytes, "{wire}");
        }
    }

    #[tokio::test]
    async fn a_copy_made_before_open_is_current_but_not_announced() {
        let name = private_name();
        let app = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
        copy(&app, &[(TEXT, b"already here")]);
        let (clip, mut rx) = MacClipboard::open(Board::Named(name)).await.unwrap();
        assert_eq!(clip.current_wire_mimes(), vec![WIRE_TEXT]);
        settle().await;
        assert!(matches!(rx.try_recv(), Err(RxEmpty::Empty)));
    }

    #[tokio::test]
    async fn a_client_offer_lands_once_fetched_and_never_echoes() {
        let (clip, mut rx, app) = open_private().await;
        // WebP is not a carried kind: it is dropped, not fetched.
        clip.set_offer(&[WIRE_HTML.into(), "image/webp".into(), WIRE_TEXT.into()]);
        let (first, text) = paste(&mut rx).await;
        let (second, html) = paste(&mut rx).await;
        assert_eq!((first.as_str(), second.as_str()), (WIRE_TEXT, WIRE_HTML));
        assert_eq!(held(&app, TEXT), None, "written before the fetch");
        text.respond(b"from the client".to_vec()).await;
        html.respond(b"<i>from the client</i>".to_vec()).await;

        until("the offer to land", || held(&app, "public.html").is_some()).await;
        assert_eq!(held(&app, TEXT).unwrap(), b"from the client");
        assert_eq!(
            held(&app, "public.html").unwrap(),
            b"<i>from the client</i>"
        );
        settle().await;
        assert!(
            matches!(rx.try_recv(), Err(RxEmpty::Empty)),
            "own write echoed"
        );
        assert!(clip.current_wire_mimes().is_empty());
        assert!(
            clip.read_current(WIRE_TEXT).await.is_err(),
            "own offer read back"
        );
    }

    #[tokio::test]
    async fn a_host_copy_during_the_fetch_wins() {
        let (clip, mut rx, app) = open_private().await;
        clip.set_offer(&[WIRE_TEXT.into()]);
        let (_, responder) = paste(&mut rx).await;
        copy(&app, &[(TEXT, b"host copy")]);
        assert_eq!(selection(&mut rx).await, vec![WIRE_TEXT]);
        responder.respond(b"late client copy".to_vec()).await;
        settle().await;
        assert_eq!(held(&app, TEXT).unwrap(), b"host copy");
    }

    #[tokio::test]
    async fn a_failed_fetch_leaves_the_clipboard_alone() {
        let (clip, mut rx, app) = open_private().await;
        copy(&app, &[(TEXT, b"keep me")]);
        assert_eq!(selection(&mut rx).await, vec![WIRE_TEXT]);
        clip.set_offer(&[WIRE_TEXT.into()]);
        let (_, responder) = paste(&mut rx).await;
        responder.respond(Vec::new()).await;
        settle().await;
        assert_eq!(held(&app, TEXT).unwrap(), b"keep me");
    }

    #[tokio::test]
    async fn clear_offer_removes_our_offer_and_only_ours() {
        let (clip, mut rx, app) = open_private().await;
        clip.set_offer(&[WIRE_TEXT.into()]);
        let (_, responder) = paste(&mut rx).await;
        responder.respond(b"from the client".to_vec()).await;
        until("the offer to land", || held(&app, TEXT).is_some()).await;

        clip.clear_offer();
        until("the offer to clear", || held(&app, TEXT).is_none()).await;
        settle().await;
        assert!(
            matches!(rx.try_recv(), Err(RxEmpty::Empty)),
            "own clear echoed"
        );

        copy(&app, &[(TEXT, b"mine")]);
        assert_eq!(selection(&mut rx).await, vec![WIRE_TEXT]);
        clip.clear_offer();
        settle().await;
        assert_eq!(
            held(&app, TEXT).unwrap(),
            b"mine",
            "a host copy was cleared"
        );
    }

    #[tokio::test]
    async fn dropping_the_backend_takes_its_offer_with_it() {
        let (clip, mut rx, app) = open_private().await;
        clip.set_offer(&[WIRE_TEXT.into()]);
        let (_, responder) = paste(&mut rx).await;
        responder.respond(b"from the client".to_vec()).await;
        until("the offer to land", || held(&app, TEXT).is_some()).await;
        drop(clip);
        assert_eq!(held(&app, TEXT), None);
        assert!(matches!(next(&mut rx).await, ClipEvent::Closed));
    }

    #[tokio::test]
    async fn a_tiff_only_copy_reads_as_png() {
        let (clip, mut rx, app) = open_private().await;
        copy(&app, &[("public.png", &png())]);
        selection(&mut rx).await;
        // AppKit hands back a TIFF for a PNG copy; a TIFF-only source is what NSImage writes.
        let tiff = held(&app, macfmt::TIFF).expect("AppKit synthesizes a TIFF");
        copy(&app, &[(macfmt::TIFF, &tiff)]);
        assert_eq!(selection(&mut rx).await, vec![WIRE_PNG]);
        let out = clip.read_current(WIRE_PNG).await.unwrap();
        assert!(out.starts_with(b"\x89PNG"), "not a PNG: {:02x?}", &out[..8]);
    }

    #[test]
    fn a_selection_over_the_cap_is_an_error() {
        assert!(within_cap(CLIP_FETCH_CAP).is_ok());
        assert!(within_cap(CLIP_FETCH_CAP + 1).is_err());
    }
}
