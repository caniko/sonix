use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

use crossbeam_queue::ArrayQueue;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use thiserror::Error;

use crate::config::{ProcessingConfig, RuntimeConfig, SCHEMA_VERSION};
use crate::control::{
    CONTROL_FRAME_MAGIC, CONTROL_SOCKET_TIMEOUT, ControlCommand, ControlError, ControlResponse,
    LEGACY_CONTROL_PROTOCOL, RuntimeStatus, access_request, read_frame_after_magic,
    read_legacy_request, write_v2_response,
};
use crate::dsp::{AudioFrame, DspError, DuplexProcessor};
use crate::state::{StateStore, Toggle, default_control_socket, default_state_path};

const QUEUE_CAPACITY: usize = 8;

/// Errors returned by the PipeWire runtime.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// Runtime configuration is invalid.
    #[error("invalid processing runtime configuration: {0}")]
    Config(#[from] crate::config::FormatError),
    /// PipeWire rejected an operation.
    #[error("PipeWire error: {0}")]
    PipeWire(#[from] pw::Error),
    /// State persistence failed.
    #[error("processing state error: {0}")]
    State(#[from] crate::state::StateError),
    /// DSP processing failed.
    #[error("DSP error: {0}")]
    Dsp(#[from] DspError),
    /// Runtime socket I/O failed.
    #[error("runtime control socket error: {0}")]
    Io(#[from] std::io::Error),
    /// The control protocol could not be decoded.
    #[error("invalid control request: {0}")]
    Json(#[from] serde_json::Error),
    /// The binary control protocol could not be decoded.
    #[error("invalid binary control request: {0}")]
    Control(#[from] ControlError),
}

#[derive(Debug)]
struct SharedRuntime {
    config: Mutex<ProcessingConfig>,
    defaults: ProcessingConfig,
    state_store: StateStore,
    render: ArrayQueue<AudioFrame>,
    capture: ArrayQueue<AudioFrame>,
    output: ArrayQueue<AudioFrame>,
    render_free: ArrayQueue<AudioFrame>,
    capture_free: ArrayQueue<AudioFrame>,
    output_free: ArrayQueue<AudioFrame>,
    stop: AtomicBool,
    config_revision: AtomicU64,
    retries: AtomicU64,
    dropped_frames: AtomicU64,
    healthy: AtomicBool,
    active: AtomicBool,
    worker: OnceLock<thread::Thread>,
    status: Mutex<RuntimeStatus>,
}

/// A long-lived PipeWire capture, processing, and virtual-source runtime.
#[derive(Debug)]
pub struct ProcessingRuntime {
    config: RuntimeConfig,
}

impl ProcessingRuntime {
    /// Creates a runtime from explicit configuration and persistent state.
    pub fn new(mut config: RuntimeConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        if config.state_path.is_none() {
            config.state_path = Some(default_state_path());
        }
        if config.control_socket.is_none() {
            config.control_socket = Some(default_control_socket());
        }
        Ok(Self { config })
    }

    /// Runs the runtime until PipeWire or the process exits.
    pub fn run(self) -> Result<(), RuntimeError> {
        let state_store = StateStore::new(
            self.config
                .state_path
                .clone()
                .expect("state path populated by new"),
        );
        let defaults = self.config.processing.clone();
        let state = state_store.load(&defaults).unwrap_or_else(|error| {
            eprintln!("warn: ignoring invalid processing state: {error}");
            crate::state::StateSnapshot {
                noise_suppression: defaults.noise_suppression,
                echo_cancellation: defaults.echo_cancellation,
                noise_persisted: false,
                echo_persisted: false,
            }
        });
        let effective = state.processing(&defaults);
        let status = RuntimeStatus {
            schema_version: SCHEMA_VERSION,
            daemon_running: true,
            healthy: false,
            active: false,
            noise_suppression: state.noise_suppression,
            echo_cancellation: state.echo_cancellation,
            noise_persisted: state.noise_persisted,
            echo_persisted: state.echo_persisted,
            processed_source: self.config.source.source_name.clone(),
            capture_source: self.config.source.capture_source.clone(),
            render_target: self.config.source.render_target.clone(),
            format: self.config.format,
            delay_ms: effective.processing_delay_ms(),
            retries: 0,
            dropped_frames: 0,
            last_error: None,
        };
        let shared = Arc::new(SharedRuntime {
            config: Mutex::new(effective),
            defaults,
            state_store,
            render: ArrayQueue::new(QUEUE_CAPACITY),
            capture: ArrayQueue::new(QUEUE_CAPACITY),
            output: ArrayQueue::new(QUEUE_CAPACITY),
            render_free: frame_pool(self.config.format),
            capture_free: frame_pool(self.config.format),
            output_free: frame_pool(self.config.format),
            stop: AtomicBool::new(false),
            config_revision: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            healthy: AtomicBool::new(false),
            active: AtomicBool::new(false),
            worker: OnceLock::new(),
            status: Mutex::new(status),
        });

        let socket_path = self
            .config
            .control_socket
            .expect("socket path populated by new");
        let server_socket_path = socket_path.clone();
        let server_shared = Arc::clone(&shared);
        let server = thread::Builder::new()
            .name("nexus-processing-control".into())
            .spawn(move || control_server(&server_socket_path, server_shared))
            .map_err(RuntimeError::Io)?;

        pw::init();
        let main_loop = pw::main_loop::MainLoopRc::new(None)?;
        let context = pw::context::ContextRc::new(&main_loop, None)?;
        let core = context.connect_rc(None)?;
        let params = audio_params(self.config.format)?;

        let render = make_input_stream(
            &core,
            InputStreamSpec {
                name: "nexus-audio-render-reference".into(),
                properties: properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Communication",
                    "target.object" => self.config.source.render_target.as_str(),
                    *pw::keys::STREAM_CAPTURE_SINK => "true",
                },
                format: self.config.format,
                shared: Arc::clone(&shared),
                role: StreamRole::Render,
                params: params.bytes.clone(),
            },
        )?;
        let capture = make_input_stream(
            &core,
            InputStreamSpec {
                name: "nexus-audio-capture".into(),
                properties: properties! {
                    *pw::keys::MEDIA_TYPE => "Audio",
                    *pw::keys::MEDIA_CATEGORY => "Capture",
                    *pw::keys::MEDIA_ROLE => "Communication",
                    "target.object" => self.config.source.capture_source.as_str(),
                },
                format: self.config.format,
                shared: Arc::clone(&shared),
                role: StreamRole::Capture,
                params: params.bytes.clone(),
            },
        )?;
        let source = make_output_stream(
            &core,
            "nexus-audio-processed-source",
            properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Communication",
                *pw::keys::MEDIA_CLASS => "Audio/Source",
                *pw::keys::NODE_NAME => self.config.source.source_name.as_str(),
                *pw::keys::NODE_DESCRIPTION => self.config.source.source_description.as_str(),
            },
            self.config.format,
            Arc::clone(&shared),
            &params,
        )?;

        let worker_shared = Arc::clone(&shared);
        let worker_format = self.config.format;
        let worker = thread::Builder::new()
            .name("nexus-audio-dsp".into())
            .spawn(move || processing_worker(worker_shared, worker_format))
            .map_err(RuntimeError::Io)?;

        shared.healthy.store(true, Ordering::Release);
        shared.active.store(
            shared
                .config
                .lock()
                .map(|config| config.active())
                .unwrap_or(false),
            Ordering::Release,
        );
        main_loop.run();
        shared.stop.store(true, Ordering::Release);
        wake_control_server(&socket_path);
        shared.worker.get().map(thread::Thread::unpark);
        let _ = worker.join();
        drop(source);
        drop(capture);
        drop(render);
        let _ = server.join();
        Ok(())
    }
}

fn frame_pool(format: crate::StreamFormat) -> ArrayQueue<AudioFrame> {
    let pool = ArrayQueue::new(QUEUE_CAPACITY);
    for _ in 0..QUEUE_CAPACITY {
        pool.push(AudioFrame::silence(format))
            .expect("new frame pool has capacity");
    }
    pool
}

impl ProcessingConfig {
    fn processing_delay_ms(&self) -> Option<u32> {
        self.echo_delay.fixed_ms().map(u32::from)
    }
}

#[derive(Debug, Clone, Copy)]
enum StreamRole {
    Render,
    Capture,
    Output,
}

#[derive(Debug)]
struct StreamData {
    format: crate::StreamFormat,
    role: StreamRole,
    shared: Arc<SharedRuntime>,
    pending: Vec<f32>,
    scratch: Vec<f32>,
}

struct InputStreamSpec {
    name: String,
    properties: pw::properties::PropertiesBox,
    format: crate::StreamFormat,
    shared: Arc<SharedRuntime>,
    role: StreamRole,
    params: Vec<u8>,
}

fn make_input_stream<'a>(
    core: &'a pw::core::CoreRc,
    spec: InputStreamSpec,
) -> Result<
    (
        pw::stream::StreamBox<'a>,
        pw::stream::StreamListener<StreamData>,
    ),
    RuntimeError,
> {
    let stream = pw::stream::StreamBox::new(core, &spec.name, spec.properties)?;
    let listener = stream
        .add_local_listener_with_user_data(StreamData {
            format: spec.format,
            role: spec.role,
            shared: spec.shared,
            pending: Vec::with_capacity(
                spec.format.frame_samples() * spec.format.channels as usize,
            ),
            scratch: Vec::new(),
        })
        .state_changed(|_, data, _, new| {
            if matches!(new, pw::stream::StreamState::Error(_)) {
                mark_unhealthy(&data.shared, format!("{new:?}"));
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                return;
            };
            for datum in buffer.datas_mut() {
                let size = datum.chunk().size() as usize;
                let Some(bytes) = datum.data() else {
                    continue;
                };
                let size = size.min(bytes.len());
                enqueue_input_frames(data, &bytes[..size]);
                break;
            }
        })
        .register()?;
    let params = AudioParams { bytes: spec.params };
    let mut param_refs = params.pod_refs();
    stream.connect(
        spa::utils::Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut param_refs,
    )?;
    Ok((stream, listener))
}

fn make_output_stream<'a>(
    core: &'a pw::core::CoreRc,
    name: &str,
    properties: pw::properties::PropertiesBox,
    format: crate::StreamFormat,
    shared: Arc<SharedRuntime>,
    params: &AudioParams,
) -> Result<
    (
        pw::stream::StreamBox<'a>,
        pw::stream::StreamListener<StreamData>,
    ),
    RuntimeError,
> {
    let stream = pw::stream::StreamBox::new(core, name, properties)?;
    let listener = stream
        .add_local_listener_with_user_data(StreamData {
            format,
            role: StreamRole::Output,
            shared,
            pending: Vec::with_capacity(format.frame_samples() * format.channels as usize * 4),
            scratch: vec![0.0; format.frame_samples() * format.channels as usize],
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                return;
            };
            for datum in buffer.datas_mut() {
                let Some(bytes) = datum.data() else {
                    continue;
                };
                let capacity = bytes.len() / std::mem::size_of::<f32>();
                while data.pending.len() < capacity {
                    let Some(frame) = data.shared.output.pop() else {
                        break;
                    };
                    let expected = data.format.frame_samples() * data.format.channels as usize;
                    let written = data.pending.len() + expected <= data.pending.capacity();
                    if written {
                        frame.write_interleaved_unchecked(&mut data.scratch);
                        data.pending.extend_from_slice(&data.scratch);
                    } else {
                        data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                    }
                    recycle_frame(&data.shared.output_free, frame);
                }
                let written = data.pending.len().min(capacity);
                for (chunk, sample) in bytes
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(data.pending.iter().take(written))
                {
                    chunk.copy_from_slice(&sample.to_ne_bytes());
                }
                for chunk in bytes.as_chunks_mut::<4>().0.iter_mut().skip(written) {
                    chunk.fill(0);
                }
                let remaining = data.pending.len() - written;
                data.pending.copy_within(written.., 0);
                data.pending.truncate(remaining);
                *datum.chunk_mut().size_mut() = (capacity * std::mem::size_of::<f32>()) as u32;
                break;
            }
        })
        .register()?;
    let mut param_refs = params.pod_refs();
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut param_refs,
    )?;
    Ok((stream, listener))
}

fn enqueue_input_frames(data: &mut StreamData, bytes: &[u8]) {
    let expected = data.format.frame_samples() * data.format.channels as usize;
    for chunk in bytes.as_chunks::<4>().0 {
        data.pending.push(f32::from_ne_bytes(*chunk));
        if data.pending.len() == expected {
            let queue = match data.role {
                StreamRole::Render => &data.shared.render,
                StreamRole::Capture => &data.shared.capture,
                StreamRole::Output => return,
            };
            let free = match data.role {
                StreamRole::Render => &data.shared.render_free,
                StreamRole::Capture => &data.shared.capture_free,
                StreamRole::Output => return,
            };
            let Some(mut frame) = free.pop() else {
                data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                data.pending.clear();
                continue;
            };
            if frame.copy_from_interleaved(&data.pending).is_err() {
                recycle_frame(free, frame);
                data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
            } else if let Err(frame) = queue.push(frame) {
                data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                recycle_frame(free, frame);
            } else if let Some(worker) = data.shared.worker.get() {
                worker.unpark();
            }
            data.pending.clear();
        }
    }
}

#[derive(Debug)]
struct AudioParams {
    bytes: Vec<u8>,
}

impl AudioParams {
    fn pod_refs(&self) -> Vec<&Pod> {
        vec![Pod::from_bytes(&self.bytes).expect("serialized audio params")]
    }
}

fn audio_params(format: crate::StreamFormat) -> Result<AudioParams, RuntimeError> {
    let mut audio = spa::param::audio::AudioInfoRaw::new();
    audio.set_format(spa::param::audio::AudioFormat::F32LE);
    audio.set_rate(format.sample_rate_hz);
    audio.set_channels(format.channels.into());
    let object = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio.into(),
    };
    let bytes = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map_err(|error| RuntimeError::Io(std::io::Error::other(error.to_string())))?
    .0
    .into_inner();
    Ok(AudioParams { bytes })
}

fn processing_worker(shared: Arc<SharedRuntime>, format: crate::StreamFormat) {
    let _ = shared.worker.set(thread::current());
    let mut config = shared
        .config
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    let mut config_revision = shared.config_revision.load(Ordering::Acquire);
    let mut processor = match DuplexProcessor::new(format, config.clone()) {
        Ok(processor) => processor,
        Err(error) => {
            mark_unhealthy(&shared, error.to_string());
            return;
        }
    };
    let silence = AudioFrame::silence(format);
    let mut render_output = AudioFrame::silence(format);
    while !shared.stop.load(Ordering::Acquire) {
        let current_revision = shared.config_revision.load(Ordering::Acquire);
        if current_revision != config_revision {
            let updated = shared
                .config
                .lock()
                .map(|value| value.clone())
                .unwrap_or_else(|_| config.clone());
            config_revision = current_revision;
            config = updated.clone();
            if let Err(error) = processor.set_config(updated) {
                mark_unhealthy(&shared, error.to_string());
                continue;
            }
        }
        let Some(capture) = shared.capture.pop() else {
            thread::park();
            continue;
        };
        let render = shared.render.pop();
        let render_input = render.as_ref().unwrap_or(&silence);
        let result = (|| {
            processor.process_render_into_unchecked(render_input, &mut render_output)?;
            let Some(mut output) = shared.output_free.pop() else {
                shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            };
            if let Err(error) = processor.process_capture_into_unchecked(&capture, &mut output) {
                recycle_frame(&shared.output_free, output);
                return Err(error);
            }
            if let Err(output) = shared.output.push(output) {
                shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                recycle_frame(&shared.output_free, output);
            }
            Ok(())
        })();
        if let Some(render) = render {
            recycle_frame(&shared.render_free, render);
        }
        recycle_frame(&shared.capture_free, capture);
        match result {
            Ok(()) => {
                if !shared.healthy.swap(true, Ordering::Release)
                    && let Ok(mut status) = shared.status.lock()
                {
                    status.last_error = None;
                }
                shared.active.store(config.active(), Ordering::Release);
            }
            Err(error) => {
                shared.retries.fetch_add(1, Ordering::Relaxed);
                mark_unhealthy(&shared, error.to_string());
            }
        }
    }
}

fn mark_unhealthy(shared: &SharedRuntime, error: String) {
    shared.healthy.store(false, Ordering::Release);
    shared.active.store(false, Ordering::Release);
    if let Ok(mut status) = shared.status.lock() {
        status.last_error = Some(error);
        status.retries = shared.retries.load(Ordering::Relaxed);
        status.dropped_frames = shared.dropped_frames.load(Ordering::Relaxed);
    }
}

fn recycle_frame(pool: &ArrayQueue<AudioFrame>, frame: AudioFrame) {
    let _ = pool.push(frame);
}

fn wake_worker(shared: &SharedRuntime) {
    if let Some(worker) = shared.worker.get() {
        worker.unpark();
    }
}

fn control_server(path: &Path, shared: Arc<SharedRuntime>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
            let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
        }
        if path.exists() {
            let _ = fs::remove_file(path);
        }
        let Ok(listener) = UnixListener::bind(path) else {
            mark_unhealthy(
                &shared,
                format!("cannot bind control socket {}", path.display()),
            );
            return;
        };
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        loop {
            if shared.stop.load(Ordering::Acquire) {
                break;
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if let Err(error) = handle_control(stream, &shared) {
                        mark_unhealthy(&shared, error.to_string());
                    }
                }
                Err(_) => break,
            }
        }
        let _ = fs::remove_file(path);
    }
    #[cfg(not(unix))]
    {
        let _ = (path, shared);
    }
}

#[cfg(unix)]
fn wake_control_server(path: &Path) {
    let _ = std::os::unix::net::UnixStream::connect(path);
}

#[cfg(not(unix))]
fn wake_control_server(_path: &Path) {}

#[cfg(unix)]
fn handle_control(
    mut stream: std::os::unix::net::UnixStream,
    shared: &Arc<SharedRuntime>,
) -> Result<(), RuntimeError> {
    stream.set_read_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_SOCKET_TIMEOUT))?;
    let mut prefix = [0_u8; 4];
    stream.read_exact(&mut prefix)?;
    if prefix == CONTROL_FRAME_MAGIC {
        let payload = read_frame_after_magic(&mut stream, prefix)?;
        let request = access_request(&payload)?;
        let request_id = request.request_id_native();
        let (ok, error) = match apply_archived_command(request.command(), shared) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.to_string())),
        };
        let status = status_snapshot(shared);
        write_v2_response(&mut stream, request_id, ok, status, error)?;
        return Ok(());
    }

    let request = read_legacy_request(&mut stream, prefix)?;
    let (ok, error) = if request.protocol != LEGACY_CONTROL_PROTOCOL {
        (
            false,
            Some(format!("unsupported protocol {}", request.protocol)),
        )
    } else {
        match apply_command(&request.command, shared) {
            Ok(()) => (true, None),
            Err(error) => (false, Some(error.to_string())),
        }
    };
    let status = status_snapshot(shared);
    let response = ControlResponse {
        protocol: LEGACY_CONTROL_PROTOCOL.to_string(),
        request_id: request.request_id,
        ok,
        status,
        error,
    };
    serde_json::to_writer(&mut stream, &response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn apply_archived_command(
    command: &crate::control::ArchivedControlCommand,
    shared: &Arc<SharedRuntime>,
) -> Result<(), RuntimeError> {
    match command {
        crate::control::ArchivedControlCommand::SetNoise { enabled } => {
            apply_command(&ControlCommand::SetNoise { enabled: *enabled }, shared)
        }
        crate::control::ArchivedControlCommand::SetEcho { enabled } => {
            apply_command(&ControlCommand::SetEcho { enabled: *enabled }, shared)
        }
        crate::control::ArchivedControlCommand::Reset => {
            apply_command(&ControlCommand::Reset, shared)
        }
        crate::control::ArchivedControlCommand::Status => {
            apply_command(&ControlCommand::Status, shared)
        }
    }
}

fn apply_command(
    command: &ControlCommand,
    shared: &Arc<SharedRuntime>,
) -> Result<(), RuntimeError> {
    match command {
        ControlCommand::SetNoise { enabled } => {
            let state = shared
                .state_store
                .set(Toggle::Noise, *enabled, &shared.defaults)?;
            *shared.config.lock().expect("runtime config lock") =
                state.processing(&shared.defaults);
            shared.config_revision.fetch_add(1, Ordering::Release);
            wake_worker(shared);
            update_status_from_state(shared, &state);
        }
        ControlCommand::SetEcho { enabled } => {
            let state = shared
                .state_store
                .set(Toggle::Echo, *enabled, &shared.defaults)?;
            *shared.config.lock().expect("runtime config lock") =
                state.processing(&shared.defaults);
            shared.config_revision.fetch_add(1, Ordering::Release);
            wake_worker(shared);
            update_status_from_state(shared, &state);
        }
        ControlCommand::Reset => {
            let state = shared.state_store.reset(&shared.defaults)?;
            *shared.config.lock().expect("runtime config lock") =
                state.processing(&shared.defaults);
            shared.config_revision.fetch_add(1, Ordering::Release);
            wake_worker(shared);
            update_status_from_state(shared, &state);
        }
        ControlCommand::Status => {}
    }
    Ok(())
}

fn update_status_from_state(shared: &Arc<SharedRuntime>, state: &crate::state::StateSnapshot) {
    shared.active.store(
        state.noise_suppression || state.echo_cancellation,
        Ordering::Release,
    );
    if let Ok(mut status) = shared.status.lock() {
        status.noise_suppression = state.noise_suppression;
        status.echo_cancellation = state.echo_cancellation;
        status.noise_persisted = state.noise_persisted;
        status.echo_persisted = state.echo_persisted;
    }
}

fn status_snapshot(shared: &SharedRuntime) -> Option<RuntimeStatus> {
    let mut status = shared.status.lock().ok()?.clone();
    status.healthy = shared.healthy.load(Ordering::Acquire);
    status.active = shared.active.load(Ordering::Acquire);
    status.retries = shared.retries.load(Ordering::Relaxed);
    status.dropped_frames = shared.dropped_frames.load(Ordering::Relaxed);
    Some(status)
}
