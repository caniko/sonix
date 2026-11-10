# VM integration test for the GoXLR systemd recovery contract.
#
# It runs a real systemd user manager and the module's actual units, but every
# GoXLR-facing binary is a fixture: goxlr-daemon, goxlr-nexus daemon-check/
# follow/fail-open, and a grep wrapper that reports the fake card state instead
# of /proc/asound/cards.  No host service is touched; the whole test is a
# throwaway NixOS VM.
{
  lib,
  pkgs,
}: let
  stateDir = "/run/goxlr-test";

  fakeDaemon = pkgs.writeShellApplication {
    name = "goxlr-daemon";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      printf 'started\n' >> ${stateDir}/daemon.log
      trap 'printf "stopped\n" >> ${stateDir}/daemon.log; exit 0' TERM INT
      while :; do sleep 1; done
    '';
  };

  fakeNexus = pkgs.writeShellApplication {
    name = "goxlr-nexus";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      mode="$(cat ${stateDir}/mode 2>/dev/null || echo healthy)"
      case "$*" in
        *daemon-check*)
          case "$mode" in
            healthy) exit 0 ;;
            hang) exec sleep 600 ;;
            *) exit 1 ;;
          esac
          ;;
        *follow*)
          printf 'router-started\n' >> ${stateDir}/router.log
          trap 'printf "router-stopped\n" >> ${stateDir}/router.log; exit 0' TERM INT
          while :; do sleep 1; done
          ;;
        *fail-open*)
          printf 'fail-open\n' >> ${stateDir}/router.log
          exit 0
          ;;
        *) exit 2 ;;
      esac
    '';
  };

  fakeSelf = {
    packages.${pkgs.stdenv.hostPlatform.system} = {
      default = fakeNexus;
      sonix = fakeNexus;
    };
  };

  fakeGrep = pkgs.writeShellApplication {
    name = "grep";
    runtimeInputs = [pkgs.coreutils];
    text = ''
      last=""
      for arg in "$@"; do last="$arg"; done
      if [ "$last" = /proc/asound/cards ]; then
        printf 'probed\n' >> ${stateDir}/card-probes.log
        [ -r ${stateDir}/cards ] || exit 1
        case "$(cat ${stateDir}/cards)" in
          *GoXLR*) exit 0 ;;
          *) exit 1 ;;
        esac
      fi
      exec ${pkgs.gnugrep}/bin/grep "$@"
    '';
  };

  goxlrModule = {
    config,
    lib,
    pkgs,
    ...
  }: {
    imports = [
      (import ../modules/nixos/sonix.nix {
        inherit config lib pkgs;
        self = fakeSelf;
      })
      (import ../modules/nixos.nix {
        inherit config lib pkgs;
        self = fakeSelf;
      })
    ];
  };
in
  pkgs.testers.runNixOSTest {
    name = "sonix-goxlr-systemd-recovery";
    # Replace only the node's dependencies, not nixpkgs' stdenv fixed point.
    node.pkgs = lib.mkForce (pkgs
      // {
        gnugrep = fakeGrep;
        goxlr-utility = fakeDaemon;
      });

    nodes.machine = {lib, ...}: {
      imports = [goxlrModule];

      services.dbus.enable = true;

      users.users.sonix = {
        isNormalUser = true;
        uid = 1000;
        linger = true;
      };

      systemd.tmpfiles.rules = [
        "d ${stateDir} 0777 root root -"
      ];

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

      # Fast timer so the test exercises timer -> oneshot -> restart instead of
      # waiting the production two-minute OnBootSec.
      systemd.user.timers.goxlr-daemon-health.timerConfig = lib.mkForce {
        OnBootSec = "1s";
        OnUnitActiveSec = "2s";
        AccuracySec = "1ms";
        Unit = "goxlr-daemon-health.service";
      };
    };

    testScript = ''
      import time

      user = "sonix"
      user_env = "XDG_RUNTIME_DIR=/run/user/1000 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus"

      def user_systemctl(*args):
          return "su - %s -c '%s systemctl --user %s'" % (user, user_env, " ".join(args))

      def count(path, needle):
          return int(machine.succeed("grep -c '^%s$' %s || true" % (needle, path)))

      def wait_count(path, needle, wanted, timeout=60):
          machine.wait_until_succeeds(
              "test $(grep -c '^%s$' %s || true) -ge %d" % (needle, path, wanted),
              timeout=timeout,
          )

      machine.wait_for_unit("user@1000.service")

      # Keep the auto-enabled timer out of the manual recovery assertions;
      # only the final timer test starts it.
      machine.succeed(user_systemctl("stop", "goxlr-daemon-health.timer"))
      machine.succeed(user_systemctl("stop", "timers.target"))

      machine.succeed("echo ' 0 [GoXLR          ]: USB-Audio - GoXLR' > ${stateDir}/cards")
      machine.succeed("echo healthy > ${stateDir}/mode")

      machine.succeed(user_systemctl("start goxlr-daemon.service"))
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-daemon.service"))
      machine.succeed(user_systemctl("start goxlr-nexus.service"))
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-nexus.service"))
      wait_count("${stateDir}/router.log", "router-started", 1)

      # CFS throttling freezes the audio threads for longer than the processed
      # source buffers; the running unit must be unthrottled whatever set it.
      quota = machine.succeed(user_systemctl("show", "goxlr-nexus.service", "-p", "CPUQuotaPerSecUSec", "--value")).strip()
      assert quota == "infinity", "goxlr-nexus runs under a CPU quota: %s" % quota

      # A healthy daemon is not restarted by the watchdog.
      machine.succeed(user_systemctl("start goxlr-daemon-health.service"))
      healthy_starts = count("${stateDir}/daemon.log", "started")
      time.sleep(1)
      assert count("${stateDir}/daemon.log", "started") == healthy_starts, "healthy daemon was restarted"

      # A failed daemon-check restarts the daemon; WantedBy pulls the router
      # back even though Requires stopped it with the daemon.
      machine.succeed("echo fail > ${stateDir}/mode")
      machine.succeed(user_systemctl("start goxlr-daemon-health.service"))
      wait_count("${stateDir}/daemon.log", "started", healthy_starts + 1)
      wait_count("${stateDir}/router.log", "router-started", 2)
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-nexus.service"))

      # The cooldown suppresses an immediate second restart.
      after_recovery = count("${stateDir}/daemon.log", "started")
      machine.succeed(user_systemctl("start goxlr-daemon-health.service"))
      time.sleep(1)
      assert count("${stateDir}/daemon.log", "started") == after_recovery, "cooldown did not suppress the retry"

      # Stopping the daemon stops the router through Requires; starting it
      # pulls the router back through WantedBy.
      machine.succeed(user_systemctl("stop", "goxlr-daemon.service"))
      machine.wait_until_fails(user_systemctl("is-active", "--quiet", "goxlr-nexus.service"))
      machine.succeed(user_systemctl("start", "goxlr-daemon.service"))
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-nexus.service"))
      wait_count("${stateDir}/router.log", "router-started", 3)

      # A restart is the watchdog's recovery path; the router must return.
      machine.succeed(user_systemctl("restart", "goxlr-daemon.service"))
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-nexus.service"))
      wait_count("${stateDir}/router.log", "router-started", 4)

      # Timer-driven recovery: no card must not restart the daemon, then a
      # replug must let the same timer recover a failing client.  Clear the
      # cooldown stamp first; the runtime directory is per-user and its stamp
      # is one level below /run/user/1000/goxlr-daemon-health.
      machine.succeed("echo ' 0 [PCH              ]: HDA-Intel - HDA Intel PCH' > ${stateDir}/cards")
      machine.succeed("rm -f /run/user/1000/goxlr-daemon-health/goxlr-daemon-health.stamp")
      before_timer = count("${stateDir}/daemon.log", "started")
      before_probes = count("${stateDir}/card-probes.log", "probed")
      machine.succeed(user_systemctl("start", "goxlr-daemon-health.timer"))
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-daemon-health.timer"))
      wait_count("${stateDir}/card-probes.log", "probed", before_probes + 1)
      assert count("${stateDir}/daemon.log", "started") == before_timer, "timer restarted the daemon without a GoXLR card"

      machine.succeed("echo ' 0 [GoXLR          ]: USB-Audio - GoXLR' > ${stateDir}/cards")
      wait_count("${stateDir}/daemon.log", "started", before_timer + 1, timeout=60)
      machine.wait_until_succeeds(user_systemctl("is-active", "goxlr-nexus.service"))
    '';
  }
