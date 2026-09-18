#!/bin/sh
# Docs drift gates (docs-and-onboarding-overhaul WP1). The docs rotted through four unchecked
# duplication channels; these are the cheap textual halves of closing them. No cargo, no bun
# install — pure git grep, so the gate runs on every push in seconds (the deep half, regenerating
# the OpenAPI spec from the host binary, lives in ci.yml's `rust` job where the build already
# exists).
#
#   1. docs-site/public/openapi.json must be a byte-for-byte copy of api/openapi.json. The
#      snapshot is a manual `cp` (docs-site/README.md); it sat stale on main for weeks once,
#      publishing an /api reference missing the whole self-update surface.
#   2. Every PUNKTFUNK_* variable the docs mention must still exist somewhere in the tree
#      (docs describing a removed knob is drift a reader pays for). Historical records —
#      docs/releases/, CHANGELOG.md — don't count as existence: a knob that lives only in old
#      release notes is gone.
#   3. Ratchet: the count of PUNKTFUNK_* env vars in code that the docs never mention must not
#      grow. 300+ internal/debug knobs are deliberately undocumented today, so this can't be a
#      hard list — but a NEW knob must either be documented in docs-site (configuration.md or
#      the page owning its feature) or the baseline below raised in the same commit, making
#      "undocumented" a decision instead of an accident. Shrink the gap? Lower the baseline.
#   4. Every command the host-cli.md tables list must still exist as a string literal in
#      crates/punktfunk-host (same "docs describing removed things" class as gate 2).
#   5. data/platforms.json (the single source for install/port facts that docs, the website
#      download page and the guided installer consume) must parse, and docs-site/src/data/
#      platforms.json — the snapshot the <Install/> and <Ports/> MDX components render from
#      (the docs Docker build context is docs-site/ alone) — must be a byte copy of it, same
#      rule as gate 1.
#   6. scripts/install.sh (the download stub) parses, verifies its download, owns no options
#      states — every `install` line of an apt/pacman/dnf/sysext host platform must appear in
#      the script verbatim (it edits channel/group into the string at run time, never the
#      literal), and the script must parse under sh.
#   7. The installer under --dry-run against faked os-release files detects every family it claims
#      to (and --uninstall prints each family's removal) — the committed half of the manual
#      16-file matrix PR #345 was verified with. Needs curl on PATH (the script's own prerequisite).
#      Per-family *defaults* (group / linger / GameStream) live in check-install-defaults.sh.
#   8. scripts/web-init.sh — the web console's host-readiness gate — actually waits for the host's
#      first-run files and actually stops waiting when they land, against a faked config dir. Not
#      docs drift, but the same shape: a packaging script no build exercises, whose failure mode
#      (the console failing its first enable on a fresh install) only shows up on real glass.
#
# Textual gates, so textual limits: gate 2/3 match token spelling, not env reads — a var name in
# a code comment counts as "exists", and a quoted constant that isn't an env var counts toward
# the ratchet. Both err toward false calm on removal and a one-line baseline bump on addition,
# which is the cheap side to be wrong on.

set -u
LC_ALL=C
export LC_ALL
cd "$(dirname "$0")/../.." || exit 2

fail=0
tmp="${TMPDIR:-/tmp}/docs-drift.$$"
mkdir -p "$tmp"
trap 'rm -rf "$tmp"' EXIT

# ---------------------------------------------------------------- gate 1: openapi snapshot
if [ "$(cksum < api/openapi.json)" != "$(cksum < docs-site/public/openapi.json)" ]; then
    echo "::error::docs-site/public/openapi.json is not a copy of api/openapi.json — re-sync it:"
    echo "  cp api/openapi.json docs-site/public/openapi.json"
    fail=1
fi

# ---------------------------------------------------------------- gate 2: docs env vars exist
git grep -ohE 'PUNKTFUNK_[A-Z0-9_]+' -- docs-site/content | sort -u > "$tmp/docs-vars"
while IFS= read -r var; do
    if ! git grep -qF "$var" -- ':!docs-site' ':!docs/releases' ':!CHANGELOG.md'; then
        echo "::error::docs-site documents $var but nothing outside the docs mentions it — the knob was removed or renamed; fix the docs page"
        fail=1
    fi
done < "$tmp/docs-vars"

# ---------------------------------------------------------------- gate 3: undocumented ratchet
# Quoted occurrences only: env reads are quoted string literals; bare identifiers are
# Rust constants / C symbols, not knobs. The committed baseline enumerates today's undocumented
# set so a violation names exactly the new knob.
baseline=scripts/ci/docs-undocumented-env-baseline.txt
sort -u "$baseline" > "$tmp/baseline"   # re-sort under our LC_ALL=C, whatever locale wrote it
git grep -ohE '"PUNKTFUNK_[A-Z0-9_]+"' -- ':!docs-site' ':!*.md' | tr -d '"' | sort -u > "$tmp/code-vars"
comm -23 "$tmp/code-vars" "$tmp/docs-vars" > "$tmp/undocumented"
comm -23 "$tmp/undocumented" "$tmp/baseline" > "$tmp/new-undocumented"
if [ -s "$tmp/new-undocumented" ]; then
    echo "::error::new PUNKTFUNK_* vars are neither documented in docs-site nor in the baseline:"
    sed 's/^/  /' "$tmp/new-undocumented"
    echo "Document each in docs-site (configuration.md or the page owning the feature), or — for a deliberately internal knob — add it to $baseline in the same commit."
    fail=1
fi
comm -13 "$tmp/undocumented" "$tmp/baseline" > "$tmp/stale-baseline"
if [ -s "$tmp/stale-baseline" ]; then
    echo "baseline entries no longer undocumented (removed or now documented) — prune them from $baseline:"
    sed 's/^/  /' "$tmp/stale-baseline"
fi

# ---------------------------------------------------------------- gate 4: documented CLI exists
# First table cell of every row in host-cli.md: subcommands, sub-actions and flags. Multi-word
# cells (flag + argument) are skipped — they don't map to one string literal.
grep -E '^\|' docs-site/content/docs/host-cli.md | awk -F'|' '{print $2}' \
    | grep -oE '`[a-z0-9-]+`|`--[a-z-]+`' | tr -d '`' | sort -u > "$tmp/cli-cmds"
while IFS= read -r cmd; do
    if ! git grep -qF "\"$cmd\"" -- crates/punktfunk-host; then
        echo "::error::host-cli.md documents \`$cmd\` but crates/punktfunk-host has no \"$cmd\" literal — removed or renamed; fix the docs page"
        fail=1
    fi
done < "$tmp/cli-cmds"

# ---------------------------------------------------------------- gate 5: platforms.json parses
# Explicit try/exit: `bun -e` (1.3.x) exits 0 on an uncaught JSON.parse throw, so relying on
# the default uncaught-exception exit code silently disarms the gate.
json_check='try{JSON.parse(require("fs").readFileSync("data/platforms.json","utf8"))}catch(e){console.error(e.message);process.exit(1)}'
if [ ! -f data/platforms.json ]; then
    echo "::error::data/platforms.json is missing"
    fail=1
elif command -v bun >/dev/null 2>&1; then
    bun -e "$json_check" || { echo "::error::data/platforms.json is not valid JSON"; fail=1; }
elif command -v node >/dev/null 2>&1; then
    node -e "$json_check" || { echo "::error::data/platforms.json is not valid JSON"; fail=1; }
fi
if [ "$(cksum < data/platforms.json)" != "$(cksum < docs-site/src/data/platforms.json 2>/dev/null)" ]; then
    echo "::error::docs-site/src/data/platforms.json is not a copy of data/platforms.json — re-sync it:"
    echo "  cp data/platforms.json docs-site/src/data/platforms.json"
    fail=1
fi

# ---------------------------------------------------------------- gate 6: the installer stub stays dumb
# scripts/install.sh is a stub: it downloads punktfunk-setup, verifies its sha256 and execs it
# (design/installer-v2.md D1). Its whole value is being boring, so this checks that it still is.
#
# The install lines moved with the behaviour. The binary embeds data/platforms.json and generates
# its commands from it, so they are verbatim by construction rather than by a substring check —
# crates/punktfunk-setup/tests/plan_goldens.rs asserts it, and the detection and channel matrices
# that used to live below are now scripts/ci/check-installer-behavior.sh, run in the rust lane
# where a binary can be built.
if ! sh -n scripts/install.sh; then
    echo "::error::scripts/install.sh does not parse under sh"
    fail=1
fi
# An unverified download must never be executed.
if ! grep -q 'checksum mismatch' scripts/install.sh; then
    echo "::error::scripts/install.sh no longer refuses a download whose sha256 does not match"
    fail=1
fi
# The override installer-smoke drives the PR's own binary with.
if ! grep -q 'PUNKTFUNK_SETUP_BIN' scripts/install.sh; then
    echo "::error::scripts/install.sh lost the PUNKTFUNK_SETUP_BIN override"
    fail=1
fi
# The stub owns no options. A flag parsed here is a flag a years-old cached stub would not know,
# which is exactly the skew the two-stage split exists to avoid.
if grep -qE '^\s*--(yes|channel|uninstall|dry-run|client|host)\)' scripts/install.sh; then
    echo "::error::scripts/install.sh parses an installer option — the binary owns the interface"
    fail=1
fi
# A stub that still carried an install line would be a second place for one to drift.
installer_check='const fs=require("fs");const p=JSON.parse(fs.readFileSync("data/platforms.json","utf8"));const sh=fs.readFileSync("scripts/install.sh","utf8");let bad=0;for(const x of p.platforms){for(const line of x.install||[]){if(line.length>30&&sh.includes(line)){console.error(`::error::scripts/install.sh carries a platforms.json install line; the stub must not know one: ${line}`);bad=1}}}process.exit(bad)'
if command -v bun >/dev/null 2>&1; then
    bun -e "$installer_check" || fail=1
elif command -v node >/dev/null 2>&1; then
    node -e "$installer_check" || fail=1
fi

# ------------------------------------------------- gate 7: the web console's host-readiness gate
# scripts/web-init.sh is what stops punktfunk-web.service from starting before the host has written
# the files it cannot start without. `After=punktfunk-host.service` never did that (the host is
# Type=simple), and the console's first enable on a fresh install failed at the systemd level as a
# result — field report 2026-08-28, Omarchy. Run the real script against a faked config dir.
wi="$tmp/web-init"
# $1 = wait seconds, $2 = config dir. Echoes the script's output; sets wi_rc / wi_secs.
run_web_init() {
    _t0=$(date +%s)
    wi_out=$(XDG_CONFIG_HOME="$2" sh scripts/web-init.sh "$1" 2>&1) && wi_rc=0 || wi_rc=$?
    wi_secs=$(( $(date +%s) - _t0 ))
}
# Report a case: $1 = name, $2 = 'wait'|'ready', the rest is context already in wi_out.
web_init_case() {
    if [ "$wi_rc" -ne 0 ]; then
        echo "::error::web-init.sh ($1) exited $wi_rc — it must never fail the unit:"
        printf '%s\n' "$wi_out" | sed 's/^/    /'
        fail=1
        return
    fi
    case "$wi_out" in
        *'has not written its mgmt token'*) _saw=wait ;;
        *) _saw=ready ;;
    esac
    if [ "$_saw" != "$2" ]; then
        echo "::error::web-init.sh ($1): expected it to $2, but it did $_saw:"
        printf '%s\n' "$wi_out" | sed 's/^/    /'
        fail=1
    fi
}
seed_token() { printf 'PUNKTFUNK_MGMT_TOKEN=deadbeef\n' > "$1/punktfunk/mgmt-token"; }
# $2 is the filename PREFIX: "native-" for the current identity, "" for the legacy pair.
seed_pair() { printf 'cert\n' > "$1/punktfunk/$2cert.pem"; printf 'key\n' > "$1/punktfunk/$2key.pem"; }

# A bare config dir: nothing the console needs. It must WAIT, then give up cleanly (exit 0, so the
# console still starts and its Restart backstop takes over) — never fail the unit.
mkdir -p "$wi/empty/punktfunk"
run_web_init 1 "$wi/empty"
web_init_case "nothing written yet" wait
case "$wi_out" in *'waiting for punktfunk-host'*) ;; *)
    echo "::error::web-init.sh never announced the wait — the sleep path did not run:"
    printf '%s\n' "$wi_out" | sed 's/^/    /'
    fail=1 ;;
esac

# THE REGRESSION THIS GATE EXISTS FOR. The host writes mgmt-token EARLY in `serve` and its identity
# cert LAST (inside mgmt::run), so a gate that waits for the token alone still hands the console a
# directory with no cert to listen with — it just moves the failure. Token present, cert absent
# must still WAIT.
mkdir -p "$wi/token-only/punktfunk"
seed_token "$wi/token-only"
run_web_init 1 "$wi/token-only"
web_init_case "token written, identity cert not yet" wait

# Everything present: return immediately, no sleeping. This is every start after the host's first
# run, so it has to be free.
mkdir -p "$wi/native/punktfunk"
seed_token "$wi/native"
seed_pair "$wi/native" native-
run_web_init 30 "$wi/native"
web_init_case "token + native identity present" ready
[ "$wi_secs" -le 2 ] || { echo "::error::web-init.sh slept ${wi_secs}s with every file already present"; fail=1; }

# A host that never took the identity split serves the LEGACY pair and has no native-*.pem at all
# (crate::identity / web/nitro-entry/tls-paths.mjs). Waiting for a file it will never write would
# stall the console on every upgraded box.
mkdir -p "$wi/legacy/punktfunk"
seed_token "$wi/legacy"
seed_pair "$wi/legacy" ""
run_web_init 30 "$wi/legacy"
web_init_case "token + legacy identity present" ready

# The property the whole gate is for: it must NOTICE the files arriving and stop waiting, rather
# than sleeping out its whole budget. Written in the host's real order (token, then cert).
mkdir -p "$wi/late/punktfunk"
(
    sleep 2
    seed_token "$wi/late"
    seed_pair "$wi/late" native-
) &
run_web_init 30 "$wi/late"
wait
web_init_case "files appear while waiting" ready
if [ "$wi_secs" -lt 2 ] || [ "$wi_secs" -ge 30 ]; then
    echo "::error::web-init.sh returned after ${wi_secs}s — expected it to wait for the files (>=2s) and stop as soon as they landed (<30s)"
    fail=1
fi
# ------------------------------------- the bind line, which decides whether the console disappears
#
# web-init.sh names PUNKTFUNK_UI_BIND in host.env once. A missing line becomes the LAN default on
# a fresh box and an upgrade alike, and a line the operator wrote is never touched.
bind_of() { sed -n 's/^PUNKTFUNK_UI_BIND=//p' "$1/punktfunk/host.env" 2>/dev/null | tail -n1; }
bind_case() {
    _got=$(bind_of "$2")
    [ "$_got" = "$3" ] && return
    echo "::error::web-init.sh ($1): PUNKTFUNK_UI_BIND is '$_got', expected '$3'"
    fail=1
}

mkdir -p "$wi/bind-fresh/punktfunk"
seed_token "$wi/bind-fresh"
seed_pair "$wi/bind-fresh" native-
run_web_init 1 "$wi/bind-fresh"
bind_case "fresh install" "$wi/bind-fresh" 0.0.0.0

mkdir -p "$wi/bind-upgrade/punktfunk"
seed_token "$wi/bind-upgrade"
seed_pair "$wi/bind-upgrade" native-
printf 'PUNKTFUNK_UI_PASSWORD=hunter2\n' > "$wi/bind-upgrade/punktfunk/web-password"
run_web_init 1 "$wi/bind-upgrade"
bind_case "upgrade" "$wi/bind-upgrade" 0.0.0.0

# An operator's answer is never rewritten, and a second run never appends a second line.
mkdir -p "$wi/bind-set/punktfunk"
seed_token "$wi/bind-set"
seed_pair "$wi/bind-set" native-
printf 'PUNKTFUNK_UI_BIND=100.64.0.3\n' > "$wi/bind-set/punktfunk/host.env"
run_web_init 1 "$wi/bind-set"
run_web_init 1 "$wi/bind-set"
bind_case "an existing line wins" "$wi/bind-set" 100.64.0.3
_n=$(grep -c '^PUNKTFUNK_UI_BIND=' "$wi/bind-set/punktfunk/host.env")
[ "$_n" = 1 ] || { echo "::error::web-init.sh wrote PUNKTFUNK_UI_BIND $_n times"; fail=1; }

rm -rf "$wi"

exit "$fail"
