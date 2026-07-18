use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The schema version used by persisted processor state and runtime status.
pub const SCHEMA_VERSION: u32 = 1;

/// Errors returned while validating an audio stream format.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FormatError {
    /// The sample rate is outside Sonora's supported range.
    #[error("sample rate must be between 8,000 and 384,000 Hz, got {0}")]
    SampleRate(u32),
    /// The stream has no channels.
    #[error("audio stream must have at least one channel")]
    Channels,
    /// The sample rate cannot be divided into ten-millisecond frames.
    #[error("sample rate {0} Hz does not produce an integral 10 ms frame")]
    FrameSize(u32),
}

/// A PCM stream format accepted by the processing core.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct StreamFormat {
    /// Samples per second.
    pub sample_rate_hz: u32,
    /// Number of interleaved input channels (the DSP deinterleaves them).
    pub channels: u16,
}

impl StreamFormat {
    /// Creates and validates a stream format.
    pub fn new(sample_rate_hz: u32, channels: u16) -> Result<Self, FormatError> {
        if !(8_000..=384_000).contains(&sample_rate_hz) {
            return Err(FormatError::SampleRate(sample_rate_hz));
        }
        if channels == 0 {
            return Err(FormatError::Channels);
        }
        if !sample_rate_hz.is_multiple_of(100) {
            return Err(FormatError::FrameSize(sample_rate_hz));
        }
        Ok(Self {
            sample_rate_hz,
            channels,
        })
    }

    /// The number of samples in one ten-millisecond frame per channel.
    pub const fn frame_samples(self) -> usize {
        (self.sample_rate_hz / 100) as usize
    }
}

impl Default for StreamFormat {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000,
            channels: 2,
        }
    }
}

/// Noise-suppression strength passed to Sonora.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum NoiseSuppressionLevel {
    /// Approximately 6 dB of suppression.
    Low,
    /// Approximately 12 dB of suppression.
    Moderate,
    /// Approximately 18 dB of suppression.
    #[default]
    High,
    /// Approximately 21 dB of suppression.
    VeryHigh,
}

/// The source used as the render reference for echo cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case", tag = "mode", content = "milliseconds")]
pub enum EchoDelay {
    /// Derive delay from the PipeWire stream timing and queue depth.
    #[default]
    Auto,
    /// Use a fixed delay, clamped to the AEC3 range of 0–500 ms.
    Fixed(u16),
}

impl EchoDelay {
    /// Returns the fixed value, if one was configured.
    pub fn fixed_ms(self) -> Option<u16> {
        match self {
            Self::Auto => None,
            Self::Fixed(value) => Some(value.min(500)),
        }
    }
}

/// Feature switches and DSP settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ProcessingConfig {
    /// Whether noise suppression is requested.
    pub noise_suppression: bool,
    /// Whether echo cancellation is requested.
    pub echo_cancellation: bool,
    /// Noise-suppression strength.
    pub noise_level: NoiseSuppressionLevel,
    /// AEC render-to-capture delay policy.
    pub echo_delay: EchoDelay,
}

impl Default for ProcessingConfig {
    fn default() -> Self {
        Self {
            noise_suppression: false,
            echo_cancellation: false,
            noise_level: NoiseSuppressionLevel::High,
            echo_delay: EchoDelay::Auto,
        }
    }
}

impl ProcessingConfig {
    /// Returns whether the virtual source should be used.
    pub const fn active(&self) -> bool {
        self.noise_suppression || self.echo_cancellation
    }
}

/// PipeWire node names and the published virtual source identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct VirtualSourceConfig {
    /// Raw capture node used as the near-end input.
    pub capture_source: String,
    /// Playback sink whose monitor is used as the far-end reference.
    pub render_target: String,
    /// Name exposed to applications for processed microphone capture.
    pub source_name: String,
    /// Human-readable description of the virtual source.
    pub source_description: String,
}

impl Default for VirtualSourceConfig {
    fn default() -> Self {
        Self {
            capture_source: "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source".into(),
            render_target: "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink".into(),
            source_name: "goxlr_nexus.processed_mic".into(),
            source_description: "GoXLR Nexus processed microphone".into(),
        }
    }
}

/// Runtime configuration for the optional PipeWire service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "kebab-case")]
pub struct RuntimeConfig {
    /// Fixed PCM format negotiated with PipeWire.
    pub format: StreamFormat,
    /// PipeWire graph identities and virtual-source metadata.
    pub source: VirtualSourceConfig,
    /// DSP feature defaults and settings.
    pub processing: ProcessingConfig,
    /// Persistent state path. `None` selects the XDG state directory.
    pub state_path: Option<std::path::PathBuf>,
    /// Control socket path. `None` selects the XDG runtime directory.
    pub control_socket: Option<std::path::PathBuf>,
}

impl RuntimeConfig {
    /// Validates the configured stream and node identities.
    pub fn validate(&self) -> Result<(), FormatError> {
        let _ = StreamFormat::new(self.format.sample_rate_hz, self.format.channels)?;
        if self.source.capture_source.trim().is_empty()
            || self.source.render_target.trim().is_empty()
            || self.source.source_name.trim().is_empty()
        {
            return Err(FormatError::Channels);
        }
        Ok(())
    }
}
