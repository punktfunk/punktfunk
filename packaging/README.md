# packaging

How each shipped artifact is built and published. **Installing** Punktfunk is
[the docs site](https://docs.punktfunk.unom.io/docs/install)'s job — it generates every install
command from the canonical [`data/platforms.json`](../data/platforms.json), so a fourth copy
here would be the one that
drifts. These files are the maintainer's half: what CI runs, how each package is signed, and the
traps in each ecosystem.

| Directory | Ships | Built by |
|---|---|---|
| [`debian/`](debian/) | `punktfunk-host` + client `.deb` (apt registry) | `deb.yml` |
| [`rpm/`](rpm/) | the RPM (Gitea RPM registry) | `rpm.yml` |
| [`copr/`](copr/) | COPR build-from-SCM settings | COPR |
| [`arch/`](arch/) | pacman binary repo, PKGBUILD, the SteamOS sysext | `arch.yml` |
| [`bazzite/`](bazzite/) | the systemd-sysext image and `host.env` for an appliance | `rpm.yml` |
| [`bootc/`](bootc/) | `Containerfile` to bake the host into an atomic image | — |
| [`flatpak/`](flatpak/) | the client Flatpak and its hosted repo | `flatpak.yml` |
| [`nix/`](nix/) | the flake's packages and the `services.punktfunk` module | `nix.yml` |
| [`windows/`](windows/) | the host installer, `pf-vdisplay` and the gamepad drivers | `windows-host.yml`, `windows-drivers.yml` |
| [`winget/`](winget/) | winget manifests and the REST source | `windows-host.yml` |
| [`macos/`](macos/) | the host as a signed development app bundle | — |
| [`gamescope/`](gamescope/) | `punktfunk-gamescope`, patched for 10-bit HDR PipeWire capture | `deb.yml` |
| [`linux/`](linux/), [`kde/`](kde/) | desktop entries, udev rules, firewall profiles, `host.env` presets | the distro packages |

The Windows *client* packages from [`clients/windows/packaging/`](../clients/windows/packaging/),
not here.

The package workflows build on main and tags only. A pull request that touches `packaging/` or
`scripts/` runs `packaging-lint.yml` instead: `bash -n` and shellcheck over the shell, `rpmspec
--parse` on each spec, `makepkg --printsrcinfo` on each PKGBUILD, and each channel's file
capabilities and groups against [`linux/capabilities.txt`](linux/capabilities.txt) and
[`linux/punktfunk.sysusers`](linux/punktfunk.sysusers). It builds nothing.

## Why the host is not a Flatpak

The host needs the zero-copy encode path, `/dev/uinput`, the PipeWire graph and the compositor's
privileged protocols. A sandbox fights all four. The **client** is a different case and *is* a
Flatpak — it needs only the GPU render node, Wayland, PipeWire audio, the network and hidraw, all
expressible as finish-args, and SteamOS's read-only `/usr` leaves no other option on a Deck.
