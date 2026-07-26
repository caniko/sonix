{
  config,
  lib,
  pkgs,
  self,
  ...
}: let
  cfg = config.programs.sonix;
  external =
    if cfg.laptop.enable
    then cfg.laptop
    else cfg.externalInput;
  package = self.packages.${pkgs.stdenv.hostPlatform.system}.sonix;
  jsonConfig = pkgs.writeText "sonix-processing-config.json" (builtins.toJSON {
    captureSource = external.captureSource;
    renderTarget = external.renderTarget;
    sourceName = external.sourceName;
    sourceDescription = external.sourceDescription;
    captureSampleRate = external.captureSampleRate;
    renderSampleRate = external.renderSampleRate;
    captureChannels = external.captureChannels;
    renderChannels = external.renderChannels;
    noiseSuppression = cfg.processing.noiseSuppression;
    echoCancellation = cfg.processing.echoCancellation;
    noiseLevel = cfg.processing.noiseLevel;
    echoDelay =
      if cfg.processing.echoDelayMs == null
      then {mode = "auto";}
      else {mode = "fixed"; milliseconds = cfg.processing.echoDelayMs;};
  });
  active = cfg.enable && (cfg.externalInput.enable || cfg.laptop.enable);
in {
  options.programs.sonix = {
    enable = lib.mkEnableOption "Sonix generic audio processing";

    externalInput = {
      enable = lib.mkEnableOption "process the configured external input";
      captureSource = lib.mkOption {
        type = lib.types.str;
        default = "default";
        description = "PipeWire capture node, or default to follow the session input.";
      };
      renderTarget = lib.mkOption {
        type = lib.types.str;
        default = "default";
        description = "PipeWire render sink, or default to follow the session output.";
      };
      sourceName = lib.mkOption {
        type = lib.types.str;
        default = "sonix.processed_mic";
      };
      sourceDescription = lib.mkOption {
        type = lib.types.str;
        default = "Sonix processed microphone";
      };
      captureSampleRate = lib.mkOption {
        type = lib.types.ints.positive;
        default = 48000;
      };
      renderSampleRate = lib.mkOption {
        type = lib.types.ints.positive;
        default = 48000;
      };
      captureChannels = lib.mkOption {
        type = lib.types.ints.between 0 32;
        default = 0;
        description = "Capture channels; zero detects the connected input layout.";
      };
      renderChannels = lib.mkOption {
        type = lib.types.ints.between 1 32;
        default = 2;
      };
    };

    laptop = {
      enable = lib.mkEnableOption "laptop Sonix input processing";
      captureSource = lib.mkOption {type = lib.types.str; default = "default";};
      renderTarget = lib.mkOption {type = lib.types.str; default = "default";};
      sourceName = lib.mkOption {type = lib.types.str; default = "sonix.laptop.processed_mic";};
      sourceDescription = lib.mkOption {type = lib.types.str; default = "Sonix laptop processed microphone";};
      captureSampleRate = lib.mkOption {type = lib.types.ints.positive; default = 48000;};
      renderSampleRate = lib.mkOption {type = lib.types.ints.positive; default = 48000;};
      captureChannels = lib.mkOption {
        type = lib.types.ints.between 0 32;
        default = 0;
        description = "Zero detects native external-microphone layout; built-ins can be pinned to mono.";
      };
      renderChannels = lib.mkOption {type = lib.types.ints.between 1 32; default = 2;};
    };

    processing = {
      noiseSuppression = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };
      echoCancellation = lib.mkOption {
        type = lib.types.bool;
        default = true;
      };
      noiseLevel = lib.mkOption {
        type = lib.types.enum ["low" "moderate" "high" "very-high"];
        default = "high";
      };
      echoDelayMs = lib.mkOption {
        type = lib.types.nullOr (lib.types.ints.between 0 500);
        default = null;
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !active || external.captureSource != "";
        message = "programs.sonix requires a captureSource for an active implementation";
      }
      {
        assertion = !cfg.laptop.enable || (!cfg.externalInput.enable);
        message = "programs.sonix.laptop and programs.sonix.externalInput are mutually exclusive";
      }
    ];
    home.packages = lib.mkIf active [package pkgs.pipewire pkgs.pulseaudio];
    systemd.user.services.sonix-noise-echo = lib.mkIf active {
      Unit = {
        Description = "Sonix echo cancellation and noise suppression";
        After = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
        Wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
        PartOf = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
      };
      Service = {
        Type = "simple";
        Environment = "PATH=${lib.makeBinPath [package pkgs.pipewire pkgs.pulseaudio]}";
        ExecStart = "${package}/bin/sonix --config ${jsonConfig} daemon";
        ExecStopPost = "${package}/bin/sonix --config ${jsonConfig} fail-open";
        Restart = "on-failure";
        RestartSec = 2;
        CPUQuota = "35%";
        MemoryHigh = "128M";
        MemoryMax = "256M";
        TasksMax = 64;
        TimeoutStopSec = 10;
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
