"""Which client this Deck has, and the headless CLI this backend drives it through.

The flatpak is the usual client; a sysext, a distro package, a nix profile or a hand build
installs a native ``punktfunk-client`` instead. Every call goes through the one client found,
which owns its identity, known hosts and settings, so no other module cares which one answered.

The CLI verbs (``discover``, ``hosts``, ``pair``, ``request_access``, ``device_code``,
``trust_host``, ``library``, ``profiles``) build argv, run ``punktfunk``, and map its exit code to a stable error
string. Nothing here parses the client's data files or re-implements a trust rule; the backend
exists because Decky's frontend cannot spawn processes.
"""

import asyncio
import json
import os
import re
import shutil
from pathlib import Path

import decky

# Flatpak application id of the GTK client (packaging/flatpak/io.unom.punktfunk.yml).
APP_ID = "io.unom.punktfunk"
# The id before the lowercase rename. A Deck still on it is driven under it until its next client
# update, which the remote's end-of-life marker turns into a rebase onto APP_ID.
LEGACY_APP_ID = "io.unom.Punktfunk"


def _runner_path() -> str:
    """Absolute path to the launch wrapper shipped with the plugin (bin/punktfunkrun.sh)."""
    return str(Path(decky.DECKY_PLUGIN_DIR) / "bin" / "punktfunkrun.sh")


def _flatpak() -> str | None:
    return shutil.which("flatpak") or (
        "/usr/bin/flatpak" if Path("/usr/bin/flatpak").exists() else None
    )


NATIVE_BIN = "punktfunk-client"
# The headless CLI: in /app/bin in the flatpak, beside `punktfunk-client` natively. Every package
# since v0.22.0 ships it.
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


def _flatpak_ref() -> dict | None:
    """The installed client flatpak as ``{scope, branch, arch, app_id, ref}``, or None.

    ``ref`` names the branch in full (``io.unom.punktfunk//canary``): the remote publishes
    ``stable`` and ``canary``, so flatpak refuses an unqualified ref even with one branch
    installed. With both installed, ``stable`` wins, because that is what a plain ``flatpak run``
    starts. ``app_id`` is :data:`LEGACY_APP_ID` only when that is all the box has.

    Read off the installed tree, not ``flatpak list``: every headless call passes through here.
    ``active`` marks a deployed branch. The arch comes from the tree too, because Decky's Python
    can be an x86_64 process on an aarch64 SteamOS.
    """
    if not _flatpak():
        return None
    scopes = (
        (Path(decky.DECKY_USER_HOME) / ".local" / "share" / "flatpak", "--user"),
        (Path("/var/lib/flatpak"), "--system"),
    )
    for app_id in (APP_ID, LEGACY_APP_ID):
        for root, scope in scopes:
            try:
                found = sorted(
                    (p.parent.name, p.name) for p in (root / "app" / app_id).glob("*/*")
                    if (p / "active").exists()
                )
            except OSError:
                continue  # not installed in this scope
            if not found:
                continue
            arch = found[0][0]
            branches = [b for a, b in found if a == arch]
            branch = "stable" if "stable" in branches else branches[0]
            if len(branches) > 1:
                decky.logger.warning(
                    "%s is installed on %d branches (%s) — using %s, the one `flatpak run` "
                    "resolves to; uninstall the others so the client you launch is the client "
                    "we update",
                    app_id, len(branches), ", ".join(branches), branch,
                )
            return {
                "scope": scope, "branch": branch, "arch": arch, "app_id": app_id,
                "ref": f"{app_id}//{branch}",
            }
    return None


def _flatpak_installed() -> bool:
    """True when the client flatpak is installed in either scope, not merely `flatpak` itself."""
    return _flatpak_ref() is not None


def _client_argv() -> list[str] | None:
    """The argv prefix that runs the client headlessly, or None when none is installed.

    An installed flatpak wins (the tested Deck path); a native binary is the fallback.
    ``PF_DECKY_CLIENT=native|flatpak`` forces one. The flatpak form pins ``--branch=`` and keeps the
    app id LAST, since flatpak hands everything after it to the app, so it runs the exact ref the
    update check reads.
    """
    forced = os.environ.get("PF_DECKY_CLIENT", "").strip().lower()
    native = _native_client()
    if forced == "native":
        return [native] if native else None
    ref = _flatpak_ref()
    if forced != "flatpak" and not ref and native:
        return [native]
    if ref:
        return [
            _flatpak(), "run", f"--arch={ref['arch']}", f"--branch={ref['branch']}", ref["app_id"],
        ]
    return [native] if native else None


def _cli_argv() -> list[str] | None:
    """The argv prefix that runs the headless CLI, or None when there is none. Flatpak:
    ``--command=punktfunk`` goes before the app id, which stays LAST. Native: the CLI is the
    client binary's sibling."""
    prefix = _client_argv()
    if not prefix:
        return None
    if prefix[0] == _flatpak():
        return [*prefix[:-1], f"--command={CLI_BIN}", prefix[-1]]
    sibling = Path(prefix[0]).with_name(CLI_BIN)
    return [str(sibling)] if sibling.exists() else None


def _client_is_flatpak() -> bool:
    """Is the client this plugin drives the flatpak one? ``PF_DECKY_CLIENT=native`` picks the
    native binary on a box with both, and the update check describes the client that launches."""
    prefix = _client_argv()
    return bool(prefix) and prefix[0] == _flatpak()


def _flatpak_env() -> dict:
    """The user-session environment for a client run from the backend, which may not inherit it.
    Right for a native client too: it needs the same HOME and library-path repair."""
    env = dict(os.environ)
    # Decky is a PyInstaller binary: its bundled libs (an older libssl) lead LD_LIBRARY_PATH, and
    # the flatpak's libcurl needs the system libssl's OPENSSL_3.3.0. Restore the value PyInstaller
    # saved as <VAR>_ORIG, or drop the var so the loader uses system libraries.
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
    # The backend may start with no PATH at all.
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
    env_extra: dict[str, str] | None = None,
) -> tuple[int, str, str]:
    """Run ``argv`` in the user-session env: ``(returncode, stdout, stderr)``, decoded.
    ``(-1, "", "")`` when ``argv`` is empty (nothing installed), or the call errors or times out.
    ``merge_stderr`` folds stderr into stdout, ``env_extra`` adds to the environment. A timed-out
    child is killed and reaped, so its pipes close with it."""
    if not argv:
        return -1, "", ""
    proc = None
    try:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdin=asyncio.subprocess.PIPE if stdin_text is not None else None,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT if merge_stderr else asyncio.subprocess.PIPE,
            env={**_flatpak_env(), **(env_extra or {})},
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


async def _run_cli(
    args: list[str], timeout: float = 20.0, stdin_text: str | None = None
) -> tuple[int, str, str]:
    """Run the headless CLI: ``(returncode, stdout, stderr)``, or ``(-1, "", "")`` when no client
    is installed or the call times out. Separate pipes: stdout is the payload, stderr the log.

    ``stdin_text`` is how a secret reaches the CLI. Argv never carries one: ``/proc/*/cmdline`` is
    world-readable."""
    prefix = _cli_argv()
    return await _exec(
        prefix and [*prefix, *args], f"cli {' '.join(args)}", timeout, stdin_text=stdin_text
    )


async def _run_client(
    client_args: list[str], timeout: float = 20.0, env_extra: dict[str, str] | None = None
) -> tuple[int, str, str]:
    """Run the GUI client binary headlessly (``--check-update``, ``--apply-update``):
    ``(returncode, stdout, stderr)`` on separate pipes, so a JSON payload stays clean of the log.
    ``(-1, "", "")`` when no client is installed or the call errors or times out."""
    prefix = _client_argv()
    return await _exec(
        prefix and [*prefix, *client_args],
        f"client {' '.join(client_args)}",
        timeout,
        env_extra=env_extra,
    )


# The CLI's exit-code contract (clients/cli/src/main.rs): 0 ok, 2 connect failed, 3 trust
# rejected, 4 renderer, 5 unresolved, 6 needs a person. Mapped to the stable strings the panel
# renders, so a reworded message can never change what the UI shows.
_CLI_ERRORS = {
    2: "unreachable",
    3: "refused",
    5: "unresolved",
    6: "needs-pairing",
}


def _cli_error(rc: int) -> str:
    """One stable error code for a nonzero CLI exit. Never reads the CLI's prose."""
    if rc == -1:
        return "client-unavailable"
    return _CLI_ERRORS.get(rc, "client-error")


def _stdout_json(out: str) -> dict:
    """The JSON object a CLI verb printed under ``--json``; ``{}`` when stdout holds none, as
    from a client too old for the flag."""
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return {}
    return data if isinstance(data, dict) else {}


_REJECT_TOKEN = re.compile(r"[a-z]+(-[a-z]+)*")


def _reject_token(out: str) -> str | None:
    """The host's refusal token (``not-armed``, ``denied``, …) in a failed ``pair --json``, for
    the panel to word. None when the host sent none or the client predates the flag."""
    token = _stdout_json(out).get("reject")
    valid = isinstance(token, str) and len(token) <= 32 and _REJECT_TOKEN.fullmatch(token)
    return token if valid else None


async def _cli_json(
    args: list[str], timeout: float = 20.0, outdated_rc: tuple[int, ...] = ()
) -> dict:
    """Run the CLI and parse its stdout as JSON: ``{"ok": True, **payload}``, else ``{"ok": False,
    "error": <code>, "detail": <the CLI's last stderr line, for the log>}``.

    ``client-outdated`` is a client too old for the verb: its ``--json`` answer says
    ``unknown-command``, and ``outdated_rc`` lists the exits that mean the same from a client
    predating that answer. A zero exit with unparseable stdout is a failure, never an empty
    result."""
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
    code = _cli_error(rc)
    if _stdout_json(out).get("error") == "unknown-command" or rc in outdated_rc:
        code = "client-outdated"
    detail = (err.strip().splitlines() or [f"{args[0]} failed"])[-1]
    decky.logger.warning("cli %s failed (rc=%s, %s): %s", args[0], rc, code, detail)
    return {"ok": False, "error": code, "detail": detail}


def _paired_fp(out: str) -> str:
    """The fingerprint a successful ``pair`` printed: the ``--json`` object's, else the
    ``fp=`` token of the plain line a client older than the flag prints."""
    fp = _stdout_json(out).get("fp")
    if isinstance(fp, str):
        return fp
    for token in out.split():
        if token.startswith("fp="):
            return token[3:]
    return ""


def _pair_failure(rc: int, out: str, err: str, fallback: str) -> dict:
    """A failed ``pair`` as the panel gets it: the exit-code error (``unreachable`` when the
    ``--json`` answer says the host never replied, which exit 3 alone cannot), the host's
    ``reject`` token when it sent one, and the CLI's last stderr line (``fallback`` when none)
    for the log."""
    detail = (err.strip().splitlines() or [fallback])[-1]
    decky.logger.warning("pair failed (rc=%s): %s", rc, detail)
    silent = _stdout_json(out).get("kind") in ("timeout", "unreachable")
    failure = {"ok": False, "error": "unreachable" if silent else _cli_error(rc), "detail": detail}
    token = _reject_token(out)
    if token:
        failure["reject"] = token
    return failure


async def discover() -> dict:
    """The LAN's hosts (``punktfunk discover --json``): ``{ok, hosts: [{name, addr, port, fp,
    pair, id, mgmt, os, saved, paired}]}``. 12 s covers a cold flatpak start on top of the CLI's
    3 s browse. A client with the verb never exits 5 from it, so 5 is ``client-outdated``."""
    return await _cli_json(["discover", "--json"], timeout=12.0, outdated_rc=(5,))


async def hosts() -> dict:
    """The saved hosts (``punktfunk hosts list --probe --json``). ``--probe`` asks each host
    directly, so one on a routed network (Tailscale, VPN) reads online without an mDNS advert.
    Preset bindings and pinned cards come back resolved against the preset catalog."""
    return await _cli_json(["hosts", "list", "--probe", "--json"], timeout=30.0)


async def pair(addr: str, port: int, pin: str, name: str) -> dict:
    """The PIN ceremony (``punktfunk pair <addr:port> --pin - --name LABEL --json``).

    ``{ok: True, fp}``, or ``{ok: False, error, reject?}``: ``refused`` is a wrong PIN or a host
    that turned the device away (``reject`` is the host's token for why), ``unreachable`` a host
    that never answered. The PIN goes down stdin (``--pin -``), never argv: a command line is
    readable by every local process for the ~100 s the ceremony waits on a person, long enough
    to pair another keypair with it.
    """
    rc, out, err = await _run_cli(
        [
            "pair", f"{addr}:{int(port)}",
            "--pin", "-",
            "--name", name,
            "--json",
        ],
        timeout=100.0,
        stdin_text=f"{str(pin).strip()}\n",
    )
    if rc == 0:
        decky.logger.info("paired %s:%s", addr, port)
        return {"ok": True, "fp": _paired_fp(out)}
    return _pair_failure(rc, out, err, "pairing failed")


async def request_access(addr: str, port: int, name: str, fp: str) -> dict:
    """No-PIN pairing (``punktfunk pair <addr:port> --request-access --name LABEL --fp HEX``).

    Waits up to ~185 s for the host's operator, pinned to ``fp``, the fingerprint the host
    advertised and :func:`trust_host` saved. Without it the CLI takes the newest record at the
    address, which at a dual-boot box can be the other OS. Never streams. Same result as
    :func:`pair`, plus ``bad-fingerprint`` for an ``fp`` that is not 64 hex digits. A client too
    old for the flag asks for a PIN and exits 6, which a current one never does here:
    ``client-outdated``.
    """
    fp = str(fp).strip()
    if fp and not re.fullmatch(r"[0-9a-fA-F]{64}", fp):
        return {"ok": False, "error": "bad-fingerprint", "detail": "bad host fingerprint"}
    args = ["pair", f"{addr}:{int(port)}", "--request-access", "--name", name, "--json"]
    if fp:
        args += ["--fp", fp]
    rc, out, err = await _run_cli(args, timeout=200.0)
    if rc == 0:
        decky.logger.info("access granted by %s:%s", addr, port)
        return {"ok": True, "fp": _paired_fp(out)}
    failure = _pair_failure(rc, out, err, "the request failed")
    if rc == 6:
        failure["error"] = "client-outdated"
    return failure


async def device_code() -> dict:
    """This Deck's device code (``punktfunk device-code --json``): ``{ok, code}``. The host's
    console shows the same code beside a request for access. A client without the verb answers
    ``client-outdated``, and the panel shows no code."""
    return await _cli_json(["device-code", "--json"])


async def trust_host(addr: str, port: int, fp: str, name: str) -> dict:
    """Step 1 of request access: save the host pinned to the fingerprint it ADVERTISED, unpaired
    (``punktfunk hosts add <addr:port> --fp <hex> --name <label>``).

    The launch that follows pins the same fingerprint, so the 185 s wait for an operator cannot be
    answered by an impostor. Idempotent; a DIFFERENT fingerprint comes back ``refused`` and is
    never overwritten. The only write this backend makes to the client's store, and the CLI makes
    it.
    """
    args = ["hosts", "add", f"{addr}:{int(port)}", "--fp", fp.strip()]
    if name.strip():
        args += ["--name", name.strip()]
    rc, _out, err = await _run_cli(args, timeout=20.0)
    if rc == 0:
        return {"ok": True}
    detail = (err.strip().splitlines() or ["saving the host failed"])[-1]
    decky.logger.warning("trust_host failed (rc=%s): %s", rc, detail)
    return {"ok": False, "error": _cli_error(rc), "detail": detail}


async def library(ref: str) -> dict:
    """The host's games (``punktfunk library <host-ref> --json``): ``{ok, games: [{id, store,
    title}]}``, ``id`` store-qualified (``steam:570``). Paired hosts only: a host with just a pinned
    fingerprint answers ``needs-pairing``, one that is off ``unreachable``. A ref starting with
    ``-`` is refused here, never handed to the CLI as a flag."""
    ref = str(ref).strip()
    if not ref or ref.startswith("-"):
        return {"ok": False, "error": "unresolved", "detail": "bad host reference"}
    return await _cli_json(["library", ref, "--json"], timeout=20.0)


async def profiles(ref: str) -> dict:
    """Who plays on the host (``punktfunk profiles <host-ref> --json``): ``{ok, profiles: [{id,
    display_name, accent, owner, note, picture}], picked}``, a picture a data URL or null. A host
    without profiles answers ``unresolved``. A flag-shaped ref is refused, as in :func:`library`."""
    ref = str(ref).strip()
    if not ref or ref.startswith("-"):
        return {"ok": False, "error": "unresolved", "detail": "bad host reference"}
    return await _cli_json(["profiles", ref, "--json"], timeout=20.0)


async def runner_info() -> dict:
    """The wrapper path and flatpak app id the frontend needs for the Steam shortcut.

    The shortcut runs the wrapper through ``/bin/sh``, so it needs no exec bit: Decky's unzip drops
    it, and this backend cannot chmod inside the root-owned plugins dir. ``client_bin`` is set only
    for a native client; the frontend passes it as ``PF_CLIENT_BIN`` and the wrapper execs it
    instead of ``flatpak run``.
    """
    path = _runner_path()
    prefix = _client_argv()
    native = bool(prefix) and prefix[0] != _flatpak()
    return {
        "runner": path,
        "app_id": (_flatpak_ref() or {}).get("app_id", APP_ID),
        "exists": Path(path).exists(),
        "client_kind": "native" if native else ("flatpak" if prefix else "none"),
        "client_bin": prefix[0] if native else "",
    }
