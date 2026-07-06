use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agent_provider::{
    ModelTranscriptEntry, PromptSections, ProviderModelInput, ProviderToolProfile,
};
use agent_session::TranscriptStorageNode;
use agent_store::TokenUsageEstimate;
use agent_vocab::{
    AssistantItem, AssistantMessage, ProviderKind, ProviderReplayItem, ReasoningEffort, ToolCall,
    ToolCallId, TranscriptItem, TurnId, UserMessage,
};
use serde_json::json;

use super::{
    estimate_codex_model_input_tokens, provider_transcript_after_first_model_generated_item,
};

fn storage_node(index: usize, item: TranscriptItem) -> TranscriptStorageNode {
    TranscriptStorageNode {
        id: format!("entry-{index}"),
        parent_id: index.checked_sub(1).map(|index| format!("entry-{index}")),
        timestamp_ms: index as u64,
        item,
        provider_replay: Vec::new(),
    }
}

fn provider_input() -> ProviderModelInput {
    ProviderModelInput::new(
        "test-model",
        PromptSections::stable("stable prompt"),
        vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
            UserMessage::text("full input"),
        ))],
        ProviderToolProfile::None,
        Vec::new(),
        ReasoningEffort::Medium,
    )
}

#[test]
fn anchored_suffix_moves_payload_allocations_into_normalized_provider_entries() {
    let discarded = "discarded assistant payload ".repeat(4_096);
    let retained = "retained suffix payload ".repeat(4_096);
    let replay = ProviderReplayItem::new(
        ProviderKind::OpenAi,
        &json!({
            "type": "function_call_output",
            "output": "opaque replay payload ".repeat(4_096),
        }),
    )
    .expect("replay serializes");
    let retained_pointer = retained.as_ptr();
    let replay_pointer = replay.raw_json.as_ptr();
    let entries = vec![
        storage_node(
            0,
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text(discarded)],
            }),
        ),
        TranscriptStorageNode {
            provider_replay: vec![replay],
            ..storage_node(1, TranscriptItem::UserMessage(UserMessage::text(retained)))
        },
    ];

    let transcript = provider_transcript_after_first_model_generated_item(entries);

    let [ModelTranscriptEntry {
        item: TranscriptItem::UserMessage(message),
        provider_replay,
    }] = transcript.as_slice()
    else {
        panic!("expected exactly the retained user suffix");
    };
    assert_eq!(
        message
            .as_text()
            .expect("expected text-only retained message")
            .as_ptr(),
        retained_pointer
    );
    assert_eq!(provider_replay[0].raw_json.as_ptr(), replay_pointer);
}

#[test]
fn anchored_suffix_canonicalizes_tool_calls_without_moving_owned_payloads() {
    let assistant_replay = ProviderReplayItem::new(
        ProviderKind::OpenAi,
        &json!({"type": "function_call", "call_id": "assistant-call"}),
    )
    .expect("assistant replay serializes");
    let started_replay = ProviderReplayItem::new(
        ProviderKind::OpenAi,
        &json!({"type": "function_call", "call_id": "started-call"}),
    )
    .expect("started replay serializes");
    let assistant_call = ToolCall {
        id: ToolCallId::new("assistant-call"),
        tool_name: "apply_patch".to_string(),
        args_json: json!({"patch": "assistant argument payload ".repeat(4_096)}).to_string(),
    };
    let started_call = ToolCall {
        id: ToolCallId::new("started-call"),
        tool_name: "web_search".to_string(),
        args_json: json!({"query": "started argument payload ".repeat(4_096)}).to_string(),
    };
    let assistant_id_pointer = assistant_call.id.as_str().as_ptr();
    let assistant_args_pointer = assistant_call.args_json.as_ptr();
    let started_id_pointer = started_call.id.as_str().as_ptr();
    let started_args_pointer = started_call.args_json.as_ptr();
    let expected = vec![
        ModelTranscriptEntry {
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(ToolCall {
                    id: assistant_call.id.clone(),
                    tool_name: "Edit".to_string(),
                    args_json: assistant_call.args_json.clone(),
                })],
            }),
            provider_replay: vec![assistant_replay.clone()],
        },
        ModelTranscriptEntry {
            item: TranscriptItem::ToolCallStarted {
                turn_id: TurnId(7),
                tool_call: ToolCall {
                    id: started_call.id.clone(),
                    tool_name: "WebSearch".to_string(),
                    args_json: started_call.args_json.clone(),
                },
            },
            provider_replay: vec![started_replay.clone()],
        },
    ];
    let entries = vec![
        storage_node(
            0,
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("usage anchor".to_string())],
            }),
        ),
        TranscriptStorageNode {
            provider_replay: vec![assistant_replay],
            ..storage_node(
                1,
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::ToolCall(assistant_call)],
                }),
            )
        },
        TranscriptStorageNode {
            provider_replay: vec![started_replay],
            ..storage_node(
                2,
                TranscriptItem::ToolCallStarted {
                    turn_id: TurnId(7),
                    tool_call: started_call,
                },
            )
        },
    ];

    let transcript = provider_transcript_after_first_model_generated_item(entries);

    assert_eq!(transcript, expected);
    let TranscriptItem::AssistantMessage(message) = &transcript[0].item else {
        panic!("expected retained assistant message");
    };
    let [AssistantItem::ToolCall(assistant_call)] = message.items.as_slice() else {
        panic!("expected retained assistant tool call");
    };
    let TranscriptItem::ToolCallStarted { tool_call, .. } = &transcript[1].item else {
        panic!("expected retained tool-call-started item");
    };
    assert_eq!(assistant_call.id.as_str().as_ptr(), assistant_id_pointer);
    assert_eq!(assistant_call.args_json.as_ptr(), assistant_args_pointer);
    assert_eq!(tool_call.id.as_str().as_ptr(), started_id_pointer);
    assert_eq!(tool_call.args_json.as_ptr(), started_args_pointer);
}

#[test]
fn anchored_suffix_processing_is_linear_in_owned_suffix_length() {
    const SUFFIX_LEN: usize = 10_000;
    let entries = (0..SUFFIX_LEN)
        .map(|index| {
            let item = if index == 0 {
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("anchor output".to_string())],
                })
            } else {
                TranscriptItem::UserMessage(UserMessage::text(format!("suffix-{index}")))
            };
            storage_node(index, item)
        })
        .collect::<Vec<_>>();

    let transcript = provider_transcript_after_first_model_generated_item(entries);

    assert_eq!(transcript.len(), SUFFIX_LEN - 1);
}

#[test]
fn anchored_accounting_never_runs_the_full_input_fallback() {
    let full_estimates = AtomicUsize::new(0);
    let usage = TokenUsageEstimate {
        total_tokens: 50_000,
        base_tokens: 50_000,
        estimated_suffix_tokens: 0,
        suffix_start_leaf_id: Some("anchor".to_string()),
        suffix_entries: vec![storage_node(
            0,
            TranscriptItem::UserMessage(UserMessage::text("suffix")),
        )],
    };

    let tokens = estimate_codex_model_input_tokens(Some(usage), &provider_input(), |_| {
        full_estimates.fetch_add(1, Ordering::Relaxed);
        Ok(usize::MAX)
    })
    .expect("anchored estimate succeeds");

    assert!(tokens > 50_000);
    assert_eq!(full_estimates.load(Ordering::Relaxed), 0);
}

#[test]
fn missing_anchor_runs_one_full_input_estimate() {
    let full_estimates = Arc::new(AtomicUsize::new(0));
    let estimates = Arc::clone(&full_estimates);

    let tokens = estimate_codex_model_input_tokens(None, &provider_input(), move |_| {
        estimates.fetch_add(1, Ordering::Relaxed);
        Ok(67_890)
    })
    .expect("fallback estimate succeeds");

    assert_eq!(tokens, 67_890);
    assert_eq!(full_estimates.load(Ordering::Relaxed), 1);
}
