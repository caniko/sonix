//! Local-only GoXLR FX telemetry for crackling/distortion triage.
//!
//! Runtime opt-in (`telemetry.enable`, default off). No audio is recorded,
//! nothing leaves the host.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Runtime opt-in. Default-off; Pkl/Nix set `telemetry.enable`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub(crate) struct TelemetrySettings {
    pub(crate) enable: bool,
}

pub(crate) const TELEMETRY_SCHEMA: &str = "sonix.goxlr.telemetry/v1";
const TELEMETRY_FILE: &str = "telemetry.jsonl";
const ROTATED_FILE: &str = "telemetry.jsonl.1";
// ponytail: fixed 2x16MiB rotation, configurable size if retention ever matters.
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Unchanged samples are written at most this often (in ticks), keeping
/// days of history inside the rotation budget instead of a few hours.
const HEARTBEAT_TICKS: u64 = 60;

/// Allowlisted FX/button state plus a redacted detail object (no serials).
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FxState {
    pub(crate) fx_enabled: Option<bool>,
    pub(crate) fx_button: Option<bool>,
    pub(crate) detail: Value,
}

/// Best-effort probe bundle. `None` fields record gaps, never stale data.
#[derive(Debug, Default)]
pub(crate) struct TelemetryProbe {
    pub(crate) goxlr: Option<Value>,
    pub(crate) goxlr_error: Option<String>,
    pub(crate) processing: Option<nexus_audio_processing::RuntimeStatus>,
    pub(crate) defaults: Option<Value>,
    pub(crate) defaults_error: Option<String>,
    pub(crate) pipewire: Option<Value>,
    pub(crate) pipewire_error: Option<String>,
}

impl TelemetryProbe {
    pub(crate) fn failed(error: String) -> Self {
        Self {
            goxlr_error: Some(error),
            ..Self::default()
        }
    }
}

pub(crate) struct TelemetryCollector {
    seq: u64,
    session: String,
    dir: PathBuf,
    last_fx: Option<bool>,
    last_button: Option<bool>,
    last_retries: Option<u64>,
    last_dropped: Option<u64>,
    ticks: u64,
    last_sample: Option<(u64, Value)>,
    last_topology: Option<Value>,
}

impl TelemetryCollector {
    pub(crate) fn start(enabled: bool) -> Result<Option<Self>> {
        if !enabled {
            return Ok(None);
        }
        let dir = telemetry_dir()?;
        ensure_private_dir(&dir)?;
        let mut collector = Self {
            seq: 0,
            session: session_id(),
            dir,
            last_fx: None,
            last_button: None,
            last_retries: None,
            last_dropped: None,
            ticks: 0,
            last_sample: None,
            last_topology: None,
        };
        collector.record("sessionStart", json!({}))?;
        tracing::info!(
            dir = %collector.dir.display(),
            "goxlr telemetry collection started (local-only)"
        );
        Ok(Some(collector))
    }

    /// Monotonic tick counter; pipewire probes should run every 5th tick.
    pub(crate) fn next_tick(&mut self) -> u64 {
        self.ticks += 1;
        self.ticks
    }

    /// Returns true when FX enable or physical FX button changed.
    /// Gaps (`None`) never synthesize transitions; the first known sample
    /// establishes the baseline.
    pub(crate) fn fx_changed(&mut self, fx: Option<bool>, button: Option<bool>) -> bool {
        let had_baseline = self.last_fx.is_some() || self.last_button.is_some();
        let fx_delta = fx.is_some_and(|value| Some(value) != self.last_fx);
        let button_delta = button.is_some_and(|value| Some(value) != self.last_button);
        if fx.is_some() {
            self.last_fx = fx;
        }
        if button.is_some() {
            self.last_button = button;
        }
        had_baseline && (fx_delta || button_delta)
    }

    /// Whether a sample differs from the last written one or the heartbeat
    /// is due; records it as written when true.
    pub(crate) fn sample_due(&mut self, detail: &Value) -> bool {
        let due = self.last_sample.as_ref().is_none_or(|(tick, last)| {
            last != detail || self.ticks.saturating_sub(*tick) >= HEARTBEAT_TICKS
        });
        if due {
            self.last_sample = Some((self.ticks, detail.clone()));
        }
        due
    }

    /// Whether the periodic defaults/PipeWire summary changed since the last
    /// `topology` event; records it when true.
    pub(crate) fn topology_changed(&mut self, topology: &Value) -> bool {
        let changed = self.last_topology.as_ref() != Some(topology);
        if changed {
            self.last_topology = Some(topology.clone());
        }
        changed
    }

    /// Counter deltas; `None` on first sample or reset (new < old).
    pub(crate) fn counter_deltas(
        &mut self,
        retries: Option<u64>,
        dropped: Option<u64>,
    ) -> (Option<u64>, Option<u64>) {
        let retry_delta = delta_and_update(&mut self.last_retries, retries);
        let dropped_delta = delta_and_update(&mut self.last_dropped, dropped);
        (retry_delta, dropped_delta)
    }

    pub(crate) fn record(&mut self, event: &str, detail: Value) -> Result<()> {
        self.seq += 1;
        let envelope = json!({
            "schema": TELEMETRY_SCHEMA,
            "session": self.session,
            "seq": self.seq,
            "capturedAt": now_secs(),
            "event": event,
            "detail": detail,
        });
        maybe_rotate(&self.dir)?;
        append_jsonl(&self.dir, &envelope)
    }
}

fn delta_and_update(last: &mut Option<u64>, current: Option<u64>) -> Option<u64> {
    let current = current?;
    let delta = match *last {
        Some(previous) if current >= previous => Some(current - previous),
        Some(_) => None, // counter reset
        None => None,    // first sample
    };
    *last = Some(current);
    delta
}

pub(crate) fn telemetry_dir() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| {
            anyhow::anyhow!("cannot determine state directory; set XDG_STATE_HOME or HOME")
        })?;
    if !base.is_absolute() {
        bail!("state directory must be absolute for telemetry");
    }
    Ok(base.join("sonix/telemetry"))
}

fn session_id() -> String {
    format!("{}-{}", std::process::id(), now_secs())
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn ensure_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)
        .with_context(|| format!("create telemetry directory {}", dir.display()))?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("restrict telemetry directory {}", dir.display()))?;
    Ok(())
}

fn current_path(dir: &Path) -> PathBuf {
    dir.join(TELEMETRY_FILE)
}

fn rotated_path(dir: &Path) -> PathBuf {
    dir.join(ROTATED_FILE)
}

fn maybe_rotate(dir: &Path) -> Result<()> {
    let current = current_path(dir);
    let size = fs::metadata(&current).map(|meta| meta.len()).unwrap_or(0);
    if size < MAX_FILE_BYTES {
        return Ok(());
    }
    let rotated = rotated_path(dir);
    let _ = fs::remove_file(&rotated);
    fs::rename(&current, &rotated)
        .with_context(|| format!("rotate telemetry file {}", current.display()))?;
    Ok(())
}

fn append_jsonl(dir: &Path, value: &Value) -> Result<()> {
    let path = current_path(dir);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("open telemetry file {}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).ok();
    use std::io::Write;
    serde_json::to_writer(&mut file, value).context("encode telemetry event")?;
    file.write_all(b"\n").context("append telemetry event")?;
    Ok(())
}

/// Extract allowlisted FX state. Never includes serials, profile/sample
/// names, preset-name maps, paths, or USB topology.
pub(crate) fn summarize_goxlr(status: &Value, serial: Option<&str>) -> Option<FxState> {
    let mixers = status.get("mixers")?.as_object()?;
    let mixer = match serial {
        Some(serial) => mixers.get(serial)?,
        None => mixers.values().next()?,
    };
    let fx_enabled = mixer
        .pointer("/effects/is_enabled")
        .and_then(Value::as_bool);
    let fx_button = mixer
        .pointer("/button_down/EffectFx")
        .and_then(Value::as_bool);
    let detail = json!({
        "daemonVersion": status.pointer("/config/daemon_version"),
        "deviceType": mixer.pointer("/hardware/device_type"),
        "firmware": mixer.pointer("/hardware/versions/firmware"),
        "fxEnabled": fx_enabled,
        "fxButton": fx_button,
        "activePreset": mixer.pointer("/effects/active_preset"),
        "effects": mixer.get("effects").map(|effects| json!({
            "current": effects.get("current"),
        })),
        "mic": mixer.get("mic_status").map(|mic| json!({
            "micType": mic.get("mic_type"),
            "gate": mic.get("noise_gate").map(|gate| json!({
                "enabled": gate.get("enabled"),
                "threshold": gate.get("threshold"),
                "attenuation": gate.get("attenuation"),
            })),
            "compressor": mic.get("compressor"),
        })),
        "volumes": mixer.pointer("/levels/volumes"),
        "monitorWithFx": mixer.pointer("/settings/enable_monitor_with_fx"),
        "routerMic": mixer.pointer("/router/Microphone"),
        "faderMute": mixer.get("fader_status"),
        "cough": mixer.get("cough_button"),
    });
    Some(FxState {
        fx_enabled,
        fx_button,
        detail,
    })
}

/// Allowlisted Sonix processing health (reuses existing counters).
pub(crate) fn summarize_processing(status: &nexus_audio_processing::RuntimeStatus) -> Value {
    json!({
        "healthy": status.healthy,
        "active": status.active,
        "noise": status.noise_suppression,
        "echo": status.echo_cancellation,
        "retries": status.retries,
        "droppedFrames": status.dropped_frames,
        "lastError": status.last_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_status() -> Value {
        json!({
            "config": {"daemon_version": "1.2.4"},
            "mixers": {
                "SERIAL-REDACT-ME": {
                    "hardware": {
                        "device_type": "Full",
                        "serial_number": "SERIAL-REDACT-ME",
                        "versions": {"firmware": [1, 4, 3, 110]},
                        "usb_device": {"bus_number": 1, "address": 25}
                    },
                    "effects": {
                        "is_enabled": true,
                        "active_preset": "Preset1",
                        "preset_names": {"Preset1": "My Secret Preset"},
                        "current": {"echo": {"style": "Quarter"}}
                    },
                    "button_down": {"EffectFx": true},
                    "levels": {"volumes": {"Mic": 255}},
                    "router": {"Microphone": {"Headphones": true}},
                    "mic_status": {"mic_type": "Condenser"},
                    "settings": {"enable_monitor_with_fx": false},
                    "profile_name": "Secret Profile",
                    "fader_status": {"A": {}},
                    "cough_button": {}
                }
            }
        })
    }

    #[test]
    fn redacts_serials_names_paths_and_usb_topology() {
        let summary = summarize_goxlr(&sample_status(), Some("SERIAL-REDACT-ME")).unwrap();
        let encoded = serde_json::to_string(&summary.detail).unwrap();
        for forbidden in [
            "SERIAL-REDACT-ME",
            "My Secret Preset",
            "preset_names",
            "Secret Profile",
            "profile_name",
            "bus_number",
            "usb_device",
        ] {
            assert!(!encoded.contains(forbidden), "leaked {forbidden}");
        }
        assert_eq!(summary.fx_enabled, Some(true));
        assert_eq!(summary.fx_button, Some(true));
    }

    #[test]
    fn selects_configured_mixer_without_logging_serial() {
        let mut status = sample_status();
        status["mixers"]["OTHER"] = json!({
            "hardware": {"device_type": "Mini", "versions": {}},
            "effects": {"is_enabled": false},
            "button_down": {"EffectFx": false}
        });
        let summary = summarize_goxlr(&status, Some("SERIAL-REDACT-ME")).unwrap();
        assert_eq!(summary.fx_enabled, Some(true));
    }

    #[test]
    fn detects_fx_transitions_without_gap_chatter() {
        let mut collector = TelemetryCollector {
            seq: 0,
            session: "test".to_string(),
            dir: std::env::temp_dir(),
            last_fx: None,
            last_button: None,
            last_retries: None,
            last_dropped: None,
            ticks: 0,
            last_sample: None,
            last_topology: None,
        };
        // First known sample establishes the baseline, not a transition.
        assert!(!collector.fx_changed(Some(false), Some(false)));
        assert!(!collector.fx_changed(Some(false), Some(false)));
        assert!(collector.fx_changed(Some(true), Some(false)));
        assert!(collector.fx_changed(Some(true), Some(true)));
        assert!(!collector.fx_changed(None, None));
        assert!(!collector.fx_changed(Some(true), Some(true)));
    }

    #[test]
    fn writes_changed_samples_and_heartbeats_only() {
        let mut collector = TelemetryCollector {
            seq: 0,
            session: "test".to_string(),
            dir: std::env::temp_dir(),
            last_fx: None,
            last_button: None,
            last_retries: None,
            last_dropped: None,
            ticks: 0,
            last_sample: None,
            last_topology: None,
        };
        let quiet = json!({"fx": false});
        assert!(collector.sample_due(&quiet));
        collector.next_tick();
        assert!(!collector.sample_due(&quiet));
        assert!(collector.sample_due(&json!({"fx": true})));
        for _ in 0..HEARTBEAT_TICKS {
            collector.next_tick();
        }
        assert!(collector.sample_due(&json!({"fx": true})));
        assert!(collector.topology_changed(&json!({"nodes": 3})));
        assert!(!collector.topology_changed(&json!({"nodes": 3})));
    }

    #[test]
    fn counter_deltas_handle_first_sample_and_reset() {
        let mut collector = TelemetryCollector {
            seq: 0,
            session: "test".to_string(),
            dir: std::env::temp_dir(),
            last_fx: None,
            last_button: None,
            last_retries: None,
            last_dropped: None,
            ticks: 0,
            last_sample: None,
            last_topology: None,
        };
        assert_eq!(collector.counter_deltas(Some(5), Some(7)), (None, None));
        assert_eq!(
            collector.counter_deltas(Some(8), Some(7)),
            (Some(3), Some(0))
        );
        assert_eq!(collector.counter_deltas(Some(2), Some(9)), (None, Some(2)));
    }
}
