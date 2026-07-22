{
  config,
  lib,
  pkgs,
  self,
  ...
}: let
  cfg = config.programs.sonix.goxlr;
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
  pklString = value: builtins.toJSON value;
  outputSinksPkl = lib.concatMapStringsSep "\n" (sink: "    ${pklString sink}") effectiveOutputSinks;
  goxlrDaemonUnit = "app-goxlr\\x2ddaemon@autostart.service";
  servicePath = lib.makeBinPath [
    package
    pkgs.goxlr-utility
    pkgs.pipewire
    pkgs.pulseaudio
  ];
  configFile = pkgs.writeText "goxlr-nexus-config.pkl" ''
        outputSinks = new Listing {
    ${outputSinksPkl}
        }
        ${lib.optionalString (cfg.jdsSink != null) ''jdsSink = ${pklString cfg.jdsSink}''}
        ${lib.optionalString (effectiveFallbackSink != null) ''fallbackSink = ${pklString effectiveFallbackSink}''}
        ${lib.optionalString (cfg.thinkpadSink != null) ''thinkpadSink = ${pklString cfg.thinkpadSink}''}
        ${lib.optionalString (cfg.goxlrSerial != null) ''goxlrSerial = ${pklString cfg.goxlrSerial}''}

        profile = new {
          defaultSink = ${pklString cfg.defaultSink}
          defaultSource = ${pklString cfg.defaultSource}
          monitorSource = ${pklString cfg.monitorSource}
        }

        processing = new {
        enable = ${
      if cfg.processing.enable
      then "true"
      else "false"
    }
        sourceName = ${pklString cfg.processing.sourceName}
        sourceDescription = ${pklString cfg.processing.sourceDescription}
        noiseSuppression = ${
      if cfg.processing.noiseSuppression
      then "true"
      else "false"
    }
        echoCancellation = ${
      if cfg.processing.echoCancellation
      then "true"
      else "false"
    }
        noiseLevel = ${pklString cfg.processing.noiseLevel}
        ${lib.optionalString (cfg.processing.echoDelayMs != null) ''echoDelay = new { mode = "fixed"; milliseconds = ${toString cfg.processing.echoDelayMs} }''}
        }

        obs = new {
        enable = ${
      if cfg.obs.enable
      then "true"
      else "false"
    }
        host = ${pklString cfg.obs.host}
        port = ${toString cfg.obs.port}
        ${lib.optionalString (cfg.obs.passwordFile != null) "passwordFile = ${pklString cfg.obs.passwordFile}"}
        }
  '';
in {
  options.programs.sonix.goxlr = {
    enable = mkEnableOption "GoXLR/JDS/PipeWire/OBS audio orchestration";
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
    processing = {
      enable = mkOption {
        type = types.bool;
        default = true;
        description = "Run the separate user-level PipeWire processing service.";
      };
      sourceName = mkOption {
        type = types.str;
        default = "goxlr_nexus.processed_mic";
      };
      sourceDescription = mkOption {
        type = types.str;
        default = "GoXLR Nexus processed microphone";
      };
      noiseSuppression = mkOption {
        type = types.bool;
        default = false;
      };
      echoCancellation = mkOption {
        type = types.bool;
        default = false;
      };
      noiseLevel = mkOption {
        type = types.enum ["low" "moderate" "high" "very-high"];
        default = "high";
      };
      echoDelayMs = mkOption {
        type = types.nullOr types.ints.between 0 500;
        default = null;
        description = "Optional fixed AEC delay in milliseconds; null selects automatic timing.";
      };
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
      after =
        ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
      wants =
        ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
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
