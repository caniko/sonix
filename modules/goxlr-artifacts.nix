# GoXLR routing artifacts, kept pure so CI can assert on them without a
# Home Manager evaluation (sonix deliberately has no home-manager input).
#
# The values here encode two hard-won invariants:
#
#  * WirePlumber must never select a UCM SplitPCM verb while it activates the
#    card.  0.5.16+ builds the split loopbacks inside device activation while
#    the client-context lock is held, which deadlocks the session manager and
#    hangs every PipeWire client.  The split itself must stay enabled, because
#    it is what exposes the `HiFi` verb and every `HiFi__*` node name the
#    routing addresses.
#  * The GoXLR daemon is a long-lived USB session, not a desktop extra.  It is
#    owned here so activation restarts it, and the package's XDG autostart
#    entry is shadowed so only one process can hold the IPC socket.
{
  lib,
  pkgs,
}: {
  cfg,
  package,
  configFile,
  servicePath,
}: let
  # A stale profile pin here is the deadlock trigger, so the activation verb is
  # separate from the SplitPCM verb the router selects afterwards.
  activationProfile = cfg.activationProfile;
  routingProfile = cfg.goxlrProfile;
  card = cfg.goxlrCard;
  pactlProfile = profile: "${pkgs.coreutils}/bin/timeout 5 ${pkgs.pulseaudio}/bin/pactl set-card-profile ${lib.escapeShellArg card} ${lib.escapeShellArg profile}";
  # systemd ignores a failing ExecStartPre/ExecStopPost command when it is
  # prefixed with `-`.
  optionalCommand = command: "-${command}";
  failOpenCommand = optionalCommand "${package}/bin/goxlr-nexus --config ${configFile} processing fail-open";
in {
  inherit activationProfile routingProfile card;

  daemonUnit = "goxlr-daemon.service";

  # `api.acp.auto-profile = false` keeps the selection deterministic;
  # `api.alsa.split-enable` must never appear here (it removes the HiFi verb).
  wireplumberRule = "monitor.alsa.rules = ${builtins.toJSON [
    {
      matches = [{"device.name" = card;}];
      actions.update-props = {
        "api.acp.auto-profile" = false;
        "device.profile" = activationProfile;
      };
    }
  ]}";

  # `Hidden=true` shadows the package entry of the same name in
  # ~/.config/autostart over the system-wide one, so the desktop session no
  # longer races our unit for the IPC socket.
  autostartMask = ''
    [Desktop Entry]
    Type=Application
    Name=GoXLR Utility
    Hidden=true
  '';

  daemon = {
    Unit = {
      Description = "GoXLR Utility daemon";
      StartLimitIntervalSec = 60;
      StartLimitBurst = 10;
    };
    Service = {
      ExecStart = "${pkgs.goxlr-utility}/bin/goxlr-daemon";
      Restart = "always";
      RestartSec = 2;
      TimeoutStopSec = 10;
    };
    Install.WantedBy = ["default.target"];
  };

  nexus = {
    Unit = {
      Description = "GoXLR Nexus routing with embedded Sonix processing";
      After = [
        "pipewire.service"
        "pipewire-pulse.service"
        "wireplumber.service"
        "goxlr-daemon.service"
      ];
      Wants = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
      # A hard dependency: without device status the router cannot build a
      # plan, and a silent no-op is worse than a visible failure.
      Requires = ["goxlr-daemon.service"];
      PartOf = ["pipewire.service" "pipewire-pulse.service" "wireplumber.service"];
      StartLimitIntervalSec = 300;
      StartLimitBurst = 20;
    };
    Service = {
      Type = "simple";
      Environment = "PATH=${servicePath}";
      # The real transition happens here, after the session manager finished
      # activating the card, which is the window where SplitPCM loopbacks load
      # without the activation lock held.
      ExecStartPre = lib.optionals (card != null) (map optionalCommand [
        (pactlProfile "off")
        (pactlProfile routingProfile)
      ]);
      ExecStart = "${package}/bin/goxlr-nexus --config ${configFile} follow";
      # Fail-open routing, then restore the activation verb so the profile
      # persisted for the next boot cannot deadlock the session manager.
      ExecStopPost =
        [failOpenCommand]
        ++ lib.optionals (card != null) [(optionalCommand (pactlProfile activationProfile))];
      Restart = "on-failure";
      RestartSec = 5;
      CPUQuota = "35%";
      MemoryHigh = "128M";
      MemoryMax = "256M";
      TasksMax = 64;
      TimeoutStopSec = 10;
      LogRateLimitIntervalSec = 30;
      LogRateLimitBurst = 100;
    };
    # PartOf only stops this unit when PipeWire goes away; a crash that
    # PipeWire recovers from with its own restart job never pulls the
    # controller back up.  Being wanted by PipeWire restarts it every time.
    Install.WantedBy = ["default.target" "pipewire.service"];
  };
}
