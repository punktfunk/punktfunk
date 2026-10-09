#!/usr/bin/env bash
# Derive every client's UI icons from the Lucide masters in assets/lucide.
#
# The third sibling of gen-os-icons.sh and gen-launcher-icons.sh, and the same discipline —
# one master per icon, original viewBox, provenance in the README — but the vocabulary is
# different again: these are UI marks (back, refresh, microphone, gamepad), not brand marks,
# so they are keyed by Lucide's own icon names and every client draws them in a theme colour
# rather than a fixed brand one.
#
# NOTHING IS RASTERIZED. Each shell draws the marks as vectors, in the form it can:
#
#   Skia console   folds the path with Skia    -> crates/client/pf-client-core/src/lucide.rs
#   GTK shell      folds the path with gsk     -> the same table
#   WinUI shell    draws Lucide's icon FONT    -> the same table's codepoints
#                                              +  clients/windows/packaging/assets/lucide.ttf
#
# The WinUI shell is the odd one only because windows-reactor has no vector element: its
# `Image` takes a raster URI and it builds every `BitmapIcon` with `ShowAsMonochrome(false)`,
# so a bitmap there can be neither sized by the control nor tinted by the theme. A font glyph
# can be both — WinUI sizes and tints a `FontIcon` exactly as it did the `SymbolIcon` this
# replaced. An earlier version of this script baked PNGs instead; they came out the wrong size
# on every button and stuck at one grey on every theme.
#
# Idempotent. Usage: bash scripts/gen-lucide-assets.sh
set -euo pipefail

cd "$(dirname "$0")/.."

MASTERS=assets/lucide
TABLE=crates/client/pf-client-core/src/lucide.rs
# The MSIX layout's Assets\ — pack-msix.ps1 copies this whole directory in, and the installer
# and the portable zip are packed from that same layout, so one copy reaches all three. The
# dev-build copy is the Windows client's build.rs, which stages it next to the exe.
WIN_FONT=clients/windows/packaging/assets/lucide.ttf

log() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }

log "Shared table: path data + font codepoints"
python3 scripts/gen-lucide-icons.py "$MASTERS" "$TABLE"

log "Icon font for the WinUI shell"
cp "$MASTERS/font/lucide.ttf" "$WIN_FONT"
log "  $WIN_FONT ($(du -h "$WIN_FONT" | cut -f1))"

# The generated table goes through rustfmt: `cargo fmt --all --check` is a CI gate, and a
# GENERATED file that fails it would fail the build every time someone re-ran this script.
if command -v rustfmt >/dev/null 2>&1; then
  rustfmt --edition 2021 "$TABLE"
  log "  rustfmt'd $TABLE"
else
  log "  rustfmt not found — run 'cargo fmt' before committing"
fi

echo
log "A NEW icon needs only its master here: every shell reads the table above, and the font"
log "already carries all ~1500 Lucide glyphs. Add the SVG, re-run this, use the name."
