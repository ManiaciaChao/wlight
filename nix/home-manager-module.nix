self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.wlight;
in
{
  options.services.wlight = {
    enable = lib.mkEnableOption "the wlight user daemon";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.wlight;
      defaultText = lib.literalExpression "inputs.wlight.packages.${pkgs.stdenv.hostPlatform.system}.wlight";
      description = "The wlight package to use.";
    };
    autostart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Start wlightd with the graphical user session.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];
    systemd.user.services.wlight = lib.mkIf cfg.autostart {
      Unit = {
        Description = "wlight monitor brightness service";
        PartOf = [ "graphical-session.target" ];
        After = [ "graphical-session.target" ];
      };
      Service = {
        Type = "dbus";
        BusName = "io.github.wlight";
        ExecStart = "${cfg.package}/bin/wlightd";
        Restart = "on-failure";
        RestartSec = 2;
      };
      Install.WantedBy = [ "graphical-session.target" ];
    };
  };
}
