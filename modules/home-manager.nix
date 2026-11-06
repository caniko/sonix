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
  artifacts = import ./goxlr-artifacts.nix {inherit lib pkgs;} {
    inherit cfg package configFile servicePath;
  };
  servicePath = lib.makeBinPath [
    package
    pkgs.goxlr-utility
    pkgs.pipewire
    pkgs.pulseaudio
  ];
  obsSourcesPkl =
    lib.concatMapStringsSep
    "\n"
    (source: ''
      new {
        name = ${pklString source.name}
        deviceId = ${pklString source.deviceId}
      }
    '')
    cfg.obs.sources;
  configFile = pkgs.writeText "goxlr-nexus-home-config.pkl" ''
        outputSinks = new Listing {
    ${outputSinksPkl}
        }
        ${lib.optionalString (cfg.jdsSink != null) ''jdsSink = ${pklString cfg.jdsSink}''}
        ${lib.optionalString (effectiveFallbackSink != null) ''fallbackSink = ${pklString effectiveFallbackSink}''}
        ${lib.optionalString (cfg.thinkpadSink != null) ''thinkpadSink = ${pklString cfg.thinkpadSink}''}
        ${lib.optionalString (cfg.goxlrSerial != null) ''goxlrSerial = ${pklString cfg.goxlrSerial}''}
        ${lib.optionalString (cfg.maxMonitorSinkVolume != null) ''maxMonitorSinkVolume = ${toString cfg.maxMonitorSinkVolume}''}
        observeOnly = ${
      if cfg.observeOnly
      then "true"
      else "false"
    }

        profile = new {
        defaultSink = ${pklString effectiveRenderTarget}
        defaultSource = ${pklString effectiveCaptureSource}
        monitorSource = ${pklString cfg.monitorSource}
        captureSampleRate = ${toString effectiveCaptureSampleRate}
        renderSampleRate = ${toString effectiveRenderSampleRate}
        captureChannels = ${toString effectiveCaptureChannels}
        renderChannels = ${toString effectiveRenderChannels}
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
        sources = new Listing {
    ${obsSourcesPkl}
        }
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

        telemetry = new {
        enable = ${lib.boolToString cfg.telemetry.enable}
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
    goxlrCard = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Pulse/PipeWire card to reselect before starting the routing controller.";
    };
    goxlrProfile = mkOption {
      type = types.str;
      default = "HiFi";
      description = "GoXLR ALSA card profile to activate after the session manager has finished activating the card.";
    };
    activationProfile = mkOption {
      type = types.str;
      default = "Direct";
      description = ''
        Card profile WirePlumber selects while it activates the GoXLR. It must
        stay a single-PCM verb: selecting a UCM SplitPCM verb (the `HiFi` verb
        that `goxlrProfile` needs) makes WirePlumber 0.5.16+ build the split
        loopbacks inside device activation, which deadlocks the session manager
        and hangs every PipeWire client. `goxlrProfile` is applied by the
        routing controller afterwards, outside that window.
      '';
    };
    maxMonitorSinkVolume = mkOption {
      type = types.nullOr types.float;
      default = null;
      description = "Maximum selected monitor sink volume as a normalized value between 0 and 1.";
    };
    observeOnly = mkOption {
      type = types.bool;
      default = false;
      description = "Observe and report routing drift without applying audio mutations.";
    };
    telemetry.enable = mkOption {
      type = types.bool;
      default = false;
      description = "Record local-only GoXLR FX telemetry for crackling/distortion triage. No audio is recorded and nothing leaves the host.";
    };
    monitorSource = mkOption {
      type = types.str;
      default = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
      description = "GoXLR stream-mix source used for routing and monitoring.";
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
      manageSettings = mkEnableOption "OBS websocket plugin settings";
      host = mkOption {
        type = types.str;
        default = "127.0.0.1";
      };
      port = mkOption {
        type = types.port;
        default = 4455;
      };
      serverEnabled = mkOption {
        type = types.bool;
        default = true;
        description = "Whether Home Manager should enable the OBS websocket server.";
      };
      authRequired = mkOption {
        type = types.bool;
        default = false;
        description = "Whether Home Manager should require OBS websocket authentication.";
      };
      passwordFile = mkOption {
        type = types.nullOr types.str;
        default = null;
      };
      sources = mkOption {
        type = types.listOf (types.submodule {
          options = {
            name = mkOption {
              type = types.str;
              description = "OBS source name to create or update.";
            };
            deviceId = mkOption {
              type = types.str;
              description = "Pulse/PipeWire device id for the OBS source.";
            };
          };
        });
        default = [
          {
            name = "GoXLR Mic";
            # Follows the default source: processed when healthy, raw otherwise.
            deviceId = "default";
          }
          {
            name = "GoXLR Stream Mix";
            deviceId = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
          }
          {
            name = "GoXLR Sampler";
            deviceId = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line5__source";
          }
        ];
      };
    };
  };

  config = mkIf cfg.enable {
    home.packages = [package];
    assertions = [
      {
        assertion = !sonixCfg.laptop.enable;
        message = "programs.sonix.goxlr.enable cannot be combined with programs.sonix.laptop.enable.";
      }
      {
        # WirePlumber 0.5.16+ deadlocks activating a card whose profile builds
        # UCM SplitPCM loopbacks.  The activation profile must therefore differ
        # from the SplitPCM verb the routing controller selects afterwards, or
        # the session manager hangs on the next device activation.
        assertion = cfg.goxlrCard == null || cfg.activationProfile != cfg.goxlrProfile;
        message = "programs.sonix.goxlr.activationProfile must differ from goxlrProfile: selecting ${cfg.goxlrProfile} while WirePlumber activates the card deadlocks the session manager. Leave activationProfile on a single-PCM verb and let goxlr-nexus do the transition.";
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
      {
        assertion = cfg.maxMonitorSinkVolume == null || (cfg.maxMonitorSinkVolume >= 0.0 && cfg.maxMonitorSinkVolume <= 1.0);
        message = "programs.sonix.goxlr.maxMonitorSinkVolume must be between 0 and 1.";
      }
      {
        assertion = !cfg.obs.manageSettings || !cfg.obs.authRequired || cfg.obs.passwordFile != null;
        message = "programs.sonix.goxlr.obs.authRequired requires programs.sonix.goxlr.obs.passwordFile when manageSettings is enabled.";
      }
    ];
    xdg.configFile."obs-studio/plugin_config/obs-websocket/config.json" = mkIf cfg.obs.manageSettings {
      text = builtins.toJSON {
        alerts_enabled = false;
        auth_required = cfg.obs.authRequired;
        first_load = false;
        server_enabled = cfg.obs.serverEnabled;
        server_password = "";
        server_port = cfg.obs.port;
      };
    };
    # WirePlumber must never select a UCM SplitPCM verb while it activates the
    # card, and the GoXLR daemon must be owned rather than left to the
    # package's XDG autostart entry; modules/goxlr-artifacts.nix holds the
    # reasoning and the values, and CI asserts them.
    xdg.configFile."wireplumber/wireplumber.conf.d/60-sonix-goxlr.conf" = mkIf (cfg.goxlrCard != null) {
      text = artifacts.wireplumberRule;
    };
    xdg.configFile."autostart/goxlr-daemon.desktop".text = artifacts.autostartMask;
    systemd.user.services.goxlr-daemon = artifacts.daemon;
    systemd.user.services.goxlr-daemon-health = {
      Unit.Description = artifacts.daemonHealth.description;
      Service = artifacts.daemonHealth.serviceConfig;
    };
    systemd.user.timers.goxlr-daemon-health = {
      Unit.Description = artifacts.daemonHealth.timer.description;
      Timer = artifacts.daemonHealth.timer.timerConfig;
      Install.WantedBy = artifacts.daemonHealth.timer.wantedBy;
    };
    systemd.user.services.goxlr-nexus = artifacts.nexus;
  };
}
