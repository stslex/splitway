# NixOS module for Splitway.
#
# Runtime dependencies: the daemon shells out to `nmcli` (NetworkManager)
# and `resolvectl` (systemd-resolved). Hosts that set
# `networking.networkmanager.enable = true` and
# `services.resolved.enable = true` already have both binaries in PATH,
# so this module does not pull them in itself — enabling those services
# is left to the host configuration.
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
        ExecStart = "${lib.getExe cfg.package} run";
        Restart = "on-failure";
        RestartSec = 2;
        # systemd creates /run/splitway (0700) before start and removes it on
        # stop; the daemon binds its 0600 control socket inside it.
        RuntimeDirectory = "splitway";
        RuntimeDirectoryMode = "0700";
        # SIGTERM is trapped by the daemon to revert DNS rules before exit,
        # so a stop never leaves the system half-configured.
        KillSignal = "SIGTERM";
        TimeoutStopSec = 10;
      };
    };
  };
}
