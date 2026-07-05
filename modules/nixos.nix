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
  configFile = pkgs.writeText "goxlr-nexus-config.toml" ''
    user = "${cfg.user}"
    jds-sink = "${cfg.jdsSink}"
    goxlr-serial = "${cfg.goxlrSerial}"

    [profile]
    default-sink = "${cfg.defaultSink}"
    default-source = "${cfg.defaultSource}"
    monitor-source = "${cfg.monitorSource}"

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
    defaultSink = mkOption {
      type = types.str;
      default = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
    };
    defaultSource = mkOption {
      type = types.str;
      default = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
    };
    monitorSource = mkOption {
      type = types.str;
      default = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
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
    environment.systemPackages = [package pkgs.pipewire pkgs.pulseaudio];
    systemd.user.services.goxlr-nexus = {
      description = "GoXLR Nexus audio routing repair";
      after = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
      wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
      wantedBy = ["default.target"];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${package}/bin/goxlr-nexus --config ${configFile} apply";
      };
    };
  };
}
