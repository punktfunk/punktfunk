"""Updates for the plugin and for the client, behind one result cache.

The plugin installs from a URL, not the Decky store, so :func:`check_update` polls the
per-channel ``manifest.json`` CI publishes beside the zip, and the frontend hands its artifact
and SHA-256 to Decky's own install RPC. CI bakes the ``{channel, manifest}`` pointer into
``update.json``; a dev or sideloaded build has none, and its plugin check is off.

The client versions on its own. A flatpak compares installed and remote commits for the full ref,
in the scope it is installed in. A native client answers for itself (``--check-update --json``)
and updates through its packaged root helper (``--apply-update``): its signed manifest needs
crypto the embedded Python lacks. A check that could not run sets an error, never "up to date".

``_update_cache`` is this module's only result state; :func:`update_client` clears it.
"""

import asyncio
import json
import ssl
import time
import urllib.request
from pathlib import Path

import decky

from . import client

_UPDATE_TTL_S = 1800.0  # cache a successful check for 30 min (the QAM remounts often)
_update_cache: dict = {"at": 0.0, "data": None}


def _update_config() -> dict:
    """The CI-baked ``{channel, manifest}`` next to the plugin (absent on dev builds)."""
    try:
        return json.loads((Path(decky.DECKY_PLUGIN_DIR) / "update.json").read_text())
    except (OSError, json.JSONDecodeError):
        return {}


def _installed_version() -> str:
    """The version Decky reports for this plugin. Decky reads ``package.json``, not
    plugin.json, so CI stamps the build version there."""
    try:
        pkg = json.loads((Path(decky.DECKY_PLUGIN_DIR) / "package.json").read_text())
        return str(pkg.get("version", "0.0.0"))
    except (OSError, json.JSONDecodeError):
        return "0.0.0"


def _semver_tuple(v: str) -> tuple[int, int, int]:
    """A tolerant (major, minor, patch) for ``>``: the leading digits of each component, any
    pre-release suffix dropped. Both channels publish plain numeric ``X.Y.Z``."""
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


# Decky's embedded (PyInstaller) Python has no usable CA roots on SteamOS: its compiled-in verify
# paths do not exist, so a default context trusts nothing. Load a real bundle from disk instead.
# Verification is never disabled; with no bundle the fetch fails, and the check is non-fatal.
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
    """The cached context for every HTTPS fetch in the backend; building it scans disk."""
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


# Output read by its English words (``flatpak info`` labels, a GTK client's startup error) is
# requested in the C locale, so the user's language cannot change what it finds.
_C_LOCALE = {"LC_ALL": "C"}


async def _flatpak_capture(args: list[str], timeout: float = 20.0) -> tuple[int, str]:
    """Run ``flatpak <args>`` in the user-session env and the C locale, stderr merged into
    stdout: ``(returncode, output)``, or ``(-1, "")`` when flatpak is missing or the call errors
    or times out. Every caller reads a failure as "can't tell"."""
    flatpak = client._flatpak()
    rc, out, _ = await client._exec(
        flatpak and [flatpak, *args],
        f"flatpak {' '.join(args)}",
        timeout,
        merge_stderr=True,
        env_extra=_C_LOCALE,
    )
    return rc, out


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
    """Does this stderr show a GUI client too old for the headless flag it was handed? Such a
    client ignores the flag and fails GTK init with no display. Read in the C locale. The CLI says
    the same with ``unknown-command``; only ``--check-update`` still needs this guess."""
    s = stderr.lower()
    return "display" in s or "gtk" in s


async def _client_update_state() -> dict:
    """Is a newer commit of the flatpak client in the remote it tracks? Commits, not versions: a
    flatpak built from main between releases carries the release's version. Every query names
    the full ref, in the install's own scope, which a system-wide ``sudo flatpak update`` misses
    for a per-user install."""
    state = {"available": False, "installed": "", "remote": "", "error": ""}
    ref = client._flatpak_ref()
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
        # A check that could not run says so. It is never "up to date".
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


async def _native_update_state() -> dict:
    """A native client's own answer to ``--check-update --json`` (``pf_client_core::update::
    Status``: ``{kind, channel, current, latest, update_available, apply, applier, command,
    opt_in_hint?, notes_url, error?}``), or ``{}`` when no client answered. ``applier`` routes
    the update: ``helper`` runs the root helper, ``none`` leaves ``command`` to the user."""
    rc, out, err = await client._run_client(
        ["--check-update", "--json"], timeout=30.0, env_extra=_C_LOCALE
    )
    # Any JSON is authoritative, whatever the exit (0 current, 10 update available, 1 failed):
    # a failed check still prints `error`, the install kind and the command, which the panel shows.
    if out.strip():
        try:
            data = json.loads(out)
            if isinstance(data, dict):
                return data
        except json.JSONDecodeError:
            decky.logger.warning("check-update: unparseable output: %s", out[:200])
    if rc == -1:
        return {}
    outdated = _looks_outdated(err)
    decky.logger.info("native check-update unavailable (rc=%s, outdated=%s)", rc, outdated)
    return {"error": "client-outdated"} if outdated else {}


async def _update_native_client() -> dict:
    """The native leg of :func:`update_client`: the client's ``--apply-update`` starts the
    packaged root helper. 900 s sits below the client's own 30-minute cap, so a slow package
    manager reports a timeout instead of hanging the panel."""
    state = await _native_update_state()
    if not state:
        # No client answered. Say so: "update by hand" would be a guess.
        return {"ok": False, "updated": False, "error": "client-unavailable"}
    applier = state.get("applier")
    if applier != "helper":
        # Nothing here can install it: hand back the command the client computed for this box.
        return {
            "ok": False,
            "updated": False,
            "error": "manual",
            "command": state.get("opt_in_hint") or state.get("command", ""),
        }
    rc, out, err = await client._run_client(["--apply-update", "--json"], timeout=900.0)
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


async def update_client() -> dict:
    """Update the client by the route its install supports: ``{ok, updated, error?, detail?,
    command?, staged?}``. Best-effort, non-fatal.

    * flatpak: ``flatpak update`` on the full ref, in the scope it is installed in.
    * native with the packaged root helper and the group opt-in: ``--apply-update``, which starts
      the parameterless ``punktfunk-client-update.service`` through polkit. Nothing is passed in;
      the helper derives everything from root-owned state.
    * anything else (sysext, nix, a source build, no opt-in): ``manual`` with the command to run.
      :func:`check_update` reports the same, so the panel offers no button.
    """
    if not client._client_is_flatpak():
        return await _update_native_client()
    ref = client._flatpak_ref()
    if not ref:
        return {"ok": False, "updated": False, "error": "client-unavailable"}
    scope, full = ref["scope"], ref["ref"]
    _, before = await _flatpak_capture(["info", scope, full], timeout=10.0)
    before_commit = _field_from(before, "Commit")
    rc, out = await _flatpak_capture(["update", scope, "-y", full], timeout=300.0)
    if rc != 0:
        decky.logger.warning("flatpak client update failed (rc=%s): %s", rc, out[-400:])
        return {"ok": False, "updated": False, "error": "update-failed"}
    # An end-of-life rebase leaves the old ref uninstalled, so its commit cannot be compared.
    moved = client._flatpak_ref()
    if moved and moved["app_id"] != ref["app_id"]:
        decky.logger.info("flatpak client rebased: %s -> %s", ref["ref"], moved["ref"])
        _update_cache["data"] = None
        return {"ok": True, "updated": True}
    _, after = await _flatpak_capture(["info", scope, full], timeout=10.0)
    after_commit = _field_from(after, "Commit")
    updated = bool(before_commit and after_commit and before_commit != after_commit)
    decky.logger.info(
        "flatpak client update (%s %s): %s -> %s (updated=%s)",
        scope, full, before_commit[:10], after_commit[:10], updated,
    )
    _update_cache["data"] = None  # invalidate the cached "update available" snapshot
    return {"ok": True, "updated": updated}


async def check_update(force: bool) -> dict:
    """Pending updates for the plugin and the client, cached for :data:`_UPDATE_TTL_S` unless
    ``force``. Non-fatal: a client check that could not run sets ``client_error``; a failed
    manifest fetch sets ``error`` and is not cached, so the next open retries."""
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
        # How the client was installed (`flatpak`, `apt`, `dnf`, `sysext`, `nix`, `source`, …),
        # who can apply an update (`flatpak` | `helper` | `none`), and the line that does it by
        # hand. The opt-in hint is set only for a native install.
        "client_install": "",
        "client_applier": "",
        "client_command": "",
        "client_opt_in": "",
    }

    now = time.monotonic()
    cached = _update_cache["data"]
    if not force and cached and (now - _update_cache["at"]) < _UPDATE_TTL_S:
        return cached

    # The client is checked on every build, a sideloaded plugin included.
    try:
        if client._client_is_flatpak():
            cu = await _client_update_state()
            ref = client._flatpak_ref()
            result["client_update_available"] = bool(cu["available"])
            result["client_current"] = (cu["installed"] or "")[:10]
            result["client_latest"] = (cu["remote"] or "")[:10]
            result["client_install"] = "flatpak"
            result["client_applier"] = "flatpak"
            # The line a user can paste: the same scope and full ref this module updates.
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
