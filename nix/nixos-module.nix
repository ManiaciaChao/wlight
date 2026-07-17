self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.wlight;
in
{
  options.programs.wlight = {
    enable = lib.mkEnableOption "wlight monitor brightness control";
    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.wlight;
      defaultText = lib.literalExpression "inputs.wlight.packages.${pkgs.stdenv.hostPlatform.system}.wlight";
      description = "The wlight package to install.";
    };
    enableI2cAccess = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Load i2c-dev and grant local users access through the standard NixOS i2c rules.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
    hardware.i2c.enable = lib.mkIf cfg.enableI2cAccess true;
  };
}
