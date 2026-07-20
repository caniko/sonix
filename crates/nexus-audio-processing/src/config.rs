use serde::{Deserialize, Serialize, de::Error as DeserializeError};
use thiserror::Error;

/// The schema version used by persisted processor state and runtime status.
pub const SCHEMA_VERSION: u32 = 1;

/// Errors returned while validating an audio stream format.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
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

/// Errors returned while validating a PipeWire runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ConfigError {
    /// The configured audio stream format is invalid.
    #[error("invalid audio stream format: {0}")]
    Format(#[from] FormatError),
    /// A PipeWire node identity is empty.
    #[error("{field} must not be empty")]
    EmptyNodeName {
        /// Configuration field containing the empty node identity.
        field: &'static str,
    },
    /// The published virtual source description is empty.
    #[error("source description must not be empty")]
    EmptySourceDescription,
}

/// A PCM stream format accepted by the processing core.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
pub struct StreamFormat {
    /// Samples per second.
    sample_rate_hz: u32,
    /// Number of interleaved input channels (the DSP deinterleaves them).
    channels: u16,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStreamFormat {
    sample_rate_hz: u32,
    channels: u16,
}

impl<'de> Deserialize<'de> for StreamFormat {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawStreamFormat::deserialize(deserializer)?;
        Self::new(raw.sample_rate_hz, raw.channels).map_err(DeserializeError::custom)
    }
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

    /// Returns the samples-per-second rate.
    pub const fn sample_rate_hz(self) -> u32 {
        self.sample_rate_hz
    }

    /// Returns the number of interleaved channels.
    pub const fn channels(self) -> u16 {
        self.channels
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
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct VirtualSourceConfig {
    /// Raw capture node used as the near-end input.
    capture_source: String,
    /// Playback sink whose monitor is used as the far-end reference.
    render_target: String,
    /// Name exposed to applications for processed microphone capture.
    source_name: String,
    /// Human-readable description of the virtual source.
    source_description: String,
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
struct RawVirtualSourceConfig {
    capture_source: String,
    render_target: String,
    source_name: String,
    source_description: String,
}

impl Default for RawVirtualSourceConfig {
    fn default() -> Self {
        let source = VirtualSourceConfig::default();
        Self {
            capture_source: source.capture_source,
            render_target: source.render_target,
            source_name: source.source_name,
            source_description: source.source_description,
        }
    }
}

impl<'de> Deserialize<'de> for VirtualSourceConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = RawVirtualSourceConfig::deserialize(deserializer)?;
        Self::new(
            raw.capture_source,
            raw.render_target,
            raw.source_name,
            raw.source_description,
        )
        .map_err(DeserializeError::custom)
    }
}

impl VirtualSourceConfig {
    /// Creates a validated virtual-source configuration.
    pub fn new(
        capture_source: impl Into<String>,
        render_target: impl Into<String>,
        source_name: impl Into<String>,
        source_description: impl Into<String>,
    ) -> Result<Self, ConfigError> {
        let config = Self {
            capture_source: capture_source.into(),
            render_target: render_target.into(),
            source_name: source_name.into(),
            source_description: source_description.into(),
        };
        config.validate()?;
        Ok(config)
    }

    /// Returns the raw capture node identity.
    pub fn capture_source(&self) -> &str {
        &self.capture_source
    }

    /// Returns the render monitor node identity.
    pub fn render_target(&self) -> &str {
        &self.render_target
    }

    /// Returns the published virtual source name.
    pub fn source_name(&self) -> &str {
        &self.source_name
    }

    /// Returns the published virtual source description.
    pub fn source_description(&self) -> &str {
        &self.source_description
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (field, value) in [
            ("capture source", &self.capture_source),
            ("render target", &self.render_target),
            ("source name", &self.source_name),
        ] {
            if value.trim().is_empty() {
                return Err(ConfigError::EmptyNodeName { field });
            }
        }
        if self.source_description.trim().is_empty() {
            return Err(ConfigError::EmptySourceDescription);
        }
        Ok(())
    }
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
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct RuntimeConfig {
    /// Fixed PCM format negotiated with PipeWire.
    format: StreamFormat,
    /// PipeWire graph identities and virtual-source metadata.
    source: VirtualSourceConfig,
    /// DSP feature defaults and settings.
    processing: ProcessingConfig,
    /// Persistent state path. `None` selects the XDG state directory.
    state_path: Option<std::path::PathBuf>,
    /// Control socket path. `None` selects the XDG runtime directory.
    control_socket: Option<std::path::PathBuf>,
}

impl RuntimeConfig {
    /// Creates a runtime configuration from validated stream and node data.
    pub fn new(
        format: StreamFormat,
        source: VirtualSourceConfig,
        processing: ProcessingConfig,
    ) -> Result<Self, ConfigError> {
        let config = Self {
            format,
            source,
            processing,
            state_path: None,
            control_socket: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Sets an explicit persistent-state path.
    pub fn with_state_path(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.state_path = Some(path.into());
        self
    }

    /// Sets an explicit control-socket path.
    pub fn with_control_socket(mut self, path: impl Into<std::path::PathBuf>) -> Self {
        self.control_socket = Some(path.into());
        self
    }

    /// Returns the negotiated stream format.
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// Returns the virtual-source configuration.
    pub fn source(&self) -> &VirtualSourceConfig {
        &self.source
    }

    /// Returns the default processing settings.
    pub fn processing(&self) -> &ProcessingConfig {
        &self.processing
    }

    /// Returns the explicit state path, if configured.
    pub fn state_path(&self) -> Option<&std::path::Path> {
        self.state_path.as_deref()
    }

    /// Returns the explicit control socket, if configured.
    pub fn control_socket(&self) -> Option<&std::path::Path> {
        self.control_socket.as_deref()
    }

    /// Validates the configured stream and node identities.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let _ = StreamFormat::new(self.format.sample_rate_hz(), self.format.channels())?;
        self.source.validate()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_invalid_stream_formats() {
        assert_eq!(
            StreamFormat::new(44_101, 2),
            Err(FormatError::FrameSize(44_101))
        );
        assert_eq!(StreamFormat::new(48_000, 0), Err(FormatError::Channels));
    }

    #[test]
    fn rejects_invalid_stream_format_during_deserialization() {
        let error = serde_json::from_value::<StreamFormat>(serde_json::json!({
            "sample_rate_hz": 44101,
            "channels": 2
        }))
        .expect_err("invalid frame rates must not deserialize");
        assert!(error.to_string().contains("does not produce"));
    }

    #[test]
    fn reports_empty_runtime_node_names() {
        let mut config = RuntimeConfig::default();
        config.source.capture_source.clear();
        assert_eq!(
            config.validate(),
            Err(ConfigError::EmptyNodeName {
                field: "capture source"
            })
        );
    }

    #[test]
    fn rejects_invalid_virtual_source_during_deserialization() {
        let error = serde_json::from_value::<VirtualSourceConfig>(serde_json::json!({
            "capture-source": "capture",
            "render-target": "render",
            "source-name": "processed",
            "source-description": ""
        }))
        .expect_err("empty descriptions must not deserialize");
        assert!(error.to_string().contains("source description"));
    }

    #[test]
    fn clamps_fixed_echo_delay_to_supported_range() {
        assert_eq!(EchoDelay::Fixed(700).fixed_ms(), Some(500));
    }
}
