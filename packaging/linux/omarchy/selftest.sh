#!/usr/bin/env bash
#
# Self-check for the pieces of `punktfunk-omarchy` that parse or generate a file the USER owns,
# plus the firewall source list — all without root or an Omarchy box.
#
#     bash packaging/linux/omarchy/selftest.sh
#
# Everything else in that script is systemctl/ufw/omarchy calls, which are the box's to answer.

set -euo pipefail
cd "$(dirname "$0")"

SCRIPT=./punktfunk-omarchy
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
fails=0

check() {  # check <name> <expected-file> <actual-file>
  if diff -u "$2" "$3" >/dev/null; then
    printf '  ok   %s\n' "$1"
  else
    printf '  FAIL %s\n' "$1"; diff -u "$2" "$3" | sed 's/^/       /'; fails=$((fails + 1))
  fi
}

# Source the script's functions without running its dispatcher: it dispatches on "$1", and "help"
# only prints. `set +e` around it because the script itself sets -e.
# shellcheck disable=SC1090
source "$SCRIPT" help >/dev/null

echo "xdph picker restore"

# 1. The Omarchy case: they had their own picker, we took it over, `remove` puts it back verbatim.
mkdir -p "$WORK/hypr"
cat > "$WORK/hypr/xdph.conf" <<'EOF'
screencopy {
    allow_token_by_default = true
    # punktfunk: previous custom_picker_binary = hyprland-preview-share-picker
    custom_picker_binary = /run/user/1000/punktfunk-xdph-picker.sh
}
EOF
cat > "$WORK/expected" <<'EOF'
screencopy {
    allow_token_by_default = true
    custom_picker_binary = hyprland-preview-share-picker
}
EOF
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
check "their picker comes back and their other keys survive" "$WORK/expected" "$WORK/hypr/xdph.conf"

# 2. The key did not exist before us: restoring must REMOVE our line, not blank it or invent a value.
cat > "$WORK/hypr/xdph.conf" <<'EOF'
screencopy {
    # punktfunk: previous custom_picker_binary = (none)
    custom_picker_binary = /run/user/1000/punktfunk-xdph-picker.sh
}
EOF
printf 'screencopy {\n}\n' > "$WORK/expected"
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
check "a key we invented is removed, not blanked" "$WORK/expected" "$WORK/hypr/xdph.conf"

# 3. A config that was never ours must come through byte-identical — this runs on every `remove`.
cat > "$WORK/hypr/xdph.conf" <<'EOF'
screencopy {
    custom_picker_binary = hyprland-preview-share-picker
}
EOF
cp "$WORK/hypr/xdph.conf" "$WORK/expected"
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
check "a config without our marker is untouched" "$WORK/expected" "$WORK/hypr/xdph.conf"

# 4. No config at all: a no-op, and it must not CREATE one.
rm -f "$WORK/hypr/xdph.conf"
XDG_CONFIG_HOME="$WORK" restore_picker >/dev/null
if [[ -e "$WORK/hypr/xdph.conf" ]]; then
  printf '  FAIL restoring created a config that did not exist\n'; fails=$((fails + 1))
else
  printf '  ok   an absent config stays absent\n'
fi

echo "hooks.json"

# 5. Every combination of the two opt-ins must be valid JSON — the blocks are concatenated, so a
#    stray or missing comma between them is the failure mode.
for combo in "hooks_json" "idle_hooks_json" "hooks_json idle_hooks_json"; do
  # shellcheck disable=SC2086
  XDG_CONFIG_HOME="$WORK/fresh-${combo// /-}" write_hooks $combo >/dev/null
  f="$WORK/fresh-${combo// /-}/punktfunk/hooks.json"
  if python3 -c "import json,sys; d=json.load(open(sys.argv[1])); assert d['hooks'] and all('on' in h and 'run' in h for h in d['hooks'])" "$f"; then
    printf '  ok   valid JSON for [%s]\n' "$combo"
  else
    printf '  FAIL invalid JSON for [%s]\n' "$combo"; cat "$f"; fails=$((fails + 1))
  fi
done

# 6. An existing hooks.json is the operator's document: print, never overwrite.
mkdir -p "$WORK/mine/punktfunk"
echo '{"hooks":[{"on":"stream.started","webhook":"https://example.invalid/x"}]}' > "$WORK/mine/punktfunk/hooks.json"
cp "$WORK/mine/punktfunk/hooks.json" "$WORK/expected"
XDG_CONFIG_HOME="$WORK/mine" write_hooks hooks_json >/dev/null
check "an operator's own hooks.json is never overwritten" "$WORK/expected" "$WORK/mine/punktfunk/hooks.json"

echo "omarchy menu merge"

# The menu is a SINGLE JSONC document and one parse error drops every row the user owns, so the
# merge gets the same scrutiny as the picker restore.
menudir="$WORK/menu/omarchy/extensions"
mkdir -p "$menudir"
# The URL row matters: `//` inside a STRING is not a comment, and the first validator treated it
# as one — truncating the row mid-string and refusing to touch a file the user never broke.
cat > "$menudir/omarchy-menu.jsonc" <<'EOF'
{
  // a comment the user wrote
  "personal": {"icon":"","label":"Personal"},
  "personal.notes": {"icon":"󰎞","label":"Notes","action":"true"},
  "personal.site": {"icon":"󰖟","label":"Site","action":"omarchy-launch-webapp https://example.com"},
}
EOF
cp "$menudir/omarchy-menu.jsonc" "$WORK/menu-before"

XDG_CONFIG_HOME="$WORK/menu" setup_menu >/dev/null 2>&1
f="$menudir/omarchy-menu.jsonc"

if XDG_CONFIG_HOME="$WORK/menu" menu_is_valid "$f"; then
  printf '  ok   the merged menu still parses as JSONC\n'
else
  printf '  FAIL the merged menu does not parse\n'; cat "$f"; fails=$((fails + 1))
fi
if grep -q '"personal.notes"' "$f" && grep -q '"punktfunk-host.console"' "$f"; then
  printf "  ok   the user's rows survived and ours were added\n"
else
  printf "  FAIL rows lost in the merge\n"; fails=$((fails + 1))
fi

missing=
for id in punktfunk-host.dedicated punktfunk-host.mirror punktfunk-host.pin \
          punktfunk-host.unpair punktfunk-host.gamestream; do
  grep -q "\"$id\"" "$f" || missing="$missing $id"
done
if [[ -z "$missing" ]]; then
  printf '  ok   dedicated, mirror, pin, unpair and gamestream rows are present\n'
else
  printf '  FAIL menu missing ids:%s\n' "$missing"; fails=$((fails + 1))
fi
if grep -q 'punktfunk-omarchy mode --status | grep -qx dedicated' "$f" &&
   grep -q 'punktfunk-omarchy mode --status | grep -qx mirror' "$f"; then
  printf '  ok   dedicated and this-screen rows check mode --status\n'
else
  printf '  FAIL dedicated/mirror rows do not check mode --status\n'; fails=$((fails + 1))
fi
if grep -q '"punktfunk-host.dedicated".*aliases' "$f" ||
   grep -q '"punktfunk-host.mirror".*aliases' "$f"; then
  printf '  FAIL new host rows must not add aliases\n'; fails=$((fails + 1))
else
  printf '  ok   new host rows have no aliases\n'
fi

# Idempotent: a second run must not stack a second copy.
XDG_CONFIG_HOME="$WORK/menu" setup_menu >/dev/null 2>&1
n=$(grep -c '"punktfunk-host.console"' "$f")
if [[ "$n" == "1" ]]; then printf '  ok   re-running does not duplicate the block\n'
else printf '  FAIL block appears %s times after two runs\n' "$n"; fails=$((fails + 1)); fi

# And `remove` puts the file back exactly as the user had it.
XDG_CONFIG_HOME="$WORK/menu" remove_menu >/dev/null 2>&1
check "remove restores the user's file byte for byte" "$WORK/menu-before" "$f"

# A file that does not parse to begin with is not ours to repair — leave it untouched.
printf '{ this is not json\n' > "$f"
cp "$f" "$WORK/menu-broken"
XDG_CONFIG_HOME="$WORK/menu" setup_menu >/dev/null 2>&1
check "a config we cannot parse is left alone" "$WORK/menu-broken" "$f"

echo "app menu"

# `setup_webapp` must run to its END on a box with nothing of ours installed yet — no pre-rename
# entry, no applications dir at all. That is the ordinary first install, and it is where a bare
# `x=$(grep … | head -1)` under `set -e` + `pipefail` killed the script dead: the step header had
# already printed, so the operator saw "==> App menu" and then nothing, and every later step
# (plugin, menu, hooks, theme, status) silently never ran. Reaching `setup_menu` is the proof.
mkdir -p "$WORK/bin" "$WORK/webapp"
printf '#!/bin/sh\nexit 0\n' > "$WORK/bin/omarchy-webapp-install"
printf '#!/bin/sh\nexit 0\n' > "$WORK/bin/omarchy-webapp-remove"
chmod +x "$WORK/bin/omarchy-webapp-install" "$WORK/bin/omarchy-webapp-remove"

# A CHILD bash, not a subshell: `set -e` is suppressed for everything inside an `if` condition,
# subshells and called functions included, so an in-process call would pass even while the real
# `punktfunk-omarchy setup` dies. The child re-sources the script, so its own `set -euo pipefail`
# is what governs — the same shell state an operator gets.
if env HOME="$WORK/webapp" XDG_CONFIG_HOME="$WORK/webapp/config" PATH="$WORK/bin:$PATH" \
     bash -c "source $SCRIPT help >/dev/null 2>&1; setup_webapp" >/dev/null 2>&1 &&
   grep -q '"punktfunk-host.console"' "$WORK/webapp/config/omarchy/extensions/omarchy-menu.jsonc"
then
  printf '  ok   setup_webapp completes with no prior entry and reaches the menu\n'
else
  printf '  FAIL setup_webapp aborted before the menu on a clean box\n'; fails=$((fails + 1))
fi

echo "bar widget"

# The shell registers a freshly copied plugin asynchronously, so `plugin_known` must poll the
# list rather than trust the first answer. The stub lists punktfunk from its third call on.
mkdir -p "$WORK/om/bin"
cat > "$WORK/om/bin/omarchy" <<'EOF'
#!/bin/sh
n=$(cat "$OM_CALLS" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$OM_CALLS"
[ "$1 $2" = "plugin list" ] && [ "$n" -ge 3 ] && echo "punktfunk   enabled   third-party bar-widget   Punktfunk"
exit 0
EOF
chmod +x "$WORK/om/bin/omarchy"
if OM_CALLS="$WORK/om/calls" PATH="$WORK/om/bin:$PATH" plugin_known >/dev/null 2>&1 &&
   [[ "$(cat "$WORK/om/calls")" -ge 3 ]]; then
  printf '  ok   plugin_known waits for the shell to list the plugin\n'
else
  printf '  FAIL plugin_known gave up before the shell listed the plugin\n'; fails=$((fails + 1))
fi

echo "setup options"

# The installer asks for every optional step on its own screen and passes the answers here, so
# nothing in `setup` may prompt. A stray `read`, a missing case or a silently accepted value all
# put an Omarchy box back to two rounds of questions, which is what these four checks catch.
if grep -q 'read -r -p' "$SCRIPT"; then
  printf '  FAIL setup still prompts for something\n'; fails=$((fails + 1))
else
  printf '  ok   nothing in the script prompts\n'
fi

if (parse_setup_opts --toasts=0 --theme=0 >/dev/null 2>&1
    [[ "$OPT_TOASTS" == 0 && "$OPT_THEME" == 0 && "$OPT_IDLE" == 1 ]]); then
  printf '  ok   an option sets its own row and leaves the rest alone\n'
else
  printf '  FAIL parse_setup_opts did not apply the options\n'; fails=$((fails + 1))
fi

if (parse_setup_opts --toasts=maybe >/dev/null 2>&1); then
  printf '  FAIL a value that is not 1 or 0 was accepted\n'; fails=$((fails + 1))
else
  printf '  ok   a value that is not 1 or 0 is refused\n'
fi

if (parse_setup_opts --nonsense=1 >/dev/null 2>&1); then
  printf '  FAIL an unknown option was accepted\n'; fails=$((fails + 1))
else
  printf '  ok   an unknown option is refused\n'
fi

echo "lan_sources / ufw"

{
  printf '%s\n' 192.168.0.0/16 10.0.0.0/8 172.16.0.0/12 tailscale0 wt0
} > "$WORK/expected-lan"
lan_sources > "$WORK/actual-lan"
check "lan_sources preinstalls the tailscale0 and wt0 rules" "$WORK/expected-lan" "$WORK/actual-lan"

# A CIDR takes a `from` rule and an interface an `in on` rule; `remove` must name each the same way.
sudo() { printf '%s\n' "$*" >> "$WORK/actual-ufw"; }
ufw_rule 10.0.0.0/8 app punktfunk-native
ufw_rule wt0 app punktfunk-native
ufw_unrule 10.0.0.0/8 app punktfunk-native
ufw_unrule wt0 app punktfunk-native
unset -f sudo
{
  printf '%s\n' "ufw allow from 10.0.0.0/8 to any app punktfunk-native comment $TAG"
  printf '%s\n' "ufw allow in on wt0 to any app punktfunk-native comment $TAG"
  printf '%s\n' "ufw --force delete allow from 10.0.0.0/8 to any app punktfunk-native"
  printf '%s\n' "ufw --force delete allow in on wt0 to any app punktfunk-native"
} > "$WORK/expected-ufw"
check "a CIDR and an interface each get their own ufw rule" "$WORK/expected-ufw" "$WORK/actual-ufw"

echo "moved mgmt port"

while IFS='|' read -r body want; do
  printf '%b' "$body" > "$WORK/mgmt.env"
  got=$(moved_mgmt_port "$WORK/mgmt.env")
  if [[ "$got" == "$want" ]]; then
    printf '  ok   %s -> %s\n' "$body" "${want:-none}"
  else
    printf '  FAIL %s -> %s, want %s\n' "$body" "${got:-none}" "${want:-none}"; fails=$((fails + 1))
  fi
done <<'EOF'
PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\n|47991
PUNKTFUNK_MGMT_BIND="[::]:48123"\n|48123
PUNKTFUNK_MGMT_BIND=0.0.0.0:47990\n|
# PUNKTFUNK_MGMT_BIND=0.0.0.0:48123\n|
PUNKTFUNK_MGMT_BIND=0.0.0.0:47991\nPUNKTFUNK_MGMT_BIND=\n|
EOF
if [[ -z "$(moved_mgmt_port "$WORK/absent.env")" ]]; then
  printf '  ok   no host.env -> none\n'
else
  printf '  FAIL no host.env reported a port\n'; fails=$((fails + 1))
fi

echo "mode / gamestream env"

mod="$WORK/mode"
mkdir -p "$mod"
if got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "dedicated" ]]; then
  printf '  ok   mode --status is dedicated when the policy file is absent\n'
else
  printf '  FAIL mode --status without a file: %s\n' "${got:-<empty>}"; fails=$((fails + 1))
fi

mkdir -p "$mod/punktfunk"
printf '{ "preset": "default", "capture_monitor": null, "max_displays": 4 }\n' \
  > "$mod/punktfunk/display-settings.json"
if got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "dedicated" ]]; then
  printf '  ok   mode --status is dedicated when capture_monitor is null\n'
else
  printf '  FAIL mode --status null pin: %s\n' "${got:-<empty>}"; fails=$((fails + 1))
fi

printf 'PUNKTFUNK_MGMT_BIND=127.0.0.1:47990\nPUNKTFUNK_CAPTURE_MONITOR=DP-9\n' \
  > "$mod/punktfunk/host.env"
if got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "mirror" ]]; then
  printf '  ok   mode --status honors the stronger host.env screen pin\n'
else
  printf '  FAIL mode --status ignored host.env: %s\n' "${got:-<empty>}"; fails=$((fails + 1))
fi

XDG_CONFIG_HOME="$mod" write_capture_monitor "$mod/punktfunk/display-settings.json" "" >/dev/null
if got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "dedicated" ]] &&
   grep -qx 'PUNKTFUNK_MGMT_BIND=127.0.0.1:47990' "$mod/punktfunk/host.env" &&
   ! grep -q '^PUNKTFUNK_CAPTURE_MONITOR=' "$mod/punktfunk/host.env"; then
  printf '  ok   dedicated clears the stronger host.env screen pin\n'
else
  printf '  FAIL dedicated left the host.env screen pin active\n'; fails=$((fails + 1))
fi

printf '{ "preset": "default", "capture_monitor": "DP-2", "max_displays": 4 }\n' \
  > "$mod/punktfunk/display-settings.json"
if got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "mirror" ]]; then
  printf '  ok   mode --status is mirror when capture_monitor is set\n'
else
  printf '  FAIL mode --status pinned: %s\n' "${got:-<empty>}"; fails=$((fails + 1))
fi

# Write helpers only — `mode dedicated` would try-restart the real user unit.
XDG_CONFIG_HOME="$mod" write_capture_monitor "$mod/punktfunk/display-settings.json" "" >/dev/null
if got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "dedicated" ]] &&
   python3 -c 'import json,sys; d=json.load(open(sys.argv[1])); assert d["capture_monitor"] is None; assert d["preset"]=="default"; assert d["max_displays"]==4' \
     "$mod/punktfunk/display-settings.json"; then
  printf '  ok   dedicated clears capture_monitor and keeps the other axes\n'
else
  printf '  FAIL dedicated did not clear the pin in place\n'; fails=$((fails + 1))
  cat "$mod/punktfunk/display-settings.json"
fi

mkdir -p "$WORK/bin"
cat > "$WORK/bin/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >> "${SYSTEMCTL_LOG:-/dev/null}"
exit 0
EOF
cat > "$WORK/bin/punktfunk-host" <<'EOF'
#!/bin/sh
if [ "$1" = "list-monitors" ]; then
  cat <<'MON'
Hyprland:
  eDP-1         1920x1080@60 at +0,+0  scale 1  Built-in  [primary]
  HDMI-A-1      2560x1440@144 at +1920,+0  scale 1  Desk
MON
  exit 0
fi
exit 1
EOF
chmod +x "$WORK/bin/systemctl" "$WORK/bin/punktfunk-host"

SYSTEMCTL_LOG="$WORK/systemctl.log"
: > "$SYSTEMCTL_LOG"
printf 'PUNKTFUNK_CAPTURE_MONITOR=DP-9\n' > "$mod/punktfunk/host.env"
if PATH="$WORK/bin:$PATH" SYSTEMCTL_LOG="$SYSTEMCTL_LOG" XDG_CONFIG_HOME="$mod" \
     "$SCRIPT" mode mirror >/dev/null &&
   got=$(XDG_CONFIG_HOME="$mod" "$SCRIPT" mode --status) && [[ "$got" == "mirror" ]] &&
   python3 -c 'import json,sys; assert json.load(open(sys.argv[1]))["capture_monitor"]=="eDP-1"' \
     "$mod/punktfunk/display-settings.json" &&
   ! grep -q '^PUNKTFUNK_CAPTURE_MONITOR=' "$mod/punktfunk/host.env"; then
  printf '  ok   mode mirror pins the primary connector and clears host.env\n'
else
  printf '  FAIL mode mirror did not pin the primary\n'; fails=$((fails + 1))
  cat "$mod/punktfunk/display-settings.json" 2>/dev/null || true
fi
if grep -q 'try-restart punktfunk-host' "$SYSTEMCTL_LOG"; then
  printf '  ok   mode mirror reloads the host unit\n'
else
  printf '  FAIL mode mirror did not try-restart the host\n'; fails=$((fails + 1))
fi

if { printf '%s\n' 'Hyprland:' '  DP-1  1920x1080@60 at +0,+0  scale 1  Desk'
   } | primary_connector > "$WORK/sole" && [[ "$(cat "$WORK/sole")" == "DP-1" ]]; then
  printf '  ok   a sole head is the mirror pin even without a primary tag\n'
else
  printf '  FAIL sole-head primary_connector: %s\n' "$(cat "$WORK/sole" 2>/dev/null)"
  fails=$((fails + 1))
fi

if printf '%s\n' 'Hyprland:' \
     '  DP-1  1920x1080@60 at +0,+0  scale 1  A' \
     '  DP-2  1920x1080@60 at +1920,+0  scale 1  B' \
     | primary_connector >/dev/null 2>&1; then
  printf '  FAIL two untagged heads were accepted as a pin\n'; fails=$((fails + 1))
else
  printf '  ok   two untagged heads are not a pin\n'
fi

gs="$WORK/gs/punktfunk"
mkdir -p "$gs"
punktfunk-host() { printf '%s\n' "$*" > "$WORK/gs/host-cmd"; }
printf 'PUNKTFUNK_MGMT_BIND=127.0.0.1:47990\nPUNKTFUNK_GAMESTREAM=0\n' > "$gs/host.env"
XDG_CONFIG_HOME="$WORK/gs" enable_gamestream
if ! grep -q '^PUNKTFUNK_GAMESTREAM=' "$gs/host.env" &&
   grep -qx 'PUNKTFUNK_MGMT_BIND=127.0.0.1:47990' "$gs/host.env" &&
   grep -qx 'settings set gamestream true' "$WORK/gs/host-cmd"; then
  printf '  ok   GameStream opt-in stores the setting and drops the host.env pin\n'
else
  printf '  FAIL GameStream opt-in left a host.env pin or skipped the store\n'; fails=$((fails + 1))
  cat "$gs/host.env"
fi
unset -f punktfunk-host

pin_json='{"v":1,"data":{"pin_pending":true,"pending":[{"uniqueid":"u1","fingerprint":"abc123def456","peer_ip":"10.0.0.8"}]}}'
if rows=$(ctl_json_rows "$pin_json" pin) && [[ "$rows" == $'u1\tabc123def456\t10.0.0.8' ]]; then
  printf '  ok   pin TUI reads the waiting ceremony from ctl pair --json\n'
else
  printf '  FAIL pin TUI parse: %s\n' "${rows:-<empty>}"; fails=$((fails + 1))
fi
clients_json='{"v":1,"data":{"native":[{"name":"Pad","fingerprint":"deadbeef0123"}],"gamestream":[{"label":"Moon","fingerprint":"cafebabef00d"}]}}'
if rows=$(ctl_json_rows "$clients_json" unpair) &&
   grep -qx $'native\tPad\tdeadbeef0123' <<<"$rows" &&
   grep -qx $'gamestream\tMoon\tcafebabef00d' <<<"$rows"; then
  printf '  ok   unpair TUI lists both planes from ctl clients --json\n'
else
  printf '  FAIL unpair TUI parse: %s\n' "${rows:-<empty>}"; fails=$((fails + 1))
fi

help_out="$("$SCRIPT" help)"
if grep -q 'autostart, ufw' <<<"$help_out"; then
  printf '  ok   help names ufw as a setup step\n'
else
  printf '  FAIL help dropped the ufw setup mention\n'; fails=$((fails + 1))
fi
if grep -q 'punktfunk-omarchy mode' <<<"$help_out"; then
  printf '  ok   help names mode\n'
else
  printf '  FAIL help dropped mode\n'; fails=$((fails + 1))
fi
if grep -qE 'punktfunk-omarchy (pin|unpair|gamestream)[[:space:]]' <<<"$help_out"; then
  printf '  FAIL hidden menu verbs leaked into public help\n'; fails=$((fails + 1))
else
  printf '  ok   pin, unpair and gamestream stay out of public help\n'
fi

echo
if [[ $fails -eq 0 ]]; then echo "all checks passed"; else echo "$fails check(s) failed"; exit 1; fi
