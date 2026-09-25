{
  description = "punktfunk — low-latency desktop/game streaming host + native Linux client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # The bun packages' node_modules (punktfunk-web, punktfunk-scripting): one fetchurl per package,
    # straight out of `bun.lock`'s integrity hashes — no hand-maintained aggregate deps hash to bump.
    # PIN THE TAG. `bun.nix` has no schema stability guarantee across bun2nix versions, so this ref
    # must move together with the `bun2nix` devDependency in web/package.json + sdk/package.json
    # (which regenerates the file on every `bun install`). See packaging/nix/README.md.
    bun2nix = {
      url = "github:nix-community/bun2nix?ref=2.1.2";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      rust-overlay,
      bun2nix,
    }:
    let
      # Linux/x86_64 only — the host encodes with desktop NVENC and CI publishes no aarch64 leg
      # (mirrors the RPM's `ExclusiveArch: x86_64`). Add a system here once there's an arm64 build.
      systems = [ "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f system);

      # The workspace version is the single source of truth (crates/*/Cargo.toml inherit it).
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [
            (import rust-overlay)
            # nixpkgs lags bun; the web console and plugin runner exec this one. Same release as
            # the deb/rpm/arch/windows pins — bump them together (SHASUMS256.txt, as SRI).
            (final: prev: {
              bun = prev.bun.overrideAttrs (old: {
                version = "1.4.2";
                # src is read from passthru.sources, so it follows without being set here.
                __intentionallyOverridingVersion = true;
                passthru = old.passthru // {
                  sources = {
                    x86_64-linux = final.fetchurl {
                      url = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-linux-x64-baseline.zip";
                      hash = "sha256-xngEDxT+BEDrg503y9DOTAUaMtpygGrJfeamqra/co8=";
                    };
                    aarch64-linux = final.fetchurl {
                      url = "https://github.com/oven-sh/bun/releases/download/bun-v1.4.2/bun-linux-aarch64.zip";
                      hash = "sha256-VDKLvC2cjgyfiSxUTWbFeoO4QTnjSQnl7oF1jxrI/ac=";
                    };
                  };
                };
              });
            })
          ];
        };

      # Pin cargo/rustc EXACTLY to rust-toolchain.toml (channel 1.96.0 + rustfmt/clippy) so a Nix
      # build, a dev shell and CI all use the identical toolchain — the repo is deliberate about
      # this (see rust-toolchain.toml's header on rustfmt format-drift).
      toolchainFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      craneLibFor = pkgs: (crane.mkLib pkgs).overrideToolchain toolchainFor;

      packagesFor =
        system:
        let
          pkgs = pkgsFor system;
        in
        pkgs.callPackage ./packaging/nix/packages.nix {
          craneLib = craneLibFor pkgs;
          src = self;
          inherit version;
          # A dirty tree has no `shortRev`; Nix before 2.20 has no `dirtyShortRev` either.
          rev = self.shortRev or self.dirtyShortRev or null;
          # `.hook` + `.fetchBunDeps` (bun2nix v2 API) — see packages.nix.
          bun2nix = bun2nix.packages.${system}.default;
        }
        // {
          # gamescope + our `pipewire-hdr` patches, so the gamescope backend can stream HDR.
          # Kept OUT of packages.nix (a Rust/crane file) — it is a C++ meson override with a
          # disjoint dependency set — and out of `checks` below, because it rebuilds gamescope
          # from source and would make `nix flake check` an hour long.
          punktfunk-gamescope = pkgs.callPackage ./packaging/nix/gamescope.nix {
            patchDir = ./packaging/gamescope/patches;
            # Shared verbatim with build-punktfunk-gamescope.sh, which is the whole reason it is a
            # file: the FHS packages and the Nix store must rename the WSI layer identically, or
            # the host looks for a layer name that only one of them produces.
            manifestRewriter = ./packaging/gamescope/rewrite-wsi-layer-manifest.py;
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pf = packagesFor system;
        in
        {
          inherit (pf)
            punktfunk-host
            punktfunk-client
            punktfunk-web
            punktfunk-scripting
            punktfunk-tray
            punktfunk-gamescope
            ;
          default = pf.punktfunk-host;
        }
      );

      # `nix run .#punktfunk-host -- serve` / `nix run .#punktfunk-client`.
      apps = forAllSystems (
        system:
        let
          pf = packagesFor system;
        in
        {
          punktfunk-host = {
            type = "app";
            program = "${pf.punktfunk-host}/bin/punktfunk-host";
          };
          punktfunk-client = {
            type = "app";
            program = "${pf.punktfunk-client}/bin/punktfunk-client";
          };
          # `nix run .#punktfunk-web` — the console (auto-wire the mgmt token / cert via env or the
          # NixOS module; see packaging/nix/README.md).
          punktfunk-web = {
            type = "app";
            program = "${pf.punktfunk-web}/bin/punktfunk-web-server";
          };
          # `nix run .#punktfunk-scripting -- --list` — the plugin/script runner.
          punktfunk-scripting = {
            type = "app";
            program = "${pf.punktfunk-scripting}/bin/punktfunk-scripting";
          };
          default = self.apps.${system}.punktfunk-host;
        }
      );

      # `nix flake check` builds every package.
      checks = forAllSystems (
        system:
        let
          pf = packagesFor system;
          pkgs = pkgsFor system;
        in
        {
          inherit (pf)
            punktfunk-host
            punktfunk-client
            punktfunk-web
            punktfunk-scripting
            ;

          # The NixOS module, actually evaluated. `nix flake check` does NOT do this for
          # `nixosModules` — it only forces the value and asserts it is a lambda taking an open
          # attribute set, so a module with a nonexistent option, a nonexistent `pkgs` attribute
          # and a nonexistent `lib` function passes clean (measured). Routing the module through a
          # `checks` entry instead means the eval-only CI leg has to instantiate it, and every
          # assertion in module-check.nix is pure Nix so instantiation is enough to run them.
          nixos-module = pkgs.callPackage ./packaging/nix/module-check.nix {
            inherit nixpkgs system;
            module = import ./packaging/nix/nixos-module.nix;
          };
        }
      );

      # `nix develop` — the pinned toolchain plus every system lib the workspace links, wired so
      # `cargo build` (all crates, host + client) works out of the box.
      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
          toolchain = toolchainFor pkgs;
          gbm = pkgs.libgbm or pkgs.mesa;
        in
        {
          default = pkgs.mkShell {
            strictDeps = false;
            nativeBuildInputs = [
              toolchain
              pkgs.rust-analyzer
              pkgs.pkg-config
              pkgs.cmake
              pkgs.nasm
              pkgs.perl
              pkgs.rustPlatform.bindgenHook
            ];
            buildInputs = [
              # host
                pkgs.pipewire
              pkgs.libopus
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.libGL
              gbm
              pkgs.vulkan-loader
              # client
              pkgs.sdl3
              pkgs.gtk4
              pkgs.libadwaita
              pkgs.glib
              pkgs.librsvg
              pkgs.gsettings-desktop-schemas
              pkgs.adwaita-icon-theme
            ];
            # CMake ≥ 4 rejects the pre-3.5 minimums some vendored C libs (libopus) still declare.
            CMAKE_POLICY_VERSION_MINIMUM = "3.5";
            LD_LIBRARY_PATH = "/run/opengl-driver/lib:${
              pkgs.lib.makeLibraryPath [
                pkgs.vulkan-loader
                pkgs.libGL
                gbm
              ]
            }";
          };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt-rfc-style);

      # NixOS integration — see packaging/nix/nixos-module.nix and packaging/nix/README.md.
      #   imports = [ punktfunk.nixosModules.default ];
      #   services.punktfunk.host.enable = true;
      nixosModules.default = import ./packaging/nix/nixos-module.nix self;
      nixosModules.punktfunk = self.nixosModules.default;
    };
}
