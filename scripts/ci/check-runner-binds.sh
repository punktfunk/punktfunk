#!/bin/sh
# Every config-dir file the plugin runner reads must be bound into its unit's empty home.
#
# `ProtectHome=tmpfs` means a path the unit does not name does not exist for the runner, whatever
# is on disk. `plugin-tokens.json` shipped unbound and no plugin on Linux could start, with an
# error blaming the host for a file the host had already written. The names come out of the SDK
# here rather than from a second hand-kept list; a join whose first argument is not one of the
# three spellings below is invisible to this gate.
set -eu
cd "$(dirname "$0")/../.."

UNIT=scripts/punktfunk-scripting.service
NIX=packaging/nix/nixos-module.nix

names=$(grep -rhoE 'path\.join\((configDir\(\)|configDir|config), "[^"]+"' sdk/src |
	sed -E 's/.*"([^"]+)"/\1/' | sort -u)

rc=0
for n in $names; do
	for f in "$UNIT" "$NIX"; do
		grep -q "punktfunk/$n" "$f" || {
			echo "$f: the runner reads \$config/$n and this unit never binds it"
			rc=1
		}
	done
done

if [ "$rc" != 0 ]; then
	echo "add a Bind*Paths line — under the tmpfs home an unbound path is ENOENT, not a permission error"
fi
exit "$rc"
