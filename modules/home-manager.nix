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
  obsSourceToml =
    lib.concatMapStrings
    (source: ''

      [[obs.sources]]
      name = "${source.name}"
      device-id = "${source.deviceId}"
    '')
    cfg.obs.sources;
  configFile = pkgs.writeText "goxlr-nexus-home-config.toml" ''
    user = "${cfg.user}"
    output-sinks = [${outputSinksToml}]
    ${lib.optionalString (cfg.jdsSink != null) ''jds-sink = "${cfg.jdsSink}"''}
    ${lib.optionalString (effectiveFallbackSink != null) ''fallback-sink = "${effectiveFallbackSink}"''}
    ${lib.optionalString (cfg.thinkpadSink != null) ''thinkpad-sink = "${cfg.thinkpadSink}"''}
    ${lib.optionalString (cfg.goxlrSerial != null) ''goxlr-serial = "${cfg.goxlrSerial}"''}
    ${lib.optionalString (cfg.maxMonitorSinkVolume != null) ''max-monitor-sink-volume = ${toString cfg.maxMonitorSinkVolume}''}
    observe-only = ${if cfg.observeOnly then "true" else "false"}

    [obs]
    enable = ${
      if cfg.obs.enable
      then "true"
      else "false"
    }
    host = "${cfg.obs.host}"
    port = ${toString cfg.obs.port}
    ${lib.optionalString (cfg.obs.passwordFile != null) "password-file = \"${cfg.obs.passwordFile}\""}
    ${obsSourceToml}
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
    goxlrCard = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Pulse/PipeWire card to reselect before starting the routing controller.";
    };
    goxlrProfile = mkOption {
      type = types.str;
      default = "HiFi";
      description = "GoXLR ALSA card profile to activate before starting the routing controller.";
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
            deviceId = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
          }
          {
            name = "GoXLR Stream Mix";
            deviceId = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
          }
          {
            name = "GoXLR Sampler";
            deviceId = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line5__source";
          }
          {
            name = "GoXLR Desktop Mix";
            deviceId = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
          }
        ];
      };
    };
  };

  config = mkIf cfg.enable {
    home.packages = [package];
    assertions = [
      {
        assertion = !cfg.obs.manageSettings || !cfg.obs.authRequired || cfg.obs.passwordFile != null;
        message = "programs.goxlr-nexus.obs.authRequired requires programs.goxlr-nexus.obs.passwordFile when manageSettings is enabled.";
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
    systemd.user.services.goxlr-nexus = {
      Unit = {
        Description = "GoXLR Nexus audio routing repair";
        After = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
        Wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service" goxlrDaemonUnit];
        PartOf = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
        StartLimitIntervalSec = 300;
        StartLimitBurst = 20;
      };
      Service = {
        Type = "simple";
        Environment = "PATH=${servicePath}";
        # WirePlumber 0.5 can report the requested profile while retaining
        # only the hidden raw ALSA nodes after a restart.  A real profile
        # transition (off -> HiFi) makes ACP recreate the Pulse-visible
        # named source/sink nodes that desktop selectors consume.
        ExecStartPre = lib.optionals (cfg.goxlrCard != null) [
          "-${pkgs.pulseaudio}/bin/pactl set-card-profile ${lib.escapeShellArg cfg.goxlrCard} off"
          "-${pkgs.pulseaudio}/bin/pactl set-card-profile ${lib.escapeShellArg cfg.goxlrCard} ${lib.escapeShellArg cfg.goxlrProfile}"
        ];
        ExecStart = "${package}/bin/goxlr-nexus --config ${configFile} follow";
        Restart = "on-failure";
        RestartSec = 5;
        CPUQuota = "10%";
        MemoryHigh = "64M";
        MemoryMax = "128M";
        TasksMax = 32;
        TimeoutStopSec = 10;
        LogRateLimitIntervalSec = 30;
        LogRateLimitBurst = 100;
      };
      Install.WantedBy = ["default.target"];
    };
  };
}
