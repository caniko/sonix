#![allow(missing_docs)]

use nexus_audio_processing::{
    ControlCommand, ControlRequest, RuntimeStatus, SCHEMA_VERSION, StreamFormat,
};

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct RkyvRequest {
    request_id: u128,
    command: ControlCommand,
}

fn main() {
    divan::main();
}

fn request() -> ControlRequest {
    ControlRequest {
        protocol: "nexus-audio-processing.control/v1".into(),
        request_id: "12345-67890".into(),
        command: ControlCommand::SetNoise { enabled: true },
    }
}

fn status() -> RuntimeStatus {
    RuntimeStatus {
        schema_version: SCHEMA_VERSION,
        daemon_running: true,
        healthy: true,
        active: true,
        noise_suppression: true,
        echo_cancellation: true,
        noise_persisted: true,
        echo_persisted: true,
        processed_source: "goxlr_nexus.processed_mic".into(),
        capture_source: "alsa_input.capture".into(),
        render_target: "alsa_output.render".into(),
        format: StreamFormat::default(),
        delay_ms: Some(80),
        retries: 3,
        dropped_frames: 4,
        last_error: None,
    }
}

#[divan::bench]
fn json_request_encode() -> Vec<u8> {
    divan::black_box(serde_json::to_vec(&request()).unwrap())
}

#[divan::bench]
fn rkyv_request_encode() -> rkyv::util::AlignedVec {
    divan::black_box(
        rkyv::to_bytes::<rkyv::rancor::Error>(&RkyvRequest {
            request_id: 12_345,
            command: ControlCommand::SetNoise { enabled: true },
        })
        .unwrap(),
    )
}

#[divan::bench]
fn json_status_round_trip() -> RuntimeStatus {
    let bytes = serde_json::to_vec(&status()).unwrap();
    divan::black_box(serde_json::from_slice(&bytes).unwrap())
}

#[divan::bench]
fn rkyv_status_round_trip() -> RuntimeStatus {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&status()).unwrap();
    let archived =
        rkyv::access::<rkyv::Archived<RuntimeStatus>, rkyv::rancor::Error>(&bytes).unwrap();
    divan::black_box(rkyv::deserialize::<RuntimeStatus, rkyv::rancor::Error>(archived).unwrap())
}
