use agent_store::{HistoryTree, TranscriptEntryRecord};
use agent_vocab::{
    AssistantItem, AssistantMessage, ToolCall, ToolCallId, ToolResultMessage, ToolResultStatus,
    TranscriptItem, TurnId, TurnOutcome, UserMessage,
};

use super::*;

fn entry(id: &str, item: TranscriptItem) -> TranscriptEntryRecord {
    TranscriptEntryRecord {
        id: id.to_string(),
        parent_id: None,
        timestamp_ms: 0,
        sequence: 0,
        item,
        provider_replay: Vec::new(),
    }
}

fn history(entries: Vec<TranscriptEntryRecord>) -> HistoryTree {
    HistoryTree {
        session_id: "child".to_string(),
        active_leaf_id: entries.last().map(|entry| entry.id.clone()),
        entries,
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: ToolCallId(id.to_string()),
        tool_name: name.to_string(),
        args_json: args.to_string(),
    }
}

#[test]
fn render_is_exhaustive_and_dedups_tool_calls() {
    // A model completion persists the tool call BOTH inside the assistant
    // message items AND as a standalone ToolCallStarted entry. The renderer must
    // emit the call exactly once (from the ToolCallStarted entry).
    let call = tool_call("call_1", "Bash", r#"{"command":"ls"}"#);
    let history = history(vec![
        entry("u", TranscriptItem::UserMessage(UserMessage::text("do it"))),
        entry(
            "a",
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::Text("running a command".to_string()),
                    AssistantItem::ToolCall(call.clone()),
                ],
            }),
        ),
        entry(
            "tcs",
            TranscriptItem::ToolCallStarted {
                turn_id: TurnId(1),
                tool_call: call.clone(),
            },
        ),
        entry(
            "tr",
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId("call_1".to_string()),
                "Bash",
                "file_a\nfile_b",
            )),
        ),
        entry(
            "tf",
            TranscriptItem::TurnFinished {
                turn_id: TurnId(1),
                outcome: TurnOutcome::Graceful,
            },
        ),
    ]);

    let rendered = render_transcript_markdown(&history);
    assert!(rendered.contains("## User\n\ndo it"));
    assert!(rendered.contains("## Assistant\n\nrunning a command"));
    assert_eq!(rendered.matches("### Tool call: Bash").count(), 1);
    assert!(rendered.contains("\"command\": \"ls\""));
    assert!(rendered.contains("### Tool result: Bash [success]"));
    assert!(rendered.contains("file_a\nfile_b"));
}

#[test]
fn render_includes_failed_tool_results_and_compaction() {
    let history = history(vec![
        entry(
            "tr",
            TranscriptItem::ToolResult(ToolResultMessage {
                tool_call_id: ToolCallId("c".to_string()),
                tool_name: "Bash".to_string(),
                output: "boom".to_string(),
                status: ToolResultStatus::Error,
            }),
        ),
        entry(
            "tr2",
            TranscriptItem::ToolResult(ToolResultMessage::crashed(
                ToolCallId("c2".to_string()),
                "Edit",
            )),
        ),
        entry(
            "cs",
            TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "child",
                "leaf",
                "summarized history",
                None,
                TurnId(1),
            )),
        ),
    ]);
    let rendered = render_transcript_markdown(&history);
    assert!(rendered.contains("### Tool result: Bash [error]"));
    assert!(rendered.contains("boom"));
    assert!(rendered.contains("### Tool result: Edit [crashed]"));
    assert!(rendered.contains("## Compaction summary\n\nsummarized history"));
}

#[test]
fn extract_final_message_takes_last_assistant_text() {
    let history = history(vec![
        entry(
            "a1",
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("first".to_string())],
            }),
        ),
        entry(
            "a2",
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("final answer".to_string())],
            }),
        ),
        // A trailing tool-call-only assistant message has no text and is skipped.
        entry(
            "a3",
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call("c", "Bash", "{}"))],
            }),
        ),
    ]);
    assert_eq!(extract_final_message(&history), "final answer");
}

#[test]
fn suggested_next_reads_trailing_line_verbatim() {
    assert_eq!(
        extract_suggested_next("Looks good.\n\nsuggested_next: approved"),
        Some("approved".to_string())
    );
    // Out-of-set values are recorded verbatim, never validated against an enum.
    assert_eq!(
        extract_suggested_next("done\nsuggested_next: ship_it_now"),
        Some("ship_it_now".to_string())
    );
    assert_eq!(extract_suggested_next("no marker here"), None);
    assert_eq!(extract_suggested_next("suggested_next:"), None);
}

#[test]
fn outcome_defaults_to_crashed_without_a_finished_turn() {
    let empty = history(vec![entry(
        "u",
        TranscriptItem::UserMessage(UserMessage::text("hi")),
    )]);
    assert_eq!(subagent_outcome(&empty), TurnOutcome::Crashed);

    let interrupted = history(vec![entry(
        "tf",
        TranscriptItem::TurnFinished {
            turn_id: TurnId(1),
            outcome: TurnOutcome::Interrupted,
        },
    )]);
    assert_eq!(subagent_outcome(&interrupted), TurnOutcome::Interrupted);
}
