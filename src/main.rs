use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket, connect};

const GOXLR_SYSTEM: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
const GOXLR_CHAT: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Headphones__sink";
const GOXLR_GAME: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line1__sink";
const GOXLR_MUSIC: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line2__sink";
const GOXLR_SAMPLE: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Line3__sink";
const GOXLR_CHAT_MIC: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
const GOXLR_STREAM_MIX: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
const GOXLR_SAMPLER: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line5__source";
const OBS_INPUT_KIND: &str = "pulse_input_capture";

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
    Follow,
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
    jds_sink: Option<String>,
    fallback_sink: Option<String>,
    thinkpad_sink: Option<String>,
    output_sinks: Vec<String>,
    goxlr_serial: Option<String>,
    profile: ProfileConfig,
    obs: ObsConfig,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct ObsSourceConfig {
    name: String,
    device_id: String,
}

impl Default for ObsSourceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            device_id: String::new(),
        }
    }
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
    name: String,
    description: Option<String>,
    media_class: Option<String>,
}

#[derive(Debug)]
struct Snapshot {
    nodes: Vec<Node>,
    goxlr_status: Result<Value, String>,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = load_config(cli.config.as_deref())?;

    match cli.command {
        CommandKind::Doctor => doctor(&config),
        CommandKind::Status { json } => status(&config, json),
        CommandKind::Apply(args) => apply(&config, args.dry_run),
        CommandKind::Follow => follow(&config),
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
    match usable_goxlr_status(config, &snapshot) {
        Ok(()) => apply_goxlr_profile(config, &snapshot, dry_run),
        Err(err) => apply_fallback_profile(config, &snapshot, dry_run, &err),
    }
}

fn follow(config: &Config) -> Result<()> {
    loop {
        if let Err(err) = follow_once(config) {
            eprintln!("warn: GoXLR Nexus watcher unavailable: {err:#}; retrying in 5s");
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn follow_once(config: &Config) -> Result<()> {
    if let Err(err) = apply(config, false) {
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

    for line in BufReader::new(stdout).lines() {
        let line = line.context("failed to read pactl subscribe event")?;
        if line.contains(" on sink ") || line.contains(" on server ") {
            if let Err(err) = apply(config, false) {
                eprintln!("warn: GoXLR Nexus sync failed after PulseAudio event: {err:#}");
            }
        }
    }

    let status = child.wait().context("failed to wait for pactl subscribe")?;
    if !status.success() {
        bail!("pactl subscribe exited with status {status}");
    }
    Ok(())
}

fn usable_goxlr_status(config: &Config, snapshot: &Snapshot) -> Result<()> {
    let status = snapshot
        .goxlr_status
        .as_ref()
        .map_err(|err| anyhow!("{err}"))?;
    validate_goxlr_status(config, status)
}

fn apply_goxlr_profile(config: &Config, snapshot: &Snapshot, dry_run: bool) -> Result<()> {
    let required = required_nodes(config, snapshot)?;

    run_or_print(
        dry_run,
        "pactl",
        &["set-default-source", &required.default_source.name],
    )?;
    sync_monitor_to_selected_output(&required, dry_run)?;
    Ok(())
}

fn apply_fallback_profile(
    config: &Config,
    snapshot: &Snapshot,
    dry_run: bool,
    reason: &anyhow::Error,
) -> Result<()> {
    let outputs = match output_nodes(config, snapshot) {
        Ok(outputs) => outputs,
        Err(err) => {
            eprintln!(
                "warn: GoXLR unavailable and no configured fallback output is present: {err:#}"
            );
            return Ok(());
        }
    };
    eprintln!("warn: GoXLR unavailable, applying output fallback: {reason:#}");
    sync_default_output(&outputs, dry_run).map(|_| ())
}

fn sync_monitor_to_selected_output(required: &RequiredNodes, dry_run: bool) -> Result<()> {
    let selected = sync_default_output(&required.outputs, dry_run)?;

    for output in &required.outputs.output_sinks {
        disconnect_monitor_from_output(&required.monitor_source, output, dry_run);
    }

    link_monitor_to_output(&required.monitor_source, &selected, dry_run)
}

fn sync_default_output(outputs: &OutputNodes, dry_run: bool) -> Result<Node> {
    let selected_name = current_default_sink()?;
    if let Some(selected) = selected_output_sink(outputs, &selected_name) {
        return Ok(selected.clone());
    }

    let fallback = &outputs.fallback_sink;
    if selected_name == fallback.name {
        return Ok(fallback.clone());
    }

    eprintln!(
        "warn: default sink {selected_name} is not managed by goxlr-nexus; switching to fallback sink {}",
        fallback.name
    );
    run_or_print(dry_run, "pactl", &["set-default-sink", &fallback.name])?;
    Ok(fallback.clone())
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
            run_or_print(
                dry_run,
                "pactl",
                &["set-default-source", &config.profile.default_source],
            )?;
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
            device_id: source.device_id.clone(),
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
    Ok(Snapshot {
        nodes: pipewire_nodes()?,
        goxlr_status: goxlr_status().map_err(|err| format!("{err:#}")),
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

fn required_nodes(config: &Config, snapshot: &Snapshot) -> Result<RequiredNodes> {
    Ok(RequiredNodes {
        outputs: output_nodes(config, snapshot)?,
        default_source: find_node(snapshot, &config.profile.default_source)?,
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
    let output = audio_command("pactl")
        .args(["get-default-sink"])
        .output()
        .context("failed to run pactl get-default-sink")?;
    if !output.status.success() {
        bail!(
            "pactl get-default-sink failed with status {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn disconnect_monitor_from_output(monitor_source: &Node, output: &Node, dry_run: bool) {
    for (source_port, sink_port) in [("capture_FL", "playback_FL"), ("capture_FR", "playback_FR")] {
        let source = format!("{}:{source_port}", monitor_source.name);
        let sink = format!("{}:{sink_port}", output.name);
        run_or_print(dry_run, "pw-link", &["--disconnect", &source, &sink]).ok();
    }
}

fn link_monitor_to_output(monitor_source: &Node, output: &Node, dry_run: bool) -> Result<()> {
    for (source_port, sink_port) in [("capture_FL", "playback_FL"), ("capture_FR", "playback_FR")] {
        let source = format!("{}:{source_port}", monitor_source.name);
        let sink = format!("{}:{sink_port}", output.name);
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
        Node {
            name: name.to_string(),
            description: None,
            media_class: Some("Audio/Sink".to_string()),
        }
    }
}
