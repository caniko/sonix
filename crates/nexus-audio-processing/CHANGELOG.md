# Changelog

## 0.1.0 - Unreleased

- Hardened runtime configuration, DSP frame validation, state persistence, and
  control error handling.
- Use validated `rkyv` v2 frames for private control IPC while retaining a v1
  JSON compatibility path.
- Initial publish-ready release with Sonora DSP, persistent controls, and the
  optional PipeWire runtime.
- The PipeWire runtime now uses preallocated frame pools, in-place processing,
  event-driven worker wakeups, and atomic hot-path status/config state to keep
  callback CPU and memory use bounded.
- Runtime configuration and persisted state now reject unknown or invalid
  values, resolve only absolute per-user XDG paths, and serialize updates under
  an advisory lock with explicit startup/shutdown error propagation.
