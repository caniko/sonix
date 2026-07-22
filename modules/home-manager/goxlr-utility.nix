{
  config,
  lib,
  pkgs,
  self,
  ...
}:
let
  cfg = config.programs.goxlr-utility;
  inherit (lib) mkEnableOption mkIf mkOption types;
  system = pkgs.stdenv.hostPlatform.system;
  package =
    if cfg.package != null
    then cfg.package
    else self.packages.${system}.default;
  dataRoot = "${config.xdg.dataHome}/goxlr-utility";
  configRoot = "${config.xdg.configHome}/goxlr-utility";
  daemonUnit = "app-goxlr\\x2ddaemon@autostart.service";
  servicePath = lib.makeBinPath [ package pkgs.goxlr-utility ];

  mkFile =
    kind: name: source: {
      path = "${kind}/${name}";
      inherit source;
      target = "${dataRoot}/${kind}/${name}";
    };

  files =
    (lib.mapAttrsToList (mkFile "profiles") cfg.profileFiles)
    ++ (lib.mapAttrsToList (mkFile "mic-profiles") cfg.micProfileFiles)
    ++ (lib.mapAttrsToList (mkFile "presets") cfg.presetFiles)
    ++ (lib.mapAttrsToList (mkFile "samples") cfg.sampleFiles)
    ++ (lib.mapAttrsToList (mkFile "icons") cfg.iconFiles)
    ++ lib.optional (cfg.settingsFile != null) {
      path = "settings.json";
      source = cfg.settingsFile;
      target = "${configRoot}/settings.json";
    };

  manifest = pkgs.writeText "goxlr-config-manifest.json" (builtins.toJSON {
    version = 1;
    module = "programs.goxlr-utility";
    inherit files;
  });

  applyFlag = lib.optionalString cfg.applyOnActivation " --apply";
in
{
  options.programs.goxlr-utility = {
    enable = mkEnableOption "declarative GoXLR Utility artifacts";

    package = mkOption {
      type = types.nullOr types.package;
      default = null;
      defaultText = lib.literalExpression "inputs.sonix.packages.${system}.default";
      description = "Package providing the deterministic goxlr-config CLI.";
    };

    settingsFile = mkOption {
      type = types.nullOr types.path;
      default = null;
      description = ''
        Complete GoXLR Utility settings.json source. Native settings are kept
        as an opaque, validated artifact so no persistent field is lost.
      '';
    };

    profileFiles = mkOption {
      type = types.attrsOf types.path;
      default = {};
      description = "Native .goxlr profile files keyed by their exact filename.";
    };

    micProfileFiles = mkOption {
      type = types.attrsOf types.path;
      default = {};
      description = "Native .goxlrMicProfile files keyed by their exact filename.";
    };

    presetFiles = mkOption {
      type = types.attrsOf types.path;
      default = {};
      description = "Native .preset files keyed by their exact filename.";
    };

    sampleFiles = mkOption {
      type = types.attrsOf types.path;
      default = {};
      description = "Optional sample assets keyed by their exact filename.";
    };

    iconFiles = mkOption {
      type = types.attrsOf types.path;
      default = {};
      description = "Optional scribble/icon assets keyed by their exact filename.";
    };

    applyOnActivation = mkOption {
      type = types.bool;
      default = false;
      description = ''
        Apply declared artifacts during the user systemd startup transaction.
        When false, the service remains available for an explicit start.
      '';
    };
  };

  config = mkIf cfg.enable {
    home.packages = [ package pkgs.goxlr-utility ];

    assertions =
      (lib.concatLists (lib.mapAttrsToList (kind: values:
        lib.mapAttrsToList (name: _source: {
          assertion = name != "" && !(lib.hasInfix "/" name);
          message = "programs.goxlr-utility.${kind} keys must be simple filenames: ${name}";
        }) values
      ) {
        profileFiles = cfg.profileFiles;
        micProfileFiles = cfg.micProfileFiles;
        presetFiles = cfg.presetFiles;
        sampleFiles = cfg.sampleFiles;
        iconFiles = cfg.iconFiles;
      }))
      ++ [
        {
          assertion = files != [];
          message = "programs.goxlr-utility requires at least one declared artifact";
        }
      ];

    systemd.user.services.goxlr-utility-config = {
      Unit = {
        Description = "Reconcile declared GoXLR Utility configuration";
        After = [ daemonUnit ];
        Before = [ "goxlr-nexus.service" ];
      };
      Service = {
        Type = "oneshot";
        Environment = "PATH=${servicePath}";
        ExecStart = "${package}/bin/goxlr-config reconcile --manifest ${manifest}${applyFlag} --json";
      };
      Install.WantedBy = lib.optional cfg.applyOnActivation "default.target";
    };
  };
}
