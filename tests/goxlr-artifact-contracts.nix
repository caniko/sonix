# CI contract for the GoXLR routing artifacts.
#
# These assertions encode the two incidents that broke atlas audio:
#
#  1. A WirePlumber rule that selected the UCM SplitPCM `HiFi` verb while the
#     card was activating deadlocked the session manager (futex wait, SIGKILL
#     needed), and a rule that disabled the split instead removed the verb
#     entirely so no `HiFi__*` node existed.  Both directions are asserted here.
#  2. The GoXLR daemon was started by the package's XDG autostart entry, so a
#     deploy left a stale daemon holding the IPC socket; `goxlr-client` hung and
#     the router idled `active` with no device status.
{
  lib,
  pkgs,
  cfg ? {
    goxlrCard = "alsa_card.usb-TC-Helicon_GoXLR-00";
    activationProfile = "Direct";
    goxlrProfile = "HiFi";
  },
}: let
  card = "alsa_card.usb-TC-Helicon_GoXLR-00";
  artifacts = import ../modules/goxlr-artifacts.nix {inherit lib pkgs;} {
    inherit cfg;
    package = "/nix/store/00000000000000000000000000000000-sonix/bin/goxlr-nexus";
    configFile = "/nix/store/00000000000000000000000000000000-config.pkl";
    servicePath = "/run/current-system/sw/bin";
  };

  ruleText = artifacts.wireplumberRule;
  rule = builtins.head (builtins.fromJSON (lib.removePrefix "monitor.alsa.rules = " ruleText));
  props = rule.actions.update-props;
  nexus = artifacts.nexus;
  daemon = artifacts.daemon;

  anyInfix = needle: haystack:
    builtins.any (value: lib.hasInfix needle (toString value)) haystack;

  check = assertion: message: {inherit assertion message;};

  assertions = [
    # --- WirePlumber rule ---------------------------------------------------
    (check (!(lib.hasInfix "api.alsa.split-enable" ruleText))
      "the GoXLR WirePlumber rule must not touch api.alsa.split-enable: disabling the split removes the HiFi verb and every HiFi__* node the routing needs")
    (check (props."device.profile" == "Direct")
      "activation must pin a single-PCM verb, got ${props."device.profile"}")
    (check (props."device.profile" != "HiFi")
      "pinning HiFi at device activation deadlocks WirePlumber 0.5.16+ on SplitPCM loopbacks")
    (check (props."api.acp.auto-profile" == false)
      "the activation profile must be deterministic")
    (check (rule.matches == [{"device.name" = card;}])
      "the rule must match the configured card only")

    # --- routing controller -------------------------------------------------
    (check (builtins.length nexus.Service.ExecStartPre == 2)
      "goxlr-nexus must perform the off -> routing profile transition itself")
    (check (anyInfix "off" nexus.Service.ExecStartPre && anyInfix "HiFi" nexus.Service.ExecStartPre)
      "the ExecStartPre transition must go off -> HiFi, got ${toString nexus.Service.ExecStartPre}")
    (check (anyInfix "processing fail-open" nexus.Service.ExecStopPost)
      "stopping the router must fail open to the raw microphone")
    (check (anyInfix "set-card-profile" nexus.Service.ExecStopPost && anyInfix "Direct" nexus.Service.ExecStopPost)
      "stopping the router must restore the activation verb so the next boot cannot deadlock")
    (check (builtins.elem "wireplumber.service" nexus.Unit.After)
      "the profile transition must run after the session manager finished activating the card")
    (check (builtins.elem "pipewire.service" nexus.Install.WantedBy)
      "a PipeWire crash restart must bring the router back")
    (check (builtins.elem "goxlr-daemon.service" nexus.Unit.Requires)
      "the router must not silently no-op without the daemon")

    # --- daemon ownership ---------------------------------------------------
    (check (daemon.Service.Restart == "always")
      "the GoXLR daemon must be supervised")
    (check (lib.hasInfix "goxlr-daemon" daemon.Service.ExecStart)
      "the daemon unit must run goxlr-daemon, got ${daemon.Service.ExecStart}")
    (check (builtins.elem "app-goxlr\\x2ddaemon@autostart.service" daemon.Unit.Conflicts)
      "the daemon unit must conflict with the desktop-session autostart daemon so the old process releases the IPC socket")
    (check (lib.hasInfix "Hidden=true" artifacts.autostartMask)
      "the package XDG autostart entry must be shadowed so only one process holds the IPC socket")
  ];

  failures = builtins.filter (entry: !entry.assertion) assertions;
  report = lib.concatMapStringsSep "\n" (entry: "  - ${entry.message}") failures;
in
  if failures != []
  then throw "GoXLR artifact contracts failed:\n${report}"
  else {
    inherit card;
    activationProfile = artifacts.activationProfile;
    routingProfile = artifacts.routingProfile;
  }
