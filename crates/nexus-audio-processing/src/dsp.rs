use crate::config::StreamFormat;
#[cfg(feature = "sonora")]
use crate::config::{EchoDelay, ProcessingConfig};
use thiserror::Error;

/// Errors returned by the portable DSP layer.
#[derive(Debug, Error)]
pub enum DspError {
    /// The configured stream format is invalid.
    #[error("invalid stream format: {0}")]
    Format(#[from] crate::config::FormatError),
    /// Sonora rejected a frame or stream parameter.
    #[cfg(feature = "sonora")]
    #[error("sonora rejected audio: {0}")]
    Sonora(#[from] sonora::Error),
    /// The interleaved frame length does not match the configured format.
    #[error("interleaved frame has {actual} samples; expected {expected}")]
    FrameLength {
        /// Number of samples supplied by the caller.
        actual: usize,
        /// Number of samples required by the configured format.
        expected: usize,
    },
}

/// One deinterleaved ten-millisecond PCM frame.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioFrame {
    /// One vector per channel, each containing `format.frame_samples()` f32s.
    pub channels: Vec<Vec<f32>>,
}

impl AudioFrame {
    /// Creates a silent frame for a format.
    pub fn silence(format: StreamFormat) -> Self {
        Self {
            channels: vec![vec![0.0; format.frame_samples()]; format.channels as usize],
        }
    }

    /// Deinterleaves an f32 frame from PipeWire.
    pub fn from_interleaved(format: StreamFormat, samples: &[f32]) -> Result<Self, DspError> {
        let expected = format.frame_samples() * format.channels as usize;
        if samples.len() != expected {
            return Err(DspError::FrameLength {
                actual: samples.len(),
                expected,
            });
        }
        let mut channels = vec![vec![0.0; format.frame_samples()]; format.channels as usize];
        for (frame, sample_set) in samples.chunks_exact(format.channels as usize).enumerate() {
            for (channel, sample) in sample_set.iter().enumerate() {
                channels[channel][frame] = *sample;
            }
        }
        Ok(Self { channels })
    }

    /// Interleaves this frame for a PipeWire output buffer.
    pub fn write_interleaved(&self, output: &mut [f32]) -> Result<(), DspError> {
        let channels = self.channels.len();
        let frames = self.channels.first().map_or(0, Vec::len);
        let expected = channels * frames;
        if output.len() != expected {
            return Err(DspError::FrameLength {
                actual: output.len(),
                expected,
            });
        }
        for frame in 0..frames {
            for channel in 0..channels {
                output[frame * channels + channel] = self.channels[channel][frame];
            }
        }
        Ok(())
    }
}

/// A Sonora-backed duplex processor.
#[cfg(feature = "sonora")]
#[derive(Debug)]
pub struct DuplexProcessor {
    format: StreamFormat,
    config: ProcessingConfig,
    apm: sonora::AudioProcessing,
}

#[cfg(feature = "sonora")]
impl DuplexProcessor {
    /// Creates a processor with the supplied format and feature state.
    pub fn new(format: StreamFormat, config: ProcessingConfig) -> Result<Self, DspError> {
        let format = StreamFormat::new(format.sample_rate_hz, format.channels)?;
        let apm = build_apm(format, &config);
        let mut processor = Self {
            format,
            config,
            apm,
        };
        processor.apply_delay()?;
        Ok(processor)
    }

    /// Returns the configured stream format.
    pub const fn format(&self) -> StreamFormat {
        self.format
    }

    /// Returns the active processing configuration.
    pub const fn config(&self) -> &ProcessingConfig {
        &self.config
    }

    /// Rebuilds the processor after changing feature state.
    pub fn set_config(&mut self, config: ProcessingConfig) -> Result<(), DspError> {
        self.config = config;
        self.apm = build_apm(self.format, &self.config);
        self.apply_delay()
    }

    /// Sets the AEC delay without rebuilding state.
    pub fn set_echo_delay(&mut self, delay: EchoDelay) -> Result<(), DspError> {
        self.config.echo_delay = delay;
        self.apply_delay()
    }

    /// Supplies a render frame to AEC3. The output is unchanged.
    pub fn process_render(&mut self, frame: &AudioFrame) -> Result<AudioFrame, DspError> {
        if !self.config.echo_cancellation {
            return Ok(frame.clone());
        }
        let mut output = AudioFrame::silence(self.format);
        let input = frame.channels.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut destination = output
            .channels
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        self.apm.process_render_f32(&input, &mut destination)?;
        Ok(output)
    }

    /// Processes one microphone frame after the matching render frame.
    pub fn process_capture(&mut self, frame: &AudioFrame) -> Result<AudioFrame, DspError> {
        if !self.config.active() {
            return Ok(frame.clone());
        }
        let mut output = AudioFrame::silence(self.format);
        let input = frame.channels.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let mut destination = output
            .channels
            .iter_mut()
            .map(Vec::as_mut_slice)
            .collect::<Vec<_>>();
        self.apm.process_capture_f32(&input, &mut destination)?;
        Ok(output)
    }

    fn apply_delay(&mut self) -> Result<(), DspError> {
        if self.config.echo_cancellation {
            if let Some(delay) = self.config.echo_delay.fixed_ms() {
                self.apm.set_stream_delay_ms(delay as i32)?;
            } else {
                self.apm.set_stream_delay_ms(0)?;
            }
        }
        Ok(())
    }
}

#[cfg(feature = "sonora")]
fn build_apm(format: StreamFormat, config: &ProcessingConfig) -> sonora::AudioProcessing {
    use sonora::config::{EchoCanceller, MaxProcessingRate, NoiseSuppression, TransparentModeType};
    use sonora::{AudioProcessing, Config, StreamConfig};

    let mut sonora_config = Config::default();
    sonora_config.pipeline.maximum_internal_processing_rate = MaxProcessingRate::Rate48kHz;
    sonora_config.pipeline.multi_channel_render = true;
    sonora_config.pipeline.multi_channel_capture = true;
    if config.echo_cancellation {
        sonora_config.echo_canceller = Some(EchoCanceller {
            transparent_mode: TransparentModeType::Hmm,
            ..Default::default()
        });
    }
    if config.noise_suppression {
        sonora_config.noise_suppression = Some(NoiseSuppression {
            level: match config.noise_level {
                crate::config::NoiseSuppressionLevel::Low => {
                    sonora::config::NoiseSuppressionLevel::Low
                }
                crate::config::NoiseSuppressionLevel::Moderate => {
                    sonora::config::NoiseSuppressionLevel::Moderate
                }
                crate::config::NoiseSuppressionLevel::High => {
                    sonora::config::NoiseSuppressionLevel::High
                }
                crate::config::NoiseSuppressionLevel::VeryHigh => {
                    sonora::config::NoiseSuppressionLevel::VeryHigh
                }
            },
            analyze_linear_aec_output_when_available: config.echo_cancellation,
        });
    }
    let stream = StreamConfig::new(format.sample_rate_hz, format.channels);
    AudioProcessing::builder()
        .config(sonora_config)
        .capture_config(stream)
        .render_config(stream)
        .build()
}

#[cfg(all(test, feature = "sonora"))]
mod tests {
    use super::*;
    use crate::config::{NoiseSuppressionLevel, ProcessingConfig};

    #[test]
    fn frame_round_trip_preserves_stereo_channels() {
        let format = StreamFormat::default();
        let input = (0..format.frame_samples() * 2)
            .map(|value| value as f32 / 1000.0)
            .collect::<Vec<_>>();
        let frame = AudioFrame::from_interleaved(format, &input).unwrap();
        let mut output = vec![0.0; input.len()];
        frame.write_interleaved(&mut output).unwrap();
        assert_eq!(input, output);
    }

    #[test]
    fn inactive_processor_is_bit_transparent() {
        let format = StreamFormat::default();
        let config = ProcessingConfig::default();
        let mut processor = DuplexProcessor::new(format, config).unwrap();
        let frame = AudioFrame::from_interleaved(format, &vec![0.25; 960]).unwrap();
        let render = processor.process_render(&frame).unwrap();
        let capture = processor.process_capture(&frame).unwrap();
        assert_eq!(render, frame);
        assert_eq!(capture, frame);
    }

    #[test]
    fn processing_config_maps_noise_level() {
        let format = StreamFormat::default();
        let config = ProcessingConfig {
            noise_suppression: true,
            noise_level: NoiseSuppressionLevel::VeryHigh,
            ..Default::default()
        };
        let processor = DuplexProcessor::new(format, config.clone()).unwrap();
        assert_eq!(processor.config(), &config);
    }

    #[test]
    fn echo_processor_accepts_render_before_capture() {
        let format = StreamFormat::default();
        let config = ProcessingConfig {
            echo_cancellation: true,
            ..Default::default()
        };
        let mut processor = DuplexProcessor::new(format, config).unwrap();
        let render = AudioFrame::silence(format);
        let capture = AudioFrame::silence(format);
        processor.process_render(&render).unwrap();
        let output = processor.process_capture(&capture).unwrap();
        assert_eq!(output.channels.len(), format.channels as usize);
        assert_eq!(output.channels[0].len(), format.frame_samples());
    }
}
