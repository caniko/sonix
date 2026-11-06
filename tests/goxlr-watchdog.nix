# Executable tests for the packaged GoXLR daemon watchdog.
#
# The production wrapper is used unchanged; only its dependency packages are
# replaced with fixtures, so a missing runtimeInput (the original `grep:
# command not found` bug) or a lost timeout still fails this test.
{
  lib,
  pkgs,
}: let
  fakeNexus = pkgs.writeShellApplication {
    name = "goxlr-nexus";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      state="''${STATE:?STATE must be set}"
      printf '%s\n' "$*" >> "$state/probe.log"
      mode="$(cat "$state/probe-mode" 2>/dev/null || echo fail)"
      case "$mode" in
        healthy) exit 0 ;;
        # The real daemon-check fails when the configured serial is missing,
        # empty in the client status, or not a Full mixer.  The watchdog must
        # act only on that exit code.
        fail | empty-serial | wrong-serial) exit 1 ;;
        hang) exec sleep 600 ;;
        term-resistant)
          trap ''' TERM
          while :; do sleep 0.2; done
          ;;
        *) exit 1 ;;
      esac
    '';
  };

  fakeSystemctl = pkgs.writeShellApplication {
    name = "systemctl";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      state="''${STATE:?STATE must be set}"
      printf '%s\n' "$*" >> "$state/systemctl.log"
      count="$(cat "$state/restart.count" 2>/dev/null || echo 0)"
      printf '%s\n' "$((count + 1))" > "$state/restart.count"
      exit "$(cat "$state/systemctl-exit" 2>/dev/null || echo 0)"
    '';
  };

  fakeGrep = pkgs.writeShellApplication {
    name = "grep";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      state="''${STATE:?STATE must be set}"
      [ -r "$state/cards" ] || exit 1
      case "$(cat "$state/cards")" in
        *GoXLR*) exit 0 ;;
        *) exit 1 ;;
      esac
    '';
  };

  # Override only the dependency packages, exactly as production receives them
  # through `{ lib, pkgs }`.  The production wrapper (including the real
  # coreutils `timeout --kill-after`) is otherwise used unchanged.
  testPkgs =
    pkgs
    // {
      gnugrep = fakeGrep;
      systemd = fakeSystemctl;
    };

  mkArtifacts = {serial ? "S200805412CQK"}:
    import ../modules/goxlr-artifacts.nix {
      inherit lib;
      pkgs = testPkgs;
    } {
      cfg = {
        goxlrCard = "alsa_card.usb-TC-Helicon_GoXLR-00";
        activationProfile = "Direct";
        goxlrProfile = "HiFi";
        goxlrSerial = serial;
      };
      package = fakeNexus;
      configFile = pkgs.writeText "goxlr-nexus-test-config.pkl" ''
        goxlrSerial = "S200805412CQK";
      '';
      servicePath = "/nonexistent";
    };

  withSerial = mkArtifacts {};
  withoutSerial = mkArtifacts {serial = null;};
  withEmptySerial = mkArtifacts {serial = "";};
in
  pkgs.runCommand "sonix-goxlr-watchdog-test" {
    nativeBuildInputs = [pkgs.bash pkgs.coreutils pkgs.gnugrep];
    GOXLR_HEALTH = "${withSerial.daemonHealthScript}/bin/goxlr-daemon-health";
    GOXLR_HEALTH_NO_SERIAL = "${withoutSerial.daemonHealthScript}/bin/goxlr-daemon-health";
    GOXLR_HEALTH_EMPTY_SERIAL = "${withEmptySerial.daemonHealthScript}/bin/goxlr-daemon-health";
  } ''
    bash ${./goxlr-watchdog.sh}
    touch "$out"
  ''
