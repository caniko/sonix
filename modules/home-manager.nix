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
    jds-sink = "${cfg.jdsSink}"
    goxlr-serial = "${cfg.goxlrSerial}"

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
      type = types.str;
      default = "alsa_output.usb-Yoyodyne_Consulting_JDS_Labs_Element_DAC-01.analog-stereo";
    };
    goxlrSerial = mkOption {
      type = types.str;
      default = "S200805412CQK";
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
