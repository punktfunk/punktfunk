# OS icon masters

The canonical OS/distro brand marks every client derives its host-card OS icon from
(web console inline SVGs, GTK symbolic icons, Windows PNGs, Apple template imagesets,
Android `ImageVector`s). One file per **icon token** of the host's OS-identity chain
(see `crates/host/punktfunk-host/src/hostsys/osinfo.rs` and `crates/client/pf-client-core/src/os.rs`):

| token | mark | source |
|---|---|---|
| `windows` | Windows (the current four-pane mark, no perspective skew) | own geometry |
| `apple` | Apple (also `macos` via alias) | Font Awesome Free brands (CC BY 4.0) |
| `linux` | Tux | Font Awesome Free brands (CC BY 4.0) |
| `steam` | Steam (also `steamos` via alias) | Font Awesome Free brands (CC BY 4.0) |
| `ubuntu` | Ubuntu | Font Awesome Free brands (CC BY 4.0) |
| `fedora` | Fedora | Font Awesome Free brands (CC BY 4.0) |
| `opensuse` | SUSE | Font Awesome Free brands (CC BY 4.0) |
| `arch` | Arch Linux | Simple Icons (CC0 1.0) |
| `nixos` | NixOS | Simple Icons (CC0 1.0) |
| `debian` | Debian | Simple Icons (CC0 1.0) |
| `bazzite` | Bazzite | ublue-os/bazzite (Apache-2.0) |
| `cachyos` | CachyOS | Simple Icons (CC0 1.0) |
| `nobara` | Nobara | Simple Icons (CC0 1.0, slug `nobaralinux`) |
| `omarchy` | Omarchy | Simple Icons (CC0 1.0) |

The last four are **distro leaves, not families**: a chain walks most-specific-first, so
`linux/fedora/bazzite` would otherwise draw the Fedora mark, and `linux/arch/omarchy` the Arch
one. They earn their own art because "a Bazzite box" and "a Fedora box" are different machines
to the person reading the card, and they are what this project's hosts actually run. Every other
distro with no file here (Pop!_OS, Mint, …) still degrades to its family's mark and finally to
Tux — that fallback is the design, not a gap.

Windows is the one mark drawn here rather than sourced: every icon set that ships a "Windows"
brand glyph still carries the **Windows 8/10 flag with the perspective skew**, which reads as
dated next to the flat four-pane mark Microsoft has used since Windows 11. Four equal squares
(11.377 + 1.246 gap) is the current proportion.

All files are monochrome (`fill="currentColor"`), original per-icon viewBoxes preserved. Because
those viewBoxes are not all square, a client must letterbox rather than stretch — see the aspect
note in `clients/android/.../components/OsIcons.kt`.

## Regenerating the per-client derivatives

`bash scripts/gen-os-icons.sh [token ...]` turns a master into the three baked forms (GTK
symbolic SVG, Windows PNG, Apple template PDF) and regenerates the path registries of the four
clients that inline it (Skia console, web console, Decky plugin, Android). Adding a **new** token
also means adding it to each baked client's shipped-token list — the script prints that checklist
too.

## Licensing

Attribution notices live in `LICENSES/` and are folded into `THIRD-PARTY-NOTICES.txt` by
`scripts/gen-third-party-notices.py`. The marks are trademarks of their respective owners; they
are used here nominatively — to *identify* the operating system a host runs, the standard
practice in this ecosystem — and imply no affiliation or endorsement.
