#!/usr/bin/env python3
"""Every packaging channel states the same capabilities and groups, before anything is built.

packaging/linux/capabilities.txt is the capability matrix. Each channel grants capabilities in
its own recipe (a deb postinst, an rpm `%caps`, a pacman scriptlet, a NixOS wrapper, the Steam
Deck installer, the sysext builders). This reads each recipe and fails when a channel grants what
the matrix does not, or omits what it does. A grant it cannot attribute to a known binary fails
too. scripts/ci/assert-cap-matrix.sh checks the host and worker rows on the built package.

packaging/linux/punktfunk.sysusers is the group matrix. The deb, rpm, Arch and Steam Deck
recipes must install it, NixOS must declare the same groups, and no script or rule may create,
join or name a group it lacks.

packaging/linux/payload.txt's seats rows are the seat supervisor's files. The Nix and Steam Deck
recipes build nothing CI can read, so each must name every one of those files. The deb, rpm,
pacman and sysext removal steps must run door-remove.

The plugin runner is on by default: the game library's scanners are plugins, so a host whose runner
is off has an empty library. Each Linux channel's recipe must turn it on at install.

A SteamOS update rebuilds /etc and keeps only what its keep lists name, so every /etc path the
Steam Deck scripts install must be on scripts/punktfunk-atomic-keep.conf or under a tree SteamOS
keeps itself.

packaging/linux's units are system units. PID 1 reads each EnvironmentFile= as root and follows a
link, whatever User= says, and a `+` or `!` line hands what it read to root. So each such file sits
directly in a directory only root writes.

packaging/linux/restart-user-units.sh's two lists are the units an update restarts. The Bazzite and
Steam Deck updaters and the host's own restart them without it, so each names the same units. A
deb, rpm or pacman scriptlet restarts none: the helper does, once the host is unpacked.

The ports a firewall opener opens are the ports the host binds: the constants in pf-host-config's
listen.rs, pf-seats' model.rs and the host's gamestream/mod.rs, and the console unit's PORT with
the plugin origin one above it. Each firewalld service, ufw profile and NixOS list opens exactly
those, and data/platforms.json, the docs' table, states no other.

Usage: python3 scripts/ci/check-packaging-policy.py [--self-test | --inputs]
--inputs lists the files read outside packaging/ and scripts/, for check-workflow-paths.py.
"""
import glob
import itertools
import json
import os
import posixpath
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
MATRIX = "packaging/linux/capabilities.txt"
GATE = "scripts/ci/assert-cap-matrix.sh"
SYSUSERS = "packaging/linux/punktfunk.sysusers"
NIX_MODULE = "packaging/nix/nixos-module.nix"
# Recipes that install the sysusers file, by the text they must hold.
GROUP_RECIPES = ["packaging/debian/build-deb.sh", "packaging/debian/build-client-deb.sh",
                 "packaging/rpm/punktfunk.spec", "packaging/arch/PKGBUILD",
                 "scripts/steamdeck/lib.sh"]
PAYLOAD = "packaging/linux/payload.txt"
# The seats recipes CI can't build, and the payload files each may leave out. Neither has a
# removal step to run door-remove from.
SEATS_RECIPES = {
    # The module orders user@ after the supervisor itself.
    "packaging/nix/packages.nix": {"50-punktfunk-seats.conf", "door-remove"},
    "scripts/steamdeck/door.sh": {"door-remove"},
}
# Each channel that removes the seats as a package: the file holding its removal step, and the line
# that opens it. The step runs until a line opening with `}`, `EOF` or a spec section, and must run
# door-remove, which turns the door off before the package goes.
REMOVE_STEP = "door-remove"
SEATS_REMOVAL = [
    ("packaging/debian/build-seats-deb.sh", '"$STAGE/DEBIAN/prerm"'),
    ("packaging/rpm/punktfunk.spec", "%preun seats"),
    ("packaging/arch/punktfunk-seats.install", "pre_remove()"),
    ("packaging/bazzite/punktfunk-sysext.sh", "cmd_remove()"),
]
# A command at the start of a line or after `;`, `&&`, `then`: not inside prose or a string.
AT_COMMAND = r"(?:^|[;&|({]|\bthen|\bdo|\belse)\s*"
GLOBAL_ENABLE = (
    re.compile(AT_COMMAND + r"systemctl\s+--global\s+enable\s+punktfunk-scripting\.service", re.M),
    "systemctl --global enable punktfunk-scripting.service")
# Each channel's way of turning the runner on at install: the pattern its command must match
# outside a comment, and what the error names. `(?![\w@-])` ends a match at the unit's name, so
# punktfunk-scripting-seat@.service is not the runner.
RUNNER_ON = {
    "packaging/debian/build-scripting-deb.sh": GLOBAL_ENABLE,
    "packaging/rpm/punktfunk.spec": GLOBAL_ENABLE,
    "packaging/arch/punktfunk-scripting.install": GLOBAL_ENABLE,
    "packaging/bazzite/build-sysext.sh": (
        re.compile(AT_COMMAND + r"ln\s+-\w+\s+\S+\s+\S*default\.target\.wants/"
                                r"punktfunk-scripting\.service(?![\w@-])", re.M),
        "a link at default.target.wants/punktfunk-scripting.service"),
    "scripts/steamdeck/install.sh": (
        re.compile(AT_COMMAND + r"systemctl\s+--user\s+enable\s+--now\s+punktfunk-scripting"
                                r"(?![\w@-])", re.M),
        "systemctl --user enable --now punktfunk-scripting"),
    NIX_MODULE: (
        re.compile(r"\bautoStart\s*=\s*mkOption\s*\{[^}]*?\bdefault\s*=\s*cfg\.scripting\.enable\s*;"),
        "a scripting autoStart that defaults to cfg.scripting.enable"),
}
RESTART_TABLE = "packaging/linux/restart-user-units.sh"
HOST_UPDATER = "crates/host/punktfunk-host/src/update/linux.rs"
# Each updater that restarts the services itself, and the helper's lists it covers. The Steam
# Deck's system units are door.sh's.
RESTART_SITES = {
    "packaging/bazzite/punktfunk-sysext.sh": ("SYSTEM_UNITS", "USER_UNITS"),
    "scripts/steamdeck/install.sh": ("USER_UNITS",),
    "scripts/steamdeck/update.sh": ("USER_UNITS",),
    HOST_UPDATER: ("USER_UNITS",),
}
# The scriptlets of the channels whose packages run the helper.
RESTART_CALLERS = ["packaging/debian/*.sh", "packaging/rpm/*.spec", "packaging/arch/PKGBUILD",
                   "packaging/arch/*.install"]
# Where each port is bound, and the constants read from it.
LISTEN = "crates/host/pf-host-config/src/listen.rs"
SEAT_MODEL = "crates/host/pf-seats/src/model.rs"
GAMESTREAM = "crates/host/punktfunk-host/src/gamestream/mod.rs"
PORT_CONSTS = {
    LISTEN: ("NATIVE_PORT", "MGMT_PORT", "BROWSER_PORT"),
    SEAT_MODEL: ("DEFAULT_NATIVE_PORTS", "DEFAULT_BROWSER_PORTS"),
    GAMESTREAM: ("HTTPS_PORT", "HTTP_PORT", "RTSP_PORT", "VIDEO_PORT", "CONTROL_PORT", "AUDIO_PORT"),
}
# The packaged console's unit: PORT is the console's, PORT + 1 the plugin origin's.
WEB_UNIT = "scripts/punktfunk-web.service"
MDNS = 5353
FIREWALLD = {"native": "packaging/linux/punktfunk-native.xml",
             "gamestream": "packaging/linux/punktfunk-gamestream.xml",
             "web": "packaging/linux/punktfunk-web.xml"}
UFW = "packaging/linux/punktfunk.ufw"
# Tells an open firewall which port its stored rule lacks, by number.
PORT_HINTS = ["packaging/linux/post-install.sh"]
# The port table the docs and the website render.
PLATFORMS = "data/platforms.json"
# Scripts that tell a moved management port from the default, by the default's number.
MGMT_DEFAULT = ["packaging/linux/omarchy/punktfunk-omarchy", "scripts/steamdeck/lib.sh"]
# What the checks read outside packaging/ and scripts/: packaging-lint.yml runs on each.
INPUTS = [HOST_UPDATER, LISTEN, SEAT_MODEL, GAMESTREAM, PLATFORMS]
UNITS = "packaging/linux/*.service"
# Directories only root writes (punktfunk-seats.tmpfiles; pf-seats' env files are root's with a
# read ACL). Their subdirectories hold seat accounts' files, so a unit's file sits directly in one.
ROOT_DIRS = ("/var/lib/punktfunk-seats", "/run/punktfunk/seats")
KEEP_LIST = "scripts/punktfunk-atomic-keep.conf"
DECK_SCRIPTS = "scripts/steamdeck/*.sh"
# The /etc trees SteamOS's own keep list holds, as KEEP_LIST's comments say.
STOCK_KEPT = ("/etc/systemd/system/", "/etc/atomic-update.conf.d/")
# Created on first use by door-helper and the seat supervisor, so no package ships them.
RUNTIME_GROUPS = {"punktfunk-games"}
# Groups the distribution provides.
DISTRO_GROUPS = {"input", "root"}
HOST = "punktfunk-host"
WORKER = "punktfunk-encode-worker"
GAMESCOPE = "punktfunk-gamescope"


def strip_comments(text):
    """Drop `#` comments (shell, spec, nix); `$#` and `${#x}` have no space before the `#`."""
    return "\n".join(re.sub(r"(^|\s)#.*$", "", line) for line in text.splitlines())


def caps_norm(caps):
    """`CAP_SYS_NICE+ep` and `cap_sys_nice=ep` are one capability; none is `-`."""
    caps = caps.strip().lower().replace("+", "=")
    return "-" if caps in ("", "-", "(none)", "<none>") else caps


def binary_of(target, variables):
    """The punktfunk binary a setcap target names, by literal path or by shell variable."""
    target = target.strip("'\"")
    for var, binary in variables.items():
        if re.search(r"\$\{?%s\b" % var, target):
            return binary
    return os.path.basename(target)


# `setcap` in command position: after a line start, `sudo`, `;`, `&&`, `then`, not inside prose.
SETCAP = re.compile(r"""(?:^|[;&|({]|\bsudo|\bthen|\bdo|\belse)\s*setcap\s+(?!-)"""
                    r"""(?:'([^']*)'|"([^"]*)"|(\S+))\s+(\S+)""", re.M)


def read_setcap(text, variables):
    """[(binary, caps)] for each `setcap <caps> <path>`; `setcap -r` grants nothing."""
    return [(binary_of(m.group(4), variables), caps_norm(m.group(1) or m.group(2) or m.group(3)))
            for m in SETCAP.finditer(strip_comments(text))]


def read_rpm(text, variables):
    """[(binary, caps)] for each `%caps(<caps>) <path>` line of a spec."""
    return [(binary_of(m.group(2), variables), caps_norm(m.group(1)))
            for m in re.finditer(r"^%caps\(([^)]*)\)\s+(\S+)", strip_comments(text), re.M)]


def read_nix(text, variables):
    """[(binary, caps)] for each `security.wrappers.<name> = { capabilities = "…"; }`."""
    out = []
    for m in re.finditer(r"security\.wrappers\.([\w-]+)\s*=\s*\{(.*?)\n\s*\};",
                         strip_comments(text), re.S):
        caps = re.search(r'capabilities\s*=\s*"([^"]*)"', m.group(2))
        out.append((m.group(1), caps_norm(caps.group(1) if caps else "")))
    return out


def read_seal(text, variables):
    """The sysext builders grant the worker through `pf_seal_caps`, which reads the gate."""
    if re.search(r"^\s*pf_seal_caps\s", strip_comments(text), re.M):
        return [(WORKER, variables["WANT_WORKER_CAPS"])]
    return []


# channel -> (binaries it ships or None for all, [(files, reader, shell variables naming a binary)]).
# A glob reads every recipe of the channel, so a setcap added to a sibling script is seen.
CHANNELS = {
    "deb": (None, [(["packaging/debian/*.sh"], read_setcap, {})]),
    "rpm": (None, [(["packaging/rpm/*.spec", "packaging/gamescope/*.spec"], read_rpm, {})]),
    "arch": (None, [(["packaging/arch/PKGBUILD", "packaging/arch/*.install",
                      "packaging/gamescope/PKGBUILD", "packaging/gamescope/*.install"],
                     read_setcap, {})]),
    "nix": (None, [(["packaging/nix/*.nix"], read_nix, {})]),
    "steamdeck": (None, [(["scripts/steamdeck/*.sh"], read_setcap,
                          {"BIN": HOST, "WORKER": WORKER, "GS_BIN": GAMESCOPE})]),
    "source": ([GAMESCOPE], [(["packaging/gamescope/build-punktfunk-gamescope.sh"], read_setcap,
                              {"DEST": GAMESCOPE})]),
    "sysext-arch": (None, [(["packaging/arch/build-sysext.sh"], read_seal, {})]),
    "sysext-bazzite": (None, [(["packaging/bazzite/build-sysext.sh"], read_seal, {})]),
}


def read_matrix(text):
    """{binary: (caps, channels or None)}"""
    rows = {}
    for line in text.splitlines():
        fields = line.split("#")[0].split()
        if fields:
            rows[fields[0]] = (caps_norm(fields[1]), fields[2:] or None)
    return rows


def expected(matrix, binary, channel):
    caps, channels = matrix[binary]
    return caps if channels is None or channel in channels else "-"


def compare(matrix, channel, grants, covers=None):
    """[(binary, error)] for one channel's [(binary, caps)] against the matrix rows it covers."""
    errors = [(binary, f"{channel} grants {caps} to {binary}, which {MATRIX} has no row for")
              for binary, caps in grants if binary not in matrix]
    for binary in covers or matrix:
        got = {caps for b, caps in grants if b == binary} or {"-"}
        want = expected(matrix, binary, channel)
        if got != {want}:
            errors.append((binary, f"{channel} grants {binary} {' and '.join(sorted(got))}; "
                                   f"{MATRIX} says {want}"))
    return errors


# The gate's constants: the host and worker rows the matrix must hold.
GATE_CONST = re.compile(r"^(HOST_REL|WORKER_REL|WANT_WORKER_CAPS)='([^']*)'", re.M)


def gate_errors(matrix, const):
    errors = []
    for key, binary in (("HOST_REL", HOST), ("WORKER_REL", WORKER)):
        if os.path.basename(const.get(key, "")) != binary:
            errors.append(f"{GATE} {key} is {const.get(key)!r}, not .../{binary}")
    want = caps_norm(const.get("WANT_WORKER_CAPS", ""))
    if want != matrix[WORKER][0]:
        errors.append(f"{GATE} WANT_WORKER_CAPS is {want}; {MATRIX} says {matrix[WORKER][0]}")
    if matrix[HOST][0] != "-":
        errors.append(f"{MATRIX} gives {HOST} {matrix[HOST][0]}; {GATE} fails any host capability")
    return errors


def read(path):
    with open(os.path.join(ROOT, path), encoding="utf-8", errors="replace") as f:
        return f.read()


SYSUSERS_GROUP = re.compile(r"^g\s+(\S+)", re.M)
NIX_GROUP = re.compile(r"\busers\.groups\.([\w-]+)")


# The commands that name a group, and whether a runtime group may be named there. A rule or a
# polkit test needs the group to exist already; a script that creates or joins one may be first.
GROUP_NAMES = [
    (re.compile(r"\bgroupadd(?:\s+-{1,2}[\w-]+)*\s+([A-Za-z_][\w-]*)"), True),
    (re.compile(r"\busermod\s+-aG\s+([A-Za-z_][\w-]*)"), True),
    (re.compile(r"^\s*(?:if\s+)?ensure_group\s+([\w-]+)", re.M), False),
    (re.compile(r'\bGROUP:?="([^"]+)"'), False),
    (re.compile(r"\bchgrp\s+([A-Za-z_][\w-]*)"), False),
    (re.compile(r'isInGroup\("([^"]+)"\)'), False),
]


def group_errors(sysusers, nix_groups, texts):
    """[(path, error)] for the groups the sysusers file declares against what the channels create
    and name. `texts` is {path: recipe, script or rule text}."""
    declared = set(SYSUSERS_GROUP.findall(sysusers))
    nix = nix_groups - DISTRO_GROUPS
    errors = [(NIX_MODULE, f"{SYSUSERS} declares group {g}; the NixOS module does not")
              for g in sorted(declared - nix)]
    errors += [(NIX_MODULE, f"the NixOS module declares group {g}; {SYSUSERS} does not")
               for g in sorted(nix - declared)]
    for path, text in texts.items():
        text = strip_comments(text)
        for pattern, runtime_ok in GROUP_NAMES:
            allowed = declared | DISTRO_GROUPS | (RUNTIME_GROUPS if runtime_ok else set())
            errors += [(path, f"names group {g}, which no channel creates ({SYSUSERS})")
                       for g in pattern.findall(text) if g not in allowed]
    return errors


def tracked(*dirs):
    """{path: text} of the tracked files under `dirs`, less Markdown and Python."""
    names = subprocess.check_output(["git", "ls-files", "--", *dirs], cwd=ROOT, text=True)
    return {p: read(p) for p in names.split("\n") if p and not p.endswith((".md", ".py"))}


def tree_group_errors():
    installs = os.path.basename(SYSUSERS)
    errors = [(path, f"does not install {installs}")
              for path in GROUP_RECIPES if installs not in strip_comments(read(path))]
    nix = set(NIX_GROUP.findall(strip_comments(read(NIX_MODULE))))
    return errors + group_errors(read(SYSUSERS), nix, tracked("packaging", "scripts"))


def seats_errors(payload, texts):
    """[(path, error)] for each seats file in `payload` a recipe in `texts` never names."""
    rows = (line.split() for line in payload.splitlines() if line[:1].isalpha())
    names = sorted({os.path.basename(row[1]) for row in rows if row[0] == "seats"})
    return [(path, f"ships no {name}, which {PAYLOAD} lists for seats")
            for path, text in texts.items() for name in names
            if name not in SEATS_RECIPES.get(path, set()) and name not in strip_comments(text)]


def removal_errors(texts):
    """[(path, error)] for each SEATS_REMOVAL step in `texts` ({path: text}) that is missing or
    never runs door-remove."""
    errors = []
    for path, opener in SEATS_REMOVAL:
        lines = strip_comments(texts[path]).splitlines()
        start = next((i for i, line in enumerate(lines) if opener in line), None)
        if start is None:
            errors.append((path, f"has no removal step ({opener}) to run {REMOVE_STEP} from"))
            continue
        step = itertools.takewhile(lambda line: not re.match(r"(\}|EOF|%[a-z])", line),
                                   lines[start + 1:])
        if not any(REMOVE_STEP in line for line in step):
            errors.append((path, f"{opener} never runs {REMOVE_STEP}, so a removal with the door on "
                                 "leaves the owner's files and record behind"))
    return errors


def env_file_errors(units):
    """[(path, error)] for each EnvironmentFile= in `units` ({path: unit text}) that is not directly
    in one of ROOT_DIRS: an account that owns its directory could swap it for a link to any file
    root reads, or for settings a root line then runs with."""
    errors = []
    for path, text in units.items():
        for m in re.finditer(r"^[ \t]*EnvironmentFile[ \t]*=[ \t]*-?(.*?)[ \t]*$", text, re.M):
            name = m.group(1)
            if name and posixpath.dirname(name) not in ROOT_DIRS:
                errors.append((path, f"EnvironmentFile={name}: PID 1 reads it as root, from a "
                                     f"directory root doesn't own alone ({', '.join(ROOT_DIRS)})"))
    return errors


# A file path under /etc, not a tail of another path such as /usr/share/punktfunk/etc.
ETC_PATH = re.compile(r"(?<![\w.-])/etc/[^\s\"'$)]+")


def keep_errors(keep, texts):
    """[(path, error)] for each /etc file a script in `texts` installs or tees that `keep`, the
    keep list, does not keep. Its `*` matches within a path segment and `**` across."""
    kept = [re.compile("".join(".*" if t == "**" else "[^/]*" if t == "*" else re.escape(t)
                               for t in re.split(r"(\*\*|\*)", line.strip())) + r"\Z")
            for line in keep.splitlines() if line.startswith("/")]
    errors = []
    for path, text in texts.items():
        for line in strip_comments(text.replace("\\\n", " ")).splitlines():
            if not re.search(r"\b(install|tee)\b", line):
                continue
            errors += [(path, f"installs {etc}, which {KEEP_LIST} does not keep across a SteamOS update")
                       for etc in ETC_PATH.findall(line)
                       if not etc.endswith("/") and not etc.startswith(STOCK_KEPT)
                       and not any(k.match(etc) for k in kept)]
    return errors


def runner_errors(texts):
    """[(path, error)] for each RUNNER_ON recipe in `texts` ({path: text}) that never turns the
    plugin runner on. A comment or a string that names the command does not."""
    return [(path, f"never runs {shown}, so the plugin runner stays off here and the game library "
                   "comes up empty")
            for path, (pattern, shown) in RUNNER_ON.items()
            if not pattern.search(strip_comments(texts[path].replace("\\\n", " ")))]


UNIT_TABLE = re.compile(r'^(SYSTEM_UNITS|USER_UNITS)="([^"]*)"', re.M)
UNIT = re.compile(r"\bpunktfunk-[\w@-]+\.service\b")
# What follows `systemctl … try-restart|restart|enable --now`, and the list a `for` walks.
RESTARTS = re.compile(AT_COMMAND + r"(?:sudo\s+)?systemctl\b[^\n;&|]*?"
                                   r"\s(?:try-restart|restart|enable\s+--now)\s([^\n;&|]*)", re.M)
LOOPS = re.compile(AT_COMMAND + r"for\s+\w+\s+in\s([^\n;&|]*)", re.M)
# A row of the host updater's argument table: `["try-restart", "punktfunk-web.service"],`.
RUST_RESTART = re.compile(r'^\s*\["(?:try-)?restart",\s*"([^"]+)"\],?\s*$', re.M)


def restarted(text, loops=False):
    """The punktfunk units `text` restarts by a systemctl command or a row of Rust arguments.
    `loops` counts the list a `for` walks too, which is how an updater restarts several."""
    shell = strip_comments(text.replace("\\\n", " "))
    found = [m.group(1) for m in RESTARTS.finditer(shell)]
    if loops:
        found += [m.group(1) for m in LOOPS.finditer(shell)]
    return {u for rest in found for u in UNIT.findall(rest)} | set(RUST_RESTART.findall(text))


def restart_errors(table, sites, callers):
    """[(path, error)] for the helper's unit lists (`table`, its text) against each updater in
    `sites` and each scriptlet in `callers` (both {path: text})."""
    lists = {name: units.split() for name, units in UNIT_TABLE.findall(table)}
    errors = [(RESTART_TABLE, f"has no {name} list for the other updaters to match")
              for name in ("SYSTEM_UNITS", "USER_UNITS") if not lists.get(name)]
    if errors:
        return errors
    errors = [(RESTART_TABLE, f"restarts {u} by name; a unit belongs in SYSTEM_UNITS or USER_UNITS")
              for u in sorted(restarted(table, loops=True))]
    for path, names in RESTART_SITES.items():
        want = {u for name in names for u in lists[name]}
        got = restarted(sites[path], loops=True)
        errors += [(path, f"never restarts {u}, which {RESTART_TABLE} lists, so an update here "
                          "leaves the old build of it running")
                   for u in sorted(want - got)]
        errors += [(path, f"restarts {u}, which {RESTART_TABLE} does not list")
                   for u in sorted(got - want)]
    every = {u for units in lists.values() for u in units}
    return errors + [(path, f"restarts {u} itself; {RESTART_TABLE} restarts it once, after the "
                            "host it runs is unpacked")
                     for path, text in callers.items() for u in sorted(restarted(text) & every)]


RUST_PORTS = re.compile(r"^pub const (\w+): (?:u16|\[u16; \w+\]) = \[?([\d,\s]+)\]?;", re.M)


def rust_ports(text):
    """{constant: [ports]} for each `pub const NAME: u16 = n;` or `[u16; N] = [a, b];`."""
    return {name: [int(p) for p in re.findall(r"\d+", ports)]
            for name, ports in RUST_PORTS.findall(text)}


def profiles(consts, console):
    """{profile: {(proto, port)}}: what a firewall opener of each profile opens. `consts` is
    {constant: [ports]}, `console` the console unit's PORT."""
    def of(proto, *names):
        return {(proto, p) for name in names for p in consts[name]}

    mdns = {("udp", MDNS)}
    return {
        "native": of("udp", "NATIVE_PORT", "BROWSER_PORT", "DEFAULT_NATIVE_PORTS",
                     "DEFAULT_BROWSER_PORTS") | of("tcp", "MGMT_PORT") | mdns,
        "gamestream": of("tcp", "HTTPS_PORT", "HTTP_PORT", "RTSP_PORT")
        | of("udp", "VIDEO_PORT", "CONTROL_PORT", "AUDIO_PORT") | mdns,
        "web": {("tcp", console), ("tcp", console + 1)},
    }


def spans(proto, pairs):
    """{(proto, port)} for each (low, high or '') pair of port numbers."""
    return {(proto, p) for lo, hi in pairs for p in range(int(lo), int(hi or lo) + 1)}


def read_firewalld(text):
    """{(proto, port)} of a firewalld service: each `<port protocol= port=/>` outside a comment."""
    text = re.sub(r"<!--.*?-->", "", text, flags=re.S)
    return {rule for proto, lo, hi in
            re.findall(r'<port\s+protocol="(\w+)"\s+port="(\d+)(?:-(\d+))?"\s*/>', text)
            for rule in spans(proto, [(lo, hi)])}


def read_ufw(text):
    """{profile: {(proto, port)}} of a ufw application file: `ports=a,b/tcp|c:d/udp` per section."""
    out = {}
    for section in re.split(r"^(?=\[)", strip_comments(text), flags=re.M):
        name = re.match(r"\[punktfunk-([\w-]+)\]", section)
        ports = re.search(r"^ports=(.*)$", section, re.M)
        if name:
            out[name.group(1)] = {rule for spec, proto in
                                  re.findall(r"([\d,:]+)/(\w+)", ports.group(1) if ports else "")
                                  for rule in spans(proto, re.findall(r"(\d+)(?::(\d+))?", spec))}
    return out


def read_nix_ports(text):
    """{profile: {(proto, port)}} of the NixOS module's port lists. GameStream's lists add to the
    native ones, which carry mDNS, and the seats' range is the native profile's."""
    text = strip_comments(text)
    lists = {name: ports.split()
             for name, ports in re.findall(r"\b(\w+)\s*=\s*\[([\d\s]+)\]", text)}

    def of(proto, name):
        return spans(proto, [(p, "") for p in lists.get(name, ())])

    native = (of("tcp", "nativeTCP") | of("udp", "nativeUDP")
              | spans("udp", re.findall(r"\bfrom\s*=\s*(\d+);\s*to\s*=\s*(\d+);", text)))
    return {"native": native,
            "gamestream": of("tcp", "gamestreamTCP") | of("udp", "gamestreamUDP")
            | (native & {("udp", MDNS)}),
            "web": of("tcp", "allowedTCPPorts")}


def port_errors(want, openers):
    """[(path, error)] for each of `openers` ([(path, profile, {(proto, port)})]) that opens
    another set of ports than `want` ({profile: {(proto, port)}}) holds for its profile."""
    errors = []
    for path, profile, got in openers:
        errors += [(path, f"the {profile} profile leaves {proto} {port} closed, where the host or "
                          "its console listens")
                   for proto, port in sorted(want[profile] - got)]
        errors += [(path, f"the {profile} profile opens {proto} {port}, where nothing of ours "
                          "listens")
                   for proto, port in sorted(got - want[profile])]
    return errors


PORT_NUMBER = r"(?<![\w.-])%s(?![\w.-])"


def hint_errors(want, texts):
    """[(path, error)] for each four- or five-digit number outside a comment in `texts`
    ({path: text}) that is no port of `want`: a hint names the port a stored rule lacks."""
    ports = {str(port) for rules in want.values() for _, port in rules}
    return [(path, f"names port {n}, which no firewall profile opens")
            for path, text in texts.items()
            for n in sorted(set(re.findall(PORT_NUMBER % r"\d{4,5}", strip_comments(text))) - ports)]


def default_errors(port, texts):
    """[(path, error)] for each script in `texts` ({path: text}) that never names `port`, the
    default management port, outside a comment."""
    return [(path, f"never names {port}, the management port {LISTEN} defaults to")
            for path, text in texts.items()
            if not re.search(PORT_NUMBER % port, strip_comments(text))]


def platform_ports(text):
    """{(proto, port)} of the docs' port table: a row holds one `port` with its `also`, or a list
    per protocol."""
    out = set()
    for row in json.loads(text)["ports"].values():
        if "port" in row:
            row = {row["proto"]: [row["port"], *row.get("also", [])]}
        out |= {(proto, p) for proto in ("tcp", "udp") for p in row.get(proto, [])}
    return out


def tree_port_errors():
    consts, errors = {}, []
    for path, names in PORT_CONSTS.items():
        found = rust_ports(read(path))
        errors += [(path, f"has no `pub const {name}` port for the firewall openers to match")
                   for name in names if name not in found]
        consts.update(found)
    console = re.search(r"^Environment=PORT=(\d+)$", read(WEB_UNIT), re.M)
    if not console:
        errors.append((WEB_UNIT, "sets no PORT for the web firewall openers to match"))
    if errors:
        return errors
    want = profiles(consts, int(console.group(1)))
    ufw, nix = read_ufw(read(UFW)), read_nix_ports(read(NIX_MODULE))
    openers = [(path, profile, read_firewalld(read(path))) for profile, path in FIREWALLD.items()]
    openers += [(UFW, profile, ufw.get(profile, set())) for profile in want]
    openers += [(NIX_MODULE, profile, nix[profile]) for profile in want]
    return (port_errors(want, openers) + hint_errors(want, {p: read(p) for p in PORT_HINTS})
            + [(PLATFORMS, f"states {proto} {port}, which no firewall profile opens")
               for proto, port in sorted(platform_ports(read(PLATFORMS)) - set().union(*want.values()))]
            + default_errors(consts["MGMT_PORT"][0], {p: read(p) for p in MGMT_DEFAULT}))


def tree_errors():
    matrix = read_matrix(read(MATRIX))
    const = dict(GATE_CONST.findall(read(GATE)))
    variables = {"WANT_WORKER_CAPS": caps_norm(const.get("WANT_WORKER_CAPS", ""))}
    errors = [(GATE, e) for e in gate_errors(matrix, const)]
    channels = {c for _, chans in matrix.values() if chans for c in chans}
    errors += [(MATRIX, f"channel {c} is not one of {sorted(CHANNELS)}")
               for c in sorted(channels - CHANNELS.keys())]
    for channel, (covers, sources) in CHANNELS.items():
        grants, where, listed = [], {}, []
        for patterns, reader, names in sources:
            # Pattern order: the channel's own recipe comes first and takes a missing grant.
            files = [os.path.relpath(f, ROOT) for p in patterns
                     for f in sorted(glob.glob(os.path.join(ROOT, p)))]
            if not files:
                errors.append((patterns[0], f"{channel} has no recipe to read"))
            listed += files
            for f in files:
                found = reader(read(f), {**variables, **names})
                grants += found
                where.update({binary: f for binary, _ in found})
        first = listed[0] if listed else sources[0][0][0]
        errors += [(where.get(binary, first), e)
                   for binary, e in compare(matrix, channel, grants, covers)]
    seats = {path: read(path) for path in SEATS_RECIPES}
    deck = {os.path.relpath(f, ROOT): read(os.path.relpath(f, ROOT))
            for f in sorted(glob.glob(os.path.join(ROOT, DECK_SCRIPTS)))}
    units = {os.path.relpath(f, ROOT): read(os.path.relpath(f, ROOT))
             for f in sorted(glob.glob(os.path.join(ROOT, UNITS)))}
    if not units:
        errors.append((UNITS, "no unit to check"))
    return (errors + tree_group_errors() + seats_errors(read(PAYLOAD), seats)
            + removal_errors({path: read(path) for path, _ in SEATS_REMOVAL})
            + keep_errors(read(KEEP_LIST), deck) + env_file_errors(units)
            + runner_errors({path: read(path) for path in RUNNER_ON})
            + restart_errors(read(RESTART_TABLE), {path: read(path) for path in RESTART_SITES},
                             {os.path.relpath(f, ROOT): read(os.path.relpath(f, ROOT))
                              for p in RESTART_CALLERS
                              for f in sorted(glob.glob(os.path.join(ROOT, p)))})
            + tree_port_errors())


def check():
    errors = tree_errors()
    for path, message in errors:
        print(f"::error file={path}::{message}")
    if errors:
        print(f"{len(errors)} packaging policy mismatch(es): make the channel and its matrix agree.",
              file=sys.stderr)
        return 1
    print(f"check-packaging-policy: {len(CHANNELS)} channels match {MATRIX} and {SYSUSERS}")
    return 0


def self_test():
    fails = 0

    def expect(name, got, want):
        nonlocal fails
        if got != want:
            print(f"self-test {name}: got {got!r}, want {want!r}", file=sys.stderr)
            fails += 1

    ep, eip = "cap_sys_nice=ep", "cap_sys_nice=eip"
    expect("caps spellings", [caps_norm(c) for c in ("CAP_SYS_NICE+ep", ep, "(none)", "")],
           [ep, ep, "-", "-"])
    sh = """\
# setcap 'cap_sys_nice=ep' /usr/bin/punktfunk-host   <- a comment
setcap -r /usr/bin/punktfunk-host 2>/dev/null || true
  setcap 'cap_sys_nice=ep' usr/bin/punktfunk-encode-worker 2>/dev/null || true
sudo setcap "CAP_SYS_NICE=eip" "$GS_BIN" || warn x
sudo setcap cap_sys_nice=ep ${WORKER}
"""
    expect("setcap", read_setcap(sh, {"GS_BIN": GAMESCOPE, "WORKER": WORKER}),
           [(WORKER, ep), (GAMESCOPE, eip), (WORKER, ep)])
    spec = """\
# %caps(cap_sys_nice=ep) %{_bindir}/punktfunk-host
# Declared with %%caps rather than a %%post setcap
%{_bindir}/punktfunk-host
%caps(cap_sys_nice=ep) %{_bindir}/punktfunk-encode-worker
"""
    expect("rpm", read_rpm(spec, {}), [(WORKER, ep)])
    nix = """\
      # security.wrappers.punktfunk-host = { capabilities = "cap_sys_nice=ep"; };
      security.wrappers.punktfunk-encode-worker = {
        source = "${cfg.host.package}/bin/punktfunk-encode-worker";
        capabilities = "cap_sys_nice=ep";
        owner = "root";
      };
"""
    expect("nix", read_nix(nix, {}), [(WORKER, ep)])
    expect("seal", read_seal("# pf_seal_caps x\npf_seal_caps \"$STAGE\" why\n", {"WANT_WORKER_CAPS": ep}),
           [(WORKER, ep)])
    expect("seal absent", read_seal("# pf_seal_caps x\n", {"WANT_WORKER_CAPS": ep}), [])

    matrix = read_matrix(f"{HOST} -\n{WORKER} {ep}\n{GAMESCOPE} {eip} arch\n")
    ok = [(WORKER, ep)]
    expect("clean", compare(matrix, "deb", ok), [])
    expect("scoped row", compare(matrix, "arch", ok + [(GAMESCOPE, eip)]), [])
    expect("scoped row elsewhere", len(compare(matrix, "deb", ok + [(GAMESCOPE, eip)])), 1)
    expect("scoped row missing", len(compare(matrix, "arch", ok)), 1)
    expect("channel covering one binary", compare(matrix, "source", [(GAMESCOPE, "-")], [GAMESCOPE]), [])
    expect("host granted", len(compare(matrix, "rpm", ok + [(HOST, ep)])), 1)
    expect("worker changed in one channel", len(compare(matrix, "deb", [(WORKER, eip)])), 1)
    expect("worker grant removed", len(compare(matrix, "nix", [])), 1)
    expect("unknown binary", len(compare(matrix, "deb", ok + [("punktfunk-tray", ep)])), 1)
    expect("worker twice, two ways", len(compare(matrix, "deb", ok + [(WORKER, eip)])), 1)

    const = {"HOST_REL": "usr/bin/punktfunk-host", "WORKER_REL": "usr/bin/punktfunk-encode-worker",
             "WANT_WORKER_CAPS": ep}
    expect("gate agrees", gate_errors(matrix, const), [])
    expect("gate caps drift", len(gate_errors(matrix, {**const, "WANT_WORKER_CAPS": eip})), 1)
    expect("gate path drift", len(gate_errors(matrix, {**const, "HOST_REL": "usr/bin/x"})), 1)
    expect("matrix host capped", len(gate_errors(read_matrix(f"{HOST} {ep}\n{WORKER} {ep}\n"), const)), 1)
    expect("gate reader", GATE_CONST.findall("HOST_REL='usr/bin/punktfunk-host'\nWANT_WORKER_CAPS='x'\n"),
           [("HOST_REL", "usr/bin/punktfunk-host"), ("WANT_WORKER_CAPS", "x")])

    users = "# c\ng punktfunk -\ng punktfunk-update -\n"
    expect("sysusers", SYSUSERS_GROUP.findall(users), ["punktfunk", "punktfunk-update"])
    expect("nix groups", NIX_GROUP.findall("users.groups.input = { };\nusers.groups.punktfunk = { };\n"),
           ["input", "punktfunk"])
    nix_ok = {"input", "punktfunk", "punktfunk-update"}
    texts = {"a.sh": "getent group x || groupadd --system punktfunk\n# groupadd old\n"
                     "groupadd punktfunk-games\nif ensure_group punktfunk; then\n"
                     'usermod -aG input "$USER"\nusermod -aG punktfunk-games u\n',
             "a.rules": 'GROUP="input"\nRUN+="chgrp punktfunk /x"\n# GROUP="old"\n'}
    expect("groups agree", group_errors(users, nix_ok, texts), [])
    expect("nix lacks a group", len(group_errors(users, nix_ok - {"punktfunk-update"}, {})), 1)
    expect("nix adds a group", len(group_errors(users, nix_ok | {"extra"}, {})), 1)
    for name, text in (("groupadd", "groupadd --system x\n"), ("ensure_group", "ensure_group x\n"),
                       ("usermod", 'sudo usermod -aG x "$USER"\n'), ("udev GROUP", 'GROUP="plugdev"\n'),
                       ("chgrp", "chgrp x /sys/a\n"), ("polkit", 'subject.isInGroup("x")\n'),
                       ("a rule needs the group to exist", 'GROUP="punktfunk-games"\n')):
        expect(f"unlisted group: {name}", len(group_errors(users, nix_ok, {"b": text})), 1)

    payload = "# c\nhost usr/bin/punktfunk-host\nseats usr/libexec/punktfunk/door-helper\n" \
              "seats usr/lib/systemd/system/user@.service.d/50-punktfunk-seats.conf\n"
    nix = "packaging/nix/packages.nix"
    expect("seats named", seats_errors(payload, {nix: "install door-helper\n"}), [])
    expect("seats file missing", len(seats_errors(payload, {nix: "# door-helper\n"})), 1)
    expect("seats exemption is per recipe",
           len(seats_errors(payload, {"scripts/steamdeck/door.sh": "door-helper\n"})), 1)

    steps = {
        "packaging/debian/build-seats-deb.sh":
            "cat > \"$STAGE/DEBIAN/prerm\" <<'EOF'\nif [ \"$1\" = remove ]; then\n"
            "    /usr/libexec/punktfunk/door-remove || true\nfi\nEOF\n",
        "packaging/rpm/punktfunk.spec":
            "%preun seats\nif [ \"$1\" -eq 0 ]; then\n    %{_libexecdir}/punktfunk/door-remove || :\nfi\n"
            "%endif\n",
        "packaging/arch/punktfunk-seats.install": "pre_remove() {\n  /usr/libexec/punktfunk/door-remove\n}\n",
        "packaging/bazzite/punktfunk-sysext.sh": "cmd_remove() {\n  need_root\n  door-remove\n}\n",
    }
    expect("removal steps run door-remove", removal_errors(steps), [])
    for name, path, text in (
            ("only a disable", "packaging/arch/punktfunk-seats.install",
             "pre_remove() {\n  systemctl disable --now punktfunk-door.service\n}\n"),
            ("only in a comment", "packaging/arch/punktfunk-seats.install",
             "pre_remove() {\n  # door-remove\n  true\n}\n"),
            ("after the step", "packaging/rpm/punktfunk.spec",
             "%preun seats\n%systemd_preun punktfunk-door.service\n%endif\n%{_libexecdir}/door-remove\n"),
            ("no step", "packaging/bazzite/punktfunk-sysext.sh", "cmd_status() {\n  door-remove\n}\n")):
        expect(f"removal: {name}", len(removal_errors({**steps, path: text})), 1)

    keep = "# c\n/etc/udev/rules.d/60-punktfunk.rules\n/etc/pam.d/punktfunk-*\n/etc/a/**\n"
    deck = ("sudo install -Dm 0644 x /etc/pam.d/punktfunk-seat\n"
            "door_paths x \\\n    | sudo install -Dm 0644 /dev/stdin /etc/a/b/c.conf\n"
            'sudo tee "/etc/systemd/system/$unit" >/dev/null\n'
            'warn "sudo install -m644 x /etc/udev/rules.d/ &&"\n'
            "# sudo install x /etc/old.conf\n"
            "[ -f /etc/os-release ] && . /etc/os-release\n"
            'install -D x "$PF_OPT/etc/y.conf"\n')
    expect("deck installs kept", keep_errors(keep, {"d.sh": deck}), [])
    for name, text in (("not kept", "sudo install x /etc/pam.d/other\n"),
                       ("* stays in a segment", "sudo tee /etc/pam.d/punktfunk-x/y\n"),
                       ("continued line", "sudo install x \\\n  /etc/b.conf\n")):
        expect(f"deck install: {name}", len(keep_errors(keep, {"d.sh": text})), 1)

    door = ("[Service]\nUser=punktfunk\nExecStartPre=+-/usr/libexec/punktfunk/door-helper ports\n"
            "EnvironmentFile=-/var/lib/punktfunk-seats/host.env\n"
            "# EnvironmentFile=-/var/lib/punktfunk/host.env\nEnvironmentFile=\n")
    seat = "[Service]\nUser=%i\nEnvironmentFile=/run/punktfunk/seats/%i.env\nExecStopPost=-+/x %i\n"
    expect("env files root's", env_file_errors({"door": door, "seat": seat}), [])
    for name, text in (
            ("the door's own host.env", door + "EnvironmentFile=-/var/lib/punktfunk/host.env\n"),
            ("the owner's host.env", seat + "EnvironmentFile=-%h/.config/punktfunk/host.env\n"),
            ("a console without + lines", "User=punktfunk\nEnvironmentFile=/var/lib/punktfunk/web-password\n"),
            ("a seat's folder in root's", "EnvironmentFile=/var/lib/punktfunk-seats/hosts/%i/mgmt-token\n"),
            ("a way out of root's", "EnvironmentFile=/var/lib/punktfunk-seats/../punktfunk/host.env\n"),
            ("spaced", "  EnvironmentFile = /var/lib/punktfunk/host.env \n")):
        expect(f"env file: {name}", len(env_file_errors({"u": text})), 1)

    on = {
        "packaging/debian/build-scripting-deb.sh":
            'if [ -z "$2" ]; then\n    systemctl --global enable punktfunk-scripting.service || true\nfi\n',
        "packaging/rpm/punktfunk.spec":
            "if [ \"$1\" -eq 1 ]; then\n    systemctl --global enable punktfunk-scripting.service || :\nfi\n",
        "packaging/arch/punktfunk-scripting.install":
            "post_install() {\n  command -v systemctl && systemctl --global enable punktfunk-scripting.service\n}\n",
        "packaging/bazzite/build-sysext.sh":
            "ln -sf ../punktfunk-scripting.service \\\n"
            '  "$STAGE/usr/lib/systemd/user/default.target.wants/punktfunk-scripting.service"\n',
        "scripts/steamdeck/install.sh":
            "if [ \"$FRESH\" = 1 ]; then\n    systemctl --user enable --now punktfunk-scripting.service 2>/dev/null\nfi\n",
        NIX_MODULE:
            "scripting = {\n  autoStart = mkOption {\n    type = types.bool;\n"
            "    default = cfg.scripting.enable;\n    description = ''x'';\n  };\n};\n",
    }
    expect("runner on everywhere", runner_errors(on), [])
    expect("runner has a row per channel", sorted(on), sorted(RUNNER_ON))
    gl, ln, now = ("packaging/rpm/punktfunk.spec", "packaging/bazzite/build-sysext.sh",
                   "scripts/steamdeck/install.sh")
    for name, path, text in (
            ("global enable only in a comment", gl, "# systemctl --global enable punktfunk-scripting.service\n"),
            ("global enable only in a string", gl, 'echo "run systemctl --global enable punktfunk-scripting.service"\n'),
            ("global enable of another unit", gl, "systemctl --global enable punktfunk-web.service\n"),
            ("global enable of the seat unit", gl, "systemctl --global enable punktfunk-scripting-seat@.service\n"),
            ("global disable", gl, "systemctl --global disable punktfunk-scripting.service\n"),
            ("no global enable", gl, "echo installed\n"),
            ("wants link only in a comment", ln, "# ln -sf x default.target.wants/punktfunk-scripting.service\n"),
            ("wants link removed", ln, "rm -f $STAGE/default.target.wants/punktfunk-scripting.service\n"),
            ("wants link of another unit", ln, "ln -sf ../punktfunk-web.service $S/default.target.wants/punktfunk-web.service\n"),
            ("deck enable only in a message", now,
             'warn "couldn\'t start it — systemctl --user enable --now punktfunk-scripting"\n'),
            ("deck enable without --now", now, "systemctl --user enable punktfunk-scripting.service\n"),
            ("deck enable of the seat unit", now, "systemctl --user enable --now punktfunk-scripting-seat@1.service\n"),
            ("nix autoStart off", NIX_MODULE, "autoStart = mkOption {\n  default = false;\n};\n"),
            ("nix autoStart follows the host", NIX_MODULE, "autoStart = mkOption {\n  default = cfg.host.autoStart;\n};\n"),
            ("nix autoStart only in a comment", NIX_MODULE, "# autoStart = mkOption { default = cfg.scripting.enable; };\n")):
        expect(f"runner: {name}", len(runner_errors({**on, path: text})), 1)

    table = ('SYSTEM_UNITS="punktfunk-seats.service punktfunk-door.service"\n'
             'USER_UNITS="punktfunk-web.service punktfunk-host.service"\n'
             "systemctl --no-block try-restart $SYSTEM_UNITS 2>/dev/null\n"
             "systemctl show -P ActiveState punktfunk-update.service\n"
             'for unit in $USER_UNITS; do\n    systemctl --user try-restart "$unit"\ndone\n')
    bazzite, install, update = list(RESTART_SITES)[:3]
    # The boot branch starts what the first never names: a loop's list counts.
    sysext = ("restart_user_units() {\n  systemctl --no-block try-restart punktfunk-seats.service \\\n"
              "    punktfunk-door.service 2>/dev/null || :\n"
              '  if [ -n "$SUDO_USER" ] \\\n'
              '     && systemctl --user -M "$SUDO_USER@" try-restart punktfunk-web.service; then\n'
              "    echo restarted\n  else\n"
              "    for s in punktfunk-web.service punktfunk-host.service; do\n"
              '      systemctl --user --no-block start "$s"\n    done\n'
              '    echo "    systemctl --user restart punktfunk-web punktfunk-host"\n  fi\n}\n')
    deck = ("systemctl --user enable punktfunk-rebuild-check.service 2>/dev/null || true\n"
            "systemctl --user enable punktfunk-host.service 2>/dev/null\n"
            "systemctl --user restart punktfunk-host.service\n"
            'if [ "$WEB" = 1 ]; then systemctl --user restart --no-block punktfunk-web.service; fi\n'
            "systemctl --user restart xdg-permission-store 2>/dev/null || true\n")
    rust = ('        .args(["start", "--no-ask-password", "punktfunk-update.service"])\n'
            '    for args in [\n        ["restart", "punktfunk-web.service"],\n'
            '        ["try-restart", "punktfunk-host.service"],\n    ] {\n')
    sites = {bazzite: sysext, install: deck, update: deck, HOST_UPDATER: rust}
    unpack = {"p.sh": "for f in punktfunk-seats.service punktfunk-door.service; do\n"
                      '  install -Dm0644 "packaging/linux/$f" "$STAGE/$f"\ndone\n'
                      "systemctl daemon-reload || true\n"}
    expect("restarts agree", restart_errors(table, sites, unpack), [])
    expect("restart has a row per updater", sorted(sites), sorted(RESTART_SITES))
    host_row = '        ["try-restart", "punktfunk-host.service"],\n'
    for name, changed in (
            ("no user list", {"table": table.replace("USER_UNITS=", "OTHER=")}),
            ("helper restarts a unit by name",
             {"table": table + "systemctl try-restart punktfunk-web.service\n"}),
            ("sysext drops the door", {bazzite: sysext.replace("    punktfunk-door.service", "   ")}),
            ("sysext drops the loop's unit", {bazzite: sysext.replace(" punktfunk-host.service;", ";")}),
            ("deck restart only in a comment",
             {update: deck.replace("systemctl --user restart punktfunk-host", "# systemctl restart x")}),
            ("deck restart only in a message",
             {update: deck.replace("systemctl --user restart punktfunk-host",
                                   'warn "run systemctl --user restart punktfunk-host')}),
            ("deck enables without restarting",
             {install: deck.replace("systemctl --user restart punktfunk-host.service\n", "")}),
            ("deck restarts an unlisted unit",
             {install: deck + "systemctl --user try-restart punktfunk-tray.service\n"}),
            ("deck restarts a seat's runner",
             {install: deck + "systemctl --user try-restart punktfunk-scripting-seat@1.service\n"}),
            ("host updater drops a row", {HOST_UPDATER: rust.replace(host_row, "")}),
            ("host updater row only in a comment",
             {HOST_UPDATER: rust.replace(host_row, "        // " + host_row.strip() + "\n")}),
            ("a scriptlet restarts the supervisor itself",
             {"callers": {"p.install": "post_upgrade() {\n  systemctl try-restart punktfunk-seats.service\n}\n"}})):
        got = restart_errors(changed.get("table", table), {**sites, **changed},
                             changed.get("callers", unpack))
        expect(f"restart: {name}", len(got), 1)

    consts = rust_ports(
        "/// The native QUIC port.\npub const NATIVE_PORT: u16 = 9777;\n"
        "pub const MGMT_PORT: u16 = 47990;\npub const BROWSER_PORT: u16 = 9778;\n"
        "const PRIVATE_PORT: u16 = 1;\npub const MAX_SEATS: usize = 4;\n"
        "pub const RESERVED_PORTS: [u16; 2] = [CONSOLE_NATIVE_PORT, CONSOLE_MGMT_PORT];\n"
        "pub const DEFAULT_NATIVE_PORTS: [u16; MAX_SEATS] = [9779, 9780];\n"
        "pub const DEFAULT_BROWSER_PORTS: [u16; MAX_SEATS] = [\n    9781,\n    9782,\n];\n"
        "pub const HTTP_PORT: u16 = 47989;\npub const HTTPS_PORT: u16 = 47984;\n"
        "pub const RTSP_PORT: u16 = 48010;\npub const VIDEO_PORT: u16 = 47998;\n"
        "pub const CONTROL_PORT: u16 = 47999;\npub const AUDIO_PORT: u16 = 48000;\n")
    expect("rust ports", (consts["NATIVE_PORT"], consts["DEFAULT_NATIVE_PORTS"],
                          sorted(set(consts) & {"PRIVATE_PORT", "MAX_SEATS", "RESERVED_PORTS"})),
           ([9777], [9779, 9780], []))
    expect("rust ports has every constant", sorted(n for names in PORT_CONSTS.values() for n in names),
           sorted(consts))
    want = profiles(consts, 47992)
    xml = ('<!-- <port protocol="udp" port="1"/> -->\n<service>\n'
           '  <port protocol="udp" port="9777"/>  <!-- QUIC (default 9777) -->\n'
           '  <port protocol="udp" port="9778-9782"/>\n  <port protocol="udp" port="5353"/>\n'
           '  <port protocol="tcp" port="47990"/>\n</service>\n')
    ufw = ("# [punktfunk-old]\n# ports=1/tcp\n[punktfunk-native]\ntitle=host (9999)\n"
           "ports=9777/udp|9778:9782/udp|5353/udp|47990/tcp\n\n[punktfunk-gamestream]\n"
           "ports=47984,47989,48010/tcp|47998:48000/udp|5353/udp\n\n"
           "[punktfunk-web]\nports=47992,47993/tcp\n")
    nix = ("  nativeTCP = [ 47990 ]; # mgmt\n  nativeUDP = [\n    9777\n    9778\n    5353\n  ]; # x\n"
           "  gamestreamTCP = [\n    47984\n    47989\n    48010\n  ];\n"
           "  gamestreamUDP = [ 47998 47999 48000 ];\n  allowedUDPPorts = [ 5353 ];\n"
           "  # allowedTCPPorts = [ 1 ];\n  allowedTCPPorts = [\n    47992\n    47993\n  ];\n"
           "  { from = 9779; to = 9782; }\n")
    expect("firewalld", read_firewalld(xml), want["native"])
    expect("ufw", read_ufw(ufw), want)
    expect("nix ports", read_nix_ports(nix), want)
    expect("ports agree", port_errors(want, [("x", "native", read_firewalld(xml))]), [])
    for name, profile, got, count in (
            ("browser port closed", "native", read_firewalld(xml.replace("9778-", "9779-")), 1),
            ("seat range one short", "native", read_firewalld(xml.replace("-9782", "-9781")), 1),
            ("tcp port opened as udp", "native", read_firewalld(xml.replace('"tcp"', '"udp"')), 2),
            ("mDNS closed", "native", read_ufw(ufw.replace("|5353/udp|47990", "|47990"))["native"], 1),
            ("idle range opened", "gamestream",
             read_ufw(ufw.replace("47998:48000", "47998:48002"))["gamestream"], 2),
            ("profile removed", "web", read_ufw(ufw.replace("[punktfunk-web]", "")).get("web", set()), 2),
            ("nix lacks the plugin origin", "web", read_nix_ports(nix.replace("    47993\n", ""))["web"], 1),
            ("nix lacks the seats", "native", read_nix_ports(nix.replace("from = 9779", "x = 1"))["native"], 4),
            ("nix opens GameStream always", "native",
             read_nix_ports(nix.replace("    5353\n", "    5353\n    47998\n"))["native"], 1)):
        expect(f"ports: {name}", len(port_errors(want, [("x", profile, got)])), count)
    moved = profiles({**consts, "BROWSER_PORT": [9790]}, 47992)
    expect("ports: constant moved", len(port_errors(moved, [("x", "native", read_firewalld(xml))])), 2)
    expect("ports: console moved",
           len(port_errors(profiles(consts, 48100), [("x", "web", read_ufw(ufw)["web"])])), 4)
    hint = ("ufw status verbose | grep -q '47993'\necho \"predates UDP 9778 (browser streaming)\"\n"
            "# 1234\ninstall -Dm0755 a /etc/punktfunk/b\nTimeoutStartSec=20min\n")
    docs = ('{"ports": {"web": {"port": 47992, "proto": "tcp", "also": [47993]},\n'
            ' "mdns": {"port": 5353, "proto": "udp", "what": "mDNS"},\n'
            ' "gamestream": {"tcp": [47984, 48010], "udp": [47998], "what": "x"}}, "firewall": {}}')
    expect("docs port table", platform_ports(docs),
           {("tcp", 47992), ("tcp", 47993), ("udp", 5353), ("tcp", 47984), ("tcp", 48010),
            ("udp", 47998)})
    expect("docs port table agrees", platform_ports(docs) - set().union(*want.values()), set())
    expect("docs port table: stale port",
           platform_ports(docs) - set().union(*profiles(consts, 48100).values()),
           {("tcp", 47992), ("tcp", 47993)})
    expect("hints name ports", hint_errors(want, {"p.sh": hint}), [])
    expect("hints: stale port", len(hint_errors(moved, {"p.sh": hint})), 1)
    expect("default named", default_errors(47990, {"a.sh": 'if [ "$bind" != 0.0.0.0:47990 ]; then\n',
                                                   "b": '[[ "$port" != 47990 ]] && echo "$port"\n'}), [])
    for name, text in (("only in a comment", "# off 47990 (Sunshine's web UI)\n"),
                       ("another port", '[[ "$port" != 47991 ]]\n'),
                       ("inside a longer number", "sleep 147990\n")):
        expect(f"default: {name}", len(default_errors(47990, {"a.sh": text})), 1)
    if fails:
        return 1
    print("check-packaging-policy self-test ok")
    return 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if argv[1:] == ["--inputs"]:
        print("\n".join(INPUTS))
        return 0
    if argv[1:]:
        print("usage: check-packaging-policy.py [--self-test | --inputs]", file=sys.stderr)
        return 2
    return check()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
