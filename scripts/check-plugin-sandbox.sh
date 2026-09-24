#!/bin/bash
# What a sandboxed plugin can actually reach — driven through the REAL runner, against a real
# kernel, rather than asserted.
#
# This script used to re-declare bwrap's flags by hand. A flag the copy omitted was a flag no
# check ever ran, which is how `--disable-userns` shipped needing an `--unshare-user` nothing
# supplied (bwrap refused every plugin), and how `--clearenv` shipped discarding the whole
# environment the plugin needs. So: no copy. It builds the runner, installs a probe plugin, and
# reads what that plugin reports from inside its own sandbox.
#
#   docker run --rm --privileged -v "$PWD":/w -v "$PWD/scripts":/s oven/bun:1 \
#     bash /s/check-plugin-sandbox.sh
#
# Needs bubblewrap and unprivileged user namespaces, so it runs on Linux only.
set -u
W=${PUNKTFUNK_REPO:-/w}
apt-get update -qq >/dev/null 2>&1
apt-get install -y -qq bubblewrap iproute2 >/dev/null 2>&1 || { echo "FAIL: no bubblewrap"; exit 1; }

cd "$W/sdk" || { echo "FAIL: no $W/sdk — mount the repo at /w"; exit 1; }
bun install --ignore-scripts >/dev/null 2>&1
# The sandbox binds ONE runner file, so the shipped runner is a bundle. Test what ships.
bun build src/runner-cli.ts --target=bun --outfile /runner.js >/dev/null || { echo "FAIL: bundle"; exit 1; }

export HOME=/root
CFG=$HOME/.config/punktfunk
P=$CFG/plugins/node_modules/punktfunk-plugin-probe
mkdir -p "$P" "$CFG/plugin-run" "$HOME/.ssh" "$HOME/steamlike" "$HOME/granted" "$HOME/dynamic"
echo "secret-admin-token"  > "$CFG/mgmt-token"
echo "private key"         > "$HOME/.ssh/id_ed25519"
echo "library-data"        > "$HOME/steamlike/marker"
echo "granted-data"        > "$HOME/granted/marker"
echo "dynamic-data"        > "$HOME/dynamic/marker"
echo '{"probe":"testtoken"}' > "$CFG/plugin-run/plugin-tokens.json"
printf '{"probe":["/root/granted"]}' > "$CFG/plugin-run/plugin-grants.json"
printf '{"dependencies":{"punktfunk-plugin-probe":"*"}}' > "$CFG/plugins/package.json"
printf '{"name":"punktfunk-plugin-probe","version":"1.0.0","main":"index.js","punktfunk":{"schema":1,"id":"probe","reads":["~/steamlike"]}}' > "$P/package.json"

cat > "$P/index.js" <<'JS'
import fs from "node:fs";
const say = (k, v) => `${k}=${v}`;
const o = [];
let home = "UNSET";
try { home = (await import("node:os")).homedir(); } catch (e) { home = "THREW"; }
o.push(say("homedir", home));
const gone = (f) => { try { f(); return "READABLE"; } catch { return "blocked"; } };
o.push(say("mgmt", gone(() => fs.readFileSync("/root/.config/punktfunk/mgmt-token", "utf8"))));
o.push(say("ssh", gone(() => fs.readFileSync("/root/.ssh/id_ed25519", "utf8"))));
o.push(say("declared", (() => { try { return fs.readFileSync(home + "/steamlike/marker", "utf8").trim(); } catch { return "UNREACHABLE"; } })()));
o.push(say("declared_ro", (() => { try { fs.writeFileSync(home + "/steamlike/w", "x"); return "WRITABLE"; } catch { return "readonly"; } })()));
o.push(say("granted", (() => { try { return fs.readFileSync(home + "/granted/marker", "utf8").trim(); } catch { return "UNREACHABLE"; } })()));
o.push(say("granted_write", (() => { try { fs.writeFileSync(home + "/granted/w", "x"); return "WRITABLE"; } catch (e) { return e.code; } })()));
o.push(say("dynamic", (() => { try { return fs.readFileSync(home + "/dynamic/marker", "utf8").trim(); } catch { return "UNREACHABLE"; } })()));
o.push(say("state", (() => { try { fs.writeFileSync("/run/punktfunk/plugin-state/probe/w", "x"); return "writable"; } catch { return "UNWRITABLE"; } })()));
o.push(say("owntoken", (() => { try { fs.readFileSync("/run/punktfunk/plugin-token", "utf8"); return "present"; } catch { return "MISSING"; } })()));
o.push(say("procs", fs.readdirSync("/proc").filter((d) => /^\d+$/.test(d)).length));
const ip = (await import("node:child_process")).spawnSync("ip", ["-o", "link"]);
o.push(say("netlink", ip.error ? "NO_IP" : ip.status === 0 ? "OPEN" : "refused"));
o.push(say("tunables", (() => { try { fs.writeFileSync("/proc/sys/kernel/sysrq", "1"); return "WRITABLE"; } catch (e) { return e.code; } })()));
o.push(say("cfgdir", process.env.PUNKTFUNK_CONFIG_DIR ?? "UNSET"));
o.push(say("sock", process.env.PUNKTFUNK_MGMT_UNIX ?? "UNSET"));
console.log("PROBE " + o.join(" "));
JS

chmod -R go-w "$CFG"
LOG=$(mktemp)
timeout 60 bun /runner.js --plugins "$CFG/plugins" --scripts /nonexistent > "$LOG" 2>&1 &
RUNNER_PID=$!
cleanup() {
  kill "$RUNNER_PID" 2>/dev/null || true
  wait "$RUNNER_PID" 2>/dev/null || true
}
trap cleanup EXIT

line=""
for _ in $(seq 1 100); do
  line=$(grep -m1 '^PROBE ' "$LOG" || true)
  [ -n "$line" ] && break
  sleep 0.1
done
[ -n "$line" ] || { echo "FAIL: the plugin never started"; tail -20 "$LOG"; exit 1; }

# The same runner must notice the atomic grant rewrite and restart only this plugin.
printf '{"probe":["/root/granted","/root/dynamic"]}' > "$CFG/plugin-run/plugin-grants.json.tmp"
mv "$CFG/plugin-run/plugin-grants.json.tmp" "$CFG/plugin-run/plugin-grants.json"
dynamic_line=""
for _ in $(seq 1 50); do
  dynamic_line=$(grep '^PROBE ' "$LOG" | grep 'dynamic=dynamic-data' | tail -1 || true)
  [ -n "$dynamic_line" ] && break
  sleep 0.1
done

pass=0; fail=0
want() { # label, key, expected
  got=$(echo "$line" | tr ' ' '\n' | grep "^$2=" | cut -d= -f2-)
  if [ "$got" = "$3" ]; then echo "  ok   $1"; pass=$((pass+1));
  else echo "  FAIL $1 -> $2=$got (want $3)"; fail=$((fail+1)); fi
}

echo "== what a sandboxed plugin can reach"
want "the admin token is not there"      mgmt        blocked
want "~/.ssh is not there"               ssh         blocked
want "the declared root IS there"        declared    library-data
want "the declared root is READ-ONLY"    declared_ro readonly
want "the granted root IS there"         granted     granted-data
want "the granted root is READ-ONLY"     granted_write EROFS
want "an ungranted root is absent"        dynamic     UNREACHABLE
if [ -n "$dynamic_line" ]; then
  echo "  ok   a live grant restarts the plugin with the new root"; pass=$((pass+1))
else
  echo "  FAIL the live grant was not visible within 5 seconds"; fail=$((fail+1))
  tail -20 "$LOG"
fi
restart_count=$(grep -c '\[runner\] probe: folder access changed — restarting' "$LOG" || true)
if [ "$restart_count" -eq 1 ]; then
  echo "  ok   the grant caused exactly one targeted restart"; pass=$((pass+1))
else
  echo "  FAIL targeted restart count is $restart_count (want 1)"; fail=$((fail+1))
fi
want "its own state dir IS writable"     state       writable
# The file must land in the plugin's state dir on the host, not one level below it.
if [ -f "$CFG/plugin-state/probe/w" ]; then echo "  ok   state lands in plugin-state/probe"; pass=$((pass+1))
else echo "  FAIL state did not land in plugin-state/probe"; fail=$((fail+1)); fi
want "its own token IS there"            owntoken    present
want "HOME is the real home"             homedir     /root
want "the config dir is set"             cfgdir      /run/punktfunk
want "the host socket is set"            sock        /run/punktfunk/host.sock
echo "== the namespace holds"
# Exactly two: bwrap's own init as pid 1, the plugin as pid 2. The point is the count does not
# grow with the host's process list — a shared /proc here would be hundreds.
want "only the sandbox's own processes"  procs       2
# The runner's unit allows both for bwrap's own setup; the plugin inside gets neither.
want "netlink is refused"                netlink     refused
want "kernel tunables are read-only"     tunables    EROFS

echo "== the capability probe agrees with reality"
if grep -q 'cannot be sandboxed' "$LOG"; then
  echo "  FAIL the probe called this box incapable while the sandbox worked"; fail=$((fail+1))
else echo "  ok   the probe called this box capable"; pass=$((pass+1)); fi

echo "$pass passed, $fail failed"
[ "$fail" -eq 0 ]
