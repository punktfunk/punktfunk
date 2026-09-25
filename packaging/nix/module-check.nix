# Does the NixOS module actually evaluate, and does it still render the units we decided on?
#
# WHY THIS FILE EXISTS. `nix flake check` does NOT check `nixosModules`. It forces the value and
# asserts it is a lambda taking an open attribute set — nothing more; nix's own source carries
# `// FIXME: if we have a 'nixpkgs' input, use it to check the module.` MEASURED: a flake whose
# `nixosModules.default` sets a nonexistent OPTION, references a nonexistent `pkgs` attribute AND
# calls a nonexistent `lib` function passes clean, printing the thoroughly reassuring
#
#     checking NixOS module 'nixosModules.default'... all checks passed!
#
# So every option name, type, `pkgs.*` reference and systemd directive in nixos-module.nix was
# unverified by CI while reading as covered — on a flake whose own history is Nix regressions
# reaching main invisibly (`nix build .#punktfunk-web` was broken for 553 commits).
#
# HOW IT CLOSES THAT. Exposed as a flake `checks` output, so `nix flake check --no-build` — the leg
# CI already runs — must INSTANTIATE it, and instantiating forces the `assert` below. Every
# assertion is therefore pure Nix, evaluated at instantiation: a shell script inside the derivation
# would only run under a full `nix flake check`, which builds the hour-long Rust packages and is
# exactly what CI cannot afford. Keep it that way — if you add a check, add it to `results`, not to
# a `runCommand` body.
#
# STUB PACKAGES, on purpose. The real derivations would drag punktfunk-host, punktfunk-client and
# (via `gamescopeHdr`) a from-source gamescope into this check's closure, making the cheap leg
# expensive and coupling a module regression to a Rust build. What is under test here is the MODULE.
# ⚠ The stubs must be fake DERIVATIONS, not store-path strings: `types.package` accepts anything
# `isDerivation` as-is, but runs a store-path string through `builtins.storePath`, which demands the
# path actually exist and fails eval with "no substituter can build it".
{
  lib,
  runCommand,
  # The nixpkgs SOURCE. Interpolated rather than `nixpkgs + "/..."` because this is called with the
  # flake INPUT, which is an attribute set (coerced through its `outPath`) and not a path — the
  # difference only shows up as "expected a set but found a string" from whichever side is wrong.
  nixpkgs,
  system,
  module,
}:
let
  fakeDrv = name: {
    type = "derivation";
    inherit name;
    outPath = "/pf-stub/${name}";
    outputs = [ "out" ];
  };

  stubSelf = {
    packages.${system} = lib.genAttrs [
      "punktfunk-host"
      "punktfunk-client"
      "punktfunk-web"
      "punktfunk-scripting"
      "punktfunk-gamescope"
    ] fakeDrv;
  };

  # A machine just complete enough for eval-config, plus the scenario under test.
  evalWith =
    scenario:
    (import "${nixpkgs}/nixos/lib/eval-config.nix" {
      inherit system;
      modules = [
        (module stubSelf)
        {
          boot.loader.grub.enable = false;
          fileSystems."/" = {
            device = "/dev/sda1";
            fsType = "ext4";
          };
          system.stateVersion = "24.11";
          nixpkgs.hostPlatform = system;
          # `host.users` adds group membership to an existing user; declare one so NixOS's own
          # "isNormalUser or isSystemUser" assertion is not what this check trips over.
          users.users.alice.isNormalUser = true;
        }
        scenario
      ];
    }).config;

  # Every scenario keeps `gamescopeHdr = false`: its default would pull a real (stub, here)
  # gamescope onto the host unit's PATH, and nothing below is about that. `scripting` is left at
  # its default (on with the host) precisely BECAUSE the runner belongs on that PATH — see the
  # "not just in systemPackages" check.
  desktop = evalWith {
    services.punktfunk.host = {
      enable = true;
      users = [ "alice" ];
      # Explicit: gamestream defaults to false now (opt-in on every route) — this fixture
      # is the one that proves opting IN still reaches the argv and the firewall.
      gamestream = true;
      gamescopeHdr = false;
      desktopSession = true;
    };
  };

  appliance = evalWith {
    services.punktfunk.host = {
      enable = true;
      autoStart = true;
      openFirewall = true;
      gamescopeHdr = false;
    };
  };

  nativeOnly = evalWith {
    services.punktfunk.host = {
      enable = true;
      openFirewall = true;
      gamestream = false;
      gamescopeHdr = false;
    };
  };

  # A host that has opted out of the runner — the negative half of the PATH check.
  noScripting = evalWith {
    services.punktfunk.host = {
      enable = true;
      gamescopeHdr = false;
    };
    services.punktfunk.scripting.enable = false;
  };

  clientOnly = evalWith { services.punktfunk.client.enable = true; };

  unit = cfg: name: cfg.systemd.user.units."${name}.service".text;
  has =
    cfg: name: infix:
    lib.hasInfix infix (unit cfg name);
  # A module's own failed assertions, as messages.
  failedAssertions = cfg: map (a: a.message) (lib.filter (a: !a.assertion) cfg.assertions);

  results = [
    # --- the module evaluates at all, in every shape an operator can ask for -------------------
    {
      name = "desktop scenario has no failing assertions";
      ok = failedAssertions desktop == [ ];
    }
    {
      name = "appliance scenario has no failing assertions";
      ok = failedAssertions appliance == [ ];
    }
    {
      name = "client-only scenario has no failing assertions";
      ok = failedAssertions clientOnly == [ ];
    }

    # --- user scoping: the second-copy-steals-the-ports trap -----------------------------------
    # `systemd.user.*` installs into EVERY user's manager, root's included (user@0.service exists
    # as soon as anyone logs in as root), and `autoStart` puts these in default.target. Root's host
    # then wins the fixed ports and the desktop user's restarts forever on
    # `bind RTSP 48010: Address already in use` — every other listener in its log having bound
    # fine, so it reads like an unrelated program. MEASURED on a real box before this was fixed.
    {
      # `|` = TRIGGERING condition, which systemd ORs. Plain repeated ConditionUser= lines are
      # ANDed and would match nobody — the whole reason the prefix is there.
      name = "host.users scopes every user unit to those users, OR-ed";
      ok =
        let
          scoped = name: has desktop name "ConditionUser=|alice";
        in
        scoped "punktfunk-host" && scoped "punktfunk-web" && scoped "punktfunk-scripting";
    }
    {
      # web-init is the console's readiness gate (it blocks until the host has written the mgmt
      # token + identity cert), so it has to RUN on every start — not just the first. The
      # `ConditionPathExists=!…/web-password` it used to carry skipped it from the second boot
      # onward, which is exactly when the wait still matters; web-init.sh is idempotent instead.
      # Keep the user scope: root's instance has no business generating the operator's password.
      name = "web-init is user-scoped and NOT self-skipped by the password file";
      ok =
        has desktop "punktfunk-web-init" "ConditionUser=|alice"
        && !(has desktop "punktfunk-web-init" "ConditionPathExists=");
    }
    {
      # With no host.users to name, still keep SYSTEM users (root) out, while leaving the module
      # header's manual `systemctl --user enable --now punktfunk-host` working for a normal login.
      name = "with no host.users, the units still refuse system users (root)";
      ok =
        has appliance "punktfunk-host" "ConditionUser=!@system"
        && !(has appliance "punktfunk-host" "ConditionUser=|");
    }

    # --- the KWin identification trap (packaging/arch/punktfunk-host.install) -------------------
    # The host MUST exec the plain store path. A capability wrapper here would put CAP_SYS_NICE in
    # the process's permitted set, and the kernel then refuses KWin the /proc/<pid>/exe readlink it
    # identifies the client by — which cost every KDE box its desktop streaming in 0.26.0-1.
    {
      name = "host ExecStart is the store binary, never a capability wrapper";
      ok =
        has desktop "punktfunk-host" "ExecStart=/pf-stub/punktfunk-host/bin/punktfunk-host serve"
        && !(has desktop "punktfunk-host" "ExecStart=/run/wrappers");
    }
    # ...while the ENCODE WORKER, which nothing ever has to identify, is pointed at the wrapper.
    {
      name = "host points PUNKTFUNK_ENCODE_WORKER at the capability wrapper";
      ok =
        has desktop "punktfunk-host"
          "PUNKTFUNK_ENCODE_WORKER=/run/wrappers/bin/punktfunk-encode-worker";
    }
    {
      name = "the encode-worker wrapper carries exactly cap_sys_nice=ep";
      ok = desktop.security.wrappers.punktfunk-encode-worker.capabilities == "cap_sys_nice=ep";
    }

    # --- the desktop-login route (scripts/punktfunk-host-desktop-session.conf) -----------------
    # Asserted on the evaluated LISTS, not the rendered text: systemd renders `After=` as one
    # space-separated line, so `hasInfix "After=graphical-session.target"` silently depends on
    # ordering — it failed against a correct module the first time this check ran.
    {
      name = "desktopSession binds the host to graphical-session.target";
      ok =
        let
          u = desktop.systemd.user.services.punktfunk-host;
        in
        lib.elem "graphical-session.target" u.after
        && lib.elem "graphical-session.target" u.partOf
        # IN ADDITION to default.target, never instead of it.
        && lib.elem "graphical-session.target" u.wantedBy;
    }
    {
      name = "desktopSession is NOT applied to the appliance route (it would stay stopped there)";
      ok =
        let
          u = appliance.systemd.user.services.punktfunk-host;
        in
        !(lib.elem "graphical-session.target" u.partOf) && lib.elem "default.target" u.wantedBy;
    }

    # --- GameStream opt-out reaches both the argv and the firewall -----------------------------
    {
      name = "gamestream=true passes --gamestream";
      ok = has desktop "punktfunk-host" "serve --gamestream";
    }
    {
      name = "gamestream=false drops --gamestream and its firewall ports";
      ok =
        !(has nativeOnly "punktfunk-host" "--gamestream")
        && !(lib.elem 47984 nativeOnly.networking.firewall.allowedTCPPorts)
        && lib.elem 47990 nativeOnly.networking.firewall.allowedTCPPorts;
    }
    {
      # The DEFAULT is the secure native-only host — gamestream unset must behave like
      # gamestream=false (opt-in on every package route; the appliance fixture leaves it unset).
      name = "gamestream default (unset) is native-only";
      ok =
        !(has appliance "punktfunk-host" "--gamestream")
        && !(lib.elem 47984 appliance.networking.firewall.allowedTCPPorts);
    }
    {
      name = "openFirewall opens the console AND its plugin origin";
      ok =
        lib.elem 47992 appliance.networking.firewall.allowedTCPPorts
        && lib.elem 47993 appliance.networking.firewall.allowedTCPPorts;
    }

    # --- the three divergences from the shipped units, as regression guards --------------------
    # Each of these was ONCE wrong here while right in scripts/*.service. Assert the decision, so a
    # future edit cannot quietly drift back.
    {
      # Without this, systemd's default 5-starts-per-10s against RestartSec=2 gives up permanently
      # after ~10 s — the exact window before the host's first `serve` writes the mgmt token.
      name = "web console retries indefinitely while the host writes its mgmt token";
      ok = has appliance "punktfunk-web" "StartLimitIntervalSec=0";
    }
    {
      # A console that exits 0 has still stopped serving.
      name = "web console restarts on ANY exit, not just failure";
      ok =
        has appliance "punktfunk-web" "Restart=always"
        && !(has appliance "punktfunk-web" "Restart=on-failure");
    }
    {
      # Unit hardening for the supervisor and the operator's own scripts. What confines a PLUGIN
      # is its own bwrap sandbox — these directives are a mount namespace, which does not hold
      # against a process of the same uid.
      name = "the plugin runner is hardened like the deb/rpm unit";
      ok =
        has appliance "punktfunk-scripting" "NoNewPrivileges=true"
        && has appliance "punktfunk-scripting" "ProtectSystem=strict"
        && has appliance "punktfunk-scripting" "ReadWritePaths=/tmp"
        && has appliance "punktfunk-scripting" "RestrictAddressFamilies=AF_UNIX"
        # bwrap needs netlink and a fresh /proc; the sandbox takes both back from the plugin.
        && !(has appliance "punktfunk-scripting" "ProtectKernelTunables=true")
        && has appliance "punktfunk-scripting" "AF_NETLINK"
        && has appliance "punktfunk-scripting" "ProtectControlGroups=true"
        && has appliance "punktfunk-scripting" "RestrictNamespaces=user mnt pid net ipc uts cgroup"
        && has appliance "punktfunk-scripting" "SystemCallArchitectures=native"
        && has appliance "punktfunk-scripting" "CapabilityBoundingSet=";
    }
    {
      # `ReadWritePaths=%h` re-opened the whole home, so the runner read `mgmt-token` and `key.pem`
      # beside the scoped token it is supposed to be limited to — the privilege split the docs
      # promise did not hold on Linux. The home is a tmpfs now and the binds are the allow-list.
      name = "the plugin runner cannot read the host's admin token or identity key";
      ok =
        has appliance "punktfunk-scripting" "ProtectHome=tmpfs"
        && !(has appliance "punktfunk-scripting" "ReadWritePaths=%h")
        && has appliance "punktfunk-scripting" "InaccessiblePaths=-%h/.config/punktfunk/mgmt-token"
        && has appliance "punktfunk-scripting" "InaccessiblePaths=-%h/.config/punktfunk/key.pem";
    }
    {
      # Everything ProtectHome=tmpfs takes away that the runner genuinely needs. Drop one of these
      # and the runner comes up unable to authenticate, unable to persist, or with an EMPTY
      # LIBRARY — on Linux a game library lives in the home the tmpfs just hid.
      name = "the plugin runner keeps the paths it needs through the empty home";
      ok =
        has appliance "punktfunk-scripting" "BindPaths=-%h/.config/punktfunk/plugins"
        && has appliance "punktfunk-scripting" "BindPaths=-%h/.config/punktfunk/plugin-state"
        # $XDG_RUNTIME_DIR: the host's live-stream marker, the session bus, compositor sockets.
        && has appliance "punktfunk-scripting" "BindPaths=%t"
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=-%h/.config/punktfunk/plugin-token"
        # A directory bind keeps atomic replacements visible; ExecStartPre makes it exist.
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=%h/.config/punktfunk/plugin-run"
        && has appliance "punktfunk-scripting" "bin/mkdir -p -m 0700 %h/.config/punktfunk/plugin-run %h/.config/punktfunk/plugin-state"
        # The TLS pin is native-cert.pem after the identity split, cert.pem before it.
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=-%h/.config/punktfunk/native-cert.pem"
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=-%h/.config/punktfunk/cert.pem"
        # Without this a moved listener leaves every plugin dialling 47990 forever.
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=-%h/.config/punktfunk/mgmt-endpoint"
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=-%h/.config/punktfunk/scripts"
        && has appliance "punktfunk-scripting" "BindReadOnlyPaths=-%h/.local/share/Steam";
    }
    {
      # Without bwrap on its PATH the runner starts no plugin at all, and the library is empty.
      name = "the plugin runner can build a sandbox";
      ok = builtins.any (p: lib.hasInfix "bubblewrap" (toString p)) (
        appliance.systemd.user.services.punktfunk-scripting.path or [ ]
      );
    }
    {
      # PrivateTmp is OFF on purpose (the VirtualHere field report: a private /tmp hides
      # /tmp/vhclient and /tmp/.X11-unix, so a plugin cannot reach the daemon it integrates with).
      name = "the plugin runner keeps the real /tmp";
      ok = has appliance "punktfunk-scripting" "PrivateTmp=false";
    }
    {
      # Since the library scanners became plugins, a runner that is off means an EMPTY LIBRARY and
      # no obvious reason why — which is why deb+rpm `systemctl --global enable` it.
      name = "the plugin runner is started by default, like every other channel";
      ok = has appliance "punktfunk-scripting" "WantedBy=default.target";
    }
    {
      # The runner has to be on the HOST unit's PATH, not merely in systemPackages. Package ops
      # (`plugins add`, and the console's store jobs, which run inside the host process) locate
      # `punktfunk-scripting` as an executable, and on NixOS PATH is the only rung that can ever
      # match — the runner is its own derivation, so it is never beside the host binary and never
      # under /usr. systemPackages covers an operator's shell and NOT this unit, which is exactly
      # how a running, enabled runner reported itself "not installed" through the console.
      name = "the plugin runner is on the host unit's PATH, not just in systemPackages";
      ok =
        has appliance "punktfunk-host" "/pf-stub/punktfunk-scripting/bin"
        && has desktop "punktfunk-host" "/pf-stub/punktfunk-scripting/bin";
    }
    {
      name = "the host unit PATH includes the system profile so nested steam resolves";
      ok = has desktop "punktfunk-host" "system-path/bin";
    }
    {
      # …and only when it is actually installed, so `scripting.enable = false` does not put a
      # package the machine never built onto a unit's PATH.
      name = "a host without the runner does not carry it on PATH";
      ok = !(has noScripting "punktfunk-host" "/pf-stub/punktfunk-scripting/bin");
    }

    # --- a switch that changes a package restarts the running user services --------------------
    {
      name = "a host package change reloads the unit that restarts the user services";
      ok =
        let
          u = desktop.systemd.services.punktfunk-restart-user-units;
        in
        lib.elem desktop.services.punktfunk.host.package u.reloadTriggers
        && lib.hasInfix "restart-user-units" u.serviceConfig.ExecReload;
    }

    # --- the client half must not drag the host's system wiring in -----------------------------
    {
      name = "a client-only machine defines no host/web/scripting units";
      ok =
        !(clientOnly.systemd.user.services ? punktfunk-host)
        && !(clientOnly.systemd.user.services ? punktfunk-web)
        && !(clientOnly.systemd.user.services ? punktfunk-scripting)
        && !(clientOnly.systemd.services ? punktfunk-restart-user-units);
    }
  ];

  failures = map (r: r.name) (lib.filter (r: !r.ok) results);
in
# The `assert` is what makes `--no-build` sufficient: instantiating this derivation forces it.
assert
  failures == [ ]
  || throw ''
    The punktfunk NixOS module no longer renders what packaging/nix/module-check.nix requires.
    Failing checks (${toString (lib.length failures)} of ${toString (lib.length results)}):
      - ${lib.concatStringsSep "\n    - " failures}
  '';
runCommand "punktfunk-nixos-module-check"
  {
    # Recorded in the output so a green run says what it actually covered.
    passed = toString (lib.length results);
  }
  ''
    echo "punktfunk NixOS module: $passed checks passed at eval time" > "$out"
  ''
