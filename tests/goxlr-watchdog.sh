#!/usr/bin/env bash
# Executable contract tests for the packaged goxlr-daemon-health watchdog.
# The packaged wrapper runs under `env -i` so only its declared runtimeInputs
# are available; fixtures simulate the cards file, daemon-check and systemctl.
set -euo pipefail

: "${GOXLR_HEALTH:?}"
: "${GOXLR_HEALTH_NO_SERIAL:?}"
: "${GOXLR_HEALTH_EMPTY_SERIAL:?}"

STATE_ROOT="$(mktemp -d "${TMPDIR:-$PWD}/sonix-watchdog.XXXXXXXX")"
trap 'rm -rf -- "$STATE_ROOT"' EXIT

failures=0
fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

setup() {
  local name="$1" mode="$2"
  local dir="$STATE_ROOT/$name"
  mkdir -p "$dir"
  printf '%s\n' "$mode" > "$dir/probe-mode"
  printf ' 0 [GoXLR          ]: USB-Audio - GoXLR\n' > "$dir/cards"
  printf '0\n' > "$dir/systemctl-exit"
  : > "$dir/probe.log"
  : > "$dir/systemctl.log"
  printf '0\n' > "$dir/restart.count"
  printf '%s\n' "$dir"
}

run_health() {
  local script="$1" state="$2"
  env -i \
    PATH=/nonexistent \
    HOME="$state" \
    STATE="$state" \
    RUNTIME_DIRECTORY="$state" \
    "$script"
}

run_health_without_runtime_dir() {
  local script="$1" state="$2"
  env -i \
    PATH=/nonexistent \
    HOME="$state" \
    STATE="$state" \
    "$script"
}

restart_count() {
  cat "$1/restart.count"
}

assert_restarted_once() {
  local dir="$1"
  if [ "$(restart_count "$dir")" != "1" ]; then
    fail "expected exactly one restart in $dir, got $(restart_count "$dir")"
  fi
  if ! grep -q -- '--user restart goxlr-daemon.service' "$dir/systemctl.log"; then
    fail "systemctl did not receive '--user restart goxlr-daemon.service' in $dir"
  fi
}

assert_no_restart() {
  local dir="$1"
  if [ "$(restart_count "$dir")" != "0" ]; then
    fail "expected no restart in $dir, got $(restart_count "$dir")"
  fi
}

# Healthy client: the hardware guard and probe both run under a clean PATH.
# If grep were not a declared runtime input the probe would be skipped.
dir="$(setup healthy healthy)"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "healthy probe must exit 0"
fi
if [ ! -s "$dir/probe.log" ]; then
  fail "healthy probe did not run; the hardware guard likely lost grep from PATH"
fi
if ! grep -q -- 'daemon-check' "$dir/probe.log"; then
  fail "watchdog must invoke 'goxlr-nexus --config <config> daemon-check'"
fi
if ! grep -q -- '--config' "$dir/probe.log"; then
  fail "daemon-check must receive the router config file"
fi
assert_no_restart "$dir"

# No GoXLR card: never probe and never restart.
dir="$(setup absent healthy)"
printf ' 0 [PCH              ]: HDA-Intel - HDA Intel PCH\n' > "$dir/cards"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "absent hardware must exit 0"
fi
if [ -s "$dir/probe.log" ]; then
  fail "absent hardware must not run daemon-check"
fi
assert_no_restart "$dir"

# No configured serial: daemon-check cannot validate anything, so stay a
# no-op instead of restarting the daemon on every timer tick.
dir="$(setup no-serial fail)"
if ! run_health "$GOXLR_HEALTH_NO_SERIAL" "$dir"; then
  fail "unconfigured serial must exit 0"
fi
if [ -s "$dir/probe.log" ]; then
  fail "unconfigured serial must not run daemon-check"
fi
assert_no_restart "$dir"

# An empty serial is a misconfiguration; the watchdog must not hide it, so it
# probes and lets the Rust validator fail the check.
dir="$(setup empty-serial-config empty-serial)"
if ! run_health "$GOXLR_HEALTH_EMPTY_SERIAL" "$dir"; then
  fail "an empty configured serial must be probed and recovered via daemon-check"
fi
if [ ! -s "$dir/probe.log" ]; then
  fail "an empty configured serial must still run daemon-check"
fi
assert_restarted_once "$dir"

# Failed client: one restart.  Empty/wrong serial is delegated to the Rust
# validator; the watchdog only sees its non-zero exit status.
for mode in fail empty-serial wrong-serial; do
  dir="$(setup "client-$mode" "$mode")"
  if ! run_health "$GOXLR_HEALTH" "$dir"; then
    fail "failed client ($mode) must be recovered and exit 0"
  fi
  assert_restarted_once "$dir"
done

# The same failing client immediately afterwards is suppressed by the 300 s
# cooldown stamp.
dir="$(setup cooldown fail)"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "first failed probe must be recovered"
fi
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "second failed probe inside the cooldown must exit 0"
fi
assert_restarted_once "$dir"

# A stamp older than the cooldown must allow the next restart immediately; no
# wall-clock sleep is needed.
dir="$(setup expired-cooldown fail)"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "first failed probe must be recovered"
fi
printf '%s\n' "$(($(date +%s) - 301))" > "$dir/goxlr-daemon-health.stamp"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "an expired cooldown stamp must allow recovery"
fi
if [ "$(restart_count "$dir")" != "2" ]; then
  fail "an expired cooldown stamp must allow a second restart, got $(restart_count "$dir")"
fi

# A corrupt stamp must be treated as expired, not crash the oneshot.
dir="$(setup corrupt-stamp fail)"
printf 'not-a-timestamp\n' > "$dir/goxlr-daemon-health.stamp"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "a corrupt cooldown stamp must not fail the watchdog"
fi
assert_restarted_once "$dir"

# A digit-only stamp with a leading zero must not be parsed as octal.
dir="$(setup leading-zero-stamp fail)"
printf '08\n' > "$dir/goxlr-daemon-health.stamp"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "a leading-zero cooldown stamp must not fail the watchdog"
fi
assert_restarted_once "$dir"

# An oversized digit-only stamp must expire, not overflow the arithmetic.
dir="$(setup oversized-stamp fail)"
printf '99999999999\n' > "$dir/goxlr-daemon-health.stamp"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "an oversized cooldown stamp must not fail the watchdog"
fi
assert_restarted_once "$dir"

# An empty stamp is the same case.
dir="$(setup empty-stamp fail)"
: > "$dir/goxlr-daemon-health.stamp"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "an empty cooldown stamp must not fail the watchdog"
fi
assert_restarted_once "$dir"

# A wall-clock jump backwards leaves a future stamp; recovery must not be
# suppressed until the clock catches up.
dir="$(setup future-stamp fail)"
printf '%s\n' "$(($(date +%s) + 100000))" > "$dir/goxlr-daemon-health.stamp"
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "a future cooldown stamp must not suppress recovery"
fi
assert_restarted_once "$dir"

# A hung client is bounded by `timeout`, and a client that ignores SIGTERM is
# killed by `--kill-after`, so the oneshot cannot wedge the timer.  These use
# the production 20 s + 5 s bound on purpose.
for mode in hang term-resistant; do
  dir="$(setup "bounded-$mode" "$mode")"
  started="$(date +%s)"
  if ! run_health "$GOXLR_HEALTH" "$dir"; then
    fail "$mode client must be killed and recovered"
  fi
  elapsed=$(($(date +%s) - started))
  if [ "$elapsed" -gt 45 ]; then
    fail "$mode client was not bounded: took ${elapsed}s"
  fi
  assert_restarted_once "$dir"
done

# A failed restart must be visible (non-zero exit for the oneshot), while the
# stamp still prevents an immediate retry storm.
dir="$(setup restart-failure fail)"
printf '1\n' > "$dir/systemctl-exit"
if run_health "$GOXLR_HEALTH" "$dir"; then
  fail "a failed restart must fail the oneshot"
fi
if [ "$(restart_count "$dir")" != "1" ]; then
  fail "a failed restart must still attempt exactly once"
fi
if ! run_health "$GOXLR_HEALTH" "$dir"; then
  fail "the cooldown must suppress an immediate retry after a failed restart"
fi
if [ "$(restart_count "$dir")" != "1" ]; then
  fail "the cooldown must not retry a failed restart inside 300 s"
fi

# No runtime directory: refuse to guess a path (never fall back to /tmp).
dir="$(setup no-runtime-dir fail)"
if run_health_without_runtime_dir "$GOXLR_HEALTH" "$dir"; then
  fail "an unset RUNTIME_DIRECTORY must fail instead of using /tmp"
fi
assert_no_restart "$dir"

if [ "$failures" -ne 0 ]; then
  printf '%d watchdog contract failure(s)\n' "$failures" >&2
  exit 1
fi

printf 'goxlr watchdog contracts passed\n'
