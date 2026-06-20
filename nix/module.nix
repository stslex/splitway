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
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

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
      serviceConfig = {
        # The writable config lives in the StateDirectory (below). On first run
        # the daemon creates an empty config there if absent.
        ExecStart = "${lib.getExe cfg.package} run --config /var/lib/splitway/config.json";
        Restart = "on-failure";
        RestartSec = 2;
        # systemd creates /run/splitway (0700) before start and removes it on
        # stop; the daemon binds its 0600 control socket inside it.
        RuntimeDirectory = "splitway";
        RuntimeDirectoryMode = "0700";
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
      };
    };
  };
}
