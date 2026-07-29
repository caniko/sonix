use anyhow::{Result, anyhow};
use clap::{Subcommand, ValueEnum};
use nexus_audio_processing::{
    ControlClient, EchoDelay, NoiseSuppressionLevel, ProcessingConfig, RuntimeConfig, StateStore,
    StreamFormat, Toggle, VirtualSourceConfig, default_control_socket, default_state_path,
    offline_status,
};

#[derive(Subcommand)]
pub(crate) enum ProcessingCommand {
    /// Toggle noise suppression.
    Noise { state: ProcessingState },
    /// Toggle echo cancellation.
    Echo { state: ProcessingState },
    /// Show processor health and effective routing.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Clear persisted feature overrides.
    Reset,
    /// Restore the raw microphone as the default source.
    #[command(hide = true)]
    FailOpen,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum ProcessingState {
    On,
    Off,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct ProcessingSettings {
    /// Accepted for compatibility with pre-embedded GoXLR configurations.
    pub(crate) enable: Option<bool>,
    pub(crate) source_name: String,
    pub(crate) source_description: String,
    pub(crate) noise_suppression: bool,
    pub(crate) echo_cancellation: bool,
    pub(crate) noise_level: NoiseSuppressionLevel,
    pub(crate) echo_delay: EchoDelay,
}

impl Default for ProcessingSettings {
    fn default() -> Self {
        Self {
            enable: None,
            source_name: "goxlr_nexus.processed_mic".to_string(),
            source_description: "GoXLR Nexus processed microphone".to_string(),
            noise_suppression: true,
            echo_cancellation: true,
            noise_level: NoiseSuppressionLevel::High,
            echo_delay: EchoDelay::Auto,
        }
    }
}

pub(crate) fn processing_runtime_config(config: &super::Config) -> Result<RuntimeConfig> {
    let source = VirtualSourceConfig::new(
        config.profile.default_source.clone(),
        config.profile.default_sink.clone(),
        config.processing.source_name.clone(),
        config.processing.source_description.clone(),
    )
    .map_err(|error| anyhow!(error))?;
    let processing = ProcessingConfig {
        noise_suppression: config.processing.enable.unwrap_or(true)
            && config.processing.noise_suppression,
        echo_cancellation: config.processing.enable.unwrap_or(true)
            && config.processing.echo_cancellation,
        noise_level: config.processing.noise_level,
        echo_delay: config.processing.echo_delay,
    };
    let capture_format = StreamFormat::new(
        config.profile.capture_sample_rate,
        config.profile.capture_channels,
    )
    .map_err(|error| anyhow!(error))?;
    let render_format = StreamFormat::new(
        config.profile.render_sample_rate,
        config.profile.render_channels,
    )
    .map_err(|error| anyhow!(error))?;
    RuntimeConfig::new_with_formats(capture_format, render_format, source, processing)
        .map_err(|error| anyhow!(error))
}

pub(crate) fn processing_command(config: &super::Config, command: ProcessingCommand) -> Result<()> {
    let runtime = processing_runtime_config(config)?;
    match command {
        ProcessingCommand::Noise { state } => processing_toggle(
            config,
            &runtime,
            Toggle::Noise,
            matches!(state, ProcessingState::On),
        ),
        ProcessingCommand::Echo { state } => processing_toggle(
            config,
            &runtime,
            Toggle::Echo,
            matches!(state, ProcessingState::On),
        ),
        ProcessingCommand::Status { json } => processing_status(&runtime, json),
        ProcessingCommand::Reset => {
            let client =
                ControlClient::new(default_control_socket().map_err(|error| anyhow!(error))?);
            match client.reset() {
                Ok(status) => {
                    print_processing_status(&status, false);
                    reconcile_processing_source(&status);
                }
                Err(_) => {
                    let store =
                        StateStore::new(default_state_path().map_err(|error| anyhow!(error))?);
                    let state = store.reset(runtime.processing())?;
                    let status = offline_status(
                        runtime.source().source_name(),
                        runtime.source().capture_source(),
                        runtime.source().render_target(),
                        runtime.format(),
                        &state,
                    );
                    print_processing_status(&status, false);
                    reconcile_processing_source(&status);
                }
            }
            Ok(())
        }
        ProcessingCommand::FailOpen => restore_raw_microphone(config),
    }
}

fn processing_toggle(
    _config: &super::Config,
    runtime: &RuntimeConfig,
    toggle: Toggle,
    enabled: bool,
) -> Result<()> {
    let client = ControlClient::new(default_control_socket().map_err(|error| anyhow!(error))?);
    let result = match toggle {
        Toggle::Noise => client.set_noise(enabled),
        Toggle::Echo => client.set_echo(enabled),
    };
    match result {
        Ok(status) => {
            print_processing_status(&status, false);
            reconcile_processing_source(&status);
        }
        Err(error) => {
            let store = StateStore::new(default_state_path().map_err(|error| anyhow!(error))?);
            let state = store.set(toggle, enabled, runtime.processing())?;
            let status = offline_status(
                runtime.source().source_name(),
                runtime.source().capture_source(),
                runtime.source().render_target(),
                runtime.format(),
                &state,
            );
            eprintln!("warn: processing daemon unavailable ({error}); change saved for startup");
            print_processing_status(&status, false);
        }
    }
    Ok(())
}

fn processing_status(runtime: &RuntimeConfig, json_output: bool) -> Result<()> {
    let client = ControlClient::new(default_control_socket().map_err(|error| anyhow!(error))?);
    let status = match client.status() {
        Ok(status) => status,
        Err(_) => {
            let state = StateStore::new(default_state_path().map_err(|error| anyhow!(error))?)
                .load(runtime.processing())?;
            offline_status(
                runtime.source().source_name(),
                runtime.source().capture_source(),
                runtime.source().render_target(),
                runtime.format(),
                &state,
            )
        }
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_processing_status(&status, true);
    }
    Ok(())
}

pub(crate) fn restore_raw_microphone(config: &super::Config) -> Result<()> {
    super::run_or_print(
        false,
        "pactl",
        &["set-default-source", &config.profile.default_source],
    )
}

fn print_processing_status(status: &nexus_audio_processing::RuntimeStatus, queried: bool) {
    if queried {
        println!("processing status:");
    }
    println!(
        "  daemon={} healthy={} active={} noise={} echo={}",
        status.daemon_running,
        status.healthy,
        status.active,
        status.noise_suppression,
        status.echo_cancellation
    );
    println!(
        "  source={} raw={}",
        status.processed_source, status.capture_source
    );
    if let Some(error) = &status.last_error {
        println!("  diagnostic: {error}");
    }
}

fn reconcile_processing_source(status: &nexus_audio_processing::RuntimeStatus) {
    let desired = if status.daemon_running && status.healthy && status.active {
        &status.processed_source
    } else {
        &status.capture_source
    };
    if let Err(error) = super::run_or_print(false, "pactl", &["set-default-source", desired]) {
        eprintln!("warn: cannot select processing microphone {desired}: {error:#}");
    }
}
