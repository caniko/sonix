#![doc = include_str!("../README.md")]
#![deny(unsafe_code)]

//! Portable audio processing and optional PipeWire runtime support.

mod config;
mod control;
mod dsp;
mod state;

#[cfg(feature = "pipewire-runtime")]
mod runtime;

pub use config::{
    EchoDelay, FormatError, NoiseSuppressionLevel, ProcessingConfig, RuntimeConfig, SCHEMA_VERSION,
    StreamFormat, VirtualSourceConfig,
};
pub use control::{
    CONTROL_PROTOCOL, ControlClient, ControlCommand, ControlError, ControlRequest, ControlResponse,
    LEGACY_CONTROL_PROTOCOL, RuntimeStatus, offline_status,
};
#[cfg(feature = "sonora")]
pub use dsp::DuplexProcessor;
pub use dsp::{AudioFrame, DspError};
pub use state::{
    StateError, StateSnapshot, StateStore, Toggle, default_control_socket, default_state_path,
};

#[cfg(feature = "pipewire-runtime")]
pub use runtime::{ProcessingRuntime, RuntimeError};
