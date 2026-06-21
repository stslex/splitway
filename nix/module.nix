# NixOS module for Splitway.
#
# Runtime dependencies: the daemon shells out to `nmcli` (NetworkManager)
# and `resolvectl` (systemd-resolved). Hosts that set
# `networking.networkmanager.enable = true` and
# `services.resolved.enable = true` already have both binaries in PATH,
# so this module does not pull them in itself — enabling those services
# is left to the host configuration.
#
# Config model — imperative, not declarative. The daemon owns a *writable*
# config at /var/lib/splitway/config.json (provisioned by systemd's
# StateDirectory) and the GUI/CLI mutate it at runtime; the daemon also picks up
# external hand-edits live. This module therefore does NOT generate a read-only
# /etc config — that would break runtime mutation. A future option could *seed*
# an initial config, but must never *lock* it read-only. Daily-driving on NixOS
# is via the flake's `services.splitway.enable = true;`. See docs/architecture.md
# ("Config is the single source of truth") and ROADMAP.md (Phase 5c).
self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.splitway;
  gui = cfg.unprivilegedGui;
  # Only appended to ExecStart when the unprivileged-GUI path is enabled, so the
  # default deployment runs byte-identically to before (no flag => 0600 socket).
  socketGroupArg = lib.optionalString gui.enable " --socket-group ${gui.group}";
in
{
  options.services.splitway = {
    enable = lib.mkEnableOption "Splitway split-DNS tool";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.system}.default;
      defaultText = lib.literalExpression "splitway.packages.\${system}.default";
      description = "The Splitway package to install.";
    };

    # Opt-in: let a non-root user in a dedicated group drive the root daemon over
    # its control socket, so the GUI can run unprivileged under niri (no system
    # tray; see ROADMAP.md Phase 7). Disabled => the socket stays 0600 root-only
    # and nothing below changes. SECURITY: membership in this group grants the
    # ability to drive the daemon's privileged split-DNS operations
    # (resolvectl/nmcli) — adding a user to it ≈ granting control of system
    # split-DNS routing. That is why it is opt-in and `users` is empty by default.
    # (Stronger per-peer auth via SO_PEERCRED is a later phase.)
    unprivilegedGui = {
      enable = lib.mkEnableOption ''
        a group-accessible control socket so an unprivileged in-group user (the
        GUI under niri) can reach the root daemon without sudo. Grants that group
        control of system split-DNS routing — see the security note'';

      group = lib.mkOption {
        type = lib.types.str;
        default = "splitway";
        description = ''
          Group that owns the control socket (`0660`) and its runtime dir
          (`0750`). Created by this module when {option}`unprivilegedGui.enable`
          is set. Members can drive the daemon's privileged DNS operations.
        '';
      };

      users = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "alice" ];
        description = ''
          Existing user accounts to add to {option}`unprivilegedGui.group`.
          Empty by default: the module never silently grants DNS-control rights —
          the operator opts in by listing users here (or adds the group in their
          own `users.users.<name>.extraGroups`). Listed users must already be
          declared elsewhere in the configuration.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [
    {
      environment.systemPackages = [ cfg.package ];
    }

    # Unprivileged-GUI path (opt-in). Create the dedicated group and (optionally)
    # add the listed users to it. Kept in a separate mkMerge branch so the default
    # deployment declares no group and no membership at all.
    (lib.mkIf gui.enable {
      # Dynamic GID is fine for a runtime socket; pin only if there's a reason to.
      users.groups.${gui.group} = { };
      # Add each listed (existing) user to the group. Empty `users` => no-op, so
      # the module never silently grants DNS-control rights. This only *augments*
      # existing users; it does not create them.
      users.users = lib.genAttrs gui.users (_: { extraGroups = [ gui.group ]; });
      # Turn the opaque downstream failure for an undeclared user (the line above
      # would otherwise materialize a half-defined account, failing eval with a
      # generic "exactly one of isNormalUser/isSystemUser" assertion) into a
      # message that points back at this option — e.g. an `alise`-for-`alice` typo.
      assertions = lib.map (user: {
        assertion = config.users.users.${user}.isNormalUser || config.users.users.${user}.isSystemUser;
        message = ''
          services.splitway.unprivilegedGui.users lists "${user}", but no such
          account is declared. Declare it (e.g. users.users."${user}".isNormalUser
          = true) or remove it from the list — this option only adds existing users
          to the "${gui.group}" group, it does not create them.
        '';
      }) gui.users;
    })

    {
    # Long-running daemon: watches the VPN interface and applies/reverts
    # split-DNS rules automatically, and serves the CLI's control socket.
    #
    # Runs as root (the default): `resolvectl` DNS changes are privileged.
    # The 0600 control socket is the privilege boundary for the CLI; see
    # packaging/README.md for the threat model.
    systemd.services.splitway = {
      description = "Splitway split-DNS daemon";
      documentation = [ "https://github.com/stslex/splitway" ];
      after = [
        "network-online.target"
        "NetworkManager.service"
        "systemd-resolved.service"
      ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      # One-time upgrade migration. The pre-5c module ran the daemon with no
      # `--config`, so it used the daemon's default path — which, for this root
      # service, resolves to /root/.config/splitway/config.json (whether or not
      # systemd set $HOME, the daemon's own fallback is that path). Now the config
      # lives in the StateDirectory. Seed the new path from the old one on the
      # first start after an upgrade so an existing vpn_name/domains are not
      # silently dropped (the daemon would otherwise create an empty config). The
      # guard makes this a no-op on fresh installs and every later start, and it
      # never overwrites an existing new-path config.
      #
      # Use an absolute `cp` (`[`/`echo` are bash builtins) rather than adding
      # coreutils to the service `path`: the daemon resolves its runtime tools
      # (`nmcli` / `resolvectl`) by bare name from the host's PATH, so the service
      # PATH must be left untouched.
      preStart = ''
        old=/root/.config/splitway/config.json
        new=/var/lib/splitway/config.json
        if [ ! -e "$new" ] && [ -e "$old" ]; then
          echo "splitway: migrating config from $old to $new"
          ${pkgs.coreutils}/bin/cp -p "$old" "$new"
        fi
      '';

      serviceConfig = {
        # The writable config lives in the StateDirectory (below). On first run
        # the daemon creates an empty config there if absent. `--socket-group` is
        # appended only when unprivilegedGui is enabled (else the socket is 0600).
        ExecStart = "${lib.getExe cfg.package} run --config /var/lib/splitway/config.json${socketGroupArg}";
        Restart = "on-failure";
        RestartSec = 2;
        # systemd creates /run/splitway before start and removes it on stop; the
        # daemon binds its control socket inside it. Default 0700 (root-only);
        # with the GUI path it is 0750 so the socket group can traverse to the
        # socket (the daemon chgrps the dir to the group on start). The daemon
        # re-applies dir+socket perms itself, so this mode is defense in depth.
        RuntimeDirectory = "splitway";
        RuntimeDirectoryMode = if gui.enable then "0750" else "0700";
        # systemd creates /var/lib/splitway (0700), owned by the service and
        # persisted across restarts: the daemon's writable config file. This is
        # the imperative model — the daemon owns the file, the GUI mutates it —
        # not a module-generated read-only /etc config.
        StateDirectory = "splitway";
        StateDirectoryMode = "0700";
        # SIGTERM is trapped by the daemon to revert DNS rules before exit,
        # so a stop never leaves the system half-configured.
        KillSignal = "SIGTERM";
        TimeoutStopSec = 10;
      }
      # Put the (root) daemon process in the socket group too. Root can chgrp the
      # dir/socket to the group regardless, so this is not strictly required today;
      # it future-proofs a later drop to a non-root user and documents intent.
      // lib.optionalAttrs gui.enable {
        SupplementaryGroups = [ gui.group ];
      };
    };
    }
  ]);
}
