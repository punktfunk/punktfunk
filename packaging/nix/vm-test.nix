# Boots the NixOS module with the real packages. The host, console and runner units come up from
# the packages' own files plus the module's drop-ins, and the runner keeps the sandbox its shipped
# unit declares. Then the door switches on and off through its helper and polkit rule, and its two
# units serve under their sandbox. It builds the host, so only the `ci:nix-rust` lane of nix.yml
# builds this.
{
  testers,
  module,
}:
testers.runNixOSTest {
  name = "punktfunk-module";

  nodes.machine = {
    imports = [ module ];
    services.punktfunk.host = {
      enable = true;
      users = [ "alice" ];
      autoStart = true;
      # A gamescope build of its own, and nothing here needs it.
      gamescopeHdr = false;
    };
    services.punktfunk.seats.enable = true;
    # Lingering starts alice's manager at boot, as on an appliance. Both may switch the door.
    users.users.alice = {
      isNormalUser = true;
      linger = true;
      extraGroups = [ "punktfunk-update" ];
    };
    users.users.bob = {
      isNormalUser = true;
      extraGroups = [ "punktfunk-update" ];
    };
    virtualisation.memorySize = 3072;
  };

  testScript =
    { nodes, ... }:
    ''
    def show(unit, *props):
        args = " ".join(f"-p {p}" for p in props)
        return machine.succeed(f"systemctl --user -M alice@ show {unit} {args}")

    machine.wait_for_unit("multi-user.target")
    for unit in ("punktfunk-host", "punktfunk-web", "punktfunk-scripting"):
        machine.wait_for_unit(f"{unit}.service", "alice")
    # The console serves only once the host has written its token and certificate.
    machine.wait_for_open_port(47990)
    machine.wait_for_open_port(47992)

    # KWin identifies the host by its store path; a wrapper there breaks desktop streaming.
    exec_start = show("punktfunk-host", "ExecStart")
    assert "-punktfunk-host-" in exec_start and "/run/wrappers" not in exec_start, exec_start

    sandbox = show(
        "punktfunk-scripting",
        "NoNewPrivileges",
        "ProtectSystem",
        "ProtectHome",
        "PrivateTmp",
        "ProtectControlGroups",
        "CapabilityBoundingSet",
        "RestartPreventExitStatus",
        "InaccessiblePaths",
    )
    for want in (
        "NoNewPrivileges=yes",
        "ProtectSystem=strict",
        "ProtectHome=tmpfs",
        "PrivateTmp=no",
        "ProtectControlGroups=yes",
        "CapabilityBoundingSet=\n",
        "RestartPreventExitStatus=78",
        "/home/alice/.config/punktfunk/mgmt-token",
    ):
        assert want in sandbox, f"{want!r} missing from:\n{sandbox}"

    # The door, as a member starts it. No seat starts here: the VM has no GPU.
    seats = "${nodes.machine.services.punktfunk.seats.package}/libexec/punktfunk/punktfunk-seats"
    box = "/var/lib/punktfunk"
    record = "/var/lib/punktfunk-seats/owner"

    def switch(user, verb):
        return f"su - {user} -c 'systemctl start punktfunk-door-{verb}@{user}.service'"

    machine.wait_for_unit("punktfunk-seats.service")
    machine.succeed(switch("alice", "on"))
    machine.wait_for_unit("punktfunk-door.service")
    assert machine.succeed(f"cat {record}").strip() == "alice"
    owner = machine.succeed(f"stat -c %U:%G {box}/native-key.pem").strip()
    assert owner == "punktfunk:punktfunk-door", owner
    machine.succeed(f"{seats} list | grep -q owner=true")

    # Both door units work under their sandbox, not only start: the console answers a request,
    # and the door answers on the management port the owner's host had.
    machine.wait_for_unit("punktfunk-web-door.service")
    machine.wait_for_open_port(47992)
    machine.wait_for_open_port(47990)
    machine.succeed("curl -fsk -o /dev/null https://127.0.0.1:47992/")
    machine.succeed("curl -fsk https://127.0.0.1:47990/api/v1/health | grep -q '\"status\":\"ok\"'")

    # A repeat changes nothing, and another member can't take a door that is on.
    machine.succeed(switch("alice", "on"))
    machine.fail(switch("bob", "on"))
    assert machine.succeed(f"cat {record}").strip() == "alice"

    # What changed on the door goes home with the switch off.
    machine.succeed(f"runuser -u punktfunk -- sh -c 'echo 7 > {box}/web-session-epoch'")
    machine.succeed(switch("alice", "off"))
    machine.succeed(f"test ! -e {record}")
    machine.fail("systemctl is-active punktfunk-door.service")
    epoch = machine.succeed("cat /home/alice/.config/punktfunk/web-session-epoch").strip()
    assert epoch == "7", epoch

    # A record whose account is gone names no one: another member's on takes the door over.
    machine.succeed(switch("alice", "on"))
    machine.wait_for_unit("punktfunk-door.service")
    machine.succeed("loginctl disable-linger alice; loginctl terminate-user alice || true")
    machine.succeed("userdel -f alice")
    machine.wait_until_fails("getent passwd alice")
    machine.succeed(switch("bob", "on"))
    assert machine.succeed(f"cat {record}").strip() == "bob"
    machine.succeed(f"{seats} list | grep owner=true | grep -q ' bob '")
    machine.wait_for_unit("punktfunk-door.service")
    machine.succeed(switch("bob", "off"))
    machine.succeed(f"test ! -e {record}")

    # A first on that fails puts the box back: no record, no mark, the supervisor as it was.
    # NixOS's units sit in /etc, above a --runtime mask: the control directory is above both.
    control = "/run/systemd/system.control"
    supervisor = machine.execute("systemctl is-active punktfunk-seats.service")[1].strip()
    machine.succeed(f"mkdir -p {control} && ln -s /dev/null {control}/punktfunk-door.service")
    machine.succeed("systemctl daemon-reload")
    machine.fail(switch("bob", "on"))
    machine.wait_until_succeeds(
        "journalctl -u punktfunk-door-on@bob.service | grep -q 'putting the box back'"
    )
    machine.succeed(f"test ! -e {record} && test ! -e /var/lib/punktfunk-seats/door-on")
    after = machine.execute("systemctl is-active punktfunk-seats.service")[1].strip()
    assert after == supervisor, f"supervisor {supervisor} before, {after} after"
    machine.succeed(f"rm {control}/punktfunk-door.service && systemctl daemon-reload")

    # An off whose hand-back fails leaves the door off and held, drops bob's contract, and keeps
    # the record and the mark: bob can't write his own folder. With it writable again, the next
    # off hands the files home.
    conf = "/home/bob/.config/punktfunk"
    hold = "/var/lib/punktfunk-seats/door-hold"
    machine.succeed(switch("bob", "on"))
    machine.wait_for_unit("punktfunk-door.service")
    machine.succeed(f"chmod 0500 {conf}")
    machine.fail(switch("bob", "off"))
    machine.wait_until_succeeds(
        "journalctl -u punktfunk-door-off@bob.service | grep -q 'switch the door off again'"
    )
    assert machine.succeed(f"cat {record}").strip() == "bob"
    machine.succeed(f"test -e /var/lib/punktfunk-seats/door-on && test -e {hold}")
    machine.succeed("test ! -e /run/punktfunk/seats/bob.env")
    machine.fail("systemctl is-active punktfunk-door.service")
    machine.succeed(f"chmod 0700 {conf}")
    machine.succeed(switch("bob", "off"))
    machine.succeed(f"test ! -e {record} && test ! -e {hold}")

    # The console's password only in web.env, as SteamOS keeps it: the on takes the password and
    # secret lines, the off splices the door's last password line back into web.env.
    machine.succeed(f"runuser -u bob -- rm -f {conf}/web-password")
    machine.succeed(
        f"runuser -u bob -- sh -c 'printf \"%s\\n\" PUNKTFUNK_UI_BIND=127.0.0.1:47990 "
        f"PUNKTFUNK_UI_PASSWORD_HASH=bob-1 PUNKTFUNK_UI_SECRET=bob-s > {conf}/web.env'"
    )
    machine.succeed(switch("bob", "on"))
    machine.wait_for_unit("punktfunk-door.service")
    taken = machine.succeed(f"runuser -u punktfunk -- cat {box}/web-password")
    assert taken == "PUNKTFUNK_UI_PASSWORD_HASH=bob-1\nPUNKTFUNK_UI_SECRET=bob-s\n", taken
    machine.succeed(
        "runuser -u punktfunk -- sh -c "
        f"'echo PUNKTFUNK_UI_PASSWORD_HASH=bob-2 >> {box}/web-password'"
    )
    machine.succeed(switch("bob", "off"))
    machine.succeed(f"test ! -e {record} && test ! -e {conf}/web-password")
    kept = machine.succeed(f"cat {conf}/web.env")
    assert kept == (
        "PUNKTFUNK_UI_BIND=127.0.0.1:47990\nPUNKTFUNK_UI_SECRET=bob-s\n"
        "PUNKTFUNK_UI_PASSWORD_HASH=bob-2\n"
    ), kept
  '';
}
