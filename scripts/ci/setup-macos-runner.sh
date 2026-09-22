#!/usr/bin/env bash
# Provision a Mac as the Gitea Actions runner for the Apple client CI
# (.gitea/workflows/apple.yml). Idempotent — safe to re-run. Run ON THE MAC, or from a
# dev box:
#
#   ssh <mac> GITEA_RUNNER_TOKEN=<registration token> bash -s < scripts/ci/setup-macos-runner.sh
#
# Installs: rustup (+ both darwin targets for the universal xcframework), Node.js (the
# runner executes JS actions like actions/checkout via `node` from PATH — host mode does
# not auto-provision it), the act_runner binary (host mode — jobs run directly on macOS,
# no containers), and a root LaunchDaemon that keeps the runner daemon alive (see the
# launchd section for why it can't be a user LaunchAgent). Registration only happens once
# (.runner file); the token is NOT persisted by this script.
#
# Env knobs: GITEA_INSTANCE (default https://git.unom.io), GITEA_RUNNER_TOKEN (required
# for first-time registration only), RUNNER_NAME (default: LocalHostName), RUNNER_LABELS
# (default macos-arm64:host — matches apple.yml's runs-on), ACT_RUNNER_VERSION,
# NODE_VERSION.
#
# NOT installed here: Xcode. swift build/test work with Command Line Tools, but
# scripts/build-xcframework.sh needs xcodebuild (-create-xcframework) from a full Xcode.
set -euo pipefail

INSTANCE="${GITEA_INSTANCE:-https://git.unom.io}"
VERSION="${ACT_RUNNER_VERSION:-1.0.8}"
RUNNER_NAME="${RUNNER_NAME:-$(scutil --get LocalHostName)}"
LABELS="${RUNNER_LABELS:-macos-arm64:host}"
RUNNER_HOME="$HOME/ci/act-runner"
BIN_DIR="$HOME/.local/bin"

# --- Rust toolchain (the xcframework is built from the Rust core) -----------------------
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
    echo "==> installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal
fi
"$HOME/.cargo/bin/rustup" target add aarch64-apple-darwin

# --- Node.js (actions runtime; sudo-free tarball install) --------------------------------
NODE_VERSION="${NODE_VERSION:-22.22.3}"
mkdir -p "$BIN_DIR"
if ! "$BIN_DIR/node" --version 2>/dev/null | grep -q "^v${NODE_VERSION}$"; then
    echo "==> installing node v$NODE_VERSION"
    NODE_DIR="$HOME/.local/node-v$NODE_VERSION"
    mkdir -p "$NODE_DIR"
    curl -fL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-darwin-arm64.tar.gz" \
        | tar -xz --strip-components=1 -C "$NODE_DIR"
    ln -sf "$NODE_DIR/bin/node" "$BIN_DIR/node"
fi
"$BIN_DIR/node" --version

# --- act_runner binary -------------------------------------------------------------------
# Renamed upstream to "gitea-runner" as of 1.0 (dl.gitea.com/act_runner/ stops at 0.6.x);
# we keep the local name act_runner — the CLI surface is unchanged.
mkdir -p "$BIN_DIR" "$RUNNER_HOME"
if ! "$BIN_DIR/act_runner" --version 2>/dev/null | grep -q "$VERSION"; then
    echo "==> installing act_runner (gitea-runner) $VERSION"
    curl -fL "https://dl.gitea.com/gitea-runner/${VERSION}/gitea-runner-${VERSION}-darwin-arm64" \
        -o "$BIN_DIR/act_runner.tmp"
    chmod +x "$BIN_DIR/act_runner.tmp"
    mv "$BIN_DIR/act_runner.tmp" "$BIN_DIR/act_runner"
fi
"$BIN_DIR/act_runner" --version

# --- config + one-time registration ------------------------------------------------------
cd "$RUNNER_HOME"
[ -f config.yaml ] || "$BIN_DIR/act_runner" generate-config > config.yaml
# generate-config seeds runner.labels with docker:// defaults, which (a) override the
# host-mode labels registered in .runner and (b) make the daemon demand a Docker engine
# ("Docker Engine socket not found"). Empty them so .runner's labels rule.
sed -i '' -e '/docker.gitea.com\/runner-images/d' \
    -e 's|^\([[:space:]]*\)labels:$|\1labels: []|' config.yaml
if [ ! -f .runner ]; then
    if [ -z "${GITEA_RUNNER_TOKEN:-}" ]; then
        echo "ERROR: not registered yet — re-run with GITEA_RUNNER_TOKEN=<token>" >&2
        echo "       (org unom -> Settings -> Actions -> Runners -> Create new runner)" >&2
        exit 1
    fi
    "$BIN_DIR/act_runner" register --no-interactive \
        --instance "$INSTANCE" \
        --token "$GITEA_RUNNER_TOKEN" \
        --name "$RUNNER_NAME" \
        --labels "$LABELS"
fi

# --- launchd service ---------------------------------------------------------------------
# macOS Local Network privacy (15+) silently denies LAN connections ("no route to host")
# to unbundled CLI binaries in gui/user launchd domains — a user LaunchAgent can NOT reach
# a Gitea instance on the LAN (curl over ssh works, the same dial from the agent fails).
# System-domain daemons are exempt and survive reboots with nobody logged in, so the
# runner ships as a root LaunchDaemon; installing it needs sudo once. Without sudo this
# script still leaves a working (but reboot-volatile) nohup daemon behind.
# PATH must carry the CLT tools, cargo, node and act_runner itself; jobs inherit it.
# Deliberately NO DEVELOPER_DIR here: cargo (rust ld) must stay on the system default —
# a newer-than-OS Xcode's ld emits dylibs the running dyld rejects ("mis-aligned
# LINKEDIT string pool"), breaking every proc-macro build. Steps that need a full Xcode
# (xcodebuild) resolve it themselves (build-xcframework.sh, apple.yml's `distribute` job).

PLIST_STAGE="$RUNNER_HOME/io.gitea.act_runner.plist"
PLIST_SYSTEM="/Library/LaunchDaemons/io.gitea.act_runner.plist"
cat > "$PLIST_STAGE" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>io.gitea.act_runner</string>
    <key>UserName</key><string>$USER</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN_DIR/act_runner</string>
        <string>daemon</string>
        <string>--config</string>
        <string>$RUNNER_HOME/config.yaml</string>
    </array>
    <key>WorkingDirectory</key><string>$RUNNER_HOME</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$HOME/.cargo/bin:$BIN_DIR:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
        <key>HOME</key><string>$HOME</string>
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$RUNNER_HOME/runner.log</string>
    <key>StandardErrorPath</key><string>$RUNNER_HOME/runner.log</string>
</dict>
</plist>
EOF

launchctl bootout "gui/$(id -u)/io.gitea.act_runner" 2>/dev/null || true
if sudo -n true 2>/dev/null; then
    sudo install -m 644 -o root -g wheel "$PLIST_STAGE" "$PLIST_SYSTEM"
    pkill -x act_runner 2>/dev/null || true
    sudo launchctl bootout system/io.gitea.act_runner 2>/dev/null || true
    sudo launchctl bootstrap system "$PLIST_SYSTEM"
    echo "==> runner LaunchDaemon bootstrapped (system domain)"
else
    if ! pgrep -x act_runner >/dev/null; then
        echo "==> no sudo: starting an interim daemon (dies on reboot)"
        (cd "$RUNNER_HOME" && \
            PATH="$HOME/.cargo/bin:$BIN_DIR:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin" \
            nohup "$BIN_DIR/act_runner" daemon --config config.yaml >> runner.log 2>&1 &)
    fi
    echo "==> for the permanent (reboot-safe) runner, run once on the Mac:"
    echo "    sudo install -m 644 -o root -g wheel $PLIST_STAGE $PLIST_SYSTEM"
    echo "    sudo launchctl bootstrap system $PLIST_SYSTEM"
fi

sleep 2
tail -5 "$RUNNER_HOME/runner.log" 2>/dev/null || true

if ! /usr/bin/xcodebuild -version >/dev/null 2>&1 && ! ls -d /Applications/Xcode*.app >/dev/null 2>&1; then
    echo "WARNING: no full Xcode found — the xcframework/release steps need one in"
    echo "         /Applications, with its license accepted once: sudo xcodebuild -license accept"
fi
echo "OK: runner '$RUNNER_NAME' labels=$LABELS instance=$INSTANCE"
