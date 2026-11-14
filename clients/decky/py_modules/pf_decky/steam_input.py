"""The Steam Input layouts the plugin points its shortcuts at.

The touchscreen reaches the app as ``wl_touch`` only while a layout with "Touchscreen Native
Support" (``controller_action ts_n``) is active for the game. The plugin ships one
(``controller_config/punktfunk.vdf``, built on Steam's gamepad-fps template), copies it into
``controller_base/templates/`` so it is also a selectable template, and points each account's
configset entry for the shortcut's name at it. Steam keys a non-Steam game by its LOWERCASE
name, so the shortcuts named "Punktfunk" share one entry.

The client forwards a Steam Controller raw, so every key or mouse binding of its Deck-side
layout doubles on the host. Both Steam Controllers get a gamepad-only layout too.
"""

import os
import shutil
from pathlib import Path

import decky

CONTROLLER_TEMPLATE = "punktfunk.vdf"
TRITON_TEMPLATE = "punktfunk_triton.vdf"

# Steam's controller type → the template its configset entry names. The Steam Controller 2
# (triton) has the Deck's inputs, so it takes our layout retyped; the 2015 one (gordon) takes
# Steam's own "Gamepad with Joystick Camera".
LAYOUTS = {
    "controller_neptune": CONTROLLER_TEMPLATE,
    "controller_triton": TRITON_TEMPLATE,
    "controller_steamcontroller_gordon": "gamepad_joystick.vdf",
}


def _steam_root() -> Path:
    """Steam's base dir on SteamOS (~/.steam/steam symlinks here)."""
    return Path(decky.DECKY_USER_HOME) / ".local" / "share" / "Steam"


def _controller_template_src() -> Path:
    return Path(decky.DECKY_PLUGIN_DIR) / "controller_config" / CONTROLLER_TEMPLATE


def _chown_like_parent(path: Path) -> None:
    """Give ``path`` its parent dir's owner, so Steam keeps write access. A no-op while the backend
    runs as the Deck's user; it matters if ``plugin.json`` gains the ``root`` flag. Best-effort."""
    try:
        st = path.parent.stat()
        os.chown(path, st.st_uid, st.st_gid)
    except OSError:
        pass


def _configset_dirs() -> list[Path]:
    """Every Steam account's controller-config dir: one ``configset_<type>.vdf`` per type."""
    base = _steam_root() / "steamapps" / "common" / "Steam Controller Configs"
    return [p / "config" for p in sorted(base.glob("*")) if (p / "config").is_dir()]


def _vdf_tokens(text: str):
    """Yield ``(start, end, token)`` for each VDF token: ``{``, ``}``, or a quoted string with
    its quotes. A brace inside quotes is text, and a backslash escapes the next character, as
    Steam writes them."""
    i, n = 0, len(text)
    while i < n:
        if text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            yield i, j + 1, text[i : j + 1]
            i = j + 1
        else:
            if text[i] in "{}":
                yield i, i + 1, text[i]
            i += 1


def _upsert_configset_entry(text: str, key: str, source_type: str, source_val: str) -> str:
    """Set the top-level ``"<key>" { "<source_type>" "<source_val>" }`` block in a
    ``configset_<type>.vdf``: replace the block of the depth-1 key (case-insensitive), or insert
    one before the last top-level closing brace. The key is written escaped and braces count
    only outside quotes, so every other game's entry stays byte-for-byte intact. An empty file
    gets the ``"controller_config" { }`` skeleton."""
    quoted = '"' + key.replace("\\", "\\\\").replace('"', '\\"') + '"'
    block = f'\t{quoted}\n\t{{\n\t\t"{source_type}"\t\t"{source_val}"\n\t}}\n'
    if '"controller_config"' not in text:
        return '"controller_config"\n{\n' + block + "}\n"

    want = quoted.lower()
    depth, prev, found, last_close = 0, None, None, -1
    for start, end, tok in _vdf_tokens(text):
        if tok == "{":
            depth += 1
            # A string followed by "{" one level into controller_config is a game's key.
            if depth == 2 and prev is not None and prev[1].lower() == want:
                found = prev[0]
        elif tok == "}":
            depth -= 1
            if depth == 1 and found is not None:
                line_start = text.rfind("\n", 0, found) + 1
                cut = line_start if not text[line_start:found].strip() else found
                # Take the block's own newline too, so rewrites don't pile up blank lines.
                if text[end : end + 1] == "\n":
                    end += 1
                return text[:cut] + block + text[end:]
            if depth == 0:
                last_close = start
        prev = (start, tok)

    if last_close == -1:
        return text.rstrip() + "\n" + block
    return text[:last_close] + block + text[last_close:]


def _point_configset(f: Path, key: str, template: str) -> None:
    """Point ``key`` at ``template`` in configset ``f``, keeping one backup of the original."""
    text = f.read_text(encoding="utf-8") if f.exists() else ""
    new = _upsert_configset_entry(text, key, "template", template)
    if new == text:
        return
    if f.exists():
        bak = f.with_name(f.name + ".pf-bak")
        if not bak.exists():
            shutil.copyfile(f, bak)
            _chown_like_parent(bak)
    existed = f.exists()
    f.write_text(new, encoding="utf-8")
    if not existed:  # a new file takes its parent's owner, as Steam's own do
        _chown_like_parent(f)


async def apply_controller_config(name: str) -> dict:
    """Install the layouts and point every account's configset entry for ``name`` at them, one
    per type in ``LAYOUTS``: ``{ok, applied, errors, accounts}``, or ``template-missing``.
    Best-effort and idempotent: a controller tweak never blocks a launch, so failures are
    reported, not raised."""
    src = _controller_template_src()
    if not src.exists():
        return {"ok": False, "error": "template-missing", "detail": str(src)}
    key = name.strip().lower()
    applied: list[str] = []
    errors: list[str] = []
    # 1) Ship ours as selectable templates (also the fallback if Steam clobbers the configset
    #    write on exit). The triton copy differs only in its controller_type line.
    try:
        tdir = _steam_root() / "controller_base" / "templates"
        tdir.mkdir(parents=True, exist_ok=True)
        text = src.read_text(encoding="utf-8")
        triton = text.replace('"controller_neptune"', '"controller_triton"')
        for file, body in ((CONTROLLER_TEMPLATE, text), (TRITON_TEMPLATE, triton)):
            dst = tdir / file
            dst.write_text(body, encoding="utf-8")
            _chown_like_parent(dst)
        applied.append("template")
    except OSError as e:
        errors.append(f"template: {e}")
    # 2) Point each Steam account's configsets at them for our game key.
    dirs = _configset_dirs()
    for d in dirs:
        try:
            for ctype, template in LAYOUTS.items():
                _point_configset(d / f"configset_{ctype}.vdf", key, template)
            applied.append(f"configset:{d.parent.name}")
        except OSError as e:
            errors.append(f"{d.parent.name}: {e}")
    decky.logger.info(
        "apply_controller_config key=%s applied=%s errors=%s", key, applied, errors
    )
    return {"ok": not errors, "applied": applied, "errors": errors, "accounts": len(dirs)}
