# Changelog

## 0.1.0 - Unreleased

- Use validated `rkyv` v2 frames for private control IPC while retaining a v1
  JSON compatibility path.
- Initial publish-ready release with Sonora DSP, persistent controls, and the
  optional PipeWire runtime.
- The PipeWire runtime now uses preallocated frame pools, in-place processing,
  event-driven worker wakeups, and atomic hot-path status/config state to keep
  callback CPU and memory use bounded.
