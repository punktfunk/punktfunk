"""The running stream, from the panel: is a client up, press a host button, end the game, and
force-stop a wedged client.

The session binary serves a control socket, one text verb per connection with ``ok``/``err``
back (``clients/session/src/main.rs``, ``ctl_socket``). A flatpak client puts it in its app
runtime dir, the one runtime path the sandbox and this backend see identically; a native client
in the plain runtime dir. A client is up only while its socket accepts a connection: the file
outlives the process.
"""

import asyncio
import os
import shutil
from pathlib import Path

import decky

from . import client


def _ctl_sockets() -> list[Path]:
    """Candidate paths of the client's control socket: each flatpak app id's runtime dir first,
    then the plain runtime dir."""
    uid = os.environ.get("PF_UID") or "1000"
    run = Path(f"/run/user/{uid}")
    return [
        run / "app" / client.APP_ID / "punktfunk-session-ctl.sock",
        run / "app" / client.LEGACY_APP_ID / "punktfunk-session-ctl.sock",
        run / "punktfunk-session-ctl.sock",
    ]


async def kill_stream() -> dict:
    """Force-stop a wedged client: ``flatpak kill`` for the sandboxed one, SIGTERM by name for a
    native install, which has no flatpak instance to kill."""
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


async def _accepts(sock: Path) -> bool:
    """Does a client hold this socket? A client that exited leaves the file behind, so the file
    alone proves nothing: only a live one accepts a connection."""
    if not sock.is_socket():
        return False
    try:
        _reader, writer = await asyncio.wait_for(
            asyncio.open_unix_connection(str(sock)), timeout=2.0
        )
    except Exception:  # noqa: BLE001 — refused or timed out: nobody is listening
        return False
    writer.close()
    return True


async def stream_running() -> dict:
    """Whether a client is up: one of its control sockets accepts. The socket lives as long as
    the client process, so it stays up between console-mode streams."""
    for sock in _ctl_sockets():
        if await _accepts(sock):
            return {"running": True}
    return {"running": False}


async def end_game(ref: str, game_id: str) -> dict:
    """End a title this device launched on the host (``punktfunk end-game <ref> --game ID``).

    ``{ok: True, notice}`` once the game is gone, ended now or already stopped (exit 5). Anything
    else is ``{ok: False, error: <exit code>, notice}``, the notice being the CLI's own sentence.
    A ref or id starting with ``-`` is refused as ``{ok: False, error: "unresolved"}``, with no
    notice, never handed to the CLI as a flag.
    """
    ref, game_id = str(ref).strip(), str(game_id).strip()
    if not ref or ref.startswith("-") or not game_id or game_id.startswith("-"):
        return {"ok": False, "error": "unresolved"}
    rc, out, err = await client._run_cli(["end-game", ref, "--game", game_id], timeout=20.0)
    lines = (out.strip() or err.strip()).splitlines()
    notice = lines[-1] if lines else ""
    if rc in (0, 5):
        return {"ok": True, "notice": notice}
    return {"ok": False, "error": str(rc), "notice": notice}


async def host_action(action: str) -> dict:
    """Press a HOST system button on the running stream: ``guide`` (the Steam/Xbox/PS menu
    button) or ``qam`` (the quick-access ``…``), through the first control socket that accepts a
    connection. ``no-stream`` when none does."""
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
