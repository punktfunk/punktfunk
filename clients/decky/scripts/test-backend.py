#!/usr/bin/env python3
"""Unit checks for the Decky backend (main.py + py_modules/pf_decky) — stdlib only, no Decky
runtime needed.

Stubs the ``decky`` module the backend imports, then asserts the argv shapes, the exit-code
mapping and the Steam VDF editor against fixtures, and that Decky's loader can import it at all.
A fixture patches the module whose function looks the name up (``client._run_cli``, never
``client._run_cli``), or the check silently runs the real code.

Needs Python >= 3.10 for `X | None` annotations — macOS ships 3.9, so run it explicitly:

    python3.13 clients/decky/scripts/test-backend.py
"""

import sys
import types
from pathlib import Path

# ---- stub the decky module before importing main.py ------------------------------------
decky = types.ModuleType("decky")
decky.DECKY_USER_HOME = "/tmp/pf-test-home"
decky.DECKY_PLUGIN_DIR = "/tmp/pf-test-plugin"


class _Log:
    def __getattr__(self, _name):
        return lambda *a, **k: None


decky.logger = _Log()
sys.modules["decky"] = decky

_PLUGIN = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(_PLUGIN / "py_modules"))
sys.path.insert(0, str(_PLUGIN))
import main  # noqa: E402  (the plugin backend)
from pf_decky import art, client, session, steam_input, update  # noqa: E402

# The argv fixtures below monkey-patch `_client_argv` to pin one install shape; the
# _flatpak_ref block wants the REAL resolver back, so keep a handle on it.
_real_client_argv = client._client_argv

failures = 0


def check(name: str, cond: bool):
    global failures
    print(("ok  " if cond else "FAIL") + " " + name)
    if not cond:
        failures += 1


# ---- _cli_argv: the flatpak app id must stay LAST ---------------------------------------
#
# `flatpak run --command=X <app-id> ARGS` — everything after the app id is the APP's argv, so
# an app id that drifts left silently turns our flags into the client's. This is the shape the
# deleted _session_argv used and the one thing about it that is easy to get wrong.
client._client_argv = lambda: ["/usr/bin/flatpak", "run", "--arch=x86_64", "io.unom.punktfunk"]
client._flatpak = lambda: "/usr/bin/flatpak"
check(
    "cli argv: flatpak form, app id last",
    client._cli_argv()
    == [
        "/usr/bin/flatpak",
        "run",
        "--arch=x86_64",
        "--command=punktfunk",
        "io.unom.punktfunk",
    ],
)

# A native install: the CLI is the client binary's sibling. Absent => no CLI at all, which the
# caller must see as "unavailable" rather than as an empty result.
#
# The fixture dir is torn down FIRST, not just created: leaving the sibling behind made the
# "absent" assertion below pass only on the first run of the day and fail on every rerun.
import shutil  # noqa: E402

shutil.rmtree("/tmp/pf-test-native", ignore_errors=True)
tmp = Path("/tmp/pf-test-native/bin")
tmp.mkdir(parents=True, exist_ok=True)
(tmp / "punktfunk-client").write_text("")
client._client_argv = lambda: [str(tmp / "punktfunk-client")]
check("cli argv: native without a sibling CLI is None", client._cli_argv() is None)
(tmp / "punktfunk").write_text("")
check("cli argv: native sibling found", client._cli_argv() == [str(tmp / "punktfunk")])

# ---- _flatpak_ref: the branch must be NAMED, always ---------------------------------------
#
# The bug this exists to prevent: every client-update query used to name no branch, and the
# punktfunk remote publishes `stable` AND `canary` — so `flatpak remote-info <origin>
# io.unom.punktfunk` failed with "Multiple branches available", the check swallowed the failure,
# and the panel reported the client up to date forever. One branch INSTALLED is not enough to
# make the query unambiguous; the ambiguity lives on the remote.
shutil.rmtree("/tmp/pf-test-home", ignore_errors=True)
# An arm install (Steam Frame): the resolver must look under the installed arch, not uname's.
_fp_root = Path("/tmp/pf-test-home/.local/share/flatpak/app/io.unom.punktfunk/aarch64")
client._flatpak = lambda: "/usr/bin/flatpak"
client._client_argv = _real_client_argv  # undo the fixture patches above

check("ref: nothing installed => None", client._flatpak_ref() is None)


def _install_branch(name: str):
    """A deployed branch: the `active` symlink is what distinguishes an install from leftovers."""
    commit = _fp_root / name / "deadbeef"
    commit.mkdir(parents=True, exist_ok=True)
    (_fp_root / name / "active").symlink_to("deadbeef")


(_fp_root / "canary").mkdir(parents=True, exist_ok=True)
check("ref: a branch dir without `active` is leftovers, not an install", client._flatpak_ref() is None)

_install_branch("canary")
ref = client._flatpak_ref()
check("ref: the single installed branch is used", ref == {
    "scope": "--user", "branch": "canary", "arch": "aarch64", "app_id": "io.unom.punktfunk",
    "ref": "io.unom.punktfunk//canary",
})
check(
    "ref: the launcher pins that branch, app id still LAST",
    client._client_argv() == [
        "/usr/bin/flatpak", "run", "--arch=aarch64", "--branch=canary", "io.unom.punktfunk",
    ],
)
# The pin must survive _cli_argv's rewrite, or the CLI runs a different build than the GUI.
check(
    "ref: --command= is inserted before the app id, keeping the pin",
    client._cli_argv() == [
        "/usr/bin/flatpak", "run", "--arch=aarch64", "--branch=canary",
        "--command=punktfunk", "io.unom.punktfunk",
    ],
)

# Two installed: `stable` is what a plain `flatpak run` resolves to, so it must be what we
# check and update too — otherwise a leftover stale `stable` wins the launch while `canary`
# gets the update, and the two halves disagree about which client is even running.
_install_branch("stable")
check("ref: with both installed, stable wins (what `flatpak run` picks)",
      client._flatpak_ref()["branch"] == "stable")

# A Deck still on the CamelCase id until its next update: it must launch and update under that
# id, and the new id wins as soon as both are there. The two ids are one directory on a
# case-insensitive /tmp (macOS), where this cannot be staged.
shutil.rmtree("/tmp/pf-test-home", ignore_errors=True)
Path("/tmp/pf-test-home").mkdir(parents=True)
(Path("/tmp/pf-test-home") / "CaseProbe").touch()
if (Path("/tmp/pf-test-home") / "caseprobe").exists():
    print("skip ref: legacy id (case-insensitive /tmp)")
else:
    _fp_root = Path("/tmp/pf-test-home/.local/share/flatpak/app/io.unom.Punktfunk/x86_64")
    _install_branch("stable")
    ref = client._flatpak_ref()
    check("ref: a legacy-only install resolves under its own id",
          ref["app_id"] == "io.unom.Punktfunk" and ref["ref"] == "io.unom.Punktfunk//stable")
    check("ref: the launcher runs the legacy id, still LAST",
          client._client_argv()[-1] == "io.unom.Punktfunk")
    _fp_root = Path("/tmp/pf-test-home/.local/share/flatpak/app/io.unom.punktfunk/x86_64")
    _install_branch("stable")
    check("ref: the new id wins once both are installed",
          client._flatpak_ref()["app_id"] == "io.unom.punktfunk")

shutil.rmtree("/tmp/pf-test-home", ignore_errors=True)

# ---- _cli_error: the CLI's exit-code contract -------------------------------------------
#
# The exit code alone picks the error: no prose is read. A client too old for a verb is told
# apart by the CLI's `--json` answer (`unknown-command`) or, for `discover`, by exit 5.
check("err: exit 5 is unresolved", client._cli_error(5) == "unresolved")
check("err: connect failed", client._cli_error(2) == "unreachable")
check("err: trust rejected", client._cli_error(3) == "refused")
check("err: needs a person", client._cli_error(6) == "needs-pairing")
check("err: nothing ran", client._cli_error(-1) == "client-unavailable")
check("err: unmapped code falls back", client._cli_error(4) == "client-error")

# ---- the host's refusal token: a safe shape, or nothing -----------------------------------
check("reject: a token passes", client._reject_token('{"ok": false, "reject": "not-armed"}') == "not-armed")
check("reject: null is none", client._reject_token('{"ok": false, "reject": null}') is None)
check("reject: junk stdout is none", client._reject_token("paired 10.0.0.5:9777 fp=ab") is None)
check("reject: a token off the shape is none", client._reject_token('{"reject": "Not Armed"}') is None)
check("reject: a non-string is none", client._reject_token('{"reject": 3}') is None)
check("fp: from the json object", client._paired_fp('{"ok": true, "fp": "ab12"}') == "ab12")
check("fp: from a client older than --json", client._paired_fp("paired 10.0.0.5:9777 fp=ab12\n") == "ab12")

# ---- _cli_json: a zero exit with junk on stdout is a FAILURE, not an empty result --------
import asyncio  # noqa: E402


def _fake_cli(rc: int, out: str, err: str = ""):
    async def run(_args, timeout=20.0, stdin_text=None):
        return rc, out, err

    return run


client._run_cli = _fake_cli(0, '{"hosts": [{"name": "desk"}]}')
got = asyncio.run(client._cli_json(["discover", "--json"]))
check("json: payload merged under ok", got == {"ok": True, "hosts": [{"name": "desk"}]})

client._run_cli = _fake_cli(0, "not json at all")
got = asyncio.run(client._cli_json(["discover", "--json"]))
check("json: unparseable stdout is an error, not an empty list", got["ok"] is False)
check("json: ...and says so specifically", got["error"] == "client-error")

client._run_cli = _fake_cli(5, '{"ok": false, "error": "unknown-command"}', 'unknown command "x"')
got = asyncio.run(client._cli_json(["library", "--json"]))
check("json: the CLI's own unknown-command answer is client-outdated", got["error"] == "client-outdated")
check("json: detail carries the CLI's own last line", "unknown command" in got["detail"])

client._run_cli = _fake_cli(5, "", 'no saved host matches "desk"')
got = asyncio.run(client._cli_json(["library", "desk", "--json"]))
check("json: exit 5 alone is unresolved, not outdated", got["error"] == "unresolved")
got = asyncio.run(client._cli_json(["discover", "--json"], outdated_rc=(5,)))
check("json: a verb that never exits 5 reads it as outdated", got["error"] == "client-outdated")

# ---- Plugin.library: the argv shape, and a ref that would read as a flag ------------------
#
# The game page's Stream button keys off this call, one per paired host per scan. The ref is
# a positional argument to the CLI, so one starting with `-` must never leave this backend.
captured: dict = {}


async def _capture_cli(args, timeout=20.0, stdin_text=None):
    captured["args"] = args
    return 0, '{"games": [{"id": "steam:570", "store": "steam", "title": "Dota 2"}]}', ""


client._run_cli = _capture_cli
got = asyncio.run(main.Plugin().library("2f1c-desk"))
check("library: argv is `library <ref> --json`", captured.get("args") == ["library", "2f1c-desk", "--json"])
check("library: games merged under ok", got["ok"] is True and got["games"][0]["id"] == "steam:570")
captured.clear()
got = asyncio.run(main.Plugin().library("--exec"))
check("library: a flag-shaped ref never reaches the CLI", "args" not in captured)
check("library: ...and is reported as unresolved", got == {
    "ok": False, "error": "unresolved", "detail": "bad host reference",
})
captured.clear()
got = asyncio.run(main.Plugin().library("   "))
check("library: an empty ref is refused the same way", "args" not in captured and got["ok"] is False)

# ---- Plugin.request_access: the advertised fingerprint pins the request -------------------
#
# Both OS installs of a dual-boot box answer at one address. Without `--fp` the CLI takes the
# newest record there, which can be the other OS.
desk = "ab" * 32
captured.clear()
asyncio.run(main.Plugin().request_access("192.168.1.9", 9777, "Steam Deck", desk))
check(
    "request access: argv pins the advertised fingerprint",
    captured.get("args")
    == [
        "pair", "192.168.1.9:9777", "--request-access", "--name", "Steam Deck", "--json",
        "--fp", desk,
    ],
)
captured.clear()
asyncio.run(main.Plugin().request_access("192.168.1.9", 9777, "Steam Deck"))
check("request access: no fingerprint, no --fp", "--fp" not in captured.get("args", []))
captured.clear()
got = asyncio.run(main.Plugin().request_access("192.168.1.9", 9777, "Steam Deck", "--exec"))
check("request access: a flag-shaped fingerprint never reaches the CLI", "args" not in captured)
check("request access: ...and is reported as bad-fingerprint", got["error"] == "bad-fingerprint")

# The host's refusal reaches the panel as a token; a client that exits 6 is too old for the flag.
client._run_cli = _fake_cli(3, '{"ok": false, "reject": "approval-timeout"}', "nobody approved it")
got = asyncio.run(main.Plugin().request_access("192.168.1.9", 9777, "Steam Deck", desk))
check("request access: refusal carries the host's token",
      got["error"] == "refused" and got["reject"] == "approval-timeout")
client._run_cli = _fake_cli(6, "", "no --pin - and no terminal to ask on")
got = asyncio.run(main.Plugin().request_access("192.168.1.9", 9777, "Steam Deck", desk))
check("request access: exit 6 is a client too old for the flag", got["error"] == "client-outdated")
# The code the panel shows while a request waits; a client without the verb gives none.
client._run_cli = _capture_cli
captured.clear()
asyncio.run(main.Plugin().device_code())
check("device code: argv is `device-code --json`", captured.get("args") == ["device-code", "--json"])
client._run_cli = _fake_cli(0, '{"code": "4F2A 91C3 07BE D512"}')
got = asyncio.run(main.Plugin().device_code())
check("device code: the code comes back under ok", got == {"code": "4F2A 91C3 07BE D512", "ok": True})
client._run_cli = _fake_cli(5, '{"ok": false, "error": "unknown-command"}', 'unknown command "device-code"')
got = asyncio.run(main.Plugin().device_code())
check("device code: an older client is client-outdated", got["error"] == "client-outdated")
client._run_cli = _fake_cli(0, '{"ok": true, "fp": "ab"}')
got = asyncio.run(main.Plugin().pair("192.168.1.9", 9777, "1234"))
check("pair: the fingerprint comes from the json object", got == {"ok": True, "fp": "ab"})
client._run_cli = _fake_cli(3, '{"ok": false, "kind": "crypto", "reject": null}', "the PIN was wrong")
got = asyncio.run(main.Plugin().pair("192.168.1.9", 9777, "1234"))
check("pair: a wrong PIN has no token", got["error"] == "refused" and "reject" not in got)
client._run_cli = _fake_cli(3, '{"ok": false, "kind": "timeout", "reject": null}', "the host did not answer")
got = asyncio.run(main.Plugin().pair("192.168.1.9", 9777, "1234"))
check("pair: a host that never answered is unreachable, not refused", got["error"] == "unreachable")

# ---- game_art: the inputs that become a URL and a file name are validated ----------------
#
# `appid` and `icon_hash` come from Steam's overview through the frontend; a bad one must fail
# closed rather than fetch an arbitrary path or write an arbitrary file name.
art._read_art = lambda appid, name: None  # no cache, no network in a unit check
shutil.rmtree("/tmp/pf-test-plugin", ignore_errors=True)  # no leftover fallback icon from a past run
check("art: a non-numeric appid is refused", asyncio.run(main.Plugin().game_art("x")) == {
    "ok": False, "error": "bad-appid",
})
check("art: zero is refused", asyncio.run(main.Plugin().game_art(0))["ok"] is False)
got = asyncio.run(main.Plugin().game_art(570, "../../etc/passwd"))
check("art: a hash that is not 40 hex chars fetches no icon", got == {"ok": True, "icon_path": ""})
check("art: local cache is asked per-app dir first, flat file second", [
    p.name for p in art._librarycache_candidates(570, "library_hero.jpg")
] == ["library_hero.jpg", "570_library_hero.jpg"])
check("art: the per-app dir is keyed by appid",
      art._librarycache_candidates(570, "logo.png")[0].parent.name == "570")
# The icon: without a hash nothing is fetched, but the Punktfunk icon stands in so the overlay
# never shows a gray box — and with no plugin assets at all, icon_path is honestly empty.
got = asyncio.run(main.Plugin().game_art(570, ""))
check("art: no hash, no assets => no icon path", got["icon_path"] == "")
Path("/tmp/pf-test-plugin/assets").mkdir(parents=True, exist_ok=True)
Path("/tmp/pf-test-plugin/assets/icon.png").write_bytes(b"png")
got = asyncio.run(main.Plugin().game_art(570, ""))
check("art: no hash => the Punktfunk icon stands in", got["icon_path"].endswith("assets/icon.png") and got["icon_type"] == "png" and got["icon"] == "cG5n")
# save_icon: only a real PNG for a real appid is written, into the settings dir, as .png
decky.DECKY_PLUGIN_SETTINGS_DIR = "/tmp/pf-test-settings"
shutil.rmtree("/tmp/pf-test-settings", ignore_errors=True)
import base64 as _b64  # noqa: E402
check("icon: non-png bytes are refused", asyncio.run(main.Plugin().save_icon(570, _b64.b64encode(b"\xff\xd8jpeg").decode()))["ok"] is False)
check("icon: junk base64 is refused", asyncio.run(main.Plugin().save_icon(570, "***"))["ok"] is False)
got = asyncio.run(main.Plugin().save_icon(570, _b64.b64encode(b"\x89PNG\r\n\x1a\n....").decode()))
check("icon: a png lands as <appid>.png in the settings dir", got["ok"] and got["path"] == "/tmp/pf-test-settings/icons/570.png" and Path(got["path"]).is_file())
check("art: Steam's current icon CDN is asked first", "shared.steamstatic.com" in art._ICON_CDNS[0])


# ---- _fetch_bytes: a body over the cap is refused rather than truncated -------------------
#
# A short read would hand a half-decoded image to the shortcut; raising lets the caller fall
# through to the next CDN instead.
class _FakeResp:
    def __init__(self, data):
        self._data = data

    def read(self, n=-1):
        return self._data[:n] if n and n > 0 else self._data

    def __enter__(self):
        return self

    def __exit__(self, *_a):
        return False


_body = b""
art.urllib.request.urlopen = lambda req, timeout=None, context=None: _FakeResp(_body)
_body = b"\xff\xd8small"
check("fetch: a body under the cap comes back whole", art._fetch_bytes("https://x/a.jpg") == b"\xff\xd8small")
_body = b"x" * (art._MAX_ART_BYTES + 1)
try:
    art._fetch_bytes("https://x/a.jpg")
    check("fetch: a body over the cap raises", False)
except ValueError:
    check("fetch: a body over the cap raises", True)

# ---- _field_from (flatpak info parsing, drives the client update check) ------------------
info = "        ID: io.unom.punktfunk\n    Origin: punktfunk-origin\n    Commit: abc123def\n"
check("field: commit", update._field_from(info, "Commit") == "abc123def")
check("field: origin", update._field_from(info, "Origin") == "punktfunk-origin")
check("field: absent", update._field_from(info, "Nope") == "")

# ---- flatpak runs in the C locale: `flatpak info` labels are localized ------------------
_seen: dict = {}
_real_exec = client._exec


async def _spy_exec(argv, label, timeout, **kw):
    _seen.update(kw)
    return 0, "", ""


client._exec = _spy_exec
client._flatpak = lambda: "/usr/bin/flatpak"
asyncio.run(update._flatpak_capture(["info", "--user", "io.unom.punktfunk//stable"]))
check("flatpak: info is read in the C locale", _seen.get("env_extra") == {"LC_ALL": "C"})
client._exec = _real_exec

# ---- end_game: a ref that would read as a flag is refused without a sentence --------------
check("end game: a flag-shaped ref is unresolved, with no English notice",
      asyncio.run(main.Plugin().end_game("--exec", "steam:570")) == {"ok": False, "error": "unresolved"})

# ---- _looks_outdated (the GTK-init signature of a client predating a headless flag) ------
check("outdated: gtk init noise", update._looks_outdated("cannot open display: \nGtk-WARNING") is True)
check("outdated: an ordinary error is not", update._looks_outdated("connection refused") is False)

# ---- _semver_tuple (plugin update comparison) --------------------------------------------
check("semver: plain", update._semver_tuple("1.2.3") == (1, 2, 3))
check("semver: pre-release suffix dropped", update._semver_tuple("1.2.3-rc1") == (1, 2, 3))
check("semver: short forms pad", update._semver_tuple("2") == (2, 0, 0))
check("semver: ordering", update._semver_tuple("0.10.0") > update._semver_tuple("0.9.9"))

# ---- the update cache has one home: update_client clears what check_update answers from ----
import time  # noqa: E402

update._update_cache.update(at=time.monotonic(), data={"current": "cached"})
check("update: check_update answers from the cache",
      asyncio.run(main.Plugin().check_update())["current"] == "cached")
_real_is_flatpak, _real_native_state = client._client_is_flatpak, update._native_update_state
_real_run_client = client._run_client


async def _helper_state():
    return {"applier": "helper"}


async def _applied(_args, timeout=20.0, env_extra=None):
    return 0, '{"ok": true, "changed": true}', ""


client._client_is_flatpak = lambda: False
update._native_update_state = _helper_state
client._run_client = _applied
got = asyncio.run(main.Plugin().update_client())
check("update: a native update lands and clears that cache",
      got == {"ok": True, "updated": True, "staged": False} and update._update_cache["data"] is None)
client._client_is_flatpak, update._native_update_state = _real_is_flatpak, _real_native_state
client._run_client = _real_run_client

# ---- _upsert_configset_entry (Steam Input layout binding) --------------------------------
#
# Untested until now, and the riskiest thing that survived the cut: it edits a file holding
# HUNDREDS of other games' controller bindings, in place. Every assertion below is about not
# touching them.
empty = steam_input._upsert_configset_entry("", "punktfunk", "template", "punktfunk.vdf")
check("vdf: builds the skeleton when the file is new", '"controller_config"' in empty)
check("vdf: the entry lands", '"punktfunk"' in empty and '"punktfunk.vdf"' in empty)

existing = (
    '"controller_config"\n'
    "{\n"
    '\t"halflife2"\n'
    "\t{\n"
    '\t\t"template"\t\t"other.vdf"\n'
    "\t}\n"
    "}\n"
)
added = steam_input._upsert_configset_entry(existing, "punktfunk", "template", "punktfunk.vdf")
check("vdf: an existing game's entry survives insertion", '"halflife2"' in added)
check("vdf: ours is inserted", '"punktfunk"' in added)

# Re-running must REPLACE our block, not accumulate a second one (this runs on every plugin
# session gated only by a localStorage marker, so idempotence is the whole contract).
twice = steam_input._upsert_configset_entry(added, "punktfunk", "template", "punktfunk.vdf")
check("vdf: idempotent", twice.count('"punktfunk"\n') == 1)
check("vdf: neighbour still intact after the rewrite", '"halflife2"' in twice)

# Steam keys non-Steam games by their LOWERCASE name, and files on disk may carry either case —
# a case-sensitive match would append a duplicate the game never reads.
mixed = existing.replace('"halflife2"', '"Punktfunk"')
replaced = steam_input._upsert_configset_entry(mixed, "punktfunk", "template", "punktfunk.vdf")
check("vdf: matches an existing key case-insensitively", replaced.count("unktfunk\"\n") == 1)

# ---- Decky's loader shape ---------------------------------------------------------------
#
# Decky appends only <plugin>/py_modules to sys.path and loads main.py as module `_`, so a
# module that imports `main` dies on the Deck while passing here. Load it the loader's way, in
# an isolated interpreter outside the repo, and hold it to every call the frontend makes.
import ast  # noqa: E402
import json  # noqa: E402
import re  # noqa: E402
import subprocess  # noqa: E402
import tempfile  # noqa: E402

_LOADER = """
import json, sys, types
from importlib.util import module_from_spec, spec_from_file_location
plugin = sys.argv[1]
decky = types.ModuleType("decky")
decky.DECKY_USER_HOME = decky.DECKY_PLUGIN_DIR = decky.DECKY_PLUGIN_SETTINGS_DIR = plugin
decky.logger = types.SimpleNamespace(**{
    n: (lambda *a, **k: None) for n in ("debug", "info", "warning", "error", "exception")
})
sys.modules["decky"] = decky
sys.path.append(plugin + "/py_modules")
spec = spec_from_file_location("_", plugin + "/main.py")
module = module_from_spec(spec)
spec.loader.exec_module(module)
print(json.dumps([n for n in dir(module.Plugin) if callable(getattr(module.Plugin, n))]))
"""
with tempfile.TemporaryDirectory() as _cwd:
    _run = subprocess.run(
        [sys.executable, "-I", "-B", "-c", _LOADER, str(_PLUGIN)],
        cwd=_cwd, capture_output=True, text=True, timeout=60,
    )
check("loader: main.py loads with only py_modules on the path", _run.returncode == 0)
if _run.returncode != 0:
    print(_run.stderr)
_methods = set(json.loads(_run.stdout or "[]"))
# Every `callable<…>("name")` under src/, wherever the frontend keeps its bridge.
_frontend: set = set()
for _ts in sorted((_PLUGIN / "src").rglob("*.ts*")):
    _frontend |= set(re.findall(r'callable\s*<[\s\S]*?>\(\s*"(\w+)"', _ts.read_text()))
check("loader: the frontend's calls are found, multi-line ones too",
      {"discover", "pair", "update_client"} <= _frontend)
_missing = sorted(_frontend - _methods)
check("loader: every frontend call has a Plugin method" + (f" (missing {_missing})" if _missing else ""),
      not _missing)


def _imports_main(path: Path) -> bool:
    for node in ast.walk(ast.parse(path.read_text())):
        if isinstance(node, ast.Import):
            names = [a.name for a in node.names]
        elif isinstance(node, ast.ImportFrom) and node.level == 0 and node.module:
            names = [node.module]
        else:
            continue
        if any(n.split(".")[0] == "main" for n in names):
            return True
    return False


_importers = [p.name for p in sorted((_PLUGIN / "py_modules" / "pf_decky").glob("*.py"))
              if _imports_main(p)]
check("loader: no pf_decky module imports main" + (f" ({_importers})" if _importers else ""),
      not _importers)

# ---- stream_running: the socket file outlives its client --------------------------------
import socket as _socket  # noqa: E402
import tempfile  # noqa: E402

with tempfile.TemporaryDirectory(dir="/tmp") as _run:
    _sock = Path(_run) / "ctl.sock"
    _real_ctl_sockets = session._ctl_sockets
    session._ctl_sockets = lambda: [Path(_run) / "absent.sock", _sock]
    check("running: no socket file is no stream",
          asyncio.run(main.Plugin().stream_running()) == {"running": False})
    _listener = _socket.socket(_socket.AF_UNIX)
    _listener.bind(str(_sock))
    _listener.listen(1)
    check("running: a socket that accepts is a stream",
          asyncio.run(main.Plugin().stream_running()) == {"running": True})
    _listener.close()  # the client exits; its socket file stays
    check("running: the stale file is left behind", _sock.is_socket())
    check("running: a socket nobody holds is no stream",
          asyncio.run(main.Plugin().stream_running()) == {"running": False})
    session._ctl_sockets = _real_ctl_sockets

# ---- kill_stream: a native client is found by its executable, never by name ---------------
import os  # noqa: E402

with tempfile.TemporaryDirectory(dir="/tmp") as _fake:
    _bin, _proc = Path(_fake) / "usr/bin", Path(_fake) / "proc"
    for _pid, _exe in (
        ("101", _bin / "punktfunk-session"),
        ("102", f"{_bin / 'punktfunk-client'} (deleted)"),
        ("103", _bin / "punktfunk"),
        ("104", _bin / "punktfunk-host"),
        ("105", "/opt/other/bin/punktfunk-session"),
    ):
        (_proc / _pid).mkdir(parents=True)
        os.symlink(str(_exe), _proc / _pid / "exe")
    (_proc / "106").mkdir()  # another user's process: no readable exe
    (_proc / "self").mkdir()
    check("kill: the session, the shell and the CLI in the client's bindir, nothing else",
          session._native_pids(_bin, _proc) == [101, 102, 103])
    check("kill: the session's name is past what the kernel keeps of one",
          len("punktfunk-session") > 15 and len(client.NATIVE_BIN) > 15)

_victim = subprocess.Popen(["sleep", "30"])
_real_native_pids, _real_argv, _real_flatpak = session._native_pids, client._client_argv, client._flatpak
session._native_pids = lambda bindir, proc=None: [_victim.pid]
client._client_argv = lambda: ["/usr/bin/punktfunk-client"]
client._flatpak = lambda: "/usr/bin/flatpak"
got = asyncio.run(main.Plugin().kill_stream())
try:
    _rc = _victim.wait(timeout=5)
except subprocess.TimeoutExpired:
    _victim.kill()
    _rc = None
check("kill: a native client's process gets SIGTERM", got == {"ok": True} and _rc == -15)
client._client_argv = lambda: None
check("kill: no client installed is an error",
      asyncio.run(main.Plugin().kill_stream()) == {"ok": False, "error": "client-not-found"})
session._native_pids, client._client_argv, client._flatpak = _real_native_pids, _real_argv, _real_flatpak

print()
if failures:
    print(f"{failures} check(s) FAILED")
    sys.exit(1)
print("all checks passed")
