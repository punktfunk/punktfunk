#!/usr/bin/env python3
"""A crate under crates/<group>/ depends only on the groups its ALLOWED row names.

core and codec serve both sides; client and host never depend on each other; install sits on core
and host. The apps under clients/ follow the client row. Dev-dependencies are exempt (a host test
drives the client C ABI), and so is tools/.

Usage: python3 scripts/ci/check-crate-groups.py [--self-test]
"""
import json
import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

ALLOWED = {
    "core": {"core"},
    "codec": {"core", "codec"},
    "client": {"core", "codec", "client"},
    "host": {"core", "codec", "host"},
    "install": {"core", "host", "install"},
    "clients": {"core", "codec", "client"},
}


def group(manifest):
    parts = os.path.relpath(manifest, ROOT).split(os.sep)
    if parts[0] == "crates" and len(parts) > 2:
        return parts[1]
    return "clients" if parts[0] == "clients" else None


def violations(packages):
    out = []
    for p in packages:
        g = group(p["manifest_path"])
        if g not in ALLOWED:
            continue
        for d in p["dependencies"]:
            if d.get("kind") == "dev" or "path" not in d:
                continue
            dg = group(os.path.join(d["path"], "Cargo.toml"))
            if dg is not None and dg not in ALLOWED[g]:
                out.append(f"{p['name']} ({g}) depends on {d['name']} ({dg})")
    return out


def self_test():
    def pkg(name, path, deps):
        return {"name": name, "manifest_path": f"{ROOT}/{path}/Cargo.toml", "dependencies": deps}

    def dep(name, path, kind=None):
        return {"name": name, "path": f"{ROOT}/{path}", "kind": kind}

    got = violations([
        pkg("c", "crates/client/c", [dep("h", "crates/host/h")]),
        pkg("h", "crates/host/h", [dep("f", "crates/client/f", "dev"), dep("k", "crates/core/k")]),
        pkg("t", "tools/t", [dep("h", "crates/host/h")]),
    ])
    assert got == ["c (client) depends on h (host)"], got
    print("check-crate-groups self-test ok")


def main(argv):
    if "--self-test" in argv:
        return self_test()
    meta = json.loads(subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT, capture_output=True, text=True, check=True).stdout)
    bad = violations(meta["packages"])
    for line in bad:
        print(f"::error::{line}")
    if bad:
        sys.exit(1)
    print(f"check-crate-groups: {len(meta['packages'])} packages within their groups")


if __name__ == "__main__":
    main(sys.argv[1:])
