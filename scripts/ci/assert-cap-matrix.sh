#!/usr/bin/env bash
# Assert the file-capability matrix of a BUILT package, not of the source tree.
#
#   usr/bin/punktfunk-host           MUST carry no capability, ever.   -> hard fail
#   usr/bin/punktfunk-encode-worker  MUST carry exactly cap_sys_nice=ep -> hard fail
#
# WHY THIS EXISTS. 0.26.0-1 shipped `cap_sys_nice=ep` on the host binary through five packaging
# channels at once. KWin identifies a Wayland client by resolving its /proc/<pid>/exe and matching
# it against an installed .desktop's Exec=, and the kernel refuses that readlink to any reader
# whose effective set is not a superset of the target's PERMITTED set (cap_ptrace_access_check).
# KWin holds no capabilities, so a capability-carrying host is unidentifiable, the restricted
# globals are never advertised, and EVERY KDE desktop session dies — presenting as a missing or
# wrong .desktop file. A merged sysext cannot even be repaired on the box (read-only /usr).
#
# Every board in that release was green. The lesson recorded at the time was "verify the PACKAGE,
# never the board"; this script is that, mechanized. It reads what the artifact will actually do on
# a user's machine — the pacman scriptlet, the dpkg postinst, rpm's file-capability metadata, the
# xattrs inside the squashfs — and refuses the release if the matrix is wrong in either direction.
#
# Usage:
#   scripts/ci/assert-cap-matrix.sh <artifact> [<artifact> ...]
#   scripts/ci/assert-cap-matrix.sh --self-test        # red-team the assertions themselves
#
# Artifacts, dispatched by extension:
#   *.pkg.tar.zst  Arch  — the payload listing + the .INSTALL scriptlet (pacman applies caps there,
#                          not from package metadata, so the scriptlet TEXT is the ground truth)
#   *.deb          Debian— the payload listing + DEBIAN/postinst (same reason)
#   *.rpm          RPM   — rpm's own file-capability metadata (%caps), which is what rpm applies,
#                          restores on upgrade and verifies — and what rpm-ostree layers on Bazzite
#   *.raw          sysext— the squashfs xattrs, read back out of the image that will actually ship
#
# A skipped artifact (no host and no worker inside, e.g. a client-only package) is reported and
# ignored. Anything it cannot READ is a failure, never a pass: a blind check is worse than none,
# which is why the sysext path proves its own reader with a capability round-trip first.
set -euo pipefail

HOST_REL='usr/bin/punktfunk-host'
WORKER_REL='usr/bin/punktfunk-encode-worker'
WANT_WORKER_CAPS='cap_sys_nice=ep'

RC=0
err() { printf '::error::%s\n' "$*" >&2; }
note() { printf '%s\n' "$*"; }

# --- the matrix -------------------------------------------------------------------------------
# Pure function of four already-extracted facts, so it can be (and is, below) unit-tested on any
# box with a bash — including one with no setcap, no rpm and no dpkg.
#
#   $1 label          human-readable artifact name, for the message
#   $2 host_caps      canonical capability string on the host binary, "" = none
#   $3 worker_caps    canonical capability string on the worker binary, "" = none
#   $4 worker_present 1 if the artifact ships the worker at all
#   $5 "uncapped-ok"  optional: an uncapped worker only notes (the sysext builders' best-effort
#                     grant). Nothing that gates a published artifact passes it.
assert_matrix() {
  local label="$1" host_caps="$2" worker_caps="$3" worker_present="$4" uncapped_ok="${5:-}" rc=0
  if [ -n "$host_caps" ]; then
    err "$label: $HOST_REL carries '$host_caps' — it must carry NO capability, ever."
    err "$label: a capability makes the host unidentifiable to KWin (it cannot readlink"
    err "$label: /proc/<pid>/exe of a capability-carrying process), so every KDE desktop session"
    err "$label: dies with 'KWin does not expose zkde_screencast_unstable_v1 to this client'."
    err "$label: The GPU-priority capability belongs on $WORKER_REL. This is the 0.26.0-1 incident."
    rc=1
  fi
  if [ "$worker_present" != 1 ]; then
    err "$label: does not ship $WORKER_REL. Host and worker must move lockstep — they"
    err "$label: version-check each other over their socket — and the GPU-priority lever is inert"
    err "$label: without the worker."
    rc=1
  elif [ -z "$worker_caps" ] && [ "$uncapped_ok" = uncapped-ok ]; then
    note "--  $label: $WORKER_REL ships uncapped — PyroWave encodes at default GPU priority"
  elif [ "$worker_caps" != "$WANT_WORKER_CAPS" ]; then
    err "$label: $WORKER_REL carries '${worker_caps:-<none>}', expected exactly '$WANT_WORKER_CAPS'."
    if [ -z "$worker_caps" ]; then
      err "$label: without it every driver refuses every elevated VK_KHR_global_priority class and"
      err "$label: PyroWave encodes at default GPU priority. Granting it needs CAP_SETFCAP at build"
      err "$label: or install time — check the scriptlet/%caps/setcap for this channel."
    fi
    rc=1
  fi
  if [ "$rc" = 0 ] && [ -n "$worker_caps" ]; then
    note "OK  $label: host uncapped, worker $WANT_WORKER_CAPS"
  fi
  return "$rc"
}

# Canonicalize a capability string. getcap has printed two forms over its life
# ("path cap_sys_nice=ep" since libcap ~2.36, "path = cap_sys_nice+ep" before) and rpm renders
# "(none)" for a file with no capability. Everything downstream compares canonical strings.
caps_norm() {
  local s="${1:-}"
  case "$s" in ''|'(none)'|'<none>') printf ''; return 0 ;; esac
  printf '%s' "$s" | sed -e 's/^= *//' -e 's/+/=/g' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//'
}

# --- scriptlet readers (Arch .INSTALL, dpkg postinst) -----------------------------------------
# pacman and dpkg do NOT carry file capabilities in package metadata: the scriptlet applies them.
# So for those two channels the scriptlet text IS the shipped behaviour, and that is what gets
# read. Comments are stripped first — every one of these files carries a long comment block that
# quotes the very commands being searched for.
#
# Limitation, stated rather than hidden: this reads literal `setcap` invocations. A grant smuggled
# through a shell variable or an eval would not be seen. Nothing in this repo does that, and the
# reviewer-facing rule is simply "spell setcap out".
scriptlet_strip_comments() { sed -e 's/#.*$//'; }

# Any capability GRANT naming the host -> echoed (and therefore fatal). `setcap -r <host>` is the
# removal we ship and carries no `cap_` token, so it is correctly invisible here.
scriptlet_host_grant() {
  scriptlet_strip_comments \
    | grep -E 'setcap' \
    | grep -E 'punktfunk-host' \
    | grep -E 'cap_[a-z_]+[=+]' \
    | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
    | head -1 || true
}

# The worker grant, echoed as its canonical capability string when present.
scriptlet_worker_grant() {
  scriptlet_strip_comments \
    | grep -E 'setcap' \
    | grep -E 'punktfunk-encode-worker' \
    | grep -oE 'cap_[a-z_]+[=+][a-z]+' \
    | head -1 || true
}

# --- per-format extractors ---------------------------------------------------------------------

# payload_listing <artifact>: the payload's paths, one per line, relative to / ("usr/bin/…"), for
# every format above. Empty when it cannot be read. assert-payload-parity.sh reads through it too.
# A .deb without dpkg-deb is an `ar` of two tarballs that libarchive reads; Apple's `ar` rewrites
# the archive and loses members, so it is never used.
payload_listing() {
  case "$1" in
    *.pkg.tar.zst|*.pkg.tar.xz) bsdtar -tf "$1" 2>/dev/null || tar -tf "$1" 2>/dev/null ;;
    *.deb)
      if command -v dpkg-deb >/dev/null 2>&1; then
        dpkg-deb --fsys-tarfile "$1" 2>/dev/null | tar -tf - 2>/dev/null
      else
        bsdtar -xOf "$1" 'data.tar*' 2>/dev/null | bsdtar -tf - 2>/dev/null
      fi ;;
    *.rpm) rpm -qp --qf '[%{FILENAMES}\n]' "$1" 2>/dev/null ;;
    *.raw) unsquashfs -no-progress -l "$1" 2>/dev/null ;;
  esac | sed -e 's#^\./##' -e 's#^/##' -e 's#^squashfs-root/##' -e 's#^squashfs-root$##' -e 's#/$##' \
    | grep . || true
}

# An artifact whose payload could not be listed must FAIL, never "skip": a reader that silently
# produces nothing would wave through the exact package this script exists to reject.
require_listing() {
  local label="$1" list="$2"
  if [ -z "$list" ]; then
    err "$label: could not list the payload — refusing to report a PASS from an empty read."
    return 1
  fi
  return 0
}

check_arch_pkg() {
  local pkg="$1" label; label="$(basename "$pkg")"
  local list scriptlet worker_present=0 host_grant worker_grant
  list="$(payload_listing "$pkg")"
  require_listing "$label" "$list" || return 1
  case "$list" in *"$WORKER_REL"*) worker_present=1 ;; esac
  case "$list" in *"$HOST_REL"*) ;; *)
    if [ "$worker_present" = 0 ]; then note "--  $label: no host and no worker inside, skipping"; return 0; fi ;;
  esac
  scriptlet="$(bsdtar -xOf "$pkg" .INSTALL 2>/dev/null || true)"
  if [ -z "$scriptlet" ]; then
    err "$label: no .INSTALL scriptlet in the package — pacman applies capabilities ONLY from the"
    err "$label: scriptlet, so a package without one cannot grant the worker anything."
    return 1
  fi
  host_grant="$(printf '%s\n' "$scriptlet" | scriptlet_host_grant)"
  worker_grant="$(printf '%s\n' "$scriptlet" | scriptlet_worker_grant)"
  assert_matrix "$label" "$(caps_norm "$host_grant")" "$(caps_norm "$worker_grant")" "$worker_present"
}

check_deb() {
  local deb="$1" label; label="$(basename "$deb")"
  local list postinst worker_present=0 host_grant worker_grant
  if command -v dpkg-deb >/dev/null 2>&1; then
    postinst="$(dpkg-deb --info "$deb" postinst 2>/dev/null || true)"
  elif command -v bsdtar >/dev/null 2>&1; then
    postinst="$(bsdtar -xOf "$deb" 'control.tar*' 2>/dev/null | bsdtar -xOf - './postinst' 'postinst' 2>/dev/null || true)"
  else
    err "$label: neither dpkg-deb nor bsdtar available — cannot read this package"
    return 1
  fi
  list="$(payload_listing "$deb")"
  require_listing "$label" "$list" || return 1
  case "$list" in *"$WORKER_REL"*) worker_present=1 ;; esac
  case "$list" in *"$HOST_REL"*) ;; *)
    if [ "$worker_present" = 0 ]; then note "--  $label: no host and no worker inside, skipping"; return 0; fi ;;
  esac
  if [ -z "$postinst" ]; then
    err "$label: no DEBIAN/postinst — dpkg applies capabilities only from the postinst, so this"
    err "$label: package cannot grant the worker anything."
    return 1
  fi
  host_grant="$(printf '%s\n' "$postinst" | scriptlet_host_grant)"
  worker_grant="$(printf '%s\n' "$postinst" | scriptlet_worker_grant)"
  assert_matrix "$label" "$(caps_norm "$host_grant")" "$(caps_norm "$worker_grant")" "$worker_present"
}

check_rpm() {
  local rpm_file="$1" label; label="$(basename "$rpm_file")"
  local caps_table host_caps worker_caps worker_present=0
  command -v rpm >/dev/null 2>&1 || { err "$label: no rpm(8) to read file capabilities with"; return 1; }
  # rpm carries capabilities in its own header (%caps) and applies/restores/verifies them itself —
  # this is the metadata, i.e. exactly what lands on the box (and what rpm-ostree layers).
  caps_table="$(rpm -qp --qf '[%{FILENAMES} %{FILECAPS}\n]' "$rpm_file" 2>/dev/null || true)"
  require_listing "$label" "$caps_table" || return 1
  case "$caps_table" in *"/$WORKER_REL"*|*"$WORKER_REL"*) worker_present=1 ;; esac
  case "$caps_table" in
    *"$HOST_REL"*) ;;
    *) if [ "$worker_present" = 0 ]; then note "--  $label: no host and no worker inside, skipping"; return 0; fi ;;
  esac
  host_caps="$(printf '%s\n' "$caps_table"   | awk -v p="/$HOST_REL"   '$1 == p { $1=""; sub(/^ /,""); print; exit }')"
  worker_caps="$(printf '%s\n' "$caps_table" | awk -v p="/$WORKER_REL" '$1 == p { $1=""; sub(/^ /,""); print; exit }')"
  assert_matrix "$label" "$(caps_norm "$host_caps")" "$(caps_norm "$worker_caps")" "$worker_present"
}

# Prove the reader is not blind BEFORE trusting an empty read from a squashfs. A check that cannot
# see a capability would pass the exact image it exists to reject, so: stage a file, cap it, squash
# it, unsquash it, read it back. If that round trip loses the capability (no CAP_SETFCAP in the
# container, a filesystem that cannot store security.capability, an unsquashfs without xattr
# support) this returns non-zero and the caller FAILS rather than silently approving.
squashfs_reader_is_honest() {
  local probe img out got
  probe="$(mktemp -d)"; img="$probe/probe.squashfs"; out="$probe/out"
  mkdir -p "$probe/tree"
  printf '#!/bin/true\n' > "$probe/tree/capped"; chmod 0755 "$probe/tree/capped"
  printf '#!/bin/true\n' > "$probe/tree/plain";  chmod 0755 "$probe/tree/plain"
  if ! setcap "$WANT_WORKER_CAPS" "$probe/tree/capped" 2>/dev/null; then
    rm -rf "$probe"; return 1
  fi
  mksquashfs "$probe/tree" "$img" -noappend -quiet >/dev/null 2>&1 || { rm -rf "$probe"; return 1; }
  unsquashfs -no-progress -xattrs -d "$out" "$img" >/dev/null 2>&1 || { rm -rf "$probe"; return 1; }
  got="$(caps_norm "$(getcap "$out/capped" 2>/dev/null | sed 's/^[^ ]* //')")"
  # Positive control AND negative control: it must see the capability that is there, and must not
  # invent one that is not.
  [ "$got" = "$WANT_WORKER_CAPS" ] || { rm -rf "$probe"; return 1; }
  [ -z "$(caps_norm "$(getcap "$out/plain" 2>/dev/null | sed 's/^[^ ]* //')")" ] || { rm -rf "$probe"; return 1; }
  rm -rf "$probe"; return 0
}

check_sysext_raw() {
  local raw="$1" label; label="$(basename "$raw")"
  local tmp list host_caps worker_caps worker_present=0
  for t in unsquashfs mksquashfs getcap setcap; do
    command -v "$t" >/dev/null 2>&1 || { err "$label: missing $t — cannot read the image's capabilities"; return 1; }
  done
  if ! squashfs_reader_is_honest; then
    err "$label: this runner cannot round-trip a file capability through squashfs (no CAP_SETFCAP,"
    err "$label: or an unsquashfs/filesystem without xattr support). Refusing to report a PASS that"
    err "$label: would be blind — a guard that cannot fail is not a guard. Run this leg as root on"
    err "$label: a filesystem that stores security.capability."
    return 1
  fi
  list="$(payload_listing "$raw")"
  require_listing "$label" "$list" || return 1
  case "$list" in *"$WORKER_REL"*) worker_present=1 ;; esac
  case "$list" in
    *"$HOST_REL"*) ;;
    *) if [ "$worker_present" = 0 ]; then note "--  $label: no host and no worker inside, skipping"; return 0; fi ;;
  esac
  tmp="$(mktemp -d)"
  unsquashfs -no-progress -xattrs -d "$tmp/x" "$raw" "$HOST_REL" "$WORKER_REL" >/dev/null 2>&1 || true
  host_caps=""; worker_caps=""
  [ -f "$tmp/x/$HOST_REL" ]   && host_caps="$(getcap "$tmp/x/$HOST_REL" 2>/dev/null | sed 's/^[^ ]* //')"
  [ -f "$tmp/x/$WORKER_REL" ] && worker_caps="$(getcap "$tmp/x/$WORKER_REL" 2>/dev/null | sed 's/^[^ ]* //')"
  rm -rf "$tmp"
  assert_matrix "$label" "$(caps_norm "$host_caps")" "$(caps_norm "$worker_caps")" "$worker_present"
}

# --- self-test ---------------------------------------------------------------------------------
# Red-teams the assertions themselves: every row states the verdict it MUST produce, and the row
# that matters most is the 0.26.0-1 one — a capped host has to come out RED. Pure bash, so it runs
# anywhere (macOS included) with no setcap, rpm or dpkg in sight.
self_test() {
  local failures=0
  _expect() {  # _expect <want:pass|fail> <label> <host> <worker> <present>
    local want="$1"; shift
    local got=pass
    assert_matrix "$@" >/dev/null 2>&1 || got=fail
    if [ "$got" = "$want" ]; then
      printf '  ok    %-42s -> %s\n' "$1" "$got"
    else
      printf '  FAIL  %-42s -> %s (wanted %s)\n' "$1" "$got" "$want"; failures=$((failures + 1))
    fi
  }
  note "matrix:"
  _expect pass "clean package"                        "" "$WANT_WORKER_CAPS" 1
  _expect fail "HOST CAPPED (the 0.26.0-1 regression)" "cap_sys_nice=ep" "$WANT_WORKER_CAPS" 1
  _expect fail "host capped with something else"       "cap_net_admin=ep" "$WANT_WORKER_CAPS" 1
  _expect fail "host capped, worker fine but missing"  "cap_sys_nice=ep" "" 0
  _expect fail "worker absent"                         "" "" 0
  _expect fail "worker present but uncapped"           "" "" 1
  _expect fail "worker over-granted"                   "" "cap_sys_admin=ep" 1
  _expect fail "worker granted the wrong flags"        "" "cap_sys_nice=eip" 1
  _expect pass "build time: worker left uncapped"      "" "" 1 uncapped-ok
  _expect fail "build time: host capped"               "cap_sys_nice=ep" "" 1 uncapped-ok
  _expect fail "build time: worker over-granted"       "" "cap_sys_admin=ep" 1 uncapped-ok
  _expect fail "build time: worker absent"             "" "" 0 uncapped-ok

  note "canonicalisation:"
  _norm() {
    local got; got="$(caps_norm "$1")"
    if [ "$got" = "$2" ]; then printf '  ok    %-42s -> %s\n' "${1:-<empty>}" "${got:-<empty>}"
    else printf '  FAIL  %-42s -> %s (wanted %s)\n' "${1:-<empty>}" "${got:-<empty>}" "$2"; failures=$((failures + 1)); fi
  }
  _norm 'cap_sys_nice=ep'   'cap_sys_nice=ep'    # libcap >= ~2.36
  _norm '= cap_sys_nice+ep' 'cap_sys_nice=ep'    # older libcap
  _norm '(none)'            ''                   # rpm, no %caps
  _norm ''                  ''

  note "scriptlet reader (the Arch .INSTALL / dpkg postinst channels):"
  local _sl
  _slcase() {  # _slcase <label> <text> <want_host_grant:yes|no> <want_worker_caps>
    local label="$1" text="$2" want_host="$3" want_worker="$4" gh gw ok=1
    gh="$(printf '%s\n' "$text" | scriptlet_host_grant)"
    gw="$(caps_norm "$(printf '%s\n' "$text" | scriptlet_worker_grant)")"
    case "$want_host" in
      yes) [ -n "$gh" ] || ok=0 ;;
      no)  [ -z "$gh" ] || ok=0 ;;
    esac
    [ "$gw" = "$want_worker" ] || ok=0
    if [ "$ok" = 1 ]; then printf '  ok    %-42s\n' "$label"
    else printf '  FAIL  %-42s host_grant=%s worker=%s\n' "$label" "${gh:-<none>}" "${gw:-<none>}"; failures=$((failures + 1)); fi
  }
  _sl='_grant() { setcap '"'"'cap_sys_nice=ep'"'"' usr/bin/punktfunk-encode-worker; }
_revoke() { setcap -r usr/bin/punktfunk-host 2>/dev/null || true; }'
  _slcase "what this repo ships" "$_sl" no 'cap_sys_nice=ep'
  _sl='# setcap '"'"'cap_sys_nice=ep'"'"' usr/bin/punktfunk-host   <- only a comment
setcap '"'"'cap_sys_nice=ep'"'"' usr/bin/punktfunk-encode-worker'
  _slcase "a commented-out host grant is not a grant" "$_sl" no 'cap_sys_nice=ep'
  _sl='setcap '"'"'cap_sys_nice=ep'"'"' usr/bin/punktfunk-host
setcap '"'"'cap_sys_nice=ep'"'"' usr/bin/punktfunk-encode-worker'
  _slcase "0.26.0-1 scriptlet (must be caught)" "$_sl" yes 'cap_sys_nice=ep'
  _sl='setcap -r usr/bin/punktfunk-host 2>/dev/null || true'
  _slcase "removal only, no worker grant" "$_sl" no ''

  if [ "$failures" != 0 ]; then err "self-test: $failures case(s) wrong"; return 1; fi
  note "self-test: all cases behaved as specified"
  return 0
}

# --- main ---------------------------------------------------------------------------------------
main() {
  [ $# -gt 0 ] || { echo "usage: $0 <artifact> [...] | --self-test" >&2; return 2; }
  if [ "$1" = "--self-test" ]; then self_test; return $?; fi

  local artifact
  for artifact in "$@"; do
    [ -e "$artifact" ] || { err "no such artifact: $artifact"; RC=1; continue; }
    case "$artifact" in
      *.pkg.tar.zst|*.pkg.tar.xz) check_arch_pkg   "$artifact" || RC=1 ;;
      *.deb)                      check_deb        "$artifact" || RC=1 ;;
      *.rpm)                      check_rpm        "$artifact" || RC=1 ;;
      *.raw)                      check_sysext_raw "$artifact" || RC=1 ;;
      *) err "don't know how to read capabilities out of $artifact"; RC=1 ;;
    esac
  done

  if [ "$RC" != 0 ]; then
    err "capability matrix WRONG — refusing to publish. The host must never carry a capability"
    err "(0.26.0-1 killed every KDE desktop session that way); the GPU-priority grant belongs on"
    err "$WORKER_REL and nowhere else."
  fi
  return "$RC"
}

# Sourceable: `source scripts/ci/assert-cap-matrix.sh` defines the readers and `assert_matrix`
# without running anything, so they can be driven from a test harness on a box that has none of
# the packaging tools (this is how the scriptlet reader is exercised against the real
# packaging/arch/punktfunk-host.install and the real dpkg postinst).
if [ "${BASH_SOURCE[0]}" = "$0" ]; then
  main "$@"
  exit $?
fi
