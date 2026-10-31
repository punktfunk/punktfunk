#!/bin/bash
# Runs scripts/steamdeck/rebuild-check.sh against a stub box: no Deck, no build.
#
#   a host binary that loads         -> exit 0, update.sh never runs
#   no host binary                   -> update.sh runs once and the attempt stamp is cleared
#   the same with no host.env        -> update.sh still runs
#   update.sh fails                  -> the stamp stays, and the next boot does not retry
#   door on, no sudo, a new build    -> update.sh stops before pulling or building
#   door on, no sudo, the same build -> update.sh rebuilds, as a post-OS-update check needs
#
# bash -n and shellcheck cannot see an unbound variable in lib.sh or a broken `set -e` path; this
# sources the real lib.sh and takes each branch. rebuild-check.sh runs a stub update.sh that logs
# its calls; the door paths run the real one on stub commands.
set -euo pipefail
cd "$(dirname "$0")/../.."
repo=$PWD
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
src=$tmp/src
host=$src/target-steamos/release/punktfunk-host
mkdir -p "$src/scripts/steamdeck" "$(dirname "$host")" "$tmp/home/.config/punktfunk"
: > "$tmp/home/.config/punktfunk/host.env" # install.sh always writes one

stub() {  # stub <exit status>: an update.sh that logs a call and exits with it
    printf '#!/bin/sh\necho ran >> "%s/calls"\nexit %s\n' "$tmp" "$1" > "$src/scripts/steamdeck/update.sh"
}
run() { HOME=$tmp/home PUNKTFUNK_SRC=$src bash "$repo/scripts/steamdeck/rebuild-check.sh" 2>&1; }
calls() { if [ -f "$tmp/calls" ]; then wc -l < "$tmp/calls" | tr -d ' '; else echo 0; fi; }
stamp=$tmp/home/.cache/punktfunk/rebuild-attempt
fail() { echo "check-steamdeck-rebuild: $*" >&2; exit 1; }

stub 0
cp "$(type -P true)" "$host"
run > /dev/null
[ "$(calls)" = 0 ] || fail "a loadable host binary still ran update.sh"

rm "$host"
run > /dev/null
[ "$(calls)" = 1 ] || fail "a missing host binary ran update.sh $(calls) times, not once"
[ ! -e "$stamp" ] || fail "a successful rebuild left its attempt stamp"

rm "$tmp/home/.config/punktfunk/host.env"
run > /dev/null || fail "with no host.env a missing host binary did not rebuild"
[ "$(calls)" = 2 ] || fail "with no host.env update.sh ran $(calls) times in all, not twice"
: > "$tmp/home/.config/punktfunk/host.env"

stub 1
run > /dev/null && fail "a failed update.sh did not fail the check"
[ -e "$stamp" ] || fail "a failed rebuild cleared its attempt stamp"
out=$(run)
case $out in *"not retrying"*) ;; *) fail "the second boot retried the rebuild: $out" ;; esac
[ "$(calls)" = 3 ] || fail "the second boot ran update.sh again"

# The real update.sh on a box whose door is on, without sudo. Its steps past the gate are stubs:
# heal-box.sh stops the run, so reaching it means a build would have started.
stubs=$tmp/stubs
mkdir -p "$stubs" "$src/.git" "$src/crates/host/punktfunk-host"
printf '#!/bin/sh\nexit 1\n' > "$stubs/sudo"
printf '#!/bin/sh\necho "git $*" >> "%s/steps"\n' "$tmp" > "$stubs/git"
printf '#!/bin/sh\n[ -e "%s/door-on" ]\n' "$tmp" > "$stubs/systemctl"
chmod +x "$stubs"/*
printf '#!/bin/sh\necho build >> "%s/steps"\nexit 7\n' "$tmp" > "$src/scripts/steamdeck/heal-box.sh"
# shellcheck disable=SC2016 # the stub expands it
printf '#!/bin/sh\necho "repulled sudo=$PF_SUDO_OK" >> "%s/steps"\n' "$tmp" > "$src/scripts/steamdeck/update.sh"
version() { printf '#!/bin/sh\necho %s\n' "$1" > "$src/scripts/steamdeck/build-version.sh"; }
update() {
    : > "$tmp/steps"
    HOME=$tmp/home PUNKTFUNK_SRC=$src PATH=$stubs:$PATH bash "$repo/scripts/steamdeck/update.sh" "$@" </dev/null 2>&1
}
steps() { grep -c "$1" "$tmp/steps" || true; }

: > "$tmp/door-on"
version 0.46.0+gnew
out=$(update --pull) && fail "--pull with the door on and no sudo went through"
case $out in *"needs your password"*) ;; *) fail "the refused --pull didn't say why: $out" ;; esac
[ "$(steps ' pull --ff-only')" = 0 ] || fail "the refused --pull still pulled"
update > /dev/null && fail "a new version with the door on and no sudo went through"
[ "$(steps build)" = 0 ] || fail "a new version built with the door on and no sudo"
# /opt has no version file here, so an empty build version reads as the version the door holds.
version ''
update > /dev/null || true
[ "$(steps build)" = 1 ] || fail "the same version stopped a post-OS-update rebuild"
rm "$tmp/door-on"
version 0.46.0+gnew
update --pull > /dev/null
[ "$(steps ' pull --ff-only')" = 1 ] && [ "$(steps 'repulled sudo=0')" = 1 ] || fail "with the door off --pull didn't go on"
echo "check-steamdeck-rebuild: healthy, rebuild, no-retry and door paths behave"
