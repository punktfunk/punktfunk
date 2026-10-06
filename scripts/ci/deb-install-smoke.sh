#!/usr/bin/env bash
# Install the published .debs on every supported apt distro, in a pristine image, the way a user
# would — repo key, sources.list line, `apt-get install`.
#
# WHY THIS EXISTS: nothing in deb.yml ever installed a package it built. Two facts survived a long
# time in that blind spot, in opposite directions:
#   * `punktfunk-host` installed cleanly on Debian 13 for months while docs-site said Debian was
#     unsupported and unverified.
#   * `punktfunk-gamescope` was missing from the apt registry across two releases, while the docs
#     told Debian/Ubuntu users to `apt install` it.
# Both are what a five-minute install check catches.
#
# Usage:  bash scripts/ci/deb-install-smoke.sh
#   PF_APT_DISTRIBUTION   stable | canary   (default: stable)
#   PF_EXPECT_VERSION     if set, `punktfunk-host --version` must contain it — proves the run is
#                         testing the artifact THIS run published and not a leftover from an
#                         earlier one (the failure mode that makes a green check meaningless).
#   PF_SMOKE_IMAGES       override the image list, space-separated (local runs / bisecting).
set -euo pipefail

DIST="${PF_APT_DISTRIBUTION:-stable}"
EXPECT="${PF_EXPECT_VERSION:-}"
REPO_URL="https://git.unom.io/api/packages/unom/debian"

# THE SUPPORT MATRIX, as an assertion instead of a paragraph. Each row is
# "<image>|<packages that must install>". A package absent from a row is one we do NOT claim works
# there, and it is deliberately NOT asserted absent — that would turn every future improvement into
# a red build. The omissions and their reasons:
#   ubuntu:24.04   no client    — built on 26.04, floors at libc6 >= 2.43 (noble has 2.39)
#                  no gamescope — noble's wayland is 1.22.0; the vendored wlroots needs >= 1.23.1,
#                                 so the binary cannot even load there
#   debian:trixie  no client    — same libc6 >= 2.43 floor (trixie has 2.41), plus GTK4 >= 4.20
# Debian 12 (bookworm) is absent entirely: glibc 2.36 is below the host's 2.39 floor, so nothing
# we ship installs there and there is nothing to smoke-test.
MATRIX=(
  "ubuntu:24.04|punktfunk-host punktfunk-web punktfunk-scripting punktfunk-seats"
  "ubuntu:26.04|punktfunk-host punktfunk-web punktfunk-scripting punktfunk-seats punktfunk-client punktfunk-gamescope"
  "debian:trixie|punktfunk-host punktfunk-web punktfunk-scripting punktfunk-seats punktfunk-gamescope"
)

if [ -n "${PF_SMOKE_IMAGES:-}" ]; then
  FILTERED=()
  for row in "${MATRIX[@]}"; do
    for want in $PF_SMOKE_IMAGES; do
      [ "${row%%|*}" = "$want" ] && FILTERED+=("$row")
    done
  done
  MATRIX=("${FILTERED[@]}")
fi

echo "smoke-installing from '$DIST'${EXPECT:+ (expecting host version $EXPECT)}"
FAILED=()

for row in "${MATRIX[@]}"; do
  IMAGE="${row%%|*}"
  PACKAGES="${row#*|}"
  echo
  echo "==================== $IMAGE ===================="
  echo "packages: $PACKAGES"

  # `docker run` inherits nothing from this shell — every value the container needs is passed
  # explicitly, so a typo here is a hard failure rather than a silently empty variable.
  if docker run --rm --platform linux/amd64 \
        -e DEBIAN_FRONTEND=noninteractive \
        -e DIST="$DIST" -e PACKAGES="$PACKAGES" -e EXPECT="$EXPECT" -e REPO_URL="$REPO_URL" \
        "$IMAGE" bash -euxc '
          apt-get update -qq
          apt-get install -y -qq --no-install-recommends curl ca-certificates
          install -d -m 0755 /etc/apt/keyrings
          curl -fsSL --max-time 60 "$REPO_URL/repository.key" -o /etc/apt/keyrings/punktfunk.asc
          echo "deb [signed-by=/etc/apt/keyrings/punktfunk.asc] $REPO_URL $DIST main" \
            > /etc/apt/sources.list.d/punktfunk.list
          apt-get update -qq

          # Wait for the index to actually carry the version this run published. Gitea regenerates
          # the apt Packages file after an upload, so a smoke job that starts immediately can see
          # the PREVIOUS build — install it, pass, and prove nothing about the new one. Bounded:
          # if it never appears, that is a real publishing failure and the job should say so.
          if [ -n "$EXPECT" ]; then
            for i in $(seq 1 10); do
              apt-cache policy punktfunk-host | grep -qF "$EXPECT" && break
              echo "index does not carry $EXPECT yet (attempt $i) — waiting"
              sleep 15
              apt-get update -qq
            done
            apt-cache policy punktfunk-host | grep -qF "$EXPECT" || {
              echo "the apt index never served $EXPECT — publish did not land"
              apt-cache policy punktfunk-host
              exit 1
            }
          fi

          # The real thing: unpack + run every maintainer script, exactly as a user would.
          apt-get install -y $PACKAGES

          # Installed is not the same as working. Assert every shipped binary RESOLVED its shared
          # libraries and, where it is safe to run headless, that it executes — an unsatisfied
          # soname is invisible to dpkg but fatal to the user, and it is exactly what a
          # distro-mismatched build produces.
          # (`if ldd | grep; then fail` rather than `grep && exit 1`: the latter leaves the block
          # returning grep NOT-found = 1, which under `set -e` fails the container on success.)
          for pkg in $PACKAGES; do
            for bin in $(dpkg -L "$pkg" | grep -E "^/usr/(bin|libexec/punktfunk)/" || true); do
              if ldd "$bin" 2>/dev/null | grep -F "not found"; then
                echo "UNRESOLVED SONAME in $bin (from $pkg)"
                exit 1
              fi
            done
          done
          # --version is the cheapest proof of "actually runs". Only for binaries that answer it
          # without a session/GPU: the client opens GTK, the console is a bun bundle.
          if echo "$PACKAGES" | grep -q punktfunk-host; then
            punktfunk-host --version
            [ -z "$EXPECT" ] || punktfunk-host --version | grep -F "$EXPECT"
          fi
          if echo "$PACKAGES" | grep -q punktfunk-gamescope; then
            punktfunk-gamescope --version
          fi
          # The console and the runner both exec this one private bun.
          if echo "$PACKAGES" | grep -qE "punktfunk-(web|scripting)"; then
            /usr/lib/punktfunk-bun/bun --version
          fi
        '; then
    echo "PASS: $IMAGE"
  else
    echo "::error::$IMAGE — the published packages do not install ($PACKAGES)"
    FAILED+=("$IMAGE")
  fi
done

echo
if [ ${#FAILED[@]} -gt 0 ]; then
  echo "install smoke FAILED on: ${FAILED[*]}"
  exit 1
fi
echo "install smoke passed on every supported distro"
