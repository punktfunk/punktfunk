# NixOS integration for punktfunk — the declarative equivalent of everything the RPM/deb do in
# their %install + %post (packaging/rpm/punktfunk.spec, packaging/debian/build-deb.sh):
# the systemd *user* service, the uinput/uhid/vhci udev rules, the vhci-hcd autoload, the 32 MB
# UDP socket-buffer sysctls, the firewall openers, the `input`- and `punktfunk`-group membership
# for virtual gamepads, the management web console (`services.punktfunk.web`, on by default with
# the host — the RPM/deb Recommends), and the plugin/script runner
# (`services.punktfunk.scripting`, likewise on by default — the game-library scanners are plugins).
#
# Usage (flake):
#   { inputs.punktfunk.url = "git+https://git.unom.io/unom/punktfunk?ref=nix-stable";
#     outputs = { punktfunk, nixpkgs, ... }: {
#       nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
#         modules = [ punktfunk.nixosModules.default
#                     { services.punktfunk.host.enable = true;
#                       services.punktfunk.host.users = [ "alice" ]; } ];
#       };
#     };
#   }
#
# The host is fundamentally a per-user, in-graphical-session service (it drives the live
# compositor, PipeWire and the desktop portals), so it ships as a `systemd.user` unit. Enable it
# for a session with `systemctl --user enable --now punktfunk-host` (or set `autoStart = true` for a
# headless appliance with `users.users.<u>.linger = true`).
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (lib)
    mkEnableOption
    mkOption
    mkIf
    mkMerge
    mkDefault
    types
    optional
    optionals
    optionalString
    literalExpression
    concatStringsSep
    mapAttrsToList
    genAttrs
    ;

  cfg = config.services.punktfunk;
  system = pkgs.stdenv.hostPlatform.system;

  # host.env rendering: booleans → 1/0 (what PUNKTFUNK_* knobs expect), everything else verbatim.
  renderVal = v: if lib.isBool v then (if v then "1" else "0") else toString v;
  renderEnv =
    attrs: concatStringsSep "\n" (mapAttrsToList (k: v: "${k}=${renderVal v}") attrs) + "\n";

  hostSettingsFile = pkgs.writeText "punktfunk-host.env" (renderEnv cfg.host.settings);

  # WHICH users' `systemd --user` instances may run these units — and why they need saying at all.
  #
  # `systemd.user.*` installs into EVERY user's manager; there is no per-user form of it in NixOS.
  # Combined with `autoStart` putting the units in `default.target`, that includes **root**, whose
  # `user@0.service` springs into existence the moment anybody so much as SSHes in as root. Root's
  # copy of the host then wins the race for the fixed ports and the desktop user's copy fails
  # forever on `bind RTSP 48010: Address already in use` — with every other listener in its log
  # having bound fine, so it reads like a clash with some unrelated program rather than a second
  # copy of itself. MEASURED 2026-08-14 on a fresh NixOS 26.05 box.
  #
  # `host.users` is already documented as "the host runs as these users' systemd --user service",
  # so it is the right scope. When it is empty we cannot name the intended user, so fall back to
  # excluding system users — which is precisely what keeps root out — and leave the module header's
  # manual `systemctl --user enable --now punktfunk-host` route working for any normal login.
  #
  # ⚠ The `|` prefix is load-bearing: it makes each entry a TRIGGERING condition, and systemd ORs
  # those. Plain repeated `ConditionUser=` lines are ANDed, so a two-user list would match NOBODY.
  # Non-triggering conditions on the same unit (punktfunk-web-init's ConditionPathExists) still
  # have to hold, which is the behaviour we want.
  userScope = if cfg.host.users == [ ] then [ "!@system" ] else map (u: "|${u}") cfg.host.users;

  # Native punktfunk/1 ports (control plane + discovery + mgmt API). The media data plane is an
  # ephemeral per-session UDP port the host hole-punches, so nothing fixed to open (see
  # packaging/linux/punktfunk.ufw).
  nativeTCP = [ 47990 ]; # mgmt/library REST API (HTTPS + mTLS)
  nativeUDP = [
    9777
    5353
  ]; # QUIC control plane + mDNS
  # GameStream/Moonlight-compat fixed ports (opt-in with `host.gamestream`).
  gamestreamTCP = [
    47984
    47989
    48010
  ];
  gamestreamUDP = [
    47998
    47999
    48000
  ];
in
{
  options.services.punktfunk = {
    host = {
      enable = mkEnableOption "the punktfunk streaming host (systemd --user service + system wiring)";

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-host;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-host";
        description = "The punktfunk-host package (bundles punktfunk-host + punktfunk-tray).";
      };

      gamestream = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Advertise the GameStream/Moonlight-compatible planes (`serve --gamestream`) so a stock
          Moonlight client can pair, and open their firewall ports. OPT-IN (default `false`,
          matching every other package route): the compat planes carry plain-HTTP pairing and the
          legacy GCM-nonce path (security-review #5/#9), so the default is the secure native-only
          host — Punktfunk clients only. Enable only on a trusted LAN.
        '';
      };

      autoStart = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Start the host automatically in every user's graphical session (adds it to the user
          `default.target`). For a login-less appliance, also enable lingering for the host user
          (`users.users.<name>.linger = true`) so the user service comes up at boot.

          "Every user" is bounded by `host.users` via `ConditionUser=` — without that bound this
          option also starts a host in ROOT's user manager the moment anybody logs in as root, and
          that copy takes the ports from the real one. Set `host.users` on a multi-user box.
        '';
      };

      desktopSession = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Bind the host to the DESKTOP LOGIN session's lifetime
          (`PartOf=`/`WantedBy=graphical-session.target`), so a Plasma/GNOME restart restarts the
          host with it.

          Turn this on for a machine somebody logs into. Without it, when the compositor restarts
          (a crash, a logout/login, "restart the shell") the host keeps running while holding a
          Wayland socket and a portal D-Bus connection that both died with the old compositor. It
          cannot recover either in-process, and the failure is silent: the host still listens,
          still answers, and every session it then serves fails at capture. A host that idles for
          days between sessions is exactly the shape that gets discovered at the worst moment.

          Leave it OFF for an appliance — a pinned `PUNKTFUNK_COMPOSITOR`, a headless KWin or a
          gamescope box. Those start their own compositor and may never reach
          `graphical-session.target` at all, and this would leave the host permanently stopped.

          No effect under sway/Hyprland or any session not managed by systemd (they never reach
          that target either): there, start the host from the compositor's own config, after
          `systemctl --user import-environment`, so it dies and comes back with the session.

          This is the declarative equivalent of the `scripts/punktfunk-host-desktop-session.conf`
          drop-in the deb/rpm document for the same route.
        '';
      };

      users = mkOption {
        type = types.listOf types.str;
        default = [ ];
        example = [ "alice" ];
        description = ''
          Users to add to the `input` and `punktfunk` groups — required for the virtual gamepads
          the host creates: `input` covers `/dev/uinput` and `/dev/uhid`, `punktfunk` covers the
          usbip/vhci nodes the virtual Steam Deck pad attaches through. The second is separate on
          purpose — it can emulate arbitrary USB hardware, so only list users you would trust with
          that. The host runs as these users' `systemd --user` service.

          This list ALSO scopes the units themselves: they carry a `ConditionUser=` for these
          users, so no other user's `systemd --user` instance can start them. That matters because
          `systemd.user.*` installs into every user's manager — including root's, which exists as
          soon as anyone logs in as root — and a second host silently wins the race for the fixed
          ports, leaving the real one restarting forever on "Address already in use".

          Left empty, the units are merely refused to SYSTEM users (`ConditionUser=!@system`), so
          any normal login can still run the host by hand and root still cannot.
        '';
      };

      settings = mkOption {
        type = types.attrsOf (
          types.oneOf [
            types.str
            types.int
            types.bool
          ]
        );
        default = { };
        example = literalExpression ''
          {
            PUNKTFUNK_VIDEO_SOURCE = "virtual";
            PUNKTFUNK_COMPOSITOR = "kwin";
            PUNKTFUNK_444 = true;
            RUST_LOG = "info";
          }
        '';
        description = ''
          `host.env` key/value pairs passed to the service via `EnvironmentFile`. See
          `''${package}/share/punktfunk-host/host.env.example` for the full surface. Booleans render
          as `1`/`0`. A key the web console also offers (Host → Settings) is locked there while it
          is set here. Leave empty to rely on the host's per-connect auto-detection of the
          compositor + input backend. Secrets are REFUSED here (this renders to a world-readable
          store path) — use `environmentFile`, or let the host generate them.
        '';
      };

      environmentFile = mkOption {
        type = types.nullOr types.path;
        default = null;
        example = "/run/secrets/punktfunk-host.env";
        description = ''
          Extra `EnvironmentFile` layered AFTER `settings` (its values win), for values that must
          not reach the store. Loaded optionally (a missing file does not fail the unit). The host
          keeps its own credentials in `~/.config/punktfunk/`, owner-only, and takes them out of its
          environment at startup so the games and hooks it launches cannot inherit them.
        '';
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = ''
          Open the host's inbound ports. Native punktfunk/1 always: UDP 9777 (QUIC) + 5353 (mDNS),
          TCP 47990 (mgmt API). With `gamestream = true` also TCP 47984/47989/48010 and UDP
          47998/47999/48000. The ephemeral media UDP port is hole-punched, so a default-deny
          firewall still streams (it just adds ~2.5 s at session start).
        '';
      };

      gamescopeHdr = mkOption {
        type = types.bool;
        default = true;
        description = ''
          Put `punktfunk-gamescope` — gamescope carrying punktfunk's `pipewire-hdr` patches — on
          the host service's PATH, so a 10-bit-capable client can stream true HDR10 (BT.2020 PQ)
          off a gamescope virtual output. HDR is attempted by default once the binary is present
          (`PUNKTFUNK_GAMESCOPE_HDR=0` in `settings`/`environmentFile` forces SDR).

          It does NOT replace the system's `gamescope`: the binary has its own name and the host
          prefers it only for the sessions it spawns itself. Costs a gamescope build from source
          — set `false` to skip that build; the host then stays SDR on the gamescope backend.
        '';
      };

      gamescopePackage = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-gamescope;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-gamescope";
        description = ''
          The patched gamescope used when `gamescopeHdr = true`. Override to build it from a
          different nixpkgs (the patches apply to whatever gamescope that nixpkgs pins).
        '';
      };
    };

    client = {
      enable = mkEnableOption "the native punktfunk Linux client";

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-client;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-client";
        description = "The punktfunk-client package (bundles punktfunk-client + punktfunk-session).";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = false;
        description = "Open UDP 5353 (mDNS) so the client can auto-discover hosts on the LAN.";
      };
    };

    # The management web console (SPAKE2 PIN pairing + host status) — the browser UI every client
    # needs. Ships by DEFAULT alongside the host (mirrors the RPM's `Recommends: punktfunk-web` and
    # the .deb the host package pulls in), auto-wired to the host's mgmt token + identity cert.
    web = {
      enable = mkOption {
        type = types.bool;
        default = cfg.host.enable;
        defaultText = literalExpression "config.services.punktfunk.host.enable";
        description = ''
          Run the management web console as a `systemd --user` service on TCP 47992 (HTTPS). Enabled
          by default whenever the host is enabled — set to `false` for a console-less host. It
          auto-wires to `~/.config/punktfunk/{mgmt-token,cert.pem,key.pem}` (written by the host's
          `serve`) and generates a login password on first start.
        '';
      };

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-web;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-web";
        description = "The punktfunk-web package (the bun-built Nitro SSR console bundle).";
      };

      openFirewall = mkOption {
        type = types.bool;
        default = cfg.host.openFirewall;
        defaultText = literalExpression "config.services.punktfunk.host.openFirewall";
        description = ''
          Open TCP 47992 so the console is reachable from other devices on the LAN, and TCP 47993,
          the separate origin its plugin UIs are served from (without it, plugin interfaces do not
          load in the console).
        '';
      };

      bind = mkOption {
        type = types.str;
        default = "0.0.0.0";
        example = "127.0.0.1";
        description = ''
          The address the console listens on: `0.0.0.0` for every interface, `127.0.0.1` for this
          machine only, or one address such as a VPN interface. The plugin-UI origin on 47993
          follows it, always. On any bind the console answers only peers on the local network or a
          VPN (RFC 1918, link-local, IPv6 unique-local, Tailscale's 100.64/10), never the internet.

          This is the imperative install's `PUNKTFUNK_UI_BIND` in `host.env`; on NixOS the option
          is the source and `host.env` is not read by the console.
        '';
      };

      autoStart = mkOption {
        type = types.bool;
        default = cfg.host.autoStart;
        defaultText = literalExpression "config.services.punktfunk.host.autoStart";
        description = ''
          Start the console automatically in every user's graphical session (adds it to the user
          `default.target`). Follows the host's `autoStart` by default — for a login-less appliance,
          enable lingering for the user as well.
        '';
      };
    };

    # The plugin/script runner — host automation on bun. Ships with the host (the RPM/deb Recommends
    # it) and, like them, runs by default: the game-library scanners are plugins, so a host with the
    # runner off has an empty library. Opt out with `scripting.autoStart = false` or, per user,
    # `systemctl --user mask punktfunk-scripting`.
    scripting = {
      enable = mkOption {
        type = types.bool;
        default = cfg.host.enable;
        defaultText = literalExpression "config.services.punktfunk.host.enable";
        description = ''
          Install the plugin/script runner and define its `systemd --user` unit
          (`punktfunk-scripting`). Enabled by default whenever the host is, and started by default
          too (see `autoStart`) — the game-library scanners are plugins, so a host without the
          runner has an empty library. It also runs whatever you put in
          `~/.config/punktfunk/scripts` or install as `punktfunk-plugin-*` under
          `~/.config/punktfunk/plugins`. A plugin auto-wires to the host's mgmt token + identity cert.
        '';
      };

      package = mkOption {
        type = types.package;
        default = self.packages.${system}.punktfunk-scripting;
        defaultText = literalExpression "punktfunk.packages.\${system}.punktfunk-scripting";
        description = "The punktfunk-scripting package (the bun-bundled Effect SDK runner).";
      };

      autoStart = mkOption {
        type = types.bool;
        default = cfg.scripting.enable;
        defaultText = literalExpression "config.services.punktfunk.scripting.enable";
        description = ''
          Start the runner automatically in every user's graphical session (adds it to the user
          `default.target`).

          ON by default, matching every other channel: the deb postinst and the RPM `%post` both
          run `systemctl --global enable punktfunk-scripting.service`, and the sysext image bakes
          in the `default.target.wants` symlink. It used to be opt-in here, on the reasoning that
          the runner does nothing until you add scripts or plugins — that stopped being true when
          the game-library scanners became plugins. A host whose runner is off now comes up with an
          empty library and no obvious reason why (design/library-scanner-plugins.md D9).

          It remains opt-OUT: set this to `false`, or per user
          `systemctl --user mask punktfunk-scripting`.
        '';
      };
    };
  };

  config = mkMerge [
    # --- shared: whenever either half is enabled -----------------------------------------------
    (mkIf (cfg.host.enable || cfg.client.enable) {
      assertions = [
        {
          # `settings` renders to a world-readable store path, and every value in it is inherited
          # by the games and hooks the host launches.
          assertion = !(lib.any (k: lib.hasInfix "TOKEN" k || lib.hasInfix "PASSWORD" k) (
            lib.attrNames cfg.host.settings
          ));
          message = ''
            services.punktfunk.host.settings must not carry a token or a password: it becomes a
            world-readable file in the Nix store. Use services.punktfunk.host.environmentFile, or
            let the host generate its own credentials in ~/.config/punktfunk/.
          '';
        }
        {
          assertion = system == "x86_64-linux";
          message = "services.punktfunk is x86_64-linux only (desktop NVENC host; no aarch64 build).";
        }
      ];
      # The GPU driver libs the binaries dlopen at runtime (libcuda / libnvidia-encode / libEGL /
      # the Vulkan ICD) live under /run/opengl-driver/lib — provided by hardware.graphics.
      hardware.graphics.enable = mkDefault true;

      # A WARNING, not `xdg.portal.enable = mkDefault true`: enabling the portal service without an
      # `extraPortals` backend is its own broken state, and only the operator knows which backend
      # their compositor needs. The desktop-manager modules (plasma6, gnome) already wire theirs, so
      # this fires exactly where it should — a headless/appliance or sway/Hyprland box assembled by
      # hand. It matters because the host reaches the desktop through portals on several backends:
      # Mutter's virtual output is ashpd ScreenCast/RemoteDesktop, and the libei input path's own
      # error message is "is xdg-desktop-portal-kde/gnome running and XDG_CURRENT_DESKTOP set?".
      warnings = optional (cfg.host.enable && !config.xdg.portal.enable) ''
        services.punktfunk.host is enabled but xdg.portal.enable is false. The host drives the
        compositor through xdg-desktop-portal on several backends (Mutter's ScreenCast/RemoteDesktop
        and the libei input path), so capture or input will fail there with a portal error. Set
        xdg.portal.enable = true and add the backend for your compositor, e.g.
        xdg.portal.extraPortals = [ pkgs.xdg-desktop-portal-kde ]   # or -gnome / -hyprland / -wlr
        (the KDE and GNOME desktop-manager modules already do this for you). The KWin backend's own
        virtual output uses the privileged zkde_screencast protocol and needs no portal.
      '';
      # 32 MB UDP socket buffers — without this the kernel clamps the host's SO_SNDBUF / client's
      # SO_RCVBUF and high-bitrate frames overflow (measured: 4 MB cap = 31.6 % loss at 2 Gbps).
      boot.kernel.sysctl = {
        "net.core.wmem_max" = mkDefault 33554432;
        "net.core.rmem_max" = mkDefault 33554432;
      };
    })

    # --- host ----------------------------------------------------------------------------------
    (mkIf cfg.host.enable {
      environment.systemPackages = [ cfg.host.package ];
      # 60-punktfunk.rules: /dev/uinput + /dev/uhid group access + the vhci sysfs perms.
      services.udev.packages = [ cfg.host.package ];
      # The vhci attach/detach rule shells out to chgrp/chmod (coreutils) — put them on udev's PATH.
      services.udev.path = [ pkgs.coreutils ];
      # uinput/uhid: the virtual X360 + DualSense nodes. vhci-hcd: the usbip transport that makes
      # the virtual Steam Deck a real USB device (Steam Input only adopts USB pads).
      boot.kernelModules = [
        "uinput"
        "uhid"
        "vhci-hcd"
      ];

      # `input` group membership for the virtual-gamepad nodes (mirrors the RPM's usermod hint).
      #
      # `punktfunk` is the SECOND group 60-punktfunk.rules needs: it owns the usbip vhci
      # attach/detach nodes, and is deliberately not `input` because writing `attach` materialises
      # an arbitrary emulated USB device — a root-only kernel primitive that must not ride on the
      # group every gamepad guide tells you to join (security-review 2026-08-05 M-4). Declaring the
      # group is not optional: the rule shells out to `chgrp punktfunk`, which fails outright if
      # nothing ever created it, leaving the nodes root-only and the virtual Steam Deck pad unable
      # to attach. Membership follows `host.users`, which is already the explicit "these users run
      # the host" list this option's description scopes to the usbip/vhci pad.
      users.groups.input = { };
      users.groups.punktfunk = { };
      users.users = genAttrs cfg.host.users (_: {
        extraGroups = [
          "input"
          "punktfunk"
        ];
      });

      # Status-tray autostart entry (self-gating: `--autostart` exits unless this user runs a host).
      environment.etc."xdg/autostart/io.unom.Punktfunk.Tray.desktop".source =
        "${cfg.host.package}/etc/xdg/autostart/io.unom.Punktfunk.Tray.desktop";

      networking.firewall = mkIf cfg.host.openFirewall {
        allowedTCPPorts = nativeTCP ++ optionals cfg.host.gamestream gamestreamTCP;
        allowedUDPPorts = nativeUDP ++ optionals cfg.host.gamestream gamestreamUDP;
      };

      # NO CAP_SYS_NICE wrapper for the HOST — deliberately, and note there is deliberately one for
      # the WORKER just below; the difference is the whole point. 0.26.0-1 gave the host a
      # `security.wrappers.punktfunk-host` carrying `cap_sys_nice=ep` for the GPU-priority lever,
      # and that broke desktop streaming on every KDE box.
      #
      # KWin advertises its restricted Wayland protocols (zkde_screencast_unstable_v1 for the
      # virtual output, org_kde_kwin_fake_input for input) only to a client it can IDENTIFY, by
      # resolving that client's /proc/<pid>/exe and matching it against an installed .desktop's
      # Exec= (packages.nix substitutes ours to the store path). The kernel refuses that readlink to
      # any reader whose effective set is not a superset of the target's PERMITTED set
      # (cap_ptrace_access_check), and KWin holds no capabilities.
      #
      # A NixOS wrapper does not dodge this. It raises the capability into its AMBIENT set before
      # exec'ing the store binary, precisely so the capability survives — which lands CAP_SYS_NICE
      # in the exec'd process's permitted set and fails the readlink identically. Measured: an
      # ambient-only grant (dumpable=1, CapPrm set) is refused exactly like a file capability. See
      # packaging/arch/punktfunk-host.install for the full matrix.
      #
      # The capability lives on the ENCODE WORKER instead — a different binary, and one nothing
      # ever has to identify.
      #
      # punktfunk-encode-worker is spawned per PyroWave session, speaks one socketpair to its
      # parent, and never connects to Wayland, D-Bus or the network. Nothing resolves ITS
      # /proc/<pid>/exe, so the ambient grant a NixOS wrapper performs — the very thing that makes
      # a wrapper useless for the host — is exactly right here. (It must also stay a SEPARATE file:
      # a hardlink or a host subcommand would share the inode, hence the capability, and re-create
      # the breakage above on every file-capability channel.)
      #
      # A file capability cannot live on a store path (read-only, and shared by every generation),
      # so `security.wrappers` is the only mechanism NixOS has — which is why the unit below points
      # PUNKTFUNK_ENCODE_WORKER at `config.security.wrapperDir` rather than the store path. The
      # host's own ExecStart stays on the store path and must never move.
      #
      # Best-effort by construction: if the wrapper is absent or the operator overrides the env,
      # the host falls back to its in-process encoder at default GPU priority — one warn, never a
      # dead session. What the capability buys: PyroWave encodes on the GPU shader cores a game
      # saturates, and every driver tested (NVIDIA and RADV) refuses EVERY elevated
      # VK_KHR_global_priority class without CAP_SYS_NICE. Measured on an RTX 5070 Ti under load:
      # encode p99 6.4 -> 4.4 ms.
      #
      # Narrow: CAP_SYS_NICE permits raising scheduling priority only — no filesystem, network or
      # user-switching privilege, and the wrapper is capability-based, NOT setuid.
      security.wrappers.punktfunk-encode-worker = {
        source = "${cfg.host.package}/bin/punktfunk-encode-worker";
        capabilities = "cap_sys_nice=ep";
        owner = "root";
        group = "root";
      };

      # CPU-side thread priority for the host's data-plane threads (capture/encode/send and the
      # 5 ms audio loop) rides RealtimeKit: the host asks rtkit to renice the thread when a direct
      # setpriority() is refused — the same unprivileged broker PipeWire clients use, so nothing
      # ever enters the host's permitted set and the KWin identification above stays intact.
      # NixOS is the one distro family where rtkit is not a given, hence the default here;
      # mkDefault so an operator who runs without rtkit can turn it off (the host then keeps its
      # pre-0.29 best-effort no-op behaviour, a pacing cost only).
      security.rtkit.enable = mkDefault true;

      systemd.user.services.punktfunk-host = {
        description = "punktfunk GameStream + punktfunk/1 streaming host";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # Keep root (and every other system user) from starting a second host that steals the
        # fixed ports from the desktop user's — see `userScope`.
        unitConfig.ConditionUser = userScope;
        # Soft ordering: the host listens immediately and only touches the compositor per session.
        after = [ "pipewire.service" ] ++ optional cfg.host.desktopSession "graphical-session.target";
        wants = [ "pipewire.service" ];
        # `graphical-session.target` is IN ADDITION to `default.target`, never instead of it: the
        # host still comes up at login before the graphical session is ready — it listens without
        # touching the compositor and only opens one per client connect, so an early start costs
        # nothing. `partOf` is the half that matters, taking the host down with the session so the
        # next one gets a fresh compositor connection (see `desktopSession`).
        partOf = optional cfg.host.desktopSession "graphical-session.target";
        wantedBy =
          optional cfg.host.autoStart "default.target"
          ++ optional cfg.host.desktopSession "graphical-session.target";
        # Nested gamescope does `exec steam -gamepadui …`. bash/coreutils/pipewire alone cannot
        # see programs.steam on /run/current-system/sw, so the child exits and PipeWire dies.
        path = [
          config.system.path
          pkgs.bash
          pkgs.coreutils
          pkgs.pipewire
        ]
        # The HDR-capable gamescope, if enabled. On PATH rather than pinned through
        # PUNKTFUNK_GAMESCOPE_BIN so an operator's own override of that env still wins.
        ++ optional cfg.host.gamescopeHdr cfg.host.gamescopePackage
        # The plugin runner, if enabled. Package ops (`plugins add`, and the console's store jobs,
        # which run INSIDE this service) exec `punktfunk-scripting`; its resolution order is
        # PUNKTFUNK_SCRIPTING -> beside the host binary -> PATH -> the /usr and ~/.local layouts.
        # On NixOS only the PATH rung can ever match: the runner is a derivation of its OWN, so it
        # is never beside the host binary and nothing lands in /usr. `environment.systemPackages`
        # covers an operator's interactive shell but NOT this unit, whose PATH is exactly this
        # list — without it the console reports a running, enabled runner as "not installed".
        ++ optional cfg.scripting.enable cfg.scripting.package
        # Host diagnostics exec `bwrap` on THIS unit's PATH, not the runner's.
        ++ optional cfg.scripting.enable pkgs.bubblewrap;
        # Point the host at the WRAPPED encode worker (see `security.wrappers` above). The host's
        # own resolution order is PUNKTFUNK_ENCODE_WORKER -> alongside /proc/self/exe -> PATH, and
        # on NixOS the sibling of the store binary is the UNCAPPED store copy — it would run, and
        # be refused every priority class, silently. This env is the whole reason the override
        # exists. `config.security.wrapperDir` rather than a hard-coded /run/wrappers/bin so an
        # operator who has moved it is still correct.
        #
        # NixOS renders `Environment=` before `EnvironmentFile=`, so `settings`/`environmentFile`
        # can still override this (or set it to `off` to force the in-process encoder) — the same
        # "an operator's own override still wins" posture as PUNKTFUNK_GAMESCOPE_BIN above.
        environment.PUNKTFUNK_ENCODE_WORKER = "${config.security.wrapperDir}/punktfunk-encode-worker";
        # Where our Vulkan WSI layer's manifest lives. The host defaults to the FHS path every
        # distro package uses, which no NixOS box has — here the layer travels inside the gamescope
        # derivation, so point at it. Without this a game nested under the compositor gets no HDR10
        # swapchain at all: that layer is the only route to one, and the host falls back to
        # disabling the system layer, which is HDR-less by construction.
        #
        # Same override posture as PUNKTFUNK_GAMESCOPE_BIN: `Environment=` renders before
        # `EnvironmentFile=`, so an operator's `settings` still wins.
        environment.PUNKTFUNK_GAMESCOPE_WSI_LAYER_DIR = mkIf cfg.host.gamescopeHdr
          "${cfg.host.gamescopePackage}/lib/punktfunk/vulkan/implicit_layer.d";
        serviceConfig = {
          # The store path DIRECTLY — not a capability wrapper. /proc/<pid>/exe then resolves to the
          # very path packages.nix substituted into io.unom.Punktfunk.Host.desktop's Exec=, which is
          # what lets KWin identify the host and grant it the screencast/fake-input protocols (see
          # the note above the firewall block).
          ExecStart =
            "${cfg.host.package}/bin/punktfunk-host serve" + optionalString cfg.host.gamestream " --gamestream";
          Restart = "on-failure";
          RestartSec = 2;
          EnvironmentFile =
            (optional (cfg.host.settings != { }) "${hostSettingsFile}")
            ++ (optional (cfg.host.environmentFile != null) "-${toString cfg.host.environmentFile}");
        };
        # No PUNKTFUNK_GAMESCOPE_HDR here: the host defaults it on, and the capability probe on
        # the resolved binary keeps a build-less box SDR. `gamescopeHdr` only controls whether
        # the patched binary is on PATH; `settings`/`environmentFile` can still set =0 to force SDR.
      };

      # A switch reloads user managers but never restarts a user service. A changed package
      # reloads this unit instead, which restarts the running host, console and runner.
      systemd.services.punktfunk-restart-user-units = {
        description = "Restart the punktfunk user services after a package change";
        wantedBy = [ "multi-user.target" ];
        reloadTriggers =
          [ cfg.host.package ]
          ++ optional cfg.web.enable cfg.web.package
          ++ optional cfg.scripting.enable cfg.scripting.package;
        serviceConfig = {
          Type = "oneshot";
          RemainAfterExit = true;
          ExecStart = "${pkgs.coreutils}/bin/true";
          ExecReload = "${pkgs.runtimeShell} ${../linux/restart-user-units.sh}";
        };
      };
    })

    # --- client --------------------------------------------------------------------------------
    (mkIf cfg.client.enable {
      environment.systemPackages = [ cfg.client.package ];
      # 70-punktfunk-client.rules: hidraw access for the seated user's DualSense (SDL HIDAPI). The
      # rule is uaccess-tagged, so the active-seat user gets it with no group membership.
      services.udev.packages = [ cfg.client.package ];

      networking.firewall = mkIf cfg.client.openFirewall {
        allowedUDPPorts = [ 5353 ];
      };
    })

    # --- web console ---------------------------------------------------------------------------
    # The declarative equivalent of the punktfunk-web .deb / RPM subpackage: the two systemd --user
    # units (the console + its first-run password generator) plus the firewall opener, all auto-wired
    # to the host's per-user mgmt token + identity cert (no env editing on a packaged install).
    (mkIf cfg.web.enable {
      environment.systemPackages = [ cfg.web.package ];

      networking.firewall = mkIf cfg.web.openFirewall {
        # 47992 = the console itself. 47993 = the SEPARATE ORIGIN its plugin UIs are served from
        # (console port + 1): same host and certificate, different port, so the browser's same-origin
        # policy keeps a plugin from acting as the logged-in operator. Leaving it closed does not
        # degrade gracefully — every plugin interface is simply an empty panel from any other device.
        # Keep in step with packaging/linux/punktfunk-web.xml and punktfunk.ufw.
        allowedTCPPorts = [
          47992
          47993
        ];
      };

      # Console pre-start: generate the login password once, in the user's config dir, surface it to
      # the --user journal, THEN block until the host has written the mgmt token + identity cert.
      # Being Type=oneshot, this is the readiness gate punktfunk-web's `after` already implies but
      # cannot enforce on its own (the host is Type=simple). Mirrors
      # scripts/punktfunk-web-init.service — keep both in step.
      systemd.user.services.punktfunk-web-init = {
        description = "punktfunk web console pre-start (login password; waits for the host)";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # NO ConditionPathExists on web-password: that skipped this unit from the second boot
        # onward, which is precisely when the host-readiness wait must still run. web-init.sh is
        # idempotent instead (it writes the password only when absent, and returns without sleeping
        # once the host's files exist). ConditionUser is TRIGGERING (`|`), so at least one must hold
        # — same shape as punktfunk-web below.
        unitConfig = {
          ConditionUser = userScope;
        };
        path = [ pkgs.coreutils ];
        serviceConfig = {
          Type = "oneshot";
          # Not remain-after-exit: an active oneshot is never re-run, so a console restart in a
          # live session skipped the host-readiness gate. Mirrors scripts/punktfunk-web-init.service.
          RemainAfterExit = false;
          ExecStart = "${cfg.web.package}/share/punktfunk-web/web-init.sh";
        };
      };

      # The console itself: Nitro SSR on bun, HTTPS on 47992 with the host's identity cert, proxying
      # the host's loopback mgmt API with the bearer token injected server-side. mgmt-token is
      # REQUIRED (the host's `serve` writes it) — a console with no token cannot reach the mgmt API
      # at all, so failing closed beats a login page that 401s on every call. punktfunk-web-init
      # waits for it, so this no longer fails on a fresh install's first start; if it is somehow
      # still absent the unit fails and Restart retries. web-password is optional ('-'). Mirrors
      # scripts/punktfunk-web.service.
      systemd.user.services.punktfunk-web = {
        description = "punktfunk management web console";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # Same scoping as the host: root's instance would take 47992 from the real one.
        unitConfig.ConditionUser = userScope;
        # punktfunk-web-init is the readiness gate (Type=oneshot, so ordering after it genuinely
        # waits). `punktfunk-host.service` alone is NOT one — the host is Type=simple, so systemd
        # calls it started the instant it is spawned, seconds before it writes the mgmt token and
        # its identity cert. Keep web-init in BOTH lists: the ordering is what makes it a gate.
        after = [
          "punktfunk-web-init.service"
          "punktfunk-host.service"
        ];
        # wants, not requires: if the pre-start unit itself fails, still try — the Restart backstop
        # below recovers, where a hard dependency would leave the console down.
        wants = [ "punktfunk-web-init.service" ];
        wantedBy = optional cfg.web.autoStart "default.target";
        # Backstop for whatever the gate cannot cover (web-init timed out; the host restarted and
        # re-minted its identity under a running console; someone deleted a file). Retry
        # INDEFINITELY: the EnvironmentFile below is mandatory on purpose, so the unit genuinely
        # fails while the token is missing — and systemd's default rate limit (5 starts / 10 s)
        # against `RestartSec = 2` gives up permanently after ~10 s. A console that hit a missing
        # file then stayed dead until someone restarted it by hand. The shipped unit
        # (scripts/punktfunk-web.service) has carried this since that defect was found; it was
        # missed in the port, while the comment below went on promising the behaviour it removes.
        unitConfig.StartLimitIntervalSec = 0;
        environment = {
          # PUNKTFUNK_MGMT_URL is deliberately absent: the host publishes the port it actually bound
          # to ~/.config/punktfunk/mgmt-endpoint (mgmt::publish_endpoint), sourced below, and the
          # server falls back to https://127.0.0.1:47990 on its own when that file does not exist.
          # Hardcoding it here would have to out-rank the file, which is a directive-ordering
          # question in the generated unit — so we simply do not create the conflict.
          PORT = "47992";
          # Where both listeners bind (`web.bind`).
          PUNKTFUNK_UI_BIND = cfg.web.bind;
          # Serve HTTPS with the host's own identity cert (the anchor native clients already pin) and
          # mark the session cookie Secure. The host's `serve` writes these PEMs.
          #
          # These name the LEGACY pair; the server prefers the native sibling
          # (native-cert.pem/native-key.pem) when it exists, because a generated unit cannot express
          # "this file, else that one" any more than the hand-written one can. The choice is made in
          # web/nitro-entry/tls-paths.mjs — keep this in step with scripts/punktfunk-web.service.
          PUNKTFUNK_UI_TLS_CERT = "%h/.config/punktfunk/cert.pem";
          PUNKTFUNK_UI_TLS_KEY = "%h/.config/punktfunk/key.pem";
          PUNKTFUNK_UI_SECURE = "1";
        };
        serviceConfig = {
          Type = "simple";
          EnvironmentFile = [
            "%h/.config/punktfunk/mgmt-token"
            # Generated by the web-init unit, then rewritten by the console itself as
            # PUNKTFUNK_UI_PASSWORD_HASH on the first sign-in. Optional ('-') either way.
            "-%h/.config/punktfunk/web-password"
            # The host's effective mgmt URL — see the `environment` note above. Optional: absent on
            # a host predating mgmt::publish_endpoint, and the server default covers that.
            "-%h/.config/punktfunk/mgmt-endpoint"
          ];
          ExecStart = "${cfg.web.package}/bin/punktfunk-web-server";
          # `always`, not `on-failure`: a console that exits 0 has still stopped serving, and
          # `on-failure` would leave it down. An explicit `systemctl --user stop` is still honoured
          # (Restart= never fights that). Matches scripts/punktfunk-web.service and the Windows
          # web-run.cmd, both of which relaunch bun on ANY exit.
          Restart = "always";
          RestartSec = 2;
        };
      };
    })

    # --- plugin/script runner ------------------------------------------------------------------
    # Installs the runner + defines its opt-in `systemd --user` unit (mirrors the deb/rpm
    # punktfunk-scripting subpackage). NOT auto-started unless `scripting.autoStart` is set.
    (mkIf cfg.scripting.enable {
      environment.systemPackages = [
        cfg.scripting.package
        pkgs.bubblewrap
      ];

      systemd.user.services.punktfunk-scripting = {
        description = "punktfunk plugin/script runner";
        documentation = [ "https://git.unom.io/unom/punktfunk" ];
        # Same scoping as the host: a root-side runner would talk to the wrong session's mgmt API.
        unitConfig.ConditionUser = userScope;
        # Plugins talk to the host's loopback mgmt API; order after it (soft — the runner backs off
        # and retries per unit, so this is ordering only, not a hard requirement).
        after = [ "punktfunk-host.service" ];
        wantedBy = optional cfg.scripting.autoStart "default.target";
        # Each plugin runs inside its own `bwrap` sandbox, built by the runner.
        path = [ pkgs.bubblewrap ];
        serviceConfig = {
          Type = "simple";
          ExecStart = "${cfg.scripting.package}/bin/punktfunk-scripting";
          # `+` runs outside the namespace: the plugin-run and plugin-state binds need both
          # directories, and inside the read-only home nothing can create them.
          ExecStartPre = "+${pkgs.coreutils}/bin/mkdir -p -m 0700 %h/.config/punktfunk/plugin-run %h/.config/punktfunk/plugin-state";
          Restart = "on-failure";
          RestartSec = 2;
          # Deliver SIGTERM to the runner (it orchestrates the structural shutdown of its unit
          # fibers) and give it room to run their finalizers before the cgroup is reaped.
          KillMode = "mixed";
          KillSignal = "SIGTERM";
          TimeoutStopSec = 30;

          # Unit-level hardening, the same scripts/punktfunk-scripting.service gives the deb/rpm
          # installs — keep the two in step, module-check.nix asserts each directive. It confines
          # the supervisor and the operator's loose scripts, not a PLUGIN: those get their own
          # bwrap sandbox, because a mount namespace on a same-uid unit is no boundary.
          NoNewPrivileges = true;
          # PrivateTmp deliberately OFF (field report 2026-08-03, the VirtualHere plugin). A
          # plugin's whole job is integrating with things already running on this box, and on Linux
          # those talk over /tmp: VirtualHere's client IPC is the FIFO pair /tmp/vhclient +
          # /tmp/vhclient_response, X11 is /tmp/.X11-unix. A private /tmp hides all of it — the
          # plugin launches the vendor binary fine and then cannot reach the daemon behind it,
          # which presents as an error no amount of config fixes.
          PrivateTmp = false;
          ProtectSystem = "strict";
          # The plugin token's privilege split, made real by the filesystem: a plugin holds a
          # capability-limited token, so the runner must not reach the full-admin `mgmt-token` or
          # the identity key beside it. Same uid as the operator means file modes cannot say that;
          # an empty home plus an explicit allow-list can. Mirrors the Windows runner's ACL, which
          # grants LocalService the scoped token and the TLS pin and nothing else.
          #
          # InaccessiblePaths is belt and braces: wherever the home tmpfs does not land, or a
          # drop-in removes ProtectHome, the denylist is what still stands.
          #
          # ⚠ All of these are MOUNT-NAMESPACE options, and for a *user* unit that needs
          # unprivileged user namespaces. On a kernel/config that restricts those they fail the
          # unit rather than degrading — drop them via
          #   systemctl --user edit punktfunk-scripting
          # which is also where a loose script that must reach elsewhere gets `ReadWritePaths=`
          # (outside the home) or `BindReadOnlyPaths=` (inside it). Plugins get folder grants.
          ProtectHome = "tmpfs";
          InaccessiblePaths = [
            "-%h/.config/punktfunk/mgmt-token"
            "-%h/.config/punktfunk/key.pem"
            "-%h/.config/punktfunk/native-key.pem"
          ];
          # Writable: the installed packages (the runner refreshes their shared SDK with
          # `bun install` on every start) and the one directory a plugin may persist into. `%t` is
          # $XDG_RUNTIME_DIR, which ProtectHome empties too — the host publishes its live-stream
          # marker there, and the session bus and compositor sockets live there.
          BindPaths = [
            "-%h/.config/punktfunk/plugins"
            "-%h/.config/punktfunk/plugin-state"
            # bun's package cache ($BUN_INSTALL). Only the SDK refresh writes here, and it fails
            # soft — but soft means plugins keep an old SDK across host upgrades.
            "-%h/.bun"
            "%t"
          ];
          # Read-only. The punktfunk entries are the runner's credentials and its inputs: the
          # scoped token, the TLS pin (native-cert.pem after the identity split, cert.pem before
          # it), the port the host really bound, the operator's loose scripts, and the drop box
          # another local account fills.
          #
          # What the plugins read in the home — manifest paths and folder grants — the host writes
          # into ~/.config/systemd/user/punktfunk-scripting.service.d/50-plugin-roots.conf.
          #
          # The punktfunk-scripting entries cover the SteamOS layout, which builds the runner under
          # the home and points ExecStart at it. Every path but plugin-run is `-`: ExecStartPre
          # creates that one.
          BindReadOnlyPaths = [
            "-%h/.config/punktfunk/plugin-token"
            # Per-plugin tokens and grants. A directory bind keeps the host's atomic replacements
            # visible; a file bind would pin the deleted inode.
            "%h/.config/punktfunk/plugin-run"
            "-%h/.config/punktfunk/native-cert.pem"
            "-%h/.config/punktfunk/cert.pem"
            "-%h/.config/punktfunk/mgmt-endpoint"
            "-%h/.config/punktfunk/scripts"
            "-%h/.config/punktfunk/ingest"
            "-%h/.local/bin/punktfunk-scripting"
            "-%h/.local/lib/punktfunk-scripting"
            "-%h/.local/share/punktfunk-scripting"
          ];
          # Puts back the write bit ProtectSystem=strict takes away from the real /tmp above.
          ReadWritePaths = [ "/tmp" ];
          # Free for a JS runtime that only talks HTTP and reads files. Absent on purpose:
          # MemoryDenyWriteExecute (bun JITs), PrivateDevices (VirtualHere forwards USB), and
          # ProtectKernelTunables, which refuses bwrap a fresh /proc; the sandbox binds /proc/sys ro.
          ProtectControlGroups = true;
          # Exactly the namespaces each plugin's bwrap sandbox is built from; `true` starts no plugin.
          RestrictNamespaces = "user mnt pid net ipc uts cgroup";
          SystemCallArchitectures = "native";
          CapabilityBoundingSet = "";
          # AF_NETLINK brings up a sandbox's loopback; its seccomp filter keeps it from the plugin.
          RestrictAddressFamilies = [
            "AF_UNIX"
            "AF_INET"
            "AF_INET6"
            "AF_NETLINK"
          ];
        };
      };
    })
  ];
}
