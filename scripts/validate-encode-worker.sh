#!/usr/bin/env bash
# WP3 — the on-glass validation kit for `punktfunk-encode-worker`.
#
# Plan: punktfunk-planning/design/gpu-priority-capability-worker-implementation-plan.md §2/WP3.
# This script is the mechanical half of that table. It runs ON a real box; it needs a GPU, and for
# the KDE legs a live KDE session. Nothing here talks to another machine: no ssh, no sudo, no
# setcap.
#
# WHAT IS BEING VALIDATED. `punktfunk-encode-worker` is a separate binary carrying
# `cap_sys_nice=ep`; it encodes PyroWave at an elevated VK_KHR_global_priority class so the encode
# dispatch can preempt a GPU-bound game. `punktfunk-host` carries NO capability, ever: KWin
# identifies a Wayland client by resolving its /proc/<pid>/exe, the kernel refuses that readlink to
# any reader whose effective set is not a superset of the target's PERMITTED set
# (cap_ptrace_access_check), KWin holds no capabilities — so a capability-carrying host is
# unidentifiable, `zkde_screencast_unstable_v1` is never advertised, and every KDE desktop session
# loses streaming. V1 below is the regression test for that.
#
# SAFE BY DEFAULT. With no arguments this only READS: /proc, getcap, the installed .desktop. It
# starts no session, kills nothing, and never calls setcap. Every leg that runs a capture+encode
# session must be named explicitly; the one leg that kills a process needs --allow-mutate on top.
# This script NEVER setcaps anything, least of all the host — it asserts the host is uncapped and
# hard-fails if it is not, which is the entire point of V1.
#
# SKIPPED IS NOT A PASS. A leg that cannot run prints SKIP with the reason and the exit code says
# so (2). Only PASS counts, and anything this script cannot determine is reported, never assumed.
#
#   Usage:  scripts/validate-encode-worker.sh [LEG ...] [OPTIONS]
#           scripts/validate-encode-worker.sh recipe      # the per-box human recipe, incl. sudo
#           scripts/validate-encode-worker.sh --help
#
#   Legs:
#     inspect  (default) pure read-only inspection: the capability matrix as it exists on THIS box,
#              plus every running host's runtime capability state. No session, no spawn.
#     v1       the capability-on-host regression test — KDE only.
#     v2       grant: the worker reports REALTIME.
#     v3a      IPC hop cost: in-process vs an UNCAPPED worker, both at default GPU priority.
#              R1's pre-registered abandonment gate. Needs no capability and no KDE.
#     v3b      lever benefit: in-process at default priority vs the CAPPED worker.
#     v4       the fallback ladder (chaos). Read-only rungs by default; --allow-mutate adds kill -9.
#     v5       fd hygiene over a long session (default 10 min).
#     auto     inspect + v1 + v2 + v3a + v4 — everything that needs no extra permission.
#     all      auto + v3b + v5.
#     recipe   the per-box human recipe, including the privileged steps this script will not run.
#     selftest red-team this kit's own readers against synthetic logs. No box, no GPU, no Linux —
#              run it before trusting a green run, because an assertion that cannot fail is not one.
#
# ⚠ USE INSTALLED PATHS ON THE KDE LEGS. KWin caches the grant per EXECUTABLE PATH, matched against
#   an installed .desktop's `Exec=`. A binary run from a scratch build directory is a different
#   path, so KWin refuses it and `zkde_screencast_unstable_v1` never appears. That is identification
#   WORKING, not a bug — do not go debugging KWin. This script takes the running host's binary,
#   else the installed .desktop's `Exec=`, so the legs run against the path KWin knows.
#
# ⚠ THE WORKER'S PERF SUMMARY GOES TO INHERITED STDERR. The p50/p99 lines are emitted by the worker
#   process, whose stderr is the host's. They reach `journalctl`; they do NOT reach the web
#   console's Logs tab (that ring is a tracing layer INSIDE the host process). This kit runs the
#   spike itself and captures stderr directly, so it never depends on either.

# No `set -e`. This harness deliberately runs commands that are EXPECTED to fail (a readlink that
# must be refused, a worker that must not spawn); under -e a normal negative result aborts the run
# and looks like a crash. Every fallible call below checks its own status instead.
set -uo pipefail

VERSION='WP3 kit 1'
SELF="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

# --- the capability vocabulary, reused rather than re-derived ------------------------------------
# scripts/ci/assert-cap-matrix.sh (WP4) is sourceable by design and owns `caps_norm` — the
# canonicalizer for getcap's two output forms ("path cap_sys_nice=ep" since libcap ~2.36,
# "path = cap_sys_nice+ep" before) and rpm's "(none)". Reuse it: two normalizers that disagree is
# exactly how a wrong capability string gets waved through.
#
# It carries `set -euo pipefail` at the top, which sourcing would import into THIS shell, so the
# option set is saved and restored around the source.
CAP_MATRIX_SH="$SELF/ci/assert-cap-matrix.sh"
if [ ! -r "$CAP_MATRIX_SH" ]; then
  echo "FATAL: cannot read $CAP_MATRIX_SH — run this from a Punktfunk checkout (it owns the" >&2
  echo "       capability-string canonicalizer this kit reuses)." >&2
  exit 3
fi
_saved_opts="$(set +o)"
# shellcheck source=scripts/ci/assert-cap-matrix.sh
. "$CAP_MATRIX_SH"
eval "$_saved_opts"
unset _saved_opts
set +e   # `eval` above restored -e from the sourced file's perspective; this is our posture.

WORKER_BIN_NAME='punktfunk-encode-worker'
HOST_BIN_NAME='punktfunk-host'
# CAP_SYS_NICE is capability bit 23, so the kernel prints it as CapPrm: 0000000000800000.
CAP_SYS_NICE_BIT=23

# --- the log lines this kit asserts on, verbatim -------------------------------------------------
# Every one of these is a fixed substring of a real log message or error context (grep -F, never a
# regex — the messages contain em dashes and parentheses). If a message is reworded, this block is
# the single place to fix, and a leg that stops matching FAILS rather than silently passing.
L_FALLBACK='encoding in-process at default GPU priority'        # the clause EVERY fallback ends with
L_OFF='pyrowave: PUNKTFUNK_ENCODE_WORKER=off'
L_NOTFOUND='the encode worker was not found beside the host binary or on PATH'
L_NOTUP='the encode worker did not come up'
L_GRANTED='encoding in the capability-carrying worker at an elevated global queue priority'
L_NOPRIO='encoding in the capability-carrying worker, no queue priority requested'
L_INERT='every global queue priority class was refused'
L_INERT_WORKER='the encode worker needs CAP_SYS_NICE'              # the worker-side wording (host logs it)
L_INERT_HOST='only the encode worker should carry CAP_SYS_NICE'    # the in-process wording
L_LEAVING='pyrowave: leaving the encode worker'
L_DIED='the encode worker died mid-session'
L_RESPAWNED='respawned the encode worker after a mid-session death'
L_NORESPAWN='the encode worker would not respawn'
# The proxy's attributable "my peer went away" errors. `spike` has NO encoder-recovery loop — it does
# `encoder.submit(..)?` and exits — so under the spike a killed worker surfaces as one of these and
# never reaches `Encoder::reset`, which is where L_DIED/L_RESPAWNED are emitted. See v4.e.
L_PEERGONE='no reply from the encode worker'
L_PEERPIPE='send to the encode worker'
L_WORKER_READY='punktfunk-encode-worker ready'
L_PERF='pyrowave encode, submit->AU'
L_CAPTURE='capture pipeline resolved:'
L_KWIN_DENIED='does not expose zkde_screencast_unstable_v1'

# --- options -------------------------------------------------------------------------------------
OPT_HOST_BIN=''
OPT_WORKER_BIN=''
OPT_HOST_PID=''
OPT_SOURCE=''
OPT_SECONDS=45
OPT_MINUTES=10
OPT_WIDTH=1920
OPT_HEIGHT=1080
OPT_FPS=60
OPT_BITRATE=20
OPT_GATE_MS='1.0'
OPT_FD_TOLERANCE=0
OPT_ALLOW_MUTATE=0
OPT_KEEP_LOGS=0
OPT_LOG_DIR=''
OPT_LOOPBACK=0
LEGS=()

usage() {
  sed -n '2,56p' "${BASH_SOURCE[0]}" | sed -e 's/^# \{0,1\}//'
  cat <<'EOF'

  Options:
    --host-bin PATH     the INSTALLED punktfunk-host (default: from a running host, then the
                        installed .desktop's Exec=, then PATH)
    --worker-bin PATH   the INSTALLED punktfunk-encode-worker (default: $PUNKTFUNK_ENCODE_WORKER,
                        then beside the host binary, then PATH — mirrors the host's own resolution)
    --host-pid PID      inspect this host process only (default: every running punktfunk-host)
    --source SRC        spike capture source: kwin-virtual | portal (default: kwin-virtual under
                        KWin, portal elsewhere). ⚠ synthetic is NOT usable — see the note in v3a.
    --seconds N         wall clock per measured arm (default 45)
    --minutes N         v5 session length (default 10)
    --width/--height/--fps N        spike geometry (default 1920x1080@60)
    --bitrate MBPS      spike bitrate (default 20)
    --gate-ms X         v3a abandonment gate, p99 headroom in ms (default 1.0)
    --fd-tolerance N    v5 allowed fd growth in the steady-state window (default 0)
    --with-loopback     keep the spike's punktfunk-core loopback on (default: --no-loopback, less
                        CPU noise in the latency arms)
    --allow-mutate      permit the legs that change the box: v4's `kill -9` of a live worker
    --log-dir DIR       where the captured spike logs go (default: a mktemp dir)
    --keep-logs         do not delete the captured logs on exit
    -h, --help          this text

  Exit: 0 every attempted leg PASSed and none was skipped · 1 at least one FAIL · 2 no FAIL but at
        least one SKIP (the run is incomplete — a skip is never a pass) · 3 the kit could not start.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    inspect|v1|v2|v3a|v3b|v4|v5|auto|all) LEGS+=("$1") ;;
    recipe|selftest) LEGS+=("$1") ;;
    --host-bin)   OPT_HOST_BIN="${2:-}"; shift ;;
    --worker-bin) OPT_WORKER_BIN="${2:-}"; shift ;;
    --host-pid)   OPT_HOST_PID="${2:-}"; shift ;;
    --source)     OPT_SOURCE="${2:-}"; shift ;;
    --seconds)    OPT_SECONDS="${2:-}"; shift ;;
    --minutes)    OPT_MINUTES="${2:-}"; shift ;;
    --width)      OPT_WIDTH="${2:-}"; shift ;;
    --height)     OPT_HEIGHT="${2:-}"; shift ;;
    --fps)        OPT_FPS="${2:-}"; shift ;;
    --bitrate)    OPT_BITRATE="${2:-}"; shift ;;
    --gate-ms)    OPT_GATE_MS="${2:-}"; shift ;;
    --fd-tolerance) OPT_FD_TOLERANCE="${2:-}"; shift ;;
    --with-loopback) OPT_LOOPBACK=1 ;;
    --allow-mutate)  OPT_ALLOW_MUTATE=1 ;;
    --log-dir)    OPT_LOG_DIR="${2:-}"; shift ;;
    --keep-logs)  OPT_KEEP_LOGS=1 ;;
    -h|--help)    usage; exit 0 ;;
    *) echo "unknown argument '$1' (try --help)" >&2; exit 3 ;;
  esac
  shift
done
[ ${#LEGS[@]} -gt 0 ] || LEGS=(inspect)

# Numeric options are used in arithmetic; a typo must be a refusal, not a bash error mid-leg.
for _n in OPT_SECONDS OPT_MINUTES OPT_WIDTH OPT_HEIGHT OPT_FPS OPT_BITRATE OPT_FD_TOLERANCE; do
  case "${!_n}" in
    ''|*[!0-9]*) echo "FATAL: --${_n#OPT_} must be a whole number, got '${!_n}'" >&2; exit 3 ;;
  esac
done
case "$OPT_GATE_MS" in ''|*[!0-9.]*) echo "FATAL: --gate-ms must be numeric, got '$OPT_GATE_MS'" >&2; exit 3 ;; esac
unset _n

# --- result bookkeeping ---------------------------------------------------------------------------
N_PASS=0; N_FAIL=0; N_SKIP=0
RESULTS=()
C_OK=''; C_BAD=''; C_WARN=''; C_OFF=''
if [ -t 1 ]; then C_OK=$'\033[32m'; C_BAD=$'\033[31m'; C_WARN=$'\033[33m'; C_OFF=$'\033[0m'; fi

pass() { N_PASS=$((N_PASS+1)); RESULTS+=("PASS|$1|$2"); printf '  %sPASS%s  %-6s %s\n' "$C_OK" "$C_OFF" "$1" "$2"; }
fail() { N_FAIL=$((N_FAIL+1)); RESULTS+=("FAIL|$1|$2"); printf '  %sFAIL%s  %-6s %s\n' "$C_BAD" "$C_OFF" "$1" "$2"; }
skip() { N_SKIP=$((N_SKIP+1)); RESULTS+=("SKIP|$1|$2"); printf '  %sSKIP%s  %-6s %s\n' "$C_WARN" "$C_OFF" "$1" "$2"; }
info() { printf '        %s\n' "$*"; }
head2() { printf '\n%s\n' "$*"; }

# --- capability readers ---------------------------------------------------------------------------

# The canonical capability string of a FILE, or the marker `?` when it genuinely could not be read.
# `?` is never treated as "none" — a blind read that reports "no capability" would wave through the
# exact state V1 exists to catch.
file_caps() {
  local f="$1" raw
  [ -e "$f" ] || { printf '?'; return 1; }
  if command -v getcap >/dev/null 2>&1; then
    raw="$(getcap "$f" 2>/dev/null)"
    # getcap prints nothing at all for a file with no capability, and exits 0 — so an empty read
    # here is a real "none", not a failed read.
    if [ -z "$raw" ]; then printf ''; return 0; fi
    caps_norm "$(printf '%s' "$raw" | sed -e 's/^[^ ]* //' )"
    return 0
  fi
  if command -v getfattr >/dev/null 2>&1; then
    # Fallback that at least distinguishes present-vs-absent, even though it cannot render flags.
    if getfattr -n security.capability --only-values -- "$f" >/dev/null 2>&1; then
      printf 'cap_present_unreadable'; return 0
    fi
    printf ''; return 0
  fi
  printf '?'; return 1
}

# The PERMITTED capability mask of a live process, as the kernel's 16-hex-digit word, or '' if the
# field could not be read. /proc/<pid>/status is world-readable and carries CapPrm unconditionally,
# so this works across a capability boundary where a readlink of /proc/<pid>/exe does not.
proc_capprm() {
  local pid="$1" line
  line="$(grep -m1 '^CapPrm:' "/proc/$pid/status" 2>/dev/null)"
  [ -n "$line" ] || { printf ''; return 1; }
  printf '%s' "$line" | awk '{print $2}'
}

capprm_is_zero() { case "${1:-}" in ''|*[!0]*) return 1 ;; *) return 0 ;; esac; }

# Bit test against a 16-hex-digit mask. Only the low 32 bits are converted, which is both enough
# (CAP_SYS_NICE is bit 23) and immune to the signed-64-bit overflow a full-width $((16#...)) invites.
capprm_has_bit() {
  local hex="${1:-}" bit="${2:-0}" low
  [ -n "$hex" ] || return 2
  case "$hex" in *[!0-9a-fA-F]*) return 2 ;; esac
  low="${hex: -8}"
  (( (16#$low >> bit) & 1 ))
}

# --- process discovery ----------------------------------------------------------------------------

host_pids() {
  if [ -n "$OPT_HOST_PID" ]; then printf '%s\n' "$OPT_HOST_PID"; return 0; fi
  # -x matches `comm`, which for a normally-exec'd host IS punktfunk-host.
  pgrep -x "$HOST_BIN_NAME" 2>/dev/null
}

# The worker processes spawned by a given host pid.
#
# ⚠ `pgrep -x punktfunk-encode-worker` does NOT find them. The host execs the worker through the
# pinned `/proc/self/fd/<n>` path (so a replaced binary cannot be swapped under a spawn), and the
# kernel sets `comm` from the basename of the path passed to execve — i.e. a bare number. argv[0]
# IS `punktfunk-encode-worker`, so a full-cmdline match works — but matching by PARENT is exact and
# cannot pick up somebody else's worker, which matters a great deal before a `kill -9`.
worker_pids_of() {
  local parent="$1" p cmd
  for p in $(pgrep -P "$parent" 2>/dev/null); do
    cmd="$(tr '\0' ' ' < "/proc/$p/cmdline" 2>/dev/null)"
    case "$cmd" in *"$WORKER_BIN_NAME"*) printf '%s\n' "$p" ;; esac
  done
}

fd_count() {
  local pid="$1" n
  n="$(ls "/proc/$pid/fd" 2>/dev/null | wc -l | tr -d ' ')"
  [ -n "$n" ] || { printf ''; return 1; }
  printf '%s' "$n"
}

# --- binary resolution ----------------------------------------------------------------------------

DESKTOP_FILE=''
DESKTOP_EXEC=''
find_desktop() {
  local d f
  for d in "${XDG_DATA_HOME:-$HOME/.local/share}/applications" \
           /usr/share/applications /usr/local/share/applications \
           /var/lib/flatpak/exports/share/applications; do
    f="$d/io.unom.punktfunk.host.desktop"
    if [ -r "$f" ]; then
      DESKTOP_FILE="$f"
      DESKTOP_EXEC="$(sed -n 's/^Exec=//p' "$f" | head -1 | awk '{print $1}')"
      return 0
    fi
  done
  return 1
}

HOST_BIN=''; HOST_BIN_FROM=''
WORKER_BIN=''; WORKER_BIN_FROM=''
resolve_binaries() {
  local pid
  if [ -n "$OPT_HOST_BIN" ]; then
    HOST_BIN="$OPT_HOST_BIN"; HOST_BIN_FROM='--host-bin'
  else
    for pid in $(host_pids); do
      HOST_BIN="$(readlink -f "/proc/$pid/exe" 2>/dev/null)"
      [ -n "$HOST_BIN" ] && { HOST_BIN_FROM="the running host, pid $pid"; break; }
    done
    if [ -z "$HOST_BIN" ] && find_desktop && [ -n "$DESKTOP_EXEC" ]; then
      HOST_BIN="$DESKTOP_EXEC"; HOST_BIN_FROM="$DESKTOP_FILE (Exec=)"
    fi
    if [ -z "$HOST_BIN" ]; then
      HOST_BIN="$(command -v "$HOST_BIN_NAME" 2>/dev/null)"
      [ -n "$HOST_BIN" ] && HOST_BIN_FROM='PATH'
    fi
  fi
  # Absolute, always: these strings become argv[0] of a spawned process, and `env ./relative` is a
  # PATH lookup that fails rather than the file the operator meant.
  [ -n "$HOST_BIN" ] && HOST_BIN="$(readlink -f "$HOST_BIN" 2>/dev/null || printf '%s' "$HOST_BIN")"
  find_desktop >/dev/null 2>&1

  # Mirror the host's own resolution order (pf-zerocopy's ipc::resolve_worker_path).
  if [ -n "$OPT_WORKER_BIN" ]; then
    WORKER_BIN="$OPT_WORKER_BIN"; WORKER_BIN_FROM='--worker-bin'
  elif [ -n "${PUNKTFUNK_ENCODE_WORKER:-}" ] && \
       [ "$(printf '%s' "${PUNKTFUNK_ENCODE_WORKER}" | tr 'A-Z' 'a-z')" != off ]; then
    WORKER_BIN="$PUNKTFUNK_ENCODE_WORKER"; WORKER_BIN_FROM='$PUNKTFUNK_ENCODE_WORKER'
  elif [ -n "$HOST_BIN" ] && [ -f "$(dirname "$HOST_BIN")/$WORKER_BIN_NAME" ]; then
    WORKER_BIN="$(dirname "$HOST_BIN")/$WORKER_BIN_NAME"; WORKER_BIN_FROM='beside the host binary'
  else
    WORKER_BIN="$(command -v "$WORKER_BIN_NAME" 2>/dev/null)"
    [ -n "$WORKER_BIN" ] && WORKER_BIN_FROM='PATH'
  fi
  [ -n "$WORKER_BIN" ] && WORKER_BIN="$(readlink -f "$WORKER_BIN" 2>/dev/null || printf '%s' "$WORKER_BIN")"
  return 0
}

# --- log assertions --------------------------------------------------------------------------------

# EVERY log read goes through this. `tracing`'s fmt layer emits SGR escapes around field NAMES, so a
# real line reads `p99_us\e[0m\e[2m=\e[0m4601` — the message text is plain but `key=value` is not.
# That is invisible in a hand-written fixture. Anything matching a FIELD (priority=, p99_us=,
# reason=) breaks without this; anything matching prose only got lucky.
# The spike is also launched with NO_COLOR=1 so fresh logs are plain — this strips the rest
# (journalctl, an operator's own capture, a log from an older build).
strip_ansi() { LC_ALL=C sed -E $'s/\x1b\\[[0-9;]*[a-zA-Z]//g'; }
log_cat()    { [ -r "$1" ] && LC_ALL=C strip_ansi <"$1"; }

logs_have()    { log_cat "$1" | LC_ALL=C grep -qF -- "$2"; }
logs_first()   { log_cat "$1" | LC_ALL=C grep -m1 -F -- "$2"; }
# Always a number, even for a log that does not exist — a bare `grep -c` prints nothing on a missing
# file and `[ "" -ge 1 ]` is a bash syntax error, not a failed assertion. (The default has to be
# applied in the SHELL: `sed 's/^$/0/'` sees no line at all on empty input and emits nothing.)
logs_count() {
  local n; n="$(log_cat "$1" | LC_ALL=C grep -cF -- "$2" | tr -dc '0-9')"
  printf '%s' "${n:-0}"
}
perf_windows() { logs_count "$1" "$L_PERF"; }
# Perf windows that appear AFTER a marker line. This is what "the stream survived" actually means
# for the chaos leg: a warn followed by silence is a dead session that logged politely.
perf_windows_after() {
  log_cat "$1" | LC_ALL=C awk -v m="$2" -v p="$L_PERF" \
    'index($0,m){seen=1; next} seen && index($0,p){n++} END{print n+0}'
}

# The single most important assertion in the measured legs: prove the arm is the arm it claims.
#
# A PyroWave session that receives a CPU frame pins itself in-process for the rest of the session
# (a socket copy of 1080p BGRA is ~480 MB/s), so a `cpu → pyrowave` capture arm silently turns the
# "worker" arm into a second in-process arm — an A/B of inline against inline that PASSES the gate
# for entirely the wrong reason. This refuses that.
assert_worker_arm() {
  local log="$1" tag="$2" rc=0
  if ! logs_have "$log" "$L_GRANTED" && ! logs_have "$log" "$L_NOPRIO" && ! logs_have "$log" "$L_INERT_WORKER"; then
    info "$tag: no 'capability-carrying worker' line — the worker never took the session"; rc=1
  fi
  if logs_have "$log" "$L_FALLBACK"; then
    info "$tag: a fallback rung fired: $(logs_first "$log" "$L_FALLBACK" | sed 's/^.*pyrowave: /pyrowave: /')"; rc=1
  fi
  if logs_have "$log" "$L_LEAVING"; then
    info "$tag: the session LEFT the worker mid-run: $(logs_first "$log" "$L_LEAVING" | sed 's/^.*pyrowave: /pyrowave: /')"; rc=1
  fi
  if logs_have "$log" "$L_CAPTURE" && ! log_cat "$log" | LC_ALL=C grep -F -- "$L_CAPTURE" | grep -q 'dmabuf-passthrough'; then
    info "$tag: capture arm is not dmabuf-passthrough — $(logs_first "$log" "$L_CAPTURE" | sed 's/^.*capture pipeline/capture pipeline/')"
    info "$tag: a non-dmabuf frame pins the session in-process, so this arm is NOT the worker."
    rc=1
  fi
  return $rc
}

# Why an arm produced nothing — checked BEFORE any arm-identity assert.
#
# "this arm is not the in-process arm" is wrong when the truth is "capture never came up" (an
# unanswered GNOME consent dialog, say), and it sends the reader looking at the wrong thing.
# Prints a human reason and returns 0 when the arm never got far enough to be judged; returns 1
# when the run is judgeable.
spike_failure_reason() {
  local log="$1" line
  if logs_have "$log" 'timed out waiting for the ScreenCast portal'; then
    printf '%s' "the xdg ScreenCast portal was never answered. GNOME and KDE raise a CONSENT DIALOG on the host's own screen, and it cannot be answered from inside a stream — approve it there, or run a leg that needs no portal"
    return 0
  fi
  line="$(log_cat "$log" | LC_ALL=C grep -E 'ERROR' | head -1 | sed 's/^.*ERROR[[:space:]]*//')"
  if [ -n "$line" ]; then printf '%s' "$line"; return 0; fi
  if [ "$(perf_windows "$log")" = 0 ]; then
    printf '%s' "the spike never encoded a frame — no PUNKTFUNK_PERF window in the log at all"
    return 0
  fi
  return 1
}

assert_inline_arm() {
  local log="$1" tag="$2" rc=0
  if ! logs_have "$log" "$L_OFF"; then
    info "$tag: no 'PUNKTFUNK_ENCODE_WORKER=off' line — this arm did not force the in-process path"; rc=1
  fi
  if logs_have "$log" "$L_GRANTED" || logs_have "$log" "$L_NOPRIO"; then
    info "$tag: a 'capability-carrying worker' line is present — this arm is not in-process"; rc=1
  fi
  return $rc
}

# --- the perf instrument ----------------------------------------------------------------------------
#
# `PUNKTFUNK_PERF` makes the PyroWave encoder emit one summary line every ~2 s over >=30 frames:
#   frames=30 mean_us=… p50_us=… p99_us=… max_us=… depth=… pyrowave encode, submit->AU (…)
# The measured quantity is submit→AU: CSC + encode + fence wait + packetize. Per PW1 the FIRST
# window is warm-up and is dropped, and `encode_fps` is a VACUOUS metric here (a starved capture
# makes both arms report the capture rate) — latency is the whole point.
#
# In worker mode the line comes from the WORKER process on inherited stderr; in-process it comes
# from the host. Either way it lands in the log this kit captures.
perf_p99_series() { log_cat "$1" | LC_ALL=C grep -F -- "$L_PERF" | sed -n 's/.*p99_us=\([0-9][0-9]*\).*/\1/p'; }
perf_p50_series() { log_cat "$1" | LC_ALL=C grep -F -- "$L_PERF" | sed -n 's/.*p50_us=\([0-9][0-9]*\).*/\1/p'; }

# LC_ALL=C on every awk here is load-bearing, not decoration: under a comma-decimal locale awk
# PRINTS "6,40" for a millisecond figure and, worse, READS "1.0" as 1 — which would silently turn
# the --gate-ms 1.0 abandonment gate into a 1 µs gate. Numbers in this kit are C-locale throughout.
median_us() {
  LC_ALL=C sort -n | LC_ALL=C awk 'NF{a[++n]=$1} END{ if(!n) exit 1; if(n%2) print a[(n+1)/2]; else print int((a[n/2]+a[n/2+1])/2) }'
}
ms() { LC_ALL=C awk -v u="${1:-0}" 'BEGIN{ printf "%.2f", u/1000 }'; }

# Median p99 (and p50) across all windows after the warm-up one. Prints "p50_us p99_us windows",
# or nothing when there is not enough data to mean anything.
perf_summary() {
  local log="$1" min_windows="${2:-3}" p99s p50s n m99 m50
  p99s="$(perf_p99_series "$log" | tail -n +2)"
  p50s="$(perf_p50_series "$log" | tail -n +2)"
  n="$(printf '%s\n' "$p99s" | grep -c '[0-9]')"
  [ "$n" -ge "$min_windows" ] 2>/dev/null || return 1
  m99="$(printf '%s\n' "$p99s" | median_us)" || return 1
  m50="$(printf '%s\n' "$p50s" | median_us)" || return 1
  printf '%s %s %s' "$m50" "$m99" "$n"
}

# --- the spike vehicle -------------------------------------------------------------------------------

LOG_DIR=''
setup_logdir() {
  [ -n "$LOG_DIR" ] && return 0
  if [ -n "$OPT_LOG_DIR" ]; then LOG_DIR="$OPT_LOG_DIR"; mkdir -p "$LOG_DIR"
  else LOG_DIR="$(mktemp -d "${TMPDIR:-/tmp}/pf-wp3.XXXXXX")"; fi
  return 0
}
cleanup() {
  # Never leave a spike (and through PR_SET_PDEATHSIG, its worker) running because the kit was
  # interrupted — a stray capture session holds a virtual output and a GPU context.
  stop_spike
  [ -n "$LOG_DIR" ] || return 0
  if [ "$OPT_KEEP_LOGS" = 1 ] || [ -n "$OPT_LOG_DIR" ]; then
    printf '\nlogs kept in %s\n' "$LOG_DIR"
  else
    rm -rf "$LOG_DIR"
  fi
}
trap cleanup EXIT
trap 'printf "\ninterrupted\n" >&2; exit 130' INT TERM

SPIKE_SOURCE=''
resolve_source() {
  [ -n "$SPIKE_SOURCE" ] && return 0
  if [ -n "$OPT_SOURCE" ]; then SPIKE_SOURCE="$OPT_SOURCE"; return 0; fi
  case "$(compositor_guess)" in
    kwin) SPIKE_SOURCE='kwin-virtual' ;;
    *)    SPIKE_SOURCE='portal' ;;
  esac
}

compositor_guess() {
  # Cheap and honest: a name, never a claim of readiness (probe-compositor is the readiness check).
  if pgrep -x kwin_wayland >/dev/null 2>&1; then printf 'kwin'; return 0; fi
  case "${XDG_CURRENT_DESKTOP:-}" in *KDE*|*kde*) printf 'kwin'; return 0 ;; esac
  if pgrep -x gnome-shell >/dev/null 2>&1; then printf 'mutter'; return 0; fi
  if pgrep -x sway >/dev/null 2>&1; then printf 'sway'; return 0; fi
  if pgrep -x gamescope >/dev/null 2>&1; then printf 'gamescope'; return 0; fi
  printf 'unknown'
}

# run_spike <arm-name> <wall-seconds> <env-assignment>...
#
# Launches `punktfunk-host spike --codec pyrowave` with PUNKTFUNK_PERF armed, captures the combined
# stderr of the host AND its worker (one inherited stream), waits at most `wall + grace`, then
# terminates it. Echoes the log path. Returns non-zero only if the spike could not be started.
#
# Output goes to /dev/null: a 10-minute 1080p PyroWave dump is ~1.5 GB and nothing here reads the
# bitstream. The loopback is off by default — less CPU noise in a latency A/B.
SPIKE_LOG=''
SPIKE_PID=''
run_spike() {
  local arm="$1" wall="$2"; shift 2
  local log="$LOG_DIR/$arm.log" kv
  SPIKE_LOG="$log"; SPIKE_PID=''
  # NO_COLOR: keep the captured log plain at the source. `log_cat` strips SGR escapes anyway, but a
  # plain log is also the one a human greps by hand when a leg goes red, and `p99_us\e[0m\e[2m=…`
  # defeats the obvious grep.
  #
  # PUNKTFUNK_ENCODER=pyrowave changes no capture route. `--codec pyrowave` makes the virtual
  # source pass its dmabuf to PyroWave; the portal source is never a PyroWave session, so on
  # NVIDIA it can resolve `cuda-import`, whose payload PyroWave rejects.
  local -a envs=(PUNKTFUNK_PERF=1 NO_COLOR=1 PUNKTFUNK_ENCODER=pyrowave)
  for kv in "$@"; do envs+=("$kv"); done
  local -a cmd=("$HOST_BIN" spike --codec pyrowave --source "$SPIKE_SOURCE"
                --width "$OPT_WIDTH" --height "$OPT_HEIGHT" --fps "$OPT_FPS"
                --bitrate "$OPT_BITRATE" --seconds "$wall" --out /dev/null)
  [ "$OPT_LOOPBACK" = 1 ] || cmd+=(--no-loopback)
  {
    printf '### %s\n### env: %s\n### cmd: %s\n' "$arm" "${envs[*]}" "${cmd[*]}"
  } > "$log"
  env "${envs[@]}" "${cmd[@]}" >>"$log" 2>&1 &
  SPIKE_PID=$!
  sleep 1
  kill -0 "$SPIKE_PID" 2>/dev/null || {
    # It may have finished legitimately (unlikely in 1 s) or died on open. Let the caller's
    # assertions decide; report the start as OK either way.
    wait "$SPIKE_PID" 2>/dev/null
    SPIKE_PID=''
    return 0
  }
  return 0
}

# Wait out an arm: the spike's own `--seconds` budget plus a grace, then stop it regardless. The
# grace matters because a frame-starved capture will never reach its frame target — the wall
# clock, not the frame count, is what bounds a leg.
wait_spike() {
  local wall=$(( $1 + 15 )) waited=0
  [ -n "$SPIKE_PID" ] || return 0
  while [ "$waited" -lt "$wall" ]; do
    kill -0 "$SPIKE_PID" 2>/dev/null || break
    sleep 1; waited=$((waited+1))
  done
  stop_spike
}

stop_spike() {
  [ -n "$SPIKE_PID" ] || return 0
  if kill -0 "$SPIKE_PID" 2>/dev/null; then
    kill -TERM "$SPIKE_PID" 2>/dev/null
    local t=0
    while [ "$t" -lt 5 ] && kill -0 "$SPIKE_PID" 2>/dev/null; do sleep 1; t=$((t+1)); done
    kill -KILL "$SPIKE_PID" 2>/dev/null
  fi
  wait "$SPIKE_PID" 2>/dev/null
  SPIKE_PID=''
}

# An UNCAPPED copy of the worker binary, in a temp dir. This is how the kit gets a default-priority
# worker without touching the box: a plain `cp` does NOT carry the `security.capability` xattr, so
# the copy is uncapped by construction — no setcap, no sudo, nothing restored afterwards. The copy
# is VERIFIED uncapped before use; an unverifiable copy is refused, never assumed.
#
# ⚠ It is also the only arm immune to R4: the real capped worker is AT_SECURE, so the dynamic loader
# ignores LD_LIBRARY_PATH/LD_PRELOAD. On a box propped up by such a loader shim the copy will start
# and the capped original will not — a v3a PASS beside a v3b spawn failure is that, not a
# regression.
UNCAPPED_COPY=''
make_uncapped_copy() {
  [ -n "$UNCAPPED_COPY" ] && return 0
  [ -n "$WORKER_BIN" ] && [ -x "$WORKER_BIN" ] || return 1
  setup_logdir
  local dst="$LOG_DIR/uncapped-$WORKER_BIN_NAME"
  cp -- "$WORKER_BIN" "$dst" 2>/dev/null || return 1
  chmod 0755 "$dst" 2>/dev/null
  local c; c="$(file_caps "$dst")"
  case "$c" in
    '') : ;;                    # verified uncapped — what we want
    '?') return 2 ;;            # could not read: refuse rather than assume
    *) return 3 ;;              # the copy carried the capability (cp --preserve=xattr?): refuse
  esac
  # A noexec /tmp would make this arm fail for a reason that has nothing to do with the worker.
  "$dst" >/dev/null 2>&1
  case $? in 126|127) return 4 ;; esac
  UNCAPPED_COPY="$dst"
  return 0
}

# ==================================================================================================
# LEGS
# ==================================================================================================

leg_inspect() {
  head2 "INSPECT — the capability matrix on this box (read-only)"

  # --- I1 the worker binary --------------------------------------------------------------------
  if [ -z "$WORKER_BIN" ]; then
    skip I1 "no $WORKER_BIN_NAME found (env, beside the host, PATH) — the host encodes in-process"
    info  "at default GPU priority. That is a legitimate best-effort state, not a failure; it is"
    info  "also why every grant leg below will skip."
  elif [ ! -x "$WORKER_BIN" ]; then
    fail I1 "$WORKER_BIN exists but is not executable"
  else
    pass I1 "worker binary: $WORKER_BIN ($WORKER_BIN_FROM)"
  fi

  # --- I2 the host binary carries NO capability — the whole point ------------------------------
  if [ -z "$HOST_BIN" ]; then
    fail I2 "could not determine the host binary — pass --host-bin. Refusing to report a PASS from"
    info  "a check that read nothing."
  elif [ ! -e "$HOST_BIN" ]; then
    fail I2 "host binary $HOST_BIN does not exist"
  else
    local hc; hc="$(file_caps "$HOST_BIN")"
    case "$hc" in
      '')  pass I2 "host binary carries NO capability: $HOST_BIN" ;;
      '?') fail I2 "could not read capabilities of $HOST_BIN (no getcap/getfattr) — a blind check"
           info  "cannot clear the host, and this is the assertion the leg exists for." ;;
      *)   fail I2 "THE 0.26.0-1 REGRESSION: $HOST_BIN carries '$hc'."
           info  "KWin cannot readlink /proc/<pid>/exe of a capability-carrying process, so it"
           info  "cannot identify the host, never advertises zkde_screencast_unstable_v1, and every"
           info  "KDE desktop session loses streaming. Repair:  sudo setcap -r '$HOST_BIN'"
           info  "The GPU-priority grant belongs on $WORKER_BIN_NAME and nowhere else." ;;
    esac
  fi

  # --- I3 the worker's capability ----------------------------------------------------------------
  if [ -n "$WORKER_BIN" ] && [ -e "$WORKER_BIN" ]; then
    local wc; wc="$(file_caps "$WORKER_BIN")"
    case "$wc" in
      "$WANT_WORKER_CAPS") pass I3 "worker carries exactly $WANT_WORKER_CAPS — the lever is live" ;;
      '')  skip I3 "worker carries NO capability — the GPU-preemption lever is INERT here."
           info  "Legitimate for a source build; the packaging channels grant it at install time."
           info  "By hand:  sudo setcap 'cap_sys_nice=ep' '$WORKER_BIN'   (a REBUILD is a new inode"
           info  "and drops it again). v2/v3b need this; v3a deliberately does not." ;;
      '?') fail I3 "could not read capabilities of $WORKER_BIN — refusing to guess" ;;
      *)   fail I3 "worker carries '$wc', expected exactly '$WANT_WORKER_CAPS' — over-granting a"
           info  "capability-carrying binary is its own hazard; fix the packaging channel." ;;
    esac
  else
    skip I3 "no worker binary to inspect"
  fi

  # --- I4 host and worker are DIFFERENT files ------------------------------------------------------
  # The plan's non-negotiable: never a hardlink, never a subcommand. A shared inode shares the file
  # capability and silently puts it on the host (see I2).
  if [ -n "$HOST_BIN" ] && [ -n "$WORKER_BIN" ] && [ -e "$HOST_BIN" ] && [ -e "$WORKER_BIN" ]; then
    local hi wi
    hi="$(stat -Lc '%d:%i' "$HOST_BIN" 2>/dev/null)"
    wi="$(stat -Lc '%d:%i' "$WORKER_BIN" 2>/dev/null)"
    if [ -z "$hi" ] || [ -z "$wi" ]; then
      skip I4 "no stat(1) that reports device:inode — cannot prove the two are separate files"
    elif [ "$hi" = "$wi" ]; then
      fail I4 "host and worker are THE SAME INODE ($hi) — a hardlink shares the file capability,"
      info  "which puts the capability on the host and re-creates 0.26.0-1."
    else
      pass I4 "host and worker are separate files (inodes $hi / $wi)"
    fi
  else
    skip I4 "need both binaries to compare inodes"
  fi

  # --- I5 every RUNNING host is uncapped and identifiable ------------------------------------------
  local pids; pids="$(host_pids)"
  if [ -z "$pids" ]; then
    skip I5 "no running $HOST_BIN_NAME — start one (or a session) to check the runtime state"
  else
    local pid ok=1 checked=0
    for pid in $pids; do
      checked=$((checked+1))
      local prm; prm="$(proc_capprm "$pid")"
      if [ -z "$prm" ]; then
        info "pid $pid: no CapPrm in /proc/$pid/status — cannot determine, treating as a failure"; ok=0; continue
      fi
      if ! capprm_is_zero "$prm"; then
        info "pid $pid: CapPrm=$prm — this host process HOLDS capabilities. KWin cannot identify it."; ok=0; continue
      fi
      local owner; owner="$(stat -c '%u' "/proc/$pid" 2>/dev/null)"
      if [ -n "$owner" ] && [ "$owner" != "$(id -u)" ]; then
        info "pid $pid: runs as uid $owner, this shell is uid $(id -u) — the /proc/<pid>/exe read"
        info "pid $pid: is only meaningful from the session user KWin runs as. CapPrm=$prm is clean."
        continue
      fi
      if ! readlink "/proc/$pid/exe" >/dev/null 2>&1; then
        info "pid $pid: readlink /proc/$pid/exe REFUSED although CapPrm is zero — something else"
        info "pid $pid: (Yama ptrace_scope, an LSM) is blocking the read KWin identifies clients by."; ok=0; continue
      fi
    done
    if [ "$ok" = 1 ]; then
      pass I5 "$checked running host process(es): CapPrm all zero, /proc/<pid>/exe readable"
    else
      fail I5 "a running host process is not identifiable — see above"
    fi
  fi

  # --- I6 the KWin .desktop grant -------------------------------------------------------------------
  if [ -z "$DESKTOP_FILE" ]; then
    skip I6 "no io.unom.punktfunk.host.desktop installed — KWin has nothing to match the host"
    info  "against, so zkde_screencast_unstable_v1 is never advertised however clean the capability"
    info  "state is. On the Deck this file is written by scripts/steamdeck/install.sh; packaged"
    info  "installs ship it. This is a MISSING PRECONDITION, not the capability regression."
  else
    local exec_ok=1 iface_ok=1
    [ -n "$HOST_BIN" ] && [ "$DESKTOP_EXEC" != "$HOST_BIN" ] && exec_ok=0
    grep -q '^X-KDE-Wayland-Interfaces=.*zkde_screencast_unstable_v1' "$DESKTOP_FILE" 2>/dev/null || iface_ok=0
    if [ "$exec_ok" = 1 ] && [ "$iface_ok" = 1 ]; then
      pass I6 ".desktop grants the screencast interface to $DESKTOP_EXEC"
    else
      fail I6 "$DESKTOP_FILE does not authorize this host binary"
      [ "$exec_ok" = 0 ] && info "Exec=$DESKTOP_EXEC but the host binary is $HOST_BIN — KWin caches the"
      [ "$exec_ok" = 0 ] && info "grant per EXECUTABLE PATH, so a build run from anywhere else is refused."
      [ "$iface_ok" = 0 ] && info "X-KDE-Wayland-Interfaces does not list zkde_screencast_unstable_v1."
    fi
  fi
}

leg_v1() {
  head2 "V1 — the 0.26.0-1 regression test (KDE desktop session)"
  local comp; comp="$(compositor_guess)"
  if [ "$comp" != kwin ]; then
    skip V1 "compositor is '$comp', not KWin — V1 is the KWin identification gate and cannot run here"
    return 0
  fi
  if [ -z "${WAYLAND_DISPLAY:-}" ]; then
    skip V1 "WAYLAND_DISPLAY is unset — run this from inside the KDE session, on glass"
    return 0
  fi
  if [ -z "$DESKTOP_FILE" ]; then
    skip V1 "precondition missing: no io.unom.punktfunk.host.desktop (run scripts/steamdeck/install.sh"
    info  "on the Deck, or install a package). Without it KWin cannot advertise the screencast"
    info  "global for any reason, which would mask the capability question this leg exists to ask."
    return 0
  fi
  if [ -z "$HOST_BIN" ] || [ ! -x "$HOST_BIN" ]; then
    skip V1 "no runnable host binary"; return 0
  fi
  # The scratch-path trap, stated before it can be mis-debugged: KWin caches its grant per
  # EXECUTABLE PATH against the .desktop's Exec=. Probing a different build is a refusal by design.
  if [ -n "$DESKTOP_EXEC" ] && [ "$DESKTOP_EXEC" != "$HOST_BIN" ]; then
    skip V1 "the installed .desktop authorizes '$DESKTOP_EXEC', but this leg would probe '$HOST_BIN'."
    info  "KWin would refuse the screencast global — correctly, because it identifies clients by"
    info  "executable path. Run the INSTALLED binary (or pass --host-bin '$DESKTOP_EXEC'); do not"
    info  "go debugging KWin."
    return 0
  fi

  # The static half must already be clean; V1 restates it because a PASS here has to mean the
  # capability state is right, not just that a probe happened to answer.
  local hc; hc="$(file_caps "$HOST_BIN")"
  if [ "$hc" != '' ]; then
    fail V1 "host binary carries '${hc:-?}' — hard fail. See I2."
    return 0
  fi

  # The instrument: probe-compositor connects as a KWin client from THIS binary and exits 0 only if
  # zkde_screencast_unstable_v1 was actually advertised to it. That is the identification path
  # end-to-end — the readlink, the .desktop match and the global — in one exit code.
  setup_logdir
  local plog="$LOG_DIR/v1-probe.log"
  "$HOST_BIN" probe-compositor >"$plog" 2>&1
  local prc=$?
  if [ "$prc" != 0 ]; then
    fail V1 "probe-compositor exited $prc — KWin did not advertise the screencast global"
    if LC_ALL=C grep -qF -- "$L_KWIN_DENIED" "$plog"; then
      info "$(LC_ALL=C grep -m1 -F -- "$L_KWIN_DENIED" "$plog" | cut -c1-200)"
    fi
    info "If the message mentions CapPrm, this IS the 0.26.0-1 regression. If it does not, the"
    info ".desktop is missing/stale or KWin has cached a grant for a different path (re-login)."
    return 0
  fi

  # …and a real capture+encode pass over that same grant, which is the "stream up" half.
  resolve_source
  run_spike v1-stream "$OPT_SECONDS"
  wait_spike "$OPT_SECONDS"
  local frames; frames="$(perf_windows "$SPIKE_LOG")"
  if [ "${frames:-0}" -lt 1 ]; then
    fail V1 "probe passed but the spike produced no encode windows — see $SPIKE_LOG"
    return 0
  fi
  pass V1 "host uncapped, /proc/<pid>/exe readable, zkde_screencast_unstable_v1 advertised, stream up"
  info "source=$SPIKE_SOURCE, $frames perf window(s). A client session on glass is the human half."
}

leg_v2() {
  head2 "V2 — grant: the worker reports an elevated global queue priority"
  if [ -z "$WORKER_BIN" ] || [ ! -x "$WORKER_BIN" ]; then
    skip V2 "no worker binary"; return 0
  fi
  local wc; wc="$(file_caps "$WORKER_BIN")"
  if [ "$wc" != "$WANT_WORKER_CAPS" ]; then
    skip V2 "the worker carries '${wc:-<none>}', not $WANT_WORKER_CAPS — every driver refuses every"
    info  "elevated class without it, so a REFUSED result here would say nothing about the driver."
    return 0
  fi
  [ -n "$HOST_BIN" ] && [ -x "$HOST_BIN" ] || { skip V2 "no runnable host binary"; return 0; }
  resolve_source
  setup_logdir
  # The intent is pinned rather than inherited: a host.env or shell with PYROWAVE_QUEUE_PRIORITY=off
  # in it would produce "no queue priority requested" and read as a failed grant.
  run_spike v2-grant "$OPT_SECONDS" "PUNKTFUNK_ENCODE_WORKER=$WORKER_BIN" "PYROWAVE_QUEUE_PRIORITY=realtime"
  wait_spike "$OPT_SECONDS"
  local log="$SPIKE_LOG"

  if why="$(spike_failure_reason "$log")"; then fail V2 "the run never got as far as an encoder: $why"; return 0; fi
  if ! assert_worker_arm "$log" V2; then
    fail V2 "the session did not run in the worker — see $log"; return 0
  fi
  if logs_have "$log" "$L_INERT_WORKER"; then
    fail V2 "every global priority class was REFUSED even though the worker is capped."
    info "Either the driver does not honour VK_KHR_global_priority here, or the capability did not"
    info "survive to the running process. Check: $(logs_first "$log" "$L_WORKER_READY" | cut -c1-160)"
    return 0
  fi
  local line; line="$(logs_first "$log" "$L_GRANTED")"
  if [ -z "$line" ]; then
    fail V2 "no grant line. Closest match: $(logs_first "$log" "$L_NOPRIO" | cut -c1-160)"
    info "PYROWAVE_QUEUE_PRIORITY=off would produce exactly that; unset it for this leg."
    return 0
  fi
  case "$line" in
    *priority=Realtime*) pass V2 "Ready.granted = Realtime  ($(printf '%s' "$line" | sed -n 's/.*device=\([^ ]*\).*/device=\1/p'))" ;;
    *priority=High*)     fail V2 "granted HIGH, not REALTIME — the plan's criterion is REALTIME."
                         info "A driver that caps at HIGH is a finding, not a kit failure; record it." ;;
    *) fail V2 "grant line carries no recognisable priority= field: $(printf '%s' "$line" | cut -c1-200)" ;;
  esac
  if logs_have "$log" "$L_WORKER_READY"; then
    info "worker: $(logs_first "$log" "$L_WORKER_READY" | sed 's/^.*  INFO //' | cut -c1-180)"
  fi
}

# The shared A/B engine. $1 tag, $2/$3 arm-A log and name, $4/$5 arm-B log and name, $6 gate µs
# (empty = report only). Returns 0 within the gate, 2 when the gate fires, 1 when an arm has no
# data.
ab_report() {
  local tag="$1" a_log="$2" a_name="$3" b_log="$4" b_name="$5" gate_us="${6:-}"
  local a b a50 a99 an b50 b99 bn
  a="$(perf_summary "$a_log")" || { fail "$tag" "$a_name: fewer than 3 usable perf windows — the"
    info "capture may be starved, or PUNKTFUNK_PERF did not arm. Log: $a_log"; return 1; }
  b="$(perf_summary "$b_log")" || { fail "$tag" "$b_name: fewer than 3 usable perf windows. Log: $b_log"; return 1; }
  read -r a50 a99 an <<<"$a"
  read -r b50 b99 bn <<<"$b"
  info "$(printf '%-28s p50 %6s ms   p99 %6s ms   (%s windows)' "$a_name" "$(ms "$a50")" "$(ms "$a99")" "$an")"
  info "$(printf '%-28s p50 %6s ms   p99 %6s ms   (%s windows)' "$b_name" "$(ms "$b50")" "$(ms "$b99")" "$bn")"
  local delta=$((b99 - a99))
  info "$(printf 'p99 delta (%s - %s): %s ms' "$b_name" "$a_name" "$(ms "$delta")")"
  [ -z "$gate_us" ] && return 0
  if [ "$delta" -le "$gate_us" ]; then return 0; fi
  return 2
}

leg_v3a() {
  head2 "V3a — IPC hop cost: in-process vs an UNCAPPED worker (R1's abandonment gate)"
  info "Both arms encode at DEFAULT GPU priority, so the only difference is the process boundary."
  info "The AU crosses via a memfd (pwrite/pread), not a socket copy, so this measures two small"
  info "JSON messages plus one memfd round trip per frame. Needs no capability and no KDE."
  info "⚠ --source synthetic CANNOT be used for this (or any) worker leg: a CPU-backed frame is"
  info "refused by the worker path on purpose (1080p BGRA is ~480 MB/s over the socket) and pins"
  info "the session in-process, so the 'worker' arm would silently be a second in-process arm."
  [ -n "$HOST_BIN" ] && [ -x "$HOST_BIN" ] || { skip V3a "no runnable host binary"; return 0; }
  if [ -z "$WORKER_BIN" ] || [ ! -x "$WORKER_BIN" ]; then skip V3a "no worker binary"; return 0; fi
  setup_logdir
  make_uncapped_copy
  case $? in
    0) : ;;
    2) skip V3a "cannot verify the working copy is uncapped (no getcap) — refusing to measure an arm"
       info "whose priority state is unknown"; return 0 ;;
    3) skip V3a "the working copy carried the capability across — refusing to call it the uncapped arm"; return 0 ;;
    4) skip V3a "the working copy is not executable where it was written (noexec ${TMPDIR:-/tmp}?) —"
       info "re-run with --log-dir pointing somewhere executable"; return 0 ;;
    *) skip V3a "could not copy $WORKER_BIN"; return 0 ;;
  esac
  resolve_source

  # Arm A: in-process, priority explicitly OFF so the two arms differ in nothing but the boundary.
  run_spike v3a-inline "$OPT_SECONDS" "PUNKTFUNK_ENCODE_WORKER=off" "PYROWAVE_QUEUE_PRIORITY=off"
  wait_spike "$OPT_SECONDS"
  local a_log="$SPIKE_LOG"
  # Arm B: the uncapped worker, same intent.
  run_spike v3a-worker "$OPT_SECONDS" "PUNKTFUNK_ENCODE_WORKER=$UNCAPPED_COPY" "PYROWAVE_QUEUE_PRIORITY=off"
  wait_spike "$OPT_SECONDS"
  local b_log="$SPIKE_LOG"

  if why="$(spike_failure_reason "$a_log")"; then fail V3a "arm A never ran: $why"; return 0; fi
  if why="$(spike_failure_reason "$b_log")"; then fail V3a "arm B never ran: $why"; return 0; fi
  assert_inline_arm "$a_log" V3a || { fail V3a "arm A is not the in-process arm — see $a_log"; return 0; }
  assert_worker_arm "$b_log" V3a || { fail V3a "arm B is not the worker arm — see $b_log"; return 0; }

  local gate_us; gate_us="$(LC_ALL=C awk -v m="$OPT_GATE_MS" 'BEGIN{ printf "%d", m*1000 }')"
  ab_report V3a "$a_log" "in-process (default prio)" "$b_log" "uncapped worker" "$gate_us"
  case $? in
    0) pass V3a "the IPC hop costs <= ${OPT_GATE_MS} ms at p99 — R1's abandonment gate does NOT fire" ;;
    2) fail V3a "THE PRE-REGISTERED ABANDONMENT GATE FIRED: the hop costs more than ${OPT_GATE_MS} ms"
       info "at p99. Per the plan, STOP and do the shm-ring AU return before any packaging ships." ;;
    *) : ;;  # ab_report already recorded the failure
  esac
}

leg_v3b() {
  head2 "V3b — lever benefit: in-process at default priority vs the CAPPED worker"
  info "PW1's baseline on .21 (RTX 5070 Ti, GRID 2 at 54-87% GPU, 1080p): refused p99 ~6.4 ms,"
  info "REALTIME p99 ~4.4 ms. This leg needs a GPU-BOUND LOAD to mean anything — an idle GPU has"
  info "nothing to preempt and both arms will look identical. Start the game first."
  [ -n "$HOST_BIN" ] && [ -x "$HOST_BIN" ] || { skip V3b "no runnable host binary"; return 0; }
  if [ -z "$WORKER_BIN" ] || [ ! -x "$WORKER_BIN" ]; then skip V3b "no worker binary"; return 0; fi
  local wc; wc="$(file_caps "$WORKER_BIN")"
  if [ "$wc" != "$WANT_WORKER_CAPS" ]; then
    skip V3b "the worker is not capped ($wc) — there is no lever to measure. See I3."; return 0
  fi
  setup_logdir; resolve_source

  # Arm A is exactly PW1's "refused" arm: in-process, asking for priority and being refused (the
  # host binary is uncapped by construction, so the ladder walks REALTIME -> HIGH -> default).
  run_spike v3b-inline "$OPT_SECONDS" "PUNKTFUNK_ENCODE_WORKER=off" "PYROWAVE_QUEUE_PRIORITY=realtime"
  wait_spike "$OPT_SECONDS"
  local a_log="$SPIKE_LOG"
  run_spike v3b-worker "$OPT_SECONDS" "PUNKTFUNK_ENCODE_WORKER=$WORKER_BIN" "PYROWAVE_QUEUE_PRIORITY=realtime"
  wait_spike "$OPT_SECONDS"
  local b_log="$SPIKE_LOG"

  if why="$(spike_failure_reason "$a_log")"; then fail V3b "arm A never ran: $why"; return 0; fi
  if why="$(spike_failure_reason "$b_log")"; then fail V3b "arm B never ran: $why"; return 0; fi
  assert_inline_arm "$a_log" V3b || { fail V3b "arm A is not the in-process arm — see $a_log"; return 0; }
  assert_worker_arm "$b_log" V3b || { fail V3b "arm B is not the worker arm — see $b_log"; return 0; }
  if ! logs_have "$b_log" "$L_GRANTED"; then
    fail V3b "arm B did not get a grant — this would be an A/B of two default-priority arms"; return 0
  fi
  if ! logs_have "$a_log" "$L_INERT_HOST"; then
    info "note: arm A did not log the in-process INERT warn; it may already be at default priority"
    info "for another reason (PYROWAVE_QUEUE_PRIORITY=off in this environment?)."
  fi

  ab_report V3b "$a_log" "in-process, refused" "$b_log" "capped worker, granted"
  case $? in
    0) local a99 b99
       a99="$(perf_summary "$a_log" | awk '{print $2}')"; b99="$(perf_summary "$b_log" | awk '{print $2}')"
       if [ "${b99:-0}" -lt "${a99:-0}" ]; then
         pass V3b "the capped worker's p99 is BELOW the refused arm's — the lever pays for the hop"
       else
         fail V3b "the capped worker's p99 is NOT below the refused arm's. On an idle GPU that is"
         info "expected and the leg is meaningless — re-run under a real GPU-bound load."
       fi ;;
    *) : ;;
  esac
}

leg_v4() {
  head2 "V4 — the fallback ladder: no rung may kill a negotiated session"
  [ -n "$HOST_BIN" ] && [ -x "$HOST_BIN" ] || { skip V4 "no runnable host binary"; return 0; }
  setup_logdir; resolve_source
  local short=$(( OPT_SECONDS < 20 ? OPT_SECONDS : 20 ))

  # --- rung: the binary is not there. Read-only — a path that does not exist, never a deletion.
  # An operator-set PUNKTFUNK_ENCODE_WORKER is deliberately NOT existence-checked (a named path is
  # entitled to a failure that names it back), so this lands on the SPAWN rung ("did not come up")
  # rather than the not-found one; both are accepted here because both are the same guarantee.
  run_spike v4-missing "$short" "PUNKTFUNK_ENCODE_WORKER=$LOG_DIR/definitely-not-here"
  wait_spike "$short"
  local log="$SPIKE_LOG"
  if logs_have "$log" "$L_NOTUP" || logs_have "$log" "$L_NOTFOUND"; then
    if [ "$(perf_windows "$log")" -ge 1 ]; then
      pass V4.a "a missing worker: one warn, encoding in-process, stream up"
    else
      fail V4.a "the warn fired but nothing encoded — see $log"
    fi
  else
    fail V4.a "no fallback warn for a worker path that does not exist — see $log"
  fi

  # --- rung: PUNKTFUNK_ENCODE_WORKER=off (the documented escape hatch).
  run_spike v4-off "$short" "PUNKTFUNK_ENCODE_WORKER=off"
  wait_spike "$short"
  log="$SPIKE_LOG"
  if logs_have "$log" "$L_OFF" && [ "$(perf_windows "$log")" -ge 1 ]; then
    pass V4.b "PUNKTFUNK_ENCODE_WORKER=off: one info line, encoding in-process, stream up"
  else
    fail V4.b "the off escape hatch did not behave — see $log"
  fi

  # --- rung: a worker that starts and immediately dies (spawn ok, handshake never completes).
  if [ -x /bin/false ]; then
    run_spike v4-false "$short" "PUNKTFUNK_ENCODE_WORKER=/bin/false"
    wait_spike "$short"
    log="$SPIKE_LOG"
    if logs_have "$log" "$L_NOTUP" && [ "$(perf_windows "$log")" -ge 1 ]; then
      pass V4.c "a worker that dies at handshake: one warn with error=, in-process, stream up"
      info "$(logs_first "$log" "$L_NOTUP" | sed -n 's/.*\(error=[^ ]*\).*/\1/p' | cut -c1-120)"
    else
      fail V4.c "a /bin/false worker did not produce the handshake-failure rung — see $log"
    fi
  else
    skip V4.c "no /bin/false to stand in for a worker that dies at handshake"
  fi

  # --- rung: the capability is absent → INERT warn, default priority, stream still up.
  # Uses the uncapped COPY, so nothing on the box is stripped and no sudo is needed.
  if [ -n "$WORKER_BIN" ] && [ -x "$WORKER_BIN" ] && [ "$(file_caps "$WORKER_BIN")" = "$WANT_WORKER_CAPS" ]; then
    make_uncapped_copy
    if [ $? = 0 ]; then
      run_spike v4-inert "$short" "PUNKTFUNK_ENCODE_WORKER=$UNCAPPED_COPY" "PYROWAVE_QUEUE_PRIORITY=realtime"
      wait_spike "$short"
      log="$SPIKE_LOG"
      if logs_have "$log" "$L_INERT" && logs_have "$log" "$L_INERT_WORKER" \
         && [ "$(perf_windows "$log")" -ge 1 ]; then
        pass V4.d "an uncapped worker: the INERT warn names the WORKER binary, stream still up"
        if logs_have "$log" "$L_INERT_HOST"; then
          fail V4.d2 "the INERT warn ALSO fired with the in-process wording — the in-process"
          info "encoder ran beside the worker. It must not double-fire while the worker is active."
        fi
      else
        fail V4.d "an uncapped worker did not produce the INERT warn + a live stream — see $log"
      fi
    else
      skip V4.d "could not produce a verified-uncapped worker copy"
    fi
  else
    skip V4.d "the worker is not capped here, so the INERT arm is already the normal state (see I3)"
  fi

  # --- rung: kill -9 mid-session. This one genuinely kills a process.
  if [ "$OPT_ALLOW_MUTATE" != 1 ]; then
    skip V4.e "kill -9 of a live worker needs --allow-mutate (it is the one step here that changes"
    info "the state of the box). It kills ONLY a worker that is a child of the spike this script"
    info "started — never a worker belonging to a real session."
  elif [ -z "$WORKER_BIN" ] || [ ! -x "$WORKER_BIN" ]; then
    skip V4.e "no worker binary to kill"
  else
    local kwall=$(( short * 3 ))
    run_spike v4-kill "$kwall" "PUNKTFUNK_ENCODE_WORKER=$WORKER_BIN" "PYROWAVE_QUEUE_PRIORITY=realtime"
    local spike_pid="$SPIKE_PID" wlog="$SPIKE_LOG" wpid='' t=0
    while [ "$t" -lt 30 ] && [ -z "$wpid" ]; do
      wpid="$(worker_pids_of "$spike_pid" | head -1)"
      [ -n "$wpid" ] && break
      sleep 1; t=$((t+1))
    done
    if [ -z "$wpid" ]; then
      stop_spike
      skip V4.e "no worker process appeared as a child of the spike within 30 s — nothing to kill"
      info "(the spike may have fallen back before spawning; check $wlog)"
    else
      sleep 5   # let it encode for a while first, so the death is genuinely mid-session
      kill -9 "$wpid" 2>/dev/null
      info "killed worker pid $wpid (child of spike pid $spike_pid)"
      sleep 8
      local still=1; kill -0 "$spike_pid" 2>/dev/null || still=0
      stop_spike
      local total after
      total="$(perf_windows "$wlog")"; after="$(perf_windows_after "$wlog" "$L_DIED")"
      if ! logs_have "$wlog" "$L_DIED"; then
        # THE VEHICLE, NOT THE LADDER. L_DIED/L_RESPAWNED are emitted by `RemotePyroWave::reset`,
        # and the only caller of `Encoder::reset` is the real session's `reset_stalled_encoder`
        # loop (encode_recovery.rs). `spike` is a dev tool with no recovery loop at all — it does
        # `encoder.submit(&frame).context("encoder submit")?` and exits — so a worker killed under
        # the spike can never reach reset, and demanding L_DIED here is a FALSE NEGATIVE that
        # reports a shipping blocker for a rung the product implements correctly.
        #
        # What the spike CAN prove, and what is asserted instead: the worker's death is surfaced as
        # an ATTRIBUTABLE proxy error naming the worker — not a hang, not an unexplained failure,
        # and never the host process dying with it. Anything else still fails.
        if { logs_have "$wlog" "$L_PEERGONE" || logs_have "$wlog" "$L_PEERPIPE"; } \
           && [ "${total:-0}" -ge 1 ]; then
          pass V4.e "kill -9 under the spike: the death surfaces as an attributable worker-IPC error"
          info "after $total encode window(s), and the host process did not die with it."
          info "⚠ THE RESPAWN RUNG IS NOT OBSERVABLE HERE. \`spike\` has no encoder-recovery loop"
          info "(submit errors propagate and it exits), so \`Encoder::reset\` — the only emitter of"
          info "'$L_DIED' — is never called. Verify that half against a REAL session:"
          info "    1. connect a client, then:  pkill -f punktfunk-encode-worker"
          info "    2. journalctl --user -u punktfunk-host -b | grep -E 'died mid-session|respawned'"
          info "  Confirmed on glass 2026-08-10 (home-nobara-1, KDE, RTX 5070 Ti): stream never"
          info "  dropped, 'respawned the encode worker after a mid-session death"
          info "  priority=Granted(Realtime)', 'encoder rebuilt in place, forcing an IDR reset=1'."
        else
          fail V4.e "killing pid $wpid produced neither '$L_DIED' nor an attributable worker-IPC"
          info "error — the death was not surfaced at all. See $wlog"
        fi
      elif [ "${after:-0}" -lt 1 ]; then
        fail V4.e "the death was logged but NOTHING encoded afterwards ($total window(s), all before"
        info "the kill) — the session did not actually survive. See $wlog"
      elif logs_have "$wlog" "$L_RESPAWNED"; then
        pass V4.e "kill -9 mid-session: one respawn, then $after more encode window(s) (host alive=$still)"
      elif logs_have "$wlog" "$L_NORESPAWN" || logs_have "$wlog" "$L_FALLBACK"; then
        pass V4.e "kill -9 mid-session: pinned in-process, $after more encode window(s), no dead stream"
      else
        fail V4.e "the death was logged and encoding continued, but neither a respawn nor an"
        info "in-process pin was logged — the ladder took an undeclared path. See $wlog"
      fi
    fi
  fi
}

# Median of a space-separated integer list. Used by v5 instead of a raw min/max spread: an fd count
# legitimately jitters by one (a dmabuf fd in flight at the sampling instant), and a median over a
# third of the run is immune to that while still moving under a real leak.
_median() {
  local -a s
  # shellcheck disable=SC2206  # deliberate word-splitting: the argument IS a list of integers
  s=( $(printf '%s\n' $1 | sort -n) )
  local n=${#s[@]}
  [ "$n" -eq 0 ] && return 1
  printf '%s' "${s[$(( n / 2 ))]}"
}
# First / last third of a space-separated list (at least one element each).
_first_third() { set -- $1; local k=$(( ($# + 2) / 3 )); [ "$k" -lt 1 ] && k=1; printf '%s' "${*:1:$k}"; }
_last_third()  { set -- $1; local k=$(( ($# + 2) / 3 )); [ "$k" -lt 1 ] && k=1; printf '%s' "${*: -$k}"; }

leg_v5() {
  head2 "V5 — fd hygiene over a long session"
  info "The fd-identity cache passes a dmabuf's fds only on FIRST sight of its buffer key, and the"
  info "PipeWire pool recycles a small set, so steady state passes zero fds. A rising count is R2."
  [ -n "$HOST_BIN" ] && [ -x "$HOST_BIN" ] || { skip V5 "no runnable host binary"; return 0; }
  if [ -z "$WORKER_BIN" ] || [ ! -x "$WORKER_BIN" ]; then skip V5 "no worker binary"; return 0; fi
  setup_logdir; resolve_source
  local wall=$(( OPT_MINUTES * 60 ))
  run_spike v5-fd "$wall" "PUNKTFUNK_ENCODE_WORKER=$WORKER_BIN"
  local spike_pid="$SPIKE_PID" log="$SPIKE_LOG"
  if [ -z "$spike_pid" ]; then skip V5 "the spike did not stay up"; return 0; fi

  local wpid='' t=0
  while [ "$t" -lt 30 ] && [ -z "$wpid" ]; do
    wpid="$(worker_pids_of "$spike_pid" | head -1)"
    [ -n "$wpid" ] && break
    sleep 1; t=$((t+1))
  done
  if [ -z "$wpid" ]; then
    stop_spike
    skip V5 "no worker child appeared — the session fell back before spawning (see $log)"
    return 0
  fi
  if [ -z "$(fd_count "$wpid")" ]; then
    stop_spike
    skip V5 "cannot read /proc/$wpid/fd. A capability-carrying process is normally opaque, which is"
    info "why the worker sets PR_SET_DUMPABLE(1) at startup — if this fails, that did not take."
    info "Re-run this leg as root to measure anyway, and treat the opacity itself as a finding."
    return 0
  fi

  # The file capability, cross-checked at RUNTIME on the live worker. /proc/<pid>/status is
  # world-readable and carries CapPrm unconditionally, so this reads across the capability boundary
  # that refuses a /proc/<pid>/exe readlink. (That refusal is EXPECTED here and is not a problem:
  # nothing resolves the worker's exe — it speaks one socketpair to its parent and is not a KWin
  # client. It is only the HOST that must stay readable, which is I5.)
  local wprm; wprm="$(proc_capprm "$wpid")"
  if [ -z "$wprm" ]; then
    info "worker pid $wpid: no CapPrm field to read"
  elif capprm_has_bit "$wprm" "$CAP_SYS_NICE_BIT"; then
    info "worker pid $wpid: CapPrm=$wprm — CAP_SYS_NICE is live in the running process"
  else
    info "worker pid $wpid: CapPrm=$wprm — CAP_SYS_NICE is NOT in the running process's permitted"
    info "set, so the lever is inert however the file looks. A rebuild is a new inode and drops the"
    info "capability; re-run the installer or setcap the worker again."
  fi

  # Warm up, then sample. The first minute covers pool negotiation and the first sight of every
  # recycled buffer — the identity cache is only expected to be quiet AFTER that.
  local warm=60; [ "$wall" -gt 180 ] || warm=$(( wall / 3 ))
  info "warm-up ${warm}s, then sampling every 15 s for $(( wall - warm ))s (host pid $spike_pid, worker pid $wpid)"
  sleep "$warm"
  local hmin='' hmax='' wmin='' wmax='' hlast='' wlast='' n=0 elapsed="$warm"
  local hseries='' wseries=''
  while [ "$elapsed" -lt "$wall" ]; do
    kill -0 "$spike_pid" 2>/dev/null || break
    kill -0 "$wpid" 2>/dev/null || { info "the worker exited at t=${elapsed}s"; break; }
    local h w
    h="$(fd_count "$spike_pid")"; w="$(fd_count "$wpid")"
    if [ -n "$h" ] && [ -n "$w" ]; then
      n=$((n+1)); hlast="$h"; wlast="$w"
      hseries="$hseries $h"; wseries="$wseries $w"
      [ -z "$hmin" ] && { hmin="$h"; hmax="$h"; wmin="$w"; wmax="$w"; }
      [ "$h" -lt "$hmin" ] && hmin="$h"; [ "$h" -gt "$hmax" ] && hmax="$h"
      [ "$w" -lt "$wmin" ] && wmin="$w"; [ "$w" -gt "$wmax" ] && wmax="$w"
      printf '        t=%4ss  host fd=%-5s worker fd=%s\n' "$elapsed" "$h" "$w"
    fi
    sleep 15; elapsed=$((elapsed+15))
  done
  stop_spike

  if [ "$n" -lt 4 ]; then
    skip V5 "only $n samples — the session did not stay up long enough to say anything"
    return 0
  fi
  # A LEAK IS A TREND, NOT A SPREAD. A `max - min` verdict cannot tell a leak from jitter: the
  # worker's count legitimately moves by one when a dmabuf fd happens to be in flight at the
  # sampling instant, so `max - min` would be permanently 1 and the leg would fail forever on a
  # healthy box.
  #
  # Median-of-thirds is strictly MORE sensitive to the thing R2 is about: a steady leak moves the
  # trend just as much as it moves the spread, while bounded jitter moves only the spread. The
  # warm-up window above already covers the one-off first-sight-of-each-buffer cost, so a plateau
  # inside it is by design not a leak.
  local hfirst hlastm wfirst wlastm
  hfirst="$(_median "$(_first_third "$hseries")")"; hlastm="$(_median "$(_last_third "$hseries")")"
  wfirst="$(_median "$(_first_third "$wseries")")"; wlastm="$(_median "$(_last_third "$wseries")")"
  local hg=$(( hlastm - hfirst )) wg=$(( wlastm - wfirst ))
  info "host   fd min=$hmin max=$hmax last=$hlast  median first/last third=$hfirst/$hlastm  trend=$hg"
  info "worker fd min=$wmin max=$wmax last=$wlast  median first/last third=$wfirst/$wlastm  trend=$wg"
  if [ "$(( hmax - hmin ))" -gt 0 ] || [ "$(( wmax - wmin ))" -gt 0 ]; then
    info "(spread host $(( hmax - hmin )) / worker $(( wmax - wmin )) with a flat trend is in-flight"
    info "jitter — one dmabuf fd caught mid-handover — not accumulation)"
  fi
  if [ "$hg" -le "$OPT_FD_TOLERANCE" ] && [ "$wg" -le "$OPT_FD_TOLERANCE" ]; then
    pass V5 "fd counts show no upward trend across ${n} samples (tolerance $OPT_FD_TOLERANCE)"
  else
    fail V5 "fd count TRENDS up (host +$hg, worker +$wg) beyond the tolerance $OPT_FD_TOLERANCE — R2."
    info "Compare \`ls -l /proc/$wpid/fd\` at both ends of a run to see WHAT is accumulating."
  fi
}

# Red-teams this kit's own readers against synthetic logs, so the assertions are known to be able to
# FAIL before anyone trusts a green run on a box. Pure bash + sed/awk: runs anywhere, including a
# laptop with no GPU, no getcap and no Linux.
leg_selftest() {
  local fails=0 d; d="$(mktemp -d "${TMPDIR:-/tmp}/pf-wp3-st.XXXXXX")"
  _t() {  # _t <label> <want:ok|no> <command...>
    local label="$1" want="$2"; shift 2
    local got=ok; "$@" >/dev/null 2>&1 || got=no
    if [ "$got" = "$want" ]; then printf '  ok    %s\n' "$label"
    else printf '  FAIL  %s -> %s (wanted %s)\n' "$label" "$got" "$want"; fails=$((fails+1)); fi
  }
  _eq() {  # _eq <label> <got> <want>
    if [ "$2" = "$3" ]; then printf '  ok    %-46s -> %s\n' "$1" "${2:-<empty>}"
    else printf '  FAIL  %-46s -> %s (wanted %s)\n' "$1" "${2:-<empty>}" "$3"; fails=$((fails+1)); fi
  }

  printf '\nreused canonicalizer (scripts/ci/assert-cap-matrix.sh):\n'
  _eq 'WANT_WORKER_CAPS came across the source' "${WANT_WORKER_CAPS:-}" 'cap_sys_nice=ep'
  _eq 'caps_norm libcap >= 2.36'   "$(caps_norm 'cap_sys_nice=ep')"   'cap_sys_nice=ep'
  _eq 'caps_norm older libcap'     "$(caps_norm '= cap_sys_nice+ep')" 'cap_sys_nice=ep'

  printf 'runtime capability masks:\n'
  _t 'CapPrm all zeroes is "no capability"'      ok capprm_is_zero '0000000000000000'
  _t 'CapPrm with CAP_SYS_NICE is not zero'      no capprm_is_zero '0000000000800000'
  _t 'an empty CapPrm is NOT quietly zero'       no capprm_is_zero ''
  _t 'bit 23 set in 0000000000800000'            ok capprm_has_bit '0000000000800000' 23
  _t 'bit 23 clear in 0000000000000000'          no capprm_has_bit '0000000000000000' 23
  _t 'bit 23 clear in a full root mask minus it' no capprm_has_bit '000001ffff7fffff' 23
  _t 'garbage is an error, not a verdict'        no capprm_has_bit 'zzzz' 23

  printf 'perf parsing (warm-up window dropped, median p99 across the rest):\n'
  local log="$d/perf.log"
  {
    printf '%s\n' "  INFO x: frames=30 mean_us=9000 p50_us=9000 p99_us=99000 max_us=99000 depth=1 $L_PERF (CSC…)"
    printf '%s\n' "  INFO x: frames=30 mean_us=2600 p50_us=2600 p99_us=6000 max_us=9500 depth=1 $L_PERF (CSC…)"
    printf '%s\n' "  INFO x: frames=30 mean_us=2600 p50_us=2800 p99_us=6400 max_us=9500 depth=1 $L_PERF (CSC…)"
    printf '%s\n' "  INFO x: frames=30 mean_us=2600 p50_us=3000 p99_us=7000 max_us=9500 depth=1 $L_PERF (CSC…)"
  } > "$log"
  _eq 'median p50/p99/windows, warm-up excluded' "$(perf_summary "$log")" '2800 6400 3'
  _eq 'perf window count'                        "$(perf_windows "$log")" '4'

  # A real log is not this clean, and the difference is not cosmetic. `tracing` wraps every field
  # NAME in SGR escapes, so the bytes are `p99_us\e[0m\e[2m=\e[0m6400` and `p99_us=\([0-9]*\)` never
  # matches. The plain fixtures above alone would pass while the kit could not read a real log.
  # Same numbers as the plain fixture, same expected answer — the ONLY difference is the escapes,
  # so a regression here can only mean the stripping broke.
  local alog="$d/perf-ansi.log" E=$'\033'
  # One field at a time, deliberately: one printf for the whole line is easy to get out of step
  # (placeholders against arguments), and the fixture would fail for its own reasons rather than
  # the code's.
  _ansi_kv()   { printf '%s[3m%s%s[0m%s[2m=%s[0m%s' "$E" "$1" "$E" "$E" "$E" "$2"; }
  _ansi_perf() { # $1 p50_us, $2 p99_us — the exact field shape `tracing` emits
    printf '%s[2m2026-08-09T14:25:07Z%s[0m %s[32m INFO%s[0m %s ' "$E" "$E" "$E" "$E" "$L_PERF"
    _ansi_kv frames 120 ; printf ' '
    _ansi_kv p50_us "$1"; printf ' '
    _ansi_kv p99_us "$2"; printf ' '
    _ansi_kv depth 1    ; printf '\n'
  }
  { _ansi_perf 9000 99000; _ansi_perf 2600 6000; _ansi_perf 2800 6400; _ansi_perf 3000 7000; } > "$alog"
  _eq 'ANSI-wrapped fields parse identically'    "$(perf_summary "$alog")" '2800 6400 3'
  _eq 'ANSI window count'                        "$(perf_windows "$alog")" '4'
  _eq 'a missing log counts 0, never empty'      "$(perf_windows "$d/nope.log")" '0'
  # "the stream survived the kill" is windows AFTER the death line, not windows anywhere.
  local klog="$d/kill.log"
  { head -2 "$log"; printf '%s\n' "  WARN x: pyrowave: $L_DIED"; tail -2 "$log"; } > "$klog"
  _eq 'perf windows after a marker'              "$(perf_windows_after "$klog" "$L_DIED")" '2'
  _eq 'a dead-after-kill run scores 0'           "$(perf_windows_after "$log" "$L_DIED")" '0'
  : > "$d/thin.log"
  _t  'too few windows is a refusal, not a 0 ms' no perf_summary "$d/thin.log"
  _eq 'microseconds render as ms'                "$(ms 6400)" '6.40'

  printf 'arm identity (the false-PASS this kit exists to refuse):\n'
  local wl="$d/worker.log" il="$d/inline.log" cl="$d/cpu.log"
  {
    printf '%s\n' "  INFO x: $L_CAPTURE dmabuf-passthrough → pyrowave"
    printf '%s\n' "  INFO x: priority=Realtime device=NVIDIA worker=/usr/bin/punktfunk-encode-worker pyrowave: $L_GRANTED (…)"
  } > "$wl"
  printf '%s\n' "  INFO x: $L_OFF — $L_FALLBACK" > "$il"
  # A CPU capture arm: the worker takes the session, then leaves it on the first frame. Every
  # "worker" line is present, so a naive reader calls this the worker arm and A/Bs inline vs inline.
  {
    printf '%s\n' "  INFO x: $L_CAPTURE cpu → pyrowave"
    printf '%s\n' "  INFO x: priority=Realtime pyrowave: $L_GRANTED (…)"
    printf '%s\n' "  WARN x: $L_LEAVING — $L_FALLBACK for the rest of this session"
  } > "$cl"
  _t 'a real worker log IS the worker arm'          ok assert_worker_arm "$wl" st
  _t 'a real worker log is NOT the inline arm'      no assert_inline_arm "$wl" st
  _t 'an inline log IS the inline arm'              ok assert_inline_arm "$il" st
  _t 'an inline log is NOT the worker arm'          no assert_worker_arm "$il" st
  _t 'a CPU-capture run is REFUSED as the worker'   no assert_worker_arm "$cl" st

  printf 'log matchers:\n'
  _t 'the shared fallback clause is found'      ok logs_have "$il" "$L_FALLBACK"
  _t 'the INERT wordings are distinguishable'   no logs_have "$wl" "$L_INERT_WORKER"

  # v5's leak verdict. The series below are the shapes that matter: a bounded oscillation (which a
  # max-min verdict would call a leak) and a real leak. A trend reader has to get BOTH right or it
  # is not worth having.
  printf 'v5 fd trend (a leak is a trend, not a spread):\n'
  local jitter='54 54 54 54 55 55 54 54 55 54 54 54 55 54 54 54 54 54'
  local leak='54 54 55 55 56 57 57 58 59 60 61 61 62 63 64 65 66 67'
  local flat='31 31 31 31 31 31 31 31 31 31 31 31'
  local plateau='54 54 54 54 54 54 58 58 58 58 58 58'
  _eq 'median of a sorted-odd list'              "$(_median '3 1 2')"            '2'
  _eq 'median ignores a single outlier'          "$(_median '54 54 54 99 54')"   '54'
  _eq 'first third of 18 samples'                "$(_first_third "$jitter")"     '54 54 54 54 55 55'
  _eq 'last third of 18 samples'                 "$(_last_third "$jitter")"      '55 54 54 54 54 54'
  _eq 'MEASURED oscillation trends to zero'      \
      "$(( $(_median "$(_last_third "$jitter")") - $(_median "$(_first_third "$jitter")") ))" '0'
  _eq 'a real leak still trends up'              \
      "$(( $(_median "$(_last_third "$leak")") - $(_median "$(_first_third "$leak")") ))"    '10'
  _eq 'a flat series trends to zero'             \
      "$(( $(_median "$(_last_third "$flat")") - $(_median "$(_first_third "$flat")") ))"     '0'
  _eq 'a step that never comes back is caught'   \
      "$(( $(_median "$(_last_third "$plateau")") - $(_median "$(_first_third "$plateau")") ))" '4'

  rm -rf "$d"
  if [ "$fails" != 0 ]; then
    printf '\n  %sself-test: %d case(s) wrong — do not trust a run of this kit%s\n' "$C_BAD" "$fails" "$C_OFF"
    return 1
  fi
  printf '\n  %sself-test: every reader behaved as specified%s\n' "$C_OK" "$C_OFF"
  return 0
}

leg_recipe() {
  cat <<'EOF'

RECIPE — what a human has to do, per box
========================================

Everything below runs ON the box, from a shell inside the graphical session (the KDE legs need
WAYLAND_DISPLAY set to the KDE socket — an ssh shell has neither that nor XDG_RUNTIME_DIR unless
you export them).

  export XDG_RUNTIME_DIR=/run/user/$(id -u)
  export WAYLAND_DISPLAY=wayland-0        # check `ls $XDG_RUNTIME_DIR/wayland-*`

⚠ USE THE INSTALLED BINARY. KWin caches its per-executable grant against the path in the installed
  .desktop's Exec=. A build run from a scratch directory is a different path and KWin refuses it —
  that is identification working. This kit resolves the host from the .desktop for exactly that
  reason; only override with --host-bin if you know the installed path.

--- every box, first ------------------------------------------------------------------------------
  scripts/validate-encode-worker.sh                    # pure inspection, changes nothing
This is also the fastest way to see whether the worker is installed and capped at all.

--- Steam Deck (deck@192.168.1.253), DESKTOP mode -- V1 + V2 -------------------------------------
The Deck is the RADV half of V2 and the KWin×RADV half of V1. /usr is read-only (SteamOS); the
install is home-scoped under ~/punktfunk, and file capabilities do NOT survive a rebuild (a rebuild
is a new inode).

  1. PRIVILEGED, run by a human — sudo on the Deck needs the `deck` password, it is NOT passwordless.
     This also writes ~/.local/share/applications/io.unom.punktfunk.host.desktop, which V1 needs and
     which is NOT currently present on that box:

         cd ~/punktfunk && scripts/steamdeck/install.sh

     Then LOG OUT AND BACK IN. KWin reads the .desktop when the client first connects and caches the
     grant; an already-running session will not pick up a newly written file.

  2. Verify by hand, no root needed:
         getcap ~/punktfunk/target-steamos/release/punktfunk-host            # MUST print nothing
         getcap ~/punktfunk/target-steamos/release/punktfunk-encode-worker   # cap_sys_nice=ep

  3. Unprivileged, from a Desktop-mode terminal:
         scripts/validate-encode-worker.sh inspect v1 v2 v4

--- .21 CachyOS KDE / RTX 5070 Ti -- the A/B home -------------------------------------------------
PW1's baselines live here and nowhere else, so V3b is only comparable to them on this box. It needs
a GPU-BOUND LOAD: start the game (PW1 used the GRID 2 benchmark loop, steam:44350, at 54-87% GPU)
and leave it running for both arms.

         scripts/validate-encode-worker.sh inspect v1 v2 v3a v3b v4 v5

--- .25 Ubuntu 26.04 (enricobuehler@192.168.1.25) -- V3a/V4/V5 + the deb leg ----------------------
No KDE at all (sway only), so V1 cannot run there and will SKIP with that reason. V3a needs neither
a capability nor KDE, which is the point of splitting it out.

         scripts/validate-encode-worker.sh inspect v3a v4 v5 --source portal

--- V4.e's second half: the respawn rung needs a REAL session -------------------------------------
v4.e kills the worker under `spike`, and `spike` has no encoder-recovery loop — submit errors
propagate and it exits. `Encoder::reset` is therefore never called, and reset is the ONLY emitter of
"the encode worker died mid-session" / "respawned the encode worker". So the leg asserts the half it
can see (the death is surfaced as an attributable worker-IPC error, host still alive) and leaves the
respawn to a real session. To close it, with a client connected and streaming:

         pkill -f punktfunk-encode-worker
         journalctl --user -u punktfunk-host -b | grep -E 'died mid-session|respawned|rebuilt in place'

Expect the stream never to drop, plus "respawned the encode worker after a mid-session death
priority=Granted(Realtime)" and "encoder submit failed — encoder rebuilt in place, forcing an IDR
reset=1 max=5". Confirmed on home-nobara-1 (KDE, RTX 5070 Ti) 2026-08-10.

--- the one step this script will never do --------------------------------------------------------
It never calls setcap. If a leg says the worker is uncapped and you want the lever:

         sudo setcap 'cap_sys_nice=ep' /path/to/punktfunk-encode-worker

NEVER setcap punktfunk-host. Not "temporarily", not "to test". A capability on the host makes it
unidentifiable to KWin and takes out desktop streaming on every KDE box — that is 0.26.0-1, and I2
above hard-fails on it. To undo one:  sudo setcap -r /path/to/punktfunk-host

--- reading the logs afterwards -------------------------------------------------------------------
Every fallback rung ends with the same clause, so one grep finds them all whichever rung fired:

         journalctl --user -u punktfunk-host -b | grep -F 'encoding in-process at default GPU priority'

⚠ The worker's PUNKTFUNK_PERF p50/p99 summary is written by the WORKER process to inherited stderr.
  It reaches journalctl. It does NOT reach the web console's Logs tab — that ring is a tracing layer
  inside the host process and the worker is not in it. Read the journal, not the console.
EOF
}

# ==================================================================================================
# main
# ==================================================================================================

case " ${LEGS[*]} " in *" recipe "*) leg_recipe; exit 0 ;; esac
# Runs anywhere, deliberately: the readers have to be known-good before a box run is trusted, and
# the person writing them may not have a Linux box in front of them.
case " ${LEGS[*]} " in *" selftest "*) leg_selftest; exit $? ;; esac

if [ "$(uname -s)" != Linux ]; then
  echo "FATAL: this kit validates a Linux-only worker; run it on the box under test." >&2
  exit 3
fi

resolve_binaries

printf '%s — punktfunk-encode-worker on-glass validation (%s)\n' "$(hostname 2>/dev/null || echo box)" "$VERSION"
printf 'kernel %s · compositor %s · uid %s%s\n' \
  "$(uname -r)" "$(compositor_guess)" "$(id -u)" \
  "$([ -n "${WAYLAND_DISPLAY:-}" ] && printf ' · WAYLAND_DISPLAY=%s' "$WAYLAND_DISPLAY")"
printf 'host   %s%s\n' "${HOST_BIN:-<not found>}" "$([ -n "$HOST_BIN_FROM" ] && printf '   (%s)' "$HOST_BIN_FROM")"
printf 'worker %s%s\n' "${WORKER_BIN:-<not found>}" "$([ -n "$WORKER_BIN_FROM" ] && printf '   (%s)' "$WORKER_BIN_FROM")"
[ -n "$DESKTOP_FILE" ] && printf 'desktop %s (Exec=%s)\n' "$DESKTOP_FILE" "$DESKTOP_EXEC"

RUN=()
for leg in "${LEGS[@]}"; do
  case "$leg" in
    auto) RUN+=(inspect v1 v2 v3a v4) ;;
    all)  RUN+=(inspect v1 v2 v3a v3b v4 v5) ;;
    *)    RUN+=("$leg") ;;
  esac
done

DONE=' '
for leg in "${RUN[@]}"; do
  case "$DONE" in *" $leg "*) continue ;; esac
  DONE="$DONE$leg "
  case "$leg" in
    inspect) leg_inspect ;;
    v1) leg_v1 ;;
    v2) leg_v2 ;;
    v3a) leg_v3a ;;
    v3b) leg_v3b ;;
    v4) leg_v4 ;;
    v5) leg_v5 ;;
  esac
done

head2 "SUMMARY"
if [ "${#RESULTS[@]}" -gt 0 ]; then
  for r in "${RESULTS[@]}"; do
    IFS='|' read -r verdict tag msg <<<"$r"
    printf '  %-4s %-6s %s\n' "$verdict" "$tag" "$msg"
  done
fi
printf '\n  %d passed, %d failed, %d skipped\n' "$N_PASS" "$N_FAIL" "$N_SKIP"
if [ "$N_FAIL" -gt 0 ]; then
  printf '  %sFAILED%s — a red leg here is a shipping blocker, not a flake.\n' "$C_BAD" "$C_OFF"
  exit 1
fi
if [ "$N_SKIP" -gt 0 ]; then
  printf '  %sINCOMPLETE%s — %d leg(s) could not run. A skip is never a pass; see the reasons above.\n' \
    "$C_WARN" "$C_OFF" "$N_SKIP"
  exit 2
fi
printf '  %sALL PASS%s\n' "$C_OK" "$C_OFF"
exit 0
