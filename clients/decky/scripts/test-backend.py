#!/usr/bin/env python3
"""Unit checks for main.py's pure helpers — stdlib only, no Decky runtime needed.

Stubs the ``decky`` module (main.py imports it at module level), then asserts the argv
shapes, the exit-code mapping and the Steam VDF editor against fixtures.

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

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import main  # noqa: E402  (the plugin backend)

# The argv fixtures below monkey-patch `_client_argv` to pin one install shape; the
# _flatpak_ref block wants the REAL resolver back, so keep a handle on it.
_real_client_argv = main._client_argv

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
main._client_argv = lambda: ["/usr/bin/flatpak", "run", "--arch=x86_64", "io.unom.Punktfunk"]
main._flatpak = lambda: "/usr/bin/flatpak"
check(
    "cli argv: flatpak form, app id last",
    main._cli_argv()
    == [
        "/usr/bin/flatpak",
        "run",
        "--arch=x86_64",
        "--command=punktfunk",
        "io.unom.Punktfunk",
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
main._client_argv = lambda: [str(tmp / "punktfunk-client")]
check("cli argv: native without a sibling CLI is None", main._cli_argv() is None)
(tmp / "punktfunk").write_text("")
check("cli argv: native sibling found", main._cli_argv() == [str(tmp / "punktfunk")])

# ---- _flatpak_ref: the branch must be NAMED, always ---------------------------------------
#
# The bug this exists to prevent: every client-update query used to name no branch, and the
# punktfunk remote publishes `stable` AND `canary` — so `flatpak remote-info <origin>
# io.unom.Punktfunk` failed with "Multiple branches available", the check swallowed the failure,
# and the panel reported the client up to date forever. One branch INSTALLED is not enough to
# make the query unambiguous; the ambiguity lives on the remote.
shutil.rmtree("/tmp/pf-test-home", ignore_errors=True)
# An arm install (Steam Frame): the resolver must look under the machine's arch, not x86_64.
main._FLATPAK_ARCH = "aarch64"
_fp_root = Path("/tmp/pf-test-home/.local/share/flatpak/app/io.unom.Punktfunk/aarch64")
main._flatpak = lambda: "/usr/bin/flatpak"
main._client_argv = _real_client_argv  # undo the fixture patches above

check("ref: nothing installed => None", main._flatpak_ref() is None)


def _install_branch(name: str):
    """A deployed branch: the `active` symlink is what distinguishes an install from leftovers."""
    commit = _fp_root / name / "deadbeef"
    commit.mkdir(parents=True, exist_ok=True)
    (_fp_root / name / "active").symlink_to("deadbeef")


(_fp_root / "canary").mkdir(parents=True, exist_ok=True)
check("ref: a branch dir without `active` is leftovers, not an install", main._flatpak_ref() is None)

_install_branch("canary")
ref = main._flatpak_ref()
check("ref: the single installed branch is used", ref == {
    "scope": "--user", "branch": "canary", "ref": "io.unom.Punktfunk//canary",
})
check(
    "ref: the launcher pins that branch, app id still LAST",
    main._client_argv() == [
        "/usr/bin/flatpak", "run", "--arch=aarch64", "--branch=canary", "io.unom.Punktfunk",
    ],
)
# The pin must survive _cli_argv's rewrite, or the CLI runs a different build than the GUI.
check(
    "ref: --command= is inserted before the app id, keeping the pin",
    main._cli_argv() == [
        "/usr/bin/flatpak", "run", "--arch=aarch64", "--branch=canary",
        "--command=punktfunk", "io.unom.Punktfunk",
    ],
)

# Two installed: `stable` is what a plain `flatpak run` resolves to, so it must be what we
# check and update too — otherwise a leftover stale `stable` wins the launch while `canary`
# gets the update, and the two halves disagree about which client is even running.
_install_branch("stable")
check("ref: with both installed, stable wins (what `flatpak run` picks)",
      main._flatpak_ref()["branch"] == "stable")

shutil.rmtree("/tmp/pf-test-home", ignore_errors=True)

# ---- _cli_error: the CLI's exit-code contract -------------------------------------------
#
# Exit 5 + `unknown command` is how a client too old for a verb announces itself — the ONE
# signature the panel turns into "update the client" plus the button that fixes it. Getting it
# wrong makes an out-of-date client look like a broken plugin.
check(
    "err: unknown verb => client-outdated",
    main._cli_error(5, 'unknown command "discover"\n\npunktfunk — the Punktfunk client')
    == "client-outdated",
)
check(
    "err: exit 5 without that phrase is NOT outdated",
    main._cli_error(5, 'no saved host matches "desk"') == "unresolved",
)
check("err: connect failed", main._cli_error(2, "unreachable 10.0.0.1:9777") == "unreachable")
check("err: trust rejected", main._cli_error(3, "wrong PIN") == "refused")
check("err: needs a person", main._cli_error(6, "pair it first") == "needs-pairing")
check("err: nothing ran", main._cli_error(-1, "") == "client-unavailable")
check("err: unmapped code falls back", main._cli_error(4, "renderer") == "client-error")

# ---- _cli_json: a zero exit with junk on stdout is a FAILURE, not an empty result --------
import asyncio  # noqa: E402


def _fake_cli(rc: int, out: str, err: str = ""):
    async def run(_args, timeout=20.0):
        return rc, out, err

    return run


main._run_cli = _fake_cli(0, '{"hosts": [{"name": "desk"}]}')
got = asyncio.run(main._cli_json(["discover", "--json"]))
check("json: payload merged under ok", got == {"ok": True, "hosts": [{"name": "desk"}]})

main._run_cli = _fake_cli(0, "not json at all")
got = asyncio.run(main._cli_json(["discover", "--json"]))
check("json: unparseable stdout is an error, not an empty list", got["ok"] is False)
check("json: ...and says so specifically", got["error"] == "client-error")

main._run_cli = _fake_cli(5, "", 'unknown command "discover"')
got = asyncio.run(main._cli_json(["discover", "--json"]))
check("json: old client surfaces as client-outdated", got["error"] == "client-outdated")
check("json: detail carries the CLI's own last line", "unknown command" in got["detail"])

# ---- Plugin.library: the argv shape, and a ref that would read as a flag ------------------
#
# The game page's Stream button keys off this call, one per paired host per scan. The ref is
# a positional argument to the CLI, so one starting with `-` must never leave this backend.
captured: dict = {}


async def _capture_cli(args, timeout=20.0, stdin_text=None):
    captured["args"] = args
    return 0, '{"games": [{"id": "steam:570", "store": "steam", "title": "Dota 2"}]}', ""


main._run_cli = _capture_cli
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

# ---- game_art: the inputs that become a URL and a file name are validated ----------------
#
# `appid` and `icon_hash` come from Steam's overview through the frontend; a bad one must fail
# closed rather than fetch an arbitrary path or write an arbitrary file name.
main._read_art = lambda appid, name: None  # no cache, no network in a unit check
shutil.rmtree("/tmp/pf-test-plugin", ignore_errors=True)  # no leftover fallback icon from a past run
check("art: a non-numeric appid is refused", asyncio.run(main.Plugin().game_art("x")) == {
    "ok": False, "error": "bad-appid",
})
check("art: zero is refused", asyncio.run(main.Plugin().game_art(0))["ok"] is False)
got = asyncio.run(main.Plugin().game_art(570, "../../etc/passwd"))
check("art: a hash that is not 40 hex chars fetches no icon", got == {"ok": True, "icon_path": ""})
check("art: local cache is asked per-app dir first, flat file second", [
    p.name for p in main._librarycache_candidates(570, "library_hero.jpg")
] == ["library_hero.jpg", "570_library_hero.jpg"])
check("art: the per-app dir is keyed by appid",
      main._librarycache_candidates(570, "logo.png")[0].parent.name == "570")
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
check("art: Steam's current icon CDN is asked first", "shared.steamstatic.com" in main._ICON_CDNS[0])


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
main.urllib.request.urlopen = lambda req, timeout=None, context=None: _FakeResp(_body)
_body = b"\xff\xd8small"
check("fetch: a body under the cap comes back whole", main._fetch_bytes("https://x/a.jpg") == b"\xff\xd8small")
_body = b"x" * (main._MAX_ART_BYTES + 1)
try:
    main._fetch_bytes("https://x/a.jpg")
    check("fetch: a body over the cap raises", False)
except ValueError:
    check("fetch: a body over the cap raises", True)

# ---- _field_from (flatpak info parsing, drives the client update check) ------------------
info = "        ID: io.unom.Punktfunk\n    Origin: punktfunk-origin\n    Commit: abc123def\n"
check("field: commit", main._field_from(info, "Commit") == "abc123def")
check("field: origin", main._field_from(info, "Origin") == "punktfunk-origin")
check("field: absent", main._field_from(info, "Nope") == "")

# ---- _looks_outdated (the GTK-init signature of a client predating a headless flag) ------
check("outdated: gtk init noise", main._looks_outdated("cannot open display: \nGtk-WARNING") is True)
check("outdated: an ordinary error is not", main._looks_outdated("connection refused") is False)

# ---- _semver_tuple (plugin update comparison) --------------------------------------------
check("semver: plain", main._semver_tuple("1.2.3") == (1, 2, 3))
check("semver: pre-release suffix dropped", main._semver_tuple("1.2.3-rc1") == (1, 2, 3))
check("semver: short forms pad", main._semver_tuple("2") == (2, 0, 0))
check("semver: ordering", main._semver_tuple("0.10.0") > main._semver_tuple("0.9.9"))

# ---- _upsert_configset_entry (Steam Input layout binding) --------------------------------
#
# Untested until now, and the riskiest thing that survived the cut: it edits a file holding
# HUNDREDS of other games' controller bindings, in place. Every assertion below is about not
# touching them.
empty = main._upsert_configset_entry("", "punktfunk", "template", "punktfunk.vdf")
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
added = main._upsert_configset_entry(existing, "punktfunk", "template", "punktfunk.vdf")
check("vdf: an existing game's entry survives insertion", '"halflife2"' in added)
check("vdf: ours is inserted", '"punktfunk"' in added)

# Re-running must REPLACE our block, not accumulate a second one (this runs on every plugin
# session gated only by a localStorage marker, so idempotence is the whole contract).
twice = main._upsert_configset_entry(added, "punktfunk", "template", "punktfunk.vdf")
check("vdf: idempotent", twice.count('"punktfunk"\n') == 1)
check("vdf: neighbour still intact after the rewrite", '"halflife2"' in twice)

# Steam keys non-Steam games by their LOWERCASE name, and files on disk may carry either case —
# a case-sensitive match would append a duplicate the game never reads.
mixed = existing.replace('"halflife2"', '"Punktfunk"')
replaced = main._upsert_configset_entry(mixed, "punktfunk", "template", "punktfunk.vdf")
check("vdf: matches an existing key case-insensitively", replaced.count("unktfunk\"\n") == 1)

print()
if failures:
    print(f"{failures} check(s) FAILED")
    sys.exit(1)
print("all checks passed")
