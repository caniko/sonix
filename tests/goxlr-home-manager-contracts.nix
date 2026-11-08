# Contract test for the actual exported Home Manager module.
#
# Home Manager is a test-only flake input; sonix itself deliberately does not
# depend on it.  The parent check passes the flake output as `homeManager`, e.g.
#
#   import ./tests/goxlr-home-manager-contracts.nix {
#     inherit lib pkgs self;
#     homeManager = inputs.home-manager;
#   }
#
# Besides the exported unit shape, this test exercises the real
# activationProfile/goxlrProfile assertion: the invalid configuration must be
# rejected and the valid configuration must satisfy every assertion.
{
  lib,
  pkgs,
  self,
  homeManager,
}: let
  mkConfiguration = goxlr:
    homeManager.lib.homeManagerConfiguration {
      inherit pkgs;
      modules = [
        self.homeModules.goxlr-nexus
        {
          home.username = "sonix-test";
          home.homeDirectory = "/home/sonix-test";
          home.stateVersion = "25.11";
          programs.sonix = {
            enable = true;
            externalInput = {
              enable = true;
              captureSource = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
              renderTarget = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
              captureChannels = 2;
            };
            inherit goxlr;
          };
        }
      ];
    };

  valid = mkConfiguration {
    enable = true;
    goxlrSerial = "S200805412CQK";
    goxlrCard = "alsa_card.usb-TC-Helicon_GoXLR-00";
    activationProfile = "Direct";
    goxlrProfile = "HiFi";
  };

  # Selecting the SplitPCM verb while the card activates is the deadlock this
  # module asserts against.
  invalid = mkConfiguration {
    enable = true;
    goxlrSerial = "S200805412CQK";
    goxlrCard = "alsa_card.usb-TC-Helicon_GoXLR-00";
    activationProfile = "HiFi";
    goxlrProfile = "HiFi";
  };

  services = valid.config.systemd.user.services;
  timers = valid.config.systemd.user.timers;
  health = services.goxlr-daemon-health;
  healthTimer = timers.goxlr-daemon-health;
  daemon = services.goxlr-daemon;
  nexus = services.goxlr-nexus;

  # Home Manager's generated ExecStart is a list; accept a scalar too so this
  # test does not break on a future representation change.
  firstExec = value:
    if builtins.isList value
    then builtins.head value
    else value;

  # Home Manager checks assertions before exposing config, so test rejection
  # through its public activationPackage rather than inspecting invalid.config.
  invalidActivationBuild = builtins.tryEval invalid.activationPackage.drvPath;

  validAssertionsHold = builtins.all (entry: entry.assertion) valid.config.assertions;

  check = assertion: message: {inherit assertion message;};

  assertions = [
    (check (health.Service.Type == "oneshot")
      "the Home Manager watchdog unit must be a oneshot")
    (check (lib.hasInfix "goxlr-daemon-health" (firstExec health.Service.ExecStart))
      "the Home Manager watchdog must run the packaged health script")
    (check ((health.Service.TimeoutStartSec or null) != null)
      "the Home Manager oneshot must have a bounded TimeoutStartSec")
    (check (health.Service.RuntimeDirectory == "goxlr-daemon-health")
      "the Home Manager oneshot must own its runtime directory for the cooldown stamp")
    (check (health.Service.RuntimeDirectoryPreserve == "yes")
      "the Home Manager runtime directory must survive oneshot stops, or the cooldown is forgotten")
    (check (healthTimer.Timer.OnUnitActiveSec == "30s")
      "the Home Manager timer must probe a wedged daemon every 30 s")
    (check (builtins.elem "timers.target" healthTimer.Install.WantedBy)
      "the Home Manager timer must be wanted by timers.target")

    (check (builtins.elem "app-goxlr\\x2ddaemon@autostart.service" daemon.Unit.Conflicts)
      "the Home Manager daemon must conflict with the desktop-session autostart daemon")
    (check (daemon.Service.Restart == "always")
      "the Home Manager daemon must stay supervised")
    (check (lib.hasInfix "Hidden=true" valid.config.xdg.configFile."autostart/goxlr-daemon.desktop".text)
      "the Home Manager module must shadow the package XDG autostart entry")

    (check (builtins.elem "goxlr-daemon.service" nexus.Install.WantedBy)
      "a watchdog daemon restart must pull the Home Manager router back up")
    (check (builtins.elem "pipewire.service" nexus.Install.WantedBy)
      "a PipeWire crash restart must pull the Home Manager router back up")

    (check validAssertionsHold
      "the valid Home Manager configuration must satisfy every assertion")
    (check (!invalidActivationBuild.success)
      "building the invalid Home Manager activation must be rejected")
  ];

  failures = builtins.filter (entry: !entry.assertion) assertions;
  report = lib.concatMapStringsSep "\n" (entry: "  - ${entry.message}") failures;
in
  if failures != []
  then throw "GoXLR Home Manager contracts failed:\n${report}"
  else true
