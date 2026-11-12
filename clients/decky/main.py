"""
punktfunk Decky plugin — backend.

The Gaming-Mode UI (``src/index.tsx``) calls these methods over the Decky bridge. The actual
STREAM is NOT launched here — it is launched by the frontend through Steam
(SteamClient.Apps.RunGame on a hidden non-Steam shortcut that points at ``bin/punktfunkrun.sh``),
because gamescope only focuses/fullscreens windows in the process tree Steam launched via
``reaper``. A flatpak spawned from this backend would be invisible/unfocused (gamescope#484).
This backend is a THIN SHELL OVER THE HEADLESS CLI (``punktfunk``, shipped in every package
since v0.22.0), plus the handful of things that are genuinely Steam's business. It used to be
a second client — its own mDNS parser, its own host-store editor, its own settings writer —
and every one of those was a copy of a rule that already lives in Rust, drifting from it. The
rule now has one home; this file builds argv and maps exit codes.

Thin CLI shells — each is build argv, run, parse JSON, map the exit code:

* **discover()** — ``punktfunk discover --json``: the LAN's hosts, already annotated with
  whether this device has them saved and paired.
* **hosts()** — ``punktfunk hosts list --probe --json``: the saved hosts with a live,
  mDNS-independent reachability probe, and their preset bindings and pinned cards already
  resolved against the preset catalog.
* **pair(addr, port, pin, name)** — ``punktfunk pair``: the SPAKE2 PIN ceremony.
* **trust_host(addr, port, fp, name)** — ``punktfunk hosts add --fp``: step 1 of request
  access, and the ONLY write this backend makes to the client's store.

Kept because only a Decky plugin can do them:

* **runner_info()** — resolve flatpak vs native and hand the frontend the wrapper path.
* **shortcut_art()** — base64 grid/hero/logo + icon path for the Steam shortcut.
* **apply_controller_config()** — write the native-touch layout into every Steam account's
  configset dir, chowned back to the user (this backend is root; Steam is not).
* **check_update() / update_client()** — the plugin's own registry manifest (Decky's install
  RPC needs artifact + SHA-256) and the client's update route.
* **kill_stream()** — force-stop a wedged client.

What is deliberately NOT here: the stream launch. It goes through Steam
(SteamClient.Apps.RunGame on a non-Steam shortcut pointing at ``bin/punktfunkrun.sh``),
because gamescope only focuses/fullscreens windows in the process tree Steam launched via
``reaper`` — a client spawned from this backend would come up invisible and unfocused
(gamescope#484). Settings, add-host-by-address, the library browser and preset editing are
not here either: they are one shortcut away in the client's own console home.
"""

import asyncio
import base64
import json
import os
import re
import shutil
import ssl
import time
import urllib.request
from pathlib import Path

import decky
from pf_decky import client, steam_input, update

# A library hero is a few hundred KB; nothing fetched here is a tenth of this. The cap keeps a
# wrong URL from reading a stream into the plugin's memory.
_MAX_ART_BYTES = 8 * 1024 * 1024


def _fetch_bytes(url: str, timeout: float = 10.0) -> bytes:
    """Blocking HTTPS GET of a small binary (run in an executor). Raises on any failure, an
    over-long body included — a truncated image is worse than falling through to the next CDN."""
    req = urllib.request.Request(url, headers={"User-Agent": "punktfunk-decky"})
    with urllib.request.urlopen(req, timeout=timeout, context=update._ssl_context()) as resp:
        data = resp.read(_MAX_ART_BYTES + 1)
    if len(data) > _MAX_ART_BYTES:
        raise ValueError(f"image over {_MAX_ART_BYTES} bytes")
    return data


# --- a Steam game's own artwork, for the per-game stream shortcut -----------------------------
#
# A stream started from a game's page runs under a hidden shortcut NAMED after the game, so the
# overlay, the "now playing" surfaces and friends see the game and not "Punktfunk". The art has
# to match. Steam keeps every library image it has shown in `appcache/librarycache`; what is not
# there is on the store CDN under the same file names.
_ART_KINDS = (
    # key, file name, image type
    ("grid", "library_600x900.jpg", "jpg"),
    ("hero", "library_hero.jpg", "jpg"),
    ("logo", "logo.png", "png"),
    ("gridwide", "header.jpg", "jpg"),
)
_ART_CDNS = (
    "https://cdn.cloudflare.steamstatic.com/steam/apps/{appid}/{name}",
    "https://shared.steamstatic.com/store_item_assets/steam/apps/{appid}/{name}",
)
# The game's small icon, by the overview's `icon_hash`: the URL Steam's own client resolves first,
# the older community path second.
_ICON_CDNS = (
    "https://shared.steamstatic.com/community_assets/images/apps/{appid}/{hash}.jpg",
    "https://cdn.cloudflare.steamstatic.com/steamcommunity/public/images/apps/{appid}/{hash}.jpg",
)
_ICON_HASH = re.compile(r"^[0-9a-f]{40}$")


def _librarycache_candidates(appid: int, name: str) -> list[Path]:
    """Where Steam has cached this image locally, oldest layout last: the per-app directory
    current clients write, then the flat `<appid>_<name>` files older ones did."""
    base = steam_input._steam_root() / "appcache" / "librarycache"
    return [base / str(appid) / name, base / f"{appid}_{name}"]


def _read_art(appid: int, name: str) -> bytes | None:
    """The image bytes from the local cache, else the CDN; None when nowhere has it."""
    for path in _librarycache_candidates(appid, name):
        try:
            data = path.read_bytes()
            if data:
                return data
        except OSError:
            continue
    for cdn in _ART_CDNS:
        try:
            return _fetch_bytes(cdn.format(appid=appid, name=name))
        except Exception:  # noqa: BLE001 — a 404 on one CDN is the next one's turn
            continue
    return None


def _icon_dir() -> Path:
    """Where fetched game icons land — Steam reads the path, so it must be user-readable."""
    return Path(decky.DECKY_PLUGIN_SETTINGS_DIR) / "icons"


def _read_icon(appid: int, icon_hash: str) -> bytes | None:
    """The game's icon bytes: Steam's own cache (`librarycache/<appid>/<hash>.jpg`) first, the
    CDNs second; None when nowhere has it."""
    cached = steam_input._steam_root() / "appcache" / "librarycache" / str(appid) / f"{icon_hash}.jpg"
    try:
        data = cached.read_bytes()
        if data:
            return data
    except OSError:
        pass
    for cdn in _ICON_CDNS:
        try:
            return _fetch_bytes(cdn.format(appid=appid, hash=icon_hash))
        except Exception:  # noqa: BLE001 — a 404 on one CDN is the next one's turn
            continue
    return None


def _ctl_sockets() -> list[Path]:
    """Candidate paths of the streaming client's control socket (guide/QAM injection):
    the flatpak app runtime dir first (the one runtime path the sandbox and this backend
    see identically), then the plain runtime dir (native installs). Mirrors the session
    binary's ``ctl_socket::path``."""
    uid = os.environ.get("PF_UID") or "1000"
    run = Path(f"/run/user/{uid}")
    return [
        run / "app" / client.APP_ID / "punktfunk-session-ctl.sock",
        run / "app" / client.LEGACY_APP_ID / "punktfunk-session-ctl.sock",
        run / "punktfunk-session-ctl.sock",
    ]


class Plugin:
    # The headless CLI: build argv, run, map the exit code.

    async def discover(self) -> dict:
        return await client.discover()

    async def hosts(self) -> dict:
        return await client.hosts()

    async def pair(self, addr: str, port: int, pin: str, name: str = "Steam Deck") -> dict:
        return await client.pair(addr, port, pin, name)

    async def request_access(
        self, addr: str, port: int, name: str = "Steam Deck", fp: str = ""
    ) -> dict:
        return await client.request_access(addr, port, name, fp)

    async def trust_host(self, addr: str, port: int, fp: str, name: str = "") -> dict:
        return await client.trust_host(addr, port, fp, name)

    async def library(self, ref: str) -> dict:
        return await client.library(ref)

    async def profiles(self, ref: str) -> dict:
        return await client.profiles(ref)

    async def shortcut_art(self) -> dict:
        """The Steam-shortcut artwork shipped with the plugin (committed under ``assets/``):
        base64 PNGs (grid/gridwide/hero/logo) for SetCustomArtworkForApp plus the icon's
        absolute path for SetShortcutIcon (which wants a file, not bytes). Missing files are
        simply omitted — artwork is cosmetic and must never block a launch."""
        art: dict = {}
        base = Path(decky.DECKY_PLUGIN_DIR) / "assets"
        for key, fname in (
            ("grid", "grid.png"),
            ("gridwide", "gridwide.png"),
            ("hero", "hero.png"),
            ("logo", "logo.png"),
        ):
            try:
                art[key] = base64.b64encode((base / fname).read_bytes()).decode()
            except OSError:
                pass
        icon = base / "icon.png"
        art["icon_path"] = str(icon) if icon.exists() else ""
        return art

    async def game_art(self, appid: int, icon_hash: str = "") -> dict:
        """A Steam game's own artwork for its stream shortcut: base64 grid / hero / logo /
        gridwide with their image types, and the icon as bytes (the frontend writes it as PNG
        through :meth:`save_icon`). Local cache first, the store CDN second; a missing piece is
        omitted, never a failure — art is cosmetic and the launch does not wait on it.

        `icon_hash` is the overview's `icon_hash`, validated to 40 hex characters because it
        becomes part of a URL."""
        try:
            appid = int(appid)
        except (TypeError, ValueError):
            return {"ok": False, "error": "bad-appid"}
        if appid <= 0:
            return {"ok": False, "error": "bad-appid"}
        loop = asyncio.get_running_loop()
        art: dict = {"ok": True, "icon_path": ""}
        for key, name, fmt in _ART_KINDS:
            data = await loop.run_in_executor(None, _read_art, appid, name)
            if data:
                art[key] = base64.b64encode(data).decode()
                art[f"{key}_type"] = fmt
        # The icon travels as bytes as well as a path: Steam embeds a shortcut's icon into its
        # overview only when it loads shortcuts at startup, so the frontend sets the file for
        # the next start AND injects the bytes into the live overview for this one.
        icon_hash = str(icon_hash or "").strip().lower()
        if _ICON_HASH.match(icon_hash):
            data = await loop.run_in_executor(None, _read_icon, appid, icon_hash)
            if data:
                art["icon"] = base64.b64encode(data).decode()
                art["icon_type"] = "jpg"
            else:
                decky.logger.info("no icon for %s (hash %s)", appid, icon_hash[:8])
        if "icon" not in art:
            # Never a gray box: the Punktfunk icon stands in when the game's own is nowhere.
            fallback = Path(decky.DECKY_PLUGIN_DIR) / "assets" / "icon.png"
            try:
                art["icon"] = base64.b64encode(fallback.read_bytes()).decode()
                art["icon_type"] = "png"
                art["icon_path"] = str(fallback)
            except OSError:
                pass
        return art

    async def save_icon(self, appid: int, png_base64: str) -> dict:
        """Write a per-game shortcut's icon as PNG (Steam's shortcut icons are PNG or ICO; the
        frontend converts the game's JPG on a canvas) and hand back the path SetShortcutIcon
        wants. User-readable, since Steam reads it as the user."""
        try:
            appid = int(appid)
            data = base64.b64decode(str(png_base64), validate=True)
        except (TypeError, ValueError):
            return {"ok": False, "error": "bad-input"}
        if appid <= 0 or not data.startswith(b"\x89PNG"):
            return {"ok": False, "error": "bad-input"}
        dest = _icon_dir() / f"{appid}.png"
        try:
            dest.parent.mkdir(parents=True, exist_ok=True)
            dest.parent.chmod(0o755)
            dest.write_bytes(data)
            dest.chmod(0o644)  # this backend is root; Steam is not
        except OSError as e:
            return {"ok": False, "error": "write-failed", "detail": str(e)}
        return {"ok": True, "path": str(dest)}

    async def apply_controller_config(self, name: str = "Punktfunk") -> dict:
        return await steam_input.apply_controller_config(name)

    async def runner_info(self) -> dict:
        return await client.runner_info()

    # ---- Shared known-hosts store (the SAME file the desktop client reads/writes) ----

    async def kill_stream(self) -> dict:
        """Force-stop a wedged stream client — ``flatpak kill`` for the sandboxed one, a plain
        SIGTERM by name for a native install (which has no flatpak instance to kill)."""
        prefix = client._client_argv()
        if not prefix:
            return {"ok": False, "error": "client-not-found"}
        if prefix[0] == client._flatpak():
            argv = [prefix[0], "kill", prefix[-1]]  # the app id is last, see client._client_argv
        else:
            # -x: whole-name match, so this can only ever hit the client itself.
            killer = shutil.which("pkill") or "/usr/bin/pkill"
            argv = [killer, "-x", client.NATIVE_BIN]
        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                env=client._flatpak_env(),
            )
            await asyncio.wait_for(proc.wait(), timeout=10.0)
        except Exception:  # noqa: BLE001
            decky.logger.exception("kill_stream (%s) failed", argv[0])
            return {"ok": False}
        return {"ok": True}

    async def stream_running(self) -> dict:
        """Whether the streaming client's control socket exists — i.e. the client is up.

        The socket appears at the client's first stream and lives for the process, so
        between console-mode streams it lingers; that only leaves the panel's host
        buttons harmlessly visible.
        """
        return {"running": any(p.is_socket() for p in _ctl_sockets())}

    async def end_game(self, ref: str, game_id: str) -> dict:
        """End a title this device launched on the host (``punktfunk end-game <ref> --game ID``).

        ``{ok: True, notice}`` once the game is gone — ended now, or not running any more (exit
        5). Anything else is ``{ok: False, error: <exit code>, notice}``: the CLI's own sentence,
        which says what to do. Both references are positional or flag values, so one that starts
        with ``-`` is refused here as ``{ok: False, error: "unresolved"}``, with no notice, rather
        than handed to the CLI as a flag."""
        ref, game_id = str(ref).strip(), str(game_id).strip()
        if not ref or ref.startswith("-") or not game_id or game_id.startswith("-"):
            return {"ok": False, "error": "unresolved"}
        rc, out, err = await client._run_cli(["end-game", ref, "--game", game_id], timeout=20.0)
        lines = (out.strip() or err.strip()).splitlines()
        notice = lines[-1] if lines else ""
        if rc in (0, 5):
            return {"ok": True, "notice": notice}
        return {"ok": False, "error": str(rc), "notice": notice}

    async def host_action(self, action: str) -> dict:
        """Press a HOST system button on the running stream: ``guide`` (the Steam/Xbox/PS
        menu button) or ``qam`` (the quick-access ``…``).

        Talks to the session binary's control socket (one text verb per connection,
        ``ok``/``err`` back) — the flatpak app runtime dir first (the sandboxed client;
        that dir is the one runtime path host and sandbox see identically), then the
        plain runtime dir (native installs). No socket = no running stream.
        """
        if action not in ("guide", "qam"):
            return {"ok": False, "error": f"unknown action {action!r}"}
        for sock in _ctl_sockets():
            if not sock.is_socket():
                continue
            try:
                reader, writer = await asyncio.wait_for(
                    asyncio.open_unix_connection(str(sock)), timeout=2.0
                )
            except Exception:  # noqa: BLE001 — a stale socket file; try the next path
                continue
            try:
                writer.write(f"{action}\n".encode())
                await writer.drain()
                reply = await asyncio.wait_for(reader.readline(), timeout=2.0)
                return {"ok": reply.strip() == b"ok"}
            except Exception as e:  # noqa: BLE001
                return {"ok": False, "error": str(e)}
            finally:
                writer.close()
        return {"ok": False, "error": "no-stream"}

    async def update_client(self) -> dict:
        return await update.update_client()

    async def check_update(self, force: bool = False) -> dict:
        return await update.check_update(force)

    # ---- Decky lifecycle ----

    async def _main(self):
        decky.logger.info("punktfunk plugin loaded (runner=%s)", client._runner_path())

    async def _unload(self):
        decky.logger.info("punktfunk plugin unloading")

    async def _uninstall(self):
        decky.logger.info("punktfunk plugin uninstalled")
