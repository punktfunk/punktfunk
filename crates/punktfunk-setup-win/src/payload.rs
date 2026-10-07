//! What the D3 sandwich carries: one tar, xz-compressed behind the x86 BCJ filter (S2
//! measured xz 40 % under zstd on the installed tree; BCJ is this WP's extra on the exes and
//! DLLs), holding up to three trees plus a manifest:
//!
//!   runtime/       the self-contained WinAppSDK set the wizard runs from (always)
//!   app/           what lands in `{app}` (the installer)
//!   staging/       driver payloads the plan hands `driver install --dir <staging>\…`
//!   manifest.json  `{ artifact, version, uninstaller }` — decides the mode (D1, D6)
//!
//! Both directions stream through the compressor, so a 500 MB tree never sits in memory.
//! `extract` relies on the tar crate's path checks (no `..`, no absolute paths), so a crafted
//! archive cannot escape the extract dir; the footer sha (`overlay`) plus the Authenticode
//! signature over the whole file are what make the bytes trusted in the first place.

use std::io::Write;
use std::path::Path;

use punktfunk_setup::platform::windows::plan::Artifact;
use serde::{Deserialize, Serialize};

pub const MANIFEST: &str = "manifest.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub artifact: Artifact,
    pub version: String,
    /// D6: the payload-less `unins000.exe` — runtime only, teardown path only.
    pub uninstaller: bool,
}

/// The trees to pack; `None` is simply absent from the archive.
pub struct Trees<'a> {
    pub runtime: &'a Path,
    pub app: Option<&'a Path>,
    pub staging: Option<&'a Path>,
}

pub fn build(trees: &Trees, manifest: &Manifest) -> Result<Vec<u8>, String> {
    use xz2::stream::{Check, Filters, LzmaOptions, MtStreamBuilder};
    let mut filters = Filters::new();
    filters.x86();
    filters.lzma2(&LzmaOptions::new_preset(9).map_err(|e| e.to_string())?);
    // One preset-9 dictionary (64 MiB) per block: the 216 MB host tree packs 2.6× faster on
    // 8 threads for 1.2 % more bytes. Still one xz stream, so `XzDecoder` reads it unchanged.
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    let stream = MtStreamBuilder::new()
        .threads(threads)
        .block_size(64 << 20)
        .filters(filters)
        .check(Check::Crc64)
        .encoder()
        .map_err(|e| e.to_string())?;
    let mut tar = tar::Builder::new(xz2::write::XzEncoder::new_stream(Vec::new(), stream));
    tar.follow_symlinks(false);
    append(&mut tar, "runtime", trees.runtime)?;
    if let Some(app) = trees.app {
        append(&mut tar, "app", app)?;
    }
    if let Some(staging) = trees.staging {
        append(&mut tar, "staging", staging)?;
    }
    // Last on purpose: `inspect` stops streaming as soon as it has read it.
    let json = serde_json::to_vec_pretty(manifest).map_err(|e| e.to_string())?;
    let mut header = tar::Header::new_gnu();
    header.set_size(json.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    tar.append_data(&mut header, MANIFEST, json.as_slice())
        .map_err(|e| e.to_string())?;
    let mut enc = tar.into_inner().map_err(|e| e.to_string())?;
    enc.flush().map_err(|e| e.to_string())?;
    enc.finish().map_err(|e| e.to_string())
}

fn append<W: Write>(tar: &mut tar::Builder<W>, name: &str, dir: &Path) -> Result<(), String> {
    tar.append_dir_all(name, dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

/// Unpack into `into` (created as needed) and return the manifest.
pub fn extract(payload: &[u8], into: &Path) -> Result<Manifest, String> {
    let mut archive = tar::Archive::new(xz2::read::XzDecoder::new(payload));
    archive
        .unpack(into)
        .map_err(|e| format!("unpack into {}: {e}", into.display()))?;
    manifest_at(into)
}

pub fn manifest_at(root: &Path) -> Result<Manifest, String> {
    let text =
        std::fs::read_to_string(root.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{MANIFEST}: {e}"))
}

/// Read the manifest and count entries without touching the disk.
pub fn peek(payload: &[u8]) -> Result<(Manifest, usize), String> {
    use std::io::Read;
    let mut archive = tar::Archive::new(xz2::read::XzDecoder::new(payload));
    let mut count = 0;
    for entry in archive.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        count += 1;
        if entry.path().map_err(|e| e.to_string())?.as_os_str() == MANIFEST {
            let mut text = String::new();
            entry.read_to_string(&mut text).map_err(|e| e.to_string())?;
            let manifest = serde_json::from_str(&text).map_err(|e| format!("{MANIFEST}: {e}"))?;
            return Ok((manifest, count));
        }
    }
    Err(format!("no {MANIFEST} in the payload"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(root: &Path, files: &[(&str, &[u8])]) {
        for (rel, bytes) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
    }

    fn manifest() -> Manifest {
        Manifest {
            artifact: Artifact::Host,
            version: "0.35.0".into(),
            uninstaller: false,
        }
    }

    #[test]
    fn the_three_trees_and_the_manifest_round_trip() {
        let src = tempfile::tempdir().unwrap();
        tree(
            src.path(),
            &[
                ("runtime/microsoft.ui.xaml.dll", b"xaml"),
                ("runtime/en-us/microsoft.ui.xaml.dll.mui", b"mui"),
                ("app/punktfunk-host.exe", &[0x4d, 0x5a, 0, 1, 2, 3]),
                ("app/web/.output/index.mjs", b"export {}"),
                ("staging/pfvdisplay/pf_vdisplay.inf", b"[Version]"),
                ("app/staging/pfvdisplay/pf_vdisplay_seats.inf", b"[Version]"),
            ],
        );
        let payload = build(
            &Trees {
                runtime: &src.path().join("runtime"),
                app: Some(&src.path().join("app")),
                staging: Some(&src.path().join("staging")),
            },
            &manifest(),
        )
        .unwrap();

        let (peeked, entries) = peek(&payload).unwrap();
        assert_eq!(peeked, manifest());
        assert!(entries > 5, "{entries}");

        let dst = tempfile::tempdir().unwrap();
        assert_eq!(extract(&payload, dst.path()).unwrap(), manifest());
        for (rel, bytes) in [
            ("runtime/microsoft.ui.xaml.dll", &b"xaml"[..]),
            ("runtime/en-us/microsoft.ui.xaml.dll.mui", b"mui"),
            ("app/web/.output/index.mjs", b"export {}"),
            ("staging/pfvdisplay/pf_vdisplay.inf", b"[Version]"),
            ("app/staging/pfvdisplay/pf_vdisplay_seats.inf", b"[Version]"),
        ] {
            assert_eq!(std::fs::read(dst.path().join(rel)).unwrap(), bytes, "{rel}");
        }
    }

    // D6: the uninstaller carries the runtime and the manifest, nothing else.
    #[test]
    fn a_runtime_only_payload_has_no_app_tree() {
        let src = tempfile::tempdir().unwrap();
        tree(src.path(), &[("runtime/microsoft.ui.dll", b"ui")]);
        let payload = build(
            &Trees {
                runtime: &src.path().join("runtime"),
                app: None,
                staging: None,
            },
            &Manifest {
                uninstaller: true,
                ..manifest()
            },
        )
        .unwrap();
        let dst = tempfile::tempdir().unwrap();
        let m = extract(&payload, dst.path()).unwrap();
        assert!(m.uninstaller);
        assert!(dst.path().join("runtime/microsoft.ui.dll").is_file());
        assert!(!dst.path().join("app").exists());
    }

    #[test]
    fn a_truncated_payload_is_an_error_not_a_partial_tree() {
        let src = tempfile::tempdir().unwrap();
        tree(src.path(), &[("runtime/a.dll", &[7u8; 4096])]);
        let payload = build(
            &Trees {
                runtime: &src.path().join("runtime"),
                app: None,
                staging: None,
            },
            &manifest(),
        )
        .unwrap();
        let dst = tempfile::tempdir().unwrap();
        assert!(extract(&payload[..payload.len() / 2], dst.path()).is_err());
    }
}
