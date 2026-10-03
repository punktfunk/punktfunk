# `punktfunk-gamescope` — nixpkgs' gamescope carrying punktfunk's `pipewire-hdr` patches, exposed
# under its own name so it sits BESIDE the system gamescope instead of replacing it.
#
# An override rather than a from-scratch derivation on purpose: gamescope vendors wlroots,
# vkroots, libliftoff, libdisplay-info, SPIRV-Headers and reshade as git submodules plus two meson
# wraps, and nixpkgs already solves all of that. What we add is the patch set and a rename.
#
# This is also why the override needs none of the `force_fallback_for` armour
# `build-punktfunk-gamescope.sh` carries: that exists because meson silently prefers a SYSTEM
# wlroots when the build host has one and links it shared, producing a binary that starts only on
# machines with that dev library. A nix closure names every library it links, so the outcome is
# whatever nixpkgs' own gamescope already does — reproducibly.
#
# `gamescope` in nixpkgs is a wrapper (it wires the WSI layer + capabilities); the buildable
# derivation is `gamescope.unwrapped` — patching the wrapper would be a no-op, so this asserts on
# it rather than silently shipping an unpatched binary.
#
# `gamescope` is the recipe from the flake's pinned nixpkgs, called with the system's libraries
# (flake.nix). Its NixOS patches and postPatch match the source pinned below; another release's
# recipe patches files our source does not have, and nixos-26.05's fails at patchPhase.
#
# ⚠️ Kept deliberately free of any dependency on the pinned rev. The pin moved past upstream's
# `vulkan_get_rgb10_capture_format()` (`ff6b924`, after 3.16.25) to fix red/blue on NVIDIA, and it
# would have been natural to have patch `0001` call it — that is what the host-side note in
# `crates/pf-capture/src/linux/pw_pods.rs` proposes. It does NOT, precisely so this derivation
# keeps building against a nixpkgs that still pins 3.16.25, where that symbol does not exist and
# the failure would be an opaque C++ error rather than a patch conflict. Patch `0001` gets the
# same outcome version-independently by offering `xBGR_210LE` ahead of `xRGB_210LE`.
{
  lib,
  gamescope,
  fetchFromGitHub,
  python3,
  patchDir,
  manifestRewriter,
}:
let
  # PIN THE COMPOSITOR SOURCE, rather than patching whatever gamescope nixpkgs happens to carry.
  # Every other channel already ships this exact commit — packaging/gamescope/README.md,
  # punktfunk-gamescope.spec, the PKGBUILD and build-punktfunk-gamescope.sh — and nix was the
  # only one tracking nixpkgs' version and hoping ten patches still applied.
  #
  # They did not, and the failures were not academic (MEASURED 2026-08-19/20):
  #   * nixpkgs shipped 3.16.24 and patch 0009's context did not exist there at all, so the
  #     build died at patchPhase — every `services.punktfunk.host.enable = true` with it.
  #   * bumping the lock to 3.16.25 fixed that, then `--version` printed NOTHING: upstream's
  #     `gamescope::PrintVersion()` landed AFTER the 3.16.25 tag. The host reads that banner to
  #     decide a session's bit depth and cursor compositing BEFORE the virtual display exists,
  #     so a silent banner means a silent fall back to SDR — the exact failure every guard in
  #     this file is written to prevent.
  # Both are the same bug: nixpkgs' gamescope is older than the tree these patches target.
  # Pinning makes the nix package agree with every other channel byte for byte.
  #
  # Bumping this: move the rev, then `nix-prefetch-git --url https://github.com/ValveSoftware/gamescope
  # --rev <new> --fetch-submodules` for the hash, and keep packaging/gamescope/README.md in step.
  pfRev = "6867f509874f9bc52e12d6f4c4596cdf0d5be6b4";
  pfVersion = "3.16.31";
  pfSrc = fetchFromGitHub {
    owner = "ValveSoftware";
    repo = "gamescope";
    rev = pfRev;
    fetchSubmodules = true;
    hash = "sha256-mCCf/CfKBqxNvmRbaAJsOMrnnRutPr9nEePHmAq/PUM=";
  };
  # As of nixos-unstable (checked 2026-07-28) `gamescope` IS the buildable derivation — pname
  # "gamescope", version 3.16.25, carrying `src`/`patches`/`mesonFlags`. Revisions that wrap it
  # (to wire the WSI layer + capabilities) expose the build as `.unwrapped`, so prefer that where
  # it exists and take `gamescope` itself otherwise.
  #
  # The check is on the RESULT, not on which attribute we found: `overrideAttrs` on a symlinkJoin
  # wrapper succeeds and does nothing, which would hand us an UNPATCHED gamescope installed under
  # our own name — the single worst outcome here, because the host reads the name as a promise of
  # HDR. (`installCheckPhase` below greps for the marker as the second line of defence; this one
  # fails at eval, before anything is built.)
  # `enableWsi = true` is NOT optional here, and it is a FUNCTION ARGUMENT — `overrideAttrs`
  # cannot reach it. nixpkgs defaults `enableWsi ? false` and feeds it to
  # `mesonBool "enable_gamescope_wsi_layer"`, so the plain derivation installs the compositor and
  # no layer at all; nixpkgs gets its layer by instantiating a SECOND copy inside the wrapper.
  # Without the override the build gets all the way through compile, link and install before
  # postInstall's find turns up nothing and fails with "built no WSI layer" (MEASURED 2026-08-19,
  # run 19323) — an expensive way to discover a default.
  #
  # `.override` before `.overrideAttrs`: the former re-invokes the package function with the new
  # argument, so the latter must come after or it would be applied to the derivation being
  # replaced. The `? override` test only skips the call for something that is not overridable at
  # all (a symlinkJoin) — it does NOT make an unknown argument safe: a nixpkgs whose gamescope
  # dropped `enableWsi` fails at EVAL with "function has no argument named 'enableWsi'". That is
  # the right failure. It names the cause outright, and it costs nothing, where the alternative is
  # discovering the same fact after a full compositor build.
  raw = gamescope.unwrapped or gamescope;
  base = if raw ? override then raw.override { enableWsi = true; } else raw;
  unwrapped =
    if base ? src then
      base
    else
      throw ''
        punktfunk-gamescope needs a buildable gamescope derivation (one with a `src` that
        `overrideAttrs` can patch); this nixpkgs' `gamescope` is neither that nor a wrapper
        exposing `.unwrapped`. Update nixpkgs, or build the compositor with
        packaging/gamescope/build-punktfunk-gamescope.sh instead.
      '';
in
unwrapped.overrideAttrs (old: {
  pname = "punktfunk-gamescope";
  version = pfVersion;
  src = pfSrc;

  # Read the patch DIRECTORY rather than naming files: `builtins.attrNames` sorts
  # lexicographically, which for `000N-` prefixes is exactly the apply order, and a patch added or
  # renamed upstream of this file can no longer leave nix silently building a subset. (That already
  # happened once — this list still named the level-1 banner patch after level 2 landed, so a nix
  # build would have shipped a binary with no cursor patch and a marker claiming otherwise.)
  patches =
    (old.patches or [ ])
    ++ map (f: "${patchDir}/${f}") (
      builtins.filter (lib.hasSuffix ".patch") (builtins.attrNames (builtins.readDir patchDir))
    );

  # nixpkgs builds from a `fetchFromGitHub` src, so there is no `.git` for `git describe` and the
  # banner would read `+pfhdrN (gcc …)` with no version at all — which the host's diagnostic
  # version gate then misreads (it takes the first X.Y.Z triple it finds, i.e. the compiler's).
  # Substituting the real version in keeps `--version` honest AND keeps our marker.
  postPatch = (old.postPatch or "") + ''
    substituteInPlace src/meson.build \
      --replace-fail \
        "vcs_tag = run_command(vcs_tag_cmd, check: false).stdout().strip()" \
        "vcs_tag = '${pfVersion}'"

    # Source-level gate, the same one packaging/gamescope/build-punktfunk-gamescope.sh applies.
    # Splits a missing marker into its two possible stages: fire HERE and patch 0005 or the
    # substitution above lost it; pass here and fail the ELF check later, and it was lost in
    # meson configuration or compilation instead. Without this the two are indistinguishable,
    # at a full compositor build per guess.
    grep -q '+pfhdr' src/meson.build || {
      echo "punktfunk-gamescope: +pfhdr is not in src/meson.build after patching" >&2
      echo "  --- version block as patched: ---" >&2
      sed -n '/^vcs_tag_cmd/,/^gamescope_version_conf/p' src/meson.build | sed 's/^/    | /' >&2
      exit 1
    }
  '';

  # Ship the compositor, renamed, its reaper, AND the WSI layer built beside it. gamescope starts
  # its primary child through `gamescopereaper`, which it resolves next to /proc/self/exe — the
  # real gamescope package on PATH cannot answer that, so without the reaper here a spawned
  # session comes up empty and logs "Primary child shut down!". The rest (gamescopectl,
  # gamescopestream, .desktop files) is the real package's to install.
  #
  # The layer is not dressing: a game nested under this compositor gets its HDR10 swapchain from it
  # or from nowhere, and a layer built for a DIFFERENT gamescope makes the compositor reject the
  # client's swapchain_feedback and kills every Vulkan client. So it travels with the binary it was
  # built against. It is renamed and re-homed under $out/lib/punktfunk, with its own enable
  # variable, so it sits beside the system gamescope's layer rather than shadowing it — the Vulkan
  # loader deduplicates implicit layers by name, so two of the same name would be a coin toss.
  #
  # Staged through $TMPDIR because the prune below removes $out/lib and $out/share wholesale.
  postInstall = (old.postInstall or "") + ''
    layerSo=$(find $out -type f -name 'libVkLayer_*gamescope_wsi*.so' | head -1)
    layerJson=$(find $out -type f -name '*gamescope_wsi*.json' | head -1)
    if [ -z "$layerSo" ] || [ -z "$layerJson" ]; then
      echo "punktfunk-gamescope: this nixpkgs' gamescope built no WSI layer, so no game under the" >&2
      echo "                     compositor could ever obtain an HDR10 swapchain" >&2
      exit 1
    fi
    cp "$layerSo" "$TMPDIR/pf-layer.so"
    ${python3}/bin/python3 ${manifestRewriter} \
      "$layerJson" "$TMPDIR/pf-layer.json" \
      "$out/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"

    # gamescope's own default_extras_install.sh lays the reshade shaders and textures down under
    # read-only DIRECTORIES (mode 555). `rm` needs write permission on the CONTAINING directory,
    # not on the file, so the prune below otherwise dies with "Permission denied" on every one of
    # them — after a full compositor build, with the compositor itself already installed and the
    # log looking finished. MEASURED 2026-08-19 (run 19341), reachable only once the enableWsi
    # override got the build past the layer assertion above. Nix seals $out read-only after the
    # builder exits, so widening it here costs nothing and changes nothing in the output.
    chmod -R u+w $out

    find $out -mindepth 1 -maxdepth 1 ! -name bin -exec rm -rf {} +
    # KEEP `.gamescope-wrapped`. nixpkgs wraps this package: makeWrapper leaves the real
    # compositor ELF at bin/.gamescope-wrapped and installs a small launcher at bin/gamescope
    # that sets PATH (xwininfo) before exec'ing it. A prune that keeps only `gamescope` deletes
    # the compositor and ships the launcher alone — MEASURED 2026-08-20 (run 19622): $out/bin
    # held a single 16 KB file, `--version` printed nothing because the launcher exec'd a path
    # that no longer existed, and no +pfhdr marker was present because a wrapper carries no
    # version string. Every symptom chased for three builds came from this one line.
    #
    # The launcher references its target by ABSOLUTE path, so renaming the launcher is safe
    # while the target keeps its name.
    find $out/bin -mindepth 1 ! -name gamescope ! -name '.gamescope-wrapped' \
      ! -name gamescopereaper -delete
    mv $out/bin/gamescope $out/bin/punktfunk-gamescope

    install -Dm0755 "$TMPDIR/pf-layer.so" \
      "$out/lib/punktfunk/libVkLayer_PUNKTFUNK_gamescope_wsi.so"
    install -Dm0644 "$TMPDIR/pf-layer.json" \
      "$out/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json"
  '';

  # `gamescope --version` exits non-zero on some builds; the grep is the real assertion.
  doInstallCheck = true;
  installCheckPhase = ''
    runHook preInstallCheck
    # Assert the marker is compiled INTO the shipped binary, rather than running it.
    #
    # Running it does not work here and never did: `--version` produced EMPTY output under the
    # build sandbox on BOTH nixpkgs' 3.16.25 and the pinned 5fb8dce4 (MEASURED 2026-08-19/20,
    # runs 19551 / 19573 / 19594). That is a property of the sandbox, not a defect in the binary:
    # gamescope calls PrintVersion() before the getopt loop (src/main.cpp:721 at the pinned rev),
    # so `gamescope --version` DOES print the banner on a real system — which is what the host's
    # capability probe reads.
    #
    # packaging/gamescope/build-punktfunk-gamescope.sh makes the same call, asserting on
    # src/meson.build. Grepping the installed ELF is strictly stronger: the version string reaches
    # .rodata through GamescopeVersion.h's k_szGamescopeVersion, so this proves the marker survived
    # patching, meson configuration AND compilation into the artifact we actually ship, and it
    # cannot be defeated by the binary being unable to start.
    # Grep the WRAPPED ELF: bin/punktfunk-gamescope is nixpkgs' launcher and carries no version
    # string at all, so asserting on it would pass only by accident. Fall back to the launcher
    # for a future nixpkgs that stops wrapping.
    gsElf=$out/bin/.gamescope-wrapped
    [ -f "$gsElf" ] || gsElf=$out/bin/punktfunk-gamescope
    grep -aq '+pfhdr' "$gsElf" || {
      echo "punktfunk-gamescope: the +pfhdr marker is not in the installed binary." >&2
      echo "  src/meson.build carried it (asserted in postPatch), so it was lost between" >&2
      echo "  meson configuration and the linked artifact. Evidence:" >&2
      echo "  --- $out/bin ---" >&2
      ls -l $out/bin 2>&1 | sed 's/^/    | /' >&2
      echo "  --- anything under $out mentioning pfhdr ---" >&2
      grep -ral 'pfhdr' $out 2>/dev/null | sed 's/^/    | /' >&2 || echo "    | (nothing)" >&2
      echo "  --- version-ish strings in the binary ---" >&2
      grep -aoE '[0-9]+\.[0-9]+\.[0-9]+[^ ]*' "$gsElf" 2>/dev/null \
        | sort -u | head -5 | sed 's/^/    | /' >&2 || true
      exit 1
    }
    # The compositor spawns its child through this, by a path next to /proc/self/exe. Missing, it
    # still starts, still paints, and never runs the game.
    [ -x $out/bin/gamescopereaper ] \
      || { echo "punktfunk-gamescope: no gamescopereaper, so a spawned session would run nothing"; exit 1; }
    # The manifest must name a library this derivation actually installed. A manifest pointing at a
    # path that does not exist is the worst shape of this bug: the loader reads it, finds nothing,
    # and carries on silently, so the box looks healthy and every game renders SDR.
    lib=$(sed -n 's/.*"library_path"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' \
      $out/lib/punktfunk/vulkan/implicit_layer.d/punktfunk_gamescope_wsi.json)
    [ -f "$lib" ] \
      || { echo "punktfunk-gamescope: the layer manifest points at $lib, which is not installed"; exit 1; }
    runHook postInstallCheck
  '';

  meta = (old.meta or { }) // {
    description = "gamescope with 10-bit BT.2020/PQ PipeWire capture, for punktfunk HDR streaming";
    mainProgram = "punktfunk-gamescope";
  };
})
