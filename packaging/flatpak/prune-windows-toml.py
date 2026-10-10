#!/usr/bin/env python3
"""Strip microsoft/windows-rs git dependency entries from a Cargo.toml, in place.

The sibling of prune-windows-lock.py, for the OTHER place the un-vendored windows-rs git
source leaks into the flatpak's offline build: `crates/client/pf-client-core` declares
`windows = { git = ... }` under `[target.'cfg(windows)'.dependencies]` (the D3D11VA decode
backend). The dependency is cfg-gated and never COMPILES on Linux, but `cargo --offline`
still needs every declared dependency's source available just to build the unit graph — and
windows-rs is deliberately not vendored into cargo-sources.json (flatpak-builder would
full-clone the multi-GB repo; see prune-windows-lock.py). Removing the entry from the
sandbox copy of the manifest is safe for a Linux-only build: nothing behind cfg(windows)
is compiled, so nothing misses the crate.

Removes any top-level `name = { ... git = "...github.com/microsoft/windows-rs..." ... }`
entry, single- or multi-line (the entry ends at the first line that closes back to depth 0),
and every `"dep:name"` a `[features]` list makes of it — cargo refuses a feature that names a
dependency the manifest no longer declares. The pin lives in the root
`[workspace.dependencies]`, so a member's `name = { workspace = true, ... }` naming one of
those entries goes too, and so do the root entries themselves. Registry deps in the same
tables (wasapi, sdl3, windows-sys) are kept — they vendor normally.

Dependency-free (no tomlkit) so it also runs inside the flatpak build sandbox and on the
Steam Deck's stock python.

Usage: prune-windows-toml.py <Cargo.toml> [<Cargo.toml> ...]
"""

import os
import re
import sys
from typing import Optional

WINDOWS_RS = "github.com/microsoft/windows-rs"
WORKSPACE_TRUE = re.compile(r"\bworkspace\s*=\s*true\b")


def prune(text: str, inherited: frozenset[str] = frozenset()) -> tuple[str, int]:
    lines = text.splitlines(keepends=True)
    kept: list[str] = []
    removed = 0
    names: list[str] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        stripped = line.strip()
        # A dependency entry opening an inline table that pins the windows-rs git repo.
        # Single-line entries contain the URL and balance their braces on the same line;
        # multi-line entries (features arrays) run until the braces balance again.
        if "=" in stripped and "{" in stripped and not stripped.startswith("#"):
            depth = stripped.count("{") - stripped.count("}")
            entry = [line]
            j = i + 1
            while depth > 0 and j < len(lines):
                s = lines[j]
                depth += s.count("{") - s.count("}")
                entry.append(s)
                j += 1
            name = stripped.split("=", 1)[0].strip()
            joined = "".join(entry)
            if WINDOWS_RS in joined or (name in inherited and WORKSPACE_TRUE.search(joined)):
                removed += 1
                names.append(name)
                i = j
                continue
        kept.append(line)
        i += 1
    out = "".join(kept)
    for name in names:
        # A trailing comma before `]` is valid TOML, so the element goes and the comma stays.
        out = re.sub(r'\s*"dep:' + re.escape(name) + r'"\s*,?', "", out)
    return out, removed


def workspace_root(member: str) -> Optional[str]:
    """The nearest Cargo.toml above the member's directory that declares `[workspace]`."""
    here = os.path.dirname(os.path.dirname(os.path.abspath(member)))
    while True:
        candidate = os.path.join(here, "Cargo.toml")
        if os.path.isfile(candidate) and re.search(r"^\[workspace\]", open(candidate).read(), re.M):
            return candidate
        parent = os.path.dirname(here)
        if parent == here:
            return None
        here = parent


def windows_rs_entries(text: str) -> frozenset[str]:
    """Names in `[workspace.dependencies]` that pin the windows-rs git repo."""
    table = re.search(r"^\[workspace\.dependencies\]\n(.*?)(?=^\[|\Z)", text, re.M | re.S)
    if not table:
        return frozenset()
    pin = r"^([A-Za-z0-9_-]+)\s*=\s*\{[^\n]*" + re.escape(WINDOWS_RS)
    return frozenset(m.group(1) for m in re.finditer(pin, table.group(1), re.M))


def main() -> None:
    roots: set[str] = set()
    for path in sys.argv[1:]:
        root = workspace_root(path)
        inherited = windows_rs_entries(open(root).read()) if root else frozenset()
        if inherited:
            roots.add(root)
        out, removed = prune(open(path).read(), inherited)
        if removed == 0:
            sys.exit(f"{path}: no windows-rs git entry found — already pruned, or the "
                     "dependency moved (update this script's callers)")
        open(path, "w").write(out)
        print(f"{path}: removed {removed} windows-rs git dependenc{'y' if removed == 1 else 'ies'}")
    for root in roots:
        out, removed = prune(open(root).read())
        open(root, "w").write(out)
        print(f"{root}: removed {removed} windows-rs workspace entr{'y' if removed == 1 else 'ies'}")


if __name__ == "__main__":
    if sys.argv[1:] == ["--self-test"]:
        sample = (
            "[target.'cfg(windows)'.dependencies]\n"
            "winreg = \"0.55\"\n"
            "windows = { git = \"https://github.com/microsoft/windows-rs\", rev = \"abc\", features = [\n"
            "    \"Win32_Foundation\",\n"
            "] }\n\n"
            "[features]\n"
            "desktop = [\n"
            "    \"dep:winreg\", \"dep:windows\",\n"
            "]\n"
        )
        out, n = prune(sample)
        assert n == 1, n
        assert "windows-rs" not in out and "dep:windows" not in out, out
        assert "dep:winreg" in out and "winreg = " in out, out
        # The workspace shape: the member inherits the pin, the root declares it.
        root = (
            "[workspace]\nmembers = []\n\n[workspace.dependencies]\n"
            "windows = { git = \"https://github.com/microsoft/windows-rs\", rev = \"abc\" }\n"
            "windows-sys = \"0.61\"\n"
        )
        inherited = windows_rs_entries(root)
        assert inherited == frozenset({"windows"}), inherited
        member = (
            "[target.'cfg(windows)'.dependencies]\n"
            "windows = { workspace = true, optional = true, features = [\n"
            "    \"Win32_Foundation\",\n"
            "] }\n"
            "windows-sys = { workspace = true }\n\n"
            "[features]\ndesktop = [\"dep:windows\", \"dep:windows-sys\"]\n"
        )
        out, n = prune(member, inherited)
        assert n == 1 and "Win32_Foundation" not in out and "dep:windows\"" not in out, out
        assert "windows-sys = { workspace = true }" in out and "dep:windows-sys" in out, out
        out, n = prune(root)
        assert n == 1 and "windows-rs" not in out and "windows-sys" in out, out
        print("prune-windows-toml: self-test ok")
    else:
        main()
