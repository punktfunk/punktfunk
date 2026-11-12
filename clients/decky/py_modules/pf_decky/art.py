"""Artwork for the Steam shortcuts: the plugin's own, and a game's own for its stream shortcut.

A stream started from a game's page runs under a hidden shortcut named after the game, so the
overlay, the "now playing" surfaces and friends see the game, not "Punktfunk", and the art has to
match. Steam keeps every library image it has shown in ``appcache/librarycache``; what is not
there is on the store CDN under the same file name. Art is cosmetic: a missing piece is omitted,
never a failure, and no launch waits on it.
"""

import asyncio
import base64
import re
import urllib.request
from pathlib import Path

import decky

from . import steam_input, update

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


async def shortcut_art() -> dict:
    """The plugin's own shortcut art (``assets/``): base64 PNGs keyed grid / gridwide / hero /
    logo for SetCustomArtworkForApp, and ``icon_path`` for SetShortcutIcon, which wants a file.
    A missing file is omitted."""
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


async def game_art(appid: int, icon_hash: str) -> dict:
    """A Steam game's own art for its stream shortcut: base64 grid / hero / logo / gridwide,
    each with its ``<key>_type``, and the icon as bytes for :func:`save_icon` to land as PNG.
    ``bad-appid`` for an appid that is not a positive integer. ``icon_hash`` is validated to 40 hex
    characters, because it becomes part of a URL and a cache path."""
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


async def save_icon(appid: int, png_base64: str) -> dict:
    """Write a per-game shortcut's icon as ``<appid>.png`` and return the path SetShortcutIcon
    wants: ``{ok, path}``, or ``bad-input`` unless it is real PNG bytes for a positive appid.
    Steam's shortcut icons are PNG or ICO, so the frontend converts the game's JPG first."""
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
        dest.chmod(0o644)  # Steam reads it as the Deck's user, whoever runs this backend
    except OSError as e:
        return {"ok": False, "error": "write-failed", "detail": str(e)}
    return {"ok": True, "path": str(dest)}
