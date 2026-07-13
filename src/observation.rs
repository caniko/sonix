use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const SCHEMA: &str = "goxlr-nexus.observation/v1";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub schema: &'static str,
    pub producer: Producer,
    #[serde(rename = "capturedAt")]
    pub captured_at: u64,
    pub host: String,
    pub sources: Vec<SourceStatus>,
    pub facts: Facts,
    pub facts_digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Producer {
    pub name: &'static str,
    pub version: &'static str,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceStatus {
    pub source: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    pub pipewire: PipewireFacts,
    pub goxlr: GoxlrFacts,
    pub obs: ObsFacts,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipewireFacts {
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
    pub nodes: Vec<PipewireNode>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PipewireNode {
    pub runtime_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_class: Option<String>,
    pub properties: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GoxlrFacts {
    pub status: String,
    pub mixers: BTreeMap<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ObsFacts {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_scene: Option<String>,
    pub input_kinds: Vec<String>,
    pub existing_inputs: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_settings: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_settings: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_choices: Option<Value>,
    pub choice_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

pub fn facts_digest(facts: &Facts) -> String {
    let mut canonical = facts.clone();
    canonical
        .pipewire
        .nodes
        .sort_by(|left, right| left.runtime_name.cmp(&right.runtime_name));
    canonical.obs.input_kinds.sort();
    canonical.obs.existing_inputs.sort_by_key(|value| {
        value
            .get("inputName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    // All fields are serde_json::Value or ordinary serializable data, so this
    // serialization cannot fail unless the observation model changes to add a
    // non-JSON type.
    let bytes = serde_json::to_vec(&canonical).expect("observation facts are serializable");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts_with_order(order: &[&str]) -> Facts {
        Facts {
            pipewire: PipewireFacts {
                default_sink: None,
                default_source: None,
                nodes: order
                    .iter()
                    .map(|name| PipewireNode {
                        runtime_name: (*name).to_string(),
                        description: None,
                        media_class: None,
                        properties: BTreeMap::new(),
                    })
                    .collect(),
            },
            goxlr: GoxlrFacts {
                status: "ok".to_string(),
                mixers: BTreeMap::new(),
                diagnostic: None,
            },
            obs: ObsFacts {
                status: "ok".to_string(),
                version: None,
                current_scene: None,
                input_kinds: vec!["z-kind".to_string(), "a-kind".to_string()],
                existing_inputs: vec![
                    serde_json::json!({"inputName": "Z"}),
                    serde_json::json!({"inputName": "A"}),
                ],
                default_settings: None,
                input_settings: None,
                device_choices: None,
                choice_status: "no-reference-input".to_string(),
                diagnostic: None,
            },
        }
    }

    #[test]
    fn digest_is_stable_when_runtime_lists_are_reordered() {
        assert_eq!(
            facts_digest(&facts_with_order(&["node-b", "node-a"])),
            facts_digest(&facts_with_order(&["node-a", "node-b"]))
        );
    }
}
