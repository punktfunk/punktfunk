#!/bin/sh
# Which ci.yml buckets a PR's changed paths can break. Reads a file of paths, writes
# `bucket=true|false` lines for $GITHUB_OUTPUT. Buckets are coarse and must fail towards
# true: an unlisted path that belongs in a bucket is a break that merges green.
set -eu

cd "$(dirname "$0")/../.." || exit 2

classify() {
    files=$1
    rust=false
    rust_arm64=false
    web=false
    docs_site=false
    sdk_plugin_kit=false
    decky_typecheck=false

    if [ "${CHANGED_ALL:-0}" = 1 ]; then
        rust=true
        rust_arm64=true
        web=true
        docs_site=true
        sdk_plugin_kit=true
        decky_typecheck=true
    else
        while IFS= read -r path; do
            case "$path" in
                .gitea/workflows/ci.yml|scripts/ci/changed.sh)
                    rust=true
                    rust_arm64=true
                    web=true
                    docs_site=true
                    sdk_plugin_kit=true
                    decky_typecheck=true
                    ;;
            esac
            case "$path" in
                .cargo/*|Cargo.toml|Cargo.lock|rust-toolchain.toml|rustfmt.toml|\
                crates/*|tools/*|clients/cli/*|clients/linux/*|clients/probe/*|clients/session/*|\
                clients/shared/*|clients/android/native/*|clients/apple/native/*|include/*|\
                api/openapi.json|\
                data/platforms.json|ci/rust-ci.Dockerfile|web/src/lib/command-execution.ts|\
                scripts/ci/ensure-sccache.sh|scripts/ci/install-retrying-curl.sh|\
                scripts/ci/check-installer-behavior.sh|scripts/ci/check-install-defaults.sh|\
                scripts/ci/check-unsafe-hygiene.sh|\
                scripts/ci/check-workflow-paths.py|scripts/ci/cargo_graph.py|\
                scripts/gen-third-party-notices.sh|scripts/gen-third-party-notices.py|\
                about.toml|about.hbs|\
                assets/os-icons/LICENSES/*|assets/launcher-icons/LICENSES/*|\
                THIRD-PARTY-NOTICES.txt|clients/windows/THIRD-PARTY-NOTICES.txt|\
                clients/apple/Sources/PunktfunkKit/Resources/THIRD-PARTY-NOTICES.txt|\
                clients/android/app/src/main/assets/THIRD-PARTY-NOTICES.txt)
                    rust=true
                    ;;
            esac
            case "$path" in
                .cargo/*|Cargo.toml|Cargo.lock|rust-toolchain.toml|rustfmt.toml|\
                clients/linux/*|clients/session/*|clients/shared/*|\
                crates/punktfunk-core/*|crates/punktfunk-ffi/*|\
                crates/pf-bitstream/*|crates/pf-client-core/*|crates/pf-console-ui/*|\
                crates/pf-dmabuf/*|crates/pf-dxvadec/*|crates/pf-libva/*|crates/pf-presenter/*|\
                crates/pf-update-check/*|crates/pf-vaapi/*|crates/pf-v4l2dec/*|crates/pf-v4l2/*|crates/pf-vkdecode/*|\
                crates/pyrowave-sys/*|ci/rust-ci-arm64cross.Dockerfile|\
                scripts/ci/ensure-sccache.sh|scripts/ci/install-retrying-curl.sh)
                    rust_arm64=true
                    ;;
            esac
            case "$path" in
                web/*|api/openapi.json|scripts/ci/retry.sh|clients/shared/library-id-vectors.json|\
                crates/punktfunk-host/src/library/custom.rs|plugin-kit/src/wire.ts|\
                crates/punktfunk-core/testdata/grant-vectors.json|\
                crates/punktfunk-core/testdata/gamepad-button-vectors.json)
                    web=true
                    ;;
            esac
            case "$path" in
                docs-site/*|scripts/ci/retry.sh)
                    docs_site=true
                    ;;
            esac
            case "$path" in
                sdk/*|plugin-kit/*|api/openapi.json|scripts/ci/retry.sh|\
                clients/shared/library-id-vectors.json|\
                crates/punktfunk-host/testdata/path-refusal-vectors.json)
                    sdk_plugin_kit=true
                    ;;
            esac
            case "$path" in
                clients/decky/*)
                    decky_typecheck=true
                    ;;
            esac
        done < "$files"
    fi

    printf '%s\n' \
        "rust=$rust" \
        "rust_arm64=$rust_arm64" \
        "web=$web" \
        "docs_site=$docs_site" \
        "sdk_plugin_kit=$sdk_plugin_kit" \
        "decky_typecheck=$decky_typecheck"
}

self_test() {
    tmp="${TMPDIR:-/tmp}/changed-self-test.$$"
    trap 'rm -f "$tmp"' EXIT

    check() {
        name=$1
        paths=$2
        want=$3
        printf '%s\n' "$paths" > "$tmp"
        got=$(classify "$tmp" | tr '\n' ' ')
        if [ "$got" != "$want " ]; then
            echo "changed self-test $name: got '$got', want '$want'" >&2
            exit 1
        fi
    }

    check docs-site 'docs-site/src/app.tsx' \
        'rust=false rust_arm64=false web=false docs_site=true sdk_plugin_kit=false decky_typecheck=false'
    check packaging 'packaging/linux/omarchy/plugin/Panel.qml' \
        'rust=false rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check android 'clients/android/app/build.gradle.kts' \
        'rust=false rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    # A workspace member with no cfg gate: workspace clippy compiles it on Linux, android.yml
    # (cargo-ndk, android cfg) does not stand in for that.
    check android-native 'clients/android/native/src/lib.rs' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    # The console header's drift gate runs in the rust job.
    check apple-console 'clients/apple/native/src/console.rs' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check notice-config 'about.toml' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check notice-template 'about.hbs' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check host 'crates/punktfunk-host/src/main.rs' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check workspace-tool 'tools/loss-harness/src/main.rs' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check notice-generator 'scripts/gen-third-party-notices.py' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check workflow-paths 'scripts/ci/cargo_graph.py' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check notice-license 'assets/os-icons/LICENSES/simple-icons.txt' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check client-shared 'clients/shared/deeplink-vectors.json' \
        'rust=true rust_arm64=true web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check library-id-vectors 'clients/shared/library-id-vectors.json' \
        'rust=true rust_arm64=true web=true docs_site=false sdk_plugin_kit=true decky_typecheck=false'
    # The host's unprivileged launch kinds and the console's copy: each side's test reads the other.
    check launch-kinds-console 'web/src/lib/command-execution.ts' \
        'rust=true rust_arm64=false web=true docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check launch-kinds-host 'crates/punktfunk-host/src/library/custom.rs' \
        'rust=true rust_arm64=false web=true docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check grant-vectors 'crates/punktfunk-core/testdata/grant-vectors.json' \
        'rust=true rust_arm64=true web=true docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check gamepad-button-vectors 'crates/punktfunk-core/testdata/gamepad-button-vectors.json' \
        'rust=true rust_arm64=true web=true docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    # The console types the plugin surfaces from the kit's wire.ts.
    check plugin-surface 'plugin-kit/src/wire.ts' \
        'rust=false rust_arm64=false web=true docs_site=false sdk_plugin_kit=true decky_typecheck=false'
    # The runner's sandbox replays the host's path refusals.
    check path-refusal-vectors 'crates/punktfunk-host/testdata/path-refusal-vectors.json' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=true decky_typecheck=false'
    check openapi 'api/openapi.json' \
        'rust=true rust_arm64=false web=true docs_site=false sdk_plugin_kit=true decky_typecheck=false'
    check platforms 'data/platforms.json' \
        'rust=true rust_arm64=false web=false docs_site=false sdk_plugin_kit=false decky_typecheck=false'
    check workflow '.gitea/workflows/ci.yml' \
        'rust=true rust_arm64=true web=true docs_site=true sdk_plugin_kit=true decky_typecheck=true'

    echo "changed: self-test passed"
}

case "${1:-}" in
    --self-test)
        self_test
        ;;
    "")
        echo "usage: $0 <changed-paths-file> | --self-test" >&2
        exit 2
        ;;
    *)
        classify "$1"
        ;;
esac
