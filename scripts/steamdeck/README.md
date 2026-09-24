# punktfunk host on a Steam Deck

Run a punktfunk **host** on a Steam Deck — stream its Game Mode (or KDE desktop) *to* other devices.
(Streaming *to* a Deck is the client; use the Flatpak + [Decky plugin](../../clients/decky/) instead.)

User-facing guide: **docs-site → "SteamOS (Host)"** (`docs-site/content/docs/(guide)/(install)/steamos-host.mdx`).
This README is the deep reference for what the scripts do and how to operate them by hand.

## Why build on-device (not a package or prebuilt binary)

SteamOS 3 is an **immutable, read-only Arch** base:

- No `pacman -S` for system libs; `/usr` is read-only and reset on A/B updates.
- A **prebuilt binary is fragile** — it links the system glibc, PipeWire and libva, and a SteamOS
  update can bump those sonames out from under it (the same class of breakage as the
  NVIDIA-driver-after-update issue).
- The host needs **unsandboxed** `/dev/uinput` + `/dev/uhid`, PipeWire, the compositor, and VAAPI — so
  Flatpak (the normal Deck app channel) doesn't fit. Flatpak/Decky are for the *client*.

So the host is built **natively inside a Debian-trixie distrobox** (`pf2`), chosen because its
glibc ABI matches SteamOS's — the resulting binary runs **natively on SteamOS** (the container
is only the build environment; `punktfunk-host` is launched directly, not via `distrobox enter`). A
rebuild always matches the running OS. Encode is **VAAPI** on the Deck's AMD GPU (NVENC on NVIDIA),
auto-selected by `PUNKTFUNK_ENCODER=auto`.

The honest trade-off: on-device building costs a slow first install (~10–15 min, ~1 GB of image +
toolchain) and adds moving parts (apt mirrors, rustup, bun) — the price of an install that can
always chase the OS. Both failure modes of that chase are automated away now:
`punktfunk-rebuild-check` rebuilds when an OS update breaks the binary's links, and the
atomic-update keep list preserves the `/etc` tuning. The eventual lighter-weight alternative is a
CI-prebuilt bundle with the volatile libraries vendored under an `$ORIGIN` rpath —
OS-update-proof without a toolchain on the device — worth it once SteamOS host volume justifies
per-release artifact signing/hosting; the from-source path would stay as the dev/fallback route.

The web console is the one part that stays in the container at runtime: it's a Nitro **`bun`**
build (`bun` both builds **and runs** it — the bun-preset output uses `Bun.serve` with TLS,
serving HTTPS (HTTP/1.1 over TLS) with the host's identity cert), so its service does
`distrobox enter pf2 -- … bun .output/server/index.mjs`. `bun` is provisioned in the container.

## Scripts

| Script | What it does |
|--------|--------------|
| `install.sh` | Idempotent installer: ensure the `pf2` distrobox + toolchain → build host + web + **plugin runner** → write config → build the **HDR gamescope** below → tune sysctl + udev + `vhci-hcd` + `input` group and **register it on SteamOS's atomic-update keep list** (sudo) → install + start `punktfunk-host` / `punktfunk-web` systemd **user** services with linger, plus the **rebuild check** below. |
| `update.sh` | Rebuild everything from the current source and restart the services (config + pairings persist). `--pull` does `git pull` first. Also retrofits anything a newer install.sh writes (runner, HDR gamescope, keep-list registration, rebuild check) onto older installs. |
| `build-gamescope.sh` | Build gamescope + the `pipewire-hdr` patches (`packaging/gamescope`) in the same distrobox and install it as `~/.local/bin/punktfunk-gamescope`, wiring `PUNKTFUNK_GAMESCOPE_BIN` into `host.env` — what lets Game Mode stream **10-bit BT.2020 PQ (HDR)** instead of 8-bit SDR. Best-effort: a failure warns and the host streams SDR. Content-stamped — a no-op unless `packaging/gamescope/` changed or the binary broke. |
| `rebuild-check.sh` | The post-OS-update self-heal (run by `punktfunk-rebuild-check.service` before the host at session start): `ldd`-probes the host binary **and the HDR gamescope** — milliseconds when healthy, a full `update.sh` rebuild only when a SteamOS update actually broke library links. |

```sh
git clone https://git.unom.io/unom/punktfunk ~/punktfunk
bash ~/punktfunk/scripts/steamdeck/install.sh            # PIN pairing required (secure default)
bash ~/punktfunk/scripts/steamdeck/install.sh --open     # trusted LAN: accept unpaired clients
bash ~/punktfunk/scripts/steamdeck/install.sh --no-web   # host only, no web console
bash ~/punktfunk/scripts/steamdeck/install.sh --gamestream     # also serve stock Moonlight (trusted LAN)
bash ~/punktfunk/scripts/steamdeck/update.sh             # after pulling new source
```

Note: the Deck install matches a bare `serve` — native-only, PIN pairing required. Pass `--gamestream`
to also serve stock Moonlight clients; `--no-gamestream` is kept for old command lines / re-runs.

Env overrides: `PUNKTFUNK_SRC` (source dir, default `~/punktfunk`), `PUNKTFUNK_BOX` (container name,
default `pf2`), `PUNKTFUNK_MGMT_PORT` (47990), `PUNKTFUNK_WEB_PORT` (47992).

## What gets installed

- **Binary:** `~/punktfunk/target-steamos/release/punktfunk-host` (built in `pf2`, run natively).
- **Config:** `~/.config/punktfunk/host.env` (encoder/compositor) and `web.env` (generated web login
  password + session secret). Trust material (`cert.pem`, `mgmt-token`, `punktfunk1-paired.json`) lives
  here too and persists across updates.
- **Services:** `~/.config/systemd/user/punktfunk-host.service` (runs `serve --mgmt-bind
  0.0.0.0:47990`, `+ --gamestream` / `+ --open` if chosen — the native `punktfunk/1` plane is always
  on; `--gamestream` adds the Moonlight-compat planes so Game Mode also streams to stock Moonlight),
  `punktfunk-web.service`, `punktfunk-rebuild-check.service` (post-OS-update self-heal, enabled), and
  `punktfunk-scripting.service` (plugin runner, **opt-in** — enable it once you use plugins/scripts).
  Linger is enabled so they run without a login session.
- **Plugin runner:** the deb's payload laid out user-scoped (read-only `/usr` can't take the
  package): wrapper `~/.local/bin/punktfunk-scripting`, pinned `bun` in
  `~/.local/lib/punktfunk-scripting/`, bundle in `~/.local/share/punktfunk-scripting/`.
- **HDR gamescope:** `~/.local/bin/punktfunk-gamescope` (gamescope + the `pipewire-hdr` patches,
  built in `pf2`, run natively — it does **not** replace the system gamescope; only the sessions
  the host spawns use it). `host.env` gains `PUNKTFUNK_GAMESCOPE_BIN=` pointing at it — a line
  `build-gamescope.sh` maintains and removes again if the binary ever stops working, because a
  stale absolute override would break session spawning, not just HDR. HDR is attempted by
  default when present; `PUNKTFUNK_GAMESCOPE_HDR=0` in `host.env` forces SDR. Verify with
  `punktfunk-host hdr-probe`.
- **System tuning (sudo):** `/etc/sysctl.d/99-punktfunk-net.conf` (32 MB UDP buffers — the #1
  high-bitrate lever), `/etc/udev/rules.d/60-punktfunk.rules` (`uinput`/`uhid` access),
  `/etc/modules-load.d/punktfunk.conf` (`vhci-hcd` for the native Deck pad), `$USER` in the `input`
  group **and in `punktfunk`** — the latter created here if missing, because the udev rule
  `chgrp`s the vhci `attach`/`detach` nodes to it and a rule that names a nonexistent group fails
  silently, leaving the native Deck pad unable to attach (the deb/rpm/arch scriptlets `groupadd` it;
  nothing on this path did until now). It is separate from `input` on purpose: writing `attach`
  materialises an arbitrary emulated USB device (security-review 2026-08-05 M-4). Drop it with
  `sudo gpasswd -d "$USER" punktfunk` if you would rather stream without that pad.
  Plus `/etc/atomic-update.conf.d/punktfunk.conf`, which registers the three files on
  SteamOS's atomic-update keep list so A/B OS updates carry them over (verified: without it an
  update silently strips them — pads degrade to Xbox 360, buffers drop to 208 KB).

## Operating

```sh
systemctl --user status  punktfunk-host punktfunk-web
journalctl --user -u punktfunk-host -f          # watch sessions / pairing PIN
systemctl --user restart punktfunk-host         # after editing host.env
```

Pair from the web console (Devices → arm pairing) or directly from a client with the host's PIN. The
host advertises over mDNS as `_punktfunk._udp`, so clients discover it automatically.

## Gotchas

- **distrobox required.** If missing: `curl -sfL https://raw.githubusercontent.com/89luca89/distrobox/main/install | sh -s -- --prefix ~/.local` (then ensure `~/.local/bin` is on PATH).
- **First build is slow** (~20 min with the HDR gamescope, + ~1 GB toolchain/image). Incremental afterwards.
- **No passwordless sudo** → the installer skips the sysctl/udev/group steps with a warning; high
  bitrates will drop packets until you apply `99-punktfunk-net.conf` and join `input` (and
  `punktfunk`, for the native Deck pad) yourself. The script prints the exact commands.
- **Installed before 0.25.0?** `web.env` was written at the ambient umask, i.e. world-readable, so
  the console password and session secret leaked to every local account. `install.sh`/`update.sh`
  now tighten `~/.config/punktfunk` to `0700` and `web.env` to `0600` on every run and say so —
  but reset the password afterwards, because a chmod does not un-leak a read secret: put a
  `PUNKTFUNK_UI_PASSWORD=<new>` line in `web.env` and `systemctl --user restart punktfunk-web`.
- **Game Mode auto-suspend** drops the host off the network on idle — disable it (Settings → Power)
  for a headless host.
- **WiFi tx ceiling** ≈ 250 Mbps goodput (a Deck hardware/driver packet-rate limit, band-independent);
  fine for 1080p/1440p60. A wired dock lifts it.
- **After a SteamOS update** nothing should be needed: the `/etc` tuning survives via the
  atomic-update keep list, and `punktfunk-rebuild-check` rebuilds the binary automatically if the
  new base actually broke its library links (first session start after the update takes the build's
  few minutes in that case). A manual `update.sh` remains harmless.
