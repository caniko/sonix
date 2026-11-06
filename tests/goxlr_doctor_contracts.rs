//! End-to-end contracts for `goxlr-nexus doctor` against a shimmed PipeWire and
//! GoXLR daemon.
//!
//! These cover the failures that leave the router `active` while no audio is
//! routed, which is how the GoXLR outages presented on atlas:
//!
//!   * `goxlr-client` hangs because a stale daemon holds the IPC socket.
//!   * The card is on the single-PCM `Direct` verb, so none of the `HiFi__*`
//!     nodes the routing addresses exist.
//!
//! Both must fail loudly. A router that reports success in those states hides
//! the outage until someone runs `doctor` by hand.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

const SERIAL: &str = "S200805412CQK";
const CAPTURE: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Headset__source";
const RENDER: &str = "alsa_output.usb-TC-Helicon_GoXLR-00.HiFi__Speaker__sink";
const MONITOR: &str = "alsa_input.usb-TC-Helicon_GoXLR-00.HiFi__Line4__source";
const PROCESSED: &str = "goxlr_nexus.processed_mic";
const JDS: &str = "alsa_output.test.jds.analog-stereo";
const THINKPAD: &str = "alsa_output.test.thinkpad.analog-stereo";

struct Fixture {
    dir: PathBuf,
    config: PathBuf,
}

impl Fixture {
    /// Build a throwaway HOME/PATH with `pw-dump` and `goxlr-client` shims.
    fn new(name: &str, graph: Value, client: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "sonix-doctor-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("bin")).expect("create shim dir");
        fs::create_dir_all(dir.join("config")).expect("create config dir");
        fs::create_dir_all(dir.join("runtime")).expect("create runtime dir");
        fs::create_dir_all(dir.join("state")).expect("create state dir");

        write_shim(
            &dir.join("bin/pw-dump"),
            &format!("cat <<'JSON'\n{graph}\nJSON\n"),
        );
        write_shim(&dir.join("bin/goxlr-client"), client);

        let config = dir.join("config/goxlr.pkl");
        fs::write(&config, config_pkl()).expect("write config");
        Self { dir, config }
    }

    fn run(&self, subcommand: &str) -> std::process::Output {
        // Shim directory first, real PATH after it: the tools under test are
        // replaced, but the shims themselves still need basic utilities.
        let path = match std::env::var_os("PATH") {
            Some(existing) => std::env::join_paths(
                std::iter::once(self.dir.join("bin")).chain(std::env::split_paths(&existing)),
            )
            .expect("join PATH"),
            None => self.dir.join("bin").into_os_string(),
        };
        Command::new(env!("CARGO_BIN_EXE_goxlr-nexus"))
            .arg("--config")
            .arg(&self.config)
            .arg(subcommand)
            .env("PATH", path)
            .env("HOME", self.dir.join("config"))
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            // Never inherit the live session's socket directory: these shims
            // must not be able to reach a real processor.
            .env("XDG_RUNTIME_DIR", self.dir.join("runtime"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .output()
            .unwrap_or_else(|error| panic!("run goxlr-nexus {subcommand}: {error}"))
    }

    fn doctor(&self) -> std::process::Output {
        self.run("doctor")
    }

    fn daemon_check(&self) -> std::process::Output {
        self.run("daemon-check")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn write_shim(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}")).expect("write shim");
    let mut perms = fs::metadata(path).expect("shim metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod shim");
}

fn config_pkl() -> String {
    format!(
        r#"
jdsSink = "{JDS}"
fallbackSink = "{JDS}"
thinkpadSink = "{THINKPAD}"
outputSinks = new Listing {{
  "{JDS}"
  "{THINKPAD}"
}}
goxlrSerial = "{SERIAL}"

profile = new {{
  defaultSink = "{RENDER}"
  defaultSource = "{CAPTURE}"
  monitorSource = "{MONITOR}"
  captureSampleRate = 48000
  renderSampleRate = 48000
  captureChannels = 2
  renderChannels = 2
}}

processing = new {{
  sourceName = "{PROCESSED}"
  sourceDescription = "GoXLR Nexus processed microphone"
  noiseSuppression = false
  echoCancellation = false
  noiseLevel = "moderate"
}}
"#
    )
}

fn card(profile: &str, split: bool) -> Value {
    json!({
        "id": 1,
        "type": "PipeWire:Interface:Device",
        "info": {"props": {
            "device.name": "alsa_card.usb-TC-Helicon_GoXLR-00",
            "device.profile.name": profile,
            "api.alsa.split-enable": split,
        }},
    })
}

fn node(id: u64, name: &str, class: &str) -> Value {
    json!({
        "id": id,
        "type": "PipeWire:Interface:Node",
        "info": {"state": "idle", "props": {"node.name": name, "media.class": class}},
    })
}

/// A healthy GoXLR on the SplitPCM `HiFi` verb: every addressed node exists.
fn hifi_graph() -> Value {
    json!([
        card("HiFi", true),
        node(10, CAPTURE, "Audio/Source"),
        node(11, MONITOR, "Audio/Source"),
        node(12, RENDER, "Audio/Sink"),
        node(13, PROCESSED, "Audio/Source"),
        node(14, JDS, "Audio/Sink"),
        node(15, THINKPAD, "Audio/Sink"),
    ])
}

/// The same card stuck on the single-PCM verb: no `HiFi__*` node exists, which
/// is exactly the state the SplitPCM rule regression produced.
fn direct_graph() -> Value {
    json!([
        card("Direct", false),
        node(
            10,
            "alsa_input.usb-TC-Helicon_GoXLR-00.Direct__Direct__source",
            "Audio/Source"
        ),
        node(
            12,
            "alsa_output.usb-TC-Helicon_GoXLR-00.Direct__Direct__sink",
            "Audio/Sink"
        ),
        node(14, JDS, "Audio/Sink"),
        node(15, THINKPAD, "Audio/Sink"),
    ])
}

fn healthy_status() -> String {
    format!(
        "cat <<'JSON'\n{}\nJSON\n",
        json!({"mixers": {SERIAL: {"hardware": {"device_type": "Full"}}}, "config": {}, "files": {}})
    )
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn doctor_succeeds_on_a_healthy_split_pcm_graph() {
    let fixture = Fixture::new("healthy", hifi_graph(), &healthy_status());
    let output = fixture.doctor();
    let text = output_text(&output);

    assert!(output.status.success(), "healthy fixture must pass: {text}");
    assert!(
        text.contains(&format!("ok: GoXLR serial {SERIAL}")),
        "must report the device: {text}"
    );
    assert!(
        text.contains("ok: GoXLR UCM SplitPCM is enabled"),
        "must confirm the split that carries the HiFi verb: {text}"
    );
    assert!(
        !text.contains("fail:"),
        "healthy fixture must not report failures: {text}"
    );
}

#[test]
fn doctor_fails_when_the_daemon_does_not_answer() {
    // A stale daemon holds the IPC socket and never replies. `goxlr-client` is
    // bounded by a 5 s timeout, so this hangs long past it.
    let fixture = Fixture::new("hung-daemon", hifi_graph(), "exec sleep 6\n");
    let output = fixture.doctor();
    let text = output_text(&output);

    assert!(
        !output.status.success(),
        "a hanging goxlr-client must fail doctor: {text}"
    );
    assert!(
        text.contains("goxlr-client"),
        "the failure must name the client so the stale daemon is findable: {text}"
    );
}

#[test]
fn doctor_fails_when_the_split_pcm_nodes_are_missing() {
    let fixture = Fixture::new("direct-verb", direct_graph(), &healthy_status());
    let output = fixture.doctor();
    let text = output_text(&output);

    assert!(
        !output.status.success(),
        "a Direct-only card must fail doctor instead of passing silently: {text}"
    );
    assert!(
        text.contains(CAPTURE),
        "the failure must name the node that is missing: {text}"
    );
}

#[test]
fn doctor_fails_when_the_configured_serial_is_absent() {
    let other_serial = json!({"mixers": {"OTHER": {"hardware": {"device_type": "Full"}}}});
    let fixture = Fixture::new(
        "wrong-serial",
        hifi_graph(),
        &format!("cat <<'JSON'\n{other_serial}\nJSON\n"),
    );
    let output = fixture.doctor();
    let text = output_text(&output);

    assert!(
        !output.status.success(),
        "a mismatched serial must fail: {text}"
    );
    assert!(
        text.contains(SERIAL),
        "the failure must name the serial: {text}"
    );
}

fn status_for(device_type: &str) -> String {
    format!(
        "cat <<'JSON'\n{}\nJSON\n",
        json!({"mixers": {SERIAL: {"hardware": {"device_type": device_type}}}, "config": {}, "files": {}})
    )
}

#[test]
fn daemon_check_succeeds_for_a_full_device_on_the_configured_serial() {
    let fixture = Fixture::new("daemon-check-ok", hifi_graph(), &status_for("Full"));
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(output.status.success(), "healthy daemon must pass: {text}");
    assert!(
        text.contains(SERIAL),
        "the check must report the configured serial: {text}"
    );
}

#[test]
fn daemon_check_does_not_probe_pipewire_or_touch_audio() {
    let fixture = Fixture::new("daemon-check-read-only", hifi_graph(), &status_for("Full"));
    // Any PipeWire probe would fail here; the check must only read the daemon.
    write_shim(&fixture.dir.join("bin/pw-dump"), "exit 1\n");
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(
        output.status.success(),
        "daemon-check must not depend on PipeWire: {text}"
    );
}

#[test]
fn daemon_check_fails_when_the_daemon_returns_malformed_json() {
    let fixture = Fixture::new(
        "daemon-check-malformed",
        hifi_graph(),
        "printf 'not json'\n",
    );
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(!output.status.success(), "malformed JSON must fail: {text}");
    assert!(
        text.contains("JSON"),
        "the failure must name the unparseable status: {text}"
    );
}

#[test]
fn daemon_check_fails_when_no_mixer_is_reported() {
    let empty = json!({"mixers": {}, "config": {}, "files": {}});
    let fixture = Fixture::new(
        "daemon-check-empty",
        hifi_graph(),
        &format!("cat <<'JSON'\n{empty}\nJSON\n"),
    );
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(
        !output.status.success(),
        "an empty mixer list must fail: {text}"
    );
    assert!(
        text.contains(SERIAL),
        "the failure must name the serial: {text}"
    );
}

#[test]
fn daemon_check_fails_when_the_configured_serial_is_wrong() {
    let other_serial = json!({"mixers": {"OTHER": {"hardware": {"device_type": "Full"}}}});
    let fixture = Fixture::new(
        "daemon-check-wrong-serial",
        hifi_graph(),
        &format!("cat <<'JSON'\n{other_serial}\nJSON\n"),
    );
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(
        !output.status.success(),
        "a mismatched serial must fail: {text}"
    );
    assert!(
        text.contains(SERIAL),
        "the failure must name the serial: {text}"
    );
}

#[test]
fn daemon_check_fails_when_the_configured_device_is_not_full() {
    let fixture = Fixture::new("daemon-check-lite", hifi_graph(), &status_for("Lite"));
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(!output.status.success(), "a Lite device must fail: {text}");
    assert!(
        text.contains("device_type"),
        "the failure must name the device type: {text}"
    );
}

#[test]
fn daemon_check_fails_when_the_daemon_does_not_answer() {
    // A stale daemon holds the IPC socket and never replies. `goxlr-client` is
    // bounded by a 5 s timeout, so this hangs long past it.
    let fixture = Fixture::new("daemon-check-hung", hifi_graph(), "exec sleep 6\n");
    let output = fixture.daemon_check();
    let text = output_text(&output);

    assert!(
        !output.status.success(),
        "a hanging goxlr-client must fail the check: {text}"
    );
    assert!(
        text.contains("timed out") && text.contains("goxlr-client"),
        "the failure must name the timeout and the client: {text}"
    );
}
