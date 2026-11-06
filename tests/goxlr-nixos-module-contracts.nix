# Contract test for the actual exported NixOS module.
#
# The previous revision assigned Home Manager unit shapes
# (`Unit`/`Service`/`Timer`/`Install`) to `systemd.user.*` on NixOS, which the
# NixOS module system rejects.  Evaluating the real exported module through
# nixpkgs catches that class of regression; no build or VM is needed.
{
  lib,
  pkgs,
  self,
}: let
  evalConfig = import (pkgs.path + "/nixos/lib/eval-config.nix") {
    system = pkgs.stdenv.hostPlatform.system;
    modules = [
      {nixpkgs.pkgs = pkgs;}
      self.nixosModules.default
      {
        programs.sonix = {
          enable = true;
          externalInput = {
            enable = true;
            captureSource = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
            renderTarget = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
            captureChannels = 2;
          };
          goxlr = {
            enable = true;
            goxlrSerial = "S200805412CQK";
            monitorSource = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
          };
        };
      }
    ];
  };

  services = evalConfig.config.systemd.user.services;
  timers = evalConfig.config.systemd.user.timers;

  health = services.goxlr-daemon-health;
  healthTimer = timers.goxlr-daemon-health;
  daemon = services.goxlr-daemon;
  nexus = services.goxlr-nexus;

  check = assertion: message: {inherit assertion message;};

  assertions = [
    (check (health.serviceConfig.Type == "oneshot")
      "the NixOS watchdog unit must be a oneshot")
    (check (lib.hasInfix "goxlr-daemon-health" health.serviceConfig.ExecStart)
      "the NixOS watchdog must run the packaged health script")
    (check ((health.serviceConfig.TimeoutStartSec or null) != null)
      "the NixOS oneshot must have a bounded TimeoutStartSec")
    (check (health.serviceConfig.RuntimeDirectory == "goxlr-daemon-health")
      "the NixOS oneshot must own its runtime directory for the cooldown stamp")
    (check (health.serviceConfig.RuntimeDirectoryPreserve == "yes")
      "the NixOS runtime directory must survive oneshot stops, or the cooldown is forgotten")
    (check (!(health ? Unit) && !(health ? Service) && !(health ? Timer) && !(health ? Install))
      "NixOS units must use description/serviceConfig, not the Home Manager Unit/Service/Timer/Install shape")
    (check (healthTimer.timerConfig.OnUnitActiveSec == "30s")
      "the NixOS timer must probe a wedged daemon every 30 s")
    (check (healthTimer.timerConfig.Unit == "goxlr-daemon-health.service")
      "the NixOS timer must drive the watchdog oneshot")
    (check (builtins.elem "timers.target" healthTimer.wantedBy)
      "the NixOS timer must be wanted by timers.target")

    (check (builtins.elem "app-goxlr\\x2ddaemon@autostart.service" daemon.unitConfig.Conflicts)
      "the NixOS daemon must conflict with the desktop-session autostart daemon")
    (check (daemon.serviceConfig.Restart == "always")
      "the NixOS daemon must stay supervised")

    (check (builtins.elem "goxlr-daemon.service" nexus.wantedBy)
      "a watchdog daemon restart must pull the NixOS router back up")
    (check (builtins.elem "pipewire.service" nexus.wantedBy)
      "a PipeWire crash restart must pull the NixOS router back up")
    (check (builtins.elem "goxlr-daemon.service" nexus.requires)
      "the NixOS router must not silently no-op without the daemon")
    (check (lib.hasInfix "follow" nexus.serviceConfig.ExecStart)
      "the NixOS router must run goxlr-nexus follow")
  ];

  failures = builtins.filter (entry: !entry.assertion) assertions;
  report = lib.concatMapStringsSep "\n" (entry: "  - ${entry.message}") failures;
in
  if failures != []
  then throw "GoXLR NixOS module contracts failed:\n${report}"
  else {
    inherit health healthTimer daemon nexus;
  }
