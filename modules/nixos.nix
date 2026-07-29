{
  config,
  lib,
  pkgs,
  self,
  ...
}: let
  sonixCfg = config.programs.sonix;
  cfg = sonixCfg.goxlr;
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
  effectiveCaptureSource =
    if cfg.defaultSource == null
    then
      if sonixCfg.externalInput.enable
      then sonixCfg.externalInput.captureSource
      else "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source"
    else cfg.defaultSource;
  effectiveRenderTarget =
    if cfg.defaultSink == null
    then
      if sonixCfg.externalInput.enable
      then sonixCfg.externalInput.renderTarget
      else "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink"
    else cfg.defaultSink;
  effectiveCaptureSampleRate =
    if sonixCfg.externalInput.enable
    then sonixCfg.externalInput.captureSampleRate
    else 48000;
  effectiveRenderSampleRate =
    if sonixCfg.externalInput.enable
    then sonixCfg.externalInput.renderSampleRate
    else 48000;
  effectiveCaptureChannels =
    if sonixCfg.externalInput.enable
    then sonixCfg.externalInput.captureChannels
    else 2;
  effectiveRenderChannels =
    if sonixCfg.externalInput.enable
    then sonixCfg.externalInput.renderChannels
    else 2;
  effectiveProcessing = {
    sourceName =
      if cfg.processing.sourceName == null
      then
        if sonixCfg.externalInput.enable
        then sonixCfg.externalInput.sourceName
        else "goxlr_nexus.processed_mic"
      else cfg.processing.sourceName;
    sourceDescription =
      if cfg.processing.sourceDescription == null
      then
        if sonixCfg.externalInput.enable
        then sonixCfg.externalInput.sourceDescription
        else "GoXLR Nexus processed microphone"
      else cfg.processing.sourceDescription;
    noiseSuppression =
      cfg.processing.enable
      != false
      && (
        if cfg.processing.noiseSuppression == null
        then sonixCfg.processing.noiseSuppression
        else cfg.processing.noiseSuppression
      );
    echoCancellation =
      cfg.processing.enable
      != false
      && (
        if cfg.processing.echoCancellation == null
        then sonixCfg.processing.echoCancellation
        else cfg.processing.echoCancellation
      );
    noiseLevel =
      if cfg.processing.noiseLevel == null
      then sonixCfg.processing.noiseLevel
      else cfg.processing.noiseLevel;
    echoDelayMs =
      if cfg.processing.echoDelayMs == null
      then sonixCfg.processing.echoDelayMs
      else cfg.processing.echoDelayMs;
  };
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
          defaultSink = ${pklString effectiveRenderTarget}
          defaultSource = ${pklString effectiveCaptureSource}
          monitorSource = ${pklString cfg.monitorSource}
          captureSampleRate = ${toString effectiveCaptureSampleRate}
          renderSampleRate = ${toString effectiveRenderSampleRate}
          captureChannels = ${toString effectiveCaptureChannels}
          renderChannels = ${toString effectiveRenderChannels}
        }

        processing = new {
        sourceName = ${pklString effectiveProcessing.sourceName}
        sourceDescription = ${pklString effectiveProcessing.sourceDescription}
        noiseSuppression = ${
      if effectiveProcessing.noiseSuppression
      then "true"
      else "false"
    }
        echoCancellation = ${
      if effectiveProcessing.echoCancellation
      then "true"
      else "false"
    }
        noiseLevel = ${pklString effectiveProcessing.noiseLevel}
        ${lib.optionalString (effectiveProcessing.echoDelayMs != null) ''echoDelay = new { mode = "fixed"; milliseconds = ${toString effectiveProcessing.echoDelayMs} }''}
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
    defaultSink = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Deprecated alias for programs.sonix.externalInput.renderTarget.";
    };
    defaultSource = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Deprecated alias for programs.sonix.externalInput.captureSource.";
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
    monitorSource = mkOption {
      type = types.str;
      default = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
    };
    processing = {
      enable = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Deprecated compatibility option; processing is embedded in GoXLR Nexus.";
      };
      sourceName = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Deprecated alias for programs.sonix.externalInput.sourceName.";
      };
      sourceDescription = mkOption {
        type = types.nullOr types.str;
        default = null;
        description = "Deprecated alias for programs.sonix.externalInput.sourceDescription.";
      };
      noiseSuppression = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Deprecated alias for programs.sonix.processing.noiseSuppression.";
      };
      echoCancellation = mkOption {
        type = types.nullOr types.bool;
        default = null;
        description = "Deprecated alias for programs.sonix.processing.echoCancellation.";
      };
      noiseLevel = mkOption {
        type = types.nullOr (types.enum ["low" "moderate" "high" "very-high"]);
        default = null;
        description = "Deprecated alias for programs.sonix.processing.noiseLevel.";
      };
      echoDelayMs = mkOption {
        type = types.nullOr (types.ints.between 0 500);
        default = null;
        description = "Deprecated alias for programs.sonix.processing.echoDelayMs.";
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
    assertions = [
      {
        assertion = !sonixCfg.laptop.enable;
        message = "programs.sonix.goxlr.enable cannot be combined with programs.sonix.laptop.enable.";
      }
      {
        assertion = effectiveCaptureSource != "default" && effectiveRenderTarget != "default";
        message = "programs.sonix.goxlr requires concrete externalInput captureSource and renderTarget node names.";
      }
      {
        assertion = !sonixCfg.externalInput.enable || (effectiveCaptureChannels > 0 && effectiveRenderChannels > 0);
        message = "programs.sonix.goxlr requires positive externalInput channel counts.";
      }
      {
        assertion = builtins.all (value: lib.strings.trim value != "") [
          effectiveCaptureSource
          effectiveRenderTarget
          effectiveProcessing.sourceName
          effectiveProcessing.sourceDescription
          cfg.monitorSource
        ];
        message = "Sonix and GoXLR audio node names must not be blank.";
      }
      {
        assertion = builtins.all (rate: rate >= 8000 && rate <= 384000 && (rate / 100) * 100 == rate) [
          effectiveCaptureSampleRate
          effectiveRenderSampleRate
        ];
        message = "Sonix sample rates must be between 8000 and 384000 Hz and divisible by 100.";
      }
      {
        assertion = cfg.outputSinks == null || lib.unique cfg.outputSinks == cfg.outputSinks;
        message = "programs.sonix.goxlr.outputSinks must not contain duplicates.";
      }
    ];
    systemd.user.services.goxlr-nexus = {
      description = "GoXLR Nexus routing with embedded Sonix processing";
      after = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
      wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
      wantedBy = ["default.target"];
      serviceConfig = {
        Type = "simple";
        Environment = "PATH=${servicePath}";
        ExecStart = "${package}/bin/goxlr-nexus --config ${configFile} follow";
        ExecStopPost = "-${package}/bin/goxlr-nexus --config ${configFile} processing fail-open";
        Restart = "on-failure";
        RestartSec = 5;
        CPUQuota = "35%";
        MemoryHigh = "128M";
        MemoryMax = "256M";
        TasksMax = 64;
        TimeoutStopSec = 10;
      };
      unitConfig = {
        StartLimitIntervalSec = 300;
        StartLimitBurst = 20;
      };
    };
  };
}
