use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

#[cfg(any(feature = "pipewire-runtime", test))]
use std::io::Cursor;

use rkyv::util::AlignedVec;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{SCHEMA_VERSION, StreamFormat};

/// The current binary control protocol identifier.
pub const CONTROL_PROTOCOL: &str = "nexus-audio-processing.control/v2";

/// The legacy JSON control protocol identifier.
pub const LEGACY_CONTROL_PROTOCOL: &str = "nexus-audio-processing.control/v1";

pub(crate) const CONTROL_PROTOCOL_VERSION: u16 = 2;
pub(crate) const CONTROL_FRAME_MAGIC: [u8; 4] = *b"NXAP";
pub(crate) const MAX_CONTROL_FRAME: usize = 64 * 1024;
pub(crate) const CONTROL_SOCKET_TIMEOUT: Duration = Duration::from_secs(2);

/// The feature command sent over the control socket.
#[derive(
    Debug, Clone, Copy, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
#[serde(deny_unknown_fields, rename_all = "kebab-case", tag = "action")]
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

/// A versioned legacy JSON control request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    /// Protocol identifier.
    pub protocol: String,
    /// Caller-provided correlation identifier.
    pub request_id: String,
    /// Requested operation.
    pub command: ControlCommand,
}

/// Runtime health and routing status returned by the control API.
#[derive(
    Debug, Clone, Serialize, Deserialize, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize,
)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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

/// A successful or failed legacy JSON control response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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

#[derive(Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct WireRequest {
    pub(crate) request_id: u128,
    pub(crate) command: ControlCommand,
}

#[derive(Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub(crate) struct WireResponse {
    request_id: u128,
    ok: bool,
    status: Option<RuntimeStatus>,
    error: Option<String>,
}

#[cfg(any(feature = "pipewire-runtime", test))]
impl ArchivedWireRequest {
    pub(crate) fn request_id_native(&self) -> u128 {
        self.request_id.to_native()
    }

    pub(crate) fn command(&self) -> &ArchivedControlCommand {
        &self.command
    }
}

/// Errors returned by the local control client.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ControlError {
    /// Unix-domain sockets are only available on Unix hosts.
    #[error("control IPC is only supported on Unix")]
    Unsupported,
    /// Socket I/O failed.
    #[error("control socket I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A request or response was not valid legacy JSON.
    #[error("invalid legacy control protocol JSON: {0}")]
    Json(#[from] serde_json::Error),
    /// An rkyv archive failed validation or decoding.
    #[error("invalid control archive: {0}")]
    Archive(String),
    /// The frame header was malformed.
    #[error("invalid control frame: {0}")]
    Frame(String),
    /// The peer uses an unsupported binary protocol version.
    #[error("unsupported control protocol version {0}")]
    ProtocolVersion(u16),
    /// The peer sent more data than the bounded frame limit allows.
    #[error("control frame is too large: {actual} bytes (maximum {maximum})")]
    FrameTooLarge {
        /// Received payload length.
        actual: usize,
        /// Maximum accepted payload length.
        maximum: usize,
    },
    /// A binary response did not correspond to the request that was sent.
    #[error("control response correlation identifier did not match the request")]
    CorrelationMismatch,
    /// An old JSON daemon closed the binary connection without responding.
    #[error("legacy daemon did not answer the binary control request")]
    NoBinaryResponse,
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

            let request_id = monotonic_id();
            match self.send_binary(&UnixStream::connect(&self.socket)?, request_id, command) {
                Ok(status) => Ok(status),
                Err(ControlError::NoBinaryResponse) => self.send_legacy(command, request_id),
                Err(error) => Err(error),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = command;
            Err(ControlError::Unsupported)
        }
    }

    #[cfg(unix)]
    fn send_binary(
        &self,
        stream: &std::os::unix::net::UnixStream,
        request_id: u128,
        command: ControlCommand,
    ) -> Result<RuntimeStatus, ControlError> {
        stream.set_read_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
        let mut stream = stream.try_clone()?;
        write_v2_request(&mut stream, request_id, command)?;
        stream.shutdown(std::net::Shutdown::Write)?;

        let payload = match read_frame(&mut stream) {
            Ok(payload) => payload,
            Err(ControlError::Io(error))
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(ControlError::NoBinaryResponse);
            }
            Err(error) => return Err(error),
        };
        let archived = access_response(&payload)?;
        if archived.request_id.to_native() != request_id {
            return Err(ControlError::CorrelationMismatch);
        }
        if !archived.ok {
            let error = archived
                .error
                .as_ref()
                .map(|value| value.as_str().to_owned())
                .unwrap_or_else(|| "unknown daemon error".into());
            return Err(ControlError::Rejected(error));
        }
        let status = archived.status.as_ref().ok_or_else(|| {
            ControlError::Rejected("daemon returned no status for a successful request".into())
        })?;
        rkyv::deserialize::<RuntimeStatus, rkyv::rancor::Error>(status)
            .map_err(|error| ControlError::Archive(error.to_string()))
    }

    #[cfg(unix)]
    fn send_legacy(
        &self,
        command: ControlCommand,
        request_id: u128,
    ) -> Result<RuntimeStatus, ControlError> {
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(&self.socket)?;
        stream.set_read_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
        let request = ControlRequest {
            protocol: LEGACY_CONTROL_PROTOCOL.to_string(),
            request_id: request_id.to_string(),
            command,
        };
        serde_json::to_writer(&mut stream, &request)?;
        stream.write_all(b"\n")?;
        stream.shutdown(std::net::Shutdown::Write)?;
        let line = read_legacy_message(&mut stream)?;
        let response: ControlResponse = serde_json::from_slice(&line)?;
        if response.protocol != LEGACY_CONTROL_PROTOCOL
            || response.request_id != request_id.to_string()
        {
            return Err(ControlError::CorrelationMismatch);
        }
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
}

fn monotonic_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default()
}

fn write_payload<W: Write>(writer: &mut W, payload: &[u8]) -> Result<(), ControlError> {
    if payload.len() > MAX_CONTROL_FRAME {
        return Err(ControlError::FrameTooLarge {
            actual: payload.len(),
            maximum: MAX_CONTROL_FRAME,
        });
    }
    writer.write_all(&CONTROL_FRAME_MAGIC)?;
    writer.write_all(&CONTROL_PROTOCOL_VERSION.to_le_bytes())?;
    writer.write_all(&0_u16.to_le_bytes())?;
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn write_v2_request<W: Write>(
    writer: &mut W,
    request_id: u128,
    command: ControlCommand,
) -> Result<(), ControlError> {
    let payload = rkyv::to_bytes::<rkyv::rancor::Error>(&WireRequest {
        request_id,
        command,
    })
    .map_err(|error| ControlError::Archive(error.to_string()))?;
    write_payload(writer, &payload)
}

pub(crate) fn read_frame<R: Read>(reader: &mut R) -> Result<AlignedVec, ControlError> {
    let mut magic = [0_u8; 4];
    reader.read_exact(&mut magic)?;
    read_frame_after_magic(reader, magic)
}

pub(crate) fn read_frame_after_magic<R: Read>(
    reader: &mut R,
    magic: [u8; 4],
) -> Result<AlignedVec, ControlError> {
    if magic != CONTROL_FRAME_MAGIC {
        return Err(ControlError::Frame("invalid control frame magic".into()));
    }
    let mut header = [0_u8; 8];
    reader.read_exact(&mut header)?;
    let version = u16::from_le_bytes([header[0], header[1]]);
    if version != CONTROL_PROTOCOL_VERSION {
        return Err(ControlError::ProtocolVersion(version));
    }
    let reserved = u16::from_le_bytes([header[2], header[3]]);
    if reserved != 0 {
        return Err(ControlError::Frame(
            "reserved frame bits are not zero".into(),
        ));
    }
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if length > MAX_CONTROL_FRAME {
        return Err(ControlError::FrameTooLarge {
            actual: length,
            maximum: MAX_CONTROL_FRAME,
        });
    }
    let mut payload = AlignedVec::with_capacity(length);
    payload.resize(length, 0);
    reader.read_exact(&mut payload)?;
    Ok(payload)
}

pub(crate) fn read_legacy_message<R: Read>(reader: &mut R) -> Result<Vec<u8>, ControlError> {
    let mut bytes = Vec::new();
    let mut limited = reader.take((MAX_CONTROL_FRAME + 1) as u64);
    limited.read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONTROL_FRAME {
        return Err(ControlError::FrameTooLarge {
            actual: bytes.len(),
            maximum: MAX_CONTROL_FRAME,
        });
    }
    if !bytes.ends_with(b"\n") {
        return Err(ControlError::Frame(
            "legacy control message was not newline terminated".into(),
        ));
    }
    Ok(bytes)
}

#[cfg(any(feature = "pipewire-runtime", test))]
pub(crate) fn read_legacy_request<R: Read>(
    reader: &mut R,
    prefix: [u8; 4],
) -> Result<ControlRequest, ControlError> {
    let mut chained = Cursor::new(prefix).chain(reader);
    let bytes = read_legacy_message(&mut chained)?;
    Ok(serde_json::from_slice(&bytes)?)
}

#[cfg(any(feature = "pipewire-runtime", test))]
pub(crate) fn access_request(payload: &[u8]) -> Result<&ArchivedWireRequest, ControlError> {
    rkyv::access::<ArchivedWireRequest, rkyv::rancor::Error>(payload)
        .map_err(|error| ControlError::Archive(error.to_string()))
}

pub(crate) fn access_response(payload: &[u8]) -> Result<&ArchivedWireResponse, ControlError> {
    rkyv::access::<ArchivedWireResponse, rkyv::rancor::Error>(payload)
        .map_err(|error| ControlError::Archive(error.to_string()))
}

#[cfg(any(feature = "pipewire-runtime", test))]
pub(crate) fn write_v2_response<W: Write>(
    writer: &mut W,
    request_id: u128,
    ok: bool,
    status: Option<RuntimeStatus>,
    error: Option<String>,
) -> Result<(), ControlError> {
    let payload = rkyv::to_bytes::<rkyv::rancor::Error>(&WireResponse {
        request_id,
        ok,
        status,
        error,
    })
    .map_err(|error| ControlError::Archive(error.to_string()))?;
    write_payload(writer, &payload)
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
        noise_suppression: state.noise_suppression(),
        echo_cancellation: state.echo_cancellation(),
        noise_persisted: state.noise_persisted(),
        echo_persisted: state.echo_persisted(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> RuntimeStatus {
        RuntimeStatus {
            schema_version: SCHEMA_VERSION,
            daemon_running: true,
            healthy: true,
            active: true,
            noise_suppression: true,
            echo_cancellation: false,
            noise_persisted: true,
            echo_persisted: false,
            processed_source: "processed".into(),
            capture_source: "capture".into(),
            render_target: "render".into(),
            format: StreamFormat::default(),
            delay_ms: Some(80),
            retries: 2,
            dropped_frames: 3,
            last_error: None,
        }
    }

    #[test]
    fn binary_request_is_framed_and_validated_without_deserializing_command() {
        let mut frame = Vec::new();
        write_v2_request(&mut frame, 42, ControlCommand::SetNoise { enabled: true }).unwrap();
        assert_eq!(&frame[..4], &CONTROL_FRAME_MAGIC);
        assert_eq!(
            u16::from_le_bytes([frame[4], frame[5]]),
            CONTROL_PROTOCOL_VERSION
        );

        let payload = read_frame(&mut Cursor::new(frame)).unwrap();
        let request = access_request(&payload).unwrap();
        assert_eq!(request.request_id.to_native(), 42);
        assert!(matches!(
            request.command,
            ArchivedControlCommand::SetNoise { enabled: true }
        ));
    }

    #[test]
    fn binary_response_round_trips_status() {
        let mut frame = Vec::new();
        write_v2_response(&mut frame, 7, true, Some(status()), None).unwrap();
        let payload = read_frame(&mut Cursor::new(frame)).unwrap();
        let response = access_response(&payload).unwrap();
        assert_eq!(response.request_id.to_native(), 7);
        assert!(response.ok);
        let decoded = rkyv::deserialize::<RuntimeStatus, rkyv::rancor::Error>(
            response.status.as_ref().unwrap(),
        )
        .unwrap();
        assert_eq!(decoded.processed_source, "processed");
        assert_eq!(decoded.format, StreamFormat::default());
    }

    #[test]
    fn invalid_headers_and_lengths_are_rejected() {
        let error = read_frame(&mut Cursor::new(b"NOPE")).unwrap_err();
        assert!(matches!(error, ControlError::Frame(_)));

        let mut frame = Vec::from(CONTROL_FRAME_MAGIC);
        frame.extend_from_slice(&99_u16.to_le_bytes());
        frame.extend_from_slice(&0_u16.to_le_bytes());
        frame.extend_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(frame)),
            Err(ControlError::ProtocolVersion(99))
        ));

        let mut frame = Vec::from(CONTROL_FRAME_MAGIC);
        frame.extend_from_slice(&CONTROL_PROTOCOL_VERSION.to_le_bytes());
        frame.extend_from_slice(&1_u16.to_le_bytes());
        frame.extend_from_slice(&0_u32.to_le_bytes());
        assert!(matches!(
            read_frame(&mut Cursor::new(frame)),
            Err(ControlError::Frame(_))
        ));

        let mut frame = Vec::from(CONTROL_FRAME_MAGIC);
        frame.extend_from_slice(&CONTROL_PROTOCOL_VERSION.to_le_bytes());
        frame.extend_from_slice(&0_u16.to_le_bytes());
        frame.extend_from_slice(&((MAX_CONTROL_FRAME as u32) + 1).to_le_bytes());
        let error = read_frame(&mut Cursor::new(frame)).unwrap_err();
        assert!(matches!(error, ControlError::FrameTooLarge { .. }));

        let mut frame = Vec::from(CONTROL_FRAME_MAGIC);
        frame.extend_from_slice(&CONTROL_PROTOCOL_VERSION.to_le_bytes());
        frame.extend_from_slice(&0_u16.to_le_bytes());
        frame.extend_from_slice(&4_u32.to_le_bytes());
        frame.extend_from_slice(&[1, 2]);
        assert!(matches!(
            read_frame(&mut Cursor::new(frame)),
            Err(ControlError::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn legacy_request_is_bounded_and_decoded() {
        let request = ControlRequest {
            protocol: LEGACY_CONTROL_PROTOCOL.into(),
            request_id: "42".into(),
            command: ControlCommand::Status,
        };
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        let prefix = [bytes[0], bytes[1], bytes[2], bytes[3]];
        let decoded = read_legacy_request(&mut Cursor::new(bytes[4..].to_vec()), prefix).unwrap();
        assert_eq!(decoded.request_id, "42");
        assert!(matches!(decoded.command, ControlCommand::Status));
    }

    #[cfg(unix)]
    #[test]
    fn client_uses_binary_protocol() {
        use std::os::unix::net::UnixListener;
        use std::thread;
        use tempfile::tempdir;

        let directory = tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut prefix = [0_u8; 4];
            stream.read_exact(&mut prefix).unwrap();
            assert_eq!(prefix, CONTROL_FRAME_MAGIC);
            let payload = read_frame_after_magic(&mut stream, prefix).unwrap();
            let request = access_request(&payload).unwrap();
            assert!(matches!(request.command(), ArchivedControlCommand::Status));
            write_v2_response(
                &mut stream,
                request.request_id_native(),
                true,
                Some(status()),
                None,
            )
            .unwrap();
        });

        let result = ControlClient::new(&path).status().unwrap();
        server.join().unwrap();
        assert_eq!(result.processed_source, "processed");
    }

    #[cfg(unix)]
    #[test]
    fn client_falls_back_to_legacy_json_for_old_daemon() {
        use std::io::{BufRead, BufReader};
        use std::os::unix::net::UnixListener;
        use std::thread;
        use tempfile::tempdir;

        let directory = tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            {
                let (mut binary, _) = listener.accept().unwrap();
                let mut ignored = Vec::new();
                binary.read_to_end(&mut ignored).unwrap();
            }

            let (mut legacy, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(legacy.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: ControlRequest = serde_json::from_str(&line).unwrap();
            assert_eq!(request.protocol, LEGACY_CONTROL_PROTOCOL);
            let response = ControlResponse {
                protocol: LEGACY_CONTROL_PROTOCOL.into(),
                request_id: request.request_id,
                ok: true,
                status: Some(status()),
                error: None,
            };
            serde_json::to_writer(&mut legacy, &response).unwrap();
            legacy.write_all(b"\n").unwrap();
        });

        let result = ControlClient::new(&path).status().unwrap();
        server.join().unwrap();
        assert_eq!(result.processed_source, "processed");
    }
}
