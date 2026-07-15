# goxlr-nexus

`goxlr-nexus` coordinates the atlas GoXLR, JDS Labs Element DAC, PipeWire, and
OBS setup.

The GoXLR remains the mixer and effects source. The JDS Labs Element DAC remains
the listening output. `goxlr-nexus` discovers the live PipeWire graph, checks the
GoXLR Utility status, sets defaults, and links the GoXLR stream mix to the JDS
sink.

## Commands

```sh
goxlr-nexus discover
goxlr-nexus discover --json
goxlr-nexus adopt
goxlr-nexus adopt --json
goxlr-nexus plan --json
goxlr-nexus doctor
goxlr-nexus status
goxlr-nexus apply --dry-run
goxlr-nexus apply
goxlr-nexus profile stream
goxlr-nexus profile desktop
goxlr-nexus obs sync --dry-run
goxlr-nexus obs sync
```

`discover` is the human- and tooling-facing observation command. It reads the
PipeWire graph, current defaults, GoXLR status, and the OBS input/device
catalog without creating, linking, or changing anything. `--json` emits the
versioned `goxlr-nexus.observation/v1` document; the corresponding schema is
checked in at `schemas/observation-v1.schema.json`. The document is an input
for a future resolver, not a configuration file to edit by hand.

`adopt` reads the same live graph and prints a reviewable Home Manager
fragment when product labels identify exactly one desk DAC, dock, GoXLR source,
and mixer. It never writes configuration; missing or ambiguous matches are
reported as diagnostics. `adopt --json` emits the versioned
`goxlr-nexus.adopt/v1` report described by `schemas/adopt-v1.schema.json`.

`plan --json` evaluates the current defaults, GoXLR/fallback path, PipeWire
links, and (when enabled) OBS source mutations. It is strictly read-only;
`apply` remains the only command that executes the proposed mutations. The
versioned `goxlr-nexus.plan/v2` document is described by
`schemas/plan-v2.schema.json`; `requiresApply` is false when the live graph is
already converged.

`follow` coalesces PipeWire events and periodically resynchronizes the graph.
It applies only observed deltas, verifies the graph through the next
observation, and accepts `--observe-only` for a non-mutating canary.
The Home Manager module can force a real ALSA profile transition before the
controller starts (`goxlrCard` + `goxlrProfile`); this repairs WirePlumber
restarts that leave only hidden raw nodes while still reporting the requested
profile.

`obs sync` uses obs-websocket v5 to create or update dedicated GoXLR audio
sources in the active OBS scene. It does not rewrite OBS global Desktop Audio or
Mic/Aux devices.

## Declarative GoXLR Utility artifacts

The `goxlr-config` companion binary and `homeModules.goxlr-utility` module keep
the Utility-owned files lossless by treating native settings, profiles,
presets, samples, and icons as content-addressed artifacts:

```nix
inputs.goxlr-nexus.homeModules.goxlr-utility

programs.goxlr-utility = {
  enable = true;
  applyOnActivation = true;
  settingsFile = ./goxlr/settings.json;
  profileFiles = { "HiFi.goxlr" = ./goxlr/profiles/HiFi.goxlr; };
  micProfileFiles = { "Default.goxlrMicProfile" = ./goxlr/mic-profiles/Default.goxlrMicProfile; };
  presetFiles = { "Broadcast.preset" = ./goxlr/presets/Broadcast.preset; };
};
```

`goxlr-config plan` is read-only; `apply` and `reconcile --apply` are the only
mutating operations. Every write is staged atomically and followed by a hash
verification. The manifest format is intentionally opaque so new Utility
fields remain reproducible before a typed schema is available. The machine
contracts are checked in under `schemas/config-*-v1.schema.json`.
