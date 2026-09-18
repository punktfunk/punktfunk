---
title: Running as a Service
description: Start the host at boot — for a desktop you log into, or a fully headless always-on machine.
---

Running `serve` in a terminal is fine for trying Punktfunk out; an always-available host runs as a
service. First what the service starts, then the two cases: a desktop you log into, and a headless
box.

## What the unit starts

The bundled unit runs `serve`: the **secure native-only host** — the `punktfunk/1` plane plus the
management API. Stock [Moonlight](/docs/moonlight) (GameStream) support is **opt-in** on every
install route: its pairing runs over plain HTTP and its legacy encryption is weaker (see
[Security & Safe Use](/docs/security)), so it belongs on trusted LANs you chose to enable it on.
See what yours starts:

```sh
systemctl --user cat punktfunk-host
```

To serve stock Moonlight clients too, add one line to `~/.config/punktfunk/host.env` (the unit's
`EnvironmentFile` — no drop-in or unit editing needed) and restart:

```ini
PUNKTFUNK_GAMESTREAM=1
```

```sh
systemctl --user restart punktfunk-host
```

Then open the `punktfunk-gamestream` firewall service alongside `punktfunk-native` — your distro
guide's firewall step has the commands.

> **Upgrading?** Earlier releases baked `--gamestream` into the unit's `ExecStart`, so a packaged
> host served Moonlight by default. The upgrade replaces that unit with the native-only one — if
> you relied on Moonlight, add `PUNKTFUNK_GAMESTREAM=1` to `host.env` as above. (A hand-made
> `systemctl --user edit` drop-in that sets its own `ExecStart` keeps winning either way —
> `systemctl --user cat punktfunk-host` shows what is in effect.)

Windows is the same by default — GameStream **off** unless you tick it in the installer, through
its own mechanism — see [Windows](#windows).

## The browser client (preview)

Punktfunk can also accept a browser over WebTransport. It is **off** by default and still a
preview — a browser that connects now runs a full session — so turn it on only if you are trying
it out:

```ini
PUNKTFUNK_WEBTRANSPORT=1
PUNKTFUNK_WEBTRANSPORT_PORT=9778
```

The port is UDP and separate from the native plane's, so open it in your firewall alongside
`punktfunk-native`. Leave `PUNKTFUNK_WEBTRANSPORT_PORT` out unless 9778 is taken.

Two more knobs, both worth setting if you leave the plane on:

```ini
PUNKTFUNK_WEBTRANSPORT_BIND=192.168.1.10
PUNKTFUNK_WEBTRANSPORT_ORIGINS=https://192.168.1.10:47990
```

`_BIND` keeps the plane on one interface instead of all of them. `_ORIGINS` is the list of web
pages allowed to open a session, and it matters more than it looks: a browser applies none of its
usual same-origin rules to WebTransport, so with the list empty **any** page you happen to have
open can reach this port. Set it to the address you load the client from.

## A. A desktop you log into

If you sit at the machine (or it auto-logs-in to a desktop), run the host as a **systemd user
service** that starts with your session.

**`host.env` is optional.** The unit reads `~/.config/punktfunk/host.env` if it exists (no package
creates it — they ship templates under `/usr/share`) and runs with sane defaults without it: the host
auto-detects the live session, so an ordinary desktop needs no file. Copy a template when you want
to set a knob (the Bazzite one is `host.env.bazzite`; every knob is in
[Configuration](/docs/configuration)):

```sh
mkdir -p ~/.config/punktfunk
# /usr/share/punktfunk/ on Fedora/Arch/Bazzite, /usr/share/punktfunk-host/ on Ubuntu
cp /usr/share/punktfunk/host.env.example ~/.config/punktfunk/host.env
```

**Installed from a package** (apt, dnf, pacman, or the Bazzite sysext) — the unit is already at
`/usr/lib/systemd/user/punktfunk-host.service`, its `ExecStart` pointing at the installed binary.
Nothing to copy:

```sh
systemctl --user daemon-reload             # the sysext route needs this; harmless elsewhere
systemctl --user enable --now punktfunk-host
```

**Built from source** — install the unit from your checkout, and take `host.env` from there too
(`cp scripts/host.env.example ~/.config/punktfunk/host.env`). The unit's `ExecStart` points at
`%h/punktfunk/target/release/punktfunk-host` (`%h` is your home directory); edit the copy if your
checkout lives elsewhere:

```sh
mkdir -p ~/.config/systemd/user
cp scripts/punktfunk-host.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now punktfunk-host
```

Don't do the copy on a packaged install: a unit in `~/.config/systemd/user/` shadows the packaged
one, and the source unit points at a build tree you don't have — the service fails with
`status=203/EXEC`.

The host now starts whenever you log in. Check with `systemctl --user status punktfunk-host`.

**You don't need to export anything for it.** The host finds the live compositor session itself on
every connect and works out where to reach it (`WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, the session bus,
sway's `SWAYSOCK`, Hyprland's instance signature) from the running compositor — `host.env` is for
policy, not session plumbing, and `systemctl --user import-environment` is not a prerequisite.

### Restart the host with your desktop

One drop-in makes the host follow your session's lifetime:

```sh
mkdir -p ~/.config/systemd/user/punktfunk-host.service.d
# /usr/share/punktfunk/ on Fedora/Arch/Bazzite, /usr/share/punktfunk-host/ on Ubuntu,
# scripts/ in a source checkout
cp /usr/share/punktfunk/punktfunk-host-desktop-session.conf \
   ~/.config/systemd/user/punktfunk-host.service.d/desktop-session.conf
systemctl --user daemon-reload
systemctl --user reenable punktfunk-host
systemctl --user restart punktfunk-host
```

Without it, restarting Plasma or GNOME — a crash, a log out and back in, "restart the shell" — leaves
the host running against a compositor that no longer exists: it keeps listening and answering, and
every session after that fails at capture. The drop-in makes a compositor restart a host restart.

On **NixOS** don't copy anything — the module has the option:

```nix
services.punktfunk.host.desktopSession = true;
```

Skip it on the headless/appliance route below (which has its own session unit), and on **Sway or
Hyprland**, which don't hand their session to systemd: they never reach `graphical-session.target`, so
the drop-in is harmless but does nothing. There, start the host from the compositor's own config
instead of enabling the unit — `exec systemctl --user start punktfunk-host` in your sway config, or
`exec-once = systemctl --user start punktfunk-host` in Hyprland's — and leave the unit disabled
(`systemctl --user disable punktfunk-host`), so it isn't also started at login.

## B. A headless, always-on host

**No monitor and no login** — a machine in a closet that's always ready — needs two things: a
desktop session that comes up at boot, and the host service started without a login.

First let the host service start at boot with nobody logged in:

```sh
sudo loginctl enable-linger "$USER"
```

Then bring up a session automatically. Auto-login, lock disable and the session unit differ per
compositor, so each has its own page:

- GNOME: [GNOME → Headless session](/docs/gnome#headless-session).
- KDE Plasma: [KDE → Headless session](/docs/kde#headless-session).
- Steam / gamescope: [gamescope](/docs/gamescope) — the host launches its own session per client, so
  there's no separate session unit. A headless box that autologins into **Gaming Mode** needs one
  more thing: your user in the `punktfunk` group (`sudo usermod -aG punktfunk "$USER"`, then log
  out and back in). Without it the host cannot stop the display manager to take that session over,
  so every connect quietly mirrors the box's own screen — which, headless, is a black one. See
  [gamescope → autologin display managers](/docs/gamescope#nobara-and-other-autologin-display-managers).

Once a session comes up at boot, enable the host user service (section A) and reboot. The host comes
up on that session.

### Headless Bazzite

On Bazzite the host launches its own gamescope/Steam session per client, so no separate session
unit is needed — see [Bazzite](/docs/bazzite) and [gamescope](/docs/gamescope).

## Windows

> The Windows host (newer than the Linux one; not the Windows *client*, which streams *to* a PC)
> ships as a signed installer with an SCM service and Punktfunk's own **indirect display driver**
> the host pushes frames straight into.

The host runs as a `LocalSystem` service that launches into the interactive session, so it captures
the secure desktop (UAC / lock screen) and survives reboots with nobody logged in — the same model
Sunshine/Apollo use. At that privilege level, keep it on a trusted network and be deliberate about
which machine you host on — see [Security & Safe Use](/docs/security).

The easy path is the **signed installer**: download `punktfunk-host-setup-<ver>.exe` from the package
registry ([`punktfunk-host-windows`](https://git.unom.io/unom/-/packages)) and run it. It drops the host
into `C:\Program Files\punktfunk`, installs the bundled **pf-vdisplay** virtual-display driver, and
registers + starts the service (`/VERYSILENT` for unattended). Upgrades and uninstall go through
Add/Remove Programs.

Prefer the CLI? `punktfunk-host service install` from an elevated prompt — see
[Windows Host](/docs/windows-host). Hardware encode needs a GPU — NVIDIA (NVENC), AMD (AMF), or
Intel (QSV); the host falls back to software H.264 without one.

**GameStream on Windows.** The installer leaves Moonlight compatibility **off** — a checkbox in the
wizard (`/MERGETASKS="gamestream"` to select it unattended). Change it later on the web console's
**Host → Settings** page, or from an elevated prompt:

```powershell
punktfunk-host service install --gamestream=on   # or --gamestream=off
punktfunk-host service restart
```

> **Firewall scope.** The installer opens the streaming + console ports on **Private and Domain**
> networks only — not **Public**. If your LAN is (mis)classified Public, clients won't connect until
> you set it to Private (Windows Settings → Network), and the host logs a warning when it's on a Public
> network. For a trusted network Windows insists is Public, tick **"Allow connections on Public
> networks"** at install (or pass `--allow-public-network` to `service install`). See
> [Security & Safe Use](/docs/security) for the reasoning.

## Verifying

After a reboot, from another machine on the network:

```sh
punktfunk reachable 192.168.1.50   # exit 0 = the host answered, 2 = it didn't
punktfunk hosts list --probe       # every saved host, online or offline
```

`punktfunk` is the headless client CLI — it ships in the Linux client packages (`punktfunk-client`)
and with the Windows client. From a source checkout, `punktfunk-probe --discover` browses the LAN
instead; it's a dev tool and isn't packaged. Or open a native client / Moonlight and look for the
host.

If the host answers, it's up. If not, check `journalctl --user -u punktfunk-host` on the host — on
Windows, `punktfunk-host service status` from an elevated prompt on the machine itself.

## GPU scheduling priority

The [PyroWave](/docs/pyrowave) codec encodes on the same GPU shader cores your game uses, so a
demanding game can crowd it out and the stream's frame rate drops. The fix is to ask the driver to
schedule that encode ahead of the game, and every driver we tested gates the request on a single
Linux capability, `CAP_SYS_NICE`. The other codecs use a separate video engine on the GPU and are
unaffected either way.

That capability cannot live on the host, so it lives next to it. Every way of installing a Linux
host — apt, dnf, pacman, the Bazzite sysext, the NixOS module, the Steam Deck script — ships a
second, deliberately small program, **`punktfunk-encode-worker`**, and grants `cap_sys_nice=ep` to
*that*. The host starts one per PyroWave session, hands it the captured frames, and takes the
compressed video back; the worker talks to nothing else, not your desktop and not the network.
`punktfunk-host` itself must carry **no capability** — see below.

> **Never `setcap` `punktfunk-host`.** Not by hand, not through a systemd `AmbientCapabilities=`
> line, not through a NixOS `security.wrappers` entry. All three put the capability in the same
> place, and all three take KDE desktop streaming away completely. There is no capability the host
> wants: the worker needs one, and your packages already gave it one.

Why: to hand the host its virtual display, KWin first works out *which* program is asking, by
reading the connecting process's `/proc/<pid>/exe` and matching it against the `.desktop` file the
packages install. Linux refuses that read unless the reader holds every capability the target holds
— and KWin holds none. So a host carrying a capability is one KWin cannot identify, its restricted
protocols are never offered, and every session fails at capture with:

```
KWin virtual output failed: KWin does not expose zkde_screencast_unstable_v1 to this client
```

which reads exactly like a missing or mis-installed `.desktop` file and survives reinstalling both
ends. If you see that error, check the binaries — the host's own message names the capability when
it finds one:

```sh
getcap /usr/bin/punktfunk-host              # correct output is nothing at all
getcap /usr/bin/punktfunk-encode-worker     # /usr/bin/punktfunk-encode-worker cap_sys_nice=ep

sudo setcap -r /usr/bin/punktfunk-host      # clear it, then restart the host
```

On the Bazzite image `/usr` is read-only, so there is nothing to repair in place — take the next
image (`sudo punktfunk-sysext update`). On NixOS the worker *is* wrapped, because a file capability
cannot live on a read-only store path: the module creates the wrapper and points the host at it,
and the host's own `ExecStart` stays on the plain store path.

**The grant is best-effort, and no session depends on it.** A worker without the capability still
encodes — it asks for the elevated priority, is refused, says so once, and runs at the normal one.
So does a host that cannot find or start a worker at all: it encodes in-process, logs one line, and
streams. The only thing at stake is frame pacing under a GPU-bound game.
`PYROWAVE_QUEUE_PRIORITY=off` stops the host asking for priority, and `PUNKTFUNK_ENCODE_WORKER=off`
keeps the encode in the host process — both on
[Configuration](/docs/configuration#advanced-performance-tuning).

## Stopping and removing

After a Linux package update the user service keeps running the old binary until restarted, and a
package can't restart another user's `--user` units for you — [Updating](/docs/updating) has the
update command for every install method and the restart that finishes the job. The Windows
installer restarts its own service.

To stop the host for now:

```sh
systemctl --user stop punktfunk-host          # Linux
punktfunk-host service stop                   # Windows, elevated prompt
```

To stop it for good, so it doesn't come back at login or boot:

```sh
# add punktfunk-web (the console) and punktfunk-kde-session (the headless KDE route) if you enabled them
systemctl --user disable --now punktfunk-host
rm -rf ~/.config/systemd/user/punktfunk-host.service.d   # any drop-ins you added
sudo loginctl disable-linger "$USER"                     # only if you enabled lingering
```

On Windows, `punktfunk-host service uninstall` from an elevated prompt stops the service, removes it,
and removes the firewall rules it added. To remove the whole install, use Add/Remove Programs.

Neither removes `~/.config/punktfunk` (Linux) or `%ProgramData%\punktfunk` (Windows) — your
certificate, pairings and console password stay, so a reinstall picks up where you left off. See
[Uninstall](/docs/uninstall) to clear them out.
