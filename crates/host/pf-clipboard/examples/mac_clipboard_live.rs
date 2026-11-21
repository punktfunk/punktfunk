//! Drives the macOS clipboard backend against the real general pasteboard, then puts the
//! user's clipboard back.
//!
//! `check` saves every item and type on the general pasteboard to a private directory, opens the
//! production backend, copies and offers text and images through it, prints one line per
//! check, then restores the saved clipboard and verifies it by reading it back. The save
//! directory is deleted only after a verified restore; `restore <dir>` puts a left-over one
//! back. `peek` prints the pasteboard's access setting and type names and reads no data.
//!
//! Reading another app's clipboard from a process can raise the system's paste-access prompt.
//! Run it only while nobody streams to this Mac, and never in a loop.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mac_clipboard_live runs on macOS");
}

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    live::main()
}

#[cfg(target_os = "macos")]
mod live {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use anyhow::Context as _;
    use objc2::rc::Retained;
    use objc2::runtime::ProtocolObject;
    use objc2::AnyThread as _;
    use objc2_app_kit::{NSImage, NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
    use objc2_foundation::{NSArray, NSAttributedString, NSData, NSString};

    use pf_clipboard::host::{ClipEvent, HostClipboard};
    use pf_clipboard::ClipTarget;

    const WIRE_TEXT: &str = "text/plain;charset=utf-8";
    const WIRE_PNG: &str = "image/png";

    pub fn main() -> anyhow::Result<()> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
            ["peek"] => peek(),
            ["restore", dir] => restore_dir(Path::new(dir)),
            ["write", kind] => write_as_an_app(kind),
            ["check"] => check(),
            _ => anyhow::bail!("usage: mac_clipboard_live peek | check | restore <dir>"),
        }
    }

    type Item = Vec<(String, Option<Vec<u8>>)>;

    /// Every item of a pasteboard with every type's bytes. `None` is a type whose data the
    /// pasteboard would not hand over.
    struct Snapshot(Vec<Item>);

    impl Snapshot {
        fn take(pb: &NSPasteboard) -> Snapshot {
            let items = pb.pasteboardItems().map(|a| a.to_vec()).unwrap_or_default();
            Snapshot(
                items
                    .iter()
                    .map(|item| {
                        item.types()
                            .iter()
                            .map(|t| {
                                let data = item.dataForType(&t).map(|d| d.to_vec());
                                (t.to_string(), data)
                            })
                            .collect()
                    })
                    .collect(),
            )
        }

        fn describe(&self) -> String {
            let mut out = String::new();
            for (i, item) in self.0.iter().enumerate() {
                out += &format!("  item {i}:\n");
                for (t, d) in item {
                    match d {
                        Some(d) => out += &format!("    {t} ({} bytes)\n", d.len()),
                        None => out += &format!("    {t} (no data)\n"),
                    }
                }
            }
            if self.0.is_empty() {
                out += "  (empty)\n";
            }
            out
        }

        fn put_back(&self, pb: &NSPasteboard) {
            pb.clearContents();
            let items: Vec<Retained<ProtocolObject<dyn NSPasteboardWriting>>> = self
                .0
                .iter()
                .map(|types| {
                    let item = NSPasteboardItem::new();
                    for (t, d) in types {
                        if let Some(d) = d {
                            item.setData_forType(&NSData::with_bytes(d), &NSString::from_str(t));
                        }
                    }
                    ProtocolObject::from_retained(item)
                })
                .collect();
            if !items.is_empty() {
                pb.writeObjects(&NSArray::from_retained_slice(&items));
            }
        }

        /// What `put_back` did not reproduce: a saved type with other bytes, or none.
        fn diff_against(&self, now: &Snapshot) -> Vec<String> {
            let mut bad = Vec::new();
            if self.0.len() != now.0.len() {
                bad.push(format!("{} items saved, {} now", self.0.len(), now.0.len()));
            }
            for (i, item) in self.0.iter().enumerate() {
                for (t, d) in item {
                    let Some(d) = d else { continue };
                    let got = now
                        .0
                        .get(i)
                        .and_then(|it| it.iter().find(|(nt, _)| nt == t))
                        .and_then(|(_, nd)| nd.as_ref());
                    if got != Some(d) {
                        bad.push(format!("item {i} {t}: not restored byte for byte"));
                    }
                }
            }
            bad
        }

        fn save(&self, dir: &Path) -> anyhow::Result<()> {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new().mode(0o700).create(dir)?;
            for (i, item) in self.0.iter().enumerate() {
                for (j, (t, d)) in item.iter().enumerate() {
                    std::fs::write(dir.join(format!("{i}-{j}.type")), t)?;
                    if let Some(d) = d {
                        std::fs::write(dir.join(format!("{i}-{j}.data")), d)?;
                    }
                }
            }
            Ok(())
        }

        fn load(dir: &Path) -> anyhow::Result<Snapshot> {
            let mut files: Vec<(usize, usize, String)> = Vec::new();
            for entry in std::fs::read_dir(dir)? {
                let name = entry?.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".type") else {
                    continue;
                };
                let (i, j) = stem.split_once('-').context("save file name")?;
                files.push((i.parse()?, j.parse()?, name));
            }
            files.sort();
            let mut items: Vec<Item> = Vec::new();
            for (i, j, name) in files {
                while items.len() <= i {
                    items.push(Vec::new());
                }
                let t = std::fs::read_to_string(dir.join(name))?;
                let data = std::fs::read(dir.join(format!("{i}-{j}.data"))).ok();
                items[i].push((t, data));
            }
            Ok(Snapshot(items))
        }
    }

    fn general() -> Retained<NSPasteboard> {
        NSPasteboard::generalPasteboard()
    }

    fn peek() -> anyhow::Result<()> {
        let pb = general();
        println!("access behavior: {:?}", pb.accessBehavior());
        println!("change count: {}", pb.changeCount());
        println!("types: {:?}", pb.types().map(|t| t.to_vec()));
        Ok(())
    }

    fn restore_dir(dir: &Path) -> anyhow::Result<()> {
        let saved = Snapshot::load(dir)?;
        saved.put_back(&general());
        let bad = saved.diff_against(&Snapshot::take(&general()));
        anyhow::ensure!(bad.is_empty(), "restore differs: {bad:?}");
        std::fs::remove_dir_all(dir)?;
        println!("restored and verified; save directory removed");
        Ok(())
    }

    /// Writes what a real app writes, from this separate process.
    fn write_as_an_app(kind: &str) -> anyhow::Result<()> {
        let pb = general();
        match kind {
            // A bare PNG, as a screenshot tool writes.
            "png" => {
                pb.clearContents();
                pb.setData_forType(
                    Some(&NSData::with_bytes(&png())),
                    &NSString::from_str("public.png"),
                );
            }
            // What `-[NSPasteboard writeObjects:]` does with an `NSImage`.
            "image" => {
                let img = NSImage::initWithData(NSImage::alloc(), &NSData::with_bytes(&png()))
                    .context("decode the image")?;
                pb.clearContents();
                anyhow::ensure!(pb.writeObjects(&NSArray::from_retained_slice(&[
                    ProtocolObject::from_retained(img)
                ])));
            }
            // What a Cocoa app's plain-text copy does.
            "string" => {
                pb.clearContents();
                anyhow::ensure!(pb.writeObjects(&NSArray::from_retained_slice(&[
                    ProtocolObject::from_retained(NSString::from_str("cocoa copy"))
                ])));
            }
            // A password manager's copy: the text, marked concealed (nspasteboard.org).
            "secret" => {
                pb.clearContents();
                pb.setData_forType(
                    Some(&NSData::with_bytes(b"pf-live secret")),
                    &NSString::from_str("public.utf8-plain-text"),
                );
                pb.setData_forType(
                    Some(&NSData::with_bytes(b"1")),
                    &NSString::from_str("org.nspasteboard.ConcealedType"),
                );
            }
            // What a rich-text editor's copy does.
            "rich" => {
                let text = NSAttributedString::from_nsstring(&NSString::from_str("rich copy"));
                pb.clearContents();
                anyhow::ensure!(pb.writeObjects(&NSArray::from_retained_slice(&[
                    ProtocolObject::from_retained(text)
                ])));
            }
            _ => anyhow::bail!("unknown kind {kind}"),
        }
        Ok(())
    }

    /// A small deterministic image, so a copy reads back comparably.
    fn png() -> Vec<u8> {
        let mut img = image::RgbaImage::new(16, 16);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgba([(x * 16) as u8, (y * 16) as u8, 128, 255]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encode a PNG");
        out
    }

    fn pbcopy(text: &str) -> anyhow::Result<()> {
        let mut child = Command::new("pbcopy")
            .env("LANG", "en_US.UTF-8")
            .stdin(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("pbcopy stdin")?
            .write_all(text.as_bytes())?;
        anyhow::ensure!(child.wait()?.success(), "pbcopy failed");
        Ok(())
    }

    fn pbpaste() -> anyhow::Result<String> {
        let out = Command::new("pbpaste")
            .env("LANG", "en_US.UTF-8")
            .output()?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn write_in_a_child(kind: &str) -> anyhow::Result<()> {
        let status = Command::new(std::env::current_exe()?)
            .args(["write", kind])
            .status()?;
        anyhow::ensure!(status.success(), "writer {kind} failed");
        Ok(())
    }

    fn check() -> anyhow::Result<()> {
        let pb = general();
        println!("access behavior: {:?}", pb.accessBehavior());
        let saved = Arc::new(Snapshot::take(&pb));
        let types_before = types_now();
        print!("saved the clipboard:\n{}", saved.describe());
        let unreadable = saved
            .0
            .iter()
            .flatten()
            .filter(|(_, d)| d.is_none())
            .count();
        println!("types that would not hand over data: {unreadable}");
        let dir: PathBuf =
            std::env::temp_dir().join(format!("pf-clipboard-live-{}", std::process::id()));
        saved.save(&dir)?;

        // A read stuck on a prompt must not leave the clipboard changed.
        let on_hang = Arc::clone(&saved);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(30));
            on_hang.put_back(&general());
            eprintln!("watchdog: the run hung; the clipboard was put back");
            std::process::exit(3);
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(run_checks())
        }))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("a check panicked")));

        saved.put_back(&general());
        let bad = saved.diff_against(&Snapshot::take(&general()));
        let types_after = types_now();
        if bad.is_empty() {
            std::fs::remove_dir_all(&dir)?;
            println!(
                "clipboard restored and verified byte for byte; type list unchanged: {}",
                types_after == types_before
            );
            if types_after != types_before {
                println!("  before {types_before:?}\n  after  {types_after:?}");
            }
        } else {
            println!(
                "RESTORE DIFFERS: {bad:?}; saved copy kept in {}",
                dir.display()
            );
        }
        result
    }

    type Rx = tokio::sync::mpsc::UnboundedReceiver<ClipEvent>;

    async fn event_within(rx: &mut Rx, within: Duration) -> Option<ClipEvent> {
        tokio::time::timeout(within, rx.recv()).await.ok().flatten()
    }

    async fn event(rx: &mut Rx) -> Option<ClipEvent> {
        event_within(rx, Duration::from_secs(3)).await
    }

    async fn selection(rx: &mut Rx) -> Option<Vec<String>> {
        match event(rx).await {
            Some(ClipEvent::Selection { mimes }) => Some(mimes),
            _ => None,
        }
    }

    fn report(ok: bool, what: &str) {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
    }

    fn types_now() -> Vec<String> {
        general()
            .types()
            .map(|t| t.iter().map(|s| s.to_string()).collect())
            .unwrap_or_default()
    }

    async fn run_checks() -> anyhow::Result<()> {
        let (clip, mut rx) = HostClipboard::open(ClipTarget::Session).await?;
        let clip = Arc::new(clip);
        println!("opened; offering {:?}", clip.current_wire_mimes());

        // 2. a host copy is offered and reads back: text from a stock app, then images.
        let started = Instant::now();
        pbcopy("pf-live host text ✓")?;
        let mimes = selection(&mut rx).await;
        let latency = started.elapsed();
        report(
            mimes
                .as_ref()
                .is_some_and(|m| m.iter().any(|w| w == WIRE_TEXT)),
            &format!(
                "pbcopy text offered {mimes:?} after {latency:?}; pasteboard types {:?}",
                types_now()
            ),
        );
        let got = tokio::time::timeout(Duration::from_secs(5), clip.read_current(WIRE_TEXT)).await;
        report(
            matches!(&got, Ok(Ok(b)) if b == "pf-live host text ✓".as_bytes()),
            &format!(
                "text reads back: {:?}",
                got.as_ref().map(|r| r.as_ref().map(Vec::len))
            ),
        );

        for kind in ["png", "image"] {
            write_in_a_child(kind)?;
            let mimes = selection(&mut rx).await;
            report(
                mimes
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|w| w == WIRE_PNG)),
                &format!(
                    "{kind} copy offered {mimes:?}; pasteboard types {:?}",
                    types_now()
                ),
            );
            let got =
                tokio::time::timeout(Duration::from_secs(5), clip.read_current(WIRE_PNG)).await;
            match got {
                Ok(Ok(b)) => report(
                    b.starts_with(b"\x89PNG"),
                    &format!(
                        "{kind} reads back as PNG, {} bytes (same as written: {})",
                        b.len(),
                        b == png()
                    ),
                ),
                other => report(
                    false,
                    &format!("{kind} read: {:?}", other.map(|r| r.map(|b| b.len()))),
                ),
            }
        }
        for kind in ["string", "rich"] {
            write_in_a_child(kind)?;
            let mimes = selection(&mut rx).await;
            report(
                mimes
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|w| w == WIRE_TEXT)),
                &format!(
                    "{kind} copy offered {mimes:?}; pasteboard types {:?}",
                    types_now()
                ),
            );
        }
        write_in_a_child("secret")?;
        let mimes = selection(&mut rx).await;
        report(
            mimes.as_ref().is_some_and(Vec::is_empty),
            &format!(
                "a concealed copy offers nothing: {mimes:?}; pasteboard types {:?}",
                types_now()
            ),
        );
        report(
            clip.read_current(WIRE_TEXT).await.is_err(),
            "a concealed copy is not read",
        );

        // 3. a client offer lands and another process sees it.
        let started = Instant::now();
        clip.set_offer(&[WIRE_TEXT.to_string(), WIRE_PNG.to_string()])?;
        for _ in 0..2 {
            match event(&mut rx).await {
                Some(ClipEvent::Paste { mime, responder }) => {
                    let bytes = if mime == WIRE_PNG {
                        png()
                    } else {
                        "pf-live client text".as_bytes().to_vec()
                    };
                    responder.respond(bytes).await;
                }
                _ => report(false, "offer asked the client for nothing"),
            }
        }
        let mut seen = String::new();
        while started.elapsed() < Duration::from_secs(3) && seen != "pf-live client text" {
            tokio::time::sleep(Duration::from_millis(20)).await;
            seen = pbpaste()?;
        }
        report(
            seen == "pf-live client text",
            &format!(
                "pbpaste sees the client's text after {:?}; pasteboard types {:?}",
                started.elapsed(),
                types_now()
            ),
        );
        let png_back = general().dataForType(&NSString::from_str("public.png"));
        report(
            png_back.is_some_and(|d| d.to_vec() == png()),
            "the client's PNG is on the pasteboard byte for byte",
        );

        // 4. no echo, and a user copy while a fetch runs wins.
        let echo = event_within(&mut rx, Duration::from_secs(1)).await;
        report(echo.is_none(), "no Selection echoes our own write");
        report(
            clip.read_current(WIRE_TEXT).await.is_err(),
            "our own offer is not read back",
        );

        clip.set_offer(&[WIRE_TEXT.to_string()])?;
        let responder = match event(&mut rx).await {
            Some(ClipEvent::Paste { responder, .. }) => Some(responder),
            _ => None,
        };
        pbcopy("pf-live user copy")?;
        let mimes = selection(&mut rx).await;
        report(mimes.is_some(), "the user's copy is announced");
        if let Some(r) = responder {
            r.respond("pf-live late client text".as_bytes().to_vec())
                .await;
        } else {
            report(false, "the second offer fetched nothing");
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        report(
            pbpaste()? == "pf-live user copy",
            "the user's copy wins over the late fetch",
        );
        Ok(())
    }
}
