use std::collections::BTreeMap;

use agent_vocab::{
    validate_durable_content, validate_inline_image, ContentBlock, ImageArtifactId,
    ToolResultMessage, TranscriptItem, UserMessage, MAX_AGGREGATE_IMAGE_BYTES,
    MAX_IMAGES_PER_CONTENT,
};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const INPUT_EVENTS: &[&str] = &[
    "input.queued",
    "input.promoted",
    "input.updated",
    "input.cancelled",
    "input.reordered",
    "input.consumed",
    "input.accepted",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingArtifact {
    pub artifact_id: ImageArtifactId,
    pub mime_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolActionBookkeeping {
    Reason,
    Control,
    Error,
}

fn tool_action_bookkeeping(value: &Value) -> Option<ToolActionBookkeeping> {
    let object = value.as_object()?;
    match object.len() {
        1 if object.contains_key("reason")
            && object.get("reason").and_then(Value::as_str).is_some() =>
        {
            Some(ToolActionBookkeeping::Reason)
        }
        1 if object.contains_key("error")
            && object.get("error").and_then(Value::as_str).is_some() =>
        {
            Some(ToolActionBookkeeping::Error)
        }
        2 if object.get("reason").and_then(Value::as_str).is_some()
            && object
                .get("control_input_id")
                .and_then(Value::as_str)
                .is_some() =>
        {
            Some(ToolActionBookkeeping::Control)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CutoverValue {
    pub changed: bool,
    pub applicable: bool,
    pub artifacts: BTreeMap<ImageArtifactId, PendingArtifact>,
    pub content_image_sets: Vec<Vec<ImageArtifactId>>,
}

impl CutoverValue {
    fn applicable(changed: bool) -> Self {
        Self {
            changed,
            applicable: true,
            ..Self::default()
        }
    }

    fn opaque() -> Self {
        Self::default()
    }

    fn merge(&mut self, other: Self) -> Result<(), String> {
        self.changed |= other.changed;
        self.applicable |= other.applicable;
        self.content_image_sets.extend(other.content_image_sets);
        for (id, artifact) in other.artifacts {
            if let Some(existing) = self.artifacts.get(&id) {
                if existing != &artifact {
                    return Err(format!("conflicting bytes for image artifact {id}"));
                }
            } else {
                self.artifacts.insert(id, artifact);
            }
        }
        Ok(())
    }
}

pub fn cutover_transcript_item(value: &mut Value) -> Result<CutoverValue, String> {
    let kind = typed_object(value, "transcript item")?;
    let result = match kind {
        "user_message" => cutover_object_content(value, "transcript user message")?,
        "tool_result" => cutover_tool_result(value)?,
        _ => return Ok(CutoverValue::opaque()),
    };
    serde_json::from_value::<TranscriptItem>(value.clone())
        .map_err(|error| format!("invalid transcript item: {error}"))?;
    Ok(result)
}

pub fn cutover_tool_action_result(
    action_status: &str,
    value: &mut Value,
) -> Result<CutoverValue, String> {
    if let Some(bookkeeping) = tool_action_bookkeeping(value) {
        let valid = matches!(
            (action_status, bookkeeping),
            (
                "interrupted",
                ToolActionBookkeeping::Reason | ToolActionBookkeeping::Control
            ) | ("error", ToolActionBookkeeping::Error)
        );
        return if valid {
            Ok(CutoverValue::opaque())
        } else {
            Err(format!(
                "tool action bookkeeping does not match status {action_status}"
            ))
        };
    }
    if !matches!(action_status, "completed" | "error") {
        return Err(format!(
            "canonical tool result does not match status {action_status}"
        ));
    }
    let result = cutover_tool_result(value)?;
    let typed: ToolResultMessage = serde_json::from_value(value.clone())
        .map_err(|error| format!("invalid tool result: {error}"))?;
    let status_matches = match action_status {
        "completed" => matches!(typed.status, agent_vocab::ToolResultStatus::Success),
        "error" => !matches!(typed.status, agent_vocab::ToolResultStatus::Success),
        _ => false,
    };
    if !status_matches {
        return Err(format!(
            "canonical tool result status does not match action status {action_status}"
        ));
    }
    let canonical =
        serde_json::to_value(typed).map_err(|error| format!("serialize tool result: {error}"))?;
    if canonical != *value {
        return Err("tool result contains unknown or noncanonical fields".to_string());
    }
    *value = canonical;
    Ok(result)
}

pub fn cutover_tool_result(value: &mut Value) -> Result<CutoverValue, String> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| "tool result is not an object".to_string())?;
    let has_output = object.contains_key("output");
    let has_content = object.contains_key("content");
    if has_output && has_content {
        return Err("tool result has both output and content".to_string());
    }
    let mut output_changed = false;
    if has_output {
        let output = object
            .remove("output")
            .and_then(|value| value.as_str().map(str::to_string))
            .ok_or_else(|| "tool result output is not a string".to_string())?;
        object.insert(
            "content".to_string(),
            json!([{"type":"text","text":output}]),
        );
        output_changed = true;
    } else if !has_content {
        return Err("tool result has neither output nor content".to_string());
    }
    let mut result = cutover_object_content(value, "tool result")?;
    result.changed |= output_changed;
    serde_json::from_value::<ToolResultMessage>(value.clone())
        .map_err(|error| format!("invalid tool result: {error}"))?;
    Ok(result)
}

pub fn cutover_queued_input(value: &mut Value) -> Result<CutoverValue, String> {
    let tagged = value.get("type").and_then(Value::as_str);
    let wrap_legacy = match tagged {
        Some("user_message") => false,
        Some(_) => return Ok(CutoverValue::opaque()),
        None if value.is_object() => true,
        None => return Err("queued input is not an object".to_string()),
    };
    let message = if wrap_legacy {
        &mut *value
    } else {
        value
            .get_mut("content")
            .ok_or_else(|| "queued user message is missing content".to_string())?
    };
    let mut result = cutover_object_content(message, "queued user message")?;
    serde_json::from_value::<UserMessage>(message.clone())
        .map_err(|error| format!("invalid queued user message: {error}"))?;
    if wrap_legacy {
        let message = std::mem::take(value);
        *value = json!({"type":"user_message","content":message});
        result.changed = true;
    }
    Ok(result)
}

pub fn cutover_event(event_type: &str, payload: &mut Value) -> Result<CutoverValue, String> {
    if event_type == "transcript.appended" {
        let object = payload
            .as_object_mut()
            .ok_or_else(|| "transcript.appended payload is not an object".to_string())?;
        let top_level_item = object.get("item").cloned();
        let Some(entry) = object.get_mut("entry") else {
            if top_level_item.is_some() {
                return Err(
                    "transcript.appended top-level item exists without entry.item".to_string(),
                );
            }
            return Ok(CutoverValue::opaque());
        };
        if entry.is_null() {
            if top_level_item.is_some() {
                return Err(
                    "transcript.appended top-level item exists without entry.item".to_string(),
                );
            }
            return Ok(CutoverValue::opaque());
        }
        let entry = entry
            .as_object_mut()
            .ok_or_else(|| "transcript.appended entry is not an object".to_string())?;
        let item = entry
            .get_mut("item")
            .ok_or_else(|| "transcript.appended entry.item is missing".to_string())?;
        if top_level_item
            .as_ref()
            .is_some_and(|duplicate| duplicate != item)
        {
            return Err("transcript.appended top-level item does not match entry.item".to_string());
        }
        let mut result = cutover_transcript_item(item)?;
        if top_level_item.is_some() {
            object.remove("item");
            result.changed = true;
        }
        return Ok(result);
    }
    if !INPUT_EVENTS.contains(&event_type) {
        return Ok(CutoverValue::opaque());
    }
    let object = payload
        .as_object_mut()
        .ok_or_else(|| format!("{event_type} payload is not an object"))?;
    let mut result = CutoverValue::applicable(false);
    if object.get("content_type").and_then(Value::as_str) == Some("user_message") {
        if let Some(content) = object.get_mut("content") {
            result.merge(cutover_content(content)?)?;
        }
    }
    if let Some(queued_inputs) = object.get_mut("queued_inputs") {
        let projections = queued_inputs
            .as_array_mut()
            .ok_or_else(|| format!("{event_type} queued_inputs is not an array"))?;
        for (index, projection) in projections.iter_mut().enumerate() {
            let projection = projection
                .as_object_mut()
                .ok_or_else(|| format!("{event_type} queued_inputs[{index}] is not an object"))?;
            if projection.get("content_type").and_then(Value::as_str) == Some("user_message") {
                let content = projection
                    .get_mut("content")
                    .ok_or_else(|| format!("{event_type} queued_inputs[{index}] lacks content"))?;
                result.merge(cutover_content(content)?)?;
            }
        }
    }
    Ok(result)
}

fn cutover_object_content(value: &mut Value, context: &str) -> Result<CutoverValue, String> {
    let content = value
        .as_object_mut()
        .and_then(|object| object.get_mut("content"))
        .ok_or_else(|| format!("{context} content is missing"))?;
    cutover_content(content)
}

fn cutover_content(value: &mut Value) -> Result<CutoverValue, String> {
    let blocks = value
        .as_array_mut()
        .ok_or_else(|| "content is not an array".to_string())?;
    if blocks.is_empty() {
        return Err("content is empty".to_string());
    }
    let mut result = CutoverValue::applicable(false);
    let mut ids = Vec::new();
    for (index, block) in blocks.iter_mut().enumerate() {
        let converted =
            cutover_block(block).map_err(|error| format!("content[{index}]: {error}"))?;
        result.changed |= converted.changed;
        if let Some(artifact) = converted.artifact {
            ids.push(artifact.artifact_id.clone());
            result
                .artifacts
                .insert(artifact.artifact_id.clone(), artifact);
        } else if let Some(id) = converted.artifact_id {
            ids.push(id);
        }
    }
    if ids.len() > MAX_IMAGES_PER_CONTENT {
        return Err(format!(
            "at most {MAX_IMAGES_PER_CONTENT} images are allowed"
        ));
    }
    let pending_bytes = ids.iter().try_fold(0usize, |total, id| {
        Ok::<usize, String>(
            total
                + result
                    .artifacts
                    .get(id)
                    .map(|artifact| artifact.data.len())
                    .unwrap_or(0),
        )
    })?;
    if pending_bytes > MAX_AGGREGATE_IMAGE_BYTES {
        return Err(format!(
            "aggregate image bytes exceed {MAX_AGGREGATE_IMAGE_BYTES}"
        ));
    }
    let content: Vec<ContentBlock> = serde_json::from_value(std::mem::take(value))
        .map_err(|error| format!("invalid content: {error}"))?;
    validate_durable_content(&content).map_err(|error| error.to_string())?;
    *value = serde_json::to_value(content).map_err(|error| error.to_string())?;
    result.content_image_sets.push(ids);
    Ok(result)
}

struct ConvertedBlock {
    changed: bool,
    artifact: Option<PendingArtifact>,
    artifact_id: Option<ImageArtifactId>,
}

fn cutover_block(block: &mut Value) -> Result<ConvertedBlock, String> {
    let object = block
        .as_object_mut()
        .ok_or_else(|| "block is not an object".to_string())?;
    match object.get("type").and_then(Value::as_str) {
        Some("text") => {
            exact_keys(object, &["type", "text"])?;
            if object.get("text").and_then(Value::as_str).is_none() {
                return Err("text is not a string".to_string());
            }
            Ok(ConvertedBlock {
                changed: false,
                artifact: None,
                artifact_id: None,
            })
        }
        Some("image") => cutover_image(object),
        Some(other) => Err(format!("unsupported block type {other}")),
        None => Err("block type is missing".to_string()),
    }
}

fn cutover_image(block: &mut Map<String, Value>) -> Result<ConvertedBlock, String> {
    if let Some(id) = block.get("artifact_id") {
        exact_keys(block, &["type", "artifact_id"])?;
        let id = id
            .as_str()
            .ok_or_else(|| "artifact_id is not a string".to_string())?;
        return Ok(ConvertedBlock {
            changed: false,
            artifact: None,
            artifact_id: Some(ImageArtifactId::parse(id)?),
        });
    }
    exact_keys(block, &["type", "image"])?;
    let image = block
        .get("image")
        .and_then(Value::as_object)
        .ok_or_else(|| "image is not an object".to_string())?;
    let (kind, mime_type, value) = if image.contains_key("mime_type") {
        exact_keys(image, &["mime_type", "source"])?;
        let source = image
            .get("source")
            .and_then(Value::as_object)
            .ok_or_else(|| "legacy source is not an object".to_string())?;
        exact_keys(source, &["kind", "value"])?;
        (
            required_string(source, "kind")?.to_string(),
            Some(required_string(image, "mime_type")?.to_string()),
            required_string(source, "value")?.to_string(),
        )
    } else {
        exact_keys(image, &["source"])?;
        let source = image
            .get("source")
            .and_then(Value::as_object)
            .ok_or_else(|| "inline source is not an object".to_string())?;
        let kind = required_string(source, "kind")?.to_string();
        match kind.as_str() {
            "base64" => {
                exact_keys(source, &["kind", "mime_type", "data"])?;
                (
                    kind,
                    Some(required_string(source, "mime_type")?.to_string()),
                    required_string(source, "data")?.to_string(),
                )
            }
            "url" => {
                exact_keys(source, &["kind", "url"])?;
                (kind, None, required_string(source, "url")?.to_string())
            }
            other => return Err(format!("unsupported inline source kind {other}")),
        }
    };
    if kind == "url" {
        replace_url_image_with_text(block, &value);
        return Ok(ConvertedBlock {
            changed: true,
            artifact: None,
            artifact_id: None,
        });
    }
    if kind != "base64" {
        return Err(format!("unsupported legacy source kind {kind}"));
    }
    let (mime_type, data) = validate_inline_image(mime_type.as_deref().unwrap_or_default(), &value)
        .map_err(|error| error.to_string())?;
    let artifact_id = id_for_bytes(&data);
    *block = Map::from_iter([
        ("type".to_string(), Value::String("image".to_string())),
        (
            "artifact_id".to_string(),
            Value::String(artifact_id.as_str().to_string()),
        ),
    ]);
    Ok(ConvertedBlock {
        changed: true,
        artifact: Some(PendingArtifact {
            artifact_id,
            mime_type: mime_type.to_string(),
            data,
        }),
        artifact_id: None,
    })
}

fn replace_url_image_with_text(block: &mut Map<String, Value>, url: &str) {
    *block = Map::from_iter([
        ("type".to_string(), Value::String("text".to_string())),
        (
            "text".to_string(),
            Value::String(format!("[remote image preserved as URL: {url}]")),
        ),
    ]);
}

fn id_for_bytes(bytes: &[u8]) -> ImageArtifactId {
    let digest = Sha256::digest(bytes);
    let hex = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    ImageArtifactId::from_sha256_hex(&hex).expect("SHA-256 produces a valid artifact id")
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is not a string"))
}

fn exact_keys(object: &Map<String, Value>, expected: &[&str]) -> Result<(), String> {
    if object.len() != expected.len() || expected.iter().any(|key| !object.contains_key(*key)) {
        return Err(format!(
            "unexpected fields; expected {}",
            expected.join(", ")
        ));
    }
    Ok(())
}

fn typed_object<'a>(value: &'a Value, context: &str) -> Result<&'a str, String> {
    value
        .as_object()
        .and_then(|object| object.get("type"))
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{context} is not a typed object"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png() -> &'static str {
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg=="
    }

    #[test]
    fn inline_images_become_deduplicated_refs() {
        let mut value = json!({"type":"user_message","content":[
            {"type":"image","image":{"mime_type":"image/png","source":{"kind":"base64","value":png()}}},
            {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":png()}}}
        ]});
        let result = cutover_transcript_item(&mut value).expect("convert");
        assert!(result.changed);
        assert_eq!(result.artifacts.len(), 1);
        assert_eq!(
            value.pointer("/content/0/artifact_id"),
            value.pointer("/content/1/artifact_id")
        );
        assert!(value.pointer("/content/0/image").is_none());
    }

    #[test]
    fn url_images_preserve_exact_value_and_order() {
        let url = "https://example.test/a.png?raw=\"yes\"&line=\n";
        let mut value = json!({"type":"user_message","content":[
            {"type":"text","text":"before"},
            {"type":"image","image":{"source":{"kind":"url","url":url}}},
            {"type":"text","text":"after"}
        ]});
        cutover_transcript_item(&mut value).expect("convert");
        assert_eq!(
            value.pointer("/content/1/text"),
            Some(&json!(format!("[remote image preserved as URL: {url}]")))
        );
    }

    #[test]
    fn unrelated_event_collisions_remain_opaque() {
        let mut value = json!({
            "content_type":"user_message",
            "content":[{"type":"image","image":{"mime_type":"image/png","source":{"kind":"blob","value":"opaque"}}}]
        });
        let original = value.clone();
        assert!(
            !cutover_event("tool.completed", &mut value)
                .unwrap()
                .applicable
        );
        assert_eq!(value, original);
    }

    #[test]
    fn exact_tool_action_bookkeeping_is_status_bound_and_near_misses_fail() {
        for (status, mut value) in [
            ("interrupted", json!({"reason":"session interrupted"})),
            ("interrupted", json!({"reason":"cancelled"})),
            (
                "interrupted",
                json!({"reason":"combined subagent control","control_input_id":"input-1"}),
            ),
            (
                "error",
                json!({"error":"tool failed before producing content"}),
            ),
        ] {
            let original = value.clone();
            let result = cutover_tool_action_result(status, &mut value)
                .expect("sanctioned status-specific bookkeeping");
            assert!(!result.applicable);
            assert_eq!(value, original);
        }

        for (status, mut value) in [
            (
                "interrupted",
                json!({"reason":"session interrupted","content":[]}),
            ),
            (
                "interrupted",
                json!({"reason":"session interrupted","extra":true}),
            ),
            ("interrupted", json!({"control_input_id":"input-1"})),
            ("error", json!({"error":{"message":"not a string"}})),
            ("completed", json!({"reason":"session interrupted"})),
            ("error", json!({"reason":"session interrupted"})),
            (
                "completed",
                json!({"reason":"control","control_input_id":"input-2"}),
            ),
            (
                "error",
                json!({"reason":"control","control_input_id":"input-3"}),
            ),
            ("completed", json!({"error":"failed"})),
            ("interrupted", json!({"error":"failed"})),
        ] {
            assert!(cutover_tool_action_result(status, &mut value).is_err());
        }

        let mut canonical = json!({
            "tool_call_id":"call",
            "tool_name":"Bash",
            "content":[{"type":"text","text":"ok"}],
            "status":"Success"
        });
        assert!(cutover_tool_action_result("interrupted", &mut canonical).is_err());
        cutover_tool_action_result("completed", &mut canonical).expect("completed exact writer");
        assert!(cutover_tool_action_result("error", &mut canonical).is_err());
        canonical["status"] = json!("Error");
        assert!(cutover_tool_action_result("completed", &mut canonical).is_err());
        cutover_tool_action_result("error", &mut canonical).expect("error exact writer");

        for mut noncanonical in [
            json!({
                "tool_call_id":"call-extra-root",
                "tool_name":"Bash",
                "content":[{"type":"text","text":"ok"}],
                "status":"Success",
                "future_metadata":{"forbidden":true}
            }),
            json!({
                "tool_call_id":"call-extra-text",
                "tool_name":"Bash",
                "content":[{"type":"text","text":"ok","future_field":true}],
                "status":"Success"
            }),
            json!({
                "tool_call_id":"call-extra-image",
                "tool_name":"ReadImage",
                "content":[{
                    "type":"image",
                    "artifact_id":format!("sha256:{}", "a".repeat(64)),
                    "future_field":true
                }],
                "status":"Success"
            }),
        ] {
            assert!(cutover_tool_action_result("completed", &mut noncanonical).is_err());
        }
    }

    #[test]
    fn enclosing_metadata_survives_owned_field_conversion() {
        let mut transcript = json!({
            "type":"user_message",
            "content":[
                {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":png()}}}
            ],
            "future_metadata":{"exact":["keep", 7]}
        });
        cutover_transcript_item(&mut transcript).expect("convert transcript");
        assert_eq!(transcript["future_metadata"], json!({"exact":["keep", 7]}));

        let mut queue = json!({
            "type":"user_message",
            "content":{
                "content":[
                    {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":png()}}}
                ],
                "future_metadata":{"exact":["keep", 9]}
            },
            "projection_metadata":{"keep":true}
        });
        cutover_queued_input(&mut queue).expect("convert queue");
        assert_eq!(
            queue.pointer("/content/future_metadata"),
            Some(&json!({"exact":["keep", 9]}))
        );
        assert_eq!(queue["projection_metadata"], json!({"keep":true}));
    }

    #[test]
    fn transcript_event_canonicalizes_matching_historical_duplicate() {
        let item = json!({
            "type":"user_message",
            "content":[
                {"type":"image","image":{"source":{"kind":"base64","mime_type":"image/png","data":png()}}}
            ],
            "future_metadata":{"keep":true}
        });
        let mut payload = json!({
            "item":item,
            "entry":{"item":item,"entry_metadata":{"keep":true}},
            "event_metadata":{"keep":true}
        });

        let first = cutover_event("transcript.appended", &mut payload).expect("convert event");
        assert!(first.changed);
        assert!(payload.get("item").is_none());
        assert!(payload
            .pointer("/entry/item/content/0/artifact_id")
            .is_some());
        assert_eq!(
            payload.pointer("/entry/item/future_metadata"),
            Some(&json!({"keep":true}))
        );
        assert_eq!(payload["event_metadata"], json!({"keep":true}));

        let fixed = payload.clone();
        let second = cutover_event("transcript.appended", &mut payload).expect("fixed point");
        assert!(!second.changed);
        assert_eq!(payload, fixed);
    }

    #[test]
    fn transcript_event_rejects_mismatched_historical_duplicate() {
        let mut payload = json!({
            "item":{"type":"user_message","content":[{"type":"text","text":"top"}]},
            "entry":{"item":{"type":"user_message","content":[{"type":"text","text":"nested"}]}}
        });
        let original = payload.clone();

        assert!(cutover_event("transcript.appended", &mut payload)
            .unwrap_err()
            .contains("does not match"));
        assert_eq!(payload, original);
    }

    #[test]
    fn transcript_event_rejects_orphaned_historical_top_level_item() {
        for mut payload in [
            json!({
                "item":{"type":"user_message","content":[{"type":"text","text":"orphaned"}]}
            }),
            json!({
                "item":{"type":"user_message","content":[{"type":"text","text":"orphaned"}]},
                "entry":null
            }),
        ] {
            assert!(cutover_event("transcript.appended", &mut payload).is_err());
        }
    }
}
