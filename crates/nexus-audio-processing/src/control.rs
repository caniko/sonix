use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{SCHEMA_VERSION, StreamFormat};

/// The versioned control protocol identifier.
pub const CONTROL_PROTOCOL: &str = "nexus-audio-processing.control/v1";

/// The feature command sent over the control socket.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "action")]
pub enum ControlCommand {
    /// Set noise suppression.
    SetNoise {
        /// Desired state.
        enabled: bool,
    },
    /// Set echo cancellation.
    SetEcho {
        /// Desired state.
        enabled: bool,
    },
    /// Clear persisted overrides.
    Reset,
    /// Return current state without changing it.
    Status,
}

/// A versioned control request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlRequest {
    /// Protocol identifier.
    pub protocol: String,
    /// Caller-provided correlation identifier.
    pub request_id: String,
    /// Requested operation.
    pub command: ControlCommand,
}

/// Runtime health and routing status returned by the control API.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    /// State schema version.
    pub schema_version: u32,
    /// Whether the daemon is serving the control socket.
    pub daemon_running: bool,
    /// Whether all requested stages are healthy.
    pub healthy: bool,
    /// Whether the processed source is currently active.
    pub active: bool,
    /// Effective noise-suppression state.
    pub noise_suppression: bool,
    /// Effective echo-cancellation state.
    pub echo_cancellation: bool,
    /// Whether noise state is persisted.
    pub noise_persisted: bool,
    /// Whether echo state is persisted.
    pub echo_persisted: bool,
    /// Published source name, when configured.
    pub processed_source: String,
    /// Raw capture source name.
    pub capture_source: String,
    /// Render reference node name.
    pub render_target: String,
    /// Negotiated stream format.
    pub format: StreamFormat,
    /// Effective AEC delay, when known.
    pub delay_ms: Option<u32>,
    /// Number of worker retries.
    pub retries: u64,
    /// Number of dropped frames.
    pub dropped_frames: u64,
    /// Last degraded-runtime error.
    pub last_error: Option<String>,
}

/// A successful or failed control response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlResponse {
    /// Protocol identifier.
    pub protocol: String,
    /// Correlation identifier copied from the request.
    pub request_id: String,
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Current runtime status, when available.
    pub status: Option<RuntimeStatus>,
    /// Human-readable failure text.
    pub error: Option<String>,
}

/// Errors returned by the local control client.
#[derive(Debug, Error)]
pub enum ControlError {
    /// Unix-domain sockets are only available on Unix hosts.
    #[error("control IPC is only supported on Unix")]
    Unsupported,
    /// Socket I/O failed.
    #[error("control socket I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// A request or response was not valid JSON.
    #[error("invalid control protocol JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// The daemon returned a failed operation.
    #[error("processing daemon rejected request: {0}")]
    Rejected(String),
}

/// A small synchronous client for the local processing daemon.
#[derive(Debug, Clone)]
pub struct ControlClient {
    socket: std::path::PathBuf,
}

impl ControlClient {
    /// Creates a client targeting an explicit Unix socket.
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            socket: path.into(),
        }
    }

    /// Returns the configured socket path.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Sets noise suppression and returns the daemon status.
    pub fn set_noise(&self, enabled: bool) -> Result<RuntimeStatus, ControlError> {
        self.send(ControlCommand::SetNoise { enabled })
    }

    /// Sets echo cancellation and returns the daemon status.
    pub fn set_echo(&self, enabled: bool) -> Result<RuntimeStatus, ControlError> {
        self.send(ControlCommand::SetEcho { enabled })
    }

    /// Clears persisted overrides and returns the daemon status.
    pub fn reset(&self) -> Result<RuntimeStatus, ControlError> {
        self.send(ControlCommand::Reset)
    }

    /// Reads the daemon status.
    pub fn status(&self) -> Result<RuntimeStatus, ControlError> {
        self.send(ControlCommand::Status)
    }

    fn send(&self, command: ControlCommand) -> Result<RuntimeStatus, ControlError> {
        #[cfg(unix)]
        {
            use std::os::unix::net::UnixStream;

            let mut stream = UnixStream::connect(&self.socket)?;
            let request = ControlRequest {
                protocol: CONTROL_PROTOCOL.to_string(),
                request_id: format!("{}-{}", std::process::id(), monotonic_id()),
                command,
            };
            serde_json::to_writer(&mut stream, &request)?;
            stream.write_all(b"\n")?;
            stream.shutdown(std::net::Shutdown::Write)?;
            let mut line = String::new();
            BufReader::new(stream).read_line(&mut line)?;
            let response: ControlResponse = serde_json::from_str(&line)?;
            if !response.ok {
                return Err(ControlError::Rejected(
                    response
                        .error
                        .unwrap_or_else(|| "unknown daemon error".into()),
                ));
            }
            response.status.ok_or_else(|| {
                ControlError::Rejected("daemon returned no status for a successful request".into())
            })
        }
        #[cfg(not(unix))]
        {
            let _ = command;
            Err(ControlError::Unsupported)
        }
    }
}

fn monotonic_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

/// Builds an offline status for a CLI invocation when the daemon is absent.
pub fn offline_status(
    processed_source: impl Into<String>,
    capture_source: impl Into<String>,
    render_target: impl Into<String>,
    format: StreamFormat,
    state: &crate::state::StateSnapshot,
) -> RuntimeStatus {
    RuntimeStatus {
        schema_version: SCHEMA_VERSION,
        daemon_running: false,
        healthy: false,
        active: false,
        noise_suppression: state.noise_suppression,
        echo_cancellation: state.echo_cancellation,
        noise_persisted: state.noise_persisted,
        echo_persisted: state.echo_persisted,
        processed_source: processed_source.into(),
        capture_source: capture_source.into(),
        render_target: render_target.into(),
        format,
        delay_ms: None,
        retries: 0,
        dropped_frames: 0,
        last_error: Some("processing daemon is not running; change is pending".into()),
    }
}
