#!/usr/bin/env python3
"""Every crate a workflow compiles is in that workflow's `paths:` filter.

For each row of BUILDS, `cargo metadata --filter-platform <triple>` gives the target's
dependency graph; every local path crate reachable from the row's packages must match a glob
in each `paths:` leg (push, pull_request) the workflow has. A leg with no `paths:` runs on
every change and needs nothing. A missing line means a change to that crate merges without
the build that compiles it.

A package recipe reads files that are not crates: udev rules, units, desktop entries. For each
row of RECIPES, every tracked file the workflow's recipes name outside a comment must match a
glob in each `paths:` leg too. A workflow with both a push and a pull_request leg keeps the two
lists equal, so the second is never the one that rots.

packaging-lint compiles nothing: its `pull_request` paths must match every file it lints and
every workflow it checks.

Usage: python3 scripts/ci/check-workflow-paths.py [--self-test | --lanes]
--lanes runs the checks that need no cargo: the lane, the recipes and the two-leg match.
"""
import functools
import glob
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from cargo_graph import closure  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

LINUX_HOST_AND_CLIENT = [
    "punktfunk-host", "punktfunk-encode-worker", "punktfunk-tray",
    "punktfunk-client-linux", "punktfunk-client-session", "punktfunk-cli", "pf-update",
]
LINUX_HOST_FEATURES = "punktfunk-host/nvenc,punktfunk-host/vulkan-encode"

# (workflow, manifest, target triple, packages, --features). Packages None = every member.
# One row per workflow and workspace: the arch with the widest feature set.
BUILDS = [
    ("windows-host", "Cargo.toml", "x86_64-pc-windows-msvc",
     ["punktfunk-host", "punktfunk-tray", "display-disturb"],
     "punktfunk-host/nvenc,punktfunk-host/qsv"),
    ("windows-client", "Cargo.toml", "x86_64-pc-windows-msvc",
     ["punktfunk-client-windows", "punktfunk-client-session", "punktfunk-cli"], None),
    ("windows-drivers", "packaging/windows/drivers/Cargo.toml", "x86_64-pc-windows-msvc",
     None, None),
    ("windows-host", "crates/host/pf-seat-keeper/Cargo.toml", "x86_64-pc-windows-msvc", None, None),
    ("setup-windows", "Cargo.toml", "x86_64-pc-windows-msvc",
     ["punktfunk-setup", "punktfunk-setup-win"], None),
    ("macos-host", "Cargo.toml", "aarch64-apple-darwin",
     ["punktfunk-host", "punktfunk-setup"], None),
    ("apple", "Cargo.toml", "aarch64-apple-ios", ["punktfunk-client-apple"], None),
    ("deb", "Cargo.toml", "x86_64-unknown-linux-gnu", LINUX_HOST_AND_CLIENT, LINUX_HOST_FEATURES),
    ("rpm", "Cargo.toml", "x86_64-unknown-linux-gnu", LINUX_HOST_AND_CLIENT, LINUX_HOST_FEATURES),
    ("arch", "Cargo.toml", "x86_64-unknown-linux-gnu", LINUX_HOST_AND_CLIENT, LINUX_HOST_FEATURES),
    ("flatpak", "Cargo.toml", "x86_64-unknown-linux-gnu",
     ["punktfunk-client-linux", "punktfunk-client-session", "punktfunk-cli"], None),
    ("setup-binary", "Cargo.toml", "x86_64-unknown-linux-musl", ["punktfunk-setup"], None),
    ("installer-smoke", "Cargo.toml", "x86_64-unknown-linux-musl", ["punktfunk-setup"], None),
]

LEGS = ("push", "pull_request")

# The lane re-runs when any workflow changes, since it holds each one's path lists.
LINT_LANE = ".gitea/workflows/packaging-lint.yml"

# workflow -> the recipes it builds from. A file a recipe names is an input of that workflow.
RECIPES = {
    "deb": ["packaging/debian/*.sh"],
    "rpm": ["packaging/rpm/*.spec", "packaging/rpm/*.sh", "packaging/gamescope/*.spec",
            "packaging/gamescope/*.sh", "packaging/bazzite/build-sysext.sh",
            "packaging/linux/sysext-lib.sh"],
    "arch": ["packaging/arch/PKGBUILD", "packaging/arch/*.sh", "packaging/gamescope/PKGBUILD"],
    "nix": ["packaging/nix/*.nix", "flake.nix"],
    "flatpak": ["packaging/flatpak/*.yml", "packaging/flatpak/*.sh", "packaging/flatpak/*.py"],
    "macos-host": ["packaging/macos/*.sh"],
}


def path_legs(text):
    """{leg: [glob, …]} for each `on:` leg that carries a `paths:` list (2-space YAML)."""
    legs, leg, in_on, in_paths = {}, None, False, False
    for line in text.splitlines():
        body = line.strip()
        if not body or body.startswith("#"):
            continue
        indent = len(line) - len(line.lstrip(" "))
        if indent == 0:
            in_on, leg, in_paths = body == "on:", None, False
        elif not in_on:
            continue
        elif indent == 2:
            leg, in_paths = body.rstrip(":"), False
        elif indent == 4:
            in_paths = body == "paths:"
            if in_paths:
                legs[leg] = []
        elif in_paths and body.startswith("- "):
            item = body[2:].strip()
            if item[:1] in "'\"":
                item = item[1:item.index(item[0], 1)]
            else:
                item = item.split(" #")[0].strip()
            legs[leg].append(item)
    return legs


def glob_re(pattern):
    """Gitea's `paths:` glob: `**` crosses `/`, `*` and `?` do not."""
    out = []
    for tok in re.split(r"(\*\*|\*|\?)", pattern):
        out.append({"**": ".*", "*": "[^/]*", "?": "[^/]"}.get(tok, re.escape(tok)))
    return re.compile("".join(out) + r"\Z")


def uncovered(crate_dirs, globs):
    """Crate dirs whose Cargo.toml no glob matches, less those nested in another one."""
    res = [glob_re(g) for g in globs]
    miss = [d for d in crate_dirs if not any(r.match(d + "/Cargo.toml") for r in res)]
    return sorted(d for d in miss if not any(d.startswith(m + "/") for m in miss))


def unmatched(files, globs):
    """Files no glob matches."""
    res = [glob_re(g) for g in globs]
    return sorted(f for f in files if not any(r.match(f) for r in res))


@functools.lru_cache(maxsize=None)
def tracked_files():
    return subprocess.check_output(["git", "ls-files"], cwd=ROOT, text=True).split("\n")[:-1]


def recipe_files(workflow):
    return sorted({os.path.relpath(f, ROOT) for g in RECIPES[workflow]
                   for f in glob.glob(os.path.join(ROOT, g))})


def lane_files():
    """Everything packaging-lint.yml lints, reads or re-runs on."""
    shell = subprocess.check_output(["sh", "scripts/ci/lint-shell.sh", "--list"], cwd=ROOT,
                                    text=True).split()
    policy = subprocess.check_output([sys.executable, "scripts/ci/check-packaging-policy.py",
                                      "--inputs"], cwd=ROOT, text=True).split()
    tracked = tracked_files()
    return {LINT_LANE, *shell, *policy,
            *(f for f in tracked if f.endswith((".spec", "PKGBUILD"))
              or (f.startswith(".gitea/workflows/") and f.endswith(".yml"))),
            *(f for w in RECIPES for f in recipe_files(w))}


def named_files(text, tracked):
    """Tracked files and directories `text` names outside a comment.

    A crate or client directory is the cargo graph's. `$R/` and `%{buildroot}/` prefixes are
    stepped over; `/usr/…` is not a repo path.
    """
    def alt(names):
        return "|".join(map(re.escape, sorted(names)))

    top_dirs = {t.split("/")[0] for t in tracked if "/" in t} - {"crates", "clients"}
    token = re.compile(r"(?:(?<![\w/.$}%%-])|\$\{?\w+\}?/|%%\{\w+\}/)"
                       r"((?:%s)/[\w./@+-]*[\w/]|%s)(?![\w/-]|\.\w)"
                       % (alt(top_dirs), alt(t for t in tracked if "/" not in t)))
    known = set(tracked)
    dirs = {t.rsplit("/", 1)[0] for t in tracked if "/" in t}
    dirs |= {d.rsplit("/", 1)[0] for d in dirs if "/" in d}
    out = set()
    for line in text.splitlines():
        for m in token.finditer(re.sub(r"(^|\s)#.*$", "", line)):
            name = m.group(1).rstrip("/")
            if name in known or name in dirs:
                out.add(name)
    return out


def input_misses(names, globs, tracked):
    """Names no glob reaches. A directory counts when any file under it matches."""
    res = [glob_re(g) for g in globs]
    known = set(tracked)

    def hit(path):
        return any(r.match(path) for r in res)

    return sorted(n for n in names
                  if not (hit(n) if n in known
                          else any(hit(t) for t in tracked if t.startswith(n + "/"))))


@functools.lru_cache(maxsize=None)
def metadata(manifest, triple, features):
    cmd = ["cargo", "metadata", "--format-version", "1", "--locked",
           "--manifest-path", os.path.join(ROOT, manifest), "--filter-platform", triple]
    if features:
        cmd += ["--features", features]
    return json.loads(subprocess.check_output(cmd, cwd=ROOT, text=True))


def local_crate_dirs(meta, roots):
    if roots is None:
        members = set(meta["workspace_members"])
        roots = [p["name"] for p in meta["packages"] if p["id"] in members]
    ids = closure(meta, roots)
    return {
        os.path.relpath(os.path.dirname(p["manifest_path"]), ROOT).replace(os.sep, "/")
        for p in meta["packages"]
        if p["id"] in ids and p["source"] is None
    }


def build_errors():
    errors = []
    for workflow, manifest, triple, roots, features in BUILDS:
        path = f".gitea/workflows/{workflow}.yml"
        with open(os.path.join(ROOT, path), encoding="utf-8") as f:
            legs = path_legs(f.read())
        dirs = local_crate_dirs(metadata(manifest, triple, features), roots)
        for leg in LEGS:
            for d in uncovered(dirs, legs.get(leg, ["**"])):
                errors.append(f"::error file={path}::{leg} paths miss '{d}/**' "
                              f"(compiled for {triple})")
    return errors


def lane_errors():
    with open(os.path.join(ROOT, LINT_LANE), encoding="utf-8") as f:
        globs = path_legs(f.read()).get("pull_request", ["**"])
    return [f"::error file={LINT_LANE}::pull_request paths miss '{m}'"
            for m in unmatched(lane_files(), globs)]


def recipe_errors():
    errors, tracked = [], tracked_files()
    for workflow in RECIPES:
        path = f".gitea/workflows/{workflow}.yml"
        with open(os.path.join(ROOT, path), encoding="utf-8") as f:
            legs = path_legs(f.read())
        where = {}
        for recipe in recipe_files(workflow):
            where.setdefault(recipe, recipe)
            with open(os.path.join(ROOT, recipe), encoding="utf-8") as f:
                for name in named_files(f.read(), tracked):
                    where.setdefault(name, recipe)
        for leg in LEGS:
            for name in input_misses(where, legs.get(leg, ["**"]), tracked):
                errors.append(f"::error file={path}::{leg} paths miss '{name}' "
                              f"(read by {where[name]})")
    return errors


def leg_errors():
    errors = []
    for path in sorted(glob.glob(os.path.join(ROOT, ".gitea/workflows/*.yml"))):
        with open(path, encoding="utf-8") as f:
            legs = path_legs(f.read())
        if all(leg in legs for leg in LEGS) and legs["push"] != legs["pull_request"]:
            diff = sorted(set(legs["push"]) ^ set(legs["pull_request"]))
            errors.append(f"::error file={os.path.relpath(path, ROOT)}::push and pull_request "
                          f"paths differ at '{diff[0]}' and {len(diff) - 1} more")
    return errors


def check(lanes_only=False):
    errors = lane_errors() + recipe_errors() + leg_errors()
    if not lanes_only:
        errors = build_errors() + errors
    for e in errors:
        print(e)
    if errors:
        print(f"{len(errors)} missing path filter(s): add each line to that leg.", file=sys.stderr)
        return 1
    print("check-workflow-paths: packaging lane and recipes covered" if lanes_only
          else f"check-workflow-paths: {len(BUILDS)} builds, the recipes and the packaging lane "
               "covered")
    return 0


def self_test():
    wf = """name: t
on:
  push:
    branches: [main]
    paths:
      # a comment
      - 'crates/a/**'
      - crates/b/**  # trailing
      - "clients/*/Cargo.toml"
    tags: ['v*']
  pull_request:
    paths:
      - 'crates/a/**'
  workflow_dispatch:
jobs:
  x:
    paths:
      - 'not/a/trigger/**'
"""
    fails = 0

    def expect(name, got, want):
        nonlocal fails
        if got != want:
            print(f"self-test {name}: got {got!r}, want {want!r}", file=sys.stderr)
            fails += 1

    legs = path_legs(wf)
    expect("legs", legs, {"push": ["crates/a/**", "crates/b/**", "clients/*/Cargo.toml"],
                          "pull_request": ["crates/a/**"]})
    dirs = {"crates/a", "crates/a/vendor/x", "crates/b", "clients/cli"}
    expect("push covers", uncovered(dirs, legs["push"]), [])
    expect("pr misses", uncovered(dirs, legs["pull_request"]), ["clients/cli", "crates/b"])
    expect("star stays in one dir", uncovered({"clients/cli/sub"}, ["clients/*/Cargo.toml"]),
           ["clients/cli/sub"])
    expect("no paths leg runs always", uncovered(dirs, ["**"]), [])
    expect("prefix is not a match", uncovered({"crates/ab"}, ["crates/a/**"]), ["crates/ab"])
    expect("one line per tree", uncovered(dirs, []), ["clients/cli", "crates/a", "crates/b"])
    files = ["packaging/arch/PKGBUILD", "scripts/ci/x.sh", "scripts/pf-dm-helper"]
    expect("lane misses a script", unmatched(files, ["packaging/**", "scripts/*.sh"]),
           ["scripts/ci/x.sh", "scripts/pf-dm-helper"])
    expect("lane covers a tree", unmatched(files, ["packaging/**", "scripts/**"]), [])
    files = ["scripts/60-x.rules", "scripts/headless/run.sh", "scripts/headless/env.sh",
             "packaging/linux/a.desktop", "packaging/linux/b.service", "web/x.ts",
             "crates/host/h/src/lib.rs", "Cargo.lock", "README.md", "flake.nix"]
    recipe = """\
# install scripts/skipped-in-comment
install -Dm0644 "$R/scripts/60-x.rules" "$pkgdir/usr/lib/udev/rules.d/60-x.rules"
install -Dm0644 packaging/linux/a.desktop %{buildroot}%{_datadir}/a.desktop # packaging/linux/b.service
cp -r scripts/headless/ "$STAGE/usr/share/"
cp /usr/share/packaging/linux/b.service crates/host/h/src/lib.rs Cargo.lock.bak README.md.
install -Dm0644 %{buildroot}/packaging/linux/b.service the web console
"""
    expect("recipe names", sorted(named_files(recipe, files)),
           ["README.md", "packaging/linux/a.desktop", "packaging/linux/b.service",
            "scripts/60-x.rules", "scripts/headless"])
    expect("a glob per file", input_misses({"scripts/60-x.rules", "web/x.ts"}, ["scripts/*.rules"],
                                            files), ["web/x.ts"])
    expect("a directory needs one match", input_misses({"scripts/headless"},
                                                       ["scripts/headless/env.sh"], files), [])
    expect("a directory with none", input_misses({"scripts/headless"}, ["scripts/*.rules"], files),
           ["scripts/headless"])
    if fails:
        return 1
    print("check-workflow-paths self-test ok")
    return 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if argv[1:] == ["--lanes"]:
        return check(lanes_only=True)
    if argv[1:]:
        print("usage: check-workflow-paths.py [--self-test | --lanes]", file=sys.stderr)
        return 2
    return check()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
