# CI contract for the GoXLR routing artifacts.
#
# These assertions encode the incidents that broke atlas audio:
#
#  1. A WirePlumber rule that selected the UCM SplitPCM `HiFi` verb while the
#     card was activating deadlocked the session manager (futex wait, SIGKILL
#     needed), and a rule that disabled the split instead removed the verb
#     entirely so no `HiFi__*` node existed.  Both directions are asserted here.
#  2. The GoXLR daemon was started by the package's XDG autostart entry, so a
#     deploy left a stale daemon holding the IPC socket; `goxlr-client` hung and
#     the router idled `active` with no device status.
#  3. The wedge watchdog ran `goxlr-client --status-json` with a clean PATH, so
#     `grep` was missing, the hardware guard silently succeeded, and no restart
#     ever happened.  The probe is now the validated daemon-check with every
#     runtime dependency declared and a hard timeout.
{
  lib,
  pkgs,
  cfg ? {
    goxlrCard = "alsa_card.usb-TC-Helicon_GoXLR-00";
    activationProfile = "Direct";
    goxlrProfile = "HiFi";
    goxlrSerial = "S200805412CQK";
  },
}: let
  card = "alsa_card.usb-TC-Helicon_GoXLR-00";
  build = config:
    import ../modules/goxlr-artifacts.nix {inherit lib pkgs;} {
      cfg = config;
      package = "/nix/store/00000000000000000000000000000000-sonix/bin/goxlr-nexus";
      configFile = "/nix/store/00000000000000000000000000000000-config.pkl";
      servicePath = "/run/current-system/sw/bin";
    };
  artifacts = build cfg;
  artifactsWithoutSerial = build (cfg // {goxlrSerial = null;});
  artifactsWithEmptySerial = build (cfg // {goxlrSerial = "";});

  ruleText = artifacts.wireplumberRule;
  rule = builtins.head (builtins.fromJSON (lib.removePrefix "monitor.alsa.rules = " ruleText));
  props = rule.actions.update-props;
  inherit (artifacts) nexus;
  inherit (artifacts) daemon;
  health = artifacts.daemonHealth;

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
    (check (!(nexus.Service ? CPUQuota))
      "goxlr-nexus must not carry CPUQuota: a CPU quota freezes the PipeWire and DSP threads for longer than the processed source buffers, which is heard as stutter")
    (check (builtins.elem "wireplumber.service" nexus.Unit.After)
      "the profile transition must run after the session manager finished activating the card")
    (check (builtins.elem "pipewire.service" nexus.Install.WantedBy)
      "a PipeWire crash restart must bring the router back")
    (check (builtins.elem "goxlr-daemon.service" nexus.Unit.Requires)
      "the router must not silently no-op without the daemon")
    (check (builtins.elem "goxlr-daemon.service" nexus.Install.WantedBy)
      "a daemon restart must pull the router back up, or a watchdog restart leaves audio dead")

    # --- daemon ownership ---------------------------------------------------
    (check (daemon.Service.Restart == "always")
      "the GoXLR daemon must be supervised")
    (check (lib.hasInfix "goxlr-daemon" daemon.Service.ExecStart)
      "the daemon unit must run goxlr-daemon, got ${daemon.Service.ExecStart}")
    (check (builtins.elem "app-goxlr\\x2ddaemon@autostart.service" daemon.Unit.Conflicts)
      "the daemon unit must conflict with the desktop-session autostart daemon so the old process releases the IPC socket")
    (check (lib.hasInfix "Hidden=true" artifacts.autostartMask)
      "the package XDG autostart entry must be shadowed so only one process holds the IPC socket")

    # --- watchdog probe -----------------------------------------------------
    (check (lib.hasInfix "daemon-check" artifacts.daemonHealthScriptText)
      "the watchdog must use the validated daemon-check, not an exit-code-only client probe")
    (check (lib.hasInfix "--config" artifacts.daemonHealthScriptText)
      "daemon-check must read the same config file as the router, including goxlrSerial")
    (check (!(lib.hasInfix "goxlr-client" artifacts.daemonHealthScriptText))
      "the watchdog must not probe goxlr-client itself: serial/Full validation lives in daemon-check")
    (check (lib.hasInfix "--kill-after" artifacts.daemonHealthScriptText)
      "a TERM-resistant or hung daemon-check must be killed, not left to block the timer")
    (check (lib.hasInfix "restart goxlr-daemon.service" artifacts.daemonHealthScriptText)
      "the daemon watchdog must restart the daemon, not just log")
    (check (lib.hasInfix "-lt 300" artifacts.daemonHealthScriptText)
      "a persistent failure must keep the 300 s cooldown instead of cycling every tick")
    (check (lib.hasInfix "*[!0-9]*" artifacts.daemonHealthScriptText)
      "an empty or corrupt cooldown stamp must be treated as expired, not as an arithmetic error")
    (check (lib.hasInfix "10#" artifacts.daemonHealthScriptText)
      "a leading-zero cooldown stamp must be forced to base 10, not parsed as octal")
    (check (lib.hasInfix "-gt 10" artifacts.daemonHealthScriptText)
      "an oversized cooldown stamp must expire before arithmetic overflows")
    (check (lib.hasInfix "-gt \"$now\"" artifacts.daemonHealthScriptText)
      "a wall-clock jump backwards must not suppress recovery against a future stamp")
    (check (lib.hasInfix "RUNTIME_DIRECTORY" artifacts.daemonHealthScriptText)
      "the stamp must live in the systemd runtime directory")
    (check (!(lib.hasInfix "/tmp" artifacts.daemonHealthScriptText))
      "the stamp must never fall back to a guessable /tmp path")
    (check (!(lib.hasInfix "daemon-check" artifactsWithoutSerial.daemonHealthScriptText))
      "without a configured serial the watchdog must stay a no-op instead of restarting every tick")
    (check (lib.hasInfix "daemon-check" artifactsWithEmptySerial.daemonHealthScriptText)
      "an empty serial must be delegated to daemon-check so the misconfiguration is visible")
    (check (builtins.elem pkgs.gnugrep artifacts.daemonHealthRuntimeInputs)
      "grep must be a declared runtime input: a clean PATH otherwise skips the hardware guard")
    (check (builtins.elem pkgs.coreutils artifacts.daemonHealthRuntimeInputs)
      "timeout/date must be declared runtime inputs")
    (check (builtins.elem pkgs.systemd artifacts.daemonHealthRuntimeInputs)
      "systemctl must be a declared runtime input")
    (check (builtins.elem pkgs.goxlr-utility artifacts.daemonHealthRuntimeInputs)
      "daemon-check resolves goxlr-client from PATH, so goxlr-utility must be a declared runtime input")

    # --- watchdog unit ------------------------------------------------------
    (check (health.serviceConfig.Type == "oneshot")
      "the watchdog is a oneshot timer service")
    (check ((health.serviceConfig.TimeoutStartSec or null) != null)
      "the oneshot must have a bounded TimeoutStartSec")
    (check ((health.serviceConfig.TimeoutStartSec or 0) <= 60)
      "TimeoutStartSec must stay bounded, got ${toString (health.serviceConfig.TimeoutStartSec or 0)}")
    (check (health.serviceConfig.RuntimeDirectory == "goxlr-daemon-health")
      "the oneshot must own the runtime directory that holds the cooldown stamp")
    (check (health.serviceConfig.RuntimeDirectoryPreserve == "yes")
      "the runtime directory must survive oneshot stops, or the cooldown is forgotten every tick")
    (check (health.timer.timerConfig.OnUnitActiveSec == "30s")
      "a wedged-but-alive daemon must be probed on a timer: Restart= only covers process exit")
    (check (health.timer.timerConfig.Unit == "goxlr-daemon-health.service")
      "the timer must drive the watchdog oneshot")
    (check (builtins.elem "timers.target" health.timer.wantedBy)
      "the watchdog timer must be wanted by timers.target")
  ];

  failures = builtins.filter (entry: !entry.assertion) assertions;
  report = lib.concatMapStringsSep "\n" (entry: "  - ${entry.message}") failures;
in
  if failures != []
  then throw "GoXLR artifact contracts failed:\n${report}"
  else {
    inherit card;
    inherit (artifacts) activationProfile;
    inherit (artifacts) routingProfile;
  }
