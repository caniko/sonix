# Sonix vision

Sonix provides reusable external-input audio processing for PipeWire and, in
the same repository, `goxlr-nexus`, which routes a GoXLR mixer, a desk DAC, and
OBS on top of it. The `sonix` processor enables noise suppression and echo
cancellation when its configured capture input is connected, and idles when it
is not. The repository ships the `nexus-audio-processing` crate, the `sonix`,
`goxlr-config`, and `goxlr-nexus` binaries, and NixOS and Home Manager modules.

## Problem

A microphone on a desktop is not one graph. The GoXLR is the mixer and effects
source, a separate DAC is the listening output, OBS consumes its own channels,
and noise/echo processing has to sit between a capture input and that graph.
Wiring this by hand leaves it broken after every device change, and a router
that mutates blindly can leave a machine with no working audio path.

## Principles

- Processing fails open. The processor idles when its capture input disappears
  rather than breaking the audio path.
- Capture and render layouts are independent, so a mono laptop microphone can
  use a stereo render reference while external microphones keep native layouts.
- Observe before mutating. `discover`, `adopt`, and `plan` are read-only;
  `apply` is the only command that executes proposed mutations, and
  `apply --dry-run` is explicit.
- Observations are versioned contracts. `--json` emits `goxlr-nexus.observation/v1`,
  `adopt/v1`, and `plan/v2` documents whose schemas are checked in under
  `schemas/`.
- Convergence is checked, not assumed. `follow` applies only observed deltas,
  verifies the graph through the next observation, and supports
  `--observe-only` as a non-mutating canary.
- Known platform hazards are handled in code: the GoXLR needs its `HiFi` verb,
  while WirePlumber 0.5.16+ deadlocks building split loopbacks inside device
  activation, so the rule pins a single-PCM activation verb and the controller
  performs the real profile transition once the session manager is up.
- Configuration is Pkl. The NixOS and Home Manager modules generate the same Pkl
  format that the CLI reads, and an existing default-path `config.toml` is
  rejected with a migration message instead of being silently ignored.

## Scope

- `nexus-audio-processing`: reusable PipeWire processing with noise suppression
  and echo cancellation.
- `sonix`: the processing processor as a standalone package and module.
- `goxlr-nexus`: discovery, adoption, planning, follow, doctor, status, profile,
  OBS sync, and processing control for the GoXLR graph.
- `goxlr-config`: configuration rendering for the modules.
- NixOS and Home Manager modules, with the daemon supervised as a long-lived USB
  session.

## Non-goals

- It is not a mixer or a DAW. The GoXLR remains the mixer and effects source,
  and the JDS Labs Element DAC remains the listening output.
- It does not replace PipeWire or WirePlumber; it reads the live graph and asks
  them to converge.

## Direction

Recent history is GoXLR correctness: profile and daemon ownership, keeping the
SplitPCM split without deadlocking WirePlumber, re-routing on processor health
changes, capping the graph quantum, and local FX telemetry for distortion
triage. No roadmap file is checked in.

## Place in the fleet

Canix consumes Sonix as a flake input. The atlas host imports
`inputs.sonix.homeModules.goxlr-nexus` and configures `programs.sonix`; nomad,
murph, and starlord import `inputs.sonix.homeModules.sonix` and configure their
own processed microphone source. Canix comments record that build inputs stay
Sonix's own so the canix cache can substitute Sonix's trunk prebuild.
