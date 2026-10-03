# Vendored & bundled components — CVE watch and update cadence

Due-diligence record for every third-party component that ships with Punktfunk but is
**not** tracked by a package manager's advisory feed (CRA Art. 13(5); Annex I Part II §1).
Everything resolved through Cargo/bun/pnpm lockfiles is already scanned weekly by
`.gitea/workflows/audit.yml` (cargo-audit against RustSec, bun/pnpm audit) — this file
covers what those scanners cannot see: vendored source trees, git-rev pins, and binaries
staged into installers. The component inventory itself lives in
`compliance/sbom/manual-components.cdx.json` and is merged into every release SBOM;
keep the two files in sync when a component is added, removed, or re-pinned. audit.yml
fails when a `vendor/` tree under `crates/`, `clients/` or `packaging/` has no row here.

Owner for all of it: Enrico (sole maintainer). Standing cadence: **walk this table once
per quarter and before every stable release**; act immediately on any advisory from the
watch feeds below.

| Component | Where / pin | How to update | Watch |
|---|---|---|---|
| **pyrowave** (+ Granite, volk, Vulkan-Headers subtree) | `crates/pyrowave-sys/vendor/pyrowave`, pin = `PYROWAVE_COMMIT` in `scripts/vendor-pyrowave.sh`; exact commits recorded in `vendor/pyrowave/PUNKTFUNK-VENDOR.txt` | Bump the commit in the script, re-run it (network required; never from CI), re-apply `crates/pyrowave-sys/patches/`. ⚠️ **Bitstream changes are protocol-affecting** — the wire bit means "PyroWave as of this pin"; a bitstream-changing bump must bump the protocol version and re-diff the Apple Metal hand-port (see the script header). | GitHub releases/commits of Themaister/pyrowave + Themaister/Granite (niche projects, no CVE feed — repo watch is the feed) |
| **libvpl** 2.17.0 | `crates/libvpl-sys/vendor/libvpl` (dispatcher statically linked; needs cmake + libclang) | Manual re-vendor from intel/libvpl at the new tag; rebuild `libvpl-sys` | Intel Security Center (INTEL-SA advisories for oneVPL/media) + intel/libvpl releases |
| **fec-rs** 0.1.0, forked | `crates/punktfunk-core/vendor/fec-rs`: hgaiser/fec-rs with the Cauchy generator matrix Moonlight's `nanors` uses (see `crates/punktfunk-core/Cargo.toml`) | First-party fork: fix in place, port upstream fixes by hand. ⚠️ The matrix is wire format: a stock Moonlight client must still decode our parity | hgaiser/fec-rs commits + releases (no CVE feed) |
| **cros-codecs** parser layer | `crates/pf-bitstream/vendor/cros-codecs`, pin and every local deviation in its `PROVENANCE.md`. Parses untrusted stream bytes in every client | Follow the re-sync procedure at the end of `PROVENANCE.md`, then re-apply the deviations | AOSP `platform/system/cros-codecs` log (GitHub mirror chromeos/cros-codecs) + Android Security Bulletins |
| **usbip** 0.8.0, trimmed as `usbip-sim` | `crates/pf-inject/vendor/usbip-sim`; removals and additions listed in its `NOTICE` | Diff the new upstream release against that list and port by hand | jiegec/usbip releases + RustSec `usbip` |
| **ndk** 0.9.0, patched | `clients/android/native/vendor/ndk`, wired in by `[patch.crates-io]` in the root `Cargo.toml`, which lists the patch set | Copy the new published crate and re-apply the patches. Only `as_ptr` carries the `punktfunk vendored patch` marker; diff against published 0.9.0 for the rest | rust-mobile/ndk releases + RustSec `ndk` |
| **windows-drivers-rs** `wdk-sys` / `wdk-build` 0.5.1 | `packaging/windows/drivers/vendor/wdk-sys` and `packaging/windows/drivers/vendor/wdk-build`, wired in by `[patch.crates-io]` in `packaging/windows/drivers/Cargo.toml`, plus an added `iddcx` ApiSubset | Re-vendor the published crates at the new version and re-apply the `iddcx` subset; the pin is deliberate | microsoft/windows-drivers-rs releases + GitHub security advisories |
| **windows-rs** git pin | `rev = acb5a1a7…` on microsoft/windows-rs (git entries in the root `[workspace.dependencies]`: `windows`, `windows-reactor`, …) | Move the rev / return to crates.io once the needed fixes are released. Note: cargo-audit matches these by name+version from Cargo.lock, but a pre-release rev may not map cleanly onto RustSec advisories — treat the pin itself as the thing to retire. | RustSec (already weekly) + microsoft/windows-rs releases |
| **usbfs-iso / uac-host** git pin | `rev = f3de1fd…` on unom-io/usbfs-iso | First-party fork — we are upstream; fix in the fork, move the rev | Own repo (issues land in our tracker) |
| **SDL3** | Desktop clients, dynamically linked; system-provided or bundled per platform package | Bump the bundled copy in the affected package; system copies are distro-updated | libsdl-org/SDL GitHub security advisories + releases |
| **gamescope** + patch series | Pin in `packaging/nix/gamescope.nix` / built by `packaging/gamescope/build-punktfunk-gamescope.sh`; local patches in `packaging/gamescope/patches/` | Bump the pin, re-rebase the patch series, rebuild sysext/Arch/nix + .deb channels. ⚠️ the gamescope CI legs are best-effort: a broken patch shows up as a *missing package*, not a red build | ValveSoftware/gamescope releases + security advisories |
| **wayland, libdrm, xkbcommon, pixman** | Debian 13 `punktfunk-gamescope` (.deb, SteamOS box) only: pinned wraps in `packaging/gamescope/subprojects/`, linked static into the compositor where the system is below wlroots' floors | Move the wrap to the fixed release's commit; rebuild the .deb and the SteamOS gamescope | freedesktop.org GitLab releases (wayland, mesa/drm, pixman), xkbcommon GitHub releases; Debian security tracker for the same CVEs |
| **Bun runtime** 1.4.2 | Pinned in `.gitea/workflows/windows-host.yml` (`bun-v1.4.2`); bundled portable in the Windows host installer to run the web console + plugin runner. Embeds JavaScriptCore | Bump the version string in the workflow; next installer build picks it up | oven-sh/bun releases (security notes ride in release notes) |

Not on this list on purpose:

- **VB-CABLE** — no longer bundled (audio-substrate program, 2026-08; the host mints its
  own virtual audio devices). If it ever returns, it returns to this table first.
- **openh264 / rav1d CPU decode floor** — crates.io dependencies with vendored C/asm
  inside the `-sys` crates; cargo-audit tracks the crate advisories, and the upstream
  (Cisco openh264, memorysafety/rav1d) security feeds surface through RustSec. No
  separate manual watch needed unless we pin them to git.

## Security-update availability (CRA: ≥10 years)

Where users fetch fixes, and why old artifacts don't vanish (verified 2026-09-18):

- **Gitea releases + package registries** (git.unom.io): Gitea never expires releases.
  The org package cleanup rules prune canary builds only: each rule's `keep_pattern`
  matches release versions (and the generic `latest`/`canary` aliases). Gitea matches
  the pattern against the whole version string. The full release history
  (v0.17.x through current) is still served with assets. Blobs live in the `unom-git`
  S3 bucket with an R2 mirror, and the box is restic-backed every 6 h. Old release
  assets (and their `.sha256` sidecars) therefore stay downloadable.
- **Bazzite sysext feeds**: stable channels publish with `KEEP=0` (keep everything);
  only canary channels prune (`KEEP=6`) — see `rpm.yml` + `publish-sysext-feed.sh`.
- **Flatpak repo** (flatpak.unom.io): published by rsync *without* `--delete`; old
  OSTree commits accumulate, both channels stay in the signed summary.
- **Policy**: never add cleanup that deletes *security* releases; if storage pressure
  ever forces pruning, prune canary builds, never tagged stable releases. SBOMs are
  release assets, so the ≥10-year SBOM retention rides on the same guarantee.
