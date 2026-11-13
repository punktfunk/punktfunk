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

A SteamOS update rebuilds /etc and keeps only what its keep lists name, so every /etc path the
Steam Deck scripts install must be on scripts/punktfunk-atomic-keep.conf or under a tree SteamOS
keeps itself.

packaging/linux's units are system units. PID 1 reads each EnvironmentFile= as root and follows a
link, whatever User= says, and a `+` or `!` line hands what it read to root. So each such file sits
directly in a directory only root writes.

Usage: python3 scripts/ci/check-packaging-policy.py [--self-test]
"""
import glob
import itertools
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
            + keep_errors(read(KEEP_LIST), deck) + env_file_errors(units))


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
    if fails:
        return 1
    print("check-packaging-policy self-test ok")
    return 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if argv[1:]:
        print("usage: check-packaging-policy.py [--self-test]", file=sys.stderr)
        return 2
    return check()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
