use std::collections::BTreeMap;
use std::fs;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::Value;

const DEFAULT_JDS_SINK: &str =
    "alsa_output.usb-Yoyodyne_Consulting_JDS_Labs_Element_DAC-01.analog-stereo";
const DEFAULT_GOXLR_SERIAL: &str = "S200805412CQK";
const GOXLR_SYSTEM: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
const GOXLR_CHAT: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Headphones__sink";
const GOXLR_GAME: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line1__sink";
const GOXLR_MUSIC: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line2__sink";
const GOXLR_SAMPLE: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line3__sink";
const GOXLR_CHAT_MIC: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
const GOXLR_STREAM_MIX: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
const GOXLR_SAMPLER: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line5__source";

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
    Doctor,
    Status {
        #[arg(long)]
        json: bool,
    },
    Apply(ApplyArgs),
    Profile {
        profile: Profile,
        #[arg(long)]
        dry_run: bool,
    },
    Obs {
        #[command(subcommand)]
        command: ObsCommand,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct Config {
    user: String,
    jds_sink: String,
    goxlr_serial: String,
    profile: ProfileConfig,
    obs: ObsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            user: "can".to_string(),
            jds_sink: DEFAULT_JDS_SINK.to_string(),
            goxlr_serial: DEFAULT_GOXLR_SERIAL.to_string(),
            profile: ProfileConfig::default(),
            obs: ObsConfig::default(),
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
}

impl Default for ObsConfig {
    fn default() -> Self {
        Self {
            enable: false,
            host: "127.0.0.1".to_string(),
            port: 4455,
            password_file: None,
        }
    }
}

#[derive(Debug, Clone)]
struct Node {
    name: String,
    description: Option<String>,
    media_class: Option<String>,
}

#[derive(Debug)]
struct Snapshot {
    nodes: Vec<Node>,
    goxlr_status: Value,
}

#[derive(Debug)]
struct RequiredNodes {
    jds_sink: Node,
    default_sink: Node,
    default_source: Node,
    monitor_source: Node,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;

    match cli.command {
        CommandKind::Doctor => doctor(&config),
        CommandKind::Status { json } => status(&config, json),
        CommandKind::Apply(args) => apply(&config, args.dry_run),
        CommandKind::Profile { profile, dry_run } => apply_profile(&config, profile, dry_run),
        CommandKind::Obs {
            command: ObsCommand::Sync { dry_run },
        } => obs_sync(&config, dry_run),
    }
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

fn doctor(config: &Config) -> Result<()> {
    let snapshot = snapshot()?;
    let required = required_nodes(config, &snapshot)?;
    validate_goxlr_status(config, &snapshot.goxlr_status)?;

    println!("ok: JDS sink {}", required.jds_sink.name);
    println!("ok: GoXLR default sink {}", required.default_sink.name);
    println!("ok: GoXLR default source {}", required.default_source.name);
    println!("ok: GoXLR monitor source {}", required.monitor_source.name);
    println!("ok: GoXLR serial {}", config.goxlr_serial);

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
    if json {
        let mut out = BTreeMap::new();
        out.insert("jdsSink", Value::String(config.jds_sink.clone()));
        out.insert("goxlrSerial", Value::String(config.goxlr_serial.clone()));
        out.insert("doctorOk", Value::Bool(required.is_ok()));
        out.insert("obsEnabled", Value::Bool(config.obs.enable));
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }

    println!("JDS sink: {}", config.jds_sink);
    println!("GoXLR serial: {}", config.goxlr_serial);
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
        Ok(_) => println!("doctor: ok"),
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
    let required = required_nodes(config, &snapshot)?;
    validate_goxlr_status(config, &snapshot.goxlr_status)?;

    run_or_print(
        dry_run,
        "pactl",
        &["set-default-sink", &required.default_sink.name],
    )?;
    run_or_print(
        dry_run,
        "pactl",
        &["set-default-source", &required.default_source.name],
    )?;
    link_monitor_to_jds(config, &required, dry_run)?;
    Ok(())
}

fn apply_profile(config: &Config, profile: Profile, dry_run: bool) -> Result<()> {
    match profile {
        Profile::Stream => apply(config, dry_run),
        Profile::Desktop => {
            let snapshot = snapshot()?;
            let jds = find_node(&snapshot, &config.jds_sink)?;
            run_or_print(dry_run, "pactl", &["set-default-sink", &jds.name])?;
            run_or_print(
                dry_run,
                "pactl",
                &["set-default-source", &config.profile.default_source],
            )?;
            Ok(())
        }
        Profile::Off => {
            let snapshot = snapshot()?;
            let jds = find_node(&snapshot, &config.jds_sink)?;
            run_or_print(dry_run, "pactl", &["set-default-sink", &jds.name])
        }
    }
}

fn obs_sync(config: &Config, dry_run: bool) -> Result<()> {
    if !config.obs.enable {
        bail!("OBS integration is disabled; set programs.goxlr-nexus.obs.enable = true");
    }
    obs_ready(config)?;

    let password_state = if let Some(path) = &config.obs.password_file {
        if path.exists() {
            "password file present"
        } else {
            bail!("OBS password file {} is missing", path.display());
        }
    } else {
        "no password file configured"
    };

    let sources = [
        ("GoXLR Mic", GOXLR_CHAT_MIC),
        ("GoXLR Stream Mix", GOXLR_STREAM_MIX),
        ("GoXLR Sampler", GOXLR_SAMPLER),
        ("GoXLR Desktop Mix", GOXLR_STREAM_MIX),
    ];

    if dry_run {
        println!(
            "would sync {} OBS sources via {}:{} ({password_state})",
            sources.len(),
            config.obs.host,
            config.obs.port
        );
        for (label, node) in sources {
            println!("  {label}: {node}");
        }
        return Ok(());
    }

    bail!(
        "OBS websocket is reachable, but v1 only supports dry-run source planning; run `goxlr-nexus obs sync --dry-run`"
    );
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

fn snapshot() -> Result<Snapshot> {
    Ok(Snapshot {
        nodes: pipewire_nodes()?,
        goxlr_status: goxlr_status()?,
    })
}

fn pipewire_nodes() -> Result<Vec<Node>> {
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
    parse_pw_dump(&output.stdout)
}

fn parse_pw_dump(bytes: &[u8]) -> Result<Vec<Node>> {
    let value: Value = serde_json::from_slice(bytes).context("failed to parse pw-dump JSON")?;
    let array = value
        .as_array()
        .ok_or_else(|| anyhow!("pw-dump JSON root was not an array"))?;
    let mut nodes = Vec::new();
    for item in array {
        if item.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Node") {
            continue;
        }
        let Some(props) = item.pointer("/info/props").and_then(Value::as_object) else {
            continue;
        };
        let Some(name) = props.get("node.name").and_then(Value::as_str) else {
            continue;
        };
        nodes.push(Node {
            name: name.to_string(),
            description: props
                .get("node.description")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            media_class: props
                .get("media.class")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
        });
    }
    Ok(nodes)
}

fn goxlr_status() -> Result<Value> {
    let output = audio_command("goxlr-client")
        .args(["--status-json"])
        .output()
        .context("failed to run goxlr-client --status-json")?;
    if !output.status.success() {
        bail!(
            "goxlr-client --status-json failed with status {}",
            output.status
        );
    }
    serde_json::from_slice(&output.stdout).context("failed to parse goxlr-client JSON")
}

fn validate_goxlr_status(config: &Config, status: &Value) -> Result<()> {
    let mixers = status
        .get("mixers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("goxlr-client JSON has no mixers object"))?;
    let mixer = mixers.get(&config.goxlr_serial).ok_or_else(|| {
        anyhow!(
            "GoXLR serial {} is missing from goxlr-client status; available serials: {}",
            config.goxlr_serial,
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

fn required_nodes(config: &Config, snapshot: &Snapshot) -> Result<RequiredNodes> {
    Ok(RequiredNodes {
        jds_sink: find_node(snapshot, &config.jds_sink)?,
        default_sink: find_node(snapshot, &config.profile.default_sink)?,
        default_source: find_node(snapshot, &config.profile.default_source)?,
        monitor_source: find_node(snapshot, &config.profile.monitor_source)?,
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
                .filter(|node| node.name.contains("GoXLR") || node.name.contains("JDS"))
                .map(|node| node.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!("required PipeWire node {name} is missing; observed relevant nodes: {relevant}")
        })
}

fn link_monitor_to_jds(config: &Config, required: &RequiredNodes, dry_run: bool) -> Result<()> {
    for (source_port, sink_port) in [("capture_FL", "playback_FL"), ("capture_FR", "playback_FR")] {
        let source = format!("{}:{source_port}", required.monitor_source.name);
        let sink = format!("{}:{sink_port}", config.jds_sink);
        run_or_print(dry_run, "pw-link", &["--disconnect", &source, &sink]).ok();
        run_or_print(dry_run, "pw-link", &[&source, &sink])?;
    }
    Ok(())
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
        let nodes = parse_pw_dump(json).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].description.as_deref(), Some("GoXLR System"));
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
        validate_goxlr_status(&Config::default(), &status).unwrap();
    }

    #[test]
    fn rejects_missing_serial() {
        let status: Value = serde_json::json!({"mixers": {}});
        let err = validate_goxlr_status(&Config::default(), &status).unwrap_err();
        assert!(err.to_string().contains("S200805412CQK"));
    }
}
