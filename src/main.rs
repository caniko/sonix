use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Args, Parser, Subcommand, ValueEnum};
use nexus_audio_processing::{
    ControlClient, EchoDelay, NoiseSuppressionLevel, ProcessingConfig, ProcessingRuntime,
    RuntimeConfig, StateStore, Toggle, VirtualSourceConfig, default_control_socket,
    default_state_path, offline_status,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

mod observation;
use observation::{
    Facts, GoxlrFacts, ObsFacts, Observation, PipewireFacts, PipewireNode, Producer, SCHEMA,
    SourceStatus, facts_digest,
};

const GOXLR_SYSTEM: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
const GOXLR_CHAT: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Headphones__sink";
const GOXLR_GAME: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line1__sink";
const GOXLR_MUSIC: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line2__sink";
const GOXLR_SAMPLE: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line3__sink";
const GOXLR_CHAT_MIC: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
const GOXLR_STREAM_MIX: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
const GOXLR_SAMPLER: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line5__source";
const OBS_INPUT_KIND: &str = "pulse_input_capture";
const PLAN_SCHEMA: &str = "goxlr-nexus.plan/v2";
const ADOPT_SCHEMA: &str = "goxlr-nexus.adopt/v1";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Subcommand)]
enum CommandKind {
    Discover {
        #[arg(long)]
        json: bool,
    },
    /// Suggest a reviewable config from uniquely identified live devices.
    Adopt {
        #[arg(long)]
        json: bool,
    },
    Doctor,
    /// Compute a read-only reconciliation plan from the live graph.
    Plan {
        #[arg(long)]
        json: bool,
    },
    Status {
        #[arg(long)]
        json: bool,
    },
    Apply(ApplyArgs),
    Follow {
        #[arg(long)]
        observe_only: bool,
    },
    Profile {
        profile: Profile,
        #[arg(long)]
        dry_run: bool,
    },
    Obs {
        #[command(subcommand)]
        command: ObsCommand,
    },
    /// Toggle and inspect the optional microphone processor.
    Processing {
        #[command(subcommand)]
        command: ProcessingCommand,
    },
}

#[derive(Args)]
struct ApplyArgs {
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Profile {
    Stream,
    Desktop,
    Off,
}

#[derive(Subcommand)]
enum ObsCommand {
    Sync {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ProcessingCommand {
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
    /// Run the PipeWire processor service.
    #[command(hide = true)]
    Daemon,
    /// Restore the raw microphone as the default source.
    #[command(hide = true)]
    FailOpen,
}

#[derive(Clone, Copy, ValueEnum)]
enum ProcessingState {
    On,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Config {
    user: String,
    jds_sink: Option<String>,
    fallback_sink: Option<String>,
    thinkpad_sink: Option<String>,
    output_sinks: Vec<String>,
    goxlr_serial: Option<String>,
    max_monitor_sink_volume: Option<f64>,
    observe_only: bool,
    profile: ProfileConfig,
    obs: ObsConfig,
    processing: ProcessingSettings,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            user: "can".to_string(),
            jds_sink: None,
            fallback_sink: None,
            thinkpad_sink: None,
            output_sinks: Vec::new(),
            goxlr_serial: None,
            max_monitor_sink_volume: None,
            observe_only: false,
            profile: ProfileConfig::default(),
            obs: ObsConfig::default(),
            processing: ProcessingSettings::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ProcessingSettings {
    enable: bool,
    source_name: String,
    source_description: String,
    noise_suppression: bool,
    echo_cancellation: bool,
    noise_level: NoiseSuppressionLevel,
    echo_delay: EchoDelay,
}

impl Default for ProcessingSettings {
    fn default() -> Self {
        Self {
            enable: true,
            source_name: "goxlr_nexus.processed_mic".to_string(),
            source_description: "GoXLR Nexus processed microphone".to_string(),
            noise_suppression: false,
            echo_cancellation: false,
            noise_level: NoiseSuppressionLevel::High,
            echo_delay: EchoDelay::Auto,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ProfileConfig {
    default_sink: String,
    default_source: String,
    monitor_source: String,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            default_sink: GOXLR_SYSTEM.to_string(),
            default_source: GOXLR_CHAT_MIC.to_string(),
            monitor_source: GOXLR_STREAM_MIX.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ObsConfig {
    enable: bool,
    host: String,
    port: u16,
    password_file: Option<PathBuf>,
    sources: Vec<ObsSourceConfig>,
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            enable: false,
            host: "127.0.0.1".to_string(),
            port: 4455,
            password_file: None,
            sources: default_obs_sources(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ObsSourceConfig {
    name: String,
    device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ObsSourcePlan {
    name: String,
    input_kind: String,
    device_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObsHello {
    authentication: Option<ObsAuthentication>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObsAuthentication {
    challenge: String,
    salt: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ObsRequestStatus {
    result: bool,
    code: Option<u16>,
    comment: Option<String>,
}

#[derive(Debug, Clone)]
struct Node {
    id: u32,
    name: String,
    description: Option<String>,
    media_class: Option<String>,
    properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Port {
    id: u32,
    node_id: u32,
    direction: String,
    name: String,
    channel: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Link {
    id: u32,
    output_node_id: u32,
    output_port_id: u32,
    input_node_id: u32,
    input_port_id: u32,
    state: Option<String>,
}

#[derive(Debug, Default)]
struct PipewireGraph {
    nodes: Vec<Node>,
    ports: Vec<Port>,
    links: Vec<Link>,
}

#[derive(Debug)]
struct Snapshot {
    nodes: Vec<Node>,
    ports: Vec<Port>,
    links: Vec<Link>,
    goxlr_status: Result<Value, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AudioOperation {
    SetDefaultSink(String),
    SetDefaultSource(String),
    SetSinkVolume {
        sink: String,
        percent: u32,
    },
    Connect {
        source_node: String,
        source_port: String,
        sink_node: String,
        sink_port: String,
    },
    Disconnect {
        source_node: String,
        source_port: String,
        sink_node: String,
        sink_port: String,
    },
}

#[derive(Debug)]
struct RequiredNodes {
    outputs: OutputNodes,
    default_source: Node,
    monitor_source: Node,
}

#[derive(Debug)]
struct OutputNodes {
    output_sinks: Vec<Node>,
    fallback_sink: Node,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReconciliationPlan {
    schema: &'static str,
    producer: Producer,
    read_only: bool,
    requires_apply: bool,
    current: PlanCurrent,
    goxlr_status: String,
    operations: Vec<PlanOperation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    obs: Option<PlanObs>,
    diagnostics: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanCurrent {
    default_sink: String,
    default_source: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanOperation {
    action: String,
    target: Option<String>,
    arguments: Vec<String>,
    reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanObs {
    status: String,
    scene: Option<String>,
    choice_status: String,
    sources: Vec<PlanObsSource>,
    diagnostic: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PlanObsSource {
    name: String,
    device_id: String,
    action: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdoptionReport {
    schema: &'static str,
    producer: Producer,
    read_only: bool,
    config: AdoptConfig,
    candidates: Vec<AdoptCandidate>,
    diagnostics: Vec<String>,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdoptConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    goxlr_serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jds_sink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_sink: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinkpad_sink: Option<String>,
    output_sinks: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    monitor_source: Option<String>,
    obs_sources: Vec<AdoptObsSource>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdoptCandidate {
    role: String,
    runtime_name: String,
    description: Option<String>,
    confidence: String,
    reason: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AdoptObsSource {
    name: String,
    device_id: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config;
    match cli.command {
        CommandKind::Discover { json } => discover(config_path.as_deref(), json),
        CommandKind::Adopt { json } => adopt(json),
        command => {
            let config = load_config(config_path.as_deref())?;
            match command {
                CommandKind::Discover { .. } => unreachable!("discover is handled above"),
                CommandKind::Adopt { .. } => unreachable!("adopt is handled above"),
                CommandKind::Doctor => doctor(&config),
                CommandKind::Plan { json } => plan(&config, json),
                CommandKind::Status { json } => status(&config, json),
                CommandKind::Apply(args) => apply(&config, args.dry_run),
                CommandKind::Follow { observe_only } => follow(&config, observe_only),
                CommandKind::Profile { profile, dry_run } => {
                    apply_profile(&config, profile, dry_run)
                }
                CommandKind::Obs {
                    command: ObsCommand::Sync { dry_run },
                } => obs_sync(&config, dry_run),
                CommandKind::Processing { command } => processing_command(&config, command),
            }
        }
    }
}

fn discover(explicit_config: Option<&Path>, json_output: bool) -> Result<()> {
    let config = match load_config(explicit_config) {
        Ok(config) => config,
        Err(err) if explicit_config.is_none() => {
            eprintln!("warn: ignoring invalid existing config during discovery: {err:#}");
            Config::default()
        }
        Err(err) => return Err(err),
    };

    let mut sources = Vec::new();
    let mut nodes = match pipewire_nodes() {
        Ok(nodes) => {
            sources.push(SourceStatus {
                source: "pw-dump".to_string(),
                status: "ok".to_string(),
                diagnostic: None,
            });
            nodes
        }
        Err(err) => {
            sources.push(SourceStatus {
                source: "pw-dump".to_string(),
                status: "unavailable".to_string(),
                diagnostic: Some(format!("{err:#}")),
            });
            Vec::new()
        }
    };
    nodes.sort_by(|left, right| left.name.cmp(&right.name));

    let default_sink = match current_default_sink() {
        Ok(value) => {
            sources.push(SourceStatus {
                source: "pactl.get-default-sink".to_string(),
                status: "ok".to_string(),
                diagnostic: None,
            });
            Some(value)
        }
        Err(err) => {
            sources.push(SourceStatus {
                source: "pactl.get-default-sink".to_string(),
                status: "unavailable".to_string(),
                diagnostic: Some(format!("{err:#}")),
            });
            None
        }
    };
    let default_source = match current_default_source() {
        Ok(value) => {
            sources.push(SourceStatus {
                source: "pactl.get-default-source".to_string(),
                status: "ok".to_string(),
                diagnostic: None,
            });
            Some(value)
        }
        Err(err) => {
            sources.push(SourceStatus {
                source: "pactl.get-default-source".to_string(),
                status: "unavailable".to_string(),
                diagnostic: Some(format!("{err:#}")),
            });
            None
        }
    };

    let (goxlr, goxlr_source) = match goxlr_facts() {
        Ok(facts) => (
            facts,
            SourceStatus {
                source: "goxlr-client --status-json".to_string(),
                status: "ok".to_string(),
                diagnostic: None,
            },
        ),
        Err(err) => (
            GoxlrFacts {
                status: "unavailable".to_string(),
                mixers: BTreeMap::new(),
                diagnostic: Some(format!("{err:#}")),
            },
            SourceStatus {
                source: "goxlr-client --status-json".to_string(),
                status: "unavailable".to_string(),
                diagnostic: Some(format!("{err:#}")),
            },
        ),
    };
    sources.push(goxlr_source);

    let (obs, obs_source) = match obs_facts(&config) {
        Ok(facts) => {
            let status = facts.status.clone();
            let diagnostic = facts.diagnostic.clone();
            (
                facts,
                SourceStatus {
                    source: "obs-websocket".to_string(),
                    status,
                    diagnostic,
                },
            )
        }
        Err(err) => (
            unavailable_obs_facts(format!("{err:#}")),
            SourceStatus {
                source: "obs-websocket".to_string(),
                status: "unavailable".to_string(),
                diagnostic: Some(format!("{err:#}")),
            },
        ),
    };
    sources.push(obs_source);

    let facts = Facts {
        pipewire: PipewireFacts {
            default_sink,
            default_source,
            nodes: nodes.into_iter().map(pipewire_node).collect(),
        },
        goxlr,
        obs,
    };
    let observation = Observation {
        schema: SCHEMA,
        producer: Producer {
            name: "goxlr-nexus",
            version: env!("CARGO_PKG_VERSION"),
        },
        captured_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default(),
        host: std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string()),
        sources,
        facts_digest: facts_digest(&facts),
        facts,
    };

    if json_output {
        println!("{}", serde_json::to_string_pretty(&observation)?);
    } else {
        print_observation(&observation);
    }
    Ok(())
}

/// Suggest a human-readable Home Manager fragment from stable live labels.
/// Ambiguous or missing labels are reported instead of being guessed.
fn adopt(json_output: bool) -> Result<()> {
    let nodes = pipewire_nodes()?;
    let mut candidates = Vec::new();
    let mut diagnostics = Vec::new();
    let mut config = AdoptConfig::default();

    let jds = adopt_node(
        &nodes,
        "audio.output.desk",
        "Audio/Sink",
        |node| {
            node.description
                .as_deref()
                .is_some_and(|value| value.contains("JDS Labs Element"))
        },
        "description contains the JDS Labs Element DAC product label",
        &mut candidates,
        &mut diagnostics,
    );
    let thinkpad = adopt_node(
        &nodes,
        "audio.output.dock",
        "Audio/Sink",
        |node| {
            node.description
                .as_deref()
                .is_some_and(|value| value.contains("ThinkPad Thunderbolt 4 Dock"))
        },
        "description contains the ThinkPad Thunderbolt 4 Dock product label",
        &mut candidates,
        &mut diagnostics,
    );
    let default_source = adopt_node(
        &nodes,
        "audio.source.chat-mic",
        "Audio/Source",
        |node| node.description.as_deref() == Some("GoXLR Chat Mic"),
        "exact GoXLR Chat Mic description",
        &mut candidates,
        &mut diagnostics,
    );
    let monitor_source = adopt_node(
        &nodes,
        "audio.source.stream-mix",
        "Audio/Source",
        |node| {
            node.description
                .as_deref()
                .is_some_and(|value| value.starts_with("GoXLR Stream Mix"))
        },
        "GoXLR Stream Mix description",
        &mut candidates,
        &mut diagnostics,
    );
    let sampler = adopt_node(
        &nodes,
        "audio.source.sampler",
        "Audio/Source",
        |node| node.description.as_deref() == Some("GoXLR Sampler"),
        "exact GoXLR Sampler description",
        &mut candidates,
        &mut diagnostics,
    );

    if let Some(node) = jds {
        config.jds_sink = Some(node.name.clone());
        config.fallback_sink = Some(node.name.clone());
        config.output_sinks.push(node.name);
    }
    if let Some(node) = thinkpad {
        config.output_sinks.push(node.name.clone());
        config.thinkpad_sink = Some(node.name);
    }
    config.default_source = default_source.as_ref().map(|node| node.name.clone());
    config.monitor_source = monitor_source.as_ref().map(|node| node.name.clone());
    if let Some(node) = default_source {
        config.obs_sources.push(AdoptObsSource {
            name: "GoXLR Mic".to_string(),
            device_id: node.name,
        });
    }
    if let Some(node) = monitor_source.as_ref() {
        config.obs_sources.push(AdoptObsSource {
            name: "GoXLR Stream Mix".to_string(),
            device_id: node.name.clone(),
        });
        config.obs_sources.push(AdoptObsSource {
            name: "GoXLR Desktop Mix".to_string(),
            device_id: node.name.clone(),
        });
    }
    if let Some(node) = sampler {
        config.obs_sources.push(AdoptObsSource {
            name: "GoXLR Sampler".to_string(),
            device_id: node.name,
        });
    }

    match goxlr_facts() {
        Ok(facts) if facts.mixers.len() == 1 => {
            let (serial, mixer) = facts.mixers.into_iter().next().expect("length checked");
            let device_type = mixer
                .pointer("/hardware/device_type")
                .and_then(Value::as_str);
            if device_type == Some("Full") {
                candidates.push(AdoptCandidate {
                    role: "audio.mixer".to_string(),
                    runtime_name: serial.clone(),
                    description: Some("GoXLR Full mixer".to_string()),
                    confidence: "high".to_string(),
                    reason: "exactly one full GoXLR is reported by goxlr-client".to_string(),
                });
                config.goxlr_serial = Some(serial);
            } else {
                diagnostics.push(format!(
                    "the only GoXLR mixer is not a full device (device_type={device_type:?})"
                ));
            }
        }
        Ok(facts) if facts.mixers.is_empty() => {
            diagnostics.push("goxlr-client reported no mixers".to_string());
        }
        Ok(facts) => {
            diagnostics.push(format!(
                "goxlr-client reported multiple mixers ({}); set goxlr-serial explicitly",
                facts.mixers.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Err(error) => diagnostics.push(format!("GoXLR serial discovery unavailable: {error:#}")),
    }

    let report = AdoptionReport {
        schema: ADOPT_SCHEMA,
        producer: Producer {
            name: "goxlr-nexus",
            version: env!("CARGO_PKG_VERSION"),
        },
        read_only: true,
        config,
        candidates,
        diagnostics,
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_adoption(&report);
    }
    Ok(())
}

fn adopt_node<F>(
    nodes: &[Node],
    role: &str,
    media_class: &str,
    predicate: F,
    reason: &str,
    candidates: &mut Vec<AdoptCandidate>,
    diagnostics: &mut Vec<String>,
) -> Option<Node>
where
    F: Fn(&Node) -> bool,
{
    let matches: Vec<Node> = nodes
        .iter()
        .filter(|node| node.media_class.as_deref() == Some(media_class) && predicate(node))
        .cloned()
        .collect();
    match matches.as_slice() {
        [node] => {
            candidates.push(AdoptCandidate {
                role: role.to_string(),
                runtime_name: node.name.clone(),
                description: node.description.clone(),
                confidence: "high".to_string(),
                reason: reason.to_string(),
            });
            Some(node.clone())
        }
        [] => {
            diagnostics.push(format!(
                "no unique candidate for {role}: no {media_class} node matched"
            ));
            None
        }
        _ => {
            for node in &matches {
                candidates.push(AdoptCandidate {
                    role: role.to_string(),
                    runtime_name: node.name.clone(),
                    description: node.description.clone(),
                    confidence: "ambiguous".to_string(),
                    reason: reason.to_string(),
                });
            }
            diagnostics.push(format!(
                "no unique candidate for {role}: {} {media_class} nodes matched",
                matches.len()
            ));
            None
        }
    }
}

fn print_adoption(report: &AdoptionReport) {
    println!("{} on {}", report.schema, report.producer.name);
    println!("Review this Home Manager fragment before copying it:");
    println!("programs.goxlr-nexus = {{");
    if let Some(value) = &report.config.jds_sink {
        println!("  jdsSink = \"{}\";", nix_quote(value));
    }
    if let Some(value) = &report.config.fallback_sink {
        println!("  fallbackSink = \"{}\";", nix_quote(value));
    }
    if let Some(value) = &report.config.thinkpad_sink {
        println!("  thinkpadSink = \"{}\";", nix_quote(value));
    }
    if !report.config.output_sinks.is_empty() {
        println!("  outputSinks = [");
        for value in &report.config.output_sinks {
            println!("    \"{}\"", nix_quote(value));
        }
        println!("  ];");
    }
    if let Some(value) = &report.config.goxlr_serial {
        println!("  goxlrSerial = \"{}\";", nix_quote(value));
    }
    if let Some(value) = &report.config.default_source {
        println!("  defaultSource = \"{}\";", nix_quote(value));
    }
    if let Some(value) = &report.config.monitor_source {
        println!("  monitorSource = \"{}\";", nix_quote(value));
    }
    if !report.config.obs_sources.is_empty() {
        println!("  obs.sources = [");
        for source in &report.config.obs_sources {
            println!(
                "    {{ name = \"{}\"; deviceId = \"{}\"; }}",
                nix_quote(&source.name),
                nix_quote(&source.device_id)
            );
        }
        println!("  ];");
    }
    println!("}};");
    for candidate in &report.candidates {
        println!(
            "candidate {} [{}]: {} ({})",
            candidate.role, candidate.confidence, candidate.runtime_name, candidate.reason
        );
    }
    for diagnostic in &report.diagnostics {
        println!("diagnostic: {diagnostic}");
    }
}

fn nix_quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn pipewire_node(node: Node) -> PipewireNode {
    PipewireNode {
        runtime_name: node.name,
        description: node.description,
        media_class: node.media_class,
        properties: node.properties,
    }
}

fn goxlr_facts() -> Result<GoxlrFacts> {
    let status = goxlr_status()?;
    let mixers = status
        .get("mixers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("goxlr-client JSON has no mixers object"))?
        .iter()
        .map(|(serial, mixer)| (serial.clone(), mixer.clone()))
        .collect();
    Ok(GoxlrFacts {
        status: "ok".to_string(),
        mixers,
        diagnostic: None,
    })
}

fn obs_facts(config: &Config) -> Result<ObsFacts> {
    let mut client = ObsClient::connect(config)?;
    let version = client.request("GetVersion", json!({}))?;
    let scene = client.target_scene().ok();
    let mut scene_sources = scene
        .as_deref()
        .and_then(|name| client.scene_source_names(name).ok());
    if let Some(sources) = &mut scene_sources {
        sources.sort();
    }
    let input_kind_response = client.request("GetInputKindList", json!({}))?;
    let input_kinds: Vec<String> = input_kind_response
        .get("inputKindList")
        .or_else(|| input_kind_response.get("inputKinds"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let existing_inputs = client
        .request("GetInputList", json!({}))?
        .get("inputs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let default_settings = client
        .request(
            "GetInputDefaultSettings",
            json!({"inputKind": OBS_INPUT_KIND}),
        )
        .ok();
    let pulse_input = existing_inputs
        .iter()
        .filter(|input| input.get("inputKind").and_then(Value::as_str) == Some(OBS_INPUT_KIND))
        .min_by_key(|input| input.get("inputName").and_then(Value::as_str).unwrap_or(""));
    let (input_settings, device_choices, choice_status, optional_diagnostic) = match pulse_input {
        Some(input) => {
            let input_name = input
                .get("inputName")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("OBS pulse input has no inputName"))?;
            let settings = client
                .request("GetInputSettings", json!({"inputName": input_name}))
                .ok();
            match client.request(
                "GetInputPropertiesListPropertyItems",
                json!({"inputName": input_name, "propertyName": "device_id"}),
            ) {
                Ok(choices) => (settings, Some(choices), "ok".to_string(), None),
                Err(err) => (
                    settings,
                    None,
                    "unavailable".to_string(),
                    Some(format!("device_id choices unavailable: {err:#}")),
                ),
            }
        }
        None => (None, None, "no-reference-input".to_string(), None),
    };

    let mut input_kinds = input_kinds;
    input_kinds.sort();
    let mut existing_inputs = existing_inputs;
    existing_inputs.sort_by(|left, right| {
        left.get("inputName")
            .and_then(Value::as_str)
            .cmp(&right.get("inputName").and_then(Value::as_str))
    });
    Ok(ObsFacts {
        status: if optional_diagnostic.is_some() {
            "partial"
        } else {
            "ok"
        }
        .to_string(),
        version: Some(version),
        current_scene: scene,
        scene_sources,
        input_kinds,
        existing_inputs,
        default_settings,
        input_settings,
        device_choices,
        choice_status,
        diagnostic: optional_diagnostic,
    })
}

fn unavailable_obs_facts(diagnostic: String) -> ObsFacts {
    ObsFacts {
        status: "unavailable".to_string(),
        version: None,
        current_scene: None,
        scene_sources: None,
        input_kinds: Vec::new(),
        existing_inputs: Vec::new(),
        default_settings: None,
        input_settings: None,
        device_choices: None,
        choice_status: "unavailable".to_string(),
        diagnostic: Some(diagnostic),
    }
}

fn print_observation(observation: &Observation) {
    println!("{} on {}", observation.schema, observation.host);
    for source in &observation.sources {
        match &source.diagnostic {
            Some(diagnostic) => println!("{}: {} ({diagnostic})", source.source, source.status),
            None => println!("{}: {}", source.source, source.status),
        }
    }
    println!("PipeWire nodes: {}", observation.facts.pipewire.nodes.len());
    for node in &observation.facts.pipewire.nodes {
        println!(
            "  {} — {}",
            node.runtime_name,
            node.description.as_deref().unwrap_or("unnamed")
        );
    }
    println!("GoXLR mixers: {}", observation.facts.goxlr.mixers.len());
    println!(
        "OBS input kinds: {}",
        observation.facts.obs.input_kinds.len()
    );
    println!(
        "OBS device choices: {}",
        observation.facts.obs.choice_status
    );
}

fn load_config(explicit: Option<&Path>) -> Result<Config> {
    let path = explicit
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("GOXLR_NEXUS_CONFIG").map(PathBuf::from))
        .or_else(|| dirs::config_dir().map(|p| p.join("goxlr-nexus/config.toml")));

    let Some(path) = path else {
        return Ok(Config::default());
    };
    if !path.exists() {
        return Ok(Config::default());
    }

    let text = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("failed to parse config {}", path.display()))
}

fn processing_runtime_config(config: &Config) -> RuntimeConfig {
    RuntimeConfig {
        format: nexus_audio_processing::StreamFormat::default(),
        source: VirtualSourceConfig {
            capture_source: config.profile.default_source.clone(),
            render_target: config.profile.default_sink.clone(),
            source_name: config.processing.source_name.clone(),
            source_description: config.processing.source_description.clone(),
        },
        processing: ProcessingConfig {
            noise_suppression: config.processing.noise_suppression,
            echo_cancellation: config.processing.echo_cancellation,
            noise_level: config.processing.noise_level,
            echo_delay: config.processing.echo_delay,
        },
        state_path: Some(default_state_path()),
        control_socket: Some(default_control_socket()),
    }
}

fn processing_command(config: &Config, command: ProcessingCommand) -> Result<()> {
    let runtime = processing_runtime_config(config);
    if !config.processing.enable && !matches!(command, ProcessingCommand::Status { .. }) {
        bail!("processing runtime is disabled in the GoXLR Nexus configuration")
    }
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
        ProcessingCommand::Status { json } => processing_status(config, &runtime, json),
        ProcessingCommand::Reset => {
            let client = ControlClient::new(default_control_socket());
            match client.reset() {
                Ok(status) => {
                    print_processing_status(&status, false);
                    reconcile_processing_source(&status);
                }
                Err(_) => {
                    let store = StateStore::new(default_state_path());
                    let state = store.reset(&runtime.processing)?;
                    let status = offline_status(
                        &runtime.source.source_name,
                        &runtime.source.capture_source,
                        &runtime.source.render_target,
                        runtime.format,
                        &state,
                    );
                    print_processing_status(&status, false);
                    reconcile_processing_source(&status);
                }
            }
            Ok(())
        }
        ProcessingCommand::Daemon => {
            let runtime = ProcessingRuntime::new(runtime)?;
            runtime.run().map_err(|error| anyhow!(error))
        }
        ProcessingCommand::FailOpen => run_or_print(
            false,
            "pactl",
            &["set-default-source", &runtime.source.capture_source],
        ),
    }
}

fn processing_toggle(
    _config: &Config,
    runtime: &RuntimeConfig,
    toggle: Toggle,
    enabled: bool,
) -> Result<()> {
    let client = ControlClient::new(default_control_socket());
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
            let store = StateStore::new(default_state_path());
            let state = store.set(toggle, enabled, &runtime.processing)?;
            let status = offline_status(
                &runtime.source.source_name,
                &runtime.source.capture_source,
                &runtime.source.render_target,
                runtime.format,
                &state,
            );
            eprintln!("warn: processing daemon unavailable ({error}); change saved for startup");
            print_processing_status(&status, false);
        }
    }
    Ok(())
}

fn processing_status(config: &Config, runtime: &RuntimeConfig, json_output: bool) -> Result<()> {
    let client = ControlClient::new(default_control_socket());
    let status = match client.status() {
        Ok(status) => status,
        Err(_) => {
            let state = StateStore::new(default_state_path()).load(&runtime.processing)?;
            offline_status(
                &runtime.source.source_name,
                &runtime.source.capture_source,
                &runtime.source.render_target,
                runtime.format,
                &state,
            )
        }
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print_processing_status(&status, true);
    }
    if !config.processing.enable {
        eprintln!("processing runtime disabled in configuration");
    }
    Ok(())
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
    if let Err(error) = run_or_print(false, "pactl", &["set-default-source", desired]) {
        eprintln!("warn: cannot select processing microphone {desired}: {error:#}");
    }
}

fn doctor(config: &Config) -> Result<()> {
    let snapshot = snapshot()?;
    let required = required_nodes(config, &snapshot)?;
    let status = snapshot
        .goxlr_status
        .as_ref()
        .map_err(|err| anyhow!("{err}"))?;
    validate_goxlr_status(config, status)?;

    for output in &required.outputs.output_sinks {
        println!("ok: output sink {}", output.name);
    }
    println!("ok: GoXLR default source {}", required.default_source.name);
    println!("ok: GoXLR monitor source {}", required.monitor_source.name);
    println!(
        "ok: GoXLR serial {}",
        config.goxlr_serial.as_deref().unwrap_or("not configured")
    );

    if config.obs.enable {
        match obs_ready(config) {
            Ok(()) => println!(
                "ok: OBS websocket reachable at {}:{}",
                config.obs.host, config.obs.port
            ),
            Err(err) => println!("warn: OBS degraded: {err:#}"),
        }
    }

    Ok(())
}

fn status(config: &Config, json: bool) -> Result<()> {
    let snapshot = snapshot()?;
    let required = required_nodes(config, &snapshot);
    let goxlr_status = snapshot
        .goxlr_status
        .as_ref()
        .map_err(|err| anyhow!("{err}"))
        .and_then(|status| validate_goxlr_status(config, status));
    let goxlr_ok = goxlr_status.is_ok();
    if json {
        let mut out = BTreeMap::new();
        out.insert(
            "jdsSink",
            config
                .jds_sink
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        out.insert(
            "fallbackSink",
            config
                .fallback_sink
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        out.insert(
            "thinkpadSink",
            config
                .thinkpad_sink
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        out.insert(
            "outputSinks",
            Value::Array(
                config
                    .output_sinks
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
        out.insert(
            "goxlrSerial",
            config
                .goxlr_serial
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        out.insert("doctorOk", Value::Bool(required.is_ok() && goxlr_ok));
        out.insert("goxlrOk", Value::Bool(goxlr_ok));
        out.insert("obsEnabled", Value::Bool(config.obs.enable));
        out.insert(
            "effectiveMicrophone",
            Value::String(effective_microphone_name(config)),
        );
        let processing = ControlClient::new(default_control_socket())
            .status()
            .ok()
            .or_else(|| {
                let runtime = processing_runtime_config(config);
                let state = StateStore::new(default_state_path())
                    .load(&runtime.processing)
                    .ok()?;
                Some(offline_status(
                    &runtime.source.source_name,
                    &runtime.source.capture_source,
                    &runtime.source.render_target,
                    runtime.format,
                    &state,
                ))
            });
        if let Some(processing) = processing {
            out.insert(
                "processing",
                serde_json::to_value(processing).expect("processing status is serializable"),
            );
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!(
        "JDS sink: {}",
        config.jds_sink.as_deref().unwrap_or("not configured")
    );
    println!(
        "Fallback sink: {}",
        config.fallback_sink.as_deref().unwrap_or("not configured")
    );
    println!(
        "ThinkPad TH4 sink: {}",
        config.thinkpad_sink.as_deref().unwrap_or("not configured")
    );
    println!("Selectable output sinks:");
    for sink in &config.output_sinks {
        println!("  {sink}");
    }
    println!(
        "GoXLR serial: {}",
        config.goxlr_serial.as_deref().unwrap_or("not configured")
    );
    println!(
        "Effective microphone: {}",
        effective_microphone_name(config)
    );
    println!("GoXLR channel sinks:");
    for name in [
        GOXLR_SYSTEM,
        GOXLR_CHAT,
        GOXLR_GAME,
        GOXLR_MUSIC,
        GOXLR_SAMPLE,
    ] {
        print_node(&snapshot, name);
    }
    println!("GoXLR sources:");
    for name in [GOXLR_CHAT_MIC, GOXLR_STREAM_MIX, GOXLR_SAMPLER] {
        print_node(&snapshot, name);
    }
    match required {
        Ok(_) if goxlr_ok => println!("doctor: ok"),
        Ok(_) => match goxlr_status {
            Ok(()) => println!("doctor: ok"),
            Err(err) => println!("doctor: failed: {err:#}"),
        },
        Err(err) => println!("doctor: failed: {err:#}"),
    }
    Ok(())
}

fn print_node(snapshot: &Snapshot, name: &str) {
    match snapshot.nodes.iter().find(|node| node.name == name) {
        Some(node) => println!(
            "  ok   {name} ({}, {})",
            node.description.as_deref().unwrap_or("unnamed"),
            node.media_class.as_deref().unwrap_or("unknown")
        ),
        None => println!("  miss {name}"),
    }
}

fn apply(config: &Config, dry_run: bool) -> Result<()> {
    let snapshot = snapshot()?;
    let current_sink = current_default_sink()?;
    let current_source = current_default_source()?;
    let (operations, _, _) =
        reconcile_operations(config, &snapshot, &current_sink, &current_source)?;
    execute_audio_operations(&operations, dry_run)
}

/// Compute the exact mutations that `apply` would perform without executing
/// any of them.  This is intentionally separate from the mutation helpers so
/// adding a plan cannot accidentally turn a read-only command into a write.
fn plan(config: &Config, json_output: bool) -> Result<()> {
    let snapshot = snapshot()?;
    let current = PlanCurrent {
        default_sink: current_default_sink()?,
        default_source: current_default_source()?,
    };
    let mut operations = Vec::new();
    let mut diagnostics = Vec::new();

    let (audio_operations, goxlr_status, audio_diagnostics) = reconcile_operations(
        config,
        &snapshot,
        &current.default_sink,
        &current.default_source,
    )?;
    diagnostics.extend(audio_diagnostics);
    operations.extend(audio_operations.iter().map(audio_operation_plan));

    let obs = if config.obs.enable {
        match plan_obs(config, &mut operations) {
            Ok(obs) => Some(obs),
            Err(error) => {
                diagnostics.push(format!("OBS plan unavailable: {error:#}"));
                Some(PlanObs {
                    status: "unavailable".to_string(),
                    scene: None,
                    choice_status: "unavailable".to_string(),
                    sources: Vec::new(),
                    diagnostic: Some(format!("{error:#}")),
                })
            }
        }
    } else {
        None
    };

    let plan = ReconciliationPlan {
        schema: PLAN_SCHEMA,
        producer: Producer {
            name: "goxlr-nexus",
            version: env!("CARGO_PKG_VERSION"),
        },
        read_only: true,
        requires_apply: !operations.is_empty(),
        current,
        goxlr_status,
        operations,
        obs,
        diagnostics,
    };

    if json_output {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    } else {
        print_plan(&plan);
    }
    Ok(())
}

fn reconcile_operations(
    config: &Config,
    snapshot: &Snapshot,
    current_sink: &str,
    current_source: &str,
) -> Result<(Vec<AudioOperation>, String, Vec<String>)> {
    match usable_goxlr_status(config, snapshot) {
        Ok(()) => Ok((
            ready_audio_operations(config, snapshot, current_sink, current_source)?,
            "ready".to_string(),
            Vec::new(),
        )),
        Err(error) => {
            let diagnostic = format!("GoXLR profile unavailable: {error:#}");
            let outputs = output_nodes(config, snapshot)?;
            let selected =
                selected_output_sink(&outputs, current_sink).unwrap_or(&outputs.fallback_sink);
            let operations = if selected.name != current_sink {
                vec![AudioOperation::SetDefaultSink(selected.name.clone())]
            } else {
                Vec::new()
            };
            Ok((operations, "fallback".to_string(), vec![diagnostic]))
        }
    }
}

fn ready_audio_operations(
    config: &Config,
    snapshot: &Snapshot,
    current_sink: &str,
    current_source: &str,
) -> Result<Vec<AudioOperation>> {
    let required = required_nodes(config, snapshot)?;
    let selected = selected_output_sink(&required.outputs, current_sink)
        .unwrap_or(&required.outputs.fallback_sink);
    let mut operations = Vec::new();

    if selected.name != current_sink {
        operations.push(AudioOperation::SetDefaultSink(selected.name.clone()));
    }
    if current_source != required.default_source.name {
        operations.push(AudioOperation::SetDefaultSource(
            required.default_source.name.clone(),
        ));
    }
    if let Some(maximum) = config.max_monitor_sink_volume {
        if !maximum.is_finite() || !(0.0..=1.0).contains(&maximum) {
            bail!("max-monitor-sink-volume must be finite and between 0 and 1");
        }
        if let Some(percent) = sink_volume_percent(&selected.name)? {
            let limit = (maximum * 100.0).round() as u32;
            if percent > limit {
                operations.push(AudioOperation::SetSinkVolume {
                    sink: selected.name.clone(),
                    percent: limit,
                });
            }
        }
    }

    let (connect, disconnect) = monitor_link_operations(snapshot, &required, selected)?;
    operations.extend(connect);
    operations.extend(disconnect);
    Ok(operations)
}

fn monitor_link_operations(
    snapshot: &Snapshot,
    required: &RequiredNodes,
    selected: &Node,
) -> Result<(Vec<AudioOperation>, Vec<AudioOperation>)> {
    let mut desired = Vec::new();
    for channel in ["FL", "FR"] {
        let source = find_port(snapshot, &required.monitor_source, "output", channel)?;
        let sink = find_port(snapshot, selected, "input", channel)?;
        desired.push((source, sink));
    }

    let mut connects = Vec::new();
    for (source, sink) in &desired {
        let present = snapshot.links.iter().any(|link| {
            link.output_node_id == source.node_id
                && link.output_port_id == source.id
                && link.input_node_id == sink.node_id
                && link.input_port_id == sink.id
                && link.state.as_deref() != Some("error")
        });
        if !present {
            connects.push(AudioOperation::Connect {
                source_node: required.monitor_source.name.clone(),
                source_port: source.name.clone(),
                sink_node: selected.name.clone(),
                sink_port: sink.name.clone(),
            });
        }
    }

    let managed_outputs = required
        .outputs
        .output_sinks
        .iter()
        .map(|node| (node.id, ()))
        .collect::<BTreeMap<_, _>>();
    let mut disconnects = Vec::new();
    let mut seen = BTreeMap::new();
    for link in &snapshot.links {
        if link.output_node_id != required.monitor_source.id
            || !managed_outputs.contains_key(&link.input_node_id)
        {
            continue;
        }
        let Some(source_port) = snapshot
            .ports
            .iter()
            .find(|port| port.id == link.output_port_id)
        else {
            continue;
        };
        let Some(sink_port) = snapshot
            .ports
            .iter()
            .find(|port| port.id == link.input_port_id)
        else {
            continue;
        };
        let is_desired = desired.iter().any(|(source, sink)| {
            source.id == link.output_port_id
                && sink.id == link.input_port_id
                && sink.node_id == link.input_node_id
        });
        if !is_desired {
            let key = (
                required.monitor_source.name.clone(),
                source_port.name.clone(),
                snapshot
                    .nodes
                    .iter()
                    .find(|node| node.id == link.input_node_id)
                    .map(|node| node.name.clone())
                    .unwrap_or_default(),
                sink_port.name.clone(),
            );
            if seen.insert(key.clone(), ()).is_none() {
                disconnects.push(AudioOperation::Disconnect {
                    source_node: key.0,
                    source_port: key.1,
                    sink_node: key.2,
                    sink_port: key.3,
                });
            }
        }
    }
    Ok((connects, disconnects))
}

fn find_port(snapshot: &Snapshot, node: &Node, direction: &str, channel: &str) -> Result<Port> {
    let matches = snapshot
        .ports
        .iter()
        .filter(|port| {
            port.node_id == node.id
                && port.direction == direction
                && (port.channel.as_deref() == Some(channel)
                    || port.name.to_ascii_uppercase().contains(channel))
        })
        .cloned()
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [port] => Ok(port.clone()),
        [] => bail!(
            "no unique {direction} {channel} port for PipeWire node {}",
            node.name
        ),
        _ => bail!(
            "ambiguous {direction} {channel} ports for PipeWire node {}",
            node.name
        ),
    }
}

fn audio_operation_plan(operation: &AudioOperation) -> PlanOperation {
    match operation {
        AudioOperation::SetDefaultSink(sink) => PlanOperation {
            action: "pactl.set-default-sink".to_string(),
            target: Some(sink.clone()),
            arguments: Vec::new(),
            reason: "current default sink is not one of the configured selectable outputs"
                .to_string(),
        },
        AudioOperation::SetDefaultSource(source) => PlanOperation {
            action: "pactl.set-default-source".to_string(),
            target: Some(source.clone()),
            arguments: Vec::new(),
            reason: "GoXLR profile requires its configured default source".to_string(),
        },
        AudioOperation::SetSinkVolume { sink, percent } => PlanOperation {
            action: "pactl.set-sink-volume".to_string(),
            target: Some(sink.clone()),
            arguments: vec![format!("{percent}%")],
            reason: "selected monitor sink exceeds the configured volume ceiling".to_string(),
        },
        AudioOperation::Connect {
            source_node,
            source_port,
            sink_node,
            sink_port,
        } => PlanOperation {
            action: "pw-link.connect".to_string(),
            target: Some(format!("{source_node}:{source_port}")),
            arguments: vec![format!("{sink_node}:{sink_port}")],
            reason: "route the GoXLR monitor mix to the selected output".to_string(),
        },
        AudioOperation::Disconnect {
            source_node,
            source_port,
            sink_node,
            sink_port,
        } => PlanOperation {
            action: "pw-link.disconnect".to_string(),
            target: Some(format!("{source_node}:{source_port}")),
            arguments: vec![format!("{sink_node}:{sink_port}")],
            reason: "remove an observed stale monitor link".to_string(),
        },
    }
}

fn execute_audio_operations(operations: &[AudioOperation], dry_run: bool) -> Result<()> {
    for operation in operations {
        match operation {
            AudioOperation::SetDefaultSink(sink) => {
                run_or_print(dry_run, "pactl", &["set-default-sink", sink])?;
            }
            AudioOperation::SetDefaultSource(source) => {
                run_or_print(dry_run, "pactl", &["set-default-source", source])?;
            }
            AudioOperation::SetSinkVolume { sink, percent } => {
                let volume = format!("{percent}%");
                run_or_print(dry_run, "pactl", &["set-sink-volume", sink, &volume])?;
            }
            AudioOperation::Connect {
                source_node,
                source_port,
                sink_node,
                sink_port,
            } => {
                let source = format!("{source_node}:{source_port}");
                let sink = format!("{sink_node}:{sink_port}");
                run_or_print(dry_run, "pw-link", &[&source, &sink])?;
            }
            AudioOperation::Disconnect {
                source_node,
                source_port,
                sink_node,
                sink_port,
            } => {
                let source = format!("{source_node}:{source_port}");
                let sink = format!("{sink_node}:{sink_port}");
                run_or_print(dry_run, "pw-link", &["--disconnect", &source, &sink])?;
            }
        }
    }
    Ok(())
}

fn plan_obs(config: &Config, operations: &mut Vec<PlanOperation>) -> Result<PlanObs> {
    let facts = obs_facts(config)?;
    let scene = facts
        .current_scene
        .as_deref()
        .ok_or_else(|| anyhow!("OBS did not expose a current scene"))?;
    let scene_sources = facts
        .scene_sources
        .as_ref()
        .ok_or_else(|| anyhow!("OBS scene items were unavailable for the current scene"))?;
    let sources = obs_source_plan(config)
        .into_iter()
        .map(|source| {
            let exists = facts.existing_inputs.iter().any(|input| {
                input.get("inputName").and_then(Value::as_str) == Some(source.name.as_str())
            });
            let in_scene = scene_sources.iter().any(|name| name == &source.name);
            let action = if exists {
                "obs.set-input-settings"
            } else {
                "obs.create-input-and-scene-item"
            };
            if exists {
                operations.push(PlanOperation {
                    action: action.to_string(),
                    target: Some(source.name.clone()),
                    arguments: vec![source.device_id.clone()],
                    reason: "synchronize the configured OBS source device".to_string(),
                });
                if !in_scene {
                    operations.push(PlanOperation {
                        action: "obs.create-scene-item".to_string(),
                        target: Some(source.name.clone()),
                        arguments: vec![scene.to_string()],
                        reason: "add the existing configured source to the current scene"
                            .to_string(),
                    });
                }
            } else {
                operations.push(PlanOperation {
                    action: action.to_string(),
                    target: Some(source.name.clone()),
                    arguments: vec![
                        scene.to_string(),
                        source.input_kind.clone(),
                        source.device_id.clone(),
                    ],
                    reason: "create the configured OBS source in the current scene".to_string(),
                });
            }
            PlanObsSource {
                name: source.name,
                device_id: source.device_id,
                action: action.to_string(),
            }
        })
        .collect();
    Ok(PlanObs {
        status: facts.status,
        scene: facts.current_scene,
        choice_status: facts.choice_status,
        sources,
        diagnostic: facts.diagnostic,
    })
}

fn print_plan(plan: &ReconciliationPlan) {
    println!("{} on {}", plan.schema, plan.producer.name);
    println!("GoXLR status: {}", plan.goxlr_status);
    println!(
        "Current defaults: sink={} source={}",
        plan.current.default_sink, plan.current.default_source
    );
    for operation in &plan.operations {
        let target = operation.target.as_deref().unwrap_or("-");
        println!("  {} {} ({})", operation.action, target, operation.reason);
    }
    if let Some(obs) = &plan.obs {
        println!("OBS: {} (choices: {})", obs.status, obs.choice_status);
        for source in &obs.sources {
            println!(
                "  {} {} -> {}",
                source.action, source.name, source.device_id
            );
        }
    }
    for diagnostic in &plan.diagnostics {
        println!("diagnostic: {diagnostic}");
    }
    println!(
        "read-only: {} (apply required: {})",
        plan.read_only, plan.requires_apply
    );
}

fn follow(config: &Config, observe_only: bool) -> Result<()> {
    let observe_only = observe_only || config.observe_only;
    loop {
        if let Err(err) = follow_once(config, observe_only) {
            eprintln!("warn: GoXLR Nexus watcher unavailable: {err:#}; retrying in 5s");
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn follow_once(config: &Config, observe_only: bool) -> Result<()> {
    if let Err(err) = reconcile_for_follow(config, observe_only) {
        eprintln!("warn: initial GoXLR Nexus sync failed: {err:#}");
    }

    let mut child = audio_command("pactl")
        .args(["subscribe"])
        .stdout(Stdio::piped())
        .spawn()
        .context("failed to run pactl subscribe")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("pactl subscribe did not expose stdout"))?;

    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || read_subscription_events(stdout, sender));

    let debounce = Duration::from_millis(250);
    let maximum_batch = Duration::from_secs(1);
    let periodic_resync = Duration::from_secs(30);
    let mut pending_since: Option<Instant> = None;
    let mut last_event: Option<Instant> = None;
    let mut next_resync = Instant::now() + periodic_resync;

    loop {
        let now = Instant::now();
        let mut timeout = next_resync.saturating_duration_since(now);
        if let Some(started) = pending_since {
            let trailing = last_event
                .unwrap_or(started)
                .checked_add(debounce)
                .unwrap_or(now)
                .saturating_duration_since(now);
            let maximum = started
                .checked_add(maximum_batch)
                .unwrap_or(now)
                .saturating_duration_since(now);
            timeout = timeout.min(trailing).min(maximum);
        }

        match receiver.recv_timeout(timeout) {
            Ok(_) => {
                let now = Instant::now();
                pending_since.get_or_insert(now);
                last_event = Some(now);
            }
            Err(RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                let batch_due = pending_since.is_some_and(|started| {
                    now.duration_since(last_event.unwrap_or(started)) >= debounce
                        || now.duration_since(started) >= maximum_batch
                });
                let periodic_due = now >= next_resync;
                if batch_due || periodic_due {
                    if let Err(err) = reconcile_for_follow(config, observe_only) {
                        eprintln!("warn: GoXLR Nexus sync failed after coalesced event: {err:#}");
                    }
                    pending_since = None;
                    last_event = None;
                    next_resync = now + periodic_resync;
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let status = child.wait().context("failed to wait for pactl subscribe")?;
    if !status.success() {
        bail!("pactl subscribe exited with status {status}");
    }
    Ok(())
}

fn reconcile_for_follow(config: &Config, observe_only: bool) -> Result<()> {
    let snapshot = snapshot()?;
    let current_sink = current_default_sink()?;
    let current_source = current_default_source()?;
    let (operations, status, diagnostics) =
        reconcile_operations(config, &snapshot, &current_sink, &current_source)?;
    if let Err(error) = write_runtime_status(
        if observe_only {
            "observe-only"
        } else {
            "active"
        },
        &status,
        operations.len(),
        &diagnostics,
    ) {
        eprintln!("warn: failed to write GoXLR Nexus runtime status: {error:#}");
    }
    for diagnostic in &diagnostics {
        eprintln!("warn: {diagnostic}");
    }
    if observe_only {
        if !operations.is_empty() {
            eprintln!(
                "observe-only: {status} routing drift has {} proposed operation(s)",
                operations.len()
            );
        }
        return Ok(());
    }
    execute_audio_operations(&operations, false)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeStatus {
    schema: &'static str,
    mode: &'static str,
    profile: String,
    proposed_operations: usize,
    diagnostics: Vec<String>,
    updated_at: u64,
}

fn write_runtime_status(
    mode: &'static str,
    profile: &str,
    proposed_operations: usize,
    diagnostics: &[String],
) -> Result<()> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/user/1000"));
    let directory = runtime_dir.join("goxlr-nexus");
    fs::create_dir_all(&directory)
        .with_context(|| format!("create runtime status directory {}", directory.display()))?;
    let status = RuntimeStatus {
        schema: "goxlr-nexus.runtime/v1",
        mode,
        profile: profile.to_string(),
        proposed_operations,
        diagnostics: diagnostics.to_vec(),
        updated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or_default(),
    };
    let temporary = directory.join("status.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(&status)?)
        .with_context(|| format!("write runtime status {}", temporary.display()))?;
    fs::rename(&temporary, directory.join("status.json"))
        .context("atomically publish GoXLR Nexus runtime status")?;
    Ok(())
}

fn read_subscription_events(
    stdout: impl std::io::Read + Send + 'static,
    sender: SyncSender<String>,
) {
    for line in BufReader::new(stdout).lines() {
        match line {
            Ok(line) if relevant_subscription_event(&line) => {
                let _ = sender.try_send(line);
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("warn: failed to read pactl subscribe event: {error}");
                break;
            }
        }
    }
}

fn relevant_subscription_event(line: &str) -> bool {
    [" on sink ", " on source ", " on server ", " on card "]
        .iter()
        .any(|marker| line.contains(marker))
}

fn usable_goxlr_status(config: &Config, snapshot: &Snapshot) -> Result<()> {
    let status = snapshot
        .goxlr_status
        .as_ref()
        .map_err(|err| anyhow!("{err}"))?;
    validate_goxlr_status(config, status)
}

fn selected_output_sink<'a>(outputs: &'a OutputNodes, selected_name: &str) -> Option<&'a Node> {
    outputs
        .output_sinks
        .iter()
        .find(|node| node.name == selected_name)
}

fn apply_profile(config: &Config, profile: Profile, dry_run: bool) -> Result<()> {
    match profile {
        Profile::Stream => apply(config, dry_run),
        Profile::Desktop => {
            let snapshot = snapshot()?;
            let jds_name = config
                .jds_sink
                .as_deref()
                .ok_or_else(|| anyhow!("no JDS sink is configured"))?;
            let jds = find_node(&snapshot, jds_name)?;
            run_or_print(dry_run, "pactl", &["set-default-sink", &jds.name])?;
            let source = effective_microphone_node(config, &snapshot)?;
            run_or_print(dry_run, "pactl", &["set-default-source", &source.name])?;
            Ok(())
        }
        Profile::Off => {
            let snapshot = snapshot()?;
            let jds_name = config
                .jds_sink
                .as_deref()
                .ok_or_else(|| anyhow!("no JDS sink is configured"))?;
            let jds = find_node(&snapshot, jds_name)?;
            run_or_print(dry_run, "pactl", &["set-default-sink", &jds.name])
        }
    }
}

fn obs_sync(config: &Config, dry_run: bool) -> Result<()> {
    if !config.obs.enable {
        bail!("OBS integration is disabled; set programs.goxlr-nexus.obs.enable = true");
    }
    let sources = obs_source_plan(config);

    if dry_run {
        println!(
            "would sync {} OBS sources via {}:{}",
            sources.len(),
            config.obs.host,
            config.obs.port
        );
        for source in sources {
            println!("  {}: {}", source.name, source.device_id);
        }
        return Ok(());
    }

    let mut client = ObsClient::connect(config)?;
    let scene = client.target_scene()?;
    let existing_inputs = client.input_names()?;
    let scene_items = client.scene_source_names(&scene)?;
    for source in sources {
        if !existing_inputs.iter().any(|name| name == &source.name) {
            client.create_input(&scene, &source)?;
            println!("created OBS source {} in scene {scene}", source.name);
        } else {
            client.set_input_settings(&source)?;
            if !scene_items.iter().any(|name| name == &source.name) {
                client.create_scene_item(&scene, &source.name)?;
                println!("added existing OBS source {} to scene {scene}", source.name);
            } else {
                println!("updated OBS source {}", source.name);
            }
        }
    }
    Ok(())
}

fn obs_ready(config: &Config) -> Result<()> {
    let addr = (config.obs.host.as_str(), config.obs.port)
        .to_socket_addrs()
        .context("failed to resolve OBS websocket address")?
        .next()
        .ok_or_else(|| anyhow!("OBS websocket address did not resolve"))?;
    TcpStream::connect_timeout(&addr, Duration::from_millis(300))
        .with_context(|| format!("OBS websocket is not reachable at {addr}"))?;
    Ok(())
}

type ObsSocket = WebSocket<MaybeTlsStream<TcpStream>>;

struct ObsClient {
    socket: ObsSocket,
    next_request_id: u64,
}

impl ObsClient {
    fn connect(config: &Config) -> Result<Self> {
        let url = format!("ws://{}:{}", config.obs.host, config.obs.port);
        let (mut socket, _) = connect(&url)
            .with_context(|| format!("failed to connect to OBS websocket at {url}"))?;
        let hello = read_obs_message(&mut socket, 0).context("OBS did not send a Hello message")?;
        let hello_data: ObsHello = serde_json::from_value(
            hello
                .get("d")
                .cloned()
                .ok_or_else(|| anyhow!("OBS Hello did not include data"))?,
        )
        .context("failed to parse OBS Hello")?;

        let mut identify = json!({"rpcVersion": 1});
        if let Some(auth) = hello_data.authentication {
            let password = read_obs_password(config)?;
            identify["authentication"] =
                Value::String(obs_authentication(&password, &auth.salt, &auth.challenge));
        }
        write_obs_message(&mut socket, 1, identify)?;
        read_obs_message(&mut socket, 2).context("OBS did not accept Identify")?;
        Ok(Self {
            socket,
            next_request_id: 1,
        })
    }

    fn request(&mut self, request_type: &str, request_data: Value) -> Result<Value> {
        let request_id = self.next_request_id.to_string();
        self.next_request_id += 1;
        write_obs_message(
            &mut self.socket,
            6,
            json!({
                "requestType": request_type,
                "requestId": request_id,
                "requestData": request_data,
            }),
        )?;

        loop {
            let response = read_obs_message(&mut self.socket, 7)?;
            let data = response
                .get("d")
                .and_then(Value::as_object)
                .ok_or_else(|| anyhow!("OBS RequestResponse did not include an object payload"))?;
            if data.get("requestId").and_then(Value::as_str) != Some(request_id.as_str()) {
                continue;
            }
            let status: ObsRequestStatus = serde_json::from_value(
                data.get("requestStatus")
                    .cloned()
                    .ok_or_else(|| anyhow!("OBS response missing requestStatus"))?,
            )
            .context("failed to parse OBS requestStatus")?;
            if !status.result {
                bail!(
                    "OBS request {request_type} failed: code={:?} comment={}",
                    status.code,
                    status.comment.unwrap_or_else(|| "none".to_string())
                );
            }
            return Ok(data.get("responseData").cloned().unwrap_or(Value::Null));
        }
    }

    fn target_scene(&mut self) -> Result<String> {
        let response = self.request("GetSceneList", json!({}))?;
        if let Some(scene) = response
            .get("currentProgramSceneName")
            .and_then(Value::as_str)
        {
            return Ok(scene.to_string());
        }
        response
            .get("scenes")
            .and_then(Value::as_array)
            .and_then(|scenes| scenes.first())
            .and_then(|scene| scene.get("sceneName"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| anyhow!("OBS returned no scenes to sync into"))
    }

    fn input_names(&mut self) -> Result<Vec<String>> {
        let response = self.request("GetInputList", json!({}))?;
        Ok(response
            .get("inputs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|input| input.get("inputName").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect())
    }

    fn scene_source_names(&mut self, scene: &str) -> Result<Vec<String>> {
        let response = self.request("GetSceneItemList", json!({"sceneName": scene}))?;
        Ok(response
            .get("sceneItems")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("sourceName").and_then(Value::as_str))
            .map(ToOwned::to_owned)
            .collect())
    }

    fn create_input(&mut self, scene: &str, source: &ObsSourcePlan) -> Result<()> {
        self.request(
            "CreateInput",
            json!({
                "sceneName": scene,
                "inputName": source.name,
                "inputKind": source.input_kind,
                "inputSettings": obs_input_settings(source),
                "sceneItemEnabled": true,
            }),
        )?;
        Ok(())
    }

    fn set_input_settings(&mut self, source: &ObsSourcePlan) -> Result<()> {
        self.request(
            "SetInputSettings",
            json!({
                "inputName": source.name,
                "inputSettings": obs_input_settings(source),
                "overlay": true,
            }),
        )?;
        Ok(())
    }

    fn create_scene_item(&mut self, scene: &str, source_name: &str) -> Result<()> {
        self.request(
            "CreateSceneItem",
            json!({
                "sceneName": scene,
                "sourceName": source_name,
                "sceneItemEnabled": true,
            }),
        )?;
        Ok(())
    }
}

fn read_obs_message(socket: &mut ObsSocket, expected_op: u64) -> Result<Value> {
    loop {
        let message = socket
            .read()
            .context("failed to read OBS websocket message")?;
        let text = match message {
            Message::Text(text) => text,
            Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
                .context("OBS sent non-UTF-8 binary websocket payload")?
                .into(),
            Message::Ping(_) | Message::Pong(_) => continue,
            Message::Close(frame) => {
                bail!("OBS websocket closed before op {expected_op}: {frame:?}")
            }
            Message::Frame(_) => continue,
        };
        let value: Value =
            serde_json::from_str(&text).context("failed to parse OBS websocket JSON")?;
        if value.get("op").and_then(Value::as_u64) == Some(expected_op) {
            return Ok(value);
        }
    }
}

fn write_obs_message(socket: &mut ObsSocket, op: u64, data: Value) -> Result<()> {
    let text = serde_json::to_string(&json!({"op": op, "d": data}))
        .context("failed to encode OBS websocket message")?;
    socket
        .send(Message::Text(text.into()))
        .context("failed to write OBS websocket message")
}

fn read_obs_password(config: &Config) -> Result<String> {
    let Some(path) = &config.obs.password_file else {
        bail!("OBS websocket requires authentication, but no password-file is configured");
    };
    fs::read_to_string(path)
        .with_context(|| format!("failed to read OBS password file {}", path.display()))
        .map(|text| text.trim().to_string())
}

fn obs_authentication(password: &str, salt: &str, challenge: &str) -> String {
    let secret = sha256_base64(format!("{password}{salt}").as_bytes());
    sha256_base64(format!("{secret}{challenge}").as_bytes())
}

fn sha256_base64(bytes: &[u8]) -> String {
    BASE64.encode(Sha256::digest(bytes))
}

fn obs_source_plan(config: &Config) -> Vec<ObsSourcePlan> {
    config
        .obs
        .sources
        .iter()
        .filter(|source| !source.name.is_empty() && !source.device_id.is_empty())
        .map(|source| ObsSourcePlan {
            name: source.name.clone(),
            input_kind: OBS_INPUT_KIND.to_string(),
            device_id: if source.name == "GoXLR Mic" {
                effective_microphone_name(config)
            } else {
                source.device_id.clone()
            },
        })
        .collect()
}

fn obs_input_settings(source: &ObsSourcePlan) -> Value {
    json!({"device_id": source.device_id})
}

fn default_obs_sources() -> Vec<ObsSourceConfig> {
    [
        ("GoXLR Mic", GOXLR_CHAT_MIC),
        ("GoXLR Stream Mix", GOXLR_STREAM_MIX),
        ("GoXLR Sampler", GOXLR_SAMPLER),
        ("GoXLR Desktop Mix", GOXLR_STREAM_MIX),
    ]
    .into_iter()
    .map(|(name, device_id)| ObsSourceConfig {
        name: name.to_string(),
        device_id: device_id.to_string(),
    })
    .collect()
}

fn snapshot() -> Result<Snapshot> {
    let graph = pipewire_graph()?;
    Ok(Snapshot {
        nodes: graph.nodes,
        ports: graph.ports,
        links: graph.links,
        goxlr_status: goxlr_status().map_err(|err| format!("{err:#}")),
    })
}

fn pipewire_nodes() -> Result<Vec<Node>> {
    Ok(pipewire_graph()?.nodes)
}

fn pipewire_graph() -> Result<PipewireGraph> {
    let output = audio_command("pw-dump")
        .output()
        .context("failed to run pw-dump")?;
    if !output.status.success() {
        bail!(
            "pw-dump failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_pw_graph(&output.stdout)
}

fn parse_pw_graph(bytes: &[u8]) -> Result<PipewireGraph> {
    let value: Value = serde_json::from_slice(bytes).context("failed to parse pw-dump JSON")?;
    let array = value
        .as_array()
        .ok_or_else(|| anyhow!("pw-dump JSON root was not an array"))?;
    let mut graph = PipewireGraph::default();
    for item in array {
        let object_type = item.get("type").and_then(Value::as_str);
        match object_type {
            Some("PipeWire:Interface:Node") => {
                let Some(props) = item.pointer("/info/props").and_then(Value::as_object) else {
                    continue;
                };
                let Some(name) = props.get("node.name").and_then(Value::as_str) else {
                    continue;
                };
                let properties = props
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect();
                graph.nodes.push(Node {
                    id: item.get("id").and_then(Value::as_u64).unwrap_or_default() as u32,
                    name: name.to_string(),
                    description: props
                        .get("node.description")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    media_class: props
                        .get("media.class")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    properties,
                });
            }
            Some("PipeWire:Interface:Port") => {
                let Some(info) = item.get("info") else {
                    continue;
                };
                let Some(props) = info.get("props").and_then(Value::as_object) else {
                    continue;
                };
                let Some(node_id) = props.get("node.id").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(name) = props
                    .get("port.name")
                    .or_else(|| props.get("port.alias"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                graph.ports.push(Port {
                    id: item.get("id").and_then(Value::as_u64).unwrap_or_default() as u32,
                    node_id: node_id as u32,
                    direction: info
                        .get("direction")
                        .and_then(Value::as_str)
                        .or_else(|| props.get("port.direction").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string(),
                    name: name.to_string(),
                    channel: props
                        .get("audio.channel")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            Some("PipeWire:Interface:Link") => {
                let Some(info) = item.get("info") else {
                    continue;
                };
                let Some(output_node_id) = info.get("output-node-id").and_then(Value::as_u64)
                else {
                    continue;
                };
                let Some(output_port_id) = info.get("output-port-id").and_then(Value::as_u64)
                else {
                    continue;
                };
                let Some(input_node_id) = info.get("input-node-id").and_then(Value::as_u64) else {
                    continue;
                };
                let Some(input_port_id) = info.get("input-port-id").and_then(Value::as_u64) else {
                    continue;
                };
                graph.links.push(Link {
                    id: item.get("id").and_then(Value::as_u64).unwrap_or_default() as u32,
                    output_node_id: output_node_id as u32,
                    output_port_id: output_port_id as u32,
                    input_node_id: input_node_id as u32,
                    input_port_id: input_port_id as u32,
                    state: info
                        .get("state")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                });
            }
            _ => {}
        }
    }
    Ok(graph)
}

fn goxlr_status() -> Result<Value> {
    let output = audio_command("goxlr-client")
        .args(["--status-json"])
        .output()
        .context("failed to run goxlr-client --status-json")?;
    if !output.status.success() {
        bail!(
            "goxlr-client --status-json failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout).context("failed to parse goxlr-client JSON")
}

fn validate_goxlr_status(config: &Config, status: &Value) -> Result<()> {
    let Some(serial) = config.goxlr_serial.as_deref() else {
        bail!("no GoXLR serial is configured");
    };
    let mixers = status
        .get("mixers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("goxlr-client JSON has no mixers object"))?;
    let mixer = mixers.get(serial).ok_or_else(|| {
        anyhow!(
            "GoXLR serial {} is missing from goxlr-client status; available serials: {}",
            serial,
            mixers.keys().cloned().collect::<Vec<_>>().join(", ")
        )
    })?;
    let device_type = mixer
        .pointer("/hardware/device_type")
        .and_then(Value::as_str);
    if device_type != Some("Full") {
        bail!("configured GoXLR is not a full GoXLR: device_type={device_type:?}");
    }
    Ok(())
}

fn effective_microphone_name(config: &Config) -> String {
    if !config.processing.enable {
        return config.profile.default_source.clone();
    }
    let client = ControlClient::new(default_control_socket());
    match client.status() {
        Ok(status) if status.daemon_running && status.healthy && status.active => {
            status.processed_source
        }
        _ => config.profile.default_source.clone(),
    }
}

fn effective_microphone_node(config: &Config, snapshot: &Snapshot) -> Result<Node> {
    let name = effective_microphone_name(config);
    find_node(snapshot, &name).or_else(|error| {
        if name != config.profile.default_source {
            eprintln!("warn: processed microphone is unavailable ({error:#}); using raw GoXLR mic");
        }
        find_node(snapshot, &config.profile.default_source)
    })
}

fn required_nodes(config: &Config, snapshot: &Snapshot) -> Result<RequiredNodes> {
    Ok(RequiredNodes {
        outputs: output_nodes(config, snapshot)?,
        default_source: effective_microphone_node(config, snapshot)?,
        monitor_source: find_node(snapshot, &config.profile.monitor_source)?,
    })
}

fn output_nodes(config: &Config, snapshot: &Snapshot) -> Result<OutputNodes> {
    let fallback_name = config
        .fallback_sink
        .as_deref()
        .or(config.jds_sink.as_deref());
    let configured_fallback = fallback_name.and_then(|name| find_node(snapshot, name).ok());
    let mut output_sinks = Vec::new();
    for name in &config.output_sinks {
        match find_node(snapshot, name) {
            Ok(node) => output_sinks.push(node),
            Err(err) => eprintln!("warn: configured output sink {name} is unavailable: {err:#}"),
        }
    }
    if let Some(fallback_sink) = &configured_fallback
        && !output_sinks
            .iter()
            .any(|node| node.name == fallback_sink.name)
    {
        output_sinks.push(fallback_sink.clone());
    }

    let fallback_sink = configured_fallback
        .or_else(|| output_sinks.first().cloned())
        .ok_or_else(|| anyhow!("none of the configured output sinks is currently present"))?;

    Ok(OutputNodes {
        output_sinks,
        fallback_sink,
    })
}

fn find_node(snapshot: &Snapshot, name: &str) -> Result<Node> {
    snapshot
        .nodes
        .iter()
        .find(|node| node.name == name)
        .cloned()
        .ok_or_else(|| {
            let relevant = snapshot
                .nodes
                .iter()
                .filter(|node| {
                    node.name.contains("GoXLR")
                        || node.name.contains("JDS")
                        || node.name.contains("ThinkPad")
                        || node.name.contains("Lenovo")
                })
                .map(|node| node.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!("required PipeWire node {name} is missing; observed relevant nodes: {relevant}")
        })
}

fn current_default_sink() -> Result<String> {
    current_default_audio("sink")
}

fn current_default_source() -> Result<String> {
    current_default_audio("source")
}

fn current_default_audio(kind: &str) -> Result<String> {
    let action = format!("get-default-{kind}");
    let output = audio_command("pactl")
        .arg(&action)
        .output()
        .with_context(|| format!("failed to run pactl get-default-{kind}"))?;
    if !output.status.success() {
        bail!(
            "pactl get-default-{kind} failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn sink_volume_percent(name: &str) -> Result<Option<u32>> {
    let output = audio_command("pactl")
        .args(["--format=json", "list", "sinks"])
        .output()
        .context("failed to inspect sink volumes")?;
    if !output.status.success() {
        bail!(
            "pactl list sinks failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let sinks: Value =
        serde_json::from_slice(&output.stdout).context("failed to parse pactl JSON sink list")?;
    let Some(sink) = sinks.as_array().and_then(|items| {
        items
            .iter()
            .find(|item| item.get("name").and_then(Value::as_str) == Some(name))
    }) else {
        return Ok(None);
    };
    let Some(volumes) = sink.get("volume").and_then(Value::as_object) else {
        return Ok(None);
    };
    let percent = volumes
        .values()
        .filter_map(|volume| volume.get("value_percent").and_then(Value::as_str))
        .filter_map(|value| value.trim_end_matches('%').parse::<u32>().ok())
        .max();
    Ok(percent)
}

fn run_or_print(dry_run: bool, program: &str, args: &[&str]) -> Result<()> {
    if dry_run {
        println!("would run: {} {}", program, args.join(" "));
        return Ok(());
    }
    let status = audio_command(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} {} failed with status {status}", args.join(" "));
    }
    Ok(())
}

fn audio_command(program: &str) -> Command {
    let mut command = Command::new(program);
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() && Path::new("/run/user/1000").exists() {
        command.env("XDG_RUNTIME_DIR", "/run/user/1000");
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pipewire_nodes() {
        let json = br#"[
          {"type":"PipeWire:Interface:Node","info":{"props":{
            "node.name":"alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink",
            "node.description":"GoXLR System",
            "media.class":"Audio/Sink"
          }}},
          {"type":"PipeWire:Interface:Client","info":{"props":{"node.name":"ignored"}}}
        ]"#;
        let nodes = parse_pw_graph(json).unwrap().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].description.as_deref(), Some("GoXLR System"));
        assert_eq!(
            nodes[0].properties.get("node.name").and_then(Value::as_str),
            Some("alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink")
        );
    }

    #[test]
    fn parses_pipewire_ports_and_links() {
        let json = br#"[
          {"id": 10, "type":"PipeWire:Interface:Node","info":{"props":{"node.name":"source","media.class":"Audio/Source"}}},
          {"id": 20, "type":"PipeWire:Interface:Node","info":{"props":{"node.name":"sink","media.class":"Audio/Sink"}}},
          {"id": 11, "type":"PipeWire:Interface:Port","info":{"direction":"output","props":{"node.id":10,"port.name":"capture_FL","audio.channel":"FL"}}},
          {"id": 21, "type":"PipeWire:Interface:Port","info":{"direction":"input","props":{"node.id":20,"port.name":"playback_FL","audio.channel":"FL"}}},
          {"id": 30, "type":"PipeWire:Interface:Link","info":{"output-node-id":10,"output-port-id":11,"input-node-id":20,"input-port-id":21,"state":"active"}}
        ]"#;
        let graph = parse_pw_graph(json).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert_eq!(graph.ports.len(), 2);
        assert_eq!(graph.ports[0].channel.as_deref(), Some("FL"));
        assert_eq!(graph.links.len(), 1);
        assert_eq!(graph.links[0].output_port_id, 11);
    }

    #[test]
    fn validates_configured_serial() {
        let status: Value = serde_json::json!({
            "mixers": {
                "S200805412CQK": {
                    "hardware": {
                        "device_type": "Full"
                    }
                }
            }
        });
        let config = Config {
            goxlr_serial: Some("S200805412CQK".to_string()),
            ..Config::default()
        };
        validate_goxlr_status(&config, &status).unwrap();
    }

    #[test]
    fn rejects_missing_serial() {
        let status: Value = serde_json::json!({"mixers": {}});
        let config = Config {
            goxlr_serial: Some("S200805412CQK".to_string()),
            ..Config::default()
        };
        let err = validate_goxlr_status(&config, &status).unwrap_err();
        assert!(err.to_string().contains("S200805412CQK"));
    }

    #[test]
    fn goxlr_status_error_is_not_usable() {
        let snapshot = Snapshot {
            nodes: vec![],
            ports: vec![],
            links: vec![],
            goxlr_status: Err("goxlr-client --status-json failed".to_string()),
        };
        let err = usable_goxlr_status(&Config::default(), &snapshot).unwrap_err();
        assert!(err.to_string().contains("goxlr-client"));
    }

    #[test]
    fn parses_configured_output_sinks() {
        let config: Config = toml::from_str(
            r#"
            output-sinks = [
              "alsa_output.example_jds",
              "alsa_output.example_thinkpad",
            ]
            "#,
        )
        .unwrap();
        assert_eq!(
            config.output_sinks,
            vec!["alsa_output.example_jds", "alsa_output.example_thinkpad"]
        );
    }

    #[test]
    fn selected_supported_output_tracks_current_default_sink() {
        let required =
            output_nodes_for_outputs(["alsa_output.example_jds", "alsa_output.example_thinkpad"]);
        let selected = selected_output_sink(&required, "alsa_output.example_thinkpad").unwrap();
        assert_eq!(selected.name, "alsa_output.example_thinkpad");
    }

    #[test]
    fn unsupported_output_is_not_selected() {
        let required =
            output_nodes_for_outputs(["alsa_output.example_jds", "alsa_output.example_thinkpad"]);
        assert!(
            selected_output_sink(
                &required,
                "alsa_output.usb-Generic_USB_Audio-00.HiFi__Speaker__sink"
            )
            .is_none()
        );
    }

    #[test]
    fn subscription_events_are_relevant_without_reconciling_inline() {
        assert!(relevant_subscription_event("Event 'change' on sink #4"));
        assert!(relevant_subscription_event("Event 'new' on server #0"));
        assert!(relevant_subscription_event("Event 'remove' on card #2"));
        assert!(!relevant_subscription_event("Event 'change' on client #4"));
    }

    #[test]
    fn settled_monitor_graph_produces_no_link_operations() {
        let source = test_node_with_id(1, GOXLR_STREAM_MIX, "Audio/Source");
        let output = test_node_with_id(2, "alsa_output.example_thinkpad", "Audio/Sink");
        let required = RequiredNodes {
            outputs: OutputNodes {
                output_sinks: vec![output.clone()],
                fallback_sink: output.clone(),
            },
            default_source: test_node_with_id(3, GOXLR_CHAT_MIC, "Audio/Source"),
            monitor_source: source.clone(),
        };
        let snapshot = Snapshot {
            nodes: vec![source.clone(), output.clone()],
            ports: vec![
                test_port(11, 1, "output", "capture_FL", "FL"),
                test_port(12, 1, "output", "capture_FR", "FR"),
                test_port(21, 2, "input", "playback_FL", "FL"),
                test_port(22, 2, "input", "playback_FR", "FR"),
            ],
            links: vec![
                Link {
                    id: 31,
                    output_node_id: 1,
                    output_port_id: 11,
                    input_node_id: 2,
                    input_port_id: 21,
                    state: Some("active".to_string()),
                },
                Link {
                    id: 32,
                    output_node_id: 1,
                    output_port_id: 12,
                    input_node_id: 2,
                    input_port_id: 22,
                    state: Some("active".to_string()),
                },
            ],
            goxlr_status: Ok(Value::Null),
        };
        let (connect, disconnect) = monitor_link_operations(&snapshot, &required, &output).unwrap();
        assert!(connect.is_empty());
        assert!(disconnect.is_empty());
    }

    #[test]
    fn missing_monitor_channel_connects_only_that_channel() {
        let source = test_node_with_id(1, GOXLR_STREAM_MIX, "Audio/Source");
        let output = test_node_with_id(2, "alsa_output.example_thinkpad", "Audio/Sink");
        let required = RequiredNodes {
            outputs: OutputNodes {
                output_sinks: vec![output.clone()],
                fallback_sink: output.clone(),
            },
            default_source: test_node_with_id(3, GOXLR_CHAT_MIC, "Audio/Source"),
            monitor_source: source.clone(),
        };
        let snapshot = Snapshot {
            nodes: vec![source.clone(), output.clone()],
            ports: vec![
                test_port(11, 1, "output", "capture_FL", "FL"),
                test_port(12, 1, "output", "capture_FR", "FR"),
                test_port(21, 2, "input", "playback_FL", "FL"),
                test_port(22, 2, "input", "playback_FR", "FR"),
            ],
            links: vec![Link {
                id: 31,
                output_node_id: 1,
                output_port_id: 11,
                input_node_id: 2,
                input_port_id: 21,
                state: Some("active".to_string()),
            }],
            goxlr_status: Ok(Value::Null),
        };
        let (connect, disconnect) = monitor_link_operations(&snapshot, &required, &output).unwrap();
        assert_eq!(connect.len(), 1);
        assert!(matches!(
            &connect[0],
            AudioOperation::Connect { source_port, sink_port, .. }
                if source_port == "capture_FR" && sink_port == "playback_FR"
        ));
        assert!(disconnect.is_empty());
    }

    #[test]
    fn builds_default_obs_source_plan() {
        let mut config = Config::default();
        config.obs.enable = true;
        let plan = obs_source_plan(&config);
        assert_eq!(plan.len(), 4);
        assert_eq!(plan[0].name, "GoXLR Mic");
        assert_eq!(plan[0].input_kind, OBS_INPUT_KIND);
        assert_eq!(plan[0].device_id, GOXLR_CHAT_MIC);
        assert_eq!(plan[3].device_id, GOXLR_STREAM_MIX);
    }

    #[test]
    fn builds_obs_input_settings() {
        let source = ObsSourcePlan {
            name: "GoXLR Mic".to_string(),
            input_kind: OBS_INPUT_KIND.to_string(),
            device_id: GOXLR_CHAT_MIC.to_string(),
        };
        assert_eq!(
            obs_input_settings(&source),
            serde_json::json!({"device_id": GOXLR_CHAT_MIC})
        );
    }

    #[test]
    fn parses_obs_hello_without_authentication() {
        let hello: ObsHello = serde_json::from_value(serde_json::json!({
            "obsWebSocketVersion": "5.5.4",
            "rpcVersion": 1
        }))
        .unwrap();
        assert!(hello.authentication.is_none());
    }

    #[test]
    fn computes_obs_authentication_response() {
        assert_eq!(
            obs_authentication("password", "salt", "challenge"),
            "zTM5ki6L2vVvBQiTG9ckH1Lh64AbnCf6XZ226UmnkIA="
        );
    }

    fn output_nodes_for_outputs<const N: usize>(outputs: [&str; N]) -> OutputNodes {
        OutputNodes {
            output_sinks: outputs.into_iter().map(test_node).collect(),
            fallback_sink: test_node("alsa_output.example_jds"),
        }
    }

    fn test_node(name: &str) -> Node {
        test_node_with_id(0, name, "Audio/Sink")
    }

    fn test_node_with_id(id: u32, name: &str, media_class: &str) -> Node {
        Node {
            id,
            name: name.to_string(),
            description: None,
            media_class: Some(media_class.to_string()),
            properties: BTreeMap::new(),
        }
    }

    fn test_port(id: u32, node_id: u32, direction: &str, name: &str, channel: &str) -> Port {
        Port {
            id,
            node_id,
            direction: direction.to_string(),
            name: name.to_string(),
            channel: Some(channel.to_string()),
        }
    }
}
