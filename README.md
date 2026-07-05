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
```

OBS websocket source mutation is intentionally gated. The v1 command reports the
planned sources and fails clearly unless websocket integration is enabled and
reachable.
