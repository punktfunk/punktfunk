#!/usr/bin/env bash
# Build the punktfunk-scripting .deb — the plugin/script runner (the SDK's `punktfunk-scripting`,
# built on Effect, run on bun).
#
# Runtime is BUN: the runner `import()`s the operator's `.ts` plugin/script files directly, which
# only bun can do. Like the web console it runs on the vendored bun from punktfunk-bun
# (build-bun-deb.sh), pinned to this exact version. Unlike the console it is NOT a Nitro
# bundle: we `bun build` the runner CLI into ONE self-contained JS (effect + the SDK inlined; the
# dynamic plugin import stays a runtime import), so there is no node_modules to ship. The host's
# punktfunk-host .deb Recommends this so a default `apt install punktfunk-host` pulls the runner too;
# its systemd --user unit is installed but NOT auto-enabled (the runner is inert until you add
# scripts/plugins — enable it with `systemctl --user enable --now punktfunk-scripting`).
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [DEB_ARCH=amd64] bash packaging/debian/build-scripting-deb.sh
# Output: dist/punktfunk-scripting_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
PKG="punktfunk-scripting"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

# Per-arch, like the punktfunk-bun it depends on.
DEB_ARCH="${DEB_ARCH:-$(dpkg --print-architecture)}"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
SHAREDIR="$STAGE/usr/share/$PKG"
DOCDIR="$STAGE/usr/share/doc/$PKG"

# --- build the runner bundle -------------------------------------------------
# One self-contained JS: `bun build --target=bun` inlines effect + the @punktfunk/host SDK; the
# runner's dynamic `import()` of the operator's plugin files is left as a runtime import (bun keeps
# unresolvable dynamic specifiers external). `--ignore-scripts` on install: nothing needs a
# postinstall, and we skip the `prepare` codegen (it wants ../api/openapi.json — not needed here).
mkdir -p "$SHAREDIR"
(
  cd sdk
  bun install --frozen-lockfile --ignore-scripts
  bun build src/runner-cli.ts --target=bun --outfile="$SHAREDIR/runner-cli.js"
)
grep -q 'attempt=' "$SHAREDIR/runner-cli.js" \
  || { echo "ERROR: runner bundle missing the dynamic plugin import — wrong build" >&2; exit 1; }

# --- file layout -------------------------------------------------------------
# Stable PATH-independent launcher (the systemd unit's ExecStart) — runs the bundle on vendored bun.
install -d "$STAGE/usr/bin"
cat > "$STAGE/usr/bin/punktfunk-scripting" <<'WRAP'
#!/bin/sh
# The runner runs on punktfunk-bun's private bun (it import()s the operator's .ts plugins).
exec /usr/lib/punktfunk-bun/bun /usr/share/punktfunk-scripting/runner-cli.js "$@"
WRAP
chmod 0755 "$STAGE/usr/bin/punktfunk-scripting"
install -Dm0644 scripts/punktfunk-scripting.service "$STAGE/usr/lib/systemd/user/punktfunk-scripting.service"
# Where AppArmor restricts unprivileged user namespaces, the generator trades the unit's namespace
# sandbox for this profile.
install -Dm0644 packaging/linux/apparmor/punktfunk-scripting "$STAGE/etc/apparmor.d/punktfunk-scripting"
install -Dm0755 packaging/linux/punktfunk-scripting-apparmor.generator \
  "$STAGE/usr/lib/systemd/user-generators/punktfunk-scripting-apparmor"
install -Dm0644 LICENSE-MIT    "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE "$DOCDIR/LICENSE-APACHE"
install -Dm0644 sdk/README.md  "$DOCDIR/README.md"

cat > "$DOCDIR/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: punktfunk
Source: https://git.unom.io/unom/punktfunk

Files: *
Copyright: punktfunk contributors
License: MIT or Apache-2.0
 Dual-licensed. Full texts in /usr/share/doc/$PKG/LICENSE-MIT and
 /usr/share/doc/$PKG/LICENSE-APACHE.
EOF
printf '%s (%s) stable; urgency=medium\n\n  * Automated build %s.\n\n -- unom <packages@unom.io>  %s\n' \
  "$PKG" "$VERSION" "$VERSION" "$(date -uR 2>/dev/null || echo 'Thu, 01 Jan 1970 00:00:00 +0000')" \
  | gzip -9n > "$DOCDIR/changelog.Debian.gz"

INSTALLED_KB="$(du -k -s "$STAGE" | cut -f1)"

install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $PKG
Version: $VERSION
Architecture: $DEB_ARCH
Maintainer: unom <packages@unom.io>
Installed-Size: $INSTALLED_KB
Section: net
Priority: optional
Depends: bubblewrap, punktfunk-bun (= $VERSION)
Homepage: https://git.unom.io/unom/punktfunk
Description: punktfunk plugin/script runner (Effect SDK on bun)
 Runs a punktfunk host's automation: loose scripts in ~/.config/punktfunk/scripts and installed
 punktfunk-plugin-* packages under ~/.config/punktfunk/plugins, each supervised (Effect fibers with
 capped-jittered restart; SIGTERM shuts the whole tree down structurally so plugin finalizers run).
 Runs on the bun from punktfunk-bun (no system nodejs/bun dependency).
 .
 ON BY DEFAULT: the systemd --user unit is enabled for every user (systemctl --global). The runner is
 inert until you add scripts or plugins, and the game-library scanners now ship AS plugins — so a
 host without the runner has an empty library and no obvious reason why. A plugin auto-wires to the
 host's mgmt token + identity cert on the same box — no env editing.
 Opt out per user with: systemctl --user mask punktfunk-scripting
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    # `--global`, not `--user`: a maintainer script has no user session to act on, and this is the
    # only mechanism that makes a `--user` unit on-by-default for everyone (it symlinks into
    # /etc/systemd/user/…wants/). The library's scanners are plugins now, so the runner is a default
    # component rather than an add-on (design D9) — but installing it stays opt-OUT, and the opt-out
    # is `systemctl --user mask punktfunk-scripting`, since a plain `--user disable` cannot remove a
    # global symlink.
    #
    # Only on FIRST configure ($2 empty): re-running it on every upgrade would silently undo the
    # mask of anyone who turned it off.
    if [ -z "$2" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --global enable punktfunk-scripting.service >/dev/null 2>&1 || true
    fi
    # Boot loads it from /etc/apparmor.d; this covers the running kernel.
    if command -v apparmor_parser >/dev/null 2>&1 && aa-enabled --quiet 2>/dev/null; then
        apparmor_parser -r -T -W /etc/apparmor.d/punktfunk-scripting || true
    fi
    echo "punktfunk-scripting installed and enabled for all users."
    echo "It runs your automation — game-library sources, scripts in"
    echo "    ~/.config/punktfunk/scripts/  (loose .ts/.js files)"
    echo "and plugins under ~/.config/punktfunk/plugins/."
    echo "It starts with your next login; start it now with:"
    echo "    systemctl --user start punktfunk-scripting"
    echo "Don't want it? systemctl --user mask punktfunk-scripting"
fi
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"
echo /etc/apparmor.d/punktfunk-scripting > "$STAGE/DEBIAN/conffiles"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size|Depends' || true
