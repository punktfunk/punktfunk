#!/usr/bin/env bash
# shellcheck shell=bash
# Single source of truth for punktfunk release/canary version numbers (Linux + macOS runners).
# The base version comes from the git tags, so no workflow carries a number to hand-bump.
#
# THE RULE:
#   * stable  (a `vX.Y.Z` tag push) → PF_BASE = X.Y.Z (the tag, minus any -rc/+meta suffix).
#   * canary  (a main push)         → PF_BASE = <latest stable tag with minor+1, patch 0>.
#     i.e. latest release v0.6.0 → canary base 0.7.0. Canary is always one minor ahead of the
#     newest stable release, so a stable→canary re-point still moves forward.
#
# USAGE (bash workflows):
#     eval "$(bash scripts/ci/pf-version.sh)"                  # base keys only
#     eval "$(bash scripts/ci/pf-version.sh --format deb)"     # + PF_VERSION (+ PF_RELEASE)
#     bash scripts/ci/pf-version.sh --self-test                # check pf-version.vectors
#
# It prints `KEY=VALUE` lines (eval-able) to stdout and — when $GITHUB_ENV is set — also
# appends them there for later steps. Exports:
#     PF_CHANNEL     stable | canary (also the apt distribution and the flatpak branch)
#     PF_BASE        the base semver X.Y.Z (see THE RULE)
#     PF_MAJOR/MINOR/PATCH   the components of PF_BASE (numeric-version channels build
#                            `<MAJOR>.<MINOR>.<run>` from these — MSIX/decky need monotonic ints)
#     PF_STABLE_TAG  the latest stable release version the canary base was derived from (for logs)
#     PF_VERSION     with --format: the package version (deb, rpm, arch, flatpak)
#     PF_RELEASE     with --format rpm|arch: the package release
#     CARGO_PROFILE_RELEASE_CODEGEN_UNITS   16 on canary, 1 on a tag
#
# The pwsh twin scripts/ci/pf-version.ps1 implements the base rule for the Windows runners;
# both check the same scripts/ci/pf-version.vectors.
set -euo pipefail

_root="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/../.." && pwd)"

_format=""
case "${1:-}" in
  '') ;;
  --format)
    _format="${2:-}"
    case "$_format" in
      deb|rpm|arch|flatpak) ;;
      *) echo "pf-version.sh: --format takes deb, rpm, arch or flatpak" >&2; exit 2 ;;
    esac ;;
  --self-test)
    # Each row: tags | GITHUB_REF | format or - | expected KEY=VALUE pairs.
    _fail=0
    while IFS='|' read -r _tags _ref _fmt _want; do
      case "$_tags" in '#'*|'') continue ;; esac
      _ref="$(echo $_ref)"; _fmt="$(echo $_fmt)"
      [ "$_fmt" = - ] && _fmt=""
      _got="$(PF_VERSION_TAGS="$(echo $_tags)" GITHUB_REF="$_ref" GITHUB_REF_NAME="${_ref#refs/*/}" \
        GITHUB_RUN_NUMBER=9907 GITHUB_SHA=0123456789abcdef0123456789abcdef01234567 GITHUB_ENV='' \
        bash "${BASH_SOURCE[0]}" ${_fmt:+--format "$_fmt"})"
      for _kv in $_want; do
        printf '%s\n' "$_got" | grep -qxF "$_kv" \
          || { echo "pf-version.sh: $_ref ${_fmt:--}: want $_kv, got $(echo $_got)" >&2; _fail=1; }
      done
    done < "$_root/scripts/ci/pf-version.vectors"
    [ "$_fail" = 0 ] && echo "pf-version.sh: every vector matches"
    exit "$_fail" ;;
  *) echo "usage: pf-version.sh [--format deb|rpm|arch|flatpak | --self-test]" >&2; exit 2 ;;
esac

if [ -n "${PF_VERSION_TAGS+x}" ]; then
  _tags="$PF_VERSION_TAGS"   # --self-test's tag list
else
  # actions/checkout is shallow and fetches NO tags by default — canary needs the full tag list
  # to find the latest stable. Best-effort fetch; the Cargo.toml fallback below covers a fresh
  # repo with no tags at all.
  git -C "$_root" fetch --tags --force --quiet 2>/dev/null || true
  _tags="$(git -C "$_root" tag -l 'v*')"
fi

# Latest stable release = highest strict vX.Y.Z tag (pre-releases like v0.7.0-rc1 are ignored).
_stable="$(
  printf '%s\n' $_tags \
    | sed -n 's/^v\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)$/\1/p' \
    | sort -V | tail -n1
)"
if [ -z "${_stable:-}" ]; then
  # No tags yet — seed from the workspace Cargo.toml version so canary still has a base.
  _stable="$(sed -n 's/^version = "\([0-9][0-9]*\.[0-9][0-9]*\.[0-9][0-9]*\)".*/\1/p' "$_root/Cargo.toml" | head -n1)"
fi
_stable="${_stable:-0.0.0}"

case "${GITHUB_REF:-}" in
  refs/tags/v*)
    _channel="stable"
    _base="${GITHUB_REF_NAME#v}"
    _base="${_base%%-*}"   # drop -rc / pre-release for the numeric marketing/package version
    _base="${_base%%+*}"   # drop +build metadata
    ;;
  *)
    _channel="canary"
    _maj="${_stable%%.*}"; _rest="${_stable#*.}"; _min="${_rest%%.*}"
    _base="${_maj}.$((_min + 1)).0"
    ;;
esac

_pf_major="${_base%%.*}"; _pf_rest="${_base#*.}"; _pf_minor="${_pf_rest%%.*}"; _pf_patch="${_pf_rest##*.}"

_emit() {
  printf '%s=%s\n' "$1" "$2"
  if [ -n "${GITHUB_ENV:-}" ]; then printf '%s=%s\n' "$1" "$2" >> "$GITHUB_ENV"; fi
}
_emit PF_CHANNEL    "$_channel"
_emit PF_BASE       "$_base"
_emit PF_MAJOR      "$_pf_major"
_emit PF_MINOR      "$_pf_minor"
_emit PF_PATCH      "$_pf_patch"
_emit PF_STABLE_TAG "$_stable"
# Canary and pull-request builds compile workspace crates on 16 codegen units; a release tag
# keeps Cargo.toml's 1. Dependencies stay at 1 either way ([profile.release.package."*"]).
case "$_channel" in
  canary) _emit CARGO_PROFILE_RELEASE_CODEGEN_UNITS 16 ;;
  *)      _emit CARGO_PROFILE_RELEASE_CODEGEN_UNITS 1 ;;
esac

# The package version per format. A release keeps its full tag; a canary sorts below the release
# it precedes and climbs by run number.
[ -n "$_format" ] || exit 0
_run="${GITHUB_RUN_NUMBER:-0}"
_short="$(printf '%s' "${GITHUB_SHA:-}" | cut -c1-8)"
if [ "$_channel" = stable ]; then
  _emit PF_VERSION "${GITHUB_REF_NAME#v}"
  case "$_format" in rpm|arch) _emit PF_RELEASE 1 ;; esac
  exit 0
fi
case "$_format" in
  deb)     _emit PF_VERSION "${_base}~ci${_run}.g${_short}" ;;
  flatpak) _emit PF_VERSION "${_base}-ci${_run}.g${_short}" ;;
  rpm)     _emit PF_VERSION "$_base"; _emit PF_RELEASE "0.ci${_run}.g${_short}" ;;
  # pkgrel is digits and dots only. Gitea's Arch registry advertises the version that sorts
  # highest as a STRING, so the run number is zero-padded: unpadded, run 10000 sorts below 9999.
  arch)    _emit PF_VERSION "$_base"; _emit PF_RELEASE "0.$(printf '%08d' "$_run")" ;;
esac
