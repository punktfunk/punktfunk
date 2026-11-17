#!/usr/bin/env bash
# shellcheck shell=bash
# Does this box already have every Flathub dep a flatpak manifest declares?
#   bash scripts/ci/flatpak-deps-present.sh <manifest.yml>   -> exit 0 = yes, 1 = no
#   bash scripts/ci/flatpak-deps-present.sh --self-test      -> run the asserts below
#
# WHY THIS EXISTS: `flatpak-builder --install-deps-only` does NOT mean "install what is missing".
# builder_manifest_install_dep() (flatpak-builder 1.4.12) branches on `flatpak info --show-commit
# <ref>` succeeding and runs `flatpak update` for every dep that IS installed (a failed update is
# fatal there — it never falls back to install) — and ci/flatpak-ci.Dockerfile bakes the whole
# runtime set, so that flag would do nothing except make the build depend on Flathub being up at
# that minute. Nothing about the build wants a newer commit: the manifest pins a runtime VERSION,
# not a commit, and the baked one satisfies it.
#
# So the workflow asks this first and only reaches for Flathub on a real miss.
#
# FAILS OPEN, deliberately: an unreadable/unexpected manifest reports "not present" (1), so the
# caller does the full install. Silently skipping the install on a manifest we stopped
# understanding is how you build against the wrong runtime.
set -uo pipefail

deps_present() {
  local manifest="$1" runtime rt_ver sdk exts e

  runtime=$(sed -n 's/^runtime: *//p'         "$manifest" | head -1)
  rt_ver=$(sed -n  's/^runtime-version: *//p' "$manifest" | tr -d "\"'" | head -1)
  sdk=$(sed -n     's/^sdk: *//p'             "$manifest" | head -1)
  exts=$(sed -n '/^sdk-extensions:/,/^[^ #-]/p' "$manifest" | sed -n 's/^ *- *//p')

  [ -n "$runtime" ] && [ -n "$rt_ver" ] && [ -n "$sdk" ] && [ -n "$exts" ] || return 1

  flatpak info --user "$runtime//$rt_ver" >/dev/null 2>&1 || return 1
  flatpak info --user "$sdk//$rt_ver"     >/dev/null 2>&1 || return 1
  # Extensions are checked for PRESENCE, not version: flatpak-builder resolves their version from
  # the SDK's own metadata (it prints "Dependency Extension: … 25.08"), never from the manifest.
  # Any bump that moves them moves runtime-version too, which the two checks above already catch.
  for e in $exts; do
    flatpak info --user "$e" >/dev/null 2>&1 || return 1
  done
}

self_test() {
  local rc fails=0 full
  # NOT `local`: the EXIT trap fires after this function has returned.
  SELFTEST_TMP=$(mktemp -d) || return 1
  trap 'rm -rf "$SELFTEST_TMP"' EXIT
  local tmp="$SELFTEST_TMP"

  cat > "$tmp/ok.yml" <<'YML'
runtime: org.gnome.Platform
runtime-version: '50'
sdk: org.gnome.Sdk
sdk-extensions:
  - org.freedesktop.Sdk.Extension.rust-stable
  - org.freedesktop.Sdk.Extension.llvm20
command: punktfunk-client
YML
  # A manifest this script cannot read (the fail-open case).
  printf 'app-id: io.unom.punktfunk\n' > "$tmp/unparseable.yml"

  # Stub `flatpak`: $INSTALLED is the newline-separated set of refs it admits to having.
  mkdir -p "$tmp/bin"
  cat > "$tmp/bin/flatpak" <<'STUB'
#!/usr/bin/env bash
# only `flatpak info --user <ref>` is exercised here
# args are: info --user <ref>
[ "$1" = info ] || exit 0
printf '%s\n' "$INSTALLED" | grep -qxF "$3"
STUB
  chmod +x "$tmp/bin/flatpak"
  PATH="$tmp/bin:$PATH"

  check() { # <expected rc> <label> <installed set> <manifest>
    INSTALLED="$3" deps_present "$4"; rc=$?
    if [ "$rc" != "$1" ]; then
      echo "FAIL: $2 (expected rc=$1, got $rc)" >&2; fails=$((fails + 1))
    else
      echo "ok: $2"
    fi
  }

  full='org.gnome.Platform//50
org.gnome.Sdk//50
org.freedesktop.Sdk.Extension.rust-stable
org.freedesktop.Sdk.Extension.llvm20'

  check 0 "everything baked -> skip Flathub"     "$full"                              "$tmp/ok.yml"
  check 1 "cold box -> install"                  ""                                   "$tmp/ok.yml"
  check 1 "runtime missing -> install"           "${full/org.gnome.Platform\/\/50/x}" "$tmp/ok.yml"
  check 1 "sdk missing -> install"               "${full/org.gnome.Sdk\/\/50/x}"      "$tmp/ok.yml"
  # One extension present, one missing: llvm20 fine, rust-stable not.
  check 1 "one sdk-extension missing -> install" "${full/*.rust-stable/x}"            "$tmp/ok.yml"
  # A runtime installed at ANOTHER version must not pass just because the name matches.
  check 1 "runtime at the wrong version"         'org.gnome.Platform//51
org.gnome.Sdk//51
org.freedesktop.Sdk.Extension.rust-stable
org.freedesktop.Sdk.Extension.llvm20'                                                "$tmp/ok.yml"
  check 1 "unreadable manifest -> fail open"     "$full"                       "$tmp/unparseable.yml"

  [ "$fails" = 0 ] || { echo "$fails check(s) failed" >&2; return 1; }
  echo "all checks passed"
}

case "${1:---help}" in
  --self-test) self_test ;;
  --help|-h)   sed -n '2,4p' "$0"; exit 2 ;;
  *)           deps_present "$1" ;;
esac
