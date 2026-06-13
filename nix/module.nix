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

    # No systemd service yet. The binary is one-shot today
    # (`splitway-daemon run` / `revert` / `status`); the long-running
    # daemon that watches the VPN interface and applies/reverts DNS rules
    # automatically arrives in Phase 2. The eventual unit will look
    # roughly like the sketch below — kept commented out so this module
    # does not invent behavior the binary does not have yet:
    #
    # systemd.services.splitway = {
    #   description = "Splitway split-DNS daemon";
    #   after = [ "network-online.target" "NetworkManager.service" ];
    #   wants = [ "network-online.target" ];
    #   wantedBy = [ "multi-user.target" ];
    #   serviceConfig = {
    #     ExecStart = "${lib.getExe cfg.package} run";
    #     Restart = "on-failure";
    #   };
    # };
  };
}
