"""This device's own host, through its CLI: what may be synced into Steam, an entry's art, and the
map of what was synced.

The plugin reads none of the host's files and dials none of its ports: ``punktfunk-host ctl``
reads the host's token itself. Every call runs the one binary :func:`host_bin` finds, which is
also the path the synced entries name in Steam's launch options.
"""

import base64
import json
import os
import re
import shutil
from pathlib import Path

import decky

from . import art, client

HOST_BIN = "punktfunk-host"
# What a SteamOS install from source builds, when no unit and no package names a binary.
_SOURCE_BUILD = ("punktfunk", "target-steamos", "release", HOST_BIN)


def _unit_bin(text: str, home: str) -> str | None:
    """The binary of a unit's last ``ExecStart=``: ``%h`` expanded, systemd's ``-@:+!`` prefixes
    dropped. None for a reset (``ExecStart=``) or a command that is not ``punktfunk-host``."""
    lines = [ln.strip()[len("ExecStart="):] for ln in text.splitlines()
             if ln.strip().startswith("ExecStart=")]
    words = lines[-1].split() if lines else []
    if not words:
        return None
    path = words[0].lstrip("-@:+!").replace("%h", home)
    return path if path.startswith("/") and Path(path).name == HOST_BIN else None


def host_bin() -> str | None:
    """Absolute path of this device's ``punktfunk-host``: the user unit's ``ExecStart=``, else the
    client's prefixes, else a source build. None when none of them is a file."""
    home = decky.DECKY_USER_HOME
    unit = Path(home) / ".config" / "systemd" / "user" / "punktfunk-host.service"
    try:
        found = _unit_bin(unit.read_text(encoding="utf-8"), home)
    except OSError:
        found = None
    if found and Path(found).is_file():
        return found
    found = shutil.which(
        HOST_BIN, path=os.environ.get("PATH", "") + ":" + ":".join(client._NATIVE_PREFIXES)
    )
    if found:
        return found
    built = Path(home).joinpath(*_SOURCE_BUILD)
    return str(built) if built.is_file() else None


async def _run_host(
    args: list[str], timeout: float = 20.0, stdin_text: str | None = None, text: bool = True
) -> tuple[int, str | bytes, str]:
    """Run ``punktfunk-host <args>`` as :func:`client._exec` runs the CLI; ``text=False`` keeps
    stdout as bytes. ``(-1, "", "")`` when there is no host binary or the call times out."""
    path = host_bin()
    return await client._exec(
        path and [path, *args], f"host {' '.join(args[:3])}", timeout,
        stdin_text=stdin_text, text=text,
    )


def _failure(rc: int, err: str, what: str) -> dict:
    detail = (err.strip().splitlines() or [what])[-1]
    decky.logger.warning("host %s (rc=%s): %s", what, rc, detail)
    return {"ok": False, "error": "host-unavailable" if rc == -1 else "host-error"}


async def entries() -> dict:
    """What may be synced (``ctl steam entries --json``): ``{ok, steam_owns_desk, sources,
    entries}``, else ``{ok: False, error}``. A zero exit with no JSON object is a failure."""
    rc, out, err = await _run_host(["ctl", "steam", "entries", "--json"], timeout=30.0)
    data = client._stdout_json(out) if rc == 0 else {}
    if not data:
        return _failure(rc, err, "steam entries unread")
    return {**data, "ok": True}


# A library id rides argv: no leading `-`, no control character.
_GAME_ID = re.compile(r"[^\-\x00-\x1f\x7f][^\x00-\x1f\x7f]{0,511}")
_ART_KINDS = ("portrait", "hero", "logo", "header")
_IMAGE_TYPES = ((b"\x89PNG", "png"), (b"\xff\xd8\xff", "jpg"), (b"GIF8", "gif"))


def _image_type(data: bytes) -> str | None:
    """What the bytes are: png, jpg, gif or webp, else None."""
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return "webp"
    return next((kind for magic, kind in _IMAGE_TYPES if data.startswith(magic)), None)


async def entry_art(game_id: str, kind: str) -> dict:
    """One image of a library title (``ctl steam art <id> <kind>``): ``{ok, data, type}``, the
    bytes base64. ``bad-input`` for an id or kind off the shape, ``bad-image`` for bytes that are
    no image or over :data:`art._MAX_ART_BYTES`."""
    game_id, kind = str(game_id), str(kind)
    if kind not in _ART_KINDS or not _GAME_ID.fullmatch(game_id):
        return {"ok": False, "error": "bad-input"}
    rc, out, err = await _run_host(["ctl", "steam", "art", game_id, kind], text=False)
    if rc != 0:
        return _failure(rc, err, "steam art unread")
    fmt = _image_type(out) if len(out) <= art._MAX_ART_BYTES else None
    if not fmt:
        return {"ok": False, "error": "bad-image"}
    return {"ok": True, "data": base64.b64encode(out).decode(), "type": fmt}


def _app_id(n: object) -> bool:
    return type(n) is int and 0 < n < 2**32


async def put_map(steam_map: dict) -> dict:
    """Send the map of synced entries (``ctl steam map``, the JSON on stdin): ``{ok}``. A map off
    the shape (``version`` 1, ``entries`` id → 32-bit app id, ``own`` app ids) never reaches the
    host: ``bad-input``."""
    m = steam_map if isinstance(steam_map, dict) else {}
    synced, own = m.get("entries"), m.get("own")
    if (
        m.get("version") != 1
        or not isinstance(synced, dict)
        or not isinstance(own, list)
        or not all(isinstance(k, str) and _app_id(v) for k, v in synced.items())
        or not all(_app_id(n) for n in own)
    ):
        return {"ok": False, "error": "bad-input"}
    body = json.dumps({"version": 1, "entries": synced, "own": own})
    rc, _out, err = await _run_host(["ctl", "steam", "map"], stdin_text=body)
    return {"ok": True} if rc == 0 else _failure(rc, err, "steam map not sent")
