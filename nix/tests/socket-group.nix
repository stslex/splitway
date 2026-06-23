# NixOS VM test for the unprivileged-GUI socket group (Phase 6).
#
# Proves the end-to-end deployment contract that the daemon unit tests cannot:
# with `services.splitway.unprivilegedGui` enabled, an in-group user can drive
# the root daemon over its control socket using `splitway` (a read-only verb),
# while an out-of-group user is denied with EACCES — and the runtime dir + socket
# carry the expected `root:<group>` ownership and `0750`/`0660` modes.
#
# This needs /dev/kvm, which GitHub's default runners do not reliably expose, so
# the flake keeps it out of `checks` (run by CI's `nix flake check`) and exposes
# it under `legacyPackages` for a manual/local run:
#   nix build .#legacyPackages.x86_64-linux.tests.socketGroup -L
# See docs/design/socket-group.md.
{ self, pkgs }:
pkgs.testers.runNixOSTest {
  name = "splitway-socket-group";

  nodes.machine =
    { ... }:
    {
      imports = [ self.nixosModules.default ];

      services.splitway = {
        enable = true;
        unprivilegedGui = {
          enable = true;
          # `alice` is granted access; `mallory` (below) is deliberately left out.
          users = [ "alice" ];
        };
      };

      # Realism: resolvectl is the daemon's DNS backend tool. The socket test
      # never applies DNS (no VPN in the VM), but enabling resolved keeps the
      # node closer to a real deployment.
      services.resolved.enable = true;

      users.users.alice = {
        isNormalUser = true;
      };
      users.users.mallory = {
        isNormalUser = true;
      };
    };

  testScript = ''
    machine.start()
    machine.wait_for_unit("splitway.service")

    # The control socket comes up inside the runtime dir.
    machine.wait_until_succeeds("test -S /run/splitway/splitway.sock")

    # Defense in depth: dir is 0750 root:splitway, socket is 0660 root:splitway.
    machine.succeed(
        "stat -c '%U %G %a' /run/splitway | grep -x 'root splitway 750'"
    )
    machine.succeed(
        "stat -c '%U %G %a' /run/splitway/splitway.sock | grep -x 'root splitway 660'"
    )

    # The daemon (root) carries the socket group as a supplementary group
    # (SupplementaryGroups), so its /proc/<pid>/status lists the group's GID.
    machine.succeed(
        "gid=$(getent group splitway | cut -d: -f3); "
        "pid=$(systemctl show -p MainPID --value splitway.service); "
        "grep '^Groups:' /proc/$pid/status | grep -qw \"$gid\""
    )

    # An in-group user can drive the daemon over the socket (read-only verb).
    machine.succeed("su - alice -c 'splitway status'")

    # An out-of-group user is denied — and specifically with a permission error
    # (the daemon is running), not a 'not running' error. The 0750 dir blocks
    # traversal to the socket, so connect() returns EACCES.
    out = machine.fail("su - mallory -c 'splitway status' 2>&1")
    assert "permission denied" in out.lower(), (
        f"expected a permission-denied error for an out-of-group user, got: {out}"
    )

    # Sanity: mallory cannot even traverse the runtime dir to reach the socket.
    machine.fail("su - mallory -c 'test -r /run/splitway/splitway.sock'")
  '';
}
