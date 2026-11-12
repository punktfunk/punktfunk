"""The Steam Input layout that hands the Deck's touchscreen to the client as native touch.

The touchscreen reaches the app as ``wl_touch`` only while a layout with "Touchscreen Native
Support" (``controller_action ts_n``) is active for the game. The plugin ships one
(``controller_config/punktfunk.vdf``, built on Steam's gamepad-fps template), copies it into
``controller_base/templates/`` so it is also a selectable template, and points each account's
configset entry for the shortcut's name at it. Steam keys a non-Steam game by its LOWERCASE
name, so the shortcuts named "Punktfunk" share one entry. ``controller_neptune`` is the Deck's
built-in controller type.
"""

import os
import shutil
from pathlib import Path

import decky

CONTROLLER_TEMPLATE = "punktfunk.vdf"


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
    """Every Steam account's controller-config dir holding configset_controller_neptune.vdf."""
    base = _steam_root() / "steamapps" / "common" / "Steam Controller Configs"
    return [p / "config" for p in sorted(base.glob("*")) if (p / "config").is_dir()]


def _upsert_configset_entry(text: str, key: str, source_type: str, source_val: str) -> str:
    """Set the top-level ``"<key>" { "<source_type>" "<source_val>" }`` block in a
    configset_controller_neptune.vdf: replace the block for that key (case-insensitive), or insert
    one before the file's final closing brace. Every other game's entry stays byte-for-byte
    intact. An empty file gets the ``"controller_config" { }`` skeleton."""
    block = f'\t"{key}"\n\t{{\n\t\t"{source_type}"\t\t"{source_val}"\n\t}}\n'
    if '"controller_config"' not in text:
        return '"controller_config"\n{\n' + block + "}\n"

    lower = text.lower()
    needle = f'"{key.lower()}"'
    # Find the key token that begins a top-level entry (its own line), then its "{ … }" block.
    search_from = 0
    while True:
        idx = lower.find(needle, search_from)
        if idx == -1:
            break
        # Must be a standalone key line (preceded only by whitespace back to a newline).
        line_start = text.rfind("\n", 0, idx) + 1
        if text[line_start:idx].strip() != "":
            search_from = idx + len(needle)
            continue
        brace = text.find("{", idx)
        if brace == -1:
            break
        depth = 0
        i = brace
        while i < len(text):
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
                if depth == 0:
                    break
            i += 1
        end = i + 1
        # Consume the trailing newline after the block so we don't accumulate blank lines.
        if end < len(text) and text[end] == "\n":
            end += 1
        return text[:line_start] + block + text[end:]

    # Not present — insert before the last closing brace (the controller_config block's end).
    last_close = text.rstrip().rfind("}")
    if last_close == -1:
        return text.rstrip() + "\n" + block
    return text[:last_close] + block + text[last_close:]


async def apply_controller_config(name: str) -> dict:
    """Install the layout and point every account's configset entry for ``name`` at it:
    ``{ok, applied, errors, accounts}``, or ``template-missing``. Best-effort and idempotent: a
    controller tweak never blocks a launch, so failures are reported, not raised."""
    src = _controller_template_src()
    if not src.exists():
        return {"ok": False, "error": "template-missing", "detail": str(src)}
    key = name.strip().lower()
    applied: list[str] = []
    errors: list[str] = []
    # 1) Ship it as a selectable template (also the safe fallback if Steam clobbers the
    #    configset write on exit): controller_base/templates/punktfunk.vdf.
    try:
        tdir = _steam_root() / "controller_base" / "templates"
        tdir.mkdir(parents=True, exist_ok=True)
        dst = tdir / CONTROLLER_TEMPLATE
        shutil.copyfile(src, dst)
        _chown_like_parent(dst)
        applied.append("template")
    except OSError as e:
        errors.append(f"template: {e}")
    # 2) Point each Steam account's configset at that template for our game key.
    dirs = _configset_dirs()
    for d in dirs:
        f = d / "configset_controller_neptune.vdf"
        try:
            text = f.read_text(encoding="utf-8") if f.exists() else ""
            new = _upsert_configset_entry(text, key, "template", CONTROLLER_TEMPLATE)
            if new != text:
                if f.exists():  # keep one recoverable backup before our first edit
                    bak = f.with_name(f.name + ".pf-bak")
                    if not bak.exists():
                        shutil.copyfile(f, bak)
                        _chown_like_parent(bak)
                existed = f.exists()
                f.write_text(new, encoding="utf-8")
                if not existed:  # a new file takes its parent's owner, as Steam's own do
                    _chown_like_parent(f)
            applied.append(f"configset:{d.parent.name}")
        except OSError as e:
            errors.append(f"{d.parent.name}: {e}")
    decky.logger.info(
        "apply_controller_config key=%s applied=%s errors=%s", key, applied, errors
    )
    return {"ok": not errors, "applied": applied, "errors": errors, "accounts": len(dirs)}
