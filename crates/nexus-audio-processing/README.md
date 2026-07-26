# nexus-audio-processing

`nexus-audio-processing` provides a portable, frame-oriented API for Sonora
noise suppression and WebRTC AEC3 echo cancellation. Linux consumers can opt
into the `pipewire-runtime` feature to run the same processor as a user-space
PipeWire virtual microphone with persistent state and Unix-socket control.

The crate is intentionally independent of GoXLR, OBS, and any particular
configuration system. Applications provide source and render node names and
decide how to expose the resulting virtual source to their own routing layer.

## Library use

Add the portable processor to an application:

```toml
nexus-audio-processing = "0.1"
```

Create a validated [`StreamFormat`], then pass matching ten-millisecond
[`AudioFrame`] values to [`DuplexProcessor::process_render`] and
[`DuplexProcessor::process_capture`] in that order. The capture result is the
processed microphone frame. A disabled [`ProcessingConfig`] is intentionally
bit-transparent, which lets callers keep a raw-source fallback while the
processor starts or recovers.

The `pipewire-runtime` feature adds [`ProcessingRuntime`]. It fixes the graph
contract at 48 kHz stereo, publishes a virtual source, persists independent
noise/echo overrides, and serves the versioned Unix-socket control protocol.
Control v2 uses a bounded, validated `rkyv` binary frame and reads archived
commands without allocating; v1 JSON is accepted for compatibility with an
older daemon during the 0.1 release line.
The PipeWire callbacks use preallocated frame pools and pending buffers, while
the worker processes into caller-owned frames and sleeps until input arrives.
Use `process_render_into` and `process_capture_into` when a caller owns a frame
pool; the allocating methods remain available for simpler integrations.
The runtime is Linux/PipeWire integration; the frame and control modules stay
portable for other consumers.

`StreamFormat::new` and the processing methods return typed errors for invalid
formats, frame lengths, Sonora failures, and control I/O. The library forbids
unsafe code. Callers should treat PipeWire node names as untrusted external
configuration and keep the control socket in a private runtime directory.

Release validation from the repository root is:

```sh
cargo fmt --all -- --check
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo package -p nexus-audio-processing
cargo publish -p nexus-audio-processing --dry-run
```

## Features

- `pipewire-runtime`: Linux PipeWire streams, runtime state, and control IPC.

The persisted processor state remains the small, human-inspectable
`processing-v1.json` file. JSON is also retained for external CLI, OBS, GoXLR,
and PipeWire interfaces whose schemas are outside this crate.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or the
[MIT license](LICENSE-MIT), at your option.
