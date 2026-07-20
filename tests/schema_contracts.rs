use serde_json::{Value, json};

fn assert_valid(schema_text: &str, instance: Value) {
    let schema: Value = serde_json::from_str(schema_text).expect("schema JSON must parse");
    let validator = jsonschema::validator_for(&schema).expect("schema must compile");
    if let Err(error) = validator.validate(&instance) {
        panic!("schema rejected representative output: {error}");
    }
}

#[test]
fn root_artifact_schemas_accept_representative_outputs() {
    assert_valid(
        include_str!("../schemas/config-manifest-v1.schema.json"),
        json!({
            "version": 1,
            "module": "goxlr-utility",
            "files": [{
                "path": "settings.json",
                "source": "/nix/store/example-settings.json",
                "target": "/home/can/.config/goxlr-utility/settings.json",
                "sha256": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "kind": "json",
                "origin": "home-manager"
            }]
        }),
    );
    assert_valid(
        include_str!("../schemas/config-plan-v1.schema.json"),
        json!({
            "schema": "goxlr-config.plan/v1",
            "module": "goxlr-utility",
            "manifestVersion": 1,
            "requiresApply": true,
            "files": [{
                "path": "settings.json",
                "source": "/nix/store/example-settings.json",
                "target": "/home/can/.config/goxlr-utility/settings.json",
                "status": "missing",
                "sourceSha256": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "targetSha256": null
            }]
        }),
    );
}

#[test]
fn observation_and_reconciliation_schemas_accept_representative_outputs() {
    assert_valid(
        include_str!("../schemas/observation-v1.schema.json"),
        json!({
            "schema": "goxlr-nexus.observation/v1",
            "producer": {"name": "goxlr-nexus", "version": "0.1.0"},
            "capturedAt": 1,
            "host": "atlas",
            "sources": [{"source": "pipewire", "status": "ok"}],
            "facts": {"nodes": []},
            "factsDigest": "sha256:0000000000000000000000000000000000000000000000000000000000000000"
        }),
    );
    assert_valid(
        include_str!("../schemas/plan-v1.schema.json"),
        json!({
            "schema": "goxlr-nexus.plan/v1",
            "producer": {"name": "goxlr-nexus", "version": "0.1.0"},
            "readOnly": true,
            "requiresApply": true,
            "current": {"defaultSink": "sink", "defaultSource": "source"},
            "goxlrStatus": "ready",
            "operations": [],
            "diagnostics": []
        }),
    );
    assert_valid(
        include_str!("../schemas/adopt-v1.schema.json"),
        json!({
            "schema": "goxlr-nexus.adopt/v1",
            "producer": {"name": "goxlr-nexus", "version": "0.1.0"},
            "readOnly": true,
            "config": {"outputSinks": [], "obsSources": []},
            "candidates": [],
            "diagnostics": []
        }),
    );
}

#[test]
fn processing_schemas_accept_serialized_state_and_status() {
    assert_valid(
        include_str!("../crates/nexus-audio-processing/schemas/processing-state-v1.schema.json"),
        json!({
            "version": 1,
            "noise-suppression": true,
            "echo-cancellation": null
        }),
    );
    assert_valid(
        include_str!("../crates/nexus-audio-processing/schemas/processing-status-v1.schema.json"),
        json!({
            "schemaVersion": 1,
            "daemonRunning": false,
            "healthy": false,
            "active": false,
            "noiseSuppression": true,
            "echoCancellation": false,
            "noisePersisted": true,
            "echoPersisted": false,
            "processedSource": "processed",
            "captureSource": "capture",
            "renderTarget": "render",
            "format": {"sample_rate_hz": 48000, "channels": 2},
            "delayMs": null,
            "retries": 0,
            "droppedFrames": 0,
            "lastError": "offline"
        }),
    );
}

#[test]
fn schemas_reject_unknown_fields() {
    let schema: Value =
        serde_json::from_str(include_str!("../schemas/config-manifest-v1.schema.json"))
            .expect("schema JSON must parse");
    let validator = jsonschema::validator_for(&schema).expect("schema must compile");
    let invalid = json!({"version": 1, "module": "x", "files": [], "unexpected": true});
    assert!(!validator.is_valid(&invalid));
}
