# goxlr-nexus

`goxlr-nexus` coordinates the atlas GoXLR, JDS Labs Element DAC, PipeWire, and
OBS setup.

The GoXLR remains the mixer and effects source. The JDS Labs Element DAC remains
the listening output. `goxlr-nexus` discovers the live PipeWire graph, checks the
GoXLR Utility status, sets defaults, and links the GoXLR stream mix to the JDS
sink.

## Commands

```sh
goxlr-nexus doctor
goxlr-nexus status
goxlr-nexus apply --dry-run
goxlr-nexus apply
goxlr-nexus profile stream
goxlr-nexus profile desktop
goxlr-nexus obs sync --dry-run
goxlr-nexus obs sync
```

`obs sync` uses obs-websocket v5 to create or update dedicated GoXLR audio
sources in the active OBS scene. It does not rewrite OBS global Desktop Audio or
Mic/Aux devices.
