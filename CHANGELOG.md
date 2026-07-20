# Changelog

## 0.1.0 - Unreleased

- Hardened follow reconciliation, OBS password handling, and runtime error
  paths against blocking work, secret retention, and panic-prone failures.
- Added the reusable `nexus-audio-processing` crate with Sonora noise
  suppression, WebRTC AEC3 echo cancellation, persistent toggles, and an
  optional PipeWire virtual-source runtime.
- Added `goxlr-nexus processing` CLI controls and fail-open routing integration.
- Migrated runtime configuration from TOML to native Pkl evaluation through
  `pklr`; NixOS and Home Manager now generate `config.pkl` files.
- Reworked `follow` around a single current-thread Tokio runtime with async
  subscription reads, debouncing, periodic resync, and signal-aware child
  cleanup. Default-audio and sink-volume probes now use bounded pactl queries.
- Reduced the PipeWire processing hot path to pooled frames, in-place DSP,
  atomic status/config updates, and worker wakeups instead of polling and
  per-frame allocations.
- Updated the Rust dependency graph, added structured long-running diagnostics,
  strict configuration decoding, secure XDG-only runtime paths, advisory state
  locking, and transactional artifact rollback verification.
- Extracted processing CLI concerns into a dedicated module and added executable
  schema-contract and rollback regression tests.
