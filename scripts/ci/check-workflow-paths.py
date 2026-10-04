#!/usr/bin/env python3
"""Every crate a workflow compiles is in that workflow's `paths:` filter.

For each row of BUILDS, `cargo metadata --filter-platform <triple>` gives the target's
dependency graph; every local path crate reachable from the row's packages must match a glob
in each `paths:` leg (push, pull_request) the workflow has. A leg with no `paths:` runs on
every change and needs nothing. A missing line means a change to that crate merges without
the build that compiles it.

Usage: python3 scripts/ci/check-workflow-paths.py [--self-test]
"""
import functools
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
    ("windows-host", "crates/pf-seat-keeper/Cargo.toml", "x86_64-pc-windows-msvc", None, None),
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


def check():
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
    for e in errors:
        print(e)
    if errors:
        print(f"{len(errors)} missing path filter(s): add each line to that leg.", file=sys.stderr)
        return 1
    print(f"check-workflow-paths: {len(BUILDS)} builds covered")
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
    if fails:
        return 1
    print("check-workflow-paths self-test ok")
    return 0


def main(argv):
    if argv[1:] == ["--self-test"]:
        return self_test()
    if argv[1:]:
        print("usage: check-workflow-paths.py [--self-test]", file=sys.stderr)
        return 2
    return check()


if __name__ == "__main__":
    sys.exit(main(sys.argv))
