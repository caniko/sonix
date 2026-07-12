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
  effectiveOutputSinks =
    if cfg.outputSinks == null
    then lib.filter (sink: sink != null) [cfg.jdsSink cfg.thinkpadSink]
    else cfg.outputSinks;
  effectiveFallbackSink =
    if cfg.fallbackSink == null
    then cfg.jdsSink
    else cfg.fallbackSink;
  outputSinksToml = lib.concatMapStringsSep ", " (sink: "\"${sink}\"") effectiveOutputSinks;
  goxlrDaemonUnit = "app-goxlr\\x2ddaemon@autostart.service";
  servicePath = lib.makeBinPath [
    package
    pkgs.goxlr-utility
    pkgs.pipewire
    pkgs.pulseaudio
  ];
  configFile = pkgs.writeText "goxlr-nexus-config.toml" ''
    user = "${cfg.user}"
    output-sinks = [${outputSinksToml}]
    ${lib.optionalString (cfg.jdsSink != null) ''jds-sink = "${cfg.jdsSink}"''}
    ${lib.optionalString (effectiveFallbackSink != null) ''fallback-sink = "${effectiveFallbackSink}"''}
    ${lib.optionalString (cfg.thinkpadSink != null) ''thinkpad-sink = "${cfg.thinkpadSink}"''}
    ${lib.optionalString (cfg.goxlrSerial != null) ''goxlr-serial = "${cfg.goxlrSerial}"''}

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
      type = types.nullOr types.str;
      default = null;
    };
    fallbackSink = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "PipeWire sink to select when the current desktop output is not managed. Defaults to jdsSink.";
    };
    thinkpadSink = mkOption {
      type = types.nullOr types.str;
      default = null;
    };
    outputSinks = mkOption {
      type = types.nullOr (types.listOf types.str);
      default = null;
      description = "Selectable PipeWire output sinks. Defaults to jdsSink and thinkpadSink.";
    };
    goxlrSerial = mkOption {
      type = types.nullOr types.str;
      default = null;
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
      after = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
      wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
      wantedBy = ["default.target"];
      serviceConfig = {
        Type = "simple";
        Environment = "PATH=${servicePath}";
        ExecStart = "${package}/bin/goxlr-nexus --config ${configFile} follow";
        Restart = "always";
        RestartSec = 5;
      };
      unitConfig = {
        StartLimitIntervalSec = 300;
        StartLimitBurst = 20;
      };
    };
  };
}
