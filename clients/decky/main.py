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
from pf_decky import art, client, steam_input, update

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
        return await art.shortcut_art()

    async def game_art(self, appid: int, icon_hash: str = "") -> dict:
        return await art.game_art(appid, icon_hash)

    async def save_icon(self, appid: int, png_base64: str) -> dict:
        return await art.save_icon(appid, png_base64)

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
