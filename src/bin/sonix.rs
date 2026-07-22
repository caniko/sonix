//! Generic Sonix external-input processor entrypoint.
//!
//! GoXLR routing remains in the adapter binary; this binary only owns the
//! reusable PipeWire capture/render/DSP lifecycle used by every setup.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use nexus_audio_processing::{
    ControlClient, EchoDelay, NoiseSuppressionLevel, ProcessingConfig, ProcessingRuntime,
    RuntimeConfig, StateStore, StreamFormat, VirtualSourceConfig, default_control_socket,
    default_state_path, offline_status,
};
use serde::Deserialize;

#[derive(Parser)]
#[command(about = "Generic Sonix external-input echo/noise processor")]
struct Cli {
    #[arg(long)]
    config: PathBuf,
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    #[command(hide = true)]
    Daemon,
    Status {
        #[arg(long)]
        json: bool,
    },
    #[command(hide = true)]
    FailOpen,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Config {
    capture_source: String,
    render_target: String,
    source_name: String,
    source_description: String,
    capture_sample_rate: u32,
    render_sample_rate: u32,
    capture_channels: u16,
    render_channels: u16,
    noise_suppression: bool,
    echo_cancellation: bool,
    noise_level: NoiseSuppressionLevel,
    echo_delay: EchoDelay,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            capture_source: String::new(),
            render_target: String::new(),
            source_name: "sonix.processed_mic".into(),
            source_description: "Sonix processed microphone".into(),
            capture_sample_rate: 48_000,
            render_sample_rate: 48_000,
            capture_channels: 1,
            render_channels: 2,
            noise_suppression: true,
            echo_cancellation: true,
            noise_level: NoiseSuppressionLevel::High,
            echo_delay: EchoDelay::Auto,
        }
    }
}

fn runtime(config: Config) -> Result<RuntimeConfig> {
    let capture_source = resolve_node(&config.capture_source, "source")?;
    let render_target = resolve_node(&config.render_target, "sink")?;
    let capture_channels = if config.capture_channels == 0 {
        detect_channels(&capture_source).unwrap_or(1)
    } else {
        config.capture_channels
    };
    let source = VirtualSourceConfig::new(
        capture_source,
        render_target,
        config.source_name,
        config.source_description,
    )
    .map_err(|error| anyhow!(error))?;
    let capture = StreamFormat::new(config.capture_sample_rate, capture_channels)
        .map_err(|error| anyhow!(error))?;
    let render = StreamFormat::new(config.render_sample_rate, config.render_channels)
        .map_err(|error| anyhow!(error))?;
    RuntimeConfig::new_with_formats(
        capture,
        render,
        source,
        ProcessingConfig {
            noise_suppression: config.noise_suppression,
            echo_cancellation: config.echo_cancellation,
            noise_level: config.noise_level,
            echo_delay: config.echo_delay,
        },
    )
    .map_err(|error| anyhow!(error))
}

fn resolve_node(value: &str, kind: &str) -> Result<String> {
    if value != "default" {
        return Ok(value.to_owned());
    }
    let selector = if kind == "source" {
        "get-default-source"
    } else {
        "get-default-sink"
    };
    let output = Command::new("pactl")
        .arg(selector)
        .output()
        .with_context(|| format!("resolve default PipeWire {kind}"))?;
    if !output.status.success() {
        bail!("pactl {selector} failed with {}", output.status);
    }
    let value = String::from_utf8(output.stdout).context("default PipeWire node was not UTF-8")?;
    let value = value.trim();
    if value.is_empty() {
        bail!("pactl {selector} returned an empty node name");
    }
    Ok(value.to_owned())
}

fn detect_channels(node: &str) -> Option<u16> {
    let output = Command::new("pw-dump").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let objects = value.as_array()?;
    for object in objects {
        let props = object.pointer("/info/props")?;
        if props.get("node.name").and_then(serde_json::Value::as_str) != Some(node) {
            continue;
        }
        if let Some(channels) = find_number(object, "channels")
            && (1..=32).contains(&channels)
        {
            return u16::try_from(channels).ok();
        }
    }
    None
}

fn find_number(value: &serde_json::Value, key: &str) -> Option<u64> {
    match value {
        serde_json::Value::Object(map) => map.iter().find_map(|(name, child)| {
            if name == key {
                child.as_u64()
            } else {
                find_number(child, key)
            }
        }),
        serde_json::Value::Array(values) => values.iter().find_map(|child| find_number(child, key)),
        _ => None,
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config: Config = serde_json::from_slice(
        &std::fs::read(&cli.config)
            .with_context(|| format!("read Sonix config {}", cli.config.display()))?,
    )
    .context("decode Sonix config JSON")?;
    let runtime_config = runtime(config)?;
    match cli.command {
        CommandKind::Daemon => ProcessingRuntime::new(runtime_config)?
            .run()
            .map_err(|error| anyhow!(error)),
        CommandKind::Status { json } => {
            let client = ControlClient::new(default_control_socket()?);
            let status = match client.status() {
                Ok(status) => status,
                Err(_) => {
                    let state = StateStore::new(default_state_path()?);
                    let state = state.load(runtime_config.processing())?;
                    offline_status(
                        runtime_config.source().source_name(),
                        runtime_config.source().capture_source(),
                        runtime_config.source().render_target(),
                        runtime_config.format(),
                        &state,
                    )
                }
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "daemon={} healthy={} active={} capture={} render={}",
                    status.daemon_running,
                    status.healthy,
                    status.active,
                    status.capture_source,
                    status.render_target
                );
            }
            Ok(())
        }
        CommandKind::FailOpen => {
            let source = runtime_config.source().capture_source();
            let status = Command::new("pactl")
                .args(["set-default-source", source])
                .status()
                .context("run pactl set-default-source")?;
            if !status.success() {
                bail!("pactl set-default-source failed with {status}");
            }
            Ok(())
        }
    }
}
