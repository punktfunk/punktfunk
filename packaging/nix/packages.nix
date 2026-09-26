# punktfunk Nix packages — built with crane (github:ipetkov/crane).
#
# Two derivations, mirroring the RPM's `punktfunk` / `punktfunk-client` split (packaging/rpm/
# punktfunk.spec) so a headless streaming host never drags in the GTK client closure:
#
#   * punktfunk-host   — the streaming host + status tray (punktfunk-host, punktfunk-tray).
#                        Built with the same feature set CI ships: the direct-SDK NVENC path
#                        (`nvenc`) + the raw Vulkan-Video HEVC encoder (`vulkan-encode`). Both are
#                        AMD/Intel-safe (NVENC/CUDA entry points are dlopen'd at RUNTIME, never
#                        link-imported), so ONE binary runs on NVIDIA, AMD and Intel.
#   * punktfunk-client — the native Linux client: the GTK4/libadwaita shell (punktfunk-client) plus
#                        the Vulkan/ash session streamer it execs (punktfunk-session).
#
# Why crane and not rustPlatform.buildRustPackage:
#   * The lockfile carries `windows 0.62.2` (and friends) from BOTH crates.io AND a pinned
#     microsoft/windows-rs git rev — the Windows client uses the git one. importCargoLock keys its
#     vendor dir by `name-version`, so those same-name-same-version-different-source entries collide;
#     crane vendors per-source and handles it. (Those crates are `cfg(windows)`-gated and never
#     COMPILE on Linux — they only need to be vendored to satisfy the lock.)
#   * Git checkouts have unpacked hashes so evaluation does not fetch GitHub. This keeps
#     `nix flake check --no-build` offline after its flake inputs are available.
#
# The whole workspace is built from real source (no crane dep-only "dummy" pre-build): pyrowave-sys
# runs CMake over its committed `vendor/pyrowave` tree in build.rs, which a dummy src would omit —
# so `cargoArtifacts = null` keeps deps + crate in one derivation.
{
  lib,
  craneLib,
  src,
  version,
  # Short commit of `src`; null when the tree is dirty on a Nix without `dirtyShortRev`.
  rev ? null,
  # native tooling
  pkg-config,
  cmake,
  nasm,
  perl,
  rustPlatform,
  addDriverRunpath,
  wrapGAppsHook4,
  # host + client shared libs
  pipewire,
  libopus,
  wayland,
  libxkbcommon,
  libGL,
  vulkan-loader,
  libva,
  # gbm lives in `libgbm` on recent nixpkgs, `mesa` on older ones.
  libgbm ? null,
  mesa,
  # client-only libs
  sdl3,
  gtk4,
  libadwaita,
  glib,
  librsvg,
  gsettings-desktop-schemas,
  adwaita-icon-theme,
  # web console (punktfunk-web): a bun-built Nitro SSR bundle, run on bun.
  bun,
  nodejs,
  makeWrapper,
  stdenvNoCC,
  # bun2nix (github:nix-community/bun2nix, pinned in flake.nix): `.fetchBunDeps` turns a generated
  # `bun.nix` into bun's global install cache, and `.hook` runs an offline `bun install` off it.
  bun2nix,
}:
let
  gbm = if libgbm != null then libgbm else mesa;

  # Build provenance stamped into the binary (build.rs reads PUNKTFUNK_BUILD_VERSION for
  # `--version` / mgmt /health). A main checkout keeps Cargo.toml's last stable version, so
  # only the commit tells two Nix builds apart. `+g<rev>` is the Deck's source-build shape:
  # no `ci` in it, so the update check never reads it as a CI run.
  buildVersion = "${version}-nix" + lib.optionalString (rev != null) "+g${rev}";

  # `cargo build --release --locked` scoped to the crates each package ships. crane appends this to
  # both the vendor step and the compile.
  commonArgs = {
    inherit src version;
    strictDeps = true;

    outputHashes = {
      "git+https://github.com/microsoft/windows-rs?rev=acb5a1a7441033d9312b16842af02eb0c2b403dc#acb5a1a7441033d9312b16842af02eb0c2b403dc" =
        "sha256-i92qO/7YO4XB9LQ2w9etTAwGebM/SdwSi8hJaGoGq/Y=";
      "git+https://github.com/unom-io/usbfs-iso?rev=f3de1fd62cec271d07f45664dc464f23e423e721#f3de1fd62cec271d07f45664dc464f23e423e721" =
        "sha256-RWQgE6AHnvXKwbBRw0dVavZy0TLngCs3C+OZENqYG2c=";
    };

    # nixpkgs ships CMake ≥ 4, which errors on `cmake_minimum_required(VERSION <3.5)`. Several
    # vendored C libraries built through the `cmake` crate still declare a pre-3.5 minimum
    # (audiopus_sys' libopus; belt-and-braces for pyrowave-sys / aws-lc-sys). CMake reads this env
    # var as the floor policy version, letting those configure instead of aborting.
    CMAKE_POLICY_VERSION_MINIMUM = "3.5";

    # crane's default is a two-derivation split (buildDepsOnly → buildPackage); disable it, because
    # the deps-only pass builds a "dummy" source tree that drops pyrowave-sys's `vendor/pyrowave`
    # CMake inputs and would fail its build.rs. One derivation compiles everything from real src.
    cargoArtifacts = null;

    # The workspace has hardware-bound tests (uinput, a real GPU); packaging builds never run them,
    # exactly like the deb/rpm/PKGBUILD paths.
    doCheck = false;

    nativeBuildInputs = [
      pkg-config
      cmake # pyrowave-sys (C++/Vulkan), the vendored libopus (opus crate), aws-lc-sys (rustls)
      nasm # libopus SIMD + OpenH264 (openh264 `source` feature)
      perl # aws-lc-sys asm generation (rustls' aws-lc-rs crypto provider)
      rustPlatform.bindgenHook # LIBCLANG_PATH + clang args for pyrowave-sys bindgen
      addDriverRunpath # provides the `addDriverRunpath` shell fn used in postFixup
    ];
  };

  meta = {
    homepage = "https://git.unom.io/unom/punktfunk";
    license = with lib.licenses; [
      mit
      asl20
    ];
    platforms = [ "x86_64-linux" ]; # NVENC desktop host; matches the RPM's ExclusiveArch: x86_64
    maintainers = [ ];
  };

  # The Linux status tray, built in ISOLATION from the host — this is load-bearing, not tidiness.
  # Cargo unifies features across every package in a single `cargo build`, so co-building the tray
  # with the host would pull the host's `ashpd → zbus/tokio` onto the tray's SHARED `zbus`. The tray
  # deliberately runs zbus on `ksni`'s `async-io` executor with the `blocking` API and no tokio
  # runtime (see crates/punktfunk-tray/Cargo.toml), so a tokio-flavoured zbus panics at startup:
  # "there is no reactor running, must be called from the context of a Tokio 1.x runtime". A separate
  # invocation keeps the tray's zbus on async-io. (The host package copies this binary into its $out.)
  punktfunk-tray = craneLib.buildPackage (
    commonArgs
    // {
      pname = "punktfunk-tray";
      cargoExtraArgs = "--locked -p punktfunk-tray";
      PUNKTFUNK_BUILD_VERSION = buildVersion;
      # Pure-Rust leaf: ksni/zbus talk to the dbus socket, ureq+rustls(ring) + punktfunk-core `tls` —
      # nothing to link (no buildInputs) and no GPU driver runpath (it never dlopens libcuda/EGL/vulkan).
      meta = meta // {
        description = "punktfunk host status tray (Linux StatusNotifierItem)";
        mainProgram = "punktfunk-tray";
      };
    }
  );
in
{
  inherit punktfunk-tray;

  punktfunk-host = craneLib.buildPackage (
    commonArgs
    // {
      pname = "punktfunk-host";
      # HOST + ENCODE WORKER — the tray is a separate derivation (see the note above; co-building
      # crashes it), but punktfunk-encode-worker belongs here: it is the capability-carrying half of
      # the PyroWave encode path, it shares the host's dependency graph by design, and host and
      # worker version-check each other over their socket, so they must be built and shipped
      # lockstep. It is a SEPARATE executable, never a hardlink or a host subcommand — on
      # file-capability channels a shared inode would share the capability and make the host
      # unidentifiable to KWin (see the note in nixos-module.nix). Without `-p` here crane never
      # builds it and `$out/bin` simply would not contain it.
      cargoExtraArgs =
        "--locked -p punktfunk-host -p punktfunk-encode-worker "
        + "--features punktfunk-host/nvenc,punktfunk-host/vulkan-encode";

      PUNKTFUNK_BUILD_VERSION = buildVersion;

      buildInputs = [
        pipewire # libpipewire-0.3 + libspa-0.2 (portal capture + the `pipewire` crate)
        libopus # audiopus_sys links system opus via pkg-config (else it vendors a static libopus that mis-links)
        wayland # libwayland-client (wlr / KWin fake_input backends)
        libxkbcommon # virtual-keyboard keymap build/validate
        libGL # src/linux/zerocopy/egl.rs `#[link(name = "GL")]`
        gbm # src/linux/zerocopy/egl.rs `#[link(name = "gbm")]`
        vulkan-loader # ash (Vulkan-Video encode + the dmabuf→VK bridge); loaded at runtime
        libva # pf-libva (native VAAPI encode); dlopen'd, and no longer transitively via FFmpeg
      ];

      # KWin resolves a streaming client to a .desktop by matching /proc/<pid>/exe against `Exec`, so
      # the host's authorization .desktop must name the ACTUAL binary path (the store path here). The
      # tray/client .desktop Exec lines are rewritten for the same reason (menu launch / autostart).
      postInstall = ''
        # The status tray, built in its own derivation (see the punktfunk-tray note above) so its zbus
        # stays on async-io; ship it in the host's $out so the tray .desktop below resolves it.
        install -Dm0755 ${punktfunk-tray}/bin/punktfunk-tray "$out/bin/punktfunk-tray"

        # udev: /dev/uinput + /dev/uhid (virtual gamepads) + the vhci sysfs perms for the virtual Deck.
        install -Dm0644 scripts/60-punktfunk.rules "$out/lib/udev/rules.d/60-punktfunk.rules"
        # WirePlumber: hold a DualSense's sound card open + keep it off the graph clock.
        install -Dm0644 scripts/60-punktfunk-dualsense.conf "$out/share/wireplumber/wireplumber.conf.d/60-punktfunk-dualsense.conf"

        # KWin Desktop-mode authorization (zkde_screencast + fake_input). Point Exec at the store binary.
        install -Dm0644 packaging/linux/io.unom.Punktfunk.Host.desktop \
          "$out/share/applications/io.unom.Punktfunk.Host.desktop"
        substituteInPlace "$out/share/applications/io.unom.Punktfunk.Host.desktop" \
          --replace-fail "/usr/bin/punktfunk-host" "$out/bin/punktfunk-host"

        # Status-tray autostart entry + its hicolor status icons.
        install -Dm0644 packaging/linux/io.unom.Punktfunk.Tray.desktop \
          "$out/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop"
        substituteInPlace "$out/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop" \
          --replace-fail "/usr/bin/punktfunk-tray" "$out/bin/punktfunk-tray"
        for sz in 22x22 48x48; do
          for png in packaging/linux/icons/hicolor/$sz/apps/*.png; do
            install -Dm0644 "$png" "$out/share/icons/hicolor/$sz/apps/$(basename "$png")"
          done
        done

        # Reference material: headless session helpers, example env files, the OpenAPI doc.
        install -Dm0755 scripts/headless/run-headless-kde.sh  "$out/share/punktfunk-host/headless/run-headless-kde.sh"
        install -Dm0755 scripts/headless/run-headless-sway.sh "$out/share/punktfunk-host/headless/run-headless-sway.sh"
        install -Dm0644 scripts/headless/kde-authorized       "$out/share/punktfunk-host/headless/kde-authorized"
        install -Dm0644 scripts/headless/punktfunk-sink.conf  "$out/share/punktfunk-host/headless/punktfunk-sink.conf"
        install -Dm0644 scripts/host.env.example              "$out/share/punktfunk-host/host.env.example"
        install -Dm0644 packaging/bazzite/host.env            "$out/share/punktfunk-host/host.env.bazzite"
        install -Dm0644 packaging/kde/host.env                "$out/share/punktfunk-host/host.env.kde"
        install -Dm0644 api/openapi.json                      "$out/share/punktfunk-host/openapi.json"
      '';

      # Run AFTER fixup, and add every dlopen'd library by hand. A `buildInputs` entry only reaches
      # RUNPATH when something DT_NEEDEDs it, and nothing links these: ash and volk dlopen
      # libvulkan.so.1, pf-libva dlopens libva.so.2. The driver link is the third entry and covers
      # libcuda.so.1 / libnvidia-encode.so.1 / libnvidia-ml.so.1 / libEGL.so.1 and the vendor ICD,
      # but carries neither the Khronos loader nor libva. A miss here is silent: the codepath just
      # reports "unavailable" and falls back.
      postFixup = ''
        # Both binaries dlopen the GPU stack; the worker owns a Vulkan device of its own for
        # PyroWave, and without it the host silently falls back to the in-process encoder at
        # default priority. The tray (its own derivation, copied in above) does not.
        for b in punktfunk-host punktfunk-encode-worker; do
          addDriverRunpath "$out/bin/$b"
          patchelf --add-rpath "${vulkan-loader.out}/lib" "$out/bin/$b"
          patchelf --add-rpath "${libva.out}/lib" "$out/bin/$b"
          # Assert each soname RESOLVES, not merely that a directory is listed: a package whose
          # default output is `dev` puts a lib-less path on RUNPATH and the dlopen still fails.
          for s in libvulkan.so.1 libva.so.2; do
            ok=""
            for d in $(patchelf --print-rpath "$out/bin/$b" | tr : ' '); do
              [ -e "$d/$s" ] && ok=1
            done
            [ -n "$ok" ] || { echo "$b: $s unresolvable from RUNPATH"; exit 1; }
          done
        done
      '';

      meta = meta // {
        description = "Low-latency desktop/game streaming host (Moonlight-compatible + native punktfunk/1)";
        mainProgram = "punktfunk-host";
      };
    }
  );

  punktfunk-client = craneLib.buildPackage (
    commonArgs
    // {
      pname = "punktfunk-client";
      # The session's default `ui` feature pulls skia-safe, whose build DOWNLOADS a prebuilt Skia
      # (rust-skia releases) — impossible in Nix's network-less build sandbox, and a from-source Skia
      # build needs the full gn/ninja/python toolchain + network-fetched third-party. The `ui` feature
      # is explicitly droppable (clients/session/Cargo.toml: "same streaming, stats on stdout only"),
      # so build the session without it. The GTK shell (punktfunk-client-linux) is skia-free and full.
      # Re-adding the Skia OSD under Nix is tracked in packaging/nix/README.md.
      cargoExtraArgs =
        "--locked -p punktfunk-client-linux -p punktfunk-client-session -p punktfunk-cli "
        + "--no-default-features --features punktfunk-client-session/pyrowave";

      nativeBuildInputs = commonArgs.nativeBuildInputs ++ [ wrapGAppsHook4 ];

      # The client decodes natively (pf-vkdecode / pf-vaapi — libva is dlopen'd, never linked —
      # / openh264 + rav1d, both built from vendored source).
      buildInputs = [
        pipewire # PipeWire audio playback + mic capture
        libopus # audiopus_sys → system opus via pkg-config (Opus decode)
        sdl3 # window + gamepads (SDL3 HIDAPI: DualSense touchpad/motion/triggers)
        gtk4 # the GTK4 shell (clients/linux, relm4)
        libadwaita
        glib
        librsvg # gdk-pixbuf SVG loader for symbolic icons
        gsettings-desktop-schemas
        adwaita-icon-theme
        libxkbcommon
        libGL # pf-zerocopy (via pf-client-core) `#[link(name = "GL")]`
        gbm # pf-zerocopy (via pf-client-core) `#[link(name = "gbm")]`
        vulkan-loader # ash presenter + Vulkan-Video decode (loaded at runtime)
        libva # pf-vaapi decode; dlopen'd, so the postFixup below is what actually finds it
      ];

      # wrapGApp needs to run on the real ELF, and addDriverRunpath must run post-shrink — so disable
      # the hook's blanket auto-wrap and drive both by hand in postFixup (see the host note above).
      dontWrapGApps = true;

      postInstall = ''
        # hidraw access for the seated user's DualSense (SDL HIDAPI full-fidelity path).
        install -Dm0644 scripts/70-punktfunk-client.rules "$out/lib/udev/rules.d/70-punktfunk-client.rules"

        # The app icon the desktop entry (and the About dialog) name.
        install -Dm0644 packaging/linux/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg \
          "$out/share/icons/hicolor/scalable/apps/io.unom.Punktfunk.svg"

        # Desktop launcher — point Exec at the store binary (wrapped below).
        install -Dm0644 packaging/linux/io.unom.Punktfunk.desktop \
          "$out/share/applications/io.unom.Punktfunk.desktop"
        substituteInPlace "$out/share/applications/io.unom.Punktfunk.desktop" \
          --replace-fail "Exec=punktfunk-client" "Exec=$out/bin/punktfunk-client"
      '';

      postFixup = ''
        # Same three runpath entries as the host: the driver link for the vendor ICD and libEGL,
        # vulkan-loader for the presenter and Vulkan-Video decode, libva for pf-vaapi decode.
        for b in punktfunk-client punktfunk-session; do
          addDriverRunpath "$out/bin/$b"
          patchelf --add-rpath "${vulkan-loader.out}/lib" "$out/bin/$b"
          patchelf --add-rpath "${libva.out}/lib" "$out/bin/$b"
          for s in libvulkan.so.1 libva.so.2; do
            ok=""
            for d in $(patchelf --print-rpath "$out/bin/$b" | tr : ' '); do
              [ -e "$d/$s" ] && ok=1
            done
            [ -n "$ok" ] || { echo "$b: $s unresolvable from RUNPATH"; exit 1; }
          done
        done
        # Only the GTK shell needs the GApps wrapper (GSETTINGS_SCHEMA_DIR, icon themes, typelibs);
        # the ash session binary is not a GTK app. Wrap last: it renames the ELF just patched.
        wrapGApp "$out/bin/punktfunk-client"
      '';

      meta = meta // {
        description = "Native Linux punktfunk/1 streaming client (GTK4 shell + Vulkan session)";
        mainProgram = "punktfunk-client";
      };
    }
  );

  # --- management web console (punktfunk-web) ------------------------------------------------------
  # The browser console every client needs for SPAKE2 PIN pairing + host status: a TanStack Start /
  # React app that vite builds into a Nitro SSR bundle, run on `bun` (the Nitro `bun` preset + a
  # custom Bun.serve TLS entry — node can't run it; web/nitro-entry/bun-https.mjs). This mirrors the
  # Debian punktfunk-web .deb (packaging/debian/build-web-deb.sh) and the RPM's `--with web`
  # subpackage, which the host package Recommends so a default install pulls the console too.
  #
  # Unlike apt/dnf — which have no bun in their repos and so VENDOR a bun binary into the package —
  # Nix has `pkgs.bun`, so the launcher just execs it from the store (no vendored runtime). The
  # systemd `--user` units + firewall wiring live in the NixOS module, pointed at this store path.
  punktfunk-web = stdenvNoCC.mkDerivation {
    pname = "punktfunk-web";
    inherit src version;
    # nodejs: `bun run codegen` ends in a literal `node tools/check-i18n.mjs`, so `node` must be on
    # PATH. (The JS CLIs' own `#!/usr/bin/env node` shebangs are already rewritten inside the
    # bun2nix cache.) bun is the RUNTIME — the launcher execs it; node is build-time only.
    nativeBuildInputs = [
      bun
      nodejs
      makeWrapper
      bun2nix.hook
    ];

    # node_modules, materialised offline. `bun.nix` (generated from web/bun.lock by the `bun2nix`
    # devDependency's postinstall hook, and committed) lists ONE `fetchurl` per package, keyed by
    # the lockfile's own integrity hash — including the @unom scope, whose tarball URLs the
    # lockfile records in full (web/.npmrc → https://git.unom.io/api/packages/unom/npm/, read-public,
    # the same anonymous pull CI's rpm/deb builds do). That replaces the old single fixed-output
    # `bun install` derivation whose aggregate `outputHash` had to be hand-bumped on every lockfile
    # change — the failure mode this design removes.
    bunDeps = bun2nix.fetchBunDeps { bunNix = src + "/web/bun.nix"; };
    # `src` is the whole repo; the console (and its lockfile) live in web/.
    bunRoot = "web";
    # The install-time `prepare` codegen is run explicitly in buildPhase instead (below), matching
    # the deb/rpm builders — and the dependency lifecycle scripts stay off, as they were under the
    # old `--ignore-scripts` FOD (playwright's would try to download browsers).
    dontRunLifecycleScripts = true;
    # We drive the build/install ourselves; don't let the hook default them to `bun build`/`bun test`.
    dontUseBunBuild = true;
    dontUseBunCheck = true;
    dontUseBunInstall = true;
    # The hook's own patch phase would `patchShebangs .` over the ENTIRE repo checkout, rewriting
    # shell scripts we ship verbatim (scripts/web-init.sh). node_modules needs no patching — bun2nix
    # already patched each package's shebangs inside the cache.
    dontUseBunPatch = true;
    # …which also means setting HOME ourselves; bun writes there during install.
    preConfigure = ''
      export HOME=$TMPDIR
    '';

    # Everything past the deps fetch is offline: codegen + the vite build take every input from the
    # installed node_modules, the checked-in api/openapi.json, and web/project.inlang.
    #
    # ⚠ "Offline" is load-bearing and NOT self-enforcing. inlang resolves the plugins in
    # web/project.inlang/settings.json `modules`, and a failed import is only a WARNING there:
    # paraglide then prints "Successfully compiled", exits 0, and emits ZERO messages, so the
    # console builds fine and dies at SSR time with every `m.foo()` undefined. That is exactly
    # what a CDN URL in `modules` did in this network-less sandbox. The plugin is now a normal
    # devDependency referenced by path, and `bun run codegen` ends in tools/check-i18n.mjs, which
    # fails the build on a remote module or a short message count. Keep both properties.
    buildPhase = ''
      runHook preBuild
      # node_modules is already in place: the bun2nix hook ran an offline `bun install` in web/
      # (bunRoot) off the store-fetched cache, before this phase.
      cd web
      # `codegen` = orval (a typed React-Query client from ../api/openapi.json) + paraglide-js i18n
      # compile; both write into src/ and are prerequisites of the build (normally the install-time
      # `prepare` hook, which `dontRunLifecycleScripts` skips above).
      bun run codegen
      # `build` = vite build ⇒ the Nitro `bun`-preset SSR bundle in .output (our Bun.serve TLS entry).
      bun run build
      # Guard: assert we produced the bun bundle, not a node one (same check the deb/rpm builders do).
      grep -q 'Bun\.serve' .output/server/index.mjs \
        || { echo "ERROR: web/.output is not a bun bundle (wrong nitro preset)" >&2; exit 1; }
      cd ..
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      # The SSR bundle + its static assets, plus the first-run helper and env sample.
      mkdir -p $out/share/punktfunk-web/.output
      cp -R web/.output/server $out/share/punktfunk-web/.output/server
      cp -R web/.output/public $out/share/punktfunk-web/.output/public
      install -Dm0755 scripts/web-init.sh $out/share/punktfunk-web/web-init.sh
      install -Dm0644 web/web.env.example $out/share/punktfunk-web/web.env.example

      # PATH-stable launcher: run the SSR bundle on bun from the store (mirrors the deb/rpm
      # /usr/bin/punktfunk-web-server, minus the vendored-bun indirection).
      makeWrapper ${bun}/bin/bun $out/bin/punktfunk-web-server \
        --add-flags "$out/share/punktfunk-web/.output/server/index.mjs"
      runHook postInstall
    '';

    dontFixup = true;

    meta = meta // {
      description = "punktfunk management web console (Nitro SSR on bun + React)";
      mainProgram = "punktfunk-web-server";
    };
  };

  # --- plugin/script runner (punktfunk-scripting) --------------------------------------------------
  # The host's automation runner: the `@punktfunk/host` SDK's `punktfunk-scripting` CLI (built on
  # Effect), which discovers ~/.config/punktfunk/{scripts,plugins} and supervises each unit as an
  # Effect fiber. It runs on `bun` (it import()s the operator's `.ts` plugin files, which only bun
  # can do). Mirrors the Debian punktfunk-scripting .deb / the RPM's `--with scripting` subpackage,
  # which the host package Recommends — the NixOS module wires the opt-in systemd --user unit.
  #
  # Unlike the deb/rpm we don't `bun build` into a bundle + vendor bun; we still bundle (one
  # self-contained JS, effect inlined) but the launcher execs `pkgs.bun` from the store.
  punktfunk-scripting = stdenvNoCC.mkDerivation {
    pname = "punktfunk-scripting";
    inherit src version;
    nativeBuildInputs = [
      bun
      makeWrapper
      bun2nix.hook
    ];

    # Same bun2nix wiring as punktfunk-web, against sdk/bun.lock's generated sdk/bun.nix (kept in
    # step by the `bun2nix` devDependency's `prepare` script — `prepare`, not `postinstall`,
    # because sdk/ is the PUBLISHED @punktfunk/host package and a postinstall would then run on
    # every consumer's install). No aggregate deps hash to bump.
    bunDeps = bun2nix.fetchBunDeps { bunNix = src + "/sdk/bun.nix"; };
    bunRoot = "sdk";
    dontRunLifecycleScripts = true; # matches the deb/rpm/windows SDK builds' `--ignore-scripts`
    dontUseBunBuild = true;
    dontUseBunCheck = true;
    dontUseBunInstall = true;
    dontUseBunPatch = true;
    preConfigure = ''
      export HOME=$TMPDIR
    '';

    # `bun build --target=bun` bundles the runner CLI to ONE self-contained JS: effect + the SDK
    # are inlined, and the runner's dynamic `import()` of the operator's plugin files is left as a
    # runtime import (bun keeps unresolvable dynamic specifiers external). Fully offline.
    buildPhase = ''
      runHook preBuild
      # node_modules is already in place (bun2nix hook, offline `bun install` in sdk/).
      ( cd sdk && bun build src/runner-cli.ts --target=bun --outfile=$TMPDIR/runner-cli.js )
      grep -q 'attempt=' $TMPDIR/runner-cli.js \
        || { echo "ERROR: runner bundle missing the dynamic plugin import — wrong build" >&2; exit 1; }
      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      install -Dm0644 $TMPDIR/runner-cli.js $out/share/punktfunk-scripting/runner-cli.js
      # Launcher: run the bundle on bun from the store (mirrors the deb/rpm /usr/bin/punktfunk-scripting).
      makeWrapper ${bun}/bin/bun $out/bin/punktfunk-scripting \
        --add-flags "$out/share/punktfunk-scripting/runner-cli.js"
      runHook postInstall
    '';

    dontFixup = true;

    meta = meta // {
      description = "punktfunk plugin/script runner (Effect SDK on bun)";
      mainProgram = "punktfunk-scripting";
    };
  };
}
