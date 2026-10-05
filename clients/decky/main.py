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

# Flatpak application id of the GTK client (packaging/flatpak/io.unom.Punktfunk.yml).
APP_ID = "io.unom.Punktfunk"

def _runner_path() -> str:
    """Absolute path to the launch wrapper shipped with the plugin (bin/punktfunkrun.sh)."""
    return str(Path(decky.DECKY_PLUGIN_DIR) / "bin" / "punktfunkrun.sh")


# --- Steam Input controller config injection (native touchscreen via the ts_n command) --------
# The Deck's touchscreen only reaches the app as native wl_touch when a Steam Input layout with
# the "Touchscreen Native Support" (controller_action ts_n) command is active for the game. We
# ship that layout (controller_config/punktfunk.vdf, built on Steam's gamepad-fps template) and
# point our shortcuts at it, EmuDeck-style: drop it in controller_base/templates/ (so it is also
# a selectable "Punktfunk" template) AND set each account's configset entry for our shortcut's
# game key to that template. Steam keys non-Steam games by their LOWERCASE NAME (verified on the
# Deck: our "Punktfunk" shortcut → the "punktfunk" configset key), so both our shortcuts (same
# name) share one entry. controller_neptune = the Deck's built-in controller type.
CONTROLLER_TEMPLATE = "punktfunk.vdf"


def _steam_root() -> Path:
    """Steam's base dir on SteamOS (~/.steam/steam symlinks here)."""
    return Path(decky.DECKY_USER_HOME) / ".local" / "share" / "Steam"


def _controller_template_src() -> Path:
    return Path(decky.DECKY_PLUGIN_DIR) / "controller_config" / CONTROLLER_TEMPLATE


def _chown_like_parent(path: Path) -> None:
    """The Decky backend runs as root, so files it CREATES in the deck-owned Steam tree land
    root-owned — which would stop Steam (running as the user) from rewriting them. Match the
    parent dir's owner so Steam retains write access. Best-effort."""
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
    configset_controller_neptune.vdf, replacing any existing block for that key (case-insensitive)
    or inserting one before the file's final closing brace. Targeted (only our key is touched) so
    the hundreds of other game entries stay byte-for-byte intact. Creates the wrapping
    ``"controller_config" { }`` skeleton when the file is empty/new."""
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


# ----------------------------------------------------------------------------------------
# Self-update check (no Decky store). The plugin is distributed via "Install Plugin from
# URL" pointing at our Gitea generic registry, so the official store never sees it and
# can't offer updates. Instead the backend polls a tiny per-channel ``manifest.json`` the
# CI publishes next to the zip, compares it to the installed version, and the frontend
# offers a one-tap update that drives Decky's own (root, privileged) install RPC. The
# channel + manifest URL are baked into ``update.json`` by CI (.gitea/workflows/decky.yml);
# a dev/sideload build has no ``update.json`` and update checks are simply disabled.
_UPDATE_TTL_S = 1800.0  # cache a successful check for 30 min (the QAM remounts often)
_update_cache: dict = {"at": 0.0, "data": None}


def _update_config() -> dict:
    """The CI-baked ``{channel, manifest}`` next to the plugin (absent on dev builds)."""
    try:
        return json.loads((Path(decky.DECKY_PLUGIN_DIR) / "update.json").read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def _installed_version() -> str:
    """The version Decky itself reports for this plugin — it reads ``package.json`` (NOT
    plugin.json), so the CI stamps the build version there."""
    try:
        pkg = json.loads((Path(decky.DECKY_PLUGIN_DIR) / "package.json").read_text())
        return str(pkg.get("version", "0.0.0"))
    except (OSError, json.JSONDecodeError):
        return "0.0.0"


def _semver_tuple(v: str) -> tuple[int, int, int]:
    """A tolerant (major, minor, patch) tuple for ``>`` comparison. We control the version
    format (plain numeric ``X.Y.Z`` on both channels), so leading-int-per-component is
    enough; any pre-release suffix is dropped before comparing."""
    parts: list[int] = []
    for comp in str(v).split("-", 1)[0].split(".")[:3]:
        digits = ""
        for ch in comp:
            if ch.isdigit():
                digits += ch
            else:
                break
        parts.append(int(digits) if digits else 0)
    while len(parts) < 3:
        parts.append(0)
    return (parts[0], parts[1], parts[2])


# Decky Loader ships its own embedded (PyInstaller) Python whose compiled-in OpenSSL default
# verify paths don't exist on SteamOS — ``ssl.create_default_context()`` then trusts NOTHING
# and every HTTPS fetch dies with CERTIFICATE_VERIFY_FAILED (seen live on the Deck). Fix: find
# a real CA bundle on disk and load it explicitly. Verification is NEVER disabled — if no
# bundle exists the fetch just fails, and check_update() is non-fatal by design.
_CA_BUNDLES = (
    "/etc/ssl/certs/ca-certificates.crt",  # SteamOS / Arch / Debian / Ubuntu
    "/etc/ssl/cert.pem",  # Arch/openssl compat symlink
    "/etc/pki/tls/certs/ca-bundle.crt",  # Fedora / Bazzite
    "/etc/ssl/ca-bundle.pem",  # openSUSE
)
_ssl_context_cache: ssl.SSLContext | None = None


def _build_ssl_context() -> ssl.SSLContext:
    """A verifying SSLContext that actually has CA roots under Decky's embedded Python."""
    ctx = ssl.create_default_context()  # honors SSL_CERT_FILE / SSL_CERT_DIR when set
    if ctx.cert_store_stats().get("x509_ca", 0):
        return ctx  # the interpreter found its own roots (e.g. a system python)

    dvp = ssl.get_default_verify_paths()
    candidates: list[str | None] = [dvp.cafile, dvp.openssl_cafile, *_CA_BUNDLES]
    try:  # not shipped by Decky's runtime, but honor it when importable
        import certifi

        candidates.append(certifi.where())
    except ImportError:
        pass

    tried: set[str] = set()
    for cafile in candidates:
        if not cafile or cafile in tried or not Path(cafile).is_file():
            continue
        tried.add(cafile)
        try:
            ctx.load_verify_locations(cafile=cafile)
        except (ssl.SSLError, OSError):
            continue
        if ctx.cert_store_stats().get("x509_ca", 0):
            decky.logger.info("TLS roots loaded from %s", cafile)
            return ctx

    decky.logger.warning(
        "no CA bundle found — HTTPS update checks will fail certificate verification"
    )
    return ctx


def _ssl_context() -> ssl.SSLContext:
    """The (cached) context for registry fetches; building it scans disk, so do it once."""
    global _ssl_context_cache
    if _ssl_context_cache is None:
        _ssl_context_cache = _build_ssl_context()
    return _ssl_context_cache


def _fetch_json(url: str, timeout: float = 8.0) -> dict:
    """Blocking HTTPS GET of a small JSON document (run in an executor)."""
    req = urllib.request.Request(
        url, headers={"Accept": "application/json", "User-Agent": "punktfunk-decky"}
    )
    with urllib.request.urlopen(req, timeout=timeout, context=_ssl_context()) as resp:
        return json.loads(resp.read().decode("utf-8", errors="replace"))


# A library hero is a few hundred KB; nothing fetched here is a tenth of this. The cap keeps a
# wrong URL from reading a stream into the plugin's memory.
_MAX_ART_BYTES = 8 * 1024 * 1024


def _fetch_bytes(url: str, timeout: float = 10.0) -> bytes:
    """Blocking HTTPS GET of a small binary (run in an executor). Raises on any failure, an
    over-long body included — a truncated image is worse than falling through to the next CDN."""
    req = urllib.request.Request(url, headers={"User-Agent": "punktfunk-decky"})
    with urllib.request.urlopen(req, timeout=timeout, context=_ssl_context()) as resp:
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
    base = _steam_root() / "appcache" / "librarycache"
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
    cached = _steam_root() / "appcache" / "librarycache" / str(appid) / f"{icon_hash}.jpg"
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


def _flatpak() -> str | None:
    return shutil.which("flatpak") or (
        "/usr/bin/flatpak" if Path("/usr/bin/flatpak").exists() else None
    )


# --- which client is installed -------------------------------------------------------------
#
# The flatpak is the Deck's usual client, but it is not the only one: a sysext, a .deb/.rpm, an
# AUR build, a nix profile and a hand-built binary all install a NATIVE `punktfunk-client`, and
# on those the plugin used to be dead in the water — every headless call went through
# `flatpak run io.unom.Punktfunk` and simply failed. Both kinds keep identity, known-hosts and
# settings in the same ~/.config/punktfunk (the flatpak's sandbox HOME resolves to the real
# home), so nothing else in this file has to care which one answered.
NATIVE_BIN = "punktfunk-client"
# The headless CLI — the door this backend does almost everything through (discover, hosts,
# pair, trust). Shipped beside the GTK client in every package since v0.22.0: /app/bin in the
# flatpak, the same bindir as `punktfunk-client` natively.
CLI_BIN = "punktfunk"

# Prefixes to try when PATH doesn't have it. The Decky backend runs with a minimal PATH, and
# SteamOS's read-only /usr pushes native installs into a sysext or the user's own prefix.
_NATIVE_PREFIXES = (
    "/usr/bin",
    "/usr/local/bin",
    "/run/host/usr/bin",
    "/var/lib/extensions/punktfunk/usr/bin",
)


def _native_client() -> str | None:
    """Absolute path of a native (non-flatpak) client binary, or None."""
    found = shutil.which(NATIVE_BIN, path=os.environ.get("PATH", "") + ":" + ":".join(_NATIVE_PREFIXES))
    if found:
        return found
    for prefix in (str(Path(decky.DECKY_USER_HOME) / ".local" / "bin"),):
        candidate = Path(prefix) / NATIVE_BIN
        if candidate.exists():
            return str(candidate)
    return None


# The client flatpak ships x86_64 and aarch64; flatpak names arches the way uname does.
_FLATPAK_ARCH = os.uname().machine


def _flatpak_ref() -> dict | None:
    """The INSTALLED client flatpak resolved to a SCOPE and a BRANCH, or None when there is none.

    ``{"scope": "--user"|"--system", "branch": "canary", "ref": "io.unom.Punktfunk//canary"}``.

    ⭐⭐ **Naming no branch is not a shorthand for "the only one".** flatpak refuses an ambiguous
    ref rather than guessing at one, and the ambiguity does not need two branches *installed*:
    the punktfunk remote publishes `stable` AND `canary`, so an unqualified
    ``flatpak remote-info <origin> io.unom.Punktfunk`` errors with "Multiple branches available"
    on a Deck that has exactly one. That error is why the client update check silently answered
    "up to date" on every Deck — so every query downstream now names the ref in full.

    Read off the exported tree rather than by shelling out to ``flatpak list``, because
    :func:`_client_argv` is on the path of every headless call and a subprocess per call would be
    absurd (the same reason :func:`_flatpak_installed` reads the filesystem). ``active`` is the
    symlink flatpak points at the deployed commit — its presence is what makes a branch directory
    an INSTALL rather than the leftovers of one.

    With more than one branch installed, `stable` wins, because that is the branch a plain
    ``flatpak run`` resolves to: the check has to describe the client the launcher really starts,
    or a stale `stable` silently beats a current `canary` in both places at once.
    """
    if not _flatpak():
        return None
    for root, scope in (
        (Path(decky.DECKY_USER_HOME) / ".local" / "share" / "flatpak", "--user"),
        (Path("/var/lib/flatpak"), "--system"),
    ):
        try:
            branches = sorted(
                p.name for p in (root / "app" / APP_ID / _FLATPAK_ARCH).iterdir()
                if (p / "active").exists()
            )
        except OSError:
            continue  # not installed in this scope
        if not branches:
            continue
        branch = "stable" if "stable" in branches else branches[0]
        if len(branches) > 1:
            decky.logger.warning(
                "%s is installed on %d branches (%s) — using %s, the one `flatpak run` resolves "
                "to; uninstall the others so the client you launch is the client we update",
                APP_ID, len(branches), ", ".join(branches), branch,
            )
        return {"scope": scope, "branch": branch, "ref": f"{APP_ID}//{branch}"}
    return None


def _flatpak_installed() -> bool:
    """True when the flatpak APP is actually installed — not merely that `flatpak` exists.

    Checked by the app's own exported directory rather than by shelling out to `flatpak info`,
    because this is on the path of every headless call and a subprocess per call would be absurd.
    Both scopes count: the Deck installs --user, a distro image may ship it system-wide.
    """
    return _flatpak_ref() is not None


def _client_argv() -> list[str] | None:
    """The argv PREFIX that runs the client headlessly, or None when no client is installed.

    Flatpak wins when it is installed: it is the tested Deck path, so an existing install keeps
    behaving exactly as it did. A native binary is the fallback — and on a machine with no
    flatpak client, the thing that makes the plugin work at all. `PF_DECKY_CLIENT=native|flatpak`
    forces one when a machine has both.

    The branch is PINNED (`--branch=`, which keeps the app id last — :func:`_cli_argv` appends
    `--command=` and flatpak treats everything after the id as the app's own argv), so the client
    this launches is the exact ref :func:`_client_update_state` checks and :meth:`Plugin.
    update_client` updates.
    """
    forced = os.environ.get("PF_DECKY_CLIENT", "").strip().lower()
    native = _native_client()
    if forced == "native":
        return [native] if native else None
    ref = _flatpak_ref()
    if forced != "flatpak" and not ref and native:
        return [native]
    if ref:
        return [_flatpak(), "run", f"--arch={_FLATPAK_ARCH}", f"--branch={ref['branch']}", APP_ID]
    return [native] if native else None


def _cli_argv() -> list[str] | None:
    """The argv PREFIX that runs the headless CLI, or None when no client is installed.

    Exactly the shape the old ``_session_argv`` used, pointed at ``punktfunk`` instead: the
    flatpak ships both binaries in /app/bin so ``--command=`` picks the other one (**the app id
    stays LAST** — flatpak treats everything after it as the app's own argv), and a native
    install puts the CLI in the same bindir as ``punktfunk-client``, so it is its sibling.
    """
    prefix = _client_argv()
    if not prefix:
        return None
    if prefix[0] == _flatpak():
        return [*prefix[:-1], f"--command={CLI_BIN}", prefix[-1]]
    sibling = Path(prefix[0]).with_name(CLI_BIN)
    return [str(sibling)] if sibling.exists() else None


async def _run_cli(
    args: list[str], timeout: float = 20.0, stdin_text: str | None = None
) -> tuple[int, str, str]:
    """Run the headless CLI, returning ``(returncode, stdout, stderr)``. SEPARATE pipes: stdout
    is the machine interface (JSON/TSV) and stderr carries the log lines, and merging them would
    corrupt every payload. ``(-1, "", "")`` when no client is installed or the call times out.

    ``stdin_text`` is written to the child and the pipe closed — the way a secret reaches the CLI,
    because argv does not qualify: ``/proc/*/cmdline`` is world-readable, so every process on the
    Deck can read a flag's value. See :meth:`Plugin.pair`.

    The same ``_flatpak_env`` repair the client runs needed applies here unchanged — Decky's
    PyInstaller ``LD_LIBRARY_PATH`` leak breaks the flatpak's libcurl whatever binary inside the
    sandbox is being started."""
    prefix = _cli_argv()
    return await _exec(
        prefix and [*prefix, *args], f"cli {' '.join(args)}", timeout, stdin_text=stdin_text
    )


# The CLI's exit-code contract (clients/cli/src/main.rs): 0 ok, 2 connect failed, 3 trust
# rejected, 4 renderer, 5 could not resolve what was asked for, 6 needs a person. Mapped to the
# stable strings the panel renders, so a reworded message can never change what the UI shows.
_CLI_ERRORS = {
    2: "unreachable",
    3: "refused",
    5: "unresolved",
    6: "needs-pairing",
}


def _cli_error(rc: int, stderr: str) -> str:
    """One stable error code for a nonzero CLI exit.

    The interesting case is a client too old for the verb we just used. That announces itself
    DETERMINISTICALLY — exit 5 plus ``unknown command "<verb>"`` on stderr — rather than by the
    guesswork the GTK headless modes needed, so the panel can say "update the client" with
    confidence and offer the button that fixes it."""
    if rc == -1:
        return "client-unavailable"
    if rc == 5 and "unknown command" in stderr:
        return "client-outdated"
    return _CLI_ERRORS.get(rc, "client-error")


async def _cli_json(args: list[str], timeout: float = 20.0) -> dict:
    """Run the CLI and parse its stdout as JSON. ``{"ok": True, **payload}`` on success, else
    ``{"ok": False, "error": <code>, "detail": <the CLI's last stderr line>}``.

    A zero exit with unparseable stdout is a failure, not an empty result: silently returning
    "no hosts" for a broken client is exactly the answer a user cannot debug."""
    rc, out, err = await _run_cli(args, timeout=timeout)
    if rc == 0:
        try:
            data = json.loads(out)
            if isinstance(data, dict):
                # `ok` last: a payload that ever grows its own `ok` key must not be able to
                # report failure through the field this layer owns.
                return {**data, "ok": True}
        except json.JSONDecodeError:
            decky.logger.warning("cli %s: unparseable output: %s", args[0], out[:200])
        return {"ok": False, "error": "client-error", "detail": "unreadable output"}
    code = _cli_error(rc, err)
    detail = (err.strip().splitlines() or [f"{args[0]} failed"])[-1]
    decky.logger.warning("cli %s failed (rc=%s, %s): %s", args[0], rc, code, detail)
    return {"ok": False, "error": code, "detail": detail}


def _client_is_flatpak() -> bool:
    """Is the client this plugin actually drives the FLATPAK one?

    Not the same question as "is the flatpak installed": `PF_DECKY_CLIENT=native` forces the
    native binary on a box that has both, and the update check has to describe the client the
    launcher will really run — otherwise a Deck with both would be offered a flatpak update for
    a client it never starts.
    """
    prefix = _client_argv()
    return bool(prefix) and prefix[0] == _flatpak()


def _flatpak_env() -> dict:
    """Environment for a headless client run from the backend (no display needed for pairing).
    Reconstruct the user-session bits flatpak wants; the backend may not inherit them. Harmless
    if some are already set — and correct for a NATIVE client too, which needs the same HOME and
    the same LD_LIBRARY_PATH repair below."""
    env = dict(os.environ)
    # Decky Loader is a PyInstaller binary: it prepends its bundled libs (an older libssl) to
    # LD_LIBRARY_PATH (its /tmp/_MEI* unpack dir), and that env leaks into our subprocess. The
    # SYSTEM flatpak's libcurl needs OPENSSL_3.3.0 from the SYSTEM libssl, so the bundled libssl
    # breaks it ("libssl.so.3: version OPENSSL_3.3.0 not found"). Restore the pre-bundle value
    # PyInstaller saved as <VAR>_ORIG, or drop the var so the dynamic loader uses system libraries.
    for var in ("LD_LIBRARY_PATH", "LD_PRELOAD"):
        orig = env.pop(f"{var}_ORIG", None)
        if orig:
            env[var] = orig
        else:
            env.pop(var, None)
    env.setdefault("HOME", decky.DECKY_USER_HOME)
    uid = os.environ.get("PF_UID") or "1000"
    env.setdefault("XDG_RUNTIME_DIR", f"/run/user/{uid}")
    env.setdefault(
        "DBUS_SESSION_BUS_ADDRESS", f"unix:path=/run/user/{uid}/bus"
    )
    # Ensure flatpak can find the user installation.
    env.setdefault(
        "PATH", "/usr/bin:/bin:" + env.get("PATH", "")
    )
    return env


async def _exec(
    argv: list[str] | None,
    label: str,
    timeout: float,
    *,
    stdin_text: str | None = None,
    merge_stderr: bool = False,
) -> tuple[int, str, str]:
    """Run ``argv`` with the user-session env, returning ``(returncode, stdout, stderr)`` decoded.
    ``(-1, "", "")`` when ``argv`` is empty (nothing installed) or the call errors or times out.
    ``merge_stderr`` folds stderr into stdout. A timed-out child is killed and reaped, so its
    pipes close with it rather than at garbage collection."""
    if not argv:
        return -1, "", ""
    proc = None
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE if stdin_text is not None else None,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT if merge_stderr else asyncio.subprocess.PIPE,
            env=_flatpak_env(),
        )
        payload = stdin_text.encode() if stdin_text is not None else None
        out, err = await asyncio.wait_for(proc.communicate(payload), timeout=timeout)
        rc = proc.returncode if proc.returncode is not None else -1
        return (
            rc,
            (out or b"").decode("utf-8", "replace"),
            (err or b"").decode("utf-8", "replace"),
        )
    except asyncio.TimeoutError:
        decky.logger.warning("%s timed out", label)
        if proc:
            try:
                proc.kill()
            except ProcessLookupError:
                pass
            await proc.wait()
        return -1, "", ""
    except Exception:  # noqa: BLE001
        decky.logger.exception("%s failed", label)
        return -1, "", ""


async def _flatpak_capture(args: list[str], timeout: float = 20.0) -> tuple[int, str]:
    """Run ``flatpak <args>`` with the user-session env, merging stderr into stdout. Returns
    ``(returncode, output)``; ``(-1, "")`` if the binary is missing or the call errors/times out.
    Best-effort by design — every caller here treats a failure as "no update / can't tell"."""
    flatpak = _flatpak()
    rc, out, _ = await _exec(
        flatpak and [flatpak, *args], f"flatpak {' '.join(args)}", timeout, merge_stderr=True
    )
    return rc, out


async def _run_client(client_args: list[str], timeout: float = 20.0) -> tuple[int, str, str]:
    """Run the CLIENT headlessly with the user-session env, returning ``(returncode, stdout,
    stderr)`` with SEPARATE pipes so a JSON payload on stdout stays clean of the client's log
    lines on stderr. ``(-1, "", "")`` when no client is installed or the call errors/times out.

    Whether that client is the flatpak or a native install is [_client_argv]'s business; both
    read and write the SAME ``client-known-hosts.json`` the desktop client uses. This is the
    single entry point for the headless host-store modes (``--list-hosts`` / ``--add-host`` /
    ``--set-host`` / ``--forget-host`` / ``--reset`` / ``--reachable``), so state is shared, not
    duplicated."""
    prefix = _client_argv()
    return await _exec(
        prefix and [*prefix, *client_args], f"client {' '.join(client_args)}", timeout
    )


def _field_from(text: str, name: str) -> str:
    """Pull ``<name>: value`` out of ``flatpak info`` / ``remote-info`` output (e.g. ``Commit``,
    ``Origin``)."""
    prefix = f"{name}:"
    for line in text.splitlines():
        s = line.strip()
        if s.startswith(prefix):
            return s.split(":", 1)[1].strip()
    return ""


def _looks_outdated(stderr: str) -> bool:
    """Does this stderr have the signature of a client too old for the headless flag it was just
    handed? Such a client ignores the unknown flag and falls through to GTK init, which fails
    with no display — so the give-away is display/GTK noise rather than anything about the flag.

    Narrow on purpose: the CLI announces the same condition deterministically (exit 5 plus
    ``unknown command``, see :func:`_cli_error`), and this heuristic is only still here because
    the update check drives the GTK client's ``--check-update``, not the CLI."""
    s = stderr.lower()
    return "display" in s or "gtk" in s


async def _client_update_state() -> dict:
    """Is a newer commit of the flatpak client available in the remote it tracks? The client
    versions independently of this plugin, so we compare the installed commit against the
    remote's here and let the QAM offer an update in the scope the client is actually installed
    in — a per-user install is one ``sudo flatpak update`` (system-scope) never reaches.

    Flatpak keeps its OWN comparison (commits, not versions) because it is the exact one: a
    flatpak built from main between releases carries the release's crate version, so the
    signed-manifest comparison the native path uses would call it up to date when it isn't.
    Native installs have no commit to compare and go through :func:`_native_update_state`.

    ⚠ Every query names the ref IN FULL (see :func:`_flatpak_ref`) — the remote publishes both
    `stable` and `canary`, and an unqualified one is an error, not a default."""
    state = {"available": False, "installed": "", "remote": "", "error": ""}
    ref = _flatpak_ref()
    if not ref:
        return state  # no flatpak client in either scope
    scope, full = ref["scope"], ref["ref"]
    rc, info = await _flatpak_capture(["info", scope, full], timeout=10.0)
    if rc != 0:
        decky.logger.warning("flatpak info %s %s failed (rc=%s): %s", scope, full, rc, info[-200:])
        state["error"] = "client-unavailable"
        return state
    state["installed"] = _field_from(info, "Commit")
    origin = _field_from(info, "Origin")
    if not origin:
        state["error"] = "no-origin"  # a sideloaded bundle tracks no remote to compare against
        return state
    rc, rinfo = await _flatpak_capture(["remote-info", scope, origin, full], timeout=25.0)
    if rc != 0:
        # ⭐ NOT "up to date". Silently swallowing this is precisely how the whole leg stayed
        # broken in the field: an unqualified ref made every one of these calls fail, and
        # returning `available=False` dressed the failure up as good news. A check that could
        # not run says so, and the panel says so too.
        decky.logger.warning(
            "flatpak remote-info %s %s failed (rc=%s): %s", origin, full, rc, rinfo.strip()[-200:]
        )
        state["error"] = "fetch-failed"
        return state
    state["remote"] = _field_from(rinfo, "Commit")
    state["available"] = bool(
        state["installed"] and state["remote"] and state["installed"] != state["remote"]
    )
    return state


# --- native (non-flatpak) client updates ----------------------------------------------------
#
# A .deb/.rpm/pacman/sysext/nix client is not something this plugin can reason about on its own:
# working out whether a newer build exists means fetching a per-channel manifest and verifying
# its Ed25519 signature, and Decky's embedded Python has no crypto library to do that with (nor
# should the trust rule live in two languages). So the CLIENT answers both questions —
# `--check-update --json` says what is available and who could install it, `--apply-update`
# drives the packaged root helper — and this backend is a UI over those, exactly as it already
# is for `--pair` / `--library` / `--list-hosts`.
#
# Shape of `--check-update --json` (pf_client_core::update::Status):
#   {kind, channel, current, latest, update_available, apply, applier, command,
#    opt_in_hint?, notes_url, error?}
# `applier` is what this file routes on: "flatpak" (we run flatpak), "helper" (the client runs
# the root helper), "none" (show `command` — nothing here can install it).


async def _native_update_state() -> dict:
    """Ask a NATIVE client whether a newer build exists for its channel. Returns the client's
    own status dict, or ``{}`` when it couldn't be asked (no native client, a client too old to
    have the mode, offline). Best-effort by design: an unanswerable check must read as "can't
    tell", never as "up to date"."""
    rc, out, err = await _run_client(["--check-update", "--json"], timeout=30.0)
    # The JSON is authoritative whenever there IS JSON, whatever the exit code: the client
    # exits 0 up-to-date, 10 update-available, and 1 when the check failed — but in that last
    # case it STILL prints a status carrying `error` plus the install kind and the command for
    # this box, which is exactly what the UI needs to explain itself. Reading only the exit
    # code would throw all of that away and report a bare "couldn't check".
    if out.strip():
        try:
            data = json.loads(out)
            if isinstance(data, dict):
                return data
        except json.JSONDecodeError:
            decky.logger.warning("check-update: unparseable output: %s", out[:200])
    if rc == -1:
        return {}
    # A client predating `--check-update` ignores the flag and falls through to GTK init, which
    # fails headless — that is the signature, and it is the one thing worth reporting here.
    outdated = _looks_outdated(err)
    decky.logger.info("native check-update unavailable (rc=%s, outdated=%s)", rc, outdated)
    return {"error": "client-outdated"} if outdated else {}


def _ctl_sockets() -> list[Path]:
    """Candidate paths of the streaming client's control socket (guide/QAM injection):
    the flatpak app runtime dir first (the one runtime path the sandbox and this backend
    see identically), then the plain runtime dir (native installs). Mirrors the session
    binary's ``ctl_socket::path``."""
    uid = os.environ.get("PF_UID") or "1000"
    run = Path(f"/run/user/{uid}")
    return [
        run / "app" / APP_ID / "punktfunk-session-ctl.sock",
        run / "punktfunk-session-ctl.sock",
    ]


class Plugin:
    # ---- Thin shells over the headless CLI -------------------------------------------------
    #
    # Each is "build argv, run, parse JSON, map the exit code". No parsing of the client's data
    # files happens here and no trust rule is re-implemented here: this backend exists because
    # Decky's frontend cannot spawn processes, not because it knows anything the client doesn't.

    async def discover(self) -> dict:
        """Browse the LAN for hosts (``punktfunk discover --json``).

        ``{ok: True, hosts: [{name, addr, port, fp, pair, id, mgmt, os, saved, paired}]}``, or
        ``{ok: False, error}`` — ``client-outdated`` when the installed client predates the
        verb, which the panel renders as one explanatory row plus the update button.

        The 12 s budget covers a cold flatpak start on top of the CLI's own 3 s browse."""
        return await _cli_json(["discover", "--json"], timeout=12.0)

    async def hosts(self) -> dict:
        """The saved hosts with a live reachability probe
        (``punktfunk hosts list --probe --json``).

        ``--probe`` asks each host directly rather than waiting for an advert, so a host reached
        over a routed network (Tailscale/VPN) reports online instead of looking dead. Preset
        bindings and pinned cards come back already resolved against the preset catalog —
        dangling ids dropped, names attached — so the panel renders them without ever opening
        the catalog file."""
        return await _cli_json(["hosts", "list", "--probe", "--json"], timeout=30.0)

    async def pair(self, addr: str, port: int, pin: str, name: str = "Steam Deck") -> dict:
        """The PIN ceremony (``punktfunk pair <addr:port> --pin - --name LABEL``).

        The operator arms pairing on the host, which shows a 4-digit PIN; entering it here
        verifies the host end to end and pins its fingerprint, so every later connect is silent.
        ``{ok: True}``, or ``{ok: False, error}`` where ``refused`` is a wrong PIN or a host
        that isn't armed, and ``unreachable`` is a host that never answered.

        The PIN goes down the child's STDIN (``--pin -``), never on argv: it is the only secret
        binding the ceremony to the operator's intent, and a value on the command line is readable
        by every local process for as long as the call runs (~100 s here) — long enough for anyone
        on the Deck to complete the pairing with their own keypair instead.

        The budget is generous because the ceremony waits on a person at the other end."""
        rc, out, err = await _run_cli(
            [
                "pair", f"{addr}:{int(port)}",
                "--pin", "-",
                "--name", name,
            ],
            timeout=100.0,
            stdin_text=f"{str(pin).strip()}\n",
        )
        if rc == 0:
            fp = ""
            for token in out.split():
                if token.startswith("fp="):
                    fp = token[3:]
            decky.logger.info("paired %s:%s", addr, port)
            return {"ok": True, "fp": fp}
        detail = (err.strip().splitlines() or ["pairing failed"])[-1]
        decky.logger.warning("pairing failed (rc=%s): %s", rc, detail)
        return {"ok": False, "error": _cli_error(rc, err), "detail": detail}

    async def trust_host(self, addr: str, port: int, fp: str, name: str = "") -> dict:
        """Step 1 of request access: save the host with the fingerprint it ADVERTISED
        (``punktfunk hosts add <addr:port> --fp <hex> --name <label>``).

        The record lands pinned but unpaired — "trusted" — which is exactly what a
        discovered-but-unapproved host is. The stream launched right after pins that same
        fingerprint, so the 185 s wait for an operator's approval cannot be answered by an
        impostor. Idempotent: re-running it with the same fingerprint is a no-op that still
        exits 0. A DIFFERENT fingerprint comes back ``refused`` and is never overwritten.

        This is the ONLY write this backend makes to the client's store, and it goes through
        the CLI — which writes temp+rename into a user-owned directory, so a root backend
        driving it cannot lock the desktop client out of its own files."""
        args = ["hosts", "add", f"{addr}:{int(port)}", "--fp", fp.strip()]
        if name.strip():
            args += ["--name", name.strip()]
        rc, _out, err = await _run_cli(args, timeout=20.0)
        if rc == 0:
            return {"ok": True}
        detail = (err.strip().splitlines() or ["saving the host failed"])[-1]
        decky.logger.warning("trust_host failed (rc=%s): %s", rc, detail)
        return {"ok": False, "error": _cli_error(rc, err), "detail": detail}

    async def library(self, ref: str) -> dict:
        """The host's game library (``punktfunk library <host-ref> --json``).

        ``{ok: True, games: [{id, store, title}]}`` — ``id`` is store-qualified (``steam:570``),
        which is what lets the game page match a host title to a Steam appid. Paired hosts
        only: the CLI answers ``needs-pairing`` for a host that merely has a pinned fingerprint
        and ``unreachable`` for one that is off.

        The ref is a positional argument, so one that starts with ``-`` is refused here rather
        than handed to the CLI as a flag."""
        ref = str(ref).strip()
        if not ref or ref.startswith("-"):
            return {"ok": False, "error": "unresolved", "detail": "bad host reference"}
        return await _cli_json(["library", ref, "--json"], timeout=20.0)

    async def profiles(self, ref: str) -> dict:
        """Who plays on the host (``punktfunk profiles <host-ref> --json``).

        ``{ok: True, profiles: [{id, display_name, accent, owner, note}], picked}``. A host
        without profiles answers ``unresolved``, which the panel reads as "no chips"."""
        ref = str(ref).strip()
        if not ref or ref.startswith("-"):
            return {"ok": False, "error": "unresolved", "detail": "bad host reference"}
        return await _cli_json(["profiles", ref, "--json"], timeout=20.0)

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
        """Install our Steam Input layout (native touchscreen `ts_n` + gamepad passthrough) and
        point the shortcut(s) at it, so the Deck touchscreen reaches the client as native touch
        with zero manual controller setup. Best-effort + idempotent — a controller tweak must
        never block a launch, so failures are reported, not raised. Both shortcuts share the same
        name → the same lowercase configset key, so one entry per account covers both."""
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
                    if not existed:  # a freshly-created file is root-owned — hand it to the user
                        _chown_like_parent(f)
                applied.append(f"configset:{d.parent.name}")
            except OSError as e:
                errors.append(f"{d.parent.name}: {e}")
        decky.logger.info(
            "apply_controller_config key=%s applied=%s errors=%s", key, applied, errors
        )
        return {"ok": not errors, "applied": applied, "errors": errors, "accounts": len(dirs)}

    async def runner_info(self) -> dict:
        """The wrapper-script path + flatpak app id the frontend needs to create the Steam
        shortcut. The shortcut invokes the script through ``/bin/sh`` (see steam.ts), so no
        exec bit is needed — Decky's zip extraction drops it, and the root-owned plugins dir
        means this unprivileged backend couldn't chmod it back on anyway.

        ``client_bin`` is set only when the resolved client is a NATIVE install; the frontend
        passes it to the wrapper as ``PF_CLIENT_BIN`` so the launch execs the binary instead of
        ``flatpak run``. Absent = the wrapper's flatpak default, i.e. every existing Deck
        install is unaffected."""
        path = _runner_path()
        prefix = _client_argv()
        native = bool(prefix) and prefix[0] != _flatpak()
        return {
            "runner": path,
            "app_id": APP_ID,
            "exists": Path(path).exists(),
            "client_kind": "native" if native else ("flatpak" if prefix else "none"),
            "client_bin": prefix[0] if native else "",
        }

    # ---- Shared known-hosts store (the SAME file the desktop client reads/writes) ----

    async def kill_stream(self) -> dict:
        """Force-stop a wedged stream client — ``flatpak kill`` for the sandboxed one, a plain
        SIGTERM by name for a native install (which has no flatpak instance to kill)."""
        prefix = _client_argv()
        if not prefix:
            return {"ok": False, "error": "client-not-found"}
        if prefix[0] == _flatpak():
            argv = [prefix[0], "kill", APP_ID]
        else:
            # -x: whole-name match, so this can only ever hit the client itself.
            killer = shutil.which("pkill") or "/usr/bin/pkill"
            argv = [killer, "-x", NATIVE_BIN]
        try:
            proc = await asyncio.create_subprocess_exec(
                *argv,
                stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                env=_flatpak_env(),
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
        with ``-`` is refused here rather than handed to the CLI as a flag."""
        ref, game_id = str(ref).strip(), str(game_id).strip()
        if not ref or ref.startswith("-") or not game_id or game_id.startswith("-"):
            return {"ok": False, "error": "unresolved", "notice": "Couldn't tell which game to end."}
        rc, out, err = await _run_cli(["end-game", ref, "--game", game_id], timeout=20.0)
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

    async def _update_native_client(self) -> dict:
        """The non-flatpak leg of :meth:`update_client` — drive the client's own
        ``--apply-update``, which starts the packaged root helper.

        The timeout is generous because a package-manager run on a stale box is slow; the
        client caps its own wait at 30 min, so this one sits below that and reports a timeout
        rather than hanging the QAM forever.
        """
        state = await _native_update_state()
        if not state:
            # No client answered at all (none installed, or one too old for the mode) — say
            # that, rather than "this install updates by hand", which would be a guess.
            return {"ok": False, "updated": False, "error": "client-unavailable"}
        applier = state.get("applier")
        if applier != "helper":
            # Nothing here can install it — hand back the command the client computed, so the
            # UI shows one true line instead of guessing per install kind.
            return {
                "ok": False,
                "updated": False,
                "error": "manual",
                "command": state.get("opt_in_hint") or state.get("command", ""),
            }
        rc, out, err = await _run_client(["--apply-update", "--json"], timeout=900.0)
        if rc == -1:
            return {"ok": False, "updated": False, "error": "timeout"}
        outcome: dict = {}
        if out.strip():
            try:
                outcome = json.loads(out)
            except json.JSONDecodeError:
                pass
        if not outcome.get("ok"):
            detail = outcome.get("error") or (err.strip().splitlines() or ["update failed"])[-1]
            decky.logger.warning("native client update failed (rc=%s): %s", rc, detail)
            return {"ok": False, "updated": False, "error": "update-failed", "detail": detail}
        _update_cache["data"] = None  # invalidate the cached "update available" snapshot
        decky.logger.info(
            "native client update: %s -> %s (changed=%s, staged=%s)",
            outcome.get("before", ""), outcome.get("after", ""),
            outcome.get("changed"), outcome.get("staged"),
        )
        return {
            "ok": True,
            "updated": bool(outcome.get("changed")),
            "staged": bool(outcome.get("staged")),
        }

    async def update_client(self) -> dict:
        """Update the **client**, by whichever route this box's install actually supports.

        * **flatpak** — ``flatpak update`` against the FULL ref, in the scope the client is
          installed in (a per-user install is one ``sudo flatpak update`` never reaches, and an
          unqualified ref is an error on a remote publishing more than one branch).
        * **native, one-tap capable** (.deb / .rpm / pacman with the packaged root helper and
          the operator's group opt-in) — ``punktfunk-client --apply-update``, which starts the
          fixed, parameterless ``punktfunk-client-update.service`` through polkit. This backend
          passes nothing to it; the helper derives everything from root-owned state.
        * **anything else** (sysext, nix, a source build, no opt-in) — refused with the exact
          command to run, which the UI shows. `check_update` reports the same, so the UI knows
          not to offer a button in the first place.

        Returns ``{ok, updated, error?, detail?, command?, staged?}``. Best-effort; non-fatal.
        """
        if not _client_is_flatpak():
            return await self._update_native_client()
        ref = _flatpak_ref()
        if not ref:
            return {"ok": False, "updated": False, "error": "client-unavailable"}
        scope, full = ref["scope"], ref["ref"]
        _, before = await _flatpak_capture(["info", scope, full], timeout=10.0)
        before_commit = _field_from(before, "Commit")
        rc, out = await _flatpak_capture(["update", scope, "-y", full], timeout=300.0)
        if rc != 0:
            decky.logger.warning("flatpak client update failed (rc=%s): %s", rc, out[-400:])
            return {"ok": False, "updated": False, "error": "update-failed"}
        _, after = await _flatpak_capture(["info", scope, full], timeout=10.0)
        after_commit = _field_from(after, "Commit")
        updated = bool(before_commit and after_commit and before_commit != after_commit)
        decky.logger.info(
            "flatpak client update (%s %s): %s -> %s (updated=%s)",
            scope, full, before_commit[:10], after_commit[:10], updated,
        )
        _update_cache["data"] = None  # invalidate the cached "update available" snapshot
        return {"ok": True, "updated": updated}

    async def check_update(self, force: bool = False) -> dict:
        """Report pending updates for BOTH the plugin and the flatpak client.

        The plugin updates via Decky's install RPC (the per-channel ``manifest.json`` the CI
        publishes); the **client** updates via ``flatpak update --user`` (a per-user install, so
        ``sudo flatpak update`` — system-scope — never touches it) and versions independently, so
        it's checked here too and applied through :meth:`update_client`. Non-fatal: any failure
        leaves the respective ``*_update_available`` ``False``.
        """
        current = _installed_version()
        cfg = _update_config()
        result = {
            "current": current,
            "latest": current,
            "artifact": "",
            "hash": "",
            "channel": str(cfg.get("channel", "")),
            "update_available": False,
            "client_update_available": False,
            "client_current": "",
            "client_latest": "",
            # How the client got here (`flatpak`, `apt`, `dnf`, `sysext`, `nix`, `source`, …),
            # who could install an update (`flatpak` | `helper` | `none`), and the one line that
            # does it by hand. Empty on a flatpak-only box that never reaches the native path.
            "client_install": "",
            "client_applier": "",
            "client_command": "",
            "client_opt_in": "",
        }

        now = time.monotonic()
        cached = _update_cache["data"]
        if not force and cached and (now - _update_cache["at"]) < _UPDATE_TTL_S:
            return cached

        # Client update — checked ALWAYS, even on a dev/sideloaded plugin build. Which check
        # runs depends on how the client was installed: the flatpak compares OSTree commits
        # (exact for a per-user flatpak), everything else asks the client itself, which
        # verifies the signed per-channel manifest. See _client_update_state / _native_update_state.
        try:
            if _client_is_flatpak():
                cu = await _client_update_state()
                ref = _flatpak_ref()
                result["client_update_available"] = bool(cu["available"])
                result["client_current"] = (cu["installed"] or "")[:10]
                result["client_latest"] = (cu["remote"] or "")[:10]
                result["client_install"] = "flatpak"
                result["client_applier"] = "flatpak"
                # The line a user could actually run — same scope, same full ref we use. The old
                # unqualified one errored out ("Multiple branches available") when pasted, too.
                result["client_command"] = (
                    f"flatpak update {ref['scope']} -y {ref['ref']}" if ref else ""
                )
                if cu["error"]:
                    # Same contract as the native leg: "couldn't tell" is never "up to date".
                    result["client_error"] = cu["error"]
            else:
                nu = await _native_update_state()
                result["client_update_available"] = bool(nu.get("update_available"))
                result["client_current"] = str(nu.get("current", ""))
                result["client_latest"] = str(nu.get("latest", ""))
                result["client_install"] = str(nu.get("kind", ""))
                result["client_applier"] = str(nu.get("applier", ""))
                result["client_command"] = str(nu.get("command", ""))
                result["client_opt_in"] = str(nu.get("opt_in_hint", "") or "")
                if nu.get("error"):
                    # "Couldn't tell" — never rendered as up to date; the UI shows the reason.
                    result["client_error"] = str(nu["error"])
        except Exception:  # noqa: BLE001
            decky.logger.warning("client update check failed", exc_info=True)

        manifest_url = cfg.get("manifest")
        if not manifest_url:
            result["error"] = "update-channel-unknown"  # dev / sideloaded plugin build
            _update_cache["at"] = now
            _update_cache["data"] = result  # the client info is still valid to cache
            return result

        try:
            loop = asyncio.get_running_loop()
            manifest = await loop.run_in_executor(None, _fetch_json, manifest_url)
        except Exception as exc:  # noqa: BLE001
            decky.logger.warning("plugin update check failed: %s", exc)
            result["error"] = "fetch-failed"
            return result  # transient — don't cache, retry next open

        latest = str(manifest.get("version", current))
        result["latest"] = latest
        result["artifact"] = str(manifest.get("artifact", ""))
        result["hash"] = str(manifest.get("sha256", ""))
        result["update_available"] = bool(result["artifact"]) and (
            _semver_tuple(latest) > _semver_tuple(current)
        )
        if result["update_available"] or result["client_update_available"]:
            decky.logger.info(
                "updates: plugin %s->%s (avail=%s), client->%s (avail=%s)",
                current, latest, result["update_available"],
                result["client_latest"], result["client_update_available"],
            )
        _update_cache["at"] = now
        _update_cache["data"] = result
        return result

    # ---- Decky lifecycle ----

    async def _main(self):
        decky.logger.info("punktfunk plugin loaded (runner=%s)", _runner_path())

    async def _unload(self):
        decky.logger.info("punktfunk plugin unloading")

    async def _uninstall(self):
        decky.logger.info("punktfunk plugin uninstalled")
