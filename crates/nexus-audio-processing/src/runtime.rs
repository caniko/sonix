use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_queue::ArrayQueue;
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::Pod;
use thiserror::Error;

use crate::config::{ProcessingConfig, RuntimeConfig, SCHEMA_VERSION};
use crate::control::{
    CONTROL_PROTOCOL, ControlCommand, ControlRequest, ControlResponse, RuntimeStatus,
};
use crate::dsp::{AudioFrame, DspError, DuplexProcessor};
use crate::state::{StateStore, Toggle, default_control_socket, default_state_path};

const QUEUE_CAPACITY: usize = 32;

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
}

#[derive(Debug)]
struct SharedRuntime {
    config: Mutex<ProcessingConfig>,
    defaults: ProcessingConfig,
    state_store: StateStore,
    render: ArrayQueue<AudioFrame>,
    capture: ArrayQueue<AudioFrame>,
    output: ArrayQueue<AudioFrame>,
    stop: AtomicBool,
    retries: AtomicU64,
    dropped_frames: AtomicU64,
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
            stop: AtomicBool::new(false),
            retries: AtomicU64::new(0),
            dropped_frames: AtomicU64::new(0),
            status: Mutex::new(status),
        });

        let socket_path = self
            .config
            .control_socket
            .expect("socket path populated by new");
        let server_shared = Arc::clone(&shared);
        let server = thread::Builder::new()
            .name("nexus-processing-control".into())
            .spawn(move || control_server(&socket_path, server_shared))
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

        if let Ok(mut status) = shared.status.lock() {
            status.healthy = true;
            status.active = shared
                .config
                .lock()
                .map(|config| config.active())
                .unwrap_or(false);
        }
        main_loop.run();
        shared.stop.store(true, Ordering::Release);
        let _ = worker.join();
        drop(source);
        drop(capture);
        drop(render);
        let _ = server.join();
        Ok(())
    }
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
    pending: Mutex<Vec<f32>>,
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
            pending: Mutex::new(Vec::with_capacity(
                spec.format.frame_samples() * spec.format.channels as usize,
            )),
        })
        .state_changed(|_, data, _, new| {
            if matches!(new, pw::stream::StreamState::Error(_))
                && let Ok(mut status) = data.shared.status.lock()
            {
                status.healthy = false;
                status.last_error = Some(format!("{new:?}"));
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
                let samples = bytes[..size]
                    .chunks_exact(std::mem::size_of::<f32>())
                    .map(|chunk| f32::from_ne_bytes(chunk.try_into().expect("f32 chunk")))
                    .collect::<Vec<_>>();
                enqueue_input_frames(data, samples);
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
            pending: Mutex::new(Vec::with_capacity(
                format.frame_samples() * format.channels as usize,
            )),
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
                let mut pending = data.pending.lock().expect("output pending lock");
                while pending.len() < capacity {
                    let Some(frame) = data.shared.output.pop() else {
                        break;
                    };
                    let expected = data.format.frame_samples() * data.format.channels as usize;
                    let mut samples = vec![0.0; expected];
                    if frame.write_interleaved(&mut samples).is_ok() {
                        pending.extend(samples);
                    }
                }
                let written = pending.len().min(capacity);
                for (chunk, sample) in bytes
                    .chunks_exact_mut(std::mem::size_of::<f32>())
                    .zip(pending.iter().take(written))
                {
                    chunk.copy_from_slice(&sample.to_ne_bytes());
                }
                for chunk in bytes
                    .chunks_exact_mut(std::mem::size_of::<f32>())
                    .skip(written)
                {
                    chunk.fill(0);
                }
                pending.drain(..written);
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

fn enqueue_input_frames(data: &StreamData, samples: Vec<f32>) {
    let expected = data.format.frame_samples() * data.format.channels as usize;
    let mut frames = Vec::new();
    {
        let Ok(mut pending) = data.pending.lock() else {
            return;
        };
        pending.extend(samples);
        while pending.len() >= expected {
            let frame_samples = pending.drain(..expected).collect::<Vec<_>>();
            if let Ok(frame) = AudioFrame::from_interleaved(data.format, &frame_samples) {
                frames.push(frame);
            }
        }
    }
    let queue = match data.role {
        StreamRole::Render => &data.shared.render,
        StreamRole::Capture => &data.shared.capture,
        StreamRole::Output => return,
    };
    for frame in frames {
        if queue.push(frame).is_err() {
            data.shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
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
    let mut config = shared
        .config
        .lock()
        .map(|value| value.clone())
        .unwrap_or_default();
    let mut processor = match DuplexProcessor::new(format, config.clone()) {
        Ok(processor) => processor,
        Err(error) => {
            mark_unhealthy(&shared, error.to_string());
            return;
        }
    };
    while !shared.stop.load(Ordering::Acquire) {
        if let Ok(updated) = shared.config.lock().map(|value| value.clone())
            && updated != config
        {
            config = updated.clone();
            if let Err(error) = processor.set_config(updated) {
                mark_unhealthy(&shared, error.to_string());
                continue;
            }
        }
        let Some(capture) = shared.capture.pop() else {
            thread::sleep(Duration::from_millis(1));
            continue;
        };
        let render = shared
            .render
            .pop()
            .unwrap_or_else(|| AudioFrame::silence(format));
        let result = (|| {
            let _ = processor.process_render(&render)?;
            processor.process_capture(&capture)
        })();
        match result {
            Ok(frame) => {
                if shared.output.push(frame).is_err() {
                    shared.dropped_frames.fetch_add(1, Ordering::Relaxed);
                }
                if let Ok(mut status) = shared.status.lock() {
                    status.healthy = true;
                    status.active = config.active();
                    status.last_error = None;
                    status.retries = shared.retries.load(Ordering::Relaxed);
                    status.dropped_frames = shared.dropped_frames.load(Ordering::Relaxed);
                }
            }
            Err(error) => {
                shared.retries.fetch_add(1, Ordering::Relaxed);
                mark_unhealthy(&shared, error.to_string());
            }
        }
    }
}

fn mark_unhealthy(shared: &SharedRuntime, error: String) {
    if let Ok(mut status) = shared.status.lock() {
        status.healthy = false;
        status.active = false;
        status.last_error = Some(error);
        status.retries = shared.retries.load(Ordering::Relaxed);
        status.dropped_frames = shared.dropped_frames.load(Ordering::Relaxed);
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
        // Keep the control thread interruptible so a PipeWire shutdown can
        // stop the runtime without leaving a blocking `incoming()` iterator
        // behind.  This also lets the worker's stop flag be observed when
        // the process is shutting down cleanly under systemd.
        let _ = listener.set_nonblocking(true);
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
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(25));
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
fn handle_control(
    mut stream: std::os::unix::net::UnixStream,
    shared: &Arc<SharedRuntime>,
) -> Result<(), RuntimeError> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let request: ControlRequest = serde_json::from_str(&line)?;
    let (ok, error) = if request.protocol != CONTROL_PROTOCOL {
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
    let status = shared.status.lock().ok().map(|status| status.clone());
    let response = ControlResponse {
        protocol: CONTROL_PROTOCOL.to_string(),
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
            update_status_from_state(shared, &state);
        }
        ControlCommand::SetEcho { enabled } => {
            let state = shared
                .state_store
                .set(Toggle::Echo, *enabled, &shared.defaults)?;
            *shared.config.lock().expect("runtime config lock") =
                state.processing(&shared.defaults);
            update_status_from_state(shared, &state);
        }
        ControlCommand::Reset => {
            let state = shared.state_store.reset(&shared.defaults)?;
            *shared.config.lock().expect("runtime config lock") =
                state.processing(&shared.defaults);
            update_status_from_state(shared, &state);
        }
        ControlCommand::Status => {}
    }
    Ok(())
}

fn update_status_from_state(shared: &Arc<SharedRuntime>, state: &crate::state::StateSnapshot) {
    if let Ok(mut status) = shared.status.lock() {
        status.noise_suppression = state.noise_suppression;
        status.echo_cancellation = state.echo_cancellation;
        status.noise_persisted = state.noise_persisted;
        status.echo_persisted = state.echo_persisted;
        status.active = state.noise_suppression || state.echo_cancellation;
    }
}
