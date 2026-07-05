{
  config,
  lib,
  pkgs,
  self,
  ...
}: let
  cfg = config.programs.goxlr-nexus;
  inherit (lib) mkEnableOption mkIf mkOption types;
  package = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
  configFile = pkgs.writeText "goxlr-nexus-home-config.toml" ''
    user = "${cfg.user}"
    jds-sink = "${cfg.jdsSink}"
    goxlr-serial = "${cfg.goxlrSerial}"

    [obs]
    enable = ${if cfg.obs.enable then "true" else "false"}
    host = "${cfg.obs.host}"
    port = ${toString cfg.obs.port}
    ${lib.optionalString (cfg.obs.passwordFile != null) "password-file = \"${cfg.obs.passwordFile}\""}
  '';
in {
  options.programs.goxlr-nexus = {
    enable = mkEnableOption "GoXLR/JDS/PipeWire/OBS audio orchestration";
    user = mkOption {
      type = types.str;
      default = "can";
    };
    jdsSink = mkOption {
      type = types.str;
      default = "alsa_output.usb-Yoyodyne_Consulting_JDS_Labs_Element_DAC-01.analog-stereo";
    };
    goxlrSerial = mkOption {
      type = types.str;
      default = "S200805412CQK";
    };
    obs = {
      enable = mkEnableOption "OBS websocket synchronization";
      host = mkOption {
        type = types.str;
        default = "127.0.0.1";
      };
      port = mkOption {
        type = types.port;
        default = 4455;
      };
      passwordFile = mkOption {
        type = types.nullOr types.str;
        default = null;
      };
    };
  };

  config = mkIf cfg.enable {
    home.packages = [package];
    systemd.user.services.goxlr-nexus = {
      Unit = {
        Description = "GoXLR Nexus audio routing repair";
        After = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
        Wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
      };
      Service = {
        Type = "oneshot";
        ExecStart = "${package}/bin/goxlr-nexus --config ${configFile} apply";
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
