#!/usr/bin/env bash
# Build the punktfunk-web .deb — the management web console (Nitro SSR on bun + React).
#
# Runtime is BUN: the console is built with Nitro's `bun` preset + a custom Bun.serve entry that
# serves HTTPS (HTTP/1.1 over TLS) with the host's identity cert (web/nitro-entry/bun-https.mjs). It
# runs on the vendored bun from punktfunk-bun (build-bun-deb.sh), pinned to this exact version. The
# host's punktfunk-host .deb Recommends this, so a default `apt install punktfunk-host` pulls the
# console too; it is auto-wired to the host's mgmt token + identity cert via the systemd --user
# units (no env editing on a packaged install).
#
# Usage: VERSION=0.0.1~ci42.gdeadbee [DEB_ARCH=amd64] bash packaging/debian/build-web-deb.sh
# Output: dist/punktfunk-web_<version>_<arch>.deb
set -euo pipefail

VERSION="${VERSION:?set VERSION (e.g. 0.0.1 or 0.0.1~ci42.gdeadbee)}"
PKG="punktfunk-web"
ROOTDIR="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOTDIR"

# Per-arch, like the punktfunk-bun it depends on.
DEB_ARCH="${DEB_ARCH:-$(dpkg --print-architecture)}"

# Build the console if not already built (.output is gitignored — CI builds it each run).
if [ ! -f web/.output/server/index.mjs ]; then
  echo "==> building web console"
  (cd web && bun install --frozen-lockfile --ignore-scripts && bun run build)
fi
# The build MUST be the bun preset (our Bun.serve TLS entry) — node can't run Bun.serve.
if ! grep -rq 'Bun\.serve' web/.output/server/index.mjs 2>/dev/null; then
  echo "ERROR: web/.output has no Bun.serve — wrong nitro preset (need 'bun' + the custom entry)" >&2
  exit 1
fi

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
SHAREDIR="$STAGE/usr/share/$PKG"
DOCDIR="$STAGE/usr/share/doc/$PKG"

# --- file layout -------------------------------------------------------------
mkdir -p "$SHAREDIR/.output"
cp -r web/.output/server "$SHAREDIR/.output/server"
cp -r web/.output/public "$SHAREDIR/.output/public"
# Stable PATH-independent ExecStart wrapper.
install -d "$STAGE/usr/bin"
cat > "$STAGE/usr/bin/punktfunk-web-server" <<'WRAP'
#!/bin/sh
# The console runs on punktfunk-bun's private bun (Bun.serve TLS), never one from PATH.
exec /usr/lib/punktfunk-bun/bun /usr/share/punktfunk-web/.output/server/index.mjs "$@"
WRAP
chmod 0755 "$STAGE/usr/bin/punktfunk-web-server"
install -Dm0644 scripts/punktfunk-web.service      "$STAGE/usr/lib/systemd/user/punktfunk-web.service"
install -Dm0644 scripts/punktfunk-web-init.service "$STAGE/usr/lib/systemd/user/punktfunk-web-init.service"
install -Dm0755 scripts/web-init.sh                "$SHAREDIR/web-init.sh"
install -Dm0644 web/web.env.example                "$SHAREDIR/web.env.example"
install -Dm0644 LICENSE-MIT                         "$DOCDIR/LICENSE-MIT"
install -Dm0644 LICENSE-APACHE                      "$DOCDIR/LICENSE-APACHE"
install -Dm0644 web/README.md                       "$DOCDIR/README.md"

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
Depends: punktfunk-bun (= $VERSION)
Homepage: https://git.unom.io/unom/punktfunk
Description: punktfunk management web console (Nitro SSR on bun + React)
 The browser console for a punktfunk streaming host: status, paired devices, and the
 SPAKE2 PIN pairing flow every client needs. Runs as a systemd --user service on port
 47992 over HTTPS (HTTP/1.1 over TLS, with the host's own identity cert), login-gated (a
 password generated on first start), proxying the host's loopback HTTPS management API
 with a bearer token injected server-side (never sent to the browser). Runs on the bun
 from punktfunk-bun (no system nodejs/bun dependency).
 .
 Auto-wired to the host on a packaged install: it sources the host's
 ~/.config/punktfunk/mgmt-token and a generated login password — no env editing. Enable
 the systemd user service punktfunk-web; read the login password out of web-password once,
 before the console replaces it with a salted argon2id hash on your first sign-in.
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    echo "punktfunk-web installed. Enable it for your user:"
    echo "    systemctl --user enable --now punktfunk-web"
    echo "A login password is generated on first start. Read it once, before you sign in:"
    echo "    sed -n 's/^PUNKTFUNK_UI_PASSWORD=//p' ~/.config/punktfunk/web-password"
    echo "After that the console keeps only a salted hash, so a forgotten password is reset:"
    echo "put a PUNKTFUNK_UI_PASSWORD=<your-password> line in that file, then"
    echo "    systemctl --user restart punktfunk-web"
    echo "Then open https://<host-ip>:47992 (self-signed host cert — trust it once)"
fi
exit 0
EOF
chmod 0755 "$STAGE/DEBIAN/postinst"

mkdir -p dist
OUT="dist/${PKG}_${VERSION}_${DEB_ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "built $OUT"
dpkg-deb -I "$OUT" | sed -n 's/^/  /p' | grep -E 'Version|Installed-Size|Depends' || true
